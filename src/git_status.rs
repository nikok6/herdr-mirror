// Git status for mirror workspaces, pushed onto the LOCAL mirror rows as
// `$mgit_*` workspace metadata tokens.
//
// Why this exists: herdr derives the sidebar's branch/ahead-behind chip from
// the workspace's LOCAL cwd, and mirror workspaces deliberately sit on a
// non-git marker cwd (see mirror.rs `mirror_pane_cwd`) so they'd otherwise
// show a wrong branch — so they show none. There is no API to feed the
// built-in chip a remote repo's state. Custom tokens are the sanctioned seam:
// since herdr 0.7.5 the spaces sidebar renders `$name` tokens reported per
// workspace (`workspace.report_metadata`), and this daemon already holds an
// exec channel to the remote (`RemoteHost::exec`), so it can measure the
// remote repos itself and report the result locally. Nothing is installed on
// the remote — just `git`.
//
// Design:
//   * ONE batched `git status --porcelain=v1 -b` per host per interval over
//     ssh/docker exec (one round trip, N workspaces), never one call per
//     workspace; `--no-optional-locks` keeps it from contending with the
//     user's own git commands for the index lock.
//   * Token names are deliberately `mgit_*`, NOT herdr-git-status's `git_*`:
//     a remote running that plugin has its tokens forwarded under the mirror
//     source (mirror.rs §3b), and per-key merge would make the two reporters
//     fight. Different names coexist; the sidebar row decides what to show.
//   * Severity is encoded in the token NAME (`mgit_clean` / `mgit_dirty` /
//     `mgit_conflict`, exactly one set per report, the others null) because
//     herdr renders token values as flat text — colour comes from a per-token
//     `fg` in the sidebar row config.
//   * Every report carries a TTL of three intervals, so a daemon that dies, a
//     host that disconnects, or a dir that stops being a repo self-clears
//     instead of lying forever. No explicit clears are sent.
//   * seq is seeded from wall-clock milliseconds, then incremented: the server
//     silently drops a report whose seq is not greater than the source's last
//     for that workspace, and an in-memory counter restarting at zero on every
//     daemon restart would be dropped forever, freezing the tokens until TTL.
//
// The tokens land on whatever `[ui.sidebar.spaces]` rows name them; mirror
// rows show `$mgit_branch`/`$mgit_ab` where native workspaces show nothing
// (the built-in `branch`/`git_status` still elide for mirrors). See README.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::api::ApiClient;
use crate::config::HostConfig;
use crate::mirror::{fetch_snapshot, Snapshot};
use crate::remote::RemoteHost;
use crate::state::HostState;
use crate::util::{err, Env, Logger, Result};

/// Marker line ending each per-dir record. A porcelain line is `XY <path>` or
/// `## …`, so no git output line can equal this whole line — a repo containing
/// a file named after it still can't collide.
const DELIMITER: &str = "~~~HERDR-MIRROR-GIT-END~~~";

/// `git --no-optional-locks` needs 2.22+ (2018); older git fails the probe and
/// the workspace just shows no git tokens rather than wrong ones.
pub const DEFAULT_INTERVAL_SECS: u64 = 20;
pub const MIN_INTERVAL_SECS: u64 = 5;

pub fn source(host_name: &str) -> String {
    // Same charset the server enforces for metadata sources (letters, digits,
    // colon, dot, underscore, hyphen).
    format!("plugin:mirror:{host_name}:git")
}

/// Source ID for the LOCAL relay (native workspaces on this machine).
/// Deliberately a different source from the host relays: the server checks
/// seq freshness per source, so the two relays never invalidate each other's
/// counters — and per-key token merge lets `$mgit_*` from whichever relay owns
/// a workspace land under its own source.
pub fn source_local() -> String {
    "plugin:mirror:local:git".into()
}

/// A report dies this long after its last refresh. Three intervals rides out
/// one missed probe without masking a genuinely dead daemon for long.
pub fn ttl_ms(interval_secs: u64) -> u64 {
    // the API field is MILLISECONDS; three intervals, clamped to the server's
    // 24h metadata cap
    interval_secs
        .saturating_mul(3)
        .saturating_mul(1000)
        .clamp(1_000, 86_400_000)
}

