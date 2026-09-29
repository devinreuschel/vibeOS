//! In-guest tests for dev (kernel_tests only). Rows: the list in crate::ktest.

use core::sync::atomic::{AtomicBool, Ordering};
use vibeos::dev::{ClaimError, Device, Driver, IdMatch, ProbeError};
use vibeos::dma::{self, DMA32_BOUNDARY, DmaAlloc};
use vibeos::fs::O_RDWR;
use vibeos::pci::{self, Bdf, CFG_COMMAND, CFG_VENDOR, CMD_MASTER, CMD_MEM};

use vibeos::kalloc::TryBox;
use vibeos::pci::CfgIo;
use vibeos::virtio::{F_EVENT_IDX, F_INDIRECT_DESC, F_VERSION_1};

use crate::arch::current::Arch;
use crate::dev_init;
use crate::dma_init;
use crate::fs_init;
use crate::heap_init::{self, fail_after::Scope};
use crate::ktest::{
    EDU_IDENT, EDU_IDENT_VAL, Outcome, bar0_va, fid, find_edu, mmio_r32, mmio_w32,
    quiescent_free_frames, spin_until_ns,
};
use crate::log_init;
use crate::paging_init;
use crate::pci_init;
use crate::thread_init;
use crate::virtio_init;

// ---- Hooks the dev tests (and other subsystems' tests) use. Their state
// stays in the production files as `pub(super)` items.

/// Devices in the registry.
pub(crate) fn len() -> usize {
    dev_init::REG.lock().len()
}

/// The first device with `vendor:device`, and its registry index.
pub(crate) fn find_id(vendor: u16, device: u16) -> Option<(usize, Device)> {
    let g = dev_init::REG.lock();
    (0..g.len()).find_map(|i| {
        g.get(i)
            .filter(|d| d.vendor == vendor && d.device_id == device)
            .map(|d| (i, *d))
    })
}

