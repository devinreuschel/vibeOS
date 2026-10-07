//! GICv2 / GICv3 + ITS / GICv2m as `IrqChip`s (DESIGN §5.4, §11.5).

use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

use vibeos::irq::{IrqChip, IrqError, IrqSpecifier, ItsCommand, gic};
use vibeos::lock::RANK_DEVICE;
use vibeos::log::Level;
use vibeos::machine::PhysRange;
use vibeos::paging::PhysAddr;

use crate::cell::BootCell;
use crate::machine_init;
use crate::paging_init;
use crate::sync_init::SpinMutex;

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
const GICR_TYPER: u64 = 0x0008;
const GICR_TYPER_LAST: u64 = 1 << 4;
const GICR_STRIDE: u64 = 0x2_0000;
const GICR_WAKER: u64 = 0x0014;
const GICR_WAKER_PS: u32 = 1 << 1;
const GICR_WAKER_CA: u32 = 1 << 2;
const GICR_CTLR_ENABLE_LPIS: u32 = 1;
const GICR_SGI_BASE: u64 = 0x1_0000;
const GICR_IGROUPR0: u64 = GICR_SGI_BASE + 0x0080;
const GICR_ISENABLER0: u64 = GICR_SGI_BASE + 0x0100;
const GICR_ICENABLER0: u64 = GICR_SGI_BASE + 0x0180;
const GICR_IPRIORITYR: u64 = GICR_SGI_BASE + 0x0400;
const GICR_PROPBASER: u64 = 0x0070;
const GICR_PENDBASER: u64 = 0x0078;

const GITS_CTLR: u64 = 0x0000;
const GITS_TYPER: u64 = 0x0008;
/// GITS_TYPER.PTA: MAPC's RDbase is the redistributor PA[51:16], not its
/// processor number (IHI 0069G).
const GITS_TYPER_PTA: u64 = 1 << 19;
const GITS_CBASER: u64 = 0x0080;
const GITS_CWRITER: u64 = 0x0088;
const GITS_CREADR: u64 = 0x0090;
const GITS_BASER: u64 = 0x0100;
const GITS_CTLR_ENABLED: u32 = 1;
/// GITS_CREADR bit 0: the ITS stalled on the command at this offset.
const GITS_CREADR_STALLED: u64 = 1;
const GICR_INVALLR: u64 = 0x00B0;
const GICR_SYNCR: u64 = 0x00C0;
/// GITS_CBASER / GITS_BASER physical address (IHI 0069G bits [51:12]).
const ITS_PA_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const GITS_BASER_TYPE_DEVICE: u64 = 1;
const GITS_BASER_TYPE_COLLECTION: u64 = 4;

const SPI_SLOTS: usize = 1024;
const LPI_SLOTS: usize = 256;
const LPI_BASE: u32 = gic::LPI_BASE;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    V2,
    V3,
}

struct Gic {
    kind: Kind,
    /// `ICC_CTLR_EL1.RSS`, sampled on the BSP in `init`.
    rss: bool,
    dist: u64,
    cpu_or_redist: u64,
    /// Physical base of the redistributor window (`cpu_or_redist` is its VA).
    redist_pa: u64,
    redist_size: u64,
    lpi_prop: AtomicU64,
    its: Option<u64>,
    v2m: Option<(u64, u32, u16)>,
    spi_irqs: [AtomicU32; SPI_SLOTS],
    lpi_irqs: [AtomicU32; LPI_SLOTS],
    next_lpi: AtomicU32,
    next_v2m: AtomicU32,
}

static CHIP: BootCell<Gic> = BootCell::new();
static CHIP_OBJ: GicChip = GicChip;
static CHIP_DYN: BootCell<&'static dyn IrqChip> = BootCell::new();
static DISPATCH: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the INTID dispatcher. `irq_init::init` calls it once.
pub fn set_dispatch(f: fn(u32)) {
    // Release: pairs with the Acquire load in `handle_irq`.
    DISPATCH.store(f as *mut (), Ordering::Release);
}

