//! aarch64 GIC proof: the priority classes DESIGN §5.3 gives, read back
//! from the GICv3 or GICv2 the boot found (ROADMAP §11.3).

use crate::arch::current::InterruptGuard;
use crate::ktest::Outcome;

/// The GIC holds DESIGN §5.3's priority classes, read back from the
/// hardware: the reschedule SGI and the chosen timer's PPI at the IPI and
/// tick class, and a device-tree device's SPI at the device class
/// (ROADMAP §11.3).
pub(crate) fn test_gic_priorities() -> Outcome {
    use crate::arch::aarch64::gic::priority_readback;
    use vibeos::irq::gic::{PRIO_DEVICE, PRIO_IPI_TICK, SGI_RESCHEDULE};
    let Some(desc) = crate::machine_init::info() else {
        return Outcome::Fail("no machine desc");
    };
    let spi = desc
        .rtc
        .map(|d| d.irq)
        .into_iter()
        .chain(
            desc.virtio_mmio
                .iter()
                .take(desc.virtio_mmio_count)
                .map(|d| d.irq),
        )
        .find(|&i| vibeos::irq::gic::is_spi(i));
    let Some(spi) = spi else {
        return Outcome::Fail("no device SPI in the device tree");
    };
    let timer = crate::arch::aarch64::timer::intid();
    let _g = InterruptGuard::enter();
    let rows = [
        (SGI_RESCHEDULE, PRIO_IPI_TICK),
        (timer, PRIO_IPI_TICK),
        (spi, PRIO_DEVICE),
    ];
    for (intid, want) in rows {
        match priority_readback(intid) {
            Some(got) if got == want => {}
            got => return crate::fail_fmt!("intid {intid}: priority {got:?}, want {want:#x}"),
        }
    }
    Outcome::Ok
}
