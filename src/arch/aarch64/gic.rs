//! GICv2 / GICv3 + ITS / GICv2m as `IrqChip`s (DESIGN §5.4, §11.5).

use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use vibeos::irq::{
    ITS_FREE_WAIT_NS, IrqChip, IrqError, IrqSpecifier, ItsCommand, encode_free_sequence, gic,
};
use vibeos::log::Level;
use vibeos::machine::PhysRange;
use vibeos::paging::PhysAddr;

use crate::cell::BootCell;
use crate::machine_init;
use crate::paging_init;

const GICD_CTLR: u64 = 0x0000;
const GICD_TYPER: u64 = 0x0004;
const GICD_ISENABLER: u64 = 0x0100;
const GICD_ICENABLER: u64 = 0x0180;
const GICD_IPRIORITYR: u64 = 0x0400;
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
const GICD_ITARGETSR: u64 = 0x0800;
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
const GICD_ICFGR: u64 = 0x0C00;
const GICD_IROUTER: u64 = 0x6100;
const GICD_SGIR: u64 = 0x0F00;
const GICD_CTLR_ENABLE_G1: u32 = 1;
const GICD_CTLR_ARE_NS: u32 = 1 << 4;
const GICD_CTLR_ENABLE_G1A: u32 = 1 << 1;

const GICC_CTLR: u64 = 0x0000;
const GICC_PMR: u64 = 0x0004;
const GICC_BPR: u64 = 0x0008;
const GICC_IAR: u64 = 0x000C;
const GICC_EOIR: u64 = 0x0010;

const GICR_CTLR: u64 = 0x0000;
const GICR_WAKER: u64 = 0x0014;
const GICR_WAKER_PS: u32 = 1 << 1;
const GICR_WAKER_CA: u32 = 1 << 2;
const GICR_CTLR_ENABLE_LPIS: u32 = 1;
const GICR_SGI_BASE: u64 = 0x1_0000;
const GICR_ISENABLER0: u64 = GICR_SGI_BASE + 0x0100;
const GICR_ICENABLER0: u64 = GICR_SGI_BASE + 0x0180;
const GICR_IPRIORITYR: u64 = GICR_SGI_BASE + 0x0400;
const GICR_PROPBASER: u64 = 0x0070;
const GICR_PENDBASER: u64 = 0x0078;

const GITS_CTLR: u64 = 0x0000;
const GITS_CBASER: u64 = 0x0080;
const GITS_CWRITER: u64 = 0x0088;
const GITS_CREADR: u64 = 0x0090;
const GITS_BASER: u64 = 0x0100;
const GITS_CTLR_ENABLED: u32 = 1;
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
    dist: u64,
    cpu_or_redist: u64,
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
            size: cpu_or_redist.size.max(0x2_0000),
        },
        Kind::V2 => cpu_or_redist,
    };
    let cpu_va = match map_mmio(cpu_or_redist) {
        Some(v) => v,
        None => crate::boot::halt_with("vibeOS: gic: cpu/redist map"),
    };
    let its = desc.gic_its().and_then(|(r, _)| map_mmio(r));
    let v2m = desc
        .gic_v2m()
        .next()
        .and_then(|(r, base, n)| map_mmio(r).map(|va| (va, base, n)));
    let g = Gic {
        kind,
        dist: dist_va,
        cpu_or_redist: cpu_va,
        its,
        v2m,
        spi_irqs: [const { AtomicU32::new(0) }; SPI_SLOTS],
        lpi_irqs: [const { AtomicU32::new(0) }; LPI_SLOTS],
        next_lpi: AtomicU32::new(0),
        next_v2m: AtomicU32::new(0),
    };
    match kind {
        Kind::V3 => init_v3(&g),
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
            if i >= 32 {
                mmio32w(g.dist, GICD_ICENABLER + u64::from(i / 32) * 4, 0xFFFF_FFFF);
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
        // SGIs and PPIs: enable 0..31.
        mmio32w(g.dist, GICD_ISENABLER, 0xFFFF_FFFF);
        mmio32w(g.dist, GICD_CTLR, GICD_CTLR_ENABLE_G1);
        mmio32w(g.cpu_or_redist, GICC_PMR, 0xFF);
        mmio32w(g.cpu_or_redist, GICC_BPR, 0);
        mmio32w(g.cpu_or_redist, GICC_CTLR, 1);
    }
}

