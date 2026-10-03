//! In-guest test registry. DESIGN §8.2.
//!
//! Built only with `--features kernel_tests`. After normal init this
//! module runs the registry over the real IDT, prints the serial protocol,
//! and exits QEMU through `isa-debug-exit`.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::arch::CycleCounter;
use vibeos::dev::DevRef;
use vibeos::fmt_util::StackBuf;
use vibeos::lock::RANK_DEVICE;
use vibeos::paging::PhysAddr;
use vibeos::per_cpu::PerCpuRemote;
use vibeos::pmm::Frames;
use vibeos::thread::{ThreadId, ThreadState};
use vibeos::vectors;

use crate::arch::current::{
    Arch, InterruptGuard, interrupts_enabled, irq_disable, irq_enable, qemu_exit,
};
use crate::ipi_init;
use crate::kva_init;
use crate::per_cpu_init;
use crate::pmm_init;
use crate::sync_init::SpinMutex;
use crate::thread_init::{self, ThreadHandle};
use crate::time_init;
#[cfg(target_arch = "x86_64")]
use crate::x86;
use crate::{
    acpi, arch, block, boot, console, dev, drivers, fs, irq, log, mm, proc, sched, shell, smp,
    sync, time,
};
pub(crate) mod user;

const EXIT_PASS: u32 = 0x10;
const EXIT_FAIL: u32 = 0x11;

#[derive(Clone, Copy)]
pub(crate) enum Outcome {
    Ok,
    Fail(&'static str),
    FailFmt(FailMsg),
    Skip(&'static str),
}

pub(crate) const FAIL_MSG_BYTES: usize = 120;

/// A formatted failure reason, cut at [`FAIL_MSG_BYTES`] on a character
/// boundary. Build one with [`crate::fail_fmt!`]. It writes through
/// [`StackBuf`], the one fixed-buffer writer (DESIGN §8.2).
#[derive(Clone, Copy)]
pub(crate) struct FailMsg {
    buf: [u8; FAIL_MSG_BYTES],
    len: u8,
    full: bool,
}

impl FailMsg {
    pub(crate) fn from_args(args: fmt::Arguments<'_>) -> FailMsg {
        let mut m = FailMsg {
            buf: [0; FAIL_MSG_BYTES],
            len: 0,
            full: false,
        };
        if fmt::write(&mut m, args).is_err() {
            // A `Display` impl failed: say so rather than drop the error
            // (DESIGN §2.5). Appended only if it fits whole.
            const MARK: &str = " <fmt error>";
            if m.full || (m.len as usize) + MARK.len() > FAIL_MSG_BYTES {
                return m;
            }
            m.push_whole(MARK);
        }
        m
    }

    pub(crate) fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("<invalid utf-8>")
    }

    /// Append the longest prefix of `s` that ends on a character boundary
    /// and fits; after the first character that does not fit, nothing more.
    fn push_whole(&mut self, s: &str) {
        if self.full {
            return;
        }
        let at = self.len as usize;
        let room = FAIL_MSG_BYTES - at;
        let mut n = s.len().min(room);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        let mut w = StackBuf::new(&mut self.buf[at..]);
        w.push_bytes(&s.as_bytes()[..n]);
        // `n <= room <= FAIL_MSG_BYTES`, which fits a u8.
        self.len += w.len() as u8;
        if n < s.len() {
            self.full = true;
        }
    }
}

impl fmt::Write for FailMsg {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push_whole(s);
        Ok(())
    }
}

/// `Outcome::FailFmt` with a `format_args!` reason, cut at 120 bytes.
#[macro_export]
macro_rules! fail_fmt {
    ($($arg:tt)*) => {
        $crate::ktest::Outcome::FailFmt($crate::ktest::FailMsg::from_args(format_args!($($arg)*)))
    };
}

pub(crate) type TestFn = fn() -> Outcome;

/// One registry row. `deadline_ms`, `once` and `opt_in` are data until the
/// runner enforces them (ROADMAP §10.2).
#[derive(Clone, Copy)]
pub(crate) struct Test {
    pub name: &'static str,
    pub run: TestFn,
    pub deadline_ms: u32,
    pub once: bool,
    pub opt_in: bool,
}

/// A row with the default deadline (10 s), run on every repeat, not opt-in.
pub(crate) const fn test(name: &'static str, run: TestFn) -> Test {
    Test {
        name,
        run,
        deadline_ms: vibeos::ktest::DEFAULT_DEADLINE_MS,
        once: false,
        opt_in: false,
    }
}

impl Test {
    pub const fn deadline(self, ms: u32) -> Test {
        Test {
            deadline_ms: ms,
            ..self
        }
    }

    pub const fn once(self) -> Test {
        Test { once: true, ..self }
    }

    pub const fn opt_in(self) -> Test {
        Test {
            opt_in: true,
            ..self
        }
    }
}

pub(crate) type Suite = &'static [Test];

/// The runner's own rows (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[
    test("ktest_names_unique", test_ktest_names_unique),
    test("ktest_once_probe", test_ktest_once_probe).once(),
    test("ktest_optin_probe", test_ktest_optin_probe).opt_in(),
    test("ktest_deadline_hang", test_ktest_deadline_hang)
        .deadline(500)
        .opt_in(),
];

