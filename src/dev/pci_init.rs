//! PCI config (CF8 + ECAM) and bus scan. ROADMAP §6.2.
//!
//! Legacy `0xCF8`/`0xCFC` for bus 0 when there is no MCFG. Beyond bus 0,
//! MCFG → ECAM (DESIGN §7.1). Scan fills the device list, with each
//! function's parent bridge, and maps no BAR. A driver maps a memory BAR
//! it has claimed, in its `probe`, through [`map_bar`] (DESIGN §12.3 rule
//! 8): through ioremap or the capped physmap, never a multi-TiB page walk
//! (DESIGN §4.1).

use core::fmt::Write;
#[cfg(feature = "kernel_tests")]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::dev::{self, BarClaim, DevRef, Device};
use vibeos::ipi::{SHOOT_RANGE_PAGES, SHOOT_RANGES, ShootRange};
use vibeos::kalloc::AllocError;
use vibeos::lock::RANK_DEVICE;
use vibeos::paging::{IOREMAP_BASE, IOREMAP_LEN, PAGE_SIZE_4K, PhysAddr, VirtAddr};
use vibeos::pci::{self, Bdf, CFG_COMMAND, CfgIo, FuncInfo, MAX_SCAN, bar_map_allowed};

use crate::arch::current::InterruptGuard;
use crate::boot;
use crate::cell::BootCell;
use crate::fb_init;
use crate::machine_init;
use crate::paging_init;
use crate::sync_init::SpinMutex;
#[cfg(target_arch = "x86_64")]
use crate::x86;

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

static ECAM: SpinMutex<Ecam> = SpinMutex::with_rank(Ecam::empty(), RANK_DEVICE);

/// Run `f` on the ECAM window.
fn with_ecam<R>(f: impl FnOnce(&mut Ecam) -> R) -> R {
    let mut g = ECAM.lock();
    f(&mut g)
}
static CFG_LOCK: AtomicBool = AtomicBool::new(false);
/// Set once the scan has run; `dev::ktest::pci_live` reads it.
pub(super) static LIVE: AtomicBool = AtomicBool::new(false);
/// The online-CPU mask while the scan sized the BARs; 0 before it runs.
/// `dev::ktest::test_pci_scan_bsp_only` reads it.
#[cfg(feature = "kernel_tests")]
pub(super) static SCAN_ONLINE: AtomicU64 = AtomicU64::new(0);

fn with_cfg<R>(f: impl FnOnce() -> R) -> R {
    let _irq = InterruptGuard::enter();
    // Acquire: pairs with the Release store that unlocks below.
    // Relaxed on failure: the lock is held; pairs with nothing.
    while CFG_LOCK
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    let r = f();
    // Release: pairs with the next holder's Acquire compare-exchange above.
    CFG_LOCK.store(false, Ordering::Release);
    r
}

#[cfg(target_arch = "x86_64")]
fn cf8_read32(bdf: Bdf, offset: u16) -> u32 {
    let addr = pci::cf8_addr(bdf.bus, bdf.device, bdf.function, offset);
    // SAFETY: invariant: ports 0xCF8 and 0xCFC are the PCI type-1 config
    // mechanism, and `with_cfg`, every caller's wrapper, holds `CFG_LOCK`
    // across the address and data accesses; established by
    // `pci_init::with_cfg`.
    unsafe {
        x86::outl(CFG_ADDR, addr);
        x86::inl(CFG_DATA)
    }
}

#[cfg(target_arch = "x86_64")]
fn cf8_write32(bdf: Bdf, offset: u16, val: u32) {
    let addr = pci::cf8_addr(bdf.bus, bdf.device, bdf.function, offset);
    // SAFETY: as in `cf8_read32`; established by `pci_init::with_cfg`.
    unsafe {
        x86::outl(CFG_ADDR, addr);
        x86::outl(CFG_DATA, val);
    }
}

