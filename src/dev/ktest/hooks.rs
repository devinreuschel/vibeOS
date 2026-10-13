//! Shared hooks the dev tests (and other subsystems' tests) use.
//! Their state stays in the production files as `pub(super)` items.

use core::sync::atomic::Ordering;

use vibeos::dev::{DevRef, Driver, IdMatch, Instance, ProbeError};
use vibeos::lock::RANK_DEVICE;
use vibeos::paging::PAGE_SIZE_2M;
use vibeos::pci::{Bdf, CFG_COMMAND, CMD_MASTER, CMD_MEM, CfgIo};

use crate::dev_init;
use crate::ktest::quiescent_free_frames;
use crate::paging_init;
use crate::pci_init;
use crate::sync::blocking_init::BlockingMutex;
use crate::sync_init::SpinMutex;
use crate::virtio_init;

/// Devices in the registry.
pub(crate) fn len() -> usize {
    dev_init::REG.lock().len()
}

/// The first device with `vendor:device`.
pub(crate) fn find_id(vendor: u16, device: u16) -> Option<DevRef> {
    dev_init::find_id(vendor, device)
}

/// Whether the PCI scan has run.
pub(crate) fn pci_live() -> bool {
    pci_init::LIVE.load(Ordering::Acquire)
}

pub(crate) fn cfg_read32(bdf: Bdf, offset: u16) -> u32 {
    pci_init::HwCfg.read32(bdf, offset)
}

pub(crate) fn cfg_write32(bdf: Bdf, offset: u16, value: u32) {
    pci_init::HwCfg.write32(bdf, offset, value)
}

// ---- `bar-test`: the `kernel_tests` driver that claims and maps the BARs
// of the functions no production driver binds, for the tests that drive
// them (ROADMAP §10.12, F115).

static BAR_TEST_IDS: &[IdMatch] = &[
    IdMatch::vid_did(0x1234, 0x11e8), // edu, QEMU 8.x
    IdMatch::vid_did(0x1b36, 0x11e8), // edu, later trees
    IdMatch::vid_did(0x8086, 0x10d3), // e1000e
];

/// Claims and maps every memory BAR in `probe`, then turns on memory
/// decode; `remove` turns bus mastering off and gives the BARs back.
struct BarTestDrv;

static BAR_TEST_DRV: BarTestDrv = BarTestDrv;

impl Driver for BarTestDrv {
    fn name(&self) -> &'static str {
        "bar-test"
    }
    fn ids(&self) -> &'static [IdMatch] {
        BAR_TEST_IDS
    }
    fn order(&self) -> u8 {
        90
    }
    fn probe(&self, dev: &DevRef) -> Result<Option<Instance>, ProbeError> {
        dev_init::claim_mem_bars(dev)?;
        pci_init::update_command(dev.addr, CMD_MEM, 0);
        Ok(None)
    }
    fn remove(&self, dev: &DevRef) {
        pci_init::update_command(dev.addr, 0, CMD_MASTER);
        dev_init::release_bars(dev);
    }
}

/// Set once `bar-test` is registered and has had its bind.
static BAR_TEST_BOUND: BlockingMutex<bool> = BlockingMutex::new(false);

/// Register `bar-test` and bind it, once per boot; later calls return at
/// once. A second registration is refused, which is fine.
pub(crate) fn bind_bar_test_driver() {
    let mut done = BAR_TEST_BOUND.lock();
    if *done {
        return;
    }
    let _ = dev_init::register_driver(&BAR_TEST_DRV);
    dev_init::bind_all();
    *done = true;
}

/// A page-aligned 4 KiB page inside a usable range above 1 MiB.
pub(crate) fn usable_page() -> Option<u64> {
    crate::boot::info().usable().find_map(|r| {
        let start = r.start.max(0x10_0000).checked_add(0xFFF)? & !0xFFF;
        (start.checked_add(0x1000)? <= r.end).then_some(start)
    })
}

// ---- The fail-after-`QENABLE` hook both virtio probes call, and what
// `virtio_init::stop_device` saw (ROADMAP §10.12, F116; AGENTS rule 9:
// `kernel_tests` only).

/// The virtio-blk function `tests/harness/harness.py: ktest_devices`
/// reserves: its probe fails after `QENABLE` at every boot, so it stays
/// unbound for `virtio_probe_fail_quiesces`.
pub(crate) const PROBE_BLK_BDF: Bdf = Bdf::new(0, 0x1e, 0);

