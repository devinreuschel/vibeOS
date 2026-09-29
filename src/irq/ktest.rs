//! In-guest tests for irq (kernel_tests only). Rows: [`TESTS`].

use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::apic::{Polarity, Trigger};
use vibeos::ipi::MAX_IPI_CPUS;
use vibeos::irq::{self, IrqError};
use vibeos::kalloc::TryVec;
use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::pci::{Bdf, CFG_COMMAND, CMD_INTX_DISABLE};
use vibeos::thread::ThreadState;
use vibeos::vectors;

use crate::apic_init;
use crate::ipi_init;
use crate::irq_init;
use crate::ktest::{
    EDU_IDENT, EDU_IDENT_VAL, Outcome, Test, alloc_frames_owned, bar0_va, cpu_remote, find_edu,
    free_frames_owned, mmio_r32, mmio_w32, quiescent_free_frames, second_cpu, spawn_thread_on,
    spin_until_ns, test,
};
use crate::kva_init;
use crate::pci_init;
use crate::per_cpu_init;
use crate::sync::blocking_init::Semaphore;
use crate::thread_init;
use crate::time_init;
use crate::x86;

static WAKE_FLAG: AtomicU64 = AtomicU64::new(0);

fn wake_ap_entry() {
    WAKE_FLAG.store(1, Ordering::SeqCst);
}

pub(crate) fn test_reschedule_ipi_wake_ap() -> Outcome {
    let Some(ap) = second_cpu() else {
        return Outcome::Skip("no AP");
    };
    WAKE_FLAG.store(0, Ordering::SeqCst);
    let before = reschedule_count();
    let Ok(_h) = thread_init::spawn_on("wake-ap", wake_ap_entry, ap) else {
        return Outcome::Fail("spawn");
    };
    if !spin_until_ns(|| WAKE_FLAG.load(Ordering::SeqCst) != 0, 500_000_000) {
        return Outcome::Fail("idle AP not woken");
    }
    let after = reschedule_count();
    if after <= before {
        return Outcome::Fail("no reschedule IPI");
    }
    Outcome::Ok
}

static CALL_CPU: AtomicU32 = AtomicU32::new(0xFFFF);

fn call_mark(arg: *mut ()) {
    let _ = arg;
    CALL_CPU.store(per_cpu_init::current().cpu_id, Ordering::SeqCst);
}

pub(crate) fn test_call_function_ipi() -> Outcome {
    let Some(ap) = second_cpu() else {
        return Outcome::Skip("no AP");
    };
    CALL_CPU.store(0xFFFF, Ordering::SeqCst);
    let before = call_count();
    ipi_init::call_cpu(ap, call_mark, core::ptr::null_mut(), true);
    if CALL_CPU.load(Ordering::SeqCst) != ap {
        return Outcome::Fail("call-function did not run on AP");
    }
    if call_count() <= before {
        return Outcome::Fail("call count stuck");
    }
    Outcome::Ok
}

const E1000_ICR: u32 = 0xC0;

const E1000_ICS: u32 = 0xC8;

const E1000_IMS: u32 = 0xD0;

const E1000_IMC: u32 = 0xD8;

const E1000_IVAR: u32 = 0xE4;

const E1000_ICR_LSC: u32 = 1 << 2;

const E1000_ICR_OTHER: u32 = 1 << 24;

/// Other -> MSI-X table entry 0, valid.
const E1000_IVAR_OTHER0: u32 = 0x8 << 16;

const EDU_IRQSTAT: u32 = 0x24;

const EDU_RAISE: u32 = 0x60;

const EDU_ACK: u32 = 0x64;

static IRQ_CPU: AtomicU32 = AtomicU32::new(0xFFFF);

static IRQ_HITS: AtomicU32 = AtomicU32::new(0);

static IRQ_ALLOC: AtomicU32 = AtomicU32::new(0);

static IRQ_MMIO: AtomicU64 = AtomicU64::new(0);

static CPU_HITS: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];

/// Zero the observer. `IRQ_HITS` goes last, as `record_irq_cpu` publishes
/// it last (AGENTS rule 5).
fn reset_irq_obs() {
    IRQ_CPU.store(0xFFFF, Ordering::SeqCst);
    IRQ_ALLOC.store(0, Ordering::SeqCst);
    let mut i = 0usize;
    while i < CPU_HITS.len() {
        CPU_HITS[i].store(0, Ordering::SeqCst);
        i += 1;
    }
    IRQ_HITS.store(0, Ordering::SeqCst);
}

/// `time_init::now_ns` value [`obs_stall`] spins until; 0 is disarmed.
static OBS_STALL_NS: AtomicU64 = AtomicU64::new(0);

/// Stall [`record_irq_cpu`] before its last store while
/// `msix_cpu_publish_last` has the stall armed.
fn obs_stall() {
    let until = OBS_STALL_NS.load(Ordering::Acquire);
    if until == 0 {
        return;
    }
    while time_init::now_ns() < until {
        core::hint::spin_loop();
    }
}

