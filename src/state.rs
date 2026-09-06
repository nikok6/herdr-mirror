// The persisted per-host id map — the heart of reconciliation.
//
// remote id → { local id, tombstone, seq, reported }. A tombstone means "the
// user closed this mirror" — never recreate it until restore. Absence of a
// remote id means "remote went away" — close the mirror. Restart-idempotent.
// The camelCase JSON shape matches the TS implementation so an existing
// <host>-map.json carries over.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::util::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneEntry {
    pub local_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tombstone: Option<bool>,
    #[serde(default)]
    pub seq: u64,
    /// agent label last reported onto this pane; must be explicitly released
    /// when the remote agent goes away, or it sticks forever
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reported: Option<String>,
    /// ssh stream-pool slot this pane's data stream is assigned to (see
    /// `stream_pool`). `None` on every pane when pooling is off (the
    /// default), and on any pane created before this field existed — a
    /// pre-pooling `*-map.json` has no `streamSlot` key at all and deserializes
    /// here exactly as it always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_slot: Option<u32>,
    /// Remote workspace this pane belongs to, set once when the pane is
    /// first created/observed and never moved after — a pane doesn't change
    /// workspace. Exists so a pane can still be recognized as belonging to a
    /// (now) tombstoned workspace after it goes missing from a later,
    /// transient/partial remote snapshot: without this, that association is
    /// only ever visible in the exact pass that observed both the pane and
    /// its workspace_id together, and `stream_pool_candidates`'s existing
    /// "keep a once-off-missing pane's slot" restraint would otherwise let
    /// it keep consuming pool capacity indefinitely once its workspace is
    /// gone. `None` on any pane created before this field existed — a
    /// pre-existing `*-map.json` has no `workspaceId` key and deserializes
    /// here exactly as it always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