/// Claim BAR `bar` of registry device `dev_i`.
pub(crate) fn claim(dev_i: usize, bar: u8) -> Result<(), ClaimError> {
    dev_init::REG.lock().claim(dev_i, bar)
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

fn rng_features() -> u64 {
    virtio_init::FEATURES.load(Ordering::Acquire)
}

fn rng_uses_indirect() -> bool {
    rng_features() & F_INDIRECT_DESC != 0
}

fn rng_uses_event_idx() -> bool {
    rng_features() & F_EVENT_IDX != 0
}

fn rng_qdma_device() -> u64 {
    virtio_init::QDMA_DEV.load(Ordering::Acquire)
}

fn rng_data_device() -> u64 {
    virtio_init::DATA_DEV.load(Ordering::Acquire)
}

fn rng_data_virt() -> u64 {
    virtio_init::DATA_VIRT.load(Ordering::Acquire)
}

fn rng_completions() -> u32 {
    virtio_init::COMPLETIONS.load(Ordering::Acquire)
}

fn rng_top_hits() -> u32 {
    virtio_init::TOP_HITS.load(Ordering::Acquire)
}

fn rng_thread_hits() -> u32 {
    virtio_init::THREAD_HITS.load(Ordering::Acquire)
}

fn rng_alloced() -> bool {
    virtio_init::ALLOCED.load(Ordering::Acquire)
}

fn rng_soft_hits() -> u32 {
    virtio_init::SOFT_HITS.load(Ordering::Acquire)
}

fn rng_last_len() -> u32 {
    virtio_init::LAST_LEN.load(Ordering::Acquire)
}

const PCI_QEMU_IDS: &[(u16, u16)] = &[
    (0x8086, 0x1237), // 440FX
    (0x8086, 0x7000), // PIIX3 ISA
    (0x8086, 0x7010), // PIIX3 IDE
    (0x8086, 0x7113), // PIIX4 ACPI
    (0x1234, 0x1111), // Bochs VGA
    (0x8086, 0x100e), // e1000
];

pub(crate) fn test_pci_qemu_set() -> Outcome {
    if !pci_live() {
        return Outcome::Fail("pci not live");
    }
    if len() < PCI_QEMU_IDS.len() {
        return Outcome::Fail("device count");
    }
    let mut i = 0usize;
    while i < PCI_QEMU_IDS.len() {
        let (v, d) = PCI_QEMU_IDS[i];
        if find_id(v, d).is_none() {
            return Outcome::Fail("missing qemu id");
        }
        i += 1;
    }
    Outcome::Ok
}

pub(crate) fn test_pci_bar_map() -> Outcome {
    let Some((_, d)) = find_id(0x1234, 0x1111) else {
        return Outcome::Fail("no vga");
    };
    let r = d.resources[0];
    if r.is_empty() {
        return Outcome::Fail("vga bar0 empty");
    }
    if r.size == 0 || r.size > pci::MAX_BAR_MAP {
        return Outcome::Fail("vga bar0 size");
    }
    if r.mapped_va == 0 {
        return Outcome::Fail("vga bar0 unmapped");
    }
    // WB physmap alias, not an ioremap UC window over the console FB.
    if r.mapped_va != paging_init::HHDM_BASE.wrapping_add(r.addr) {
        return Outcome::Fail("vga bar0 not wb physmap");
    }
    Outcome::Ok
}

pub(crate) fn test_pci_cfg_rw() -> Outcome {
    let bdf = Bdf::new(0, 0, 0);
    let id = cfg_read32(bdf, CFG_VENDOR);
    if id as u16 != 0x8086 {
        return Outcome::Fail("host vendor");
    }
    if (id >> 16) as u16 != 0x1237 {
        return Outcome::Fail("host device");
    }
    let prev = cfg_read32(bdf, CFG_COMMAND) as u16;
    pci_init::enable_mem_master(bdf);
    let now = cfg_read32(bdf, CFG_COMMAND) as u16;
    cfg_write32(bdf, CFG_COMMAND, prev as u32);
    if now & (CMD_MEM | CMD_MASTER) != CMD_MEM | CMD_MASTER {
        return Outcome::Fail("cmd bits");
    }
    Outcome::Ok
}

pub(crate) fn test_pci_claim_exclusive() -> Outcome {
    let Some((i, d)) = find_id(0x8086, 0x100e) else {
        return Outcome::Fail("no e1000");
    };
    let mut b = 0u8;
    let mut found = false;
    while (b as usize) < pci::MAX_BARS {
        if !d.resources[b as usize].is_empty() {
            found = true;
            break;
        }
        b += 1;
    }
    if !found {
        return Outcome::Fail("e1000 no bar");
    }
    if let Err(e) = claim(i, b) {
        return Outcome::Fail(e.as_str());
    }
    match claim(i, b) {
        Err(ClaimError::Already) => Outcome::Ok,
        Err(_) => Outcome::Fail("wrong claim err"),
        Ok(()) => Outcome::Fail("double claim"),
    }
}

struct HostBridgeDrv;

static HOST_BRIDGE_IDS: &[IdMatch] = &[IdMatch::vid_did(0x8086, 0x1237)];

static HOST_BRIDGE_DRV: HostBridgeDrv = HostBridgeDrv;

impl Driver for HostBridgeDrv {
    fn name(&self) -> &'static str {
        "host-bridge"
    }
    fn ids(&self) -> &'static [IdMatch] {
        HOST_BRIDGE_IDS
    }
    fn order(&self) -> u8 {
        1
    }
    fn probe(&self, _dev: &mut Device) -> Result<(), ProbeError> {
        Ok(())
    }
    fn remove(&self, _dev: &mut Device) {}
}

pub(crate) fn test_pci_bind_order() -> Outcome {
    if !dev_init::register_driver(&HOST_BRIDGE_DRV) {
        return Outcome::Fail("register");
    }
    dev_init::bind_all();
    let Some((_, d)) = find_id(0x8086, 0x1237) else {
        return Outcome::Fail("no host");
    };
    match d.bound {
        Some("host-bridge") => Outcome::Ok,
        Some(_) => Outcome::Fail("wrong driver"),
        None => Outcome::Fail("unbound"),
    }
}