/// The planted hang (opt-in): IF=0 on this CPU and no return, so only
/// another CPU's tick can see its 500 ms deadline pass. `run_ktest.py`'s
/// `ktest_deadline_trip` boot expects the FAIL line and the panic.
fn test_ktest_deadline_hang() -> Outcome {
    let _g = InterruptGuard::enter();
    loop {
        core::hint::spin_loop();
    }
}

/// Set by [`test_ktest_once_probe`]'s first run.
static ONCE_PROBE_RAN: AtomicBool = AtomicBool::new(false);

/// A `.once()` row: fails if the runner runs it twice in one boot.
fn test_ktest_once_probe() -> Outcome {
    if ONCE_PROBE_RAN.swap(true, Ordering::Relaxed) {
        return Outcome::Fail("a once row ran twice");
    }
    Outcome::Ok
}

/// An `.opt_in()` row: runs only when `vibeos.ktest=` names it.
fn test_ktest_optin_probe() -> Outcome {
    Outcome::Ok
}

/// Test names are unique across [`GROUPS`] and match `[a-z0-9_]+`, the
/// form `vibeos.ktest=` globs and the harness's results name.
fn test_ktest_names_unique() -> Outcome {
    for (i, (_, _, t)) in rows().enumerate() {
        if t.name.is_empty()
            || !t
                .name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return crate::fail_fmt!("name {:?} is not [a-z0-9_]+", t.name);
        }
        if rows().skip(i + 1).any(|(_, _, o)| o.name == t.name) {
            return crate::fail_fmt!("duplicate test name {}", t.name);
        }
    }
    crate::ktest_info!("{} rows in {} groups", rows().count(), GROUPS.len());
    Outcome::Ok
}

/// Every subsystem's rows, in run order: each group's `TESTS` lives in its
/// subsystem's `ktest.rs`, and a new test goes in its subsystem's list
/// (DESIGN §8.2). A group missing here is dead code, which the
/// `kernel_tests` clippy run denies. The groups follow their first row's
/// place in the old single list, except log, which runs first:
/// `log_boot_captured` reads boot lines that the other groups' lines push
/// out of the log ring.
pub(crate) const GROUPS: &[Suite] = &[
    TESTS,
    log::ktest::TESTS,
    mm::ktest::TESTS,
    acpi::ktest::TESTS,
    arch::ktest::TESTS,
    proc::ktest::TESTS,
    sync::ktest::TESTS,
    boot::ktest::TESTS,
    time::ktest::TESTS,
    smp::ktest::TESTS,
    sched::ktest::TESTS,
    irq::ktest::TESTS,
    console::ktest::TESTS,
    shell::ktest::TESTS,
    dev::ktest::TESTS,
    block::ktest::TESTS,
    drivers::ktest::TESTS,
    fs::ktest::TESTS,
];

/// Name of the registry's kernel thread.
const REGISTRY_NAME: &str = "ktest";
/// The registry's stack: 64 KiB, the boot stack size Limine guarantees
/// (ROADMAP §10.2).
const REGISTRY_STACK_PAGES: usize = 16;

/// The registry thread's id, `u32::MAX` until it starts.
static REGISTRY_TID: AtomicU32 = AtomicU32::new(u32::MAX);

/// The id of the thread [`registry_main`] runs on.
pub(crate) fn registry_tid() -> ThreadId {
    ThreadId(REGISTRY_TID.load(Ordering::Acquire))
}

/// Start the registry on its own kernel thread, pinned to CPU 0, and park
/// the bootstrap thread for good (DESIGN §8.2).
pub fn run() -> ! {
    if let Err(e) = thread_init::spawn_opts(
        REGISTRY_NAME,
        registry_main,
        thread_init::SpawnOpts {
            stack_pages: REGISTRY_STACK_PAGES,
            cpu: Some(0),
        },
    ) {
        panic!("ktest: registry thread: {}", e.as_str());
    }
    loop {
        thread_init::park(None);
    }
}

/// Every selected row, as (group, row, test), in run order.
fn rows() -> impl Iterator<Item = (usize, usize, &'static Test)> {
    GROUPS
        .iter()
        .enumerate()
        .flat_map(|(g, suite)| suite.iter().enumerate().map(move |(r, t)| (g, r, t)))
}

/// [`CURRENT`] when no test runs.
const NO_TEST: u32 = u32::MAX;

/// The running row as `group << 16 | row`, or [`NO_TEST`].
static CURRENT: AtomicU32 = AtomicU32::new(NO_TEST);

/// The running row's deadline in cycles of `Arch`'s counter; 0 when none is
/// armed. [`arm`] sets it, and [`disarm`] or the tick that finds it passed
/// ([`on_tick`]) clears it, whichever comes first.
static DEADLINE: AtomicU64 = AtomicU64::new(0);

/// Make row `r` of group `g` the running row and arm its deadline, `ms`
/// from now. False, and no deadline armed, while the cycle counter's
/// frequency is unknown.
fn arm(g: usize, r: usize, ms: u32) -> bool {
    CURRENT.store(((g as u32) << 16) | r as u32, Ordering::Relaxed);
    let Some(freq) = Arch::freq_hz() else {
        return false;
    };
    let at = Arch::now()
        .saturating_add(vibeos::ktest::deadline_cycles(ms, freq))
        .max(1);
    // Release: pairs with `on_tick`'s Acquire load and compare-exchange, so
    // a tick that sees this deadline sees the `CURRENT` stored above.
    DEADLINE.store(at, Ordering::Release);
    true
}

