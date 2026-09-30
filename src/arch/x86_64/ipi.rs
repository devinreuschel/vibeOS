//! `IpiSend` on x86_64 (PORTABILITY §11.1): each portable `Ipi` is its fixed
//! vector (SMP §7.6), sent through the LAPIC's ICR by `apic_init`.

use vibeos::apic::IpiError;
use vibeos::arch::{Ipi, IpiSend};
use vibeos::vectors;

use super::{Arch, apic_init};

/// The fixed vector each IPI kind takes (DESIGN §5.3).
const fn vector(ipi: Ipi) -> u8 {
    match ipi {
        Ipi::Call => vectors::IPI_CALL,
        Ipi::Shootdown => vectors::IPI_SHOOTDOWN,
        Ipi::Reschedule => vectors::IPI_RESCHEDULE,
        Ipi::Halt => vectors::IPI_HALT,
    }
}

/// Stores before a send are visible to the target's handler: the ICR write
/// is a store to the xAPIC's uncached page, which x86 never orders before
/// an earlier store (this port does not use x2APIC's MSR interface).
impl IpiSend for Arch {
    type Error = IpiError;

    #[inline]
    fn send(cpu: u32, ipi: Ipi) -> Result<(), IpiError> {
        apic_init::send_ipi_cpu(cpu, vector(ipi))
    }

    #[inline]
    fn send_others(ipi: Ipi) -> Result<(), IpiError> {
        apic_init::send_ipi_all_ex_self(vector(ipi))
    }
}
