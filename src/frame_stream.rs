use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use crate::util::{err, Result};

const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub struct CompleteFrame {
    pub seq: u64,
    pub width: usize,
    pub height: usize,
    pub full: bool,
    pub bytes: Vec<u8>,
}

struct PendingFrame {
    seq: u64,
    width: usize,
    height: usize,
    full: bool,
    total_bytes: usize,
    next_index: usize,
    bytes: Vec<u8>,
}

#[derive(Default)]
pub struct FrameAssembler {
    pending: Option<PendingFrame>,
}

impl FrameAssembler {
    pub fn start(
        &mut self,
        seq: u64,
        width: usize,
        height: usize,
        full: bool,
        total_bytes: usize,
    ) -> Result<()> {
        if total_bytes > MAX_FRAME_BYTES {
            return Err(err("terminal frame exceeds 2 MiB limit"));
        }
        self.pending = Some(PendingFrame {
            seq,
            width,
            height,
            full,
            total_bytes,
            next_index: 0,
            bytes: Vec::with_capacity(total_bytes),
        });
        Ok(())
    }

    pub fn chunk(&mut self, seq: u64, index: usize, encoded: &str) -> Result<()> {
        let pending = self
            .pending
            .as_mut()
            .ok_or_else(|| err("frame chunk before start"))?;
        if pending.seq != seq || pending.next_index != index {
            self.pending = None;
            return Err(err("terminal frame chunk sequence mismatch"));
        }
        let decoded = B64.decode(encoded)?;
        if pending.bytes.len().saturating_add(decoded.len()) > pending.total_bytes {
            self.pending = None;
            return Err(err("terminal frame chunk exceeds declared size"));
        }
        pending.bytes.extend_from_slice(&decoded);
        pending.next_index += 1;
        Ok(())
    }

    pub fn finish(&mut self, seq: u64) -> Result<CompleteFrame> {
        let pending = self
            .pending
            .take()
            .ok_or_else(|| err("frame end before start"))?;
        if pending.seq != seq || pending.bytes.len() != pending.total_bytes {
            return Err(err("terminal frame ended incomplete"));
        }
        Ok(CompleteFrame {
            seq,
            width: pending.width,
            height: pending.height,
            full: pending.full,
            bytes: pending.bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_reassemble_exactly_and_gaps_reset_the_frame() {
        let mut assembler = FrameAssembler::default();
        assembler.start(4, 80, 24, true, 5).unwrap();
        assembler.chunk(4, 0, &B64.encode(b"hel")).unwrap();
        assembler.chunk(4, 1, &B64.encode(b"lo")).unwrap();
        assert_eq!(
            assembler.finish(4).unwrap(),
            CompleteFrame {
                seq: 4,
                width: 80,
                height: 24,
                full: true,
                bytes: b"hello".to_vec(),
            }
        );

        assembler.start(5, 80, 24, false, 1).unwrap();
        assert!(assembler.chunk(5, 1, &B64.encode(b"x")).is_err());
        assert!(assembler.finish(5).is_err());
    }
}
