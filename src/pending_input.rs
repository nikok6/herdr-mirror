use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

const MAX_PENDING_BYTES: usize = 64 * 1024;
const MAX_PENDING_AGE: Duration = Duration::from_secs(10);

struct Entry {
    queued_at: Instant,
    bytes: Vec<u8>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct DrainedInput {
    pub chunks: Vec<Vec<u8>>,
    pub dropped_bytes: usize,
}

#[derive(Default)]
pub struct PendingInput {
    entries: VecDeque<Entry>,
    queued_bytes: usize,
    dropped_bytes: usize,
}

impl PendingInput {
    pub fn push(&mut self, queued_at: Instant, bytes: Vec<u8>) {
        self.expire(queued_at);
        if bytes.len() > MAX_PENDING_BYTES
            || self.queued_bytes.saturating_add(bytes.len()) > MAX_PENDING_BYTES
        {
            self.dropped_bytes = self.dropped_bytes.saturating_add(bytes.len());
            return;
        }
        self.queued_bytes += bytes.len();
        self.entries.push_back(Entry { queued_at, bytes });
    }

    pub fn drain_ready(&mut self, now: Instant) -> DrainedInput {
        self.expire(now);
        self.queued_bytes = 0;
        DrainedInput {
            chunks: self.entries.drain(..).map(|entry| entry.bytes).collect(),
            dropped_bytes: std::mem::take(&mut self.dropped_bytes),
        }
    }

    fn expire(&mut self, now: Instant) {
        while self.entries.front().is_some_and(|entry| {
            now.checked_duration_since(entry.queued_at)
                .is_some_and(|age| age >= MAX_PENDING_AGE)
        }) {
            if let Some(entry) = self.entries.pop_front() {
                self.queued_bytes = self.queued_bytes.saturating_sub(entry.bytes.len());
                self.dropped_bytes = self.dropped_bytes.saturating_add(entry.bytes.len());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_is_held_until_acquisition_and_drained_in_order() {
        let now = Instant::now();
        let mut pending = PendingInput::default();
        pending.push(now, b"one".to_vec());
        pending.push(now + Duration::from_millis(1), b"two".to_vec());

        assert_eq!(
            pending.drain_ready(now + Duration::from_secs(1)),
            DrainedInput {
                chunks: vec![b"one".to_vec(), b"two".to_vec()],
                dropped_bytes: 0
            }
        );
    }

    #[test]
    fn stale_and_excess_input_is_bounded_and_reported() {
        let now = Instant::now();
        let mut pending = PendingInput::default();
        pending.push(now, vec![b'a'; 32]);
        pending.push(
            now + Duration::from_secs(11),
            vec![b'b'; MAX_PENDING_BYTES + 1],
        );

        assert_eq!(
            pending.drain_ready(now + Duration::from_secs(11)),
            DrainedInput {
                chunks: Vec::new(),
                dropped_bytes: MAX_PENDING_BYTES + 33
            }
        );
    }
}