/// Map `[phys, phys + len)` for the kernel: uncached through the physmap
/// below `map_end`, or through ioremap above it; a range that overlaps a
/// framebuffer stays write-back on the physmap. `None` for a range that
/// overlaps a RAM-typed range of the boot memory map, checked before
/// anything is patched or mapped (DESIGN §12.3 rule 8), and when the UC
/// patch fails. Reached only from [`map_bar`], through a live claim
/// (invariant I58), from `ecam_va`, and from the in-guest tests.
pub(super) fn map_mmio(phys: u64, len: u64) -> Option<u64> {
    if phys == 0 || len == 0 {
        return None;
    }
    if dev::overlaps_any(phys, len, boot::info().ram_ranges()) {
        return None;
    }
    let end = phys.checked_add(len)?;
    // A BAR that aliases the Limine FB stays WB on the physmap: a UC patch
    // or an ioremap would alias the console UC (DESIGN §4.1 / §9.2).
    // Limine's surface (and thus map_end) is often smaller than the BAR
    // (16 MiB); fill missing physmap leaves as WB so the BAR is mapped.
    if fb_init::overlaps_phys(phys, len) {
        // SAFETY: `ensure_physmap_wb`'s contract; this range overlaps the
        // framebuffer, which stays WB on the physmap and is never
        // UC-patched or ioremapped (invariant I17, established here).
        if unsafe { paging_init::ensure_physmap_wb(PhysAddr(phys), len) }
            || phys < paging_init::map_end()
        {
            return Some(paging_init::hhdm_offset().wrapping_add(phys));
        }
        return None;
    }
    if end <= paging_init::map_end() {
        // A failed patch leaves some of the range write-back: refuse it.
        // SAFETY: `paging_init::install` ran at boot, long before the PCI
        // scan, and `end <= map_end()`, so the physmap covers the range;
        // established by `paging_init::map_end`.
        unsafe { paging_init::patch_physmap_uc(PhysAddr(phys), len) }.ok()?;
        Some(paging_init::hhdm_offset().wrapping_add(phys))
    } else {
        // SAFETY: invariant I58: the range is a BAR its caller holds a
        // claim on, which overlaps no other claim and no RAM, or an ECAM
        // page, above the physmap, which only this mapping reaches;
        // established by `dev::Registry::claim` and `pci_init::ecam_va`.
        unsafe { paging_init::ioremap(PhysAddr(phys), len) }.map(|v| v.as_u64())
    }
}

/// Map the BAR `claim` holds, for the driver that claimed it; `None` for
/// an I/O BAR, one above the 32 MiB cap (§9.2), which is claimed but not
/// mapped, or a failed mapping.
pub fn map_bar(claim: &BarClaim) -> Option<u64> {
    if !claim.is_mem() {
        return None;
    }
    if !bar_map_allowed(claim.len()) {
        crate::marker!(
            "vibeOS: pci: skip bar {} size {:#x}",
            claim.bdf(),
            claim.len()
        );
        return None;
    }
    map_mmio(claim.phys(), claim.len())
}

/// Unmap the BAR `claim` holds from `va`, where [`map_bar`] mapped it. An
/// ioremap VA loses its leaves, on every CPU, before this returns; the
/// window never hands the VA out again (DESIGN §4.1). A physmap VA stays
/// mapped, uncached or write-back as `map_bar` left it, until ROADMAP
/// §11.2 makes the physmap RAM-only.
///
/// # Safety
/// `va` is what `map_bar(claim)` returned, and nothing touches it any
/// more: the driver has stopped its device and dropped every copy of it.
pub unsafe fn unmap_bar(claim: &BarClaim, va: u64) {
    let window = IOREMAP_BASE..IOREMAP_BASE.saturating_add(IOREMAP_LEN);
    if !window.contains(&va) {
        return;
    }
    let start = va & !(PAGE_SIZE_4K - 1);
    let head = va - start;
    let Some(end) = head
        .checked_add(claim.len())
        .and_then(|l| l.checked_add(PAGE_SIZE_4K - 1))
        .map(|l| start.saturating_add(l & !(PAGE_SIZE_4K - 1)))
    else {
        return;
    };
    let end = end.min(window.end);
    paging_init::with_pt(|pt| {
        let mut v = start;
        while v < end {
            // SAFETY: `unmap_4k_locked`'s contract: nothing uses the BAR's
            // VA any more (this fn's `# Safety` contract), and the
            // shootdown below runs before this fn returns; established by
            // `pci_init::unmap_bar`.
            let step = match unsafe { paging_init::unmap_4k_locked(pt, VirtAddr(v)) } {
                Some((_, size)) => size.bytes(),
                None => PAGE_SIZE_4K,
            };
            v = v.saturating_add(step);
        }
    });
    // Every 4 KiB page of the span, 32 pages a range, a round of ranges at
    // a time; an `invlpg` of any page of a 2 MiB leaf drops that leaf.
    let mut ranges = [ShootRange::page(start); SHOOT_RANGES];
    let mut n = 0usize;
    let mut v = start;
    while v < end {
        let pages = ((end - v) / PAGE_SIZE_4K).min(SHOOT_RANGE_PAGES);
        let Some(r) = ShootRange::new(v, pages) else {
            break;
        };
        if let Some(slot) = ranges.get_mut(n) {
            *slot = r;
            n += 1;
        }
        if n == SHOOT_RANGES {
            vibeos::paging::tlb_shootdown_ranges(&ranges);
            n = 0;
        }
        v = v.saturating_add(pages * PAGE_SIZE_4K);
    }
    if let Some(rest) = ranges.get(..n)
        && !rest.is_empty()
    {
        vibeos::paging::tlb_shootdown_ranges(rest);
    }
}

