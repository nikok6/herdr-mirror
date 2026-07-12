// Foreground-process detection for the mirror streamer.
//
// herdr strips the mouse-mode DECSET from the frames the plugin observes, so the
// streamer can't tell whether the remote pane's app wants the mouse. As a proxy,
// query the remote pane's foreground process (`herdr pane process-info`) and
// classify it: a plain shell at a prompt never enables mouse reporting, so mouse
// events should stay local (no garbage in the prompt); anything else is treated
// as a possible mouse-aware TUI and clicks are forwarded. This is a heuristic
// stand-in until herdr exposes the pane's mouse-reporting state through the API.

use std::process::Stdio;

use tokio::process::Command;

use crate::pane::sh_quote;
use crate::remote::SSH_COMMON_OPTS;

/// Interactive shells: at a prompt these don't enable mouse reporting, so mouse
/// events over them should stay local rather than being forwarded to the pty.
const SHELLS: &[&str] = &[
    "bash", "zsh", "fish", "sh", "dash", "ksh", "ksh93", "mksh", "ash", "tcsh",
    "csh", "nu", "elvish", "xonsh", "osh", "ysh", "oil", "ion", "murex", "ngs",
    "pwsh", "powershell", "cmd",
];

/// Is `name` one of the known interactive shells? Normalizes a login-shell dash
/// (`-bash`), a leading path, and a Windows `.exe` suffix before matching.
pub fn is_shell(name: &str) -> bool {
    let base = name.trim_start_matches('-').rsplit(['/', '\\']).next().unwrap_or(name);
    let n = base.trim_end_matches(".exe").to_ascii_lowercase();
    SHELLS.contains(&n.as_str())
}

/// Classify a `pane process-info` JSON response. `Some(true)` = foreground is a
/// shell, `Some(false)` = something else, `None` = indeterminate (empty/unparseable).
pub fn classify(json: &str) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let fg = v.get("result")?.get("process_info")?.get("foreground_processes")?.as_array()?;
    // the last foreground process is the actually-running leaf, so `sudo vim`
    // classifies on `vim`, not `sudo`
    let name = fg.last()?.get("name")?.as_str()?;
    Some(is_shell(name))
}

/// Query the remote pane's foreground process over ssh and classify it. `None`
/// on any failure (ssh/network/parse) so the caller keeps its last known value.
pub async fn poll(ssh_target: &str, remote_bin: &str, pane: &str) -> Option<bool> {
    // remote_bin stays unquoted for remote-shell ~ expansion, matching the
    // observe session's command construction
    let cmd = format!("exec {} pane process-info --pane {}", remote_bin, sh_quote(pane));
    let out = Command::new("ssh")
        .args(SSH_COMMON_OPTS)
        .arg(ssh_target)
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    classify(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_recognized_including_login_and_path() {
        assert!(is_shell("zsh"));
        assert!(is_shell("bash"));
        assert!(is_shell("-bash")); // login shell
        assert!(is_shell("/usr/bin/fish")); // full path
        assert!(is_shell("pwsh.exe")); // windows
        assert!(!is_shell("vim"));
        assert!(!is_shell("htop"));
        assert!(!is_shell("nvim"));
        assert!(!is_shell("lazygit"));
    }

    #[test]
    fn classify_reads_leaf_foreground() {
        let shell = r#"{"result":{"process_info":{"foreground_processes":[{"name":"zsh"}]}}}"#;
        assert_eq!(classify(shell), Some(true));
        let tui = r#"{"result":{"process_info":{"foreground_processes":[{"name":"vim"}]}}}"#;
        assert_eq!(classify(tui), Some(false));
        // sudo wrapper: the leaf is the real program
        let sudo =
            r#"{"result":{"process_info":{"foreground_processes":[{"name":"sudo"},{"name":"vim"}]}}}"#;
        assert_eq!(classify(sudo), Some(false));
    }

    #[test]
    fn classify_indeterminate_on_empty_or_garbage() {
        assert_eq!(classify(r#"{"result":{"process_info":{"foreground_processes":[]}}}"#), None);
        assert_eq!(classify("not json"), None);
        assert_eq!(classify(r#"{"result":{}}"#), None);
    }
}
