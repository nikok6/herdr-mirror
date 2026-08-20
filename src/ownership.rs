use std::time::Duration;

use tokio::time::Instant;

pub const OWNER_CONFLICT_SUFFIX: &str = "already has an attached client; retry with --takeover";
pub const TAKEN_OVER_REASON: &str = "terminal attach taken over";
const TAKEOVER_COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollisionDecision {
    ObserveSticky,
    RetryWithTakeover,
}

#[derive(Debug, Default)]
pub struct LegacyTakeoverPolicy {
    last_attempt: Option<Instant>,
}

impl LegacyTakeoverPolicy {
    pub fn decide(
        &mut self,
        reason: &str,
        always_control: bool,
        enabled: bool,
        now: Instant,
    ) -> Option<CollisionDecision> {
        if reason.starts_with("owned_by_other")
            || reason.starts_with("taken_over")
            || reason.ends_with(TAKEN_OVER_REASON)
        {
            return Some(CollisionDecision::ObserveSticky);
        }
        if !reason.ends_with(OWNER_CONFLICT_SUFFIX) {
            return None;
        }
        if !always_control || !enabled {
            return Some(CollisionDecision::ObserveSticky);
        }
        let cooled_down = self.last_attempt.is_none_or(|last| {
            now.checked_duration_since(last)
                .is_some_and(|elapsed| elapsed >= TAKEOVER_COOLDOWN)
        });
        if !cooled_down {
            return Some(CollisionDecision::ObserveSticky);
        }
        self.last_attempt = Some(now);
        Some(CollisionDecision::RetryWithTakeover)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opted_in_always_control_retries_one_takeover_per_cooldown() {
        let mut policy = LegacyTakeoverPolicy::default();
        let now = Instant::now();

        assert_eq!(
            policy.decide(OWNER_CONFLICT_SUFFIX, true, true, now),
            Some(CollisionDecision::RetryWithTakeover)
        );
        assert_eq!(
            policy.decide(
                OWNER_CONFLICT_SUFFIX,
                true,
                true,
                now + Duration::from_secs(59)
            ),
            Some(CollisionDecision::ObserveSticky)
        );
        assert_eq!(
            policy.decide(OWNER_CONFLICT_SUFFIX, true, true, now + TAKEOVER_COOLDOWN),
            Some(CollisionDecision::RetryWithTakeover)
        );
    }

    #[test]
    fn safe_modes_never_take_over() {
        let now = Instant::now();
        for (always_control, enabled) in [(true, false), (false, true), (false, false)] {
            let mut policy = LegacyTakeoverPolicy::default();
            assert_eq!(
                policy.decide(OWNER_CONFLICT_SUFFIX, always_control, enabled, now),
                Some(CollisionDecision::ObserveSticky)
            );
        }
    }

    #[test]
    fn a_displaced_stream_never_immediately_takes_back() {
        let mut policy = LegacyTakeoverPolicy::default();
        assert_eq!(
            policy.decide(TAKEN_OVER_REASON, true, true, Instant::now()),
            Some(CollisionDecision::ObserveSticky)
        );
    }

    #[test]
    fn unrelated_failures_remain_on_the_normal_reconnect_path() {
        let mut policy = LegacyTakeoverPolicy::default();
        assert_eq!(
            policy.decide("ssh timeout", true, true, Instant::now()),
            None
        );
    }
}
