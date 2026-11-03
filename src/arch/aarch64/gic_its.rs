//! GICv3 ITS: command queue, device and collection tables, LPI mapping
//! (ARM IHI 0069G, DESIGN §5.4). The distributor and redistributors are in
//! `gic.rs`, which re-exports the entry points defined here.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::irq::{
    ITS_MAPD_SIZE, IrqError, ItsCommand, baser_entry_bytes, baser_indirect, encode_device_unmap,
    encode_lpi_free, gits_devbits, its_device_fits, its_event_limit, rdbase_field,
};
use vibeos::lock::RANK_DEVICE;
use vibeos::log::Level;

use crate::cell::BootCell;
use crate::sync_init::SpinMutex;

use super::gic_common::{
    GICR_TYPER, Gic, LPI_BASE, LPI_SLOTS, XBASER_SHARE_IS, alloc_pages, clean_table_if_needed,
    cleans_to_poc, gic, mmio32w, mmio64, mmio64w, note_shareability,
};

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
/// GITS_CBASER / GITS_BASER physical address (IHI 0069G bits [51:12]).
const ITS_PA_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const GITS_BASER_TYPE_DEVICE: u64 = 1;
const GITS_BASER_TYPE_COLLECTION: u64 = 4;

/// GITS_CBASER / GITS_BASER InnerCache (bits [61:59]) and OuterCache
/// (bits [55:53]): Normal write-back, `0b111` (IHI 0069G). The ITS reads its
/// tables with these attributes, so Device-nGnRnE would not do.
const ITS_TABLE_WB_IS: u64 = (0b111 << 59) | (0b111 << 53) | XBASER_SHARE_IS;
/// Device table geometry for `map_its_event`. Set once in `init_its`.
#[derive(Clone, Copy)]
struct DevTable {
    /// Bytes in the one device table page; 0 when none was allocated.
    bytes: u64,
    /// Bytes per device table entry, from GITS_BASER.Entry_Size.
    entry: u64,
    /// DeviceID bits the ITS implements, from GITS_TYPER.Devbits.
    devbits: u32,
}
static DEV_TABLE: BootCell<DevTable> = BootCell::new();

const ITS_DEV_MAX: usize = 16;
const ITS_DEV_EMPTY: u32 = u32::MAX;

struct ItsDev {
    id: AtomicU32,
    itt: AtomicU64,
    next_event: AtomicU32,
    /// MAPD V=1 has completed for this DeviceID.
    mapped: AtomicBool,
}

static ITS_DEVS: [ItsDev; ITS_DEV_MAX] = [const {
    ItsDev {
        id: AtomicU32::new(ITS_DEV_EMPTY),
        itt: AtomicU64::new(0),
        next_event: AtomicU32::new(0),
        mapped: AtomicBool::new(false),
    }
}; ITS_DEV_MAX];
/// EventID programmed for each LPI slot, for `compose_msi`.
pub(super) static LPI_EVENT: [AtomicU32; LPI_SLOTS] = [const { AtomicU32::new(0) }; LPI_SLOTS];
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

/// Allocate ITS tables, the command queue, and enable the ITS. `rd` is the
/// boot CPU's redistributor, which the BSP collection maps.
///
/// # Safety
/// `its` is the mapped ITS; paging and the buddy are up.
pub(super) unsafe fn init_its(g: &Gic, its: u64, rd: u64) {
    let Some(cmd) = alloc_pages(1) else {
        crate::klog!(Level::Error, "vibeOS: gic: its cmdq");
        return;
    };
    // Relaxed: command-queue PA; pairs with nothing.
    CMDQ.store(cmd, Ordering::Relaxed);
    // SAFETY: ITS tables and command queue, never freed; `DEV_TABLE` is set
    // once, on the BSP before SMP (invariant I22). established here.
    unsafe {
        let dev = init_its_basers(its);
        DEV_TABLE.set(dev);
        // Valid, write-back, Inner Shareable. Size 0 = one 4 KiB page (IHI 0069G).
        let cbaser = (cmd & ITS_PA_MASK) | (1u64 << 63) | ITS_TABLE_WB_IS;
        mmio64w(its, GITS_CBASER, cbaser);
        note_shareability(cbaser, mmio64(its, GITS_CBASER));
        clean_table_if_needed(cmd, 4096);
        mmio64w(its, GITS_CWRITER, 0);
        mmio32w(its, GITS_CTLR, GITS_CTLR_ENABLED);
    }
    // BSP collection. APs map theirs in `enable_ap`.
    map_collection(g, its, rd);
}

