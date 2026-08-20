use std::time::{Duration, SystemTime};

use tokio::time::Instant;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const ACK_TIMEOUT: Duration = Duration::from_secs(4);
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const WAKE_DIVERGENCE: Duration = Duration::from_secs(2);
const WAKE_ACK_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Heartbeat { nonce: u64, wake_probe: bool },
    Reconnect { waiting_for_ready: bool },
}

pub(crate) struct Liveness {
    enabled: bool,
    active: bool,
    ready: bool,
    heartbeat_at: Option<Instant>,
    watchdog_at: Option<Instant>,
    nonce: u64,
    last_ack_nonce: u64,
    wake_probe_nonce: Option<u64>,
    wall_sample: SystemTime,
    monotonic_sample: Instant,
}

impl Liveness {
    pub(crate) fn new(enabled: bool, now: Instant, wall_now: SystemTime) -> Self {
        Self {
            enabled,
            active: false,
            ready: false,
            heartbeat_at: None,
            watchdog_at: None,
            nonce: 0,
            last_ack_nonce: 0,
            wake_probe_nonce: None,
            wall_sample: wall_now,
            monotonic_sample: now,
        }
    }

    pub(crate) fn connected(&mut self, now: Instant, wall_now: SystemTime) {
        self.active = true;
        self.ready = false;
        self.heartbeat_at = None;
        self.watchdog_at = self.enabled.then_some(now + READY_TIMEOUT);
        self.wake_probe_nonce = None;
        self.wall_sample = wall_now;
        self.monotonic_sample = now;
    }

    pub(crate) fn disconnected(&mut self) {
        self.active = false;
        self.ready = false;
        self.heartbeat_at = None;
        self.watchdog_at = None;
        self.wake_probe_nonce = None;
    }

    pub(crate) fn disable(&mut self) {
        self.enabled = false;
        self.disconnected();
    }

    pub(crate) fn ready(&mut self, now: Instant, wall_now: SystemTime) {
        if !self.enabled {
            return;
        }
        self.ready = true;
        self.heartbeat_at = Some(now + HEARTBEAT_INTERVAL);
        self.watchdog_at = Some(now + ACK_TIMEOUT);
        self.wall_sample = wall_now;
        self.monotonic_sample = now;
    }

    pub(crate) fn acknowledge(&mut self, nonce: u64, now: Instant) -> bool {
        if !self.enabled || !self.active || nonce <= self.last_ack_nonce || nonce > self.nonce {
            return false;
        }
        self.last_ack_nonce = nonce;
        if self
            .wake_probe_nonce
            .is_some_and(|wake_nonce| nonce != wake_nonce)
        {
            return false;
        }
        self.wake_probe_nonce = None;
        self.watchdog_at = Some(now + ACK_TIMEOUT);
        true
    }

    pub(crate) fn heartbeat_at(&self) -> Option<Instant> {
        self.heartbeat_at
    }

    pub(crate) fn watchdog_at(&self) -> Option<Instant> {
        self.watchdog_at
    }

    pub(crate) fn poll(&mut self, now: Instant, wall_now: SystemTime) -> Option<Action> {
        if !self.enabled || !self.active {
            return None;
        }
        let wall_elapsed = wall_now
            .duration_since(self.wall_sample)
            .unwrap_or_default();
        let monotonic_elapsed = now.duration_since(self.monotonic_sample);
        self.wall_sample = wall_now;
        self.monotonic_sample = now;

        if self.ready && wall_elapsed >= monotonic_elapsed.saturating_add(WAKE_DIVERGENCE) {
            self.watchdog_at = Some(now + WAKE_ACK_GRACE);
            self.heartbeat_at = Some(now + HEARTBEAT_INTERVAL);
            let nonce = self.next_nonce();
            self.wake_probe_nonce = Some(nonce);
            return Some(Action::Heartbeat {
                nonce,
                wake_probe: true,
            });
        }
        if self.watchdog_at.is_some_and(|deadline| deadline <= now) {
            let waiting_for_ready = !self.ready;
            self.heartbeat_at = None;
            self.watchdog_at = None;
            self.active = false;
            return Some(Action::Reconnect { waiting_for_ready });
        }
        if self.ready && self.heartbeat_at.is_some_and(|deadline| deadline <= now) {
            self.heartbeat_at = Some(now + HEARTBEAT_INTERVAL);
            return Some(Action::Heartbeat {
                nonce: self.next_nonce(),
                wake_probe: false,
            });
        }
        None
    }

