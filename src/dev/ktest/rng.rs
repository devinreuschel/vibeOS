//! virtio-rng and `/dev/random` in-guest tests.

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::dev::{DevRef, DevState, Driver};
use vibeos::pci::{Bdf, CFG_COMMAND, CMD_INTX_DISABLE, CMD_MASTER, CMD_MEM};
use vibeos::virtio::{F_EVENT_IDX, F_INDIRECT_DESC, F_VERSION_1};

use crate::dev_init;
use crate::ktest::{Outcome, spin_until_ns};
use crate::pci_init;
use crate::virtio_init;

use super::{arm_fail_after_qenable, find_id, probe_fails_quiesced};

#[cfg(target_arch = "x86_64")]
use crate::entropy_init;
#[cfg(target_arch = "x86_64")]
use crate::fs_init;
#[cfg(target_arch = "x86_64")]
use crate::ktest::fid;
#[cfg(target_arch = "x86_64")]
use crate::ktest::user::{self, DEFAULT, Image, user_code};
#[cfg(target_arch = "x86_64")]
use vibeos::fs::O_RDWR;
#[cfg(target_arch = "x86_64")]
use vibeos::proc::wait_exited;

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

fn find_rng() -> Option<DevRef> {
    find_id(0x1af4, 0x1044).or_else(|| find_id(0x1af4, 0x1005))
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
    if dev_init::state(&d) != Some(DevState::Bound) {
        return Outcome::Fail("rng not Bound");
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

// open("/dev/random", O_RDONLY), then read 16 bytes onto the stack;
// exit with the errno either returned (a positive count exits 0x80 | 16).
#[cfg(target_arch = "x86_64")]
user_code!(
    DEV_RANDOM_READ,
    "
    lea rdi, [rip + 90f]
    xor esi, esi
    xor edx, edx
    mov eax, 2
    syscall
    test rax, rax
    js 80f
    mov rdi, rax
    sub rsp, 64
    mov rsi, rsp
    mov edx, 16
    xor eax, eax
    syscall
    test rax, rax
    js 80f
    or eax, 0x80
    mov edi, eax
    jmp 81f
80:
    neg rax
    mov rdi, rax
81:
    mov eax, 60
    syscall
    ud2
90:
    .asciz \"/dev/random\"
    "
);

/// A `read` of `/dev/random` from ring 3, with no hardware entropy,
/// returns `EAGAIN` (11) through the syscall (SYSCALL.md §3, read's row;
/// ROADMAP §10.12).
#[cfg(target_arch = "x86_64")]
pub(crate) fn test_dev_random_eagain() -> Outcome {
    entropy_init::testing::set_dry(true);
    let st = user::run(&Image::Code(DEV_RANDOM_READ, DEFAULT), &["random_eagain"]);
    entropy_init::testing::set_dry(false);
    match st {
        Ok(st) if st == wait_exited(11) => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("status {st:#x}, want exited 11 (EAGAIN)"),
        Err(e) => crate::fail_fmt!("spawn: {}", e.as_str()),
    }
}

#[cfg(target_arch = "x86_64")]
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
        let rng = d.vendor == 0x1af4 && (d.device_id == 0x1044 || d.device_id == 0x1005);
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