/// Count a hit on this CPU. `IRQ_HITS`, which tests wait on, is published
/// last, so a waiter that sees it also sees `CPU_HITS` and `IRQ_CPU`
/// (AGENTS rule 5, F021).
fn record_irq_cpu() {
    let cpu = per_cpu_init::current().cpu_id;
    if (cpu as usize) < CPU_HITS.len() {
        CPU_HITS[cpu as usize].fetch_add(1, Ordering::SeqCst);
    }
    IRQ_CPU.store(cpu, Ordering::SeqCst);
    obs_stall();
    IRQ_HITS.fetch_add(1, Ordering::SeqCst);
}

fn on_msix() {
    match irq_init::allocate_vector(0) {
        Err(IrqError::InIrq) => IRQ_ALLOC.store(1, Ordering::SeqCst),
        Ok(_) => IRQ_ALLOC.store(2, Ordering::SeqCst),
        Err(_) => IRQ_ALLOC.store(3, Ordering::SeqCst),
    }
    let va = IRQ_MMIO.load(Ordering::SeqCst);
    if va != 0 {
        mmio_w32(va, E1000_ICR, 0xFFFF_FFFF);
    }
    // Last: `IRQ_HITS` is what `test_msix_cpu` waits on, so every store it
    // then reads (IRQ_ALLOC) comes before it (publish last, F021).
    record_irq_cpu();
}

fn on_intx() {
    let va = IRQ_MMIO.load(Ordering::SeqCst);
    if va != 0 {
        let st = mmio_r32(va, EDU_IRQSTAT);
        if st != 0 {
            mmio_w32(va, EDU_ACK, st);
        }
    }
    record_irq_cpu();
}

fn on_intx_no_ack() {
    record_irq_cpu();
}

pub(crate) fn test_irq_pool() -> Outcome {
    let n0 = allocated_count();
    let v = match irq_init::allocate_vector(0) {
        Ok(v) => v,
        Err(e) => return Outcome::Fail(e.as_str()),
    };
    if !irq::in_pool(v) || v == vectors::KBD {
        let _ = irq_init::free_vector(v);
        return Outcome::Fail("out of pool");
    }
    if irq_init::cpu_of(v) != Some(0) {
        let _ = irq_init::free_vector(v);
        return Outcome::Fail("cpu_of");
    }
    if irq_init::set_affinity(v, 0).is_err() {
        let _ = irq_init::free_vector(v);
        return Outcome::Fail("affinity");
    }
    match irq_init::free_vector(v) {
        Ok(()) => {
            if allocated_count() != n0 {
                Outcome::Fail("count")
            } else {
                Outcome::Ok
            }
        }
        Err(e) => Outcome::Fail(e.as_str()),
    }
}

fn irq_th_nop(_ctx: Option<&(dyn core::any::Any + Send + Sync)>) {}

fn irq_nop() {}

pub(crate) fn test_irq_free_threaded() -> Outcome {
    let n0 = allocated_count();
    let v = match irq_init::allocate_vector(0) {
        Ok(v) => v,
        Err(e) => return Outcome::Fail(e.as_str()),
    };
    if irq_init::set_threaded(v, Some(irq_th_nop), irq_th_nop, None).is_err() {
        let _ = irq_init::free_vector(v);
        return Outcome::Fail("set_threaded");
    }
    if !has_threaded(v) {
        let _ = irq_init::free_vector(v);
        return Outcome::Fail("threaded not armed");
    }
    if irq_init::free_vector(v).is_err() {
        return Outcome::Fail("free");
    }
    if has_threaded(v) {
        return Outcome::Fail("threaded after free");
    }
    let v2 = match irq_init::allocate_vector(0) {
        Ok(v) => v,
        Err(e) => return Outcome::Fail(e.as_str()),
    };
    if v2 != v {
        let _ = irq_init::free_vector(v2);
        return Outcome::Fail("realloc other vec");
    }
    if has_threaded(v2) {
        let _ = irq_init::free_vector(v2);
        return Outcome::Fail("recycle threaded");
    }
    if irq_init::set_handler(v2, irq_nop).is_err() {
        let _ = irq_init::free_vector(v2);
        return Outcome::Fail("set_handler");
    }
    if has_threaded(v2) {
        let _ = irq_init::free_vector(v2);
        return Outcome::Fail("handler still threaded");
    }
    if irq_init::free_vector(v2).is_err() {
        return Outcome::Fail("free2");
    }
    if allocated_count() != n0 {
        return Outcome::Fail("count");
    }
    Outcome::Ok
}

