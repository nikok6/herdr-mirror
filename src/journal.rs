// Hub-local tail copies of mirrored pi/omp session journals.
//
// A transcript viewer on the hub (Collie's History) reads a pane's agent
// journal from disk, but a mirrored agent's journal lives on the remote, is
// often well over 100 MB, and is named by a remote path. So for every mirrored
// pane whose remote agent reports a `path` session, this keeps
//
//   <state>/journals/sessions/<id>.jsonl
//
// holding the journal's first row (pi/omp's `{"type":"session",...}` header)
// plus its last `journal_tail_mb` bytes cut at a line boundary. Appends are
// followed by byte offset over the host's ControlMaster, the copy is rewritten
// to header + tail once it passes twice that, and image blobs it references are
// fetched into `<state>/journals/blobs/<sha256>` (the sibling `blobs/` that
// pi/omp readers look in).
//
// The copy's path reaches the hub pane as the `mirror_session_path` metadata
// token, NOT as a herdr agent session. herdr (0.9.0-0.9.5) takes a pi/omp
// session only from the `herdr:pi`/`herdr:omp` sources and only for a pane in
// which it detects that agent's process (`route_full_lifecycle_hook_report`),
// so a report onto a mirror pane is silently dropped; and a kept one would be
// persisted and make a cold restore type `omp --resume=<copy>` into the
// restored mirror shell. Metadata tokens are neither persisted nor read by
// restore, and the default sidebar renders none.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::Hasher;
use std::io::Write;
use std::ops::Range;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::mirror::AgentInfo;
use crate::remote::RemoteExec;
use crate::state::HostState;
use crate::util::Logger;

/// Pane metadata token carrying the hub-local copy's absolute path.
pub const TOKEN: &str = "mirror_session_path";
/// herdr keeps at most this many characters of a token value and silently
/// drops the rest, so a longer path is never published (it would name a file
/// that does not exist).
const TOKEN_VALUE_MAX: usize = 80;
pub const DEFAULT_TAIL_MB: u64 = 16;
/// The header is the journal's leading rows up to and including its
/// `{"type":"session",...}` row (omp 18 writes an in-place `title` row first),
/// else its first row; a header longer than this is dropped.
const HEADER_MAX: u64 = 64 * 1024;
/// A pane whose agent is not working and whose status did not change is
/// looked at this often; a working one every status poll.
const IDLE_RECHECK: Duration = Duration::from_secs(30);
/// A copy whose pane stopped reporting a session (agent gone, pane no longer
/// mirrored) is deleted after this long, so a blip doesn't cost a re-seed.
const ABSENT_GRACE: Duration = Duration::from_secs(30);
const BLOB_MAX: u64 = 8 * 1024 * 1024;
const BLOBS_PER_TICK: usize = 8;
const STAT_TIMEOUT_MS: u64 = 15_000;
const READ_TIMEOUT_MS: u64 = 180_000;
/// How often the daemon sweeps blobs no copy references any more.
pub const BLOB_GC_EVERY: Duration = Duration::from_secs(600);
/// A blob younger than this survives a sweep: it may belong to a copy that
/// was written after the sweep read the copies.
const BLOB_GC_MIN_AGE: Duration = Duration::from_secs(120);
const BLOB_REF: &[u8] = b"blob:sha256:";

/// Remote pane id -> published local copy path, per host. Read by the status
/// reporter, written by the journal task.
pub type Published = Arc<Mutex<HashMap<String, String>>>;

pub fn new_published() -> Published {
    Arc::default()
}

/// One mirrored pane whose remote agent reports a session file.
#[derive(Debug, Clone, PartialEq)]
pub struct Want {
    pub remote_pane: String,
    pub remote_path: String,
    pub status: Option<String>,
    /// the remote's `state_change_seq`: moves on every state change, even a
    /// turn that began and ended between two polls
    pub state_seq: Option<u64>,
}

/// The panes worth a copy: mirrored (mapped, not tombstoned) and reporting a
/// `path` session.
pub fn wants(agents: &[AgentInfo], state: &HostState) -> Vec<Want> {
    agents
        .iter()
        .filter(|a| state.panes.get(&a.pane_id).is_some_and(|e| !e.is_tombstoned()))
        .filter_map(|a| {
            let s = a.agent_session.as_ref()?;
            (s.kind == "path" && s.value.starts_with('/')).then(|| Want {
                remote_pane: a.pane_id.clone(),
                remote_path: s.value.clone(),
                status: a.agent_status.clone(),
                state_seq: a.state_change_seq,
            })
        })
        .collect()
}

pub fn root(state_dir: &Path) -> PathBuf {
    state_dir.join("journals")
}

fn sessions_dir(root: &Path) -> PathBuf {
    root.join("sessions")
}

fn blobs_dir(root: &Path) -> PathBuf {
    root.join("blobs")
}