pub(crate) fn test_dma_alloc() -> Outcome {
    let before = quiescent_free_frames();
    let Some(buf) = dma_init::alloc(DmaAlloc::dma32(0x1000)) else {
        return Outcome::Fail("alloc");
    };
    if buf.device().as_u64() != buf.phys() {
        dma_init::free(buf);
        return Outcome::Fail("device != phys");
    }
    if buf.virt() != paging_init::HHDM_BASE.wrapping_add(buf.phys()) {
        dma_init::free(buf);
        return Outcome::Fail("virt not hhdm");
    }
    if buf.device().as_u64() == buf.virt() {
        dma_init::free(buf);
        return Outcome::Fail("device is va");
    }
    if buf.phys() >= DMA32_BOUNDARY || dma::crosses_boundary(buf.phys(), buf.len(), DMA32_BOUNDARY)
    {
        dma_init::free(buf);
        return Outcome::Fail("dma32");
    }
    buf.sync_for_device::<Arch>();
    // SAFETY: `buf` is this test's own DMA buffer, at least one byte long,
    // shared with no device; established by `dma_init::alloc`.
    unsafe {
        buf.as_ptr().write_volatile(0xA5);
    }
    buf.sync_for_cpu::<Arch>();
    let sg = match dma::SgList::from_buffer(&buf) {
        Ok(s) => s,
        Err(_) => {
            dma_init::free(buf);
            return Outcome::Fail("sg");
        }
    };
    if sg.n != 1 || sg.entries[0].addr != buf.device() {
        dma_init::free(buf);
        return Outcome::Fail("sg entry");
    }
    dma_init::free(buf);
    if dma_init::alloc(DmaAlloc {
        size: 0x1000,
        align: 0x1000,
        boundary: 0x800,
    })
    .is_some()
    {
        return Outcome::Fail("boundary refuse");
    }
    if quiescent_free_frames() != before {
        return Outcome::Fail("leak");
    }
    Outcome::Ok
}

const EDU_DMA_SRC: u32 = 0x80;

const EDU_DMA_DST: u32 = 0x88;

const EDU_DMA_CNT: u32 = 0x90;

const EDU_DMA_CMD: u32 = 0x98;

const EDU_DMA_BUF: u32 = 0x4_0000;

const EDU_DMA_RUN: u32 = 1;

const EDU_DMA_TO_PCI: u32 = 2;

pub(crate) fn test_dma_edu() -> Outcome {
    let Some((_, dev)) = find_edu() else {
        return Outcome::Skip("no edu");
    };
    let Some(mmio) = bar0_va(&dev) else {
        return Outcome::Fail("edu bar0");
    };
    if mmio_r32(mmio, EDU_IDENT) != EDU_IDENT_VAL {
        return Outcome::Fail("edu ident");
    }
    pci_init::enable_mem_master(dev.addr);
    let Some(src) = dma_init::alloc(DmaAlloc::dma32(64)) else {
        return Outcome::Fail("src");
    };
    let Some(dst) = dma_init::alloc(DmaAlloc::dma32(64)) else {
        dma_init::free(src);
        return Outcome::Fail("dst");
    };
    // SAFETY: `src` and `dst` are this test's own 64-byte DMA buffers, not
    // yet handed to the device; established by `dma_init::alloc`.
    unsafe {
        let p = src.as_ptr();
        let q = dst.as_ptr();
        let mut i = 0u32;
        while i < 64 {
            p.add(i as usize).write_volatile((0xC0 + i) as u8);
            q.add(i as usize).write_volatile(0);
            i += 1;
        }
    }
    src.sync_for_device::<Arch>();
    dst.sync_for_device::<Arch>();
    mmio_w32(mmio, EDU_DMA_SRC, src.device().as_u64() as u32);
    mmio_w32(mmio, EDU_DMA_DST, EDU_DMA_BUF);
    mmio_w32(mmio, EDU_DMA_CNT, 64);
    dma::dma_wmb::<Arch>();
    mmio_w32(mmio, EDU_DMA_CMD, EDU_DMA_RUN);
    if !spin_until_ns(
        || mmio_r32(mmio, EDU_DMA_CMD) & EDU_DMA_RUN == 0,
        2_000_000_000,
    ) {
        dma_init::free(src);
        dma_init::free(dst);
        return Outcome::Fail("dma to edu");
    }
    mmio_w32(mmio, EDU_DMA_SRC, EDU_DMA_BUF);
    mmio_w32(mmio, EDU_DMA_DST, dst.device().as_u64() as u32);
    mmio_w32(mmio, EDU_DMA_CNT, 64);
    dma::dma_wmb::<Arch>();
    mmio_w32(mmio, EDU_DMA_CMD, EDU_DMA_RUN | EDU_DMA_TO_PCI);
    if !spin_until_ns(
        || mmio_r32(mmio, EDU_DMA_CMD) & EDU_DMA_RUN == 0,
        2_000_000_000,
    ) {
        dma_init::free(src);
        dma_init::free(dst);
        return Outcome::Fail("dma from edu");
    }
    dst.sync_for_cpu::<Arch>();
    let mut bad = false;
    // SAFETY: both 64-byte buffers are this test's own, and the edu DMA
    // into `dst` has completed; established by `dma_init::alloc`.
    unsafe {
        let p = src.as_ptr();
        let q = dst.as_ptr();
        let mut i = 0usize;
        while i < 64 {
            if p.add(i).read_volatile() != q.add(i).read_volatile() {
                bad = true;
                break;
            }
            i += 1;
        }
    }
    dma_init::free(src);
    dma_init::free(dst);
    if bad {
        Outcome::Fail("mismatch")
    } else {
        Outcome::Ok
    }
}