/// Clear the running row's deadline. If a tick already claimed it, that
/// tick is printing the failure and panicking, so wait for the panic.
fn disarm() {
    // AcqRel: the swap and `on_tick`'s compare-exchange are the one pair of
    // claims on the armed value; exactly one of them takes it.
    if DEADLINE.swap(0, Ordering::AcqRel) == 0 {
        loop {
            core::hint::spin_loop();
        }
    }
    CURRENT.store(NO_TEST, Ordering::Relaxed);
}

/// Every CPU's timer tick (`sched_init::on_timer_tick`): fail the running
/// test once its deadline has passed. Lock-free, so a test that hangs with
/// IF=0 on one CPU is caught by another CPU's tick (ROADMAP §10.2, T1).
/// The one tick that claims the passed deadline prints
/// `vibeOS: ktest: FAIL <name>: deadline` and panics, so the dump shows
/// where the test stood.
pub(crate) fn on_tick() {
    // Acquire: pairs with `arm`'s Release store.
    let d = DEADLINE.load(Ordering::Acquire);
    if d == 0 || Arch::now() < d {
        return;
    }
    // Acquire on success: pairs with `arm`'s Release store, as above; the
    // failure value is unused.
    if DEADLINE
        .compare_exchange(d, 0, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let name = current_name();
    crate::marker!("vibeOS: ktest: FAIL {name}: deadline");
    panic!("ktest: {name}: deadline");
}

/// The row `cur` names, if any.
fn row_of(cur: u32) -> Option<&'static Test> {
    GROUPS
        .get((cur >> 16) as usize)?
        .get((cur & 0xFFFF) as usize)
}

/// The running test's name, or `ktest` between tests.
fn current_name() -> &'static str {
    row_of(CURRENT.load(Ordering::Relaxed)).map_or(REGISTRY_NAME, |t| t.name)
}

/// Print `vibeOS: ktest: info <test>: <text>`, a counter or a measurement
/// that is never a result (DESIGN §8.2). Use [`crate::ktest_info!`].
pub(crate) fn info(args: fmt::Arguments<'_>) {
    let name = current_name();
    crate::marker!("vibeOS: ktest: info {name}: {args}");
}

/// `ktest::info` with `format_args!`: an info line for the running test.
#[macro_export]
macro_rules! ktest_info {
    ($($arg:tt)*) => {
        $crate::ktest::info(format_args!($($arg)*))
    };
}

/// Run every selected row with IF on and `irq_nest` 0, the context
/// production kernel threads run in, then exit QEMU (DESIGN §8.2).
fn registry_main() {
    REGISTRY_TID.store(thread_init::current_id().0, Ordering::Release);
    let cmdline = crate::boot::cmdline();
    let sel = vibeos::ktest::Selection::parse(cmdline.get(OPT_KTEST));
    let repeat_arg = cmdline.get(OPT_REPEAT);
    let repeat = vibeos::ktest::parse_repeat(repeat_arg).unwrap_or_else(|_| bad_repeat(repeat_arg));
    let Some(n) = vibeos::ktest::run_count(
        rows().map(|(_, _, t)| (t.name, t.once, t.opt_in)),
        &sel,
        repeat,
    ) else {
        bad_repeat(repeat_arg);
    };
    if n == 0 {
        crate::marker!("vibeOS: ktest: begin {n}");
        crate::marker!("vibeOS: ktest: end");
        qemu_exit(EXIT_FAIL);
    }
    // Setup, before `begin`: its cost scales with the thread table and the
    // KVA node pool (ROADMAP §10.4), and the first test starts right after
    // `begin`.
    quiesce_frames();
    crate::marker!("vibeOS: ktest: begin {n}");
    let freq = Arch::freq_hz().unwrap_or(0);
    let mut failed = false;
    let mut runs: u32 = 0;
    for pass in 1..=repeat {
        for (g, r, t) in rows() {
            if !sel.selects(t.name, t.opt_in) || !vibeos::ktest::runs_in_pass(t.once, pass) {
                continue;
            }
            runs += 1;
            failed |= !run_one(g, r, t, freq);
        }
    }
    // `n` and the loop ask the same two predicates.
    assert_eq!(runs, n, "ktest: runs made != begin count");
    crate::sched::ktest::report();
    crate::sync::ktest::report_spins();
    #[cfg(feature = "irqoff")]
    crate::sched::irqoff::report();
    crate::marker!("vibeOS: ktest: end");
    qemu_exit(if failed { EXIT_FAIL } else { EXIT_PASS });
}

/// The command-line options that select and repeat rows (BOOT.md §3.2).
const OPT_KTEST: &str = "vibeos.ktest";
const OPT_REPEAT: &str = "vibeos.ktest_repeat";

/// A `vibeos.ktest_repeat=` that is not 1 to `REPEAT_MAX`, or that makes
/// more runs than a `u32` counts: say so before `begin` and fail the boot.
fn bad_repeat(value: Option<&[u8]>) -> ! {
    let v = vibeos::boot::cmdline::Escaped(value.unwrap_or(b""));
    crate::marker!("vibeOS: ktest: bad option {OPT_REPEAT}={v}");
    qemu_exit(EXIT_FAIL);
}

