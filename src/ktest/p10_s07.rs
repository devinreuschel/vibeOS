//! In-guest tests of P10-S07, Address-space and mapping soundness (DESIGN §8.2).

use vibeos::paging::VirtAddr;

use super::{Outcome, Test, spin_until_ns, test};
use crate::paging_init;

pub(super) const TESTS: &[Test] = &[test("current_mapper_holds_pt", current_mapper_holds_pt)];

// ---------------------------------------------------------------------------
// current_mapper_holds_pt (ROADMAP §10.3, F018)

/// PT is held while a `current_mapper` guard lives and free once it drops.
/// Another CPU may take PT briefly after the drop, so the free check polls.
fn current_mapper_holds_pt() -> Outcome {
    let g = paging_init::current_mapper();
    let held = !paging_init::pt_lock_free();
    let walks = g
        .translate(VirtAddr(current_mapper_holds_pt as *const () as u64))
        .is_some();
    drop(g);
    if !held {
        return Outcome::Fail("PT free while a MapperGuard lives");
    }
    if !walks {
        return Outcome::Fail("guard's mapper does not translate kernel text");
    }
    if !spin_until_ns(paging_init::pt_lock_free, 100_000_000) {
        return Outcome::Fail("PT still held after drop");
    }
    Outcome::Ok
}
