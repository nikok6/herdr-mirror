// hosts.toml loader. Real TOML via the `toml` crate (the TS version hand-rolled
// a subset only because it had to stay dependency-free).

use std::path::Path;

use serde::Deserialize;

use crate::util::{err, Result};

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub name: String,
    pub target: String,
    pub prefix: String,
    pub remote_bin: String,
    /// keep each mirror pane in control (writable, no idle release, and sized to
    /// the local pane so it fills). Default on; ideal for headless remotes. Turn
    /// off per host for a remote a human is actively using directly.
    pub always_control: bool,
    /// forward ordinary SGR mouse click/release packets to the remote PTY.
    /// Default off because herdr's terminal API does not yet expose whether the
    /// remote app enabled mouse mode; wheel events still use terminal.scroll.
    pub mouse_click_passthrough: bool,
}

#[derive(Debug, Clone)]
pub struct MirrorConfig {
    pub poll_seconds: u64,
    /// let the workspace.focused hook start the daemon
    pub autostart: bool,
    /// host that remote-create actions target when invoked outside a mirror
    /// (falls back to the first host declared)
    pub default_host: Option<String>,
    /// when true (the default), closing a mirror workspace/pane locally also
    /// closes the matching object on the remote. Set false to make a local
    /// close only stop mirroring, leaving the remote — and any agent — running.
    pub close_remote_on_local_close: bool,
    pub hosts: Vec<HostConfig>,
}

impl MirrorConfig {
    pub fn default_host(&self) -> Option<&HostConfig> {
        self.default_host
            .as_ref()
            .and_then(|name| self.hosts.iter().find(|h| &h.name == name))
            .or_else(|| self.hosts.first())
    }
}

#[derive(Deserialize)]
struct RawConfig {
    autostart: Option<bool>,
    poll_seconds: Option<u64>,
    default_host: Option<String>,
    close_remote_on_local_close: Option<bool>,
    always_control: Option<bool>,
    mouse_click_passthrough: Option<bool>,
    // toml::Table (preserve_order) keeps declaration order — the first host
    // is the remote-create fallback, so order is user-visible
    #[serde(default)]
    hosts: toml::Table,
}

#[derive(Deserialize)]
struct RawHost {
    target: String,
    prefix: Option<String>,
    remote_bin: Option<String>,
    enabled: Option<bool>,
    always_control: Option<bool>,
    mouse_click_passthrough: Option<bool>,
}

pub fn load_config(config_dir: &Path) -> Result<MirrorConfig> {
    let file = config_dir.join("hosts.toml");
    let text = std::fs::read_to_string(&file).map_err(|_| {
        err(format!(
            "no config at {} — create it with:\n\n[hosts.<name>]\ntarget = \"<ssh target>\"\n",
            file.display()
        ))
    })?;
    parse_config(&text).map_err(|e| err(format!("{}: {e}", file.display())))
}