// --- parsing ---

/// One repo's measured state. Counts follow herdr-git-status's shorthand:
/// `+staged ~modified ?untracked !conflicts`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitInfo {
    pub branch: Option<String>, // None = detached HEAD
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub modified: u32,
    pub untracked: u32,
    pub conflicts: u32,
}

/// Parse `git status --porcelain=v1 -b` output. `None` = no git info (empty
/// output: not a repo, dir gone, or git too old) — the caller reports nothing
/// and the tokens expire by TTL.
pub fn parse_status_output(out: &str) -> Option<GitInfo> {
    let mut lines = out.lines().skip_while(|l| l.trim().is_empty());
    let header = lines.next()?;
    if !header.starts_with("## ") {
        return None;
    }
    let head = header[3..].trim();
    let (branch, ahead, behind) = if let Some(b) = head.strip_prefix("No commits yet on ") {
        (Some(b.trim().to_string()), 0, 0)
    } else if head.starts_with("HEAD (no branch)") {
        (None, 0, 0)
    } else {
        // `branch...upstream [ahead N, behind M]`. Git refnames cannot contain
        // spaces, so the " [" marker is unambiguous; '...' separates upstream.
        let (names, ab) = match head.find(" [") {
            Some(i) => (&head[..i], Some(&head[i + 2..head.len() - 1])),
            None => (head, None),
        };
        let mut a = 0u32;
        let mut b = 0u32;
        if let Some(ab) = ab {
            for part in ab.split(',') {
                let part = part.trim();
                if let Some(n) = part.strip_prefix("ahead ") {
                    a = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix("behind ") {
                    b = n.parse().unwrap_or(0);
                }
            }
        }
        (Some(names.split("...").next().unwrap_or("").trim().to_string()), a, b)
    };
    let mut info = GitInfo { branch, ahead, behind, ..Default::default() };
    for line in lines {
        if line.len() < 2 {
            continue;
        }
        let (x, y) = (line.as_bytes()[0], line.as_bytes()[1]);
        match (x, y) {
            (b'?', b'?') => info.untracked += 1,
            (b'!', _) | (_, b'!') => {} // ignored paths (only shown with --ignored)
            _ if matches!(
                &[x, y],
                b"DD" | b"AU" | b"UD" | b"UA" | b"DU" | b"AA" | b"UU"
            ) =>
            {
                info.conflicts += 1
            }
            _ => {
                if matches!(x, b'M' | b'A' | b'D' | b'R' | b'C') {
                    info.staged += 1;
                }
                if matches!(y, b'M' | b'D') {
                    info.modified += 1;
                }
            }
        }
    }
    Some(info)
}

/// Split one probe run's output into `cwd → status` pairs. Each record is the
/// cwd line, the `git status` output (possibly empty = not a repo), and the
/// delimiter. Anything after the final delimiter is the empty trailing chunk
/// and is dropped.
pub fn parse_probe_output(out: &str) -> HashMap<String, Option<GitInfo>> {
    out.split(DELIMITER)
        .filter_map(|chunk| {
            let chunk = chunk.strip_prefix('\n').unwrap_or(chunk);
            if chunk.is_empty() {
                return None;
            }
            let (cwd, rest) = chunk.split_once('\n')?;
            let cwd = cwd.trim();
            if !cwd.starts_with('/') {
                return None;
            }
            Some((cwd.to_string(), parse_status_output(rest)))
        })
        .collect()
}

// --- probing ---

/// The remote shell script: one loop, one `git status` per dir, framed output.
/// Runs under `sh -c` via ssh (`ssh host <script>`) or `docker exec … sh -c`.
pub fn probe_command(dirs: &[String]) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let mut s = String::from("for d in");
    for d in dirs {
        s.push(' ');
        s.push_str(&quote(d));
    }
    s.push_str("; do\n");
    s.push_str("  printf '%s\\n' \"$d\"\n");
    s.push_str("  git -C \"$d\" --no-optional-locks status --porcelain=v1 -b 2>/dev/null\n");
    s.push_str("  printf '%s\\n' '");
    s.push_str(DELIMITER);
    s.push_str("'\ndone");
    s
}