struct GicChip;

fn gic() -> Option<&'static Gic> {
    CHIP.try_get()
}

pub fn chip() -> Option<&'static dyn IrqChip> {
    CHIP_DYN.try_get().copied()
}

fn map_mmio(r: PhysRange) -> Option<u64> {
    let size = r.size.max(0x1000);
    // SAFETY: `r` is a GIC MMIO range from MachineDesc; established by
    // `crate::arch::aarch64::gic::init` from the device tree.
    unsafe { paging_init::ioremap(PhysAddr(r.start), size) }.map(|v| v.as_u64())
}

/// Read a 32-bit GIC register.
///
/// # Safety
/// `va+off` is a mapped GIC register the caller owns.
unsafe fn mmio32(va: u64, off: u64) -> u32 {
    // SAFETY: this fn's `# Safety`; established by `crate::arch::aarch64::gic::init`.
    unsafe { core::ptr::read_volatile((va.wrapping_add(off)) as *const u32) }
}

/// Write a 32-bit GIC register.
///
/// # Safety
/// As [`mmio32`].
unsafe fn mmio32w(va: u64, off: u64, v: u32) {
    // SAFETY: this fn's `# Safety`; established by `crate::arch::aarch64::gic::init`.
    unsafe { core::ptr::write_volatile((va.wrapping_add(off)) as *mut u32, v) };
}

/// Read a 64-bit GIC register.
///
/// # Safety
/// As [`mmio32`].
unsafe fn mmio64(va: u64, off: u64) -> u64 {
    // SAFETY: this fn's `# Safety`; established by `crate::arch::aarch64::gic::init`.
    unsafe { core::ptr::read_volatile((va.wrapping_add(off)) as *const u64) }
}

/// Write a 64-bit GIC register.
///
/// # Safety
/// As [`mmio32`].
unsafe fn mmio64w(va: u64, off: u64, v: u64) {
    // SAFETY: this fn's `# Safety`; established by `crate::arch::aarch64::gic::init`.
    unsafe { core::ptr::write_volatile((va.wrapping_add(off)) as *mut u64, v) };
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
        let typer = mmio32(g.dist, GICD_TYPER);
        let lines = ((typer & 0x1F) + 1) * 32;
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

/// This CPU's redistributor VA, or the mapped base if the walk misses.
fn this_redist(g: &Gic) -> u64 {
    find_redist(g, mpidr_aff32()).unwrap_or(g.cpu_or_redist)
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
        off = off.saturating_add(GICR_STRIDE);
    }
    None
}