fn find_rng() -> Option<(usize, Device)> {
    find_id(0x1af4, 0x1044).or_else(|| find_id(0x1af4, 0x1004))
}

pub(crate) fn test_virtio_bind() -> Outcome {
    let Some((_, d)) = find_rng() else {
        return Outcome::Skip("no virtio-rng");
    };
    if !virtio_init::rng_bound() {
        return Outcome::Fail("unbound");
    }
    match d.bound {
        Some("virtio-rng") => {}
        Some(_) => return Outcome::Fail("wrong driver"),
        None => return Outcome::Fail("id match"),
    }
    if rng_features() & F_VERSION_1 == 0 {
        return Outcome::Fail("no VERSION_1");
    }
    if !rng_uses_indirect() && !rng_uses_event_idx() {
        // Modern QEMU offers both; either is enough to prove negotiation.
        return Outcome::Fail("no optional feats");
    }
    Outcome::Ok
}

pub(crate) fn test_virtio_vq() -> Outcome {
    if !virtio_init::rng_bound() {
        return Outcome::Skip("no virtio-rng");
    }
    let qdev = rng_qdma_device();
    let ddev = rng_data_device();
    let dvirt = rng_data_virt();
    if qdev == 0 || ddev == 0 {
        return Outcome::Fail("dma");
    }
    if ddev == dvirt {
        return Outcome::Fail("device is va");
    }
    let c0 = rng_completions();
    let t0 = rng_top_hits();
    let th0 = rng_thread_hits();
    let s0 = rng_soft_hits();
    if virtio_init::rng_request().is_err() {
        return Outcome::Fail("request");
    }
    if !spin_until_ns(|| rng_completions() > c0, 2_000_000_000) {
        return Outcome::Fail("no complete");
    }
    if rng_last_len() == 0 {
        return Outcome::Fail("empty");
    }
    if rng_top_hits() <= t0 {
        return Outcome::Fail("no top");
    }
    if rng_thread_hits() <= th0 {
        return Outcome::Fail("no thread");
    }
    if !rng_alloced() {
        return Outcome::Fail("thread alloc");
    }
    if !spin_until_ns(|| rng_soft_hits() > s0, 2_000_000_000) {
        return Outcome::Fail("no softirq");
    }
    Outcome::Ok
}

