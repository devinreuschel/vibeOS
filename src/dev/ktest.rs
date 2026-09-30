//! In-guest tests for dev (kernel_tests only). Rows: [`TESTS`].

use core::sync::atomic::{AtomicBool, Ordering};
use vibeos::dev::{ClaimError, DevRef, Driver, IdMatch, Instance, ProbeError};
use vibeos::dma::{self, DMA32_BOUNDARY, DmaAlloc};
use vibeos::fs::O_RDWR;
use vibeos::lock::RANK_DEVICE;
use vibeos::pci::{self, Bdf, CFG_COMMAND, CFG_VENDOR, CMD_INTX_DISABLE, CMD_MASTER, CMD_MEM};

use vibeos::kalloc::TryBox;
use vibeos::pci::CfgIo;
use vibeos::virtio::{F_EVENT_IDX, F_INDIRECT_DESC, F_VERSION_1};

use crate::arch::current::Arch;
use crate::dev_init;
use crate::dma_init;
use crate::fs_init;
use crate::heap_init::{self, fail_after::Scope};
use crate::ktest::{
    EDU_IDENT, EDU_IDENT_VAL, Outcome, Test, bar0_va, fid, find_edu, mmio_r32, mmio_w32,
    quiescent_free_frames, spin_until_ns, test,
};
use crate::log_init;
use crate::paging_init;
use crate::pci_init;
use crate::sync_init::SpinMutex;
use crate::thread_init;
use crate::virtio_init;

// ---- Hooks the dev tests (and other subsystems' tests) use. Their state
// stays in the production files as `pub(super)` items.

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
    let Some(d) = find_id(0x1234, 0x1111) else {
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
    pci_init::update_command(bdf, CMD_MEM | CMD_MASTER, 0);
    let now = cfg_read32(bdf, CFG_COMMAND) as u16;
    cfg_write32(bdf, CFG_COMMAND, prev as u32);
    if now & (CMD_MEM | CMD_MASTER) != CMD_MEM | CMD_MASTER {
        return Outcome::Fail("cmd bits");
    }
    Outcome::Ok
}