/// remote workspace id → best cwd, from the first pane that has one
/// (`foreground_cwd` preferred — it tracks `cd` — falling back to the pane's
/// startup cwd). Paths must be absolute; `file://` OSC7 URLs and other
/// oddities are skipped rather than probed as garbage.
pub fn workspace_cwds(snapshot: &Snapshot) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    for p in &snapshot.panes {
        if map.contains_key(&p.workspace_id) {
            continue;
        }
        let cwd = p
            .foreground_cwd
            .as_deref()
            .filter(|s| s.starts_with('/'))
            .or_else(|| p.cwd.as_deref().filter(|s| s.starts_with('/')));
        if let Some(c) = cwd {
            map.insert(p.workspace_id.clone(), c.to_string());
        }
    }
    map
}

// --- tokens ---

/// Map a measured repo onto the `$mgit_*` token set. Null values clear a
/// token, so exactly one of clean/dirty/conflict can ever render.
pub fn tokens_for(info: &GitInfo) -> Map<String, Value> {
    let mut t = Map::new();
    t.insert(
        "mgit_branch".into(),
        info.branch.clone().map(Value::String).unwrap_or(Value::Null),
    );
    t.insert(
        "mgit_ab".into(),
        match (info.ahead, info.behind) {
            (0, 0) => Value::Null,
            (a, 0) => json!(format!("↑{a}")),
            (0, b) => json!(format!("↓{b}")),
            (a, b) => json!(format!("↑{a} ↓{b}")),
        },
    );
    let dirty = info.staged + info.modified + info.untracked;
    let (clean, dirty_t, conflict_t) = if info.conflicts > 0 {
        (Value::Null, Value::Null, json!(format!("!{}", info.conflicts)))
    } else if dirty > 0 {
        let mut parts: Vec<String> = Vec::new();
        if info.staged > 0 {
            parts.push(format!("+{}", info.staged));
        }
        if info.modified > 0 {
            parts.push(format!("~{}", info.modified));
        }
        if info.untracked > 0 {
            parts.push(format!("?{}", info.untracked));
        }
        (Value::Null, Value::String(parts.join(" ")), Value::Null)
    } else if info.ahead == 0 && info.behind == 0 {
        (json!("✓"), Value::Null, Value::Null)
    } else {
        // committed but unpushed/unpulled: `$mgit_ab` carries the numbers, and
        // a ✓ would be a lie about "settled"
        (Value::Null, Value::Null, Value::Null)
    };
    t.insert("mgit_clean".into(), clean);
    t.insert("mgit_dirty".into(), dirty_t);
    t.insert("mgit_conflict".into(), conflict_t);
    t
}

// --- the relay ---

/// Per-connection relay state: one seq per workspace (the server checks
/// freshness per workspace+source) and a once-only warning so a persistently
/// broken probe or a local server that rejects the report logs once, not every
/// interval. A success resets the flag, so the NEXT failure is logged again.
#[derive(Default)]
pub struct RefreshState {
    seqs: HashMap<String, u64>,
    warned: bool,
}

impl RefreshState {
    fn warn_once(&mut self, log: &Logger, host: &str, what: &str, e: &str) {
        if !self.warned {
            self.warned = true;
            log.log(&format!("[{host}] git status {what} (will retry silently): {e}"));
        }
    }

    /// Monotonic per workspace: wall-clock seed on first sight (never collides
    /// with a previous daemon run's counter), +1 after that. Bumped before the
    /// request, not after — a lost response must not retry the same seq.
    fn next_seq(&mut self, key: &str) -> u64 {
        let next = match self.seqs.get(key) {
            Some(s) => s + 1,
            None => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(1),
        };
        self.seqs.insert(key.to_string(), next);
        next
    }
}

