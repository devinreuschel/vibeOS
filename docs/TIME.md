# 6. Time

Index: [DESIGN.md](DESIGN.md). This file holds DESIGN §6, and its headings keep DESIGN's numbers.

Four hardware clocks, none of them good at everything. The PIT is slow and legacy but always there.
The HPET is a reliable counter with no interrupts we want. The TSC is fast and fine-grained but needs
calibration. The LAPIC timer is per-CPU and is what actually drives preemption. Those four are
x86_64's (§6.1 to §6.3); aarch64 has one clock, the generic timer (ROADMAP §11.3), and §6.4 to §6.6
hold on both architectures.

## 6.1 Roles and constants

| Source | Used for |
|--------|----------|
| PIT channel 0 | Bootstrap tick at ~1 kHz. Last-resort scheduler tick if the LAPIC timer cannot be used. |
| PIT channel 2 | TSC calibration when there is no HPET. One-shot, gated through port `0x61`. |
| HPET main counter | Preferred TSC calibration reference. Monotonic, known frequency from the ACPI table. The clocksource, second in §6.4's rank: when the TSC is not an invariant, warp-clean one. |
| ACPI PM timer | The clocksource, third in §6.4's rank: with neither an invariant TSC nor an HPET. 3.579545 MHz, 24 or 32 bits, at the FADT's `X_PM_TMR_BLK`, or `PM_TMR_BLK` when that is zero. |
| TSC | `busy_wait_ms`, the TSC-deadline arm, trace timestamps. The clocksource, first in §6.4's rank: when CPUID reports it invariant and the warp test saw no backward step. |
| LAPIC timer | Per-CPU preemption tick. TSC-deadline mode preferred. |
| RTC / CMOS | Wall clock date and time, read once at boot. |
| Generic timer `CNTVCT_EL0` (aarch64) | The cycle counter and the clocksource. Read after `isb`. `mult`/`shift` from `CNTFRQ_EL0`, or the timer node's `clock-frequency`. |
| Generic timer TVAL (aarch64) | The preemption tick. EL1 uses `CNTV_*` (`virt`). EL2+VHE uses `CNTHV_*` (`hyp-virt`) when the node has a fifth interrupt, otherwise `CNTHP_*` (`hyp-phys`). |

| Value | Meaning |
|-------|---------|
| `1_193_182` | PIT input frequency in Hz |
| `1_000` | Target tick rate, so PIT divisor is 1193 |
| `11_932` | PIT channel 2 count for a ~10 ms calibration window |
| `10` | Scheduling quantum in ms of the running thread's TSC time (`QUANTUM_MS`), checked at each local timer tick; a run-time quantum, not a tick count, so late ticks lengthen it by one tick's lateness at most (§6.3) |
| `0x40` / `0x42` / `0x43` | PIT channel 0 data / channel 2 data / command |
| `0x61` | Speaker gate. Bit 0 enables channel 2, bit 5 reads its output. |
| `CPUID.01H:ECX[24]` | TSC-deadline feature bit; the `IA32_TSC_DEADLINE` MSR is in §7.2 |

## 6.2 Calibrating the TSC

Read the reference counter, spin for a known interval, read again, divide. The reference is the HPET
main counter when ACPI provides an HPET table, otherwise PIT channel 2 over 10 ms windows.

A PIT window's ends are events, not counter reads: the gate write starts the one-shot and a poll of
port `0x61` finds its end, so a vCPU or SMI stall between either and its TSC read lengthens or
shortens the window unseen; under TCG one window moved the rate by up to 4%. `calibrate_pit`
therefore brackets each end with TSC reads (before and after the gate write; before the last poll
that found OUT low and after the one that found it high), keeps only a window whose brackets are
within 0.05% of its length (`PitWindow::tight_len`), measures until five are kept or ten ran, and
takes their median (`time::tsc_per_ms_from_pit_windows`); with none kept it takes the median of all.
That costs 50 ms of boot where the HPET is absent. The HPET window's ends are counter reads, so
`calibrate_hpet` takes each as the read the TSC brackets most tightly of 16 (TSC, HPET, TSC) triples
and places it by its bracket's middle (`time::tsc_per_ms_from_hpet_brackets`); reading the TSC only
after the first HPET read and before the last one read the rate low, by up to 0.5% under TCG.

