// hosts.toml loader. Real TOML via the `toml` crate (the TS version hand-rolled
// a subset only because it had to stay dependency-free).

use std::path::PathBuf;

use serde::Deserialize;

use crate::util::{err, Result};

/// How to reach a host. `Ssh` is the default and the only kind that existed
/// before container support; every existing hosts.toml parses to it.
#[derive(Debug, Clone, PartialEq)]
pub enum HostKind {
    Ssh,
    /// container named explicitly (brittle: docker regenerates devcontainer
    /// names on rebuild)
    DockerContainer(String),
    /// container resolved by `devcontainer.local_folder` label, which survives
    /// rebuilds where the name does not
    DockerFolder(String),
}

impl HostKind {
    pub fn is_docker(&self) -> bool {
        !matches!(self, HostKind::Ssh)
    }
}

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub name: String,
    /// ssh target for ssh hosts; for docker hosts a display-only ref (the
    /// container name or folder) — the connection details live in `kind`
    pub target: String,
    pub kind: HostKind,
    pub docker_bin: String,
    pub prefix: String,
    pub remote_bin: String,
    /// keep each mirror pane in control (writable, no idle release, and sized to
    /// the local pane so it fills). Default on; ideal for headless remotes. Turn
    /// off per host for a remote a human is actively using directly.
    pub always_control: bool,
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
    /// which hosts.toml this came from. `None` when parsed from a string
    /// (tests). Logged at startup so "which config won?" is never a guess.
    pub source: Option<PathBuf>,
    /// other candidate dirs that also hold a hosts.toml and are therefore
    /// being ignored — a silent-shadowing trap worth warning about.
    pub shadowed: Vec<PathBuf>,
    /// hosts that failed validation and were skipped. Surfaced at startup and
    /// in `status` rather than aborting the load: one malformed entry must not
    /// stop every *other* host from mirroring.
    pub warnings: Vec<String>,
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
    // toml::Table (preserve_order) keeps declaration order — the first host
    // is the remote-create fallback, so order is user-visible
    #[serde(default)]
    hosts: toml::Table,
}

#[derive(Deserialize)]
struct RawHost {
    /// required for ssh hosts, meaningless for docker ones
    target: Option<String>,
    kind: Option<String>,
    container: Option<String>,
    folder: Option<String>,
    docker_bin: Option<String>,
    prefix: Option<String>,
    remote_bin: Option<String>,
    enabled: Option<bool>,
    always_control: Option<bool>,
}

/// Resolve `kind` + its ref fields, rejecting combinations that would silently
/// do the wrong thing. Returns the kind and the display target.
fn resolve_kind(name: &str, h: &RawHost) -> Result<(HostKind, String)> {
    let bad = |m: String| err(format!("[hosts.{name}]: {m}"));
    // An empty ref is worse than a missing one: `name=^$` and an empty label
    // value match nothing, so the host reports dormant forever and a typo (or a
    // template variable that never expanded) is indistinguishable from a
    // stopped container.
    let nonempty = |field: &str, v: &str| -> Result<String> {
        match v.trim() {
            "" => Err(bad(format!("{field} is empty"))),
            s => Ok(s.to_string()),
        }
    };
    match h.kind.as_deref().unwrap_or("ssh") {
        "ssh" => {
            if h.container.is_some() || h.folder.is_some() {
                return Err(bad("container/folder need kind = \"docker\"".into()));
            }
            let target = h.target.clone().ok_or_else(|| bad("missing target".into()))?;
            Ok((HostKind::Ssh, nonempty("target", &target)?))
        }
        "docker" => {
            // the ssh arm rejects the mirror-image mistake, so silently
            // discarding target here would be an inconsistent trap
            if h.target.is_some() {
                return Err(bad("target has no meaning with kind = \"docker\" \
                                (use container or folder)"
                    .into()));
            }
            match (&h.container, &h.folder) {
                (Some(_), Some(_)) => Err(bad("set container or folder, not both".into())),
                (None, None) => Err(bad("kind = \"docker\" needs container or folder".into())),
                (Some(c), None) => {
                    let c = nonempty("container", c)?;
                    Ok((HostKind::DockerContainer(c.clone()), c))
                }
                (None, Some(f)) => {
                    let f = nonempty("folder", f)?;
                    Ok((HostKind::DockerFolder(f.clone()), f))
                }
            }
        }
        other => Err(bad(format!("unknown kind \"{other}\" (expected ssh or docker)"))),
    }
}

