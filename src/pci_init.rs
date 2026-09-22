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

fn map_mmio(phys: u64, len: u64, keep_wb: bool) -> Option<u64> {
    if phys == 0 || len == 0 {
        return None;
    }
    let end = phys.checked_add(len)?;
    // VGA BAR0 aliases the Limine FB. It stays WB on the physmap.
    // UC-patch or ioremap would alias the console UC (DESIGN §4.1 / §9.2).
    // Limine's surface (and thus map_end) is often smaller than the BAR
    // (16 MiB); fill missing physmap leaves as WB so BAR0 is mapped.
    if keep_wb || fb_init::overlaps_phys(phys, len) {
        if paging_init::ensure_physmap_wb(PhysAddr(phys), len) || phys < paging_init::map_end() {
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

fn ecam_covers(bus: u8) -> bool {
    let e = ecam();
    e.base != 0 && bus >= e.start && bus <= e.end
}

fn ecam_phys_of(bdf: Bdf, offset: u16) -> Option<u64> {
    let e = ecam();
    pci::ecam_phys(
        e.base,
        e.start,
        e.end,
        bdf.bus,
        bdf.device,
        bdf.function,
        offset,
    )
}

/// Map the 4K function page for `phys` and return the byte VA.
/// Cache miss still ioremaps; the returned VA is always used, even when
/// the table is full (a cache-only lookup would vanish later buses).
fn ecam_va(phys: u64) -> Option<u64> {
    let page = phys & !(PAGE_SIZE_4K - 1);
    let e = ecam();
    let mut i = 0usize;
    while i < e.n {
        if e.phys[i] == page {
            return Some(pci::ecam_byte_va(e.va[i], phys));
        }
        i += 1;
    }
    let va = map_mmio(page, PAGE_SIZE_4K, false)?;
    let slot = pci::ecam_cache_slot(e.n, ECAM_CACHE);
    if e.n < ECAM_CACHE {
        e.n += 1;
    }
    e.phys[slot] = page;
    e.va[slot] = va;
    Some(pci::ecam_byte_va(va, phys))
}

fn ecam_read_at(va: u64) -> u32 {
    unsafe { (va as *const u32).read_volatile() }
}

fn ecam_write_at(va: u64, val: u32) {
    unsafe { (va as *mut u32).write_volatile(val) }
}

pub struct HwCfg;

impl CfgIo for HwCfg {
    fn read32(&mut self, bdf: Bdf, offset: u16) -> u32 {
        if ecam_covers(bdf.bus) {
            let Some(phys) = ecam_phys_of(bdf, offset) else {
                return 0xFFFF_FFFF;
            };
            // Map without CFG held (ioremap takes PT).
            let Some(va) = ecam_va(phys) else {
                return 0xFFFF_FFFF;
            };
            return with_cfg(|| ecam_read_at(va));
        }
        if bdf.bus == 0 {
            return with_cfg(|| cf8_read32(bdf, offset));
        }
        0xFFFF_FFFF
    }

    fn write32(&mut self, bdf: Bdf, offset: u16, value: u32) {
        if ecam_covers(bdf.bus) {
            let Some(phys) = ecam_phys_of(bdf, offset) else {
                return;
            };
            let Some(va) = ecam_va(phys) else {
                return;
            };
            with_cfg(|| ecam_write_at(va, value));
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
            crate::marker!("vibeOS: pci: skip bar {} size {:#x}", info.bdf, bar.size);
            i += 1;
            continue;
        }
        // Class 03.00 BAR0 is the scanout aperture. Keep it WB even when
        // Limine's FB record does not overlap the full BAR.
        let keep_wb = i == 0 && info.class == 0x03 && info.subclass == 0x00;
        if let Some(va) = map_mmio(bar.addr, bar.size, keep_wb) {
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
        crate::marker!(
            "vibeOS: pci: ecam {:#x} buses {}-{}",
            m.ecam_base,
            m.start_bus,
            m.end_bus
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
    crate::marker!("vibeOS: pci: {} devices", n);
    LIVE.store(true, Ordering::Release);
}

pub fn enable_mem_master(bdf: Bdf) {
    let mut hw = HwCfg;
    let cmd = pci::read16(&mut hw, bdf, CFG_COMMAND);
    pci::write_command(&mut hw, bdf, pci::enable_mem_master(cmd));
}

pub fn cfg_read16(bdf: Bdf, offset: u16) -> u16 {
    pci::read16(&mut HwCfg, bdf, offset)
}

pub fn cfg_write_command(bdf: Bdf, cmd: u16) {
    pci::write_command(&mut HwCfg, bdf, cmd)
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
