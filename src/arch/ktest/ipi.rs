//! In-guest test of the IPI send path (kernel_tests only). Rows: the
//! parent `ktest.rs`'s `TESTS`.

use vibeos::vectors;

use crate::apic_init;
use crate::ktest::Outcome;
use crate::thread_init;
use crate::x86;

/// Sends with IF on.
const SENDS: u32 = 16;

/// An IPI sent with IF on writes ICR high and low with IF off between the
/// two writes (`apic_init::write_icr`), so a handler's own IPI cannot
/// rewrite ICR high under it and send this one to the handler's
/// destination. Each send is a reschedule IPI to this CPU.
pub(crate) fn test_ipi_icr_writes_if_off() -> Outcome {
    if !x86::interrupts_enabled() {
        return Outcome::Fail("IF off in the test body");
    }
    let before = apic_init::testing::icr_writes_if_on();
    for _ in 0..SENDS {
        let cpu = thread_init::current_cpu();
        if apic_init::send_ipi_cpu(cpu, vectors::IPI_RESCHEDULE).is_err() {
            return Outcome::Fail("send_ipi_cpu");
        }
    }
    let on = apic_init::testing::icr_writes_if_on().wrapping_sub(before);
    if on != 0 {
        return crate::fail_fmt!("{on} of {SENDS} IPIs wrote ICR low with IF on");
    }
    Outcome::Ok
}
