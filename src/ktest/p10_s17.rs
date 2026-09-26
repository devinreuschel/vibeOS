//! In-guest tests of P10-S17, Syscall exit IF=0, enter_user, USER_MAP_END and the FP binding (DESIGN §8.2).

#[allow(
    unused_imports,
    reason = "suite template; the first test here uses them"
)]
use super::{Outcome, Test, test};

pub(super) const TESTS: &[Test] = &[];
