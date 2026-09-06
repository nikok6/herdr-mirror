// hosts.toml loader. Real TOML via the `toml` crate (the TS version hand-rolled
// a subset only because it had to stay dependency-free).

use std::path::PathBuf;

use serde::Deserialize;

use crate::util::{err, Result};

/// Shell expression for `exec <expr> <command> ...` on the remote.
///
/// A configured path is used as-is (unquoted so remote-shell `~` expands).
/// When unset, `herdr` is resolved via PATH, falling back to
/// `~/.local/bin/herdr` if `command -v` finds nothing. A configured session is
/// added as Herdr's global `--session` option so every remote command selects
/// the same server.
pub fn remote_herdr_expr(remote_bin: Option<&str>, session: Option<&str>) -> String {
    let bin = match remote_bin {
        Some(b) if !b.is_empty() => b.to_string(),
        // The `$(...)` substitution must run under a POSIX sh, never the remote
        // login shell: `ssh host cmd` hands the string to that shell, and fish
        // rejects `$(...)` (in command position always; everywhere before fish
        // 3.4), as does csh. The login shell only has to parse `sh -c
        // '<literal>' herdr` plus the caller's trailing words, which every
        // shell handles alike; the args land in `"$@"`. Quotes around the
        // substitution prevent word-splitting if the resolved path contains
        // spaces; ~ still expands inside the unquoted `echo` arg.
        _ => "sh -c 'exec \"$(command -v herdr 2>/dev/null || echo ~/.local/bin/herdr)\" \"$@\"' herdr".into(),
    };
    match session {
        Some(session) => format!("{bin} --session {}", shell_quote(session)),
        None => bin,
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

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

/// How an ssh host's API socket is reached. Meaningless for docker hosts,
/// which always bridge through `docker exec` (see docker.rs) — present on
/// every host regardless of kind for the same reason `docker_bin` is present
/// on ssh hosts: a field that is a no-op for the other kind is simpler than
/// rejecting it.
///
/// `Auto` (the default) is what most hosts want: try the streamlocal `-L`
/// socket forward first, since it is one process cheaper per connection, and
/// fall back to an exec relay only if that turns out not to work. Some ssh
/// servers — notably embedded Go sshds fronting container/VM workspaces —
/// accept a direct-streamlocal channel open but never service it, which
/// without a fallback looks like the remote herdr hanging rather than what it
/// actually is: the transport silently going nowhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiTransport {
    Auto,
    Socket,
    Exec,
}

impl ApiTransport {
    fn parse(s: &str) -> Option<ApiTransport> {
        match s {
            "auto" => Some(ApiTransport::Auto),
            "socket" => Some(ApiTransport::Socket),
            "exec" => Some(ApiTransport::Exec),
            _ => None,
        }
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
    /// Remote herdr binary. `None` = auto-resolve on the remote: PATH first
    /// (`command -v herdr`), then `~/.local/bin/herdr`. See `remote_herdr_expr`.
    pub remote_bin: Option<String>,
    /// Herdr session name on the remote. `None` selects the default session.
    pub session: Option<String>,
    /// ssh hosts only; see `ApiTransport`. Default `Auto`.
    pub api_transport: ApiTransport,
    /// keep each mirror pane in control (writable, no idle release, and sized to
    /// the local pane so it fills). Default on; ideal for headless remotes. Turn
    /// off per host for a remote a human is actively using directly.
    pub always_control: bool,
    /// Cap the size control asks the remote for. `None` = uncapped: fill the
    /// local pane, which is right for a headless remote nobody looks at.
    /// Control is authoritative on the remote, so on a host with its own
    /// display an uncapped wide local window reflows a screen someone is
    /// reading; capping renders the remote at its own width and leaves the rest
    /// of the local pane blank instead. Observe is unaffected either way.
    pub max_cols: Option<usize>,
    pub max_rows: Option<usize>,
    /// ssh hosts only: how many pane streams may share one ssh ControlMaster
    /// connection. 1 (the default; absence resolves here too) is today's
    /// one-direct-connection-per-pane behavior. Docker always execs a fresh
    /// `docker exec` per stream with no multiplexing, so this is a no-op
    /// there — same reasoning as `api_transport` being ssh-only.
    pub ssh_streams_per_connection: usize,
}

#[derive(Debug, Clone)]
pub struct MirrorConfig {
    pub poll_seconds: u64,
    /// let the workspace.focused hook start the daemon
    pub autostart: bool,
    /// host that remote-create actions target when invoked outside a mirror
    /// (falls back to the first host declared)
    pub default_host: Option<String>,
    /// when true (the default), closing a mirror workspace/pane/tab locally
    /// also closes the matching object on the remote. Set false to make a local
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
    max_cols: Option<usize>,
    max_rows: Option<usize>,
    ssh_streams_per_connection: Option<usize>,
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
    session: Option<String>,
    enabled: Option<bool>,
    always_control: Option<bool>,
    max_cols: Option<usize>,
    max_rows: Option<usize>,
    api_transport: Option<String>,
    ssh_streams_per_connection: Option<usize>,
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
            let target = h
                .target
                .clone()
                .ok_or_else(|| bad("missing target".into()))?;
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
        other => Err(bad(format!(
            "unknown kind \"{other}\" (expected ssh or docker)"
        ))),
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
    let found: Vec<PathBuf> = candidates
        .iter()
        .map(|d| d.join("hosts.toml"))
        .filter(|f| f.is_file())
        .collect();
    let Some(file) = found.first() else {
        let searched = candidates
            .iter()
            .map(|d| format!("  {}", d.join("hosts.toml").display()));
        return Err(err(format!(
            "no hosts.toml found — searched:\n{}\n\ncreate one with:\n\n[hosts.<name>]\ntarget = \"<ssh target>\"\n",
            searched.collect::<Vec<_>>().join("\n")
        )));
    };
    let text =
        std::fs::read_to_string(file).map_err(|e| err(format!("{}: {e}", file.display())))?;
    let mut config = parse_config(&text).map_err(|e| err(format!("{}: {e}", file.display())))?;
    config.source = Some(file.clone());
    config.shadowed = found[1..].to_vec();
    Ok(config)
}

/// Catches a real 64-bit hash collision between two DIFFERENT unsafe host
/// names (astronomically unlikely, see util.rs, but silent unless checked).
/// A safe host name never needs this check: two different safe names are
/// never equal by construction, and a safe name's root-level path can never
/// alias an unsafe name's hashed path under `util::unsafe_host_artifacts_dir`
/// regardless of what either looks like — they're in different directories.
/// Every unsafe-host-derived path — state map, hidden marker, host lock,
/// atomic temp file, control-plane socket — comes from this exact stem, so
/// a collision here would otherwise surface at runtime as one host's state
/// silently clobbering another's, far harder to diagnose than rejecting the
/// config up front with both names in the error.
fn validate_unique_host_stems(hosts: &[HostConfig]) -> Result<()> {
    validate_unique_stems(
        "internal path stem",
        hosts
            .iter()
            .filter(|h| !crate::util::is_safe_path_component(&h.name))
            .map(|h| (h.name.as_str(), crate::util::unsafe_host_stem(&h.name))),
    )
}

/// Same idea, for the stream-pool socket namespace (`remote::stream_control_path`)
/// rather than the state/control-plane one: two DIFFERENT (host, target)
/// pairs whose `util::stream_namespace_hash` collides would otherwise let
/// one host's pooled pane streams attach to another's master. Checked once
/// per pooled host (`N > 1`), not once per slot: `ssh_streams_per_connection`
/// is a per-slot pane-count *capacity*, not a slot count — the actual
/// number of slots a host uses depends on how many panes are open and can
/// exceed N (see `stream_pool`) — so there is no finite "every slot" to
/// enumerate, and the namespace hash never includes slot anyway (see
/// `stream_control_path`), only host+target. Docker hosts (and any host
/// with pooling off) never open a stream-pool socket at all, so they have
/// nothing to check. O(hosts), regardless of how large N is configured.
fn validate_unique_stream_namespaces(hosts: &[HostConfig]) -> Result<()> {
    validate_unique_stems(
        "stream-pool namespace",
        hosts
            .iter()
            .filter(|h| !h.kind.is_docker() && h.ssh_streams_per_connection > 1)
            .map(|h| {
                (
                    h.name.as_str(),
                    crate::util::stream_namespace_hash(&h.name, &h.target),
                )
            }),
    )
}

/// The one validation that actually proves two configured hosts won't share
/// a live ControlMaster: `util::unsafe_host_stem`/`stream_namespace_hash`
/// catch a real hash collision within their OWN category, but
/// `remote::control_socket_base`'s v0.4.1-compatible path for a SAFE-but-
/// overlong host name deliberately stays on the unprefixed,
/// budget-truncated legacy hash (see its doc comment) — so a generated stem
/// like `remote-development-environment-w-<hash>` can equal a second,
/// entirely unrelated host's short verbatim name. Whether that actually
/// happens depends on the real `state_dir` (which sets the truncation
/// budget), not knowable at config PARSE time, which is why this takes it
/// as a parameter and runs separately from `parse_config` — see
/// `load_config_for_env`, the one place both are available together. Every
/// host, docker included, gets a control-plane socket via
/// `remote::control_path`, so none are excluded here (unlike the
/// stream-namespace check above, which only ssh hosts ever open).
fn validate_actual_control_paths(hosts: &[HostConfig], state_dir: &std::path::Path) -> Result<()> {
    validate_unique_stems(
        "control-plane socket path",
        hosts.iter().map(|h| {
            let path = crate::remote::control_path(state_dir, &h.name);
            (h.name.as_str(), path.display().to_string())
        }),
    )
}

/// The duplicate-detection logic itself, given each entry's display name
/// and already-derived stem: reject the first repeat, naming both entries
/// and (for `validate_actual_control_paths`) the actual colliding path.
/// Taking precomputed `(name, stem)` pairs rather than a stem function
/// keeps this one implementation shared by every caller above, which
/// otherwise differ in what a "stem" even is (a host name alone, a
/// host+target pair, or a full runtime path). A real hash collision isn't
/// practical to construct in a test, so tests exercise this with
/// deliberately colliding stems instead, which is what actually proves this
/// finds and reports a collision rather than the hash happening not to have
/// one.
fn validate_unique_stems<'a>(
    what: &str,
    entries: impl Iterator<Item = (&'a str, String)>,
) -> Result<()> {
    let mut stems: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
    for (name, stem) in entries {
        if let Some(&other) = stems.get(stem.as_str()) {
            return Err(err(format!(
                "hosts.toml: \"{other}\" and \"{name}\" both derive the same {what} \"{stem}\" \
                 — rename one of them"
            )));
        }
        stems.insert(stem, name);
    }
    Ok(())
}

/// The narrowest boundary where a config and the state_dir it will actually
/// run against are both known — every call path that will go on to touch a
/// host (daemon start, converge, `once`, hide/show, ...) must load its
/// config through here rather than `load_config` directly, or a host whose
/// legacy-hashed control-plane stem happens to equal another host's
/// literal name would only be caught the first time something actually
/// tries to use the colliding socket.
pub fn load_config_for_env(env: &crate::util::Env) -> Result<MirrorConfig> {
    let config = load_config(&env.config_search)?;
    validate_actual_control_paths(&config.hosts, &env.state_dir)?;
    for host in &config.hosts {
        crate::remote::validate_socket_path_budget(&env.state_dir, host)?;
    }
    Ok(config)
}

pub fn parse_config(text: &str) -> Result<MirrorConfig> {
    let raw: RawConfig = toml::from_str(text)?;
    let global_always_control = raw.always_control.unwrap_or(true);
    // 0 is treated as unset rather than "clamp to nothing", same as an empty
    // remote_bin: a cap that would starve the remote of every column is a typo,
    // not an instruction. Warn rather than dropping it silently — and say that
    // it falls through, since 0 is NOT a way to un-cap one host under a global.
    let mut warnings: Vec<String> = Vec::new();
    let size_cap = |v: Option<usize>| v.filter(|&n| n > 0);
    for (key, v) in [("max_cols", raw.max_cols), ("max_rows", raw.max_rows)] {
        if v == Some(0) {
            warnings.push(format!(
                "{key} = 0 ignored: 0 means unset, not \"cap to nothing\""
            ));
        }
    }
    let global_max_cols = size_cap(raw.max_cols);
    let global_max_rows = size_cap(raw.max_rows);
    // Same "0 is a typo, not an instruction" reasoning as the size caps: a
    // pool of 0 connections can't stream anything, so it is treated as unset
    // (falls back to 1, direct-per-pane) rather than silently disabling
    // streaming.
    if raw.ssh_streams_per_connection == Some(0) {
        warnings.push(
            "ssh_streams_per_connection = 0 ignored: 0 means unset, not \"no streams\"".into(),
        );
    }
    let global_streams_per_connection = size_cap(raw.ssh_streams_per_connection).unwrap_or(1);
    let mut hosts: Vec<HostConfig> = Vec::new();
    for (name, value) in raw.hosts {
        let h: RawHost = value
            .try_into()
            .map_err(|e| err(format!("[hosts.{name}]: {e}")))?;
        if h.enabled == Some(false) {
            continue;
        }
        for (key, v) in [("max_cols", h.max_cols), ("max_rows", h.max_rows)] {
            if v == Some(0) {
                warnings.push(format!(
                    "[hosts.{name}]: {key} = 0 ignored; it falls through to any global cap \
                     rather than clearing it"
                ));
            }
        }
        if h.ssh_streams_per_connection == Some(0) {
            warnings.push(format!(
                "[hosts.{name}]: ssh_streams_per_connection = 0 ignored; it falls through to \
                 any global value rather than clearing it"
            ));
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
        let api_transport = match h.api_transport.as_deref() {
            None => ApiTransport::Auto,
            Some(s) => match ApiTransport::parse(s) {
                Some(t) => t,
                None => {
                    warnings.push(format!(
                        "skipping host: [hosts.{name}]: unknown api_transport \"{s}\" \
                         (expected auto, socket, or exec)"
                    ));
                    continue;
                }
            },
        };
        hosts.push(HostConfig {
            prefix: h.prefix.unwrap_or_else(|| name.clone()),
            // empty string is treated as unset (auto PATH → ~/.local/bin/herdr)
            remote_bin: h.remote_bin.filter(|s| !s.is_empty()),
            session: h.session.filter(|s| !s.is_empty()),
            always_control: h.always_control.unwrap_or(global_always_control),
            max_cols: size_cap(h.max_cols).or(global_max_cols),
            max_rows: size_cap(h.max_rows).or(global_max_rows),
            ssh_streams_per_connection: size_cap(h.ssh_streams_per_connection)
                .unwrap_or(global_streams_per_connection),
            docker_bin: h.docker_bin.unwrap_or_else(|| "docker".into()),
            api_transport,
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
            format!(
                "hosts.toml: no usable [hosts.*] entries\n{}",
                warnings.join("\n")
            )
        }));
    }
    if let Some(d) = &raw.default_host {
        if !hosts.iter().any(|h| &h.name == d) {
            return Err(err(format!(
                "hosts.toml: default_host \"{d}\" is not an enabled [hosts.*] entry"
            )));
        }
    }
    validate_unique_host_stems(&hosts)?;
    validate_unique_stream_namespaces(&hosts)?;
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
        assert_eq!(h.remote_bin, None); // auto: PATH then ~/.local/bin/herdr
        assert_eq!(h.session, None); // default remote session
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
    fn size_caps_default_off_and_override_per_host() {
        // nothing set anywhere: uncapped, i.e. today's fill-the-pane behaviour
        let c = parse_config("[hosts.a]\ntarget = \"a\"\n").unwrap();
        assert_eq!(c.hosts[0].max_cols, None);
        assert_eq!(c.hosts[0].max_rows, None);

        // global cap, one host narrowing it further
        let c = parse_config(
            "max_cols = 200\n\
             [hosts.a]\ntarget = \"a\"\n\
             [hosts.b]\ntarget = \"b\"\nmax_cols = 120\nmax_rows = 40\n",
        )
        .unwrap();
        let a = c.hosts.iter().find(|h| h.name == "a").unwrap();
        let b = c.hosts.iter().find(|h| h.name == "b").unwrap();
        assert_eq!(a.max_cols, Some(200)); // inherits the global cap
        assert_eq!(a.max_rows, None); // rows were never capped
        assert_eq!(b.max_cols, Some(120)); // per-host override
        assert_eq!(b.max_rows, Some(40));
    }

    /// A cap of 0 would starve the remote of every column. Treat it as unset,
    /// the same way an empty remote_bin means "auto" rather than "no binary".
    #[test]
    fn a_zero_cap_is_unset_not_a_clamp_to_nothing() {
        let c = parse_config("[hosts.a]\ntarget = \"a\"\nmax_cols = 0\nmax_rows = 0\n").unwrap();
        assert_eq!(c.hosts[0].max_cols, None);
        assert_eq!(c.hosts[0].max_rows, None);

        // and a zeroed per-host value falls back to the global, not to the zero
        let c = parse_config("max_cols = 200\n[hosts.a]\ntarget = \"a\"\nmax_cols = 0\n").unwrap();
        assert_eq!(c.hosts[0].max_cols, Some(200));
    }

    #[test]
    fn parses_full() {
        let c = parse_config(
            "autostart = false\npoll_seconds = 30\ndefault_host = \"vps\"\n\
             [hosts.vps]\ntarget = \"ssh://niko@203.0.113.7:2222\"\nprefix = \"v\"\n\
             remote_bin = \"/opt/herdr\"\n\
             session = \"work\"\n\
             [hosts.off]\ntarget = \"x\"\nenabled = false\n",
        )
        .unwrap();
        assert!(!c.autostart);
        assert_eq!(c.poll_seconds, 30);
        assert_eq!(c.hosts.len(), 1);
        assert_eq!(c.hosts[0].prefix, "v");
        assert_eq!(c.hosts[0].remote_bin.as_deref(), Some("/opt/herdr"));
        assert_eq!(c.hosts[0].session.as_deref(), Some("work"));
        assert_eq!(c.default_host().unwrap().name, "vps");
    }

    #[test]
    fn default_host_must_exist() {
        assert!(parse_config("default_host = \"nope\"\n[hosts.work]\ntarget = \"w\"\n").is_err());
        // unset default_host falls back to the first host declared
        let c =
            parse_config("[hosts.zeta]\ntarget = \"z\"\n[hosts.alpha]\ntarget = \"a\"\n").unwrap();
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
        let c =
            parse_config("[hosts.zeta]\ntarget = \"z\"\n[hosts.alpha]\ntarget = \"a\"\n").unwrap();
        assert_eq!(c.hosts[0].name, "zeta");
        assert_eq!(c.hosts[1].name, "alpha");
    }

    /// Every pre-container hosts.toml must parse exactly as before.
    #[test]
    fn existing_ssh_configs_are_unchanged() {
        let c = parse_config("[hosts.work]\ntarget = \"work\"\n").unwrap();
        assert_eq!(c.hosts[0].kind, HostKind::Ssh);
        assert_eq!(c.hosts[0].target, "work");
        assert_eq!(c.hosts[0].remote_bin, None);
        assert_eq!(c.hosts[0].session, None);
    }

    #[test]
    fn remote_herdr_expr_configured_vs_auto_and_session() {
        assert_eq!(remote_herdr_expr(Some("/opt/herdr"), None), "/opt/herdr");
        assert_eq!(
            remote_herdr_expr(Some("~/.local/bin/herdr"), Some("work")),
            "~/.local/bin/herdr --session 'work'"
        );
        let auto = "sh -c 'exec \"$(command -v herdr 2>/dev/null || echo ~/.local/bin/herdr)\" \"$@\"' herdr";
        assert_eq!(remote_herdr_expr(None, None), auto);
        assert_eq!(remote_herdr_expr(Some(""), None), auto);
        assert_eq!(
            remote_herdr_expr(None, Some("team's")),
            format!("{auto} --session 'team'\\''s'")
        );
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
        assert_eq!(
            tok.target, "/Users/n/proj",
            "display target falls back to the ref"
        );
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
    fn api_transport_defaults_to_auto_and_parses_overrides() {
        let c = parse_config("[hosts.a]\ntarget = \"a\"\n").unwrap();
        assert_eq!(c.hosts[0].api_transport, ApiTransport::Auto);

        let c = parse_config("[hosts.a]\ntarget = \"a\"\napi_transport = \"socket\"\n").unwrap();
        assert_eq!(c.hosts[0].api_transport, ApiTransport::Socket);

        let c = parse_config("[hosts.a]\ntarget = \"a\"\napi_transport = \"exec\"\n").unwrap();
        assert_eq!(c.hosts[0].api_transport, ApiTransport::Exec);
    }

    /// An unknown value must be as loud as any other malformed host: skipped
    /// with a named reason, not silently coerced to a default.
    #[test]
    fn unknown_api_transport_is_skipped_with_reason() {
        let c = parse_config(
            "[hosts.good]\ntarget = \"g\"\n\
             [hosts.bad]\ntarget = \"b\"\napi_transport = \"turbo\"\n",
        )
        .unwrap();
        assert_eq!(c.hosts.len(), 1);
        assert_eq!(c.hosts[0].name, "good");
        assert!(
            c.warnings[0].contains("unknown api_transport"),
            "{:?}",
            c.warnings
        );
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

    #[test]
    fn ssh_streams_per_connection_defaults_to_one() {
        let c = parse_config("[hosts.a]\ntarget = \"a\"\n").unwrap();
        assert_eq!(c.hosts[0].ssh_streams_per_connection, 1);
    }

    #[test]
    fn ssh_streams_per_connection_global_default_and_per_host_override() {
        let c = parse_config(
            "ssh_streams_per_connection = 4\n\
             [hosts.a]\ntarget = \"a\"\n\
             [hosts.b]\ntarget = \"b\"\nssh_streams_per_connection = 8\n",
        )
        .unwrap();
        let a = c.hosts.iter().find(|h| h.name == "a").unwrap();
        let b = c.hosts.iter().find(|h| h.name == "b").unwrap();
        assert_eq!(a.ssh_streams_per_connection, 4, "inherits the global value");
        assert_eq!(b.ssh_streams_per_connection, 8, "per-host override");
    }

    /// A pool of 0 connections can't stream anything — same "typo, not an
    /// instruction" reasoning as a 0 size cap — so it is unset (falls back to
    /// 1) rather than silently disabling every stream.
    #[test]
    fn ssh_streams_per_connection_zero_warns_and_falls_through() {
        let c =
            parse_config("ssh_streams_per_connection = 0\n[hosts.a]\ntarget = \"a\"\n").unwrap();
        assert_eq!(c.hosts[0].ssh_streams_per_connection, 1);
        assert!(
            c.warnings
                .iter()
                .any(|w| w.contains("ssh_streams_per_connection = 0 ignored")),
            "{:?}",
            c.warnings
        );

        let c = parse_config(
            "ssh_streams_per_connection = 4\n[hosts.a]\ntarget = \"a\"\nssh_streams_per_connection = 0\n",
        )
        .unwrap();
        assert_eq!(
            c.hosts[0].ssh_streams_per_connection, 4,
            "0 falls through to the global"
        );
        assert!(
            c.warnings
                .iter()
                .any(|w| w.contains("falls through to any global value")),
            "{:?}",
            c.warnings
        );
    }

    /// Meaningless for docker (no ssh ControlMaster to share), but accepted
    /// rather than rejected — same reasoning as `docker_bin` being a no-op on
    /// ssh hosts: a global setting easily reaches a mixed fleet, and a field
    /// that does nothing for one kind is simpler than a kind-specific reject.
    #[test]
    fn ssh_streams_per_connection_parses_on_docker_hosts_as_a_no_op() {
        let c = parse_config(
            "ssh_streams_per_connection = 4\n\
             [hosts.a]\nkind = \"docker\"\ncontainer = \"c\"\n",
        )
        .unwrap();
        assert_eq!(c.hosts[0].ssh_streams_per_connection, 4);
    }

    /// One malformed host must not take the whole config down with it. The
    /// stricter validation added alongside container support originally
    /// aborted the load, which was worse than the behaviour it replaced: a
    /// single typo stopped every *other* host from mirroring.
    #[test]
    fn a_bad_host_is_skipped_not_fatal() {
        let c = parse_config("[hosts.good]\ntarget = \"vps\"\n[hosts.bad]\ntarget = \"\"\n")
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
        let e = parse_config("[hosts.a]\ntarget = \"\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("no usable"), "{e}");
        assert!(
            e.contains("target is empty"),
            "must name the actual reason: {e}"
        );
        // an empty file has no reasons to give, so it keeps the plain message
        let e = parse_config("").unwrap_err().to_string();
        assert!(!e.contains("no usable"), "{e}");
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("herdr-mirror-cfgtest-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_hosts(dir: &Path, name: &str) {
        std::fs::write(
            dir.join("hosts.toml"),
            format!("[hosts.{name}]\ntarget = \"t\"\n"),
        )
        .unwrap();
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
        let e = load_config(&[a.clone(), b.clone()])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains(&a.join("hosts.toml").display().to_string()),
            "{e}"
        );
        assert!(
            e.contains(&b.join("hosts.toml").display().to_string()),
            "{e}"
        );
    }

    #[test]
    fn a_zero_cap_warns_instead_of_vanishing() {
        // silently dropping a typo'd cap leaves the user believing it applied
        let c = parse_config("max_cols = 0\n[hosts.a]\ntarget = \"a\"\n").unwrap();
        assert_eq!(c.hosts[0].max_cols, None);
        assert!(
            c.warnings
                .iter()
                .any(|w| w.contains("max_cols = 0 ignored")),
            "{:?}",
            c.warnings
        );

        // and 0 per host is not a way to opt out of a global cap
        let c = parse_config("max_cols = 200\n[hosts.a]\ntarget = \"a\"\nmax_cols = 0\n").unwrap();
        assert_eq!(
            c.hosts[0].max_cols,
            Some(200),
            "0 falls through to the global"
        );
        assert!(
            c.warnings
                .iter()
                .any(|w| w.contains("falls through to any global cap")),
            "{:?}",
            c.warnings
        );
    }

    /// Real distinct host names must never trip the duplicate-stem check —
    /// this is the false-positive risk of adding it at all, so it gets its
    /// own explicit config-level test beyond the many other host configs
    /// already parsed successfully elsewhere in this module.
    #[test]
    fn distinct_ordinary_host_names_never_collide_on_stem() {
        let c = parse_config(
            "[hosts.azure]\ntarget = \"azure\"\n\
             [hosts.rdev]\ntarget = \"rdev\"\n\
             [hosts.prod-us-east-1]\ntarget = \"t\"\n",
        )
        .unwrap();
        assert_eq!(c.hosts.len(), 3);
    }

    /// The duplicate-stem check's own detection logic, independent of
    /// whether the real 64-bit hash ever actually collides (it shouldn't —
    /// see util.rs): deliberately colliding stems prove this finds and
    /// reports a duplicate rather than the hash simply not having one to
    /// find.
    #[test]
    fn validate_unique_stems_reports_both_names_on_a_forced_collision() {
        let names = ["alpha", "beta", "gamma"];
        let err = validate_unique_stems(
            "stem",
            names.into_iter().map(|n| (n, "same-stem".to_string())),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("alpha"), "{err}");
        assert!(err.contains("beta"), "{err}");
        assert!(err.contains("same-stem"), "{err}");
    }

    #[test]
    fn validate_unique_stems_accepts_genuinely_distinct_stems() {
        let names = ["alpha", "beta", "gamma"];
        assert!(
            validate_unique_stems("stem", names.into_iter().map(|n| (n, n.to_string()))).is_ok()
        );
    }

    /// `validate_unique_host_stems` (the real entry point `parse_config`
    /// calls) must actually be wired to the real `unsafe_host_stem`, not a
    /// stand-in — this is the wiring `validate_unique_stems_*` above can't
    /// cover since it always takes an injected function. Uses unsafe names:
    /// safe ones are filtered out before this check ever runs (see its doc
    /// comment), so a safe-only config would pass trivially either way.
    #[test]
    fn validate_unique_host_stems_uses_the_real_unsafe_host_stem() {
        let hosts = vec![test_host("a/b", "t1"), test_host("c/d", "t2")];
        assert!(validate_unique_host_stems(&hosts).is_ok());
    }

    fn test_host(name: &str, target: &str) -> HostConfig {
        HostConfig {
            name: name.into(),
            prefix: name.into(),
            target: target.into(),
            kind: HostKind::Ssh,
            remote_bin: None,
            session: None,
            always_control: true,
            max_cols: None,
            max_rows: None,
            ssh_streams_per_connection: 1,
            docker_bin: "docker".into(),
            api_transport: ApiTransport::Auto,
        }
    }

    /// A host with pooling actually on (`ssh_streams_per_connection > 1`):
    /// `validate_unique_stream_namespaces` skips any host at the default
    /// `1` (it never opens a stream-pool socket at all), so a test of that
    /// check needs this, not plain `test_host`.
    fn pooled_host(name: &str, target: &str) -> HostConfig {
        let mut h = test_host(name, target);
        h.ssh_streams_per_connection = 4;
        h
    }

    /// `validate_unique_stream_namespaces` (the real entry point
    /// `parse_config` calls) must actually be wired to the real
    /// `stream_namespace_hash`, keyed on host+target, not a stand-in.
    #[test]
    fn validate_unique_stream_namespaces_uses_the_real_namespace_base() {
        let hosts = vec![
            pooled_host("azure", "azure.example.com"),
            pooled_host("rdev", "rdev.example.com"),
        ];
        assert!(validate_unique_stream_namespaces(&hosts).is_ok());
    }

    /// Two hosts sharing a target (a real, supported setup — e.g. the same
    /// machine reached under two names/sessions) must not be flagged: the
    /// namespace hashes host+target together, not target alone.
    #[test]
    fn validate_unique_stream_namespaces_allows_a_shared_target() {
        let hosts = vec![
            pooled_host("a", "shared.example.com"),
            pooled_host("b", "shared.example.com"),
        ];
        assert!(validate_unique_stream_namespaces(&hosts).is_ok());
    }

    /// `ssh_streams_per_connection` is a per-slot pane-count capacity, not a
    /// slot count: an earlier version of this check enumerated `0..N` slots
    /// per host, which was both conceptually wrong (a host can use more
    /// slots than N — see `stream_pool::fresh_panes_fill_the_lowest_slot_first`,
    /// where capacity 2 still reaches slot 2 for 5 panes) and unbounded
    /// (a huge configured N made validation itself slow, and needed a
    /// usize -> u32 cast that could truncate). The namespace hash is
    /// host+target only, so validating a huge N must still be instant.
    #[test]
    fn validate_unique_stream_namespaces_is_o_of_hosts_even_for_a_huge_capacity() {
        let mut huge = pooled_host("huge", "huge.example.com");
        huge.ssh_streams_per_connection = usize::MAX;
        let started = std::time::Instant::now();
        assert!(validate_unique_stream_namespaces(&[huge]).is_ok());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "must not scale with the configured capacity"
        );
    }

    /// Docker hosts never open a stream-pool socket, so they must never be
    /// able to trip this check even if their (name, target) happen to
    /// duplicate something.
    #[test]
    fn validate_unique_stream_namespaces_skips_docker_hosts() {
        let mut docker = pooled_host("d", "same-target");
        docker.kind = HostKind::DockerContainer("c".into());
        let hosts = vec![pooled_host("a", "same-target"), docker];
        assert!(validate_unique_stream_namespaces(&hosts).is_ok());
    }

    /// An ordinary host left at the `ssh_streams_per_connection` default
    /// (pooling off) never opens a stream-pool socket at all, so it must
    /// never be able to trip this check even against an otherwise-colliding
    /// pooled host.
    #[test]
    fn validate_unique_stream_namespaces_skips_unpooled_hosts() {
        let unpooled = test_host("a", "same-target");
        let hosts = vec![unpooled, pooled_host("b", "same-target")];
        assert!(validate_unique_stream_namespaces(&hosts).is_ok());
    }

    /// The exact defect an independent review reported: preserving v0.4.1's
    /// legacy, budget-truncated control-plane stem for a safe-but-overlong
    /// host name means the generated stem (a readable prefix plus an
    /// 8-hex-char hash) can equal a second, distinct host's short verbatim
    /// name. Computed from the real `remote::control_path` rather than a
    /// hardcoded golden so this doesn't depend on matching the review's
    /// exact hash digits — the mechanism, not one specific value, is what's
    /// under test.
    #[test]
    fn validate_actual_control_paths_catches_the_reported_collision() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        let long_name = "remote-development-environment-with-a-very-long-name";
        let generated_stem = crate::remote::control_path(&state_dir, long_name)
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let hosts = vec![
            test_host(long_name, "target"),
            test_host(&generated_stem, "target2"),
        ];
        let err = validate_actual_control_paths(&hosts, &state_dir)
            .unwrap_err()
            .to_string();
        assert!(err.contains(long_name), "{err}");
        assert!(err.contains(&generated_stem), "{err}");
    }

    /// The state-dir-dependent budget is what makes this collision possible
    /// at all: the same pair of names, under a state_dir short enough that
    /// neither one needs truncating, produce two distinct, harmless paths.
    #[test]
    fn validate_actual_control_paths_depends_on_the_real_budget() {
        let short_state_dir = PathBuf::from("/s");
        let long_name = "remote-development-environment-with-a-very-long-name";
        let generated_stem_for_a_deep_dir = crate::remote::control_path(
            &PathBuf::from("/Users/example/.local/state/herdr-mirror"),
            long_name,
        )
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

        let hosts = vec![
            test_host(long_name, "target"),
            test_host(&generated_stem_for_a_deep_dir, "target2"),
        ];
        assert!(validate_actual_control_paths(&hosts, &short_state_dir).is_ok());
    }

    /// Two unrelated, ordinary host names must never be flagged — the
    /// false-positive risk of adding path validation at all.
    #[test]
    fn validate_actual_control_paths_accepts_genuinely_distinct_hosts() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        let hosts = vec![test_host("azure", "t"), test_host("rdev", "t2")];
        assert!(validate_actual_control_paths(&hosts, &state_dir).is_ok());
    }

    /// A real 32-bit FNV-1a collision between two short, plain host names
    /// (found by brute-force search, not synthetic) must NOT be rejected:
    /// both are short enough to be used verbatim under any realistic
    /// state_dir, so their control-plane paths never actually collide —
    /// exactly the false positive the old, budget-unaware
    /// `validate_no_legacy_socket_hash_collisions` check could not avoid,
    /// which is why it was replaced by this one instead of kept alongside
    /// it.
    #[test]
    fn validate_actual_control_paths_does_not_falsely_flag_a_harmless_hash_collision() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        let hosts = vec![test_host("host10579", "t"), test_host("host343082", "t2")];
        assert!(validate_actual_control_paths(&hosts, &state_dir).is_ok());
    }

    /// Every configured host, docker included, gets a control-plane socket
    /// (`RemoteHost::new` calls `socket_stem` regardless of kind) — unlike
    /// the stream-pool namespace check, this one must not skip docker
    /// hosts.
    #[test]
    fn validate_actual_control_paths_covers_docker_hosts_too() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        let mut docker = test_host("dup-name", "t2");
        docker.kind = HostKind::DockerContainer("c".into());
        let hosts = vec![test_host("dup-name", "t"), docker];
        assert!(validate_actual_control_paths(&hosts, &state_dir).is_err());
    }

    /// `load_config_for_env` is the actual entry point every real call path
    /// must use: this proves an on-disk config with a colliding pair is
    /// rejected before anything could open an ssh process, using the real
    /// file-loading path (`load_config`) plus the real state_dir check
    /// together, not either one in isolation.
    #[test]
    fn load_config_for_env_rejects_a_config_with_colliding_control_paths_before_any_host_operation()
    {
        let config_dir = tmpdir("collide-load");
        std::fs::create_dir_all(&config_dir).unwrap();
        // A fixed, realistic path budget for the collision itself — distinct
        // from `config_dir` (a real, writable location `hosts.toml` needs to
        // live in), which the test harness's own temp path depth would
        // otherwise make unpredictably short.
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        let long_name = "remote-development-environment-with-a-very-long-name";
        let generated_stem = crate::remote::control_path(&state_dir, long_name)
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        std::fs::write(
            config_dir.join("hosts.toml"),
            format!(
                "[hosts.{long_name}]\ntarget = \"t1\"\n[hosts.{generated_stem}]\ntarget = \"t2\"\n"
            ),
        )
        .unwrap();

        let env = crate::util::Env {
            config_search: vec![config_dir.clone()],
            state_dir,
            local_socket: config_dir.join("local.sock"),
        };
        let err = load_config_for_env(&env).unwrap_err().to_string();
        assert!(err.contains(long_name), "{err}");
        let _ = std::fs::remove_dir_all(&config_dir);
    }

    /// The other half of the same proof, through the identical real
    /// `load_config_for_env` call path: two short, plain host names that
    /// share a 32-bit legacy hash (found by brute-force search, not
    /// synthetic — see `remote::legacy_socket_hash_has_a_known_real_collision`)
    /// must load successfully under a normal state_dir, since their actual
    /// control-plane paths never collide (both are short enough to be used
    /// verbatim — see `remote::socket_stem`). This is the exact false
    /// positive the removed, budget-unaware `validate_no_legacy_socket_hash_collisions`
    /// check could not avoid; nothing in `parse_config` or
    /// `load_config_for_env` may reject this pair.
    #[test]
    fn load_config_for_env_accepts_a_harmless_legacy_hash_collision_before_any_host_operation() {
        let config_dir = tmpdir("no-false-positive-load");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("hosts.toml"),
            "[hosts.host10579]\ntarget = \"t1\"\n[hosts.host343082]\ntarget = \"t2\"\n",
        )
        .unwrap();

        let env = crate::util::Env {
            config_search: vec![config_dir.clone()],
            state_dir: PathBuf::from("/Users/example/.local/state/herdr-mirror"),
            local_socket: config_dir.join("local.sock"),
        };
        let config = load_config_for_env(&env).unwrap();
        assert_eq!(config.hosts.len(), 2);
        let _ = std::fs::remove_dir_all(&config_dir);
    }

    /// The call-path proof for the socket-path-budget check: a `state_dir`
    /// too deep for even a short host's live ControlPath must be rejected
    /// by `load_config_for_env` itself — before any host connection is
    /// attempted — not left to fail later inside `ssh`. See
    /// `remote::validate_socket_path_budget_rejects_a_state_dir_too_deep_for_a_live_control_path`
    /// for the underlying arithmetic; this proves it is actually wired into
    /// the one function every real call path loads config through.
    #[test]
    fn load_config_for_env_rejects_a_state_dir_too_deep_for_any_live_control_path() {
        let config_dir = tmpdir("budget-load");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("hosts.toml"),
            "[hosts.prod]\ntarget = \"t1\"\n",
        )
        .unwrap();

        let env = crate::util::Env {
            config_search: vec![config_dir.clone()],
            state_dir: PathBuf::from(format!("/{}", "a".repeat(89))),
            local_socket: config_dir.join("local.sock"),
        };
        let err = load_config_for_env(&env).unwrap_err().to_string();
        assert!(err.contains("prod"), "{err}");
        let _ = std::fs::remove_dir_all(&config_dir);
    }
}