/// The armed function, as `virtio_init::bdf_key`; 0 for none.
static FAIL_ARMED: vibeos::atomic::statics::AtomicU64 = vibeos::atomic::statics::AtomicU64::new(0);

/// Whether the probe of `bdf` fails right after it enables its first
/// queue: always for [`PROBE_BLK_BDF`], and for the armed function.
pub(crate) fn fail_after_qenable(bdf: Bdf) -> bool {
    bdf == PROBE_BLK_BDF || FAIL_ARMED.load(Ordering::Acquire) == virtio_init::bdf_key(bdf)
}

/// Arm the hook for `bdf`, or disarm it with `None`.
pub(crate) fn arm_fail_after_qenable(bdf: Option<Bdf>) {
    FAIL_ARMED.store(bdf.map_or(0, virtio_init::bdf_key), Ordering::Release);
}

/// What `virtio_init::stop_device` read back, before the caller freed
/// anything: device status, COMMAND, and the free frames then.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Quiesced {
    pub bdf: Bdf,
    pub reset_ok: bool,
    pub status: u8,
    pub command: u16,
    pub free_frames: usize,
}

/// The last [`Quiesced`]. A leaf: nothing is locked under it.
static QUIESCED: SpinMutex<Option<Quiesced>> = SpinMutex::with_rank(None, RANK_DEVICE);

/// Record a stop. `stop_device` calls it with no device lock held, and the
/// free-frame count (the buddy lock) comes before [`QUIESCED`]'s.
pub(crate) fn record_quiesce(bdf: Bdf, reset_ok: bool, status: u8, command: u16) {
    let free_frames = crate::ktest::free_frames();
    *QUIESCED.lock() = Some(Quiesced {
        bdf,
        reset_ok,
        status,
        command,
        free_frames,
    });
}

/// The last recorded stop, clearing it.
pub(crate) fn take_quiesce() -> Option<Quiesced> {
    QUIESCED.lock().take()
}

/// Check a probe of `bdf` that failed after `QENABLE`: `stop_device` saw
/// status 0 and bus mastering off before the probe's frames went back
/// (fewer free than `before`), and they all went back by `after`.
pub(crate) fn check_quiesce(
    q: Option<Quiesced>,
    bdf: Bdf,
    before: usize,
    after: usize,
) -> Result<(), &'static str> {
    let Some(q) = q else {
        return Err("no stop recorded");
    };
    if q.bdf != bdf {
        return Err("stop recorded for another function");
    }
    if !q.reset_ok || q.status != 0 {
        return Err("status not 0 before the free");
    }
    if q.command & CMD_MASTER != 0 {
        return Err("bus mastering on before the free");
    }
    if q.free_frames >= before {
        return Err("frames freed before the stop");
    }
    if after != before {
        return Err("frames not returned");
    }
    if pci_init::cfg_read16(bdf, CFG_COMMAND) & CMD_MASTER != 0 {
        return Err("bus mastering on after the probe");
    }
    Ok(())
}

/// The ioremap window's cursor: the first VA it has not handed out.
fn ioremap_next() -> u64 {
    paging_init::with_pt(|pt| pt.window().next())
}

/// The page-table frames the ioremap window took while its cursor moved
/// from `a` to `b`: one for each 2 MiB of window VA first reached. The
/// window never hands VA out twice (DESIGN §4.1), so they stay.
fn window_tables(a: u64, b: u64) -> usize {
    let region = |next: u64| next.saturating_sub(1) / PAGE_SIZE_2M;
    region(b).saturating_sub(region(a)) as usize
}

/// Fail the probe of `bdf` twice through `probe`, the first warming the
/// heap, and check the second's stop. Each probe maps its BARs afresh
/// (`dev_init::claim_mem_bars`), so a page table the window took for them
/// counts as returned.
pub(crate) fn probe_fails_quiesced(bdf: Bdf, probe: impl Fn() -> bool) -> Result<(), &'static str> {
    if probe() {
        return Err("first probe bound");
    }
    let _ = take_quiesce();
    let before = quiescent_free_frames();
    let window = ioremap_next();
    let bound = probe();
    let q = take_quiesce();
    let after = quiescent_free_frames();
    let tables = window_tables(window, ioremap_next());
    if bound {
        return Err("second probe bound");
    }
    check_quiesce(q, bdf, before, after.saturating_add(tables))
}