fn init_v3(g: &Gic) {
    // SAFETY: mapped GICD/GICR; system registers are the CPU interface. established here.
    unsafe {
        let rd = this_redist(g);
        wake_redist(rd);
        let typer = mmio32(g.dist, GICD_TYPER);
        let lines = ((typer & 0x1F) + 1) * 32;
        mmio32w(g.dist, GICD_CTLR, 0);
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
            let mut j = 0u32;
            while j < 4 {
                let n = i.saturating_add(j);
                if n < lines {
                    mmio64w(g.dist, gic::gicd_irouter(n), 0);
                }
                j = j.saturating_add(1);
            }
            i = i.saturating_add(4);
        }
        mmio32w(g.dist, GICD_CTLR, GICD_CTLR_ENABLE_G1A | GICD_CTLR_ARE_NS);
        program_sgi_ppi(rd);
        enable_lpi(g, rd);
        icc_enable();
        if let Some(its) = g.its {
            init_its(g, its);
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
/// property table, per-CPU pending table.
///
/// # Safety
/// `g` is the live GIC; `rd` is this CPU's redistributor.
unsafe fn enable_lpi(g: &Gic, rd: u64) {
    // Acquire: pairs with the Release/AcqRel store of `lpi_prop` here.
    let existing = g.lpi_prop.load(Ordering::Acquire);
    let prop = if existing != 0 {
        existing
    } else {
        let Some(prop) = alloc_pages(16) else {
            crate::klog!(Level::Error, "vibeOS: gic: lpi prop table");
            return;
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
        return;
    };
    // SAFETY: `rd` is this CPU's redistributor. established here.
    unsafe {
        let idbits = 15u64;
        mmio64w(
            rd,
            GICR_PROPBASER,
            prop | idbits | (1 << 10) | (0b11 << 8) | (0b01 << 7),
        );
        mmio64w(
            rd,
            GICR_PENDBASER,
            pend | (1 << 10) | (0b11 << 8) | (1 << 62),
        );
        let c = mmio32(rd, GICR_CTLR) | GICR_CTLR_ENABLE_LPIS;
        mmio32w(rd, GICR_CTLR, c);
        mmio64w(rd, GICR_INVALLR, 0);
        let mut n = 100_000u32;
        while n > 0 && mmio32(rd, GICR_SYNCR) & 1 != 0 {
            n -= 1;
            core::hint::spin_loop();
        }
    }
}

fn alloc_pages(pages: usize) -> Option<u64> {
    let order = pages.trailing_zeros() as u8;
    if 1usize.checked_shl(u32::from(order))? != pages {
        return None;
    }
    let f = crate::pmm_init::with_buddy(|b| b.alloc(order))?;
    let pa = f.into_entry();
    let va = crate::paging_init::hhdm_offset().wrapping_add(pa);
    // SAFETY: buddy block, HHDM maps it (I14). established here.
    unsafe { core::ptr::write_bytes(va as *mut u8, 0, pages.saturating_mul(4096)) };
    Some(pa)
}

const ITS_DEV_MAX: usize = 16;
const ITS_DEV_EMPTY: u32 = u32::MAX;

struct ItsDev {
    id: AtomicU32,
    itt: AtomicU64,
    next_event: AtomicU32,
}

static ITS_DEVS: [ItsDev; ITS_DEV_MAX] = [const {
    ItsDev {
        id: AtomicU32::new(ITS_DEV_EMPTY),
        itt: AtomicU64::new(0),
        next_event: AtomicU32::new(0),
    }
}; ITS_DEV_MAX];
/// EventID programmed for each LPI slot, for `compose_msi`.
static LPI_EVENT: [AtomicU32; LPI_SLOTS] = [const { AtomicU32::new(0) }; LPI_SLOTS];
/// DeviceID of a mapped LPI. [`LPI_NO_DEV`] means `map_its_event` has not
/// published one, so `set_affinity` cannot `MOVI` it.
const LPI_NO_DEV: u32 = u32::MAX;
static LPI_DEV: [AtomicU32; LPI_SLOTS] = [const { AtomicU32::new(LPI_NO_DEV) }; LPI_SLOTS];
/// Collection (logical CPU) the LPI was last moved to.
static LPI_COL: [AtomicU32; LPI_SLOTS] = [const { AtomicU32::new(0) }; LPI_SLOTS];
static CMDQ: AtomicU64 = AtomicU64::new(0);
/// One command queue. MAPC, MAPTI, MOVI and DISCARD all push here.
static ITS_CMDS: SpinMutex<()> = SpinMutex::with_rank((), RANK_DEVICE);
/// Bit `cpu` is set once that CPU's collection has a MAPC.
static COLLECTIONS: AtomicU64 = AtomicU64::new(0);
/// RDbase passed to that CPU's MAPC. Read only after its `COLLECTIONS` bit.
static COLLECTION_RD: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];

/// Allocate ITS tables, the command queue, and enable the ITS.
///
/// # Safety
/// `its` is the mapped ITS; paging and the buddy are up.
unsafe fn init_its(g: &Gic, its: u64) {
    let Some(cmd) = alloc_pages(1) else {
        crate::klog!(Level::Error, "vibeOS: gic: its cmdq");
        return;
    };
    // Relaxed: command-queue PA; pairs with nothing.
    CMDQ.store(cmd, Ordering::Relaxed);
    // SAFETY: ITS tables and command queue, never freed. established here.
    unsafe {
        init_its_basers(its);
        // Valid + Inner Shareable + write-back. Size 0 = one 4 KiB page (IHI 0069G).
        mmio64w(
            its,
            GITS_CBASER,
            (cmd & ITS_PA_MASK) | (1u64 << 63) | (0b111u64 << 59) | (0b111u64 << 53) | (1u64 << 10),
        );
        mmio64w(its, GITS_CWRITER, 0);
        mmio32w(its, GITS_CTLR, GITS_CTLR_ENABLED);
    }
    // BSP collection. APs map theirs in `enable_ap`.
    map_collection(g, its, this_redist(g));
}

/// Program each implemented GITS_BASER (Devices and Collections).
///
/// # Safety
/// `its` is the mapped ITS; the buddy is up.
unsafe fn init_its_basers(its: u64) {
    let mut n = 0u64;
    while n < 8 {
        // SAFETY: GITS_BASER<n> is an ITS table register. established here.
        let b = unsafe { mmio64(its, GITS_BASER + n * 8) };
        let ty = (b >> 56) & 7;
        if ty == GITS_BASER_TYPE_DEVICE || ty == GITS_BASER_TYPE_COLLECTION {
            let Some(pa) = alloc_pages(1) else {
                n += 1;
                continue;
            };
            // Keep Type and Entry_Size (RO). 4 KiB pages, Size=0, Inner Shareable, Valid.
            let keep = b & ((0x7u64 << 56) | (0x1Fu64 << 48));
            let val = keep | (pa & ITS_PA_MASK) | (1u64 << 63) | (1u64 << 10);
            // SAFETY: as `init_its_basers`. established here.
            unsafe { mmio64w(its, GITS_BASER + n * 8, val) };
        }
        n += 1;
    }
}

/// One ITT page per DeviceID. EventIDs are per-device (ITS MSI data).
fn claim_its_dev(device_id: u32) -> Option<&'static ItsDev> {
    let mut i = 0usize;
    while i < ITS_DEV_MAX {
        let Some(d) = ITS_DEVS.get(i) else {
            break;
        };
        // Relaxed: pairs with nothing; DeviceID is written once.
        if d.id.load(Ordering::Relaxed) == device_id {
            return Some(d);
        }
        i = i.saturating_add(1);
    }
    let itt = alloc_pages(1)?;
    i = 0;
    while i < ITS_DEV_MAX {
        let Some(d) = ITS_DEVS.get(i) else {
            break;
        };
        // AcqRel: publishes DeviceID; Acquire failure pairs with another claimer's AcqRel.
        if d.id
            .compare_exchange(
                ITS_DEV_EMPTY,
                device_id,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            // Release: pairs with the Acquire ITT load in `map_its_event`.
            d.itt.store(itt, Ordering::Release);
            return Some(d);
        }
        // Relaxed: pairs with nothing; another CPU claimed this DeviceID.
        if d.id.load(Ordering::Relaxed) == device_id {
            return Some(d);
        }
        i = i.saturating_add(1);
    }
    None
}

/// Bind `hwirq` (an LPI) as EventID on `device_id`.
pub fn map_its_event(device_id: u32, hwirq: u32) {
    let Some(g) = gic() else {
        return;
    };
    let Some(its) = g.its else {
        return;
    };
    if !gic::is_lpi(hwirq) {
        return;
    }
    let Some(dev) = claim_its_dev(device_id) else {
        crate::klog!(Level::Error, "vibeOS: gic: its itt");
        return;
    };
    // Acquire: pairs with the Release store of `itt` in `claim_its_dev`.
    let itt = dev.itt.load(Ordering::Acquire);
    if itt == 0 {
        crate::klog!(Level::Error, "vibeOS: gic: its itt");
        return;
    }
    let Some(rd0) = collection_rd(0) else {
        crate::klog!(Level::Error, "vibeOS: gic: its no collection 0");
        return;
    };
    // The bump and the commands share `ITS_CMDS`: another CPU's MAPTI for
    // this device has to land after MAPD of event 0.
    let (ok, event) = {
        let _hold = ITS_CMDS.lock();
        // Relaxed: per-device EventID bump; pairs with nothing.
        let event = dev.next_event.fetch_add(1, Ordering::Relaxed);
        let mut ok = true;
        if event == 0
            && let Ok(mapd) = ItsCommand::mapd(device_id, itt, 3, true)
        {
            // SAFETY: `its` is the mapped ITS and `ITS_CMDS` is held.
            // established here.
            ok = unsafe { push_cmds(its, &[mapd]) };
        }
        ok = ok && {
            // SAFETY: as the MAPD push above. established here.
            unsafe {
                push_cmds(
                    its,
                    &[
                        ItsCommand::mapti(device_id, event, hwirq, 0),
                        ItsCommand::sync(rd0),
                    ],
                )
            }
        };
        (ok, event)
    };
    if !ok {
        crate::klog!(
            Level::Error,
            "vibeOS: gic: its map dev {device_id:#x} ev {event}"
        );
        return;
    }
    let slot = (hwirq - LPI_BASE) as usize;
    if let Some(e) = LPI_EVENT.get(slot) {
        // Release: pairs with the Acquire load in `compose_msi` and `movi_lpi`.
        e.store(event, Ordering::Release);
    }
    if let Some(c) = LPI_COL.get(slot) {
        // Release: pairs with the Acquire load in `discard_lpi`.
        c.store(0, Ordering::Release);
    }
    if let Some(d) = LPI_DEV.get(slot) {
        // Release: pairs with the Acquire load in `movi_lpi` and the AcqRel
        // swap in `discard_lpi`.
        d.store(device_id, Ordering::Release);
    }
}

/// MAPC this CPU's collection to `rd_va`, then SYNC. The BSP calls it from
/// `init_its`; each AP from `enable_ap`, after its redistributor has LPIs on.
fn map_collection(g: &Gic, its: u64, rd_va: u64) {
    let cpu = crate::per_cpu_init::current().cpu_id;
    if cpu >= 64 {
        crate::klog!(Level::Error, "vibeOS: gic: its mapc cpu {cpu}");
        return;
    }
    let rd = collection_rdbase(g, its, rd_va);
    if !submit_its(
        its,
        &[ItsCommand::mapc(cpu as u16, rd, true), ItsCommand::sync(rd)],
    ) {
        crate::klog!(Level::Error, "vibeOS: gic: its mapc cpu {cpu} rd {rd:#x}");
        return;
    }
    if let Some(slot) = COLLECTION_RD.get(cpu as usize) {
        // Relaxed: pairs with the Release or of `COLLECTIONS` below, which publishes it.
        slot.store(rd, Ordering::Relaxed);
    }
    // Release: pairs with the Acquire load in `collection_rd`.
    COLLECTIONS.fetch_or(1u64 << cpu, Ordering::Release);
}

/// RDbase for MAPC/SYNC (IHI 0069G): processor number, or PA[51:16] when PTA=1.
fn collection_rdbase(g: &Gic, its: u64, rd_va: u64) -> u64 {
    // SAFETY: GITS_TYPER and this redistributor's GICR_TYPER. established here.
    unsafe {
        let typer = mmio64(its, GITS_TYPER);
        if typer & GITS_TYPER_PTA != 0 {
            let off = rd_va.wrapping_sub(g.cpu_or_redist);
            let pa = g.redist_pa.wrapping_add(off);
            return (pa >> 16) & ((1u64 << 35) - 1);
        }
        let rd_typer = mmio64(rd_va, GICR_TYPER);
        (rd_typer >> 8) & 0xFFFF
    }
}

/// RDbase of a collection `map_collection` published, if it has.
fn collection_rd(cpu: u32) -> Option<u64> {
    if cpu >= 64 {
        return None;
    }
    // Acquire: pairs with the Release or in `map_collection`.
    if COLLECTIONS.load(Ordering::Acquire) & (1u64 << cpu) == 0 {
        return None;
    }
    // Relaxed: pairs with the Release or of `COLLECTIONS` in `map_collection`.
    COLLECTION_RD
        .get(cpu as usize)
        .map(|s| s.load(Ordering::Relaxed))
}

/// MOVI an already-mapped LPI onto `cpu`'s collection. An LPI with no
/// DeviceID yet can only stay on CPU 0, which is where MAPTI puts it.
fn movi_lpi(its: u64, hwirq: u32, cpu: u32) -> Result<(), IrqError> {
    let Some(rd) = collection_rd(cpu) else {
        return Err(IrqError::BadCpu);
    };
    let slot = (hwirq - LPI_BASE) as usize;
    // Acquire: pairs with the Release store in `map_its_event`.
    let dev = LPI_DEV
        .get(slot)
        .map(|d| d.load(Ordering::Acquire))
        .unwrap_or(LPI_NO_DEV);
    if dev == LPI_NO_DEV {
        if cpu != 0 {
            return Err(IrqError::BadCpu);
        }
        return Ok(());
    }
    // Acquire: pairs with the Release store in `map_its_event`.
    let event = LPI_EVENT
        .get(slot)
        .map(|e| e.load(Ordering::Acquire))
        .unwrap_or(0);
    let icid = u16::try_from(cpu).map_err(|_| IrqError::BadCpu)?;
    if !submit_its(
        its,
        &[ItsCommand::movi(dev, event, icid), ItsCommand::sync(rd)],
    ) {
        return Err(IrqError::NoRoute);
    }
    if let Some(c) = LPI_COL.get(slot) {
        // Release: pairs with the Acquire load in `discard_lpi`.
        c.store(cpu, Ordering::Release);
    }
    Ok(())
}

/// DISCARD one mapped event. The device's ITT stays: another LPI may still
/// use it. Nothing was mapped when `LPI_DEV` is empty.
fn discard_lpi(hwirq: u32) {
    let Some(g) = gic() else {
        return;
    };
    let Some(its) = g.its else {
        return;
    };
    if !gic::is_lpi(hwirq) {
        return;
    }
    let slot = (hwirq - LPI_BASE) as usize;
    let Some(dev_slot) = LPI_DEV.get(slot) else {
        return;
    };
    // AcqRel: the swapped-out DeviceID pairs with the Release store in
    // `map_its_event`; the store of `LPI_NO_DEV` pairs with a later Acquire.
    let dev = dev_slot.swap(LPI_NO_DEV, Ordering::AcqRel);
    if dev == LPI_NO_DEV {
        return;
    }
    // Acquire: pairs with the Release stores in `map_its_event` and `movi_lpi`.
    let event = LPI_EVENT
        .get(slot)
        .map(|e| e.load(Ordering::Acquire))
        .unwrap_or(0);
    // Acquire: pairs with the Release stores in `map_its_event` and `movi_lpi`.
    let col = LPI_COL
        .get(slot)
        .map(|c| c.load(Ordering::Acquire))
        .unwrap_or(0);
    let rd = collection_rd(col).unwrap_or(0);
    if !submit_its(
        its,
        &[ItsCommand::discard(dev, event), ItsCommand::sync(rd)],
    ) {
        crate::klog!(
            Level::Error,
            "vibeOS: gic: its discard dev {dev:#x} ev {event}"
        );
    }
}

/// Push `cmds` in order. False when the ITS stalls or does not catch up.
fn submit_its(its: u64, cmds: &[ItsCommand]) -> bool {
    let _hold = ITS_CMDS.lock();
    // SAFETY: `its` is the mapped ITS; `ITS_CMDS` is held, so this is
    // the only writer of the command queue. established here.
    unsafe { push_cmds(its, cmds) }
}

/// Push `cmds` with [`ITS_CMDS`] already held.
///
/// # Safety
/// `its` is the mapped ITS and the caller holds [`ITS_CMDS`].
unsafe fn push_cmds(its: u64, cmds: &[ItsCommand]) -> bool {
    for c in cmds {
        // SAFETY: this fn's `# Safety`. established here.
        if unsafe { !its_push(its, *c) } {
            return false;
        }
    }
    true
}

/// Push one ITS command and wait for CREADR.
///
/// # Safety
/// `its` is the mapped ITS, the command queue page is live, and the caller
/// holds [`ITS_CMDS`].
unsafe fn its_push(its: u64, cmd: ItsCommand) -> bool {
    // SAFETY: this fn's `# Safety`. established here.
    unsafe {
        let rd0 = mmio64(its, GITS_CREADR);
        if rd0 & GITS_CREADR_STALLED != 0 {
            crate::klog!(Level::Error, "vibeOS: gic: its stalled rd={rd0:#x}");
            return false;
        }
        let wr = mmio64(its, GITS_CWRITER) & !GITS_CREADR_STALLED;
        // Relaxed: pairs with nothing.
        let base = CMDQ.load(Ordering::Relaxed);
        let va = crate::paging_init::hhdm_offset()
            .wrapping_add(base)
            .wrapping_add(wr);
        core::ptr::copy_nonoverlapping(cmd.dw.as_ptr(), va as *mut u64, 4);
        let next = (wr + 32) & 0xFFF;
        core::arch::asm!("dsb oshst", options(nostack, preserves_flags));
        mmio64w(its, GITS_CWRITER, next);
        let mut n = 100_000u32;
        while n > 0 {
            let rd = mmio64(its, GITS_CREADR);
            if rd & GITS_CREADR_STALLED != 0 {
                crate::klog!(Level::Error, "vibeOS: gic: its stalled rd={rd:#x}");
                return false;
            }
            if (rd & !GITS_CREADR_STALLED) == next {
                return true;
            }
            n -= 1;
            core::hint::spin_loop();
        }
        crate::klog!(
            Level::Error,
            "vibeOS: gic: its creadr timeout wr={next:#x} rd={:#x}",
            mmio64(its, GITS_CREADR)
        );
        false
    }
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

/// Aff0 of `hw_id` can be named in `ICC_SGI1R_EL1`. GICv2 does not use
/// that register. No chip yet is not a refusal.
pub fn cpu_ok(hw_id: u64) -> Result<(), gic::SgiError> {
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

/// Per-CPU redistributor / CPU interface on a secondary.
///
/// # Safety
/// This CPU's GIC interface is unused; IRQs masked.
pub unsafe fn enable_ap() {
    let Some(g) = gic() else {
        return;
    };
    // SAFETY: this fn's `# Safety`; established here.
    unsafe {
        match g.kind {
            Kind::V3 => {
                let rd = this_redist(g);
                wake_redist(rd);
                program_sgi_ppi(rd);
                enable_lpi(g, rd);
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
        // SAFETY: this CPU's redistributor SGI/PPI enable. established here.
        unsafe { mmio32w(this_redist(g), o, bit) };
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
        if g.kind == Kind::V3 && g.its.is_some() {
            let mut i = 0;
            while i < want {
                // Relaxed: LPI bump; pairs with nothing.
                let slot = g.next_lpi.fetch_add(1, Ordering::Relaxed);
                if slot as usize >= LPI_SLOTS {
                    return Err(IrqError::Exhausted);
                }
                if let Some(s) = out.get_mut(i) {
                    *s = LPI_BASE + slot;
                }
                i += 1;
            }
            return Ok(want);
        }
        if let Some((_, base, count)) = g.v2m {
            let mut i = 0;
            while i < want {
                // Relaxed: v2m bump; pairs with nothing.
                let slot = g.next_v2m.fetch_add(1, Ordering::Relaxed);
                if slot >= u32::from(count) {
                    return Err(IrqError::Exhausted);
                }
                if let Some(s) = out.get_mut(i) {
                    *s = base + slot;
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
