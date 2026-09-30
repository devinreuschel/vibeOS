//! In-guest test of the vector table's registration rules (kernel_tests
//! only). Rows: the parent `ktest.rs`'s `TESTS`.

use vibeos::vectors;

use crate::arch;
use crate::arch::idt::TrapFrame;
use crate::ktest::Outcome;

/// `idt::set_handler` refuses exception and fixed-owner vectors (ROADMAP
/// §10.3, INTERRUPTS.md §5.3): `registrable` names exactly those, and the
/// assertion fires for an IPI and an exception vector before any body is
/// stored.
pub(crate) fn test_idt_set_handler_refuses_fixed() -> Outcome {
    const FIXED: [u8; 8] = [
        vectors::LAPIC_TIMER,
        vectors::LAPIC_ERROR,
        vectors::LAPIC_THERMAL,
        vectors::IPI_CALL,
        vectors::IPI_SHOOTDOWN,
        vectors::IPI_RESCHEDULE,
        vectors::IPI_HALT,
        vectors::LAPIC_SPURIOUS,
    ];
    for v in 0..=255u8 {
        let want = v >= 32 && !FIXED.contains(&v);
        if arch::idt::registrable(v) != want {
            return crate::fail_fmt!("registrable({v:#x}) is {}", !want);
        }
    }
    fn never(_: &mut TrapFrame) {}
    if !arch::catch::catch_panic(|| arch::idt::set_handler(vectors::IPI_RESCHEDULE, never)) {
        return Outcome::Fail("IPI_RESCHEDULE registered");
    }
    if !arch::catch::catch_panic(|| arch::idt::set_handler(vectors::GP, never)) {
        return Outcome::Fail("GP registered");
    }
    Outcome::Ok
}
