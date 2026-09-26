//! In-guest tests of P10-S06, Frame, DMA and stack ownership tokens (DESIGN §8.2).

use core::fmt;

use super::{Outcome, Test, test};
use crate::diag;

pub(super) const TESTS: &[Test] = &[test("frames_none_leaked", frames_none_leaked)];

// ---------------------------------------------------------------------------
// frames_none_leaked (ROADMAP §10.3, F018)

const LEAK_LINE: &[u8] = b"vibeOS: meminfo: leaked 0 frames";

/// A `fmt::Write` sink that splits what `meminfo_to` writes into lines
/// and records whether one of them is [`LEAK_LINE`]. Lines longer than
/// its buffer are cut, which only makes them not match.
struct LineSeen {
    line: [u8; 96],
    len: usize,
    seen: bool,
}

impl fmt::Write for LineSeen {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if b == b'\n' {
                if &self.line[..self.len] == LEAK_LINE {
                    self.seen = true;
                }
                self.len = 0;
            } else if self.len < self.line.len() {
                self.line[self.len] = b;
                self.len += 1;
            }
        }
        Ok(())
    }
}

/// Every test before this one (the legacy list and the earlier suites)
/// freed each `Frames` it took, so none was dropped, and `meminfo` says
/// so on its `leaked` line.
fn frames_none_leaked() -> Outcome {
    let leaked = vibeos::pmm::leaked_frames();
    if leaked != 0 {
        return crate::fail_fmt!("{leaked} frames leaked by dropped Frames");
    }
    let mut sink = LineSeen {
        line: [0; 96],
        len: 0,
        seen: false,
    };
    diag::meminfo_to(&mut sink);
    if !sink.seen {
        return Outcome::Fail("meminfo printed no `leaked 0 frames` line");
    }
    Outcome::Ok
}
