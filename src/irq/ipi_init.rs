//! Fixed IPIs: reschedule, TLB shootdown, call-function, and the panic
//! stop primitive. ROADMAP §4.9–§4.10, §10.7, DESIGN §2.5 step 1, §7.6 /
//! §7.9.
//!
//! Handlers are allocation-free. Shootdown and call-function take neither
//! the page-table lock nor SCHED. A waiter with IF off polls inbound
//! slots so two concurrent shootdowns cannot deadlock.

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use vibeos::apic::{IpiError, IpiMode};
use vibeos::arch::{CycleCounter, InterruptMask, Ipi, IpiSend, PageTable};
use vibeos::ipi::{MAX_IPI_CPUS, SHOOT_RANGES, ShootRange, all_acked, waiter_mask};
use vibeos::irq::stop::{self, CrashRegs, NmiAction, StopBudget, StopHow};
use vibeos::log::Level;
use vibeos::paging::VirtAddr;
use vibeos::per_cpu::PerCpuRemote;
use vibeos::thread::ThreadId;
use vibeos::vectors;

use crate::apic_init;
use crate::arch::current::{self, Arch};
use crate::per_cpu_init;
use crate::serial::raw;
use crate::sync_init;
use crate::time_init;

/// One CPU's shootdown round: `n` packed [`ShootRange`]s in `ranges`.
struct Slot {
    n: AtomicU64,
    ranges: [AtomicU64; SHOOT_RANGES],
    waiters: AtomicU64,
    acked: AtomicU64,
}