/// One relay pass: snapshot the remote for each mirror workspace's cwd, run
/// one batched `git status` probe over the host's exec channel, and report
/// `$mgit_*` tokens to the LOCAL mirror workspaces. Returns how many
/// workspaces were reported. Never fails the caller: probe/report errors are
/// logged (once) and the next interval retries; stale tokens expire via TTL.
pub async fn refresh(
    local: &ApiClient,
    remote_api: &ApiClient,
    remote_exec: &RemoteHost,
    host: &HostConfig,
    state: &HostState,
    relay: &mut RefreshState,
    log: &Logger,
) -> usize {
    if !host.git_status.enabled {
        return 0;
    }
    let live: Vec<(&str, &str)> = state
        .workspaces
        .iter()
        .filter(|(_, e)| !e.is_tombstoned())
        .map(|(rid, e)| (rid.as_str(), e.local_id.as_str()))
        .collect();
    if live.is_empty() {
        return 0;
    }
    let snapshot = match fetch_snapshot(remote_api).await {
        Ok(s) => s,
        Err(e) => {
            relay.warn_once(log, &host.name, "snapshot failed", &e.to_string());
            return 0;
        }
    };
    let cwds = workspace_cwds(&snapshot);
    let mut dirs: Vec<String> = live.iter().filter_map(|(rid, _)| cwds.get(*rid).cloned()).collect();
    dirs.sort();
    dirs.dedup();
    if dirs.is_empty() {
        return 0;
    }
    let out = match remote_exec.exec(&probe_command(&dirs), 30_000).await {
        Ok(o) => o,
        Err(e) => {
            relay.warn_once(log, &host.name, "probe failed", &e.to_string());
            return 0;
        }
    };
    let by_cwd = parse_probe_output(&out);
    let ttl = ttl_ms(host.git_status.interval_secs);
    let mut reported = 0usize;
    for (rid, lid) in &live {
        // No measured info → report nothing; the tokens from the last pass
        // expire by TTL (three intervals) instead of being actively cleared.
        let Some(info) = cwds.get(*rid).and_then(|c| by_cwd.get(c)).and_then(|o| o.as_ref()) else {
            continue;
        };
        let seq = relay.next_seq(lid);
        let report = json!({
            "workspace_id": lid,
            "source": source(&host.name),
            "tokens": Value::Object(tokens_for(info)),
            "seq": seq,
            "ttl_ms": ttl,
        });
        match local.request("workspace.report_metadata", report).await {
            Ok(_) => {
                relay.warned = false;
                reported += 1;
            }
            Err(e) => relay.warn_once(log, &host.name, "report failed", &e.to_string()),
        }
    }
    reported
}

// --- the LOCAL relay (native workspaces on this machine) ---

/// Run the same batched probe against local directories — `sh -c`, no ssh.
async fn run_local_probe(dirs: &[String]) -> std::io::Result<String> {
    use std::process::Stdio;
    let fut = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(probe_command(dirs))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match tokio::time::timeout(std::time::Duration::from_secs(30), fut).await {
        Ok(Ok(out)) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "local probe timeout")),
    }
}

