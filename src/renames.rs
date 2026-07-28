// Local tab-rename echo tracking.
//
// A remote rename is mirrored by renaming the local tab. That local API call
// raises the same event as a user rename, so forwarding every event back to the
// remote would turn one rename into an endless round trip.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A failed local rename produces no echo. Expiry keeps that missing event from
/// causing a later user rename of the same tab to be swallowed.
const SELF_RENAME_TTL: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct RenameTracker {
    /// local tab ids we are renaming ourselves
    self_renamed: HashMap<String, Instant>,
}

impl RenameTracker {
    /// Must be called before issuing the local rename whose event is ours.
    pub fn mark_self_rename(&mut self, local_id: &str) {
        self.expire();
        self.self_renamed.insert(local_id.to_string(), Instant::now());
    }

    /// Returns true only when the event is a genuine local user rename.
    pub fn note_rename_event(&mut self, local_id: &str) -> bool {
        self.expire();
        self.self_renamed.remove(local_id).is_none()
    }

    fn expire(&mut self) {
        let now = Instant::now();
        self.self_renamed.retain(|_, at| now.duration_since(*at) < SELF_RENAME_TTL);
    }
}

pub type Renames = Arc<Mutex<RenameTracker>>;

pub fn new_renames() -> Renames {
    Arc::new(Mutex::new(RenameTracker::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_rename_echo_is_suppressed_but_user_rename_passes() {
        let mut tracker = RenameTracker::default();
        tracker.mark_self_rename("t1");
        assert!(!tracker.note_rename_event("t1"));
        assert!(tracker.note_rename_event("t1"));
        assert!(tracker.note_rename_event("t2"));
    }

    #[test]
    fn missing_echo_expires_instead_of_suppressing_a_later_user_rename() {
        let mut tracker = RenameTracker::default();
        tracker
            .self_renamed
            .insert("t1".into(), Instant::now() - SELF_RENAME_TTL);
        assert!(tracker.note_rename_event("t1"));
    }
}