pub(crate) fn test_dev_random_source() -> Outcome {
    if !virtio_init::rng_bound() {
        return Outcome::Skip("no virtio-rng");
    }
    if !fs_init::live() {
        return Outcome::Fail("not live");
    }
    let Ok(f) = fid::open("/dev/random", O_RDWR, 0) else {
        return Outcome::Fail("open");
    };
    let mut buf = [0u8; 16];
    let n = fid::read(f, &mut buf);
    let _ = fid::close(f);
    if n.ok() != Some(16) {
        return Outcome::Fail("read");
    }
    match vibeos::entropy::last_source() {
        vibeos::entropy::Source::XorShift => Outcome::Fail("xorshift"),
        vibeos::entropy::Source::VirtioRng | vibeos::entropy::Source::RdRand => Outcome::Ok,
    }
}

/// Set while `dev_probe_alloc_fail` binds, so `ktest-nomem` matches PIIX4
/// ACPI only then and later `bind_all` calls ignore it.
static NOMEM_ARMED: AtomicBool = AtomicBool::new(false);

static NOMEM_IDS: &[IdMatch] = &[IdMatch::vid_did(0x8086, 0x7113)];

/// A driver whose probe allocates, for PIIX4 ACPI, which no other driver
/// binds (ROADMAP §10.4's fallible-allocation box).
struct NoMemDrv;

static NOMEM_DRV: NoMemDrv = NoMemDrv;

impl Driver for NoMemDrv {
    fn name(&self) -> &'static str {
        "ktest-nomem"
    }
    fn ids(&self) -> &'static [IdMatch] {
        if NOMEM_ARMED.load(Ordering::Acquire) {
            NOMEM_IDS
        } else {
            &[]
        }
    }
    fn probe(&self, _dev: &mut Device) -> Result<(), ProbeError> {
        let b = TryBox::try_new([0u8; 64])?;
        drop(b);
        Ok(())
    }
    fn remove(&self, _dev: &mut Device) {}
}

/// Whether a record written after `mark` (a `log_init::written` count)
/// contains every one of `needles`.
fn logged_since(mark: u64, needles: &[&str]) -> bool {
    let new = log_init::written().saturating_sub(mark) as usize;
    let len = log_init::ring_len();
    (len.saturating_sub(new)..len).any(|i| {
        log_init::record_at(i).is_some_and(|r| {
            let m = r.msg();
            needles.iter().all(|n| {
                let n = n.as_bytes();
                m.windows(n.len()).any(|w| w == n)
            })
        })
    })
}

/// A probe whose allocation fails leaves its device unbound, with a log
/// line naming the driver and the device, and the kernel up.
pub(crate) fn test_dev_probe_alloc_fail() -> Outcome {
    let registered = dev_init::register_driver(&NOMEM_DRV) || {
        let g = dev_init::REG.lock();
        (0..g.driver_count()).any(|i| g.driver_at(i).is_some_and(|d| d.name() == "ktest-nomem"))
    };
    if !registered {
        return Outcome::Fail("register");
    }
    let Some((_, before)) = find_id(0x8086, 0x7113) else {
        return Outcome::Skip("no PIIX4 ACPI function");
    };
    if before.bound.is_some() {
        return Outcome::Fail("already bound");
    }
    let mark = log_init::written();
    NOMEM_ARMED.store(true, Ordering::Release);
    heap_init::fail_after::arm(0, Scope::Thread(thread_init::current_id()));
    dev_init::bind_all();
    let seen = heap_init::fail_after::disarm();
    NOMEM_ARMED.store(false, Ordering::Release);
    let Some((_, after)) = find_id(0x8086, 0x7113) else {
        return Outcome::Fail("device gone");
    };
    if after.bound.is_some() {
        return Outcome::Fail("bound");
    }
    let bdf = alloc::format!("{}", after.addr);
    if !logged_since(mark, &["probe ktest-nomem", &bdf]) {
        return Outcome::Fail("no log line");
    }
    if seen.refused < 1 {
        return Outcome::Fail("not refused");
    }
    if TryBox::try_new(0u64).is_err() {
        return Outcome::Fail("alloc after disarm");
    }
    Outcome::Ok
}

/// Test hooks in virtio-rng's pool path (AGENTS rule 9: `kernel_tests`
/// only). `virtio_init::publish_pool` calls [`on_publish`](rng_hooks::on_publish)
/// with each completion's payload, and `virtio_init::rng_take` calls
/// [`on_take_claim`](rng_hooks::on_take_claim) between a claim and its read.
pub(crate) mod rng_hooks {
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use crate::virtio_init::RNG_PAYLOAD;

