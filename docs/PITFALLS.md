# 9. Pitfalls

Index: [DESIGN.md](DESIGN.md). This file holds DESIGN §9, and its headings keep DESIGN's numbers.

Symptom first, because that is how you will arrive here.

## 9.1 Boot and build

**Kernel faults immediately after CR3 install, before any output.**
LLVM emits GOT-relative accesses and `.got` landed outside the range the kernel mapped for itself.
Rule: `linker.ld` places `.got` before `.bss`, inside `__kernel_vma_start..__kernel_vma_end`.

**Debug builds fault on entry to page table setup; release builds are fine.**
At `opt-level = 0` the function's stack frame exceeded the boot stack Limine provides. Rule: dev
profile uses `opt-level = 1`, and boot-path functions keep their frames small.

**A build succeeds but the ISO behaves like the previous build.**
Two causes, both real. `CARGO_TARGET_DIR` pointed at a shared cache so the ISO copied a stale ELF, and
separately the Makefile's prerequisite list was hand-maintained and did not include newly added source
directories. Rule: `make` pins `CARGO_TARGET_DIR` to `./target`, and prerequisites are a `find` over
`src/`.

**Bare `cargo build` has an empty initrd; a relative linker script used to fail off-root.**
`build.rs` only copies `VIBEOS_INITRD` (64 KiB) and passes an absolute `-T linker.ld`. Unset
`VIBEOS_INITRD` embeds zeros so `cargo check` works. Rule: `make` stages `build/initrd.fat` via
hostlib `mkinitrd`. Do not generate the image inside `build.rs`.

**A Limine response pointer is null and the kernel dies with no explanation.**
The request static was not in the `.limine_requests` section, so the loader never saw it. Rule: every
request is `#[used]` with an explicit `link_section`, and the base revision is verified before any other
response is read.

**Panic backtrace addresses have no names, or name the wrong function.**
Earlier builds put the symbol table in `.text` or patched it in place. Today the second link moves `.text`: with
pass 1's empty `KSYMS`, `print_frame_addr` encodes the table reference as short immediates, pass 2
grows it from 0x2a2 to 0x2b2 bytes, and every later function shifts. The panic ISO's table is wrong for
every function from `panic::finish` on (36 entries), and a `CARGO_PROFILE=release` table is wrong in 499 of 1106 entries.
Rule: first link with an empty `.rodata` table, `nm --demangle` the ELF, second link with the filled
table (Makefile `KERNEL_VARIANT`). `.text` must not move, so the reference to the table compiles to
the same size empty and filled. Planned (ROADMAP §10.2, F084): the table moves to its own `.ksyms`
section reached only through linker-defined bounds, and the build regenerates it from the final ELF
and fails on any difference.

**QEMU framebuffer reprints the prompt on every key; serial looks fine.**
The FB write path skipped `\r` before the text grid saw it, so the line editor's in-place paint
(`\r` + rewrite) homed only on serial. Rule: `\r` sets column 0 on the same row; do not drop it.
Host: `cr_homes_column_same_row`, `cr_paint_overwrites_in_place`. In-guest: `fb_cr_home`.

## 9.2 Memory

**AP bring-up hangs with no output, or faults at a low address.**
The low identity window was mapped with NX on 2 MiB pages, and the AP fetched the trampoline from
its low page after enabling paging. Rule: the first 2 MiB of the identity window is executable.
Everything else stays NX.

**Building page tables at boot never finishes.**
`map_end` was computed from raw memory map entries, and firmware described an MMIO BAR as a
multi-terabyte region. Rule: derive the physmap extent from usable RAM, kernel image end, and
framebuffer extent, and cap it (8 GiB). PCI BAR size probes that return > 32 MiB are recorded
and not page-walked into the ioremap window or physmap. Planned (ROADMAP §11.2): the physmap maps
only RAM-typed ranges, so a huge MMIO descriptor is never walked and the cap goes (§4.1).