impl Slot {
    const fn empty() -> Self {
        Self {
            n: AtomicU64::new(0),
            ranges: [const { AtomicU64::new(0) }; SHOOT_RANGES],
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
pub(super) static RESCHED_COUNT: AtomicU64 = AtomicU64::new(0);
pub(super) static SHOOT_COUNT: AtomicU64 = AtomicU64::new(0);
pub(super) static CALL_COUNT: AtomicU64 = AtomicU64::new(0);
/// IPIs the LAPIC refused to send since boot.
static SEND_FAILS: AtomicU64 = AtomicU64::new(0);

/// Count a refused IPI and log it at most once a second (DESIGN §2.5).
/// The caller goes on: a shootdown or call waiter still polls its acks and
/// logs each late second, and a reschedule leaves its thread in the inbox.
#[inline]
fn note_send(what: &str, r: Result<(), IpiError>) {
    if let Err(e) = r {
        send_failed(what, e);
    }
}

/// `note_send`'s failure path, out of line so the IPI senders' frames stay
/// as small as they were.
#[cold]
#[inline(never)]
fn send_failed(what: &str, e: IpiError) {
    // Relaxed: a count for the rate-limited line; pairs with nothing.
    let n = SEND_FAILS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    crate::klog_ratelimited!(
        1000,
        Level::Warn,
        "vibeOS: ipi: {} not sent: {} ({} since boot)",
        what,
        e.as_str(),
        n
    );
}

/// This CPU's bit. IF=0 callers only (DESIGN §2.9 rule 5).
fn my_bit() -> u64 {
    let id = per_cpu_init::try_current().map(|c| c.cpu_id).unwrap_or(0);
    if id >= 64 { 0 } else { 1u64 << id }
}

/// This CPU's index. IF=0 callers only (DESIGN §2.9 rule 5).
pub(super) fn my_index() -> usize {
    per_cpu_init::try_current()
        .map(|c| c.cpu_id as usize)
        .unwrap_or(0)
        .min(MAX_IPI_CPUS - 1)
}

/// Run pending shootdown and call-function work. No alloc, no PT/SCHED.
/// It reads this CPU's stop request first, so a CPU in a serviced spin
/// stops for a panic before it serves any slot (DESIGN §2.5 step 1).
pub fn service_incoming() {
    poll_stop();
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
        // Acquire: pairs with the Release store of `waiters` in `shootdown_round`.
        let w = s.waiters.load(Ordering::Acquire);
        // Relaxed: pairs with nothing; the Acquire load of `waiters` orders
        // it after the round's reset.
        if w & me != 0 && s.acked.load(Ordering::Relaxed) & me == 0 {
            {
                let _lockless = sync_init::lockless_section();
                // Relaxed: pairs with nothing; the Acquire load of `waiters`
                // orders it after the round's stores.
                let n = s.n.load(Ordering::Relaxed);
                for r in s.ranges.iter().take(n as usize) {
                    // Relaxed: as `n`; pairs with nothing.
                    let r = ShootRange::from_raw(r.load(Ordering::Relaxed));
                    let mut p = 0u64;
                    while p < r.pages() {
                        Arch::flush_local(VirtAddr(r.start().wrapping_add(p << 12)));
                        p += 1;
                    }
                }
            }
            // Relaxed: the Release `fetch_or` of `acked` below publishes it; pairs with nothing.
            SHOOT_COUNT.fetch_add(1, Ordering::Relaxed);
            vibeos::trace!(IpiAck, u64::from(vectors::IPI_SHOOTDOWN), i as u64);
            // Release: pairs with the Acquire load in `wait_acks`. The ack is
            // this handler's last access to the round (AGENTS.md rule 5): once
            // `wait_acks` sees it, the initiator may start the next round, and
            // a reader of `SHOOT_COUNT` that saw the ack (Acquire) sees this
            // round counted.
            s.acked.fetch_or(me, Ordering::Release);
        }
        i += 1;
    }
}

fn service_calls() {
    let me = my_bit();
    if me == 0 {
        return;
    }
    // Acquire: pairs with the Release store of `waiters` in `call_mask`.
    let w = CALL.waiters.load(Ordering::Acquire);
    if w & me == 0 {
        return;
    }
    // Relaxed: the Acquire load of `waiters` orders it after the call's reset; pairs with nothing.
    if CALL.acked.load(Ordering::Relaxed) & me != 0 {
        return;
    }
    // Relaxed: the Acquire load of `waiters` orders it after the call's stores; pairs with nothing.
    let f = CALL.func.load(Ordering::Relaxed);
    // Relaxed: as `func`; pairs with nothing.
    let arg = CALL.arg.load(Ordering::Relaxed);
    if !f.is_null() {
        // SAFETY: invariant: a non-null `CALL.func` holds a `fn(*mut ())`;
        // established by `ipi_init::call_mask`, its only non-null store.
        let f: fn(*mut ()) = unsafe { core::mem::transmute(f) };
        // Call-function work runs inside whatever this CPU holds, so it
        // takes no lock (DESIGN §2.2's last row).
        let _lockless = sync_init::lockless_section();
        f(arg);
    }
    // Relaxed: the Release `fetch_or` of `acked` below publishes it; pairs with nothing.
    CALL_COUNT.fetch_add(1, Ordering::Relaxed);
    vibeos::trace!(IpiAck, u64::from(vectors::IPI_CALL), u64::MAX);
    // Release: pairs with the Acquire load in `wait_acks`; publish last
    // (AGENTS.md rule 5), as in `service_shootdowns`.
    CALL.acked.fetch_or(me, Ordering::Release);
}

/// Wait until every CPU in `waiters` has acked. Never panics and never
/// returns early: `shootdown_ranges` and `call_mask` callers free frames and
/// reuse their slot as soon as it returns (DESIGN §7.9). After each second
/// without every ack (or [`NO_TSC_LATE_POLLS`] polls without a TSC) it logs
/// the CPUs that have not acked and counts it in [`ack_late_count`]; a CPU
/// that never acks is a hang for ROADMAP §10.7's forensics.
fn wait_acks(waiters: u64, acked: &AtomicU64) {
    // Rule 2's exemption (INVARIANTS.md §2.9): the irqoff tracer
    // subtracts this wait from the stretch.
    let _x = crate::sched::irqoff::exempt();
    if waiters == 0 {
        return;
    }
    assert!(!Arch::enabled(), "ipi: ack wait with IF on");
    let k = time_init::tsc_per_ms();
    let period = k.saturating_mul(1000);
    let start = time_init::read_tsc();
    let mut last = start;
    let mut spins = 0u64;
    loop {
        // Acquire: pairs with each handler's Release `fetch_or` of its ack.
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
            // Relaxed: a statistic; pairs with nothing.
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
pub(super) static ACK_LATE: AtomicU64 = AtomicU64::new(0);

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

/// After the local `invlpg`s. For each [`SHOOT_RANGES`] of `ranges`,
/// one round: publish them, broadcast 0xFC, wait for every other online
/// CPU to invalidate every page of them, service inbound.
///
/// IRQ-off for publish→wait→clear: this CPU's `SHOOT` slot is not
/// reentered by a timer/reschedule switch. Inbound shootdowns still
/// run through `service_incoming` (IF off cannot take the IPI).
pub fn shootdown_ranges(ranges: &[ShootRange]) {
    for chunk in ranges.chunks(SHOOT_RANGES) {
        shootdown_round(chunk);
    }
}

/// One round for at most [`SHOOT_RANGES`] ranges.
fn shootdown_round(ranges: &[ShootRange]) {
    let _irq = crate::arch::current::InterruptGuard::enter();
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
    crate::irq::ktest::note_shootdown();
    let me = my_index() as u32;
    let waiters = waiter_mask(per_cpu_init::online_mask(), me);
    if waiters == 0 {
        return;
    }
    let slot = &SHOOT[me as usize];
    let mut n = 0u64;
    for (dst, r) in slot.ranges.iter().zip(ranges) {
        // Relaxed: the Release store of `waiters` below publishes it; pairs with nothing.
        dst.store(r.raw(), Ordering::Relaxed);
        n += 1;
    }
    // Relaxed: as the ranges; pairs with nothing.
    slot.n.store(n, Ordering::Relaxed);
    // Relaxed: as the ranges; pairs with nothing.
    slot.acked.store(0, Ordering::Relaxed);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    // Release: pairs with the Acquire load of `waiters` in `service_shootdowns`.
    slot.waiters.store(waiters, Ordering::Release);
    note_send("shootdown", Arch::send_others(Ipi::Shootdown));
    wait_acks(waiters, &slot.acked);
    // Release: pairs with the Acquire load of `waiters` in `service_shootdowns`.
    slot.waiters.store(0, Ordering::Release);
}

/// Queue thread-table slot `slot` on `cpu`'s wake inbox.
fn inbox_push(cpu: u32, slot: usize) {
    let Some(pc) = per_cpu_init::cpu(cpu) else {
        return;
    };
    // Invariant: `slot` is a thread-table slot, below `MAX_THREADS`, and the
    // inbox has a bit for each (`vibeos::ipi::INBOX_WORDS`).
    assert!(
        pc.wake_inbox.push(slot),
        "wake inbox: slot {slot} out of range"
    );
}

/// Send `cpu` the reschedule IPI with nothing queued: its interrupt exit
/// then runs the exit work of DESIGN §5.10 rule 11 for whatever the caller
/// published first. A failed send is counted and logged ([`note_send`]).
pub fn kick(cpu: u32) {
    note_send("reschedule", Arch::send(cpu, Ipi::Reschedule));
}

/// Place `id`, in thread-table slot `slot`, on `cpu`. Local: runq. Remote:
/// inbox + 0xFD. Never a remote queue lock. IRQ-off for the local runq.
pub fn place_ready(cpu: u32, id: ThreadId, slot: usize) {
    vibeos::trace!(Wake, u64::from(id.0), u64::from(cpu));
    let _irq = crate::arch::current::InterruptGuard::enter();
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
    inbox_push(cpu, slot);
    note_send("reschedule", Arch::send(cpu, Ipi::Reschedule));
}

/// A wake-inbox slot's tid: `thread_init::tid_of_slot`, which
/// `thread_init::init_bootstrap` sets (DESIGN §1.2). Unset, no thread
/// table exists yet and [`drain_inbox`] leaves the inbox as it is.
static SLOT_TID_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the slot-to-tid lookup the drain uses.
pub fn set_slot_tid_hook(f: fn(usize) -> Option<ThreadId>) {
    // Release: pairs with the Acquire load in `drain_inbox`.
    SLOT_TID_HOOK.store(f as *mut (), Ordering::Release);
}

/// Move this CPU's wake inbox onto its run queue. Each slot maps to its
/// tid through the slot-to-tid hook (`thread_init::tid_of_slot`). True if
/// a slot was queued.
pub fn drain_inbox() -> bool {
    // Acquire: pairs with the Release store in `set_slot_tid_hook`.
    let p = SLOT_TID_HOOK.load(Ordering::Acquire);
    if p.is_null() {
        return false;
    }
    // SAFETY: invariant: a non-null `SLOT_TID_HOOK` holds a
    // `fn(usize) -> Option<ThreadId>`; established by
    // `ipi_init::set_slot_tid_hook`, its only store.
    let tid_of_slot = unsafe { core::mem::transmute::<*mut (), fn(usize) -> Option<ThreadId>>(p) };
    per_cpu_init::with_current(|pc| {
        let remote = pc.remote;
        remote.wake_inbox.drain::<Arch>(|slot| {
            if let Some(id) = tid_of_slot(slot) {
                pc.runq.push_back(id);
            }
        })
    })
}

/// What a reschedule IPI runs after it drains the inbox: the scheduler's
/// preemption point, which `sched_init::init` sets (DESIGN §1.2). Unset,
/// the IPI only counts and drains.
pub(super) static RESCHED: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the reschedule hook.
pub fn set_reschedule_hook(f: fn()) {
    // Release: pairs with the Acquire load in `on_reschedule_ipi`.
    RESCHED.store(f as *mut (), Ordering::Release);
}

pub fn on_reschedule_ipi() {
    vibeos::trace!(IpiAck, u64::from(vectors::IPI_RESCHEDULE), u64::MAX);
    // Relaxed: a statistic; pairs with nothing.
    RESCHED_COUNT.fetch_add(1, Ordering::Relaxed);
    drain_inbox();
    // Acquire: pairs with the Release store in `set_reschedule_hook`.
    let p = RESCHED.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: invariant: a non-null `RESCHED` holds a `fn()`; established
    // by `ipi_init::set_reschedule_hook`, its only store.
    let f = unsafe { core::mem::transmute::<*mut (), fn()>(p) };
    f();
}

#[cfg_attr(
    target_arch = "aarch64",
    expect(dead_code, reason = "x86-only on the boot-CPU slice")
)]
pub fn on_shootdown_ipi() {
    service_shootdowns();
}

pub fn on_call_ipi() {
    service_calls();
}

/// NMIs the panic dump's owner returned from (its own, from a self-NMI
/// test, or any that lands mid-dump): the NMI body's `Return` arm.
#[cfg_attr(
    target_arch = "aarch64",
    expect(dead_code, reason = "x86-only on the boot-CPU slice")
)]
pub static OWNER_NMI_RETURNS: AtomicU64 = AtomicU64::new(0);