pub(crate) fn test_pci_claim_exclusive() -> Outcome {
    let Some(d) = find_id(0x8086, 0x100e) else {
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
    if let Err(e) = dev_init::claim(&d, b) {
        return Outcome::Fail(e.as_str());
    }
    match dev_init::claim(&d, b) {
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
    fn probe(&self, _dev: &DevRef) -> Result<Option<Instance>, ProbeError> {
        Ok(None)
    }
    fn remove(&self, _dev: &DevRef) {}
}

pub(crate) fn test_pci_bind_order() -> Outcome {
    if !dev_init::register_driver(&HOST_BRIDGE_DRV) {
        return Outcome::Fail("register");
    }
    dev_init::bind_all();
    let Some(d) = find_id(0x8086, 0x1237) else {
        return Outcome::Fail("no host");
    };
    match dev_init::bound(&d) {
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
    let Some(dev) = find_edu() else {
        return Outcome::Skip("no edu");
    };
    let Some(mmio) = bar0_va(&dev) else {
        return Outcome::Fail("edu bar0");
    };
    if mmio_r32(mmio, EDU_IDENT) != EDU_IDENT_VAL {
        return Outcome::Fail("edu ident");
    }
    pci_init::update_command(dev.addr, CMD_MEM | CMD_MASTER, 0);
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

fn find_rng() -> Option<DevRef> {
    find_id(0x1af4, 0x1044).or_else(|| find_id(0x1af4, 0x1004))
}

pub(crate) fn test_virtio_bind() -> Outcome {
    let Some(d) = find_rng() else {
        return Outcome::Skip("no virtio-rng");
    };
    if !virtio_init::rng_bound() {
        return Outcome::Fail("unbound");
    }
    match dev_init::bound(&d) {
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
    // Hardware bytes only (ROADMAP §10.12): a short count, or `Again`
    // while virtio-rng refills and RDRAND is absent. Each attempt is its own
    // VFS section, and the wait between them holds no lock (AGENTS rule 2).
    let mut buf = [0u8; 16];
    let t0 = crate::time_init::now_ns();
    let n = loop {
        match fid::read(f, &mut buf) {
            Err(vibeos::fs::FsError::Again)
                if crate::time_init::now_ns().saturating_sub(t0) < 2_000_000_000 =>
            {
                spin_until_ns(|| false, 1_000_000);
            }
            r => break r,
        }
    };
    let _ = fid::close(f);
    match n {
        Ok(1..=16) => {}
        Ok(n) => return crate::fail_fmt!("read {n} bytes"),
        Err(e) => return crate::fail_fmt!("read: {}", e.as_str()),
    }
    match vibeos::entropy::last_source() {
        Some(_) => Outcome::Ok,
        None => Outcome::Fail("no source"),
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
    fn probe(&self, _dev: &DevRef) -> Result<Option<Instance>, ProbeError> {
        let b = TryBox::try_new([0u8; 64])?;
        drop(b);
        Ok(None)
    }
    fn remove(&self, _dev: &DevRef) {}
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
    let Some(before) = find_id(0x8086, 0x7113) else {
        return Outcome::Skip("no PIIX4 ACPI function");
    };
    if dev_init::bound(&before).is_some() {
        return Outcome::Fail("already bound");
    }
    let mark = log_init::written();
    NOMEM_ARMED.store(true, Ordering::Release);
    heap_init::fail_after::arm(0, Scope::Thread(thread_init::current_id()));
    dev_init::bind_all();
    let seen = heap_init::fail_after::disarm();
    NOMEM_ARMED.store(false, Ordering::Release);
    let Some(after) = find_id(0x8086, 0x7113) else {
        return Outcome::Fail("device gone");
    };
    if dev_init::bound(&after).is_some() {
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

/// The second virtio-rng function `tests/harness/harness.py: ktest_devices`
/// adds, in a slot after the first rng in bus order, so it is refused.
pub(crate) const SPARE_RNG_BDF: Bdf = Bdf::new(0, 0x1d, 0);

/// Every virtio-rng function in the registry, in registration order.
fn rng_functions() -> ([Option<DevRef>; 4], usize) {
    let mut out: [Option<DevRef>; 4] = [const { None }; 4];
    let mut n = 0usize;
    let mut i = 0usize;
    while let Some(d) = dev_init::get(i) {
        i += 1;
        let rng = d.vendor == 0x1af4 && (d.device_id == 0x1044 || d.device_id == 0x1004);
        if !rng {
            continue;
        }
        if let Some(slot) = out.get_mut(n) {
            *slot = Some(d);
        }
        n += 1;
    }
    (out, n)
}

/// ROADMAP §10.12 (F121): a second virtio-rng function is refused, and
/// the refusal touches neither the device nor the first one's state.
pub(crate) fn rng_second_probe_refused() -> Outcome {
    let (fns, n) = rng_functions();
    if n != 2 {
        return crate::fail_fmt!("{n} virtio-rng functions, want 2");
    }
    let Some(spare) = fns.iter().flatten().find(|d| d.addr == SPARE_RNG_BDF) else {
        return Outcome::Fail("no rng at 00:1d.0");
    };
    let Some(first) = fns.iter().flatten().find(|d| d.addr != SPARE_RNG_BDF) else {
        return Outcome::Fail("no first rng");
    };
    if dev_init::bound(spare).is_some() {
        return Outcome::Fail("spare rng bound");
    }
    if dev_init::bound(first) != Some("virtio-rng") {
        return Outcome::Fail("first rng unbound");
    }
    let cmd0 = pci_init::cfg_read16(SPARE_RNG_BDF, CFG_COMMAND);
    let isr0 = virtio_init::ISR_VA.load(Ordering::Acquire);
    let q0 = rng_qdma_device();
    let r = virtio_init::RNG_DRV.probe(spare);
    if r.is_ok() {
        return Outcome::Fail("second probe bound");
    }
    if pci_init::cfg_read16(SPARE_RNG_BDF, CFG_COMMAND) != cmd0 {
        return Outcome::Fail("refused probe wrote COMMAND");
    }
    if virtio_init::ISR_VA.load(Ordering::Acquire) != isr0 {
        return Outcome::Fail("ISR_VA changed");
    }
    if rng_qdma_device() != q0 {
        return Outcome::Fail("queue changed");
    }
    if !virtio_init::rng_bound() {
        return Outcome::Fail("BOUND cleared");
    }
    let c0 = rng_completions();
    if virtio_init::rng_request().is_err() {
        return Outcome::Fail("request");
    }
    if !spin_until_ns(|| rng_completions() > c0, 2_000_000_000) {
        return Outcome::Fail("first rng no completion");
    }
    Outcome::Ok
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

/// Fail the probe of `bdf` twice through `probe`, the first warming the
/// heap, and check the second's stop.
pub(crate) fn probe_fails_quiesced(bdf: Bdf, probe: impl Fn() -> bool) -> Result<(), &'static str> {
    if probe() {
        return Err("first probe bound");
    }
    let _ = take_quiesce();
    let before = quiescent_free_frames();
    let bound = probe();
    let q = take_quiesce();
    let after = quiescent_free_frames();
    if bound {
        return Err("second probe bound");
    }
    check_quiesce(q, bdf, before, after)
}

/// The virtio-rng half of `virtio_probe_fail_quiesces` (ROADMAP §10.12,
/// F116): remove the bound rng, fail its probe after `QENABLE` twice, then
/// bind it again and use it.
pub(crate) fn rng_fail_after_qenable_case() -> Outcome {
    let (fns, _) = rng_functions();
    let Some(d) = fns.iter().flatten().find(|d| d.addr != SPARE_RNG_BDF) else {
        return Outcome::Fail("no bound rng");
    };
    if !virtio_init::rng_bound() {
        return Outcome::Fail("rng unbound");
    }
    virtio_init::RNG_DRV.remove(d);
    if virtio_init::rng_bound() {
        return Outcome::Fail("BOUND after remove");
    }
    arm_fail_after_qenable(Some(d.addr));
    let failed = probe_fails_quiesced(d.addr, || virtio_init::RNG_DRV.probe(d).is_ok());
    arm_fail_after_qenable(None);
    // Bind it again whatever the checks found, for the tests after this.
    let rebound = virtio_init::RNG_DRV.probe(d).is_ok();
    if let Err(why) = failed {
        return Outcome::Fail(why);
    }
    if !rebound || !virtio_init::rng_bound() {
        return Outcome::Fail("rng not bound again");
    }
    let want = CMD_MEM | CMD_MASTER | CMD_INTX_DISABLE;
    if pci_init::cfg_read16(d.addr, CFG_COMMAND) & want != want {
        return Outcome::Fail("driver did not turn its device on");
    }
    let c0 = rng_completions();
    if virtio_init::rng_request().is_err() {
        return Outcome::Fail("request");
    }
    if !spin_until_ns(|| rng_completions() > c0, 2_000_000_000) {
        return Outcome::Fail("rng no completion after rebind");
    }
    Outcome::Ok
}

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[
    test("pci_qemu_set", test_pci_qemu_set),
    test("pci_bar_map", test_pci_bar_map),
    test("pci_cfg_rw", test_pci_cfg_rw),
    test("pci_claim_exclusive", test_pci_claim_exclusive).once(),
    test("pci_bind_order", test_pci_bind_order).once(),
    test("dma_alloc", test_dma_alloc),
    test("dma_edu", test_dma_edu),
    test("virtio_bind", test_virtio_bind),
    test("virtio_vq", test_virtio_vq),
    test("dev_random_source", test_dev_random_source),
    test("rng_pool_no_dup", rng_pool_no_dup),
    test(
        "rng_refill_after_empty_completion",
        rng_refill_after_empty_completion,
    ),
    test("dev_probe_alloc_fail", test_dev_probe_alloc_fail),
    test("rng_second_probe_refused", rng_second_probe_refused),
];