fn init_v3(g: &Gic) {
    // SAFETY: mapped GICD/GICR; system registers are the CPU interface. established here.
    unsafe {
        wake_redist(g.cpu_or_redist);
        let typer = mmio32(g.dist, GICD_TYPER);
        let lines = ((typer & 0x1F) + 1) * 32;
        mmio32w(g.dist, GICD_CTLR, 0);
        let mut i = 32u32;
        while i < lines {
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
                    mmio64w(g.dist, GICD_IROUTER + u64::from(n) * 8, 0);
                }
                j = j.saturating_add(1);
            }
            i = i.saturating_add(4);
        }
        mmio32w(g.dist, GICD_CTLR, GICD_CTLR_ENABLE_G1A | GICD_CTLR_ARE_NS);
        // SGI/PPI on the redistributor.
        mmio32w(g.cpu_or_redist, GICR_ICENABLER0, 0);
        let mut s = 0u32;
        while s < 32 {
            let p = gic::priority_for(s);
            mmio32w(
                g.cpu_or_redist,
                GICR_IPRIORITYR + u64::from(s),
                u32::from(p) | u32::from(p) << 8 | u32::from(p) << 16 | u32::from(p) << 24,
            );
            s = s.saturating_add(4);
        }
        mmio32w(g.cpu_or_redist, GICR_ISENABLER0, 0xFFFF_FFFF);
        enable_lpi(g);
        icc_enable();
        if let Some(its) = g.its {
            init_its(its);
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

/// Program redistributor LPI tables once; never clear EnableLPIs.
///
/// # Safety
/// `g` is the live GIC; paging and the buddy are up.
unsafe fn enable_lpi(g: &Gic) {
    // Property table: 64K bytes (IDbits 16), pending: 64K bits / 8.
    let Some(prop) = alloc_pages(16) else {
        crate::klog!(Level::Error, "vibeOS: gic: lpi prop table");
        return;
    };
    let Some(pend) = alloc_pages(8) else {
        crate::klog!(Level::Error, "vibeOS: gic: lpi pend table");
        return;
    };
    let va = crate::paging_init::hhdm_offset().wrapping_add(prop);
    // Enable the LPIs this chip will allocate (DESIGN §5.4).
    // SAFETY: `va` is the HHDM alias of the property table (I14). established here.
    unsafe {
        let mut i = 0u32;
        while i < LPI_SLOTS as u32 {
            let off = (LPI_BASE + i) as usize;
            let p = (va as *mut u8).wrapping_add(off);
            p.write(1 | (gic::PRIO_DEVICE & 0xFC));
            i = i.saturating_add(1);
        }
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
        let idbits = 15u64;
        mmio64w(
            g.cpu_or_redist,
            GICR_PROPBASER,
            prop | idbits | (1 << 10) | (0b11 << 8) | (0b01 << 7),
        );
        mmio64w(
            g.cpu_or_redist,
            GICR_PENDBASER,
            pend | (1 << 10) | (0b11 << 8) | (1 << 62),
        );
        let c = mmio32(g.cpu_or_redist, GICR_CTLR) | GICR_CTLR_ENABLE_LPIS;
        mmio32w(g.cpu_or_redist, GICR_CTLR, c);
        mmio64w(g.cpu_or_redist, GICR_INVALLR, 0);
        let mut n = 100_000u32;
        while n > 0 && mmio32(g.cpu_or_redist, GICR_SYNCR) & 1 != 0 {
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

static ITT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static CMDQ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Allocate ITS tables, the command queue, and enable the ITS.
///
/// # Safety
/// `its` is the mapped ITS; paging and the buddy are up.
unsafe fn init_its(its: u64) {
    let Some(cmd) = alloc_pages(1) else {
        crate::klog!(Level::Error, "vibeOS: gic: its cmdq");
        return;
    };
    if let Some(itt) = alloc_pages(1) {
        // Relaxed: ITT physical address, published once on the BSP.
        // Relaxed: pairs with nothing.
        ITT.store(itt, Ordering::Relaxed);
    } else {
        crate::klog!(Level::Error, "vibeOS: gic: its itt");
    }
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
        its_cmd(its, ItsCommand::mapc(0, 0, true));
        its_cmd(its, ItsCommand::sync(0));
    }
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
    // Relaxed: pairs with nothing.
    let itt = ITT.load(Ordering::Relaxed);
    if itt == 0 {
        return;
    }
    let event = hwirq - LPI_BASE;
    // SAFETY: ITS command queue the boot CPU owns. established here.
    unsafe {
        if let Ok(mapd) = ItsCommand::mapd(device_id, itt, 7, true) {
            its_cmd(its, mapd);
        }
        its_cmd(its, ItsCommand::mapti(device_id, event, hwirq, 0));
        its_cmd(its, ItsCommand::sync(0));
    }
}

/// Push one ITS command and wait for CREADR.
///
/// # Safety
/// `its` is the mapped ITS; the command queue page is live.
unsafe fn its_cmd(its: u64, cmd: ItsCommand) {
    // SAFETY: ITS command queue the boot CPU owns. established here.
    unsafe {
        let wr = mmio64(its, GITS_CWRITER);
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
            if mmio64(its, GITS_CREADR) == next {
                break;
            }
            n -= 1;
            core::hint::spin_loop();
        }
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
    let intid = ack(g);
    if intid >= 1020 {
        return;
    }
    // DESIGN §5.8: EOI before the timer/IPI body, which may preempt.
    eoi(g, intid);
    // Acquire: pairs with the Release store in `set_dispatch`.
    let p = DISPATCH.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: a non-null hook holds a `fn(u32)`; established by
        // `crate::arch::aarch64::gic::set_dispatch`, its only store.
        let f = unsafe { core::mem::transmute::<*mut (), fn(u32)>(p) };
        f(intid);
    }
}

fn ack(g: &Gic) -> u32 {
    match g.kind {
        Kind::V2 => {
            // SAFETY: GICC_IAR. established here.
            unsafe { mmio32(g.cpu_or_redist, GICC_IAR) & 0x3FF }
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
            v as u32
        }
    }
}

fn eoi(g: &Gic, intid: u32) {
    match g.kind {
        Kind::V2 => {
            // SAFETY: GICC_EOIR. established here.
            unsafe { mmio32w(g.cpu_or_redist, GICC_EOIR, intid) };
        }
        Kind::V3 => {
            // SAFETY: ICC_EOIR1_EL1. established here.
            unsafe {
                core::arch::asm!(
                    "msr ICC_EOIR1_EL1, {0}",
                    in(reg) u64::from(intid),
                    options(nostack, preserves_flags)
                );
            }
        }
    }
}

pub fn send_sgi(intid: u32) {
    let Some(g) = gic() else {
        return;
    };
    match g.kind {
        Kind::V3 => {
            let aff = ((u64::from(intid) & 0xF) << 24) | 1;
            // SAFETY: DESIGN §7.6: dsb ishst; ICC_SGI1R; isb. established here.
            unsafe {
                core::arch::asm!(
                    "dsb ishst",
                    "msr ICC_SGI1R_EL1, {0}",
                    "isb",
                    in(reg) aff,
                    options(nostack, preserves_flags),
                );
            }
        }
        Kind::V2 => {
            // TargetListFilter 0b10 = this CPU (IHI 0048).
            let sgir = (intid & 0xF) | (2 << 24);
            // SAFETY: ordered mmio_write (DESIGN §4.7). established here.
            unsafe {
                core::arch::asm!("dsb oshst", options(nostack, preserves_flags));
                mmio32w(g.dist, GICD_SGIR, sgir);
            }
        }
    }
}

fn enable_intid(g: &Gic, intid: u32, on: bool) {
    let bit = 1u32 << (intid % 32);
    let off = if on { GICD_ISENABLER } else { GICD_ICENABLER };
    if g.kind == Kind::V3 && intid < 32 {
        let o = if on { GICR_ISENABLER0 } else { GICR_ICENABLER0 };
        // SAFETY: redistributor SGI/PPI enable. established here.
        unsafe { mmio32w(g.cpu_or_redist, o, bit) };
        return;
    }
    // SAFETY: distributor enable bit. established here.
    unsafe { mmio32w(g.dist, off + u64::from(intid / 32) * 4, bit) };
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
        if cpu != 0 {
            return Err(IrqError::BadCpu);
        }
        let Some(g) = gic() else {
            return Err(IrqError::NoRoute);
        };
        if g.kind == Kind::V3 && gic::is_lpi(hwirq) {
            // Boot CPU: collection 0. MOVI is a no-op to the same collection.
            return Ok(());
        }
        if g.kind == Kind::V3 && gic::is_spi(hwirq) {
            // SAFETY: GICD_IROUTER for this SPI. established here.
            unsafe { mmio64w(g.dist, GICD_IROUTER + u64::from(hwirq) * 8, 0) };
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
            let event = hwirq - LPI_BASE;
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
        if let Some(g) = gic()
            && gic::is_lpi(hwirq)
            && g.its.is_some()
        {
            let mut cmds = [ItsCommand { dw: [0; 4] }; 4];
            if encode_free_sequence(0, &[hwirq - LPI_BASE], 0, &mut cmds).is_ok()
                && let Some(its) = g.its
            {
                for c in cmds {
                    if c.dw[0] != 0 {
                        // SAFETY: ITS command queue. established here.
                        unsafe { its_cmd(its, c) };
                    }
                }
                let hz = crate::arch::aarch64::timer::hz();
                if hz != 0 {
                    let ticks = ITS_FREE_WAIT_NS.saturating_mul(hz) / 1_000_000_000;
                    let t0 = crate::arch::aarch64::cpu::cntvct();
                    while crate::arch::aarch64::cpu::cntvct().wrapping_sub(t0) < ticks {
                        core::hint::spin_loop();
                    }
                }
            }
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