pub(crate) fn test_msix_cpu() -> Outcome {
    let Some(ap) = second_cpu() else {
        return Outcome::Skip("no AP");
    };
    let Some(dev) = crate::dev::ktest::find_id(0x8086, 0x10d3) else {
        return Outcome::Skip("no e1000e");
    };
    if dev.caps.msix.is_none() {
        return Outcome::Fail("no msix cap");
    }
    let Some(mmio) = bar0_va(&dev) else {
        return Outcome::Fail("e1000e bar0");
    };
    let Some(cpu) = cpu_remote(ap) else {
        return Outcome::Fail("no apic id");
    };
    let vec = match irq_init::allocate_vector(0) {
        Ok(v) => v,
        Err(e) => return Outcome::Fail(e.as_str()),
    };
    if irq_init::set_handler(vec, on_msix).is_err() {
        let _ = irq_init::free_vector(vec);
        return Outcome::Fail("handler");
    }
    if irq_init::set_affinity(vec, ap).is_err() {
        let _ = irq_init::free_vector(vec);
        return Outcome::Fail("affinity");
    }
    if irq_init::cpu_of(vec) != Some(ap) {
        let _ = irq_init::free_vector(vec);
        return Outcome::Fail("cpu_of ap");
    }
    reset_irq_obs();
    IRQ_MMIO.store(mmio, Ordering::SeqCst);
    if let Err(e) = irq_init::enable_msix(&dev, 0, vec, cpu.apic_id.load(Ordering::Relaxed) as u8) {
        let _ = irq_init::free_vector(vec);
        return Outcome::Fail(e.as_str());
    }
    let cmd = pci_init::cfg_read16(dev.addr, CFG_COMMAND);
    if cmd & CMD_INTX_DISABLE == 0 {
        irq_init::disable_msix(&dev);
        let _ = irq_init::free_vector(vec);
        return Outcome::Fail("intx live");
    }
    mmio_w32(mmio, E1000_IMC, 0xFFFF_FFFF);
    mmio_w32(mmio, E1000_IVAR, E1000_IVAR_OTHER0);
    mmio_w32(mmio, E1000_IMS, E1000_ICR_LSC | E1000_ICR_OTHER);
    mmio_w32(mmio, E1000_ICS, E1000_ICR_LSC);
    let fired = spin_until_ns(|| IRQ_HITS.load(Ordering::SeqCst) != 0, 500_000_000);
    mmio_w32(mmio, E1000_IMC, 0xFFFF_FFFF);
    irq_init::disable_msix(&dev);
    let _ = irq_init::free_vector(vec);
    IRQ_MMIO.store(0, Ordering::SeqCst);
    if !fired {
        return Outcome::Fail("no msix");
    }
    if IRQ_CPU.load(Ordering::SeqCst) != ap {
        return Outcome::Fail("wrong cpu");
    }
    if CPU_HITS[ap as usize].load(Ordering::SeqCst) == 0 {
        return Outcome::Fail("ap counter");
    }
    if IRQ_ALLOC.load(Ordering::SeqCst) != 1 {
        return Outcome::Fail("alloc in irq");
    }
    Outcome::Ok
}

pub(crate) fn test_intx_fallback() -> Outcome {
    let Some(ap) = second_cpu() else {
        return Outcome::Skip("no AP");
    };
    let Some(dev) = find_edu() else {
        return Outcome::Skip("no edu");
    };
    if dev.irq.pin == 0 {
        return Outcome::Fail("no pin");
    }
    let line = dev.irq.line;
    if line == 0 || line == 0xFF {
        return Outcome::Fail("irq line");
    }
    let Some(mmio) = bar0_va(&dev) else {
        return Outcome::Fail("edu bar0");
    };
    if mmio_r32(mmio, EDU_IDENT) != EDU_IDENT_VAL {
        return Outcome::Fail("edu ident");
    }
    let vec = match irq_init::allocate_vector(0) {
        Ok(v) => v,
        Err(e) => return Outcome::Fail(e.as_str()),
    };
    if irq_init::set_handler(vec, on_intx).is_err() {
        let _ = irq_init::free_vector(vec);
        return Outcome::Fail("handler");
    }
    let gsi = line as u32;
    irq_init::mask_intx(dev.addr, false);
    if irq_init::route_intx(gsi, vec, 0, Trigger::Level, Polarity::Low).is_err() {
        let _ = irq_init::free_vector(vec);
        return Outcome::Fail("route");
    }
    if irq_init::set_affinity(vec, ap).is_err() {
        apic_init::mask_gsi(gsi);
        let _ = irq_init::free_vector(vec);
        return Outcome::Fail("affinity");
    }
    reset_irq_obs();
    IRQ_MMIO.store(mmio, Ordering::SeqCst);
    mmio_w32(mmio, EDU_RAISE, 1);
    let fired = spin_until_ns(|| IRQ_HITS.load(Ordering::SeqCst) != 0, 500_000_000);
    let st = mmio_r32(mmio, EDU_IRQSTAT);
    if st != 0 {
        mmio_w32(mmio, EDU_ACK, st);
    }
    apic_init::mask_gsi(gsi);
    irq_init::mask_intx(dev.addr, true);
    let _ = irq_init::free_vector(vec);
    IRQ_MMIO.store(0, Ordering::SeqCst);
    if !fired {
        return Outcome::Fail("no intx");
    }
    if IRQ_CPU.load(Ordering::SeqCst) != ap {
        return Outcome::Fail("wrong cpu");
    }
    Outcome::Ok
}

fn edu_intx_teardown(bdf: Bdf, gsi: u32, vec: Option<u8>, mmio: u64) {
    let st = mmio_r32(mmio, EDU_IRQSTAT);
    if st != 0 {
        mmio_w32(mmio, EDU_ACK, st);
    }
    apic_init::mask_gsi(gsi);
    irq_init::mask_intx(bdf, true);
    if let Some(v) = vec {
        let _ = irq_init::free_vector(v);
    }
    IRQ_MMIO.store(0, Ordering::SeqCst);
}

