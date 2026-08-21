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

/// What the remote pane's foreground implies for local input handling.
///
/// Three states because two different questions hide in "is it a TUI?": which
/// cursor-key encoding to use, and who should get the mouse. An agent CLI is not
/// a shell (it sets DECCKM, so arrows must be application mode) and still does
/// not read mouse reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fg {
    /// interactive shell at a prompt: sets no mouse modes, so the grab is
    /// released and herdr does its own native selection
    Shell,
    /// an agent CLI. herdr identified it, so this is not a guess.
    Agent,
    /// anything else: assume it wants the mouse, which is the safe default
    /// because being wrong only costs a selection, never an app's clicks
    Mouse,
}

/// Classify from the remote pane's `agent` field and its foreground job.
///
/// The agent question is answered by HERDR, not by us: `PaneInfo.agent` comes
/// from its `identify_agent_in_job`, which scans the whole foreground job across
/// its own canonical agent table and resolves CLIs shipped behind `node`, `bun`
/// or `python` wrappers using argv0/argv/cmdline. A hardcoded list here would be
/// a second, worse copy of data herdr already maintains and already serves over
/// the API we are calling anyway — and it would drift the day a new agent ships.
///
/// It also fixes the leaf problem for free: `process-info` returns the whole
/// foreground process GROUP, so an agent's leaf is whatever tool it just spawned
/// (`node`, `rg`, `bash`) and moves every few seconds. `agent` does not move.
pub fn classify(pane_json: &str, proc_json: &str) -> Option<Fg> {
    let pane: serde_json::Value = serde_json::from_str(pane_json).ok()?;
    if pane.get("result")?.get("pane")?.get("agent").and_then(|v| v.as_str()).is_some() {
        return Some(Fg::Agent);
    }
    let v: serde_json::Value = serde_json::from_str(proc_json).ok()?;
    let fg = v.get("result")?.get("process_info")?.get("foreground_processes")?.as_array()?;
    // the last foreground process is the actually-running leaf, so `sudo vim`
    // classifies on `vim`, not `sudo`
    let name = fg.last()?.get("name")?.as_str()?;
    Some(if is_shell(name) { Fg::Shell } else { Fg::Mouse })
}

/// Query the remote pane's foreground process over ssh and classify it. `None`
/// on any failure (ssh/network/parse) so the caller keeps its last known value.
pub async fn poll(
    ssh_target: &str,
    remote_bin: Option<&str>,
    session: Option<&str>,
    pane: &str,
    ctl_path: Option<&str>,
    container: Option<&crate::pane::ContainerArg>,
) -> Option<Fg> {
    // same expression as the observe session (configured path or PATH auto)
    let bin = crate::config::remote_herdr_expr(remote_bin, session);
    // both answers in ONE hop: same ssh round trip cost as the old single query
    let cmd = format!(
        "{b} pane get {p}; echo '<<>>'; exec {b} pane process-info --pane {p}",
        b = bin,
        p = sh_quote(pane)
    );
    let mut sc = match container {
        Some(ct) => {
            // async resolve, not the blocking one: this runs on the pane's
            // single-threaded runtime and fires on every keystroke burst, so a
            // blocking `docker ps` would stall input and rendering (and hang
            // the pane outright if the Docker daemon wedges).
            //
            // No ControlMaster equivalent is needed — docker exec is local, so
            // there is no handshake to amortize.
            let ids = crate::docker::resolve(&ct.docker_bin, &ct.kind).await.ok()?;
            let id = ids.into_iter().next()?;
            let mut c = Command::new(&ct.docker_bin);
            // `sh -c` not `-lc`: match ssh's non-login remote shell
            c.args(["exec", &id, "sh", "-c", &cmd]);
            c
        }
        None => {
            let mut c = Command::new("ssh");
            // reuse the daemon's ControlMaster when given so the poll skips the
            // handshake; `-S` without `-M` uses an existing master or, if the socket
            // isn't there, connects directly — so this degrades gracefully
            if let Some(path) = ctl_path {
                c.arg("-S").arg(path);
            }
            c.args(SSH_COMMON_OPTS).arg(ssh_target).arg(cmd);
            c
        }
    };
    let out = sc
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (pane_json, proc_json) = text.split_once("<<>>")?;
    classify(pane_json, proc_json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_with_agent(a: Option<&str>) -> String {
        match a {
            Some(a) => format!(r#"{{"result":{{"pane":{{"agent":"{a}"}}}}}}"#),
            None => r#"{"result":{"pane":{}}}"#.to_string(),
        }
    }

    fn proc_with(leaf: &str) -> String {
        format!(r#"{{"result":{{"process_info":{{"foreground_processes":[{{"name":"{leaf}"}}]}}}}}}"#)
    }

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
    fn classify_indeterminate_on_empty_or_garbage() {
        let none = pane_with_agent(None);
        assert_eq!(
            classify(&none, r#"{"result":{"process_info":{"foreground_processes":[]}}}"#),
            None
        );
        assert_eq!(classify(&none, "not json"), None);
        assert_eq!(classify("not json", &proc_with("zsh")), None);
    }
}
