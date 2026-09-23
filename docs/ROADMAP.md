# Roadmap

Where this goes. The destination is a self-hosting operating system: one that boots on real hardware,
runs a graphical userspace, has a network stack you can serve from, and can compile and test its own
source tree on itself. Written by agents.

That is absurd. Good. The interesting failures happen past the point where the tutorials stop.

## How to read this

Twenty-three phases in four eras, then a list of what comes after. Ordering is by dependency, not by
preference: a phase's exit gate is the thing the next phase assumes. Within a phase, parts are mostly
parallelizable. Where two phases are independent, the era preamble says so; the numbers are not a queue.

**Two architectures.** x86_64 and aarch64 are both first class from [Phase 11](#phase-11-portability)
on. A gate is met on both, or the phase says which lines are single-architecture and why. From Phase 12
on, every phase has an **Architectures** line that says which. x86_64 came first and is the reference
when they disagree.

`- [ ]` and `- [x]` are the live status. Edit them in the commit that lands the work. There is no third
state. A deferral is an open box with a trailing note naming the phase that lands it, and that phase's
gate cannot close while the box is open.

**Slices.** A phase lands as two to four PRs named A, B, C, D, each with its own in-guest tests and each
leaving `main` green. The last slice closes the gate and tags the release. Phase 10 is the exception. It
lands as one PR per review-issue code or small group of codes, named by those codes (for example
`B1+DX1`), and its lines without a code land as PRs named after their subsection.

**Stretch** subsections and the [Beyond](#beyond) list are excluded from exit gates. They are where the
hard, optional things go, so that a phase is either done or not.

**Exit gate** is the definition of done. Gates are verifiable from outside the code: a marker appears
in serial output, a test target passes, a command produces the right result. "The code is written" is
not a gate. If a gate cannot be checked by running something, it is written wrong. A document counts as a
gate only when a script in `make check` verifies its required parts.

A gate that measures time, a rate, or throughput, or sizes its test against memory, names the
conditions it holds under. In a guest those are the accelerator, guest memory, and CPU count; on real
hardware, the machine. A gate that names none holds in the default harness guest (the `VIBEOS_*`
defaults: TCG, 128 MiB, 2 CPUs). "Under KVM" means KVM on an x86_64 host, and KVM or HVF on an arm64 host.

A line in phase *N* never depends on work in a later phase. When it would, the work moves earlier or the
line moves later. A pointer to a later phase is only a cross-reference, such as "USB HID arrives through
§20.3". A deferral box is the one exception: it stays open in its own phase and blocks only the gate of
the phase it names.

**Standing gates** apply to every phase and are not repeated:

- `make` builds clean with warnings denied
- `make check` green (fast local gate)
- `make test` green, all tiers, including the SMP and timer fallback variants once they exist, and on both architectures once Phase 11 lands
- CI green: the per-push jobs, and the scheduled jobs in the §10.1 CI budget before a phase tag
- new serial markers registered in the contract in [DESIGN.md](DESIGN.md#83-end-to-end), same commit
- new portable logic has host unit tests; new hardware behavior has an in-guest test
- every fixed bug gets a regression test in the cheapest tier that catches it
- `CHANGELOG.md` entry for anything visible to someone running the kernel (≤ 2 lines, user-facing)
- tag `v0.<phase>.0` at phase exit (first published tag is `v0.8.0` for Phase 8)
- design docs updated in the same commit as any change to an invariant or a constant
- no `TODO` describing a correctness gap. Those become lines in this file.
- every `unsafe fn` has a `# Safety` section and every `unsafe` block a one-line `// SAFETY:` reason; clippy's `missing_safety_doc` (with `check-private-items`) and `undocumented_unsafe_blocks` are denied from §10.1 on

## Non-goals

Stated so nobody spends a week on them.

- POSIX certification. Compatibility is a means to running real software, not a goal.
- Microkernel architecture. Monolithic, deliberately.
- Loadable kernel modules. One image, drivers in-tree. Signing and KASLR cover a single ELF.
- Windows or macOS binary compatibility.
- CPU hotplug. Offlining a core for power management is not hotplug; that is §19.6.
- 32-bit x86. Long mode only.
- Being a good first kernel to read. Other projects do that better and on purpose.

## The arc

| Era | Phase | | Unlocks |
|-----|-------|---|---------|
| **I. Metal** | 0 | [Ignition](#phase-0-ignition) | A booting, testable, CI-gated tree |
| | 1 | [Memory](#phase-1-memory) | `alloc`, own address space |
| | 2 | [Traps, ACPI, Time](#phase-2-traps-acpi-and-time) | Interrupts, a clock, hardware discovery |
| | 3 | [Threads](#phase-3-threads-and-scheduling) | Concurrency, preemption |
| | 4 | [SMP](#phase-4-smp) | All cores, per-CPU everything |
| **II. System** | 5 | [Console and Log](#phase-5-console-input-and-logging) | Interactive, observable |
| | 6 | [Devices](#phase-6-device-model-and-buses) | PCIe, MSI, DMA, virtio |
| | 7 | [Block storage](#phase-7-block-storage) | Persistent bytes |
| | 8 | [Filesystems](#phase-8-filesystems) | Files, paths, mounts |
| | 9 | [User mode](#phase-9-user-mode-and-processes) | Ring 3, syscalls, processes |
| | 10 | [Consolidation](#phase-10-consolidation) | Portable core, user runtime, real tables |
| | 11 | [Portability](#phase-11-portability) | aarch64 first class, native runs on the dev host |
| | 12 | [Fault-driven memory](#phase-12-fault-driven-memory) | Demand paging, COW, mmap |
| | 13 | [Threads and IPC](#phase-13-threads-ipc-signals-and-the-posix-surface) | clone, pipes, sockets, futex, TTY, signals |
| **III. Platform** | 14 | [Userspace](#phase-14-userspace) | libc, init, coreutils, real shell, crypto |
| | 15 | [Network](#phase-15-networking) | TCP/IP, sockets, DNS, TLS |
| | 16 | [Graphics](#phase-16-graphics-and-windowing) | Compositor, windows, terminal |
| | 17 | [Self-hosting](#phase-17-self-hosting-toolchain) | vibeOS compiles vibeOS, on both architectures |
| **IV. Frontier** | 18 | [Hardening](#phase-18-hardening) | KASLR, W^X, sandboxing, fuzzing |
| | 19 | [Performance](#phase-19-performance-and-observability) | Tracing, RCU, tickless, NUMA, slab, reclaim |
| | 20 | [Real hardware](#phase-20-real-hardware) | Bare metal on both architectures, USB, hardware CI |
| | 21 | [Virtualization](#phase-21-virtualization) | Hypervisor, containers |
| | 22 | [Distribution](#phase-22-distribution) | Installer, releases, self-hosted CI |
| | | [Beyond](#beyond) | Hard things with no gate |

Eras I and II are the ones with known answers, so they are where agent performance is measurable
against a clear correct result. Eras III and IV are where it stops being clear, which is the point.

---

# Era I. Metal

Power-on to a preemptively scheduled multiprocessor kernel. Every mistake made here is paid for
repeatedly, so the bar is higher than the feature count suggests.

## Phase 0: Ignition

**Goal.** A tree that builds, boots, prints, panics usefully, and fails CI when broken. No kernel
features. The entire value of this phase is that everything after it can be trusted.

**Unlocks.** Everything. Also sets the crate structure that determines what can be host-tested for the
rest of the project, which is the single most consequential decision in this phase.

**Exit gate**
- [x] `make` produces a hybrid BIOS + UEFI ISO
- [x] boots in QEMU under both `-bios` default and OVMF
- [x] serial shows `vibeOS: serial online` as its first line
- [x] a deliberate `panic!()` prints file, line, and message and halts without rebooting
- [x] `make test-e2e` asserts markers in order, fails fast on panic signatures, exits early on success
- [x] `make test-harness` passes: the harness has its own unit tests
- [x] CI runs the full ladder on push and pull request

### 0.1 Toolchain and target
- [x] `rust-toolchain.toml`: dated nightly, `rust-src`, `llvm-tools`, `targets = ["x86_64-unknown-none"]`
- [x] built-in `x86_64-unknown-none`: `code-model: kernel`, `disable-redzone`, `-mmx,-sse,+soft-float`; rustflags force frame pointers, static relocation, `-no-pie`, `-znorelro`
- [x] no RELRO (`-znorelro`); it conflicts with a non-PIE static kernel
- [x] `.cargo/config.toml`: default target `x86_64-unknown-none`; linker script is an absolute `-T` from `build.rs`
- [x] `Cargo.toml`: kernel target aborts; host tests unwind (`profile.dev`); `opt-level = 1` for dev
- [x] library and binary targets split from the first commit, not retrofitted
- [x] `kernel_tests` feature declared now, wired in phase 1

### 0.2 Linker and image layout
- [x] `linker.ld` places the image at `0xFFFF_FFFF_8000_0000`
- [x] page-align every section boundary so per-section permissions are possible
- [x] `.got` before `.bss` and inside the mapped range
- [x] export `__kernel_vma_start`, `__kernel_vma_end`, and per-section start/end symbols
- [x] `make layout` target: `llvm-objdump` section table plus `llvm-nm` for the exported symbols, so layout mistakes are visible without booting

### 0.3 Limine handshake
- [x] request statics in `.limine_requests` with the start and end markers, all `#[used]`
- [x] base revision verified before reading any other response, with its own marker
- [x] requests: framebuffer, memory map, HHDM, executable address, RSDP  (all five wired as of phase 1 slice A; framebuffer response only used for pmm exclusion so far)
- [x] a `BootInfo` struct captured once at entry; nothing else reads Limine statics
- [x] each null response produces a named halt, not an unwrap panic in a function with no context
- [x] `limine.conf` with a single entry, serial console enabled

### 0.4 Serial and panic
- [x] COM1 16550 init: 115200 8N1, FIFO enabled, DLAB dance
- [x] polled TX with a bounded THRE wait; drop the byte at the cap rather than spinning forever
- [x] polled RX on the data-ready bit  (landed with §5.3)
- [x] `fmt::Write` implementation with no allocation, usable before the heap exists
- [x] `print!` / `println!` macros routed to it
- [x] `#[panic_handler]`: re-init the port from scratch, print location and message, `cli; hlt` loop
- [x] a `panic_test` build feature or shell command so the panic path is exercised, not assumed

### 0.5 Build system
- [x] `Makefile`: `all`, `run`, `clean`, plus the test targets
- [x] `CARGO_TARGET_DIR` pinned to `./target`
- [x] kernel prerequisites from a `find` over `src/`, never a hand-written list
- [x] ISO staging: kernel ELF, `limine.conf`, BIOS and UEFI Limine artifacts, `xorriso` hybrid image, `limine bios-install`
- [x] `setup.sh`: fetch the Limine binary branch, verify `qemu-system-x86_64`, `xorriso`, `nasm`, `python3`; never rewrite project files
- [x] `make run` uses `-smp 2` so the default loop is multiprocessor from day one

### 0.6 Test harness
- [x] Python, standard library only; `subprocess` with its own timeout, no GNU `timeout`
- [x] QEMU spawned with serial captured and a monitor socket
- [x] ordered marker assertion, each check individually named in output
- [x] panic and exception signature scan, fail immediately with the captured line
- [x] early `quit` through the monitor on success
- [x] `VIBEOS_SMP` and `VIBEOS_QEMU_CPU` overrides
- [x] harness helpers have unit tests under `make test-harness`
- [x] targets: `test-unit`, `test-harness`, `test-e2e`, `test`

### 0.7 CI
- [x] GitHub Actions on push and pull request, Linux runner
- [x] install `qemu-system-x86`, `nasm`, `xorriso`; bootstrap Limine
- [x] `RUSTFLAGS=-Dwarnings`, `cargo clippy -- -D warnings`, `cargo fmt --check`
- [x] `check` job (`make check` + hostlib llvm-cov floor) before the QEMU ladder
- [x] run host units, harness units, ISO build, e2e
- [x] cache the cargo registry and the Limine checkout so the loop stays fast

---

## Phase 1: Memory

**Goal.** Own the address space and get a working `alloc`. Also stand up in-guest testing, because
paging cannot be verified from the host.

**Unlocks.** Every data structure in the kernel. `Vec`, `Box`, `String`. Thread stacks.

**Exit gate**
- [x] `vibeOS: pmm: <n> free 4KiB frames`, `paging: cr3 ok`, `heap ok`, `kva: ready` in order
- [x] kernel runs entirely on its own PML4 with Limine's tables abandoned
- [x] `make test-kernel` passes with in-guest tests for map/unmap, NX enforcement, heap growth, and the stack guard page
- [x] host tests cover the buddy allocator including fragmentation and exhaustion
- [x] a shell-less `meminfo` dump on the boot log reports plausible totals

### 1.1 Buddy physical allocator
- [x] `Buddy` in the library half: `allocate(order)`, `deallocate(phys, order)`, intrusive free lists stored in the free pages
- [x] split on allocation, merge with the buddy on free, up to a max order covering at least 4 MiB
- [x] O(1) free frame counter, no list walking
- [x] `stats()`: total, free, largest available order
- [x] host tests: exhaustion returns `None`, random alloc/free returns the count to its initial value, per-order alignment, coalescing after freeing alternate blocks, double free detected
- [x] init from the Limine memory map, excluding frame 0, the kernel image, the framebuffer, `0x8000`, and everything not `USABLE`
- [x] design for a future per-frame metadata array; do not paint into a corner where refcounts cannot be added

### 1.2 Page tables
- [x] 4-level walk with typed abstractions for `PhysAddr`, `VirtAddr`, and PTE flags
- [x] `map_page`, `map_range`, `unmap_page`, `translate`, with 2 MiB page support
- [x] build a fresh PML4 from buddy frames: kernel image per section, physmap at the HHDM offset with 2 MiB pages, 512 MiB low identity with the first 2 MiB executable, bootloader stack window duplicated from Limine's tables
- [x] `map_end` derived from usable RAM, kernel image end, and framebuffer extent, capped at 8 GiB
- [x] set `EFER.NXE` before installing, then `mov cr3`
- [x] `invlpg` after every single-PTE edit
- [x] TLB shootdown hook present as a single-CPU no-op so phase 4 changes one function, not fifty call sites
- [x] assert on overlapping regions and on mapping over an existing present entry unless explicitly asked to remap

### 1.3 MMIO attributes
- [x] `patch_physmap_uc(phys, len)` setting PCD and PWT without splitting 2 MiB entries
- [x] `ioremap(phys, len)` reserving from the dedicated MMIO window for devices that should not be reached through the physmap
- [x] in-guest test: patch a page, verify the PTE flags read back

### 1.4 Kernel heap
- [x] free-list heap at `HEAP_START`, 1 MiB initial, growing in page increments to the 64 MiB region cap
- [x] backing pages from the buddy allocator, mapped writable and NX
- [x] `GlobalAlloc` wrapper disabling interrupts across `alloc` and `dealloc`
- [x] `#[alloc_error_handler]` panicking with the failed `Layout`
- [x] in-guest tests: `Box`, `Vec` growth past the initial mapping, alignment from 1 to 4096, allocate/free/reallocate reuse, OOM reaching the error handler rather than corrupting

### 1.5 Kernel VA allocator
- [x] range allocator over the 64 GiB KVA region, first fit, freed ranges to the tail of the free list
- [x] `alloc_guarded_stack(pages)`: reserve `pages + 1`, map the upper `pages` from separate order-0 frames, leave the bottom unmapped
- [x] `vmap(frames)` for non-contiguous physical memory presented contiguously
- [x] deferred free list for stacks that cannot be unmapped yet, with an explicit drain
- [x] assert the region is entirely unmapped before claiming it
- [x] in-guest tests: guard page write faults, alloc/free roundtrip restores the frame count, deferred drain actually frees

### 1.6 In-guest test infrastructure
- [x] `kernel_tests` feature builds a second kernel that runs a test registry after init
- [x] separate Cargo target directory and separate ISO, so a test build can never be packaged as production
- [x] `ktest_ok` / `ktest_fail` / `ktest_skip(name, reason)` with the serial protocol from [DESIGN.md](DESIGN.md#82-in-guest-tests)
- [x] `isa-debug-exit` at port `0xf4`: `0x10` for pass, `0x11` for fail
- [x] `tests/harness/run_ktest.py` requiring `begin` and `end`, rejecting any `FAIL`, checking the exit status
- [x] failing tests print enough context to diagnose without a rerun

### 1.7 Diagnostics
- [x] `meminfo`-style dump: total, free, used, largest order, heap used and capacity, KVA used
- [x] page table dump walker for debugging, printing ranges rather than individual entries

---

## Phase 2: Traps, ACPI, and Time

**Goal.** Interrupts that work, exceptions that explain themselves, hardware discovery through ACPI,
and a monotonic clock nobody has to distrust.

**Unlocks.** Preemption. Delays that AP bring-up needs. Every driver.

**Exit gate**
- [x] `gdt ok`, `pic: remapped`, `idt ok`, `acpi: xsdt <n> tables`, `time: tsc <n>/ms` in the order [DESIGN.md](DESIGN.md#33-_start-order) specifies
- [x] `int3` returns cleanly; a deliberate `#GP` prints a full register dump and halts
- [x] a deliberate stack overflow lands in the double fault handler on its IST stack, proven by an in-guest test
- [x] PIT tick advances `uptime_ms` at 1 kHz within tolerance
- [x] TSC calibrated against the HPET when present, PIT channel 2 otherwise, both paths tested
- [x] `now_us` monotonic across 10k reads with a timer firing underneath, both straight-line and under yields
- [x] host tests: RSDP v1 and v2 checksum rejection, HPET table validation, MADT iteration over truncated input, `now_us` seqlock retry under a simulated concurrent writer

### 2.1 GDT, TSS, IST
- [x] flat GDT: null, kernel code, kernel data, user code, user data, TSS
- [x] user selectors placed in the order `syscall`/`sysret` requires, before ring 3 exists
- [x] TSS with `RSP0` and the IST array
- [x] IST stacks page-aligned and per-CPU: 1 double fault, 2 NMI, 3 machine check, 4 debug
- [x] structure it so per-CPU GDT and TSS instances are natural, since phase 4 needs them

### 2.2 IDT and exceptions
- [x] all 256 entries populated; unhandled vectors get a diagnostic default rather than a reserved gate
- [x] `x86-interrupt` ABI handlers
- [x] halting handlers dump RIP, CS, RFLAGS, RSP, SS, the error code, and CR2 for faults
- [x] `#BP` logs and returns; `#UD`, `#GP`, `#PF`, `#DF`, `#MC` log and halt
- [x] a scoped transient fault handler for tests: install, run a faulting operation, step RIP past it, restore
- [x] named vector constants in one module with a host test asserting uniqueness
- [x] in-guest tests: `int3` roundtrip, double fault on IST via stack overflow, the scoped handler catching a deliberate `#PF`

### 2.3 8259 PIC
- [x] remap master to `0x20`, slave to `0x28`, ICW sequence with `io_wait` between writes
- [x] mask everything immediately after remap
- [x] `unmask(irq)` / `mask(irq)` / `disable_all()`
- [x] read FADT `iapc_boot_arch` bit 0 and skip PIC setup entirely when no legacy 8259 exists
- [x] spurious IRQ7 and IRQ15 handled without a bogus EOI

### 2.4 ACPI
- [x] RSDP validation: signature, v1 checksum over 20 bytes, v2 extended checksum over the full length
- [x] XSDT walk with per-table checksum validation, RSDT fallback
- [x] all packed field access through `read_unaligned`
- [x] MADT: LAPIC base, type 5 address override, I/O APIC entries with GSI bases, type 2 interrupt source overrides, enabled processor APIC IDs
- [x] HPET: main counter base and period, rejecting zero addresses and I/O-space generic address structures
- [x] FADT: legacy 8259 presence, and the reset and shutdown registers for later
- [x] MCFG: PCIe ECAM base, stored for phase 6
- [x] parsing lives in the library half against synthetic table bytes, so all of it is host-tested
- [x] MMIO for every discovered table patched uncacheable before first access
- [x] boot log summarizing what was found: table count, CPU count, I/O APIC count, HPET presence

### 2.5 PIT and the bootstrap tick
- [x] channel 0, mode 2, divisor 1193 for ~1 kHz on IRQ0
- [x] `io_wait` between the low and high divisor byte writes
- [x] handler: increment the tick counter, snapshot the TSC, EOI, return. No allocation, no logging.
- [x] channel 2 one-shot for calibration, gated through port `0x61`

### 2.6 TSC calibration
- [x] check the invariant TSC CPUID bit and log loudly if absent
- [x] `lfence` before `rdtsc`, or use `rdtscp`
- [x] calibrate against the HPET main counter over ~10 ms when available
- [x] fall back to PIT channel 2 with a count of 11932
- [x] store `tsc_per_ms` per CPU, not globally
- [x] sanity-check the result against a plausible range and refuse a value that would poison every delay downstream
- [x] `busy_wait_ms` on the TSC, using `hlt` when interrupts are enabled

### 2.7 Timekeeping
- [x] tick counter and TSC snapshot published under a seqlock: bump, write both, bump, with release ordering
- [x] reader retries until it sees a stable even sequence, with acquire ordering
- [x] `uptime_ms`, `now_us`, `now_ns` built on it
- [x] the interpolation arithmetic lives in the library half and is host-tested including near `u64::MAX`
- [x] seqlock test publishes an independent observed timestamp so a torn read actually fails the test
- [x] a `next_deadline(instant)` interface rather than a hardcoded periodic tick, so tickless is possible later
- [x] RTC read once at boot for wall clock, tracked forward with the monotonic clock plus an offset

### 2.8 Diagnostics
- [x] `uptime` reporting tick milliseconds and TSC microseconds side by side, so divergence is visible
- [x] a boot line naming the calibration source and the measured frequency

---

## Phase 3: Threads and Scheduling

**Goal.** More than one thing running. Preemptive round-robin over kernel threads with real
synchronization primitives.

**Unlocks.** Blocking drivers. Anything that waits. Processes, eventually.

**Exit gate**
- [x] `sched: cpu0 ready` marker, and `irq: enabled` after it
- [x] two spawned threads interleave visibly in the log without either yielding voluntarily
- [x] `sleep_ms(50)` returns between 50 and 100 ms, measured in-guest
- [x] in-guest: 1000 iterations of two threads contending a blocking mutex, no deadlock, correct final count
- [x] a thread that returns is reaped and its stack returned to the allocator, proven by the frame count
- [x] the idle thread runs when nothing else is ready and the system does not wedge
- [x] host tests for the run queue state machine and the timeout ordering structure

### 3.1 Thread abstraction
- [x] `ThreadId`, `Tcb` with state, kernel stack handle, saved context, entry point, name
- [x] states: ready, running, sleeping with deadline, blocked on a wait queue, dead
- [x] guarded 16 KiB kernel stack per thread from the KVA allocator
- [x] `spawn(name, fn)` returning a handle
- [x] a global TCB table so a thread is addressable by id from anywhere, laid out for per-CPU queues in phase 4

### 3.2 Context switch
- [x] `switch_context(old: *mut CpuContext, new: *const CpuContext)` in `global_asm!`
- [x] save and restore callee-saved GPRs, `rflags`, `rsp`, and the return address
- [x] new threads start on a synthetic frame that returns into the trampoline that calls the entry point
- [x] a thread returning from its entry point marks itself dead and schedules, never falls off the stack
- [x] no FPU or SSE state yet, consistent with the soft-float target; revisit deliberately, not accidentally

### 3.3 Scheduler
- [x] ready queue, sleep queue ordered by deadline, per-wait-queue blocked lists (`WaitQueue`)
- [x] `schedule()` for the voluntary path
- [x] `on_timer_tick()` called after EOI, preempting every 10 ticks
- [x] `yield_now()`
- [x] dead thread reaping deferred to a context not running on the dying stack
- [x] every scheduler lock acquisition inside an interrupt guard, no exceptions
- [x] context switch counter and per-thread run time accounting from the start; retrofitting instrumentation is worse than building it in

### 3.4 Sleep and timeouts
- [x] `thread::sleep_ms` parks on the sleep queue and wakes from the timer path
- [x] one timeout structure: sorted list under a lock initially, with the interface a timing wheel can replace
- [x] every blocking primitive takes an optional deadline. No exceptions, so a permanently blocked thread is impossible by construction
- [x] a periodic sweep that logs threads blocked far past any plausible timeout

### 3.5 Synchronization
- [x] `InterruptGuard`: save `RFLAGS.IF`, `cli`, restore on drop, correctly nested
- [x] one `SpinMutex`: CAS acquire, IRQ-aware, `assert!` not `debug_assert!` on misuse
- [x] `BlockingMutex` over a wait queue, with the enqueue then mark then drop then schedule ordering
- [x] `RwLock`, `Semaphore`, `Condvar`
- [x] `Channel<T>` bounded MPSC for producer/consumer between threads
- [x] `WaitQueue` as the shared primitive underneath all of them
- [x] host tests for whatever is testable without hardware; in-guest tests for contention

### 3.6 Idle
- [x] one idle thread, always runnable, lowest priority, `sti; hlt`
- [x] idle time accounted so a load figure is possible later
- [x] structured for one idle thread per CPU in phase 4

### 3.7 Verification
- [x] in-guest: spawn writes a sentinel; N threads increment a shared counter under the blocking mutex; `sleep_ms` accuracy; `yield_now` actually switches; the idle thread runs
- [x] hold the scheduler lock with IF off, assert ticks freeze (cannot `int $0x20` while holding: recursive SCHED), then force a timer IRQ after drop and check nest. Regression for a lock taken with interrupts enabled.
- [x] stress: spawn and exit thousands of threads, assert the frame count returns to baseline

---

## Phase 4: SMP

**Goal.** Every core running, every core scheduling, and the locking correct enough that it stays up
under load.

**Unlocks.** Real parallelism. Also every latent locking bug in phases 1 through 3, which is why this
comes before drivers rather than after.

**Exit gate**
- [x] `smp: done` before `shell ready`, with exactly `N-1` `smp: ap online` lines at `-smp N`
- [x] `sched: cpu<i> ready` for every CPU
- [x] `time: lapic_timer ok (<mode>)` naming the mode that was selected
- [x] `make test-kernel` at `-smp 2` and `-smp 4` both pass
- [x] `make test-lapic-fallback` passes with `-cpu qemu64,-tsc-deadline`
- [x] a thread spawned on CPU 0 observably runs on another CPU
- [x] remote unmap and remap through the shootdown path passes in-guest with 2+ CPUs
- [x] failed AP bring-up frees everything it allocated, verified by frame count with a fault injected

### 4.1 LAPIC
- [x] enable via `IA32_APIC_BASE` bit 11, honoring the MADT type 5 address override
- [x] spurious vector register set to `0xFF` with the enable bit
- [x] TPR set to 0
- [x] EOI helper, and dispatch that knows PIC-routed from APIC-routed vectors
- [x] `send_ipi(dest, vector, mode)` with a bounded delivery-pending poll
- [x] the poll loop in the library half with host tests for both the clears and the timeout case
- [x] LVT setup for error and thermal, so a LAPIC error is reported rather than silent

### 4.2 I/O APIC
- [x] enumerate every I/O APIC from the MADT with its GSI base
- [x] indirect register access through `IOREGSEL` and `IOWIN`
- [x] `route_gsi(gsi, vector, cpu, trigger, polarity)` writing the high dword before the low
- [x] apply interrupt source overrides; never assume ISA IRQ *n* is GSI *n*
- [x] `mask_gsi` / `unmask_gsi`
- [x] mask the PIC completely once routing is live and the LAPIC timer is verified

### 4.3 LAPIC timer
- [x] detect TSC-deadline via `CPUID.01H:ECX[24]`; LVT mode `10b`, arm `IA32_TSC_DEADLINE`
- [x] fall back to periodic mode, calibrating LAPIC ticks per millisecond against the HPET with divider 16
- [x] fall back to the PIT with a single global tick and no per-CPU preemption
- [x] rearm before calling the scheduler
- [x] mask the PIT's GSI when the LAPIC owns the tick
- [x] log which mode was chosen, and make it an e2e assertion so a silent downgrade is not invisible
- [x] in-guest tests: the timer fires, and rearm works across many ticks

### 4.4 AP trampoline
- [x] `trampoline.S` via `global_asm!` into `.trampoline`, copied to `0x8000`; blob fits below the param block
- [x] real mode to protected mode to long mode, setting `EFER.LME` and `EFER.NXE`
- [x] parameter block at the documented offsets: CR3, stack top, entry point, IDT pointer
- [x] each parameter written with `write_volatile`, `compiler_fence(SeqCst)` before the SIPI
- [x] `0x8000` identity mapped, executable, and permanently excluded from the PMM
- [x] a static assertion that the blob fits below its parameter block

### 4.5 AP bring-up
- [x] one AP at a time, INIT, 10 ms, SIPI, ~1 ms, SIPI
- [x] per-CPU area and guarded stack allocated and published with a fence before the SIPI
- [x] 3 second ready-flag timeout; on failure free the stack and per-CPU area, log, and continue
- [x] AP path in order: per-CPU GDT and TSS, per-CPU MSRs (`GS_BASE` before any `lidt`), IDT, LAPIC enable, timer calibrate and arm, ready flag, `sti`, enter as idle
- [x] `GS_BASE` set before the IDT is live and before `sti`
- [x] an online mask, and a barrier the BSP waits on before declaring `smp: done`

### 4.6 Per-CPU data
- [x] `PerCpu` with `self_ptr` at offset 0, reached through `GS_BASE`
- [x] `KERNEL_GS_BASE` set to the same value, with scratch fields reserved for the future syscall path
- [x] heap-allocated array sized from the actual MADT CPU count
- [x] `per_cpu!` accessors safe from interrupt context
- [x] contents: cpu id, APIC id, ready queue, wake inbox, idle handle, current thread, local ticks, context switches, `tsc_per_ms`, timer mode
- [x] in-guest identity tests on both BSP and AP

### 4.7 Locking audit
- [x] every lock taken from an ISR audited for interrupt-disabled acquisition in all contexts
- [x] the documented global lock order enforced by review, and by a debug-build lock order tracker if that turns out to be cheap
- [x] serial TX locked at byte granularity
- [x] buddy, heap, page tables, scheduler all confirmed SMP-safe under contention
- [x] a stress test hammering the allocator from every CPU simultaneously

### 4.8 Per-CPU scheduling
- [x] per-CPU ready queues, global TCB table
- [x] `CpuAffinity::{Any, Pinned(cpu)}`
- [x] round-robin placement for `Any`
- [x] cross-CPU wake through the target's inbox plus a reschedule IPI, never a remote queue lock
- [x] one idle thread per CPU with its own stack
- [x] global sleep queue under one lock for now
- [x] when locking two CPU-local structures, lower `cpu_id` first
- [x] in-guest: cross-CPU spawn roundtrip, reschedule IPI delivery, waking an idle AP

### 4.9 IPIs
- [x] `0xFD` reschedule
- [x] `0xFC` TLB shootdown
- [x] `0xFB` call-function, with a wait-for-completion variant
- [x] `0xFE` panic halt broadcast, so a panic stops the other cores before they overwrite the log
- [x] handlers allocation-free; shootdown/call/halt take neither PT nor SCHED; reschedule takes SCHED IRQ-off

### 4.10 TLB shootdown
- [x] update the PTE, broadcast, wait for acknowledgement from every online CPU
- [x] the waiting initiator keeps IF off and services incoming shootdown requests so two simultaneous shootdowns cannot deadlock
- [x] KVA free deferred until the shootdown completes; freed ranges to the tail of the free list
- [x] in-guest: unmap on one CPU, verify a fault on another, remap, verify access

### 4.11 CI variants
- [x] `-smp 2` as the default everywhere including e2e
- [x] `-smp 4` target
- [x] `-cpu qemu64,-tsc-deadline` target
- [x] a longer-running high-CPU stress variant on a schedule rather than every push
- [x] `cpus` diagnostic output: logical and APIC id, online mask, timer mode, local ticks, ready depth, context switches

---

# Era II. System

From a kernel that schedules to an operating system that runs programs against files, then the same
operating system on a second architecture, then the memory and process semantics real software assumes.

## Phase 5: Console, Input, and Logging

**Goal.** Interactive from the QEMU window, and observable enough to debug the phases that follow.

**Unlocks.** Manual exploration. `dmesg`. A shell to hang commands off.

**Exit gate**
- [x] `console ok` and `shell ready` markers
- [x] typing in the QEMU window echoes on the framebuffer and drives commands
- [x] `dmesg` shows the full boot log including lines emitted before the framebuffer existed
- [x] a panic on any CPU stops the others and leaves a readable dump with a symbolized backtrace
- [x] host tests: scan code decoding, ring buffer wrap, line editing, command tokenization
- [x] log level filtering changeable at runtime and visible in output

### 5.1 Framebuffer console
- [x] BGRX pixel writes at `base + y * pitch + x * 4`, with bounds checks that are not `debug_assert`
- [x] 8x8 bitmap font, LSB leftmost, ASCII 32 to 126, with a defined glyph for everything else
- [x] text grid with wrap and scroll; scroll by `memmove` of whole rows
- [x] a banner region that survives scrolling
- [x] host tests for the font table and for the pixel bounds arithmetic
- [ ] double buffering off the physmap once there is memory to spare, so scrolling stops tearing

Double buffering parked (Design ACK). Lands with §16.1, where the display abstraction owns the scanout buffer.

### 5.2 PS/2 keyboard
- [x] controller init, self test, and enabling the first port
- [x] scan code set 1 decoding including `0xE0` prefixes
- [x] modifier state: shift, ctrl, alt, caps lock, num lock
- [x] `DecodedKey` as either a character or a named non-printing key
- [x] fixed-size ring buffer written by the ISR, drained by the consumer with interrupts disabled
- [x] the decoder and ring buffer in the library half, fully host-tested
- [x] ISR does no allocation and no logging

### 5.3 Console multiplexer
- [x] `ConsoleBackend` trait: write, and optional read
- [x] serial and framebuffer backends registered, output fanned out to both
- [x] input merged from the PS/2 ring and from serial RX
- [x] one lock discipline that does not deadlock against the keyboard ISR
- [x] runtime enable and disable per backend

### 5.4 Shell
- [x] line editor: echo, backspace, ctrl+C, ctrl+U, cursor movement, history
- [x] tokenizer handling quotes and consistent whitespace behavior, in the library half with host tests
- [x] built-ins: `help`, `echo`, `meminfo`, `uptime`, `cpus`, `dmesg`, `ps`, `panic`, `reboot`, `poweroff`
- [x] a command table that new subsystems register into rather than a growing match arm
- [x] running as a kernel thread, not in `_start`, so it can block

### 5.5 Kernel log
- [x] levels: error, warn, info, debug, trace, with compile-time and runtime filtering
- [x] a lock-free-enough ring buffer that survives a panic and is readable afterward
- [x] every record timestamped from the monotonic clock and tagged with its CPU
- [x] boot lines captured into the ring before the framebuffer exists (replay when FB lands in B)
- [x] `dmesg` with level filtering and follow (`dmesg -f`; Ctrl+C to stop)
- [ ] a per-CPU buffer with a printer thread, so log lines become atomic rather than merely non-interleaved bytes

Printer thread parked: global IRQ-safe ring + serial try-lock sink (Design ACK). Lands with §19.5.

### 5.6 Panic and diagnostics
- [x] broadcast the halt IPI first so other cores stop before the log is written
- [x] symbolized backtrace: keep a symbol table in the image, walk frame pointers
- [x] dump the register state, the current thread, and the last few log records
- [x] optional QEMU exit on panic under a test feature, so a panic fails a run immediately instead of hanging

---

## Phase 6: Device Model and Buses

**Goal.** Find hardware, talk to it, and take interrupts from it without every driver reinventing
enumeration and DMA.

**Unlocks.** All of storage, network, and graphics.

**Exit gate**
- [x] `pci: <n> devices` marker and `lspci` output matching QEMU's configuration
- [x] MSI-X interrupts delivered to a chosen CPU, verified in-guest
- [x] a virtio device negotiated through modern PCI capabilities with a working virtqueue
- [x] DMA buffers allocated, mapped, and verified for correct device-visible addresses
- [x] a driver bound to a device automatically by id match, not by hardcoded probing order
- [x] workqueue and threaded IRQ handlers exercised in-guest

### 6.1 Device model
- [x] `Device` with a bus address, ids, resources, and an interrupt binding
- [x] `Driver` trait: `probe`, `remove`, plus an id match table
- [x] a registry matching drivers to devices, with driver init ordered by dependency
- [x] resource tracking so two drivers cannot claim the same BAR
- [x] a device tree dump for the shell

### 6.2 PCI and PCIe
- [x] legacy configuration access through ports `0xCF8` and `0xCFC`
- [x] ECAM through the MCFG base, which is required for anything beyond bus 0
- [x] recursive bus enumeration across bridges
- [x] BAR decoding: memory versus I/O, 32 versus 64 bit, size probing, `ioremap` of memory BARs
- [x] capability list walk: MSI, MSI-X, PCIe, vendor specific, power management
- [x] enable bus master and memory space in the command register
- [x] `lspci` with vendor and device names for the handful worth naming

### 6.3 MSI and MSI-X
- [x] MSI configuration: message address, message data, enable
- [x] MSI-X table in a BAR, per-vector address and data, per-vector mask
- [x] allocate vectors from the dynamic pool, bound to a chosen CPU
- [x] fall back to legacy INTx through the I/O APIC when a device has neither
- [x] `free_vector` masks the I/O APIC GSI before dropping an INTx route
- [x] in-guest test: trigger a device interrupt, assert it arrived on the intended CPU
- [x] interrupt affinity API, so phase 19 can rebalance without redesign

### 6.4 DMA
- [x] `DmaBuffer`: physically contiguous, known device address, explicit coherency
- [x] allocation from the buddy allocator with an alignment and boundary constraint
- [x] `sync_for_device` and `sync_for_cpu` as explicit calls even when they are no-ops on x86, because aarch64 will need them
- [x] scatter-gather list construction for devices that support it
- [x] barriers around descriptor publication, using the right fences rather than `compiler_fence` everywhere
- [x] IOMMU support deferred to §18.1, with the address translation kept behind an interface so it can be inserted

### 6.5 virtio
- [x] modern virtio over PCI: common configuration, notify, ISR, and device-specific capability regions
- [x] feature negotiation with the `VIRTIO_F_VERSION_1` handshake and a clear failure when features are missing
- [x] split virtqueue: descriptor table, available ring, used ring
- [x] queue setup, kick, and completion handling with the correct barriers
- [x] indirect descriptors, and `VIRTIO_F_EVENT_IDX` for interrupt suppression
- [x] the ring index arithmetic in the library half, host-tested against a simulated device

### 6.6 Deferred work
- [x] a workqueue: kernel threads consuming queued work items
- [x] threaded IRQ handlers, where the top half acknowledges and wakes a thread that may allocate and block
- [x] softirq-equivalent for latency-sensitive deferred work such as network receive
- [x] a documented rule for which handlers may block, because getting this wrong is a class of bug rather than an instance

---

## Phase 7: Block Storage

**Goal.** Bytes that survive a reboot, behind an interface a filesystem can use.

**Unlocks.** Filesystems, swap, a real userspace on disk.

**Exit gate**
- [x] `block: <name> <n> sectors` for each detected device
- [x] write a pattern, reboot the VM, read it back intact
- [x] GPT and MBR partition tables parsed and exposed as separate block devices
- [x] concurrent reads and writes from multiple threads with no corruption, verified in-guest
- [x] the cache demonstrably reduces device requests, measured by a counter not by assertion
- [x] host tests: partition table parsing including deliberately corrupt tables, request merging, cache eviction

### 7.1 Block layer
- [x] `BlockDevice` trait: logical block size, capacity, read, write, flush, discard
- [x] a request structure with a completion, supporting both blocking and async submission
- [x] per-device request queue with adjacent-request merging and a simple elevator
- [x] barrier and flush semantics defined, since a journaling filesystem depends on them
- [x] a ramdisk implementation first, so the layer is testable before any real driver
- [x] error propagation with retry, and a device marked failed rather than retried forever

### 7.2 virtio-blk
- [x] probe and configuration read: capacity, block size, topology
- [x] request submission through the virtqueue with proper descriptor chaining
- [x] completion through the interrupt path into request completions
- [x] multi-queue with one queue per CPU
- [x] flush and discard support
- [x] in-guest: sector roundtrip, unaligned multi-sector, deep queue with concurrent submitters

### 7.3 Partitions
- [x] MBR parsing including extended and logical partitions
- [x] GPT parsing with header and entry CRC validation and backup header fallback
- [x] partitions exposed as offset-limited block devices
- [x] type GUID recognition for the ones that matter
- [x] host tests over real table images, including truncated and CRC-broken cases

### 7.4 Cache
- [x] a page-granular cache over block devices, keyed by device and offset
- [x] read-through, write-back with an explicit flush, and dirty tracking
- [x] LRU eviction with a clock or second-chance approximation
- [x] readahead on detected sequential access
- [x] a writeback thread with a bounded dirty ratio
- [x] built so phase 12 can unify it with the page cache rather than maintaining two caches
- [x] hit and miss counters exposed in the shell

---

## Phase 8: Filesystems

**Goal.** Paths, files, directories, mounts. Then a filesystem of our own that does not have FAT's
limitations.

**Unlocks.** Loading binaries. A userspace that persists. Configuration.

**Exit gate**
- [x] mount a FAT32 image and `ls`, `cat`, `mkdir`, `rm`, `cp` behave correctly
- [x] write, unmount, and verify the image with host `fsck.fat` clean
- [x] `/dev`, `/proc`, and `/tmp` populated by their respective filesystems
- [x] vibefs survives injected power loss during a write, verified by a crash-consistency test
- [x] path resolution handles `.`, `..`, symlinks, mount point crossing, and a symlink loop without recursing to death
- [x] host tests over synthetic filesystem images, including deliberately corrupted ones
- [ ] tag `v0.8.0`

### 8.1 VFS
- [x] `Inode` with a type, size, mode, times, and link count
- [x] `Dentry` with a name-to-inode cache and negative caching
- [x] `Superblock` per mount, with a mount table and mount point crossing
- [x] `FileSystem` trait for mount and root lookup; `InodeOps` for lookup, create, unlink, read, write, truncate, readdir, stat
- [x] path resolution with a bounded symlink depth
- [x] `File` with an offset and flags, and a descriptor table that phase 9 hands to processes
- [x] reference counting and a defined lifetime for an unlinked-but-open file
- [x] inode and dentry caches with eviction, since an unbounded cache is a slow memory leak

### 8.2 FAT32 read
- [x] BPB parsing and validation
- [x] FAT chain walking with a cluster cache
- [x] directory entry parsing, including long file names and their checksum validation
- [x] file read across cluster boundaries
- [x] `readdir`, `stat`, timestamp conversion
- [x] the on-disk structure parsing in the library half, host-tested against a generated image

### 8.3 FAT32 write
- [x] cluster allocation with a free cluster hint, FAT chain extension
- [x] file create, write, truncate, delete
- [x] directory create and delete, including long file name entry generation
- [x] both FAT copies and `FSInfo` kept in sync
- [x] flush ordering that does not leave a directory entry pointing at unallocated clusters
- [x] verified by mounting the result on the host and running `fsck.fat`

### 8.4 Pseudo filesystems
- [x] `devfs`: block devices, `null`, `zero`, `random`, `console`, `tty`
- [x] `tmpfs` on the Phase 7 block cache over a fixed-size ramdisk, so writes evict through the cache instead of growing a buffer; the reclaimable page-cache version is §12.5
- [x] `procfs`: per-process directories, `cmdline`, `status`, `maps`, `fd`  (a pid-1 stub until §13.9 backs it with the process table)
- [x] `sysfs`-equivalent for the device tree and driver bindings
- [x] a `kernfs`-style shared implementation so the four do not duplicate directory logic

### 8.5 vibefs
- [x] the case for it: FAT32 has no permissions, no symlinks, no journaling, and no checksums, and every one of those becomes a wall
- [x] on-disk format documented in `docs/` before any code, with an explicit version field
- [x] extent-based allocation rather than block lists
- [x] B-tree directories, so a large directory is not a linear scan
- [x] checksums on metadata and optionally on data, with corruption reported rather than propagated
- [x] copy-on-write metadata updates with atomic superblock switching, or a write-ahead journal. Pick one, document why.
- [x] snapshots, which fall out nearly free from copy-on-write
- [x] inline data for small files
- [x] a host-side `mkfs` and `fsck` sharing the same format code as the kernel, so they cannot disagree
- [x] crash consistency testing by killing QEMU at randomized points during a write workload, then checking with `fsck`

### 8.6 File API and shell
- [x] kernel-side open, read, write, seek, close, stat, readdir, mkdir, unlink, rename, symlink, link, truncate
- [x] shell commands: `ls -l`, `cat`, `cp`, `mv`, `rm -r`, `mkdir -p`, `touch`, `stat`, `df`, `mount`, `umount`, `sync`
- [x] tab completion over the current directory, which is disproportionately useful when debugging by hand
- [x] an initial ramdisk image built by the Makefile and mounted at boot, so there is a root filesystem before block drivers are trustworthy

---

## Phase 9: User Mode and Processes

**Goal.** Untrusted code in ring 3, isolated by hardware, talking to the kernel only through syscalls.

**Unlocks.** Everything that makes this an operating system rather than a large program.

**Exit gate**
- [x] a static ELF binary loaded from the filesystem runs in ring 3 and exits with a status the kernel reports
- [x] `write` to fd 1 from userspace reaches the console
- [x] `fork` and `exec` produce a child that runs a different binary; the parent's `wait` returns its status
- [x] a user process faulting on a bad pointer is killed with a diagnostic and does not take the kernel with it
- [x] the interactive shell runs as a user process, not in the kernel
- [x] every syscall argument validated: an in-guest test passes kernel pointers, unmapped pointers, and huge lengths and gets `EFAULT` rather than a panic
- [x] `ps` lists processes with real state
- [ ] tag `v0.9.0`

### 9.1 Ring 3 plumbing
- [x] user code and data selectors already in the GDT, verified against the `sysret` layout
- [x] `IA32_STAR`, `IA32_LSTAR`, `IA32_FMASK` configured; `EFER.SCE` enabled
- [x] `syscall` entry: `swapgs`, switch to the kernel stack from `PerCpu`, save the user context, dispatch
- [x] exit path restoring the context and `sysretq`, with the `iretq` slow path for cases `sysret` cannot express
- [x] `RSP0` in the TSS updated on every context switch so an interrupt in ring 3 lands on the right kernel stack
- [x] SSE and FPU state actually saved and restored now: `fxsave`/`xsave`, lazily if the accounting is right, and the target spec's float settings revisited
- [x] SMEP/SMAP/UMIP where CPUID allows, `CR0.WP` asserted on every CPU, `stac`/`clac` helpers (S1)

### 9.2 Address spaces
- [x] `AddressSpace` owning a PML4, with the kernel half shared by mapping the same upper entries
- [x] user mappings tracked as regions with permissions and a backing source, not just raw PTEs
- [x] CR3 switch on context switch, skipped when the next thread shares the address space
- [x] teardown freeing every user frame and page table page, verified against the frame count
- [x] user pointer access helpers that check the range and handle a fault, rather than trusting and hoping
- [x] a guard region at address 0 so a null dereference faults rather than reading something

### 9.3 Syscall ABI
- [x] the ABI documented in `docs/`: register assignment, return convention, error encoding
- [x] a dispatch table indexed by number, with an arity and a validation policy per entry
- [x] first set wired for proof: `write`, `exit`, `getpid`, `sched_yield`
- [x] remainder: `read`, `open`, `close`, `lseek`, `fork`, `execve`, `wait4`, `getppid`, `dup`, `dup2`, `kill`, `fcntl`
- [ ] `brk`, anonymous `mmap`/`munmap`, `getdents64`, `fstat`, and `nanosleep`: land in §10.5
- [ ] `openat`, `dup3`, and fork-shaped `clone`: land in §11.6
- [ ] `stat` and the rest of the POSIX floor: land in §13.9
- [x] `errno` values matching Linux where a name exists, so ported software behaves
- [x] every pointer argument validated against the caller's address space before use
- [x] syscall tracing behind a flag, since the alternative is guessing why a program failed
- [x] a syscall counter per process for `procfs`

### 9.4 ELF loader
- [x] ELF64 header validation: class, endianness, machine, type
- [x] `PT_LOAD` segments mapped with permissions from the flags, honoring `p_filesz` versus `p_memsz` zero fill
- [x] `PT_GNU_STACK` respected for stack executability
- [x] `PT_INTERP` recognized, dynamic loading deferred to phase 14 but detected rather than silently ignored
- [x] stack set up with argv, envp, and the auxiliary vector
- [x] `PT_TLS` and the TLS layout, since Rust and C both want it
- [x] the header parsing in the library half, host-tested against real binaries and truncated ones
- [x] refuse a malformed binary with an error rather than mapping garbage

### 9.5 Process abstraction
- [x] `Process`: pid, parent, address space, descriptor table, working directory, credentials, exit status
- [x] threads belong to a process; a process is one or more threads sharing an address space
- [x] the descriptor table with per-fd flags, `dup`, `dup2`, and close-on-exec
- [x] a process tree with reparenting to init on parent death
- [x] zombie state until reaped, and a defined resource release point
- [x] uid and gid present from the start even if nothing enforces them yet, because retrofitting credentials is painful

### 9.6 fork, exec, wait
- [x] `fork`: clone the address space, duplicate descriptors, copy the thread context, return 0 in the child
- [x] initially a full copy; copy-on-write in phase 12, with the interface unchanged
- [x] `execve`: build the new address space first, and only replace the old one after the load succeeds, so a failed exec leaves the caller intact
- [x] `exit`: release resources, become a zombie, signal the parent
- [x] `wait4`: block for a child, return its status, reap it, with `WNOHANG`
- [x] orphan reaping by init
- [x] in-guest: fork bomb bounded by a process limit, exec chain, wait ordering, orphan reparenting

### 9.7 Early signals
- [x] `SIGKILL` and `SIGSTOP` handled in the kernel with no user handler
- [x] `SIGSEGV`, `SIGBUS`, `SIGFPE`, `SIGILL` generated from the corresponding exceptions
- [x] `SIGCHLD` on child exit
- [x] default actions: terminate, ignore, stop
- [x] user-installed handlers, masking, and queueing deferred to phase 13

### 9.8 First userspace
- [x] a minimal freestanding user program with hand-written syscall stubs and no libc, to prove the path
- [x] a userspace test runner exercising each syscall and its error cases
- [x] the shell moved out of the kernel and into a user process, keeping the kernel one only under a debug feature
- [x] the kernel's job after init becomes starting `/sbin/init` and nothing else

---

## Phase 10: Consolidation

**Goal.** Pay down what Phase 0 laid down in a hurry, before the kernel grows a second architecture and
a real userspace. Most of it came out of the 2026-09-22
[architecture review](reviews/ARCHITECTURE_REVIEW.md); the per-item plans are under
[reviews/issues/](reviews/issues/README.md) and the letter codes below name them. The rest came out of
the [roadmap review](reviews/ROADMAP_REVIEW.md): the entry paths and user memory, the user runtime, and
the CI budget. Nothing here is a feature for its own sake. All of it is what makes the next three phases
checkable.

**Unlocks.** A portable core that a second architecture can share. A user runtime that can express the
Phase 12 and Phase 13 gates. Tables that do not cap a shell pipeline at sixteen processes. Kernel entry
and user-memory paths that copy-on-write and demand paging can build on without a security hole.

**Exit gate**
- [x] `make check` (fmt, clippy with warnings denied, host units, harness units) gates CI ahead of the QEMU ladder
- [ ] `make check` passes on macOS and Linux, and a scheduled macOS CI job proves it
- [x] the nightly is pinned by date; Limine and every GitHub action are pinned by hash
- [ ] the portable crate contains no inline assembly and no `cfg(target_arch)`; hostlib is a normal workspace member
- [ ] the portable core builds and passes its host tests against the stub `arch` from §10.3, as part of `make check`
- [ ] syscall copies to and from the calling process go only through the §10.6 accessors, and only the named fill API writes to an address space that is not running. In-guest tests show three things. SMAP faults a stray kernel data access to a user page, including one from an exception handler entered with the accessor window open. SMEP faults a kernel jump into a user page. A kernel-half pointer, and `read()` into a read-only user page, both return `EFAULT`
- [ ] no user context reaches `iretq` or `sysretq` with a non-canonical RIP, and the kernel survives an ELF whose last page holds a `syscall` (§10.6)
- [ ] the growable tables are heap-sized at init from one `limits` module, and this gate states the limits Phase 12 is tested against: 256 processes, 256 descriptors per process, 1024 threads, 256 regions per address space
- [ ] FAT and vibefs implement `InodeOps`; the `Back` enum in `file_init` is gone; every file operation goes through `Vfs`
- [ ] one errno-shaped kernel error type; the errno table in `docs/SYSCALL.md` §2 is generated from it
- [ ] a Rust user runtime replaces the four assembly programs; `utest_*` results are asserted by the harness the way `ktest_*` are; the user `/bin/sh` runs programs from `PATH` and reports their exit status
- [ ] `make test` is green with no retry left in the harness (§10.2)
- [x] SMEP, SMAP, UMIP, and `CR0.WP` set on every CPU; `/dev/random` fed by virtio-rng or `RDRAND`
- [x] `BootInfo` captured once; nothing outside `boot` reads a Limine response
- [x] `AGENTS.md` and an MIT `LICENSE` in the tree; README status current
- [ ] every item in [reviews/issues/README.md](reviews/issues/README.md) is marked implemented or declined with a reason
- [ ] tag `v0.10.0`

### 10.1 Gates and pinning
- [x] `rustfmt.toml`, one `cargo fmt` commit, `cargo clippy -- -D warnings`, `RUSTFLAGS=-Dwarnings` (Q1)
- [x] a fast `check` CI job before the QEMU ladder, with an llvm-cov floor on the portable crate that only ratchets up (T3)
- [x] dated nightly; action SHAs; Limine commit verified after clone; cargo cache keyed on `Cargo.lock` (C1)
- [x] `make check` as the local gate; `ruff` and `mypy --strict` for `tests/` (DX1)
- [x] restriction lints on the portable crate: no `unwrap`, `expect`, or `panic!` outside tests (E1)
- [ ] the `unsafe` standing gate enforced by lint: `clippy::undocumented_unsafe_blocks` denied through `[workspace.lints.clippy]`, so every member inherits it (including `user/` from §10.5), and `check-private-items = true` in `clippy.toml`, so `missing_safety_doc` reaches the kernel binary's private items; a `// SAFETY:` line on every existing `unsafe` block (653 at `90ce475`)
- [ ] a nightly x86_64 KVM leg that runs `make test-kernel` now and, from Phase 12 on, every benchmark and every gate number measured under KVM, since TCG and KVM each hide bugs the other finds; the timing tests that made the harness default to TCG are fixed first, which HVF in Phase 11 also needs
- [ ] the CI budget, written into DESIGN §8.6 in the same commit as the workflow change. Every push runs `check` and, alongside it, one `build` job per architecture (x86_64 now, aarch64 from Phase 11). The build job builds every ISO variant and the host `mkfs`/`fsck` tools once and uploads them. A matrix of tier jobs per architecture (`needs: [check, build]`, `fail-fast: false`, TCG) downloads them and runs the same `make test-*` targets through a prebuilt-ISO switch, so the Makefile stays the one definition of each tier. Tiers are grouped to about 40 s of QEMU each (x86_64 at `88370e5`: the five e2e boots; in-guest at `-smp 2` and `-smp 4` plus the LAPIC fallback; the vibefs crash test), and each gets its own check name, so a red PR names the failing tier. Everything else runs on a schedule: the macOS job, the KVM leg, the fuzzers, stress, and any job with a performance threshold. A later line that says "in CI" for a functional test means a ladder tier; for a benchmark or a threshold it means the KVM leg. A red scheduled job blocks the next phase tag. The earlier no-matrix rule (runner queues) is lifted: the repository is public, so minutes are free, and the limit that matters is 20 concurrent jobs per account
- [ ] CI wall time cut inside each job: host packages from a cache or a prebuilt image rather than `apt-get` on every run (about 20 s of every job at `88370e5`), and independent QEMU runs in parallel inside a tier once the timing tests and the §10.2 retried failures are fixed, since concurrent QEMU under TCG makes both more likely. Push-to-green wall time is recorded in DESIGN §8.6 before and after; the target is under two minutes for x86_64 (3m40s at `88370e5` with no retry; a retried hang adds 60 to 90 s)
- [ ] `make debug`: QEMU `-s -S` plus a `gdb` script that loads the kernel ELF and the user ELFs, documented in DESIGN §8.4

### 10.2 Build and harness
- [x] built-in `x86_64-unknown-none` target; the custom JSON, `-Zbuild-std`, and `-Zjson-target-spec` deleted (B2)
- [x] one parametrized ISO recipe; a variant is one line (B1)
- [x] one QEMU launcher and one `VIBEOS_*` reader shared by every driver (T2, C2)
- [ ] one `target/` for every feature build: each variant's ELF is copied to a named output under `build/`, and every ISO recipe reads only its own named ELF, so a test build still cannot be packaged as production; DESIGN §8.2 and the pitfall that says "separate target directory" rewritten to that guard; `target/` is the only kernel target directory cached in CI (P1)
- [x] one initrd generator; the trampoline assembled by `global_asm!` so the kernel build no longer needs `nasm` (B4); the assembly user programs still do until §10.5
- [ ] the in-guest registry split per subsystem; a test's name printed before it runs; a per-test deadline (T1)
- [ ] `kernel_tests` hooks isolated in per-subsystem `ktest.rs` files; no blanket `allow(dead_code)` in production modules (Q2)
- [ ] `cargo-fuzz` targets for every byte-slice parser, the ELF header parser included, on the weekly job; each crash becomes a replayed regression (T4)
- [ ] a scheduled macOS CI job running `make check`; OVMF and the aarch64 edk2 firmware located by one probe mechanism with one variable per architecture, and a skip made visible (I1)
- [ ] the harness retries nothing. Each failure it retries today is root-caused and fixed, with a regression test in the cheapest tier that catches it: the `/bin/tests` stall after `user: dup ok` (sometimes after `user: pid 3 killed SIGSEGV`) that leaves the e2e boot short of `shell ready` and the in-guest boot hanging under the periodic LAPIC (retried since PR #75); the `ipi: ack timeout` panic on the `-smp 4` persist reboot; the `-smp 4` `msix_cpu: ap counter` assertion; and the `-smp 2` `per_cpu_bsp: ready_head should be empty` assertion. Then `_retry_hang`, `retryable_ktest_failure`, `silent_user_syscalls_hang`, and the retry loop in `_ktest_boot` are deleted. A failure that proves to be a QEMU bug keeps its retry only with an upstream report linked from DESIGN §8.6
- [ ] until those retries are gone, every retry the harness takes is written to the job summary with its failure line, so a green run that retried is visible

### 10.3 Portable core and the architecture seam
- [ ] `switch_context` and the DMA fences out of the portable half; hostlib as a workspace member; host tests on any OS (A2 landed the crate and host tests; `thread.rs` still carries `cfg(target_arch)` and `global_asm!`)
- [ ] an `arch` module boundary with one trait per concern: page table format and flags, context switch, interrupt controller and vector map, timer and cycle counter, atomics and barriers, MMIO accessors, per-CPU base register, syscall entry and user context, user-memory access, cache maintenance, the boot handshake
- [ ] every x86 assumption outside `arch/` found by grep for `asm!`, `x86`, and CR and MSR names, then moved or fenced; the audit checked in as `docs/ARCH.md`, which maps each seam trait to its implementing module
- [ ] the seam proven by a host build of the portable core against a stub `arch`, which is the cheapest second architecture there is
- [ ] one directory per subsystem, portable and hardware halves adjacent; DESIGN §1.3 rewritten to match the tree (A1)
- [ ] `fs/mod.rs`, `vibefs.rs`, `fat.rs`, and `ktest.rs` split by responsibility; a file-size guard in `make check` (Q5)
- [ ] the nine two-way module dependencies broken; `serial` has a raw layer with no upward calls (A4)
- [x] one `BootCell` and one `IrqCell`; no `static mut`; no `&'static mut` accessors (Q3; the asm-owned setjmp buffer in `arch/catch.rs` is the documented exception)
- [x] `BootInfo` captured once at entry, the only consumer of Limine responses (D3)
- [ ] DESIGN.md split per its own §1.4 rule: invariants and pitfalls as their own files, the boot order as a table not prose (DOC2); its `scripts/doc_refs.py` resolves both `DESIGN §x.y` and `ROADMAP §x.y` citations in docs, source comments, and scripts against the headings, and runs in `make check`

### 10.4 Tables, VFS, errors
- [ ] threads, processes, descriptors, inodes, dentries, files, mounts, and regions allocated at init from a `limits` module; a cap is a constant, not a type (D1)
- [ ] the cross-CPU wake inbox holds any `ThreadId` up to the `limits` thread count (a bitmap sized from the limits, or an MPSC list), and the KVA free-list node pool no longer caps freed thread stacks at 128 ranges; today's `u64` inbox caps ids at 64 and drops the rest silently (D1)
- [ ] `MAX_ELF` removed: the loader maps segments from the file instead of reading the whole binary into a bounded buffer
- [ ] FAT and vibefs behind `InodeOps`; `Vfs` owns inodes, dentries, mounts, and files and nothing backend-specific (A3)
- [ ] driver and volume state as instances referenced from the device registry, so a second disk is a second instance (D2)
- [ ] one `KError` with `From` for every module error and the Linux errno mapping in one table; syscall dispatch returns `Result<usize, KError>`; the table emits the errno table in `docs/SYSCALL.md` §2 through the §10.5 generator, and `make check` fails if the checked-in copy differs (E2)

### 10.5 User runtime
- [ ] a `no_std` Rust crate under `user/` as a workspace member, statically linked, with `_start`, argument and environment parsing, and a panic reported on fd 2 and turned into a non-zero exit
- [ ] built for the bare target as a non-PIE `ET_EXEC` with the small code model, through the static relocation model the `[target.x86_64-unknown-none]` rustflags already set (the built-in spec defaults to static-PIE, which the loader refuses until §14.2); the `std` port in Phase 17 introduces the `*-unknown-vibeos` triples
- [ ] syscall stubs generated from one table shared with the kernel, with a number column per architecture (§11.6) and an argument order where Linux's differs by architecture (raw `clone`: arm64 swaps `tls` and `ctid`), so numbers, arities, and argument positions cannot drift; the same generator emits `docs/SYSCALL.md` §3
- [ ] `brk` and anonymous `mmap`/`munmap`, eagerly backed for now; Phase 12 makes them lazy without changing the interface
- [ ] `getdents64`, `fstat`, and `nanosleep`, which `ls` and `sleep` below need; the rest of the floor stays in §13.9
- [ ] `reboot` (power off and restart), so the user `/bin/sh` keeps the `poweroff` and `reboot` the Phase 5 kernel shell had; x86_64 uses the ACPI and reset paths, and §11.4 puts PSCI behind the same call
- [ ] an allocator over `brk`, so `alloc` works in userspace
- [ ] `utest_ok` / `utest_fail` / `utest_skip` on serial, asserted by `tests/harness` like the `ktest_*` protocol; a failing user test fails `make test`
- [ ] `/sbin/init`, `/bin/sh`, `/bin/tests`, and `/hello` rewritten in the crate; the assembly sources and `mkuserelf.py` deleted
- [ ] `/bin/sh` runs programs from `PATH` with `fork`, `execve`, and `wait4`, reports their exit status, and has `poweroff` and `reboot`; pipes and job control are §13.7
- [ ] `ls`, `cat`, `echo`, `grep`, `wc`, `true`, `false`, `sleep`, `yes`, and `cmp`, each a few dozen lines, because the Phase 13 gate is a pipeline of them
- [ ] the initrd is sized by `mkinitrd` from its contents plus fixed free space for the write tests, and loaded as a Limine module on both architectures instead of a 64 KiB image embedded by `build.rs`; `INITRD_BYTES` and its size check are deleted, and DESIGN's boot order and `build.rs` pitfall are updated to match
- [ ] nothing in the crate names an architecture outside one `arch` module, so Phase 11 builds it for aarch64 by adding a directory

### 10.6 Entry paths and user memory
The kernel's entry paths and its user-memory copies predate copy-on-write, demand paging, and signal
return. Each item here is a live bug or a trap for Phase 12 and 13.

- [ ] user-VA accessors replace the HHDM copy for syscalls. `copy_from_user` and `copy_to_user` keep the range check (canonical, inside the user half, no overflow), because inside `stac`/`clac` the MMU still allows supervisor access to kernel pages. They then dereference the user address inside `stac`/`clac`, so `CR0.WP` faults a write to a read-only page and a not-present page faults. A fault inside an accessor becomes `EFAULT` through an exception-table fixup, which replaces the page-table pre-walk. Today `write_bytes` writes through the physmap and ignores `WRITABLE`, which Phase 12's COW would turn into cross-process corruption. DESIGN §5.1 and SYSCALL.md §5 describe it as built
- [ ] one named fill API for writing an address space that is not running: ELF segments, zero fill, TLS, the exec stack, and `fork`'s copy until §12.3. It writes frames through the physmap and syscall code cannot reach it. §12.2 teaches it to fault pages in with write intent
- [ ] every interrupt and exception entry clears `RFLAGS.AC` (`clac` when SMAP is live) before anything else. Interrupt gates do not clear AC, ring 3 can set it with `popf`, and an IRQ or `#PF` inside an accessor would otherwise run its handler with SMAP off; `iretq` restores the interrupted value. An in-guest test opens the accessor window, takes an exception, and has a test hook in the handler make a stray access to a user page, which must fault; an x86_64-only variant does the same after a user program sets AC with `popf`
- [ ] the top user page is never mappable: one `USER_MAP_END` (`0x0000_7FFF_FFFF_F000`) used by the ELF loader and the address-space range checks, so a `syscall` in the last page cannot return to a non-canonical RIP. The exit path sends a non-canonical saved RIP to `SIGSEGV` before `swapgs`, never to `iretq`. A `#GP` on any `iretq` to a user frame is recognized, handled on the kernel GS, and turned into `SIGSEGV` for the process instead of `exception_halt`. DESIGN §4.1 and SYSCALL.md §1 updated in the same commit; the in-guest test runs on the KVM leg too, since TCG may not model `iretq`'s canonical check
- [ ] the IST vectors (NMI, `#MC`, `#DB`, `#DF`) decide whether to `swapgs` from the sign of `GS_BASE`, not from CS.RPL. The syscall entry before its `swapgs` and the exit's `swapgs; sysretq` and `swapgs; iretq` run at CPL 0 with the user GS base loaded. An in-guest test puts hardware execute breakpoints on those instructions, and the `#DB` handler finds its `PerCpu`. The sign check holds until §18.3 lets userspace write a kernel-half GS base
- [ ] the 512 MiB low identity window torn down after `smp: done`, so VA 0 faults in kernel mode as it does in user mode. Only the `0x8000` trampoline page stays: 4 KiB, read-only, executable, not global, with the trampoline GDT's accessed bits preset so the AP never writes it. The BSP writes the blob and parameter block through the physmap. The teardown flushes global TLB entries on every CPU. An in-guest test finds that a kernel read of VA 0 faults. DESIGN §4.1 and §4.3 updated in the same commit

## Phase 11: Portability

**Goal.** A second architecture, first class. aarch64 boots the same kernel to the same Phase 9 gate,
under the same harness, with the same markers. x86_64 and aarch64 are peers from here on: every later
phase lands on both, or says which lines are single-architecture and why.

**Unlocks.** Native-speed local runs on Apple Silicon, which is where the kernel is booted by hand.
Proof that the seam from §10.3 is real. Every phase after this one designed for two architectures at
design time rather than retrofitted, which matters most for page faults, signal frames, TLS, and the
syscall ABI.

Why here rather than at the end: Phase 12 (the page fault path, including faults inside the §10.6
accessors) and Phase 13 (signal frames, `sigreturn`, the TLS register) are where the kernel's
architecture-specific surface grows most. Doing them with two ports in the tree forces the seam to be
right where it is designed rather than where it is discovered. The cost is that Phase 12 waits for this
phase. x86_64 came first and stays the reference when the two disagree.

**Exit gate**
- [ ] `make ARCH=aarch64` produces a bootable image; `make ARCH=aarch64 run` boots under `qemu-system-aarch64 -machine virt,acpi=off` (§11.5), with `-accel hvf` on macOS and `-accel kvm` on an arm64 Linux host
- [ ] every marker in the shared and aarch64 lists of the [DESIGN.md](DESIGN.md#83-end-to-end) contract, from `serial online` through `shell ready`, appears in order on aarch64, from one harness with an `ARCH` parameter
- [ ] `make test-kernel ARCH=aarch64` passes every in-guest test that is not x86-specific, including the Phase 6 MSI-X tests and the §10.6 user-memory tests, and the harness reads the verdict from the exit status under TCG, HVF, and KVM (§11.7); each x86-specific test is `ktest_skip`ped with a reason naming the x86 feature it needs, and the harness fails if the skipped set differs from the list in `docs/ARCH.md`
- [ ] `/bin/tests` from the Phase 10 user crate passes on aarch64 at EL0, against the arm64 syscall numbers
- [ ] the harness types a command into the shell on aarch64 through the QEMU monitor's `sendkey` and virtio-keyboard, and reads the echo on serial, as the x86 e2e boot does through PS/2
- [ ] `-smp 4` under `virt` brings up every core through PSCI, and the TLB shootdown test passes
- [ ] a scheduled run boots aarch64 under TCG with `-machine virt,acpi=off,virtualization=on -cpu max` to `shell ready`, and the exception-level marker reports EL2
- [ ] CI runs the aarch64 build and tier jobs on every push beside the x86_64 ones (§10.1 budget), under TCG, since GitHub's arm64 runners have no `/dev/kvm`
- [ ] `docs/ARCH.md` maps each §10.3 seam trait to its module in each port and lists every other architecture-specific module; `scripts/check_arch.py` in `make check` fails when a file under either port's `arch/` directory is missing from it, or a module it names does not exist
- [ ] README describes both architectures, with the aarch64 quickstart on Apple Silicon
- [ ] no x86 regression: the full x86 ladder stays green on every commit of this phase
- [ ] tag `v0.11.0`

### 11.1 Boot
- [ ] Limine on aarch64 over UEFI, so the boot protocol, memory map, HHDM, and framebuffer handshake are shared with x86 rather than reimplemented
- [ ] Limine 12.9 or later, pinned by commit in `setup.sh` and the CI cache keys (12.6 deletes the device-tree `memory@` nodes that contradicted the memory map; 12.9 enters at EL1 on CPUs without VHE). aarch64 requests base revision 6, the only aarch64 revision Limine 11 and later accept; x86_64 may stay at revision 3, with `BootInfo` normalizing the difference. The full x86 ladder stays green across the bump
- [ ] the kernel builds for `aarch64-unknown-none-softfloat`, the counterpart of the soft-float x86 target, so kernel code never touches FP or SIMD registers and lazy user FP saving stays sound (Limine enters with `CPACR_EL1` zero, so the first SIMD instruction would trap anyway); user code builds for `aarch64-unknown-none`; `rust-toolchain.toml` and the CI `targets:` inputs gain both
- [ ] exception level: Limine chooses it, and a registered marker records whether entry was at EL1 or at EL2 with VHE (`HCR_EL2.{E2H,TGE}` set). The kernel never changes exception level itself. HVF, KVM without nested virtualization, and TCG without `virtualization=on` enter at EL1. At EL2 the same kernel runs as a VHE host (§11.3, §11.4). Only an EL2 entry keeps the Phase 21 hypervisor possible on that machine; at EL1 the VM layer reports that EL2 is unavailable
- [ ] MMU enable: 4 KiB granule, 48-bit VA, TTBR0 for user and TTBR1 for kernel, which maps onto the existing address map split
- [ ] MAIR and the memory attribute policy: Normal write-back for RAM, Device-nGnRE for MMIO. aarch64 has no MTRRs to override a cacheable alias, so the aarch64 physmap maps only the RAM entries of the Limine memory map and never the holes where the GIC, PL011, virtio-mmio, and PCIe windows sit; it splits to 4 KiB where a 2 MiB block would reach into one. MMIO is reached only through `ioremap` as Device, and a framebuffer outside RAM as Normal non-cacheable. There is no equivalent of the in-place x86 UC patch
- [ ] the `_start` order table in DESIGN §3.3 gains an aarch64 column
- [ ] early serial on PL011; the same `serial online` first line

### 11.2 Memory
- [ ] the page table format behind the §10.3 trait: descriptor bits, access permissions, `UXN` and `PXN`, the access flag, shareability
- [ ] TLB maintenance with the Inner Shareable broadcast forms, so the §1.2 shootdown hook needs no IPI: `dsb ishst` after the PTE write, then `tlbi vale1is` for a leaf change (`vae1is` when a table page is freed, `aside1is` for one ASID, `vmalle1is` on ASID rollover), then `dsb ish`, and `isb` on the issuing core; the §4.10 KVA-free deferral is satisfied once the `dsb ish` completes. DESIGN §7.9 gains an aarch64 paragraph
- [ ] break-before-make for any change to a live descriptor's memory type, cacheability, shareability, output address, or block size: write an invalid entry, `dsb ishst`, `tlbi ...is`, `dsb ish`, write the new entry, `dsb ishst`, `isb`; permission-only changes need only the TLB maintenance above
- [ ] ASIDs, so a context switch does not flush the TLB
- [ ] cache maintenance for DMA, and for code: whenever a user page becomes executable, `dc cvau` over it (omitted when `CTR_EL0.IDC` is set), `dsb ish`, `ic ivau` (omitted when `CTR_EL0.DIC` is set), `dsb ish`, `isb`, which x86 got for free; ELF load is the first caller, and §12.2 adds the fault path
- [ ] the buddy, heap, and KVA allocators unchanged; that is the point of the split

### 11.3 Interrupts and time
- [ ] exception vectors: the sixteen-entry table, synchronous versus IRQ versus FIQ versus SError, and `ESR_EL1` decoded into the same fault kinds the x86 handlers produce
- [ ] GICv2 and GICv3: distributor, redistributors, CPU interface, priorities
- [ ] MSI through the GICv3 ITS (GICv2m on GICv2), so virtio-pci and the Phase 6 MSI-X tests run unchanged; INTx through the device tree's `interrupt-map` as the fallback
- [ ] the generic timer: `CNTVCT` as the cycle counter, `CNTV_TVAL` for the tick, the same `next_deadline` interface; the tick's interrupt comes from the device-tree timer node for the entry level: the EL1 virtual timer at EL1, the EL2 virtual timer at EL2 with VHE, where `CNTV_*` accesses are redirected
- [ ] a monotonic clock on `CNTVCT` with the same seqlock publication and the same host tests
- [ ] SGIs as the IPI mechanism: reschedule, call-function, panic halt; no shootdown SGI, because §11.2's broadcast TLB maintenance replaces it

### 11.4 SMP and per-CPU
- [ ] PSCI `CPU_ON` for secondary cores, through the conduit the device tree's `/psci` `method` names (`hvc` at EL1, `smc` at EL2). The core enters at a physical address with the MMU and D-cache off, so before the call the boot CPU cleans to the Point of Coherency (`dc cvac`, `dsb sy`) everything the entry stub reads with its MMU off. The stub enables the MMU through a temporary identity map in TTBR0, jumps to the TTBR1 kernel address, then points TTBR0 at the empty user root and invalidates the local TLB, so no identity entry survives. The online mask and the §4.5 rendezvous barrier are shared with x86. The kernel brings cores up itself rather than through Limine's MP feature, since §19.6 offlining needs it
- [ ] the per-CPU base, chosen once at boot: `TPIDR_EL1` at EL1, `TPIDR_EL2` at EL2, so that Phase 21 guests own `TPIDR_EL1`; the `per_cpu!` accessors unchanged above the seam
- [ ] per-CPU GIC redistributor and timer setup on each core
- [ ] the bring-up failure path: a core that never arrives frees what it was given, as on x86
- [ ] PSCI `SYSTEM_OFF` and `SYSTEM_RESET` behind the §10.5 `reboot` syscall, through the same conduit

### 11.5 Devices
- [ ] the device tree from Limine's DTB response, parsed in the portable half and host-tested like the ACPI parser: CPUs, GIC and ITS, timer, PL011, PL031, PCIe ECAM with its `interrupt-map`, virtio-mmio, `/psci`. RAM comes from the Limine memory map, never from `memory@` nodes, which Limine removes. `/chosen` is ignored as the protocol requires, so the console is the PL011 that `/aliases` names `serial0`
- [ ] PCIe through the ECAM the device tree names, so virtio-pci, MSI-X, and the block driver are shared with x86
- [ ] virtio-mmio transport as well, since single-board hardware uses it
- [ ] virtio-input for keyboard and pointer, since `virt` has no PS/2 controller; x86 can use it too, and §16.4 builds on it
- [ ] PL031 for the §2.7 wall clock; `RNDR` where the CPU has it, then virtio-rng, behind `/dev/random`
- [ ] the framebuffer console over the Limine framebuffer, which on `virt` needs `-device ramfb`: edk2's virtio-gpu-pci driver is Blt-only and leaves no linear framebuffer after `ExitBootServices`; virtio-gpu gets a native driver in §16.2

Decided: a device tree, not ACPI, on aarch64 until §20.7. edk2 gives the OS either ACPI or a device tree,
and picks ACPI when QEMU generates tables, so the aarch64 command line passes `-machine virt,acpi=off`. A
device tree describes virtio-mmio and INTx routing without an AML interpreter, and it is what boards ship.
ACPI on aarch64 comes with the server-class machines in §20.7, reusing the §2.4 parser and the §20.2
interpreter.

### 11.6 User mode
- [ ] `svc` entry and `eret` exit, the user context saved in the same shape the x86 path produces
- [ ] `TTBR0` switch on context switch, skipped when the address space is shared
- [ ] PAN enabled, with `SCTLR_EL1.SPAN` clear so every exception entry to EL1 sets PAN again, and cleared only inside the §10.6 accessors (SMAP's equivalent); PXN on every user page (SMEP's); the §10.6 in-guest tests pass unchanged, except the x86-only `popf` variant, which has no counterpart because EL0 cannot write PAN
- [ ] `TPIDR_EL0` for user TLS; the ELF loader's TLS layout handles variant I on aarch64 and variant II on x86_64
- [ ] the aarch64 syscall convention documented in `docs/SYSCALL.md`: `svc #0`, number in `x8`, arguments in `x0` to `x5`, result in `x0`, errnos as on x86_64. The numbers are the asm-generic table Linux uses on arm64, which musl's aarch64 port and static Linux binaries need. Numbers are necessary but not sufficient: the per-architecture argument order (§10.5) and struct layouts (below, §13.6, §13.8) do the rest
- [ ] `openat` (with `AT_FDCWD`), `dup3`, and `clone` with fork semantics, because asm-generic has no `open`, `dup2`, or `fork`; the user runtime uses these on both architectures, and the legacy calls stay x86_64-only entry points onto the same paths
- [ ] `struct stat` for `fstat` in each architecture's Linux layout: x86_64's 144-byte layout with a 64-bit `st_nlink` before `st_mode`, and the 128-byte asm-generic layout on aarch64; a host test pins sizes and field offsets to values copied from the Linux uapi headers, because the dev host may be a Mac
- [ ] `/sbin/init`, `/bin/sh`, `/bin/tests` from the user crate, built for `aarch64-unknown-none`
- [ ] user FP and SIMD state saved lazily on first use, since every aarch64 user compiler emits NEON; the kernel itself never touches those registers (§11.1)

### 11.7 Build, harness, CI
- [ ] `ARCH=` in the Makefile and `--arch` in the harness; one code path, two QEMU command lines; the aarch64 line carries `-machine virt,acpi=off`, `-device ramfb`, `virtio-keyboard-pci`, `virtio-tablet-pci`, and `pvpanic-pci`
- [ ] the DESIGN §8.3 marker contract split into a shared list and one list per architecture; x86-only markers (`gdt ok`, `pic: remapped`, `time: tsc`, `lapic_timer`) get aarch64 counterparts or are listed as x86-only, and the harness picks lists by `--arch`
- [ ] a test verdict leaves the guest as an exit status that works under TCG, HVF, and KVM: pass is PSCI `SYSTEM_OFF` (QEMU exits 0), and fail triggers `pvpanic-pci` with `-action panic=exit-failure`; the harness also requires the serial `begin` and `end` lines, as §1.6 does. Semihosting is not used, because QEMU supports it only under TCG
- [ ] the panic path, backtrace, and symbol table working on aarch64: a frame-pointer walk along the `x29` chain
- [ ] `make test-kernel ARCH=aarch64` and the e2e ladder, including the `sendkey` echo through virtio-keyboard, in the aarch64 CI tier jobs
- [ ] the aarch64 CI jobs run under TCG: GitHub-hosted arm64 runners have no `/dev/kvm` (the request was closed as not planned); native aarch64 runs are HVF on the dev host until §20.8
- [ ] the timing tests pass under HVF on the dev host, the same fix as the §10.1 KVM leg
- [ ] the weekly smp-stress job on both architectures

### 11.8 Stretch: riscv64
- [ ] mostly a test of whether the §10.3 seam was real: SBI, PLIC and CLINT, Sv39 and Sv48 paging, QEMU `virt`
- [ ] a third port that costs a week says the seam is right; one that costs a month says where it is wrong

---

## Phase 12: Fault-driven Memory

**Goal.** Stop pretending memory is eagerly mapped. Make `fork` cheap, `mmap` real, and a page fault a
routine event on both architectures. Slab, background reclaim, and swap used to live here. Slab and
background reclaim are §19.9 and §19.10, because their gates are measurements. Swap is a stretch at the
end of this phase, because nothing before Phase 19 needs it. What stays is reclaim on demand, which the
unified page cache cannot do without.

**Unlocks.** Real program startup costs. Large sparse allocations. File-backed memory, which the dynamic
linker in §14.2 needs. A `fork` cheap enough that Phase 13's pipelines and Phase 14's shell are not
dominated by it.

**Architectures.** Both. The fault decoding differs (the error code on x86_64, `ESR_EL1` on aarch64), and
so do TLB maintenance and the §11.2 I-cache maintenance aarch64 needs whenever a fault, a COW copy, or
`mprotect` installs an executable PTE. Everything from one fault kind upward is shared.

**Exit gate**
- [ ] `fork` of a process with 100 MB resident completes in under 10 ms, measured by the §12.3 ktest in a 512 MiB, 2-CPU guest under KVM on both architectures
- [ ] page faults are counted per fault kind (demand-zero, COW, file, stack) and shown by `meminfo`; an in-guest test in which one process first-writes N untouched pages of a fresh anonymous mapping sees its demand-zero count rise by exactly N, on both architectures
- [ ] `brk` and anonymous `mmap` regions from §10.5 are demand-faulted: touching one page maps one page, verified by the frame count
- [ ] `read()` into a COW page of a forked child leaves the parent's page unchanged; `read()` into an untouched `brk` page faults it in rather than returning `EFAULT`
- [ ] the same fault tests pass on x86_64 and aarch64, with the error code and `ESR_EL1` decoded into one fault kind
- [ ] `mmap` of a file, modify, `msync`, and the change is on disk
- [ ] the OOM path kills a chosen process with a logged reason rather than panicking or hanging
- [ ] no frame leaks: after 1000 fork, exec, `mmap`, `munmap`, and exit cycles and the §12.6 exhaustion test, the §12.1 user-anonymous and page-table counts return to baseline, and free plus page-cache frames return to baseline, on both architectures
- [ ] tag `v0.12.0`

### 12.1 Frame metadata
- [ ] a `Frame` array indexed by physical frame number, allocated at boot from a known-size region
- [ ] per-frame: refcount, flags, owner, and a list link for LRU; §19.8 adds a pin count
- [ ] `get_frame` / `put_frame` with the last reference freeing to the buddy allocator
- [ ] reverse mapping from a frame to the PTEs referencing it, which reclaiming a mapped page-cache page requires (§12.6) and swap reuses
- [ ] accounting by category: kernel heap, page tables, user anonymous, page cache, free; §19.9 adds slab

### 12.2 Demand paging
- [ ] a real page-fault handler decoding the fault: present, write, user, reserved, instruction fetch (the x86 error code, `ESR_EL1` on aarch64)
- [ ] region lookup for the faulting address, then a per-region fault handler
- [ ] anonymous regions faulting in a zero page; a shared read-only zero page until first write
- [ ] file-backed regions faulting in from the page cache
- [ ] ELF `PT_LOAD` segments become file-backed private regions, so the §10.6 fill API no longer copies segment bytes or zero-fills whole bss pages at exec
- [ ] stack regions growing down on fault, up to a limit
- [ ] a fault that resolves to no region is `SIGSEGV` for user, panic for kernel
- [ ] a fault inside a §10.6 accessor runs the region fault handler before the exception-table fixup, so a copy into an untouched page demand-faults, and a copy into a COW page breaks COW, as a user store would; only an unresolvable fault becomes `EFAULT`
- [ ] the §10.6 fill API resolves each page through the region fault handler with write intent before writing, so exec's stack, TLS, and bss-tail writes never land in a page-cache or COW-shared frame
- [ ] installing an executable user PTE (a file-backed or anonymous fault, a §12.3 COW copy, `mprotect` adding `PROT_EXEC` in §12.4) runs the §11.2 I-cache maintenance first on aarch64, and nothing on x86_64
- [ ] per-process and global fault counters by fault kind, shown by `meminfo`
- [ ] the fault path must be reentrant-safe: it can block on I/O, so it cannot hold the page table lock across a read

### 12.3 Copy on write
- [ ] `fork` marks every writable private mapping read-only in both address spaces and increments frame refcounts; it no longer calls the §10.6 fill API
- [ ] a write fault on a COW frame with refcount 1 just makes it writable again; with refcount above 1 it copies; §19.8 refines this for pinned pages
- [ ] shared mappings excluded correctly, since getting this wrong silently breaks shared memory
- [ ] a TLB shootdown on the write-protect step, which is the expensive part and worth measuring
- [ ] in-guest: fork, write in the child, assert the parent's memory is unchanged; the same with the child's write done by a `read()` syscall into the page; assert the frame count matches expectations at each step
- [ ] a ktest that times `fork` of a process with 100 MB resident against the kernel's monotonic clock and fails at 10 ms or more; it skips with a reason unless the guest has at least 512 MiB under KVM or HVF, so the per-push TCG tiers skip it, and the §10.1 KVM leg runs it on x86_64; the aarch64 number is taken under HVF on the dev host and recorded in DESIGN §8.6

### 12.4 mmap
- [ ] `mmap`, `munmap`, `mprotect`, `mremap`, `msync`, `madvise`; `brk` and anonymous `mmap` from §10.5 become lazy behind the same interface
- [ ] anonymous private, anonymous shared, file private, file shared
- [ ] `MAP_FIXED` handling, including replacing existing mappings
- [ ] region splitting and merging on partial unmap and protect
- [ ] a VA space allocator for the user half with a bottom-up hint and a gap search, below the §10.6 `USER_MAP_END`
- [ ] dirty page writeback for shared file mappings
- [ ] `mlock` for pages that must not be evicted

### 12.5 Unified page cache
- [ ] one cache serving file reads, `mmap`, and the block layer, rather than a block cache and a page cache disagreeing
- [ ] radix tree or B-tree per inode mapping offset to frame
- [ ] writeback threads with a dirty limit and per-inode ordering
- [ ] one LRU with second-chance aging, which §12.6 reclaims from; §19.10 splits it into an active and an inactive list
- [ ] readahead driven by detected access patterns
- [ ] `tmpfs` pages participating, replacing the §8.4 version that sits on the block cache plus a fixed ramdisk, so a full `tmpfs` is reclaimable rather than pinned

### 12.6 Reclaim and OOM
- [ ] direct reclaim when an allocation fails: drop clean page-cache pages, write dirty ones back and then drop them, and unmap mapped pages through §12.1's reverse map; §19.10 adds watermarks and a background thread
- [ ] a reserve pool for allocations that must succeed to make progress, since reclaim itself needs memory
- [ ] an OOM killer scoring by resident size, logging the score table before killing; §19.10 adds nice and a per-process adjustment
- [ ] a kernel allocation failure that cannot be resolved panics with the full memory state, rather than returning an error nobody checks
- [ ] in-guest: allocate to exhaustion and assert the system survives, with the expected process killed

### 12.7 Stretch: swap
Nothing before Phase 19 needs swap; a self-hosting build is given RAM, not swap. Not gating.

- [ ] allocate well past physical memory with swap enabled and the workload completes
- [ ] a swap device or file with a slot allocator
- [ ] page-out: pick a victim via reclaim, write it, replace the PTE with a swap entry, free the frame
- [ ] page-in on fault from the swap entry
- [ ] reverse mapping used to find every PTE referencing a shared frame being swapped
- [ ] readahead on swap-in, since thrashing one page at a time is unusable
- [ ] a swap cache to avoid duplicate I/O for a shared page
- [ ] `swapon` / `swapoff`, with `swapoff` faulting everything back in

---

## Phase 13: Threads, IPC, Signals, and the POSIX Surface

**Goal.** Processes with more than one thread, that talk to each other, respond to events, and present
enough of a POSIX surface that real software can be ported without patching every call site.

**Unlocks.** Shell pipelines. Job control. `pthreads`. Anything ported from Unix.

**Architectures.** Both. Signal frames, `sigreturn`, the TLS register, `clone`'s argument order, and the
user-visible structs Linux defines per architecture (`stat`, `epoll_event`, `sigaction`, `ucontext`) are
per architecture, each pinned by a size-and-offset host test; the rest is shared. Every call lands under
its asm-generic name, which both architectures have; the legacy x86_64 names are entry points onto it.

**Exit gate**
- [ ] `ls | grep foo | wc -l`, typed into the §13.7 `/bin/sh` with the §10.5 utilities, works with correct exit statuses and no deadlock on a full pipe
- [ ] ctrl+C kills the foreground job and leaves the shell alive; ctrl+Z stops it and `fg` resumes it
- [ ] a user signal handler runs on a proper user stack and returns correctly through `sigreturn`, and a crafted frame cannot leave user mode or crash the kernel (§13.8)
- [ ] `ppoll` on 100 descriptors wakes only for the ready ones, verified by a syscall counter
- [ ] a futex-based userspace mutex under contention across processes, correct and without spinning
- [ ] a Unix domain socket carries a passed file descriptor between processes
- [ ] sixteen user threads in one process contend a futex mutex from the user crate: the count is right, `exit_group` tears all of them down while some are blocked in `read`, and the frame count returns to baseline
- [ ] `ps` and `/proc/<pid>/*` come from the process table; syscall 500 is gone
- [ ] tag `v0.13.0`

### 13.1 Threads
- [ ] `clone` with `CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SETTLS | CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID`; a plain fork is `clone(SIGCHLD)` on both architectures (§11.6), and x86_64 keeps the `fork` number as an entry point onto it. `CLONE_SYSVSEM` is accepted as a no-op and `CLONE_DETACHED` ignored, since musl's `pthread_create` passes both; `CLONE_VM | CLONE_VFORK` gets vfork semantics, since musl's `posix_spawn`, `system`, and `popen` use it
- [ ] `exit` ends a thread and `exit_group` ends the process; the last thread out releases the address space
- [ ] TLS through `clone`'s `tls` argument (the fifth argument on x86_64, the fourth on aarch64): `FS_BASE` on x86_64 and `TPIDR_EL0` on aarch64, plus `arch_prctl(ARCH_SET_FS)` for musl on x86_64
- [ ] `set_tid_address` and the clear-child-tid wake, which is what `pthread_join` stands on
- [ ] a per-thread kernel stack, signal mask, and pending set; process-directed signals delivered to one eligible thread
- [ ] `gettid`, `tgkill`
- [ ] thread-group exit while other threads are in a blocking syscall, which is the case everyone gets wrong
- [ ] the user crate gains `thread::spawn`, `join`, and a futex-backed mutex, which the gate's counter test uses
- [ ] in-guest: the counter test from the gate; create and join a thousand threads, no more than 64 alive at once, within the Phase 10 thread limit; `exit_group` from a non-main thread

### 13.2 Pipes
- [ ] a bounded ring buffer with blocking read and write and correct partial-write semantics
- [ ] read end closed produces `SIGPIPE` and `EPIPE`; write end closed produces EOF
- [ ] `pipe2` with `O_CLOEXEC` and `O_NONBLOCK`, and `pipe` as its x86_64 entry point
- [ ] named pipes through the VFS, created with §13.9's `mknodat`
- [ ] `PIPE_BUF` atomicity guarantee actually honored
- [ ] in-guest: fill the pipe, assert the writer blocks, drain, assert it proceeds

### 13.3 Unix domain sockets
- [ ] `socket`, `socketpair`, `bind`, `listen`, `accept`, `accept4`, `connect`, `sendto`, `recvfrom`, `sendmsg`, `recvmsg`, `shutdown`, `getsockopt`, `setsockopt` for `AF_UNIX`; `send` and `recv` are runtime wrappers over `sendto` and `recvfrom`, since neither syscall table has them
- [ ] stream, datagram, and seqpacket types
- [ ] filesystem-bound and abstract namespaces
- [ ] `SCM_RIGHTS` file descriptor passing and `SCM_CREDENTIALS` as `sendmsg`/`recvmsg` control messages, with `setsockopt(SO_PASSCRED)` on the receiving socket
- [ ] the same buffering machinery reused for network sockets in phase 15, decided now rather than duplicated later

### 13.4 Shared memory
- [ ] POSIX `shm_open` backed by `tmpfs`, sized with §13.9's `ftruncate`
- [ ] anonymous shared mappings inherited across `fork`
- [ ] `memfd_create` for anonymous named regions
- [ ] correct refcounting so a region survives until the last mapper unmaps

### 13.5 futex
- [ ] `FUTEX_WAIT` and `FUTEX_WAKE` on a user address, with a hash table of wait queues
- [ ] private futexes keyed by (address space, virtual address); shared futexes by (backing object, offset), which is the inode for `shm_open` and `memfd_create`. Never by physical frame: COW, page migration, and swap all move a frame under a sleeping waiter, and after `fork` unrelated processes share frames
- [ ] requeue and `WAKE_OP` for condition variables
- [ ] the wait queue can record a lock owner, so §19.4 adds priority inheritance without changing this interface
- [ ] a timeout on every wait
- [ ] in-guest: a forked parent and child wait and wake on the same private address without seeing each other's wakes; two processes mapping one `shm_open` object do see them; a waiter survives a COW break on its page
- [ ] this is the primitive userspace threading stands on; correctness matters more than speed here

### 13.6 Event notification
- [ ] `ppoll`, and `poll` as its x86_64 entry point
- [ ] `pselect6`, and `select` as its x86_64 entry point, for compatibility
- [ ] `epoll` (`epoll_create1`, `epoll_ctl`, `epoll_pwait`) with edge and level triggering, since `poll` is O(n) per call and that becomes the bottleneck; `struct epoll_event` is packed on x86_64 only (12 bytes, 16 on aarch64), and a host test pins both
- [ ] `eventfd2`, `signalfd4`, `timerfd_create` and its `settime`/`gettime`
- [ ] one internal readiness and wait-queue mechanism underneath all of them
- [ ] in-guest: 200 descriptors, within the Phase 10 per-process limit, with a handful ready, asserting the wakeup count

### 13.7 TTY and job control
- [ ] a TTY layer between the console and processes, with a line discipline
- [ ] one TTY per console backend, each with its own input queue: `/dev/ttyS0` over the serial port and `/dev/tty1` over the framebuffer with the PS/2 or virtio-input keyboard. §5.3's merged input stops reaching userspace. `/dev/console` resolves to one of them for init's stdio, serial by default, and `/dev/tty` becomes the controlling-terminal alias. Kernel log lines and markers keep fanning out to every backend
- [ ] the e2e console-input check (DESIGN §8.3) follows the split: the `sendkey` line goes to the shell on `/dev/tty1`, and its reply is made visible on serial; DESIGN §8.3 updated in the same commit
- [ ] in-guest: bytes injected on serial RX reach only `/dev/ttyS0`'s reader, and a keypress reaches only `/dev/tty1`'s
- [ ] canonical mode: line buffering, erase, kill, EOF
- [ ] raw mode, and the `termios` interface to switch between them
- [ ] control character handling generating signals: ctrl+C, ctrl+Z, ctrl+\
- [ ] sessions, process groups, controlling terminal, foreground group: `setsid`, `setpgid`, `getpgid`, `getsid`, `TIOCSPGRP` and `TIOCGPGRP`
- [ ] `wait4` with `WUNTRACED` and `WCONTINUED`, and `waitid`
- [ ] `SIGTTIN` and `SIGTTOU` for background access
- [ ] pseudo-terminals (`/dev/ptmx` and `/dev/pts`), which the terminal emulator in phase 16 and `sshd` in phase 15 require
- [ ] window size and `SIGWINCH`
- [ ] the §10.5 `/bin/sh` gains `|`, `<`, `>`, `&`, one process group per job, terminal handoff, `fg`, `bg`, and `jobs`, which this phase's gate uses; §14.5's POSIX shell replaces it

### 13.8 Full signals
- [ ] the full signal set with correct default actions
- [ ] `rt_sigaction` with `SA_RESTART`, `SA_SIGINFO`, and an alternate stack (`sigaltstack`); the kernel `struct sigaction` per architecture as Linux defines it
- [ ] `rt_sigaction` honors `SA_RESTORER`. On x86_64 it is the only return path: a handler installed without it is not run, and the process gets `SIGSEGV`, as on Linux; nothing is ever written to the NX stack as code. On aarch64, a handler without a restorer returns through a read-only, user-executable trampoline page the kernel maps into every process at `exec`, so glibc-static binaries work. The user crate gains `sigaction`, `sigprocmask`, and a restorer stub per architecture, which the gate's handler test uses
- [ ] `rt_sigprocmask`, `rt_sigpending`, `rt_sigsuspend`, `rt_sigtimedwait`
- [ ] delivery on the return to userspace: build a Linux `rt_sigframe` on the user stack (`siginfo`, `ucontext` with `uc_mcontext`, and the FP state), run the handler, return through `rt_sigreturn`; the `ucontext` layout per architecture is pinned by a host test, since musl reads it
- [ ] x86_64 frame: on the current stack it begins at least 128 bytes below the interrupted RSP (the red zone), on the alternate stack at that stack's top, and RSP+8 is 16-byte aligned at handler entry. The frame carries the FPU image, with lazily held live state saved first. aarch64 frame: the FPSIMD record, SP 16-byte aligned
- [ ] signal delivery (the `sa_handler` and `sa_restorer` addresses) and `rt_sigreturn` (the whole restored frame) validate the user context before the return to userspace. On x86_64: RIP canonical and below `USER_MAP_END`, CS and SS forced to the user selectors, RFLAGS masked to user-settable bits (never IOPL, NT, VM, or RF from the frame), and MXCSR masked to `MXCSR_MASK` and `XSTATE_BV` checked against XCR0 before the image reaches `fxrstor`/`xrstor`. On aarch64: SPSR forced to EL0t, with only NZCV and the other user-settable bits taken from the frame
- [ ] in-guest, per architecture: `rt_sigreturn` with a non-canonical PC, kernel selectors or EL1 mode, IOPL set, or reserved MXCSR bits gets `SIGSEGV` and the kernel stays up; a handler that interrupts a leaf function holding live data in its red zone leaves that data intact
- [ ] real-time signals with queueing, since standard signals coalesce and that surprises people
- [ ] interaction with blocking syscalls: interrupt with `EINTR` or restart, per `SA_RESTART`
- [ ] per-thread signal masks with process-directed signals delivered to an eligible thread

### 13.9 POSIX floor
- [ ] a tracked list of the syscalls needed to build and run the target software set, checked off as implemented
- [ ] every call in this phase lands under its asm-generic name, which x86_64 also has; the legacy names are x86_64-only entry points onto it, as §11.6 does for `openat`, `dup3`, and `clone`: `faccessat` for `access`, `fchmodat` for `chmod`, `fchownat` for `chown`, `newfstatat` for `stat` and `lstat`, `readlinkat` for `readlink`, and the §13.2 and §13.6 pairs
- [ ] `getcwd`, `chdir`, `fchdir`, `faccessat`, `fchmodat`, `fchownat`, `umask`, `utimensat`
- [ ] `newfstatat`, `readlinkat`, `openat` with any `dirfd`, `pread64`, `pwrite64`, `readv`, `writev`, which every coreutil calls before anything else; `getdents64`, `fstat`, and `nanosleep` came with §10.5
- [ ] `mkdirat`, `unlinkat`, `renameat`, `renameat2`, `linkat`, `symlinkat`, `mknodat`, `truncate`, `ftruncate`, `fsync`, `fdatasync`, `sync`, `statfs`, `fstatfs`, `mount`, `umount2`, `sethostname`, `settimeofday`, and `clock_settime`, with the x86_64 legacy names (`mkdir`, `rmdir`, `unlink`, `rename`, `link`, `symlink`, `mknod`) as entry points onto them; §13.2's named pipes, §13.4's `shm_open`, §14.3's init, §14.4's coreutils, §14.6's installs, and §15.8's SNTP client need them; the calls that change system state become root-only when §18.6 enforces credentials
- [ ] `/dev/kmsg` in devfs, so a user `dmesg` reads the kernel log
- [ ] credentials in userspace: the §9.5 uid and gid gain saved set-IDs and a supplementary group list, inherited across `fork` and `execve`; `getuid`, `geteuid`, `getgid`, `getegid`, `getresuid`, `getresgid`, `getgroups`, `setuid`, `setgid`, `setreuid`, `setregid`, `setresuid`, `setresgid`, `setgroups` over them. Until §18.6 any caller may make any transition; the privilege check and the set-user-ID bits on `execve` are §18.6
- [ ] `procfs` backed by the process table: `cmdline`, `status`, `maps`, `fd`, `stat` per pid; `ps` reads it and syscall 500 is deleted
- [ ] `getrlimit` / `setrlimit` (`prlimit64`), `getrusage`; `RLIMIT_NOFILE` lifts a process from the default of 256 descriptors (Phase 10) to a hard cap in the `limits` module, since toolchains open more than 256 files
- [ ] `uname`, `sysinfo`, `gettimeofday`, `clock_gettime`, `clock_nanosleep`
- [ ] `ioctl` with a registry rather than a growing match arm
- [ ] `prctl` for the few things that need it
- [ ] `ENOSYS` for the unimplemented, logged once per syscall number, so a port's failure is immediately legible

---

# Era III. Platform

Where it stops being a kernel demo and becomes something you can use. Nothing here has a canonical
right answer, which is the interesting part.

Phases 15, 16, and 17 all start from 14. Phase 16 needs nothing else, so self-hosting does not wait for a
compositor. Phase 17 needs 15, because `cargo`, `git` remotes, and fetching sources need TCP and TLS. Its
C toolchain (§17.2) can start as soon as 14 closes. The numbering is a reading order, not a build order.

## Phase 14: Userspace

**Goal.** A real userspace: a C library, a dynamic linker, an init system, crypto primitives, a root
filesystem on disk, and enough utilities that the shell is useful.

**Unlocks.** Porting software instead of writing everything. Logins, and packages that can be trusted. A
root filesystem large enough for the Phase 17 toolchains.

**Architectures.** Both. musl's x86_64 and aarch64 ports bind to the one generated table (§11.6). The
dynamic linker's relocation types and TLS are per architecture.

**Exit gate**
- [ ] a C program cross-compiled on the host with the §14.1 sysroot runs correctly on both architectures
- [ ] a dynamically linked binary against a shared libc runs, and `ldd` lists its dependencies
- [ ] init starts services from configuration, restarts a crashed one, and reaps orphans
- [ ] `login` on the serial console accepts a user from the hashed shadow file and refuses a wrong password, and `id` in the new session reports that user's uid, gid, and groups
- [ ] a shell script with pipes, redirection, variables, conditionals, and loops runs
- [ ] `/bin/tests` (§10.5) and `libc-test` (§14.1), minus the cases on §14.1's checked-in expected-failure list, pass inside the VM on both architectures, run automatically in CI
- [ ] a package installs, upgrades, and removes cleanly with file conflict detection, and a package with a bad signature is refused
- [ ] `getrandom` returns seeded output on both architectures, and the §14.7 primitives pass their published test vectors in-guest
- [ ] the system boots with root on a vibefs v2 disk image built from the §14.6 packages, on both architectures, and this gate runs from it
- [ ] tag `v0.14.0`

### 14.1 libc
- [x] decided in Phase 10: the Rust user runtime from §10.5 is the native library for everything vibeOS ships; musl is ported for the C surface, since Phase 17's toolchains need it anyway. Both bind to the one generated syscall table.
- [ ] the musl port: `crt1` and `__libc_start_main`, the `syscall` shim, `errno`, `__set_thread_area` over §13.1's TLS, and `libc-test` as the conformance suite
- [ ] a host cross toolchain: `make sysroot` builds, for each architecture, musl, our headers, and compiler-rt's builtins and crt objects (aarch64 `long double` is binary128, so musl's `printf` alone needs `__multf3` and relatives). A per-triple clang config file sets `--sysroot`, `-resource-dir`, `-fuse-ld=lld`, and `--rtlib=compiler-rt`, so `clang --config=<it> --target=<arch>-linux-musl` builds and links C for vibeOS on the host. That needs an LLVM clang and `ld.lld`, which on macOS means Homebrew `llvm` and `lld`, since Apple's clang ships neither; README and AGENTS.md gain them in the same commit. The Linux syscall numbers and struct layouts (§11.6) are what make that triple work
- [ ] musl's `bits/syscall.h` checked against the generated table in `make check`, so the two cannot drift
- [ ] musl's `mallocng` over `brk` and §12.4's `mmap`, `mremap`, and `madvise`; §18.4 hardens it
- [ ] musl's stdio, string, math, `pthreads` (over §13.1 threads and §13.5 futexes, cancellation over §13.8 signals), `setjmp`, `dlopen`, and locale pass `libc-test` on both architectures. Each failure is fixed in the kernel in this phase, or recorded on a checked-in expected-failure list with its reason, or added as an open box to the phase that lands it
- [ ] unmodified static Linux binaries built against musl run: busybox's own test suite passes on both architectures, minus a checked-in expected-failure list, since the syscall numbers, struct layouts, and auxiliary vector are Linux's

### 14.2 Dynamic linking
- [ ] the kernel side: `execve` maps an `ET_DYN` image at a load bias it chooses, and for `PT_INTERP` also maps the named interpreter and enters at its entry point. `AT_BASE` carries the interpreter's base, and `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, and `AT_ENTRY` describe the main program with its bias. `src/elf.rs` stops refusing `ET_DYN` and `PT_INTERP`, and SYSCALL.md §7 says so. Host tests cover the bias arithmetic; an in-guest test runs a static-PIE binary and an interpreted one on both architectures
- [ ] musl's dynamic linker, which is `libc.so` itself, static and self-relocating: `DT_NEEDED` and search paths, `RELA`, `JMPREL`, and `RELR`, and symbol resolution with the correct scope and interposition order
- [ ] binding is immediate, as musl does it; `RTLD_LAZY` defers only unresolved symbols, and there is no lazy PLT binding
- [ ] TLS: initial-exec and dynamic models, `__tls_get_addr`
- [ ] `dlopen`, `dlsym`, `dlclose`, `dladdr`
- [ ] `LD_PRELOAD` and `LD_LIBRARY_PATH`, useful for debugging more than for anything else
- [ ] `libc.so` invoked as `ldd` lists each `DT_NEEDED` object and the path it resolved to

### 14.3 init and services
- [ ] PID 1: mount the base filesystems, start services, reap orphans, handle shutdown
- [ ] a declarative service definition: dependencies, restart policy, environment, working directory
- [ ] dependency-ordered parallel startup
- [ ] service supervision with restart backoff
- [ ] log collection from service stdout and stderr into the system log
- [ ] socket activation, which is genuinely elegant and not much work once sockets exist
- [ ] shutdown: signal services, wait with a timeout, unmount, and power off through `reboot` (ACPI on x86_64, PSCI on aarch64)
- [ ] a control tool for start, stop, restart, status, and logs
- [ ] `login` and a getty on `/dev/tty1` and on `/dev/ttyS0` (§13.7); `passwd` and `su`; a shadow file hashed with §14.7's password hash. `login` and `su` switch identity with §13.9's calls; until §18.6 nothing checks the switch, so an ordinary user's `su` and `passwd` do not need the set-user-ID bit until then

### 14.4 Coreutils
- [ ] file and directory: `ls`, `cp`, `mv`, `rm`, `mkdir`, `rmdir`, `ln`, `touch`, `stat`, `find`, `du`, `df`
- [ ] text: `cat`, `head`, `tail`, `wc`, `sort`, `uniq`, `cut`, `tr`, `grep`, `sed`, `diff`
- [ ] process: `ps`, `kill`, `top`, `time`, `nice`
- [ ] system: `uname`, `date`, `uptime`, `free`, `mount`, `dmesg`, `env`, `id`, `hostname`
- [ ] archive: `tar`, `gzip`
- [ ] editor: something small, `vi`-flavored. Editing on the machine matters more than it sounds like.
- [ ] each with real argument parsing and correct exit statuses, because scripts depend on both

### 14.5 Shell
- [ ] a POSIX-shaped shell: word splitting, quoting, expansion, globbing
- [ ] redirection including here-documents and file descriptor manipulation
- [ ] pipelines, `&&`, `||`, `;`, subshells, command substitution
- [ ] variables, environment, `export`, arithmetic expansion
- [ ] control flow: `if`, `while`, `for`, `case`, functions
- [ ] job control: background, `fg`, `bg`, `jobs`, `wait`
- [ ] interactive: history, completion, line editing, prompt expansion
- [ ] scripts with a shebang, and enough correctness to run a build script

### 14.6 Packaging
- [ ] a package format: metadata, dependencies, file list, checksums, install scripts
- [ ] a local package database with installed files and owners
- [ ] install, remove, upgrade, query, verify, with file conflict detection
- [ ] dependency resolution, and a clear error rather than a partial install
- [ ] a build recipe format and a tool that produces packages reproducibly
- [ ] a repository format signed with §14.7's Ed25519, read from a local directory or a mounted image; §15.8 fetches it over HTTP
- [ ] signature verification on every install and upgrade, with an unsigned or tampered package refused
- [ ] the base system itself shipped as packages, which is the test that the format is real

### 14.7 Crypto primitives
Moved here from Networking, which had them from Hardening: login (§14.3) and signed packages (§14.6)
need them in this phase, and none of them needs the network. X.509 and TLS stay in §15.11. Disk
encryption and boot integrity stay in §18.7.

- [ ] one crypto crate, built for the user runtime and for the kernel, so login, packaging, TLS, `sshd`, the CSPRNG, and §18.7's disk encryption share one implementation
- [ ] an entropy pool: `RDRAND` and `RNDR`, virtio-rng, timer jitter, interrupt timing, with health checks (`/dev/random` already prefers virtio-rng then `RDRAND`; S1)
- [ ] a CSPRNG behind `/dev/random`, `/dev/urandom`, and `getrandom`; `getrandom` and `/dev/urandom` never block after the pool is seeded, and `/dev/random` blocks only until then
- [ ] hashes and ciphers: SHA-2, SHA-3, AES-GCM, ChaCha20-Poly1305
- [ ] public key: Ed25519, X25519, RSA verification
- [ ] a memory-hard password hash (Argon2id) for the shadow file
- [ ] constant-time primitives with the RFC test vectors as host tests, and a host-side comparison against a reference implementation; the same vectors run in-guest from `/bin/tests` on both architectures, which is what the gate checks

### 14.8 Root filesystem
vibefs v1 holds at most 4 MiB, 64 inodes, and 96 directory entries per volume ([VIBEFS.md](VIBEFS.md)
§3). The base system and the Phase 17 toolchains need a real one.

- [ ] vibefs format version 2, designed for the §12.5 page cache: multi-GiB volumes, at least a million inodes, B-tree directories without a volume-wide entry cap, and extent trees rather than four extents per inode; VIBEFS.md gains the v2 format before any code, as §8.5 did for v1
- [ ] the kernel, `mkfs-vibefs`, and `fsck-vibefs` share the v2 code, as they do for v1; `fsck` upgrades a v1 volume in place, and a v1 volume still mounts
- [ ] the §8.5 crash-consistency test runs on a v2 volume larger than any v1 limit
- [ ] root is vibefs v2 on a virtio-blk disk that `make rootfs` formats and populates on the host from the §14.6 packages, on both architectures; the initrd keeps only what init needs to mount it

---

## Phase 15: Networking

**Goal.** A TCP/IP stack good enough to serve requests and fetch a package repository.

**Unlocks.** Remote access, package distribution, and the largest available source of well-specified
protocol work for an agent to get wrong in interesting ways.

**Architectures.** Both. virtio-net and everything above the netdev layer are shared. e1000 is PCI,
builds for both, and its in-guest tests run under QEMU (`-device e1000`) on both; real hardware exercises
it in Phase 20. The §10.1 KVM leg measures the throughput numbers on x86_64. The aarch64 numbers are
measured under HVF on the dev host, since GitHub's arm64 runners have no KVM.

**Exit gate**
- [ ] `ping` from the host to the guest and back
- [ ] DHCP acquires an address, and DNS resolves a name
- [ ] a TCP server in the guest serves a file to `curl` on the host, and the bytes match
- [ ] a TCP client fetches 1 GiB from the host with no corruption, at 1 Gbit/s or better over virtio-net and 4 Gbit/s or better over loopback, both under KVM in a 2-vCPU, 512 MiB guest
- [ ] the stack survives a packet fuzzer: malformed headers, bad checksums, overlapping fragments, no panics
- [ ] `netstat`-equivalent shows sockets in correct states through a full connection lifecycle
- [ ] an HTTPS fetch from a server on the host completes with the certificate chain verified against a test CA, and a tampered certificate is refused, using §15.11
- [ ] an `sshd` login from the host reaches a shell on a pty
- [ ] SNTP against the §15.10 host responder sets the wall clock to within 100 ms of the host
- [ ] host tests for header parsing, checksums, TCP state transitions, and sequence arithmetic
- [ ] tag `v0.15.0`

### 15.1 netdev layer
- [ ] a `NetDevice` trait: transmit, MTU, MAC, link state, and statistics
- [ ] receive queues delivering into the stack from a softirq or a dedicated thread, never from the hard IRQ
- [ ] a packet buffer type with headroom and tailroom so headers can be prepended without copying
- [ ] checksum and segmentation offload flags, used when the device supports them
- [ ] a loopback device, which is also the easiest way to test everything above it
- [ ] per-device statistics: packets, bytes, errors, drops

### 15.2 Drivers
- [ ] virtio-net: receive and transmit virtqueues, mergeable receive buffers, checksum offload, multi-queue
- [ ] e1000, for real hardware and because it is well documented
- [ ] in-guest driver tests against a loopback QEMU network configuration

### 15.3 Link layer
- [ ] Ethernet framing, parsing, and dispatch by EtherType
- [ ] ARP with a cache, timeouts, request queueing for unresolved destinations, and gratuitous ARP handling
- [ ] VLAN tagging, cheap to add and annoying to retrofit
- [ ] neighbor discovery for IPv6 later, with ARP structured so it is not a special case

### 15.4 IP
- [ ] IPv4 header parse, validate, and construct, with checksum
- [ ] routing table with longest-prefix match, a default route, and per-route MTU
- [ ] fragmentation and reassembly, with a reassembly timeout and a bound on held fragments so it is not a memory attack
- [ ] ICMP: echo, destination unreachable, time exceeded, and correct generation on error
- [ ] TTL handling and forwarding, which makes it a router with very little extra work
- [ ] the header code in the library half, host-tested against captured packets

### 15.5 UDP
- [ ] datagram send and receive with port binding and demultiplexing
- [ ] checksum computation and validation, including the optional-zero case
- [ ] receive queue per socket with a bound and a drop counter
- [ ] connected UDP sockets
- [ ] enough to run DNS and DHCP, which is what unblocks everything else

### 15.6 TCP
- [ ] the full state machine, transitions tested exhaustively as a host test
- [ ] three-way handshake, and connection teardown including simultaneous close and `TIME_WAIT`
- [ ] sequence and acknowledgement arithmetic with wraparound handled, host-tested
- [ ] send and receive buffers with a sliding window and zero-window handling
- [ ] retransmission with an RTO from an RTT estimator, exponential backoff
- [ ] fast retransmit and fast recovery
- [ ] congestion control: slow start, congestion avoidance, and a modern algorithm afterward
- [ ] delayed ACK, Nagle, and a `TCP_NODELAY` option, since interactive traffic and bulk traffic want opposite things
- [ ] window scaling, timestamps, and selective acknowledgement
- [ ] keepalive, and a listen backlog with SYN flood resistance
- [ ] path MTU discovery
- [ ] this is the single largest correctness surface in the project; the test suite matters more than the implementation

### 15.7 Socket API
- [ ] the §13.3 socket calls (`socket`, `bind`, `listen`, `accept`, `accept4`, `connect`, `sendto`, `recvfrom`, `shutdown`) extended to `AF_INET`; §15.9 adds `AF_INET6`
- [ ] `getsockopt` and `setsockopt` for the options that exist, and a clear error for those that do not
- [ ] `getsockname`, `getpeername`
- [ ] non-blocking mode integrated with `ppoll` and `epoll`
- [ ] `sendmsg` and `recvmsg` (from §13.3) for `AF_INET`, with scatter-gather and IP-level control messages
- [ ] `SO_REUSEADDR`, `SO_REUSEPORT`, `SO_BROADCAST`, `SO_BINDTODEVICE`, and sending from `0.0.0.0` on an interface with no address yet, which a DHCP client needs before it has a lease
- [ ] ICMP sockets for `ping` and `traceroute`: `SOCK_DGRAM`/`IPPROTO_ICMP` ping sockets and `SOCK_RAW` ICMP sockets; `IP_TTL` on UDP and ICMP sockets; ICMP errors delivered to the socket that caused them
- [ ] an `AF_PACKET`-shaped socket that sends and captures whole frames on one interface, for the `tcpdump`-equivalent
- [ ] `NETLINK_ROUTE`-shaped configuration and enumeration of interfaces, addresses, link state, and routes, since musl's `getifaddrs` and `if_nameindex` use it; `SIOCGIF*` ioctls through the §13.9 registry for software that still calls them
- [ ] `/proc/net/tcp`, `/proc/net/udp`, and `/proc/net/unix` list every socket with its addresses and state, which the `netstat`-equivalent and `ss` read

### 15.8 Configuration and tools
- [ ] a userspace DHCP client over §15.5 UDP broadcast that applies its lease through §15.7's netlink interface: discover, request, lease renewal, and correct behavior on lease expiry
- [ ] DNS resolver: A, AAAA, CNAME, with a cache honoring TTLs, retries, and multiple servers
- [ ] static configuration files and an `ip`-equivalent tool
- [ ] `ping`, `traceroute`, `netstat`, `ss`, `tcpdump`-equivalent
- [ ] an HTTP client good enough to fetch packages from a §14.6 repository, and a server good enough to prove the stack
- [ ] an SNTP client so `date` is right
- [ ] `sshd` on §14.7's primitives (SSH does its own key exchange; it does not use TLS), with §14.3's login and §13.7's ptys, and a login from a host client in CI; this is the point at which the machine becomes genuinely usable remotely

### 15.9 IPv6
- [ ] addressing, header parsing, extension headers
- [ ] neighbor discovery and stateless address autoconfiguration
- [ ] ICMPv6
- [ ] dual stack sockets
- [ ] deliberately after IPv4 works, and deliberately not skipped

### 15.10 Testing
- [ ] a packet injection interface so the stack can be tested without a real device
- [ ] replay of captured traffic as host tests
- [ ] a fuzzer over every parser, run in CI
- [ ] throughput and latency benchmarks with regression thresholds, on the §10.1 KVM leg
- [ ] host-to-guest integration tests in CI over QEMU user networking and a tap device
- [ ] an SNTP responder on the host that the guest reaches from CI (QEMU user networking has no NTP service of its own), serving the host clock the Phase 15 SNTP gate measures against
- [ ] a deliberately hostile peer: reordering, duplication, loss, tiny windows

### 15.11 TLS
The primitives, the entropy pool, and the CSPRNG are §14.7. This is the part that needs a peer. Package
fetching over HTTPS (§15.8) and `cargo` (§17.3) need it before Hardening. `sshd` does not, since SSH
brings its own key exchange.

- [ ] X.509 parsing and chain validation with a bundled root store, host-tested against real certificates and fuzzed like every parser
- [ ] TLS 1.3 client and server in userspace, on the §14.7 crate; 1.2 only if a peer that matters demands it
- [ ] an HTTPS fetch is the integration test, run in CI against a host peer with a test CA; a fetch from a public host is a manual check, not a gate

---

## Phase 16: Graphics and Windowing

**Goal.** More than one window. A compositor, an input stack, and a terminal emulator running in it.

**Unlocks.** The thing that makes people believe it is an operating system.

**Architectures.** Both. PS/2 is x86_64 only. virtio-input (§11.5) is the keyboard and pointer on
aarch64 and works on x86_64 too. virtio-gpu is shared.

**Exit gate**
- [ ] multiple windows, movable and resizable, with correct overlap and damage handling
- [ ] the terminal emulator runs the shell through a pty, with correct escape sequence handling
- [ ] mouse and keyboard events routed to the focused window, on both architectures
- [ ] a screenshot captured programmatically and compared against a reference in CI (§16.1)
- [ ] a resolution change at runtime with clients reacting correctly
- [ ] the compositor holds 60 frames per second at 1920×1080 with ten windows moving, fewer than 1% of frames dropped over ten seconds, measured under KVM in a 4-vCPU, 2 GiB guest with virtio-gpu 2D
- [ ] the compositor killed with `SIGKILL` leaves the text console visible and taking keyboard input, on both architectures
- [ ] tag `v0.16.0`

### 16.1 Display abstraction
- [ ] a `Display` with a mode list, current mode, and framebuffer access
- [ ] the Limine framebuffer as the fallback, always available
- [ ] mode setting where the hardware supports it
- [ ] multiple outputs with positions, because a second monitor should not be a rewrite
- [ ] vsync and page flipping, so tearing is fixable rather than inherent
- [ ] the text console double-buffered through the display abstraction, which closes §5.1's parked box
- [ ] a pixel format abstraction that is not hardcoded to BGRX
- [ ] the `Display` exposed to userspace as a devfs node (DRM/KMS-shaped): the mode list, and mode set where the hardware supports it, through the §13.9 `ioctl` registry; scanout buffers the compositor maps with `mmap` as a device mapping (write-combining, not page-cache frames); page flip; and vblank events that wake `ppoll` and `epoll` (§13.6)
- [ ] one owner of the display at a time: while a process holds it, the kernel text console stops drawing to the scanout; when the holder closes the node or exits, the kernel restores the console's mode and redraws it, so a crashed compositor leaves a usable console
- [ ] the harness captures the framebuffer with QEMU's `screendump` over the monitor and compares it against a checked-in reference image with a stated per-pixel tolerance; a mismatch writes a diff image and fails the test, on both architectures

### 16.2 GPU
- [ ] virtio-gpu 2D: resource creation, transfer, `set_scanout`, flush
- [ ] virtio-gpu cursor plane, which is worth it for the latency alone
- [ ] EDID for mode discovery
- [ ] the `Display` and buffer interfaces admit a 3D backend ([Beyond](#beyond)) without changing clients: each buffer carries a type and a completion fence, and a stub second backend builds against the interface
- [ ] a software rasterizer good enough that 3D is optional: lines, rects, blits, alpha, text

### 16.3 Compositor
- [ ] a surface tree with position, size, stacking order, and transforms
- [ ] damage tracking so only changed regions are recomposited
- [ ] alpha blending and clipping
- [ ] double or triple buffering with a frame callback protocol so clients throttle to the display
- [ ] a hardware cursor when available, software otherwise
- [ ] fullscreen bypass, sending a client's buffer straight to scanout
- [ ] the frame loop must not depend on client responsiveness; a hung client freezing the desktop is the failure mode to design against

### 16.4 Input
- [ ] an input event abstraction: keyboard, pointer, and touch, with timestamps
- [ ] PS/2 mouse on x86_64, and virtio-input keyboard and pointer on both architectures; USB HID arrives through §20.3 into this same abstraction
- [ ] input devices exposed to userspace as devfs event nodes carrying the timestamped events above, read with `read` and waited on with `ppoll`/`epoll`; an exclusive grab stops the §13.7 console TTY consuming keyboard and pointer input while it is held, serial input is unaffected, and the grab drops when the holder closes the node or exits
- [ ] event routing: pointer to the surface under the cursor, keyboard to the focused surface
- [ ] focus policy, grabs, and click-to-focus
- [ ] keyboard layout handling with dead keys and compose, which is more work than it looks
- [ ] pointer acceleration, and scroll with kinetic behavior
- [ ] repeat rate and delay

### 16.5 Display protocol
- [ ] a client-server protocol over a Unix socket, with buffer sharing through shared memory or dma-buf-equivalent
- [ ] surface lifecycle, commit semantics, and damage submission
- [ ] input event delivery
- [ ] window management: title, class, minimum and maximum size, state
- [ ] clipboard and drag and drop
- [ ] the protocol documented in `docs/` before implementation, since two implementations must agree
- [ ] the protocol document maps each request to its Wayland counterpart or says why none exists (Wayland compatibility is in [Beyond](#beyond))

### 16.6 Window management and toolkit
- [ ] window decorations, or a client-side decoration protocol
- [ ] move, resize, minimize, maximize, close
- [ ] tiling and floating layouts, workspaces
- [ ] a keyboard-driven window switcher
- [ ] a widget toolkit: layout, buttons, text entry, lists, scrolling
- [ ] font rendering with real glyph rasterization, hinting, kerning, and subpixel positioning, which is a project in itself
- [ ] a panel with a clock, a launcher, and a system tray

### 16.7 Terminal emulator
- [ ] a pty client with correct `termios` interaction
- [ ] VT100 and xterm escape sequence handling: cursor, colors, attributes, scroll regions, alternate screen
- [ ] 256-color and true color
- [ ] a scrollback buffer with selection and clipboard
- [ ] UTF-8, wide characters, and combining marks
- [ ] resize with `SIGWINCH`
- [ ] this is the single most important application on the system; it gets the same care as the kernel

---

## Phase 17: Self-hosting Toolchain

**Goal.** vibeOS compiles vibeOS. Check out the source on the machine, build it, boot the result.

**Unlocks.** The claim. Also a genuinely brutal test of the POSIX surface, since compilers exercise
everything.

**Architectures.** Both, and each cross-builds the other. The byte-identical comparison of cross and
native builds is in [Beyond](#beyond).

**Exit gate**
- [ ] a C compiler runs on vibeOS and produces working binaries
- [ ] `rustc` runs on vibeOS
- [ ] the vibeOS source tree builds on vibeOS into a bootable ISO
- [ ] that ISO boots and rebuilds itself for two generations, and the loop script's comparison reports the first- and second-generation ISOs byte-identical (§17.5)
- [ ] `make test` runs on vibeOS, itself a guest under QEMU with KVM: the harness on §17.6's CPython drives §17.6's QEMU under TCG and reports results the same way CI does; the on-hardware run is in §20.8
- [ ] the loop runs on aarch64 as well as x86_64, and each architecture can cross-build the other
- [ ] the whole loop is scripted, not a sequence of manual steps someone remembers
- [ ] tag `v0.17.0`

### 17.1 POSIX completeness
- [ ] audit against what a real toolchain needs, and close the gaps rather than guessing
- [ ] filesystem behavior compilers depend on: `rename` atomicity, `O_TMPFILE`, `fsync` semantics, correct `mtime`
- [ ] process behavior: `posix_spawn`, large environments, long argument lists, pipe-heavy pipelines
- [ ] memory behavior: large `mmap`, `mprotect` for JIT, hundreds of thousands of small allocations
- [ ] `/proc` entries that build systems read
- [ ] a large-file test, since compilers write big object files and every off-by-one shows up there

### 17.2 C toolchain
- [ ] port `tcc` first: small, self-hosting, and a fast way to prove the environment works
- [ ] an assembler and linker, either ported or written, with ELF output and relocation support
- [ ] `ar`, `nm`, `objdump`, `strip`, `readelf`
- [ ] then clang and LLVM, which is a large port and mostly a matter of the C++ standard library and filesystem behavior
- [ ] `make`, and enough of `sh` and `awk` for configure scripts to survive
- [ ] `cmake` and `ninja`, which most real projects assume

### 17.3 Rust toolchain
- [ ] a `std` port: `x86_64-unknown-vibeos` and `aarch64-unknown-vibeos`, with the `sys` layer implemented over our syscalls
- [ ] thread, file, socket, process, and time support in `std`
- [ ] `rustc` running on vibeOS, which requires LLVM working first
- [ ] the kernel uses nightly features (`abi_x86_interrupt`, `alloc_error_handler`), so the on-device `rustc` is the pinned nightly, bootstrapped for the `*-unknown-vibeos` host triples from a patch set carried in-tree until the targets are upstream
- [ ] `cargo`, which requires networking (Phase 15), TLS (§15.11), and git (§17.4), with an offline vendored mode for the build loop
- [ ] a cross-compiled bootstrap first, then a native build, in that order
- [ ] the kernel itself builds on-device for `x86_64-unknown-none` and `aarch64-unknown-none-softfloat`, with no `build-std`

### 17.4 Development environment
- [ ] a git implementation or port, enough for clone, commit, branch, and push
- [ ] a real editor, ported rather than written
- [ ] a debugger: `ptrace`, breakpoints, single stepping, symbol and DWARF reading; register writes go through the §13.8 context validation
- [ ] virtio-fs or 9p to mount the host checkout, so the build loop does not start by copying the tree into an image
- [ ] `strace`-equivalent, which the kernel syscall tracing already mostly provides

### 17.5 The loop
- [ ] a build script that goes from a clean checkout to a bootable ISO on vibeOS
- [ ] a deterministic kernel, initrd, and ISO build: `SOURCE_DATE_EPOCH` honored by `mkinitrd` and the ISO writer, fixed volume and GPT GUIDs, staged file times pinned, `--remap-path-prefix` for rustc, sorted inputs; two builds of one tree on the host are byte-identical in CI before the loop depends on it
- [ ] a second-generation build, compared byte for byte against the first by the loop script
- [ ] the test suite running on-device
- [ ] timings recorded, because "it works" and "it works in under an hour" are different claims
- [ ] documented in `docs/` as a reproducible procedure, since the point is that someone else can do it

### 17.6 Build and test dependencies
Everything the build and the harness run on the host today must run on vibeOS.

- [ ] CPython, since the harness, `scripts/gen_ksyms.py`, and the `scripts/check_*.py` guards are Python; the port is also a hard test of the POSIX surface
- [ ] `xorriso`, or an ISO writer in hostlib alongside `mkinitrd`, and the Limine host tool built with the §17.2 toolchain
- [ ] the LLVM binary tools the build calls (`llvm-objdump`, `llvm-nm`), from the §17.2 LLVM port
- [ ] QEMU, ported and running under TCG, since vibeOS has no hypervisor until Phase 21; the harness gains a backend for the §21.2 VMM when that lands
- [ ] harness timeouts scaled for nested TCG, set once in the `VIBEOS_*` reader rather than per test

---

# Era IV. Frontier

The parts that separate a working system from a serious one.

Phases 18 and 19 are independent of each other and run under QEMU. Phase 20 needs both architectures
from 11, the drivers from 7 and 15, and §16.4's input abstraction, which USB HID delivers into. It also
takes the lines only hardware can close: deeper C-states, frequency scaling, and measured power extend
the §19.6 idle path, and NVMe polling follows §19.8, so Phase 20 needs those two sections of 19. Phase 21
needs 20 for its test environment, 18 for the access control and resource limits its containers build on
(§18.6), and 19 for the §19.3 benchmarks in its gate and the §19.4 group scheduling behind its
cgroup-equivalent. Phase 22 needs everything.

## Phase 18: Hardening

**Goal.** Assume everything in userspace is hostile and the kernel has bugs. Make both survivable.

**Unlocks.** Trusting the machine with anything that matters. Running untrusted code on purpose, which
Phase 21's containers and Phase 22's users both are.

**Architectures.** Both, except the `FSGSBASE` gate line and its §18.3 box, which are x86_64 only:
aarch64 keeps the kernel's per-CPU base in a register EL0 cannot write (§11.4), so it has no `swapgs`
hazard. The `LDTR`/`STTR` box in §18.3 is aarch64 only. SMEP, SMAP, and UMIP landed with S1 (§9.1),
their fault tests with §10.6, and PAN and PXN in §11.6. Control-flow integrity (CET, pointer
authentication, BTI) is the per-architecture stretch in §18.9.

**Exit gate**
- [ ] kernel `.text` read-only and executable, `.rodata` read-only and NX, `.data` NX, verified at runtime
- [ ] KASLR active on both architectures, and a crash dump still symbolizes correctly
- [ ] `FSGSBASE` enabled; an in-guest test sets the user GS base to a kernel-half value and enters the NMI, `#DB`, and `#MC` handlers from kernel mode with that base live, and on a 2-CPU guest runs a syscall loop while the other CPU sends NMI IPIs; in every case the handlers find this CPU's `PerCpu` and the user's value survives
- [ ] every speculation mitigation the kernel reports enabled at boot has a measured-cost entry in `docs/`, measured under KVM in a 2-vCPU, 512 MiB guest on each architecture; a harness test compares the boot log's list against the document
- [ ] a syscall fuzzer accumulates at least 24 hours of fuzzing per architecture each week on the weekly schedule, in shards of at most 5 hours (a GitHub-hosted job stops at 6), each in a 2-CPU, 512 MiB guest under TCG, without a kernel panic; the job summary records the total, and every crash becomes a replayed regression test (§18.5)
- [ ] KASAN builds pass the full test suite
- [ ] a DMA outside a device's mapped buffers faults instead of landing, under the §18.1 IOMMU on both architectures
- [ ] a sandboxed process cannot reach the filesystem or network outside its policy, tested
- [ ] `docs/THREAT_MODEL.md` has the parts §18.8 names; `scripts/check_threat_model.py` in `make check` fails when a part is missing, or when a known open escalation path does not link to an open box or a [Beyond](#beyond) entry
- [ ] tag `v0.18.0`

### 18.1 Kernel memory protection
- [ ] per-section permissions applied after boot, with the init sections freed or made NX
- [ ] no writable-and-executable kernel mapping anywhere, including the trampoline page §10.6 left read-only and executable, asserted by a page table audit at boot
- [ ] the physmap NX, which is easy to get wrong and a straightforward escalation primitive
- [ ] guard pages on every kernel stack including the IST stacks
- [ ] a page table walker that verifies the whole address space against a policy, run as an in-guest test
- [ ] an IOMMU behind the §6.4 translation interface: VT-d on x86_64 (a q35 harness configuration with `-device intel-iommu`, alongside the `pc` default) and SMMUv3 on aarch64 (`-machine virt,acpi=off,iommu=smmuv3`). Each device gets its own DMA domain, and virtio devices negotiate `VIRTIO_F_ACCESS_PLATFORM`

### 18.2 KASLR
- [ ] the kernel linked as a static PIE on both architectures. On x86_64, drop the `relocation-model=static` and `-no-pie` rustflags, since PIE is the built-in target's default and Limine keeps the slid image inside the top 2 GiB the `kernel` code model needs. On aarch64, pass `-C relocation-model=pie` and `-C link-arg=-pie` explicitly. `-znorelro` stays. §0.1 and the rustflags paragraph in DESIGN §1 are updated in the same commit
- [ ] `kaslr: yes` in `limine.conf`, written explicitly because Limine's default changed across major versions, so Limine picks the slide and applies the relocations before `_start`; the kernel takes its base from `BootInfo` and never computes one
- [ ] the slide's entropy named as a deliberate gap in the threat model: Limine seeds from `RDSEED`/`RDRAND` and the TSC on x86_64, but only from the generic counter on aarch64, where `RNDR` is optional and absent on Apple M-series, Cortex-A76, and Neoverse N1
- [ ] randomized physmap and KVA region bases, drawn from the same boot-time sources
- [ ] the slide (`BootInfo`'s kernel base minus the link base) recorded; ksyms and the backtrace subtract it, so a crash dump symbolizes, and it is not otherwise exposed
- [ ] user address space randomization: stack, heap, and mmap bases, drawn from the §14.7 pool

### 18.3 CPU features

S1 turned SMEP, SMAP, UMIP, and `CR0.WP` on at boot (`arch::cpu::harden`). §10.6 put user access behind
`stac`/`clac` accessors, with the in-guest fault tests, and §11.6 did PAN and PXN. What is left:

- [ ] `CR4.FSGSBASE` enabled. Once a user can `wrgsbase` a kernel-half value, the §10.6 sign check is unsound, so NMI, `#MC`, `#DB`, and `#DF` entry saves `GS_BASE` with `rdgsbase`, loads this CPU's `PerCpu` pointer (kept at the top of each per-CPU IST stack), and restores the saved value on exit. Every switch away from a user thread reads back the live user FS base and user GS base (the latter from `KERNEL_GS_BASE` while in the kernel), since `wrfsbase` and `wrgsbase` change them without a syscall. The `lfence` after a conditional `swapgs` lands with the speculation mitigations below
- [ ] aarch64: user copies through the unprivileged `LDTR`/`STTR` instructions, so PAN is never cleared, even inside the accessors; an in-guest test asserts `PSTATE.PAN` is set inside every accessor
- [ ] speculation mitigations, measured before enabling, since some cost more than the risk

### 18.4 Stack and memory safety
- [ ] stack canaries in both kernel and userspace
- [ ] `FORTIFY`-equivalent checks in libc
- [ ] a hardened `malloc`: guard pages, delayed reuse, randomized placement, double free detection
- [ ] a KASAN build with a shadow map, red zones, and quarantined frees
- [ ] a checked kernel build (`-C overflow-checks=on -C debug-assertions -Zub-checks=yes`, since rustc has no UBSAN) passing the full suite on both architectures; Miri over the portable crate's host tests; musl and the C userspace built with `-fsanitize=undefined -fsanitize-trap=undefined`, trap mode because no UBSan runtime exists on vibeOS
- [ ] the `unsafe` audit: every block's `// SAFETY:` reason from the standing gate reviewed against the invariant it claims, since a pile of stale one-liners is where this ends up otherwise

### 18.5 Fuzzing
- [ ] a syscall fuzzer generating structured calls with valid and invalid arguments, in the weekly shards the exit gate describes; the parser-level fuzzers already run from §10.2 (every byte-slice parser, ELF included) and §15.10 (network)
- [ ] fuzzed FAT and vibefs images mounted in-guest under the §18.4 KASAN build, on the weekly job
- [ ] malformed packets injected through the §15.10 interface into a running stack under KASAN, on the weekly job
- [ ] malformed ELF files exec'd in-guest under KASAN, each refused with an error and none panicking the kernel
- [ ] coverage-guided where feasible, with crash reproduction as a checked-in test case
- [ ] every crash found becomes a regression test, without exception

### 18.6 Access control
- [ ] uid and gid actually enforced: file permissions, ownership, the privilege check on the `set*id` calls (§13.9), and the set-user-ID and set-group-ID bits on `execve`, with `AT_SECURE` set for them
- [ ] capability-style privilege splitting rather than a single root bit
- [ ] a syscall filter, `seccomp`-shaped, per process
- [ ] mount namespaces and a `pivot_root`-equivalent
- [ ] resource limits enforced: memory, descriptors, processes, CPU time
- [ ] an audit log for privileged operations

### 18.7 Crypto and boot integrity
The primitives and the CSPRNG are §14.7, package signatures §14.6, and TLS §15.11. This is what needs a
boot chain.

- [ ] UEFI secure boot: a signed bootloader and kernel
- [ ] measured boot with a TPM (swtpm under QEMU on both architectures): the TPM event log is replayed, and its digests for the bootloader, kernel, initrd, and boot configuration are checked against a boot-chain manifest that the build produces and signs with §14.7's Ed25519; firmware measurements are recorded but not checked, since the build does not produce the firmware
- [ ] full disk encryption on the §14.7 ciphers, which needs the block layer to support a transform

### 18.8 Threat model
- [ ] `docs/THREAT_MODEL.md`: what is trusted, what is not, what is deliberately out of scope and why
- [ ] the escalation paths that are known to exist and why they are still open
- [ ] a security response process, however informal, so the answer is not improvised

### 18.9 Stretch: control-flow integrity
- [ ] CET shadow stacks and indirect branch tracking on x86_64
- [ ] pointer authentication and BTI on aarch64

---

## Phase 19: Performance and Observability

**Goal.** Know why it is slow, then stop being slow. Neither is possible without measurement first.

**Unlocks.** Numbers instead of adjectives, and every later performance claim in this file.

**Architectures.** Both. Topology comes from CPUID on x86_64, and from MPIDR plus the device tree's
`cpu-map` on aarch64. §19.7 NUMA is x86_64 only in this phase: Limine strips the device-tree `memory@`
nodes that carry aarch64 memory affinity, so aarch64 NUMA arrives with ACPI SRAT on the §20.7 server.
Idle is `mwait` where CPUID reports MONITOR, `hlt` otherwise (KVM hides MONITOR unless QEMU runs with
`-overcommit cpu-pm=on`), and `wfi` on aarch64. Hosted CI has no hardware PMU: GitHub's runners are VMs
without one, and HVF emulates only the cycle counter. So the sampling path is checked on aarch64 under
TCG, whose emulated PMU counts cycles and raises the overflow interrupt, and on x86_64 from a
timer-driven sampler; hardware events (cache and branch misses, LBR, PEBS) are validated on the §20.8
machines.

**Exit gate**
- [ ] a flamegraph produced from a counter-overflow sampling profile on aarch64 under TCG, and from a timer-driven sampling profile on x86_64 under KVM, both in a 2-vCPU, 1 GiB guest
- [ ] tracing captures a full request path across syscall, scheduler, and driver with correlated timestamps
- [ ] benchmarks with thresholds on the §10.1 KVM leg in a 4-vCPU, 1 GiB guest, and a regression fails that job and so blocks the phase tag
- [ ] scheduler wakeup latency at the 99th percentile under 1 ms with a CPU-bound load of four times the core count, measured under KVM in a 4-vCPU, 1 GiB guest
- [ ] an idle CPU takes fewer than 5 timer interrupts per second over one minute, measured under KVM in a 2-vCPU, 512 MiB guest
- [ ] lock contention profiled; the most contended lock's spin time on the §19.3 mixed interactive workload cut by at least half, under KVM in a 4-vCPU, 1 GiB guest, with before and after numbers recorded
- [ ] slab statistics show per-cache object counts, and buddy-lock spin time on the §19.3 allocation microbenchmark, run on every CPU of a 4-vCPU, 512 MiB guest under KVM, is at least halved by §19.9, with before and after numbers recorded
- [ ] in a 512 MiB guest, a sequential read of a 1 GiB file leaves a hot working set resident, shown by the §19.10 refault counter
- [ ] tag `v0.19.0`

### 19.1 Tracing
- [ ] static tracepoints at the boundaries that matter: syscall entry and exit, scheduler switch, page fault, IRQ, block and network I/O
- [ ] a per-CPU lock-free ring buffer with a fixed-size record
- [ ] dynamic enable and disable per tracepoint; the cost of a disabled tracepoint measured with the §19.3 syscall-latency microbenchmark against a build without tracepoints, and the number recorded
- [ ] an export format a host tool can read, ideally one an existing viewer already understands
- [ ] a userspace tracing interface so applications can emit into the same timeline
- [ ] the timestamp source must be globally monotonic, which is the phase 2 TSC work finally paying off

### 19.2 Profiling
- [ ] PMU setup: cycles and instructions where the host provides them, cache and branch misses on hardware that has them (checked on the §20.8 machines); where CPUID leaf 0xA or `ID_AA64DFR0_EL1.PMUVer` reports no PMU, setup skips with a registered marker, so CI and HVF still pass
- [ ] sampling on a counter overflow interrupt with a stack walk
- [ ] per-process and per-thread accounting
- [ ] a flamegraph pipeline from samples to output
- [ ] `perf`-equivalent for stat and record
- [ ] last branch records and precise event sampling where the hardware offers them

### 19.3 Benchmarks
- [ ] microbenchmarks: syscall latency, context switch, page fault, allocation, lock acquire
- [ ] subsystem: file read and write throughput, network throughput and latency, process creation rate
- [ ] macro: kernel build time, boot time, a mixed interactive workload
- [ ] all of it on the §10.1 KVM leg, with recorded history and a threshold that fails
- [ ] variance controlled well enough that the numbers mean something, which is most of the work

### 19.4 Scheduler
- [ ] replace round-robin with something latency-aware: weighted fair queueing or a virtual-deadline scheme
- [ ] priorities and nice values with real effect
- [ ] a real-time class with `FIFO` and `RR` policies
- [ ] priority inheritance for the real-time class: `FUTEX_LOCK_PI` and `FUTEX_UNLOCK_PI` over §13.5's owner field for user mutexes, and the kernel `BlockingMutex`; an in-guest inversion test (low priority holds the lock, medium priority spins, high priority waits) completes within a bound
- [ ] load balancing: periodic first, then work stealing, measured against the periodic baseline before keeping it
- [ ] topology awareness from CPUID on x86_64 and MPIDR plus the device tree on aarch64: prefer a sibling core, keep a thread near its cache
- [ ] group scheduling with CPU shares and quotas: the scheduler half of the cgroup-equivalent in §21.5
- [ ] latency measured under load, since the whole point is the tail and not the average
- [ ] per-CPU TCB ownership or a sharded TCB table; timeouts per CPU (timing wheel, `TimeoutQueue` replacement already anticipated in `src/sched.rs`)

### 19.5 Scalability
- [ ] RCU for read-mostly structures: the dentry cache, the routing table, the mount table
- [ ] seqlocks where readers dominate and writers are rare
- [ ] per-CPU counters aggregated on read, instead of a shared atomic on the hot path
- [ ] lock-free queues on the paths that need them, with a documented memory ordering argument
- [ ] the global locks split by hash or by CPU where profiling says it matters
- [ ] interrupt affinity rebalancing on the §6.3 table: move MSI-X and I/O APIC destinations (GIC SPIs and LPIs on aarch64) off a saturated core, measured before keeping it
- [ ] contention measured before and after each change, with the numbers recorded
- [ ] block cache: per-device or hashed locks; VFS: RCU-style dentry lookup or per-mount locks; log ring: per-CPU staging with a printer thread (ROADMAP §5.5 item already open)

### 19.6 Power and idle
- [ ] tickless idle: arm the next real deadline instead of a periodic tick
- [ ] idle through `mwait` on x86_64 where CPUID reports MONITOR, falling back to `hlt`, and `wfi` on aarch64, all at the shallowest state, which needs no firmware tables; deeper C-states, frequency scaling, and measured power are §20.2
- [ ] interrupt coalescing on the network and storage paths
- [ ] timer slack, so unrelated wakeups can batch
- [ ] CPU offlining for power management: migrate threads, redirect interrupts, park the core (PSCI `CPU_OFF` on aarch64); the reverse of bring-up, and not hotplug

### 19.7 NUMA
- [ ] node topology and distances from SRAT and SLIT, tested under QEMU `-numa` on x86_64; aarch64 takes the same code path from ACPI in §20.7
- [ ] per-node buddy allocators with node-local allocation as the default
- [ ] NUMA-aware scheduling, keeping a thread near its memory
- [ ] page migration on persistent remote access
- [ ] per-node statistics, because the failure mode is invisible without them

### 19.8 I/O
- [ ] an async submission interface, `io_uring`-shaped: submission and completion rings shared with userspace
- [ ] zero-copy paths for network send and file read
- [ ] `sendfile` and `splice`
- [ ] direct I/O bypassing the page cache
- [ ] user pages held by a device (direct I/O, the zero-copy send path, registered `io_uring` buffers) are pinned, with a pin count in the §12.1 frame metadata. A pinned anonymous page is always exclusive to one address space: pinning a COW-shared page breaks the share first, and `fork` copies a pinned page eagerly instead of sharing it, so the §12.3 write fault reuses a pinned page instead of copying it. Reclaim and §19.7 migration skip pinned pages
- [ ] in-guest: start a direct I/O read into part of a private page, then write elsewhere in that page while the read is in flight, once after a `fork` and once after an `mprotect` to read-only and back; the parent sees both the read's data and its own write, and the child's copy has neither
- [ ] polled I/O for virtio-blk, where the interrupt costs more than the spin, measured against interrupts; §20.4 applies the same to NVMe
- [ ] boot time reduced by parallelizing device probing and deferring what can be deferred
- [ ] virtio-blk zero-copy: DMA directly from page-cache pages once §12.5 unifies the caches; drop the bounce path
- [ ] packed virtqueues, measured against split before keeping them

### 19.9 Slab allocator
Moved from the memory phase: nothing before this phase needs it, and its gate is a contention measurement.

- [ ] object caches with a constructor, sized for a specific type
- [ ] slabs of one or more pages carved into objects, with a freelist in the unused object space
- [ ] per-CPU magazines so the common path takes no global lock
- [ ] caches for the hot types: TCB, process, inode, dentry, file, network buffer, request
- [ ] shrinking under memory pressure, driven by the same reclaim path as the page cache (§12.6, §19.10)
- [ ] `slabinfo` in the shell: per cache, objects active and total, pages used
- [ ] the general heap stays for odd-sized allocations; slab is not a replacement for it

### 19.10 Memory reclaim
Moved from the memory phase. §12.6 reclaims on demand, which is correct. This makes it fast and fair.

- [ ] watermarks (min, low, high) with a background reclaim thread, so allocations rarely stall in direct reclaim
- [ ] the §12.5 LRU split into an active and an inactive list, so a single sequential scan does not evict the working set; counters for promotions, demotions, and refaults
- [ ] the OOM score gains the §19.4 nice value and a per-process adjustment
- [ ] allocation latency under pressure measured before and after, with the numbers recorded

---

## Phase 20: Real Hardware

**Goal.** Boot on physical machines of both architectures, and keep booting on them.

**Unlocks.** The only honest test of everything QEMU forgives. A hardware compatibility list. Hardware
CI, which Phases 21 and 22 assume.

**Architectures.** Both, on real machines: x86_64 in §20.1 to §20.6, aarch64 in §20.7, and hardware CI
for each in §20.8. The agents choose the machines and state their cost; buying them is the maintainer's
approval, since it spends money.

**Exit gate**
- [ ] boots from USB on at least two physically different x86_64 machines, with output on a serial adapter or the screen
- [ ] real disk, real NIC, real USB keyboard, all functional
- [ ] the AML interpreter parses the DSDT and SSDTs of every machine on the compatibility list, host-tested against their `acpidump` output, and `_PRT` resolves PCI interrupt routing on each
- [ ] clean shutdown and reboot through ACPI
- [ ] suspend to RAM and resume, with devices restored
- [ ] idle power measured on each x86_64 machine at the shallowest and deepest C-state, with the numbers in the compatibility list
- [ ] a real aarch64 machine boots to the shell with a working disk and NIC
- [ ] NVMe and AHCI drives detected and used as root on real x86 machines
- [ ] hardware CI: one physical machine of each architecture netboots a built image nightly and reports results automatically
- [ ] `make test` runs on vibeOS booted on the §20.8 machine of each architecture, nightly, as the Phase 17 gate does under QEMU
- [ ] tag `v0.20.0`

### 20.1 Bare metal x86_64
- [ ] a real UEFI boot path and a real BIOS boot path, both tested on hardware rather than assumed
- [ ] x2APIC: read `IA32_APIC_BASE.EXTD` at LAPIC enable. When firmware hands off in x2APIC mode, which may be locked, drive the LAPIC and ICR through MSRs, never clear EXTD, and never map the xAPIC MMIO page. Parse MADT types 9 and 10 alongside 0 and 4. QEMU exercises the handoff by booting OVMF with more than 255 vCPUs, which hands off in x2APIC mode
- [ ] interrupt remapping on the §18.1 IOMMU (VT-d, AMD-Vi), so I/O APIC and MSI routes can target APIC IDs above 254; until it lands, the §6.3 affinity API keeps device interrupts on CPUs whose APIC ID is 254 or lower
- [ ] the memory map from real firmware, which is messier than QEMU's in ways that break assumptions
- [ ] real ACPI tables, which are also messier, including vendor quirks
- [ ] serial output over a USB adapter, and early output on the framebuffer for machines without one
- [ ] a crash dump written somewhere persistent, since there is no host to catch it
- [ ] a hardware compatibility list, honestly maintained

### 20.2 ACPI runtime
- [ ] an AML interpreter, which is a large and genuinely unpleasant subproject and unavoidable
- [ ] the interpreter in the portable half, host-tested against the `acpidump` tables of every machine on the compatibility list and fuzzed like every parser
- [ ] the device tree from the DSDT and SSDTs, resource assignment, `_CRS` parsing, `_PRT` interrupt routing
- [ ] power management: S5 shutdown, S3 suspend, S4 hibernate
- [ ] `suspend` and `resume` on the `Driver` trait (§6.1), called in dependency order, which S3 needs
- [ ] deeper C-states from `_CST`, with a governor choosing depth by predicted idle duration; P-states and frequency scaling from `_PSS` or CPPC; the §19.6 idle path gains both
- [ ] idle power measured on real hardware, which is the only honest test
- [ ] thermal zones and fan control
- [ ] battery and AC adapter status
- [ ] hotplug notifications, lid switch, power button
- [ ] `_OSI` handling and the vendor quirks that come with it

### 20.3 USB
- [ ] xHCI: controller init, command and event rings, device slots, endpoint contexts
- [ ] enumeration: address assignment, descriptor parsing, configuration selection
- [ ] hub support, including nested hubs
- [ ] HID: keyboard and mouse boot protocol, then report descriptor parsing, delivered through §16.4's input abstraction
- [ ] mass storage over bulk-only transport
- [ ] USB serial, which is how debugging a modern laptop works

### 20.4 NVMe
Moved from Phase 7: nothing before real hardware needs either driver. QEMU `-device nvme` and `-device ahci` first, real drives in §20.6.

- [ ] controller identify, admin queue setup, I/O queue creation per CPU
- [ ] submission and completion queue handling with doorbells and phase tags
- [ ] namespace enumeration
- [ ] read and write commands, flush, dataset management for discard
- [ ] MSI-X per queue
- [ ] the fast path built for depth from the start, since NVMe's whole point is parallelism
- [ ] polled completion for the fast path, measured against interrupts, as §19.8 did for virtio-blk

### 20.5 AHCI
- [ ] HBA and port initialization, command list and FIS structures
- [ ] identify device, LBA48 read and write
- [ ] ATAPI detection so a CD-ROM does not look like a broken disk
- [ ] present mainly because real hardware has it; QEMU testing via `-device ahci`

### 20.6 Real devices
- [ ] AHCI and NVMe validated on real drives, with real error handling
- [ ] SMART reporting
- [ ] real NICs: e1000e and igb, a Realtek 8168-family driver (QEMU has no model of it, so it is validated only on a compatibility-list machine that has one; it is what cheap hardware actually has), and the firmware loading some of them require
- [ ] Intel HDA audio, so there is sound
- [ ] SD and eMMC for single-board machines
- [ ] i2c and SMBus, needed for sensors and embedded controllers

### 20.7 Bare metal aarch64
- [ ] a real machine whose UEFI firmware is maintained when it is bought and boots Limine, so §11.1's boot path applies: by default a server-class Ampere machine (SBSA, ACPI) with an Intel NIC onboard or in a PCIe slot; a Raspberry Pi 5 only if a maintained edk2 port or U-Boot's EFI layer boots Limine on the board in hand (the original edk2 port was archived in February 2025). The machine that becomes §20.8's aarch64 CI machine also needs FEAT_NV2, because Phase 21's nesting gate runs on it: an AmpereOne-class server has it, while a Raspberry Pi 5 and Neoverse N1 machines do not
- [ ] ACPI on server-class machines, which ship no device tree: the §2.4 parser for MADT, GTDT, SPCR, MCFG, and SRAT (§19.7's NUMA code path on aarch64), and the §20.2 interpreter for the DSDT
- [ ] the device tree from real firmware on a board, which disagrees with QEMU's in the same ways real ACPI disagrees with QEMU's
- [ ] the GIC and ITS as the firmware describes them, rather than as QEMU's `virt` lays them out (§11.3)
- [ ] the machine's NIC for the Phase 20 gate: §20.6's igb or e1000e built and exercised on aarch64 on an Ampere machine; on a Raspberry Pi 5, the BCM2712 PCIe root complex (not ECAM, so §11.5's generic path does not reach it), the RP1 southbridge behind it, and RP1's Cadence GEM MAC
- [ ] SD or eMMC root on a board, NVMe root on a server
- [ ] USB over xHCI, shared with §20.3 (inside RP1 on a Raspberry Pi 5)
- [ ] serial over the machine's own UART, and the same hardware CI treatment as x86 in §20.8

### 20.8 Hardware CI
- [ ] one machine of each architecture: x86_64 with VMX or SVM enabled in firmware, aarch64 with FEAT_NV2 (§20.7), since Phase 21's gate runs a hypervisor inside a guest on each
- [ ] each machine netboots a built image
- [ ] serial captured automatically and results reported like any other CI job
- [ ] power control so a hung run can be recovered without a human
- [ ] a nightly run against real hardware, since QEMU-only testing hides an entire class of bug; it includes the Phase 17 `make test` running on vibeOS on each machine
- [ ] a PMU-sampled flamegraph with hardware cache-miss and branch-miss events on each machine, which §19 could not check in hosted CI

### 20.9 Stretch: legacy USB hosts
- [ ] EHCI, with UHCI and OHCI companion controllers for low- and full-speed devices, tested under QEMU (`-device usb-ehci`, `ich9-usb-uhci1`, `pci-ohci`) before any real machine

---

## Phase 21: Virtualization

**Goal.** Run other operating systems on vibeOS, and run vibeOS on vibeOS.

**Unlocks.** A hypervisor that Phase 22's CI boots test kernels in, and containers to isolate its jobs.
A conformance test for every paravirtual interface the kernel consumes as a guest.

The hypervisor gate needs hardware virtualization nested inside the test environment. Hosted CI does not
promise that, nested EL2 cannot be assumed on the dev host, and on aarch64 a guest hypervisor needs
FEAT_NV2 on the host, so this phase depends on §20.8's machines.

**Architectures.** Both: VMX and SVM on x86_64, EL2 on aarch64, behind one VM abstraction. The gate runs
on the §20.8 machine of each architecture. §21.4's CPUID detection, paravirtual clock, and paravirtual
spinlocks are x86_64 only: arm64 KVM offers no paravirtual clock or spinlock interface, because the
generic timer is already virtualized, so on aarch64 the guest uses the virtual generic timer and SMCCC
stolen time.

**Exit gate**
- [ ] a Linux kernel boots to userspace as a guest under vibeOS, on both architectures
- [ ] vibeOS boots as a guest under vibeOS, and that guest can do it again
- [ ] guests get virtio block and network devices whose sequential throughput is at least half of what the same 4-vCPU, 4 GiB guest gets under Linux KVM on the same §20.8 machine
- [ ] a container-equivalent runs an isolated process tree with its own filesystem view and resource limits
- [ ] vibeOS as a 2-vCPU, 2 GiB guest under Linux KVM, using the §21.4 paravirtual interfaces for its architecture, runs the §19.3 microbenchmarks within 20% of the same §20.8 machine booted bare metal with 2 CPUs online (§19.6 offlining)
- [ ] the harness boots a test kernel under the §21.2 VMM through its §21.2 backend, and `make test-kernel` passes that way on both architectures
- [ ] tag `v0.21.0`

### 21.1 Hypervisor
- [ ] VMX and SVM detection and enablement, with the feature MSR checks
- [ ] a VMCS or VMCB per vCPU, with guest and host state areas
- [ ] the VM entry and exit path, and exit reason decoding
- [ ] EPT or nested paging for guest physical to host physical translation
- [ ] guest interrupt injection, virtual APIC, and posted interrupts where available
- [ ] MSR and I/O bitmaps, and instruction emulation for the exits that need it
- [ ] a vCPU as a schedulable entity, so the existing scheduler runs guests
- [ ] aarch64: run as a VHE host when §11.1 recorded EL2 entry, with stage-2 translation and the virtual GIC and timer behind the same VM abstraction; when entry was at EL1, the VM layer reports that EL2 is unavailable instead of failing

### 21.2 Virtual machines
- [ ] a VM abstraction: memory regions, vCPUs, devices, lifecycle
- [ ] guest memory as an address space, with host page faults servicing guest access
- [ ] a virtual interrupt controller and timer
- [ ] virtio device backends: block, net, console, so guests need no special drivers
- [ ] a boot path for the harness's test images: SeaBIOS or OVMF (edk2 on aarch64) loaded as guest firmware, with the fw_cfg interface and the ACPI tables (x86_64) or device tree (aarch64) they hand on; direct kernel boot as well
- [ ] the machine the in-guest tests expect (DESIGN §8.2 and §8.4). On x86_64: MADT, I/O APIC, and HPET in the ACPI tables, a PIT, a 16550 at COM1, an i8042, and the `isa-debug-exit` port at `0xf4` trapped through the §21.1 I/O bitmap into the VM's exit status; HPET on or off, the CPU count, and a CPUID mask set per VM, so the PIT and LAPIC-fallback tiers run. On aarch64: the device tree, a PL011, and the §11.7 PSCI and pvpanic verdict path. Tests for devices the VMM does not model are `ktest_skip`ped with a reason on a short, reviewed list
- [ ] a `tests/harness` backend that boots test kernels under this VMM, chosen through the `VIBEOS_*` reader, used instead of §17.6's QEMU under TCG where VMX, SVM, or EL2 is available, with QEMU kept as the fallback
- [ ] a management interface and tool: create, start, stop, inspect
- [ ] a serial console per guest, which is how anything gets debugged

### 21.3 Nesting
- [ ] vibeOS on vibeOS, which mostly tests that the paravirtual interfaces are honest
- [ ] Linux as a guest, which is the real conformance test of the hypervisor
- [ ] nested virtualization, so a guest can itself be a hypervisor
- [ ] a documented performance comparison against KVM, with the gaps explained rather than hidden

### 21.4 Guest support
- [ ] x86_64: detect running under a hypervisor through CPUID leaf `0x40000000`
- [ ] x86_64: KVM paravirtual clock, so timekeeping is not calibrated against a lying TSC
- [ ] x86_64: paravirtual spinlocks, since spinning in a preempted vCPU is a disaster
- [ ] aarch64: hypervisor detection through the SMCCC vendor-hypervisor UID call, and stolen time through SMCCC `PV_TIME`; the virtualized generic timer needs no paravirtual clock
- [ ] balloon driver for memory reclaim by the host
- [ ] Hyper-V and VMware enlightenments, so it runs well on the platforms people actually have

### 21.5 Containers
- [ ] namespaces: pid, mount, network, uts, ipc, user
- [ ] cgroup-equivalent: CPU, memory, and I/O limits with accounting
- [ ] an overlay filesystem for layered images
- [ ] a container runtime: image unpack, namespace setup, process launch
- [ ] an image format, ideally one that is already standard
- [ ] a runtime tool that feels like the ones people know

---

## Phase 22: Distribution

**Goal.** Something another person can install and run, released by a process that runs on itself.

**Unlocks.** Users. The end of the roadmap and the start of the [Beyond](#beyond) list.

**Architectures.** Both. Every artifact is built, signed, and installable for each.

**Exit gate**
- [ ] a live image for each architecture boots to a graphical desktop on real hardware
- [ ] the installer partitions, installs, and produces a bootable system
- [ ] a release is built reproducibly: the same source produces byte-identical artifacts
- [ ] artifacts are signed and verified on install
- [ ] the CI that gates releases runs on vibeOS
- [ ] a fresh install can build and release the next version of vibeOS
- [ ] tag `v0.22.0`

### 22.1 Release engineering
- [ ] versioning with a defined policy for what constitutes a break
- [ ] reproducible builds extended from the §17.5 kernel and ISO to every release artifact (packages, installer images, manifests): no timestamps, no paths, no nondeterministic ordering
- [ ] a signed manifest of everything in a release, which includes §18.7's boot-chain manifest
- [ ] a release branch and backport process
- [ ] release notes generated from the changelog, which is what the changelog discipline was for

### 22.2 Installation
- [ ] a live ISO with a full desktop and an installer
- [ ] partitioning: automatic and manual, GPT with a UEFI system partition
- [ ] filesystem creation, base system install, bootloader install
- [ ] user creation, locale, timezone, network configuration
- [ ] an unattended install from a configuration file, which is how CI installs it
- [ ] upgrade in place between releases, tested from every supported prior version
- [ ] recovery: a rescue shell, `fsck` on boot, and a rollback path

### 22.3 Documentation
- [ ] an installation guide and a user handbook
- [ ] a developer guide covering the build, the test tiers, and the subsystem docs in this directory
- [ ] a hardware compatibility list from real testing
- [ ] man pages for everything shipped
- [ ] a known-issues list that includes every open box in every shipped phase, generated from this file

### 22.4 The loop
- [ ] a CI agent on vibeOS: takes a job from the forge, runs the ladder with test kernels booted under the §21.2 VMM, and reports status back
- [ ] CI running on vibeOS hardware: checkout, build, test, publish
- [ ] a release produced entirely on vibeOS, signed on vibeOS
- [ ] the resulting artifact installed on a clean machine, which then builds the next release
- [ ] the whole thing scripted and documented so it is a procedure rather than a story

Then, having proven the point, keep going: there is no version of this where the work is finished.

---

# Beyond

Not phases. No gates, no tags. Things that are hard, well specified, and welcome as soon as the phase
that enables them is closed. Each names that phase. Take one when the queue is empty, and move it into a
phase with a gate before starting it.

- **Wayland compatibility** (after 16): real applications run unmodified. §16.5 keeps the door open.
- **virtio-gpu 3D and a graphics API** (after 16 and 17): OpenGL ES over virgl or Vulkan over venus, both Mesa drivers that need §17.2's C++ standard library, then the toolkit on it.
- **A web browser port** (after 16 and 17): the single largest test of the POSIX surface, threads, and font rendering there is.
- **Apple Silicon bare metal** (after 20): m1n1 as the bootloader, the DART IOMMU, the AIC interrupt controller. Native hardware for the dev host.
- **riscv64 on a real board** (after §11.8 and 20).
- **Formal verification of one subsystem** (after 10): the buddy allocator or the vibefs commit protocol, proven rather than tested. Which one, and with what tool, is the first deliverable.
- **Deterministic simulation testing** (after 15): the network stack and both filesystems driven by a simulated clock and a seeded fault injector, every failure replayable from its seed.
- **Live kernel patching** (after 18): a fix applied to a running kernel without a reboot, with the KASLR and W^X story intact.
- **A `std`-native Rust userspace** (after 17): coreutils, shell, and init moved from the §10.5 `no_std` runtime to the §17.3 `std` target.
- **Real-time guarantees** (after 19 and 20): bounded interrupt and scheduling latency measured on hardware, and a scheduling class that documents its bound.
- **A network filesystem client** (after 15): NFS or 9p over TCP, so a cluster of vibeOS machines shares one tree.
- **NTP with clock discipline** (after 15): slewing rather than stepping, and a drift estimate. §15.8's SNTP sets the clock; this keeps it right.
- **A WASM runtime** (after 14): a sandbox that is not a process.
- **Cross self-hosting** (after 11 and 17): aarch64 vibeOS builds x86_64 vibeOS and the reverse, byte-identical to the native build.
- **vibefs v3** (after §14.8): a log-structured or journaled design measured against v2's copy-on-write metadata on the §19.3 file benchmarks, with an upgrade path from v2.

---

*Living document. Phases get reordered, split, and abandoned as the experiment finds out what is
actually hard. When that happens, edit this file rather than adding a note explaining why it is wrong.*