**Device reads return stale values on real hardware but work in QEMU.**
MMIO reached through a write-back physmap mapping. QEMU does not enforce cache attributes; hardware
does. Rule: LAPIC, I/O APIC, HPET, and every device MMIO page gets PCD + PWT, patched immediately after
CR3 install and before first access. Patch every physmap leaf the range touches, and split a 2 MiB leaf
to 4 KiB first when it also holds usable RAM, so no RAM frame gets a UC alias (§2.7, I17). Not yet
enforced: `Mapper::patch_physmap_uc` marks whole 2 MiB leaves UC and can skip a trailing leaf (ROADMAP
§11.2, F104). Planned (ROADMAP §11.2): device MMIO leaves the physmap for `ioremap`, so no physmap leaf
is ever patched (§4.1).
Do not UC-patch the console framebuffer when it aliases VGA BAR0; leave that physmap WB.

**Config space beyond bus 0 is all `0xFFFF` on a machine without MCFG.**
The kernel sends only bus 0 through `0xCF8`/`0xCFC`: for any other bus ECAM does not cover, `HwCfg::read32`
returns `0xFFFF_FFFF` and `write32` drops the write, so a device behind a PCI-PCI bridge is never
found (ROADMAP §20.1, F114). Configuration mechanism #1 addresses every bus (CONFIG_ADDRESS bits
23:16); its limit is the 256-byte config space. Rule: ECAM where MCFG or the device tree covers
the bus; mechanism #1 for offsets below `0x100` elsewhere. The two firmware sources give an ECAM
base differently: an MCFG entry's base corresponds to bus 0 even when its start bus is not 0 (PCI
Firmware Spec 3.2 §4.1.2), and a device-tree `pci-host-ecam-generic` node's `reg` corresponds to
the first bus of its `bus-range`. The kernel stores every window by its first bus's address, as the
device tree gives it and as Linux does for both, so an MCFG base becomes `base + (start_bus << 20)`
when it is parsed, and `pci::ecam_phys` returns
`base + ((bus - start_bus) << 20 | dev << 15 | fn << 12 | off)`. Rule; not yet enforced:
`acpi::parse_mcfg` passes the MCFG base through unchanged, so an entry whose start bus is not 0
reads bus `b` at bus `b - start_bus`'s configuration space (ROADMAP §20.1, F045). Type-1 headers
reuse BAR slots as bus-number registers; size-probe only the BAR count for that header type.

**Two subsystems designed for the same virtual address range.**
The heap and the kernel VA allocator were both specified at `0xFFFF_C000_*` in different documents, and
only one of them noticed. Rule: the address map in [section 4.1](MEMORY.md#41-virtual-address-map) is the single
source of truth, and every region asserts its range is unmapped before claiming it.

**Allocator corruption with a crash in an unrelated subsystem.**
Buddy free list nodes live inside free pages, and a kernel stack overflow wrote into one. Rule:
every kernel stack gets an unmapped guard below it, and stack overflow is a page fault, reported
from a stack known to be good, rather than silent corruption: x86_64's `#DF` runs on IST 1 (§5.1),
and aarch64's vector entries test the stack bit of §4.5's layout and move to a per-CPU overflow
stack (§11.5 rule 6; ROADMAP §11.3). The bootstrap thread breaks the rule (`stack: None`): `_start`
and all of boot run on Limine's stack (at least 64 KiB, no guard page, in bootloader-reclaimable
memory). The kernel sends no stack size request (ROADMAP §10.6, F072).

**A PTE edit appears to have no effect.**
No `invlpg` after the edit. Rule: `invlpg` after any single-PTE modification, including MMIO attribute
patches. Kernel mappings are `GLOBAL`, and every CPU sets `CR4.PGE`, so they do not fall out of the
TLB on a CR3 reload.

**`meminfo` is slow.**
`free_page_count()` walked the free lists. Rule: maintain a running counter.

