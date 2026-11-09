//! GIC state and register access shared by the distributor and redistributor
//! half (`gic.rs`) and the ITS half (`gic_its.rs`): the controller record,
//! its MMIO accessors, LPI page allocation, and the table cleaning that a
//! non-shareable readback asks for.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::irq::gic;

use crate::cell::BootCell;

pub(super) const GICR_TYPER: u64 = 0x0008;

/// Inner Shareable (`0b01`) in bits [11:10] of a table register (IHI 0069).
pub(super) const XBASER_SHARE_IS: u64 = 1 << 10;

/// Set once a table register reads back non-shareable (`gic::shareability_dropped`).
/// Every table write after that is cleaned to PoC before the device reads it.
static SHARE_DROPPED: AtomicBool = AtomicBool::new(false);

pub(super) const SPI_SLOTS: usize = 1024;

pub(super) const LPI_SLOTS: usize = 256;

pub(super) const LPI_BASE: u32 = gic::LPI_BASE;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    V2,
    V3,
}

pub(super) struct Gic {
    pub(super) kind: Kind,
    /// `ICC_CTLR_EL1.RSS`, sampled on the BSP in `init`.
    pub(super) rss: bool,
    pub(super) dist: u64,
    pub(super) cpu_or_redist: u64,
    /// Physical base of the redistributor window (`cpu_or_redist` is its VA).
    pub(super) redist_pa: u64,
    pub(super) redist_size: u64,
    pub(super) lpi_prop: AtomicU64,
    pub(super) its: Option<u64>,
    pub(super) v2m: Option<(u64, u32, u16)>,
    pub(super) spi_irqs: [AtomicU32; SPI_SLOTS],
    pub(super) lpi_irqs: [AtomicU32; LPI_SLOTS],
    pub(super) next_lpi: AtomicU32,
    pub(super) next_v2m: AtomicU32,
}

pub(super) static CHIP: BootCell<Gic> = BootCell::new();

pub(super) fn gic() -> Option<&'static Gic> {
    CHIP.try_get()
}

/// Read a 32-bit GIC register.
///
/// # Safety
/// `va+off` is a mapped GIC register the caller owns.
pub(super) unsafe fn mmio32(va: u64, off: u64) -> u32 {
    // SAFETY: this fn's `# Safety`; established by `crate::arch::aarch64::gic::init`.
    unsafe { core::ptr::read_volatile((va.wrapping_add(off)) as *const u32) }
}

/// Write a 32-bit GIC register.
///
/// # Safety
/// As [`mmio32`].
pub(super) unsafe fn mmio32w(va: u64, off: u64, v: u32) {
    // SAFETY: this fn's `# Safety`; established by `crate::arch::aarch64::gic::init`.
    unsafe { core::ptr::write_volatile((va.wrapping_add(off)) as *mut u32, v) };
}

/// Read a 64-bit GIC register.
///
/// # Safety
/// As [`mmio32`].
pub(super) unsafe fn mmio64(va: u64, off: u64) -> u64 {
    // SAFETY: this fn's `# Safety`; established by `crate::arch::aarch64::gic::init`.
    unsafe { core::ptr::read_volatile((va.wrapping_add(off)) as *const u64) }
}

/// Write a 64-bit GIC register.
///
/// # Safety
/// As [`mmio32`].
pub(super) unsafe fn mmio64w(va: u64, off: u64, v: u64) {
    // SAFETY: this fn's `# Safety`; established by `crate::arch::aarch64::gic::init`.
    unsafe { core::ptr::write_volatile((va.wrapping_add(off)) as *mut u64, v) };
}

/// Record a table register that read back non-shareable.
pub(super) fn note_shareability(written: u64, read: u64) {
    if gic::shareability_dropped(written, read) {
        // Release: pairs with the Acquire load in `cleans_to_poc`.
        SHARE_DROPPED.store(true, Ordering::Release);
    }
}

/// Whether a table write must be cleaned to PoC before the device reads it.
pub(super) fn cleans_to_poc() -> bool {
    // Acquire: pairs with the Release store in `note_shareability`.
    SHARE_DROPPED.load(Ordering::Acquire)
}

/// Clean one table, `pa` for `bytes`, to PoC when `cleans_to_poc`.
pub(super) fn clean_table_if_needed(pa: u64, bytes: usize) {
    if cleans_to_poc() {
        let va = crate::paging_init::hhdm_offset().wrapping_add(pa);
        crate::arch::aarch64::secondary::clean_poc(va, bytes);
    }
}