/// How long the owner waits for acknowledgements before it sends NMI, and
/// after it (DESIGN §2.5 step 1).
const STOP_WAIT_MS: u64 = 100;
const NMI_WAIT_MS: u64 = 10;

/// The registers of the code that calls this: a stop that no trap
/// delivered saves its own.
#[inline(always)]
fn here_regs() -> CrashRegs {
    CrashRegs {
        rip: current::instruction_pointer(),
        rsp: current::stack_pointer(),
        rbp: current::frame_pointer(),
        rflags: current::irq_flags(),
    }
}

/// `service_incoming`'s first step: STOP in this CPU's request word stops
/// it. IF=0 callers only, as every serviced spin is.
#[inline]
fn poll_stop() {
    let Some(r) = per_cpu_init::cpu(my_index() as u32) else {
        return;
    };
    // Acquire: pairs with the Release `fetch_or` in `stop_others`.
    if r.stop_req.load(Ordering::Acquire) & stop::STOP != 0 {
        stop_this_cpu(StopHow::Poll, here_regs());
    }
}

/// The stop routine (DESIGN §2.5 step 1): mark this CPU stopping, save
/// `regs` in its crash-register slot, store how it stopped (the
/// acknowledgement), and halt with IF=0 for good. A CPU already stopping
/// or stopped halts at once, so its slot is written once.
pub fn stop_this_cpu(how: StopHow, regs: CrashRegs) -> ! {
    current::irq_disable();
    if let Some(r) = per_cpu_init::cpu(my_index() as u32) {
        // Acquire on success: pairs with nothing; nothing this CPU writes to
        // the slot moves above the claim.
        // Relaxed on failure: the CPU halts at once; pairs with nothing.
        if r.stopped
            .compare_exchange(
                stop::RUNNING,
                stop::STOPPING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            write_slot(r, regs);
            // Release: pairs with the Acquire load in `cpu_stop_state`; the
            // acknowledgement is this CPU's last store, and the owner reads
            // the slot after it (AGENTS.md rule 5).
            r.stopped.store(how.code(), Ordering::Release);
        }
    }
    current::halt();
}