**Freeing a stack while running on it.**
An AP's stack was unmapped while it was still executing on it. Rule: a dead thread's stack is freed
only after the CPU that ran it has switched off it, and the reclaimer observes that; a reaper thread
that is not on the stack is not enough on SMP ([section 2.8](INVARIANTS.md#28-publish-last) rule 2). A global
list that any CPU drains broke this: another CPU could unmap the stack while the exiting CPU was
still running `schedule()` on it (F012), and an exit burst filled its 8 slots (F010). Now
`thread_exit` parks the stack in its own CPU's slot, and only that CPU's switch tail
(`thread_init::finish_switch`), after `switch_context` has returned, moves it to the CPU's stack
cache or dead list; that CPU's worker unmaps what the cache cannot take, with IF=1 ([§4.5](MEMORY.md#45-kernel-virtual-address-allocator)).

## 9.3 Interrupts

**A device's interrupt fires exactly once and never again.**
EOI went to the wrong controller, or was skipped entirely, or happened after a context switch. Rule:
LAPIC EOI for everything APIC-routed, and EOI before any code path that can switch threads.

**No timer interrupts, no error.**
The I/O APIC was programmed for pin 0 on the assumption that ISA IRQ0 maps to GSI 0. It commonly maps
to GSI 2. Rule: apply MADT interrupt source overrides, and take polarity and trigger mode from the
override when one exists.

**Every interrupt is delivered twice.**
The 8259 was left unmasked after the I/O APIC started routing the same sources. Rule: mask the PIC
completely once the I/O APIC owns delivery and the LAPIC timer is verified ticking.

**Every LAPIC register reads zero.**
`IA32_APIC_BASE` bit 11, the global enable, was clear. Rule: check and set it, do not assume firmware
left the LAPIC on.

**Unmasking an I/O APIC entry that points nowhere.**
The low dword, which holds the mask bit, was written before the high dword holding the destination.
Rule: high dword first, low dword second.

**A triple fault reported by QEMU as a silent reboot loop.**
The double fault handler's IST index was off by one, so it ran on the already-broken stack. Rule: the
software IST index is zero-based and the TSS descriptor field is one-based. Verify with an intentional
stack overflow test.

**Lost timer ticks under load.**
With a one-shot timer, the handler called the scheduler before rearming, so a preemption dropped the
next deadline. Rule: rearm the timer before doing anything that can yield.

**Virtio kicks vanish.**
Notify used the wrong BAR offset or ignored `notify_off_multiplier`. Rule: doorbell =
`cap.offset + queue_notify_off * multiplier` inside the notify capability; wrap or past `length` is
a failed kick, not a store into some other register. The 2-byte store needs
`queue_notify_off * multiplier + 2 <= length`: `virtio::notify_addr` accepts an offset of
`length - 1` and skips the bound when `length` is 0 (ROADMAP §18.1, F048). The value written is the
virtqueue index (without `VIRTIO_F_NOTIFICATION_DATA`); `virtio_blk_init::kick` writes 0 for every
queue, which QEMU ignores and a device that shares one doorbell does not (ROADMAP §11.5, F047).

**Device sees a virtqueue index and stale descriptors.**
`avail.idx` was published with a compiler fence. Rule: descriptor stores, then `dma_wmb` /
`fence(Release)` + `sfence`, then the index. Used-ring harvest is `dma_rmb` after observing `used.idx`.
`dma_wmb` orders stores only. The kick decision loads `avail_event` or `used.flags` after the
`avail.idx` store, and the harvest reads `used.idx` again after its `used_event` store, so each needs
a full barrier (`mfence`) between the store and the load (virtio 1.2 §2.7.13.4.1). No `dma_mb`
exists, and under `VIRTIO_F_EVENT_IDX` one lost kick stops a queue for good (ROADMAP §10.3, F016).
On aarch64 the notify is a Device store, which can reach the device before the `avail.idx` store is
visible; `mmio_write`'s `dmb oshst` orders them ([§4.7](MEMORY.md#47-dma)).

**Allocate or block in a hard-IRQ / MSI handler.**
The top half ran `Box` / `sleep` / `WaitQueue` wait. Rule: ack, set pending, wake the IRQ thread or
enqueue work. The thread may alloc and block ([section 2.2](INVARIANTS.md#22-interrupt-handler-rules)).

**An NMI, `#MC`, or `#DB` next to a syscall reads a user value as its `PerCpu`.**
The handler decided `swapgs` from CS.RPL. Between `syscall` and the entry `swapgs`, and between the
exit `swapgs` and `sysretq`/`iretq`, CS is the kernel's but `GS_BASE` holds the user base, so the
handler skips the swap and `gs:[0]` is whatever userspace set. Rule: ordinary vectors may trust
CS.RPL, except a `#GP`, `#NP`, or `#SS` raised by a user-return `iretq`, which arrives with the
kernel CS and the user GS base (ROADMAP §10.6, F007); the IST vectors may trust CS.RPL 3, since user
code cannot write `KERNEL_GS_BASE`, and move such a frame to the thread's kernel stack; a CPL-0
frame, and every `#DF`, decides from the sign of `GS_BASE` (ROADMAP §10.6), and once FSGSBASE lets a
user load a kernel-half base, saves `GS_BASE` and loads the per-CPU base unconditionally (ROADMAP
§18.3). Applying that save-and-load protocol to a CPL-3 frame as well leaves the `PerCpu` address in
`KERNEL_GS_BASE` while the thread is in the kernel, so a switch from the moved body saves it as the
thread's GS base, and the thread resumes on another CPU with a kernel address as its GS base, or
with two CPUs sharing one `PerCpu`.

**A user program halts every CPU.**
Ring-3 activity reached `exception_halt` on three paths. `debug_ex` had no ring-3 branch and
`sig_for_vec` mapped neither `#DB` nor `#AC`, so a user `popf` that set `RFLAGS.TF`, or an `int1`
(`0xF1`), halted the kernel (F005); now `sig_for_vec` reads the `vibeos::trap` table and a CPL-3
`#DB` leaves its IST stack and ends in `SIGTRAP`. A new thread's first return (`syscall_init::first_return`)
ran with IF=1, so an interrupt between its `mov gs` and its `iretq` reads `gs:[0]` at VA 0 (F006). A `syscall` in the last two bytes
of the top user page leaves RIP at the non-canonical `0x0000_8000_0000_0000`, and the `#GP` on the user-return `iretq` runs on the user GS
base; TCG skips that canonical check, and KVM and hardware do not (F007). ROADMAP §10.6 closes all
three. Rule: an exception raised by ring-3 code, or by a return to ring 3, ends in a signal to that
process; `exception_halt` is for faults in kernel code. Every x86_64 vector and every aarch64
exception class has a ring-3 row, in the [section 5.2](INTERRUPTS.md#52-idt-and-exceptions) and
[section 11.5](PORTABILITY.md#115-aarch64-exceptions-and-privilege-transitions) tables, and a new ring-3 entry or
exit path gets an in-guest test that runs it with IF=1.

## 9.4 Concurrency

**Deadlock the moment the timer starts firing.**
The scheduler lock was taken without disabling interrupts, and the timer ISR calls into the scheduler.
Rule: any lock reachable from an ISR is taken with interrupts disabled in every context. There is one
spinlock type, `SpinMutex`, and it is IRQ-aware; §2.3 lists the other spinning primitives and the
ROADMAP lines that remove them.

**A thread blocks forever despite a wakeup being sent.**
The wakeup arrived in the window between deciding to block and actually blocking. Rule: enqueue onto
the wait queue, mark self blocked, drop the inner lock, then schedule. In that order, so there is no
point where the thread is both on the wait queue and considered runnable.

**Wait queue cookie is a dangling pointer.**
`ThreadState::Blocked { wq }` stores the `WaitQueue` address so timeout can unlink. The object that
owns the queue (mutex, rwlock, condvar, channel) must outlive every waiter. Dropping it with threads
still blocked is a use-after-free on the next timeout or wake.

**An I/O completion writes into a stack frame its waiter has already reused.**
`IoWaiter` lives on the submitter's stack, and `wait()` returns as soon as its lock-free `poll()`
sees `done`, so any access the completer makes to the waiter after that store can land in whatever
frame reused that stack. A lock taken after the store does not help, and neither does storing `done`
under SCHED before the wake (F002). Rule: publish last ([section 2.8](INVARIANTS.md#28-publish-last)).
`IoWaiter::finish` takes SCHED, runs `wake_all` on the waiter's queue, and stores `done` with Release
inside that section, which `poll()` loads with Acquire ([section 10.1](BLOCK.md#101-completions)). It is an
`unsafe fn` over a raw pointer, so no reference to the waiter outlives the store.

**Condvar waiter never sees the predicate.**
Wake does not carry the condition. Mesa: `wait` re-acquires the mutex and returns; the caller loops
on the predicate. Timeout is the same path.

**Condvar wait parks still holding the mutex.**
`begin_wait` marked Blocked, SCHED dropped, then `drop(guard)` released the mutex. A timer in that
window switched the waiter off-CPU still owning it; the notifier blocked on the mutex forever. Rule:
enqueue on the CV and unlock the mutex under the same SCHED, keep IF off from that section through the
delivery of the wakes it recorded, then schedule. `with_sched` breaks the second half: it runs
`place_ready` for the recorded wakes after dropping SCHED, with IF back on, so a preemption there
switches the waiter out before the woken mutex waiter is on any queue (ROADMAP §10.10, F034).

**First-run thread `#PF`s in `schedule_inner` at `rsp = stack_top-8`.**
`popfq` restored IF before `jmp` to the trampoline. A tick landed in that window, `schedule_preempt`
saved over the synthetic frame, and `iret` jumped to the nested save's RIP with the prepared RSP.
Rule: delayed `sti` immediately before `jmp`; never `popfq` with IF set across a stack switch.

**Two `&mut T` from the same mutex in release builds only.**
The spinlock's re-entrancy check was a `debug_assert!`. Rule: real CAS spin loop, and any invariant
that must hold in release is an `assert!`. These are `debug_assert!`: `BootCell::set`'s set-once check,
`pmm` `pop_head` on an empty order, and the heap `carve` bounds (ROADMAP §10.2, F041, F137). No CI job
builds or boots `CARGO_PROFILE=release`, where they compile out (ROADMAP §10.2, F137).

**`per_cpu: with_current re-entry` on the first workqueue IPI.**
`with_current`'s busy flag spanned `switch_context`. The incoming thread resumed with the flag still
set and IF on (`irq_nest == 0` / `apply_if_on_resume`), then `0xFD` called `drain_inbox` →
`with_current` and panicked. Rule: `InterruptGuard` may span the switch (it lives on the outgoing
stack); the busy flag must not.

**Keyboard input deadlocks the shell.**
The input ring was guarded by a lock that IRQ1 also takes, held with interrupts enabled by the
consumer. Rule: same as the scheduler lock. Interrupts off around the critical section.

**QEMU window keys never reach the shell; serial stdio does.**
Two independent kills, same symptom (COM1 is polled; PS/2 needs IRQ1):

1. `DISABLE_1` sets controller config bit 4 (keyboard clock off). Rewriting that byte to enable INT1
   and translation without clearing bit 4 leaves the port clock-gated after `console ok`. Rule: config
   writes go through `cfg_probe` / `cfg_run`, which clear `CFG_CLOCK1_OFF`. Host-test the mask; ktest
   `kbd_8042_clock` reads the live byte.
2. `route_keyboard` failing then unmasking PIC IRQ1 after step 13b masked the 8259. Window PS/2 is
   silent; serial still works. Rule: after LAPIC owns the tick, IRQ1 is IOAPIC-only. PIC IRQ1 is
   fallback only on the PIT path (LINT0 ExtINT). Not a new boot marker. ktest `kbd_gsi_unmasked`.

ktest `kbd_ps2_irq` injects a scancode with 8042 `0xD2` (IRQ path; not the device clock). E2E and
`make test-ps2` type via COM1 and via `sendkey` (same i8042 as the window).

**Timestamps occasionally go backwards.**
The tick counter and the TSC snapshot were read as two independent relaxed loads. Rule: publish them
under a seqlock, release on write, acquire on read, retry on an odd or changed sequence.

**Timestamps go backwards after `hlt` on TCG.**
QEMU TCG does not set the invariant-TSC CPUID bit. Interpolation can overshoot a late tick, then the
counter moves and `now_us` drops, even with a stable seqlock pair. A wrapping TSC-behind-snapshot
delta looks like ~2^64 cycles. Rule: treat a high-bit wrapping delta as extra 0, and never publish a
`now_ns` below the last reading. Do not cap extra at one tick: across a stretch with IF off, which a
test may hold on purpose, timeouts must still advance on TSC alone.

**`sleep_ms(50)` and PIT-vs-HPET calib flake on TCG SMP.**
TCG has no invariant TSC. Boot HPET calibration runs before APs; a later PIT channel 2 window sees a
different apparent TSC rate, and LAPIC periodic ticks coalesce so `uptime_ms` during a sleep is not
50–100. Rule: a timing check measures once and holds one band; it is never retried against a fresh
sample, and it gets no wider band where it flakes (§9.8). The PIT-vs-HPET cross-check holds its
75–125% band only where the TSC is invariant, so without the CPUID bit it skips with the reason
`no invariant tsc`, and the ROADMAP §10.1 KVM leg, whose guest has the bit, runs it. Not yet
enforced: `tsc_calib_source` measures PIT three times in a 50–200% band without the bit (ROADMAP
§10.2), and `sleep_ms_50` accepts 40–400 ms of `now_us` when ticks coalesce (ROADMAP §10.3). Do not
loosen the invariant-TSC path. Under KVM too, QEMU leaves the invariant-TSC bit out of `-cpu max`
and `-cpu host` while the vCPU is migratable, its default, so the KVM leg asks for `+invtsc` and
fails if the guest still reports none (ROADMAP §10.1). Planned (ROADMAP §10.3): `now_ns` stops
counting ticks, and the coalescing allowance goes with it.

**Serial output from multiple CPUs is unreadable.**
No lock on TX. Rule: lock serial TX, and write each line whole: format it, newline included, into one
buffer and send it under one TX hold. Byte granularity keeps bytes whole, but `Serial::write_fmt`
takes TX once per `write_str` piece and `log_fmt` sends the newline separately, so another CPU can
split a line and the harness then misses a contract line ([section 7.7](SMP.md#77-locking-with-more-than-one-cpu),
ROADMAP §10.2, F138).

**Two CPUs performing a TLB shootdown at the same time hang.**
Both waited with interrupts disabled for acknowledgement the other could not send. Rule: the wait loop
also services incoming shootdown requests.

## 9.5 SMP bring-up

**An AP reads garbage parameters and dies.**
The trampoline parameter block was written with a non-volatile copy, and the compiler was free to
reorder it past the MMIO write that sent the SIPI. Rule: `write_volatile` per field, and start the AP
through the IPI send, which orders earlier stores for the APIC mode in use ([section 7.6](SMP.md#76-ipis)).
A compiler fence alone does not order an x2APIC `WRMSR`.

**Two APs corrupt each other.**
They shared the trampoline page and its parameter block. Rule: start APs one at a time and wait for
each ready flag before the next.

**An AP faults on the first kernel page it touches.**
The trampoline entered long mode without setting `EFER.NXE`, so NX bits in kernel PTEs were
reserved-bit violations. Rule: the trampoline sets NXE along with LME.

**An AP that missed its bring-up timeout runs on freed memory.**
The timeout path frees the AP's kernel, RSP0, and IST stacks and its GDT/TSS and marks its idle TCB Dead, but an AP that accepted a SIPI and then
stalled past 3 s can keep running on them, or read the next AP's parameter block. Rule: the timeout path
sends INIT, clears the AP's online bit, and leaks what it gave the AP; a failed AP costs its stack and
tables, not a second CPU on the same memory ([section 7.4](SMP.md#74-ap-bring-up-sequence)).
`smp_init::start_one` frees without INIT (ROADMAP §11.4, F032).

**A null dereference in an ISR shortly after an AP comes up.**
`sti` happened before `GS_BASE` was set, and a timer interrupt landed in code that reads per-CPU state.
Rule: per-CPU MSRs are set before the IDT is live and before `sti`.

**AP triple-faults in `ap_entry` before `lidt`.**
`IrqCell.with` / `InterruptGuard` call `try_current` → `gs:[0]` while GS is still 0 and the IDT is
not loaded. Rule: read trampoline bring-up params through `IrqCell::as_ptr()`, install `GS_BASE`,
then use cells.

**First ring-3 timer IRQ triple-faults (`#PF` at `0xffffffe8`).**
`BootCell::set` moved the BSP GDT/TSS after `tables.init` baked the stack address of that TSS into
the GDT. CPL=0 IRQs keep the current RSP so boot looked fine; the first CPL=3 IRQ loads stale
`TSS.RSP0`. Rule: init descriptor bases after the cell owns the tables (`BootCell::as_ptr`).

**A CPU never sees itself in the per-CPU table.**
The table entry was published without a fence before the SIPI. Rule: publish with a Release store,
then start the AP through the IPI send, which orders the store ([section 7.6](SMP.md#76-ipis)).

## 9.6 Hardware polling

**The kernel hangs in a panic handler.**
The serial TX loop polled the transmit-holding-register-empty bit without a bound, and the UART was not
responding. Rule: every hardware poll has an iteration cap and a defined failure action. Serial drops
the byte.

**The kernel hangs while sending an IPI, under `cli`, with no output.**
The ICR delivery-pending poll was unbounded. Rule: cap it, return failure, log at the call site. The
poll logic lives in the host-testable half of the crate so the timeout has a unit test.

**TSC calibration produces a nonsense frequency and every delay in the kernel is wrong.**
The ACPI HPET table had a zero address, or a generic address structure describing I/O space rather than
system memory, and neither was validated. Rule: validate both, and fall back to PIT channel 2.

**Slightly wrong calibration on out-of-order CPUs.**
`rdtsc` without serialization can move across the measurement boundary. Rule: `lfence` before, or use
`rdtscp`.

**Following a garbage ACPI pointer.**
No RSDP checksum validation. Rule: validate the v1 checksum over 20 bytes and, for v2, the extended
checksum over the full length, before dereferencing anything. Read packed fields with `read_unaligned`.

## 9.7 Tests

**A test passes against a known-broken implementation.**
The seqlock test computed its expected value from the same read it was validating, making it monotonic
by construction. Rule: the writer publishes an independent value for the reader to compare against.

**A test aborts the whole run.**
A `#UD` test executed `ud2` while `#UD` was a halting handler. Rule: destructive exception tests need a
scoped transient handler that steps RIP past the faulting instruction, or they stay skipped with a
stated reason.

**E2E false failures from prose.**
The panic scanner matched the phrase "page fault", which appears in normal log and help text. Rule:
match exception mnemonics (`#PF`, `#GP`, `#DF`, `#UD`) and `panicked at`, on lines the kernel framed
(§2.6), since user programs print both.

**A test-only build gets shipped in the production ISO.**
The `kernel_tests` feature build shared a Cargo target directory with the normal build. Rule: separate
target directory and separate ISO for the test build.

**A boot regression passes CI.**
The e2e harness checked that markers were present but not that they were ordered, and SMP bring-up ran
after the last marker it looked for. Rule: markers are asserted in order, and `smp: done` comes before
`console ok`, `pci: N devices`, `block: <name> <n> sectors`, and `shell ready`. A new marker is added to the harness in the same
commit that emits it.

## 9.8 Meta

**The same design decision made twice, differently.**
The old tree had a ticket lock in one design document and an IRQ-guarded spin mutex in the code, and
the mismatch confused everything downstream for weeks. Same for the TLB shootdown vector, which was
`0xFE` in a plan and `0xFC` in the code. Rule: constants and primitive choices live in exactly one
place, the design docs (DESIGN.md and its topic files), and a change updates it in the same commit.

**Critical bugs identified, documented, and never fixed.**
The old tree carried four issues marked critical, with named regression tests planned for each, for the
rest of its life. Rule: a bug that is understood well enough to write down is fixed or explicitly
deferred with a roadmap line. "Documented" is not a state a critical bug gets to rest in.

**A flaky test made green by a retry.**
The harness retried timed-out boots and three known failures, and the calibration check retried
against a fresh sample in a wider band, so runs that hit a real hang or panic came back green
(ROADMAP §10.2, F021). Rule: a flaky test is a bug. It gets a ROADMAP line that names its failure
line, and it is fixed there. No retry, skip, wider band, or longer timeout lands to make it pass,
and a test that repeats a measurement until one sample passes has a wider band. A test skips only
when its tier cannot run what it checks: its skip line names what the configuration lacks, and a
tier CI runs has it. `scripts/check_gone.py` keeps the deleted retry helpers out of the tree (ROADMAP §10.2).

**A stall retried instead of traced.**
From PR #75 on, `/bin/tests` sometimes stopped after `user: dup ok` and the harness booted again. The
cause was a forked child's first entry to ring 3 (`syscall_init::enter_user_full`, F006): it loaded
the user GS selector and wrote `GS_BASE` = 0 with IF=1, so a timer tick in that window was taken at
CPL 0 without `swapgs`, its `gs:[0]` read faulted at VA 0, and the kernel halted without printing a
line. Rule: a return to ring 3 runs `cli` before its first ring-3 segment or base load
([section 5.10](INTERRUPTS.md#510-privilege-transitions) rule 4); a hang is captured, each CPU's registers and
backtrace, before anything boots again; and a stall gets a test that holds its window open before its
fix lands (ROADMAP §10.2, F021).
