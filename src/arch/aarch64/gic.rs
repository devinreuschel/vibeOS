//! GICv2 / GICv3 + ITS / GICv2m as `IrqChip`s (DESIGN §5.4, §11.5).

use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

use vibeos::irq::{IrqChip, IrqError, IrqSpecifier, gic};
use vibeos::log::Level;
use vibeos::machine::PhysRange;
use vibeos::paging::PhysAddr;

use super::gic_common::{
    CHIP, GICR_TYPER, Gic, Kind, LPI_BASE, LPI_SLOTS, SPI_SLOTS, XBASER_SHARE_IS, alloc_pages,
    clean_table_if_needed, gic, mmio32, mmio32w, mmio64, mmio64w, note_shareability,
};
use super::gic_its::{LPI_EVENT, discard_lpi, init_its, map_collection, movi_lpi};
pub use super::gic_its::{map_its_event, remove_its_device};
use crate::cell::BootCell;
use crate::machine_init;
use crate::paging_init;

const GICD_CTLR: u64 = 0x0000;
const GICD_TYPER: u64 = 0x0004;
const GICD_IGROUPR: u64 = 0x0080;
const GICD_ISENABLER: u64 = 0x0100;
const GICD_ICENABLER: u64 = 0x0180;
const GICD_IPRIORITYR: u64 = 0x0400;
const GICD_ITARGETSR: u64 = 0x0800;
const GICD_ICFGR: u64 = 0x0C00;
const GICD_ICFGR_EDGE: u32 = 2;
const GICD_SGIR: u64 = 0x0F00;
const GICD_CTLR_ENABLE_G0: u32 = 1;
const GICD_CTLR_ENABLE_G1: u32 = 1 << 1;
const GICD_CTLR_ARE_NS: u32 = 1 << 4;
const GICD_CTLR_ENABLE_G1A: u32 = 1 << 1;
/// GICD_CTLR.RWP (bit 31): a write to GICD_CTLR is still propagating (IHI 0069).
const GICD_CTLR_RWP: u32 = 1 << 31;

const GICC_CTLR: u64 = 0x0000;
const GICC_CTLR_ENABLE_G0: u32 = 1;
const GICC_CTLR_ENABLE_G1: u32 = 1 << 1;
/// IAR acknowledges Group 1 as well as Group 0 (IHI 0048).
const GICC_CTLR_ACK_CTL: u32 = 1 << 2;
const GICC_PMR: u64 = 0x0004;
const GICC_BPR: u64 = 0x0008;
const GICC_IAR: u64 = 0x000C;
const GICC_EOIR: u64 = 0x0010;

const GICR_CTLR: u64 = 0x0000;
const GICR_TYPER_LAST: u64 = 1 << 4;
/// GICR_TYPER.PLPIS (bit 0): the redistributor implements physical LPIs (IHI 0069).
const GICR_TYPER_PLPIS: u64 = 1;
/// Smallest redistributor frame; a frame with VLPIS is 0x40000 (`gic::gicr_frame_stride`).
const GICR_STRIDE: u64 = 0x2_0000;
const GICR_WAKER: u64 = 0x0014;
const GICR_WAKER_PS: u32 = 1 << 1;
const GICR_WAKER_CA: u32 = 1 << 2;
const GICR_CTLR_ENABLE_LPIS: u32 = 1;
/// GICR_CTLR.RWP (bit 3): an EnableLPIs write is still propagating (IHI 0069).
const GICR_CTLR_RWP: u32 = 1 << 3;
const GICR_SGI_BASE: u64 = 0x1_0000;
const GICR_IGROUPR0: u64 = GICR_SGI_BASE + 0x0080;
const GICR_ISENABLER0: u64 = GICR_SGI_BASE + 0x0100;
const GICR_ICENABLER0: u64 = GICR_SGI_BASE + 0x0180;
const GICR_IPRIORITYR: u64 = GICR_SGI_BASE + 0x0400;
const GICR_PROPBASER: u64 = 0x0070;
const GICR_PENDBASER: u64 = 0x0078;

const GICR_INVALLR: u64 = 0x00B0;
const GICR_SYNCR: u64 = 0x00C0;
/// GICR_PROPBASER / GICR_PENDBASER InnerCache (bits [9:7]): Normal Inner
/// write-back, read- and write-allocate, `0b111` (IHI 0069).
const GICR_XBASER_INNER_WB: u64 = (0b11 << 8) | (0b01 << 7);
/// LPI property and pending tables are 16 pages each (`alloc_pages(16)`).
const LPI_TABLE_BYTES: usize = 16 * 4096;

static CHIP_OBJ: GicChip = GicChip;
static CHIP_DYN: BootCell<&'static dyn IrqChip> = BootCell::new();
static DISPATCH: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the INTID dispatcher. `irq_init::init` calls it once.
pub fn set_dispatch(f: fn(u32)) {
    // Release: pairs with the Acquire load in `handle_irq`.
    DISPATCH.store(f as *mut (), Ordering::Release);
}

struct GicChip;

pub fn chip() -> Option<&'static dyn IrqChip> {
    CHIP_DYN.try_get().copied()
}

fn map_mmio(r: PhysRange) -> Option<u64> {
    let size = r.size.max(0x1000);
    // SAFETY: `r` is a GIC MMIO range from MachineDesc; established by
    // `crate::arch::aarch64::gic::init` from the device tree.
    unsafe { paging_init::ioremap(PhysAddr(r.start), size) }.map(|v| v.as_u64())
}