/// Store `regs` in `r`'s crash-register slot (`irq::stop::CRASH_*`).
fn write_slot(r: &PerCpuRemote, regs: CrashRegs) {
    for (w, v) in r.crash.iter().zip(regs.to_words()) {
        // Relaxed: `stop_this_cpu`'s Release acknowledgement publishes it; pairs with nothing.
        w.store(v, Ordering::Relaxed);
    }
}

/// The dump owner's slot save: its own entry registers, without stopping
/// it, so the core tool walks the owner from where the dump began, as it
/// walks each stopped CPU from its slot (ROADMAP §10.7). Nothing on the
/// live system reads the owner's slot (`cpu_stop_state` names only how the
/// others stopped). IF=0 callers only: the dump's owner.
pub fn save_crash_regs(regs: CrashRegs) {
    if let Some(r) = per_cpu_init::cpu(my_index() as u32) {
        write_slot(r, regs);
    }
}

/// The raw serial layer's stop hook: a serial write or log append on a CPU
/// that is not the dump's owner once `HALTING` is set.
fn stop_hook() {
    stop_this_cpu(StopHow::Poll, here_regs());
}

/// The `0xFE` body: stop, saving the interrupted registers `regs` (the
/// IDT body reads them from its trap frame, so this module does not name
/// the IDT's frame type, DESIGN §1.2).
#[cfg_attr(
    target_arch = "aarch64",
    expect(dead_code, reason = "x86-only on the boot-CPU slice")
)]
pub fn on_stop_ipi(regs: CrashRegs) -> ! {
    stop_this_cpu(StopHow::Ipi, regs);
}