pub(crate) fn test_intx_free_masks() -> Outcome {
    let Some(ap) = second_cpu() else {
        return Outcome::Skip("no AP");
    };
    let Some(dev) = find_edu() else {
        return Outcome::Skip("no edu");
    };
    if dev.irq.pin == 0 {
        return Outcome::Fail("no pin");
    }
    let line = dev.irq.line;
    if line == 0 || line == 0xFF {
        return Outcome::Fail("irq line");
    }
    let Some(mmio) = bar0_va(&dev) else {
        return Outcome::Fail("edu bar0");
    };
    if mmio_r32(mmio, EDU_IDENT) != EDU_IDENT_VAL {
        return Outcome::Fail("edu ident");
    }
    let vec = match irq_init::allocate_vector(0) {
        Ok(v) => v,
        Err(e) => return Outcome::Fail(e.as_str()),
    };
    let gsi = line as u32;
    let fail = |why, live: Option<u8>| {
        edu_intx_teardown(dev.addr, gsi, live, mmio);
        Outcome::Fail(why)
    };
    if irq_init::set_handler(vec, on_intx_no_ack).is_err() {
        return fail("handler", Some(vec));
    }
    irq_init::mask_intx(dev.addr, false);
    if irq_init::route_intx(gsi, vec, 0, Trigger::Level, Polarity::Low).is_err() {
        return fail("route", Some(vec));
    }
    if irq_init::set_affinity(vec, ap).is_err() {
        return fail("affinity", Some(vec));
    }
    match crate::arch::ktest::gsi_masked(gsi) {
        Some(false) => {}
        Some(true) => return fail("masked before free", Some(vec)),
        None => return fail("gsi not on ioapic", Some(vec)),
    }
    reset_irq_obs();
    IRQ_MMIO.store(mmio, Ordering::SeqCst);
    mmio_w32(mmio, EDU_RAISE, 1);
    if !spin_until_ns(|| IRQ_HITS.load(Ordering::SeqCst) != 0, 500_000_000) {
        return fail("no intx", Some(vec));
    }
    // Line stays asserted (no ack). Free must mask before dropping the route.
    if irq_init::free_vector(vec).is_err() {
        return fail("free", Some(vec));
    }
    match crate::arch::ktest::gsi_masked(gsi) {
        Some(true) => {}
        Some(false) => return fail("gsi live after free", None),
        None => return fail("gsi vanished", None),
    }
    let after_free = IRQ_HITS.load(Ordering::SeqCst);
    time_init::busy_wait_ms(20);
    if IRQ_HITS.load(Ordering::SeqCst).saturating_sub(after_free) > 8 {
        return fail("storm after free", None);
    }
    let st = mmio_r32(mmio, EDU_IRQSTAT);
    if st != 0 {
        mmio_w32(mmio, EDU_ACK, st);
    }
    let vec2 = match irq_init::allocate_vector(0) {
        Ok(v) => v,
        Err(e) => return fail(e.as_str(), None),
    };
    if vec2 != vec {
        return fail("realloc other vec", Some(vec2));
    }
    if irq_init::set_handler(vec2, on_intx).is_err() {
        return fail("handler2", Some(vec2));
    }
    reset_irq_obs();
    mmio_w32(mmio, EDU_RAISE, 1);
    if spin_until_ns(|| IRQ_HITS.load(Ordering::SeqCst) != 0, 50_000_000) {
        return fail("delivery after free", Some(vec2));
    }
    if irq_init::route_intx(gsi, vec2, ap, Trigger::Level, Polarity::Low).is_err() {
        return fail("reroute", Some(vec2));
    }
    let fired2 = spin_until_ns(|| IRQ_HITS.load(Ordering::SeqCst) != 0, 500_000_000);
    edu_intx_teardown(dev.addr, gsi, Some(vec2), mmio);
    if !fired2 {
        return Outcome::Fail("no intx after reroute");
    }
    Outcome::Ok
}

/// How long `msix_cpu_publish_last` stalls `record_irq_cpu`.
const OBS_STALL_FOR_NS: u64 = 20_000_000;

/// Bound on each of `msix_cpu_publish_last`'s waits.
const OBS_WAIT_NS: u64 = 2_000_000_000;

static OBS_DONE: AtomicBool = AtomicBool::new(false);

fn obs_publisher() {
    record_irq_cpu();
    OBS_DONE.store(true, Ordering::Release);
}