/// Bring up the boot CPU's GIC from `MachineDesc`.
///
/// # Safety
/// KVA and paging are live; single CPU; IRQs still masked.
pub unsafe fn init() {
    let Some(desc) = machine_init::info() else {
        crate::boot::halt_with("vibeOS: gic: no machine desc");
    };
    let (kind, dist, cpu_or_redist) = if let Some((d, r)) = desc.gic_v3() {
        (Kind::V3, d, r)
    } else if let Some((d, c)) = desc.gic_v2() {
        (Kind::V2, d, c)
    } else {
        crate::boot::halt_with("vibeOS: gic: no controller");
    };
    let dist_va = match map_mmio(dist) {
        Some(v) => v,
        None => crate::boot::halt_with("vibeOS: gic: dist map"),
    };
    let cpu_or_redist = match kind {
        Kind::V3 => PhysRange {
            start: cpu_or_redist.start,
            size: cpu_or_redist.size.max(GICR_STRIDE),
        },
        Kind::V2 => cpu_or_redist,
    };
    let redist_pa = cpu_or_redist.start;
    let redist_size = cpu_or_redist.size;
    let cpu_va = match map_mmio(cpu_or_redist) {
        Some(v) => v,
        None => crate::boot::halt_with("vibeOS: gic: cpu/redist map"),
    };
    let its = desc.gic_its().and_then(|(r, _)| map_mmio(r));
    let v2m = desc.gic_v2m().next().and_then(|(r, base, n)| {
        let va = map_mmio(r)?;
        let (base, n) = v2m_spi_range(va, base, n);
        // SAFETY: `dist_va` is the GICD mapped above; GICD_TYPER is read-only,
        // read here before `init_v2`/`init_v3` program the distributor. established here.
        let lines = gic::gicd_lines(unsafe { mmio32(dist_va, GICD_TYPER) });
        if !gic::v2m_range_ok(base, u32::from(n), lines) {
            crate::klog!(
                Level::Error,
                "vibeOS: gic: v2m SPIs {base} + {n} are not all SPIs below {lines}; v2m absent"
            );
            return None;
        }
        Some((va, base, n))
    });
    let mut g = Gic {
        kind,
        rss: false,
        dist: dist_va,
        cpu_or_redist: cpu_va,
        redist_pa,
        redist_size,
        lpi_prop: AtomicU64::new(0),
        its,
        v2m,
        spi_irqs: [const { AtomicU32::new(0) }; SPI_SLOTS],
        lpi_irqs: [const { AtomicU32::new(0) }; LPI_SLOTS],
        next_lpi: AtomicU32::new(0),
        next_v2m: AtomicU32::new(0),
    };
    match kind {
        Kind::V3 => {
            init_v3(&g);
            g.rss = icc_ctlr_rss();
        }
        Kind::V2 => init_v2(&g),
    }
    // SAFETY: I22, one write on the BSP before SMP; established here.
    unsafe {
        CHIP.set(g);
        CHIP_DYN.set(&CHIP_OBJ as &'static dyn IrqChip);
    }
    match kind {
        Kind::V3 => crate::marker!("vibeOS: gic: v3"),
        Kind::V2 => crate::marker!("vibeOS: gic: v2"),
    }
}

fn init_v2(g: &Gic) {
    // SAFETY: `g.dist` / `g.cpu_or_redist` are the mapped GICD/GICC. established here.
    unsafe {
        let lines = gic::gicd_lines(mmio32(g.dist, GICD_TYPER));
        mmio32w(g.dist, GICD_CTLR, 0);
        let mut i = 0u32;
        while i < lines {
            mmio32w(g.dist, GICD_IGROUPR + u64::from(i / 32) * 4, 0xFFFF_FFFF);
            if i >= 32 {
                mmio32w(g.dist, GICD_ICENABLER + u64::from(i / 32) * 4, 0xFFFF_FFFF);
                // CPU0 in each of the four target bytes (IHI 0048).
                mmio32w(g.dist, GICD_ITARGETSR + u64::from(i), 0x0101_0101);
            }
            mmio32w(
                g.dist,
                GICD_IPRIORITYR + u64::from(i),
                u32::from(gic::priority_for(i))
                    | u32::from(gic::priority_for(i.saturating_add(1))) << 8
                    | u32::from(gic::priority_for(i.saturating_add(2))) << 16
                    | u32::from(gic::priority_for(i.saturating_add(3))) << 24,
            );
            i = i.saturating_add(4);
        }
        if let Some((_, base, count)) = g.v2m {
            let mut n = 0u32;
            while n < u32::from(count) {
                let intid = base.saturating_add(n);
                if gic::is_spi(intid) {
                    // SETSPI_NS is a pulse; the SPI must be edge (IHI 0048).
                    let off = GICD_ICFGR + u64::from(intid / 16) * 4;
                    let shift = (intid % 16) * 2;
                    let cur = mmio32(g.dist, off);
                    mmio32w(g.dist, off, (cur & !(3 << shift)) | (2 << shift));
                }
                n = n.saturating_add(1);
            }
        }
        // SGIs and PPIs: enable 0..31.
        mmio32w(g.dist, GICD_ISENABLER, 0xFFFF_FFFF);
        // GICv2 always has groups on QEMU: IGROUPR is Group 1, so both
        // EnableGrp0 and EnableGrp1 must be set (IHI 0048).
        mmio32w(g.dist, GICD_CTLR, GICD_CTLR_ENABLE_G0 | GICD_CTLR_ENABLE_G1);
        mmio32w(g.cpu_or_redist, GICC_PMR, 0xFF);
        mmio32w(g.cpu_or_redist, GICC_BPR, 0);
        mmio32w(
            g.cpu_or_redist,
            GICC_CTLR,
            GICC_CTLR_ENABLE_G0 | GICC_CTLR_ENABLE_G1 | GICC_CTLR_ACK_CTL,
        );
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

/// MSI_TYPER when the device tree omits `arm,msi-base-spi`.
fn v2m_spi_range(va: u64, base: u32, n: u16) -> (u32, u16) {
    if n != 0 {
        return (base, n);
    }
    // SAFETY: `va` is the mapped GICv2m frame. established here.
    let typer = unsafe { mmio32(va, u64::from(gic::V2M_MSI_TYPER)) };
    (
        gic::v2m_typer_spi_base(typer),
        u16::try_from(gic::v2m_typer_spi_count(typer)).unwrap_or(0),
    )
}

fn mpidr_aff32() -> u64 {
    let v: u64;
    // SAFETY: MPIDR_EL1; established here.
    unsafe {
        core::arch::asm!(
            "mrs {0}, mpidr_el1",
            out(reg) v,
            options(nomem, nostack, preserves_flags)
        );
    }
    (v & 0x00FF_FFFF) | (((v >> 32) & 0xFF) << 24)
}

/// This CPU's redistributor VA, or `None` when no frame in the window has
/// its affinity. There is no fallback frame: a CPU must own its own.
fn this_redist(g: &Gic) -> Option<u64> {
    find_redist(g, mpidr_aff32())
}

fn find_redist(g: &Gic, aff: u64) -> Option<u64> {
    if g.kind != Kind::V3 {
        return None;
    }
    let mut off = 0u64;
    while off.saturating_add(GICR_STRIDE) <= g.redist_size {
        let rd = g.cpu_or_redist.wrapping_add(off);
        // SAFETY: `rd` is inside the mapped redistributor window. established here.
        let typer = unsafe { mmio64(rd, GICR_TYPER) };
        if (typer >> 32) == aff {
            return Some(rd);
        }
        if typer & GICR_TYPER_LAST != 0 {
            break;
        }
        off = off.saturating_add(gic::gicr_frame_stride(typer));
    }
    None
}

/// Poll an `RWP` bit after a write to a GIC control register, up to a bound.
/// On timeout it logs an Error line and returns, and boot goes on.
///
/// # Safety
/// `va + off` is a mapped GIC control register the caller owns.
unsafe fn wait_rwp(va: u64, off: u64, rwp: u32, what: &str) {
    let mut n = 100_000u32;
    while n > 0 {
        // SAFETY: this fn's `# Safety`; established here.
        if unsafe { mmio32(va, off) } & rwp == 0 {
            return;
        }
        n -= 1;
        core::hint::spin_loop();
    }
    crate::klog!(Level::Error, "vibeOS: gic: {what} rwp timeout");
}

fn init_v3(g: &Gic) {
    // SAFETY: mapped GICD/GICR; system registers are the CPU interface. established here.
    unsafe {
        let Some(rd) = this_redist(g) else {
            crate::klog!(Level::Error, "vibeOS: gic: no redistributor for boot CPU");
            crate::boot::halt_with("vibeOS: gic: no redistributor");
        };
        wake_redist(rd);
        let lines = gic::gicd_lines(mmio32(g.dist, GICD_TYPER));
        mmio32w(g.dist, GICD_CTLR, 0);
        wait_rwp(g.dist, GICD_CTLR, GICD_CTLR_RWP, "gicd");
        let mut i = 32u32;
        while i < lines {
            mmio32w(g.dist, GICD_IGROUPR + u64::from(i / 32) * 4, 0xFFFF_FFFF);
            mmio32w(g.dist, GICD_ICENABLER + u64::from(i / 32) * 4, 0xFFFF_FFFF);
            let p = gic::priority_for(i);
            mmio32w(
                g.dist,
                GICD_IPRIORITYR + u64::from(i),
                u32::from(p) | u32::from(p) << 8 | u32::from(p) << 16 | u32::from(p) << 24,
            );
            i = i.saturating_add(4);
        }
        mmio32w(g.dist, GICD_CTLR, GICD_CTLR_ENABLE_G1A | GICD_CTLR_ARE_NS);
        wait_rwp(g.dist, GICD_CTLR, GICD_CTLR_RWP, "gicd");
        // IROUTER is RAZ/WI until ARE_NS is set, so route after the write and its RWP wait.
        let mut n = 32u32;
        while n < lines {
            mmio64w(g.dist, gic::gicd_irouter(n), 0);
            n = n.saturating_add(1);
        }
        program_sgi_ppi(rd);
        // Fail closed: without its LPI tables the boot CPU cannot take an MSI,
        // and an ITS would map LPIs into nothing. A machine with no physical
        // LPIs returns true from `enable_lpi` and skips this.
        if !enable_lpi(g, rd) {
            crate::boot::halt_with("vibeOS: gic: lpi tables");
        }
        icc_enable();
        if let Some(its) = g.its {
            init_its(g, its, rd);
        }
    }
}

/// Clear `GICR_WAKER.ProcessorSleep` and wait for ChildrenAsleep.
///
/// # Safety
/// `rd` is the mapped redistributor this CPU owns.
unsafe fn wake_redist(rd: u64) {
    // SAFETY: `rd` is the mapped redistributor. established here.
    unsafe {
        let w = mmio32(rd, GICR_WAKER) & !GICR_WAKER_PS;
        mmio32w(rd, GICR_WAKER, w);
        let mut n = 100_000u32;
        while n > 0 {
            if mmio32(rd, GICR_WAKER) & GICR_WAKER_CA == 0 {
                return;
            }
            n -= 1;
            core::hint::spin_loop();
        }
    }
}

/// Banked SGI/PPI Group 1, priorities, enable (GICv2 distributor view).
///
/// # Safety
/// Called on this CPU with IRQs masked; `dist` is the mapped distributor.
unsafe fn program_sgi_ppi_v2(dist: u64) {
    // SAFETY: this fn's `# Safety`; established here.
    unsafe {
        mmio32w(dist, GICD_IGROUPR, 0xFFFF_FFFF);
        let mut s = 0u32;
        while s < 32 {
            mmio32w(
                dist,
                GICD_IPRIORITYR + u64::from(s),
                u32::from(gic::priority_for(s))
                    | u32::from(gic::priority_for(s.saturating_add(1))) << 8
                    | u32::from(gic::priority_for(s.saturating_add(2))) << 16
                    | u32::from(gic::priority_for(s.saturating_add(3))) << 24,
            );
            s = s.saturating_add(4);
        }
        mmio32w(dist, GICD_ISENABLER, 0xFFFF_FFFF);
    }
}

/// SGI/PPI Group 1, priorities, enable.
///
/// # Safety
/// `rd` is this CPU's mapped redistributor.
unsafe fn program_sgi_ppi(rd: u64) {
    // SAFETY: `rd` is this CPU's redistributor. established here.
    unsafe {
        mmio32w(rd, GICR_IGROUPR0, 0xFFFF_FFFF);
        mmio32w(rd, GICR_ICENABLER0, 0);
        let mut s = 0u32;
        while s < 32 {
            let p = gic::priority_for(s);
            mmio32w(
                rd,
                GICR_IPRIORITYR + u64::from(s),
                u32::from(p) | u32::from(p) << 8 | u32::from(p) << 16 | u32::from(p) << 24,
            );
            s = s.saturating_add(4);
        }
        mmio32w(rd, GICR_ISENABLER0, 0xFFFF_FFFF);
    }
}

/// Program redistributor LPI tables; never clear EnableLPIs. Shared
/// property table, per-CPU pending table. `false` when a table could not be
/// allocated. A redistributor without physical LPIs has no tables to program,
/// so that is `true` and nothing is written.
///
/// # Safety
/// `g` is the live GIC; `rd` is this CPU's redistributor.
unsafe fn enable_lpi(g: &Gic, rd: u64) -> bool {
    // SAFETY: `rd` is this CPU's redistributor, mapped by `init`/`enable_ap`. established here.
    if unsafe { mmio64(rd, GICR_TYPER) } & GICR_TYPER_PLPIS == 0 {
        return true;
    }
    // Acquire: pairs with the Release/AcqRel store of `lpi_prop` here.
    let existing = g.lpi_prop.load(Ordering::Acquire);
    let prop = if existing != 0 {
        existing
    } else {
        let Some(prop) = alloc_pages(16) else {
            crate::klog!(Level::Error, "vibeOS: gic: lpi prop table");
            return false;
        };
        let va = crate::paging_init::hhdm_offset().wrapping_add(prop);
        // Enable the LPIs this chip will allocate as Group 1 (DESIGN §5.4).
        // SAFETY: `va` is the HHDM alias of the property table (I14). established here.
        unsafe {
            let mut i = 0u32;
            while i < LPI_SLOTS as u32 {
                let Some(off) = gic::lpi_prop_index(LPI_BASE + i) else {
                    break;
                };
                let p = (va as *mut u8).wrapping_add(off);
                p.write(gic::lpi_config(gic::PRIO_DEVICE));
                i = i.saturating_add(1);
            }
            core::arch::asm!("dsb sy", options(nostack, preserves_flags));
        }
        // AcqRel: pairs with another CPU's Acquire failure load of `lpi_prop`.
        // Acquire: pairs with the successful AcqRel store of `lpi_prop`.
        match g
            .lpi_prop
            .compare_exchange(0, prop, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => prop,
            Err(other) => other,
        }
    };
    // GICR_PENDBASER is 64 KiB-aligned (IHI 0069); 16 pages gives that.
    let Some(pend) = alloc_pages(16) else {
        crate::klog!(Level::Error, "vibeOS: gic: lpi pend table");
        return false;
    };
    // SAFETY: `rd` is this CPU's redistributor. established here.
    unsafe {
        let idbits = 15u64;
        let prop_val = prop | idbits | XBASER_SHARE_IS | GICR_XBASER_INNER_WB;
        let pend_val = pend | XBASER_SHARE_IS | GICR_XBASER_INNER_WB | (1 << 62);
        mmio64w(rd, GICR_PROPBASER, prop_val);
        mmio64w(rd, GICR_PENDBASER, pend_val);
        let prop_read = mmio64(rd, GICR_PROPBASER);
        let pend_read = mmio64(rd, GICR_PENDBASER);
        note_shareability(prop_val, prop_read);
        note_shareability(pend_val, pend_read);
        // The tables are cleaned after the readback, before LPIs can read them.
        clean_table_if_needed(prop, LPI_TABLE_BYTES);
        clean_table_if_needed(pend, LPI_TABLE_BYTES);
        let c = mmio32(rd, GICR_CTLR) | GICR_CTLR_ENABLE_LPIS;
        mmio32w(rd, GICR_CTLR, c);
        wait_rwp(rd, GICR_CTLR, GICR_CTLR_RWP, "gicr");
        mmio64w(rd, GICR_INVALLR, 0);
        let mut n = 100_000u32;
        while n > 0 && mmio32(rd, GICR_SYNCR) & 1 != 0 {
            n -= 1;
            core::hint::spin_loop();
        }
    }
    true
}

/// Enable the GICv3 system-register CPU interface.
///
/// # Safety
/// Boot CPU only; distributor and redistributor are already live.
unsafe fn icc_enable() {
    // SAFETY: ICC_* are the GICv3 CPU interface; boot CPU only here.
    unsafe {
        core::arch::asm!(
            "mrs {0}, ICC_SRE_EL1",
            "orr {0}, {0}, #1",
            "msr ICC_SRE_EL1, {0}",
            "isb",
            // EOImode (bit 1) clear: one ICC_EOIR1_EL1 write also deactivates
            // the interrupt, and nothing here writes ICC_DIR_EL1 (ARM ARM).
            // Firmware may have left it set.
            "mrs {0}, ICC_CTLR_EL1",
            "bic {0}, {0}, #2",
            "msr ICC_CTLR_EL1, {0}",
            "isb",
            "mov {0}, #0xff",
            "msr ICC_PMR_EL1, {0}",
            "mov {0}, #0",
            "msr ICC_BPR1_EL1, {0}",
            "mov {0}, #1",
            "msr ICC_IGRPEN1_EL1, {0}",
            "isb",
            out(reg) _,
            options(nostack, preserves_flags),
        );
    }
}

pub fn handle_irq() {
    let Some(g) = gic() else {
        return;
    };
    let (intid, eoi_val) = ack(g);
    // Before EOI or any GICD write: 1020–1023 are special (1023 is
    // spurious) and 1024–8191 are reserved. LPIs are 8192+ and must
    // be dispatched (IHI 0069).
    if gic::ack_drops(intid) {
        return;
    }
    // DESIGN §5.8: EOI before the timer/IPI body, which may preempt.
    // A device SPI/LPI EOIs after the top half, so a level line is
    // dropped (virtio-mmio InterruptACK) before deactivate.
    let tick = crate::arch::aarch64::timer::intid();
    let early = gic::is_sgi(intid) || (tick != 0 && intid == tick);
    if early {
        eoi(g, eoi_val);
    }
    // Acquire: pairs with the Release store in `set_dispatch`.
    let p = DISPATCH.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: a non-null hook holds a `fn(u32)`; established by
        // `crate::arch::aarch64::gic::set_dispatch`, its only store.
        let f = unsafe { core::mem::transmute::<*mut (), fn(u32)>(p) };
        f(intid);
    }
    if !early {
        eoi(g, eoi_val);
    }
}

/// `(intid, eoi)`. GICv2 EOI is the raw `GICC_IAR`, CPUID bits included.
/// GICv3 `ICC_IAR1_EL1` has no CPUID field, so both are the INTID.
fn ack(g: &Gic) -> (u32, u32) {
    match g.kind {
        Kind::V2 => {
            // SAFETY: GICC_IAR. established here.
            let raw = unsafe { mmio32(g.cpu_or_redist, GICC_IAR) };
            let split = gic::v2_ack(raw);
            (split.intid, split.eoi)
        }
        Kind::V3 => {
            let v: u64;
            // SAFETY: ICC_IAR1_EL1. established here.
            unsafe {
                core::arch::asm!(
                    "mrs {0}, ICC_IAR1_EL1",
                    out(reg) v,
                    options(nomem, nostack, preserves_flags)
                );
            }
            let intid = v as u32;
            (intid, intid)
        }
    }
}

/// Write EOI. On GICv2 `eoir` is the raw `GICC_IAR` (`gic::v2_ack`).
fn eoi(g: &Gic, eoir: u32) {
    match g.kind {
        Kind::V2 => {
            // SAFETY: GICC_EOIR. established here.
            unsafe { mmio32w(g.cpu_or_redist, GICC_EOIR, eoir) };
        }
        Kind::V3 => {
            // SAFETY: ICC_EOIR1_EL1. established here.
            unsafe {
                core::arch::asm!(
                    "msr ICC_EOIR1_EL1, {0}",
                    in(reg) u64::from(eoir),
                    options(nostack, preserves_flags)
                );
            }
        }
    }
}

/// `ICC_CTLR_EL1.RSS` (ARM ARM bit 18): range selectors in `ICC_SGI1R_EL1`.
const ICC_CTLR_RSS: u64 = 1 << 18;

fn icc_ctlr_rss() -> bool {
    let v: u64;
    // SAFETY: ICC_CTLR_EL1 is the GICv3 CPU interface control register,
    // readable after `icc_enable` sets ICC_SRE_EL1.SRE. established here.
    unsafe {
        core::arch::asm!(
            "mrs {0}, ICC_CTLR_EL1",
            out(reg) v,
            options(nomem, nostack, preserves_flags)
        );
    }
    (v & ICC_CTLR_RSS) != 0
}

/// Aff3 of `hw_id` is 0 (`gic::aff3_ok`), and Aff0 can be named in
/// `ICC_SGI1R_EL1`. GICv2 does not use that register. No chip yet is not a refusal.
pub fn cpu_ok(hw_id: u64) -> Result<(), gic::SgiError> {
    gic::aff3_ok(hw_id)?;
    let Some(g) = gic() else {
        return Ok(());
    };
    if g.kind != Kind::V3 {
        return Ok(());
    }
    gic::sgi1r(0, hw_id, g.rss)?;
    Ok(())
}

fn write_sgi1r(val: u64) {
    // SAFETY: DESIGN §7.6: dsb ishst; ICC_SGI1R; isb. established here.
    unsafe {
        core::arch::asm!(
            "dsb ishst",
            "msr ICC_SGI1R_EL1, {0}",
            "isb",
            in(reg) val,
            options(nostack, preserves_flags),
        );
    }
}

pub fn send_sgi(intid: u32) {
    send_sgi_to(0, intid);
}

/// SGI to logical CPU `cpu` (its `PerCpuRemote.apic_id` is the MPIDR).
pub fn send_sgi_to(cpu: u32, intid: u32) {
    let Some(g) = gic() else {
        return;
    };
    let hw = crate::per_cpu_init::cpu(cpu)
        .map(|r| {
            // Relaxed: the MPIDR is fixed after bring-up; pairs with nothing.
            u64::from(r.apic_id.load(Ordering::Relaxed))
        })
        .unwrap_or(0);
    match g.kind {
        Kind::V3 => match gic::sgi1r(intid, hw, g.rss) {
            Ok(v) => write_sgi1r(v),
            Err(e) => crate::klog_ratelimited!(
                1000,
                Level::Error,
                "vibeOS: gic: sgi {intid}: {}",
                e.as_str()
            ),
        },
        Kind::V2 => {
            let bit = (hw & 7) as u32;
            let sgir = (intid & 0xF) | (1 << (16 + bit));
            // SAFETY: ordered mmio_write (DESIGN §4.7). established here.
            unsafe {
                core::arch::asm!("dsb oshst", options(nostack, preserves_flags));
                mmio32w(g.dist, GICD_SGIR, sgir);
            }
        }
    }
}

/// SGI to every other CPU (ICC_SGI1R IRM, or GICD_SGIR filter 01).
pub fn send_sgi_others(intid: u32) {
    let Some(g) = gic() else {
        return;
    };
    match g.kind {
        Kind::V3 => write_sgi1r((u64::from(intid & 0xF) << 24) | (1u64 << 40)),
        Kind::V2 => {
            let sgir = (intid & 0xF) | (1 << 24);
            // SAFETY: ordered mmio_write (DESIGN §4.7). established here.
            unsafe {
                core::arch::asm!("dsb oshst", options(nostack, preserves_flags));
                mmio32w(g.dist, GICD_SGIR, sgir);
            }
        }
    }
}

/// Per-CPU redistributor / CPU interface on a secondary. `false` when this
/// CPU has no redistributor frame or its LPI tables could not be allocated;
/// the caller must park it before it reports ready, so the boot CPU sees a
/// timed-out AP.
///
/// # Safety
/// This CPU's GIC interface is unused; IRQs masked.
pub unsafe fn enable_ap() -> bool {
    let Some(g) = gic() else {
        return true;
    };
    // SAFETY: this fn's `# Safety`; established here.
    unsafe {
        match g.kind {
            Kind::V3 => {
                let Some(rd) = this_redist(g) else {
                    crate::klog!(
                        Level::Error,
                        "vibeOS: gic: no redistributor for AP aff {:#x}",
                        mpidr_aff32()
                    );
                    return false;
                };
                wake_redist(rd);
                program_sgi_ppi(rd);
                if !enable_lpi(g, rd) {
                    return false;
                }
                icc_enable();
                if let Some(its) = g.its {
                    map_collection(g, its, rd);
                }
            }
            Kind::V2 => {
                // SGI/PPI 0..31 are banked in the distributor. The BSP
                // write in `init` only armed that CPU.
                program_sgi_ppi_v2(g.dist);
                mmio32w(g.cpu_or_redist, GICC_PMR, 0xFF);
                mmio32w(g.cpu_or_redist, GICC_BPR, 0);
                mmio32w(
                    g.cpu_or_redist,
                    GICC_CTLR,
                    GICC_CTLR_ENABLE_G0 | GICC_CTLR_ENABLE_G1 | GICC_CTLR_ACK_CTL,
                );
                core::arch::asm!("dsb sy", options(nostack, preserves_flags));
            }
        }
    }
    true
}

fn enable_intid(g: &Gic, intid: u32, on: bool) {
    // LPIs are enabled in the property table, not GICD_ISENABLER (IHI 0069).
    // Special/reserved INTIDs have no enable bit; never write GICD for them.
    if gic::is_lpi(intid) || gic::is_special(intid) {
        return;
    }
    let bit = 1u32 << (intid % 32);
    let off = if on { GICD_ISENABLER } else { GICD_ICENABLER };
    if g.kind == Kind::V3 && intid < 32 {
        let o = if on { GICR_ISENABLER0 } else { GICR_ICENABLER0 };
        let Some(rd) = this_redist(g) else {
            crate::klog_ratelimited!(
                1000,
                Level::Error,
                "vibeOS: gic: no redistributor for SGI/PPI {intid}"
            );
            return;
        };
        // SAFETY: this CPU's redistributor SGI/PPI enable. established here.
        unsafe { mmio32w(rd, o, bit) };
        return;
    }
    // SAFETY: distributor enable bit. established here.
    unsafe { mmio32w(g.dist, off + u64::from(intid / 32) * 4, bit) };
}

/// Program `GICD_ICFGR` for an SPI. `edge` is virtio 1.2 / DT cell 2
/// value 1 (rising); level is 4.
pub fn set_spi_edge(intid: u32, edge: bool) {
    let Some(g) = gic() else {
        return;
    };
    if !gic::is_spi(intid) {
        return;
    }
    enable_intid(g, intid, false);
    let off = GICD_ICFGR + u64::from(intid / 16) * 4;
    let shift = (intid % 16) * 2;
    let bits = if edge { GICD_ICFGR_EDGE } else { 0 };
    // SAFETY: GICD_ICFGR for a disabled SPI. established here.
    unsafe {
        let cur = mmio32(g.dist, off);
        mmio32w(g.dist, off, (cur & !(3 << shift)) | (bits << shift));
    }
}

/// Reserve `want` slots below `limit` from `counter`, and return the first.
/// The counter moves only when all of them fit (`gic::slots_reserve`).
fn reserve_slots(counter: &AtomicU32, want: u32, limit: u32) -> Option<u32> {
    // Relaxed: slot counter; the exchange below retries on a change, and pairs with nothing.
    let mut cur = counter.load(Ordering::Relaxed);
    loop {
        let end = gic::slots_reserve(cur, want, limit)?;
        // Relaxed: slot counter; the failure value is the next try, and pairs with nothing.
        match counter.compare_exchange_weak(cur, end, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Some(cur),
            Err(seen) => cur = seen,
        }
    }
}

impl IrqChip for GicChip {
    fn translate(&self, spec: IrqSpecifier, _cpu: u32) -> Result<u32, IrqError> {
        match spec {
            IrqSpecifier::Gic { intid } => Ok(intid),
            _ => Err(IrqError::NoRoute),
        }
    }

    fn mask(&self, hwirq: u32) {
        if let Some(g) = gic() {
            enable_intid(g, hwirq, false);
        }
    }

    fn unmask(&self, hwirq: u32) {
        if let Some(g) = gic() {
            enable_intid(g, hwirq, true);
        }
    }

    fn eoi(&self, hwirq: u32) {
        if let Some(g) = gic() {
            eoi(g, hwirq);
        }
    }

    fn set_affinity(&self, hwirq: u32, cpu: u32) -> Result<(), IrqError> {
        let Some(g) = gic() else {
            return Err(IrqError::NoRoute);
        };
        let hw = crate::per_cpu_init::cpu(cpu)
            .map(|r| {
                // Relaxed: the MPIDR is fixed after bring-up; pairs with nothing.
                u64::from(r.apic_id.load(Ordering::Relaxed))
            })
            .unwrap_or(0);
        if g.kind == Kind::V3 && gic::is_lpi(hwirq) {
            let Some(its) = g.its else {
                return Err(IrqError::NoRoute);
            };
            return movi_lpi(its, hwirq, cpu);
        }
        if g.kind == Kind::V3 && gic::is_spi(hwirq) {
            let route = (hw & 0xFF)
                | (((hw >> 8) & 0xFF) << 8)
                | (((hw >> 16) & 0xFF) << 16)
                | (((hw >> 24) & 0xFF) << 32);
            // SAFETY: GICD_IROUTER for this SPI. established here.
            unsafe { mmio64w(g.dist, gic::gicd_irouter(hwirq), route) };
            return Ok(());
        }
        if g.kind == Kind::V2 && gic::is_spi(hwirq) {
            let bit = 1u32 << (hw & 7);
            let off = GICD_ITARGETSR + u64::from(hwirq & !3);
            let shift = (hwirq % 4) * 8;
            // SAFETY: GICD_ITARGETSR for this SPI. established here.
            unsafe {
                let cur = mmio32(g.dist, off);
                mmio32w(g.dist, off, (cur & !(0xFF << shift)) | (bit << shift));
            }
            return Ok(());
        }
        Ok(())
    }

    fn alloc_msi(&self, n: u8, _cpu: u32, out: &mut [u32]) -> Result<usize, IrqError> {
        let Some(g) = gic() else {
            return Err(IrqError::NoRoute);
        };
        let want = n as usize;
        if out.len() < want {
            return Err(IrqError::BadVector);
        }
        // The whole range is reserved before any slot is handed out, so a
        // refused request leaves the counter where it was.
        if g.kind == Kind::V3 && g.its.is_some() {
            let limit = u32::try_from(LPI_SLOTS).unwrap_or(0);
            let Some(first) = reserve_slots(&g.next_lpi, u32::from(n), limit) else {
                return Err(IrqError::Exhausted);
            };
            let mut i = 0;
            while i < want {
                let slot = first.checked_add(i as u32).ok_or(IrqError::Exhausted)?;
                let intid = LPI_BASE.checked_add(slot).ok_or(IrqError::Exhausted)?;
                if let Some(s) = out.get_mut(i) {
                    *s = intid;
                }
                i += 1;
            }
            return Ok(want);
        }
        if let Some((_, base, count)) = g.v2m {
            let Some(first) = reserve_slots(&g.next_v2m, u32::from(n), u32::from(count)) else {
                return Err(IrqError::Exhausted);
            };
            let mut i = 0;
            while i < want {
                let slot = first.checked_add(i as u32).ok_or(IrqError::Exhausted)?;
                // `init` checked that every SPI in the range is an SPI, so
                // this only refuses an overflow that cannot come from there.
                let Some(intid) = base.checked_add(slot).filter(|&s| gic::is_spi(s)) else {
                    return Err(IrqError::Exhausted);
                };
                if let Some(s) = out.get_mut(i) {
                    *s = intid;
                }
                i += 1;
            }
            return Ok(want);
        }
        Err(IrqError::NoRoute)
    }

    fn compose_msi(&self, hwirq: u32, _cpu: u32) -> Result<vibeos::irq::MsiMessage, IrqError> {
        let Some(g) = gic() else {
            return Err(IrqError::NoRoute);
        };
        if gic::is_lpi(hwirq) {
            let Some(its) = g.its else {
                return Err(IrqError::NoRoute);
            };
            let slot = (hwirq - LPI_BASE) as usize;
            // Acquire: pairs with the Release store in `map_its_event`.
            let event = LPI_EVENT
                .get(slot)
                .map(|e| e.load(Ordering::Acquire))
                .unwrap_or(0);
            let _ = its;
            let phys = machine_init::info()
                .and_then(|d| d.gic_its())
                .map(|(r, _)| r.start)
                .unwrap_or(0);
            let (addr, data) = gic::its_compose(phys, event);
            return Ok(vibeos::irq::MsiMessage { addr, data });
        }
        if let Some((_, _, _)) = g.v2m {
            let phys = machine_init::info()
                .and_then(|d| d.gic_v2m().next())
                .map(|(r, _, _)| r.start)
                .unwrap_or(0);
            let (addr, data) = gic::v2m_compose(phys, hwirq);
            return Ok(vibeos::irq::MsiMessage { addr, data });
        }
        Err(IrqError::NoRoute)
    }

    fn free(&self, hwirq: u32) {
        if gic::is_lpi(hwirq) {
            discard_lpi(hwirq);
        }
        self.mask(hwirq);
    }
}

pub fn publish(intid: u32, irq_raw: u32) {
    let Some(g) = gic() else {
        return;
    };
    if gic::is_lpi(intid) {
        let i = (intid - LPI_BASE) as usize;
        if let Some(s) = g.lpi_irqs.get(i) {
            // Release: pairs with the Acquire load in `lookup`.
            s.store(irq_raw, Ordering::Release);
        }
        return;
    }
    if let Some(s) = g.spi_irqs.get(intid as usize) {
        // Release: pairs with the Acquire load in `lookup`.
        s.store(irq_raw, Ordering::Release);
    }
}

pub fn lookup(intid: u32) -> u32 {
    let Some(g) = gic() else {
        return 0;
    };
    if gic::is_lpi(intid) {
        let i = (intid - LPI_BASE) as usize;
        return g
            .lpi_irqs
            .get(i)
            // Acquire: pairs with nothing.
            .map(|s| s.load(Ordering::Acquire))
            .unwrap_or(0);
    }
    g.spi_irqs
        .get(intid as usize)
        // Acquire: pairs with nothing.
        .map(|s| s.load(Ordering::Acquire))
        .unwrap_or(0)
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn version_v3() -> bool {
    gic().is_some_and(|g| g.kind == Kind::V3)
}

#[cfg(feature = "kernel_tests")]
pub fn has_its() -> bool {
    gic().is_some_and(|g| g.its.is_some())
}

#[cfg(feature = "kernel_tests")]
static WATCH_INTID: AtomicU32 = AtomicU32::new(0);
#[cfg(feature = "kernel_tests")]
static WATCH_SEEN: AtomicU32 = AtomicU32::new(0);

/// Arm a dispatch counter for `intid`. `0` disarms.
#[cfg(feature = "kernel_tests")]
pub fn ktest_watch_intid(intid: u32) {
    // Release: pairs with the Acquire load in `ktest_note`.
    WATCH_SEEN.store(0, Ordering::Release);
    // Release: pairs with the Acquire load in `ktest_note`.
    WATCH_INTID.store(intid, Ordering::Release);
}

#[cfg(feature = "kernel_tests")]
pub fn ktest_lpi_seen() -> u32 {
    // Acquire: pairs with the Release store in `ktest_note`.
    WATCH_SEEN.load(Ordering::Acquire)
}

#[cfg(feature = "kernel_tests")]
pub fn ktest_note(intid: u32) {
    // Acquire: pairs with the Release store in `ktest_watch_intid`.
    if intid != 0 && WATCH_INTID.load(Ordering::Acquire) == intid {
        // Release: pairs with the Acquire load in `ktest_lpi_seen`.
        WATCH_SEEN.fetch_add(1, Ordering::Release);
    }
}
