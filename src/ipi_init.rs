//! Fixed IPIs: reschedule, TLB shootdown, call-function, panic halt.
//! ROADMAP §4.9–§4.10, DESIGN §7.6 / §7.9.
//!
//! Handlers are allocation-free. Shootdown and call-function take neither
//! the page-table lock nor SCHED. A waiter with IF off polls inbound
//! slots so two concurrent shootdowns cannot deadlock.
#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use vibeos::ipi::{MAX_IPI_CPUS, all_acked, inbox_bit, waiter_mask};
use vibeos::log::Level;
use vibeos::paging::VirtAddr;
use vibeos::thread::ThreadId;
use vibeos::vectors;

use crate::apic_init;
use crate::per_cpu_init;
use crate::thread_init;
use crate::time_init;
use crate::x86;

struct Slot {
    va: AtomicU64,
    waiters: AtomicU64,
    acked: AtomicU64,
}

impl Slot {
    const fn empty() -> Self {
        Self {
            va: AtomicU64::new(0),
            waiters: AtomicU64::new(0),
            acked: AtomicU64::new(0),
        }
    }
}

struct CallSlot {
    func: AtomicPtr<()>,
    arg: AtomicPtr<()>,
    waiters: AtomicU64,
    acked: AtomicU64,
}

impl CallSlot {
    const fn empty() -> Self {
        Self {
            func: AtomicPtr::new(core::ptr::null_mut()),
            arg: AtomicPtr::new(core::ptr::null_mut()),
            waiters: AtomicU64::new(0),
            acked: AtomicU64::new(0),
        }
    }
}

static SHOOT: [Slot; MAX_IPI_CPUS] = [const { Slot::empty() }; MAX_IPI_CPUS];
static CALL: CallSlot = CallSlot::empty();
static CALL_BUSY: AtomicBool = AtomicBool::new(false);
static HALTING: AtomicBool = AtomicBool::new(false);
static RESCHED_COUNT: AtomicU64 = AtomicU64::new(0);
static SHOOT_COUNT: AtomicU64 = AtomicU64::new(0);
static CALL_COUNT: AtomicU64 = AtomicU64::new(0);

fn my_bit() -> u64 {
    let id = per_cpu_init::try_current().map(|c| c.cpu_id).unwrap_or(0);
    if id >= 64 { 0 } else { 1u64 << id }
}

fn my_index() -> usize {
    per_cpu_init::try_current()
        .map(|c| c.cpu_id as usize)
        .unwrap_or(0)
        .min(MAX_IPI_CPUS - 1)
}

/// Run pending shootdown and call-function work. No alloc, no PT/SCHED.
pub fn service_incoming() {
    service_shootdowns();
    service_calls();
}