/// `record_irq_cpu` publishes `IRQ_HITS` last: once a waiter sees a hit, the
/// hitting CPU's `CPU_HITS` count is already there, even when the observer
/// stalls before its last store (ROADMAP §10.2, F021).
pub(crate) fn msix_cpu_publish_last() -> Outcome {
    let Some(ap) = crate::ktest::second_cpu() else {
        return Outcome::Skip("no AP");
    };
    if ap as usize >= CPU_HITS.len() {
        return Outcome::Fail("ap index");
    }
    OBS_STALL_NS.store(
        time_init::now_ns().saturating_add(OBS_STALL_FOR_NS),
        Ordering::Release,
    );
    reset_irq_obs();
    OBS_DONE.store(false, Ordering::Relaxed);
    if thread_init::spawn_opts(
        "obs-pub",
        obs_publisher,
        thread_init::SpawnOpts {
            stack_pages: DEFAULT_STACK_PAGES,
            cpu: Some(ap),
        },
    )
    .is_err()
    {
        OBS_STALL_NS.store(0, Ordering::Release);
        return Outcome::Fail("spawn");
    }
    let hit = crate::ktest::spin_until_ns(|| IRQ_HITS.load(Ordering::SeqCst) != 0, OBS_WAIT_NS);
    let ap_hits = CPU_HITS[ap as usize].load(Ordering::SeqCst);
    let cpu = IRQ_CPU.load(Ordering::SeqCst);
    let done = crate::ktest::spin_until_ns(|| OBS_DONE.load(Ordering::Acquire), OBS_WAIT_NS);
    OBS_STALL_NS.store(0, Ordering::Release);
    if !hit || !done {
        return Outcome::Fail("publisher did not run");
    }
    if cpu != ap {
        return Outcome::Fail("wrong cpu");
    }
    if ap_hits == 0 {
        return Outcome::Fail("ap counter");
    }
    Outcome::Ok
}

// ---------------------------------------------------------------------------
// lifetime_shootdown_ack_late (ROADMAP §10.10, F011)

/// How long the holder keeps IF off, in ms of TSC time.
const HOLD_MS: u64 = 3_000;

/// 0: not started; 1: IF off and spinning; 2: IF back on.
static HOLD: AtomicU32 = AtomicU32::new(0);

/// Hold IF off for [`HOLD_MS`] without polling `service_incoming`, so this
/// CPU acks no shootdown until it lets the pending `0xFC` in.
fn ack_hold() {
    let k = time_init::tsc_per_ms();
    let g = x86::InterruptGuard::enter();
    HOLD.store(1, Ordering::Release);
    let t0 = time_init::read_tsc();
    let span = k.saturating_mul(HOLD_MS);
    while time_init::read_tsc().wrapping_sub(t0) < span {
        spin_loop();
    }
    drop(g);
    HOLD.store(2, Ordering::Release);
}

fn wait_ms(pred: impl Fn() -> bool, ms: u64) -> bool {
    let t0 = time_init::now_ns();
    while !pred() {
        if time_init::now_ns().saturating_sub(t0) > ms.saturating_mul(1_000_000) {
            return false;
        }
        thread_init::yield_now();
    }
    true
}

/// ROADMAP §10.10 (F011): one CPU holds IF off for 3 s while another
/// unmaps a KVA range; `wait_acks` waits for it without panicking, logs it
/// late once a second, and both finish.
pub(crate) fn lifetime_shootdown_ack_late() -> Outcome {
    let k = time_init::tsc_per_ms();
    if k == 0 {
        return Outcome::Skip("no TSC");
    }
    let mask = per_cpu_init::online_mask();
    if mask.count_ones() < 2 {
        return Outcome::Skip("one CPU");
    }
    let me = per_cpu_init::current().cpu_id;
    let others = mask & !(1u64 << me);
    // Prefer an AP, so the BSP's tick keeps running.
    let pick = if others & !1 != 0 {
        others & !1
    } else {
        others
    };
    let h = pick.trailing_zeros();
    let Ok(stack) = kva_init::alloc_guarded_stack(4) else {
        return Outcome::Fail("alloc_guarded_stack");
    };
    // A dead stack this CPU's worker frees would wait out the hold in
    // `wait_acks` too, and the registry would resume after it; start with
    // none on its way back (ROADMAP §10.10's owner-CPU reclaim).
    if !crate::ktest::settle_threads() {
        kva_init::free_stack(stack);
        return Outcome::Fail("threads did not settle");
    }
    let late0 = ack_late_count();
    HOLD.store(0, Ordering::Release);
    let th = spawn_thread_on("ack-hold", ack_hold, h);
    if !wait_ms(|| HOLD.load(Ordering::Acquire) != 0, 2_000) {
        kva_init::free_stack(stack);
        return Outcome::Fail("holder did not start");
    }
    let t0 = time_init::read_tsc();
    kva_init::free_stack(stack);
    let waited_ms = time_init::read_tsc().wrapping_sub(t0) / k;
    let late = ack_late_count().wrapping_sub(late0);
    if !wait_ms(
        || {
            HOLD.load(Ordering::Acquire) == 2
                && matches!(
                    thread_init::try_state(th.id()),
                    None | Some(ThreadState::Dead)
                )
        },
        5_000,
    ) {
        return Outcome::Fail("holder did not finish");
    }
    if waited_ms < 2_000 {
        return crate::fail_fmt!(
            "unmap did not wait for the IF-off CPU: {} ms on cpu{}",
            waited_ms,
            h
        );
    }
    if !(1..=3).contains(&late) {
        return crate::fail_fmt!("{} late lines in {} ms, want 1..=3", late, waited_ms);
    }
    Outcome::Ok
}

// ---------------------------------------------------------------------------
// shootdown_ack_while_busy (ROADMAP §10.2, F011, F075)

/// How long the registry stays busy, in ns of `now_ns` time.
const BUSY_NS: u64 = 2_000_000_000;