Parse the ACPI HPET table properly: reject an address of zero and reject a generic address structure
that claims I/O space rather than system memory. Both appear in the wild and both produce a
"calibration" that is pure garbage, which then poisons every delay in the kernel including the ones AP
bring-up depends on.

Serialize around `rdtsc`. Out-of-order execution can move the read across the interval boundary. Use
`lfence` before, or `rdtscp`, which serializes on its own and also gives the CPU number.

The BSP calibrates `tsc_per_ms` once (`time_init::init`), and every CPU uses that value through
`time_init::tsc_per_ms()`: delays, the TSC-deadline arm, and `now_ns` when the TSC is the clocksource
(§6.4). The LAPIC periodic
count is also measured once, on the BSP, and `apic_init::arm_ap` reuses it on every AP. Both assume
one TSC rate and one LAPIC timer rate on every CPU. `time_init::init` checks the invariant TSC CPUID
bit and prints `vibeOS: time: invariant tsc absent` when it is clear, because everything downstream
assumes the TSC does not change rate. Each AP measures its TSC against the BSP's at bring-up with a
warp test ([DESIGN §7.4](SMP.md#74-ap-bring-up-sequence)), and `vibeOS: smp: tsc skew <n> cycles`
reports the largest backward step it saw.

## 6.3 The tick

The scheduler needs a periodic interrupt. Preference order:

```
TSC-deadline mode      CPUID.01H:ECX[24] set
  LVT timer mode bits 17:18 = 10b, write IA32_TSC_DEADLINE each tick
LAPIC periodic mode    LAPIC present, no TSC-deadline
  calibrate lapic_ticks_per_ms against the HPET, divider 16, periodic LVT
PIT IRQ0               no usable LAPIC
  ~1 kHz on vector 0x20, single global tick, no per-CPU preemption
```

Arming TSC-deadline: write the LVT timer register, then `MFENCE` (or another
serializing instruction), then `IA32_TSC_DEADLINE`. `lfence;rdtsc` / `rdtscp` do
not drain the UC LVT store (SDM Vol. 3A). Rearm on the IRQ path only writes the
MSR; LVT is already in deadline mode.

Each fallback is worse than the one above it, and all three must work. The per-push tiers run two
of them, and the nightly KVM leg the third. Under TCG, QEMU never advertises `CPUID.01H:ECX[24]`, so
`make test-kernel` (`-cpu max`) and `make test-lapic-fallback` (`-cpu qemu64,-tsc-deadline`) both take
the periodic path, and `make test-e2e-pit` (`-machine pc,hpet=off`) boots on the PIT tick.
`arm_tsc_deadline`, `rearm_deadline`, and the `TscDeadline` arm of `apic_init::arm_ap` run only under
KVM or on hardware, so no per-push tier runs them; the nightly KVM leg runs them under
`-cpu max,+invtsc`, where `percpu_ticks_advance` finds every CPU's ticks advancing (ROADMAP §10.1,
F078). Under KVM, `-cpu qemu64,-tsc-deadline` forces the periodic path.

`apic_init::calib_periodic` measures the LAPIC count against the HPET as `calibrate_hpet` measures
the TSC: each end of the 10 ms window is the count read the HPET brackets most tightly of 16,
placed by its bracket's middle, and the rate divides by the HPET ticks between the two ends
(`vibeos::apic::lapic_per_ms`), not by the 10 ms planned. Dividing by the plan let a host that
stalled QEMU past the window's end lengthen the period by the stall: 80 ms stalls armed 8 to 9 ms
ticks.

`apic_init::prove` decides whether an armed mode works from the interrupts that arrive, not from
the time that passes. It unmasks the PIT on PIC IRQ0 through LINT0 ExtINT, the path the PIT
fallback uses, and halts until the LAPIC timer fires, which makes that mode the tick, or until the
PIT has fired `PROVE_PIT_FIRES` (20) times and the timer has not, which sends `prove` on to the
next mode (`vibeos::apic::timer_proof`); IRQ0 is masked again after each wait. A fixed window
cannot decide it under TCG. The TSC, the HPET and the LAPIC's own counter all follow host time
there, but the timer's interrupt is raised only when QEMU's main loop runs, so a host that
deschedules QEMU for longer than the window lets the window pass before the guest is given the
interrupt: with a 50 ms TSC window, the scheduled macOS run of `make test-lapic-fallback` fell back
to the PIT that way, and a window of HPET time fails the same way. A host that holds back the
timer's interrupt holds back the PIT's with it, and QEMU delivers a pending LAPIC interrupt before
the PIC's, so a timer that works, armed for 1 ms, fires within the PIT's first few interrupts.
Where the PIT never interrupts, nothing shows a mode silent: a mode that never fires there stops
the boot in `prove` rather than handing on to the next.

For the same reason the in-guest tests take a tick's rate from the timer's count, not from its
interrupts' arrival times. QEMU computes the PIT's and the LAPIC timer's count from its clock when
the guest reads it, so the count shows the programmed period however late the host wakes the main
loop that raises the interrupts. On the scheduled macOS runner (an Apple M1 (Virtual) host, macOS
15.7.9, QEMU 11.1.1) a 1 ms sleep takes 5.3 ms at the median, and the 1 ms ticks reached the guest
every 5 to 10 ms of TSC time. `pit_tick_rate` and `lapic_timer_rearm`'s periodic mode read the period
as the median span between reloads of PIT channel 0's latched count, or of the LAPIC's
current-count register, against the TSC (`vibeos::time::ReloadSpans`), and they count interrupts
only to show the tick still arrives. The scheduler's quantum is run time for the same reason
([§6.1](#61-roles-and-constants), [SMP §7.8](SMP.md#78-per-cpu-scheduling)): counted in ticks, ten
of them took about 100 ms there.

When the LAPIC timer owns the tick, mask the PIT's GSI at the I/O APIC. Do not merely ignore its
interrupts.

## 6.4 Timekeeping API

```rust
now_ns()    -> u64      // the clocksource since boot, clamped monotonic
now_us()    -> u64      // now_ns() / 1_000
uptime_ms() -> u64      // now_ns() / 1_000_000
busy_wait_ms(ms: u64)   // TSC spin, hlt when interrupts are on. Boot and IPI delays only.
sleep_ms(ms: u64)       // parks the calling thread. Everything after the scheduler exists uses this.
```

Planned (ROADMAP §19.4): `sleep_ms` becomes a wrapper over a sleep to a nanosecond deadline on
[§6.5](#65-timers-and-timeouts)'s deadline timers, since every blocking primitive already takes a
nanosecond deadline.

`now_ns` reads a snapshot that CPU 0's tick writes: the clocksource's id, a read of it, and the
nanoseconds at that read. Read the fields unprotected and you eventually get one from before an
interrupt and one from after, producing a timestamp that goes backwards. That is not hypothetical; it
happened, when the fields were a tick count and a TSC stamp.

Publish them as a latched seqlock, which keeps two copies of the fields so that no reader waits for
the writer. The writer, CPU 0's tick and nothing else, makes each bump `fence(Release)`, a Relaxed
`fetch_add(1)` on the sequence, and `fence(Release)` again: it bumps the sequence to odd and stores
copy 0, then bumps it to even and stores copy 1. The trailing fence pairs with the reader's
`fence(Acquire)`, so a reader that loaded any store made after a bump sees that bump when it reloads
the sequence; the leading fence orders the previous copy's stores before the bump, for a reader whose
Acquire load of the sequence sees the bump and then reads that copy (Linux's
`raw_write_seqcount_latch` has a write barrier on both sides). The reader loads the sequence with
Acquire, loads the copy its low bit names (copy 1 while it is odd, when the writer is storing copy
0), issues `fence(Acquire)`, reloads the sequence, and retries only when it changed. A plain seqlock
reader retries while the sequence is odd, so one that interrupted the writer on its own CPU, in an
NMI, `#MC`, or `#DB` handler, a pseudo-NMI (ROADMAP §25.5), or the panic path, would spin forever;
the latched reader reads the copy the writer is not storing and returns. So `now_ns` may be read
from any context, a log record's timestamp included (§2.5). Linux's NMI-safe clock,
`ktime_get_mono_fast_ns`, is built the same way. x86's locked `fetch_add` is a full barrier and would
hide a missing fence; aarch64 with LL/SC atomics does not, so the fences are written out (ROADMAP
§10.8, F098). The host test `latch_read_mid_write_returns_older` stops the writer between its two
copies and reads the older value.

Two warnings about testing this. A test that computes the expected "now" from the same tick value it
just read is monotonic by construction and passes even with torn reads, so the test needs an
independently published timestamp to compare against. And a single-threaded test never sees the race,
so the in-guest coverage has to read the clock from threads that yield while a timer fires and compare
it with that independent timestamp. The in-guest tests `now_us_monotonic` and `now_us_under_yields`
do both. They read through `time::ktest::now_ns_unclamped`, a `kernel_tests` hook that calls
`TickClock::now_ns_with` directly and so skips `LAST_NS`, and match each read within 1 µs against the
snapshots CPU 0 publishes, before each seqlock write, into a ring the seqlock does not guard. The raw
reading may land 1 ns above the next base, which `LAST_NS` hides from `now_ns`, so the tests check the
match on the raw reading and check the clamped `now_us` for order. They run readers on every CPU that
call `yield_now` while the timer fires, and every 500th read stalls inside the seqlock window until two
ticks have been published, so the retry runs. `now_us_planted_tear` makes a stalled read pair one
snapshot's nanoseconds with a later snapshot's cycles, as a skipped retry would, and requires both
tests to fail.
The host tests `now_us_seqlock_retry_under_simulated_writer` and `seqlock_threaded_writer_never_tears`
still cover the retry in the portable half (ROADMAP §10.2, F100).

### Global monotonicity under SMP

Once every CPU has its own LAPIC timer, "the tick count" stops having a single writer. Options:

1. Only the BSP's timer updates the global counters; AP timers drive local scheduling only. Simple,
   and what the old tree did, but it makes `now_us` dependent on the BSP staying alive and awake.
2. Per-CPU time, with a global monotonic clock derived from the TSC alone once it is known invariant
   and synchronized.
3. A real distributed clock with cross-CPU synchronization and drift correction.

Option 1 was the first kernel's: only CPU 0's timer interrupt advanced a tick count and `now_ns`
interpolated from it, so a CPU 0 IF-off window longer than 1 ms, whose pending timer interrupts
coalesce into one, lost time for good. The kernel runs option 2, generalized: one clocksource, a
free-running counter chosen at boot that every CPU reads, and `now_ns = base_ns + ((read() -
base_cycles) mod 2^width) × mult >> shift`, evaluated in `u128` by `vibeos::time::ns_at`. CPU 0's
tick (`apic_init::on_timer_irq` calls `time_init::on_hw_tick` on CPU 0 alone) counts itself for
diagnostics, reads the clocksource, and publishes the new base through the latch, with the
clocksource's id, so a switch and its base publish as one value. `time::ClockWriter` computes each
base from the whole count since the clock started, so rounding never accumulates, and `LAST_NS` clamps
the result monotonic across the 1 ns a reader's split rounding can add. The tick drives scheduling and
timer expiry, and no count of timer interrupts enters the clock: an IF-off window or a late
TSC-deadline rearm loses no time (ROADMAP §10.3, F027). Candidates, best first:

| Clocksource | Architecture and condition | Width | A read |
|---|---|---|---|
| TSC | x86_64, when CPUID reports it invariant and ROADMAP §10.7's warp test saw no backward step | 64 | an instruction |
| KVM clock, Hyper-V reference page | x86_64 under that hypervisor (ROADMAP §21.4, §26.5) | 64 | a shared page and an instruction |
| HPET main counter | x86_64 with an ACPI HPET table | 32 or 64 | an MMIO load: an exit under KVM, QEMU's global lock under TCG |
| ACPI PM timer | x86_64 with the FADT's timer block | 24 or 32 | a port read: an exit under KVM |
| `CNTVCT_EL0` | aarch64, always | at least 56 | an instruction after `isb` |

x86_64 builds the TSC, HPET and PM timer rows. `time_init::init` ranks them (`time::rank`) and
publishes a provisional choice, since the AP warp tests run later, inside `smp_init::init`;
`time_init::confirm_clocksource`, which `kmain` calls right after it, ranks again, switches on CPU 0
through `ClockWriter::switch` (no step in `now_ns`) if the answer changed, and prints
`vibeOS: time: clocksource <tsc|hpet|acpi_pm>`, Linux's names. A boot with no candidate halts with
`vibeOS: time: no clocksource`; there is no tick-count fallback. The kernel reads the HPET
main counter's low 32 bits in one 4-byte access and treats it as 32 bits wide whatever `GCAP_ID` bit 13
says, as Linux does: QEMU serves an 8-byte read as two halves, and one that straddled the low word's
wrap (every 42.9 s at 100 MHz) read 2^32 ticks ahead, which `LAST_NS` then held for 42.9 s, stalling
every timed wait; the PM timer is 24 bits, or 32 with the FADT's
`TMR_VAL_EXT`, and QEMU's `pc` FADT is revision 1, with no `X_` fields, so its timer is found at
`PM_TMR_BLK`.

A counter narrower than 64 bits is read at least once per half wrap. CPU 0's tick does it, every
millisecond, against the 24-bit PM timer's half wrap of about 2.34 s. Rule: CPU 0 never holds IF off
for longer than that where the PM timer is the clocksource, since a whole wrap in one IF-off stretch
is lost silently; the PM-timer ktest boot runs only clock tests for that reason. Once idle stops the
tick (§6.6), the CPU that holds that duty hands it on before it stops its own tick, and no idle CPU
sleeps past half the wrap, the bound Linux calls `max_idle_ns`. The HPET and the PM timer cost an
exit per read on KVM without an invariant TSC, and the HPET takes QEMU's global lock under TCG, on
every `now_ns`, log records included, until the paravirtual clock lands; ROADMAP §19.3 measures the
cost, and if it shows, those two read once per tick and interpolate from the TSC, which still loses
no time when ticks coalesce. ROADMAP §11.3 gives aarch64 the same function over `CNTVCT_EL0`. Trace
timestamps are separate: ROADMAP §10.7's flight-recorder
records carry raw cycle-counter reads. A trace orders records across CPUs only when CPUID reports the
TSC invariant and the bring-up warp test ([DESIGN §7.4](SMP.md#74-ap-bring-up-sequence)), whose
result `vibeOS: smp: tsc skew <n> cycles` reports, saw no backward step (Linux's `check_tsc_warp`
rule). Otherwise the export (`trace::export_chrome`) orders records within each CPU only and says so
in its `otherData`. The header of `VIBEOS_TRACE` carries the calibration and the warp result for the
core tool.

## 6.5 Timers and timeouts

Sleeps and timeouts need a data structure, not a linear scan of every thread on every tick:

- Today one sorted list of pending timeouts, `sched::TimeoutQueue`, under one lock holds every
  timeout, and the timer path wakes the threads whose deadlines have passed. Fine for tens of
  threads.
- Planned (ROADMAP §19.4): each CPU has two timer structures, as Linux has. Deadline timers sit in a
  queue ordered by nanosecond deadline. They serve sleeps (`nanosleep`, `clock_nanosleep`),
  `timerfd`, POSIX timers and itimers, the timeouts of `futex`, `poll`, `epoll`, and every blocking
  primitive, and ROADMAP §25.5's per-CPU watchdog timer. Timeout timers sit in a hierarchical wheel
  with 1 ms first-level buckets and no cascading, Linux's design since 4.8: a timer goes into the
  level whose bucket is at most an eighth of its interval wide and never moves, so it fires at most
  an eighth of its interval late. They serve timeouts that are usually cancelled before they fire:
  TCP's retransmit, delayed-ACK, zero-window-probe, keepalive, and `TIME_WAIT` timers, ARP and
  reassembly timeouts, and block request deadlines ([§10.3](BLOCK.md#103-failure)). Both structures are
  intrusive: a timer's links live in the object that owns it, so arming, re-arming, and cancelling
  allocate nothing.
- A deadline timer whose action is a wake or a signal expires in the timer interrupt's top half
  ([§2.2](INVARIANTS.md#22-interrupt-handler-rules)). The expiry takes the timer off its base under the base's
  lock, and wakes or signals after dropping it. It allocates nothing and takes only spinlocks: a
  POSIX timer's signal uses a queue entry allocated at `timer_create`, as Linux's does, and a
  `timerfd` marks itself ready through ROADMAP §13.6's readiness mechanism, which allocates nothing
  and takes only spinlocks there, as Linux's `ep_poll_callback` does. One interrupt does at most 32
  wakes, counting each thread woken and each descriptor marked ready. Due timers past that wait for
  the next interrupt, which is armed to come at once, and a `timerfd` whose waiters pass the limit
  finishes its wakes as a softirq-equivalent item. A timeout timer's callback runs in §2.2's
  timer-callback context, a softirq-equivalent item on the CPU whose wheel fired it, since TCP's
  callbacks allocate and take socket locks. `cancel_sync` returns only once the timer's expiry or
  callback runs on no CPU.
- Each CPU's timer base, its two structures, has its own `SpinMutex` at the TIMER rank (§2.1). Any
  CPU may take it to arm, re-arm, or cancel a timer there, the one exception to
  [§7.7](SMP.md#77-locking-with-more-than-one-cpu)'s owner-only rule, because TCP re-arms a connection's
  timer on every ACK from whichever CPU received it. A timer re-armed from another CPU moves to that
  CPU's base unless its callback is running, as Linux's `mod_timer` does. A move never holds two base
  locks: it marks the timer migrating, drops the old base's lock, and takes the new one's, and an arm
  or cancel that finds the timer migrating waits for the move to finish. A timer pinned to its CPU,
  such as the watchdog's, never moves.
- Where the local timer has a one-shot mode (TSC-deadline on x86_64, the generic timer's compare
  value on aarch64), a CPU arms it for the earliest of its next tick and both structures' next
  expiries. On the periodic fallbacks ([§6.3](#63-the-tick)) a deadline timer expires at the first
  tick after its deadline.
- Timer slack (ROADMAP §19.6) widens only a fair-class thread's deadline timers. A real-time thread's
  slack is 0, as on Linux.
- Every blocking primitive takes an optional deadline. A wait for a device parks with its device's
  stall bound S as its deadline, and the request's own deadline starts recovery
  ([§10.3](BLOCK.md#103-failure)), so a lost completion ends in an error, not a hang. A wait on behalf of a
  user request (`read` or `accept` on a socket, pipe, or tty; `futex`; `poll` and `epoll`; `wait4`;
  `sigsuspend`; `nanosleep`) has the deadline its caller gave, or none, as Linux allows, and is
  interruptible: any signal ends it, with `EINTR` or a restart under `SA_RESTART` (ROADMAP §13.8).
  A wait that no signal can end, or only `SIGKILL` (Linux's killable wait), is uninterruptible. An
  uninterruptible wait with no deadline, such as one for a sleeping lock ([§2.1](INVARIANTS.md#21-lock-order)), is
  where a lost wake hangs a thread for good.
- The blocked-thread sweep: every `SWEEP_TICKS` ticks CPU 0 reports each `Blocked` or `Sleeping`
  thread whose recorded deadline is at least `OVERDUE_NS` (5 s) past, as
  `vibeOS: sched: overdue tid <id>`, a registered failure line. The timeout path would have woken
  it, so its timeout entry was lost or never queued (ROADMAP §10.7). A thread records its deadline
  in its state: `Sleeping { deadline }`, and `Blocked { wq, deadline }`, which `Sched::begin_wait`
  sets to the deadline it queues (`FAR_DEADLINE`, never overdue, for a wait with none). The check
  is portable, `vibeos::sched::find_overdue`, which scans in tid order from a cursor so that
  successive calls reach every overdue thread whatever their number. It runs on `sched-sweep`, a
  kernel thread pinned to CPU 0 (`thread_init::sweep`), that sleeps `SWEEP_TICKS` ticks, scans the
  thread table 64 TCBs per SCHED hold with IF off, yields between holds, and prints from thread
  context. The scan first ran in CPU 0's `schedule_inner`, but over 1,024 TCBs in one hold it
  measured above ROADMAP §10.7's 20 µs under TCG: `sched_sweep_cost` printed max 815 µs, median
  108 µs at commit d4580d1 on an Intel(R) Xeon(R) Processor @ 2.80GHz (4 CPUs). Per 64-TCB hold it
  measured max 191 µs, median 19 µs, and on a second run max 105 µs, median 31 µs, at commit
  71fd409 on the same host, where an empty SCHED hold alone reached 362 µs, so the maxima are
  TCG and host noise. ROADMAP §25.5 extends the sweep to
  uninterruptible waits with no deadline, each reported once it has lasted 120 s, Linux's
  `hung_task_timeout_secs` default, or the largest stall bound S among registered block devices
  where that is larger ([§10.3](BLOCK.md#103-failure)). An interruptible wait is never reported, however
  long: an idle server's `epoll_wait` is one. A stall of the timer itself is for ROADMAP §25.5's
  lockup detectors (F111).

## 6.6 Tickless and wall clock

A fixed 1 kHz tick on an idle CPU is wasted interrupts and, on real hardware, wasted power. TSC-
deadline mode makes tickless operation possible: when a CPU goes idle, arm the deadline for the next
pending timer, the earliest of [§6.5](#65-timers-and-timeouts)'s two structures, instead of the next
millisecond, and skip the timer entirely if there is nothing pending.
Not day-one work, but the timer abstraction should be "next deadline" rather than "periodic tick" so
this does not require rewriting the scheduler. Planned: ROADMAP §19.6 lands tickless idle. An idle
CPU's next deadline is then no later than half the clocksource's wrap time (§6.4), and the CPU that
reads a narrow clocksource for its wrap hands that duty to one that stays awake before it stops its
own tick.

The RTC gives date and time to one-second resolution over ports `0x70`/`0x71`, with the usual
century-register and BCD-versus-binary quirks to detect. Read it once at boot, then track time with the
monotonic clock and an offset. Poll the RTC for updates and the boot log timestamps drift relative to
each other in a way that is genuinely annoying to debug. NTP over the network eventually replaces the
offset with something correct. Planned (ROADMAP §13.9): a timer set for an absolute `CLOCK_REALTIME`
time stays tied to the wall clock. When `clock_settime` or `settimeofday` changes the offset, every
such timer is re-armed at the monotonic time that now matches its wall time, as Linux does when the
clock is set, so it fires when the wall clock reaches it; a relative timer, and a timer on any other
clock, does not move.

Planned (ROADMAP §20.2): across S3 the counters restart, so at resume the RTC is read again, the
counter the clocks are computed from is re-based so that no clock steps backward, and
`CLOCK_BOOTTIME` gains the time asleep while `CLOCK_MONOTONIC` does not.