/// One run of row `r` of group `g`: its run line, the body, and its result
/// line. False when it failed.
fn run_one(g: usize, r: usize, t: &'static Test, freq: u64) -> bool {
    let name = t.name;
    crate::marker!("vibeOS: ktest: run {name} {}", t.deadline_ms);
    let armed = arm(g, r, t.deadline_ms);
    let t0 = Arch::now();
    let mut outcome = (t.run)();
    let us = vibeos::ktest::cycles_to_us(Arch::now().wrapping_sub(t0), freq);
    if armed {
        disarm();
    } else {
        CURRENT.store(NO_TEST, Ordering::Relaxed);
    }
    // A test that needs interrupts off takes its own guard and drops it
    // before it returns.
    // The depth is read with IF=0, so it is this CPU's (DESIGN §2.9 rule 5).
    let if_on = interrupts_enabled();
    irq_disable();
    let cpu = per_cpu_init::current();
    let nest = cpu.irq_nest.load(Ordering::Relaxed);
    if !if_on || nest != 0 {
        cpu.irq_nest.store(0, Ordering::Relaxed);
        if !matches!(outcome, Outcome::Fail(_) | Outcome::FailFmt(_)) {
            outcome = crate::fail_fmt!("left IF={} irq_nest={}", u8::from(if_on), nest);
        }
    }
    irq_enable();
    match outcome {
        Outcome::Ok => {
            crate::marker!("vibeOS: ktest: ok {name} ({us} us)");
            true
        }
        Outcome::Fail(why) => {
            // Same shape as skip: reason on the protocol line so
            // check_ktest_output (which raises on that line alone) is
            // enough to diagnose (DESIGN §8.2).
            crate::marker!("vibeOS: ktest: FAIL {name}: {why}");
            false
        }
        Outcome::FailFmt(msg) => {
            let why = msg.as_str();
            crate::marker!("vibeOS: ktest: FAIL {name}: {why}");
            false
        }
        Outcome::Skip(reason) => {
            crate::marker!("vibeOS: ktest: skip {name}: {reason}");
            true
        }
    }
}

/// Pages per stack in [`quiesce_frames`]' KVA walk: 17 pages of VA a round,
/// so one walk between two coalesces covers `MAX_KVA_RANGES` times that.
const WARM_STACK_PAGES: usize = 16;
/// Ceiling on walk rounds: two coalesces take at most two free lists' worth.
const WARM_ROUNDS: usize = 3 * vibeos::limits::MAX_KVA_RANGES;
/// Default-size stacks the warm-up allocates and frees: past one free-list
/// coalesce, since the node pool is `MAX_KVA_RANGES`.
const WARM_DEFAULT_STACKS: usize = 2 * vibeos::limits::MAX_KVA_RANGES;
/// Ceiling on [`settle_threads`]' wait.
const SETTLE_MS: u64 = 2_000;
/// Thread slots [`quiesce_frames`] leaves empty: `thread_init::adopt_ap_idle`
/// takes only an empty slot, and `failed_ap_cleanup` calls it twice.
const EMPTY_SLOT_RESERVE: usize = 2;

/// Set once [`quiesce_frames`] has run; it runs once per boot.
static WARMED: AtomicBool = AtomicBool::new(false);

/// Shared setup before the first frame-accounting test (ROADMAP §10.2,
/// F074): after it, `free_frames()` moves only for what a test itself
/// allocates and frees, so the tests compare against a quiescent baseline
/// and a leak in their window still shows.
///
/// Four things move the count outside a test's window. A thread spawned
/// before the registry (the boot `/hello`, whose `wait_kernel` returns at
/// the reap, before the thread parks its stack) can still be running or
/// have its stack on its CPU's dead list. A spawn into an empty
/// thread slot boxes a new `Tcb`, which can grow the heap. A stack or vmap
/// carved from KVA that no mapping has reached before takes a page-table
/// page that `unmap` never frees. A test's first user processes allocate
/// their address spaces and tables from the heap, which grows to hold
/// them and never shrinks. So: let every pending thread finish and
/// its stack come back; fill the empty thread slots, all but
/// [`EMPTY_SLOT_RESERVE`], with threads that exit at once, so later spawns
/// reuse Dead boxes; walk KVA through two coalesces with the timer on, so
/// the free list starts again at VA the walk mapped; allocate and free
/// [`WARM_DEFAULT_STACKS`] default-size stacks; and run a process that
/// forks and reaps a child ([`user::warm_processes`]). It runs once per boot, from
/// the registry or from the first `quiescent_free_frames` caller,
/// which then waits for the threads and stacks to settle before it reads
/// the count.
pub(crate) fn quiesce_frames() {
    if WARMED.swap(true, Ordering::AcqRel) {
        return;
    }
    if !settle_threads() {
        crate::marker!("vibeOS: ktest:   warm-up: threads did not settle");
    }
    let mut filled = 0usize;
    thread_init::each_thread(|_| filled += 1);
    let empty = thread_init::table_usage()
        .1
        .saturating_sub(filled)
        .saturating_sub(EMPTY_SLOT_RESERVE);
    // Under the guard none of these runs, dies, and frees its slot for the
    // next spawn before every empty slot has a Tcb.
    {
        let _g = crate::sched::irqoff::deliberate("ktest TCB slot warm-up");
        let mut i = 0;
        while i < empty {
            if thread_init::spawn_here("warm", dying_entry).is_err() {
                break;
            }
            i += 1;
        }
    }
    if !settle_threads() {
        crate::marker!("vibeOS: ktest:   warm-up: warm threads did not exit");
    }
    // The first coalesce can come after a round or two, when the free list
    // is nearly full already; the second comes after a full list of rounds,
    // so the VA it merges back to the list's head is mapped past that.
    let (coalesces, rounds) = warm_kva();
    if coalesces < 2 {
        crate::marker!("vibeOS: ktest:   warm-up: kva coalesces {coalesces} in {rounds} rounds");
    }
    // Then the size every default spawn takes, directly, so a spawn that
    // misses its CPU's stack cache maps VA the warm-up mapped.
    let mut i = 0;
    while i < WARM_DEFAULT_STACKS {
        let Ok(stack) = kva_init::alloc_guarded_stack(vibeos::kva::DEFAULT_STACK_PAGES) else {
            crate::marker!("vibeOS: ktest:   warm-up: default stack {i} failed");
            break;
        };
        kva_init::free_stack(stack);
        i += 1;
    }
    if !user::warm_processes() {
        crate::marker!("vibeOS: ktest:   warm-up: user process failed");
    }
}

