// The persisted per-host id map — the heart of reconciliation.
//
// remote id → { local id, tombstone, seq, reported }. A tombstone means "the
// user closed this mirror" — never recreate it until restore. Absence of a
// remote id means "remote went away" — close the mirror. Restart-idempotent.
// The camelCase JSON shape matches the TS implementation so an existing
// <host>-map.json carries over.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    state_dir.join(format!("{host}.hidden"))
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
    state_dir.join(format!(".hint-{}", crate::util::sane_component(local_pane_id)))
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

/// A remote pane's live terminal title, relayed to the streamer drawing its
/// mirror.
///
/// Two processes are needed for one fact. The daemon is what sees the title (on
/// the remote API), but herdr only derives a pane's `terminal_title` from what
/// the pane's own process writes (OSC 2), and the only process writing into a
/// mirror pane is its streamer. Without this every mirror reports the title
/// the shell set before `exec` — the `.mirror-pane` placeholder path — to
/// every herdr client that names panes by terminal title.
///
/// `remote` rides along because herdr reuses freed local pane ids: a streamer
/// only takes a title addressed to the remote pane it is actually showing.
/// `host` is what lets each host's daemon prune only its own files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneTitle {
    pub host: String,
    pub remote: String,
    /// `None` when the remote pane has no title, which is relayed as well: the
    /// mirror then reports none either, instead of the placeholder's.
    pub title: Option<String>,
}

fn pane_titles_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("pane-titles")
}

fn pane_title_path(state_dir: &Path, local_pane_id: &str) -> PathBuf {
    pane_titles_dir(state_dir).join(format!("{}.json", crate::util::sane_component(local_pane_id)))
}

pub fn read_pane_title(state_dir: &Path, local_pane_id: &str) -> Option<PaneTitle> {
    let text = std::fs::read_to_string(pane_title_path(state_dir, local_pane_id)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Record a pane's title. True only when it changed and landed, which is when
/// the streamer is worth waking: titles are relayed on every converge and on
/// every remote `pane.updated`, and almost all of those repeat what is there.
///
/// Written to a temp file and renamed so a streamer woken mid-write can never
/// read half a record.
pub fn put_pane_title(state_dir: &Path, local_pane_id: &str, title: &PaneTitle) -> bool {
    if read_pane_title(state_dir, local_pane_id).as_ref() == Some(title) {
        return false;
    }
    let path = pane_title_path(state_dir, local_pane_id);
    let tmp = path.with_extension("json.tmp");
    let Ok(text) = serde_json::to_string(title) else { return false };
    std::fs::create_dir_all(pane_titles_dir(state_dir)).is_ok()
        && std::fs::write(&tmp, text).is_ok()
        && std::fs::rename(&tmp, &path).is_ok()
}

/// Drop the titles `host` relayed to local panes it no longer mirrors, so the
/// directory does not grow by one file per mirror ever made. Other hosts'
/// files are left alone: each daemon task only knows its own map.
pub fn prune_pane_titles(state_dir: &Path, host: &str, live_local_ids: &std::collections::HashSet<&str>) {
    let live: std::collections::HashSet<String> =
        live_local_ids.iter().map(|id| crate::util::sane_component(id)).collect();
    let Ok(dir) = std::fs::read_dir(pane_titles_dir(state_dir)) else { return };
    for file in dir.flatten() {
        let path = file.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        if path.extension().and_then(|e| e.to_str()) != Some("json") || live.contains(stem) {
            continue;
        }
        let ours = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<PaneTitle>(&t).ok())
            .is_some_and(|t| t.host == host);
        if ours {
            let _ = std::fs::remove_file(&path);
        }
    }
}

pub fn state_path(state_dir: &Path, host: &str) -> PathBuf {
    state_dir.join(format!("{host}-map.json"))
}

pub fn load_state(state_dir: &Path, host: &str) -> HostState {
    std::fs::read_to_string(state_path(state_dir, host))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_state(state_dir: &Path, host: &str, state: &HostState) -> Result<()> {
    std::fs::create_dir_all(state_dir)?;
    std::fs::write(state_path(state_dir, host), serde_json::to_string_pretty(state)?)?;
    Ok(())
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
        assert_eq!(state.workspaces["w9"].root_tab_local_id.as_deref(), Some("t99"));
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
        assert_eq!(take_pane_hint(&dir, "wBT:p2").as_deref(), Some("closing the local tab"));
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
        let t = libc::timeval { tv_sec: secs, tv_usec: 0 };
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::utimes(c.as_ptr(), [t, t].as_ptr()) }, 0);
        assert_eq!(take_pane_hint(&dir, "wBT:p2"), None);
        assert!(!path.exists(), "stale or not, it is consumed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn title(host: &str, remote: &str, t: Option<&str>) -> PaneTitle {
        PaneTitle { host: host.into(), remote: remote.into(), title: t.map(str::to_string) }
    }

    #[test]
    fn a_pane_title_is_rewritten_only_when_it_changes() {
        let dir = std::env::temp_dir().join(format!("hm-title-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let working = title("vibe1", "wD:p3", Some("π ⠴ jev-game"));
        assert!(put_pane_title(&dir, "w5:p4", &working), "first relay lands");
        assert_eq!(read_pane_title(&dir, "w5:p4").as_ref(), Some(&working));
        // the converge pass relays every pane every time; a repeat must not
        // wake the streamer
        assert!(!put_pane_title(&dir, "w5:p4", &working));
        let idle = title("vibe1", "wD:p3", Some("π > jev-game"));
        assert!(put_pane_title(&dir, "w5:p4", &idle));
        // losing the title is a change too, and is relayed as such
        assert!(put_pane_title(&dir, "w5:p4", &title("vibe1", "wD:p3", None)));
        assert_eq!(read_pane_title(&dir, "w5:p4").unwrap().title, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pruning_titles_spares_live_panes_and_other_hosts() {
        let dir = std::env::temp_dir().join(format!("hm-title-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        put_pane_title(&dir, "w5:p2", &title("vibe1", "wD:p1", Some("live")));
        put_pane_title(&dir, "w5:p9", &title("vibe1", "wD:p8", Some("closed")));
        put_pane_title(&dir, "w7:p3", &title("office", "w4:p1M", Some("other host")));
        let live: std::collections::HashSet<&str> = ["w5:p2"].into_iter().collect();
        prune_pane_titles(&dir, "vibe1", &live);
        assert!(read_pane_title(&dir, "w5:p2").is_some());
        assert!(read_pane_title(&dir, "w5:p9").is_none(), "vibe1 no longer mirrors it");
        // vibe1's daemon task knows nothing about office's map
        assert!(read_pane_title(&dir, "w7:p3").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