/// Program each implemented GITS_BASER (Devices and Collections). Returns
/// the geometry of the device table, `bytes` 0 when none was allocated.
///
/// # Safety
/// `its` is the mapped ITS; the buddy is up.
unsafe fn init_its_basers(its: u64) -> DevTable {
    // SAFETY: GITS_TYPER is a read-only ITS register. established here.
    let devbits = gits_devbits(unsafe { mmio64(its, GITS_TYPER) });
    let mut dev = DevTable {
        bytes: 0,
        entry: 0,
        devbits,
    };
    let mut n = 0u64;
    while n < 8 {
        let off = GITS_BASER + n * 8;
        // SAFETY: GITS_BASER<n> is an ITS table register. established here.
        let b = unsafe { mmio64(its, off) };
        let ty = (b >> 56) & 7;
        if ty == GITS_BASER_TYPE_DEVICE || ty == GITS_BASER_TYPE_COLLECTION {
            let Some(pa) = alloc_pages(1) else {
                n += 1;
                continue;
            };
            // Keep Type and Entry_Size (RO). 4 KiB pages, Size=0, Valid,
            // write-back and Inner Shareable.
            let keep = b & ((0x7u64 << 56) | (0x1Fu64 << 48));
            let val = keep | (pa & ITS_PA_MASK) | (1u64 << 63) | ITS_TABLE_WB_IS;
            // SAFETY: as `init_its_basers`. established here.
            unsafe { mmio64w(its, off, val) };
            // SAFETY: as `init_its_basers`. established here.
            let read = unsafe { mmio64(its, off) };
            note_shareability(val, read);
            clean_table_if_needed(pa, 4096);
            if ty == GITS_BASER_TYPE_DEVICE && dev.bytes == 0 {
                if baser_indirect(read) {
                    // The ITS kept Indirect set, so it does not read this page as
                    // a device table. Leave `bytes` 0 and `map_its_event` refuses.
                    crate::klog!(
                        Level::Error,
                        "vibeOS: gic: its device table is indirect; no device table"
                    );
                } else {
                    dev.bytes = 4096;
                    dev.entry = baser_entry_bytes(read);
                }
            }
        }
        n += 1;
    }
    dev
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
    clean_table_if_needed(itt, 4096);
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

fn lpi_slot(hwirq: u32) -> Option<usize> {
    let slot = hwirq.checked_sub(LPI_BASE)? as usize;
    if slot >= LPI_SLOTS { None } else { Some(slot) }
}

/// Bind `hwirq` as the next EventID on `device_id`. No ITS is `Ok`:
/// GICv2 still has a v2m SPI. Past the MAPD size is [`IrqError::Exhausted`].
pub fn map_its_event(device_id: u32, hwirq: u32) -> Result<(), IrqError> {
    let Some(g) = gic() else { return Ok(()) };
    let Some(its) = g.its else { return Ok(()) };
    let Some(slot) = lpi_slot(hwirq) else {
        return Err(IrqError::BadVector);
    };
    // The device table is one page, so a DeviceID past its entries, or past
    // the DeviceID bits the ITS implements, has no entry to hold it.
    let fits = DEV_TABLE
        .try_get()
        .is_some_and(|t| its_device_fits(device_id, t.bytes, t.entry, t.devbits));
    if !fits {
        crate::klog!(
            Level::Error,
            "vibeOS: gic: its dev {device_id:#x} past the device table"
        );
        return Err(IrqError::NoRoute);
    }
    let Some(dev) = claim_its_dev(device_id) else {
        crate::klog!(Level::Error, "vibeOS: gic: its itt");
        return Err(IrqError::Exhausted);
    };
    // Acquire: pairs with the Release store of `itt` in `claim_its_dev`.
    let itt = dev.itt.load(Ordering::Acquire);
    if itt == 0 {
        crate::klog!(Level::Error, "vibeOS: gic: its itt");
        return Err(IrqError::Exhausted);
    }
    let Some(rd0) = collection_rd(0) else {
        crate::klog!(Level::Error, "vibeOS: gic: its no collection 0");
        return Err(IrqError::NoRoute);
    };
    let Some(limit) = its_event_limit(ITS_MAPD_SIZE) else {
        return Err(IrqError::Exhausted);
    };
    // EventID stays put until MAPTI and SYNC both retire, under `ITS_CMDS`.
    let _hold = ITS_CMDS.lock();
    // Relaxed: per-device EventID under `ITS_CMDS`; pairs with nothing.
    let event = dev.next_event.load(Ordering::Relaxed);
    if event >= limit {
        crate::klog!(Level::Error, "vibeOS: gic: its events");
        return Err(IrqError::Exhausted);
    }
    // SAFETY: ITS command queue; `ITS_CMDS` is held. established here.
    let ok = unsafe {
        let mut ok = true;
        if event == 0 {
            match ItsCommand::mapd(device_id, itt, ITS_MAPD_SIZE, true) {
                Ok(mapd) => {
                    ok = push_cmds(its, &[mapd]);
                    if ok {
                        // Release: pairs with the Acquire load in `remove_its_device`.
                        dev.mapped.store(true, Ordering::Release);
                    }
                }
                Err(_) => ok = false,
            }
        }
        ok && push_cmds(
            its,
            &[
                ItsCommand::mapti(device_id, event, hwirq, 0),
                ItsCommand::sync(rd0),
            ],
        )
    };
    if !ok {
        crate::klog!(
            Level::Error,
            "vibeOS: gic: its map dev {device_id:#x} ev {event}"
        );
        return Err(IrqError::Busy);
    }
    if let Some(e) = LPI_EVENT.get(slot) {
        // Release: pairs with the Acquire load in `compose_msi` and `movi_lpi`.
        e.store(event, Ordering::Release);
    }
    if let Some(c) = LPI_COL.get(slot) {
        // Release: pairs with the Acquire load in `discard_lpi`.
        c.store(0, Ordering::Release);
    }
    if let Some(d) = LPI_DEV.get(slot) {
        // Release: pairs with the Acquire loads in `movi_lpi` and `discard_lpi`.
        d.store(device_id, Ordering::Release);
    }
    if let Some(next) = event.checked_add(1) {
        // Relaxed: next EventID under `ITS_CMDS`; pairs with nothing.
        dev.next_event.store(next, Ordering::Relaxed);
    }
    Ok(())
}

/// MAPC this CPU's collection to `rd_va`, then SYNC. The BSP calls it from
/// `init_its`; each AP from `enable_ap`, after its redistributor has LPIs on.
pub(super) fn map_collection(g: &Gic, its: u64, rd_va: u64) {
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
    let (its_typer, rd_typer) = unsafe { (mmio64(its, GITS_TYPER), mmio64(rd_va, GICR_TYPER)) };
    let off = rd_va.wrapping_sub(g.cpu_or_redist);
    let pa = g.redist_pa.wrapping_add(off);
    rdbase_field(its_typer & GITS_TYPER_PTA != 0, rd_typer, pa)
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
pub(super) fn movi_lpi(its: u64, hwirq: u32, cpu: u32) -> Result<(), IrqError> {
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

/// DISCARD pushes that failed since boot.
static DISCARD_FAILS: AtomicU64 = AtomicU64::new(0);
/// `remove_its_device` calls that refused or failed since boot.
static REMOVE_FAILS: AtomicU64 = AtomicU64::new(0);

/// `remove_its_device`'s failure path, out of line: the count and the
/// rate-limited line, as `send_failed` in `ipi_init.rs`.
#[cold]
#[inline(never)]
fn remove_failed(device_id: u32, what: &str) {
    // Relaxed: a count for the rate-limited line; pairs with nothing.
    let n = REMOVE_FAILS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    crate::klog_ratelimited!(
        1000,
        Level::Error,
        "vibeOS: gic: its remove dev={device_id:#x}: {} ({} since boot)",
        what,
        n
    );
}

/// DISCARD one mapped event and SYNC. A failed command leaves `LPI_DEV` set.
pub(super) fn discard_lpi(hwirq: u32) {
    let Some(g) = gic() else { return };
    let Some(its) = g.its else { return };
    let Some(slot) = lpi_slot(hwirq) else { return };
    let Some(dev_slot) = LPI_DEV.get(slot) else {
        return;
    };
    let _hold = ITS_CMDS.lock();
    // Acquire: pairs with the Release store in `map_its_event`.
    let dev = dev_slot.load(Ordering::Acquire);
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
    // SAFETY: ITS command queue; `ITS_CMDS` is held here.
    let ok = unsafe { push_cmds(its, &encode_lpi_free(dev, event, rd)) };
    if !ok {
        // Relaxed: a count for the rate-limited line; pairs with nothing.
        let n = DISCARD_FAILS
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        crate::klog_ratelimited!(
            1000,
            Level::Error,
            "vibeOS: gic: its discard dev {dev:#x} ev {event} ({n} since boot)"
        );
        return;
    }
    // Release: pairs with the Acquire loads in `movi_lpi` and `remove_its_device`.
    dev_slot.store(LPI_NO_DEV, Ordering::Release);
}

/// MAPD V=0 once none of `device_id`'s LPIs are mapped. The ITT stays.
pub fn remove_its_device(device_id: u32) {
    let Some(g) = gic() else { return };
    let Some(its) = g.its else { return };
    let Some(rd) = collection_rd(0) else {
        remove_failed(device_id, "no collection 0");
        return;
    };
    let _hold = ITS_CMDS.lock();
    let dev = ITS_DEVS.iter().find(|d| {
        // Relaxed: DeviceID is written once; pairs with nothing.
        d.id.load(Ordering::Relaxed) == device_id
    });
    let Some(dev) = dev else { return };
    // Acquire: pairs with the Release store in `map_its_event`.
    if !dev.mapped.load(Ordering::Acquire) {
        return;
    }
    let live = LPI_DEV.iter().any(|slot| {
        // Acquire: pairs with the Release stores in `map_its_event` and `discard_lpi`.
        slot.load(Ordering::Acquire) == device_id
    });
    if live {
        remove_failed(device_id, "an LPI is still mapped");
        return;
    }
    let Ok(cmds) = encode_device_unmap(device_id, rd) else {
        remove_failed(device_id, "unmap encode failed");
        return;
    };
    // SAFETY: ITS command queue; `ITS_CMDS` is held here.
    if unsafe { !push_cmds(its, &cmds) } {
        remove_failed(device_id, "unmap command failed");
        return;
    }
    // Release: pairs with the Acquire load in `remove_its_device`.
    dev.mapped.store(false, Ordering::Release);
    // Relaxed: EventID reset under `ITS_CMDS`; pairs with nothing.
    dev.next_event.store(0, Ordering::Relaxed);
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
        if cleans_to_poc() {
            crate::arch::aarch64::secondary::clean_poc(va, 32);
        }
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
