//! PCI config (CF8 + ECAM) and bus scan. ROADMAP §6.2.
//!
//! Legacy `0xCF8`/`0xCFC` for bus 0 when there is no MCFG. Beyond bus 0,
//! MCFG → ECAM (DESIGN §7.1). Scan fills the device list; drivers bind
//! later. Memory BARs are mapped through ioremap or a capped physmap —
//! never a multi-TiB page walk (DESIGN §4.1).

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::dev::Device;
use vibeos::paging::{PAGE_SIZE_4K, PhysAddr};
use vibeos::pci::{self, Bdf, CFG_COMMAND, CfgIo, FuncInfo, MAX_SCAN, bar_map_allowed};

use crate::acpi_init;
use crate::dev_init;
use crate::fb_init;
use crate::paging_init;
use crate::serial::Serial;
use crate::x86::{self, InterruptGuard};

const CFG_ADDR: u16 = 0xCF8;
const CFG_DATA: u16 = 0xCFC;

const ECAM_CACHE: usize = 64;

struct Ecam {
    base: u64,
    start: u8,
    end: u8,
    phys: [u64; ECAM_CACHE],
    va: [u64; ECAM_CACHE],
    n: usize,
}

impl Ecam {
    const fn empty() -> Self {
        Self {
            base: 0,
            start: 0,
            end: 0,
            phys: [0; ECAM_CACHE],
            va: [0; ECAM_CACHE],
            n: 0,
        }
    }
}

struct Cell<T>(core::cell::UnsafeCell<T>);
unsafe impl<T> Sync for Cell<T> {}

static ECAM: Cell<Ecam> = Cell(core::cell::UnsafeCell::new(Ecam::empty()));
static CFG_LOCK: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicBool = AtomicBool::new(false);

fn ecam() -> &'static mut Ecam {
    unsafe { &mut *ECAM.0.get() }
}

fn with_cfg<R>(f: impl FnOnce() -> R) -> R {
    let _irq = InterruptGuard::enter();
    while CFG_LOCK
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    let r = f();
    CFG_LOCK.store(false, Ordering::Release);
    r
}

fn cf8_read32(bdf: Bdf, offset: u16) -> u32 {
    let addr = pci::cf8_addr(bdf.bus, bdf.device, bdf.function, offset);
    unsafe {
        x86::outl(CFG_ADDR, addr);
        x86::inl(CFG_DATA)
    }
}

fn cf8_write32(bdf: Bdf, offset: u16, val: u32) {
    let addr = pci::cf8_addr(bdf.bus, bdf.device, bdf.function, offset);
    unsafe {
        x86::outl(CFG_ADDR, addr);
        x86::outl(CFG_DATA, val);
    }
}

fn map_mmio(phys: u64, len: u64) -> Option<u64> {
    if phys == 0 || len == 0 {
        return None;
    }
    let end = phys.checked_add(len)?;
    // VGA BAR0 is the Limine FB. It is already WB on the physmap.
    // UC-patching it would alias the console (DESIGN §4.1 / §9.2).
    if fb_init::overlaps_phys(phys, len) {
        if end <= paging_init::map_end() {
            return Some(paging_init::HHDM_BASE.wrapping_add(phys));
        }
        return None;
    }
    if end <= paging_init::map_end() {
        let _ = unsafe { paging_init::patch_physmap_uc(PhysAddr(phys), len) };
        Some(paging_init::HHDM_BASE.wrapping_add(phys))
    } else {
        unsafe { paging_init::ioremap(PhysAddr(phys), len) }.map(|v| v.as_u64())
    }
}

fn ecam_cached_va(phys: u64) -> Option<u64> {
    let page = phys & !(PAGE_SIZE_4K - 1);
    let e = ecam();
    let mut i = 0usize;
    while i < e.n {
        if e.phys[i] == page {
            return Some(e.va[i].wrapping_add(phys & (PAGE_SIZE_4K - 1)));
        }
        i += 1;
    }
    None
}

fn ecam_page(phys: u64) -> Option<u64> {
    let page = phys & !(PAGE_SIZE_4K - 1);
    let e = ecam();
    let mut i = 0usize;
    while i < e.n {
        if e.phys[i] == page {
            return Some(e.va[i]);
        }
        i += 1;
    }
    let va = map_mmio(page, PAGE_SIZE_4K)?;
    if e.n < ECAM_CACHE {
        e.phys[e.n] = page;
        e.va[e.n] = va;
        e.n += 1;
    }
    Some(va)
}