/// The NMI body's first step, before it writes anything or takes any lock
/// (the owner may be inside `write_owner`): swap this CPU's request word
/// to 0 and act on `vibeos::irq::stop::nmi_action`. Halts on a CPU already
/// stopping, stops on STOP, and returns `Return` on the dump's owner and
/// `Dump` for any other NMI, which the caller dumps as before. `regs` are
/// the interrupted registers from the NMI's trap frame.
#[cfg_attr(
    target_arch = "aarch64",
    expect(dead_code, reason = "x86-only on the boot-CPU slice")
)]
pub fn nmi_stop(regs: CrashRegs) -> NmiAction {
    let (req, state) = match per_cpu_init::cpu(my_index() as u32) {
        // AcqRel: pairs with the owner's Release `fetch_or` in `stop_others`;
        // the swap reads it and clears the request in one step.
        // Acquire: pairs with the Release acknowledgement in `stop_this_cpu`.
        Some(r) => (
            r.stop_req.swap(0, Ordering::AcqRel),
            r.stopped.load(Ordering::Acquire),
        ),
        None => (0, stop::RUNNING),
    };
    let action = stop::nmi_action(req, state, raw::is_owner());
    match action {
        NmiAction::Halt => current::halt(),
        NmiAction::Stop => stop_this_cpu(StopHow::Nmi, regs),
        NmiAction::Return => {
            // Relaxed: a statistic; pairs with nothing.
            OWNER_NMI_RETURNS.fetch_add(1, Ordering::Relaxed);
        }
        NmiAction::Dump => {}
    }
    action
}