/// Load the first `hosts.toml` found across `candidates`, in order.
///
/// The search is deliberately env-independent. Plugin actions run with
/// `HERDR_PLUGIN_CONFIG_DIR` injected and shell invocations run without it, so
/// resolution that *branches* on that variable makes the same config file
/// visible to `herdr-mirror` as a plugin action and invisible to the identical
/// command typed in a terminal. Searching every candidate either way keeps the
/// two modes in agreement (see `util::config_candidates`).
pub fn load_config(candidates: &[PathBuf]) -> Result<MirrorConfig> {
    let found: Vec<PathBuf> =
        candidates.iter().map(|d| d.join("hosts.toml")).filter(|f| f.is_file()).collect();
    let Some(file) = found.first() else {
        let searched =
            candidates.iter().map(|d| format!("  {}", d.join("hosts.toml").display()));
        return Err(err(format!(
            "no hosts.toml found — searched:\n{}\n\ncreate one with:\n\n[hosts.<name>]\ntarget = \"<ssh target>\"\n",
            searched.collect::<Vec<_>>().join("\n")
        )));
    };
    let text = std::fs::read_to_string(file)
        .map_err(|e| err(format!("{}: {e}", file.display())))?;
    let mut config = parse_config(&text).map_err(|e| err(format!("{}: {e}", file.display())))?;
    config.source = Some(file.clone());
    config.shadowed = found[1..].to_vec();
    Ok(config)
}

