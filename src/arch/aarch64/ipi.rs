//! SGI IPIs: reschedule, call-function, panic stop. No shootdown SGI.

use vibeos::apic::IpiError;
use vibeos::arch::{Ipi, IpiSend};
use vibeos::irq::gic::{SGI_CALL, SGI_RESCHEDULE, SGI_STOP};

use super::{Arch, gic};

impl IpiSend for Arch {
    type Error = IpiError;

    fn send(_cpu: u32, ipi: Ipi) -> Result<(), IpiError> {
        match ipi {
            Ipi::Shootdown => Ok(()),
            Ipi::Reschedule => {
                gic::send_sgi(SGI_RESCHEDULE);
                Ok(())
            }
            Ipi::Call => {
                gic::send_sgi(SGI_CALL);
                Ok(())
            }
            Ipi::Halt => {
                gic::send_sgi(SGI_STOP);
                Ok(())
            }
        }
    }

    fn send_others(ipi: Ipi) -> Result<(), IpiError> {
        // Boot CPU only: no other cores.
        let _ = ipi;
        Ok(())
    }
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "in-guest test hook")
)]
pub fn send_reschedule_self() {
    gic::send_sgi(SGI_RESCHEDULE);
}