    fn next_nonce(&mut self) -> u64 {
        self.nonce = self.nonce.saturating_add(1);
        self.nonce
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blackholed_ack_recovery_fits_the_end_to_end_budget() {
        let start = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let mut liveness = Liveness::new(true, start, wall);
        liveness.connected(start, wall);
        liveness.ready(start, wall);
        assert_eq!(
            liveness.poll(
                start + Duration::from_secs(2),
                wall + Duration::from_secs(2)
            ),
            Some(Action::Heartbeat {
                nonce: 1,
                wake_probe: false,
            })
        );
        assert_eq!(
            liveness.poll(
                start + Duration::from_secs(4),
                wall + Duration::from_secs(4)
            ),
            Some(Action::Reconnect {
                waiting_for_ready: false,
            })
        );
        let reaped = start + Duration::from_millis(4_500);
        liveness.connected(reaped, wall + Duration::from_millis(4_500));
        liveness.ready(
            reaped + Duration::from_secs(5),
            wall + Duration::from_millis(9_500),
        );
        assert!(reaped + Duration::from_secs(5) <= start + Duration::from_millis(9_500));
    }

    #[test]
    fn wall_clock_divergence_sends_a_wake_probe_then_recovers_after_one_second() {
        let start = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let mut liveness = Liveness::new(true, start, wall);
        liveness.connected(start, wall);
        liveness.ready(start, wall);
        let resumed = start + Duration::from_millis(200);
        assert_eq!(
            liveness.poll(resumed, wall + Duration::from_secs(30)),
            Some(Action::Heartbeat {
                nonce: 1,
                wake_probe: true,
            })
        );
        assert_eq!(
            liveness.poll(
                resumed + Duration::from_secs(1),
                wall + Duration::from_secs(31)
            ),
            Some(Action::Reconnect {
                waiting_for_ready: false,
            })
        );
    }

    #[test]
    fn delayed_pre_suspend_ack_cannot_satisfy_the_wake_probe() {
        let start = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let mut liveness = Liveness::new(true, start, wall);
        liveness.connected(start, wall);
        liveness.ready(start, wall);
        assert!(matches!(
            liveness.poll(start + Duration::from_secs(2), wall + Duration::from_secs(2)),
            Some(Action::Heartbeat { nonce: 1, .. })
        ));
        let resumed = start + Duration::from_millis(2_100);
        assert!(matches!(
            liveness.poll(resumed, wall + Duration::from_secs(60)),
            Some(Action::Heartbeat {
                nonce: 2,
                wake_probe: true
            })
        ));
        assert!(!liveness.acknowledge(1, resumed + Duration::from_millis(100)));
        assert_eq!(
            liveness.poll(
                resumed + WAKE_ACK_GRACE,
                wall + Duration::from_secs(61)
            ),
            Some(Action::Reconnect {
                waiting_for_ready: false
            })
        );
        let replacement = resumed + WAKE_ACK_GRACE + Duration::from_millis(100);
        liveness.connected(replacement, wall + Duration::from_secs(62));
        liveness.ready(replacement, wall + Duration::from_secs(62));
        assert!(matches!(
            liveness.poll(
                replacement + HEARTBEAT_INTERVAL,
                wall + Duration::from_secs(64)
            ),
            Some(Action::Heartbeat { nonce: 3, .. })
        ));
        assert!(liveness.acknowledge(3, replacement + HEARTBEAT_INTERVAL));
    }

    #[test]
    fn disconnected_sessions_have_no_live_deadlines() {
        let start = Instant::now();
        let wall = SystemTime::UNIX_EPOCH;
        let mut liveness = Liveness::new(true, start, wall);
        liveness.connected(start, wall);
        liveness.disconnected();
        assert_eq!(liveness.heartbeat_at(), None);
        assert_eq!(liveness.watchdog_at(), None);
        assert_eq!(liveness.poll(start + READY_TIMEOUT, wall), None);
    }
}