/// A shootdown cycle this long is the traced `ipi: ack timeout` shape.
const SLOW_NS: u64 = 1_000_000_000;

/// Rounds one shooter runs at most.
const MAX_ROUNDS: u32 = 10_000;

/// Set while the registry dumps the log ring and spins.
static BUSY: AtomicBool = AtomicBool::new(false);

/// Set when the shooters are to stop.
static STOP: AtomicBool = AtomicBool::new(false);

/// Shooters that have started.
static STARTED: AtomicU32 = AtomicU32::new(0);

/// Shooters that have finished.
static DONE: AtomicU32 = AtomicU32::new(0);

/// Shooters that finished a round while `BUSY` was still set.
static OVERLAP: AtomicU32 = AtomicU32::new(0);

/// Shooters that stopped on an allocation or `vmap` error.
static ERRS: AtomicU32 = AtomicU32::new(0);

/// Longest `vmap` plus `vunmap` round any shooter saw, in ns.
static MAX_NS: AtomicU64 = AtomicU64::new(0);

/// One CPU's shooter: `vmap` and `vunmap` one page per round, each of which
/// shoots it down on every other CPU and waits for their acks.
fn shooter() {
    STARTED.fetch_add(1, Ordering::AcqRel);
    let mut overlapped = false;
    let mut rounds = 0u32;
    while !STOP.load(Ordering::Acquire) && rounds < MAX_ROUNDS {
        rounds += 1;
        let Some(f) = alloc_frames_owned(0) else {
            ERRS.fetch_add(1, Ordering::AcqRel);
            break;
        };
        let t0 = time_init::now_ns();
        // A failed `vmap` has already freed its frames (`kva_init::vmap`).
        let Ok(v) = kva_init::vmap(f) else {
            ERRS.fetch_add(1, Ordering::AcqRel);
            break;
        };
        let f = kva_init::vunmap(v);
        let took = time_init::now_ns().saturating_sub(t0);
        free_frames_owned(f);
        MAX_NS.fetch_max(took, Ordering::AcqRel);
        if !overlapped && BUSY.load(Ordering::Acquire) {
            overlapped = true;
            OVERLAP.fetch_add(1, Ordering::AcqRel);
        }
    }
    DONE.fetch_add(1, Ordering::AcqRel);
}

/// ROADMAP §10.2 (F011, F075): the traced `-smp 4` `ipi: ack timeout`.
/// The registry dumps the whole log ring to the console and then spins
/// CPU-bound for 2 s, at the IF it runs at and without polling
/// `service_incoming`, while every other online CPU loops `vmap`/`vunmap`
/// shootdowns. No shootdown cycle may take 1 s.
pub(crate) fn shootdown_ack_while_busy() -> Outcome {
    let me = per_cpu_init::current().cpu_id;
    let others = per_cpu_init::online_mask() & !(1u64 << me);
    if others == 0 {
        return Outcome::Skip("no AP");
    }
    let shooters = others.count_ones();
    BUSY.store(false, Ordering::Release);
    STOP.store(false, Ordering::Release);
    STARTED.store(0, Ordering::Release);
    DONE.store(0, Ordering::Release);
    OVERLAP.store(0, Ordering::Release);
    ERRS.store(0, Ordering::Release);
    MAX_NS.store(0, Ordering::Release);

    BUSY.store(true, Ordering::Release);
    let mut spawned = 0u32;
    let mut mask = others;
    while mask != 0 {
        let cpu = mask.trailing_zeros();
        mask &= mask - 1;
        if thread_init::spawn_on("s23-shooter", shooter, cpu).is_err() {
            break;
        }
        spawned += 1;
    }
    if spawned != shooters {
        BUSY.store(false, Ordering::Release);
        STOP.store(true, Ordering::Release);
        let _ = spin_until_ns(|| DONE.load(Ordering::Acquire) == spawned, 5_000_000_000);
        return crate::fail_fmt!("spawn_on failed after {} of {} shooters", spawned, shooters);
    }
    let started = spin_until_ns(|| STARTED.load(Ordering::Acquire) == shooters, 500_000_000);

    // The traced busy stretch: a full dmesg replay through the console,
    // then CPU-bound work that never polls for IPIs. No guard is held.
    let if_on = x86::interrupts_enabled();
    let t0 = time_init::now_ns();
    let dumped = crate::shell_init::dispatch_line("dmesg trace");
    while time_init::now_ns().saturating_sub(t0) < BUSY_NS {
        spin_loop();
    }
    BUSY.store(false, Ordering::Release);
    STOP.store(true, Ordering::Release);

    let finished = spin_until_ns(|| DONE.load(Ordering::Acquire) == shooters, 5_000_000_000);
    if !finished {
        return Outcome::Fail("shooters unfinished");
    }
    if !started {
        return Outcome::Fail("shooters did not start");
    }
    if dumped.is_err() {
        return Outcome::Fail("dmesg trace");
    }
    if ERRS.load(Ordering::Acquire) > 0 {
        return crate::fail_fmt!("{} shooters hit a vmap error", ERRS.load(Ordering::Acquire));
    }
    let max = MAX_NS.load(Ordering::Acquire);
    if max >= SLOW_NS {
        return crate::fail_fmt!(
            "shootdown waited {} ms, IF {} on cpu{}",
            max / 1_000_000,
            if if_on { "on" } else { "off" },
            me
        );
    }
    if OVERLAP.load(Ordering::Acquire) < shooters {
        return Outcome::Fail("shooter idle during busy window");
    }
    if !crate::ktest::settle_threads() {
        return Outcome::Fail("shooters did not settle");
    }
    Outcome::Ok
}