    /// Refills that publish a counter pattern before the rest publish none.
    pub(crate) const PATTERN_REFILLS: u32 = 8;

    static PATTERN: AtomicBool = AtomicBool::new(false);
    /// Completions published since [`arm_pattern`].
    static REFILLS: AtomicU32 = AtomicU32::new(0);
    static STALL: AtomicU32 = AtomicU32::new(0);
    static ZERO_NEXT: AtomicBool = AtomicBool::new(false);
    /// Completions [`arm_zero_next`] emptied.
    static ZERO_PUB: AtomicU32 = AtomicU32::new(0);

    /// From now on, refill `r` publishes `(32·r + i) as u8` for
    /// `r < PATTERN_REFILLS`, and nothing from then on: 256 distinct values.
    pub(crate) fn arm_pattern() {
        REFILLS.store(0, Ordering::Relaxed);
        PATTERN.store(true, Ordering::Release);
    }

    /// Pattern refills published since [`arm_pattern`].
    pub(crate) fn pattern_refills() -> u32 {
        REFILLS.load(Ordering::Acquire).min(PATTERN_REFILLS)
    }

    /// Spin `spins` times between each take's claim and its read.
    pub(crate) fn set_take_stall(spins: u32) {
        STALL.store(spins, Ordering::Release);
    }

    /// The next completion publishes zero bytes.
    pub(crate) fn arm_zero_next() {
        ZERO_NEXT.store(true, Ordering::Release);
    }

    /// Completions [`arm_zero_next`] has emptied.
    pub(crate) fn zero_published() -> u32 {
        ZERO_PUB.load(Ordering::Acquire)
    }

    /// Turn every hook off.
    pub(crate) fn disarm() {
        PATTERN.store(false, Ordering::Release);
        STALL.store(0, Ordering::Release);
        ZERO_NEXT.store(false, Ordering::Release);
    }

    pub(crate) fn on_publish(payload: &mut [u8; RNG_PAYLOAD], len: &mut usize) {
        if ZERO_NEXT.swap(false, Ordering::AcqRel) {
            *len = 0;
            ZERO_PUB.fetch_add(1, Ordering::AcqRel);
            return;
        }
        if !PATTERN.load(Ordering::Acquire) {
            return;
        }
        let r = REFILLS.fetch_add(1, Ordering::AcqRel);
        if r >= PATTERN_REFILLS {
            *len = 0;
            return;
        }
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (r as usize * RNG_PAYLOAD + i) as u8;
        }
        *len = RNG_PAYLOAD;
    }

    pub(crate) fn on_take_claim() {
        let n = STALL.load(Ordering::Acquire);
        for _ in 0..n {
            core::hint::spin_loop();
        }
    }
}

/// Spins between a take's claim and its read: under 100,000 instructions
/// with IF=0 once the take holds the queue lock (AGENTS rule 2).
const RNG_STALL: u32 = 10_000;
/// How long the pool reader runs at most.
const RNG_READ_NS: u64 = 2_000_000_000;

static RNG_SEEN: [core::sync::atomic::AtomicU8; 256] =
    [const { core::sync::atomic::AtomicU8::new(0) }; 256];
static RNG_READER_DONE: AtomicBool = AtomicBool::new(false);
static RNG_REQ_ERRS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Drain the pool a byte at a time and ask for the next refill after each
/// take, as `hw_fill` did before ROADMAP §10.12, so a refill is nearly
/// always in flight while it drains.
fn rng_pool_reader() {
    let t0 = crate::time_init::now_ns();
    let mut b = [0u8; 1];
    loop {
        let n = virtio_init::rng_take(&mut b);
        if n == 1
            && let Some(c) = RNG_SEEN.get(b[0] as usize)
        {
            c.fetch_add(1, Ordering::AcqRel);
        }
        if virtio_init::rng_request().is_err() {
            RNG_REQ_ERRS.fetch_add(1, Ordering::Relaxed);
        }
        if n == 0 && rng_hooks::pattern_refills() >= rng_hooks::PATTERN_REFILLS {
            break;
        }
        if crate::time_init::now_ns().saturating_sub(t0) > RNG_READ_NS {
            break;
        }
    }
    RNG_READER_DONE.store(true, Ordering::Release);
}