/// Allocate and free guarded stacks until `Kva::free` has coalesced twice.
/// The timer stays on: at `-smp 4` each round's shootdowns take long enough
/// that an IF-off walk loses PIT ticks. Returns (coalesces, rounds).
fn warm_kva() -> (usize, usize) {
    let mut coalesces = 0;
    let mut rounds = 0;
    while coalesces < 2 && rounds < WARM_ROUNDS {
        let Ok(stack) = kva_init::alloc_guarded_stack(WARM_STACK_PAGES) else {
            break;
        };
        let n = kva_init::stats().free_ranges;
        kva_init::free_stack(stack);
        // A free adds one range unless `Kva::free` ran its coalesce.
        if kva_init::stats().free_ranges <= n {
            coalesces += 1;
        }
        rounds += 1;
    }
    (coalesces, rounds)
}

/// With the timer on, sleep until no thread but this one and the idle
/// threads is Ready or Running and no dead thread's stack is still on its
/// way to a stack cache or back to the buddy. In a run, the run's deadline
/// bounds the wait ([`sleep_for`]): the stacks come back at the host's
/// rate, which no fixed bound fits (ROADMAP §10.2). Outside one, in the
/// registry's warm-up, [`SETTLE_MS`] does. False if the bound came first.
pub(crate) fn settle_threads() -> bool {
    let me = thread_init::current_id();
    let settled = || {
        let mut busy = thread_init::stacks_in_flight() != 0;
        thread_init::each_thread(|t| {
            busy |= t.id != me
                && t.name != "idle"
                && matches!(t.state, ThreadState::Ready | ThreadState::Running);
        });
        !busy
    };
    if deadline_near().is_some() {
        return sleep_for(settled);
    }
    let t0 = time_init::uptime_ms();
    loop {
        if settled() {
            break true;
        }
        if time_init::uptime_ms().saturating_sub(t0) > SETTLE_MS {
            break false;
        }
        thread_init::sleep_ms(1);
    }
}

/// Whether a record in the log ring holds `needle`.
pub(crate) fn log_contains(needle: &str) -> bool {
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    let mut found = false;
    crate::log_init::for_each_msg(|m| found |= m.windows(n.len()).any(|w| w == n));
    found
}

/// Free frames: the buddy's, and those of the stacks the CPUs' stack caches
/// hold, which a spawn reuses (ROADMAP §10.10).
pub(crate) fn free_frames() -> usize {
    pmm_init::with_buddy(|b| b.stats().free_frames) + thread_init::cached_stack_frames()
}

pub(crate) fn alloc_frame() -> Option<PhysAddr> {
    alloc_frames(0)
}

pub(crate) fn free_frame(pa: PhysAddr) {
    // SAFETY: an unknown base frees nothing, and a held one is freed once
    // (`ktest::take_held` removes it), so the contract of
    // `ktest::dealloc_frames` holds for any `pa`.
    unsafe { dealloc_frames(pa, 0) };
}

pub(crate) struct Fault {
    #[cfg(target_arch = "x86_64")]
    pub(crate) cr2: u64,
    pub(crate) error: u64,
}

pub(crate) fn catch_fault<F: FnOnce()>(f: F) -> Option<Fault> {
    // A hit longjmps out of the #PF gate and skips the `iretq` that would
    // restore IF; the guard restores it.
    let _g = InterruptGuard::enter();
    arch::catch::catch(vectors::PF, f).map(|c| Fault {
        #[cfg(target_arch = "x86_64")]
        cr2: c.cr2,
        error: c.error,
    })
}

pub(crate) fn catch_alloc_error<F: FnOnce()>(f: F) -> bool {
    arch::catch::catch_alloc(f)
}

// Helpers for APIs that concurrent Phase 10 slices change. A new test
// reaches those APIs only through these; the slice that changes one
// updates its helper here (DESIGN §8.2).

pub(crate) fn spawn_thread(name: &'static str, entry: fn()) -> ThreadHandle {
    match thread_init::spawn(name, entry) {
        Ok(h) => h,
        Err(e) => panic!("ktest: spawn {name}: {}", e.as_str()),
    }
}