fn meta_dir(root: &Path) -> PathBuf {
    root.join("meta")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Meta {
    host: String,
    remote_pane: String,
    remote_path: String,
    name: String,
    /// identity of the remote file the copy follows
    ino: u64,
    /// remote byte offset copied up to (always just past a newline)
    offset: u64,
    header_len: u64,
    local_len: u64,
    /// the copy is inside a row too long for the tail; drop up to the next
    /// newline before appending
    #[serde(default)]
    skip_partial: bool,
}

/// Stable short id for one (host, pane, journal). FNV-1a, not `DefaultHasher`,
/// because it names a file that must survive a toolchain upgrade.
fn copy_name(host: &str, pane: &str, path: &str) -> String {
    let mut h = fnv::FnvHasher::default();
    for part in [host, pane, path] {
        h.write(part.as_bytes());
        h.write_u8(0);
    }
    format!("{:012x}", h.finish() & 0xffff_ffff_ffff)
}

#[derive(Debug, PartialEq)]
enum Action {
    Nothing,
    Seed { size: u64, ino: u64 },
    Append { from: u64, len: u64 },
}

/// What one remote stat means for a copy. `local_intact`: the local file is
/// as long as the meta says.
fn decide(meta: Option<&Meta>, path: &str, stat: Option<(u64, u64)>, tail: u64, local_intact: bool) -> Action {
    // not written yet (pi/omp create the file with the first saved reply)
    let Some((size, ino)) = stat else { return Action::Nothing };
    let seed = Action::Seed { size, ino };
    let Some(m) = meta else { return seed };
    if m.remote_path != path || m.ino != ino || size < m.offset || !local_intact {
        return seed; // replaced, truncated, or the copy was damaged
    }
    if size - m.offset > tail {
        return seed; // further behind than the copy keeps: start over at the tail
    }
    if size > m.offset {
        return Action::Append { from: m.offset, len: size - m.offset };
    }
    Action::Nothing
}

#[derive(Debug, PartialEq)]
struct Cut {
    /// whole rows within `data`
    body: Range<usize>,
    /// remote offset to continue from
    next: u64,
    skip_partial: bool,
}

/// Keep the whole rows of `data`, which starts at remote offset `start`.
/// `mid_line`: `data` may begin inside a row, so everything up to and
/// including the first newline is dropped. A trailing partial row is left for
/// the next read.
fn cut(data: &[u8], start: u64, mid_line: bool) -> Cut {
    let mut b = 0;
    if mid_line {
        match data.iter().position(|&c| c == b'\n') {
            Some(i) => b = i + 1,
            None => {
                return Cut { body: 0..0, next: start + data.len() as u64, skip_partial: true };
            }
        }
    }
    match data[b..].iter().rposition(|&c| c == b'\n') {
        Some(j) => Cut { body: b..b + j + 1, next: start + (b + j + 1) as u64, skip_partial: false },
        None => Cut { body: b..b, next: start + b as u64, skip_partial: false },
    }
}

/// The last `tail` bytes of `body`, starting at a row boundary.
fn trim_to_tail(body: &[u8], tail: u64) -> &[u8] {
    let tail = tail as usize;
    if body.len() <= tail {
        return body;
    }
    let start = body.len() - tail;
    if body[start - 1] == b'\n' {
        return &body[start..];
    }
    match body[start..].iter().position(|&c| c == b'\n') {
        Some(i) => &body[start + i + 1..],
        None => &[],
    }
}

/// Every `blob:sha256:<64 hex>` reference in `data`, deduplicated.
fn blob_refs(data: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i + BLOB_REF.len() + 64 <= data.len() {
        if data[i..].starts_with(BLOB_REF) {
            let h = &data[i + BLOB_REF.len()..i + BLOB_REF.len() + 64];
            if h.iter().all(u8::is_ascii_hexdigit) {
                let s = String::from_utf8_lossy(h).to_ascii_lowercase();
                if !out.contains(&s) {
                    out.push(s);
                }
                i += BLOB_REF.len() + 64;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Where the remote keeps the blob for a journal: the `blobs/` beside its
/// `sessions/` root (`<agent>/sessions/<dir>/<file>.jsonl` -> `<agent>/blobs`).
fn remote_blob_path(journal: &str, hash: &str) -> Option<String> {
    let agent_dir = Path::new(journal).parent()?.parent()?.parent()?;
    Some(agent_dir.join("blobs").join(hash).display().to_string())
}

// --- remote commands ---
//
// Each is `sh -c '<script>' sh <args>`: the remote login shell (which may be
// fish) only has to parse that line, and the script itself always runs under
// a POSIX sh. Scripts contain no single quote; arguments are quoted.

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn sh_script(script: &str, args: &[String]) -> String {
    debug_assert!(!script.contains('\''));
    let mut cmd = format!("sh -c '{script}' sh");
    for a in args {
        cmd.push(' ');
        cmd.push_str(&sh_quote(a));
    }
    cmd
}

/// One line per path: `<size> <inode>`, or `- -` when it can't be stat'ed.
fn stat_command(paths: &[&str]) -> String {
    sh_script(
        r#"for f in "$@"; do s=$(stat -L -c "%s %i" -- "$f" 2>/dev/null) || s="- -"; printf "%s\n" "$s"; done"#,
        &paths.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
    )
}

fn parse_stats(out: &[u8], n: usize) -> Vec<Option<(u64, u64)>> {
    let text = String::from_utf8_lossy(out);
    let mut lines = text.lines();
    (0..n)
        .map(|_| {
            let mut it = lines.next()?.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect()
}

/// Seed read: `<h> <rs>\n`, then the `h`-byte header (the rows through the
/// first session row, else the first row; 0 when neither fits in HEADER_MAX),
/// then the file from `rs` to `size`.
/// `rs` is one byte before where the tail starts, so a tail that happens to
/// start on a row boundary is recognised as one (`floor` = `size - tail`).
fn seed_command(path: &str, floor: u64, size: u64) -> String {
    let script = format!(
        r#"f=$1; fl=$2; sz=$3
h=$(head -c {HEADER_MAX} -- "$f" | LC_ALL=C awk "{{n+=length(\$0)+1}} /^\{{\"type\":\"session\"/{{print n; s=1; exit}} END{{if(!s) print 0}}")
[ "$h" -le {HEADER_MAX} ] || h=0
if [ "$h" -eq 0 ]; then
h=$(head -c {HEADER_MAX} -- "$f" | head -n 1 | wc -c)
n=$(head -c {HEADER_MAX} -- "$f" | head -n 1 | wc -l)
[ "$n" -eq 1 ] || h=0
fi
st=$fl; [ "$h" -gt "$st" ] && st=$h
rs=$st; [ "$st" -gt "$h" ] && rs=$((st - 1))
printf "%s %s\n" "$h" "$rs"
head -c "$h" -- "$f"
[ "$sz" -gt "$rs" ] && tail -c +$((rs + 1)) -- "$f" | head -c $((sz - rs))
exit 0"#
    );
    sh_script(&script, &[path.to_string(), floor.to_string(), size.to_string()])
}

struct SeedRead<'a> {
    header: &'a [u8],
    rs: u64,
    mid_line: bool,
    data: &'a [u8],
}

fn parse_seed(out: &[u8]) -> Option<SeedRead<'_>> {
    let nl = out.iter().position(|&c| c == b'\n')?;
    let line = std::str::from_utf8(&out[..nl]).ok()?;
    let mut it = line.split_whitespace();
    let h: u64 = it.next()?.parse().ok()?;
    let rs: u64 = it.next()?.parse().ok()?;
    let rest = &out[nl + 1..];
    if (rest.len() as u64) < h {
        return None;
    }
    let (header, data) = rest.split_at(h as usize);
    Some(SeedRead { header, rs, mid_line: rs > h, data })
}

fn range_command(path: &str, from: u64, len: u64) -> String {
    sh_script(
        r#"tail -c +$(($2 + 1)) -- "$1" | head -c "$3""#,
        &[path.to_string(), from.to_string(), len.to_string()],
    )
}

/// The blob's bytes, or a non-zero exit when it is missing (3) or over `max`
/// (4).
fn blob_command(path: &str, max: u64) -> String {
    sh_script(
        r#"s=$(stat -L -c %s -- "$1" 2>/dev/null) || exit 3; [ "$s" -le "$2" ] || exit 4; exec cat -- "$1""#,
        &[path.to_string(), max.to_string()],
    )
}

// --- local files ---

fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Replace `dir/name` with `parts`, atomically (a dot-named temp + rename),
/// mode 0600. Readers holding the old file keep reading it.
fn write_atomic(dir: &Path, name: &str, parts: &[&[u8]]) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    for p in parts {
        f.write_all(p)?;
    }
    drop(f);
    fs::rename(&tmp, dir.join(name))
}

fn append_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = fs::OpenOptions::new().append(true).mode(0o600).open(path)?;
    f.write_all(bytes)
}

fn load_metas(root: &Path) -> Vec<Meta> {
    let Ok(rd) = fs::read_dir(meta_dir(root)) else { return Vec::new() };
    rd.flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .filter_map(|e| serde_json::from_slice::<Meta>(&fs::read(e.path()).ok()?).ok())
        .collect()
}

fn remove_copy_files(root: &Path, name: &str) {
    let _ = fs::remove_file(sessions_dir(root).join(format!("{name}.jsonl")));
    let _ = fs::remove_file(meta_dir(root).join(format!("{name}.json")));
}

/// Daemon start: drop copies no configured, enabled host owns, files without
/// a meta, and temp files a crash left behind.
pub fn prune_foreign(state_dir: &Path, hosts: &HashSet<String>, log: &Logger) {
    let root = root(state_dir);
    let mut keep: HashSet<String> = HashSet::new();
    for m in load_metas(&root) {
        if hosts.contains(&m.host) {
            keep.insert(m.name);
        } else {
            log.log(&format!("journal: removing copy {} of unconfigured host {}", m.name, m.host));
            remove_copy_files(&root, &m.name);
        }
    }
    for dir in [sessions_dir(&root), meta_dir(&root), blobs_dir(&root)] {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let stray_tmp = name.starts_with('.') && name.ends_with(".tmp");
            let orphan = dir == sessions_dir(&root)
                && name.strip_suffix(".jsonl").is_some_and(|n| !keep.contains(n));
            if stray_tmp || orphan {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

/// Delete blobs no copy references. Copies are only ever a bounded tail, so
/// this is what keeps the blob store bounded too.
pub fn gc_blobs(state_dir: &Path, log: &Logger) {
    let root = root(state_dir);
    let started = SystemTime::now();
    let Ok(blobs) = fs::read_dir(blobs_dir(&root)) else { return };
    let blobs: Vec<_> = blobs.flatten().collect();
    if blobs.is_empty() {
        return;
    }
    let mut referenced: HashSet<String> = HashSet::new();
    if let Ok(rd) = fs::read_dir(sessions_dir(&root)) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().ends_with(".jsonl") {
                if let Ok(bytes) = fs::read(e.path()) {
                    referenced.extend(blob_refs(&bytes));
                }
            }
        }
    }
    let mut removed = 0;
    for e in blobs {
        let name = e.file_name().to_string_lossy().into_owned();
        if referenced.contains(&name) || name.starts_with('.') {
            continue;
        }
        let young = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| started.duration_since(t).ok())
            .is_none_or(|age| age < BLOB_GC_MIN_AGE);
        if !young && fs::remove_file(e.path()).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        log.log(&format!("journal: removed {removed} unreferenced blob(s)"));
    }
}

// --- the per-host engine ---

#[derive(Default)]
struct Copy {
    meta: Option<Meta>,
    last_status: Option<String>,
    last_seq: Option<u64>,
    last_check: Option<Instant>,
    absent_since: Option<Instant>,
}

pub struct Journals {
    host: String,
    root: PathBuf,
    tail: u64,
    exec: RemoteExec,
    published: Published,
    log: Logger,
    copies: HashMap<String, Copy>,
    blob_queue: Vec<(String, String)>,
    blob_failed: HashSet<String>,
    exec_failing: bool,
    too_long_logged: HashSet<String>,
}

impl Journals {
    /// Take over this host's copies from disk and publish the intact ones at
    /// once, so the first status push after a (re)connect already carries
    /// their tokens.
    pub fn open(host: &str, state_dir: &Path, tail: u64, exec: RemoteExec, published: Published, log: Logger) -> Journals {
        let root = root(state_dir);
        for d in [root.clone(), sessions_dir(&root), blobs_dir(&root), meta_dir(&root)] {
            if let Err(e) = ensure_dir(&d) {
                log.log(&format!("[{host}] journal: cannot create {}: {e}", d.display()));
            }
        }
        let mut j = Journals {
            host: host.to_string(),
            root,
            tail,
            exec,
            published,
            log,
            copies: HashMap::new(),
            blob_queue: Vec::new(),
            blob_failed: HashSet::new(),
            exec_failing: false,
            too_long_logged: HashSet::new(),
        };
        let host = j.host.clone();
        for m in load_metas(&j.root).into_iter().filter(|m| m.host == host) {
            let path = j.local_path(&m.name);
            let intact = match fs::metadata(&path).map(|md| md.len()) {
                Ok(len) if len == m.local_len => true,
                // an append cut short by a stop: drop the torn bytes
                Ok(len) if len > m.local_len => {
                    fs::OpenOptions::new().write(true).open(&path).and_then(|f| f.set_len(m.local_len)).is_ok()
                }
                _ => false,
            };
            if !intact {
                remove_copy_files(&j.root, &m.name);
                continue;
            }
            let pane = m.remote_pane.clone();
            j.publish(&pane, &m.name);
            j.copies.insert(pane, Copy { meta: Some(m), ..Copy::default() });
        }
        j
    }

    fn local_path(&self, name: &str) -> PathBuf {
        sessions_dir(&self.root).join(format!("{name}.jsonl"))
    }

    fn publish(&mut self, pane: &str, name: &str) {
        let path = self.local_path(name).display().to_string();
        if path.len() > TOKEN_VALUE_MAX {
            if self.too_long_logged.insert(path.clone()) {
                self.log.log(&format!(
                    "[{}] journal: {path} is longer than the {TOKEN_VALUE_MAX} characters a herdr token holds; not published",
                    self.host
                ));
            }
            return;
        }
        if let Ok(mut p) = self.published.lock() {
            p.insert(pane.to_string(), path);
        }
    }

    fn unpublish(&self, pane: &str) {
        if let Ok(mut p) = self.published.lock() {
            p.remove(pane);
        }
    }

    fn drop_copy(&mut self, pane: &str, why: &str) {
        if let Some(copy) = self.copies.remove(pane) {
            self.unpublish(pane);
            if let Some(m) = copy.meta {
                self.log.log(&format!("[{}] journal {pane}: {why} — removing copy {}", self.host, m.name));
                remove_copy_files(&self.root, &m.name);
            }
        }
    }

    fn save_meta(&self, m: &Meta) -> std::io::Result<()> {
        let json = serde_json::to_vec(m).map_err(std::io::Error::other)?;
        write_atomic(&meta_dir(&self.root), &format!("{}.json", m.name), &[&json])
    }

    fn exec_failed(&mut self, e: impl std::fmt::Display) {
        if !self.exec_failing {
            self.log.log(&format!("[{}] journal: {e}", self.host));
        }
        self.exec_failing = true;
    }

    /// One pass over the panes `wants` names: retire copies whose pane left,
    /// stat the ones due a look in a single remote exec, seed or append, then
    /// fetch a few referenced blobs.
    pub async fn tick(&mut self, wants: &[Want]) {
        self.tick_at(wants, Instant::now()).await
    }

    async fn tick_at(&mut self, wants: &[Want], now: Instant) {
        let wanted: HashMap<&str, &Want> = wants.iter().map(|w| (w.remote_pane.as_str(), w)).collect();

        let gone: Vec<String> = self
            .copies
            .iter_mut()
            .filter(|(pane, _)| !wanted.contains_key(pane.as_str()))
            .filter_map(|(pane, c)| {
                let since = *c.absent_since.get_or_insert(now);
                (now.duration_since(since) >= ABSENT_GRACE).then(|| pane.clone())
            })
            .collect();
        for pane in gone {
            self.drop_copy(&pane, "session ended or pane no longer mirrored");
        }

        let mut due: Vec<&Want> = Vec::new();
        for w in wants {
            let changed_path = self
                .copies
                .get(&w.remote_pane)
                .and_then(|c| c.meta.as_ref())
                .is_some_and(|m| m.remote_path != w.remote_path);
            if changed_path {
                self.drop_copy(&w.remote_pane, "session path changed");
            }
            let c = self.copies.entry(w.remote_pane.clone()).or_default();
            c.absent_since = None;
            let is_due = c.meta.is_none()
                || w.status.as_deref() == Some("working")
                || c.last_status != w.status
                || c.last_seq != w.state_seq
                || c.last_check.is_none_or(|t| now.duration_since(t) >= IDLE_RECHECK);
            if is_due {
                due.push(w);
            }
        }

        if !due.is_empty() {
            let paths: Vec<&str> = due.iter().map(|w| w.remote_path.as_str()).collect();
            let stats = match self.exec.run("journal stat", &stat_command(&paths), STAT_TIMEOUT_MS).await {
                Ok(out) => parse_stats(&out, due.len()),
                Err(e) => return self.exec_failed(e),
            };
            for (w, stat) in due.into_iter().zip(stats) {
                match self.sync_one(w, stat).await {
                    Ok(()) => self.exec_failing = false,
                    Err(e) => {
                        self.exec_failed(format!("{}: {e}", w.remote_pane));
                        continue;
                    }
                }
                if let Some(c) = self.copies.get_mut(&w.remote_pane) {
                    c.last_check = Some(now);
                    c.last_status = w.status.clone();
                    c.last_seq = w.state_seq;
                }
            }
        }

        self.fetch_blobs().await;
    }

    async fn sync_one(&mut self, w: &Want, stat: Option<(u64, u64)>) -> crate::util::Result<()> {
        let meta = self.copies.get(&w.remote_pane).and_then(|c| c.meta.clone());
        let intact = meta.as_ref().is_some_and(|m| {
            fs::metadata(self.local_path(&m.name)).map(|md| md.len()).ok() == Some(m.local_len)
        });
        match decide(meta.as_ref(), &w.remote_path, stat, self.tail, intact) {
            Action::Nothing => Ok(()),
            Action::Seed { size, ino } => self.seed(w, size, ino).await,
            Action::Append { from, len } => {
                let m = meta.expect("append implies a copy");
                self.append(w, m, from, len).await
            }
        }
    }

    async fn seed(&mut self, w: &Want, size: u64, ino: u64) -> crate::util::Result<()> {
        let floor = size.saturating_sub(self.tail);
        let out = self.exec.run("journal seed", &seed_command(&w.remote_path, floor, size), READ_TIMEOUT_MS).await?;
        let read = parse_seed(&out).ok_or_else(|| crate::util::err("journal seed: malformed reply"))?;
        let c = cut(read.data, read.rs, read.mid_line);
        let body = &read.data[c.body.clone()];
        let name = copy_name(&self.host, &w.remote_pane, &w.remote_path);
        write_atomic(&sessions_dir(&self.root), &format!("{name}.jsonl"), &[read.header, body])?;
        let meta = Meta {
            host: self.host.clone(),
            remote_pane: w.remote_pane.clone(),
            remote_path: w.remote_path.clone(),
            name: name.clone(),
            ino,
            offset: c.next,
            header_len: read.header.len() as u64,
            local_len: (read.header.len() + body.len()) as u64,
            skip_partial: c.skip_partial,
        };
        self.save_meta(&meta)?;
        self.log.log(&format!(
            "[{}] journal {}: seeded {name} with {} of {size} bytes ({} received)",
            self.host,
            w.remote_pane,
            meta.local_len,
            out.len()
        ));
        self.queue_blobs(&w.remote_path, body);
        self.publish(&w.remote_pane, &name);
        self.copies.entry(w.remote_pane.clone()).or_default().meta = Some(meta);
        Ok(())
    }

    async fn append(&mut self, w: &Want, mut m: Meta, from: u64, len: u64) -> crate::util::Result<()> {
        let data = self.exec.run("journal read", &range_command(&w.remote_path, from, len), READ_TIMEOUT_MS).await?;
        let c = cut(&data, from, m.skip_partial);
        let body = &data[c.body.clone()];
        let path = self.local_path(&m.name);
        if !body.is_empty() {
            append_file(&path, body)?;
        }
        m.offset = c.next;
        m.skip_partial = c.skip_partial;
        m.local_len += body.len() as u64;
        if m.local_len > m.header_len + 2 * self.tail {
            self.rotate(&mut m)?;
        }
        self.save_meta(&m)?;
        self.queue_blobs(&w.remote_path, body);
        self.copies.entry(w.remote_pane.clone()).or_default().meta = Some(m);
        Ok(())
    }

    /// Rewrite the copy as header + its last `tail` bytes.
    fn rotate(&self, m: &mut Meta) -> std::io::Result<()> {
        let bytes = fs::read(self.local_path(&m.name))?;
        let h = (m.header_len as usize).min(bytes.len());
        let kept = trim_to_tail(&bytes[h..], self.tail);
        write_atomic(&sessions_dir(&self.root), &format!("{}.jsonl", m.name), &[&bytes[..h], kept])?;
        let before = m.local_len;
        m.local_len = (h + kept.len()) as u64;
        self.log.log(&format!(
            "[{}] journal {}: trimmed {} from {before} to {} bytes",
            self.host, m.remote_pane, m.name, m.local_len
        ));
        Ok(())
    }

    fn queue_blobs(&mut self, journal: &str, body: &[u8]) {
        let dir = blobs_dir(&self.root);
        for hash in blob_refs(body) {
            if self.blob_failed.contains(&hash)
                || dir.join(&hash).exists()
                || self.blob_queue.iter().any(|(h, _)| h == &hash)
            {
                continue;
            }
            if let Some(remote) = remote_blob_path(journal, &hash) {
                self.blob_queue.push((hash, remote));
            }
        }
    }

    async fn fetch_blobs(&mut self) {
        let n = self.blob_queue.len().min(BLOBS_PER_TICK);
        let batch: Vec<(String, String)> = self.blob_queue.drain(..n).collect();
        let dir = blobs_dir(&self.root);
        for (hash, remote) in batch {
            match self.exec.run("journal blob", &blob_command(&remote, BLOB_MAX), READ_TIMEOUT_MS).await {
                Ok(bytes) => {
                    if let Err(e) = write_atomic(&dir, &hash, &[&bytes]) {
                        self.log.log(&format!("[{}] journal: cannot store blob {hash}: {e}", self.host));
                    }
                }
                // missing, too big, or unreachable: not retried this connection
                Err(_) => {
                    self.blob_failed.insert(hash);
                }
            }
        }
    }
}

/// The journal task for one live connection: one tick per status poll.
pub async fn run(mut journals: Journals, mut rx: watch::Receiver<Vec<Want>>) {
    while rx.changed().await.is_ok() {
        let wants = rx.borrow_and_update().clone();
        journals.tick(&wants).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(path: &str, ino: u64, offset: u64) -> Meta {
        Meta {
            host: "h".into(),
            remote_pane: "w1:p1".into(),
            remote_path: path.into(),
            name: "n".into(),
            ino,
            offset,
            header_len: 10,
            local_len: 100,
            skip_partial: false,
        }
    }

    #[test]
    fn decide_seeds_follows_and_reseeds() {
        let p = "/a/sessions/d/x.jsonl";
        let tail = 1000;
        // nothing to copy until the file exists
        assert_eq!(decide(None, p, None, tail, false), Action::Nothing);
        assert_eq!(decide(None, p, Some((50, 7)), tail, false), Action::Seed { size: 50, ino: 7 });
        let m = meta(p, 7, 500);
        assert_eq!(decide(Some(&m), p, Some((500, 7)), tail, true), Action::Nothing);
        assert_eq!(decide(Some(&m), p, Some((900, 7)), tail, true), Action::Append { from: 500, len: 400 });
        // replaced (new inode), truncated, damaged locally, other session, or too far behind
        assert_eq!(decide(Some(&m), p, Some((900, 8)), tail, true), Action::Seed { size: 900, ino: 8 });
        assert_eq!(decide(Some(&m), p, Some((400, 7)), tail, true), Action::Seed { size: 400, ino: 7 });
        assert_eq!(decide(Some(&m), p, Some((900, 7)), tail, false), Action::Seed { size: 900, ino: 7 });
        assert_eq!(decide(Some(&m), "/other.jsonl", Some((900, 7)), tail, true), Action::Seed { size: 900, ino: 7 });
        assert_eq!(decide(Some(&m), p, Some((1501, 7)), tail, true), Action::Seed { size: 1501, ino: 7 });
        assert_eq!(decide(Some(&m), p, Some((1500, 7)), tail, true), Action::Append { from: 500, len: 1000 });
    }

    #[test]
    fn cut_keeps_whole_rows_from_a_line_boundary() {
        // aligned start: everything up to the last newline, partial row left
        let c = cut(b"a\nbb\ncc", 100, false);
        assert_eq!(c, Cut { body: 0..5, next: 105, skip_partial: false });
        // mid-row start: the clipped head is dropped
        let c = cut(b"xx\nbb\ncc\n", 100, true);
        assert_eq!(c, Cut { body: 3..9, next: 109, skip_partial: false });
        // the byte before the tail was a newline: nothing real is dropped
        let c = cut(b"\nbb\n", 99, true);
        assert_eq!(c, Cut { body: 1..4, next: 103, skip_partial: false });
        // no complete row yet: nothing consumed
        let c = cut(b"partial", 100, false);
        assert_eq!(c, Cut { body: 0..0, next: 100, skip_partial: false });
        // inside a row longer than the window: skip it entirely, remember to
        let c = cut(b"inside-a-long-row", 100, true);
        assert_eq!(c, Cut { body: 0..0, next: 117, skip_partial: true });
    }

    #[test]
    fn seed_reply_parses_and_marks_mid_line_starts() {
        let r = parse_seed(b"6 20\nHEADR\nyy\nrow\n").unwrap();
        assert_eq!(r.header, b"HEADR\n");
        assert_eq!(r.rs, 20);
        assert!(r.mid_line);
        assert_eq!(r.data, b"yy\nrow\n");
        // the tail begins right after the header: a boundary, not mid-row
        let r = parse_seed(b"6 6\nHEADR\nrow\n").unwrap();
        assert!(!r.mid_line);
        assert!(parse_seed(b"9 9\nshort").is_none());
        assert!(parse_seed(b"garbage").is_none());
    }

    #[test]
    fn rotation_keeps_the_tail_from_a_row_boundary() {
        assert_eq!(trim_to_tail(b"aa\nbb\ncc\n", 100), b"aa\nbb\ncc\n");
        // cut lands mid-row: start at the next row
        assert_eq!(trim_to_tail(b"aa\nbb\ncc\n", 5), b"cc\n");
        // cut lands exactly on a boundary
        assert_eq!(trim_to_tail(b"aa\nbb\ncc\n", 6), b"bb\ncc\n");
        assert_eq!(trim_to_tail(b"aaaaaaaa\n", 3), b"");
    }

    #[test]
    fn blob_refs_are_found_and_deduplicated() {
        let h = "ab".repeat(32);
        let data = format!(r#"{{"data":"blob:sha256:{h}"}} x blob:sha256:{h} blob:sha256:nothex"#);
        assert_eq!(blob_refs(data.as_bytes()), vec![h.clone()]);
        assert_eq!(
            remote_blob_path("/home/u/.omp/agent/sessions/--d--/t_u.jsonl", &h).unwrap(),
            format!("/home/u/.omp/agent/blobs/{h}")
        );
    }

    #[test]
    fn stats_parse_in_order_with_gaps() {
        assert_eq!(parse_stats(b"10 5\n- -\n7 9\n", 3), vec![Some((10, 5)), None, Some((7, 9))]);
        assert_eq!(parse_stats(b"10 5\n", 2), vec![Some((10, 5)), None]);
    }

    #[test]
    fn copy_names_are_short_stable_and_distinct() {
        let a = copy_name("vibe1", "w1:p1", "/s/x.jsonl");
        assert_eq!(a.len(), 12);
        assert_eq!(a, copy_name("vibe1", "w1:p1", "/s/x.jsonl"));
        assert_ne!(a, copy_name("vibe1", "w1:p2", "/s/x.jsonl"));
        assert_ne!(a, copy_name("latz", "w1:p1", "/s/x.jsonl"));
        // the default hub layout fits a herdr token
        let p = format!("/home/glichez/.local/state/herdr-mirror/journals/sessions/{a}.jsonl");
        assert!(p.len() <= TOKEN_VALUE_MAX, "{}", p.len());
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hm-journal-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn write_meta(root: &Path, m: &Meta, body: &[u8]) {
        for d in [sessions_dir(root), meta_dir(root), blobs_dir(root)] {
            ensure_dir(&d).unwrap();
        }
        fs::write(sessions_dir(root).join(format!("{}.jsonl", m.name)), body).unwrap();
        fs::write(meta_dir(root).join(format!("{}.json", m.name)), serde_json::to_vec(m).unwrap()).unwrap();
    }

    #[test]
    fn prune_drops_unconfigured_hosts_orphans_and_temp_files() {
        let state = tmp("prune");
        let root = root(&state);
        let mut keep = meta("/s/a.jsonl", 1, 1);
        keep.name = "keep".into();
        let mut gone = meta("/s/b.jsonl", 1, 1);
        gone.name = "gone".into();
        gone.host = "removed-host".into();
        write_meta(&root, &keep, b"x");
        write_meta(&root, &gone, b"x");
        fs::write(sessions_dir(&root).join("orphan.jsonl"), b"x").unwrap();
        fs::write(sessions_dir(&root).join(".keep.jsonl.tmp"), b"x").unwrap();
        prune_foreign(&state, &HashSet::from(["h".to_string()]), &Logger::new(&state, false));
        let names: HashSet<String> = fs::read_dir(sessions_dir(&root))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, HashSet::from(["keep.jsonl".to_string()]));
        assert!(!meta_dir(&root).join("gone.json").exists());
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn gc_keeps_referenced_and_young_blobs() {
        let state = tmp("gc");
        let root = root(&state);
        let used = "a".repeat(64);
        let unused = "b".repeat(64);
        let mut m = meta("/s/a.jsonl", 1, 1);
        m.name = "c".into();
        write_meta(&root, &m, format!("{{\"img\":\"blob:sha256:{used}\"}}\n").as_bytes());
        fs::write(blobs_dir(&root).join(&used), b"1").unwrap();
        fs::write(blobs_dir(&root).join(&unused), b"2").unwrap();
        let log = Logger::new(&state, false);
        gc_blobs(&state, &log);
        // too young to judge
        assert!(blobs_dir(&root).join(&unused).exists());
        let old = SystemTime::now() - Duration::from_secs(3600);
        fs::File::options().write(true).open(blobs_dir(&root).join(&unused)).unwrap().set_modified(old).unwrap();
        fs::File::options().write(true).open(blobs_dir(&root).join(&used)).unwrap().set_modified(old).unwrap();
        gc_blobs(&state, &log);
        assert!(!blobs_dir(&root).join(&unused).exists());
        assert!(blobs_dir(&root).join(&used).exists());
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn local_files_are_private() {
        let state = tmp("perm");
        let dir = sessions_dir(&root(&state));
        ensure_dir(&dir).unwrap();
        write_atomic(&dir, "x.jsonl", &[b"a\n"]).unwrap();
        append_file(&dir.join("x.jsonl"), b"b\n").unwrap();
        assert_eq!(fs::read(dir.join("x.jsonl")).unwrap(), b"a\nb\n");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("x.jsonl")), 0o600);
        let _ = fs::remove_dir_all(&state);
    }

    // --- the real remote scripts and the engine, against local files ---
    //
    // `RemoteExec::local()` runs each command through `sh -c`, the same line
    // the remote login shell would get over ssh.

    fn row(i: usize) -> String {
        format!("{{\"type\":\"message\",\"id\":\"r{i:04}\",\"text\":\"{}\"}}\n", "x".repeat(60))
    }

    struct Lab {
        state: PathBuf,
        remote: PathBuf,
        published: Published,
    }

    impl Lab {
        fn new(tag: &str) -> Lab {
            let state = tmp(tag);
            let remote = state.join("remote/agent/sessions/--d--/2026-01-01T00-00-00-000Z_u.jsonl");
            fs::create_dir_all(remote.parent().unwrap()).unwrap();
            fs::write(&remote, b"{\"type\":\"session\",\"id\":\"u\"}\n").unwrap();
            Lab { state, remote, published: new_published() }
        }
        fn push(&self, from: usize, to: usize) {
            let mut f = fs::OpenOptions::new().append(true).open(&self.remote).unwrap();
            for i in from..to {
                f.write_all(row(i).as_bytes()).unwrap();
            }
        }
        fn want(&self, status: &str) -> Want {
            Want {
                remote_pane: "w1:p1".into(),
                remote_path: self.remote.display().to_string(),
                status: Some(status.into()),
                state_seq: None,
            }
        }
        fn open(&self, tail: u64) -> Journals {
            Journals::open("h", &self.state, tail, RemoteExec::local(), self.published.clone(), Logger::new(&self.state, false))
        }
        fn copy(&self) -> Option<Vec<u8>> {
            let path = self.published.lock().unwrap().get("w1:p1").cloned()?;
            fs::read(path).ok()
        }
    }

    /// The copy is the header plus a row-aligned suffix of the remote file.
    fn assert_tail_of(copy: &[u8], remote: &[u8], max_body: usize) {
        let header_len = remote.iter().position(|&c| c == b'\n').unwrap() + 1;
        assert_eq!(&copy[..header_len], &remote[..header_len], "header row kept");
        let body = &copy[header_len..];
        assert!(body.len() <= max_body, "body {} > {max_body}", body.len());
        assert!(body.is_empty() || body.ends_with(b"\n"));
        assert!(body.is_empty() || body.starts_with(b"{\"type\":\"message\""), "starts on a row");
        let hay = String::from_utf8_lossy(remote);
        assert!(hay.contains(&*String::from_utf8_lossy(body)), "a contiguous run of remote rows");
    }

    #[tokio::test]
    async fn engine_seeds_follows_rotates_reseeds_and_prunes() {
        let lab = Lab::new("engine");
        let rows = row(0).len();
        let tail = (rows * 10 + rows / 2) as u64; // ten and a half rows
        lab.push(0, 40);
        let mut j = lab.open(tail);
        let t0 = Instant::now();

        // seed: header + the last whole rows that fit, from a row boundary
        j.tick_at(&[lab.want("working")], t0).await;
        let remote = fs::read(&lab.remote).unwrap();
        let copy = lab.copy().expect("published once written");
        assert_tail_of(&copy, &remote, tail as usize);
        assert!(String::from_utf8_lossy(&copy).contains("r0039"));
        assert!(!String::from_utf8_lossy(&copy).contains("r0029"));

        // follow: appends land in order, a half-written row waits for its newline
        lab.push(40, 45);
        fs::OpenOptions::new().append(true).open(&lab.remote).unwrap().write_all(b"{\"type\":\"mess").unwrap();
        j.tick_at(&[lab.want("working")], t0).await;
        let copy = lab.copy().unwrap();
        assert!(String::from_utf8_lossy(&copy).ends_with(&row(44)));
        fs::OpenOptions::new().append(true).open(&lab.remote).unwrap().write_all(&row(45).as_bytes()[13..]).unwrap();
        j.tick_at(&[lab.want("working")], t0).await;
        let copy = lab.copy().unwrap();
        assert!(String::from_utf8_lossy(&copy).ends_with(&format!("{}{}", row(44), row(45))));

        // rotation: past header + 2x tail the copy is cut back to header + tail
        for k in 0..3 {
            lab.push(46 + k * 8, 54 + k * 8);
            j.tick_at(&[lab.want("working")], t0).await;
            let copy = lab.copy().unwrap();
            let header_len = remote.iter().position(|&c| c == b'\n').unwrap() + 1;
            assert!(copy.len() <= header_len + 2 * tail as usize);
            assert_tail_of(&copy, &fs::read(&lab.remote).unwrap(), 2 * tail as usize);
        }

        // an idle pane is not even stat'ed until the recheck interval
        lab.push(70, 71);
        j.tick_at(&[lab.want("idle")], t0).await; // status changed: looked at
        lab.push(71, 72);
        j.tick_at(&[lab.want("idle")], t0 + Duration::from_secs(1)).await;
        assert!(!String::from_utf8_lossy(&lab.copy().unwrap()).contains("r0071"));
        j.tick_at(&[lab.want("idle")], t0 + IDLE_RECHECK + Duration::from_secs(1)).await;
        assert!(String::from_utf8_lossy(&lab.copy().unwrap()).contains("r0071"));

        // replaced file (new inode): re-seeded from the new content
        let fresh = lab.remote.with_extension("new");
        fs::write(&fresh, b"{\"type\":\"session\",\"id\":\"v\"}\n").unwrap();
        fs::rename(&fresh, &lab.remote).unwrap();
        lab.push(100, 103);
        j.tick_at(&[lab.want("working")], t0 + Duration::from_secs(60)).await;
        let copy = lab.copy().unwrap();
        assert_eq!(copy, fs::read(&lab.remote).unwrap());

        // a stop mid-append leaves torn bytes: reopening cuts them off
        let path = lab.published.lock().unwrap()["w1:p1"].clone();
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"torn").unwrap();
        drop(j);
        lab.published.lock().unwrap().clear();
        let mut j = lab.open(tail);
        assert_eq!(lab.copy().unwrap(), fs::read(&lab.remote).unwrap(), "republished, torn tail cut");

        // session gone: kept through the grace period, then deleted and unpublished
        let t1 = t0 + Duration::from_secs(120);
        j.tick_at(&[], t1).await;
        assert!(lab.copy().is_some());
        j.tick_at(&[], t1 + ABSENT_GRACE).await;
        assert!(lab.published.lock().unwrap().is_empty());
        assert!(!Path::new(&path).exists());
        assert_eq!(fs::read_dir(meta_dir(&root(&lab.state))).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&lab.state);
    }

    #[tokio::test]
    async fn engine_reseeds_on_a_new_session_path_and_fetches_blobs() {
        let lab = Lab::new("blobs");
        let hash = "c".repeat(64);
        let blobs = lab.remote.parent().unwrap().parent().unwrap().parent().unwrap().join("blobs");
        fs::create_dir_all(&blobs).unwrap();
        fs::write(blobs.join(&hash), b"PNGDATA").unwrap();
        let missing = "d".repeat(64);
        fs::OpenOptions::new()
            .append(true)
            .open(&lab.remote)
            .unwrap()
            .write_all(format!("{{\"img\":\"blob:sha256:{hash}\"}}\n{{\"img\":\"blob:sha256:{missing}\"}}\n").as_bytes())
            .unwrap();
        let mut j = lab.open(1 << 20);
        j.tick(&[lab.want("idle")]).await;
        let stored = blobs_dir(&root(&lab.state));
        assert_eq!(fs::read(stored.join(&hash)).unwrap(), b"PNGDATA");
        assert!(!stored.join(&missing).exists(), "a missing blob is skipped");
        let first = lab.published.lock().unwrap()["w1:p1"].clone();

        // /new: same pane, another journal — the old copy goes, a new one is seeded
        let other = lab.remote.with_file_name("2026-01-02T00-00-00-000Z_v.jsonl");
        fs::write(&other, b"{\"type\":\"session\",\"id\":\"v\"}\n").unwrap();
        let want = Want { remote_path: other.display().to_string(), ..lab.want("idle") };
        j.tick(&[want]).await;
        let second = lab.published.lock().unwrap()["w1:p1"].clone();
        assert_ne!(first, second);
        assert!(!Path::new(&first).exists());
        assert_eq!(fs::read(&second).unwrap(), fs::read(&other).unwrap());
        let _ = fs::remove_dir_all(&lab.state);
    }

    /// omp 18 opens a journal with an in-place `title` row, then the session
    /// row: the header keeps both, so a cut copy still names its session.
    #[tokio::test]
    async fn header_runs_through_the_session_row() {
        let lab = Lab::new("title");
        let head = format!("{{\"type\":\"title\",\"pad\":\"{}\"}}\n{{\"type\":\"session\",\"id\":\"u\"}}\n", " ".repeat(200));
        fs::write(&lab.remote, head.as_bytes()).unwrap();
        lab.push(0, 40);
        let mut j = lab.open((row(0).len() * 5) as u64);
        j.tick(&[lab.want("idle")]).await;
        let copy = String::from_utf8(lab.copy().unwrap()).unwrap();
        assert!(copy.starts_with(&head), "both header rows kept");
        assert!(copy.ends_with(&row(39)));
        assert!(!copy.contains("r0030"));
        let _ = fs::remove_dir_all(&lab.state);
    }

    #[tokio::test]
    async fn seed_without_a_header_row_and_with_one_huge_row() {
        let lab = Lab::new("huge");
        // a first line longer than HEADER_MAX is not a header
        let big = format!("{{\"type\":\"session\",\"pad\":\"{}\"}}\n", "p".repeat(HEADER_MAX as usize));
        fs::write(&lab.remote, big.as_bytes()).unwrap();
        lab.push(0, 3);
        let mut j = lab.open(1 << 20);
        j.tick(&[lab.want("idle")]).await;
        let copy = lab.copy().unwrap();
        assert_eq!(copy, fs::read(&lab.remote).unwrap(), "small enough to copy whole");

        // tail smaller than the one row it lands in: the row is skipped, the
        // rows after it follow
        let lab = Lab::new("huge2");
        fs::OpenOptions::new()
            .append(true)
            .open(&lab.remote)
            .unwrap()
            .write_all(format!("{{\"long\":\"{}\"}}\n", "q".repeat(5000)).as_bytes())
            .unwrap();
        let mut j = lab.open(1000);
        j.tick(&[lab.want("idle")]).await;
        let copy = lab.copy().unwrap();
        assert_eq!(copy, b"{\"type\":\"session\",\"id\":\"u\"}\n");
        lab.push(0, 2);
        j.tick(&[lab.want("working")]).await;
        assert_eq!(lab.copy().unwrap(), format!("{{\"type\":\"session\",\"id\":\"u\"}}\n{}{}", row(0), row(1)).as_bytes());
        let _ = fs::remove_dir_all(&lab.state);
    }
}
