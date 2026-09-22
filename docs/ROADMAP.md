# Roadmap

Where this goes. The destination is a self-hosting operating system: one that boots on real hardware,
runs a graphical userspace, has a network stack you can serve from, and can compile and test its own
source tree on itself. Written by agents.

That is absurd. Good. The interesting failures happen past the point where the tutorials stop.

## How to read this

Twenty one phases in four eras. Ordering is by dependency, not by preference: a phase's exit gate is
the thing the next phase assumes. Within a phase, parts are mostly parallelizable.

`- [ ]` and `- [x]` are the live status. Edit them in the commit that lands the work.

**Exit gate** is the definition of done. Gates are verifiable from outside the code: a marker appears
in serial output, a test target passes, a command produces the right result. "The code is written" is
not a gate. If a gate cannot be checked by running something, it is written wrong.

**Standing gates** apply to every phase and are not repeated:

- `make` builds clean with warnings denied
- `make check` green (fast local gate)
- `make test` green, all tiers, including the SMP and timer fallback variants once they exist
- CI green
- new serial markers registered in the contract in [DESIGN.md](DESIGN.md#83-end-to-end), same commit
- new portable logic has host unit tests; new hardware behavior has an in-guest test
- every fixed bug gets a regression test in the cheapest tier that catches it
- `CHANGELOG.md` entry for anything visible to someone running the kernel (≤ 2 lines, user-facing)
- tag `v0.<phase>.0` at phase exit (first published tag is `v0.8.0` for Phase 8)
- design docs updated in the same commit as any change to an invariant or a constant
- no `TODO` describing a correctness gap. Those become lines in this file.

## Non-goals

Stated so nobody spends a week on them.

- POSIX certification. Compatibility is a means to running real software, not a goal.
- Microkernel architecture. Monolithic, deliberately.
- Windows or macOS binary compatibility.
- CPU hotplug.
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
| | 10 | [Advanced memory](#phase-10-advanced-memory-management) | Demand paging, COW, mmap, swap |
| | 11 | [IPC](#phase-11-ipc-signals-and-the-posix-surface) | Pipes, sockets, futex, TTY |
| **III. Platform** | 12 | [Userspace](#phase-12-userspace) | libc, init, coreutils, real shell |
| | 13 | [Network](#phase-13-networking) | TCP/IP, sockets, DNS |
| | 14 | [Graphics](#phase-14-graphics-and-windowing) | Compositor, windows, terminal |
| | 15 | [Self-hosting](#phase-15-self-hosting-toolchain) | vibeOS compiles vibeOS |
| **IV. Frontier** | 16 | [Hardening](#phase-16-hardening) | KASLR, W^X, sandboxing, fuzzing |
| | 17 | [Performance](#phase-17-performance-and-observability) | Tracing, RCU, tickless, NUMA |
| | 18 | [Real hardware](#phase-18-real-hardware-and-portability) | Bare metal, USB, aarch64 |
| | 19 | [Virtualization](#phase-19-virtualization) | Hypervisor, containers |
| | 20 | [Distribution](#phase-20-distribution) | Installer, releases, self-hosted CI |

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
- [x] `rust-toolchain.toml`: nightly, `rust-src`, `llvm-tools`
- [x] `x86_64-unknown-none-executable.json`: `executable: true`, no PIE, static relocation model, `disable-redzone: true`, `-mmx,-sse,+soft-float`, `code-model: kernel`
- [x] no RELRO in pre-link args; it conflicts with a non-PIE static kernel
- [x] `.cargo/config.toml`: default target; `-Z build-std=core,compiler_builtins,alloc` is on the Makefile `CARGO` invocation so `tests/hostlib` does not inherit a second `core`; linker script lives in the target JSON
- [x] `Cargo.toml`: `panic = "abort"` in both profiles, `opt-level = 1` for dev
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
- [ ] a `BootInfo` struct captured once at entry; nothing else reads Limine statics  (deferred: phase 0 only queries a handful of responses inline)
- [~] each null response produces a named halt, not an unwrap panic in a function with no context  (base revision path only for now)
- [x] `limine.conf` with a single entry, serial console enabled

### 0.4 Serial and panic
- [x] COM1 16550 init: 115200 8N1, FIFO enabled, DLAB dance
- [x] polled TX with a bounded THRE wait; drop the byte at the cap rather than spinning forever
- [ ] polled RX on the data-ready bit  (input arrives in phase 5)
- [x] `fmt::Write` implementation with no allocation, usable before the heap exists
- [x] `print!` / `println!` macros routed to it
- [x] `#[panic_handler]`: re-init the port from scratch, print location and message, `cli; hlt` loop
- [x] a `panic-test` build feature or shell command so the panic path is exercised, not assumed

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
- [x] `trampoline.asm` assembled with `nasm -f bin`, included as a blob, `build.rs` anchored to `CARGO_MANIFEST_DIR` with assembler stderr captured
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

From a kernel that schedules to an operating system that runs programs against files.

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

Double buffering parked (Design ACK).

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

Printer thread parked: global IRQ-safe ring + serial try-lock sink (Design ACK).

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
- [x] interrupt affinity API, so phase 17 can rebalance without redesign

### 6.4 DMA
- [x] `DmaBuffer`: physically contiguous, known device address, explicit coherency
- [x] allocation from the buddy allocator with an alignment and boundary constraint
- [x] `sync_for_device` and `sync_for_cpu` as explicit calls even when they are no-ops on x86, because aarch64 will need them
- [x] scatter-gather list construction for devices that support it
- [x] barriers around descriptor publication, using the right fences rather than `compiler_fence` everywhere
- [x] IOMMU support deferred but the address translation kept behind an interface so it can be inserted

### 6.5 virtio
- [x] modern virtio over PCI: common configuration, notify, ISR, and device-specific capability regions
- [x] feature negotiation with the `VIRTIO_F_VERSION_1` handshake and a clear failure when features are missing
- [x] split virtqueue: descriptor table, available ring, used ring
- [x] queue setup, kick, and completion handling with the correct barriers
- [x] indirect descriptors, and `VIRTIO_F_EVENT_IDX` for interrupt suppression
- [ ] packed virtqueue as a later optimization
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

### 7.3 NVMe
- [ ] controller identify, admin queue setup, I/O queue creation per CPU
- [ ] submission and completion queue handling with doorbells and phase tags
- [ ] namespace enumeration
- [ ] read and write commands, flush, dataset management for discard
- [ ] MSI-X per queue
- [ ] the fast path built for depth from the start, since NVMe's whole point is parallelism

### 7.4 AHCI
- [ ] HBA and port initialization, command list and FIS structures
- [ ] identify device, LBA48 read and write
- [ ] ATAPI detection so a CD-ROM does not look like a broken disk
- [ ] present mainly because real hardware has it; QEMU testing via `-device ahci`

### 7.5 Partitions
- [x] MBR parsing including extended and logical partitions
- [x] GPT parsing with header and entry CRC validation and backup header fallback
- [x] partitions exposed as offset-limited block devices
- [x] type GUID recognition for the ones that matter
- [x] host tests over real table images, including truncated and CRC-broken cases

### 7.6 Cache
- [x] a page-granular cache over block devices, keyed by device and offset
- [x] read-through, write-back with an explicit flush, and dirty tracking
- [x] LRU eviction with a clock or second-chance approximation
- [x] readahead on detected sequential access
- [x] a writeback thread with a bounded dirty ratio
- [x] built so phase 10 can unify it with the page cache rather than maintaining two caches
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
- [x] `tmpfs` backed by the page cache, so it participates in eviction rather than pinning memory
- [x] `procfs`: per-process directories, `cmdline`, `status`, `maps`, `fd`
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
- [x] remainder (`read`, `open`, `close`, `lseek`, `fork`, `execve`, `wait4`, `getppid`); `stat`/`fstat`/`nanosleep`/`brk`/`mmap`/`munmap` still later
- [x] `errno` values matching Linux where a name exists, so ported software behaves
- [x] every pointer argument validated against the caller's address space before use
- [x] syscall tracing behind a flag, since the alternative is guessing why a program failed
- [x] a syscall counter per process for `procfs`

### 9.4 ELF loader
- [x] ELF64 header validation: class, endianness, machine, type
- [x] `PT_LOAD` segments mapped with permissions from the flags, honoring `p_filesz` versus `p_memsz` zero fill
- [x] `PT_GNU_STACK` respected for stack executability
- [x] `PT_INTERP` recognized, dynamic loading deferred to phase 12 but detected rather than silently ignored
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
- [x] initially a full copy; copy-on-write in phase 10, with the interface unchanged
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
- [x] user-installed handlers, masking, and queueing deferred to phase 11

### 9.8 First userspace
- [x] a minimal freestanding user program with hand-written syscall stubs and no libc, to prove the path
- [x] a userspace test runner exercising each syscall and its error cases
- [x] the shell moved out of the kernel and into a user process, keeping the kernel one only under a debug feature
- [x] the kernel's job after init becomes starting `/sbin/init` and nothing else

---

## Phase 10: Advanced Memory Management

**Goal.** Stop pretending memory is infinite and eagerly mapped. Make `fork` cheap, `mmap` real, and
overcommit survivable.

**Unlocks.** Real program startup costs. Large sparse allocations. File-backed memory.

**Exit gate**
- [ ] `fork` of a 100 MB process completes in single-digit milliseconds, measured
- [ ] `#PF` becomes a routine recoverable event; the fault counter climbs during normal operation
- [ ] `mmap` of a file, modify, `msync`, and the change is on disk
- [ ] allocate well past physical memory with swap enabled and the workload completes
- [ ] the OOM path kills a chosen process with a logged reason rather than panicking or hanging
- [ ] slab statistics show per-cache object counts, and the buddy lock is measurably less contended
- [ ] no frame leaks: a long allocation-heavy workload returns the frame count to baseline
- [ ] tag `v0.10.0`

### 10.1 Frame metadata
- [ ] a `Frame` array indexed by physical frame number, allocated at boot from a known-size region
- [ ] per-frame: refcount, flags, owner, and a list link for LRU
- [ ] `get_frame` / `put_frame` with the last reference freeing to the buddy allocator
- [ ] reverse mapping from a frame to the PTEs referencing it, which swap requires and COW makes easier
- [ ] accounting by category: kernel, user anonymous, page cache, slab, free

### 10.2 Demand paging
- [ ] a real `#PF` handler decoding the error code: present, write, user, reserved, instruction fetch
- [ ] region lookup for the faulting address, then a per-region fault handler
- [ ] anonymous regions faulting in a zero page; a shared read-only zero page until first write
- [ ] file-backed regions faulting in from the page cache
- [ ] stack regions growing down on fault, up to a limit
- [ ] a fault that resolves to no region is `SIGSEGV` for user, panic for kernel
- [ ] the fault path must be reentrant-safe: it can block on I/O, so it cannot hold the page table lock across a read

### 10.3 Copy on write
- [ ] `fork` marks every writable private mapping read-only in both address spaces and increments frame refcounts
- [ ] a write fault on a COW frame with refcount 1 just makes it writable again; with refcount above 1 it copies
- [ ] shared mappings excluded correctly, since getting this wrong silently breaks shared memory
- [ ] a TLB shootdown on the write-protect step, which is the expensive part and worth measuring
- [ ] in-guest: fork, write in the child, assert the parent's memory is unchanged; assert the frame count matches expectations at each step

### 10.4 mmap
- [ ] `mmap`, `munmap`, `mprotect`, `mremap`, `msync`, `madvise`
- [ ] anonymous private, anonymous shared, file private, file shared
- [ ] `MAP_FIXED` handling, including replacing existing mappings
- [ ] region splitting and merging on partial unmap and protect
- [ ] a VA space allocator for the user half with a bottom-up hint and a gap search
- [ ] dirty page writeback for shared file mappings
- [ ] `mlock` for pages that must not be evicted

### 10.5 Slab allocator
- [ ] object caches with a constructor, sized for a specific type
- [ ] slabs of one or more pages carved into objects, with a freelist in the unused object space
- [ ] per-CPU magazines so the common path takes no global lock
- [ ] caches for the hot types: TCB, process, inode, dentry, file, network buffer, request
- [ ] shrinking under memory pressure, driven by the same reclaim path as the page cache
- [ ] `slabinfo` in the shell: per cache, objects active and total, pages used
- [ ] the general heap stays for odd-sized allocations; slab is not a replacement for it

### 10.6 Unified page cache
- [ ] one cache serving file reads, `mmap`, and the block layer, rather than a block cache and a page cache disagreeing
- [ ] radix tree or B-tree per inode mapping offset to frame
- [ ] writeback threads with a dirty limit and per-inode ordering
- [ ] reclaim with an active and inactive LRU pair, so a single sequential scan does not evict the working set
- [ ] readahead driven by detected access patterns
- [ ] `tmpfs` pages participating, so a full `tmpfs` is reclaimable to swap rather than pinned

### 10.7 Swap
- [ ] a swap device or file with a slot allocator
- [ ] page-out: pick a victim via reclaim, write it, replace the PTE with a swap entry, free the frame
- [ ] page-in on fault from the swap entry
- [ ] reverse mapping used to find every PTE referencing a shared frame being swapped
- [ ] readahead on swap-in, since thrashing one page at a time is unusable
- [ ] a swap cache to avoid duplicate I/O for a shared page
- [ ] `swapon` / `swapoff`, with `swapoff` faulting everything back in

### 10.8 Pressure and OOM
- [ ] watermarks: low, high, min, with a background reclaim thread and direct reclaim when allocation fails
- [ ] a reserve pool for allocations that must succeed to make progress, since reclaim itself needs memory
- [ ] an OOM killer scoring by resident size and priority, logging the score table before killing
- [ ] a kernel allocation failure that cannot be resolved panics with the full memory state, rather than returning an error nobody checks
- [ ] in-guest: allocate to exhaustion and assert the system survives, with the expected process killed

---

## Phase 11: IPC, Signals, and the POSIX Surface

**Goal.** Processes that talk to each other, respond to events, and present enough of a POSIX surface
that real software can be ported without patching every call site.

**Unlocks.** Shell pipelines. Job control. Anything ported from Unix.

**Exit gate**
- [ ] `ls | grep foo | wc -l` works with correct exit statuses and no deadlock on a full pipe
- [ ] ctrl+C kills the foreground job and leaves the shell alive; ctrl+Z stops it and `fg` resumes it
- [ ] a user signal handler runs on a proper user stack and returns correctly through `sigreturn`
- [ ] `poll` on 100 descriptors wakes only for the ready ones, verified by a syscall counter
- [ ] a futex-based userspace mutex under contention across processes, correct and without spinning
- [ ] a Unix domain socket carries a passed file descriptor between processes
- [ ] tag `v0.11.0`

### 11.1 Pipes
- [ ] a bounded ring buffer with blocking read and write and correct partial-write semantics
- [ ] read end closed produces `SIGPIPE` and `EPIPE`; write end closed produces EOF
- [ ] `pipe` and `pipe2` with `O_CLOEXEC` and `O_NONBLOCK`
- [ ] named pipes through the VFS
- [ ] `PIPE_BUF` atomicity guarantee actually honored
- [ ] in-guest: fill the pipe, assert the writer blocks, drain, assert it proceeds

### 11.2 Unix domain sockets
- [ ] `socket`, `bind`, `listen`, `accept`, `connect`, `send`, `recv`, `shutdown` for `AF_UNIX`
- [ ] stream and datagram types
- [ ] filesystem-bound and abstract namespaces
- [ ] `SCM_RIGHTS` file descriptor passing, and `SCM_CREDENTIALS`
- [ ] socketpair
- [ ] the same buffering machinery reused for network sockets in phase 13, decided now rather than duplicated later

### 11.3 Shared memory
- [ ] POSIX `shm_open` backed by `tmpfs`
- [ ] anonymous shared mappings inherited across `fork`
- [ ] `memfd`-equivalent for anonymous named regions
- [ ] correct refcounting so a region survives until the last mapper unmaps

### 11.4 futex
- [ ] `FUTEX_WAIT` and `FUTEX_WAKE` on a user address, with a hash table of wait queues keyed by physical address so it works across processes
- [ ] requeue and `WAKE_OP` for condition variables
- [ ] priority inheritance deferred, but the interface not precluding it
- [ ] a timeout on every wait
- [ ] this is the primitive userspace threading stands on; correctness matters more than speed here

### 11.5 Event notification
- [ ] `poll` and `ppoll`
- [ ] `select` for compatibility
- [ ] an `epoll`-equivalent with edge and level triggering, since `poll` is O(n) per call and that becomes the bottleneck
- [ ] `eventfd`, `signalfd`, `timerfd`
- [ ] one internal readiness and wait-queue mechanism underneath all of them
- [ ] in-guest: a thousand descriptors with a handful ready, asserting the wakeup count

### 11.6 TTY
- [ ] a TTY layer between the console and processes, with a line discipline
- [ ] canonical mode: line buffering, erase, kill, EOF
- [ ] raw mode, and the `termios` interface to switch between them
- [ ] control character handling generating signals: ctrl+C, ctrl+Z, ctrl+\
- [ ] sessions, process groups, controlling terminal, foreground group
- [ ] `SIGTTIN` and `SIGTTOU` for background access
- [ ] pseudo-terminals, which a terminal emulator in phase 14 requires
- [ ] window size and `SIGWINCH`

### 11.7 Full signals
- [ ] the full signal set with correct default actions
- [ ] `sigaction` with `SA_RESTART`, `SA_SIGINFO`, and an alternate stack
- [ ] `sigprocmask`, `sigpending`, `sigsuspend`, `sigwaitinfo`
- [ ] delivery on the return to userspace: build a frame on the user stack, run the handler, return through `sigreturn`
- [ ] real-time signals with queueing, since standard signals coalesce and that surprises people
- [ ] interaction with blocking syscalls: interrupt with `EINTR` or restart, per `SA_RESTART`
- [ ] per-thread signal masks with process-directed signals delivered to an eligible thread

### 11.8 POSIX floor
- [ ] a tracked list of the syscalls needed to build and run the target software set, checked off as implemented
- [ ] `getcwd`, `chdir`, `fchdir`, `access`, `chmod`, `chown`, `umask`, `utimensat`
- [ ] `getrlimit` / `setrlimit`, `getrusage`
- [ ] `uname`, `sysinfo`, `gettimeofday`, `clock_gettime`, `clock_nanosleep`
- [ ] `ioctl` with a registry rather than a growing match arm
- [ ] `prctl` for the few things that need it
- [ ] `ENOSYS` for the unimplemented, logged once per syscall number, so a port's failure is immediately legible

---

# Era III. Platform

Where it stops being a kernel demo and becomes something you can use. Nothing here has a canonical
right answer, which is the interesting part.

## Phase 12: Userspace

**Goal.** A real userspace: a C library, a dynamic linker, an init system, and enough utilities that
the shell is useful.

**Unlocks.** Porting software instead of writing everything.

**Exit gate**
- [ ] a C program compiled on the host against our libc runs correctly
- [ ] a dynamically linked binary against a shared libc runs, and `ldd`-equivalent lists its dependencies
- [ ] init starts services from configuration, restarts a crashed one, and reaps orphans
- [ ] a shell script with pipes, redirection, variables, conditionals, and loops runs
- [ ] the userspace test suite passes, run automatically in CI inside the VM
- [ ] a package installs, upgrades, and removes cleanly with file conflict detection
- [ ] tag `v0.12.0`

### 12.1 libc
- [ ] decide and document: write our own, or port musl or relibc. Porting gets to real software faster; writing our own is more of what this project is for. Lean toward our own for the core and port where the surface is enormous and uninteresting.
- [ ] `crt0`: entry, stack argument extraction, TLS setup, `__libc_start_main`, `atexit`
- [ ] syscall stubs generated from one table shared with the kernel, so they cannot drift
- [ ] `malloc`: a real allocator, not a bump. Size classes, thread caches, `mmap` for large allocations.
- [ ] stdio with buffering modes, `printf` family with the full format surface, `scanf`
- [ ] string, memory, `ctype`, `stdlib` conversions
- [ ] math, which is large and where borrowing from an existing implementation is clearly correct
- [ ] `pthreads` over kernel threads and futexes: create, join, mutex, condvar, rwlock, TLS, cancellation
- [ ] `setjmp`/`longjmp`, `dlopen` family
- [ ] locale enough to not break, not more
- [ ] a conformance test suite, and the honesty to record what is deliberately unimplemented

### 12.2 Dynamic linking
- [ ] shared object loading: `PT_DYNAMIC`, `DT_NEEDED`, search paths
- [ ] relocation processing: `RELA`, `JMPREL`, `RELR`
- [ ] symbol resolution with correct scope and interposition order
- [ ] lazy binding through the PLT and GOT, with a `BIND_NOW` mode
- [ ] TLS: initial-exec and dynamic models, `__tls_get_addr`
- [ ] `dlopen`, `dlsym`, `dlclose`, `dladdr`
- [ ] `LD_PRELOAD` and `LD_LIBRARY_PATH`, useful for debugging more than for anything else
- [ ] the linker itself is static and self-relocating, which is the fiddly part

### 12.3 init and services
- [ ] PID 1: mount the base filesystems, start services, reap orphans, handle shutdown
- [ ] a declarative service definition: dependencies, restart policy, environment, working directory
- [ ] dependency-ordered parallel startup
- [ ] service supervision with restart backoff
- [ ] log collection from service stdout and stderr into the system log
- [ ] socket activation, which is genuinely elegant and not much work once sockets exist
- [ ] shutdown: signal services, wait with a timeout, unmount, ACPI power off
- [ ] a control tool for start, stop, restart, status, and logs

### 12.4 Coreutils
- [ ] file and directory: `ls`, `cp`, `mv`, `rm`, `mkdir`, `rmdir`, `ln`, `touch`, `stat`, `find`, `du`, `df`
- [ ] text: `cat`, `head`, `tail`, `wc`, `sort`, `uniq`, `cut`, `tr`, `grep`, `sed`, `diff`
- [ ] process: `ps`, `kill`, `top`, `time`, `nice`
- [ ] system: `uname`, `date`, `uptime`, `free`, `mount`, `dmesg`, `env`, `id`, `hostname`
- [ ] archive: `tar`, `gzip`
- [ ] editor: something small, `vi`-flavored. Editing on the machine matters more than it sounds like.
- [ ] each with real argument parsing and correct exit statuses, because scripts depend on both

### 12.5 Shell
- [ ] a POSIX-shaped shell: word splitting, quoting, expansion, globbing
- [ ] redirection including here-documents and file descriptor manipulation
- [ ] pipelines, `&&`, `||`, `;`, subshells, command substitution
- [ ] variables, environment, `export`, arithmetic expansion
- [ ] control flow: `if`, `while`, `for`, `case`, functions
- [ ] job control: background, `fg`, `bg`, `jobs`, `wait`
- [ ] interactive: history, completion, line editing, prompt expansion
- [ ] scripts with a shebang, and enough correctness to run a build script

### 12.6 Packaging
- [ ] a package format: metadata, dependencies, file list, checksums, install scripts
- [ ] a local package database with installed files and owners
- [ ] install, remove, upgrade, query, verify, with file conflict detection
- [ ] dependency resolution, and a clear error rather than a partial install
- [ ] a build recipe format and a tool that produces packages reproducibly
- [ ] a repository format, fetched over HTTP once networking exists, signed
- [ ] the base system itself shipped as packages, which is the test that the format is real

---

## Phase 13: Networking

**Goal.** A TCP/IP stack good enough to serve requests and fetch a package repository.

**Unlocks.** Remote access, package distribution, and the largest available source of well-specified
protocol work for an agent to get wrong in interesting ways.

**Exit gate**
- [ ] `ping` from the host to the guest and back
- [ ] DHCP acquires an address, and DNS resolves a name
- [ ] a TCP server in the guest serves a file to `curl` on the host, and the bytes match
- [ ] a TCP client fetches from the host through a large transfer with no corruption and reasonable throughput
- [ ] the stack survives a packet fuzzer: malformed headers, bad checksums, overlapping fragments, no panics
- [ ] `netstat`-equivalent shows sockets in correct states through a full connection lifecycle
- [ ] host tests for header parsing, checksums, TCP state transitions, and sequence arithmetic
- [ ] tag `v0.13.0`

### 13.1 netdev layer
- [ ] a `NetDevice` trait: transmit, MTU, MAC, link state, and statistics
- [ ] receive queues delivering into the stack from a softirq or a dedicated thread, never from the hard IRQ
- [ ] a packet buffer type with headroom and tailroom so headers can be prepended without copying
- [ ] checksum and segmentation offload flags, used when the device supports them
- [ ] a loopback device, which is also the easiest way to test everything above it
- [ ] per-device statistics: packets, bytes, errors, drops

### 13.2 Drivers
- [ ] virtio-net: receive and transmit virtqueues, mergeable receive buffers, checksum offload, multi-queue
- [ ] e1000, for real hardware and because it is well documented
- [ ] a Realtek 8168-family driver, since it is what cheap hardware actually has
- [ ] in-guest driver tests against a loopback QEMU network configuration

### 13.3 Link layer
- [ ] Ethernet framing, parsing, and dispatch by EtherType
- [ ] ARP with a cache, timeouts, request queueing for unresolved destinations, and gratuitous ARP handling
- [ ] VLAN tagging, cheap to add and annoying to retrofit
- [ ] neighbor discovery for IPv6 later, with ARP structured so it is not a special case

### 13.4 IP
- [ ] IPv4 header parse, validate, and construct, with checksum
- [ ] routing table with longest-prefix match, a default route, and per-route MTU
- [ ] fragmentation and reassembly, with a reassembly timeout and a bound on held fragments so it is not a memory attack
- [ ] ICMP: echo, destination unreachable, time exceeded, and correct generation on error
- [ ] TTL handling and forwarding, which makes it a router with very little extra work
- [ ] the header code in the library half, host-tested against captured packets

### 13.5 UDP
- [ ] datagram send and receive with port binding and demultiplexing
- [ ] checksum computation and validation, including the optional-zero case
- [ ] receive queue per socket with a bound and a drop counter
- [ ] connected UDP sockets
- [ ] enough to run DNS and DHCP, which is what unblocks everything else

### 13.6 TCP
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

### 13.7 Socket API
- [ ] `socket`, `bind`, `listen`, `accept`, `connect`, `send`, `recv`, `sendto`, `recvfrom`, `shutdown`
- [ ] `getsockopt` and `setsockopt` for the options that exist, and a clear error for those that do not
- [ ] `getsockname`, `getpeername`
- [ ] non-blocking mode integrated with `poll` and `epoll`
- [ ] `sendmsg` and `recvmsg` with scatter-gather and control messages
- [ ] `accept4`, `SO_REUSEADDR`, `SO_REUSEPORT`

### 13.8 Configuration and tools
- [ ] DHCP client: discover, request, lease renewal, and correct behavior on lease expiry
- [ ] DNS resolver: A, AAAA, CNAME, with a cache honoring TTLs, retries, and multiple servers
- [ ] static configuration files and an `ip`-equivalent tool
- [ ] `ping`, `traceroute`, `netstat`, `ss`, `tcpdump`-equivalent
- [ ] an HTTP client good enough to fetch packages, and a server good enough to prove the stack
- [ ] `sshd`, eventually, which requires the crypto from phase 16 and is the point at which the machine becomes genuinely usable remotely

### 13.9 IPv6
- [ ] addressing, header parsing, extension headers
- [ ] neighbor discovery and stateless address autoconfiguration
- [ ] ICMPv6
- [ ] dual stack sockets
- [ ] deliberately after IPv4 works, and deliberately not skipped

### 13.10 Testing
- [ ] a packet injection interface so the stack can be tested without a real device
- [ ] replay of captured traffic as host tests
- [ ] a fuzzer over every parser, run in CI
- [ ] throughput and latency benchmarks with regression thresholds
- [ ] host-to-guest integration tests in CI over QEMU user networking and a tap device
- [ ] a deliberately hostile peer: reordering, duplication, loss, tiny windows

---

## Phase 14: Graphics and Windowing

**Goal.** More than one window. A compositor, an input stack, and a terminal emulator running in it.

**Unlocks.** The thing that makes people believe it is an operating system.

**Exit gate**
- [ ] multiple windows, movable and resizable, with correct overlap and damage handling
- [ ] the terminal emulator runs the shell through a pty, with correct escape sequence handling
- [ ] mouse and keyboard events routed to the focused window
- [ ] a screenshot captured programmatically and compared against a reference in CI
- [ ] a resolution change at runtime with clients reacting correctly
- [ ] the compositor holds a steady frame rate under a moving-window workload, measured
- [ ] tag `v0.14.0`

### 14.1 Display abstraction
- [ ] a `Display` with a mode list, current mode, and framebuffer access
- [ ] the Limine framebuffer as the fallback, always available
- [ ] mode setting where the hardware supports it
- [ ] multiple outputs with positions, because a second monitor should not be a rewrite
- [ ] vsync and page flipping, so tearing is fixable rather than inherent
- [ ] a pixel format abstraction that is not hardcoded to BGRX

### 14.2 GPU
- [ ] virtio-gpu 2D: resource creation, transfer, `set_scanout`, flush
- [ ] virtio-gpu cursor plane, which is worth it for the latency alone
- [ ] EDID for mode discovery
- [ ] virtio-gpu 3D with a graphics API on top, as a much later stretch
- [ ] a software rasterizer good enough that 3D is optional: lines, rects, blits, alpha, text

### 14.3 Compositor
- [ ] a surface tree with position, size, stacking order, and transforms
- [ ] damage tracking so only changed regions are recomposited
- [ ] alpha blending and clipping
- [ ] double or triple buffering with a frame callback protocol so clients throttle to the display
- [ ] a hardware cursor when available, software otherwise
- [ ] fullscreen bypass, sending a client's buffer straight to scanout
- [ ] the frame loop must not depend on client responsiveness; a hung client freezing the desktop is the failure mode to design against

### 14.4 Input
- [ ] an input event abstraction: keyboard, pointer, and touch, with timestamps
- [ ] PS/2 mouse, then USB HID once phase 18 lands xHCI
- [ ] event routing: pointer to the surface under the cursor, keyboard to the focused surface
- [ ] focus policy, grabs, and click-to-focus
- [ ] keyboard layout handling with dead keys and compose, which is more work than it looks
- [ ] pointer acceleration, and scroll with kinetic behavior
- [ ] repeat rate and delay

### 14.5 Display protocol
- [ ] a client-server protocol over a Unix socket, with buffer sharing through shared memory or dma-buf-equivalent
- [ ] surface lifecycle, commit semantics, and damage submission
- [ ] input event delivery
- [ ] window management: title, class, minimum and maximum size, state
- [ ] clipboard and drag and drop
- [ ] the protocol documented in `docs/` before implementation, since two implementations must agree
- [ ] Wayland compatibility as a stretch, which would let real applications run

### 14.6 Window management and toolkit
- [ ] window decorations, or a client-side decoration protocol
- [ ] move, resize, minimize, maximize, close
- [ ] tiling and floating layouts, workspaces
- [ ] a keyboard-driven window switcher
- [ ] a widget toolkit: layout, buttons, text entry, lists, scrolling
- [ ] font rendering with real glyph rasterization, hinting, kerning, and subpixel positioning, which is a project in itself
- [ ] a panel with a clock, a launcher, and a system tray

### 14.7 Terminal emulator
- [ ] a pty client with correct `termios` interaction
- [ ] VT100 and xterm escape sequence handling: cursor, colors, attributes, scroll regions, alternate screen
- [ ] 256-color and true color
- [ ] a scrollback buffer with selection and clipboard
- [ ] UTF-8, wide characters, and combining marks
- [ ] resize with `SIGWINCH`
- [ ] this is the single most important application on the system; it gets the same care as the kernel

---

## Phase 15: Self-hosting Toolchain

**Goal.** vibeOS compiles vibeOS. Check out the source on the machine, build it, boot the result.

**Unlocks.** The claim. Also a genuinely brutal test of the POSIX surface, since compilers exercise
everything.

**Exit gate**
- [ ] a C compiler runs on vibeOS and produces working binaries
- [ ] `rustc` runs on vibeOS
- [ ] the vibeOS source tree builds on vibeOS into a bootable ISO
- [ ] that ISO boots and rebuilds itself: two generations, with matching output
- [ ] the test suite runs on vibeOS, on hardware, reporting results the same way CI does
- [ ] the whole loop is scripted, not a sequence of manual steps someone remembers
- [ ] tag `v0.15.0`

### 15.1 POSIX completeness
- [ ] audit against what a real toolchain needs, and close the gaps rather than guessing
- [ ] filesystem behavior compilers depend on: `rename` atomicity, `O_TMPFILE`, `fsync` semantics, correct `mtime`
- [ ] process behavior: `posix_spawn`, large environments, long argument lists, pipe-heavy pipelines
- [ ] memory behavior: large `mmap`, `mprotect` for JIT, hundreds of thousands of small allocations
- [ ] `/proc` entries that build systems read
- [ ] a large-file test, since compilers write big object files and every off-by-one shows up there

### 15.2 C toolchain
- [ ] port `tcc` first: small, self-hosting, and a fast way to prove the environment works
- [ ] an assembler and linker, either ported or written, with ELF output and relocation support
- [ ] `ar`, `nm`, `objdump`, `strip`, `readelf`
- [ ] then clang and LLVM, which is a large port and mostly a matter of the C++ standard library and filesystem behavior
- [ ] `make`, and enough of `sh` and `awk` for configure scripts to survive
- [ ] `cmake` and `ninja`, which most real projects assume

### 15.3 Rust toolchain
- [ ] a `std` port: a new target triple with the `sys` layer implemented over our syscalls
- [ ] thread, file, socket, process, and time support in `std`
- [ ] `rustc` running on vibeOS, which requires LLVM working first
- [ ] `cargo`, which requires networking, TLS, and git
- [ ] a cross-compiled bootstrap first, then a native build, in that order
- [ ] the kernel's own `build-std` requirement met on-device, which is the actual goal

### 15.4 Development environment
- [ ] a git implementation or port, enough for clone, commit, branch, and push
- [ ] a real editor, ported rather than written
- [ ] a debugger: ptrace-equivalent, breakpoints, single stepping, symbol and DWARF reading
- [ ] a profiler using the perf counters from phase 17
- [ ] `strace`-equivalent, which the kernel syscall tracing already mostly provides

### 15.5 The loop
- [ ] a build script that goes from a clean checkout to a bootable ISO on vibeOS
- [ ] a second-generation build, comparing the two ISOs and explaining any difference
- [ ] the test suite running on-device
- [ ] timings recorded, because "it works" and "it works in under an hour" are different claims
- [ ] documented in `docs/` as a reproducible procedure, since the point is that someone else can do it

---

# Era IV. Frontier

The parts that separate a working system from a serious one.

## Phase 16: Hardening

**Goal.** Assume everything in userspace is hostile and the kernel has bugs. Make both survivable.

**Exit gate**
- [ ] kernel `.text` read-only and executable, `.rodata` read-only and NX, `.data` NX, verified at runtime
- [ ] KASLR active, and a crash dump still symbolizes correctly
- [ ] SMEP and SMAP enabled, with an in-guest test proving a kernel access to a user pointer faults outside the explicit accessor
- [ ] a syscall fuzzer runs for hours without a kernel panic
- [ ] KASAN builds pass the full test suite
- [ ] a sandboxed process cannot reach the filesystem or network outside its policy, tested
- [ ] a documented threat model, with the deliberate gaps named
- [ ] tag `v0.16.0`

### 16.1 Kernel memory protection
- [ ] per-section permissions applied after boot, with the init sections freed or made NX
- [ ] no writable-and-executable kernel mapping anywhere, asserted by a page table audit at boot
- [ ] the physmap NX, which is easy to get wrong and a straightforward escalation primitive
- [ ] guard pages on every kernel stack including the IST stacks
- [ ] a page table walker that verifies the whole address space against a policy, run as an in-guest test

### 16.2 KASLR
- [ ] a random kernel base at boot, from a real entropy source
- [ ] relocation processing for the chosen base
- [ ] randomized physmap and KVA region bases
- [ ] the offset recorded so a crash dump symbolizes, and not otherwise exposed
- [ ] user address space randomization: stack, heap, and mmap bases

### 16.3 CPU features
- [ ] SMEP, so the kernel cannot execute user pages
- [ ] SMAP, with explicit `stac`/`clac` in the user access accessors and nowhere else
- [ ] UMIP, so userspace cannot read descriptor table registers
- [ ] `CR4.FSGSBASE` handled correctly with respect to `swapgs`
- [ ] CET shadow stacks and indirect branch tracking, as a stretch
- [ ] speculation mitigations, measured before enabling, since some cost more than the risk

### 16.4 Stack and memory safety
- [ ] stack canaries in both kernel and userspace
- [ ] `FORTIFY`-equivalent checks in libc
- [ ] a hardened `malloc`: guard pages, delayed reuse, randomized placement, double free detection
- [ ] a KASAN build with a shadow map, red zones, and quarantined frees
- [ ] a UBSAN build
- [ ] `unsafe` blocks audited and each given a documented invariant, since a growing pile of unaudited `unsafe` is where this ends up otherwise

### 16.5 Fuzzing
- [ ] a syscall fuzzer generating structured calls with valid and invalid arguments, running in CI
- [ ] filesystem image fuzzing against every filesystem parser
- [ ] network packet fuzzing against every protocol parser
- [ ] ELF fuzzing against the loader
- [ ] coverage-guided where feasible, with crash reproduction as a checked-in test case
- [ ] every crash found becomes a regression test, without exception

### 16.6 Access control
- [ ] uid and gid actually enforced: file permissions, ownership, `setuid`
- [ ] capability-style privilege splitting rather than a single root bit
- [ ] a syscall filter, `seccomp`-shaped, per process
- [ ] mount namespaces and a `pivot_root`-equivalent
- [ ] resource limits enforced: memory, descriptors, processes, CPU time
- [ ] an audit log for privileged operations

### 16.7 Crypto and boot integrity
- [ ] an entropy pool: `RDRAND`, timer jitter, interrupt timing, with health checks
- [ ] a CSPRNG, `/dev/random` and `/dev/urandom`, and `getrandom`
- [ ] hashes and ciphers: SHA-2, SHA-3, AES-GCM, ChaCha20-Poly1305
- [ ] public key: Ed25519, X25519, RSA verification
- [ ] TLS in userspace, needed for package fetching and `sshd`
- [ ] UEFI secure boot: a signed bootloader and kernel
- [ ] measured boot with a TPM, and signature verification on module and package loading
- [ ] full disk encryption, which needs the block layer to support a transform

### 16.8 Threat model
- [ ] documented in `docs/`: what is trusted, what is not, what is deliberately out of scope
- [ ] the escalation paths that are known to exist and why they are still open
- [ ] a security response process, however informal, so the answer is not improvised

---

## Phase 17: Performance and Observability

**Goal.** Know why it is slow, then stop being slow. Neither is possible without measurement first.

**Exit gate**
- [ ] a flamegraph produced from a sampling profile on the machine
- [ ] tracing captures a full request path across syscall, scheduler, and driver with correlated timestamps
- [ ] benchmarks in CI with thresholds, and a regression actually fails a build
- [ ] scheduler latency under load within a stated bound, measured not asserted
- [ ] a tickless idle CPU takes near zero timer interrupts, measured
- [ ] lock contention profiled, and the top contended lock addressed rather than noted
- [ ] tag `v0.17.0`

### 17.1 Tracing
- [ ] static tracepoints at the boundaries that matter: syscall entry and exit, scheduler switch, page fault, IRQ, block and network I/O
- [ ] a per-CPU lock-free ring buffer with a fixed-size record
- [ ] dynamic enable and disable per tracepoint, with near-zero cost when off
- [ ] an export format a host tool can read, ideally one an existing viewer already understands
- [ ] a userspace tracing interface so applications can emit into the same timeline
- [ ] the timestamp source must be globally monotonic, which is the phase 2 TSC work finally paying off

### 17.2 Profiling
- [ ] PMU setup: cycles, instructions, cache misses, branch misses
- [ ] sampling on a counter overflow interrupt with a stack walk
- [ ] per-process and per-thread accounting
- [ ] a flamegraph pipeline from samples to output
- [ ] `perf`-equivalent for stat and record
- [ ] last branch records and precise event sampling where the hardware offers them

### 17.3 Benchmarks
- [ ] microbenchmarks: syscall latency, context switch, page fault, allocation, lock acquire
- [ ] subsystem: file read and write throughput, network throughput and latency, process creation rate
- [ ] macro: kernel build time, boot time, a mixed interactive workload
- [ ] all of it in CI with recorded history and a threshold that fails
- [ ] variance controlled well enough that the numbers mean something, which is most of the work

### 17.4 Scheduler
- [ ] replace round-robin with something latency-aware: weighted fair queueing or a virtual-deadline scheme
- [ ] priorities and nice values with real effect
- [ ] a real-time class with `FIFO` and `RR` policies
- [ ] load balancing: periodic first, then work stealing, measured against the periodic baseline before keeping it
- [ ] topology awareness from CPUID: prefer a sibling core, keep a thread near its cache
- [ ] cgroup-style group scheduling with CPU shares and quotas
- [ ] latency measured under load, since the whole point is the tail and not the average

### 17.5 Scalability
- [ ] RCU for read-mostly structures: the dentry cache, the routing table, the module list
- [ ] seqlocks where readers dominate and writers are rare
- [ ] per-CPU counters aggregated on read, instead of a shared atomic on the hot path
- [ ] lock-free queues on the paths that need them, with a documented memory ordering argument
- [ ] the global locks split by hash or by CPU where profiling says it matters
- [ ] contention measured before and after each change, with the numbers recorded

### 17.6 Power and idle
- [ ] tickless idle: arm the next real deadline instead of a periodic tick
- [ ] `mwait` and C-state entry with a governor choosing depth by predicted idle duration
- [ ] P-state and frequency scaling
- [ ] interrupt coalescing on the network and storage paths
- [ ] timer slack, so unrelated wakeups can batch
- [ ] measured idle power on real hardware, which is the only honest test

### 17.7 NUMA
- [ ] SRAT and SLIT parsing for node topology and distances
- [ ] per-node buddy allocators with node-local allocation as the default
- [ ] NUMA-aware scheduling, keeping a thread near its memory
- [ ] page migration on persistent remote access
- [ ] per-node statistics, because the failure mode is invisible without them

### 17.8 I/O
- [ ] an async submission interface, `io_uring`-shaped: submission and completion rings shared with userspace
- [ ] zero-copy paths for network send and file read
- [ ] `sendfile` and `splice`
- [ ] direct I/O bypassing the page cache
- [ ] polled I/O for NVMe, where the interrupt costs more than the spin
- [ ] boot time reduced by parallelizing device probing and deferring what can be deferred

---

## Phase 18: Real Hardware and Portability

**Goal.** Boot on a physical machine. Then boot on a machine that is not x86.

**Exit gate**
- [ ] boots from USB on at least two physically different x86_64 machines, with output on a serial adapter or the screen
- [ ] real disk, real NIC, real USB keyboard, all functional
- [ ] clean shutdown and reboot through ACPI
- [ ] suspend to RAM and resume, with devices restored
- [ ] an aarch64 target boots to a shell under QEMU
- [ ] hardware CI: a physical machine that netboots and reports results automatically
- [ ] tag `v0.18.0`

### 18.1 Bare metal x86_64
- [ ] a real UEFI boot path and a real BIOS boot path, both tested on hardware rather than assumed
- [ ] the memory map from real firmware, which is messier than QEMU's in ways that break assumptions
- [ ] real ACPI tables, which are also messier, including vendor quirks
- [ ] serial output over a USB adapter, and early output on the framebuffer for machines without one
- [ ] a crash dump written somewhere persistent, since there is no host to catch it
- [ ] a hardware compatibility list, honestly maintained

### 18.2 ACPI runtime
- [ ] an AML interpreter, which is a large and genuinely unpleasant subproject and unavoidable
- [ ] the device tree from the DSDT and SSDTs, resource assignment, `_CRS` parsing
- [ ] power management: S5 shutdown, S3 suspend, S4 hibernate
- [ ] thermal zones and fan control
- [ ] battery and AC adapter status
- [ ] hotplug notifications, lid switch, power button
- [ ] `_OSI` handling and the vendor quirks that come with it

### 18.3 USB
- [ ] xHCI: controller init, command and event rings, device slots, endpoint contexts
- [ ] enumeration: address assignment, descriptor parsing, configuration selection
- [ ] hub support, including nested hubs
- [ ] HID: keyboard and mouse boot protocol, then report descriptor parsing
- [ ] mass storage over bulk-only transport
- [ ] USB serial, which is how debugging a modern laptop works
- [ ] EHCI and UHCI for older hardware, if the hardware in hand needs it

### 18.4 Real devices
- [ ] AHCI and NVMe validated on real drives, with real error handling
- [ ] SMART reporting
- [ ] real NICs: e1000e, igb, Realtek, and the firmware loading some of them require
- [ ] Intel HDA audio, so there is sound
- [ ] SD and eMMC for single-board machines
- [ ] i2c and SMBus, needed for sensors and embedded controllers

### 18.5 Architecture abstraction
- [ ] audit every architecture assumption and move it behind a trait or a module boundary
- [ ] abstract: page table format, context switch, interrupt controller, timer, atomics and barriers, MMIO accessors, per-CPU access, syscall entry
- [ ] keep x86_64 working at every step; a refactor that breaks the working port to enable a hypothetical one is a bad trade
- [ ] a documented list of what is architecture-specific and where it lives

### 18.6 aarch64
- [ ] a boot path: UEFI or device tree, exception level setup, MMU enable
- [ ] page tables: 4KB granule, TTBR0 and TTBR1 split, which maps cleanly onto the existing user and kernel split
- [ ] GIC v2 and v3 for interrupts
- [ ] the generic timer
- [ ] PSCI for secondary core bring-up, which is far more civilized than INIT/SIPI
- [ ] device tree parsing for hardware discovery
- [ ] a virt machine target under QEMU first, then a real board
- [ ] cache maintenance and barriers, which x86 let us be sloppy about and aarch64 will not

### 18.7 riscv64
- [ ] a stretch, and mostly a test of whether the abstraction from 18.5 was real
- [ ] SBI, PLIC and CLINT, Sv39 and Sv48 paging
- [ ] QEMU virt, then a real board

### 18.8 Hardware CI
- [ ] a physical machine that netboots a built image
- [ ] serial captured automatically and results reported like any other CI job
- [ ] power control so a hung run can be recovered without a human
- [ ] a nightly run against real hardware, since QEMU-only testing hides an entire class of bug

---

## Phase 19: Virtualization

**Goal.** Run other operating systems on vibeOS, and run vibeOS on vibeOS.

**Exit gate**
- [ ] a Linux kernel boots to userspace as a guest under vibeOS
- [ ] vibeOS boots as a guest under vibeOS, and that guest can do it again
- [ ] guests get virtio block and network devices with reasonable throughput
- [ ] a container-equivalent runs an isolated process tree with its own filesystem view and resource limits
- [ ] vibeOS as a guest under Linux KVM performs comparably to native, using paravirtual interfaces
- [ ] tag `v0.19.0`

### 19.1 Hypervisor
- [ ] VMX and SVM detection and enablement, with the feature MSR checks
- [ ] a VMCS or VMCB per vCPU, with guest and host state areas
- [ ] the VM entry and exit path, and exit reason decoding
- [ ] EPT or nested paging for guest physical to host physical translation
- [ ] guest interrupt injection, virtual APIC, and posted interrupts where available
- [ ] MSR and I/O bitmaps, and instruction emulation for the exits that need it
- [ ] a vCPU as a schedulable entity, so the existing scheduler runs guests

### 19.2 Virtual machines
- [ ] a VM abstraction: memory regions, vCPUs, devices, lifecycle
- [ ] guest memory as an address space, with host page faults servicing guest access
- [ ] a virtual interrupt controller and timer
- [ ] virtio device backends: block, net, console, so guests need no special drivers
- [ ] firmware loading, or direct kernel boot
- [ ] a management interface and tool: create, start, stop, inspect
- [ ] a serial console per guest, which is how anything gets debugged

### 19.3 Nesting
- [ ] vibeOS on vibeOS, which mostly tests that the paravirtual interfaces are honest
- [ ] Linux as a guest, which is the real conformance test of the hypervisor
- [ ] nested virtualization, so a guest can itself be a hypervisor
- [ ] a documented performance comparison against KVM, with the gaps explained rather than hidden

### 19.4 Guest support
- [ ] detect running under a hypervisor via CPUID
- [ ] KVM paravirtual clock, so timekeeping is not calibrated against a lying TSC
- [ ] paravirtual spinlocks, since spinning in a preempted vCPU is a disaster
- [ ] balloon driver for memory reclaim by the host
- [ ] Hyper-V and VMware enlightenments, so it runs well on the platforms people actually have

### 19.5 Containers
- [ ] namespaces: pid, mount, network, uts, ipc, user
- [ ] cgroup-equivalent: CPU, memory, and I/O limits with accounting
- [ ] an overlay filesystem for layered images
- [ ] a container runtime: image unpack, namespace setup, process launch
- [ ] an image format, ideally one that is already standard
- [ ] a runtime tool that feels like the ones people know

---

## Phase 20: Distribution

**Goal.** Something another person can install and run, released by a process that runs on itself.

**Exit gate**
- [ ] a live ISO boots to a graphical desktop on real hardware
- [ ] the installer partitions, installs, and produces a bootable system
- [ ] a release is built reproducibly: the same source produces byte-identical artifacts
- [ ] artifacts are signed and verified on install
- [ ] the CI that gates releases runs on vibeOS
- [ ] a fresh install can build and release the next version of vibeOS
- [ ] tag `v0.20.0`

### 20.1 Release engineering
- [ ] versioning with a defined policy for what constitutes a break
- [ ] reproducible builds: no timestamps, no paths, no nondeterministic ordering in any artifact
- [ ] a signed manifest of everything in a release
- [ ] a release branch and backport process
- [ ] release notes generated from the changelog, which is what the changelog discipline was for

### 20.2 Installation
- [ ] a live ISO with a full desktop and an installer
- [ ] partitioning: automatic and manual, GPT with a UEFI system partition
- [ ] filesystem creation, base system install, bootloader install
- [ ] user creation, locale, timezone, network configuration
- [ ] an unattended install from a configuration file, which is how CI installs it
- [ ] upgrade in place between releases, tested from every supported prior version
- [ ] recovery: a rescue shell, `fsck` on boot, and a rollback path

### 20.3 Documentation
- [ ] an installation guide and a user handbook
- [ ] a developer guide covering the build, the test tiers, and the subsystem docs in this directory
- [ ] a hardware compatibility list from real testing
- [ ] man pages for everything shipped
- [ ] an honest known-issues list

### 20.4 The loop
- [ ] CI running on vibeOS hardware: checkout, build, test, publish
- [ ] a release produced entirely on vibeOS, signed on vibeOS
- [ ] the resulting artifact installed on a clean machine, which then builds the next release
- [ ] the whole thing scripted and documented so it is a procedure rather than a story
- [ ] and then, having proven the point, keep going, because there is no version of this where the work is finished

---

*Living document. Phases get reordered, split, and abandoned as the experiment finds out what is
actually hard. When that happens, edit this file rather than adding a note explaining why it is wrong.*