fn service_shootdowns() {
    let me = my_bit();
    if me == 0 {
        return;
    }
    let mut i = 0usize;
    while i < MAX_IPI_CPUS {
        let s = &SHOOT[i];
        let w = s.waiters.load(Ordering::Acquire);
        if w & me != 0 && s.acked.load(Ordering::Relaxed) & me == 0 {
            let va = s.va.load(Ordering::Relaxed);
            x86::invlpg(va);
            s.acked.fetch_or(me, Ordering::Release);
            SHOOT_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        i += 1;
    }
}

fn service_calls() {
    let me = my_bit();
    if me == 0 {
        return;
    }
    let w = CALL.waiters.load(Ordering::Acquire);
    if w & me == 0 {
        return;
    }
    if CALL.acked.load(Ordering::Relaxed) & me != 0 {
        return;
    }
    let f = CALL.func.load(Ordering::Relaxed);
    let arg = CALL.arg.load(Ordering::Relaxed);
    if !f.is_null() {
        let f: fn(*mut ()) = unsafe { core::mem::transmute(f) };
        f(arg);
    }
    CALL.acked.fetch_or(me, Ordering::Release);
    CALL_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Wait until every CPU in `waiters` has acked. Never panics and never
/// returns early: `shootdown_va` and `call_mask` callers free frames and
/// reuse their slot as soon as it returns (DESIGN §7.9). After each second
/// without every ack (or [`NO_TSC_LATE_POLLS`] polls without a TSC) it logs
/// the CPUs that have not acked and counts it in [`ack_late_count`]; a CPU
/// that never acks is a hang for ROADMAP §10.7's forensics.
fn wait_acks(waiters: u64, acked: &AtomicU64) {
    if waiters == 0 {
        return;
    }
    assert!(!x86::interrupts_enabled(), "ipi: ack wait with IF on");
    let k = time_init::tsc_per_ms();
    let period = k.saturating_mul(1000);
    let start = time_init::read_tsc();
    let mut last = start;
    let mut spins = 0u64;
    loop {
        let got = acked.load(Ordering::Acquire);
        if all_acked(waiters, got) {
            return;
        }
        service_incoming();
        spins = spins.wrapping_add(1);
        let late = if k != 0 {
            let now = time_init::read_tsc();
            if now.wrapping_sub(last) >= period {
                last = now;
                Some((now.wrapping_sub(start) / period, "s"))
            } else {
                None
            }
        } else if spins.is_multiple_of(NO_TSC_LATE_POLLS) {
            Some((spins, "polls"))
        } else {
            None
        };
        if let Some((n, unit)) = late {
            ACK_LATE.fetch_add(1, Ordering::Relaxed);
            crate::klog!(
                Level::Warn,
                "vibeOS: ipi: wait_acks late {} {}:{}",
                n,
                unit,
                CpuList(waiters & !got)
            );
        }
        core::hint::spin_loop();
    }
}

/// Late periods [`wait_acks`] has logged since boot.
static ACK_LATE: AtomicU64 = AtomicU64::new(0);

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn ack_late_count() -> u64 {
    ACK_LATE.load(Ordering::Relaxed)
}

/// Without a TSC, [`wait_acks`] logs every this many polls.
const NO_TSC_LATE_POLLS: u64 = 50_000_000;

/// A CPU mask written as ` cpu<n>` for each set bit.
struct CpuList(u64);

impl core::fmt::Display for CpuList {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut m = self.0;
        while m != 0 {
            write!(f, " cpu{}", m.trailing_zeros())?;
            m &= m - 1;
        }
        Ok(())
    }
}

/// After local `invlpg`. Broadcast 0xFC, wait, service inbound.
///
/// IRQ-off for publish→wait→clear: this CPU's `SHOOT` slot is not
/// reentered by a timer/reschedule switch. Inbound shootdowns still
/// run through `service_incoming` (IF off cannot take the IPI).
pub fn shootdown_va(va: VirtAddr) {
    let _irq = x86::InterruptGuard::enter();
    let me = my_index() as u32;
    let waiters = waiter_mask(per_cpu_init::online_mask(), me);
    if waiters == 0 {
        return;
    }
    let slot = &SHOOT[me as usize];
    slot.va.store(va.as_u64(), Ordering::Relaxed);
    slot.acked.store(0, Ordering::Relaxed);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    slot.waiters.store(waiters, Ordering::Release);
    let _ = apic_init::send_ipi_all_ex_self(vectors::IPI_SHOOTDOWN);
    wait_acks(waiters, &slot.acked);
    slot.waiters.store(0, Ordering::Release);
}

fn inbox_push(cpu: u32, id: ThreadId) {
    let Some(bit) = inbox_bit(id) else {
        return;
    };
    let Some(pc) = per_cpu_init::cpu(cpu) else {
        return;
    };
    pc.wake_inbox.fetch_or(bit, Ordering::Release);
}

/// Place `id` on `cpu`. Local: runq. Remote: inbox + 0xFD. Never a
/// remote queue lock. IRQ-off for the local runq.
pub fn place_ready(cpu: u32, id: ThreadId) {
    let _irq = crate::x86::InterruptGuard::enter();
    let me = per_cpu_init::try_current().map(|c| c.cpu_id).unwrap_or(0);
    let cpu = if cpu == me || per_cpu_init::is_online(cpu) {
        cpu
    } else {
        me
    };
    if cpu == me {
        per_cpu_init::with_current(|pc| {
            pc.runq.push_back(id);
        });
        return;
    }
    inbox_push(cpu, id);
    let _ = apic_init::send_ipi_cpu(cpu, vectors::IPI_RESCHEDULE);
}

pub fn drain_inbox() -> bool {
    per_cpu_init::with_current(|pc| {
        let bits = pc.remote.wake_inbox.swap(0, Ordering::Acquire);
        if bits == 0 {
            return false;
        }
        let mut b = bits;
        let mut id = 0u32;
        while b != 0 {
            if b & 1 != 0 {
                pc.runq.push_back(ThreadId(id));
            }
            b >>= 1;
            id += 1;
        }
        true
    })
}

pub fn on_reschedule_ipi() {
    RESCHED_COUNT.fetch_add(1, Ordering::Relaxed);
    drain_inbox();
    thread_init::schedule_preempt();
}

pub fn on_shootdown_ipi() {
    service_shootdowns();
}

pub fn on_call_ipi() {
    service_calls();
}

pub fn on_halt_ipi() -> ! {
    HALTING.store(true, Ordering::Release);
    x86::halt();
}

/// Broadcast halt so others stop before we trash the log.
/// Fixed IPI `0xFE`, not NMI (DESIGN §2.5 / §7.6).
pub fn halt_others() {
    HALTING.store(true, Ordering::Release);
    let _ = apic_init::send_ipi_all_ex_self(vectors::IPI_HALT);
}

pub fn is_halting() -> bool {
    HALTING.load(Ordering::Acquire)
}

/// Run `f(arg)` on every online CPU in `mask` except self. Always waits
/// to reclaim the single CALL slot (`wait` is the public completion
/// contract). IRQ-off for publish → IPI → ack → clear; inbound still
/// polls `service_incoming`.
pub fn call_mask(mask: u64, f: fn(*mut ()), arg: *mut (), wait: bool) {
    let me = my_index() as u32;
    let waiters = waiter_mask(mask & per_cpu_init::online_mask(), me);
    if waiters == 0 {
        return;
    }
    let _irq = x86::InterruptGuard::enter();
    while CALL_BUSY
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        service_incoming();
        core::hint::spin_loop();
    }
    CALL.func.store(f as *mut (), Ordering::Relaxed);
    CALL.arg.store(arg, Ordering::Relaxed);
    CALL.acked.store(0, Ordering::Relaxed);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    CALL.waiters.store(waiters, Ordering::Release);
    let mut c = 0u32;
    while c < 64 {
        if waiters & (1u64 << c) != 0 {
            let _ = apic_init::send_ipi_cpu(c, vectors::IPI_CALL);
        }
        c += 1;
    }
    wait_acks(waiters, &CALL.acked);
    let _ = wait;
    CALL.waiters.store(0, Ordering::Release);
    CALL.func.store(core::ptr::null_mut(), Ordering::Relaxed);
    CALL_BUSY.store(false, Ordering::Release);
}

pub fn call_cpu(cpu: u32, f: fn(*mut ()), arg: *mut (), wait: bool) {
    if cpu >= 64 {
        return;
    }
    call_mask(1u64 << cpu, f, arg, wait);
}

/// Install the shootdown hook. IDT overlays are already in place.
pub fn init() {
    vibeos::paging::set_tlb_shootdown_hook(shootdown_va);
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn reschedule_count() -> u64 {
    RESCHED_COUNT.load(Ordering::Relaxed)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn shootdown_count() -> u64 {
    SHOOT_COUNT.load(Ordering::Relaxed)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn call_count() -> u64 {
    CALL_COUNT.load(Ordering::Relaxed)
}
