# Design

vibeOS is a monolithic x86_64 kernel in Rust. `no_std`, `alloc` enabled once the heap is up. Limine
boots the ELF in long mode; the kernel then takes over its own page tables and never looks back at
firmware except through ACPI tables.

Monolithic on purpose. Microkernel IPC design is a rabbit hole.

This file records decisions. [ROADMAP.md](ROADMAP.md) records what has landed; as of Phase 9 nearly
everything in §2–§10 exists as code. When code and this file disagree, one of them is a bug
([§1.4](#14-documentation-rules)). The numbers are load-bearing. Change them deliberately and update
this doc in the same commit. An agent implementing a subsystem does not get to re-litigate the
address map, the vector numbers, or the lock order halfway through.

## Contents

| | Section | Covers |
|---|---------|--------|
| 1 | [Overview](#1-overview) | Constraints, layers, module map |
| 2 | [Invariants](#2-invariants) | Lock order, handler rules, panic policy, markers |
| 3 | [Boot](#3-boot) | Toolchain, Limine, `_start` order, linker |
| 4 | [Memory](#4-memory) | Address map, buddy allocator, paging, heap |
| 5 | [Interrupts](#5-interrupts) | GDT/IDT, exceptions, vector map, PIC and APIC |
| 6 | [Time](#6-time) | Clock sources, calibration, timekeeping |
| 7 | [SMP](#7-smp) | ACPI, AP bring-up, per-CPU, IPIs, shootdown |
| 8 | [Testing](#8-testing) | Tiers, marker contract, QEMU flags, CI |
| 9 | [Pitfalls](#9-pitfalls) | Bugs already paid for once |
| 10 | [Block I/O](#10-block-io) | Requests, barrier vs flush, ramdisk, virtio-blk, partitions, cache |

On-disk filesystem formats live in their own docs, not here ([§1.4](#14-documentation-rules)): [VIBEFS.md](VIBEFS.md) (vibefs **version 1**, CoW metadata + atomic superblock switch). Syscall ABI: [SYSCALL.md](SYSCALL.md).

---

# 1. Overview

## 1.1 Design constraints

These are not style preferences. They shape every subsystem.

1. **Concrete over generic.** One PMM, one scheduler, one page table layout. No trait soup to support
   the second implementation nobody is writing. Traits appear where there are genuinely N backends
   (console output, block devices, filesystems).
2. **Test the algorithm on the host.** Anything that is pure logic (parsers, allocators, state
   machines, encodings) lives in the library half of the crate so `cargo test --lib` covers it.
   Hardware pokes stay in the kernel half. This split is the single biggest lever on iteration speed.
3. **Serial is the ground truth.** Every subsystem prints one line when it comes up. Those lines are
   a contract enforced by the e2e harness, not debug noise.
4. **Fail loud, fail early.** Assert invariants at boot. Bounded spins everywhere so a wedged device
   produces a diagnosable hang instead of a silent one.
5. **No unbounded loops against hardware.** Every poll gets an iteration cap and a failure path.
6. **Layering is enforced by dependency direction**, not by wishful thinking. Lower layers do not
   call up. No callback into the scheduler from the physical allocator.

## 1.2 Layers

```
                     shell / userspace
   ------------------------------------------------------
    syscall  |  vfs  |  net stack  |  window server
   ------------------------------------------------------
    process / thread / scheduler / sync
   ------------------------------------------------------
    drivers (block, net, input, gpu)  |  device model (pci, virtio, dma)
   ------------------------------------------------------
    interrupts (idt, apic)  |  time  |  smp / per-cpu
   ------------------------------------------------------
    virtual memory (paging, heap, kva)  |  physical memory (buddy)
   ------------------------------------------------------
    boot (limine, gdt, serial, panic)
```

Cross-cutting and allowed from anywhere: `serial`, `panic`, `sync`, `log`.

## 1.3 Module map

As of 2026-09-22 (Phase 9). Files that exist, not a target layout. [A1](reviews/issues/A1-directory-per-subsystem.md)
will nest this pairing; do not invent `src/mm/` or `src/drivers/` until then.

**Naming.** `src/<name>.rs` is the portable half (`src/lib.rs`, host-tested via `tests/hostlib`).
`src/<name>_init.rs` is the kernel half (`src/main.rs`). A few kernel-only files have no portable
pair. `src/arch/` holds only what touches privileged CPU state (GDT, IDT, PIC, catch, gs). Nested
also: `src/fs/` (VFS + kernfs). `user/` is freestanding ELFs, not kernel modules.

| Subsystem | Portable | Kernel |
|-----------|----------|--------|
| crate | `src/lib.rs` | `src/main.rs` (`_start`, Limine requests, boot order) |
| boot / serial | `uart.rs`, `marker.rs`, `fmt_util.rs`, `symtab.rs` | `serial.rs`, `panic.rs`, `diag.rs`, `ksyms.rs` |
| arch | `desc.rs`, `pic.rs`, `vectors.rs` | `arch/mod.rs`, `arch/gdt.rs`, `arch/idt.rs`, `arch/pic.rs`, `arch/catch.rs`, `arch/gs.rs`, `x86.rs` |
| mm | `pmm.rs`, `paging.rs`, `heap.rs`, `kva.rs` | `pmm_init.rs`, `paging_init.rs`, `heap_init.rs`, `kva_init.rs` |
| time | `time.rs` | `time_init.rs` |
| acpi | `acpi.rs` | `acpi_init.rs` |
| interrupts | `irq.rs`, `apic.rs`, `ipi.rs` | `irq_init.rs`, `apic_init.rs`, `ipi_init.rs` |
| smp | `smp.rs`, `per_cpu.rs` | `smp_init.rs`, `per_cpu_init.rs` |
| sched | `thread.rs`, `sched.rs`, `wait.rs`, `sync.rs`, `lock.rs`, `work.rs` | `thread_init.rs`, `sched_init.rs`, `sync_init.rs`, `work_init.rs` |
| log | `log.rs` | `log_init.rs` |
| console | `console.rs`, `kbd.rs`, `fb.rs`, `font.rs`, `shell.rs` | `console_init.rs`, `kbd_init.rs`, `fb_init.rs`, `shell_init.rs` |
| devices | `pci.rs`, `dev.rs`, `dma.rs`, `virtio.rs` | `pci_init.rs`, `dev_init.rs`, `dma_init.rs`, `virtio_init.rs` |
| block | `block.rs`, `virtio_blk.rs`, `part.rs`, `cache.rs` | `block_init.rs`, `virtio_blk_init.rs`, `part_init.rs`, `cache_init.rs` |
| fs | `fs/mod.rs`, `fs/kernfs.rs`, `fat.rs`, `vibefs.rs` | `fs_init.rs`, `fat_init.rs`, `vibefs_init.rs`, `file_init.rs` |
| proc | `addr_space.rs`, `elf.rs`, `proc.rs`, `syscall.rs` | `addr_space_init.rs`, `user_init.rs`, `proc_init.rs`, `syscall_init.rs` |
| ktest | — | `ktest.rs` (`kernel_tests` only) |

## 1.4 Documentation rules

- Durable intent goes in this file. Ephemeral "fixed X" notes go in `CHANGELOG.md` or nowhere.
- No code review writeups, no phase retrospectives, no status reports. The roadmap checkboxes are the
  status. `git log` is the history.
- "How does this function work" goes in a doc comment. "Why is this line here at all" goes in a short
  comment on the line. Nothing goes in a comment that describes a past state of the code.
- If this doc describes behavior the code contradicts, one of them is a bug. Say which in the commit
  that fixes it.
- Constants appear once, here, and are cross-referenced rather than restated. Address map in
  [section 4.1](#41-virtual-address-map), vector numbers in [section 5.3](#53-vector-map).
- When this file outgrows one page per subsystem, split it into `docs/<topic>.md` and leave an index
  behind. Not before. On-disk formats are that split: [VIBEFS.md](VIBEFS.md), not a novel in this
  file. Syscall ABI: [SYSCALL.md](SYSCALL.md) (Phase 9B).

---

# 2. Invariants

Break these and the failure shows up somewhere else, hours later.

## 2.1 Lock order

Acquire in this order, release in reverse. Never take a lower number while holding a higher one.

1. page tables
2. physical allocator (buddy)
3. heap
4. scheduler (also: wait-queue lists and blocking-primitive predicates)
5. device / driver locks
6. serial

Serial is last so any lock holder can still log. Page tables are first because unmapping needs to
allocate and free through everything below it. Blocking `WaitQueue`s are serialized by the scheduler
lock: the predicate check and the enqueue happen under that same lock (DESIGN [§9.4](#94-concurrency)).

Filesystem (dentry / inode / super / mount) locks are **device-rank**. The tables are static, so
bring-up does not take RANK_HEAP under them. Do not allocate while holding them; do not hold them
across serial. A dedicated numbered rank is a Design ACK, not a seventh global lock. Drop the VFS
lock before blocking block I/O (the FAT File API never holds VFS across FAT or block waits; the
volume lives in BSS behind a busy flag, not an IRQ-off mutex). Do not nest VFS with the FAT volume.

## 2.2 Interrupt handler rules

An interrupt handler must not:

- allocate or free (no heap, no buddy, no `Vec`, no `format!`)
- take any lock that is ever held with interrupts enabled
- log at anything but the most extreme failure path
- run unbounded loops

An interrupt handler must:

- EOI before it can possibly context switch, so the controller is not held across a switch
- rearm its own one-shot timer source before doing anything else that can yield
- keep its stack frame small; the double fault handler is the only one with a dedicated stack

Both of the "must" rules are expanded in [section 5.8](#58-handler-ordering-rules), because both are
easy to violate and expensive to debug.

Blocking and allocation are a class of bug, not an instance. Context rules:

| Context | May block? | May alloc? |
|---------|------------|------------|
| Hard IRQ / MSI handler | No | No |
| Softirq equivalent (high-prio workqueue) | No | No (enqueue only from IRQ) |
| Threaded IRQ / workqueue worker | Yes | Yes |
| Driver `probe` | Yes | Yes |

The hard-IRQ top half acknowledges and wakes. Work that allocates or blocks runs on a kernel thread
([section 5.4](#54-irq-registration), ROADMAP §6.6).

## 2.3 Locking with interrupts

Every spinlock that is taken from both an ISR and normal context disables interrupts for the whole
critical section. That is the default: `SpinMutex` is IRQ-aware, and the non-IRQ-aware variant does
not exist. The scheduler lock, the input ring, the buddy allocator, and the heap all qualify.

Pick one spinlock implementation and use it everywhere. The old tree ended up with two (a ticket lock
in one design doc, an IRQ-guarded spin mutex in the code) and the mismatch was a source of confusion
for weeks.

Cross-CPU rule: a CPU never touches another CPU's run queue directly. Work is handed over through a
per-CPU inbox plus a reschedule IPI. More SMP-specific rules in [section 7.7](#77-locking-with-more-than-one-cpu).

## 2.4 Memory invariants

- Physical page 0, the loaded kernel image, the AP trampoline page, and firmware-reserved regions are
  never in the buddy free lists.
- Buddy free list nodes live inside the free pages themselves. A stray write into freed memory
  corrupts the allocator, so guard pages on stacks are not optional.
- Kernel mappings are `GLOBAL`. Unmapping one requires a TLB shootdown on every online CPU before the
  virtual address may be reused.
- MMIO pages are mapped uncacheable. QEMU tolerates write-back MMIO; real hardware does not.
- Every mapping is `NO_EXECUTE` unless it holds code that will actually be fetched.

## 2.5 Panic policy

Binding order (do not invert):

1. Broadcast halt IPI `0xFE` first (Fixed delivery, not NMI) so other CPUs stop before the dump.
2. Re-initialize serial from scratch (the panic may be *in* the serial path).
3. Print location and message; dump registers, the current thread, and the last N log records.
4. Symbolized backtrace when frame pointers exist (in-image sorted table, binary search, no alloc).
5. `cli; hlt` loop, or QEMU `isa-debug-exit` under the `panic_exit` test feature.

No unwinding: `panic = "abort"`. Serial TX in this path is a bounded THRE poll; drop the byte on
timeout (see [§9.6](#96-hardware-polling)). Do not take SCHED. The log ring is readable after the
halt IPI without taking its TAS (force-unlock if the panicking CPU held it).

Log ring (ROADMAP §5.5): 256 records × 96-byte messages. Wrap **drops oldest**. Compile-time max is
`trace` (debug) / `debug` (release); runtime filter is `AtomicU8`, default `info`. Panic dump prints
the last 24. The ring TAS is IRQ-off and is not in the ranked lock order; never hold it across serial
TX. `klog!` and `Serial` formatted writes keep IF off for the whole emit so the per-CPU capture
stage cannot mix with a preempting thread. `dmesg` prints through a plain serial path that does not
re-capture. Two-pass `nm` fills an in-image `.rodata` symbol table so `.text` stays put; the target
JSON keeps `frame-pointer: always`. Overflow, filter, and lookup are host-tested. Printer thread
parked (Design ACK): global IRQ-safe ring + serial try-lock sink.

The portable half (`src/lib.rs`, host-tested) never panics on data: parsers and table walks return
the module error. Clippy restriction lints `unwrap_used`, `expect_used`, and `panic` are deny on that
crate (allowed in `#[cfg(test)]`). `indexing_slicing` stays warn, not deny, until the parser modules
(`acpi`, `part`, `fat`, `vibefs`, `pci`) are clean; it is not crate-enabled under `-D warnings`.
Hardware-half panics still halt in the binding order above.

Exceptions split into two groups. Recoverable ones (`#BP`, and `#PF` once demand paging exists)
log and continue. **Ring-3 faults** (`#PF`/`#GP`/`#UD`/`#DE`/…) kill the user process with a
diagnostic (`user: pid N killed SIG…`) and the kernel keeps running; they do not take the
DESIGN panic-halt path. **Kernel faults** dump and halt through the same order as
`#[panic_handler]`. Nothing is silently swallowed. NMI stays on IST for real NMIs; it is not
the panic broadcast.

## 2.6 Serial markers

Every boot line is `vibeOS: <subsystem>: <state>`, lowercase, no punctuation at the end. Success
markers are asserted by the e2e harness in order. Adding a marker means updating the contract in
[section 8.3](#83-end-to-end) in the same commit.

`marker!` for contract lines (never filtered, always captured); `klog!` for everything else;
`PlainSerial` only for `dmesg` and panic dumps.

```
vibeOS: serial online
vibeOS: pmm: 32741 free 4KiB frames
vibeOS: paging: cr3 ok
vibeOS: paging: mmio uc
vibeOS: heap ok
vibeOS: kva: ready
vibeOS: gdt ok
vibeOS: pic: remapped
vibeOS: idt ok
vibeOS: per_cpu: bsp ready
vibeOS: acpi: xsdt 9 tables
vibeOS: time: tsc 2500000/ms
```

---

# 3. Boot

Power-on to `sti`. Limine does the ugly part (real mode, A20, long mode, ELF loading) and hands us a
64-bit kernel with paging already on. Everything after that is ours.

## 3.1 Toolchain

| Piece | Value |
|-------|-------|
| Channel | dated nightly in `rust-toolchain.toml` (bump with CI in one PR) |
| Components | `rust-src` (required by `build-std`), `llvm-tools` (objdump/nm/size) |
| Target | `x86_64-unknown-none-executable.json`, custom spec in the repo root |
| Build | `cargo build -Z build-std=core,compiler_builtins,alloc --target x86_64-unknown-none-executable.json` |
| Panic | `abort`, both profiles |
| Extra host tools | `xorriso`, `nasm` (AP trampoline), `qemu-system-x86_64`, `python3`, `dosfstools` (`fsck.fat`) |

Plain `cargo build` does not produce a usable kernel. Use `make`. `cargo test --lib` is the only cargo
invocation that runs bare.

`make` pins `CARGO_TARGET_DIR` to `./target`. Some environments point it at a shared cache, which
leaves the ISO packaging a stale ELF from a previous build and produces genuinely baffling debugging
sessions.

Target spec notes:

- `"executable": true`, no PIE, static relocation model, `code-model: kernel`.
- `disable-redzone: true`. Interrupt handlers clobber the red zone.
- `features: "-mmx,-sse,+soft-float"` until the kernel explicitly enables SSE and saves state on
  context switch. Enabling it early means the first floating point use in a driver silently corrupts
  another thread.
- No RELRO in `pre-link-args`. It conflicts with a non-PIE static kernel and only produces confusing
  linker output.

## 3.2 Limine protocol

Limine scans the loaded ELF for request structures in linker sections, in this order:

```
.limine_requests_start   marker
.limine_requests         the request statics
.limine_requests_end     marker
```

Every request is a `#[used]` `static` placed in `.limine_requests`. Miss the section attribute and the
loader never sees the request, so the response pointer is null and the kernel dies on the first unwrap
with no explanation. Check the base revision before trusting any other response.

| Request | What we need from it |
|---------|---------------------|
| Base revision | Protocol version handshake. Halt with a serial line if unsupported. |
| Framebuffer | Linear BGRX8888, 32 bits per pixel. Row stride is `pitch` bytes, which may exceed `width * 4`. |
| Memory map | Physical regions and types. Only `USABLE` feeds the buddy allocator. |
| HHDM | Higher-half direct map offset. `virt = phys + offset` for any physical access before our own tables exist. |
| Executable address | Physical and virtual base of the loaded kernel, so we can map ourselves and exclude ourselves from the allocator. |
| RSDP | Physical pointer to the ACPI RSDP. Gates all of ACPI, APIC, HPET, SMP. |
| SMP (optional) | Limine can bring up APs for us. We do it ourselves; see [section 7](#7-smp) for why. |

Firmware reclaimable regions stay out of the free lists. Reclaiming them is a few megabytes for a
nonzero chance of stomping something ACPI still points at.

## 3.3 `_start` order

Ordering here is not a suggestion. Each step depends on state the previous one established. This table
is the authority for boot ordering; the e2e contract in [section 8.3](#83-end-to-end) asserts a subset
of it.

| # | Step | Marker | Why here |
|---|------|--------|----------|
| 1 | Serial (COM1) | `serial online` | Nothing before this is debuggable. The panic handler uses the same port. |
| 2 | Base revision check | `limine: rev N ok` | Everything downstream reads Limine responses. |
| 3 | GDT + TSS + IST | `gdt ok` | Need a known code selector and a double-fault stack before the IDT is worth installing. |
| 4 | PIC remap, all IRQs masked | `pic: remapped` | Firmware may leave the 8259 live with vectors overlapping CPU exceptions. Remap first, mask everything, unmask later. |
| 5 | IDT | `idt ok` | Exceptions become diagnosable. Hardware IRQs are still masked. |
| 6 | Buddy PMM from memory map | `pmm: N free 4KiB frames` | Page tables and heap both need frames. |
| 7 | Page tables, install CR3 | `paging: cr3 ok` | Own the address space before mapping anything device-specific. |
| 8 | MMIO PTE attribute patch | `paging: mmio uc` | LAPIC/IOAPIC/HPET pages must be uncacheable before first touch. |
| 9 | Kernel heap | `heap ok` | `alloc` becomes legal. Everything after this can use `Vec` and `Box`. |
| 10 | Kernel VA allocator | `kva: ready` | Guarded stacks need it, so threads need it. |
| 11 | Per-CPU area for the BSP | `per_cpu: bsp ready` | `GS_BASE` must be valid before any `per_cpu!` access, including from ISRs. |
| 12 | ACPI tables | `acpi: xsdt N tables` | MADT drives APIC and SMP, HPET drives calibration. |
| 13 | Time: HPET or PIT, TSC calibration | `time: tsc N/ms` | The scheduler needs a tick, and AP bring-up needs `busy_wait_ms`. |
| 13b | BSP LAPIC, I/O APIC, LAPIC timer | `time: lapic_timer ok (<mode>)` | After TSC calib. Prove a tick (TSC-deadline → periodic → PIT), then mask PIC + PIT GSI if LAPIC owns it. |
| 14 | Scheduler, idle thread on BSP | `sched: cpu0 ready` | Preemption target must exist before the timer starts firing into it. |
| 15 | Arm scheduler; emit `irq: enabled` | `irq: enabled` | Scheduler is live. The timer already ticks from steps 13/13b; this marker is post-sched arming (IF on, preemption live), not the first STI. IRQ1 stays masked until the keyboard driver (step 17). |
| 16 | APIC + SMP bring-up | `smp: done` | Needs time (delays), heap (per-CPU allocation), scheduler (AP entry point). Live Phase 4 order: SMP before console. |
| 17 | Framebuffer console, PS/2, mux | `console ok` | After `smp: done`. Install the IRQ1 / keyboard GSI handler, init the 8042, then unmask. Replay the pre-FB log ring onto the framebuffer. |
| 17b | PCI enum + device registry | `pci: N devices` | After `console ok`. Legacy `0xCF8`/`0xCFC` for bus 0; MCFG → ECAM beyond. Scan builds a device list. Workqueue + threaded IRQ start, then drivers bind by id. Memory BARs are mapped through ioremap or the capped physmap; sizes above 32 MiB are recorded and skipped (DESIGN §4.1). |
| 17c | Block layer + ramdisk + virtio-blk + partitions | `block: <name> <n> sectors` | After bind. One line per device. virtio-blk (`vda`) emits during probe; ramdisk (`ram0`) follows in `block_init`; partition children (`<parent>p<N>`) after that. |
| 17d | VFS + FAT initrd root + pseudo mounts + vibefs | (none) | After block. Makefile FAT32 initrd at `/` when live, else dummy ramfs. Then devfs/procfs/tmpfs/sysfs on `/dev` `/proc` `/tmp` `/sys`. BSS vibefs at `/vibe` (Phase 8D). No serial marker. |
| 18 | Shell thread, builtins | `shell ready` | Last marker. Spawn a kernel thread (not `_start`, not idle, not an ISR), register builtins into the command table, print the prompt. `lspci` / `devices` / `blk` are live. |

Ordering rules worth stating separately because they were learned the hard way:

- The bootstrap tick is the LAPIC timer after step 13b, or PIC IRQ0 only on the
  PIT fallback (LINT0 ExtINT). Other PIC lines stay masked; step 15 is
  `irq: enabled` (IF on, preemption live), not the first unmask. An unexpected
  line before its driver is a halt, not a useful backtrace.
- `smp: done` precedes `console ok`, `pci: N devices`, and `shell ready`. The e2e harness enforces
  it. If SMP moves after the shell, AP failures become invisible in CI.
- ACPI discovery for the step-8 UC patch may run immediately after CR3 (alongside `paging: mmio uc`).
  The `acpi: xsdt N tables` marker stays at step 12. Do not "fix" that by moving the walk after the
  heap: first touch of LAPIC/IOAPIC/HPET would then be cacheable.

Live boot through Phase 3 slice B runs steps 6–10 (PMM, paging, heap, KVA) before
steps 3–5 (GDT/TSS/IST, PIC remap, IDT). IST stacks are allocated from the KVA
allocator, which does not exist until step 10. Relative order among those three
is unchanged: GDT, then PIC remap, then IDT. Step 11 (`per_cpu: bsp ready`) runs
after IDT: `mov gs` during GDT load zeros the hidden base, so `GS_BASE` is
written after that, and before the first timer IRQ so an ISR can `gs:[0]`. ACPI table walk +
`paging: mmio uc` still run after CR3 (step 8); the `acpi: xsdt N tables` marker
stays after per_cpu (step 12), then `time: tsc N/ms` (step 13). Step 13b enables
the LAPIC, programs the I/O APIC (masked), enables IF, proves the per-CPU timer, and
emits `time: lapic_timer ok (<mode>)` before masking the PIC and the PIT GSI
when LAPIC owns the tick. PIT fallback keeps IRQ0 unmasked with LINT0 ExtINT.
The handler updates the clock, EOIs, rearms (TSC-deadline), then
`on_timer_tick`, a no-op until the idle thread exists. Step 14
(`sched: cpu0 ready`) then step 15 (`irq: enabled`) follow meminfo: IF on,
preemption live. Step 16 brings APs up one at a time; each AP prints
`sched: cpu<i> ready` then the BSP prints `smp: ap online`, then `smp: done`.
Step 17 is the framebuffer console, PS/2, and mux (`console ok`) after SMP.
IRQ1 stays masked until the keyboard handler is installed, then the 8042 is
initialized, then the keyboard GSI is unmasked. After LAPIC owns the tick the
8259 is masked: IRQ1 is IOAPIC-only. Do not unmask PIC IRQ1 as a fallback. The
default PIC handler still halts on an unexpected line. The timer path re-runs the
8259 ICW sequence even when FADT bit 0 skipped the boot remap (QEMU clears
that bit but still has a PIC on 0x08).
Step 17b enumerates PCI (CF8 on bus 0, ECAM from MCFG otherwise), maps
memory BARs under the 32 MiB cap, fills the device registry, and emits
`pci: N devices`. Workqueue workers and the threaded-IRQ bottom half start
next. Drivers register, then bind after the scan, not inline. Virtio-rng
matches by id when a modern virtio device is present (ktest adds one; e2e
does not). Ramdisk init follows bind and emits `block: <name> <n> sectors`.
Partition scan stamps an MBR on `ram0` and a GPT on `vda` (if empty) and emits
`block: <parent>p<N> <n> sectors` per child. A writeback cache thread starts
before the scan. VFS mounts a dummy ramfs at `/` after that, then mounts
devfs / procfs / tmpfs / sysfs on `/dev` `/proc` `/tmp` `/sys`, with no serial
marker. Step 18 spawns the shell as a kernel thread after `console ok` / `pci: N devices`
/ `block: …` and emits `shell ready` last. `boot: phase1 done` was a Phase 1–4 stand-in and is no
longer emitted; the trailing contract line is `shell ready`.
The e2e contract in [section 8.3](#83-end-to-end) is the live order.

## 3.4 Linker script

`linker.ld` places the kernel at the higher-half base and defines the symbols the kernel maps itself
with. Two requirements that are easy to get wrong:

- `.got` must sit inside the mapped image, before `.bss`, so `__kernel_vma_end` covers it. LLVM emits
  GOT-relative accesses; if the GOT falls outside the range the kernel maps for itself, the first such
  access faults after CR3 install.
- Section boundaries are page aligned so `.text` can be mapped executable and read-only while
  `.rodata` and `.data` are `NO_EXECUTE`. Without alignment, W^X on the kernel image is impossible
  without mapping code writable.

Export at minimum: `__kernel_vma_start`, `__kernel_vma_end`, and per-section start/end pairs for
`.text`, `.rodata`, `.data`, `.bss`.

## 3.5 Dev profile

`opt-level = 1` for the dev profile. At `opt-level = 0` the page table setup function's stack frame is
large enough to overflow the boot stack Limine provides, and it faults on entry before printing
anything. If a boot function needs a big frame, box it or move it to a thread with a real stack; do
not rely on the optimizer.

## 3.6 ISO and QEMU

`make` stages `iso_root/` with the kernel ELF, `limine.conf`, and the Limine BIOS and UEFI artifacts,
then builds a hybrid ISO with `xorriso` and runs `limine bios-install`. Hybrid means the same image
boots BIOS and UEFI, which matters for real hardware later.

The Makefile lists every `.rs` and `.asm` under `src/` as a prerequisite. A hand-maintained short list
produced stale ISOs when new subsystem directories appeared.

`make run` boots with COM1 on stdio and more than one CPU, so the default developer loop exercises SMP
rather than discovering AP bugs only in CI. Full flag set in [section 8.4](#84-qemu-flags).

Interactive input, two paths (not USB HID):

| Where you type | What the guest sees |
|----------------|---------------------|
| QEMU window (focused) | PS/2 i8042 → IRQ1/GSI → `kbd_init` ring |
| Controlling terminal | COM1 (`-serial stdio`), polled after the PS/2 pop |

Many IDE-embedded terminals do not forward keystrokes to `-serial stdio`. Output
appears, input goes nowhere. Type in the QEMU window, or run from a real terminal.
QEMU monitor `sendkey` hits the same i8042 as the window; `make test-e2e` and
`make test-ps2` use that as the TCG stand-in.

---

# 4. Memory

Three allocators, one address map. Physical frames come from a buddy allocator. Kernel virtual
addresses come from a range allocator. Small objects come from a heap layered on the other two.

## 4.1 Virtual address map

x86_64 canonical addressing splits at bit 47. Low half is user, high half is kernel, with a
non-canonical hole between. Kernel regions are fixed, not discovered:

| Range | Size | Role |
|-------|------|------|
| `0x0000_0000_0000_0000` – `0x0000_7FFF_FFFF_FFFF` | 128 TiB | User address space. Empty until ring 3 exists. |
| `0x0000_0000_0000_0000` – `0x0000_0000_2000_0000` | 512 MiB | Low identity window, kernel tables only. First 2 MiB executable. |
| *hole* | | Non-canonical. Any pointer here is a bug. |
| `0xFFFF_8000_0000_0000` + | ≤ 8 TiB | HHDM physmap, `virt = phys + HHDM_OFFSET`. Offset comes from Limine. 2 MiB pages. |
| `0xFFFF_C000_0000_0000` – `0xFFFF_C000_0400_0000` | 64 MiB | Kernel heap. Starts at 1 MiB mapped and grows. |
| `0xFFFF_D000_0000_0000` – `0xFFFF_D010_0000_0000` | 64 GiB | Kernel VA allocator: guarded stacks, `vmap`, large transient mappings. |
| `0xFFFF_E000_0000_0000` – `0xFFFF_E000_1000_0000` | 256 MiB | `ioremap` window for device MMIO that should not be reached through the physmap. |
| `0xFFFF_FFFF_8000_0000` – `0xFFFF_FFFF_FFFF_FFFF` | 2 GiB | Kernel image. Matches the `kernel` code model so `.text` relocations fit in 32-bit displacements. |

Regions must not overlap and every one asserts that its range is unmapped before claiming it. This is
a real failure mode: two subsystems in the old tree were both designed at `0xFFFF_C000_*` and only one
noticed.

The low identity window exists for one reason: an AP starting from SIPI runs in real mode and then
32-bit protected mode at a low physical address, so that address must be identity mapped and
executable. It can be torn down once every AP has reached long mode, and eventually should be, since a
writable executable identity map of the low 512 MiB is not something to keep around forever.

The physmap is capped at 8 GiB regardless of what the memory map says. Some firmware describes MMIO
BARs as multi-terabyte regions, and walking that to build page tables at boot does not finish. The cap
is computed from usable RAM high water mark, kernel image end, and framebuffer extent
(`base + height * pitch`), not from raw memory map entries.

## 4.2 Physical memory: buddy allocator

Free blocks of order *k* cover 2^k contiguous 4 KiB frames. Split on allocation, merge with the buddy
on free. Free list nodes live inside the free pages themselves, so there is no bitmap and no
allocation needed to run the allocator.

That last property has a sharp edge: a stray write into a freed page corrupts the allocator's linked
lists, and the resulting crash happens later, somewhere unrelated. This is exactly why kernel stacks
get guard pages.

```rust
alloc_frame() -> Option<PhysAddr>          // order 0
alloc_pages(order: u8) -> Option<PhysAddr>
free_frame(pa: PhysAddr)
free_pages(pa: PhysAddr, order: u8)
stats() -> PmmStats                        // total, free, largest order available
```

Initialization walks the Limine memory map and ingests every `USABLE` region as power-of-two aligned
blocks, excluding:

- physical frame 0
- the loaded kernel image span
- the AP trampoline page at `0x8000`
- the framebuffer
- anything not marked `USABLE`, including bootloader and ACPI reclaimable

`free_frame_count()` must be O(1). Maintaining a running counter is trivial; walking the free lists to
answer `meminfo` is not, and it gets called from a shell command that people hammer.

## 4.3 Page tables

The kernel builds its own PML4 from buddy frames rather than editing Limine's. Contents at install
time:

1. Kernel image, mapped per section with correct permissions.
2. Physmap over `[0, map_end)` at the HHDM offset, using 2 MiB pages.
3. Low identity window, 512 MiB, 2 MiB pages, first 2 MiB executable.
4. The bootloader stack window, duplicated out of Limine's active tables so `_start`'s own stack keeps
   working across the `mov cr3`.

Then set `EFER.NXE` if it is not already on, load CR3, and print `paging: cr3 ok`. Immediately after,
patch the physmap PTEs covering LAPIC, I/O APIC, and HPET to uncacheable, preserving the 2 MiB page
size rather than splitting.

### PTE flag policy

| Mapping | Flags |
|---------|-------|
| Kernel `.text` | present, global, read-only, executable |
| Kernel `.rodata` | present, global, read-only, NX |
| Kernel `.data` / `.bss` | present, global, writable, NX |
| Physmap | present, global, writable, NX |
| Heap | present, global, writable, NX |
| Kernel stacks | present, global, writable, NX, guard page unmapped below |
| MMIO | present, global, writable, NX, PCD + PWT |
| Low identity, first 2 MiB | present, writable, executable (trampoline) |
| Low identity, rest | present, writable, NX |

NX everywhere by default. The one thing that must stay executable low down is the trampoline page;
mapping the whole identity window NX is how the old tree produced a page fault during AP bring-up that
looked exactly like a hang.

MMIO gets PCD + PWT unconditionally. QEMU ignores cache attributes and write-back MMIO appears to
work, so this bug only shows up on real hardware, months later, as inexplicable device behavior.
The Limine framebuffer stays write-back on the HHDM physmap for now (QEMU-tolerant). WC/UC remap of
scanout is a later polish pass; double buffering is also parked (ROADMAP §5.1).

### TLB

- `invlpg` after any single-PTE edit, including MMIO attribute patches.
- Kernel mappings are `GLOBAL`, so they survive a CR3 reload. Unmapping one requires a shootdown on
  every online CPU before the VA can be reused. See [section 7.9](#79-tlb-shootdown).
- Before SMP exists, `invlpg` is sufficient. Write the shootdown hook as a no-op single-CPU function
  from the start so the call sites are already correct when APs arrive.

## 4.4 Kernel heap

A free-list heap at `HEAP_START`, backed by buddy frames mapped writable + NX. Initial mapping is
1 MiB; the allocator grows in page-sized increments up to the 64 MiB region limit. `GlobalAlloc`
disables interrupts around `alloc` and `dealloc` because allocation happens under locks that ISRs must
never contend.

`#[alloc_error_handler]` panics with the requested layout. Silent OOM is worse than a halt.

The heap is deliberately simple and deliberately temporary. A slab allocator for hot object types
(TCBs, file descriptors, inodes, network buffers) lands in the advanced memory phase; general
allocation stays on the free-list heap.

## 4.5 Kernel virtual address allocator

The heap answers "give me 40 bytes". The KVA allocator answers "give me 16 KiB of contiguous virtual
address space with a guard page below it". Guarded kernel stacks are the motivating case, `vmap` of
non-contiguous frames is the second.

- A guarded stack of *n* pages reserves *n+1* pages of VA and maps only the upper *n*. The bottom page
  stays unmapped so overflow takes a page fault instead of quietly eating whatever is below.
- Stack frames are allocated as *n* separate order-0 frames, not one order-*k* block. Stacks do not
  need physical contiguity and requesting it fragments the buddy allocator for nothing.
- Freeing a VA range requires a TLB shootdown before reuse. Recently freed ranges go to the tail of
  the free list so the window between free and shootdown is not immediately reused.
- Freeing the stack you are currently running on does not work. Dead thread stacks go on a deferred
  reap list, drained by another thread.

Default kernel stack is 4 pages (16 KiB) plus guard. If that turns out to be tight, raise it rather
than debugging mysterious corruption.

## 4.6 What comes later

The roadmap covers these in detail. Listed here so the interfaces above are designed with them in
mind:

- Demand paging. `map_page` gains a "reserve VA, populate on fault" mode, and `#PF` becomes a
  recoverable exception with a real fault handler rather than a halt.
- Copy on write. `fork` clones an address space by sharing frames read-only with a refcount; the write
  fault does the copy. Needs per-frame metadata, which means the PMM grows a `struct Frame` array.
- Slab caches, per-CPU magazines to avoid the global buddy lock on hot paths.
- Page cache unified with `mmap`, so file-backed pages and anonymous pages share eviction.
- Swap, which needs reverse mappings from a frame back to every PTE referencing it.

The per-frame metadata array is the pivot. Refcounting, reverse mapping, and page cache all need it,
so the PMM should be built expecting it to appear.

## 4.7 DMA

`DmaBuffer` is physically contiguous (buddy `allocate_constrained`: size, alignment, optional
power-of-two boundary including 4 GiB for 32-bit devices). The device-visible address is
`dma_to_device(phys)` — identity until an IOMMU exists — never the HHDM virtual address.

`sync_for_device` / `sync_for_cpu` always run at the API boundary. On x86 they are `fence(Release)` +
`sfence` and `fence(Acquire)` + `lfence`. Descriptor publish stores the index after that store-side
barrier, not a bare `compiler_fence`.

---

# 5. Interrupts

Descriptor tables, the vector map, and the migration from the legacy 8259 to the APIC. Handler rules
are in [section 2.2](#22-interrupt-handler-rules); this is the mechanism.

## 5.1 GDT and TSS

Flat segmentation. Segments exist because the CPU requires them, not because we use them.

| Selector | Descriptor |
|----------|------------|
| `0x00` | null |
| `0x08` | kernel code, 64-bit, ring 0 |
| `0x10` | kernel data, ring 0 |
| `0x18` | user data, ring 3 |
| `0x20` | user code, 64-bit, ring 3 |
| `0x28` | TSS (16 bytes, two GDT entries) |

User selectors go in from the start even before ring 3 exists. `syscall`/`sysret` reads segment
selectors out of `IA32_STAR` with a fixed layout: `STAR.SYSCALL_CS = 0x08` so kernel SS is CS+8, and
`STAR.SYSRET_CS = 0x10` so user SS is +8 (`0x18`) and user CS is +16 (`0x20`). User *data* therefore
sits before user *code*. Getting the order right up front avoids a rebuild of the GDT later.

Each CPU gets its own GDT and TSS. The TSS holds `RSP0` (the kernel stack that `syscall` and ring
transitions land on) and the IST array. `CpuTables` is the per-CPU bundle; the BSP keeps one in a
static, phase 4 allocates one per AP.

| IST index | Use |
|-----------|-----|
| 1 (`DOUBLE_FAULT_IST_INDEX = 0`) | `#DF`. A double fault often means the stack is gone, so it needs a stack that is known good. |
| 2 | NMI |
| 3 | `#MC` machine check |
| 4 | `#DB` debug |

IST stacks are page-aligned guarded stacks from the KVA allocator (4 mapped pages + unmapped guard),
one per slot, per CPU. The software index is zero-based; the hardware field in the TSS descriptor is
one-based. Off by one here means the double fault handler runs on the broken stack and turns into a
triple fault, which QEMU reports as a silent reboot loop.

## 5.2 IDT and exceptions

256 entries, filled at boot, one shared IDT for exceptions plus per-CPU LVT vectors. Handlers use
`x86-interrupt` ABI so the compiler emits the correct frame and `iretq`.

Policy is **CPL-split** (Phase 9C). Catch intercept (in-guest tests) still wins first.

| Vector | Exception | Kernel (CPL=0) | User (CPL=3) |
|--------|-----------|----------------|--------------|
| `0x03` | `#BP` breakpoint | log, continue | log, continue |
| `0x0E` | `#PF` page fault | halt (demand paging: Phase 10) | `SIGSEGV`, kill + diagnostic |
| `0x0D` | `#GP` general protection | log selector/error code, halt | `SIGSEGV`, kill + diagnostic |
| `0x06` | `#UD` invalid opcode | log CR2/RIP, halt | `SIGILL`, kill + diagnostic |
| `0x00`/`0x10`/`0x13` | `#DE`/`#MF`/`#XF` | halt | `SIGFPE`, kill + diagnostic |
| `0x08` | `#DF` double fault | log on IST stack, halt | halt (not a process kill) |
| `0x12` | `#MC` machine check | log, halt | halt |
| `0x02` | NMI | real NMIs on IST; panic halt is Fixed IPI `0xFE`, not NMI | same |
| rest | | log vector + error code, halt | halt unless mapped above |

Every **halting** handler prints the interrupt frame (RIP, CS, RFLAGS, RSP, SS), the error code, and CR2
for faults. A halt with no register dump is a wasted crash. User-kill prints
`user: pid N killed SIG…` (not a `vibeOS:` boot marker) and reaps the process.

## 5.3 Vector map

Vector number is priority on x86: the CPU's task priority compares `vector >> 4`. IPIs sit high so a
reschedule or shootdown is not starved by a busy NIC.

| Vector | Owner |
|--------|-------|
| `0x00`–`0x1F` | CPU exceptions. Reserved by hardware. |
| `0x20`–`0x2F` | Legacy PIC IRQ0–15 after remap. Live only until the I/O APIC takes over. `0x20` PIT, `0x21` keyboard, slave base `0x28`. |
| `0x30`–`0x7F` | Dynamically allocated device vectors: I/O APIC GSIs and MSI/MSI-X. |
| `0x80`–`0xEF` | Reserved. Room for more device vectors, per-CPU device queues. |
| `0xF0` | LAPIC timer |
| `0xF1` | LAPIC error LVT |
| `0xF2` | LAPIC thermal / performance counter LVT |
| `0xFB` | Call-function IPI (run a closure on another CPU) |
| `0xFC` | TLB shootdown IPI |
| `0xFD` | Reschedule IPI |
| `0xFE` | Panic halt IPI |
| `0xFF` | LAPIC spurious vector |

Vector numbers live in one module as named constants, with a host unit test asserting that no two are
equal. That test costs nothing and catches the copy-paste that assigns two subsystems the same vector.

## 5.4 IRQ registration

Drivers do not write to the IDT. They ask for a vector:

```rust
let vec = irq::allocate_vector(cpu)?;     // from the 0x30..=0x7F pool
irq::set_handler(vec, my_handler);
// or: irq::set_threaded(vec, Some(top_half), thread_fn);
```

The kernel binary exposes this as `irq_init::allocate_vector`. Allocate is refused
inside a hard-IRQ (a per-CPU `IN_ISR` flag set by the dispatcher). That flag is not
`InterruptGuard` nest: in-guest tests hold a guard on the BSP, so a nest check would
false-refuse every allocate from `ktest`.

The allocator records the dest CPU. `set_affinity` updates that binding. I/O APIC
routes are rewritten immediately; MSI/MSI-X callers reprogram the message from
`cpu_of`. Phase 17 rebalance uses this table rather than a second map.

MSI message address is `0xFEE0_0000 | (apic_id << 12)` (physical dest, RH=0). Data is
the vector (fixed, edge). MSI-X table entries live in a BAR (BIR + offset from the
capability). The dispatcher writes mask, then addr/data, then the caller's mask bit,
sets COMMAND.INTX# disable, and enables MSI-X. Leaving INTx unmasked while MSI-X is
armed duplicates IRQs.

INTx remains the fallback when a function has neither MSI nor MSI-X: route the GSI
through the I/O APIC (PCI is level, active low). Keyboard keeps hardcoded vector
`0x30`; the pool starts handing out `0x31`. `free_vector` masks that GSI before it
clears the handler and forgets a `Route::IoApic` record. It also zeros threaded
`top`/`work`/`pending` so a recycled vector cannot keep the old bottom half. MSI
and MSI-X are message-based and do not need an I/O APIC mask on free. Clearing
first would let a still-asserted level line storm empty `dispatch` calls, and a
later `allocate_vector` could take IRQs from the old device.

EOI is the dispatcher's job, not the driver's. The dispatch layer knows whether a
vector arrived via PIC or LAPIC and signals the right controller.

A threaded handler's top half runs in `dispatch` (ack / mask / wake only). The
bottom half is a kernel thread pinned to the last online CPU; it may allocate and
block. `set_threaded` is refused inside a hard-IRQ. The softirq stand-in is the
high-prio workqueue: IRQ context enqueues a `fn(usize)` and wakes workers.

## 5.5 8259 PIC

The PIC is a bootstrap artifact and a fallback, nothing more.

- Remap master to `0x20`, slave to `0x28`. Firmware may leave them at vectors 0x08–0x0F, which collide
  with `#DF` and friends, so a spurious IRQ before remap looks like a CPU exception.
- Mask everything (`0xFF` to both data ports) immediately after remap.
- After TSC calibration, the bootstrap tick is the LAPIC timer when it proves
  (live [section 3.3](#33-_start-order) step 13b). Unmask PIC IRQ0 only on the
  PIT fallback (LINT0 ExtINT). `sti` is allowed for that prove; `on_timer_tick`
  is a no-op until the idle thread exists. `irq: enabled` is Phase 3 slice B,
  after `sched: cpu0 ready`. Unmask IRQ1 only once a keyboard handler exists
  (phase 5 step 17); the default PIC path halts on an unexpected line.
- Once the I/O APIC routes devices and the LAPIC timer is verified ticking, mask the PIC completely.
  Leaving it live means every interrupt is delivered twice.
- Keep the PIT driver code. It is still the calibration fallback and still provides the delays that AP
  bring-up needs.

FADT `iapc_boot_arch` bit 0 says whether the legacy 8259 exists at all. Modern hardware may not have
one, and assuming it does means an early write to a port nobody answers. The PIC step reads that bit
from the FADT already parsed after CR3 and skips the ICW sequence when the legacy controller is
absent. Missing FADT still remaps and masks. `pic: remapped` means this step finished: the ICW
sequence ran, or FADT skip declined the ports. Unlike `paging: mmio uc`, it is not a claim that
ports were programmed.

## 5.6 I/O APIC

Indirect register access through `IOREGSEL` at MMIO base + `0x00` and `IOWIN` at + `0x10`.
Redirection entry *n* is register `0x10 + 2n` (low) and `0x11 + 2n` (high).

- Write the high dword (destination) before the low dword. The low dword contains the mask bit, so
  writing it first can unmask an entry that points nowhere.
- Apply MADT interrupt source overrides. ISA IRQ0 is commonly remapped to GSI 2. Routing pin 0 and
  assuming it is the timer is a classic way to get no timer interrupts and no error message.
- Polarity is bit 13, trigger mode bit 15. Take both from the override entry when one exists; ISA
  defaults (edge, active high) otherwise.
- Mask the GSI for any source the LAPIC now owns. When the LAPIC timer is the tick, the PIT's GSI gets
  masked, not just ignored.

## 5.7 LAPIC

Full register list and IPI encoding in [section 7.2](#72-lapic). The parts that matter for interrupt
handling:

- Enable via `IA32_APIC_BASE` (MSR `0x1B`) bit 11. If that bit is clear, every LAPIC register reads
  zero and the failure looks like missing hardware.
- Set the spurious vector register (offset `0x0F0`) with the enable bit and vector `0xFF`. Spurious
  interrupts must not EOI.
- Set TPR (offset `0x080`) to 0 so all priorities are accepted.
- EOI is a write to offset `0x0B0`. Everything routed through the APIC gets a LAPIC EOI, never a PIC
  EOI. Getting this wrong stops the second interrupt from ever arriving.

## 5.8 Handler ordering rules

**EOI before any possible context switch.** The timer handler increments its counters, EOIs, and only
then calls `on_timer_tick` / the scheduler. A context switch with the interrupt controller still holding the
in-service bit means that source never fires again on the CPU that switched away.

**Rearm before yielding.** With a one-shot timer (TSC-deadline), the handler writes the next deadline
before calling the scheduler. Interrupts are already disabled inside the handler, so this is safe, and
skipping it loses a tick whenever the handler preempts.

**IF on resume.** `prepare_thread` seeds `rflags = 0x2` (IF clear). `schedule` applies
`apply_if_on_resume` to the incoming TCB before the restore: IF is set when `irq_nest == 0`, and kept
clear while an `InterruptGuard` is live on that stack. Without this, a first-run thread or a thread
saved from the timer ISR stays tick-deaf until some later `sti`. A thread resumed in the ISR still
gets IF back from `iret`. `switch_context` must not `popfq` with IF set and then `jmp`. A timer in
that one-instruction window preempts a first-run thread whose `CpuContext` is still the synthetic
trampoline frame; `schedule_preempt` overwrites it with a nested save; `iret` then `jmp`s to
`schedule_inner` on `stack_top-8`. Strip IF from the popped flags and `sti` immediately before `jmp`
(STI takes effect after the next instruction).

A post-EOI switch to a thread with `irq_nest == 0` therefore enables IF on the incoming thread even
if the preempted stack still has an open ISR / `InterruptGuard` frame. That is expected: the guard
lives on the outgoing stack and will drop (or `iret` will restore IF) when that thread resumes. Do
not "fix" this by inheriting the outgoing nest onto the incoming thread.

## 5.9 Later

- x2APIC, for more than 255 CPUs and MSR-based register access instead of MMIO.
- Interrupt affinity *rebalancing* (Phase 17). The dest-CPU table and `set_affinity`
  already exist; what is left is a policy that moves MSI-X messages and IOAPIC
  dests when a queue saturates one core.

---

# 6. Time

Four hardware clocks, none of them good at everything. The PIT is slow and legacy but always there.
The HPET is a reliable counter with no interrupts we want. The TSC is fast and fine-grained but needs
calibration. The LAPIC timer is per-CPU and is what actually drives preemption.

## 6.1 Roles and constants

| Source | Used for |
|--------|----------|
| PIT channel 0 | Bootstrap tick at ~1 kHz. Last-resort scheduler tick if the LAPIC timer cannot be used. |
| PIT channel 2 | TSC calibration when there is no HPET. One-shot, gated through port `0x61`. |
| HPET main counter | Preferred TSC calibration reference. Monotonic, known frequency from the ACPI table. |
| TSC | Sub-millisecond timestamps, `busy_wait_ms`, deadline arithmetic. |
| LAPIC timer | Per-CPU preemption tick. TSC-deadline mode preferred. |
| RTC / CMOS | Wall clock date and time, read once at boot. |

| Value | Meaning |
|-------|---------|
| `1_193_182` | PIT input frequency in Hz |
| `1_000` | Target tick rate, so PIT divisor is 1193 |
| `11_932` | PIT channel 2 count for a ~10 ms calibration window |
| `10` | Local timer ticks per scheduling slice, so ~10 ms |
| `0x40` / `0x42` / `0x43` | PIT channel 0 data / channel 2 data / command |
| `0x61` | Speaker gate. Bit 0 enables channel 2, bit 5 reads its output. |
| `0x6E0` | `IA32_TSC_DEADLINE`. Writing 0 disarms. |
| `CPUID.01H:ECX[24]` | TSC-deadline feature bit |

## 6.2 Calibrating the TSC

Read the reference counter, spin for a known interval, read again, divide. The reference is the HPET
main counter when ACPI provides an HPET table, otherwise PIT channel 2 over a 10 ms window.

Parse the ACPI HPET table properly: reject an address of zero and reject a generic address structure
that claims I/O space rather than system memory. Both appear in the wild and both produce a
"calibration" that is pure garbage, which then poisons every delay in the kernel including the ones AP
bring-up depends on.

Serialize around `rdtsc`. Out-of-order execution can move the read across the interval boundary. Use
`lfence` before, or `rdtscp`, which serializes on its own and also gives the CPU number.

Each CPU calibrates and stores its own `tsc_per_ms` in its per-CPU area. TSCs are usually synchronized
on modern hardware but the frequency measurement is not free of noise, and a shared global means one
CPU's bad sample skews every other CPU's delays. Check the invariant TSC CPUID bit and log loudly if
it is absent, because everything downstream assumes the TSC does not change rate.

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

Each fallback is worse than the one above it and all three must work, because the difference between
them is a QEMU flag and CI runs both. `-cpu qemu64,-tsc-deadline` forces the periodic path.

When the LAPIC timer owns the tick, mask the PIT's GSI at the I/O APIC. Do not merely ignore its
interrupts.

## 6.4 Timekeeping API

```rust
uptime_ms() -> u64      // tick counter, cheap, coarse
now_us()    -> u64      // tick counter plus TSC interpolation since the last tick
now_ns()    -> u64      // same, finer, for tracing
busy_wait_ms(ms: u64)   // TSC spin, hlt when interrupts are on. Boot and IPI delays only.
sleep_ms(ms: u64)       // parks the calling thread. Everything after the scheduler exists uses this.
```

`now_us` reads two values that an interrupt handler writes: the tick counter and the TSC snapshot at
that tick. Read them unprotected and you eventually get one from before an interrupt and one from
after, producing a timestamp that goes backwards. That is not hypothetical; it happened.

Publish them as a seqlock: the writer bumps a sequence number, writes both fields, bumps it again; the
reader retries until it sees a stable even sequence. Release ordering on the write, acquire on the
read.

Two warnings about testing this. A test that computes the expected "now" from the same tick value it
just read is monotonic by construction and passes even with torn reads, so the test needs an
independently published timestamp to compare against. And a single-threaded test will never see the
race at all, so the real coverage is an in-guest test hammering `now_us` across yields with a timer
firing underneath.

### Global monotonicity under SMP

Once every CPU has its own LAPIC timer, "the tick count" stops having a single writer. Options:

1. Only the BSP's timer updates the global counters; AP timers drive local scheduling only. Simple,
   and what the old tree did, but it makes `now_us` dependent on the BSP staying alive and awake.
2. Per-CPU time, with a global monotonic clock derived from the TSC alone once it is known invariant
   and synchronized.
3. A real distributed clock with cross-CPU synchronization and drift correction.

Option 2 is the destination: with an invariant, synchronized TSC and a single calibration constant,
`now_ns` is a `rdtsc` and a multiply with no shared state at all. Get there before relying on
timestamps for tracing, because option 1 will silently produce non-monotonic values the moment the BSP
goes idle in a deep C-state.

## 6.5 Timers and timeouts

The `sleep_ms` path needs a data structure, not a linear scan of every thread on every tick:

- Start with a single sorted list of pending timeouts guarded by one lock. Fine for tens of threads.
- Move to a hierarchical timing wheel when the count grows, or a per-CPU red-black tree keyed on
  expiry.
- Every blocking operation takes an optional deadline. A blocked thread with no timeout and no waker
  is a permanent leak, and the only way to find one is to have made timeouts mandatory from the start.

## 6.6 Tickless and wall clock

A fixed 1 kHz tick on an idle CPU is wasted interrupts and, on real hardware, wasted power. TSC-
deadline mode makes tickless operation possible: when a CPU goes idle, arm the deadline for the next
pending timer instead of the next millisecond, and skip the timer entirely if there is nothing pending.
Not day-one work, but the timer abstraction should be "next deadline" rather than "periodic tick" so
this does not require rewriting the scheduler.

The RTC gives date and time to one-second resolution over ports `0x70`/`0x71`, with the usual
century-register and BCD-versus-binary quirks to detect. Read it once at boot, then track time with the
monotonic clock and an offset. Poll the RTC for updates and the boot log timestamps drift relative to
each other in a way that is genuinely annoying to debug. NTP over the network eventually replaces the
offset with something correct.

---

# 7. SMP

Discovering the other cores through ACPI, starting them, and then keeping the kernel correct with more
than one of them running. This is where every latent locking mistake turns into a hang.

We bring up APs ourselves rather than using Limine's SMP request. Limine's version works, but the
trampoline, the INIT/SIPI dance, and the per-CPU handoff are the interesting part of the problem, and
owning it is required for CPU offlining and for a future non-Limine boot path.

Not a goal: CPU hotplug. CPUs are enumerated once at boot.

## 7.1 ACPI

Limine hands over the RSDP physical address. From there:

1. Validate the RSDP. Signature `"RSD PTR "`, then the checksum: v1 sums the first 20 bytes, v2 sums
   the full `length` and also checks the extended checksum. Both must be zero mod 256. Skipping this
   means following a garbage pointer into a region that is not memory.
2. Walk the XSDT (or RSDT on ancient firmware), validating each table's own checksum before trusting
   its contents.
3. Read `packed` ACPI fields with `read_unaligned`. ACPI tables have no alignment guarantees and a
   normal load can fault or silently read the wrong bytes.

| Table | Signature | Contents |
|-------|-----------|----------|
| MADT | `APIC` | LAPIC MMIO base, I/O APIC bases and GSI bases, interrupt source overrides, per-CPU APIC IDs |
| HPET | `HPET` | Main counter MMIO base, used for TSC calibration |
| FADT | `FACP` | `iapc_boot_arch` at offset 109, bit 0 says whether a legacy 8259 exists |
| MCFG | `MCFG` | PCIe ECAM base, needed for configuration space access |

MADT entry types in use:

| Type | Meaning |
|------|---------|
| 0 | Processor Local APIC. Flags bit 0 = enabled, bit 1 = online capable. |
| 1 | I/O APIC. MMIO address and global system interrupt base. |
| 2 | Interrupt source override. ISA IRQ to GSI, plus polarity and trigger. |
| 5 | Local APIC address override, 64-bit. Replaces the 32-bit field in the MADT header when present. |

A processor is startable if flags bit 0 is set. Bit 1 alone means it could come online later, which is
hotplug territory and out of scope.

## 7.2 LAPIC

Default MMIO base `0xFEE0_0000`, overridable by MADT type 5. Registers are 32 bits on 16-byte
boundaries. Enable through `IA32_APIC_BASE` (MSR `0x1B`): bit 11 is the global enable, bits 12–35 hold
the base, bit 8 is a read-only "this is the BSP" flag.

| Offset | Register |
|--------|----------|
| `0x020` | LAPIC ID, APIC ID in bits 24–31 |
| `0x080` | Task priority. Write 0 to accept everything. |
| `0x0B0` | EOI. Write any value. |
| `0x0F0` | Spurious vector. Enable bit 8, vector in bits 0–7. |
| `0x300` | ICR low. Writing this sends. |
| `0x310` | ICR high. Destination APIC ID in bits 24–31. |
| `0x320` | LVT timer. Mode in bits 17:18: 00 one-shot, 01 periodic, 10 TSC-deadline. |
| `0x380` / `0x390` / `0x3E0` | Timer initial count / current count / divide configuration |

ICR encoding:

- Delivery pending is bit 12 of ICR low. Poll until clear before sending the next IPI.
- INIT: delivery mode `0b101`, level assert bit 14.
- SIPI: delivery mode `0b110`, vector field holds the startup page number.
- Fixed IPI: delivery mode `0b000`, vector field holds the interrupt vector.

Bound the delivery-pending poll. A cap of 1000 iterations returning failure is enough; an unbounded
poll under `cli` is an unrecoverable hang with no output. Put the poll loop in the host-testable half
of the crate so the timeout logic has a unit test.

Relevant MSRs across this section:

| MSR | Meaning |
|-----|---------|
| `0x1B` | `IA32_APIC_BASE`. Bit 11 global enable, bits 12–35 base, bit 8 BSP flag. |
| `0x6E0` | `IA32_TSC_DEADLINE`. Write 0 to disarm. After an LVT timer write into TSC-deadline mode, `MFENCE` before this MSR. |
| `0xC000_0080` | `IA32_EFER`. Bit 8 LME, bit 11 NXE. |
| `0xC000_0101` | `IA32_GS_BASE`. Per-CPU struct pointer. |
| `0xC000_0102` | `IA32_KERNEL_GS_BASE`. Equal to `GS_BASE` until userspace and `swapgs`. |

## 7.3 AP trampoline

An AP comes out of SIPI in real mode at `CS:IP = vector<<8 : 0`, so the entry point must be a 4 KiB
aligned physical page below 1 MiB. We use `0x8000`, SIPI vector `0x08`.

The trampoline is assembled separately with `nasm -f bin` and included as a blob. It goes: real mode,
set up a GDT, enable protected mode, build page tables pointer from the passed CR3, set `EFER.LME` and
`EFER.NXE`, enable paging, long jump to 64-bit, load the stack, call the Rust entry point.

`EFER.NXE` matters. Kernel pages are mapped NX, and if the AP enters long mode without NXE the NX bits
are reserved-bit violations and the first kernel page it touches faults.

The BSP patches parameters into the tail of the blob:

| Offset | Field |
|--------|-------|
| `0xD0` | CR3 for the AP |
| `0xD8` | Stack top |
| `0xE0` | 64-bit Rust entry point |
| `0xE8` | IDT pointer, 10 bytes |

Two correctness requirements:

- Write each field with `write_volatile`, and put a `compiler_fence(SeqCst)` between the last write and
  the SIPI. A plain `copy_nonoverlapping` gives the compiler license to reorder the copy past the MMIO
  write that starts the AP, and the AP then reads uninitialized parameters. This one is invisible in
  debug builds.
- The trampoline page must be identity mapped and executable. The AP starts in real mode and touches
  physical `0x8000` directly, so this cannot go through the physmap. Reserve the frame in the PMM
  forever, even after all APs are up.

## 7.4 AP bring-up sequence

The BSP starts APs one at a time. They share the trampoline page, its parameter block, and nothing
else, so two concurrent SIPIs corrupt each other's stack pointer.

For each enabled APIC ID that is not the BSP:

1. Allocate the AP's per-CPU area and its guarded stack. Publish the per-CPU table entry and
   `compiler_fence(SeqCst)` before sending anything, so the AP can find itself.
2. Patch the trampoline parameters. Fence.
3. Send INIT. Wait 10 ms, the Intel-specified minimum.
4. Send SIPI. Wait ~1 ms. Send SIPI again. Some hardware needs the second one; sending two is harmless.
5. Wait for the AP to set its ready flag, with a 3 second timeout.
6. On timeout: free the stack and the per-CPU area, log the failure, continue with the remaining CPUs.
   Leaking a 16 KiB stack per failed AP is exactly the kind of thing that goes unnoticed for months.

On the AP side, in order: load per-CPU GDT and TSS, set `GS_BASE` and
`KERNEL_GS_BASE`, load the IDT, enable the LAPIC, calibrate and arm the LAPIC timer, publish the ready
flag, `sti`, enter the scheduler as the idle thread.

`GS_BASE` must be set before any `lidt` and before `sti`. NMI and timer IRQs both
read per-CPU state through `gs:[0]`. Setting it after `lidt` is a null dereference
waiting for a non-maskable interrupt, even with IF off.

## 7.5 Per-CPU data

One `PerCpu` struct per CPU, reached through `GS_BASE`. `KERNEL_GS_BASE` is set to the same value until
userspace exists and `swapgs` becomes meaningful.

`self_ptr` sits at offset 0 so `gs:[0]` yields the struct address, which is how a `&PerCpu` is obtained
without knowing which CPU you are on.

Contents:

- logical cpu id and APIC id
- ready queue
- wake inbox for work handed over from other CPUs
- idle thread handle, current thread pointer
- local tick count, context switch count
- `tsc_per_ms` and timer mode
- reserved scratch for the syscall entry path

Phase 3 slice A installs one BSP `PerCpu` through `GS_BASE` and
`KERNEL_GS_BASE` (not a `MAX_CPUS` array, no `swapgs`). Slice B allocates
the array from the MADT CPU count, brings up APs, and fills timer mode,
wake inbox (stub), and `ready`. `current` and `idle` are `*mut Tcb`. Slice C owns per-CPU `runq` plus
`wake_inbox`; `ready_head` is that CPU's FIFO head. Cross-CPU wake is
inbox + IPI `0xFD`.

Allocate the array on the heap once the CPU count is known from the MADT rather than sizing a static
array by a `MAX_CPUS` guess.

`PerCpu::current()` is safe from an ISR because the GS base never changes on a given CPU. Do not use
`swapgs` in kernel-entry ISRs until user mode exists, and when it does, do it in exactly one place.

## 7.6 IPIs

| Vector | Purpose |
|--------|---------|
| `0xFB` | Call function. Run a closure on a target CPU, optionally waiting for completion. |
| `0xFC` | TLB shootdown. |
| `0xFD` | Reschedule. Target CPU re-evaluates its run queue, waking from `hlt` if idle. |
| `0xFE` | Panic halt. Broadcast so a panic on one CPU stops the others before they overwrite the log. |

The reschedule IPI is what makes cross-CPU wakeups work without ever locking a remote run queue: push
onto the target's inbox, send `0xFD`, done. `0xFD` then takes SCHED IRQ-off via `schedule_preempt`.
Shootdown and call-function take neither the page-table lock nor SCHED. Call-function uses one global
slot; the initiator holds IF off from publish through reclaim, polling inbound work while it waits.

## 7.7 Locking with more than one CPU

The global lock order is in [section 2.1](#21-lock-order) and the one-spinlock rule in
[section 2.3](#23-locking-with-interrupts). Additions specific to SMP:

- Never lock a remote CPU's per-CPU state. Per-CPU locks are taken only by the owning CPU, with
  interrupts off.
- If two CPU-local structures must be locked at once, for instance during load balancing, lock the
  lower `cpu_id` first.
- A lock taken from an ISR is taken with interrupts disabled in every other context too. The scheduler
  lock is the canonical case: the timer ISR calls into the scheduler, so any holder with interrupts
  enabled deadlocks the moment its own timer fires.
- Serial TX takes a lock so bytes from different CPUs do not interleave into unreadable garbage. Byte
  granularity, not line granularity; full line atomicity needs per-CPU buffers and a printer thread.
  Slice A ships a global IRQ-safe log ring plus a serial try-lock sink; the printer thread is parked
  (Design ACK). Per-CPU serial capture still assembles lines into the ring. The global SCHED lock,
  one `SpinMutex` on the block cache, one VFS lock, the log-ring TAS, and virtio-blk bounce copies
  are known scale limits; see ROADMAP §17.

## 7.8 Per-CPU scheduling

Global TCB table, per-CPU ready queues.

- Threads carry an affinity: `Any` or `Pinned(cpu)`.
- `spawn` with `Any` places the thread round-robin across CPUs. Good enough to start; measurably not
  good enough later.
- Waking a thread on another CPU goes through the inbox plus reschedule IPI.
- No work stealing in the first version. Add periodic load balancing first, since it is simpler to
  reason about, then a proper lock-free deque if the numbers justify it.
- Each CPU has its own idle thread with its own stack. An idle CPU sits in `sti; hlt` and is woken by
  the reschedule IPI.

The sleep queue starts global with one lock. Per-CPU timer queues are the right answer eventually but
they interact with thread migration in ways that are not worth solving early.

## 7.9 TLB shootdown

Kernel mappings are `GLOBAL` and therefore live in every CPU's TLB. Unmapping one requires every CPU
to invalidate before the virtual address is reused.

Protocol: update the PTE, then broadcast `0xFC` with the target address, then wait for acknowledgement
from every online CPU.

The initiator waits with interrupts disabled (`InterruptGuard` around publish → IPI → ack → clear
waiters). A waiter with IF off cannot take an incoming shootdown as an IRQ, which would deadlock two
CPUs shooting down at once. The wait loop therefore calls `service_incoming` and processes pending
slots so a spinning initiator still helps its peers. This is not optional; it is the difference
between working and a hang that only appears under load.

The shootdown handler must not allocate and must not take the page table or scheduler lock. It reads a
request slot and executes `invlpg`.

## 7.10 Verification and later work

SMP bugs are timing dependent, so the tests matter more than usual:

- `-smp 2` is the default for every QEMU invocation including the normal boot test. One AP catches most
  bring-up bugs.
- `-smp 4` as a separate target to shake out sequencing assumptions that hold for exactly one AP.
- `-cpu qemu64,-tsc-deadline` to force the LAPIC periodic path.
- In-guest tests that need real CPUs: per-CPU identity on BSP and AP, cross-CPU thread spawn, reschedule
  IPI delivery, waking an idle AP, remote unmap and remap through the shootdown path.
- `qemu` monitor `info cpus` and `info lapic` for interactive debugging, plus `-d int,cpu_reset` when a
  core is triple faulting and you need to see why.

Later:

- x2APIC. MSR-based register access, no MMIO, and APIC IDs beyond 255.
- Topology awareness: cores, threads, packages, and cache sharing from CPUID leaf 0x1F, so the
  scheduler can prefer a sibling core over a remote package.
- NUMA. SRAT and SLIT parsing, per-node buddy allocators, node-local allocation policy.
- CPU offlining for power management, which needs the reverse of bring-up: migrate threads, redirect
  interrupts, park the core.

---

# 8. Testing

Three tiers. Each catches a class of bug the others cannot, and each is progressively slower, so the
decision of where a test goes matters.

| Tier | Runs | Speed | Catches |
|------|------|-------|---------|
| Host unit | `cargo test --lib`, on the dev machine | milliseconds | Algorithms: allocators, parsers, state machines, encodings, arithmetic |
| In-guest (ktest) | QEMU, kernel built with the `kernel_tests` feature | seconds | Anything needing real hardware state: page tables, MMIO, interrupts, threads, SMP |
| End to end | QEMU boot of the normal ISO, serial captured | ~10 s | Boot regressions, marker ordering, panics, subsystem interaction |

The routing rule: if it can be a host test, it must be. Pushing logic into the library half of the
crate so it becomes host-testable is the highest-leverage thing available, and the old tree's biggest
weakness was that nearly everything lived behind `main.rs` and was therefore untestable.

## 8.1 Host unit tests

Anything in `src/lib.rs` and its submodules. No hardware access, no `unsafe` port I/O, no MMIO. The
kernel half calls into it.

Things that belong here and are easy to get wrong, so should have tests from the day they are written:

- Buddy allocator: split, merge, exhaustion, fragmentation, alignment per order, free count returning
  to its initial value after a random alloc/free sequence, double free detection.
- ACPI: RSDP v1 and v2 checksum rejection, table length validation, HPET generic address structure
  rejecting I/O space and zero addresses, MADT entry iteration over truncated tables.
- Timekeeping: the `now_us` interpolation formula, seqlock retry under a simulated concurrent writer,
  monotonicity, overflow near `u64::MAX`, HPET/PIT agreement bands (invariant vs TCG).
- ICR delivery-pending poll: returns true when the bit clears, false at the iteration cap.
- Vector table: no two named vectors are equal.
- Scan code decoding: make and break codes, `0xE0` prefixes, modifier state, unknown codes returning
  `None` rather than panicking.
- Ring buffer: wrap-around FIFO order, full and empty boundaries, overwrite-oldest semantics.
- Line editor and command tokenization: quoting, whitespace, empty input, unknown commands.
- Font: every printable ASCII code point yields eight rows.
- `align_up` and friends at 0, at exactly aligned, and near overflow.
- VFS path walk: `.` / `..`, bounded symlink depth, symlink loop → error, negative dentry
  invalidate-on-create, mount-point crossing.
- kernfs: one directory implementation shared by devfs/tmpfs/procfs/sysfs; tmpfs
  writes evict through the Phase 7 block cache rather than pinning a grow-only
  buffer; `/dev/null` `/dev/zero` `/dev/random`; procfs stubs do not panic.

Two lessons about writing these:

A test that derives its expected value from the same read it is checking proves nothing. The old
seqlock test computed the expected timestamp as `tick + 100`, which is monotonic no matter how torn the
read was, so it passed against a broken implementation. The writer has to publish an independent value
the reader can compare against.

A single-threaded test cannot observe a race. Simulate the interrupt-context writer explicitly, or
accept that the real coverage is in-guest.

## 8.2 In-guest tests

A second kernel build with `--features kernel_tests` that boots normally, runs a registry of test
functions after init, reports over serial, and exits QEMU through the `isa-debug-exit` device.

Built into a separate Cargo target directory (`target-kernel-tests`) with its own ISO. This is not
fussiness: sharing a target directory means a feature-enabled ELF can end up packaged into the
production ISO, and the difference is not visible from the outside. The panic-dump and `#GP` ISOs
are `--features panic_test --features panic_exit` and `--features gp_test --features panic_exit`
(underscores everywhere; Cargo features in this crate do not use hyphens).

```
vibeOS: ktest: begin
vibeOS: ktest: ok <name>
vibeOS: ktest: FAIL <name>
vibeOS: ktest: skip <name>: <reason>
vibeOS: ktest: end
```

The harness requires `begin` and `end`, rejects any `FAIL` line, and checks the exit status.
`isa-debug-exit` at I/O port `0xf4` maps a written value to host exit status `(value << 1) | 1`:

| Write | Host exit | Meaning |
|-------|-----------|---------|
| `0x10` | 33 | all tests passed |
| `0x11` | 35 | at least one test failed |

Skips are first class and must carry a reason. Two known ones, both real limitations rather than
laziness: a bare `ud2` test cannot run while `#UD` is a halting handler, since it aborts the whole run
(it needs a scoped transient handler that steps RIP past the faulting instruction), and a serial
loopback test cannot run under `-serial stdio` because that chardev is one-way.

When a test fails, print enough to diagnose it without a rerun. A failing test that only prints its
name costs a full debug cycle to learn anything.

Keyboard IRQ regressions (#66). `kbd_gsi_unmasked` requires a live IOAPIC route (fails if PIC IRQ1
is the fallback after the LAPIC already masked the 8259). `kbd_8042_clock` reads the live controller
byte (clock on, INT1 on). `kbd_ps2_irq` writes 8042 command `0xD2` (present the next data byte as
keyboard input) with scancode `0x1E` and expects `a` on the PS/2 ring after pulsing IF — handler,
INT1, GSI, ISR, decoder. Command `0xD2` does not exercise the device clock; that is
`kbd_8042_clock` plus e2e / `make test-ps2` `sendkey`. Serial mux cannot satisfy `kbd_ps2_irq`.

## 8.3 End to end

Boot the real ISO, capture serial, assert the boot contract. This is the test that notices when
something two subsystems away breaks.

### Marker contract

This is the full contract once the kernel is complete through the console phase. It grows one phase at
a time: a phase adds its markers to the harness in the same commit that emits them, and nothing is ever
removed silently. The authoritative ordering is the `_start` table in [section 3.3](#33-_start-order).

```
vibeOS: serial online
vibeOS: limine: rev 3 ok
vibeOS: pmm: <n> free 4KiB frames
vibeOS: paging: cr3 ok
vibeOS: paging: mmio uc
vibeOS: heap ok
vibeOS: kva: ready
vibeOS: gdt ok
vibeOS: pic: remapped
vibeOS: idt ok
vibeOS: per_cpu: bsp ready
vibeOS: acpi: xsdt <n> tables
vibeOS: time: tsc <n>/ms
vibeOS: sched: cpu0 ready
vibeOS: irq: enabled
vibeOS: smp: done
vibeOS: console ok
vibeOS: pci: <n> devices
vibeOS: block: <name> <n> sectors
vibeOS: shell ready
```

Live e2e through Phase 6 slice A asserts through `idt ok`, then `per_cpu: bsp ready`,
then `acpi: xsdt`, then `time: tsc <n>/ms`, then `time: lapic_timer ok (<mode>)`, then
`sched: cpu0 ready`, then `irq: enabled`, then for each AP `sched: cpu<i> ready`
followed by `smp: ap online`, then `smp: done`, then `console ok`, then
`pci: <n> devices`, then `block: <name> <n> sectors`, then `shell ready`.
`boot: phase1 done` was a Phase 1–4 stand-in and is no longer in the contract; the
trailing marker is `shell ready`. After that, the same ISO is booted again and the
harness types `echo serial-ok` on COM1 and `echo ps2-ok` via QEMU `sendkey` (i8042 /
IRQ1, the window-keyboard path). Both replies are required. `make test-ps2` is that
second boot alone. SMP stays before console; the old
table that listed console as step 15 before SMP was drift and is gone.
The harness pins `<mode>` for the QEMU config: TCG (CI, `make test`) cannot
advertise `CPUID.01H:ECX[24]`, so `-cpu max` expects `periodic`; `-machine pc,hpet=off`
expects `pit`; KVM `-cpu max` expects `tsc-deadline`. Default QEMU also requires
the diagnostic `time: calibrated hpet <n>/ms`; `make test-e2e-pit` asserts
`calibrated pit` instead. `make test-lapic-fallback`
(`-cpu qemu64,-tsc-deadline`) runs in-guest tests on the periodic path.

`smp: done` before `shell ready` is deliberate. Put SMP bring-up after the shell starts and an AP
failure becomes invisible, because the harness sees its last marker and passes. `pci: <n> devices`
sits between `console ok` and `shell ready` so `lspci` is registered before the prompt. The ramdisk
`block: <name> <n> sectors` line sits after PCI and still before the shell. Partition children emit
`block: <parent>p<N> <n> sectors` after the parent (e2e: `ram0p1`, `ram0p2`). virtio-blk adds
`block: vda <n> sectors` and `vdapN` when the ktest disk is present (not on the production e2e `pc` set).

With `-smp N`, additionally:

- exactly `N-1` occurrences of `vibeOS: smp: ap online`
- `vibeOS: sched: cpu<i> ready` for every `i` in `0..N`
- `vibeOS: time: lapic_timer ok (<mode>)` naming the selected timer path
  (`tsc-deadline`, `periodic`, or `pit`) rather than inferring it

### Failing fast

Scan for these and fail immediately with the captured line rather than waiting out the timeout:

```
panicked at   #PF   #GP   #UD   #DF   double fault   stack overflow
```

Match the exception mnemonics, not the phrase "page fault". Shell help text and log messages contain
English words, and a substring match on prose produces false failures that erode trust in the suite.

Expected-panic e2e waits for `vibeOS: panic: halted` so the dump (regs, thread, last log records,
backtrace) is in the captured log, then checks dump needles. `panic_exit` writes isa-debug-exit
`0x11` so QEMU leaves instead of sitting in `hlt`.

On success, exit through the QEMU monitor's `quit` rather than waiting for the timeout. Two seconds
versus forty five, on every CI run and every local invocation.

### Harness

Python, standard library only. `subprocess` with its own timeout rather than shelling out to GNU
`timeout`, which does not exist on macOS. The harness helpers get their own unit tests, because a bug
in the test harness produces either false confidence or a debugging session in the wrong repository.

## 8.4 QEMU flags

| Context | Flags |
|---------|-------|
| `make run` | `-cdrom myos.iso -m 128M -smp 2 -cpu max -serial stdio -accel tcg` |
| e2e | as above plus `-display none -no-reboot -monitor unix:...,server=on,wait=off` |
| ktest | as e2e plus `-device isa-debug-exit,iobase=0xf4,iosize=0x04`, `-device e1000e`, `-device edu`, `-device virtio-rng-pci,disable-legacy=on`, virtio-blk (`-drive file=…,if=none,id=vibehd` + `-device virtio-blk-pci,drive=vibehd,disable-legacy=on,num-queues=<smp>`). Extra NICs/edu/virtio are ktest-only; e2e stays the default `pc` set (`pci: 6 devices`). After a green first boot the harness reboots the same disk and requires `vibeOS: persist: intact`. |
| LAPIC fallback | `-cpu qemu64,-tsc-deadline` |
| SMP stress | `-smp 4` |
| Interrupt debugging | `-d int,cpu_reset`, plus `-machine q35` when chipset behavior matters |

Harness and `make test` default to `-accel tcg` so KVM does not introduce timing flakes.

`-no-reboot` matters: a triple fault otherwise reboots and loops, and the serial log fills with
repeated boot attempts instead of stopping at the interesting one.

All `VIBEOS_*` overrides are read in `tests/harness/harness.py` (`env_config` / `env_flag` /
`env_int`). Drivers do not parse the environment. Makefile `?=` values are the `make run` source;
harness defaults match them.

| Variable | Default | Who honours it |
|----------|---------|----------------|
| `VIBEOS_ISO` | per driver (`vibeos.iso`, `vibeos-ktest.iso`, `vibeos-vibefs-crash.iso`) | all drivers |
| `VIBEOS_SMP` | `2` | all; `make run` |
| `VIBEOS_QEMU_CPU` | `max` | all; `make run` |
| `VIBEOS_MEM` | `128M` | all; `make run` |
| `VIBEOS_BIOS` | unset (SeaBIOS) | all |
| `VIBEOS_QEMU_ACCEL` | `tcg` (empty omits `-accel`) | all; `make run` |
| `VIBEOS_TIMEOUT` | `60` e2e/ps2, `90` ktest/crash | all drivers |
| `VIBEOS_QEMU_EXTRA` | empty | all drivers |
| `VIBEOS_EXPECT_PANIC` | off (`""` / `0`) | `run_e2e` |
| `VIBEOS_GP_TEST` | off | `run_e2e` |
| `VIBEOS_EXPECT_PIT` | off | `run_e2e` |
| `VIBEOS_SKIP_PERSIST` | off | `run_ktest` |
| `VIBEOS_CRASH_ROUNDS` | `8` | `run_vibefs_crash` |
| `VIBEOS_CRASH_SEED` | time-based | `run_vibefs_crash` |
| `VIBEOS_MKFS` | `mkfs-vibefs` | `run_vibefs_crash` |
| `VIBEOS_FSCK` | `fsck-vibefs` | `run_vibefs_crash` |

## 8.5 Make targets

`make help` prints the live inventory. Do not hand-maintain a second list here.

`make check` is the fast local gate (host clippy, host unit tests, harness unit tests, ruff/mypy when
installed). rustfmt `--check` and clippy `-D warnings` land with Q1. `make test-e2e` is enough when
only boot output or QEMU wiring changed. `make test` is the gate before a PR. `make test-ps2` is the
focused #66 sendkey boot; `make test-e2e` already runs it, so `make test` does not boot it twice.

## 8.6 CI and coverage

Runs on every push and pull request, on Linux, from the first commit. Bootstrap Limine, install
`qemu-system-x86`, `nasm`, `xorriso`, then run the full ladder: host units, harness units, ISO build,
e2e, in-guest at `-smp 2` and `-smp 4`, and the LAPIC fallback variant.

CI existing from day one is a deliberate reordering versus the old tree, where it arrived late enough
that several regressions shipped in between.

Additions as they become relevant: `clippy` with warnings denied, `rustfmt --check`, a coverage floor
on the library half via `cargo-llvm-cov`, and a nightly long-running variant with more CPUs and more
memory pressure.

Coverage is not a percentage target, it is a rule: every bug that gets fixed gets a test that would
have caught it, in the cheapest tier that can catch it. Every entry in [section 9](#9-pitfalls) names
the rule that guards it, and where that rule is only an invariant in code with no test, that is a
weaker guarantee and should be visible as such.

---

# 9. Pitfalls

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

**Build works from the repo root and fails from anywhere else.**
`build.rs` invoked `nasm` on a relative path. Rule: anchor build script paths to `CARGO_MANIFEST_DIR`,
and capture the assembler's stderr into the build output so the failure is readable.

**A Limine response pointer is null and the kernel dies with no explanation.**
The request static was not in the `.limine_requests` section, so the loader never saw it. Rule: every
request is `#[used]` with an explicit `link_section`, and the base revision is verified before any other
response is read.

**Panic backtrace addresses have no names, or the second link moves every RIP.**
The symbol table lived in `.text` or was patched in place. Rule: first link with an empty `.rodata`
table, `nm --demangle` the ELF, second link with the filled table. `.text` must not move. The
two-pass lives in `KERNEL_VARIANT`; commas in `-Zbuild-std` stay in `BUILD_STD` and recipes
reference `$$(CARGO)` so `$(call …)` does not split them.

**QEMU framebuffer reprints the prompt on every key; serial looks fine.**
The FB write path skipped `\r` before the text grid saw it, so the line editor's in-place paint
(`\r` + rewrite) homed only on serial. Rule: `\r` sets column 0 on the same row; do not drop it.
Host: `cr_homes_column_same_row`, `cr_paint_overwrites_in_place`. In-guest: `fb_cr_home`.

## 9.2 Memory

**AP bring-up hangs with no output, or faults at a low address.**
The low identity window was mapped with NX on 2 MiB pages, and the AP fetched the trampoline at
`0x8000` after enabling paging. Rule: the first 2 MiB of the identity window is executable. Everything
else stays NX.

**Building page tables at boot never finishes.**
`map_end` was computed from raw memory map entries, and firmware described an MMIO BAR as a
multi-terabyte region. Rule: derive the physmap extent from usable RAM, kernel image end, and
framebuffer extent, and cap it (8 GiB). PCI BAR size probes that return > 32 MiB are recorded
and not page-walked into the ioremap window or physmap.

**Device reads return stale values on real hardware but work in QEMU.**
MMIO reached through a write-back physmap mapping. QEMU does not enforce cache attributes; hardware
does. Rule: LAPIC, I/O APIC, HPET, and every device MMIO page gets PCD + PWT, patched immediately after
CR3 install and before first access. Preserve the 2 MiB page size when patching rather than splitting.
Do not UC-patch the console framebuffer when it aliases VGA BAR0; leave that physmap WB.

**Config space beyond bus 0 is all `0xFFFF`.**
Legacy `0xCF8`/`0xCFC` only addresses bus 0 on typical host bridges. Rule: walk MCFG and use ECAM
(`base + (bus<<20)|(dev<<15)|(fn<<12)|off`) for any bus outside that. Type-1 headers reuse BAR
slots as bus-number registers; size-probe only the BAR count for that header type.

**Two subsystems designed for the same virtual address range.**
The heap and the kernel VA allocator were both specified at `0xFFFF_C000_*` in different documents, and
only one of them noticed. Rule: the address map in [section 4.1](#41-virtual-address-map) is the single
source of truth, and every region asserts its range is unmapped before claiming it.

**Allocator corruption with a crash in an unrelated subsystem.**
Buddy free list nodes live inside free pages, and a kernel stack overflow wrote into one. Rule: every
kernel stack gets an unmapped guard page below it, and stack overflow is a page fault rather than
silent corruption.

**A PTE edit appears to have no effect.**
No `invlpg` after the edit. Rule: `invlpg` after any single-PTE modification, including MMIO attribute
patches. Kernel mappings are `GLOBAL` and do not fall out of the TLB on a CR3 reload.

**`meminfo` is slow.**
`free_page_count()` walked the free lists. Rule: maintain a running counter.

**Freeing a stack while running on it.**
An AP's stack was unmapped while it was still executing on it. Rule: dead stacks go on a deferred reap
list, drained by a thread that is not on them.

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
a failed kick, not a store into some other register.

**Device sees a virtqueue index and stale descriptors.**
`avail.idx` was published with a compiler fence. Rule: descriptor stores, then `dma_wmb` /
`fence(Release)` + `sfence`, then the index. Used-ring harvest is `dma_rmb` after observing `used.idx`.

**Allocate or block in a hard-IRQ / MSI handler.**
The top half ran `Box` / `sleep` / `WaitQueue` wait. Rule: ack, set pending, wake the IRQ thread or
enqueue work. The thread may alloc and block ([section 2.2](#22-interrupt-handler-rules)).

## 9.4 Concurrency

**Deadlock the moment the timer starts firing.**
The scheduler lock was taken without disabling interrupts, and the timer ISR calls into the scheduler.
Rule: any lock reachable from an ISR is taken with interrupts disabled in every context. There is one
spinlock type and it is IRQ-aware.

**A thread blocks forever despite a wakeup being sent.**
The wakeup arrived in the window between deciding to block and actually blocking. Rule: enqueue onto
the wait queue, mark self blocked, drop the inner lock, then schedule. In that order, so there is no
point where the thread is both on the wait queue and considered runnable.

**Wait queue cookie is a dangling pointer.**
`ThreadState::Blocked { wq }` stores the `WaitQueue` address so timeout can unlink. The object that
owns the queue (mutex, rwlock, condvar, channel) must outlive every waiter. Dropping it with threads
still blocked is a use-after-free on the next timeout or wake.

**Condvar waiter never sees the predicate.**
Wake does not carry the condition. Mesa: `wait` re-acquires the mutex and returns; the caller loops
on the predicate. Timeout is the same path.

**Condvar wait parks still holding the mutex.**
`begin_wait` marked Blocked, SCHED dropped, then `drop(guard)` released the mutex. A timer in that
window switched the waiter off-CPU still owning it; the notifier blocked on the mutex forever. Rule:
enqueue on the CV and unlock the mutex under the same SCHED, then schedule.

**First-run thread `#PF`s in `schedule_inner` at `rsp = stack_top-8`.**
`popfq` restored IF before `jmp` to the trampoline. A tick landed in that window, `schedule_preempt`
saved over the synthetic frame, and `iret` jumped to the nested save's RIP with the prepared RSP.
Rule: delayed `sti` immediately before `jmp`; never `popfq` with IF set across a stack switch.

**Two `&mut T` from the same mutex in release builds only.**
The spinlock's re-entrancy check was a `debug_assert!`. Rule: real CAS spin loop, and any invariant
that must hold in release is an `assert!`.

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
`now_ns` below the last reading. Do not cap extra at one tick — ktest holds IF off and timeouts must
still advance on TSC alone.

**`sleep_ms(50)` and PIT-vs-HPET calib flake on TCG SMP.**
TCG has no invariant TSC. Boot HPET calibration runs before APs; a later PIT channel 2 window sees a
different apparent TSC rate, and LAPIC periodic ticks coalesce so `uptime_ms` during a sleep is not
50–100. Rule: in-guest checks key off the invariant-TSC CPUID bit. Without it, retry PIT against a
fresh HPET sample with a 50–200% band, and accept `now_us` (~50 ms) when ticks coalesce. Do not loosen
the invariant-TSC path.

**Serial output from multiple CPUs is unreadable.**
No lock on TX. Rule: lock serial TX. Byte granularity is enough to keep bytes from interleaving; full
line atomicity is a separate, later problem.

**Two CPUs performing a TLB shootdown at the same time hang.**
Both waited with interrupts disabled for acknowledgement the other could not send. Rule: the wait loop
also services incoming shootdown requests.

## 9.5 SMP bring-up

**An AP reads garbage parameters and dies.**
The trampoline parameter block was written with a non-volatile copy, and the compiler was free to
reorder it past the MMIO write that sent the SIPI. Rule: `write_volatile` per field, plus
`compiler_fence(SeqCst)` before the SIPI.

**Two APs corrupt each other.**
They shared the trampoline page and its parameter block. Rule: start APs one at a time and wait for
each ready flag before the next.

**An AP faults on the first kernel page it touches.**
The trampoline entered long mode without setting `EFER.NXE`, so NX bits in kernel PTEs were
reserved-bit violations. Rule: the trampoline sets NXE along with LME.

**Free memory shrinks by 16 KiB per boot on a flaky machine.**
Failed AP bring-up left its stack and per-CPU area allocated. Rule: the timeout path frees everything
it allocated.

**A null dereference in an ISR shortly after an AP comes up.**
`sti` happened before `GS_BASE` was set, and a timer interrupt landed in code that reads per-CPU state.
Rule: per-CPU MSRs are set before the IDT is live and before `sti`.

**A CPU never sees itself in the per-CPU table.**
The table entry was published without a fence before the SIPI. Rule: publish, fence, then start.

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
match exception mnemonics (`#PF`, `#GP`, `#DF`, `#UD`) and `panicked at`.

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
place, this file, and a change updates it in the same commit.

**Critical bugs identified, documented, and never fixed.**
The old tree carried four issues marked critical, with named regression tests planned for each, for the
rest of its life. Rule: a bug that is understood well enough to write down is fixed or explicitly
deferred with a roadmap line. "Documented" is not a state a critical bug gets to rest in.

---

# 10. Block I/O

Phase 7. Portable types live in `src/block.rs`, `src/part.rs`, and
`src/cache.rs`. Kernel ramdisk, waiters, and the boot marker live in
`src/block_init.rs`. virtio-blk packing is `src/virtio_blk.rs`;
the driver is `src/virtio_blk_init.rs`. Partition children are
`src/part_init.rs`. The write-back cache is `src/cache_init.rs`.

## 10.1 Completions

A request carries waiter cookies, not a locked queue. Submit takes the per-device
queue lock (RANK_DEVICE), merges or enqueues, and drops the lock before any copy.
The ramdisk pump runs after that drop; virtio-blk submits to a virtqueue after
the same drop and completes from a threaded IRQ. `complete` stores the status then wakes
waiters under SCHED. Never hold the queue lock across I/O or across that wake
(DEVICE then SCHED is the wrong order). Hard IRQ must not run this path: enqueue
work only (DESIGN [§2.2](#22-interrupt-handler-rules)). Ramdisk backing is BSS, not
a heap `Vec`: allocating under RANK_DEVICE would take RANK_HEAP (lock order).

Blocking wait and async submit share the same cookie. The buffer and waiter must
outlive the request.

## 10.2 Barrier vs flush

`Barrier` is an order fence: every request submitted before it finishes before any
request submitted after it starts. It does not make writes durable.

`Flush` is a barrier plus a device durable-write. Ramdisk flush is a successful
no-op (the backing is already memory). A journaling filesystem must issue flush,
not only barrier, before treating a commit as persistent. vibefs is not journaled:
CoW metadata plus atomic superblock switch ([VIBEFS.md](VIBEFS.md) §2 / §10). The
same flush rule applies: a generation is not durable until `Flush` after the new
super slot is written.

The elevator will not dispatch seq numbers past an in-queue fence. Adjacent
read/write/discard requests merge; fences do not, and they split merge runs.

## 10.3 Failure

I/O errors retry up to `DEFAULT_RETRY_BUDGET` extra attempts, then the device
goes `Failed`. Further submits return `Failed`. `Inval` (range, size) is not
retried and does not fail the device. No infinite retry loop.

Logical block size is per device. Do not assume 512. Capacity is in those
blocks. Discard on ramdisk validates the range and otherwise no-ops.

## 10.4 virtio-blk

Modern virtio-blk (`1af4:1042`, `VERSION_1` required) binds by id on the
Phase 6 transport. Config reads capacity (512-byte units), `blk_size` (512
if `F_BLK_SIZE` is absent), and topology when offered. Each request is a
descriptor chain: header + data (or discard range) + status. The status
byte is device-writable DMA, never a stack slot. Completions harvest the
used ring on the threaded IRQ and wake the same `IoWaiter` cookies as the
ramdisk. Hard IRQ only acks ISR.

`F_MQ`: one virtqueue per online CPU, capped by the device `num_queues`.
Without `F_MQ`, a single request queue. Flush and discard go to the device
when those features are negotiated; flush without `F_FLUSH` is a successful
no-op (nothing to make durable). Kick uses the Phase 6 notify formula and
`dma_wmb`; the blk driver does not invent a second fence.

## 10.5 Partitions

MBR (primary + extended/logical) and GPT parse in `src/part.rs`. Protective
MBR type `0xEE` is not a data device; GPT is. Header and entry CRCs are
checked; a bad primary falls back to the backup header at the last LBA.
EBR walk is capped at 128; a corrupt next-LBA stops the chain.

Children are offset-limited `BlockDevice`s. Child LBA `l` maps to
`start + l` and I/O past `nsectors` is `Inval`. Marker
`vibeOS: block: <parent>p<N> <n> sectors` (e.g. `ram0p1`, `vdap1`).

## 10.6 Block cache

Page-granular (4 KiB), keyed by `(dev_id, page offset)`. Read-through,
write-back, clock eviction, sequential readahead, dirty-ratio writeback
thread. `flush` writes dirty pages then calls the device `flush`.
`barrier` writes dirty pages and does not device-flush.

The cache sits above `BlockDevice`. Drivers are the miss
path. Phase 10 page cache reuses these pages (same clock, same writeback);
do not grow a second private cache. Hit/miss/device-request counters are
in the `blk` shell command. The cache lock is RANK_DEVICE and is dropped
before blocking device I/O.