/// `sched_init::init` installed the reschedule IPI's hook
/// (`ipi_init::set_reschedule_hook`), so a reschedule IPI preempts.
pub(crate) fn test_reschedule_hook_installed() -> Outcome {
    if !reschedule_hook_installed() {
        return Outcome::Fail("reschedule hook unset");
    }
    Outcome::Ok
}

// ---- hooks the tests read (Q2): counters production keeps, and the
// switch-tail shootdown count P10-S08's test uses ----

/// Reschedule IPIs this kernel has taken since boot.
pub(crate) fn reschedule_count() -> u64 {
    ipi_init::RESCHED_COUNT.load(Ordering::Relaxed)
}

/// Shootdown requests this kernel has serviced since boot.
pub(crate) fn shootdown_count() -> u64 {
    ipi_init::SHOOT_COUNT.load(Ordering::Relaxed)
}

/// Call-function requests this kernel has serviced since boot.
pub(crate) fn call_count() -> u64 {
    ipi_init::CALL_COUNT.load(Ordering::Relaxed)
}

/// Late periods `ipi_init::wait_acks` has logged since boot.
pub(crate) fn ack_late_count() -> u64 {
    ipi_init::ACK_LATE.load(Ordering::Relaxed)
}

/// Whether the reschedule hook is set.
pub(crate) fn reschedule_hook_installed() -> bool {
    !ipi_init::RESCHED.load(Ordering::Acquire).is_null()
}

/// Vectors allocated from the device pool.
pub(crate) fn allocated_count() -> usize {
    irq_init::with_pool(|p| p.allocated())
}

/// Whether `vec` has a threaded handler or pending threaded work.
pub(crate) fn has_threaded(vec: u8) -> bool {
    match irq_init::handler_slot(vec) {
        Some(i) => irq_init::with_irq(|s| {
            let t = &s.th;
            t.top[i].is_some() || t.work[i].is_some() || t.ctx[i].is_some() || t.pending[i]
        }),
        None => false,
    }
}

/// Per CPU: inside a switch tail. Owner CPU only, IF=0.
static IN_TAIL: [AtomicBool; MAX_IPI_CPUS] = [const { AtomicBool::new(false) }; MAX_IPI_CPUS];
/// Shootdowns started while their CPU's `IN_TAIL` was set.
static FROM_TAIL: AtomicU64 = AtomicU64::new(0);
/// Shootdown rounds started by any CPU: with another CPU online, each is
/// one IPI broadcast and one wait for acks.
static ROUNDS: AtomicU64 = AtomicU64::new(0);

/// This CPU enters a switch tail. IF=0.
pub(crate) fn tail_enter() {
    IN_TAIL[ipi_init::my_index()].store(true, Ordering::Relaxed);
}

/// This CPU leaves its switch tail. IF=0.
pub(crate) fn tail_leave() {
    IN_TAIL[ipi_init::my_index()].store(false, Ordering::Relaxed);
}

/// Shootdowns sent from a switch tail since boot.
pub(crate) fn shootdowns_from_tail() -> u64 {
    FROM_TAIL.load(Ordering::Acquire)
}

/// Shootdown rounds started since boot.
pub(crate) fn rounds_sent() -> u64 {
    ROUNDS.load(Ordering::Acquire)
}

/// `ipi_init::shootdown_round`'s count, IF=0.
pub(crate) fn note_shootdown() {
    ROUNDS.fetch_add(1, Ordering::AcqRel);
    if IN_TAIL[ipi_init::my_index()].load(Ordering::Relaxed) {
        FROM_TAIL.fetch_add(1, Ordering::AcqRel);
    }
}

// ---- wake_inbox_and_kva_pool (ROADMAP §10.4, D1) ----

/// Guarded stacks the KVA half frees out of order: past the old 128-node
/// free-list pool.
const INBOX_KVA_STACKS: usize = 300;

static INBOX_SEM: Semaphore = Semaphore::new(0);
/// Set once the target is about to block on [`INBOX_SEM`].
static INBOX_BLOCKING: AtomicBool = AtomicBool::new(false);
/// The tid the target ran with after its wake, `u32::MAX` until then.
static INBOX_RAN: AtomicU32 = AtomicU32::new(u32::MAX);

fn inbox_target() {
    INBOX_BLOCKING.store(true, Ordering::SeqCst);
    INBOX_SEM.acquire();
    INBOX_RAN.store(thread_init::current_id().raw(), Ordering::SeqCst);
}

fn inbox_throwaway() {}