fn ecam_covers(bus: u8) -> bool {
    with_ecam(|e| e.base != 0 && bus >= e.start && bus <= e.end)
}

fn ecam_phys_of(bdf: Bdf, offset: u16) -> Option<u64> {
    with_ecam(|e| {
        pci::ecam_phys(
            e.base,
            e.start,
            e.end,
            bdf.bus,
            bdf.device,
            bdf.function,
            offset,
        )
    })
}

/// Map the 4K function page for `phys` and return the byte VA.
/// Cache miss still ioremaps; the returned VA is always used, even when
/// the table is full (a cache-only lookup would vanish later buses).
fn ecam_va(phys: u64) -> Option<u64> {
    let page = phys & !(PAGE_SIZE_4K - 1);
    if let Some(va) = with_ecam(|e| {
        let mut i = 0usize;
        while i < e.n {
            if e.phys[i] == page {
                return Some(pci::ecam_byte_va(e.va[i], phys));
            }
            i += 1;
        }
        None
    }) {
        return Some(va);
    }
    let va = map_mmio(page, PAGE_SIZE_4K)?;
    with_ecam(|e| {
        let slot = pci::ecam_cache_slot(e.n, ECAM_CACHE);
        if e.n < ECAM_CACHE {
            e.n += 1;
        }
        e.phys[slot] = page;
        e.va[slot] = va;
    });
    Some(pci::ecam_byte_va(va, phys))
}

/// # Safety
/// Invariant I54: `va` is a dword of an ECAM page `ecam_va` mapped.
unsafe fn ecam_read_at(va: u64) -> u32 {
    // SAFETY: invariant I54; established by `pci_init::ecam_read_at`'s
    // `# Safety` contract.
    unsafe { (va as *const u32).read_volatile() }
}

/// # Safety
/// As [`ecam_read_at`].
unsafe fn ecam_write_at(va: u64, val: u32) {
    // SAFETY: invariant I54; established by `pci_init::ecam_write_at`'s
    // `# Safety` contract.
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
            // SAFETY: invariant I54: `va` is the config dword's byte in
            // the ECAM page `ecam_va` just mapped, and config offsets are
            // dword-aligned below 4 KiB; established by `pci_init::map_mmio`.
            return with_cfg(|| unsafe { ecam_read_at(va) });
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
            // SAFETY: invariant I54, as in `read32`; established by
            // `pci_init::map_mmio`.
            with_cfg(|| unsafe { ecam_write_at(va, value) });
            return;
        }
        if bdf.bus == 0 {
            with_cfg(|| cf8_write32(bdf, offset, value));
        }
    }
}

/// What [`scan`] found: `n` functions in scan order, BARs sized.
struct Scan {
    n: usize,
    found: [FuncInfo; MAX_SCAN],
}

static SCAN: BootCell<Scan> = BootCell::new();