impl PaneEntry {
    pub fn is_tombstoned(&self) -> bool {
        self.tombstone == Some(true)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WsEntry {
    pub local_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tombstone: Option<bool>,
    /// the auto-created root tab of a fresh mirror workspace; consumed by the
    /// first remote tab's layout.apply so it doesn't stack an extra tab
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_tab_local_id: Option<String>,
    /// remote label as of the last converge — distinguishes "remote renamed"
    /// (remote wins, restamp local) from "user renamed the mirror locally"
    /// (push the rename to the remote instead of stomping it)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_remote_label: Option<String>,
}

impl WsEntry {
    pub fn is_tombstoned(&self) -> bool {
        self.tombstone == Some(true)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TabEntry {
    pub local_id: String,
    /// remote label as of the last converge, exactly as on `WsEntry`: it is
    /// what tells "remote renamed" apart from "user renamed the mirror tab"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_remote_label: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostState {
    #[serde(default)]
    pub workspaces: BTreeMap<String, WsEntry>,
    #[serde(default)]
    pub tabs: BTreeMap<String, TabEntry>,
    #[serde(default)]
    pub panes: BTreeMap<String, PaneEntry>,
    /// remote object ids (ws/tab/pane) seen in the previous converge. A mirror is
    /// only closed on snapshot-absence when the object was absent last pass too,
    /// so a remote that reconnects mid-restore doesn't mass-close mirrors.
    #[serde(default)]
    pub prev_remote_ids: std::collections::BTreeSet<String>,
    /// last split ratio both sides agreed on, keyed `<remote tab id>|<path>`
    /// (see layout_sync::path_key). This is the base of the three-way merge
    /// that makes ratio sync two-way: without it a converge can see that the
    /// two sides differ but not which one was resized, so it has to pick a
    /// permanent winner and revert the other side's drag.
    #[serde(default)]
    pub ratios: BTreeMap<String, f64>,
}

/// Marker for `hide`: this host's mirrors are off the sidebar until `show`.
///
/// Deliberately its OWN file rather than a field on `HostState`. The map file is
/// load-modify-written by the daemon, by every CLI subcommand, and by converge
/// around a pass that spans dozens of awaits, with no lock anywhere — so a flag
/// living inside it is silently reset by whoever saves last, and `hide` reports
/// success having done nothing. A marker file has no such race: it is written by
/// one process and only ever read by the others. Same shape as `daemon.paused`.
pub fn hidden_path(state_dir: &Path, host: &str) -> PathBuf {
    let (dir, stem) = crate::util::host_artifact_base(state_dir, host);
    dir.join(format!("{stem}.hidden"))
}

pub fn is_hidden(state_dir: &Path, host: &str) -> bool {
    hidden_path(state_dir, host).exists()
}

/// Returns the error rather than swallowing it: this one write gates the whole
/// feature, so a read-only state dir or a host name that is not a single path
/// component would otherwise make `hide` claim success forever while nothing
/// ever acts on it.
pub fn set_hidden(state_dir: &Path, host: &str, hidden: bool) -> std::io::Result<()> {
    let path = hidden_path(state_dir, host);
    if hidden {
        std::fs::write(path, "")
    } else {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

/// A one-line notice for one specific mirror pane to show.
///
/// The interception runs in its own short-lived process and closes a plain
/// local shell — nothing of ours is in that pane to draw with, and herdr has no
/// API to write into someone else's pane. But the pane the user was *looking
/// at* when they pressed the key is a live mirror with a streamer in it, and a
/// streamer can paint its own status row. So the notice is addressed to that
/// pane by its local id, and it lands in the same row as "reconnecting in 10s".
///
/// Keyed by pane id on purpose: an earlier version left the note unaddressed
/// and the next streamer to start collected it, which was the replacement pane,
/// reporting a move that had already finished.
fn pane_hint_path(state_dir: &Path, local_pane_id: &str) -> PathBuf {
    state_dir.join(format!(
        ".hint-{}",
        crate::util::sane_component(local_pane_id)
    ))
}

pub fn set_pane_hint(state_dir: &Path, local_pane_id: &str, msg: &str) {
    let _ = std::fs::create_dir_all(state_dir);
    let _ = std::fs::write(pane_hint_path(state_dir, local_pane_id), msg);
}

/// Read and consume this pane's notice, if any.
pub fn take_pane_hint(state_dir: &Path, local_pane_id: &str) -> Option<String> {
    let path = pane_hint_path(state_dir, local_pane_id);
    let msg = std::fs::read_to_string(&path).ok()?;
    // A notice is about something that just happened. One left behind by a
    // streamer that died before collecting it is stale, and showing it later
    // would report a close the user has long since forgotten. Stat before the
    // unlink: afterwards there is nothing left to ask.
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .and_then(|t| t.elapsed().map_err(std::io::Error::other))
        .is_ok_and(|e| e < std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&path);
    let msg = msg.trim().to_string();
    (fresh && !msg.is_empty()).then_some(msg)
}

pub fn state_path(state_dir: &Path, host: &str) -> PathBuf {
    let (dir, stem) = crate::util::host_artifact_base(state_dir, host);
    dir.join(format!("{stem}-map.json"))
}

pub fn load_state(state_dir: &Path, host: &str) -> HostState {
    std::fs::read_to_string(state_path(state_dir, host))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write-then-rename, never an in-place truncate: `load_state` (and
/// `status`, which reads this same file with no lock at all) must only ever
/// see either the complete old content or the complete new content, never a
/// partial write torn by a crash or a racing reader. The temp name is unique
/// per call (pid + a nanosecond nonce), so a write that fails cleans up
/// exactly the one file it created — never a glob over the state dir, which
/// could delete a concurrent writer's own in-flight temp file. Created with
/// mode 0600 regardless of umask: a pane's remote target/command in this
/// state is not secret from its own owner, but there is no reason to leave it
/// group/world-readable either, and umask can only narrow 0600 further, never
/// widen it.
pub fn save_state(state_dir: &Path, host: &str, state: &HostState) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::create_dir_all(state_dir)?;
    let (dir, stem) = crate::util::host_artifact_base(state_dir, host);
    let final_path = dir.join(format!("{stem}-map.json"));
    let json = serde_json::to_string_pretty(state)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Same directory as `final_path` — required for the rename below to be
    // atomic (a cross-directory rename is not guaranteed to be, and for an
    // unsafe host name `dir` is `.h/`, not `state_dir` itself).
    let tmp_path = dir.join(format!(
        ".{stem}-map.json.tmp-{}-{nonce}",
        std::process::id()
    ));
    let result: std::io::Result<()> = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp_path, &final_path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    Ok(())
}

/// How long a converge/`once`/restore attempt waits for another holder of
/// this host's state lock before giving up. Bounds contention against a
/// wedged peer; the lock is held for as long as the transaction actually
/// needs once acquired (this only bounds the WAIT to acquire it).
pub const HOST_LOCK_BUDGET: Duration = Duration::from_secs(30);

fn host_lock_path(state_dir: &Path, host: &str) -> PathBuf {
    let (dir, stem) = crate::util::host_artifact_base(state_dir, host);
    dir.join(format!("{stem}.state.lock"))
}

/// Acquire this host's per-host state-transaction lock: every code path that
/// loads, mutates, and saves `<host>-map.json` (converge, status-flush,
/// hide/show, mark-unknown, teardown, `once`) must hold this for the WHOLE
/// load → mutate → save span, or the daemon and a separately-invoked `once`
/// can each load a stale copy and one's save silently discards the other's
/// changes. Distinct from the ssh master-socket lock in `remote.rs` — see
/// `filelock`'s module doc for the required acquisition order between them
/// (this one first, never the reverse).
pub async fn lock_host(state_dir: &Path, host: &str) -> Result<crate::filelock::FileLock> {
    std::fs::create_dir_all(state_dir)?;
    crate::filelock::acquire_async(&host_lock_path(state_dir, host), HOST_LOCK_BUDGET).await
}

/// Same lock as `lock_host`, blocking the calling thread — for the rare
/// synchronous call site (`cmd_restore`) that has no tokio runtime to run
/// `lock_host` on. Bounded by the same `HOST_LOCK_BUDGET`, not an unbounded
/// wait: a stuck holder must fail this loudly rather than hang the CLI.
pub fn lock_host_blocking(state_dir: &Path, host: &str) -> Result<crate::filelock::FileLock> {
    std::fs::create_dir_all(state_dir)?;
    crate::filelock::acquire_blocking(&host_lock_path(state_dir, host), HOST_LOCK_BUDGET)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape the TS implementation writes must round-trip.
    #[test]
    fn ts_state_shape_roundtrips() {
        let ts = r#"{
 "workspaces": {
  "w9": { "localId": "w1234", "rootTabLocalId": "t99" },
  "wB": { "localId": "w5678", "tombstone": true }
 },
 "tabs": { "w9:t1": { "localId": "t42" } },
 "panes": {
  "w9:p1": { "localId": "w1234:p1", "seq": 12, "reported": "claude" },
  "wB:p1": { "localId": "w5678:p1", "tombstone": true, "seq": 3 }
 }
}"#;
        let state: HostState = serde_json::from_str(ts).unwrap();
        assert_eq!(state.workspaces["w9"].local_id, "w1234");
        assert_eq!(
            state.workspaces["w9"].root_tab_local_id.as_deref(),
            Some("t99")
        );
        assert!(state.workspaces["wB"].is_tombstoned());
        // a tab mapped before label history existed loads with none, which the
        // resolver reads as "remote wins once"
        assert_eq!(state.tabs["w9:t1"].last_remote_label, None);
        assert_eq!(state.panes["w9:p1"].seq, 12);
        assert_eq!(state.panes["w9:p1"].reported.as_deref(), Some("claude"));
        assert!(state.panes["wB:p1"].is_tombstoned());

        let out = serde_json::to_string(&state).unwrap();
        let reparsed: HostState = serde_json::from_str(&out).unwrap();
        assert_eq!(reparsed.panes["w9:p1"].local_id, "w1234:p1");
        assert!(out.contains("localId"));
        assert!(out.contains("rootTabLocalId"));
        // absent options stay absent
        assert!(!out.contains("\"reported\":null"));
    }

    /// A pre-pooling (v0.4.1) `*-map.json` has no `streamSlot` key on any
    /// pane at all — this must load as `None`, not fail to parse.
    #[test]
    fn pre_pooling_state_loads_with_no_stream_slot() {
        let v0_4_1 = r#"{"panes": {"w9:p1": {"localId": "w1234:p1", "seq": 1}}}"#;
        let state: HostState = serde_json::from_str(v0_4_1).unwrap();
        assert_eq!(state.panes["w9:p1"].stream_slot, None);
    }

    /// A `*-map.json` written before `workspaceId` existed (v0.4.1, or this
    /// feature's own earlier rounds) has no such key on any pane — this must
    /// load as `None`, not fail to parse, exactly like `streamSlot` above.
    #[test]
    fn pre_workspace_tracking_state_loads_with_no_workspace_id() {
        let legacy = r#"{"panes": {"w9:p1": {"localId": "w1234:p1", "seq": 1, "streamSlot": 2}}}"#;
        let state: HostState = serde_json::from_str(legacy).unwrap();
        assert_eq!(state.panes["w9:p1"].workspace_id, None);
        assert_eq!(state.panes["w9:p1"].stream_slot, Some(2));
    }

    /// A `""`/`"."`/`".."`-named host's `*-map.json` written by a
    /// pre-this-round daemon (which already put it at exactly this path,
    /// since these names were never actually unsafe — see
    /// `dot_and_empty_host_names_stay_at_the_root_like_v0_4_1`) must still
    /// load, at the same path, on this version.
    #[test]
    fn legacy_state_for_a_dot_or_empty_host_name_still_loads() {
        for host in ["", ".", ".."] {
            let dir = tmpdir("legacy-dot-load");
            let path = state_path(&dir, host);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                r#"{"panes": {"w9:p1": {"localId": "w1234:p1", "seq": 1}}}"#,
            )
            .unwrap();
            let state = load_state(&dir, host);
            assert_eq!(state.panes["w9:p1"].local_id, "w1234:p1", "{host:?}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// `streamSlot` round-trips like every other optional field: present when
    /// assigned, omitted (not `null`) when not, so an unpooled host's state
    /// file stays untouched by the feature's existence.
    #[test]
    fn stream_slot_serializes_only_when_assigned() {
        let mut entry = PaneEntry {
            local_id: "l1".into(),
            tombstone: None,
            seq: 0,
            reported: None,
            stream_slot: None,
            workspace_id: None,
        };
        let out = serde_json::to_string(&entry).unwrap();
        assert!(!out.contains("streamSlot"), "{out}");

        entry.stream_slot = Some(3);
        let out = serde_json::to_string(&entry).unwrap();
        assert!(out.contains("\"streamSlot\":3"), "{out}");
        let reparsed: PaneEntry = serde_json::from_str(&out).unwrap();
        assert_eq!(reparsed.stream_slot, Some(3));
    }

    /// `workspaceId` round-trips the same way: present once observed,
    /// omitted (not `null`) before that.
    #[test]
    fn workspace_id_serializes_only_when_known() {
        let mut entry = PaneEntry {
            local_id: "l1".into(),
            tombstone: None,
            seq: 0,
            reported: None,
            stream_slot: None,
            workspace_id: None,
        };
        let out = serde_json::to_string(&entry).unwrap();
        assert!(!out.contains("workspaceId"), "{out}");

        entry.workspace_id = Some("w1".into());
        let out = serde_json::to_string(&entry).unwrap();
        assert!(out.contains("\"workspaceId\":\"w1\""), "{out}");
        let reparsed: PaneEntry = serde_json::from_str(&out).unwrap();
        assert_eq!(reparsed.workspace_id, Some("w1".into()));
    }

    /// Same nonce-collision fix as `filelock::tests::test_path`: pid + nanos
    /// alone is not enough under a genuinely parallel full-suite run.
    fn tmpdir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let salt = &counter as *const u64 as usize;
        let unique = crate::util::short_hash(&format!(
            "{}-{nanos}-{counter}-{salt:x}",
            std::process::id()
        ));
        let dir = std::env::temp_dir().join(format!("hm-state-{tag}-{unique}"));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn save_state_round_trips_and_leaves_no_temp_file_behind() {
        let dir = tmpdir("save-clean");
        let mut state = HostState::default();
        state.panes.insert(
            "p1".into(),
            PaneEntry {
                local_id: "l1".into(),
                tombstone: None,
                seq: 0,
                reported: None,
                stream_slot: Some(2),
                workspace_id: None,
            },
        );
        save_state(&dir, "h", &state).unwrap();
        let reloaded = load_state(&dir, "h");
        assert_eq!(reloaded.panes["p1"].stream_slot, Some(2));

        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!names.iter().any(|n| n.contains(".tmp-")), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_state_writes_a_private_map_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tmpdir("save-mode");
        save_state(&dir, "h", &HostState::default()).unwrap();
        let mode = std::fs::metadata(state_path(&dir, "h"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the renamed map file must keep the temp file's private mode"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed save (here: the final path is sabotaged into a directory, so
    /// the atomic rename itself fails) must clean up exactly its own temp
    /// file and never touch anything else in the state dir.
    #[test]
    fn save_state_cleans_up_its_own_temp_file_on_failure() {
        let dir = tmpdir("save-fail");
        std::fs::create_dir_all(state_path(&dir, "h")).unwrap(); // sabotage: rename onto a dir fails
        assert!(save_state(&dir, "h", &HostState::default()).is_err());
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!names.iter().any(|n| n.contains(".tmp-")), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The property atomicity exists for: a reader must never observe a
    /// torn/partial write, only ever the complete old or complete new
    /// content — proven by racing a reader against many rapid saves.
    #[test]
    fn concurrent_readers_never_observe_a_torn_write() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = tmpdir("torn-write");
        save_state(&dir, "h", &HostState::default()).unwrap();
        let path = state_path(&dir, "h");
        let stop = Arc::new(AtomicBool::new(false));
        let reader_path = path.clone();
        let reader_stop = stop.clone();
        let reader = std::thread::spawn(move || {
            while !reader_stop.load(Ordering::Relaxed) {
                if let Ok(content) = std::fs::read_to_string(&reader_path) {
                    assert!(
                        serde_json::from_str::<HostState>(&content).is_ok(),
                        "torn read: {content}"
                    );
                }
            }
        });
        for i in 0..200 {
            let mut s = HostState::default();
            s.panes.insert(
                format!("p{i}"),
                PaneEntry {
                    local_id: format!("l{i}"),
                    tombstone: None,
                    seq: i,
                    reported: None,
                    stream_slot: None,
                    workspace_id: None,
                },
            );
            save_state(&dir, "h", &s).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The actual correctness property `lock_host`/`lock_host_blocking` exist
    /// for: two concurrent holders of the same host's lock must never be
    /// inside the locked section together.
    #[tokio::test]
    async fn lock_host_excludes_concurrent_holders() {
        let dir = tmpdir("host-lock");
        let _first = lock_host(&dir, "h").await.unwrap();
        let second = tokio::time::timeout(Duration::from_millis(200), lock_host(&dir, "h")).await;
        assert!(
            second.is_err() || second.unwrap().is_err(),
            "a second holder must not acquire while the first is held"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression for the hide/show race: `apply_hidden` (mirror.rs) peeks
    /// the hidden marker lock-free, then must re-check it AFTER acquiring
    /// the host lock before deciding to close anything — `clear_hidden`
    /// (remote_action.rs, `show`) clearing the marker under the same lock
    /// must never be invisible to an `apply_hidden` pass already queued
    /// behind it. This exercises the exact lock/re-check protocol both call
    /// sites use, with the real `lock_host`/`is_hidden`/`set_hidden`
    /// primitives — no ApiClient is needed for this decision, only for the
    /// RPC calls after it, which is why this is deterministic here.
    #[tokio::test]
    async fn hidden_marker_race_is_resolved_by_lock_and_recheck() {
        let dir = tmpdir("hide-show-race");
        set_hidden(&dir, "h", true).unwrap();

        // apply_hidden's lock-free peek, before it ever tries to acquire the lock
        assert!(
            is_hidden(&dir, "h"),
            "peek must see the marker before the race plays out"
        );

        // `show` wins the lock first and holds it while it clears the marker
        let show_lock = lock_host(&dir, "h").await.unwrap();
        let apply_hidden_attempt = tokio::spawn({
            let dir = dir.clone();
            async move {
                let _lock = lock_host(&dir, "h").await.unwrap();
                // re-check, exactly like apply_hidden does once it holds the lock
                is_hidden(&dir, "h")
            }
        });
        // give the spawned attempt a moment to start waiting on the lock `show` holds
        tokio::time::sleep(Duration::from_millis(30)).await;
        set_hidden(&dir, "h", false).unwrap(); // show's mutation, still under show_lock
        drop(show_lock); // release — only now can the queued attempt proceed

        let rechecked_hidden = apply_hidden_attempt.await.unwrap();
        assert!(
            !rechecked_hidden,
            "the post-lock re-check must see show's clear, not the stale peek"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lock_host_blocking_and_lock_host_use_the_same_lock_file() {
        let dir = tmpdir("host-lock-shared");
        let _held = lock_host_blocking(&dir, "h").unwrap();
        // a nonblocking attempt on the identical path must see it as taken
        assert!(crate::filelock::try_lock_nb(&host_lock_path(&dir, "h"))
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: a host name is a user-chosen TOML table key, not a
    /// validated filename. Every host-derived path must stay contained
    /// under the private `.h/` subdirectory for a name that is
    /// `/`-separated — a raw `/` is the one thing no suffix can
    /// neutralize, unlike `.`/`..`/empty (see
    /// `dot_and_empty_host_names_stay_at_the_root_like_v0_4_1`).
    #[test]
    fn host_derived_paths_stay_contained_for_unsafe_host_names() {
        let dir = tmpdir("contain");
        let hosts_dir = crate::util::unsafe_host_artifacts_dir(&dir);
        for unsafe_name in ["../../escape", "a/b", "/etc/passwd"] {
            for path in [
                hidden_path(&dir, unsafe_name),
                state_path(&dir, unsafe_name),
                host_lock_path(&dir, unsafe_name),
            ] {
                assert_eq!(
                    path.parent(),
                    Some(hosts_dir.as_path()),
                    "{unsafe_name} -> {}",
                    path.display()
                );
                assert!(
                    path.starts_with(&dir),
                    "{unsafe_name} -> {} must stay within state_dir",
                    path.display()
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The private `.h/` subdirectory itself must be created mode 0700,
    /// not left to whatever umask the process happens to run under.
    #[test]
    fn unsafe_host_artifacts_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tmpdir("contain-mode");
        let _ = hidden_path(&dir, "a/b"); // side effect: creates .h/
        let hosts_dir = crate::util::unsafe_host_artifacts_dir(&dir);
        let mode = std::fs::metadata(&hosts_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ordinary host names must keep the exact paths every existing install
    /// already uses on disk — only a name that could otherwise escape or
    /// nest outside `state_dir` may move.
    #[test]
    fn host_derived_paths_are_unchanged_for_ordinary_host_names() {
        let dir = tmpdir("no-migrate");
        assert_eq!(hidden_path(&dir, "azure"), dir.join("azure.hidden"));
        assert_eq!(state_path(&dir, "rdev"), dir.join("rdev-map.json"));
        assert_eq!(host_lock_path(&dir, "rdev"), dir.join("rdev.state.lock"));
    }

    /// The exact compatibility break a later review reported: a host
    /// literally named `h#prod` (a safe, single-component TOML table key,
    /// quoted) is not reserved for anything — it keeps its exact v0.4.1
    /// root-level path, same as `azure`/`rdev` above.
    #[test]
    fn host_derived_paths_keep_an_h_hash_prefixed_name_at_the_root_too() {
        let dir = tmpdir("no-migrate-h-hash");
        assert_eq!(hidden_path(&dir, "h#prod"), dir.join("h#prod.hidden"));
        assert_eq!(state_path(&dir, "h#prod"), dir.join("h#prod-map.json"));
        assert_eq!(
            host_lock_path(&dir, "h#prod"),
            dir.join("h#prod.state.lock")
        );
    }

    /// The exact compatibility break a later review reported: every
    /// artifact appends a suffix before joining, so a host named
    /// `""`/`"."`/`".."` (all syntactically valid, if unusual, quoted TOML
    /// keys) never reaches `Path::join` as a bare special component —
    /// these must stay at the root with their exact v0.4.1 path, not be
    /// diverted into `.h/`. A backslash is likewise an ordinary character
    /// on macOS/Linux, not a separator, so it stays safe too.
    #[test]
    fn dot_and_empty_host_names_stay_at_the_root_like_v0_4_1() {
        let dir = tmpdir("no-migrate-dots");
        assert_eq!(hidden_path(&dir, ""), dir.join(".hidden"));
        assert_eq!(state_path(&dir, ""), dir.join("-map.json"));
        assert_eq!(host_lock_path(&dir, ""), dir.join(".state.lock"));

        assert_eq!(hidden_path(&dir, "."), dir.join("..hidden"));
        assert_eq!(state_path(&dir, "."), dir.join(".-map.json"));
        assert_eq!(host_lock_path(&dir, "."), dir.join("..state.lock"));

        assert_eq!(hidden_path(&dir, ".."), dir.join("...hidden"));
        assert_eq!(state_path(&dir, ".."), dir.join("..-map.json"));
        assert_eq!(host_lock_path(&dir, ".."), dir.join("...state.lock"));

        assert_eq!(hidden_path(&dir, "a\\b"), dir.join("a\\b.hidden"));
    }

    /// The exact bug an independent review reported: an unsafe host name's
    /// hash must not coincide with a distinct, ordinary safe host name that
    /// happens to look like a hash. Directory separation (not a reserved
    /// prefix) is why: the two are never even candidates to collide, since
    /// they live in different directories regardless of stem text.
    #[test]
    fn state_paths_do_not_collide_for_the_reported_pair() {
        let dir = tmpdir("no-collide");
        let unsafe_name = "x/EaL2b7VcKy";
        let lookalike_safe_name = "92f037a4";
        assert_ne!(
            state_path(&dir, unsafe_name),
            state_path(&dir, lookalike_safe_name)
        );
        assert_ne!(
            hidden_path(&dir, unsafe_name),
            hidden_path(&dir, lookalike_safe_name)
        );
        assert_ne!(
            host_lock_path(&dir, unsafe_name),
            host_lock_path(&dir, lookalike_safe_name)
        );
        assert_eq!(
            state_path(&dir, lookalike_safe_name),
            dir.join(format!("{lookalike_safe_name}-map.json"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hidden_is_a_marker_file_not_a_state_field() {
        let dir = std::env::temp_dir().join(format!("hm-hidden-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        assert!(!is_hidden(&dir, "h"));
        set_hidden(&dir, "h", true).unwrap();
        assert!(is_hidden(&dir, "h"));
        // and it survives a map rewrite, which is the whole reason it is not a
        // field on HostState
        save_state(&dir, "h", &HostState::default()).unwrap();
        assert!(is_hidden(&dir, "h"));
        set_hidden(&dir, "h", false).unwrap();
        assert!(!is_hidden(&dir, "h"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pane_hint_goes_to_one_pane_and_only_once() {
        let dir = std::env::temp_dir().join(format!("hm-hint-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        set_pane_hint(&dir, "wBT:p2", "closing the local tab");
        // not the neighbour's: an unaddressed notice is what let the
        // REPLACEMENT pane announce a move that had already finished
        assert_eq!(take_pane_hint(&dir, "wBT:p3"), None);
        assert_eq!(
            take_pane_hint(&dir, "wBT:p2").as_deref(),
            Some("closing the local tab")
        );
        // consumed, so a repaint doesn't resurrect it
        assert_eq!(take_pane_hint(&dir, "wBT:p2"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_pane_hint_is_dropped_not_shown() {
        let dir = std::env::temp_dir().join(format!("hm-hint-old-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        set_pane_hint(&dir, "wBT:p2", "closing the local tab");
        let path = pane_hint_path(&dir, "wBT:p2");
        // backdate it: only the mtime distinguishes a notice from a leftover
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - 120;
        let t = libc::timeval {
            tv_sec: secs,
            tv_usec: 0,
        };
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::utimes(c.as_ptr(), [t, t].as_ptr()) }, 0);
        assert_eq!(take_pane_hint(&dir, "wBT:p2"), None);
        assert!(!path.exists(), "stale or not, it is consumed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
