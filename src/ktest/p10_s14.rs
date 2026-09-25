//! In-guest tests of P10-S14, Volatile-cache crash test and no stray disk writes (DESIGN §8.2).

#[allow(
    unused_imports,
    reason = "suite template; the first test here uses them"
)]
use super::{Outcome, Test, test};

pub(super) const TESTS: &[Test] = &[];