pub fn parse_config(text: &str) -> Result<MirrorConfig> {
    let raw: RawConfig = toml::from_str(text)?;
    let global_always_control = raw.always_control.unwrap_or(true);
    let global_mouse_click_passthrough = raw.mouse_click_passthrough.unwrap_or(false);
    let mut hosts: Vec<HostConfig> = Vec::new();
    for (name, value) in raw.hosts {
        let h: RawHost = value.try_into().map_err(|e| err(format!("[hosts.{name}]: {e}")))?;
        if h.enabled == Some(false) {
            continue;
        }
        hosts.push(HostConfig {
            prefix: h.prefix.unwrap_or_else(|| name.clone()),
            remote_bin: h.remote_bin.unwrap_or_else(|| "~/.local/bin/herdr".into()),
            always_control: h.always_control.unwrap_or(global_always_control),
            mouse_click_passthrough: h
                .mouse_click_passthrough
                .unwrap_or(global_mouse_click_passthrough),
            target: h.target,
            name,
        });
    }
    if hosts.is_empty() {
        return Err(err("hosts.toml: no enabled [hosts.*] entries"));
    }
    if let Some(d) = &raw.default_host {
        if !hosts.iter().any(|h| &h.name == d) {
            return Err(err(format!("hosts.toml: default_host \"{d}\" is not an enabled [hosts.*] entry")));
        }
    }
    Ok(MirrorConfig {
        poll_seconds: raw.poll_seconds.unwrap_or(60),
        autostart: raw.autostart.unwrap_or(true),
        default_host: raw.default_host,
        close_remote_on_local_close: raw.close_remote_on_local_close.unwrap_or(true),
        hosts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal() {
        let c = parse_config("[hosts.work]\ntarget = \"work\"\n").unwrap();
        assert_eq!(c.poll_seconds, 60);
        assert!(c.autostart);
        assert_eq!(c.hosts.len(), 1);
        let h = &c.hosts[0];
        assert_eq!(h.name, "work");
        assert_eq!(h.prefix, "work");
        assert_eq!(h.remote_bin, "~/.local/bin/herdr");
        assert!(h.always_control); // default on
        assert!(!h.mouse_click_passthrough);
    }

    #[test]
    fn always_control_global_default_and_per_host_override() {
        // global off, one host overrides back on
        let c = parse_config(
            "always_control = false\n\
             [hosts.a]\ntarget = \"a\"\n\
             [hosts.b]\ntarget = \"b\"\nalways_control = true\n",
        )
        .unwrap();
        let a = c.hosts.iter().find(|h| h.name == "a").unwrap();
        let b = c.hosts.iter().find(|h| h.name == "b").unwrap();
        assert!(!a.always_control); // inherits global off
        assert!(b.always_control); // per-host override on
    }

    #[test]
    fn mouse_click_passthrough_default_and_overrides() {
        let c = parse_config(
            "mouse_click_passthrough = true\n\
             [hosts.a]\ntarget = \"a\"\n\
             [hosts.b]\ntarget = \"b\"\nmouse_click_passthrough = false\n",
        )
        .unwrap();
        let a = c.hosts.iter().find(|h| h.name == "a").unwrap();
        let b = c.hosts.iter().find(|h| h.name == "b").unwrap();

        assert!(a.mouse_click_passthrough);
        assert!(!b.mouse_click_passthrough);
    }

    #[test]
    fn parses_full() {
        let c = parse_config(
            "autostart = false\npoll_seconds = 30\ndefault_host = \"vps\"\n\
             [hosts.vps]\ntarget = \"ssh://niko@203.0.113.7:2222\"\nprefix = \"v\"\n\
             remote_bin = \"/opt/herdr\"\n\
             [hosts.off]\ntarget = \"x\"\nenabled = false\n",
        )
        .unwrap();
        assert!(!c.autostart);
        assert_eq!(c.poll_seconds, 30);
        assert_eq!(c.hosts.len(), 1);
        assert_eq!(c.hosts[0].prefix, "v");
        assert_eq!(c.default_host().unwrap().name, "vps");
    }

    #[test]
    fn default_host_must_exist() {
        assert!(parse_config("default_host = \"nope\"\n[hosts.work]\ntarget = \"w\"\n").is_err());
        // unset default_host falls back to the first host declared
        let c = parse_config("[hosts.zeta]\ntarget = \"z\"\n[hosts.alpha]\ntarget = \"a\"\n").unwrap();
        assert_eq!(c.default_host().unwrap().name, "zeta");
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_config("").is_err());
    }

    /// The first host is the remote-create fallback, so declaration order
    /// must survive parsing (a sorted map would put alpha first).
    #[test]
    fn preserves_declaration_order() {
        let c = parse_config("[hosts.zeta]\ntarget = \"z\"\n[hosts.alpha]\ntarget = \"a\"\n").unwrap();
        assert_eq!(c.hosts[0].name, "zeta");
        assert_eq!(c.hosts[1].name, "alpha");
    }
}