/// One local relay pass: snapshot the LOCAL server, probe each native
/// workspace's cwd, and report `$mgit_*` tokens onto the local workspace rows.
/// Mirror workspaces are skipped by default — their tokens are the host
/// relays' job, and two reporters on one token key would fight every interval.
/// Returns how many workspaces were reported.
pub async fn refresh_local(
    local: &ApiClient,
    state_dir: &std::path::Path,
    hosts: &[HostConfig],
    config: &crate::config::GitStatusLocal,
    relay: &mut RefreshState,
    log: &Logger,
) -> usize {
    if !config.enabled {
        return 0;
    }
    let snap = match fetch_snapshot(local).await {
        Ok(s) => s,
        Err(e) => {
            relay.warn_once(log, "local", "snapshot failed", &e.to_string());
            return 0;
        }
    };
    if snap.workspaces.is_empty() {
        return 0;
    }
    // Mirror workspaces are owned by the per-host relays: exclude every local
    // id any host's map claims. (Loaded fresh each pass — a workspace that
    // just became a mirror drops out on the next pass, and its last local
    // tokens expire by TTL.) With `git_status_local = "all"` the exclusion is
    // waived — the single-writer setup where per-host relays are disabled.
    let mut mirrors: std::collections::HashSet<String> = Default::default();
    if config.include_mirrors != crate::config::IncludeMirrors::Yes {
        for h in hosts {
            let st = crate::state::load_state(state_dir, &h.name);
            mirrors.extend(st.workspaces.values().map(|e| e.local_id.clone()));
        }
    }
    let cwds = workspace_cwds(&snap);
    let mut dirs: Vec<String> = snap
        .workspaces
        .iter()
        .filter(|w| !mirrors.contains(&w.workspace_id))
        .filter_map(|w| cwds.get(&w.workspace_id).cloned())
        .collect();
    dirs.sort();
    dirs.dedup();
    if dirs.is_empty() {
        return 0;
    }
    let out = match run_local_probe(&dirs).await {
        Ok(o) => o,
        Err(e) => {
            relay.warn_once(log, "local", "probe failed", &e.to_string());
            return 0;
        }
    };
    let by_cwd = parse_probe_output(&out);
    let ttl = ttl_ms(crate::git_status::DEFAULT_INTERVAL_SECS);
    let mut reported = 0usize;
    for w in &snap.workspaces {
        if mirrors.contains(&w.workspace_id) {
            continue;
        }
        let Some(info) = cwds.get(&w.workspace_id).and_then(|c| by_cwd.get(c)).and_then(|o| o.as_ref())
        else {
            continue;
        };
        let seq = relay.next_seq(&w.workspace_id);
        let report = json!({
            "workspace_id": w.workspace_id,
            "source": source_local(),
            "tokens": Value::Object(tokens_for(info)),
            "seq": seq,
            "ttl_ms": ttl,
        });
        match local.request("workspace.report_metadata", report).await {
            Ok(_) => {
                relay.warned = false;
                reported += 1;
            }
            Err(e) => relay.warn_once(log, "local", "report failed", &e.to_string()),
        }
    }
    reported
}

// --- the standalone CLI mode ---

/// `herdr-mirror git-status`: run ONLY the local relay, no daemon, no config,
/// no ssh. For machines that just run herdr (e.g. the mirrored servers
/// themselves) — gives their native workspaces the same `$mgit_*` tokens.
///
/// `--interval <secs>`: probe cadence (default 20, min 5).
/// `--socket <path>`: herdr socket (default: HERDR_SOCKET_PATH, else
/// `herdr status`);
pub fn parse_args(args: &[String]) -> Result<(u64, Option<std::path::PathBuf>)> {
    let mut interval = DEFAULT_INTERVAL_SECS;
    let mut socket = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--interval" => {
                i += 1;
                interval = args
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| err("--interval needs a number of seconds"))?;
            }
            "--socket" => {
                i += 1;
                socket = Some(
                    args.get(i)
                        .ok_or_else(|| err("--socket needs a path"))?
                        .into(),
                );
            }
            other => return Err(err(format!("unknown git-status flag: {other}"))),
        }
        i += 1;
    }
    Ok((interval.max(MIN_INTERVAL_SECS), socket))
}