/// How CPU `cpu` stopped (`None` while it has not acknowledged) and its
/// crash-register slot; `None` for a CPU with no per-CPU view.
pub fn cpu_stop_state(cpu: u32) -> Option<(Option<StopHow>, CrashRegs)> {
    let r = per_cpu_init::cpu(cpu)?;
    // Acquire: pairs with the Release acknowledgement in `stop_this_cpu`,
    // so the slot's words below are the ones stored before it.
    let how = StopHow::from_code(r.stopped.load(Ordering::Acquire));
    let mut words = [0u64; stop::CRASH_WORDS];
    for (w, v) in words.iter_mut().zip(r.crash.iter()) {
        // Relaxed: the Acquire load of `stopped` above orders it; pairs with nothing.
        *w = v.load(Ordering::Relaxed);
    }
    Some((how, CrashRegs::from_words(words)))
}

/// The CPUs of `mask` that have acknowledged a stop.
fn stopped_mask(mask: u64) -> u64 {
    let mut out = 0u64;
    let mut m = mask;
    while m != 0 {
        let c = m.trailing_zeros();
        m &= m - 1;
        if matches!(cpu_stop_state(c), Some((Some(_), _))) {
            out |= 1u64 << c;
        }
    }
    out
}

/// Wait up to `ms` of counter time (or a poll bound before the counter is
/// measured) for every CPU of `mask` to acknowledge; the CPUs still
/// running when it ends.
fn wait_stopped(mask: u64, ms: u64) -> u64 {
    let budget = stop::stop_budget(<Arch as CycleCounter>::freq_hz(), ms);
    let start = <Arch as CycleCounter>::now();
    let mut spins = 0u64;
    loop {
        let left = mask & !stopped_mask(mask);
        if left == 0 {
            return 0;
        }
        let over = match budget {
            StopBudget::Cycles(c) => <Arch as CycleCounter>::now().wrapping_sub(start) >= c,
            StopBudget::Spins(n) => {
                spins = spins.saturating_add(1);
                spins >= n
            }
        };
        if over {
            return left;
        }
        core::hint::spin_loop();
    }
}

/// Send `vector` with `mode` to CPU `cpu`. Through `apic_init::send_ipi`,
/// which records no trace event: `trace!` takes an `InterruptGuard`, which
/// the dump path never does (DESIGN §2.5 step 1).
fn send_raw(cpu: u32, vector: u8, mode: IpiMode) -> Result<(), IpiError> {
    let Some(r) = per_cpu_init::cpu(cpu) else {
        return Err(IpiError::NotReady);
    };
    // Relaxed: set before the CPU starts, fixed while it runs; pairs with nothing.
    apic_init::send_ipi(r.apic_id.load(Ordering::Relaxed) as u8, vector, mode)
}

/// Send NMI to each CPU of `mask`; a CPU whose NMI the LAPIC refused is
/// left running, and the dump reports it `not stopped`.
fn nmi_each(mask: u64) {
    let mut m = mask;
    while m != 0 {
        let c = m.trailing_zeros();
        m &= m - 1;
        #[expect(
            clippy::let_underscore_must_use,
            reason = "DESIGN §2.5: a refused NMI leaves its CPU running, which the dump reports as `not stopped`, its recorded error state"
        )]
        let _ = send_raw(c, 0, IpiMode::Nmi);
    }
}

