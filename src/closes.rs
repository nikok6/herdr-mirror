// Authoritative local-close tracking.
//
// A converge can't tell "the user closed this mirror" from "the mirror is
// missing because a rebuild failed, the server just restarted, or a converge
// raced a teardown" — snapshot absence is ambiguous. Guessing wrong is
// destructive (it closes a live remote session), while being conservative is
// benign (a mirror lingers; the user closes it again). So the remote close is
// driven by the local `workspace_closed`/`pane_closed` EVENT — which is
// authoritative — and the plugin suppresses the echo of closes it performs
// itself (teardown, zombie-heal, reap).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A self-close mark expires so a close event we never observe can't wedge the
/// id as "ours" forever.
const SELF_CLOSE_TTL: Duration = Duration::from_secs(30);
/// A user close is drained by the converge the poke triggers (milliseconds), so
/// anything unclaimed this long belongs to a non-mirror pane and is just noise.
const USER_CLOSE_TTL: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct CloseTracker {
    /// local ids we are closing ourselves — their close events are our own echo
    self_closed: HashMap<String, Instant>,
    /// local ids a close event named that weren't ours: the user's intent
    user_closed: HashMap<String, Instant>,
}

impl CloseTracker {
    /// Mark a local id we're about to close, so its close event isn't mistaken
    /// for the user closing the mirror. Must be called BEFORE the close.
    pub fn mark_self_close(&mut self, local_id: &str) {
        self.expire();
        self.self_closed.insert(local_id.to_string(), Instant::now());
    }

    /// Record a local close event. Ours → swallowed as an echo; anything else is
    /// the user deliberately closing that object.
    pub fn note_close_event(&mut self, local_id: &str) {
        self.expire();
        if self.self_closed.remove(local_id).is_some() {
            return;
        }
        self.user_closed.insert(local_id.to_string(), Instant::now());
    }

    /// Take the user-closed ids among `mine` (this host's mapped local ids).
    /// Draining keeps one host's converge from consuming another's.
    pub fn take_user_closed(&mut self, mine: &HashSet<String>) -> HashSet<String> {
        self.expire();
        let hit: HashSet<String> =
            self.user_closed.keys().filter(|id| mine.contains(*id)).cloned().collect();
        for id in &hit {
            self.user_closed.remove(id);
        }
        hit
    }

    fn expire(&mut self) {
        let now = Instant::now();
        self.self_closed.retain(|_, at| now.duration_since(*at) < SELF_CLOSE_TTL);
        self.user_closed.retain(|_, at| now.duration_since(*at) < USER_CLOSE_TTL);
    }
}

/// Shared between the local event stream (which records closes) and each host's
/// converge (which acts on them).
pub type Closes = Arc<Mutex<CloseTracker>>;

pub fn new_closes() -> Closes {
    Arc::new(Mutex::new(CloseTracker::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> HashSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn user_close_is_reported_once_to_the_owning_host() {
        let mut t = CloseTracker::default();
        t.note_close_event("w1");
        assert_eq!(t.take_user_closed(&ids(&["w1", "w2"])), ids(&["w1"]));
        // drained: a second converge must not re-close the remote
        assert!(t.take_user_closed(&ids(&["w1"])).is_empty());
    }

    #[test]
    fn our_own_close_is_not_user_intent() {
        let mut t = CloseTracker::default();
        t.mark_self_close("w1"); // teardown / heal / reap closing a mirror
        t.note_close_event("w1"); // the echo of that close
        assert!(t.take_user_closed(&ids(&["w1"])).is_empty());
    }

    #[test]
    fn self_mark_is_consumed_so_a_later_user_close_still_counts() {
        let mut t = CloseTracker::default();
        t.mark_self_close("w1");
        t.note_close_event("w1"); // ours — swallowed, mark consumed
        // the id is later re-mapped (heal adopts it) and the user closes it
        t.note_close_event("w1");
        assert_eq!(t.take_user_closed(&ids(&["w1"])), ids(&["w1"]));
    }

    #[test]
    fn closes_for_other_hosts_ids_are_left_alone() {
        let mut t = CloseTracker::default();
        t.note_close_event("wA");
        assert!(t.take_user_closed(&ids(&["wB"])).is_empty());
        assert_eq!(t.take_user_closed(&ids(&["wA"])), ids(&["wA"]));
    }
}
