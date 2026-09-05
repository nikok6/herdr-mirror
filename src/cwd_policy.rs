// Herdr owns directory policy. Read its config without guessing the release
// versus debug config directory; on uncertainty preserve cwd inheritance.
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
pub enum Policy {
    Follow,
    Home,
    Current,
    Path(String),
}

pub fn parse(text: &str) -> Option<Policy> {
    let config: toml::Value = text.parse().ok()?;
    let Some(terminal) = config.get("terminal") else { return Some(Policy::Follow) };
    let terminal = terminal.as_table()?;
    let Some(value) = terminal.get("new_cwd") else { return Some(Policy::Follow) };
    let raw = value.as_str()?;
    Some(match raw.trim() {
        "" | "follow" => Policy::Follow,
        "home" => Policy::Home,
        "current" => Policy::Current,
        _ => Policy::Path(raw.to_string()),
    })
}

pub fn inherit(kind: &str, policy: Option<&Policy>) -> bool {
    kind == "split" || matches!(policy, None | Some(Policy::Follow))
}

fn fixed_path(policy: &Policy, home: &Path) -> Option<PathBuf> {
    let path = match policy {
        Policy::Home => home.to_path_buf(),
        Policy::Path(p) if p == "~" => home.to_path_buf(),
        Policy::Path(p) => match p.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(p),
        },
        // `current` is the SERVER cwd, not this hook's cwd. Without that
        // information we cannot safely classify a newly created local pane.
        Policy::Follow | Policy::Current => return None,
    };
    path.is_absolute().then_some(path)
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    a == b || match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn path_from_help(help: &str) -> Option<PathBuf> {
    let path = PathBuf::from(help.lines().find_map(|line| line.strip_prefix("Config: "))?);
    path.is_absolute().then_some(path)
}

pub async fn local_fixed_path() -> Option<PathBuf> {
    let path = match std::env::var_os("HERDR_CONFIG_PATH").filter(|p| !p.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => {
            let bin = std::env::var_os("HERDR_BIN_PATH").filter(|p| !p.is_empty())?;
            let output = tokio::time::timeout(Duration::from_secs(3),
                tokio::process::Command::new(bin).arg("--help").kill_on_drop(true).output(),
            ).await.ok()?.ok()?;
            if !output.status.success() { return None; }
            path_from_help(std::str::from_utf8(&output.stdout).ok()?)?
        }
    };
    let text = std::fs::read_to_string(path).ok()?;
    fixed_path(&parse(&text)?, &crate::util::home_dir())
}

pub async fn remote_policy(remote: &crate::remote::RemoteHost, host: &crate::config::HostConfig) -> Option<Policy> {
    let bin = crate::config::remote_herdr_expr(host.remote_bin.as_deref(), host.session.as_deref());
    // Pass the reported path as data to POSIX sh, never interpolate it into
    // shell source. Missing config means Herdr's default; read errors don't.
    let reader = r#"IFS= read -r config || exit 1
case "$config" in /*) ;; *) exit 1 ;; esac
if [ -f "$config" ]; then cat "$config"; elif [ ! -e "$config" ]; then :; else exit 1; fi"#;
    let cmd = format!("{bin} --help | sed -n 's/^Config: //p' | sh -c {}", crate::pane::sh_quote(reader));
    parse(&remote.exec(&cmd, 3000).await.ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_policies_without_treating_current_as_follow() {
        for (text, expected) in [
            ("", Some(Policy::Follow)),
            ("[terminal]\nnew_cwd = ' follow '", Some(Policy::Follow)),
            ("[terminal]\nnew_cwd = ''", Some(Policy::Follow)),
            ("[terminal]\nnew_cwd = 'home'", Some(Policy::Home)),
            ("[terminal]\nnew_cwd = 'current'", Some(Policy::Current)),
            ("[terminal]\nnew_cwd = '~'", Some(Policy::Path("~".into()))),
            ("[terminal]\nnew_cwd = 2", None),
            ("[broken", None),
        ] { assert_eq!(parse(text), expected, "{text}"); }
    }

    #[test]
    fn expands_only_resolvable_fixed_paths() {
        let home = Path::new("/home/person");
        for (policy, expected) in [
            (Policy::Home, Some("/home/person")),
            (Policy::Path("~".into()), Some("/home/person")),
            (Policy::Path("~/projects".into()), Some("/home/person/projects")),
            (Policy::Path("/projects".into()), Some("/projects")),
            (Policy::Path("relative".into()), None),
            (Policy::Follow, None),
            (Policy::Current, None),
        ] { assert_eq!(fixed_path(&policy, home), expected.map(PathBuf::from)); }
    }

    #[test]
    fn only_fixed_policy_changes_tab_and_workspace_inheritance() {
        for kind in ["tab", "workspace", "split"] {
            assert!(inherit(kind, None));
            assert!(inherit(kind, Some(&Policy::Follow)));
            for policy in [Policy::Home, Policy::Current, Policy::Path("/projects".into())] {
                assert_eq!(inherit(kind, Some(&policy)), kind == "split");
            }
        }
    }

    #[test]
    fn reads_the_binarys_reported_config_path() {
        assert_eq!(path_from_help("Usage: herdr\nConfig: /tmp/herdr-dev/config.toml\nLogs: elsewhere"), Some("/tmp/herdr-dev/config.toml".into()));
        assert_eq!(path_from_help("Config: relative/config.toml"), None);
        assert_eq!(path_from_help("no config path"), None);
    }
}