pub(crate) fn spawn_thread_on(name: &'static str, entry: fn(), cpu: u32) -> ThreadHandle {
    match thread_init::spawn_on(name, entry, cpu) {
        Ok(h) => h,
        Err(e) => panic!("ktest: spawn {name} on cpu{cpu}: {}", e.as_str()),
    }
}

/// Blocks the frame helpers handed out, as the `Frames` that own them.
/// The helpers trade bare addresses (C-SUITES), so the tokens wait here.
/// Never held together with BUDDY.
static HELD: SpinMutex<[Option<Frames>; 16]> =
    SpinMutex::with_rank([const { None }; 16], RANK_DEVICE);

/// Remove and return the held token whose base is `pa`.
fn take_held(pa: PhysAddr) -> Option<Frames> {
    let mut held = HELD.lock();
    let slot = held
        .iter_mut()
        .find(|s| s.as_ref().is_some_and(|f| f.base() == pa.as_u64()))?;
    slot.take()
}

/// A naturally aligned block of `1 << order` frames. `None` when the
/// buddy is out, or when 16 blocks are already out through these helpers.
pub(crate) fn alloc_frames(order: u8) -> Option<PhysAddr> {
    let f = pmm_init::with_buddy(|b| b.alloc(order))?;
    let pa = PhysAddr(f.base());
    let spare = {
        let mut held = HELD.lock();
        match held.iter_mut().find(|s| s.is_none()) {
            Some(slot) => {
                *slot = Some(f);
                None
            }
            None => Some(f),
        }
    };
    match spare {
        None => Some(pa),
        Some(f) => {
            pmm_init::with_buddy(|b| b.free(f));
            None
        }
    }
}

/// Free a block [`alloc_frames`] returned. A base it did not hand out, or
/// already freed, is ignored.
///
/// # Safety
/// `pa` is a block that [`alloc_frames`] returned for this same `order`, and
/// nothing still maps or uses it.
pub(crate) unsafe fn dealloc_frames(pa: PhysAddr, order: u8) {
    if let Some(f) = take_held(pa) {
        debug_assert_eq!(f.order(), order, "ktest: dealloc_frames order");
        pmm_init::with_buddy(|b| b.free(f));
    }
}

/// A naturally aligned block of `1 << order` frames as its owning
/// `Frames`, for an API that takes the token (`kva_init::vmap`). The
/// caller gives it back with [`free_frames_owned`].
pub(crate) fn alloc_frames_owned(order: u8) -> Option<Frames> {
    pmm_init::with_buddy(|b| b.alloc(order))
}

/// Free a block [`alloc_frames_owned`] returned, once nothing maps it.
pub(crate) fn free_frames_owned(f: Frames) {
    pmm_init::with_buddy(|b| b.free(f));
}

pub(crate) fn cpu_remote(id: u32) -> Option<&'static PerCpuRemote> {
    per_cpu_init::cpu(id)
}

pub(crate) fn dying_entry() {}

/// Set this CPU's `irq_nest` to `n` after an `arch::catch` longjmp skipped
/// the guards that would have dropped it. The store runs with IF=0, so it
/// lands on the slot of the CPU the caller runs on (DESIGN §2.9 rule 5),
/// and IF is left as the caller had it.
pub(crate) fn restore_irq_nest(n: u32) {
    let if_on = interrupts_enabled();
    irq_disable();
    per_cpu_init::current().irq_nest.store(n, Ordering::Relaxed);
    if if_on {
        irq_enable();
    }
}