fn ecam_read32(bdf: Bdf, offset: u16) -> Option<u32> {
    let e = ecam();
    if e.base == 0 {
        return None;
    }
    let phys = pci::ecam_phys(
        e.base,
        e.start,
        e.end,
        bdf.bus,
        bdf.device,
        bdf.function,
        offset,
    )?;
    // Caller mapped the page first. Do not ioremap while CFG is held.
    let va = ecam_cached_va(phys)?;
    Some(unsafe { (va as *const u32).read_volatile() })
}

fn ecam_write32(bdf: Bdf, offset: u16, val: u32) -> bool {
    let e = ecam();
    if e.base == 0 {
        return false;
    }
    let Some(phys) = pci::ecam_phys(
        e.base,
        e.start,
        e.end,
        bdf.bus,
        bdf.device,
        bdf.function,
        offset,
    ) else {
        return false;
    };
    let Some(va) = ecam_cached_va(phys) else {
        return false;
    };
    unsafe { (va as *mut u32).write_volatile(val) };
    true
}

struct HwCfg;

impl CfgIo for HwCfg {
    fn read32(&mut self, bdf: Bdf, offset: u16) -> u32 {
        let e = ecam();
        if e.base != 0 && bdf.bus >= e.start && bdf.bus <= e.end {
            // Map the page without CFG held (ioremap takes PT).
            let phys = pci::ecam_phys(
                e.base,
                e.start,
                e.end,
                bdf.bus,
                bdf.device,
                bdf.function,
                offset,
            );
            if let Some(p) = phys {
                let _ = ecam_page(p);
            }
            return with_cfg(|| ecam_read32(bdf, offset).unwrap_or(0xFFFF_FFFF));
        }
        if bdf.bus == 0 {
            return with_cfg(|| cf8_read32(bdf, offset));
        }
        0xFFFF_FFFF
    }

    fn write32(&mut self, bdf: Bdf, offset: u16, value: u32) {
        let e = ecam();
        if e.base != 0 && bdf.bus >= e.start && bdf.bus <= e.end {
            let phys = pci::ecam_phys(
                e.base,
                e.start,
                e.end,
                bdf.bus,
                bdf.device,
                bdf.function,
                offset,
            );
            if let Some(p) = phys {
                let _ = ecam_page(p);
            }
            with_cfg(|| {
                let _ = ecam_write32(bdf, offset, value);
            });
            return;
        }
        if bdf.bus == 0 {
            with_cfg(|| cf8_write32(bdf, offset, value));
        }
    }
}

fn map_func_bars(info: &FuncInfo, dev: &mut Device) {
    let mut i = 0usize;
    while i < pci::MAX_BARS {
        let bar = info.bars[i];
        if !bar.kind.is_mem() || bar.addr == 0 || bar.size == 0 {
            i += 1;
            continue;
        }
        if !bar_map_allowed(bar.size) {
            let _ = writeln!(
                Serial,
                "vibeOS: pci: skip bar {} size {:#x}",
                info.bdf, bar.size
            );
            i += 1;
            continue;
        }
        if let Some(va) = map_mmio(bar.addr, bar.size) {
            dev.resources[i].mapped_va = va;
        }
        i += 1;
    }
}

/// Scan, map memory BARs, publish devices, emit `pci: N devices`.
pub fn init() {
    if let Some(m) = acpi_init::info().and_then(|i| i.mcfg) {
        let e = ecam();
        e.base = m.ecam_base;
        e.start = m.start_bus;
        e.end = m.end_bus;
        let _ = writeln!(
            Serial,
            "vibeOS: pci: ecam {:#x} buses {}-{}",
            m.ecam_base, m.start_bus, m.end_bus
        );
    }

    let mut found = [FuncInfo::empty(); MAX_SCAN];
    let n = pci::enumerate(&mut HwCfg, 0, &mut found);
    let mut i = 0usize;
    while i < n {
        let info = found[i];
        let mut dev = Device::from_func(info);
        map_func_bars(&info, &mut dev);
        let _ = write!(Serial, "vibeOS: pci: ");
        let _ = pci::write_lspci_line(&mut Serial, &info);
        let _ = writeln!(Serial);
        let _ = dev_init::push(dev);
        i += 1;
    }
    let _ = writeln!(Serial, "vibeOS: pci: {} devices", n);
    LIVE.store(true, Ordering::Release);
}

pub fn enable_mem_master(bdf: Bdf) {
    let mut hw = HwCfg;
    let cmd = pci::read16(&mut hw, bdf, CFG_COMMAND);
    pci::write_command(&mut hw, bdf, pci::enable_mem_master(cmd));
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn cfg_read32(bdf: Bdf, offset: u16) -> u32 {
    HwCfg.read32(bdf, offset)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn cfg_write32(bdf: Bdf, offset: u16, value: u32) {
    HwCfg.write32(bdf, offset, value)
}