/// ROADMAP §10.12 (F121, F140): each virtio-rng pool byte reaches one
/// reader, while refills land on another CPU.
pub(crate) fn rng_pool_no_dup() -> Outcome {
    if !virtio_init::rng_bound() {
        return Outcome::Skip("no virtio-rng");
    }
    let tcpu = crate::irq_init::threaded_cpu();
    let mask = crate::per_cpu_init::online_mask();
    let Some(cpu) = (0..64u32).find(|&c| c != tcpu && mask & (1u64 << c) != 0) else {
        return Outcome::Skip("one CPU");
    };
    // Empty the pool of the device's own bytes, which the pattern's 256
    // values could repeat, with no request left in flight.
    let c0 = rng_completions();
    if virtio_init::rng_request().is_err() {
        return Outcome::Fail("request");
    }
    // A request already in flight completes just the same.
    if !spin_until_ns(|| rng_completions() > c0, 2_000_000_000) {
        return Outcome::Fail("no completion");
    }
    let mut sink = [0u8; 64];
    while virtio_init::rng_take(&mut sink) > 0 {}
    for c in &RNG_SEEN {
        c.store(0, Ordering::Relaxed);
    }
    RNG_READER_DONE.store(false, Ordering::Release);
    RNG_REQ_ERRS.store(0, Ordering::Relaxed);
    rng_hooks::set_take_stall(RNG_STALL);
    rng_hooks::arm_pattern();
    let _reader = crate::ktest::spawn_thread_on("rng-reader", rng_pool_reader, cpu);
    let done = spin_until_ns(
        || RNG_READER_DONE.load(Ordering::Acquire),
        RNG_READ_NS + 1_000_000_000,
    );
    let refills = rng_hooks::pattern_refills();
    rng_hooks::disarm();
    // Leave the pool with the device's bytes for the tests after this one.
    let c1 = rng_completions();
    if virtio_init::rng_request().is_err() {
        return Outcome::Fail("request after");
    }
    if !spin_until_ns(|| rng_completions() > c1, 2_000_000_000) {
        return Outcome::Fail("no completion after");
    }
    if !done {
        return Outcome::Fail("reader did not finish");
    }
    let errs = RNG_REQ_ERRS.load(Ordering::Relaxed);
    if errs != 0 {
        return crate::fail_fmt!("{errs} requests failed");
    }
    for (v, c) in RNG_SEEN.iter().enumerate() {
        let n = c.load(Ordering::Acquire);
        if n > 1 {
            return crate::fail_fmt!("value {v:#04x} read {n} times");
        }
    }
    if refills < 3 {
        return crate::fail_fmt!("{refills} pattern refills landed, want 3");
    }
    Outcome::Ok
}

/// ROADMAP §10.12 (F121): after a completion that publishes zero bytes,
/// `hw_fill` asks for another refill, since the pool is empty and no
/// request is in flight.
pub(crate) fn rng_refill_after_empty_completion() -> Outcome {
    if !virtio_init::rng_bound() {
        return Outcome::Skip("no virtio-rng");
    }
    let z0 = rng_hooks::zero_published();
    rng_hooks::arm_zero_next();
    // A request already in flight completes empty just the same.
    if virtio_init::rng_request().is_err() {
        rng_hooks::disarm();
        return Outcome::Fail("request");
    }
    if !spin_until_ns(|| rng_hooks::zero_published() > z0, 2_000_000_000) {
        rng_hooks::disarm();
        return Outcome::Fail("no empty completion");
    }
    let c0 = rng_completions();
    let t0 = crate::time_init::now_ns();
    while rng_completions() <= c0 {
        if crate::time_init::now_ns().saturating_sub(t0) > 2_000_000_000 {
            return Outcome::Fail("no refill");
        }
        vibeos::entropy::hw_fill(&mut [0u8; 8]);
        core::hint::spin_loop();
    }
    Outcome::Ok
}