pub fn parse_config(text: &str) -> Result<MirrorConfig> {
    let raw: RawConfig = toml::from_str(text)?;
    let global_always_control = raw.always_control.unwrap_or(true);
    let mut hosts: Vec<HostConfig> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for (name, value) in raw.hosts {
        let h: RawHost = value.try_into().map_err(|e| err(format!("[hosts.{name}]: {e}")))?;
        if h.enabled == Some(false) {
            continue;
        }
        // Skip-with-warning, not abort. Aborting would let one typo'd entry
        // stop the daemon entirely and take every *other* host's mirrors down
        // with it — strictly worse than the behaviour this validation replaced,
        // where a bad host was simply a broken host. Matches `enabled = false`.
        let (kind, target) = match resolve_kind(&name, &h) {
            Ok(v) => v,
            Err(e) => {
                warnings.push(format!("skipping host: {e}"));
                continue;
            }
        };
        hosts.push(HostConfig {
            prefix: h.prefix.unwrap_or_else(|| name.clone()),
            remote_bin: h.remote_bin.unwrap_or_else(|| "~/.local/bin/herdr".into()),
            always_control: h.always_control.unwrap_or(global_always_control),
            docker_bin: h.docker_bin.unwrap_or_else(|| "docker".into()),
            kind,
            target,
            name,
        });
    }
    if hosts.is_empty() {
        // Carry the skip reasons into the error. Otherwise a config whose only
        // host is malformed reports "no enabled entries", which reads as "you
        // configured nothing" when the truth is "the one you configured was
        // rejected, and here is why".
        return Err(err(if warnings.is_empty() {
            "hosts.toml: no enabled [hosts.*] entries".to_string()
        } else {
            format!("hosts.toml: no usable [hosts.*] entries\n{}", warnings.join("\n"))
        }));
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
        source: None,
        shadowed: Vec::new(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

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

    /// Every pre-container hosts.toml must parse exactly as before.
    #[test]
    fn existing_ssh_configs_are_unchanged() {
        let c = parse_config("[hosts.work]\ntarget = \"work\"\n").unwrap();
        assert_eq!(c.hosts[0].kind, HostKind::Ssh);
        assert_eq!(c.hosts[0].target, "work");
        assert_eq!(c.hosts[0].remote_bin, "~/.local/bin/herdr");
    }

    #[test]
    fn parses_docker_by_folder_and_container() {
        let c = parse_config(
            "[hosts.tok]\nkind = \"docker\"\nfolder = \"/Users/n/proj\"\n\
             [hosts.named]\nkind = \"docker\"\ncontainer = \"crazy_ride\"\n",
        )
        .unwrap();
        let tok = c.hosts.iter().find(|h| h.name == "tok").unwrap();
        assert_eq!(tok.kind, HostKind::DockerFolder("/Users/n/proj".into()));
        assert_eq!(tok.target, "/Users/n/proj", "display target falls back to the ref");
        assert!(tok.kind.is_docker());
        let named = c.hosts.iter().find(|h| h.name == "named").unwrap();
        assert_eq!(named.kind, HostKind::DockerContainer("crazy_ride".into()));
    }

    /// Combinations that would silently do the wrong thing must be rejected
    /// at parse time, not discovered at connect time.
    #[test]
    fn rejects_incoherent_kinds() {
        let cases = [
            // docker with neither ref
            "[hosts.a]\nkind = \"docker\"\n",
            // docker with both refs
            "[hosts.a]\nkind = \"docker\"\ncontainer = \"c\"\nfolder = \"/f\"\n",
            // container/folder on an ssh host
            "[hosts.a]\ntarget = \"t\"\ncontainer = \"c\"\n",
            // ssh without a target
            "[hosts.a]\nprefix = \"p\"\n",
            // unknown kind
            "[hosts.a]\nkind = \"podman\"\ntarget = \"t\"\n",
            // empty refs: these match nothing, so the host would report
            // dormant forever and a typo would look like a stopped container
            "[hosts.a]\nkind = \"docker\"\ncontainer = \"\"\n",
            "[hosts.a]\nkind = \"docker\"\nfolder = \"   \"\n",
            "[hosts.a]\ntarget = \"\"\n",
            // target is meaningless for docker; the mirror-image mistake is
            // rejected, so silently discarding this would be a trap
            "[hosts.a]\nkind = \"docker\"\ncontainer = \"c\"\ntarget = \"1.2.3.4\"\n",
        ];
        for case in cases {
            assert!(parse_config(case).is_err(), "should reject: {case}");
        }
    }

    #[test]
    fn docker_bin_defaults_and_overrides() {
        let c = parse_config("[hosts.a]\nkind = \"docker\"\ncontainer = \"c\"\n").unwrap();
        assert_eq!(c.hosts[0].docker_bin, "docker");
        let c = parse_config(
            "[hosts.a]\nkind = \"docker\"\ncontainer = \"c\"\ndocker_bin = \"/usr/local/bin/docker\"\n",
        )
        .unwrap();
        assert_eq!(c.hosts[0].docker_bin, "/usr/local/bin/docker");
    }

    /// One malformed host must not take the whole config down with it. The
    /// stricter validation added alongside container support originally
    /// aborted the load, which was worse than the behaviour it replaced: a
    /// single typo stopped every *other* host from mirroring.
    #[test]
    fn a_bad_host_is_skipped_not_fatal() {
        let c = parse_config(
            "[hosts.good]\ntarget = \"vps\"\n[hosts.bad]\ntarget = \"\"\n",
        )
        .expect("one bad host must not abort the load");
        assert_eq!(c.hosts.len(), 1);
        assert_eq!(c.hosts[0].name, "good");
        assert_eq!(c.warnings.len(), 1, "the skip must be reported, not silent");
        assert!(c.warnings[0].contains("bad"), "{:?}", c.warnings);
    }

    /// ...but a config where *every* host is invalid is still an error, so a
    /// wholly broken file cannot look like a working empty one — and the error
    /// must say WHY, not just "no entries", which reads as "you configured
    /// nothing" when the user plainly did.
    #[test]
    fn all_hosts_invalid_is_still_an_error() {
        let e = parse_config("[hosts.a]\ntarget = \"\"\n").unwrap_err().to_string();
        assert!(e.contains("no usable"), "{e}");
        assert!(e.contains("target is empty"), "must name the actual reason: {e}");
        // an empty file has no reasons to give, so it keeps the plain message
        let e = parse_config("").unwrap_err().to_string();
        assert!(!e.contains("no usable"), "{e}");
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("herdr-mirror-cfgtest-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_hosts(dir: &Path, name: &str) {
        std::fs::write(dir.join("hosts.toml"), format!("[hosts.{name}]\ntarget = \"t\"\n")).unwrap();
    }

    /// A config in a *later* candidate must still be found. This is the
    /// README-follower case: config lives in the plugin dir, but the command
    /// was typed in a shell so HERDR_PLUGIN_CONFIG_DIR is absent.
    #[test]
    fn finds_config_in_any_candidate() {
        let a = tmpdir("late-a");
        let b = tmpdir("late-b");
        write_hosts(&b, "found");
        let c = load_config(&[a, b.clone()]).unwrap();
        assert_eq!(c.hosts[0].name, "found");
        assert_eq!(c.source.as_deref(), Some(b.join("hosts.toml").as_path()));
    }

    /// Earlier candidates win, and the losers are reported rather than
    /// silently dropped.
    #[test]
    fn earlier_candidate_wins_and_reports_shadowed() {
        let a = tmpdir("shadow-a");
        let b = tmpdir("shadow-b");
        write_hosts(&a, "winner");
        write_hosts(&b, "loser");
        let c = load_config(&[a.clone(), b.clone()]).unwrap();
        assert_eq!(c.hosts[0].name, "winner");
        assert_eq!(c.shadowed, vec![b.join("hosts.toml")]);
    }

    /// The not-found error must name every path searched: naming only one
    /// told users to create a config they had already created elsewhere.
    #[test]
    fn missing_config_error_lists_every_candidate() {
        let a = tmpdir("miss-a");
        let b = tmpdir("miss-b");
        let e = load_config(&[a.clone(), b.clone()]).unwrap_err().to_string();
        assert!(e.contains(&a.join("hosts.toml").display().to_string()), "{e}");
        assert!(e.contains(&b.join("hosts.toml").display().to_string()), "{e}");
    }
}