pub(crate) fn second_cpu() -> Option<u32> {
    let mask = per_cpu_init::online_mask();
    let mut i = 1u32;
    while i < 64 {
        if mask & (1u64 << i) != 0 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// `ipi_init::service_incoming` from thread context. With IF on, an IPI
/// between `service_calls`' acked check and its ack would run the callback
/// twice, so this holds IF off across the call.
pub(crate) fn service_incoming_guarded() {
    let _g = InterruptGuard::enter();
    ipi_init::service_incoming();
}

pub(crate) fn spin_until_ns(pred: impl Fn() -> bool, ns: u64) -> bool {
    let t0 = time_init::now_ns();
    while !pred() {
        if time_init::now_ns().saturating_sub(t0) > ns {
            return false;
        }
        service_incoming_guarded();
        core::hint::spin_loop();
    }
    true
}

pub(crate) fn mmio_r32(va: u64, off: u32) -> u32 {
    // SAFETY: invariant I58: every caller passes a device's BAR 0 VA,
    // which the `bar-test` driver claimed and mapped uncached, and a
    // register offset inside that BAR; established by `ktest::bar0_va`.
    unsafe { core::ptr::read_volatile((va.wrapping_add(off as u64)) as *const u32) }
}

pub(crate) fn mmio_w32(va: u64, off: u32, val: u32) {
    // SAFETY: invariant: as for `mmio_r32`; established by `ktest::bar0_va`.
    unsafe { core::ptr::write_volatile((va.wrapping_add(off as u64)) as *mut u32, val) }
}

pub(crate) const EDU_IDENT: u32 = 0x00;
pub(crate) const EDU_IDENT_VAL: u32 = 0x0100_00ED;

/// BAR 0's VA for edu or e1000e, which the `kernel_tests` driver
/// `bar-test` claims and maps (binding it on first use); `None` when that
/// driver does not hold the BAR.
pub(crate) fn bar0_va(dev: &DevRef) -> Option<u64> {
    crate::dev::ktest::bind_bar_test_driver();
    crate::dev_init::bar_va(dev, 0)
}

pub(crate) fn find_edu() -> Option<DevRef> {
    // QEMU 8.x edu is 1234:11e8 (old QEMU vendor). Later trees use 1b36:11e8.
    crate::dev::ktest::find_id(0x1234, 0x11e8)
        .or_else(|| crate::dev::ktest::find_id(0x1b36, 0x11e8))
}

/// The free-frame count at a quiescent point (ROADMAP §10.2, F074): the
/// shared warm-up has run (once per boot, from whichever caller comes
/// first), no thread but the caller and the idle threads is runnable, and
/// no dead thread's stack is still on its way back. Every frame-accounting
/// test takes its `before` and `after` from here.
pub(crate) fn quiescent_free_frames() -> usize {
    settle();
    free_frames()
}

/// Run the shared warm-up, then wait for [`quiesce`], saying so if it
/// timed out.
fn settle() {
    quiesce_frames();
    if !quiesce() {
        crate::marker!("vibeOS: ktest:   quiesce: threads did not settle");
    }
}

/// What `lifetime_stack_reclaim` accounts for, read at a quiescent point.
///
/// The kernel page tables are counted with the free frames. Each stack that
/// misses its CPU's stack cache is carved from KVA, and `Kva` hands out VA
/// first-fit over a list that frees append to and that is merged in address
/// order only when its node pool runs out; after each merge, the fresh VA
/// starts above the highest stack still live, and which stacks those are
/// (the ones the CPUs' stack caches hold, the ones still running or on a
/// dead list) depends on timing. So the highest VA the test maps moves by
/// several MiB from run to run, and each 2 MiB span it reaches for the first
/// time takes a table page, which stays in the kernel tables for good
/// (`mm::ktest::table_pages`) and is no frame lost. No warm-up can map the
/// span ahead, since nothing bounds it but the KVA window.
pub(crate) struct FrameCount {
    /// The buddy's free frames.
    buddy: usize,
    /// Frames of the stacks the stack caches hold.
    cached: usize,
    /// Page-table pages the kernel mapper has taken since boot.
    tables: usize,
    /// Mapped heap pages, for the failure line. They are not added back:
    /// heap growth in the test's window takes buddy frames and fails it.
    heap: usize,
    /// The rest are for the failure line alone, each naming one place the
    /// frames can be: dead threads' stacks not yet cached or freed when
    /// [`settle`] returned (non-zero only when it timed out), KVA bytes
    /// reserved, and frames of dropped `Frames` tokens.
    in_flight: usize,
    kva_used: u64,
    dropped: usize,
}

impl FrameCount {
    pub(crate) fn quiescent() -> Self {
        settle();
        Self {
            buddy: pmm_init::with_buddy(|b| b.stats().free_frames),
            cached: thread_init::cached_stack_frames(),
            tables: crate::mm::ktest::table_pages(),
            heap: crate::heap_init::stats().capacity / vibeos::paging::PAGE_SIZE_4K as usize,
            in_flight: thread_init::stacks_in_flight(),
            kva_used: kva_init::stats().used,
            dropped: vibeos::pmm::leaked_frames(),
        }
    }

    /// The frames free ([`free_frames`]) or in kernel page tables.
    fn total(&self) -> usize {
        self.buddy + self.cached + self.tables
    }

    /// `Ok` when `after` accounts for as many frames as `self`; otherwise a
    /// failure line that names every count before and after, and, after,
    /// the stacks in flight, the KVA KiB used, and the dropped
    /// frames, in [`FAIL_MSG_BYTES`].
    pub(crate) fn unchanged(&self, after: &Self) -> Outcome {
        if after.total() == self.total() {
            return Outcome::Ok;
        }
        crate::fail_fmt!(
            "frames {}->{} buddy {}->{} cache {}->{} pt {}->{} heap {}->{} fly {} kva {}k drop {}",
            self.total(),
            after.total(),
            self.buddy,
            after.buddy,
            self.cached,
            after.cached,
            self.tables,
            after.tables,
            self.heap,
            after.heap,
            after.in_flight,
            after.kva_used / 1024,
            after.dropped.saturating_sub(self.dropped),
        )
    }
}

/// Wait, bounded, until no thread but this one and the idle threads is
/// Ready or Running and no dead thread's stack sits in a CPU's dead-stack
/// slot or on its dead list. False if that did not happen in time.
pub(crate) fn quiesce() -> bool {
    settle_threads()
}

/// The File API with the copyable [`FileId`] handles the earlier suites
/// were written against: each call takes back, or hands out, the count a
/// [`FileRef`] carries, so their scenarios and assertions stay as they
/// were.
pub(crate) mod fid {
    use vibeos::fs::{FileId, FileRef, FsError, OpenFlags, SeekFrom, Stat};

    use crate::file_init;

    pub(crate) fn open(path: &str, flags: u32, mode: u32) -> Result<FileId, FsError> {
        file_init::open(path.as_bytes(), OpenFlags::from_bits(flags), mode).map(FileRef::into_raw)
    }

    pub(crate) fn read(id: FileId, buf: &mut [u8]) -> Result<usize, FsError> {
        file_init::read(&FileRef::from_raw(id), buf)
    }

    pub(crate) fn write(id: FileId, buf: &[u8]) -> Result<usize, FsError> {
        file_init::write(&FileRef::from_raw(id), buf)
    }

    pub(crate) fn seek(id: FileId, off: i64, whence: u32) -> Result<u64, FsError> {
        let pos = SeekFrom::from_whence(off, whence)?;
        file_init::seek(&FileRef::from_raw(id), pos)
    }

    pub(crate) fn close(id: FileId) -> Result<(), FsError> {
        file_init::close(FileRef::from_raw(id))
    }

    pub(crate) fn addref(id: FileId) -> Result<(), FsError> {
        file_init::addref(id)
    }

    pub(crate) fn stat_path(path: &str) -> Result<Stat, FsError> {
        file_init::stat_path(path.as_bytes())
    }

    /// `lstat` of absolute `path`.
    pub(crate) fn lstat_path(path: &str) -> Result<Stat, FsError> {
        crate::fs_init::api().stat_path(None, path.as_bytes(), false)
    }

    pub(crate) fn creat(path: &str) -> Result<(), FsError> {
        file_init::creat(path.as_bytes())
    }

    pub(crate) fn unlink_path(path: &str, rmdir: bool) -> Result<(), FsError> {
        if rmdir {
            file_init::rmdir(path.as_bytes())
        } else {
            file_init::unlink(path.as_bytes())
        }
    }
}

/// CPUID.01H:ECX[31] (a hypervisor is present) and leaf `0x4000_0000`
/// naming it `KVMKVMKVM\0\0\0`.
#[cfg(target_arch = "x86_64")]
pub(crate) fn on_kvm() -> bool {
    let (_, _, ecx1, _) = x86::cpuid(1, 0);
    if ecx1 & (1 << 31) == 0 {
        return false;
    }
    let (_, b, c, d) = x86::cpuid(0x4000_0000, 0);
    let mut id = [0u8; 12];
    id[..4].copy_from_slice(&b.to_le_bytes());
    id[4..8].copy_from_slice(&c.to_le_bytes());
    id[8..].copy_from_slice(&d.to_le_bytes());
    &id == b"KVMKVMKVM\0\0\0"
}

/// Sleep until `pred` holds, for at most `ms`.
pub(crate) fn sleep_until(pred: impl Fn() -> bool, ms: u64) -> bool {
    let deadline = time_init::now_ns().saturating_add(ms.saturating_mul(1_000_000));
    while !pred() {
        if time_init::now_ns() >= deadline {
            return false;
        }
        thread_init::sleep_ms(1);
    }
    true
}

/// Sleep until `pred` holds, for at most `ms`.
pub(crate) fn sleep_until_s19(pred: impl Fn() -> bool, ms: u64) -> bool {
    let deadline = time_init::now_ns().saturating_add(ms.saturating_mul(1_000_000));
    while !pred() {
        if time_init::now_ns() >= deadline {
            return false;
        }
        thread_init::sleep_ms(1);
    }
    true
}

/// How long before the running row's deadline [`wait_for`] gives up, so the
/// caller's failure line, which names what it waited on, comes before the
/// tick that fails the run on its deadline ([`on_tick`]).
const WAIT_MARGIN_MS: u32 = 500;

/// Yield until `pred` holds, with no time bound of the caller's own: the
/// running row's deadline bounds the wait, however slowly the host runs the
/// guest (ROADMAP §10.2). True once `pred` holds; false when that deadline
/// is [`WAIT_MARGIN_MS`] away and `pred` still fails, so the caller can fail
/// naming what it waited on. With no deadline armed it waits on, and the
/// harness's run deadline is the backstop.
pub(crate) fn wait_for(pred: impl Fn() -> bool) -> bool {
    loop {
        if pred() {
            return true;
        }
        if deadline_near() == Some(true) {
            return pred();
        }
        thread_init::yield_now();
    }
}

/// [`wait_for`], sleeping 1 ms between checks rather than yielding: for a
/// waiter whose CPU also runs the work it waits on, which a yield would
/// take turns with.
pub(crate) fn sleep_for(pred: impl Fn() -> bool) -> bool {
    loop {
        if pred() {
            return true;
        }
        if deadline_near() == Some(true) {
            return pred();
        }
        thread_init::sleep_ms(1);
    }
}

/// Whether the running row's deadline is [`WAIT_MARGIN_MS`] away or closer;
/// `None` when no deadline is armed.
fn deadline_near() -> Option<bool> {
    deadline_within(WAIT_MARGIN_MS)
}

/// Whether the running row's deadline is `ms` away or closer; `None` when
/// no deadline is armed. A loop of repeated work stops starting rounds by
/// it, so the run's deadline, not a count, bounds it on a slow host.
pub(crate) fn deadline_within(ms: u32) -> Option<bool> {
    // Acquire: pairs with `arm`'s Release store, as in `on_tick`.
    let d = DEADLINE.load(Ordering::Acquire);
    if d == 0 {
        return None;
    }
    let margin = Arch::freq_hz().map_or(0, |f| vibeos::ktest::deadline_cycles(ms, f));
    Some(Arch::now().saturating_add(margin) >= d)
}

/// Spin on TSC time until `pred` holds, for at most `ns`.
pub(crate) fn spin_until(pred: impl Fn() -> bool, ns: u64) -> bool {
    let t0 = time_init::now_ns();
    while !pred() {
        if time_init::now_ns().saturating_sub(t0) > ns {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}