/// Set up ECAM from MCFG, then enumerate every function and size its BARs
/// (BOOT.md §3.3 step 15b), for [`init`] to publish. Before the first AP:
/// sizing writes all-ones to a live BAR and puts it back, and each write
/// moves the BAR in the guest's physical map. QEMU's TCG rebuilds its
/// memory map for it and flushes the other vCPUs' TLBs only later, so
/// their MMIO meanwhile can reach the wrong region: a LAPIC EOI lost that
/// way leaves the timer vector in service, and that CPU never takes an
/// IPI again (ROADMAP §10.2). With the BSP alone, no other CPU runs MMIO.
///
/// # Safety
/// Once, on the BSP, before `smp_init::init` starts an AP
/// (`BootCell::set`'s contract).
pub unsafe fn scan() {
    if let Some(h) = machine_init::info().and_then(|d| d.pci_hosts().first()) {
        with_ecam(|e| {
            e.base = h.ecam_base;
            e.start = h.first_bus;
            e.end = h.last_bus;
        });
    }
    let mut found = [FuncInfo::empty(); MAX_SCAN];
    let n = pci::enumerate(&mut HwCfg, 0, &mut found);
    // Release: pairs with the Acquire load in `dev::ktest::test_pci_scan_bsp_only`.
    #[cfg(feature = "kernel_tests")]
    SCAN_ONLINE.store(crate::per_cpu_init::online_mask(), Ordering::Release);
    // SAFETY: `BootCell::set`'s contract (invariant I22): this fn runs
    // once, on the BSP, before any AP starts or any reader runs (this fn's
    // `# Safety` contract); established here.
    unsafe { SCAN.set(Scan { n, found }) };
}

/// Publish each function [`scan`] found behind its parent bridge through
/// `publish` (the device registry's `dev_init::push`, which `_start`
/// passes), emit `pci: N devices`. Maps no BAR: each driver maps what it
/// claims.
pub fn init(publish: fn(Device, Option<u64>) -> Result<DevRef, AllocError>) {
    if let Some(h) = machine_init::info().and_then(|d| d.pci_hosts().first()) {
        crate::marker!(
            "vibeOS: pci: ecam {:#x} buses {}-{}",
            h.ecam_base,
            h.first_bus,
            h.last_bus
        );
    }

    let s = SCAN.get();
    let (n, found) = (s.n, &s.found);
    let scanned = found.get(..n).unwrap_or(&[]);
    // Each published function's entry id, by scan index; 0 for none. The
    // scan is depth first, so a bridge is published before what is behind
    // it.
    let mut ids = [0u64; MAX_SCAN];
    let mut i = 0usize;
    while i < n {
        let info = found[i];
        let parent = dev::parent_bridge(scanned, i)
            .and_then(|p| ids.get(p).copied())
            .filter(|&id| id != 0);
        let dev = Device::from_func(info);
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a write to Serial cannot fail (DESIGN §2.5)"
        )]
        let _ = scan_line(&info);
        match publish(dev, parent) {
            Ok(r) => {
                if let Some(id) = ids.get_mut(i) {
                    *id = r.id();
                }
            }
            Err(_) => crate::klog_ratelimited!(
                1000,
                vibeos::log::Level::Warn,
                "vibeOS: pci: registry or heap full, {} not published",
                info.bdf
            ),
        }
        i += 1;
    }
    crate::marker!("vibeOS: pci: {} devices", n);
    // Release: pairs with the Acquire load in `dev::ktest::pci_live`.
    LIVE.store(true, Ordering::Release);
}

/// One `pci:` scan line.
fn scan_line(info: &FuncInfo) -> core::fmt::Result {
    crate::serial::write_line_with(|w| {
        w.write_str("vibeOS: pci: ")?;
        pci::write_lspci_line(w, info)
    })
}

/// Set `set` and clear `clear` in `bdf`'s COMMAND and return COMMAND as
/// read back. The write leaves the RW1C STATUS half alone. This is the
/// kernel's one COMMAND writer: each driver turns on its own device
/// (DESIGN §12.3), and neither the binder nor MSI setup writes COMMAND.
pub fn update_command(bdf: Bdf, set: u16, clear: u16) -> u16 {
    let mut hw = HwCfg;
    let cmd = pci::read16(&mut hw, bdf, CFG_COMMAND);
    pci::write_command(&mut hw, bdf, pci::command_update(cmd, set, clear));
    pci::read16(&mut hw, bdf, CFG_COMMAND)
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "only the in-guest tests read a config word outside a driver yet"
    )
)]
pub fn cfg_read16(bdf: Bdf, offset: u16) -> u16 {
    pci::read16(&mut HwCfg, bdf, offset)
}