/// The dump owner's stop primitive (DESIGN §2.5 step 1): set `HALTING`;
/// for each other online CPU set STOP in its request word (Release) and
/// send it `0xFE`; wait up to 100 ms for every acknowledgement; send NMI
/// to each CPU still running, and wait 10 ms more. A CPU whose `0xFE` the
/// LAPIC refused gets its NMI at once. The owner's own IF is off.
pub fn stop_others() {
    // Release: pairs with the Acquire loads of `HALTING` in `serial` and `sync_init`.
    raw::HALTING.store(true, Ordering::Release);
    let me = my_index() as u32;
    let others = waiter_mask(per_cpu_init::online_mask(), me);
    let mut refused = 0u64;
    let mut m = others;
    while m != 0 {
        let c = m.trailing_zeros();
        m &= m - 1;
        if let Some(r) = per_cpu_init::cpu(c) {
            // Release: pairs with the Acquire loads in `poll_stop` and the
            // AcqRel swap in `nmi_stop`.
            r.stop_req.fetch_or(stop::STOP, Ordering::Release);
        }
        if send_raw(c, vectors::IPI_HALT, IpiMode::Fixed).is_err() {
            refused |= 1u64 << c;
        }
    }
    nmi_each(refused);
    let late = wait_stopped(others, STOP_WAIT_MS);
    if late == 0 {
        return;
    }
    nmi_each(late);
    // The CPUs still running are reported `not stopped` by the dump.
    wait_stopped(late, NMI_WAIT_MS);
}

/// Run `f(arg)` on every online CPU in `mask` except self. Always waits
/// to reclaim the single CALL slot (`wait` is the public completion
/// contract). IRQ-off for publish → IPI → ack → clear; inbound still
/// polls `service_incoming`.
pub fn call_mask(mask: u64, f: fn(*mut ()), arg: *mut (), _wait: bool) {
    // IF=0 before `my_index`, so `me` stays this CPU's id (DESIGN §2.9
    // rule 5).
    let _irq = crate::arch::current::InterruptGuard::enter();
    let me = my_index() as u32;
    let waiters = waiter_mask(mask & per_cpu_init::online_mask(), me);
    if waiters == 0 {
        return;
    }
    {
        // The wait for the call slot is exempt too (rule 2).
        let _x = crate::sched::irqoff::exempt();
        // Acquire: pairs with the Release store of `false` that ends `call_mask`.
        // Relaxed on failure: the loop retries; pairs with nothing.
        while CALL_BUSY
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            service_incoming();
            core::hint::spin_loop();
        }
    }
    // Relaxed: the Release store of `waiters` below publishes it; pairs with nothing.
    CALL.func.store(f as *mut (), Ordering::Relaxed);
    // Relaxed: as `func`; pairs with nothing.
    CALL.arg.store(arg, Ordering::Relaxed);
    // Relaxed: as `func`; pairs with nothing.
    CALL.acked.store(0, Ordering::Relaxed);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    // Release: pairs with the Acquire load of `waiters` in `service_calls`.
    CALL.waiters.store(waiters, Ordering::Release);
    let mut c = 0u32;
    while c < 64 {
        if waiters & (1u64 << c) != 0 {
            note_send("call", Arch::send(c, Ipi::Call));
        }
        c += 1;
    }
    wait_acks(waiters, &CALL.acked);
    // Release: pairs with the Acquire load of `waiters` in `service_calls`.
    CALL.waiters.store(0, Ordering::Release);
    // Relaxed: the Release store of `CALL_BUSY` below publishes it; pairs with nothing.
    CALL.func.store(core::ptr::null_mut(), Ordering::Relaxed);
    // Release: pairs with the Acquire compare-exchange in the next `call_mask`.
    CALL_BUSY.store(false, Ordering::Release);
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "ROADMAP §4.9 0xFB call-function; only the in-guest tests send one yet"
    )
)]
#[cfg_attr(
    all(target_arch = "aarch64", feature = "kernel_tests"),
    expect(dead_code, reason = "boot-CPU S7; unused on this path")
)]
pub fn call_cpu(cpu: u32, f: fn(*mut ()), arg: *mut (), wait: bool) {
    if cpu >= 64 {
        return;
    }
    call_mask(1u64 << cpu, f, arg, wait);
}

/// Install the shootdown hook. IDT overlays are already in place.
pub fn init() {
    vibeos::paging::set_tlb_shootdown_hook(shootdown_ranges);
    crate::sync_init::set_spin_poll(service_incoming);
    raw::set_stop_hook(stop_hook);
}