pub async fn run_standalone(interval_secs: u64, socket: Option<std::path::PathBuf>) -> Result<()> {
    // non-interactive ssh PATHs often lack ~/.local/bin, where herdr lives;
    // Env::resolve shells out to `herdr status` for socket discovery
    if socket.is_none() && std::env::var_os("HERDR_SOCKET_PATH").is_none() {
        if let (Some(home), Some(path)) = (std::env::var_os("HOME"), std::env::var_os("PATH")) {
            let local_bin = std::path::PathBuf::from(&home).join(".local/bin");
            let mut parts: Vec<std::path::PathBuf> = std::env::split_paths(&path).collect();
            if !parts.iter().any(|p| p == &local_bin) {
                parts.insert(0, local_bin);
                std::env::set_var("PATH", std::env::join_paths(parts).unwrap_or(path));
            }
        }
    }
    let mut env = Env::resolve()?;
    if let Some(p) = socket {
        env.local_socket = p;
    }
    let local = ApiClient::connect(&env.local_socket).await?;
    let log = Logger::new(&env.state_dir, false);
    log.log(&format!(
        "git-status relay starting (standalone, interval {interval_secs}s)"
    ));
    let cfg = crate::config::GitStatusLocal {
        enabled: true,
        interval_secs,
        include_mirrors: crate::config::IncludeMirrors::Yes,
    };
    let mut relay = RefreshState::default();
    // no hosts.toml here by assumption: an empty host list means the mirror
    // exclusion pass has nothing to load, so every workspace is covered —
    // exactly right on a server that only runs its own herdr
    loop {
        refresh_local(&local, &env.state_dir, &[], &cfg, &mut relay, &log).await;
        tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_repo() {
        let info = parse_status_output("## main\n").unwrap();
        assert_eq!(info.branch.as_deref(), Some("main"));
        assert_eq!((info.ahead, info.behind), (0, 0));
        assert_eq!((info.staged, info.modified, info.untracked, info.conflicts), (0, 0, 0, 0));
    }

    #[test]
    fn parses_ahead_behind_and_counts() {
        let out = "## main...origin/main [ahead 1, behind 2]\n\
                   M  staged.txt\n\
                   MM both.txt\n\
                   ?? untracked1.txt\n\
                   ?? untracked2.txt\n\
                   ?? untracked3.txt\n";
        let info = parse_status_output(out).unwrap();
        assert_eq!(info.branch.as_deref(), Some("main"));
        assert_eq!((info.ahead, info.behind), (1, 2));
        assert_eq!(info.staged, 2, "M- and MM both have an index side");
        assert_eq!(info.modified, 1, "only MM has a worktree side");
        assert_eq!(info.untracked, 3);
        assert_eq!(info.conflicts, 0);
    }

    #[test]
    fn ahead_only_and_behind_only_parse() {
        let a = parse_status_output("## main...origin/main [ahead 3]\n").unwrap();
        assert_eq!((a.ahead, a.behind), (3, 0));
        let b = parse_status_output("## main...origin/main [behind 7]\n").unwrap();
        assert_eq!((b.ahead, b.behind), (0, 7));
    }

    #[test]
    fn detached_head_has_no_branch() {
        let info = parse_status_output("## HEAD (no branch)\n").unwrap();
        assert_eq!(info.branch, None);
    }

    #[test]
    fn init_repo_still_names_the_branch() {
        let info = parse_status_output("## No commits yet on main\n").unwrap();
        assert_eq!(info.branch.as_deref(), Some("main"));
    }

    #[test]
    fn conflict_pairs_count_as_conflicts() {
        let out = "## main\nUU both.txt\nAA added.txt\nDU deleted.txt\n";
        let info = parse_status_output(out).unwrap();
        assert_eq!(info.conflicts, 3);
        assert_eq!((info.staged, info.modified), (0, 0));
    }

    #[test]
    fn empty_output_is_no_git_info() {
        assert_eq!(parse_status_output(""), None);
        assert_eq!(parse_status_output("fatal: not a git repository\n"), None);
    }

    #[test]
    fn probe_records_round_trip() {
        let dirs = vec!["/srv/app".to_string(), "/srv/other".to_string()];
        // simulate what the remote prints for: repo(dirty), non-repo, repo(clean)
        let out = "/srv/app\n\
                   ## main...origin/main [ahead 2]\n\
                   M  a.txt\n\
                   ~~~HERDR-MIRROR-GIT-END~~~\n\
                   /srv/other\n\
                   ~~~HERDR-MIRROR-GIT-END~~~\n\
                   /srv/third\n\
                   ## dev\n\
                   ~~~HERDR-MIRROR-GIT-END~~~\n";
        let by_cwd = parse_probe_output(&out);
        assert_eq!(by_cwd.len(), 3);
        let app = by_cwd.get("/srv/app").unwrap().as_ref().unwrap();
        assert_eq!(app.branch.as_deref(), Some("main"));
        assert_eq!((app.ahead, app.behind), (2, 0));
        assert_eq!((app.staged, app.modified), (1, 0), "'M  ' is staged-only");
        // non-repo: present as a key with no info (the cwd itself was probed)
        assert_eq!(by_cwd.get("/srv/other"), Some(&None));
        let third = by_cwd.get("/srv/third").unwrap().as_ref().unwrap();
        assert_eq!(third.branch.as_deref(), Some("dev"));
    }

    #[test]
    fn probe_command_quotes_and_frames() {
        let dirs = vec!["/srv/it's".to_string()];
        let script = probe_command(&dirs);
        assert!(script.contains("'/srv/it'\\''s'"), "single quotes escaped: {script}");
        assert!(script.contains("--no-optional-locks"));
        assert!(script.contains(DELIMITER));
    }

    #[test]
    fn cwds_prefer_foreground_and_require_absolute() {
        let snap = Snapshot {
            workspaces: vec![],
            tabs: vec![],
            panes: vec![
                crate::mirror::PaneInfo {
                    pane_id: "w1:p1".into(),
                    tab_id: "w1:t1".into(),
                    workspace_id: "w1".into(),
                    label: None,
                    cwd: Some("/srv/app".into()),
                    foreground_cwd: Some("file://host/srv/elsewhere".into()), // rejected
                },
                crate::mirror::PaneInfo {
                    pane_id: "w2:p1".into(),
                    tab_id: "w2:t1".into(),
                    workspace_id: "w2".into(),
                    label: None,
                    cwd: None,
                    foreground_cwd: Some("/srv/live".into()), // preferred over nothing
                },
            ],
            agents: vec![],
            layouts: vec![],
        };
        let cwds = workspace_cwds(&snap);
        assert_eq!(cwds.get("w1").map(String::as_str), Some("/srv/app"));
        assert_eq!(cwds.get("w2").map(String::as_str), Some("/srv/live"));
    }

    #[test]
    fn tokens_are_mutually_exclusive_by_severity() {
        let mut info = GitInfo { branch: Some("main".into()), ..Default::default() };
        let t = tokens_for(&info);
        assert_eq!(t["mgit_branch"], json!("main"));
        assert_eq!(t["mgit_clean"], json!("✓"));
        assert_eq!(t["mgit_dirty"], Value::Null);
        assert_eq!(t["mgit_conflict"], Value::Null);
        assert_eq!(t["mgit_ab"], Value::Null);

        info.staged = 2;
        info.modified = 1;
        info.untracked = 3;
        let t = tokens_for(&info);
        assert_eq!(t["mgit_dirty"], json!("+2 ~1 ?3"));
        assert_eq!(t["mgit_clean"], Value::Null);
        assert_eq!(t["mgit_conflict"], Value::Null);

        info.conflicts = 2;
        info.staged = 0;
        let t = tokens_for(&info);
        assert_eq!(t["mgit_conflict"], json!("!2"));
        assert_eq!(t["mgit_dirty"], Value::Null);

        // committed but unpushed: no ✓, the ab token carries it
        info = GitInfo { branch: Some("main".into()), ahead: 1, ..Default::default() };
        let t = tokens_for(&info);
        assert_eq!(t["mgit_clean"], Value::Null);
        assert_eq!(t["mgit_ab"], json!("↑1"));

        // detached: branch token clears
        info = GitInfo::default();
        let t = tokens_for(&info);
        assert_eq!(t["mgit_branch"], Value::Null);
    }

    #[test]
    fn ttl_tracks_interval_and_respects_server_cap() {
        assert_eq!(ttl_ms(5), 15_000);
        assert_eq!(ttl_ms(20), 60_000);
        assert_eq!(ttl_ms(86_400_000), 86_400_000); // capped at the server's 24h
    }

    #[test]
    fn seq_seeds_from_the_clock_then_increments() {
        let mut relay = RefreshState::default();
        let first = relay.next_seq("wA");
        assert!(first > 1_000_000_000_000, "wall-clock seed, not a counter from zero: {first}");
        assert_eq!(relay.next_seq("wA"), first + 1);
        // a second workspace seeds from the clock too — it can land on the same
        // millisecond (>=), but never behind, or the server would drop reports
        assert!(relay.next_seq("wB") >= first);
    }

    #[test]
    fn source_uses_the_servers_charset() {
        let s = source("p25");
        assert!(s.starts_with("plugin:mirror:p25:git"));
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '-')));
    }
}
