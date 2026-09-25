//! In-guest tests of P10-S16, Per-CPU control registers and the ring-3 trap table (DESIGN §8.2).

#[allow(
    unused_imports,
    reason = "suite template; the first test here uses them"
)]
use super::{Outcome, Test, test};

pub(super) const TESTS: &[Test] = &[];