/// Free more than 128 non-adjacent guarded stacks, odd ones last first and
/// then the even ones; KVA `used` and the free frames come back.
fn kva_pool_frees_out_of_order() -> Outcome {
    let before = quiescent_free_frames();
    let tables0 = crate::mm::ktest::table_pages();
    let used0 = kva_init::stats().used;
    let Ok(mut stacks) =
        TryVec::<Option<kva_init::GuardedStack>>::try_with_capacity(INBOX_KVA_STACKS)
    else {
        return Outcome::Fail("no memory for the stack list");
    };
    for i in 0..INBOX_KVA_STACKS {
        let s = match kva_init::alloc_guarded_stack(1) {
            Ok(s) => s,
            Err(e) => {
                for s in stacks.iter_mut().filter_map(Option::take) {
                    kva_init::free_stack(s);
                }
                return crate::fail_fmt!("stack {i}: {}", e.as_str());
            }
        };
        if stacks.try_push(Some(s)).is_err() {
            return Outcome::Fail("stack list push");
        }
    }
    for i in (1..INBOX_KVA_STACKS).step_by(2).rev() {
        if let Some(s) = stacks.get_mut(i).and_then(Option::take) {
            kva_init::free_stack(s);
        }
    }
    for i in (0..INBOX_KVA_STACKS).step_by(2) {
        if let Some(s) = stacks.get_mut(i).and_then(Option::take) {
            kva_init::free_stack(s);
        }
    }
    drop(stacks);
    let used1 = kva_init::stats().used;
    if used1 != used0 {
        return crate::fail_fmt!("kva used {used0} -> {used1}");
    }
    // A stack that reached VA no mapping reached before took a page-table
    // page, which stays in the kernel tables.
    let after = quiescent_free_frames();
    let tables = crate::mm::ktest::table_pages().saturating_sub(tables0);
    if after.saturating_add(tables) != before {
        return crate::fail_fmt!("frames {before} -> {after} (+{tables} table pages)");
    }
    Outcome::Ok
}

/// Spawn and reap throwaway threads on this CPU until one's tid is 64 or
/// more, so the next spawn's is too. False if none gets there.
fn burn_tids_past_64() -> bool {
    for _ in 0..4 * vibeos::thread::MAX_THREADS {
        let Ok(h) = thread_init::spawn_here("tid-burn", inbox_throwaway) else {
            return false;
        };
        if !spin_until_ns(
            || {
                thread_init::yield_now();
                thread_init::exited(h.id())
            },
            500_000_000,
        ) {
            return false;
        }
        if h.id().raw() >= 64 {
            return true;
        }
    }
    false
}

/// The cross-CPU wake inbox holds any thread-table slot and the KVA node
/// pool frees more than 128 ranges out of order (ROADMAP §10.4, D1): a
/// thread whose tid is 64 or more, blocked on another CPU, is woken from
/// this one through that CPU's inbox and a reschedule IPI.
pub(crate) fn wake_inbox_and_kva_pool() -> Outcome {
    let kva = kva_pool_frees_out_of_order();
    if !matches!(kva, Outcome::Ok) {
        return kva;
    }
    let Some(ap) = second_cpu() else {
        return Outcome::Skip("no AP");
    };
    if !burn_tids_past_64() {
        return Outcome::Fail("no tid reached 64");
    }
    INBOX_BLOCKING.store(false, Ordering::SeqCst);
    INBOX_RAN.store(u32::MAX, Ordering::SeqCst);
    let Ok(h) = thread_init::spawn_on("inbox-target", inbox_target, ap) else {
        return Outcome::Fail("spawn target");
    };
    let blocked = spin_until_ns(
        || {
            INBOX_BLOCKING.load(Ordering::SeqCst)
                && matches!(
                    thread_init::try_state(h.id()),
                    Some(ThreadState::Blocked { .. })
                )
        },
        500_000_000,
    );
    let before = reschedule_count();
    INBOX_SEM.release();
    if !blocked {
        return Outcome::Fail("target did not block");
    }
    if !spin_until_ns(|| INBOX_RAN.load(Ordering::SeqCst) != u32::MAX, 500_000_000) {
        return Outcome::Fail("target not woken within 500 ms");
    }
    let tid = INBOX_RAN.load(Ordering::SeqCst);
    if tid != h.id().raw() || tid < 64 {
        return crate::fail_fmt!("target ran as tid {tid}, spawned as {}", h.id().raw());
    }
    if reschedule_count() <= before {
        return Outcome::Fail("no reschedule IPI");
    }
    Outcome::Ok
}

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[
    test("reschedule_ipi_wake_ap", test_reschedule_ipi_wake_ap),
    test("call_function_ipi", test_call_function_ipi),
    test("reschedule_hook_installed", test_reschedule_hook_installed),
    test("irq_pool", test_irq_pool),
    test("irq_free_threaded", test_irq_free_threaded),
    test("msix_cpu", test_msix_cpu),
    test("intx_fallback", test_intx_fallback),
    test("intx_free_masks", test_intx_free_masks),
    test("msix_cpu_publish_last", msix_cpu_publish_last),
    test("lifetime_shootdown_ack_late", lifetime_shootdown_ack_late).deadline(15_000),
    test("shootdown_ack_while_busy", shootdown_ack_while_busy).deadline(13_000),
    test("wake_inbox_and_kva_pool", wake_inbox_and_kva_pool),
];
