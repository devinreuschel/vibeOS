# Design

vibeOS is a monolithic kernel in Rust for x86_64 and, from ROADMAP Phase 11, aarch64 as a peer
([§11](#11-portability)). `no_std`, `alloc` enabled once the heap is up. Limine boots the ELF in long
mode (at EL1, or at EL2 with VHE, on aarch64); the kernel then takes over its own page tables and never
looks back at firmware except through ACPI tables (a device tree on aarch64 until ROADMAP §20.7).

Monolithic on purpose: drivers, filesystems, and the network stack run in the kernel's address space,
as Linux's do. Why: vibeOS implements Linux's interfaces (ROADMAP, How to read this), and each is
specified as a call into one kernel, so the shortest path to self-hosting, and to being measured
against Linux in the same VM, is to implement them where Linux does. Rejected:
- a microkernel, which would restate every Linux interface as messages between servers and spend the
  project on IPC design before any of it runs;
- a hybrid with drivers in user space, which no QEMU device model needs;
- loadable modules (ROADMAP Non-goals).

Cost: a driver bug is a kernel bug. The design answers with Rust's type rules (AGENTS.md rules 4 and
6), an IOMMU domain per device from ROADMAP §18.1, fuzzing of every parser that reads device or disk
data, and Phase 38's proofs, not with address-space isolation.

This file records decisions. [ROADMAP.md](ROADMAP.md) records what has landed. Present tense
describes the code at the commit that last changed the sentence. A rule the code does not meet yet
says "Rule; not yet enforced" or "Planned" and names the ROADMAP line that lands it. An `Fnnn` id
names a finding in the kernel review ([reviews/KERNEL_REVIEW.md](reviews/KERNEL_REVIEW.md)); the ROADMAP
section cited with it lands the fix. When code and
this file disagree, one of them is a bug ([§1.4](#14-documentation-rules)). The numbers are
load-bearing. Change them deliberately and update this doc in the same commit. An agent implementing
a subsystem does not get to re-litigate the address map, the vector numbers, or the lock order
halfway through.

## Contents

| | Section | Covers |
|---|---------|--------|
| 1 | [Overview](#1-overview) | Constraints, layers, module map, sources and licenses |
| 2 | [Invariants](#2-invariants) | Lock order, handler rules, panic policy, markers, invariant register, publish last, preemption, trust boundaries, object lifetimes |
| 3 | [Boot](#3-boot) | Toolchain, Limine, `_start` order, linker |
| 4 | [Memory](#4-memory) | Address map, buddy allocator, paging, heap |
| 5 | [Interrupts](#5-interrupts) | GDT/IDT, exception policy, vector map, PIC and APIC, privilege transitions |
| 6 | [Time](#6-time) | Clock sources, calibration, timekeeping |
| 7 | [SMP](#7-smp) | ACPI, AP bring-up, per-CPU, IPIs, shootdown |
| 8 | [Testing](#8-testing) | Tiers, marker contract, QEMU flags, CI |
| 9 | [Pitfalls](#9-pitfalls) | Bugs already paid for once |
| 10 | [Block I/O](#10-block-io) | Requests, barrier vs flush, ramdisk, virtio-blk, partitions, cache |
| 11 | [Portability](#11-portability) | The architecture seam, the aarch64 address space, adding a port |

On-disk filesystem formats live in their own docs, not here ([§1.4](#14-documentation-rules)): [VIBEFS.md](VIBEFS.md) (vibefs **version 1**, CoW metadata + atomic superblock switch). Syscall ABI: [SYSCALL.md](SYSCALL.md).

---

# 1. Overview

## 1.1 Design constraints

These are not style preferences. They shape every subsystem.

1. **Concrete over generic.** One PMM, one scheduler, one page table layout. No trait soup to support
   the second implementation nobody is writing. Traits appear where there are genuinely N backends
   (console output, block devices, filesystems).
2. **Test the algorithm on the host.** Anything that is pure logic (parsers, allocators, state
   machines, encodings) lives in `vibeos-core` so `make test-unit` covers it. Hardware pokes stay
   in the kernel half. This split is the single biggest lever on iteration speed.
3. **Serial is the ground truth.** Every subsystem prints one line when it comes up. Those lines are
   a contract enforced by the e2e harness, not debug noise.
4. **Fail loud, fail early.** Assert invariants at boot. Bounded spins everywhere so a wedged device
   produces a diagnosable hang instead of a silent one.
5. **No unbounded loops against hardware.** Every poll gets an iteration cap and a failure path.
6. **Layering is enforced by dependency direction**, not by wishful thinking. Lower layers do not
   call up. No callback into the scheduler from the physical allocator.
7. **The portable crate is stable Rust.** `vibeos-core` enables no `#![feature]`. Kani (ROADMAP
   §10.8) and Verus (Phase 38) each pin their own toolchain and must build the code the kernel
   links, and code with no unstable feature builds on all of them. Nightly features stay in the
   kernel binary. `scripts/check_core_stable.py` in `make check` enforces it.

When two goals or constraints pull apart, decide in this order:

1. No halt, no memory corruption, and no lost data, whatever untrusted input does (§2.10).
2. Behaviour matches its contract, which is Linux's wherever Linux defines one (ROADMAP, How to
   read this).
3. A failure explains itself from the host, without a rerun.
4. One simple mechanism rather than two.
5. Speed, as measured.

A faster or more general path that weakens an earlier item is rejected unless that item is kept by
other means, and the measurement that justifies the path is recorded beside it.

## 1.2 Layers

```
        userspace: init, shell, services, the Wayland compositor
   ------------------------------------------------------
    syscall  |  vfs  |  net stack  |  display and input (DRM, evdev)
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

**Naming.** `src/<name>.rs` is the portable half (`vibeos-core`, `src/lib.rs`, host-tested via
`make test-unit`). `src/<name>_init.rs` is the kernel half (`src/main.rs`). A few kernel-only files
have no portable pair. `src/arch/` holds only what touches privileged CPU state (GDT, IDT, PIC, catch,
gs, cpu, AP trampoline). Nested also: `src/fs/` (VFS + kernfs). `user/` is freestanding ELFs, not kernel modules.

| Subsystem | Portable | Kernel |
|-----------|----------|--------|
| crate | `src/lib.rs` (`vibeos-core`) | `src/main.rs` (`_start`, base revision, boot order) |
| boot / serial | `uart.rs`, `marker.rs`, `fmt_util.rs`, `symtab.rs` | `boot.rs` (`BootInfo`, Limine requests), `serial.rs`, `panic.rs`, `diag.rs`, `ksyms.rs` |
| arch | `desc.rs`, `pic.rs`, `vectors.rs` | `arch/mod.rs`, `arch/gdt.rs`, `arch/idt.rs`, `arch/pic.rs`, `arch/catch.rs`, `arch/gs.rs`, `arch/cpu.rs`, `arch/trampoline.rs`, `arch/trampoline.S`, `x86.rs` |
| mm | `pmm.rs`, `paging.rs`, `heap.rs`, `kva.rs` | `pmm_init.rs`, `paging_init.rs`, `heap_init.rs`, `kva_init.rs` |
| time | `time.rs` | `time_init.rs` |
| acpi | `acpi.rs` | `acpi_init.rs` |
| interrupts | `irq.rs`, `apic.rs`, `ipi.rs` | `irq_init.rs`, `apic_init.rs`, `ipi_init.rs` |
| smp | `smp.rs`, `per_cpu.rs` | `smp_init.rs`, `per_cpu_init.rs` |
| sched | `thread.rs`, `sched.rs`, `wait.rs`, `sync.rs`, `lock.rs`, `work.rs` | `thread_init.rs`, `sched_init.rs`, `sync_init.rs`, `work_init.rs` |
| cell | `cell.rs` (`#[cfg(test)]` in `vibeos-core`) | `cell.rs` (`BootCell`, `IrqCell`) |
| log | `log.rs` | `log_init.rs` |
| console | `console.rs`, `kbd.rs`, `fb.rs`, `font.rs`, `shell.rs` | `console_init.rs`, `kbd_init.rs`, `fb_init.rs`, `shell_init.rs` |
| devices | `pci.rs`, `dev.rs`, `dma.rs`, `virtio.rs` | `pci_init.rs`, `dev_init.rs`, `dma_init.rs`, `virtio_init.rs` |
| block | `block.rs`, `virtio_blk.rs`, `part.rs`, `cache.rs` | `block_init.rs`, `virtio_blk_init.rs`, `part_init.rs`, `cache_init.rs` |
| fs | `fs/mod.rs`, `fs/kernfs.rs`, `fat.rs`, `vibefs.rs` | `fs_init.rs`, `fat_init.rs`, `vibefs_init.rs`, `file_init.rs` |
| entropy | `entropy.rs` | `entropy_init.rs` |
| proc | `addr_space.rs`, `elf.rs`, `proc.rs`, `syscall.rs` | `addr_space_init.rs`, `user_init.rs`, `proc_init.rs`, `syscall_init.rs` |
| ktest | — | `ktest.rs` (`kernel_tests` only) |

## 1.4 Documentation rules

- Durable intent goes in this file. Ephemeral "fixed X" notes go in `CHANGELOG.md` or nowhere.
- No code review writeups, no phase retrospectives, no status reports. The roadmap checkboxes are the
  status. `git log` is the history. The one status this file states is the gap between a rule and the
  code, written as the header says ("Rule; not yet enforced" with the ROADMAP line that closes it),
  so a rule is never read as a description of the code.
- "How does this function work" goes in a doc comment. "Why is this line here at all" goes in a short
  comment on the line. Nothing goes in a comment that describes a past state of the code.
- If this doc describes behavior the code contradicts, one of them is a bug. Say which in the commit
  that fixes it.
- Constants appear once, here, and are cross-referenced rather than restated. Address map in
  [section 4.1](#41-virtual-address-map), vector numbers in [section 5.3](#53-vector-map).
- When this file outgrows one page per subsystem, split it into `docs/<topic>.md` and leave an index
  behind. Not before. On-disk formats are that split: [VIBEFS.md](VIBEFS.md), not a novel in this
  file. Syscall ABI: [SYSCALL.md](SYSCALL.md).

## 1.5 Sources and licenses

vibeOS is MIT ([LICENSE](../LICENSE)). What goes into the tree comes from specifications, manuals,
and measured behaviour, or from sources whose license lets it into an MIT tree:

- Nothing is copied or translated from GPL or LGPL sources: Linux, glibc, GNU tools, or QEMU's GPL
  parts. The Linux-interfaces rule asks for Linux's behaviour, which vibeOS learns from man pages,
  specifications, and running Linux (ROADMAP §13.11), never by porting Linux's code. Numeric
  constants, struct layouts, and `ioctl` numbers that an interface defines are facts; each is written
  down with a citation of where it is defined.
- MIT, BSD, ISC, zlib, or Apache-2.0 code may be adapted, with its copyright and license notice kept
  beside it (and, for Apache-2.0, its NOTICE text).
- Third-party sources the tree builds rather than copies follow ROADMAP §14.10's port policy, and test
  inputs fetched at test time (Linux's device trees, QEMU's ACPI tables) are never committed.

Why: one function derived from GPL code would put the kernel under the GPL, against the project's
license. The kernel review's spot check found no such copy, but no rule said so.

---

# 2. Invariants

Break these and the failure shows up somewhere else, hours later.

## 2.1 Lock order

Acquire in this order, release in reverse. Never take a lower number while holding a higher one, and
take a second lock of a number already held only through §2.3's nested acquire.

1. heap
2. page tables
3. physical allocator (buddy)
4. scheduler (also: wait-queue lists and blocking-primitive predicates)
5. device / driver locks
6. serial

Serial is last so any lock holder can still log. Heap is first because growing it takes PT and then
BUDDY, and nothing allocates from or frees to the heap while holding either. Page tables come before
the buddy because mapping a page allocates its table frames. Blocking `WaitQueue`s are serialized by
the scheduler lock: the predicate check and the enqueue happen under that same lock (DESIGN
[§9.4](#94-concurrency)).

The six ranks order spinlocks: `SpinMutex`, and the cross-CPU `IrqCell`s that ROADMAP §10.3 turns
into ranked `SpinMutex`es. Sleeping locks form a tier outside all six: `BlockingMutex`, `RwLock`,
`Semaphore`, and waiting for a page or buffer to finish I/O. A thread takes a sleeping lock only with
IF=1 and no spinlock held ([§2.9](#29-preemption-and-interrupt-state) rule 4), so every sleeping lock
ranks before every spin rank, and no spinlock is ever held across a sleep. Within the sleeping tier,
outermost first:

1. filesystem namespace and inode locks, the ones a call may hold across a copy to or from user
   memory: the mount table, then a directory, then an inode in it; a parent directory before its
   child. A `rename` takes the volume's rename lock, then its two directories: an ancestor before its
   descendant, and two directories neither of which contains the other in address order
2. the address-space lock (ROADMAP §13.1: `mmap`, `munmap`, and `mprotect` take it for writing, the
   fault path for reading). It guards the region tree only. A page-table entry changes under the
   page-table spinlock (the PT rank above), so the reverse-map unmap that direct reclaim does
   ([§4.4](#44-kernel-heap)) takes no address-space lock
3. waits on a page-cache page or a block buffer (ROADMAP §12.5)
4. a filesystem's block-mapping and volume I/O locks, which its page-fill and writeback paths take
   and which are never held across a user copy

A user copy may fault, and the fault path takes the address-space lock for reading, then page waits,
then, to fill a file page, the filesystem's level-4 locks. So a copy to or from user memory is
allowed while level-1 locks are held, as `write` needs, and under no lock of levels 2 to 4. The
reverse is forbidden: code that holds the address-space lock takes no level-1 lock.

A buffered `write` whose user buffer maps the very page it writes would fault on that page while it
holds the page busy (level 3) and wait on itself. So the write path copies into a busy page only
through a non-faulting accessor, which returns a short count instead of taking the fault. On a
short count it releases the page, faults the rest of the source in with no page held, and retries.
This is Linux's `fault_in_iov_iter_readable` loop. A `read` into a buffer that maps the page it
reads needs no such loop, because it copies from a page that is up to date and not busy.

The rename order is Linux's too: `rmdir` holds a parent and then the child it removes, so a
`rename` whose directories were an ancestor and its descendant, taken in address order, could hold
the child and wait for the parent. The rename lock serializes cross-directory renames, which is why
unrelated directories may go in any fixed order. `mmap` of a file takes a counted reference to the file's
page-cache object before it takes the address-space lock, never the inode lock inside it. A
filesystem with a single volume lock ranks it at level 4, so it must drop it before any user copy.
This is Linux's order (`i_rwsem`, then `mmap_lock`, then the page lock, then the filesystem's own
block-mapping locks), chosen for the same reason: a `write` that faults on its user buffer while
holding the inode lock must not meet an `mmap` that holds the address-space lock and wants that
inode lock. ROADMAP §13.12's lock-dependency build checks both tiers.

The rank check fails an allocation or a free made while PT, BUDDY, SCHED, DEVICE, or SERIAL is held,
on every call, whether or not the heap grows. Growth takes PT and then BUDDY after dropping HEAP
(`heap_init::grow_for`), so the frame allocation it makes can enter direct reclaim with nothing held
([§4.4](#44-kernel-heap) rule 1). Rule; not yet enforced: `src/lock.rs` ranks the heap third, after
PT and BUDDY, so an allocation under either fails only when it grows the heap, through PT's
recursive-lock check or the rank check on PT, and passes every test that does not grow the heap
(ROADMAP §10.3).

Filesystem spinlocks take `RANK_DEVICE`: today the VFS lock, the open-file table, and each backend's
mount and slot-allocation locks. The rank order therefore forbids heap allocation under them and
allows logging. The VFS tables are static, so bring-up allocates nothing under them. Filesystems get
no spin rank of their own; adding one changes this list and `src/lock.rs` in the same commit.

After ROADMAP §10.4's A3 work, the VFS lock is a `BlockingMutex` at level 1's mount-table position.
It guards the namespace tables (mounts, dentries, and the inode and open-file tables) and is held
for a lookup, an insert, or a removal. It is never held across a backend's data I/O, a wait on a
pipe, TTY, socket, or page, or a user copy. A lookup returns counted inode and file references
(§2.11 rules 1 and 2). `read`, `write`, `truncate`, `fsync`, and `getdents64` then run on those
references with the VFS lock dropped, under the backend's own locks (an inode's at level 1, a
volume's at level 4), and a backend operation receives its superblock and inode through them, never
`&Vfs`. A namespace change (create, unlink, rename, mkdir, mount) and the directory read of a
dentry-cache miss may hold the VFS lock across the backend's directory I/O. Lookups leave the lock
with ROADMAP §19.5's RCU walk; namespace changes stay serialized by it. The fault path and the
writeback threads reach a file's backend through the counted page-cache reference that the region or
the page holds, and never take the VFS lock. Each FAT and vibefs volume is a level-4 `BlockingMutex`
that owns the volume and that nothing force-clears. It is taken with a plain `lock()`, as Linux
takes FAT's `fat_lock`, and contention never fails an operation.

Why: a lock that every file operation takes and that backends block under serializes all file I/O
behind one block wait, deadlocks a named-pipe read against its writer, and leaves the fault path,
which holds the level-2 address-space lock, no legal way to fill a file page. Rule; not yet
enforced: the VFS lock is a `RANK_DEVICE` spinlock that the File API drops before any FAT or block
wait, and each FAT and vibefs volume sits behind a busy flag whose waiter yields and fails the
operation with `EIO` after 1,000,000 yields, and which `drop_slot` force-clears (ROADMAP §10.4,
F060).

## 2.2 Interrupt handler rules

An interrupt handler must not:

- allocate or free (no heap, no buddy, no `Vec`, no `format!`)
- take any lock that is ever held with interrupts enabled
- log at anything but the most extreme failure path
- run unbounded loops

An interrupt handler must:

- EOI before it can possibly context switch, so the controller is not held across a switch
- rearm its own one-shot timer source before doing anything else that can yield
- keep its stack frame small: only `#DF`, NMI, `#MC`, and `#DB` run on dedicated IST stacks (§5.1);
  every other vector runs on the interrupted kernel stack, or on TSS.RSP0 when it interrupts ring 3

Both of the "must" rules are expanded in [section 5.8](#58-handler-ordering-rules), because both are
easy to violate and expensive to debug.

Blocking and allocation are a class of bug, not an instance. Context rules:

| Context | May block? | May alloc? |
|---------|------------|------------|
| Hard IRQ / MSI handler | No | No |
| Softirq equivalent (high-prio workqueue) | No | Fallible only, without direct reclaim ([§4.4](#44-kernel-heap)); the hard IRQ only enqueues |
| Threaded IRQ bottom half | Yes, on its own device's state only ([§5.4](#54-irq-registration)) | Fallible only, without direct reclaim ([§4.4](#44-kernel-heap)) |
| Workqueue worker | Yes | Yes |
| Driver `probe` | Yes | Yes |
| Syscall body, fault handler for a CPL-3 fault | Yes ([§2.9](#29-preemption-and-interrupt-state)) | Yes, fallible only ([§4.4](#44-kernel-heap)) |
| NMI and `#MC` at any CPL, `#DB` at CPL 0; shootdown and call-function work run from a spin | No | No |

Code in the last row runs inside whatever IF=0 section its CPU was in, locks included: an NMI,
`#MC`, or `#DB` interrupts it, and a CPU in a serviced spin (§2.3) runs incoming shootdown and
call-function work there. So that code takes no lock of any kind (a `SpinMutex`, an `IrqCell`, or a
TAS, the log ring's TAS and the serial TX lock included), since the section it interrupted may hold
that very lock. It allocates nothing, does a bounded amount of work, and writes only per-CPU state
and lock-free rings. A call-function closure that needs a lock queues a work item on its CPU
instead. The panic path (§2.5) is the one exception: it reads the log ring and writes COM1 without
taking their locks.

The hard-IRQ top half acknowledges and wakes. Work that allocates or blocks runs on a kernel thread
([section 5.4](#54-irq-registration), ROADMAP §6.6).

## 2.3 Locking with interrupts

Every spinlock that is taken from both an ISR and normal context disables interrupts for the whole
critical section. That is the default: `SpinMutex` is IRQ-aware, and the non-IRQ-aware variant does
not exist. The scheduler lock, the input ring, the buddy allocator, and the heap all qualify.

Pick one spinlock implementation and use it everywhere. The old tree ended up with two (a ticket lock
in one design doc, an IRQ-guarded spin mutex in the code) and the mismatch was a source of confusion
for weeks.

The lock, the serviced spins, and the two cells:

| Primitive | Use |
|------|-----|
| `SpinMutex` | Shared across CPUs. IRQ-aware. Ranked (§2.1): `lock` refuses a lock whose rank, or a later one, this CPU already holds, and `lock_nested` takes a second lock of a held rank (below). Its spin is a serviced spin. |
| Serviced spin | `SpinMutex::lock`, `ipi_init::wait_acks`, and the call-function slot wait each call `ipi_init::service_incoming` on every iteration, so a CPU that waits on another with IF=0 still acknowledges shootdowns and runs call-function work ([§2.9](#29-preemption-and-interrupt-state) rule 2). That work runs inside whatever the spinning CPU holds, so, like an NMI, `#MC`, or CPL-0 `#DB` handler, it takes no lock ([§2.2](#22-interrupt-handler-rules)'s last row). |
| `IrqCell` | IRQ-off exclusive access: `with` takes IRQs off, panics on same-CPU re-entry, and spins while another CPU holds it. Used for CPU-local and boot-only state and as an unranked cross-CPU lock (among them `proc_init::TABLE`, `kva_init::KVA` and `DEFERRED`, `work_init::ST`, `irq_init::IRQ`, `file_init::CWD`, and `log_init::LOG`). Its `Sync` impl has no `T: Send` bound, and `force_unlock` is a safe fn (ROADMAP §10.3, F017). Its spin does not service IPIs, so a cross-CPU cell held across a wait on another CPU can stall a shootdown. Planned (ROADMAP §10.3, F108): every cross-CPU `IrqCell` but the log ring becomes a ranked `SpinMutex`; the log ring's holders never wait on another CPU (§2.5), and ROADMAP §19.5 replaces it. |
| `BootCell` | Write once before `smp: done`, then shared `&T`. The set-once check is a `debug_assert!` (ROADMAP §10.2, F137), and the `Sync` impl has no `T: Send + Sync` bound, so `per_cpu_init::CPUS` shares the non-`Sync` `PerCpu` (ROADMAP §10.3, F017, F039). |

Two locks of one rank nest only through `lock_nested`, in a pair order the call site's comment
names, and a per-rank count keeps the outer rank held when the inner lock drops. ROADMAP §13.12's
lock classes add address order for a socket pair and check every pair order. Rule; not yet enforced:
`lock` lets a lock of a held rank nest, the inner release clears the rank bit the outer lock still
holds, and nothing stops the code in §2.2's last row from taking a lock (I1; ROADMAP §10.3, F108).

A wake takes the scheduler lock, which ranks before device and serial locks. So code holding one of
those records the wake and performs it after dropping the lock, as §10.1's completion does, and a
top half wakes its bottom half with no device lock held; the rank check fails the other order on
every call.

They live in `src/cell.rs` (`BootCell`, `IrqCell`) and `src/sync_init.rs` (`SpinMutex`). Do not add another `UnsafeCell` + `unsafe impl<T> Sync` wrapper. `static mut` is only the asm-owned `vibeos_jmpbuf` in `arch/catch.rs`. Accessors do not return `&'static mut`.

Cross-CPU rule: a CPU never touches another CPU's run queue directly. Work is handed over through a
per-CPU inbox plus a reschedule IPI. More SMP-specific rules in [section 7.7](#77-locking-with-more-than-one-cpu).

## 2.4 Memory invariants

- Physical page 0, the loaded kernel image, the AP trampoline page, and firmware-reserved regions are
  never in the buddy free lists.
- Buddy free list nodes live inside the free pages themselves. A stray write into freed memory
  corrupts the allocator, so guard pages on stacks are not optional. One stack has none: Limine's
  boot stack (at least 64 KiB, no guard page, in bootloader-reclaimable memory), which all of boot
  and, in `kernel_tests` builds, the in-guest test registry run on, so an overflow there corrupts
  memory silently (ROADMAP §10.6, F072).
- Kernel mappings are `GLOBAL`. Unmapping one requires a TLB shootdown on every online CPU before the
  virtual address may be reused.
- MMIO pages are mapped uncacheable. QEMU tolerates write-back MMIO; real hardware does not.
- Every mapping is `NO_EXECUTE` unless it holds code that is fetched. Exception: the low identity
  window's first 2 MiB is executable, though only the `0x8000` trampoline page is fetched, and only
  during AP bring-up (ROADMAP §10.6, F085).

## 2.5 Panic policy

Binding order (do not invert):

1. Broadcast halt IPI `0xFE` first (Fixed delivery, not NMI). `ipi_init::halt_others` sets `HALTING`
   and returns without waiting. A CPU spinning with IF=0 does not take the IPI, and once `HALTING` is
   set its serial writes skip the TX lock, so it can write COM1 during the dump; a second panicking
   CPU re-runs `Serial::init` mid-dump. Planned (ROADMAP §10.7, F135): `halt_others` waits, with a
   bound, for every other online CPU to acknowledge `0xFE` and sends NMI to each that has not, and
   only the first CPU into `begin_dump` runs `Serial::init` and writes COM1.
2. Re-initialize serial from scratch (the panic may be *in* the serial path).
3. Print location and message; dump registers, the current thread, and the last N log records.
4. Symbolized backtrace when frame pointers exist (in-image sorted table, binary search, no alloc).
5. `cli; hlt` loop, or QEMU `isa-debug-exit` under the `panic_exit` test feature.

No unwinding: `panic = "abort"`. Serial TX in this path is a bounded THRE poll; drop the byte on
timeout (see [§9.6](#96-hardware-polling)). Do not take SCHED. The log ring is readable after the
halt IPI without taking its TAS (force-unlock if the panicking CPU held it).

Log ring (ROADMAP §5.5):

- 256 records of up to 96 message bytes; a wrap drops the oldest record.
- Compile-time maximum level: `trace` in debug builds, `debug` in release. Runtime filter: an
  `AtomicU8`, default `info`.
- The panic dump prints the last 24 records.
- The ring's lock (`log_init::LOG`, an `IrqCell`) is IRQ-off and outside the §2.1 rank order; never
  hold it across serial TX.
- `klog!` and formatted `Serial` writes keep IF off for the whole emit, so the per-CPU capture stage
  cannot interleave with a preempting thread.
- `dmesg` prints through a plain serial path that does not re-capture.
- Host tests cover overflow, filtering, and symbol lookup.
- One global IRQ-safe ring and a serial try-lock sink. Planned: a log record reaches serial whole
  (ROADMAP §10.2, F138); per-CPU buffers drained by a printer thread (ROADMAP §19.5).

Symbols come from a two-pass link (Makefile `KERNEL_VARIANT`): `nm` output from the first link fills
an in-image `.rodata` table (`KSYMS`) that the second link builds in. Rule: every function has the same address
in both links. Not yet enforced: nothing compares them, and in the panic-test build the
reference to the filled table compiles larger than the empty one and shifts every later function, so
that ISO's table mis-names frames (ROADMAP §10.2, F084). Frame pointers come from
`-C force-frame-pointers=yes` in `.cargo/config.toml` (§3.1); there is no target JSON.

Rule: `vibeos-core` (`src/lib.rs`) does not panic on data; its parsers and table walks return the
module error. Enforced only for `unwrap`, `expect`, and `panic!`: clippy denies `unwrap_used`,
`expect_used`, and `panic` on that crate (allowed in `#[cfg(test)]`). `indexing_slicing` is not
enabled, and `make` ships the dev profile, whose `overflow-checks = true` turns an arithmetic
overflow into a panic. Crafted input panics portable code: a FAT BPB whose
`rsvd + num_fats * FATSz32` overflows in `parse_bpb` (ROADMAP §10.2, F064); a vibefs write near file
offset 2^44, which overflows `map_block` (ROADMAP §10.11, F008); a CRC-valid vibefs leaf whose count
exceeds the per-leaf maximum (F061; ROADMAP §14.8 retires v1 for a v2 that validates every block it reads); a vibefs truncate-grow that keeps `F_INLINE`
past 128 bytes (ROADMAP §13.9, F062). Panics in the kernel binary halt in the binding order above.

Exceptions follow the per-vector table in [section 5.2](#52-idt-and-exceptions). A kernel `#BP`
logs and continues. Every other exception taken in ring 0 dumps and halts in the same order as
`#[panic_handler]`. Rule: ring 3 never halts the kernel. An exception raised by ring-3 code, or by a
return to ring 3, kills
that process with the signal §5.2 gives the vector, prints `user: pid N killed SIG<name>`, and the
kernel keeps running. Not yet enforced: ring-3 `#DB`, and `#AC` when `CR0.AM` is set, halt the kernel (ROADMAP §10.6,
F005), and so do the entry-path windows of §5.10 (ROADMAP §10.6, F004, F006, F007); §5.2's last
column lists every vector whose ring-3 action differs from the rule. A hardware NMI halts on its IST stack; the panic broadcast is
IPI `0xFE`, not NMI.

Rule: nothing is silently swallowed. Not yet enforced in three cases. An exception before `idt::init` (PMM, the CR3 switch,
ACPI discovery, heap, KVA, GDT, PIC) goes to whatever IDT Limine left and resets or hangs with no
output (ROADMAP §11.1, F136). LINT1 is masked on every CPU and MADT NMI entries (types 3 and 4) are
not parsed, so a chipset or external NMI never reaches the NMI handler (ROADMAP §20.1, F096).
`CR4.MCE` and `CR0.NE` are clear on every CPU, so a machine check shuts the CPU down with no dump,
and an x87 floating-point error raises the masked IRQ13 and is lost (ROADMAP §10.6, F026).

## 2.6 Serial markers

Every boot line is `vibeOS: <subsystem>: <state>`, lowercase, no punctuation at the end. Success
markers are asserted by the e2e harness in order. Adding a marker means updating the contract in
[section 8.3](#83-end-to-end) in the same commit.

`marker!` for contract lines (never filtered, always captured); `klog!` for everything else;
`PlainSerial` only for `dmesg` and panic dumps.

```
vibeOS: serial online
vibeOS: limine: rev 3 ok
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

## 2.7 Invariant register

Every invariant the code relies on, and how it is kept. Status: *enforced* means code, a type, or a
test fails when the invariant breaks; *documented* means a rule in the section named or in a code
comment, with nothing that checks it; *assumed* means relied on but stated only in this table.
"Holds today" is the state at the commit that last changed the row; where the answer is no or
partly, the row names the ROADMAP line that fixes it. DOC2 (ROADMAP §10.3) moves this table to its
own file. These ids name invariants; ROADMAP's bare `I1` names the architecture review's issue, a
separate namespace.

| # | Invariant | Established at | Status | Holds today |
|---|-----------|----------------|--------|-------------|
| I1 | Lock rank HEAP < PT < BUDDY < SCHED < DEVICE < SERIAL (§2.1); a second lock of a held rank only through `lock_nested` (§2.3) | `lock.rs`, `sync_init::lock_enter` | enforced at runtime, per CPU | Partly: `lock.rs` still ranks the heap after PT and BUDDY, so an allocation under either fails only when it grows the heap; a nested lock of the same rank passes the check and its release clears the rank bit the outer lock still holds, `IrqCell` has no rank, and a lock held across a switch goes unseen (ROADMAP §10.3, §13.12, F108) |
| I2 | Hard-IRQ context never blocks or allocates (§2.2) | convention | documented | Yes, unchecked: only `irq_init::dispatch` sets `IN_ISR`, and no blocking primitive asserts it (ROADMAP §10.3, F110) |
| I3 | IF=0 through every return-to-user sequence (§5.10 rule 4) | FMASK `0x47700`; `cli` in `run_user` | documented | No: the syscall exit has no `cli` and `console_init::wait_key` returns with IF=1 (F001); `enter_user_full` runs with IF=1 (F006) (ROADMAP §10.6) |
| I4 | Kernel code outside the §5.10 entry and exit sequences runs with `GS_BASE` = this CPU's `PerCpu` (§5.10) | `arch::gs`, `per_cpu_init` | documented | No: the raw gates of §5.10 rule 1 (F004), the IF=1 window in `enter_user_full` (F006), an NMI, `#MC`, or `#DB` taken in the syscall entry or exit window, and a fault on the return-to-user `iretq` (both F007) run on the user base (ROADMAP §10.6) |
| I5 | One entry stub per vector makes the `swapgs` decision (§5.10 rule 1) | `arch/idt.rs` | documented | No: the `irq_init` pool gates `0x31`–`0x7F` and the `kbd_init` gates `0x30` and `0x21` skip it (ROADMAP §10.6, F004) |
| I6 | Ring 3 never halts the kernel (§2.5, §5.2) | `proc_init::try_user_fault` | documented | No: ring-3 `#DB`, and `#AC` when `CR0.AM` is set, halt (F005), and so do the I4 windows (ROADMAP §10.6) |
| I7 | The kernel never dereferences a user VA; copies go through the physmap after `check_user_range` (§5.1) | `addr_space.rs` | enforced | Yes, but `write_bytes` ignores the PTE's `WRITABLE` bit (ROADMAP §10.6, F023) |
| I8 | One thread per address space; nothing mutates an address space concurrently | process model | assumed | Yes. Lock-free user copies, local-only `invlpg`, and `&'static AddressSpace` depend on it. ROADMAP §13.1's threads end it, and bring the address-space lock and a counted handle in place of those three |
| I9 | TCBs are never freed, so a `*mut Tcb` stays valid | 64-slot table, `thread_init` | assumed | Yes, but `spawn_inner` can reuse a Dead slot whose thread is still switching out (ROADMAP §10.10, F012) |
| I10 | A dead thread's stack is freed only after its CPU has switched off it (§2.8, §4.5) | `kva_init::DEFERRED` | documented | No: any CPU drains the global list (F012), and the 8-slot list panics when full (F010) (ROADMAP §10.10) |
| I11 | A completer's publishing store is its last access to the waiter (§2.8) | `block_init::IoWaiter` | documented | No: `IoWaiter::finish` runs `wake_all` after it stores `done` (ROADMAP §10.10, F002) |
| I12 | Every kernel PML4 slot exists before the first user address space | `AddressSpace::new` copies PML4[256..512) once | assumed | Yes, by boot order only: `paging_init::install` creates none of the heap, KVA, and `ioremap` PML4 slots; each appears on its region's first mapping, and no current path makes a first mapping after `/hello` (ROADMAP §18.2, F101) |
| I13 | The low identity window is removed after `smp: done` (§4.1) | none yet | documented | No: it stays mapped and GLOBAL, VA 0 included (ROADMAP §10.6, F085) |
| I14 | Every buddy frame and page table lies inside the physmap (§4.1) | `pmm_init::init`, `paging_init::physmap_extent` | enforced | Yes; a framebuffer above the 8 GiB cap is not covered (ROADMAP §11.2, F020) |
| I15 | Frame 0, the kernel image, `0x8000`, and the framebuffers never enter the buddy (§2.4) | `pmm_init::init`; `Buddy::insert_region` skips frame 0 | enforced | Yes with at most 6 framebuffers: `Excludes` holds 8 ranges (the trampoline page, the kernel image, one per framebuffer), and a range past the 8th stays in the buddy, with a `pmm: excludes overflow` line |
| I16 | The kernel PML4 lies below 4 GiB, because the trampoline loads a 32-bit CR3 | `smp_init::start_one` | enforced by skipping every AP | Not guaranteed: the PML4 frame has no address limit, and above 4 GiB every AP is skipped with a `smp: cr3 above 4GiB` line (ROADMAP §20.1) |
| I17 | MMIO is UC, RAM is WB, and no frame has both (§2.4) | `acpi_init`, `Mapper::patch_physmap_uc` | documented | Partly: a whole 2 MiB leaf goes UC with no RAM check, and a trailing leaf can be skipped (ROADMAP §11.2, F104) |
| I18 | EOI before any switch; a one-shot timer is rearmed before yielding (§5.8) | timer ISRs | documented | Yes; no test tier runs the TSC-deadline timer, the only one-shot source, so nothing exercises the rearm (ROADMAP §10.1, F078) |
| I19 | I/O APIC high dword written before the low; IST index zero-based in software, one-based in the gate (§5.1, §5.6) | `apic.rs`, `desc.rs` | enforced, host-tested | Yes |
| I20 | `now_ns` is monotonic | seqlock plus `time::monotonic_max` over `time_init::LAST_NS` | enforced | Yes, by construction, so the monotonicity tests cannot fail (ROADMAP §10.2, F100) |
| I21 | A run queue is touched only by its owner CPU with IF=0 (§2.3) | `per_cpu_init::with_current` | enforced (busy flag) | Partly: only the owner writes it, but `diag::cpus_to` (the shell `cpus` command) and in-guest tests read another CPU's `runq` length with no lock, and `&'static PerCpu` aliases the `&mut` (ROADMAP §10.3, F039) |
| I22 | A `BootCell` is set once, before SMP, and holds `Sync` data (§2.3) | `cell.rs` | documented | No: `per_cpu_init::CPUS` holds the non-`Sync` `PerCpu`, which the unbounded `Sync` impl allows (ROADMAP §10.3, F017, F039); the set-once check is a `debug_assert!` (ROADMAP §10.2, F137) |
| I23 | Barrier: every request before it completes before any after it starts. Flush: completed writes are durable (§10.2) | `block.rs` | documented | No: the block queue merges a request across any queued fence but the lowest and can reorder overlapping writes, virtio-blk completes a `Barrier` before earlier requests and sends a `Flush` before earlier writes complete, and a cache flush misses in-flight writeback (ROADMAP §10.11, F043) |
| I24 | vibefs never overwrites a live block before the newer superblock is durable ([VIBEFS.md](VIBEFS.md)) | vibefs commit | documented | No after a failed commit: the in-memory generation advances before the superblock write, so the retry writes the slot that holds the only valid superblock (ROADMAP §12.5, F050). Otherwise it rests on on-disk refcounts that v1's mount does not check (F061), which v2 checks as it reads each block (VIBEFS.md §15; ROADMAP §14.8) |
| I25 | Per-thread CPU state is saved and restored in full (§7.5) | `syscall_init::on_switch`, `thread::switch_context` | documented | No: `FS_BASE` is not switched (ROADMAP §13.1, F022); `fork` and `execve` get the FPU state wrong (ROADMAP §10.6, F069) |
| I26 | Every kernel stack has a guard page (§2.4) | `kva_init::alloc_guarded_stack` | documented | No: boot runs on Limine's unguarded stack (ROADMAP §10.6, F072) |
| I27 | `vibeos-core` does not panic on data (§2.5) | clippy deny on `unwrap`, `expect`, `panic` | enforced in part | No: indexing and overflow checks panic on crafted input; §2.5 lists the cases and their ROADMAP lines |
| I28 | Contract markers are kernel-emitted (§2.6) | `marker!` | documented | No: `shell ready` comes from ring-3 `/bin/sh` (ROADMAP §10.5, F073) |
| I29 | A catch hook intercepts only a CPL-0 fault on the CPU that armed it, inside an in-guest test's catch window | `arch::catch` | assumed | Partly: production never arms it, but `intercept` runs first in every exception handler of every build, and its armed state is global, so in a `kernel_tests` build a fault with the armed vector on any CPU, at any CPL, is caught (ROADMAP §10.2, F146) |
| I30 | Interrupt and exception handlers run with RFLAGS.AC=0 (§5.10 rule 5) | none | documented | No: the gates keep ring 3's AC (ROADMAP §10.6, F088) |
| I31 | Every IF=0 stretch is bounded by a constant amount of work, far below the 1 s `wait_acks` timeout ([§2.9](#29-preemption-and-interrupt-state) rule 2) | §2.9 | documented | No: syscall bodies run with IF=0 until they block, the in-guest test runner holds IF off for the whole run, and a console `write` scrolls the framebuffer once per newline with IF=0 (ROADMAP §10.6, F044; ROADMAP §10.2, F075); ROADMAP §10.10 makes a shootdown survive a violation (F011) |
| I32 | A handler on an IST stack never blocks or switches threads (§5.10 rule 6) | IST handlers | documented | Yes: every IST handler halts, except that under `kernel_tests` an armed `catch` steps RIP and returns or longjmps off the IST stack |

## 2.8 Publish last

The rule for every completion, hand-off, and deferred reclaim:

1. In a completion or hand-off, the store that lets the other side return, free, or reuse an object
   is the publisher's last access to that object; after it the publisher touches only its own stack
   and statics. The other side can see the store on a lock-free path (`IoWaiter::poll`, any Acquire
   load of a done flag) and return before the publisher's next instruction, so a lock taken after
   the store does not make a later access safe.
2. An object that its owning CPU may still use (the kernel stack it runs on, a TCB that is switching
   out, an AP's stacks and tables during bring-up) is freed only after that CPU has passed a point
   the reclaimer can observe: its `switch_context` away from the object has returned, or INIT has
   stopped the AP. A global list that any CPU drains does not meet this rule.

Rule; not yet enforced. The violations, and the ROADMAP lines that fix them:

- `IoWaiter::finish` stores `done`, then takes SCHED and runs `wake_all` on the `WaitQueue` inside
  the waiter, which lives on the submitter's stack (ROADMAP §10.10, F002).
- `thread_exit` puts its own stack on the global `kva_init::DEFERRED` list, and any CPU's
  `reap_zombies` can unmap it before the exiting CPU has finished `switch_context` (ROADMAP §10.10,
  F012).
- `spawn_inner` can reuse a Dead TCB slot while its thread is still switching out on another CPU
  (ROADMAP §10.10, F012).
- On the bring-up timeout, `smp_init::start_one` frees an AP's kernel stack, GDT/TSS, and IST and
  RSP0 stacks without an INIT, so an AP that is still running uses freed memory (ROADMAP §20.1,
  F032).

## 2.9 Preemption and interrupt state

IF here means this CPU's maskable-interrupt enable: RFLAGS.IF on x86_64, and PSTATE.I clear on
aarch64, where ROADMAP §25.5's pseudo-NMIs later make `InterruptGuard` mask by priority instead. The
rules below hold on both architectures.

The kernel is preemptible wherever IF=1. The timer tick and the reschedule IPI call
`schedule_preempt`, which may switch away from any thread whose `irq_nest` is 0: kernel threads,
syscall bodies, and fault handlers alike. Turning interrupts off is how code says "not now", so
every IF=0 stretch has a reason from the list below and a bound.

1. IF=0 only in: an interrupt or exception entry or exit stub; a hard-IRQ top half (§2.2); a
   spinlock or `IrqCell` critical section (§2.3); an `InterruptGuard` section that must not be
   preempted or moved to another CPU: a per-CPU access (rule 5), a change to this CPU's registers
   that must match the running thread (FP state, `FS_BASE`), the [§7.9](#79-tlb-shootdown)
   shootdown wait, or the [§7.6](#76-ipis) call-function wait; the scheduler's switch path; the
   return-to-user sequences of [§5.10](#510-privilege-transitions) rule 4; the panic and halt paths
   (§2.5); and a CPU's bring-up before its first `sti` (boot before `irq: enabled`, an AP before it
   enters idle).
2. An IF=0 stretch does a bounded amount of work. No loop whose trip count a user, a device, or a
   disk image controls runs with IF=0, and nothing waits for another CPU with IF=0 without servicing
   incoming IPIs ([§7.9](#79-tlb-shootdown)). A long job holds its lock for one bounded chunk at a
   time and turns IF back on between chunks.
3. A syscall body runs with IF=1. After `swapgs`, the entry stub copies the user RSP from
   `PerCpu.syscall_scratch` into its frame on the thread's kernel stack, then runs `sti`; from
   there on the scratch belongs to whichever thread next enters on this CPU. The exit stub runs
   `cli` before it writes the scratch again (§5.10 rule 4). A fault or trap taken at CPL 3 runs
   its body with IF=1 once its frame is on the thread's kernel stack, and so does a `#PF` taken at
   CPL 0 inside a user-memory accessor once ROADMAP §12.2 lets it sleep, since the code it
   interrupted ran with IF=1; a hardware interrupt's top half keeps IF=0. Rule; not yet enforced: FMASK clears IF at `syscall` and nothing sets it
   again, so a syscall body runs with IF=0 until it blocks (ROADMAP §10.6).
4. Code that may sleep (waits on a wait queue, takes a sleeping lock (§2.1), allocates with
   reclaim (ROADMAP §12.6), or copies to or from user memory once ROADMAP §12.2 lets a user-copy
   fault sleep) runs with IF=1 and no spinlock held, and asserts both in debug builds (ROADMAP
   §10.3).
5. A per-CPU field other than `current` is read or written only with IF=0 (`with_current`,
   `IrqCell`), because a preemption with IF=1 can move the thread to another CPU between the
   lookup and the use. `current` names the same thread on every CPU it runs on.

Why this model: a syscall body that runs with IF=0 cannot acknowledge a TLB shootdown (F011) or
take the tick (F044), so every long syscall (`fork`'s copy, `execve`'s load, a large `read`)
would need its own IF-on window, and ROADMAP §12.2's fault path must sleep. Linux runs syscalls
with interrupts on and preempts wherever no lock is held, so its behaviour settles edge cases.
Rejected: keeping syscall bodies at IF=0 and adding a polling window to each long call, which is
how ROADMAP §10.6 first fixed console `write`; it has to be repeated in every call that loops and
it still starves the tick.

## 2.10 Trust boundaries

Whom the kernel trusts, and the ROADMAP line where each boundary hardens. Until ROADMAP §18.8's
`docs/THREAT_MODEL.md` exists, this table is the threat model, and that document grows from it.
"Untrusted" means the source may send any bytes, at any time, as often as it likes, and the kernel
must neither halt nor corrupt memory it has not given to that source (AGENTS.md rule 4).

| Principal | Trusted for | Can do today what a hardened kernel stops | Hardens in |
|---|---|---|---|
| Ring-3 code | Nothing: it must not halt or corrupt the kernel (I6) | Halt the kernel (F004 to F010); every process is root, so it can read any file and signal any process | ROADMAP §10.6 and §10.10 (halts), §13.9 (uids), §18.6 (capabilities, `seccomp`) |
| Disk images and partition tables | Nothing: a parse returns `Corrupt` | Panic the kernel with a crafted image or table that root mounts or attaches (F061, F064, F117) | ROADMAP §10.2 (FAT BPB), §13.9 (partition tables), §14.8 (vibefs v2 validates every block it reads; v1 is retired) |
| Devices: config space, rings, registers, interrupts | Nothing for halts (rule 4); everything for DMA | Read or write any physical memory by DMA, and forge an MSI | ROADMAP §18.1 (IOMMU, interrupt remapping, used-ring checks, F048) |
| Firmware tables: ACPI, device tree, SMBIOS, the memory map | What they describe, but not their bounds: a malformed table is refused, never followed out of range | Halt boot with a malformed table before the IDT exists (F136) | ROADMAP §11.1 (early exceptions report themselves), §20.1 (table bounds) |
| The network | Nothing, from the first packet | Not reachable yet | ROADMAP §15.10 fuzzes every parser from the start |
| Speculation and timing side channels | Out of scope: no KPTI and no Spectre or MDS mitigations; the kernel half is mapped in every user address space (F024, F025, F131, F132) | Read kernel and other processes' memory on an affected CPU | ROADMAP §18.3 |
| Limine, the firmware, and the CPU | Everything | Not applicable | ROADMAP §18.7 measures and verifies the boot chain |
| The host running QEMU, the harness, and CI | Everything; they are the test oracle | Not applicable | Never |

Consequence: until Phase 18 closes, vibeOS stops a process from crashing the kernel, not from reading
another process's data. README says not to run untrusted code on it or keep secrets on it.

**Interim posture (owner decision, 2026-09-23, design review G006).** The owner accepted the open
gaps in the table above until the ROADMAP lines that close them, the last in Phase 18: speculation
side channels (with no KPTI, a user process on a Meltdown-affected Intel CPU, bare metal or under KVM,
can read all RAM through the physmap), DMA that no IOMMU confines (§18.1), every process running as
root (§13.9), and root-mounted crafted images that panic the kernel. Why: the kernel has no users and
no secrets; QEMU's TCG, the harness default, does not model the speculation Meltdown needs, and under
KVM the exposure depends on the host CPU; and ROADMAP §10.6 rewrites the entry path as one generated
stub per vector, which keeps a later KPTI CR3 switch local. Rejected: moving KPTI and syscall-index
masking (§18.3's F024 and F025 boxes) into Phase 13, which costs a slice of entry-path work, a CR3
switch on every entry, and a measurable syscall slowdown on affected CPUs; and moving all of §18.3
before Phase 14's `login`, which costs most of a phase ahead of the self-hosting work.

The acceptance assumes one user. It goes back to the owner before `login` lands (ROADMAP §14.3), when
a second user can share the machine. Keeping any gap past the line that closes it, or adding a gap,
is likewise the owner's decision, not an agent's.

## 2.11 Object lifetimes

How an object that more than one thread or CPU can reach is created, shared, and destroyed. §2.8
covers the last store of a hand-off; these rules cover the rest.

1. One owner, or a count. An object one thread uses is owned by it (a stack value, a `Box`). An
   object that more than one thread or CPU can reach (an address space, an open file description, an
   inode, a mount, a device instance, a pipe) is reference-counted, and dropping the last reference
   tears it down. `&'static` is only for boot-lifetime objects in a `BootCell` (AGENTS.md rule 6).
2. Tables hold references or quiescent slots. A lookup structure (the process table, the TCB table,
   the dentry cache, the device registry) holds a counted reference, or a slot it reuses only once
   the object's count is zero and no CPU still runs on it or through it (a TCB's `on_cpu` flag,
   ROADMAP §10.10).
3. Teardown runs in one order: unpublish the object from every lookup structure, so no new
   reference can be taken; wait until in-flight users drop theirs (the count reaching zero, or,
   for lockless readers from ROADMAP §19.5 on, an RCU grace period); release what the object holds
   (frames, vectors, DMA buffers, stopping the device first, [§5.4](#54-irq-registration)); then
   free it. No step waits for a lock that an in-flight user needs in order to finish.
4. Ids are not pointers. A pid, tid, descriptor, or device id that crosses the syscall boundary or
   sits in a table is looked up on each use, never cached as a pointer. Pids and tids are allocated
   in increasing order up to `pid_max` and then wrap, skipping ids in use, as Linux does, so a freed
   id is not handed out again at once; an id becomes free only when its object is reaped.
5. Asynchronous work owns what it touches. A request, timer, work item, or completion that outlives
   the call that started it holds counted references to every object it will touch (ROADMAP §12.5's
   owned block submission).

Why: the kernel review's CRITICAL and HIGH lifetime findings (F002, F012, F019) each came from an
object that one subsystem freed or reused while another could still reach it, under a scheme that
subsystem invented. Rejected: keeping one scheme per subsystem, which is how those bugs arose; and
never freeing (today's TCBs), which aliases a dying object as soon as its slot is reused. Epoch or
RCU reclamation is kept for lockless readers only (ROADMAP §19.5); everything else uses counts.

Today the code breaks rules 1, 2, 4, and 5: TCBs are never freed and their slots are rewritten in
place (I9; ROADMAP §10.10, F012), address spaces are reached through `&'static` references built
from table-owned boxes (ROADMAP §13.1, F019), block completions point into stack frames (ROADMAP
§10.10, F002; §12.5, F042), and a pid is the index of its process-table slot, handed out lowest
first (ROADMAP §10.4, F127).

---

# 3. Boot

Power-on to `sti`. Limine does the ugly part (real mode, A20, long mode, ELF loading) and hands us a
64-bit kernel with paging already on. Everything after that is ours.

## 3.1 Toolchain

| Piece | Value |
|-------|-------|
| Channel | dated nightly in `rust-toolchain.toml` (bump with CI in one PR) |
| Components | `llvm-tools` (objdump/nm/size), `rustfmt`, `clippy`; `rust-src` for rust-analyzer |
| Target | built-in `x86_64-unknown-none` (`rust-toolchain.toml` `targets`) |
| Build | `cargo build` (default target in `.cargo/config.toml`) |
| Panic | kernel target `abort`; host tests `unwind` (`profile.dev`) |
| Extra host tools | `xorriso`, `nasm` (`user/*.asm`), `qemu-system-x86_64`, `python3`, `dosfstools` (`fsck.fat`; host FAT tests skip if missing) |

`make` is the usual entry. It stages `build/initrd.fat` and passes `VIBEOS_INITRD` into `build.rs`.
Bare `cargo check` / `cargo build` works: `build.rs` passes `-T$CARGO_MANIFEST_DIR/linker.ld` and
embeds an empty 64 KiB initrd if the env is unset. Host tests: `make test-unit` (`cargo test -p vibeos-core
--features std --target $HOST`). `tests/hostlib` is mkfs/fsck/`mkinitrd` only.

`make` pins `CARGO_TARGET_DIR` to `./target`. Some environments point it at a shared cache, which
leaves the ISO packaging a stale ELF from a previous build and produces genuinely baffling debugging
sessions.

Target notes:

- Built-in `x86_64-unknown-none` already has `code-model: kernel`, `disable-redzone`,
  `-mmx,-sse,+soft-float`, `panic=abort`, and `rust-lld`.
- The builtin spec defaults to PIE and full RELRO. rustflags override to static relocation,
  `-no-pie`, and `-znorelro`. RELRO fights a non-PIE static kernel.
- `disable-redzone: true`. Interrupt handlers clobber the red zone.
- Frame pointers are forced (`-C force-frame-pointers=yes`) so panic dumps can symbolize.
- The kernel is built soft-float, so compiled kernel code uses no SSE or x87 registers; kernel SSE
  would need a save around each use, and none exists. User code gets SSE: `syscall_init::init_fpu`
  clears `CR0.EM` and `CR0.TS` and sets `CR0.MP` and `CR4.OSFXSR` on every CPU, and
  `syscall_init::switch_fpu` saves and restores each thread's 512-byte FXSAVE image (`Tcb.fpu`) on
  every switch. `CR0.NE` and `CR4.OSXMMEXCPT` are not set, so x87 and SSE floating-point errors do
  not reach `#MF` and `#XF` (§5.2; ROADMAP §10.6, F026).
- `build.rs` passes the linker script as an absolute `-T` so the link does not depend on cwd.

## 3.2 Limine protocol

Limine scans the loaded ELF for request structures in linker sections, in this order:

```
.limine_requests_start   marker
.limine_requests         the request statics
.limine_requests_end     marker
```

Every request is a `#[used]` `static` placed in `.limine_requests`. Miss the section attribute and the
loader never sees the request, so the response pointer is null and the kernel dies on the first unwrap
with no explanation. Check the base revision before trusting any other response. After that handshake,
`boot::capture` reads every response once into a write-once `BootInfo` (`BootCell`). Nothing else
touches the Limine request statics, and no Limine type leaves `boot`: consumers get the kernel's
physical span, the RSDP, and `usable()` / `framebuffers()` iterators, and derive the rest themselves.

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
says what each step needs and why. Its numbers are the design order, and the live order differs
where the paragraphs below the table say so. The executable contract for the markers is
`boot_contract_markers()` in `tests/harness/harness.py` ([section 8.3](#83-end-to-end)). DOC2 (ROADMAP
§10.3) rewrites this table in live order and deletes those paragraphs.

| # | Step | Marker | Why here |
|---|------|--------|----------|
| 1 | Serial (COM1) | `serial online` | Nothing before this is debuggable. The panic handler uses the same port. |
| 2 | Base revision check | `limine: rev N ok` | Everything downstream reads Limine responses. |
| 3 | GDT + TSS + IST | `gdt ok` | Need a known code selector and a double-fault stack before the IDT is worth installing. |
| 4 | PIC remap and mask, skipped when the FADT has `IAPC_BOOT_ARCH` bit 0 clear | `pic: remapped` | Firmware may leave the 8259 live with vectors overlapping CPU exceptions. Bit 0 is `LEGACY_DEVICES`, not 8259 presence. QEMU clears it, so on QEMU this step writes nothing. The remap and mask that always runs is `arch::pic::program`, after TSC calibration and before step 13b's `sti` (§5.5; ROADMAP §20.1, F094). |
| 5 | IDT | `idt ok` | Exceptions become diagnosable. Hardware IRQs are still masked. |
| 6 | Buddy PMM from memory map | `pmm: N free 4KiB frames` | Page tables and heap both need frames. |
| 7 | Page tables, install CR3 | `paging: cr3 ok` | Own the address space before mapping anything device-specific. |
| 8 | MMIO PTE attribute patch | `paging: mmio uc` | LAPIC/IOAPIC/HPET pages must be uncacheable before first touch. |
| 9 | Kernel heap | `heap ok` | `alloc` becomes legal. Everything after this can use `Vec` and `Box`. |
| 10 | Kernel VA allocator | `kva: ready` | Guarded stacks need it, so threads need it. |
| 11 | Per-CPU area for the BSP, bootstrap TCB, syscall MSRs | `per_cpu: bsp ready` | `GS_BASE` must be valid before any `per_cpu!` access, including from ISRs. Then `thread_init::init_bootstrap` makes `_start`'s context the bootstrap thread, and `syscall_init::init_bsp` programs STAR, LSTAR, FMASK (`0x47700`), and `EFER.SCE`, enables SSE for user code (§3.1), and wires TSS.RSP0. `arch::cpu::harden` (SMEP, SMAP, UMIP, `CR0.WP`) runs just before this step, after `idt ok`. |
| 12 | ACPI tables | `acpi: xsdt N tables` | MADT drives APIC and SMP, HPET drives calibration. |
| 13 | Time: HPET or PIT, TSC calibration | `time: tsc N/ms` | The scheduler needs a tick, and AP bring-up needs `busy_wait_ms`. |
| 13b | BSP LAPIC, I/O APIC, LAPIC timer | `time: lapic_timer ok (<mode>)` | After TSC calib. Prove a tick (TSC-deadline → periodic → PIT), then mask PIC + PIT GSI if LAPIC owns it. |
| 14 | Scheduler, idle thread on BSP | `sched: cpu0 ready` | Preemption target must exist before the timer starts firing into it. |
| 15 | Arm scheduler; emit `irq: enabled` | `irq: enabled` | Scheduler is live. The timer already ticks from steps 13/13b; this marker is post-sched arming (IF on, preemption live), not the first STI. IRQ1 stays masked until the keyboard driver (step 17). |
| 16 | APIC + SMP bring-up | `smp: done` | Needs time (delays), heap (per-CPU allocation), scheduler (AP entry point). Live Phase 4 order: SMP before console. |
| 17 | Framebuffer console, PS/2, mux | `console ok` | After `smp: done`. Install the IRQ1 / keyboard GSI handler, init the 8042, then unmask. Replay the pre-FB log ring onto the framebuffer. |
| 17b | PCI enum + device registry | `pci: N devices` | After `console ok`. ECAM for the buses the first MCFG allocation covers (`acpi::parse_mcfg` reads no other entry; F045); otherwise `0xCF8`/`0xCFC`, which the kernel uses only for bus 0 (a kernel limit: configuration mechanism #1 addresses any bus; ROADMAP §20.1, F114). Scan builds a device list. Workqueue + threaded IRQ start, then drivers bind by id. Memory BARs are mapped through ioremap or the capped physmap; sizes above 32 MiB are recorded and skipped (DESIGN §4.1). |
| 17c | Block layer + ramdisk + virtio-blk + partitions | `block: <name> <n> sectors` | After bind. One line per device. virtio-blk (`vda`) emits during probe; ramdisk (`ram0`) follows in `block_init`; partition children (`<parent>p<N>`) after that. |
| 17d | VFS + FAT initrd root + pseudo mounts + vibefs | (none) | After block. Makefile FAT32 initrd at `/` when live, else dummy ramfs. Then devfs/procfs/tmpfs/sysfs on `/dev` `/proc` `/tmp` `/sys`. BSS vibefs at `/vibe` (Phase 8D). No serial marker. Syscalls do not reach the VFS or kernfs: `file_init` resolves paths through its own FAT and vibefs route tables (ROADMAP §10.4, F086). |
| 18 | `/hello`, builtins, `/sbin/init` as pid 1 | `shell ready` | Last marker. `user_init::boot_hello` runs `/hello` bound to the bootstrap thread, `shell_init::init` registers the builtins, and `proc_init::start_init` spawns `/sbin/init` pinned to the BSP. `/sbin/init` forks `/bin/tests` and waits for it without reading its exit status, then forks `/bin/sh`, which writes `shell ready` from ring 3 (`user/sh.asm`); the marker is not kernel-emitted and does not show that `/bin/tests` passed (ROADMAP §10.5, F073). A `kernel_shell` build instead spawns the kernel `shell` thread, which prints `shell ready`; a `kernel_tests` build runs the in-guest registry. |

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
Step 17b enumerates PCI (ECAM where the first MCFG allocation covers the bus, else CF8 on bus 0 only), maps
memory BARs under the 32 MiB cap, fills the device registry, and emits
`pci: N devices`. Workqueue workers and the threaded-IRQ bottom half start
next. Drivers register, then bind after the scan, not inline. Virtio-rng
matches by id when a modern virtio device is present (ktest adds one; e2e
does not). Ramdisk init follows bind and emits `block: <name> <n> sectors`.
Partition scan stamps an MBR on `ram0` and, on a `vda` of at least 1024 sectors whose table fails to
parse or has no entries, a GPT that overwrites LBA 0–33 and the last 33 sectors of a whole-disk image
on first boot ([section 10.5](#105-partitions); ROADMAP §10.11, F003). It emits
`block: <parent>p<N> <n> sectors` per child. A writeback cache thread starts
before the scan. `fs_init` then makes the FAT initrd `/` (a ramfs root only when the initrd is not
live) and mounts devfs / procfs / tmpfs / sysfs on `/dev` `/proc` `/tmp` `/sys` and vibefs at
`/vibe`, with no serial marker. Step 18 runs `/hello`, then starts `/sbin/init`; the trailing
contract line is `shell ready`, from `/bin/sh` (row 18).
The harness's `boot_contract_markers()` asserts the live order ([section 8.3](#83-end-to-end)).

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
non-canonical hole between. The map assumes 4-level paging (48-bit virtual addresses) on x86_64, as
§11.2 does on aarch64; 5-level paging (LA57, and LPA2 on aarch64) is ROADMAP §27.6's stretch, and
adopting it re-plans this table. Kernel regions are fixed, not discovered, except the physmap base
(below the table):

| Range | Size | Role |
|-------|------|------|
| `0x0000_0000_0000_0000` – `0x0000_7FFF_FFFF_FFFF` | 128 TiB | User address space, one PML4 per process (`AddressSpace`), below `USER_END`. Page 0 is never mapped (`NULL_GUARD_LEN`). The top 4 KiB page is mappable, so a `syscall` in its last two bytes leaves RCX non-canonical (`0x0000_8000_0000_0000`), and on KVM and hardware the exit `iretq` raises `#GP` on the user GS base (ROADMAP §10.6, F007). Each user PML4 copies the kernel's PML4[256..512) at creation, so the whole kernel half stays mapped, supervisor-only, while ring 3 runs: no KPTI (ROADMAP §18.3, F024, F133). |
| `0x0000_0000_0000_0000` – `0x0000_0000_2000_0000` | 512 MiB | Low identity window, kernel PML4 only (user PML4s do not copy slot 0). 2 MiB pages, GLOBAL; the first 2 MiB supervisor writable and executable. |
| *hole* | | Non-canonical. Any pointer here is a bug. |
| Limine's HHDM offset (`0xFFFF_8000_0000_0000` under the pinned Limine on x86_64) + | `map_end` ≤ 8 GiB, plus leaves added above it | Physmap, `virt = phys + ` the HHDM offset, discovered at boot (below the table); today the constant `HHDM_BASE`, which `boot::capture` asserts Limine's offset equals. 2 MiB pages up to `map_end`. Above it: 4 KiB leaves from `acpi_init::map_gap` (no cap), and write-back leaves for a display BAR0 from `paging_init::ensure_physmap_wb` (below `PHYSMAP_CAP`). |
| `0xFFFF_C000_0000_0000` – `0xFFFF_C000_0400_0000` | 64 MiB | Kernel heap. Starts at 1 MiB mapped and grows. Planned (ROADMAP §12.6): the region's size is set at boot from installed memory, up to the 16 TiB below the KVA region, so the heap can grow as far as RAM does ([§4.4](#44-kernel-heap)). |
| `0xFFFF_D000_0000_0000` – `0xFFFF_D010_0000_0000` | 64 GiB | Kernel VA allocator: guarded stacks, `vmap`, large transient mappings. |
| `0xFFFF_E000_0000_0000` – `0xFFFF_E000_1000_0000` | 256 MiB | `ioremap` window for device MMIO that should not be reached through the physmap. |
| `0xFFFF_FFFF_8000_0000` – `0xFFFF_FFFF_FFFF_FFFF` | 2 GiB | Kernel image. Matches the `kernel` code model so `.text` relocations fit in 32-bit displacements. |

Regions must not overlap and every one asserts that its range is unmapped before claiming it. This is
a real failure mode: two subsystems in the old tree were both designed at `0xFFFF_C000_*` and only one
noticed.

The physmap base is the one region the kernel discovers. It is Limine's HHDM offset, which the Limine
protocol says "may vary between boots, including for randomisation", and which an executable "must
not assume". The kernel adopts that offset as its own physmap base, so a physical address has the
same alias before and after its `mov cr3`, and reads it once into `BootInfo`; every physical-to-virtual
translation uses that one value. `boot::capture` checks that the physmap's range overlaps none of the
fixed regions in the table and halts with a named reason if it does. Rule; not yet enforced:
`paging_init::HHDM_BASE` is a constant, and `boot::capture` asserts that Limine's offset equals it
(ROADMAP §11.1). ROADMAP §18.2 later draws the other bases from entropy too.

The low identity window exists for one reason: an AP starting from SIPI runs in real mode and then
32-bit protected mode at `0x8000`, so that page must be identity mapped and executable. All 512 MiB
stay mapped and GLOBAL for the life of the kernel CR3, so a NULL-plus-offset access from a kernel
thread reads low RAM instead of faulting, and buddy frames below 2 MiB have a supervisor writable,
executable alias. Planned (ROADMAP §10.6, F085): the window is torn down after `smp: done`, keeping
only the trampoline page (4 KiB, read-only, executable, not global).

The physmap is capped at 8 GiB (`PHYSMAP_CAP`) regardless of what the memory map says. Some firmware
describes MMIO BARs as multi-terabyte regions, and walking that to build page tables at boot does not
finish. `paging_init::physmap_extent` sets `map_end` to the 2 MiB-rounded maximum of the usable-RAM
end, the kernel image end, and each framebuffer's end, capped at 8 GiB, and ignores raw memory map
entries. `acpi_init::map_gap` then adds 4 KiB leaves above `map_end` for ACPI tables (write-back) and
for the LAPIC, I/O APIC, and HPET (UC), with no cap. RAM above the cap never enters the buddy:
free-list nodes, page tables, and heap pages are all reached through the physmap after `mov cr3`, so a
frame past it triple-faults on first touch. The physmap covers a framebuffer only below the cap, so for a
framebuffer that extends past 8 GiB `fb_init` writes through an unmapped address and boot halts at
console init (ROADMAP §11.2, F020).

Planned (ROADMAP §11.2): one physmap policy on both architectures. The physmap maps only the
RAM-typed ranges of the memory map (usable, bootloader-reclaimable, executable and modules, ACPI
reclaimable, ACPI NVS), plus a write-back leaf for a firmware table outside them when it is first
read, and never device memory. MMIO is reached only through `ioremap`, and each framebuffer through a
mapping of its own range. A firmware region described as terabytes of MMIO then costs nothing, which
removes the reason for the cap, so the cap goes and RAM above 8 GiB joins the buddy. Rejected:
keeping x86_64's whole-range physmap with in-place UC patches beside aarch64's RAM-only one, which
would leave the portable page-table code two policies for one primitive (AGENTS.md rule 10) and keeps
the UC-alias bug class (F104) alive.

## 4.2 Physical memory: buddy allocator

Free blocks of order *k* cover 2^k contiguous 4 KiB frames. Split on allocation, merge with the buddy
on free. Free list nodes live inside the free pages themselves, so there is no bitmap and no
allocation needed to run the allocator.

That last property has a sharp edge: a stray write into a freed page corrupts the allocator's linked
lists, and the resulting crash happens later, somewhere unrelated. This is exactly why kernel stacks
get guard pages.

```rust
pmm_init::with_buddy(|b| ..)                       // the global `Buddy`, RANK_BUDDY, IRQ-off
Buddy::allocate_frame() -> Option<PhysAddr>        // order 0
Buddy::allocate(order: u8) -> Option<PhysAddr>     // order <= MAX_ORDER (10)
Buddy::allocate_constrained(bytes, align, boundary) -> Option<(PhysAddr, u8)>  // DMA, §4.7
unsafe Buddy::deallocate_frame(pa: PhysAddr)
unsafe Buddy::deallocate(pa: PhysAddr, order: u8)  // asserts alignment and no double free
Buddy::stats() -> PmmStats                         // total, free, largest free order
```

Initialization walks the Limine memory map and ingests every `USABLE` region as power-of-two aligned
blocks, excluding:

- physical frame 0
- the loaded kernel image span
- the AP trampoline page at `0x8000`
- the framebuffer
- anything not marked `USABLE`, including bootloader and ACPI reclaimable
- anything above the 8 GiB physmap cap (§4.1)

`stats().free_frames` is a running counter, so `meminfo` costs O(1). `deallocate` does not: its
double-free check and its buddy lookup walk the free lists, O(`MAX_ORDER` × list length) per call, so
tearing down a large address space holds PT with IRQs off for that long (ROADMAP §12.1, F029).

## 4.3 Page tables

The kernel builds its own PML4 from buddy frames rather than editing Limine's. Contents at install
time:

1. Kernel image, mapped per section with correct permissions.
2. Physmap over `[0, map_end)` at the HHDM offset, using 2 MiB pages.
3. Low identity window, 512 MiB, 2 MiB pages, first 2 MiB executable.
4. The bootloader stack window, duplicated out of Limine's active tables so `_start`'s own stack keeps
   working across the `mov cr3`.

Then set `EFER.NXE` if it is not already on, load CR3, and print `paging: cr3 ok`. Immediately after,
`acpi_init` patches the physmap PTEs covering the LAPIC, I/O APIC, and HPET to PCD + PWT. An address
above `map_end` first gets fresh 4 KiB UC leaves (`map_gap`). Inside `map_end`,
`Mapper::patch_physmap_uc` marks the whole covering 2 MiB leaf UC without splitting it, so RAM that
shares the leaf becomes UC too, and its walk can skip a trailing leaf of an unaligned range (ROADMAP
§11.2, F104). Planned (ROADMAP §11.2): these devices move to `ioremap`, the physmap maps no device
memory, and this patch and `map_gap`'s UC leaves are deleted (§4.1).

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
| Low identity, first 2 MiB | present, global, writable, executable (trampoline) |
| Low identity, rest | present, global, writable, NX |

NX everywhere by default. The one thing that must stay executable low down is the trampoline page;
mapping the whole identity window NX is how the old tree produced a page fault during AP bring-up that
looked exactly like a hang.

The physmap covers the kernel image's frames, so it is a writable alias of `.text` and `.rodata`:
W^X holds per virtual address, not per frame (ROADMAP §18.1, F105).

MMIO gets PCD + PWT unconditionally. QEMU ignores cache attributes and write-back MMIO appears to
work, so this bug only shows up on real hardware, months later, as inexplicable device behavior.
The Limine framebuffer stays write-back on the HHDM physmap for now (QEMU-tolerant). WC/UC remap of
scanout is a later polish pass; double buffering is also parked (ROADMAP §5.1).

### TLB

- `invlpg` after any single-PTE edit, including MMIO attribute patches.
- Kernel mappings are `GLOBAL`. They survive a CR3 reload only where `CR4.PGE` is set: the
  trampoline sets it on each AP, and the BSP keeps the CR4 Limine left, with PGE clear under the
  pinned Limine (ROADMAP §10.6, F026, F085). Unmapping one requires a shootdown on every online CPU
  before the VA can be reused. See [section 7.9](#79-tlb-shootdown).
- A kernel-half edit runs a local `invlpg`, drops PT, then calls `paging::tlb_shootdown_others(va)`,
  a hook that `ipi_init::init` points at `ipi_init::shootdown_va` before the first AP starts (§7.9).
  Host tests, and boot before `ipi_init::init`, leave the hook unset; the local `invlpg` is enough
  there.

## 4.4 Kernel heap

A free-list heap at `HEAP_START`, backed by buddy frames mapped writable + NX. Initial mapping is
1 MiB; the allocator grows in page-sized increments up to the 64 MiB region limit. `GlobalAlloc`
takes the heap lock, a `SpinMutex`, so interrupts are off for each `alloc` and `dealloc`, and the rank
check refuses an allocation or a free made while a spinlock ranked after the heap is held (§2.1).

Planned (ROADMAP §12.6): the heap region is sized at boot from installed memory, so a heap allocation
fails only when frames run out. The two limits must be one because the failure policy below treats
every failure as a shortage of memory. A fixed region smaller than RAM would fail allocations while
frames are free, and reclaim and the OOM killer would then kill processes to make room that
memory already had.

Allocation failure has two policies, chosen by who can cause it:

- On a path that untrusted input reaches (a syscall, device data, a disk image, a network packet),
  allocation is fallible: through `vibeos::kalloc`'s owning types, whose failure becomes `ENOMEM`
  (or the errno Linux returns there, such as `EAGAIN` from `fork`). Where the
  context may sleep ([§2.9](#29-preemption-and-interrupt-state) rule 4), ROADMAP §12.6's direct
  reclaim and OOM killer run before the allocation reports failure. A user who exhausts memory
  gets an errno or the OOM killer's verdict, never a kernel halt.
- The infallible `alloc` API (`Box::new`, `Vec::push`, `vec!`, `format!`, `String` growth,
  `Arc::new`) is allowed only during boot, before `irq: enabled`, and for an allocation whose size
  and count a kernel invariant bounds. Its failure reaches `#[alloc_error_handler]`, which panics
  with the requested layout: there, a failure means a kernel invariant is false, and silent OOM is
  worse than a halt.

Fallibility is carried by type, not by a list of methods. The infallible surface of `alloc` is
large: `BTreeMap::insert`, `extend`, `collect`, `clone`, `to_vec`, `String::from`, and every other
growing call. On stable Rust, which `vibeos-core` uses (§1.1), `Box`, `Arc`, and `BTreeMap` have no
fallible constructor at all. So `vibeos-core` has a `kalloc` module of owning types (`TryBox`,
`TryVec`, `TryString`, `TryArc`, and an ordered map) whose every growing operation returns
`Result`. They are built on stable Rust: `alloc::alloc::alloc` with a null check for boxes, and
`try_reserve` for vectors. Clippy's `disallowed-types` denies `alloc`'s owning types (`Box`, `Vec`,
`String`, `Arc`, `Rc`, and the `alloc::collections` types) in both crates outside `kalloc`, and
`disallowed-macros` denies `vec!` and `format!`. A boot-time or invariant-bounded site that keeps
an `alloc` type carries an `#[allow]` whose comment names its bound. This is the shape Rust-for-Linux
settled on (`KBox`, `KVec`) after starting from `alloc`'s collections. Rejected: a
`disallowed-methods` list of infallible constructors, which misses the calls it does not name and
leaves `Box` and `Arc` with no fallible path on stable Rust.

Rule; not yet enforced: syscall paths use the infallible API today, and a `fork` near exhaustion
panics in `spawn_inner` (F010). ROADMAP §10.4 lands `kalloc` and the lints. Rejected: making small allocations never fail by having the allocator wait
until the OOM killer frees memory (Linux's "too small to fail"), because an allocation made with a
spinlock held, or on a path the OOM victim needs in order to exit, cannot wait, and a failed
`Box::new` cannot be handled by its caller; AGENTS.md rule 4 forbids a user-triggerable panic.

Direct reclaim (ROADMAP §12.6) runs inside an allocation that failed, on the allocating thread, so it
must need nothing that thread might hold. The thread may hold a filesystem's inode or block-mapping
lock, its own address-space lock, or a busy page-cache page (§2.1's sleeping tier). Rules:

1. Direct reclaim runs only for an allocation that began with IF=1, this CPU's `HELD` rank mask
   empty, and a calling thread that is not a no-reclaim thread. The allocator reads all three at
   entry, before it takes the heap lock. Any other allocation draws on ROADMAP §12.6's reserve pool
   and then fails.
2. It frees clean pages only. It drops clean page-cache pages. It unmaps clean mapped ones through
   the reverse map, taking only each address space's page-table spinlock and a try-lock of the page,
   and it skips any page it cannot take at once. It takes no sleeping lock, the address-space lock
   included.
3. It writes no page. The ROADMAP §12.5 writeback threads write dirty file pages, and ROADMAP
   §12.7's swap-out thread writes anonymous pages. Direct reclaim wakes those threads and then waits, with a
   deadline, only for writes already submitted to a device. When that frees too little, the OOM
   killer runs.
4. It never recurses. These are no-reclaim threads, whose allocations use the reserve pool and
   then fail: the writeback threads, the swap-out thread, threaded interrupt bottom halves (§5.4), a
   workqueue worker while it runs a softirq-equivalent item (§2.2), and a thread already in reclaim.

Why: writeback or an unmap that needed a lock the allocating thread holds would deadlock that thread
on itself. For example, a filesystem that allocates while holding its volume lock would reach
writeback of its own dirty pages. A bottom half that waited on reclaim could wait for an I/O
completion that only it can deliver.

Linux guards the same recursion with per-call `GFP_NOFS` and `GFP_NOIO` flags and has moved page
writeback out of direct reclaim. These rules take the second route everywhere, so no call site
carries a flag. Rejected:
- per-call reclaim flags, the shape of Rust-for-Linux's `KBox::new(x, flags)`, which add one more
  decision to every allocation;
- writeback from direct reclaim, which needs those flags.

Cost: when most reclaimable memory is dirty, an allocation waits for the writeback threads rather
than writing itself. ROADMAP §12.5's dirty limit throttles writers before it comes to that.

An operation past its point of no return cannot unwind what it built, so it makes every allocation it
needs before that point and only releases after it:

- A filesystem commit allocates its blocks, and the memory its switch to the new generation uses,
  before it starts the superblock write ([VIBEFS.md](VIBEFS.md) §10). Once the superblock is
  durable, memory must switch to the new generation, and a switch that fails halfway for want of
  memory leaves memory matching neither generation.
- `execve` builds the new address space, its stack and arguments included, before it swaps it in, and
  after the swap only releases: close-on-exec descriptors and the old address space. After the swap
  there is no old image to return an errno to. Rejected: Linux's order, which allocates past its
  point of no return and kills the process with `SIGSEGV` when that fails; building first costs
  holding both images' page tables until the swap.

The heap is deliberately simple and deliberately temporary. A slab allocator for hot object types
(TCBs, file descriptors, inodes, network buffers) lands in ROADMAP §19.9; general
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
- Freeing the stack you are running on does not work. Rule: a dead thread's stack is freed only
  after the CPU that ran it has switched off it (§2.8). Not yet enforced: `thread_exit` puts the
  stack on the global 8-slot `kva_init::DEFERRED` list, and any CPU's `reap_zombies` can drain it
  while the exiting CPU is still between `defer_free` and `switch_context` on that stack
  (ROADMAP §10.10, F012); `defer_free` panics when all 8 slots are full (ROADMAP §10.10, F010).

Default kernel stack is 4 pages (16 KiB) plus guard. If that turns out to be tight, raise it rather
than debugging mysterious corruption.

## 4.6 What comes later

The roadmap covers these in detail. Listed here so the interfaces above are designed with them in
mind:

- Demand paging (ROADMAP §12.2). `map_page` gains a "reserve VA, populate on fault" mode, and `#PF`
  becomes a recoverable exception with a real fault handler rather than a halt.
- Copy on write (ROADMAP §12.3). `fork` clones an address space by sharing frames read-only with a
  refcount; the write fault does the copy. Needs per-frame metadata, which means the PMM grows a
  `struct Frame` array (ROADMAP §12.1).
- Slab caches, per-CPU magazines to avoid the global buddy lock on hot paths (ROADMAP §19.9).
- Page cache unified with `mmap`, so file-backed pages and anonymous pages share eviction (ROADMAP
  §12.5).
- Swap, which needs reverse mappings from a frame back to every PTE referencing it (ROADMAP §12.7).

The per-frame metadata array is the pivot. Refcounting, reverse mapping, and page cache all need it,
so the PMM should be built expecting it to appear.

## 4.7 DMA

`DmaBuffer` is physically contiguous (buddy `allocate_constrained`: size, alignment, and an optional
power-of-two boundary the buffer must not cross). A boundary is not an address limit:
`DmaAlloc::dma32` sets a 4 GiB boundary, so its buffer never crosses a 4 GiB line, but the buffer can
lie above 4 GiB once RAM extends there, and no allocator keeps a 32-bit device's buffer below 4 GiB
(ROADMAP §20.6, F030). The device-visible address is `dma_to_device(phys)` (identity until an IOMMU
exists), never a physmap virtual address.

`sync_for_device` / `sync_for_cpu` always run at the API boundary. On x86 they are `fence(Release)` +
`sfence` and `fence(Acquire)` + `lfence`. Descriptor publish stores the index after that store-side
barrier, not a bare `compiler_fence`.

Neither barrier orders a store before a later load from another address. After the driver stores
`avail.idx`, it loads `avail_event` (EVENT_IDX) or `used.flags` to decide whether to kick; virtio 1.2
§2.7.13.4.1 requires a full barrier (`mfence`) between the two, and `SplitQueue::get_used` needs one
after its `used_event` store. Without them the driver and the device can each miss the other's
update and the queue stops. `SplitQueue::should_kick` and `get_used` have neither (ROADMAP §10.3,
F016).

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

User-memory access will go through `copy_from_user`/`copy_to_user`, which dereference the user VA
inside `stac`/`clac` (ROADMAP §10.6); pointer ranges are validated before use (ROADMAP §9.3). Today it
is `AddressSpace::read_bytes`/`write_bytes` after `check_user_range`, copying through the HHDM physmap
(a supervisor mapping, so SMAP does not apply until a user-VA accessor exists), and `write_bytes` does
not check the PTE's `WRITABLE` bit. `arch::cpu::harden()` sets `CR4.SMEP|SMAP|UMIP` where CPUID allows
and asserts `CR0.WP` on every CPU; `stac`/`clac` are no-ops when SMAP is missing.

Each CPU gets its own GDT and TSS (`gdt::CpuTables`): the BSP's lives in a `BootCell`, and
`gdt::alloc_ap_tables` allocates each AP's. TSS.RSP0 is the stack an interrupt or exception from
ring 3 lands on. `syscall` does not read the TSS; its entry loads `PerCpu.kernel_rsp0`.
`syscall_init::set_rsp0_for` sets both to the incoming thread's stack top on every switch (the
per-CPU `fallback_rsp0` for a thread with no stack of its own); the CPU never writes RSP0. The TSS
also holds the IST array.

| Gate IST field | `IstSlot` (index) | Use |
|----------------|-------------------|-----|
| 1 | `DoubleFault` (0) | `#DF`. A double fault often means the stack is gone, so it needs a stack that is known good. |
| 2 | `Nmi` (1) | NMI |
| 3 | `MachineCheck` (2) | `#MC` machine check |
| 4 | `Debug` (3) | `#DB` debug |

IST stacks are page-aligned guarded stacks from the KVA allocator (4 mapped pages + unmapped guard),
one per slot, per CPU. `IstSlot::index` is zero-based and selects the TSS `ist` entry;
`IstSlot::hardware` (index + 1) is the one-based IST field of the IDT gate. Off by one here means the
double fault handler runs on the broken stack and turns into a triple fault, which QEMU reports as a
silent reboot loop.

## 5.2 IDT and exceptions

One IDT of 256 gates (`arch::idt::IDT`) is shared by every CPU. `idt::init` gives every vector a
default handler (`install_defaults`) that passes a ring-3 fault to `proc_init::try_user_fault` and
otherwise dumps and halts, then installs the named exception, PIC, LAPIC, and IPI handlers
(`overlay_named`). Every gate is a DPL-0 interrupt gate (`IdtEntry::interrupt`, type `0x8E`): it
clears IF on entry, and `int n` from ring 3 raises `#GP`. `#DF`, NMI, `#MC`, and `#DB` run on IST
stacks (§5.1). Handlers use the `x86-interrupt` ABI. `idt::set_handler` also lets `irq_init`
(`0x31`–`0x7F`) and `kbd_init` (`0x21`, `0x30`) install gates of their own, which skip the GS step
([section 5.10](#510-privilege-transitions) rule 1).

`catch::intercept` runs first in every exception handler, in every build; only the in-guest test
registry (`kernel_tests`) arms it. Planned (ROADMAP §10.2, F146): it compiles only under
`kernel_tests` and acts only on a CPL-0 frame on the CPU that armed it.

Rule: ring 3 never halts the kernel. Each exception vector has one row below. The Ring 3 column is
the rule; the last column says what the code does where it differs. Planned (ROADMAP §10.6, F005):
the table lives in code, and a host test checks that each vector `0x00`–`0x1F` has a ring-3 row.

| Vector | Name | Ring 0 | Ring 3 | Ring 3, as built |
|--------|------|--------|--------|------------------|
| `0x00` | `#DE` | dump, halt | `SIGFPE` | as the rule |
| `0x01` | `#DB` | dump on IST, halt | `SIGTRAP` (RFLAGS.TF, `int1`) | halts the kernel. Rule; not yet enforced: ROADMAP §10.6 (F005) |
| `0x02` | NMI | dump on IST, halt; the panic stop is IPI `0xFE`, not NMI | not a ring-3 fault: the Ring 0 column applies | as the rule |
| `0x03` | `#BP` | log, continue | `SIGTRAP` (`int3`) | `int3` hits the DPL-0 gate, raises `#GP`, and gets `SIGSEGV`. Rule; not yet enforced: ROADMAP §10.6 (F148) |
| `0x04`, `0x05`, `0x07`, `0x0A` | `#OF`, `#BR`, `#NM`, `#TS` | dump, halt | `SIGSEGV` | `sig_for_vec` has no row, so one would halt the kernel. Rule; not yet enforced: ROADMAP §10.6 (F005) |
| `0x06` | `#UD` | dump, halt | `SIGILL` | as the rule; an SSE floating-point error also arrives here (row `0x13`) |
| `0x08` | `#DF` | dump on IST, halt | not a ring-3 fault: the Ring 0 column applies | as the rule |
| `0x0B`, `0x0C` | `#NP`, `#SS` | dump, halt | `SIGBUS`; `SIGSEGV` for a fault on the return-to-user `iretq` (§5.10 rule 2) | the `iretq` case halts. Rule; not yet enforced: ROADMAP §10.6 (F007) |
| `0x0D` | `#GP` | dump with error code, halt | `SIGSEGV`, including a fault on the return-to-user `iretq` (§5.10 rule 2) | the `iretq` case halts. Rule; not yet enforced: ROADMAP §10.6 (F007) |
| `0x0E` | `#PF` | dump with CR2, halt. Planned (ROADMAP §10.6): a fault inside a user-memory accessor returns `EFAULT` | `SIGSEGV`. Planned (ROADMAP §12.2): a fault on a page that a region reserves is resolved first | as the rule |
| `0x10` | `#MF` | dump, halt | `SIGFPE` | cannot fire: `CR0.NE` is clear, so an x87 error raises the masked IRQ13 and is lost. Rule; not yet enforced: ROADMAP §10.6 (F026) |
| `0x11` | `#AC` | dump, halt | `SIGBUS` | halts the kernel if `CR0.AM` is set (INIT clears it on each AP and no kernel code sets it; the BSP keeps Limine's value). Rule; not yet enforced: ROADMAP §10.6 (F005) |
| `0x12` | `#MC` | dump on IST, halt; `CR4.MCE` is clear, so a machine check shuts the CPU down with no dump (ROADMAP §10.6, F026) | not a ring-3 fault: the Ring 0 column applies | as the rule |
| `0x13` | `#XF` | dump, halt | `SIGFPE` | arrives as `#UD` and gets `SIGILL`: `CR4.OSXMMEXCPT` is clear. Rule; not yet enforced: ROADMAP §10.6 (F026) |
| `0x09`, `0x0F`, `0x14`–`0x1F` | reserved, `#VE`, `#CP`, `#HV`, `#VC`, `#SX` | dump, halt | `SIGSEGV` | `sig_for_vec` has no row, so one would halt the kernel. Rule; not yet enforced: ROADMAP §10.6 (F005) |
| `0x20`–`0xFF` | IRQs and IPIs | handle, return; an unused vector or an unexpected PIC line halts (§5.5) | handle, return to ring 3 | `0x21`, `0x30`, and `0x31`–`0x7F` run on the user GS base and halt the kernel. Rule; not yet enforced: ROADMAP §10.6 (F004) |

A halting handler prints the interrupt frame (RIP, CS, RFLAGS, RSP, SS), the error code where the
vector pushes one, and CR2 for `#PF`, then the common dump (§2.5). A halt with no register dump is a
wasted crash. A ring-3 kill prints `user: pid N killed SIG<name> rip=0x<rip> err=0x<err>`, plus
` cr2=0x<addr>` for `#PF` (a `user:` line, not a `vibeOS:` marker), and ends the process through
`finish_exit`. A ring-3 fault with no bound process (the in-guest test that calls `enter_user`
directly) falls through to the halt; that test's own `#UD` is taken first by the `catch` it armed.

## 5.3 Vector map

Vector number is priority on x86: the CPU's task priority compares `vector >> 4`. IPIs sit high so a
reschedule or shootdown is not starved by a busy NIC.

| Vector | Owner |
|--------|-------|
| `0x00`–`0x1F` | CPU exceptions. Reserved by hardware. |
| `0x20`–`0x2F` | Legacy PIC IRQ0–15 after remap. Live only until the I/O APIC takes over. `0x20` PIT, `0x21` keyboard, slave base `0x28`. |
| `0x30` | Keyboard: the I/O APIC route of ISA IRQ1 (`vectors::KBD`), fixed. |
| `0x31`–`0x7F` | Device pool (`DEVICE_VEC_START`..=`DEVICE_VEC_END`): I/O APIC GSIs and MSI/MSI-X, allocated at runtime. |
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
inside a device hard-IRQ: `irq_init::dispatch` sets a per-CPU `IN_ISR` flag around the handler. The
timer, IPI, and keyboard ISRs do not set it, and no blocking primitive checks it (ROADMAP §10.3,
F110). That flag is not
`InterruptGuard` nest: in-guest tests hold a guard on the BSP, so a nest check would
false-refuse every allocate from `ktest`.

The allocator records the dest CPU. `set_affinity` updates that binding. I/O APIC
routes are rewritten immediately; MSI/MSI-X callers reprogram the message from
`cpu_of`. The ROADMAP §19.5 rebalance uses this table rather than a second map.

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

Rule: `free_vector` returns only after no CPU is running that vector's top half
and its threaded bottom half has finished or been cancelled, so a driver may free
the state its handlers touch as soon as it returns. Teardown order for a device
is: stop the device (status 0, bus mastering off), free its vectors, then free
its DMA memory and handler state (§2.11 rule 3). Not yet enforced: `free_vector`
does not wait for a handler already running on another CPU (ROADMAP §20.9).

EOI is the dispatcher's job, not the driver's. The dispatch layer knows whether a
vector arrived via PIC or LAPIC and signals the right controller.

A threaded handler's top half runs in `dispatch` (ack / mask / wake only). Today one
kernel thread, pinned to the last online CPU, runs every threaded vector's bottom half.
Planned (ROADMAP §12.5): each threaded vector gets its own bottom-half thread, pinned to the
CPU the vector is routed to and moved by `set_affinity`, so a device with one queue per CPU
gets one thread per queue. A bottom half may block only on its own device's state. It takes
no lock of §2.1's sleeping tier and never waits for another device's I/O, and it allocates
without direct reclaim (§4.4). `free_vector` ends the vector's thread before it returns.

Why: one shared thread puts every device's completions on one CPU, although multi-queue
devices (virtio-blk today, NVMe in ROADMAP §20.4, RSS in Phase 28) spread them across CPUs
on purpose. One bottom half that blocks also stalls every other device, including the one
whose completion it waits for. One thread per threaded interrupt is Linux's model.
Rejected: one shared thread whose handlers promise never to block, which nothing checks;
and one bottom-half thread per CPU, in which one device's blocked handler still stalls
another device's on that CPU.

`set_threaded` is refused inside a hard-IRQ. The top half is optional:
`set_threaded(vec, None, work)` is accepted, and `dispatch` EOIs before the bottom half runs, so on a
level-triggered INTx route a device that nothing quiets raises the line again at once (ROADMAP §15.2,
F099). The softirq stand-in is the
high-prio workqueue: IRQ context enqueues a `fn(usize)` and wakes workers.

## 5.5 8259 PIC

The PIC is a bootstrap artifact and a fallback, nothing more.

- Remap master to `0x20`, slave to `0x28`. Firmware may leave them at vectors `0x08`–`0x0F`, which
  collide with CPU exceptions (`#DF` at `0x08`, `#TS` through `#PF` at `0x0A`–`0x0E`), so a spurious
  IRQ before remap looks like a CPU exception.
- Mask everything (`0xFF` to both data ports) immediately after remap.
- After TSC calibration ([section 3.3](#33-_start-order) step 13b), `apic_init::prove` arms the LAPIC
  timer (TSC-deadline, then periodic) with interrupts enabled and checks that it ticks. If it ticks,
  it is the tick. If not, the PIT drives the tick through LINT0 ExtINT, the only case that unmasks
  PIC IRQ0. `on_timer_tick` does nothing until the idle thread exists. The `irq: enabled` marker
  (step 15) follows `sched: cpu0 ready`. The keyboard (step 17) takes vector `0x30` through the
  I/O APIC whenever `kbd_init` can route ISA IRQ1; PIC IRQ1 is unmasked only when there is no route
  and the PIT owns the tick. The default PIC handler halts on an unexpected line.
- Once the I/O APIC routes devices and the LAPIC timer is verified ticking, mask the PIC completely.
  Leaving it live means every interrupt is delivered twice.
- Keep the PIT driver code. It is still the calibration fallback and still provides the delays that AP
  bring-up needs.

FADT `iapc_boot_arch` bit 0 is `LEGACY_DEVICES`, not 8259 presence: QEMU clears it and still has an
8259. The PIC step before `lidt` skips its ICW sequence when the bit is clear, but a second remap and
mask after TSC calibration always runs, so the PIC is remapped and masked before the first `sti` on
every boot. `pic: remapped` means this step finished. Deciding presence from the MADT
`PCAT_COMPAT` flag with a mask read-back probe is ROADMAP §20.1 (F094).

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
- Interrupt affinity *rebalancing* (ROADMAP §19.5). The dest-CPU table and `set_affinity`
  already exist; what is left is a policy that moves MSI-X messages and IOAPIC
  dests when a queue saturates one core.

## 5.10 Privilege transitions

Every entry to and exit from ring 3, and the state each boundary must hold. "User GS" means
`GS_BASE` holds the user base (0; `enter_user` and `enter_user_full` write it) and `KERNEL_GS_BASE`
holds this CPU's `PerCpu`. "Kernel GS" means `GS_BASE` holds this CPU's `PerCpu`; `KERNEL_GS_BASE`
then holds the user base after a `swapgs`, or the `PerCpu` address after `per_cpu_init::init_bsp`,
`install_gs`, or `arch::gs::force_kernel` (§7.5). The table is the required state. A rule the
code does not meet yet says "Rule; not yet enforced", names the ROADMAP line that lands it, and then
describes the current code.

| Point | CPL, stack | GS | IF | AC |
|-------|------------|----|----|----|
| `vibeos_syscall_entry`, before its `swapgs` | 0, user RSP | user | 0 (FMASK) | 0 (FMASK) |
| syscall entry after its `swapgs`, and the syscall body | 0, `PerCpu.kernel_rsp0` | kernel | 0 until the stub has moved the user RSP out of the scratch and runs `sti`; 1 in the body ([§2.9](#29-preemption-and-interrupt-state) rule 3) | 0 |
| syscall exit, from the return of `vibeos_syscall_stub` to `sysretq` or `iretq` | 0, kernel stack, then the user RSP | kernel; user after `swapgs` | 0 (rule 4) | 0 |
| `enter_user` and `enter_user_full`, from `mov gs` to `iretq` | 0, kernel stack | user | 0 (rule 4) | 0 |
| non-IST vector taken at CPL 3 | 0, TSS.RSP0 | user until the stub's `swapgs` | 0 (interrupt gate); a fault or trap body then runs with IF=1, an interrupt's top half with IF=0 (§2.9 rule 3) | ring 3's until the stub's `clac` (rule 5) |
| non-IST vector taken at CPL 0 | 0, interrupted stack | kernel, except rule 2's case | 0 (interrupt gate) | the interrupted value until the stub's `clac` (rule 5) |
| IST vector: `#DF`, NMI, `#MC`, `#DB` | 0, its IST stack | whatever the interrupted point held (rule 3) | 0 | the interrupted value until the stub's `clac` (rule 5) |
| vector exit to CPL 3 | 0, then 3 at `iretq` | user after `swapgs` | 0 until `iretq` restores ring 3's | `iretq` restores ring 3's |

1. One entry stub per vector, owned by `arch/idt.rs` and generated from one table. The stub runs
   `cld`, `clac` when SMAP is live, and the GS decision, calls a body function, and mirrors the GS
   decision on exit. `idt::set_handler` takes a body function, never a gate, and no
   `extern "x86-interrupt"` function exists outside `src/arch/`. Rule; not yet enforced: ROADMAP
   §10.6 (F004). Each `arch/idt.rs` handler calls `gs_enter` and `gs_leave` itself, and
   `irq_init::device_irq::<N>` (`0x31`–`0x7F`), `kbd_init::kbd_ioapic` (`0x30`), and
   `kbd_init::kbd_pic` (`0x21`, replacing the `arch/idt.rs` IRQ1 gate) are installed through
   `idt::set_handler` with no GS step. One of them taken at CPL 3 reads `gs:[0]` at VA 0 and halts
   the kernel.
2. A non-IST vector decides `swapgs` from the saved CS.RPL (`arch::gs::from_user`), with one
   exception: a `#GP`, `#NP`, or `#SS` whose saved RIP is a return-to-user `iretq` arrives with the
   kernel CS and the user GS, and its handler swaps GS and sends the process `SIGSEGV`. User
   mappings end at `USER_MAP_END` (`0x0000_7FFF_FFFF_F000`), so a `syscall` in the last user page
   cannot leave a non-canonical return RIP. Rule; not yet enforced: ROADMAP §10.6 (F007). The
   syscall exit sends a non-canonical return RIP to `swapgs; iretq`, and on KVM or hardware the
   `#GP` that `iretq` raises runs on the user GS base and ends in a silent hang or triple fault;
   TCG skips the canonical check.
3. An IST vector decides `swapgs` from the sign of `GS_BASE` (`rdmsr`; a kernel base is negative),
   because it can interrupt CPL-0 code that has the user GS loaded (the rows above whose GS column
   says user), and on exit it restores the GS state it found. Rule; not yet enforced: ROADMAP §10.6
   (F007); the IST handlers decide from CS.RPL. The sign test fails once FSGSBASE lets userspace
   write a kernel-half GS base. Planned (ROADMAP §18.3, F133): with FSGSBASE on, IST entry saves `GS_BASE` with `rdgsbase`, loads this CPU's
   `PerCpu` pointer, and restores the saved value on exit.
4. IF=0 from the first instruction of a return-to-user sequence to its `sysretq` or `iretq`:
   `PerCpu.syscall_scratch` holds the return value and the `iretq` frame per CPU, not per thread,
   and the GS base is the user's for part of each sequence. Required: the syscall exit runs `cli` right after
   `call vibeos_syscall_stub`; `enter_user` and `enter_user_full` run `cli` before `mov gs`; each
   checks IF=0 in debug builds; `iretq` restores ring 3's IF from the frame. Rule; not yet enforced:
   ROADMAP §10.6 (F001, F006). The syscall exit has no `cli`, and `console_init::wait_key`
   returns with IF=1 from its `sti; hlt` (F001); `enter_user_full` runs with IF=1 (F006);
   `syscall_init::run_user` runs `cli` before `enter_user`.
5. Every interrupt and exception entry clears RFLAGS.AC before any other code: an interrupt gate
   clears IF and TF but not AC, and ring 3 can set AC with `popf`. The syscall entry clears AC
   through FMASK (bit 18). Rule; not yet enforced: ROADMAP §10.6 (F088); no interrupt or exception
   entry runs `clac`. Planned (ROADMAP §10.6):
   `stac` appears only inside the user-memory accessors.
6. A handler on an IST stack does not block and does not switch threads: a nested entry of the same
   vector restarts at the top of that IST stack and overwrites the first frame. It also takes no lock
   ([§2.2](#22-interrupt-handler-rules)'s last row). Holds, because every IST handler halts, except
   that under `kernel_tests` an armed `catch` longjmps off the IST stack.
   Planned (ROADMAP §10.6, F005): for a CPL-3 frame, `debug_ex` moves from the IST stack to the
   thread's kernel stack and then calls `try_user_fault` (the ring-3 `#DB` kill §5.2 requires),
   because `try_user_fault` ends in `finish_exit`, which can switch threads.
7. Every gate but `#BP` is DPL 0, so `int n` from ring 3 raises `#GP`; the `#BP` gate is DPL 3, so
   `int3` delivers `SIGTRAP`. RFLAGS.TF and `int1` (`0xF1`) reach `#DB` at any DPL. Rule; not yet
   enforced: ROADMAP §10.6 (F148). Every gate is DPL 0 today (`IdtEntry::interrupt`, type `0x8E`).
8. Ring 3 never halts the kernel: §2.5 states the rule, and the §5.2 table gives each vector's ring-3
   action. Rule; not yet enforced for each §5.2 row whose last column names a ROADMAP line.

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

The BSP calibrates `tsc_per_ms` once (`time_init::init`), and every CPU uses that value through
`time_init::tsc_per_ms()`: delays, the TSC-deadline arm, and `now_ns`. Each CPU's `PerCpu.tsc_per_ms`
holds a copy that only an in-guest test reads (ROADMAP §10.7 deletes it, F111). The LAPIC periodic
count is also measured once, on the BSP, and `apic_init::arm_ap` reuses it on every AP. Both assume
one TSC rate and one LAPIC timer rate on every CPU. `time_init::init` checks the invariant TSC CPUID
bit and prints `vibeOS: time: invariant tsc absent` when it is clear, because everything downstream
assumes the TSC does not change rate. Planned (ROADMAP §10.7): each AP measures its TSC against the
BSP's at bring-up, and a marker reports the largest skew.

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

Each fallback is worse than the one above it, and all three must work. CI runs two of them. Under
TCG, QEMU never advertises `CPUID.01H:ECX[24]`, so `make test-kernel` (`-cpu max`) and
`make test-lapic-fallback` (`-cpu qemu64,-tsc-deadline`) both take the periodic path, and
`make test-e2e-pit` (`-machine pc,hpet=off`) boots on the PIT tick. `arm_tsc_deadline`,
`rearm_deadline`, and the `TscDeadline` arm of `apic_init::arm_ap` run only under KVM or on hardware,
and no CI tier runs them. Planned (ROADMAP §10.1, F078): the nightly KVM leg runs them. Under KVM,
`-cpu qemu64,-tsc-deadline` forces the periodic path.

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

Publish them as a seqlock. The writer bumps the sequence to odd, issues `fence(Release)`, stores both
fields, and bumps the sequence to even with Release. The reader loads the sequence with Acquire, loads
both fields, issues `fence(Acquire)`, reloads the sequence, and retries on an odd or changed value.
`TickClock::write` has no release fence after the odd bump, and the release half of its
`fetch_add(AcqRel)` orders only earlier accesses. x86's locked `fetch_add` is a full barrier and hides
that; aarch64 with LL/SC atomics does not (ROADMAP §10.8, F098).

Two warnings about testing this. A test that computes the expected "now" from the same tick value it
just read is monotonic by construction and passes even with torn reads, so the test needs an
independently published timestamp to compare against. And a single-threaded test never sees the race,
so the in-guest coverage has to read the clock from threads that yield while a timer fires and compare
it with that independent timestamp. The in-guest tests `now_us_monotonic` and `now_us_under_yields`
do neither. `now_ns` returns `LAST_NS.fetch_max(n).max(n)`, which never goes below an earlier return,
so neither test can fail, and the yields variant runs one thread that calls `hlt_once`. Only the host
tests `now_us_seqlock_retry_under_simulated_writer` and `seqlock_threaded_writer_never_tears` exercise a torn read, and both bypass `LAST_NS` (ROADMAP §10.2, F100).

### Global monotonicity under SMP

Once every CPU has its own LAPIC timer, "the tick count" stops having a single writer. Options:

1. Only the BSP's timer updates the global counters; AP timers drive local scheduling only. Simple,
   and what the old tree did, but it makes `now_us` dependent on the BSP staying alive and awake.
2. Per-CPU time, with a global monotonic clock derived from the TSC alone once it is known invariant
   and synchronized.
3. A real distributed clock with cross-CPU synchronization and drift correction.

The kernel runs option 1. Only CPU 0's timer interrupt advances the tick (`apic_init::on_timer_irq`
calls `time_init::on_hw_tick` on CPU 0 alone), `now_ns` interpolates from it, and `LAST_NS` clamps it
monotonic. Timer interrupts that arrive while one is already pending coalesce into one, so a CPU 0
IF-off window longer than 1 ms loses ticks, and `now_ns` stays behind the TSC from then on. In
TSC-deadline mode each rearm starts from a TSC read inside the ISR, so a period is 1 ms plus interrupt
latency but counts as 1 ms (ROADMAP §10.3, F027).

Option 2 is the destination: with an invariant, synchronized TSC and a single calibration constant,
`now_ns` is a `rdtsc` and a multiply with no shared state at all. Planned: ROADMAP §10.3 (F027)
derives `now_ns` from the TSC alone when the TSC is invariant, and §10.7's warp test decides when it
is synchronized enough to order a trace. Get there before relying on
timestamps for tracing, because option 1 will silently produce non-monotonic values the moment the BSP
goes idle in a deep C-state.

## 6.5 Timers and timeouts

The `sleep_ms` path needs a data structure, not a linear scan of every thread on every tick:

- Start with a single sorted list of pending timeouts guarded by one lock. Fine for tens of threads.
- Move to a hierarchical timing wheel when the count grows. Planned: ROADMAP §19.4 keeps timeouts
  per CPU in a timing wheel.
- Every blocking operation takes an optional deadline. A blocked thread with no timeout and no waker
  is a permanent leak, and the only way to find one is to have made timeouts mandatory from the start.
- The blocked-thread sweep in `schedule_inner` (every `SWEEP_TICKS` ticks, for timeouts at least
  `OVERDUE_NS`, 5 s, late) never reports: `pop_expired_into` has already drained every expired
  timeout when `overdue(now)` runs (ROADMAP §10.7, F111).

## 6.6 Tickless and wall clock

A fixed 1 kHz tick on an idle CPU is wasted interrupts and, on real hardware, wasted power. TSC-
deadline mode makes tickless operation possible: when a CPU goes idle, arm the deadline for the next
pending timer instead of the next millisecond, and skip the timer entirely if there is nothing pending.
Not day-one work, but the timer abstraction should be "next deadline" rather than "periodic tick" so
this does not require rewriting the scheduler. Planned: ROADMAP §19.6 lands tickless idle.

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
| FADT | `FACP` | `iapc_boot_arch` at offset 109; bit 0 is `LEGACY_DEVICES` (not 8259 presence, §5.5) |
| MCFG | `MCFG` | PCIe ECAM base. ECAM reaches extended config space (offsets `0x100` and up); below that offset, `0xCF8`/`0xCFC` reaches every bus ([section 9.2](#92-memory)) |

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
| `0xC000_0080` | `IA32_EFER`. Bit 0 SCE, bit 8 LME, bit 11 NXE. |
| `0xC000_0081` | `IA32_STAR`. `SYSCALL` loads CS `0x08`; `SYSRET` base `0x10` gives user SS `0x1B` and CS `0x23`. |
| `0xC000_0082` | `IA32_LSTAR`. `vibeos_syscall_entry`. |
| `0xC000_0084` | `IA32_FMASK`. `0x47700`: `SYSCALL` clears TF, IF, DF, IOPL, NT, and AC. |
| `0xC000_0100` | `IA32_FS_BASE`. User TLS base, written by `enter_user`, `enter_user_full`, and `execve`; not switched per thread ([section 7.5](#75-per-cpu-data)). |
| `0xC000_0101` | `IA32_GS_BASE`. The `PerCpu` address in ring 0; the user GS base (0) in ring 3. |
| `0xC000_0102` | `IA32_KERNEL_GS_BASE`. The inactive GS base: the `PerCpu` address while the CPU runs ring 3, the user GS base after an entry `swapgs` ([section 7.5](#75-per-cpu-data)). |

## 7.3 AP trampoline

An AP comes out of SIPI in real mode at `CS:IP = vector<<8 : 0`, so the entry point must be a 4 KiB
aligned physical page below 1 MiB. We use `0x8000`, SIPI vector `0x08`.

The trampoline is `src/arch/trampoline.S`, assembled with `global_asm!` into `.trampoline`
(inside `__rodata_start..__rodata_end` so the kernel map covers the copy source) and copied
to `0x8000`. It goes: real mode, set up a GDT, enable protected mode, load CR3 from the param block,
set `EFER.LME` and `EFER.NXE`, enable paging, long jump to 64-bit, load the stack, call the Rust
entry point. Addresses in the blob are physical (`0x8000`), not the kernel VMA. On the way it clears
`CR0.CD` and `CR0.NW` (then `wbinvd`), sets `CR4.PAE` and `CR4.PGE`, and sets `CR0.PG` and `CR0.WP`.

The blob loads CR3 with a 32-bit `mov`, so the kernel PML4 must lie below 4 GiB.
`smp_init::start_one` checks it and, when the PML4 is higher, skips the AP with
`vibeOS: smp: cr3 above 4GiB`; nothing allocates the PML4 below 4 GiB (ROADMAP §20.1).

`EFER.NXE` matters. Kernel pages are mapped NX, and if the AP enters long mode without NXE the NX bits
are reserved-bit violations and the first kernel page it touches faults.

The BSP patches parameters into the tail of the blob:

| Offset | Field |
|--------|-------|
| `0xD0` | CR3 for the AP |
| `0xD8` | Stack top |
| `0xE0` | 64-bit Rust entry point |
| `0xE8` | IDT pointer, 10 bytes. `patch_params` writes it and the blob never reads it; `ap_entry` loads the IDT after `GS_BASE` (ROADMAP §10.7 deletes it, F111). |

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
6. On timeout: send INIT to that APIC ID, clear its online bit, log the failure, and continue with the
   remaining CPUs. Leak the AP's stack, GDT/TSS and IST stacks, `PerCpu` slot, and idle TCB: an AP that accepted a
   SIPI and then stalled past 3 s can keep running on them, or read the next AP's parameter block, and
   the BSP cannot tell it from an AP that never started. `smp_init::start_one` breaks this rule: it
   frees the stacks and GDT/TSS and marks the idle TCB Dead, with no INIT, and does not clear the
   online bit (ROADMAP §20.1, F032).

On the AP side (`smp_init::ap_entry`), in order: `cli`; load the per-CPU GDT and TSS; set `GS_BASE`
and `KERNEL_GS_BASE` (`per_cpu_init::install_gs`); program the syscall MSRs and the FPU bits and set
RSP0 (`syscall_init::init_ap`); load the shared IDT; `arch::cpu::harden` (SMEP, SMAP, and UMIP where
CPUID allows); enable the LAPIC; copy the BSP's `tsc_per_ms` and timer mode into `PerCpu`; arm the
LAPIC timer with the BSP's calibration (`apic_init::arm_ap`); mark the CPU online and print
`vibeOS: sched: cpu<i> ready`; publish the ready flag; `sti`; enter the idle loop.

`GS_BASE` must be set before any `lidt` and before `sti`. NMI and timer IRQs both
read per-CPU state through `gs:[0]`. Setting it after `lidt` is a null dereference
waiting for a non-maskable interrupt, even with IF off.

## 7.5 Per-CPU data

One `PerCpu` struct per CPU. In ring 0, `GS_BASE` holds its address. While the CPU runs ring 3,
`KERNEL_GS_BASE` holds it and `GS_BASE` holds the user GS base (always 0). `enter_user` and
`enter_user_full` set that state, each `swapgs` exchanges the two, and `per_cpu_init::init_bsp`,
`per_cpu_init::install_gs`, and `arch::gs::force_kernel` set both MSRs to the `PerCpu` address.

`self_ptr` sits at offset 0 so `gs:[0]` yields the struct address, which is how a `&PerCpu` is obtained
without knowing which CPU you are on.

Contents (`src/per_cpu.rs`):

- `self_ptr`, logical CPU id, APIC id
- `current`, `idle`, and `idle_id`
- `runq`, this CPU's ready FIFO (owner only, IRQs off), and `ready_head`, a copy of its head that
  only an in-guest test reads (ROADMAP §10.7 deletes it, F111)
- `wake_inbox`, a `u64` `ThreadId` bitset: a remote CPU ORs in a thread's bit and sends IPI `0xFD`
- `irq_nest`, tick and switch counts, `slice_tsc`, `idle_tsc`, and `switch_scratch`, a `CpuContext` that no code reads or writes
- `tsc_per_ms` (a copy of the BSP's value, [section 6.2](#62-calibrating-the-tsc)) and `timer_mode`
- `ready`, the flag an AP sets last in bring-up ([section 7.4](#74-ap-bring-up-sequence))
- `kernel_rsp0` and `as_cr3`, which the context switch updates; `tss`, through which it writes TSS.RSP0; and `fallback_rsp0`, the RSP0 it uses for a thread without `Tcb.stack` (below)
- `syscall_scratch`: the user RSP, the syscall return value, and the `iretq` RIP, RFLAGS, and RSP.
  It is per CPU, not per thread, so it is valid only while IF=0. The syscall exit breaks this: it runs without a
  `cli`, and a console `read` that waited in `sti; hlt` returns to it with IF=1 (ROADMAP §10.6, F001).

`per_cpu_init::init_bsp` allocates one `PerCpu` per MADT CPU in a heap array, not a static array
sized by a `MAX_CPUS` guess, and installs the BSP at slot 0; each AP installs its own slot with
`per_cpu_init::install_gs`. `current` and `idle` are `*mut Tcb`. The other per-CPU tables are static
and cap the CPU count at 64: the MADT `apic_ids` array (`acpi::MAX_CPUS`), `irq_init::IN_ISR`,
`ipi_init::SHOOT`, `per_cpu_init::WITH_BUSY`, `sync_init::HELD`, `log_init::EMITTING` and `log_init::STAGE`, and the `u64` online mask. The 64-slot thread table, of which boot takes 2N+3 at
`-smp N`, limits it further (ROADMAP §10.4, F037).

`per_cpu_init::current()` is valid only after the entry path has put the kernel base in `GS_BASE`. The
`swapgs` instructions are the three in `vibeos_syscall_entry` (entry, `sysretq` exit, `iretq` exit)
and the one in `arch::gs::do_swapgs`, which the handlers defined in `arch/idt.rs` call through
`gs_enter` and `gs_leave` when the saved CS.RPL is 3. The device pool stubs (`0x31`–`0x7F`) and the
two keyboard ISRs (`0x30`, `0x21`), installed from outside `arch/idt.rs`, never call it; one taken at
CPL 3 faults on `gs:[0]` and halts the kernel ([section 5.10](#510-privilege-transitions) rule 1;
ROADMAP §10.6, F004).

The CS.RPL rule is also wrong wherever CS is the kernel's while `GS_BASE` holds the user base: NMI,
`#MC`, and `#DB` in the one-instruction windows between `syscall` and the entry `swapgs` or between
the exit `swapgs` and `sysretq`/`iretq`; a `#GP` raised by the user-return `iretq` (F007); and an
interrupt taken inside `enter_user_full`, which runs with IF=1 from its `mov gs` to its `iretq`
(F006). ROADMAP §10.6 closes these for the current kernel (the IST vectors decide from the sign of
`GS_BASE`, a `#GP`, `#NP`, or `#SS` on a labeled user-return `iretq` becomes `SIGSEGV`, and
`enter_user_full` runs `cli` before its `mov gs`) and §18.3 for FSGSBASE, where a user can load a kernel-half base. See
[section 9.3](#93-interrupts).

`with_current` gives `&mut PerCpu` with IRQs off and panics on same-CPU re-entry. `switch_now` uses
`with_current_switch`: the `InterruptGuard` spans `switch_context` (it lives on the outgoing stack)
but the re-entry flag does not, so the incoming thread can take IRQs and `with_current`. Neither
`&mut` is exclusive. `with_current_switch` keeps its `&mut` live across `switch_context`, `with_cpu`
(a safe `pub fn`) returns `&mut` to any CPU's slot, and `ap_entry` builds one from `STARTING`, while
`cpu(id)`, `current()`, and `try_current()` return `&'static PerCpu` to the same memory.
`diag::cpus_to` (also the shell `cpus` command) and in-guest tests read another CPU's non-atomic
`ticks`, `switches`, and `runq` length while its owner writes them (ROADMAP §10.3, F039).

### Per-thread CPU state

`thread_init::switch_now` switches a thread's CPU state inside `with_current_switch`: it swaps
`irq_nest` between the TCB and `PerCpu`, calls `syscall_init::on_switch` (FPU, RSP0, CR3), then
`thread::switch_context` (callee-saved GPRs, RSP, RIP, RFLAGS). AGENTS.md rule 8 governs adding
user-visible CPU state; the commit that adds it also adds its row here.

| State | Saved in | Switched by | Status |
|---|---|---|---|
| `rbx`, `rbp`, `r12`–`r15`, RSP, RIP | `Tcb.context` (`CpuContext`) | `switch_context` | switched |
| RFLAGS | `CpuContext.rflags`; IF comes from `irq_nest` (`apply_if_on_resume`) | `switch_context` | switched |
| `irq_nest` | `Tcb.irq_nest`, swapped with `PerCpu.irq_nest` | `switch_now` | switched |
| user GPRs, RIP, RSP, RFLAGS | the thread's kernel stack: the 128-byte syscall frame or the interrupt frame | the RSP0 switch, which gives each thread its own entry stack | switched |
| x87, SSE, MXCSR | `Tcb.fpu`, a 512-byte FXSAVE image | `switch_fpu` in `on_switch` (`fxsave64` the old thread, `fxrstor64` the new) on every switch; the syscall entry and exit also save and restore it | switched. `fork` gives the child `fpu_template()`, not the parent's image, and `execve` keeps the old image's registers (ROADMAP §10.6, F069). The template is captured after `fninit`, which resets only the x87 control, status, and tag words, so MXCSR and the XMM and ST registers hold whatever the loader left (ROADMAP §13.10, F129). FXSAVE covers no XSAVE state; `CR4.OSXSAVE`, `CR4.PKE`, and `EFER.FFXSR` are assumed clear and never asserted (ROADMAP §11.1, F130). |
| RSP0 | TSS.RSP0 and `PerCpu.kernel_rsp0`: the top of `Tcb.stack`, or `fallback_rsp0` for the bootstrap thread | `set_rsp0_for` in `on_switch` | switched |
| CR3 | `Tcb.as_cr3` (0 means the kernel PML4) | `switch_cr3_for` in `on_switch`, skipped when unchanged | switched; no PCID |
| FS_BASE (user TLS) | not saved | nothing | not switched. `enter_user`, `enter_user_full`, and `execve` write it; `force_kernel`'s `mov fs` zeroes it on every exit or kill; `fork` copies the live MSR, so a child can inherit another process's base (ROADMAP §13.1, F022). |
| user GS base | not saved; always 0 | nothing | holds while no `ARCH_SET_GS` or FSGSBASE exists (ROADMAP §18.3) |
| `PerCpu.syscall_scratch` | per CPU | not switched | valid only while IF=0 (above; F001) |

## 7.6 IPIs

| Vector | Purpose |
|--------|---------|
| `0xFB` | Call function. Run a closure on a target CPU, optionally waiting for completion. |
| `0xFC` | TLB shootdown. |
| `0xFD` | Reschedule. Target CPU re-evaluates its run queue, waking from `hlt` if idle. |
| `0xFE` | Panic halt, Fixed delivery. `ipi_init::halt_others` broadcasts it and does not wait, and a CPU spinning with IF=0 (in `SpinMutex::lock` or `wait_acks`) never takes it, so another CPU can write to COM1 during the dump (ROADMAP §10.7, F135). |

The reschedule IPI is what makes cross-CPU wakeups work without ever locking a remote run queue: push
onto the target's inbox, send `0xFD`, done. `0xFD` then takes SCHED IRQ-off via `schedule_preempt`.
Shootdown and call-function work take no lock at all, since a CPU in a serviced spin runs them inside
whatever it holds ([§2.2](#22-interrupt-handler-rules)). Call-function uses one global slot; the
initiator holds IF off from publish through reclaim, polling inbound work while it waits.

## 7.7 Locking with more than one CPU

The global lock order is in [section 2.1](#21-lock-order) and the one-spinlock rule in
[section 2.3](#23-locking-with-interrupts). Additions specific to SMP:

- Never lock a remote CPU's per-CPU state. Per-CPU locks are taken only by the owning CPU, with
  interrupts off. `per_cpu_init::with_cpu` can reach any CPU's slot ([section 7.5](#75-per-cpu-data),
  F039).
- If two CPU-local structures must be locked at once, for instance during load balancing, lock the
  lower `cpu_id` first.
- A lock taken from an ISR is taken with interrupts disabled in every other context too. The scheduler
  lock is the canonical case: the timer ISR calls into the scheduler, so any holder with interrupts
  enabled deadlocks the moment its own timer fires.
- Serial TX takes a lock so bytes from different CPUs do not interleave. `Serial::write_fmt` keeps
  IRQs off for the whole line but takes the TX lock once per `write_str` piece, so another CPU can
  write between two pieces of a formatted line, and `log_fmt` sends a record and its newline as two
  writes. The harness then misses a contract line split that way. Planned (ROADMAP §10.2, F138):
  each line is formatted, newline included, into one buffer and written under one TX hold.
- klog records go to one global IRQ-safe log ring and to a serial sink that only try-locks TX.
  Per-CPU serial capture assembles serial output into lines for the ring. Per-CPU buffers with a
  printer thread are planned (ROADMAP §19.5).
- The global SCHED lock, one `SpinMutex` on the block cache, one VFS lock over lookups and
  namespace changes (§2.1), the log-ring TAS, and virtio-blk bounce copies are known scale limits;
  see ROADMAP §19.4, §19.5, and §19.8. So is the one bottom-half thread for every threaded vector,
  until ROADMAP §12.5 gives each vector its own ([§5.4](#54-irq-registration)).

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

The wait is bounded: `ipi_init::wait_acks` panics after 1000 × `tsc_per_ms` TSC cycles (1 s) without
every acknowledgement. A target CPU that holds IF=0 that long without polling `service_incoming`
makes a shootdown panic the kernel. The in-guest test runner holds IF=0 for the whole run,
and a syscall body runs with the IF=0 that FMASK set until it blocks (ROADMAP §10.10, F011); a console
`write` with many newlines is the long case (ROADMAP §10.6, F044). One round invalidates one VA
(`shootdown_va`). `kva_init::unmap_shootdown` unmaps at most 32 pages (`MAX_UNMAP`) and leaves the
rest mapped with no error, while `vunmap` frees the whole VA span it was given (ROADMAP §10.3, F107).

The shootdown handler allocates nothing and takes no lock ([§2.2](#22-interrupt-handler-rules)). It
reads a request slot and executes `invlpg`.

## 7.10 Verification and later work

SMP bugs are timing dependent, so the tests matter more than usual:

- `-smp 2` is the default for every QEMU invocation including the normal boot test. One AP catches most
  bring-up bugs.
- `-smp 4` as a separate target to shake out sequencing assumptions that hold for exactly one AP.
- `-cpu qemu64,-tsc-deadline` to force the LAPIC periodic path under KVM. Under TCG every tier but `make test-e2e-pit` (PIT) takes
  the periodic path already, and no CI tier runs the TSC-deadline path ([section 6.3](#63-the-tick),
  F078).
- In-guest tests that need real CPUs: per-CPU identity on BSP and AP, cross-CPU thread spawn, reschedule
  IPI delivery, waking an idle AP, remote unmap and remap through the shootdown path.
- `qemu` monitor `info cpus` and `info lapic` for interactive debugging, plus `-d int,cpu_reset` when a
  core is triple faulting and you need to see why.

Later:

- x2APIC (ROADMAP §20.1). MSR-based register access, no MMIO, and APIC IDs beyond 255.
- Topology awareness (ROADMAP §19.4): cores, threads, packages, and cache sharing from CPUID leaf
  0x1F, so the scheduler can prefer a sibling core over a remote package.
- NUMA (ROADMAP §19.7). SRAT and SLIT parsing, per-node buddy allocators, node-local allocation
  policy.
- CPU offlining for power management (ROADMAP §19.6), which needs the reverse of bring-up: migrate
  threads, redirect interrupts, park the core.

---

# 8. Testing

Three tiers. Each catches a class of bug the others cannot, and each is progressively slower, so the
decision of where a test goes matters.

| Tier | Runs | Speed | Catches |
|------|------|-------|---------|
| Host unit | `make test-unit` (`vibeos-core`, any host triple) | milliseconds | Algorithms: allocators, parsers, state machines, encodings, arithmetic |
| In-guest (ktest) | QEMU, kernel built with the `kernel_tests` feature | seconds | Anything needing real hardware state: page tables, MMIO, interrupts, threads, SMP |
| End to end | QEMU boot of the normal ISO, serial captured | ~10 s | Boot regressions, marker ordering, panics, subsystem interaction |

The routing rule: if it can be a host test, it must be. Pushing logic into the library half of the
crate so it becomes host-testable is the highest-leverage thing available, and the old tree's biggest
weakness was that nearly everything lived behind `main.rs` and was therefore untestable.

## 8.1 Host unit tests

Anything in `src/lib.rs` and its submodules, compiled as `vibeos-core` on the host. No hardware
access, no `unsafe` port I/O, no MMIO. The kernel half calls into it. x86-only pieces
(`switch_context`) are `cfg(target_arch = "x86_64")`; they still run on Linux CI.

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
  buffer; `/dev/null` `/dev/zero` `/dev/random` (virtio-rng, then RDRAND, then xorshift);
  procfs stubs do not panic.

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
vibeOS: ktest: FAIL <name>: <reason>
vibeOS: ktest: skip <name>: <reason>
vibeOS: ktest: end
```

The harness requires `begin` and `end`, rejects any `FAIL` line and any panic signature, and checks
the exit status.
`isa-debug-exit` at I/O port `0xf4` maps a written value to host exit status `(value << 1) | 1`:

| Write | Host exit | Meaning |
|-------|-----------|---------|
| `0x10` | 33 | all tests passed |
| `0x11` | 35 | at least one test failed |

The harness also retries, and ROADMAP §10.2 removes every retry (F021). `run_ktest.py` boots again
after any timeout, after the `-smp 2` TCG `FAIL per_cpu_bsp: ready_head should be empty`, after the
`-smp 4` `FAIL msix_cpu: ap counter`, and after a `-smp 4` panic whose tail holds `ipi: ack timeout`,
the `ipi_init::wait_acks` frame, or a banner glued to a `ktest: ok` line. A second timeout whose tail
ends at `user: dup ok` gets a third boot. `make test-smp-stress` uses the same rules, so a green run
can hide an intermittent hang or panic.

Skips are first class and carry their reason on the `ktest: skip <name>: <reason>` line. Every skip
names what the configuration lacks: `no AP`, `no virtio-blk`, `no virtio-rng`, `no e1000e`, `no edu`,
`no smep/smap/umip`, `pit owns tick`, `pic fallback`, and `rtc unread` (the `Outcome::Skip` reasons
in `src/ktest.rs`). Destructive exception tests run inside `arch::catch` scopes, which longjmp out or
step RIP past the faulting instruction, instead of skipping.

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
removed silently. The executable contract is `boot_contract_markers()` in `tests/harness/harness.py`; the list
below mirrors it, and the `_start` table in [section 3.3](#33-_start-order) says why each step sits where it does.

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

In the production ISO, `shell ready` is written from ring 3 by `/bin/sh`, which `/sbin/init` starts
after waiting for `/bin/tests`. `init` passes no status pointer to `wait4`, and the harness matches
neither `user: tests ok` nor `user: tests fail`, so a failing `/bin/tests` passes every e2e variant
(ROADMAP §10.5, F073). `run_e2e.py` boots once more when either boot times out or the console-input boot
ends without `shell ready` (`_retry_hang`; ROADMAP §10.2, F021).

`smp: done` before `shell ready` is deliberate. Put SMP bring-up after the shell starts and an AP
failure becomes invisible, because the harness sees its last marker and passes. `pci: <n> devices`
sits between `console ok` and `shell ready` so `lspci` is registered before the prompt. The ramdisk
`block: <name> <n> sectors` line sits after PCI and still before the shell. Partition children emit
`block: <parent>p<N> <n> sectors` after the parent (e2e: `ram0p1`, `ram0p2`). virtio-blk adds
`block: vda <n> sectors` and `vdapN` when the ktest disk is present (not on the production e2e `pc` set).

With `-smp N`, additionally:

- for each AP `i` in `1..N`, `vibeOS: sched: cpu<i> ready` then `vibeOS: smp: ap online`, in order,
  before `smp: done`. The harness requires at least these `N-1` pairs and does not reject an extra
  `ap online` line; ROADMAP §10.2 makes it count exactly `N-1` (F141)
- `vibeOS: sched: cpu<i> ready` for every `i` in `0..N`
- `vibeOS: time: lapic_timer ok (<mode>)` naming the selected timer path
  (`tsc-deadline`, `periodic`, or `pit`) rather than inferring it

### Failing fast

Scan for these (`PANIC_SIGNATURES` in `tests/harness/harness.py`) and fail immediately with the
captured line rather than waiting out the timeout:

```
panicked at   vibeOS: panic:   #PF   #GP   #UD   #DF   double fault   stack overflow
```

Match the exception mnemonics, not the phrase "page fault". Shell help text and log messages contain
English words, and a substring match on prose produces false failures that erode trust in the suite.

Expected-panic e2e waits for `vibeOS: panic: halted` so the dump (regs, thread, last log records,
backtrace) is in the captured log, then checks dump needles. `panic_exit` writes isa-debug-exit
`0x11` so QEMU leaves instead of sitting in `hlt`. The harness kills QEMU at `panic: halted` instead of
waiting for that exit, so it never checks status 35. It also matches boot markers on every line,
including the dump's `vibeOS: logrec:` replay of earlier records, so a marker printed out of order
before the panic can match again in the dump (ROADMAP §10.2, F141).

On success, exit through the QEMU monitor's `quit` rather than waiting for the timeout. Two seconds
versus forty five, on every CI run and every local invocation.

### Harness

Python, standard library only. `subprocess` with its own timeout rather than shelling out to GNU
`timeout`, which does not exist on macOS. The harness helpers get their own unit tests, because a bug
in the test harness produces either false confidence or a debugging session in the wrong repository.
Those tests exercise `check_markers_in_order`, which no runner calls; `run_qemu_and_check`, the
matcher every e2e run uses, has no unit test (ROADMAP §10.2, F141).

`make test-vibefs-crash` (`run_vibefs_crash.py`) formats a 256 KiB image with `mkfs-vibefs`, boots
the `vibefs_crash` build on it, kills QEMU up to 0.18 s after the first `vibeOS: vibefs: wr` line,
and passes a round when `fsck-vibefs` exits 0 and prints `errors 0`. It does not mount the image or
check `/crash/w` for a committed prefix, which [VIBEFS.md](VIBEFS.md) §12 requires, and the guest
discards `sync_fs` errors, so a round passes after the volume has filled and commits have stopped
(ROADMAP §10.2, F080).

## 8.4 QEMU flags

| Context | Flags |
|---------|-------|
| `make run` | `-cdrom vibeos.iso -m 128M -smp 2 -cpu max -accel tcg -no-reboot -serial stdio` (Makefile `QEMU_BASE`, plus `-serial stdio` from the `run` recipe) |
| e2e | as above plus `-display none -monitor unix:...,server=on,wait=off` (`harness.qemu_argv`) |
| ktest | as e2e plus `-device isa-debug-exit,iobase=0xf4,iosize=0x04`, `-device e1000e`, `-device edu`, `-device virtio-rng-pci,disable-legacy=on`, virtio-blk (`-drive file=…,if=none,id=vibehd,format=raw,cache=writeback,discard=unmap` + `-device virtio-blk-pci,drive=vibehd,disable-legacy=on,num-queues=<smp>`). Extra NICs/edu/virtio are ktest-only; e2e stays the default `pc` set (`pci: 6 devices`). After a green first boot the harness reboots the same disk and requires `vibeOS: persist: intact`. |
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

`VIBEOS_BIOS` reaches QEMU as `-bios`, which accepts only an image whose size is a multiple of
64 KiB. apt's combined `/usr/share/ovmf/OVMF.fd`, the Makefile's `OVMF` default and the one CI uses,
boots. Homebrew's code-only `edk2-x86_64-code.fd` is refused and needs `-drive if=pflash` instead.
`make test-e2e-uefi` prints a skip message when `OVMF` does not exist and then runs the harness
anyway, because the check and the run are separate recipe lines (ROADMAP §10.2, F079).

## 8.5 Make targets

`make help` prints the live inventory. Do not hand-maintain a second list here.

`make check` is the fast local gate (rustfmt `--check`, `vibeos-core` clippy `-D warnings`, host unit tests,
harness unit tests, ruff/mypy when installed). CI runs it as the `check` job before QEMU (DESIGN §8.6).
`make test-e2e` is enough when only boot output or QEMU wiring changed. `make test` is the gate before
a PR. `make test-ps2` is the focused #66 sendkey boot; `make test-e2e` already runs it, so `make test`
does not boot it twice.

## 8.6 CI and coverage

Two jobs run on every push and pull request, on Linux; the other rows below are scheduled or run on
a tag. `concurrency` cancels superseded runs for the same
branch (push and PR share one slot). The earlier one-ladder-job rule (runner queues) was lifted on
2026-09-22: the repo is public, so Actions minutes are free, and agents own the CI design. ROADMAP
§10.1 plans a build-once job plus a tier matrix per architecture; until that lands the ladder is one
job.

| Job | When | What |
|---|---|---|
| `check` | push / PR | `make check` (fmt, `vibeos-core` clippy `-D warnings`, host units, harness, ruff/mypy, `scripts/check_*.py`) then `cargo llvm-cov -p vibeos-core --lib --features std --target $HOST --fail-under-lines 87`. No QEMU, no `setup.sh`. HTML report is a 7-day `hostlib-coverage` artifact. |
| `phase 0 ladder` | push / PR, `needs: check` | Limine, QEMU/nasm/xorriso/OVMF, kernel clippy `-D warnings` with `--all-features`, `kernel_tests`, and `vibefs_crash` (never the default feature set that ships), ISO, e2e (BIOS/UEFI/panic/#GP/PIT/9 GiB), in-guest at `-smp 2` and `-smp 4`, LAPIC fallback, vibefs crash. Green `main` uploads `vibeos.iso` (7 days). |
| `smp-stress` | weekly Monday 06:00 UTC + dispatch | `-smp 4`, longer timeout |
| `nightly-canary` | same workflow, non-blocking | undated latest nightly, `make iso && make test-unit` |
| `release` | `v*` tags | `make test-e2e` (BIOS) only, then production + ktest ISO, changelog section, GitHub Release. It does not wait for `ci` at the tagged commit, and the ktest ISO writes fixed LBAs of any virtio-blk disk attached at boot (ROADMAP §10.1, F145). |

`-D warnings` reaches host builds through `[build] rustflags` and the kernel clippy steps through their own `-- -D warnings`. Kernel builds (`make iso` and
every ISO variant) run without it, because `[target.x86_64-unknown-none] rustflags` in
`.cargo/config.toml` replaces `[build] rustflags` (ROADMAP §10.1, F147).

GitHub Actions records per-step duration. Measured on `main` at `88370e5` (run 35796216463): `check`
53 s, then the ladder 160 s, serialized by `needs: check`. The ladder spends 58 s on setup, toolchain,
kernel clippy, and ISO build before the first QEMU step, then 98 s across nine QEMU steps (longest:
vibefs crash, 22 s); about **3m40s** end to end. Across ten green runs up to `90ce475` the ladder took
156-307 s (median about 206 s), because the harness retried a hung boot (ROADMAP §10.2). A fmt or
hostlib lint failure should go red in about a minute without starting QEMU.

Hostlib line-coverage floor is **87%** (`--fail-under-lines 87` in `.github/workflows/ci.yml`).
Measured 87.60% on `nightly-2026-09-22` (`cargo llvm-cov --lib` in `tests/hostlib`; A2 runs the
same portable sources as `vibeos-core`). Ratchet the integer only upward. Coverage is still not a percentage target for the kernel: every bug that gets
fixed gets a test that would have caught it, in the cheapest tier that can catch it. Every entry in
[section 9](#9-pitfalls) names the rule that guards it, and where that rule is only an invariant in
code with no test, that is a weaker guarantee and should be visible as such.

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
the same size empty and filled. Planned (ROADMAP §10.2, F084): the build regenerates the table from
the final ELF and fails on any difference.

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
23:16); its limit is the 256-byte config space. Rule: ECAM where MCFG covers the bus, at
`base + (bus<<20)|(dev<<15)|(fn<<12)|off`, where `base` is the MCFG entry's base address and
corresponds to bus 0 even when the entry's start bus is not 0; mechanism #1 for offsets below `0x100`
elsewhere.
`pci::ecam_phys` offsets from the start bus instead, and its unit test pins that (ROADMAP §20.1,
F045). Type-1 headers reuse BAR slots as bus-number registers; size-probe only the BAR count for that
header type.

**Two subsystems designed for the same virtual address range.**
The heap and the kernel VA allocator were both specified at `0xFFFF_C000_*` in different documents, and
only one of them noticed. Rule: the address map in [section 4.1](#41-virtual-address-map) is the single
source of truth, and every region asserts its range is unmapped before claiming it.

**Allocator corruption with a crash in an unrelated subsystem.**
Buddy free list nodes live inside free pages, and a kernel stack overflow wrote into one. Rule: every
kernel stack gets an unmapped guard page below it, and stack overflow is a page fault rather than
silent corruption. The bootstrap thread breaks the rule (`stack: None`): `_start`, all of boot, and
the `kernel_tests` registry run on Limine's stack (at least 64 KiB, no guard page, in
bootloader-reclaimable memory). The kernel sends no stack size request (ROADMAP §10.6, F072).

**A PTE edit appears to have no effect.**
No `invlpg` after the edit. Rule: `invlpg` after any single-PTE modification, including MMIO attribute
patches. Kernel mappings are `GLOBAL`, and on a CPU with `CR4.PGE` set they do not fall out of the
TLB on a CR3 reload. The trampoline sets PGE on the APs only; the BSP runs with Limine's CR4, PGE
clear (ROADMAP §10.6, F026).

**`meminfo` is slow.**
`free_page_count()` walked the free lists. Rule: maintain a running counter.

**Freeing a stack while running on it.**
An AP's stack was unmapped while it was still executing on it. Rule: a dead thread's stack is freed
only after the CPU that ran it has switched off it, and the reclaimer observes that; a reaper thread
that is not on the stack is not enough on SMP ([section 2.8](#28-publish-last) rule 2). `thread_exit` breaks this: it puts its own stack on the
global `kva_init::DEFERRED` list and keeps running `schedule()` on it, and any CPU's `reap_zombies`
can unmap it before the exiting CPU reaches `switch_context` (ROADMAP §10.10, F012). The list has 8
slots, and `defer_free` panics when an exit burst fills it (ROADMAP §10.10, F010).

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

**Allocate or block in a hard-IRQ / MSI handler.**
The top half ran `Box` / `sleep` / `WaitQueue` wait. Rule: ack, set pending, wake the IRQ thread or
enqueue work. The thread may alloc and block ([section 2.2](#22-interrupt-handler-rules)).

**An NMI, `#MC`, or `#DB` next to a syscall reads a user value as its `PerCpu`.**
The handler decided `swapgs` from CS.RPL. Between `syscall` and the entry `swapgs`, and between the
exit `swapgs` and `sysretq`/`iretq`, CS is the kernel's but `GS_BASE` holds the user base, so the
handler skips the swap and `gs:[0]` is whatever userspace set. Rule: ordinary vectors may trust
CS.RPL, except a `#GP`, `#NP`, or `#SS` raised by a user-return `iretq`, which arrives with the kernel
CS and the user GS base (ROADMAP §10.6, F007); the IST vectors decide from the sign of `GS_BASE`
(ROADMAP §10.6), and once FSGSBASE lets a user load a kernel-half base they save `GS_BASE` and load
the per-CPU base unconditionally (ROADMAP §18.3). Not yet enforced: the `arch/idt.rs` handlers, IST vectors included,
decide from CS.RPL, and the device pool stubs and keyboard ISRs make no GS decision at all
([section 7.5](#75-per-cpu-data), F004).

**A user program halts every CPU.**
Ring-3 activity reaches `exception_halt` on four paths. `debug_ex` has no ring-3 branch and
`sig_for_vec` maps neither `#DB` nor `#AC`, so a user `popf` that sets `RFLAGS.TF`, or an `int1`
(`0xF1`), halts the kernel (F005). A device or keyboard interrupt taken at CPL 3 enters through a
gate that skips `swapgs` and faults on `gs:[0]` (F004). `enter_user_full` runs with IF=1, so an
interrupt between its `mov gs` and its `iretq` reads `gs:[0]` at VA 0 (F006). A `syscall` in the last two bytes
of the top user page leaves RIP at the non-canonical `0x0000_8000_0000_0000`, and the `#GP` on the user-return `iretq` runs on the user GS
base; TCG skips that canonical check, and KVM and hardware do not (F007). ROADMAP §10.6 closes all
four. Rule: an exception raised by ring-3 code, or by a return to ring 3, ends in a signal to that
process; `exception_halt` is for faults in kernel code. Every vector has a ring-3 row in the
[section 5.2](#52-idt-and-exceptions) table, and a new ring-3 entry or exit path gets an in-guest
test that runs it with IF=1.

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
`IoWaiter` lives on the submitter's stack. `IoWaiter::finish` stores `done` with Release, then takes
SCHED and runs `wake_all` on the `WaitQueue` inside the waiter; `wait()` returns as soon as its
lock-free `poll()` sees `done`, so the wake can land in whatever frame reused that stack (ROADMAP
§10.10, F002). Rule: publish last ([section 2.8](#28-publish-last)). For `IoWaiter`: take SCHED, wake, then store
`done` inside that section ([section 10.1](#101-completions)).

**Condvar waiter never sees the predicate.**
Wake does not carry the condition. Mesa: `wait` re-acquires the mutex and returns; the caller loops
on the predicate. Timeout is the same path.

**Condvar wait parks still holding the mutex.**
`begin_wait` marked Blocked, SCHED dropped, then `drop(guard)` released the mutex. A timer in that
window switched the waiter off-CPU still owning it; the notifier blocked on the mutex forever. Rule:
enqueue on the CV and unlock the mutex under the same SCHED, keep IF off from that section through the
delivery of the wakes it recorded, then schedule. `with_sched` breaks the second half: it runs
`place_ready` for the recorded wakes after dropping SCHED, with IF back on, so a preemption there
switches the waiter out before the woken mutex waiter is on any queue (ROADMAP §13.12, F034).

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
`now_ns` below the last reading. Do not cap extra at one tick — ktest holds IF off and timeouts must
still advance on TSC alone.

**`sleep_ms(50)` and PIT-vs-HPET calib flake on TCG SMP.**
TCG has no invariant TSC. Boot HPET calibration runs before APs; a later PIT channel 2 window sees a
different apparent TSC rate, and LAPIC periodic ticks coalesce so `uptime_ms` during a sleep is not
50–100. Rule: in-guest checks key off the invariant-TSC CPUID bit. Without it, retry PIT against a
fresh HPET sample with a 50–200% band, and accept `now_us` (~50 ms) when ticks coalesce. Do not loosen
the invariant-TSC path.

**Serial output from multiple CPUs is unreadable.**
No lock on TX. Rule: lock serial TX, and write each line whole: format it, newline included, into one
buffer and send it under one TX hold. Byte granularity keeps bytes whole, but `Serial::write_fmt`
takes TX once per `write_str` piece and `log_fmt` sends the newline separately, so another CPU can
split a line and the harness then misses a contract line ([section 7.7](#77-locking-with-more-than-one-cpu),
ROADMAP §10.2, F138).

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

**An AP that missed its bring-up timeout runs on freed memory.**
The timeout path frees the AP's kernel, RSP0, and IST stacks and its GDT/TSS and marks its idle TCB Dead, but an AP that accepted a SIPI and then
stalled past 3 s can keep running on them, or read the next AP's parameter block. Rule: the timeout path
sends INIT, clears the AP's online bit, and leaks what it gave the AP; a failed AP costs its stack and
tables, not a second CPU on the same memory ([section 7.4](#74-ap-bring-up-sequence)).
`smp_init::start_one` frees without INIT (ROADMAP §20.1, F032).

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
the same drop and completes from a threaded IRQ. Rule: completion wakes the waiters under SCHED
and then stores the status inside that section, as its last access to each waiter
([section 2.8](#28-publish-last)). `IoWaiter::finish` breaks this: it stores
`done` with Release before it takes SCHED, then wakes (ROADMAP §10.10, F002). Never hold
the queue lock across I/O or across that wake
(DEVICE then SCHED is the wrong order). Hard IRQ must not run this path: enqueue
work only (DESIGN [§2.2](#22-interrupt-handler-rules)). Ramdisk backing is BSS, not
a heap `Vec`: allocating under RANK_DEVICE would take RANK_HEAP (lock order).

Blocking wait and async submit share the same cookie. The buffer and waiter must
outlive the completer's last access to them, which under the rule above is the
status store.

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
read/write/discard requests merge; fences do not. `try_merge` checks only the
lowest queued fence, so a request can merge into one queued before a second
fence, and a fence whose seq is `u32::MAX` reads as no fence; C-LOOK can also
reorder overlapping writes whatever their seq (ROADMAP §10.11, F043).

virtio-blk does not implement the contract above. `issue()` completes a `Barrier`
locally while earlier requests are in flight, and a `Flush` goes to the device
once earlier writes are dispatched, not completed; virtio 1.2 §5.2.6.2 makes a
write stable only after a FLUSH sent after that write completed. The block
cache's `flush` does not wait for writeback in flight either
([section 10.6](#106-block-cache)). ROADMAP §10.11 lands all of it (F043).

## 10.3 Failure

I/O errors retry up to `DEFAULT_RETRY_BUDGET` extra attempts, then the device
goes `Failed`. Further submits return `Failed`. `Inval` (range, size) is not
retried and does not fail the device. No infinite retry loop.

Rule: every request has a deadline, 30 s by default, as Linux's block layer has. When it
passes, the driver's timeout handler gets the request back from the device before anything
touches its buffer. Where the device can abort one command, it aborts it (NVMe's Abort). Where
it cannot, the handler resets the device (virtio's status 0, read back as 0; AHCI's port
reset). A device the reset does not bring back goes `Failed`. Only after the device can no
longer write the buffer does the request complete, with `Io` or by resubmission, and does its
submitter get the buffer back (§2.11 rule 3: stop the device, then release). Rule; not yet
enforced: `IoWaiter::wait` parks with `FAR_DEADLINE`, so a lost completion, such as one after a
missed kick ([section 10.4](#104-virtio-blk)), blocks its submitter for good, and so does
every thread that then waits for a lock the submitter holds (ROADMAP §12.5).

Why: a lost completion is a device or driver bug the kernel must survive and report (§1.1
constraints 4 and 5, which already bound every poll). Freeing a timed-out request's buffer while
the device may still write it would turn a hang into memory corruption, so the reset comes
first. Rejected: a timeout that only reports and keeps waiting, which leaves every waiter
behind the request stuck. It is safe but not live.

Logical block size is per device. Do not assume 512. Capacity is in those
blocks. Discard on ramdisk validates the range and otherwise no-ops.

## 10.4 virtio-blk

Modern virtio-blk (`1af4:1042`, `VERSION_1` required) binds by id on the
Phase 6 transport. Config reads capacity (512-byte units), `blk_size` (512
if `F_BLK_SIZE` is absent), and topology when offered. Each request is a
descriptor chain: header + data (or discard range) + status. The status
byte is device-writable DMA, never a stack slot. Completions harvest the
used ring on the threaded IRQ and wake the same `IoWaiter` cookies as the
ramdisk. The hard IRQ only acks ISR, reading it on every interrupt. The driver always runs on
MSI-X, where virtio 1.2 §4.1.4.5.2 says a driver should not read ISR (ROADMAP §26.4, F122).

`F_MQ`: one virtqueue per online CPU, capped by the device `num_queues` and by
`MAX_VQ` (8). Without `F_MQ`, a single request queue. Data goes through 16
bounce slots of 8 KiB shared by the device's queues; a request over 8 KiB is
`Inval`, including one the block queue merged past that size (ROADMAP §12.5,
F119). Flush and discard go to the device when those features are negotiated;
flush without `F_FLUSH` is a successful no-op (nothing to make durable).

`issue()` publishes `avail.idx` after `dma_wmb`, then `should_kick` loads
`avail_event` or `used.flags` to decide whether to kick, with no full barrier
between, so a kick can be lost and the queue stop ([section 4.7](#47-dma);
ROADMAP §10.3, F016). `kick` writes the doorbell at the notify formula in
[section 9.3](#93-interrupts). The doorbell
value is the queue index; `kick` writes 0 for every queue (ROADMAP §11.5, F047).

## 10.5 Partitions

MBR (primary + extended/logical) and GPT parse in `src/part.rs`. Protective
MBR type `0xEE` is not a data device; GPT is. Header and entry CRCs are
checked; a bad primary falls back to the backup header at the last LBA.
EBR walk is capped at 128; a corrupt next-LBA stops the chain. `parse_mbr`
reads the four primary entries from `sector_buf`, which the EBR walk reuses,
so a primary listed after an extended entry is read from the last EBR
(ROADMAP §10.12, F117). Entries are checked against the disk size only: an
entry that overlaps another entry or the table itself, a GPT header whose
MyLBA is not the LBA it was read from, and a GPT entry outside the usable
range are all accepted (ROADMAP §13.9, F117).

Children are offset-limited `BlockDevice`s. Child LBA `l` maps to
`start + l` and I/O past `nsectors` is `Inval`. Marker
`vibeOS: block: <parent>p<N> <n> sectors` (e.g. `ram0p1`, `vdap1`).
`register_table` takes names from fixed tables (`ram0p1` to `ram0p5`,
`vdap1` to `vdap4`) and stops without a log line at the first entry past
them (ROADMAP §10.12, F117). Only in-guest tests use the child devices:
`mount_dev` accepts only `ram0` and `vda`, and devfs block nodes return
`NotSupp` (ROADMAP §10.4, F081).

`part_init::init` runs in every build. It stamps an MBR on `ram0`, and it
stamps a GPT on `vda` whenever `vda`'s table fails to parse or has no
entries, including after a read error; the only guard is a 512-byte block
size and at least 1024 sectors. A whole-disk vibefs or FAT32 image on `vda`
loses LBA 0 to 33 and its last 33 sectors on the first boot (ROADMAP §10.11,
F003).

## 10.6 Block cache

Page-granular (4 KiB), 16 pages (`cache::DEFAULT_PAGES`), keyed by
`(dev_id, page offset)`. Read-through, write-back, clock eviction,
sequential readahead, dirty-ratio writeback thread (`blk-wb`). `flush`
writes the pages marked dirty, then calls the device `flush`.
`barrier` writes dirty pages and does not device-flush.

A slot has no filling or writeback state. `take_dirty` clears a page's dirty
bit before `blk-wb` writes it with the lock dropped, so while that write is in
flight `flush` skips the page and can send the device flush first, a page
dirtied again can have two writes to one LBA in flight, and the slot can be
evicted and later read back stale. `find()` matches only valid slots, so a
second reader of a page being filled does not wait for it (ROADMAP §12.5,
F015; the `flush` wait lands with ROADMAP §10.11, F043).

The cache reaches the drivers by raw device id: `cache_init::raw_read`,
`raw_write`, and `raw_flush` match `DEV_RAM0` to `block_init` and `DEV_VDA`
to `virtio_blk_init`, and only in-guest tests call the `BlockDevice` trait
objects. Planned (ROADMAP §10.4, D2): the cache takes a handle from one
registry of `BlockDevice`s, partitions included, instead of matching device
ids (F081). Phase 12 page cache reuses
these pages (same clock, same writeback);
do not grow a second private cache. Hit/miss/device-request counters are
in the `blk` shell command. The cache lock is RANK_DEVICE and is dropped
before blocking device I/O.

---

# 11. Portability

x86_64 and aarch64 are peers from ROADMAP Phase 11; x86_64 came first and is the reference when the
two disagree (ROADMAP, How to read this). This section is the contract for the seam between them.
`docs/ARCH.md` (ROADMAP §10.3) maps each seam trait to the module that implements it in each port.

## 11.1 The seam

What is architecture-specific: exception entry and exit, the context switch, the page-table format
and its flags, TLB and cache maintenance, the interrupt controller and vector map, the timer and cycle
counter, the barrier helpers (`dma_wmb`, `dma_rmb`, `dma_mb`), MMIO accessors, the per-CPU base
register, the syscall instruction and the user register frame, the user-memory access primitives, FP
and SIMD state, the user TLS register, AP bring-up, and the machine state the boot handshake hands
over. Everything else is shared. The syscall table's semantics are shared too; only numbers and
argument order differ per architecture (ROADMAP §10.5), and user-visible structures keep their
meaning while their layouts follow Linux's per-architecture uapi (ROADMAP §13.10).

The mechanism:

- Each concern is a trait in `vibeos-core` (`PageTable`, `Barriers`, `CycleCounter`, and so on).
  Each port implements the traits in the kernel crate, on one zero-sized type: `arch::x86_64::Arch`
  or `arch::aarch64::Arch`. `vibeos-core` carries a third implementation, `arch::stub::Arch`, which
  the host tests use.
- Portable code that needs the seam takes the implementation as a type parameter of the type that
  uses it (`Mapper<A: PageTable>`, `SplitQueue<B: Barriers>`), never as `dyn`, so every seam call is
  resolved at compile time and inlines.
- The kernel binary names its port once, `type Arch = arch::current::Arch;`, chosen by
  `cfg(target_arch)` in the kernel crate. `vibeos-core` contains no `cfg(target_arch)` and no
  assembly (ROADMAP Phase 10 gate).
- The atomics seam is the one exception: a module selected by `cfg(loom)` (ROADMAP §10.8), because
  loom replaces types, not functions.

Why: the stub port lets the portable crate build and run its tests on any host, which ROADMAP §10.3
calls the cheapest second architecture, and the compiler checks that a port implements every
function. §1.1's "concrete over generic" allows the traits because there are three implementations.
Rejected: `dyn` traits (an indirect call in the page-table and barrier hot paths, and no inlining);
`cfg`-selected modules inside `vibeos-core` (the portable crate would carry `cfg(target_arch)` and
could not be built against a stub on a third host); and link-time `extern` symbols (signatures the
compiler does not check, and one implementation per test binary).

## 11.2 Address space on aarch64

The kernel half is TTBR1 with a 48-bit VA (`T1SZ` 16); user space is TTBR0, also 48-bit, so user
addresses run to 2^48, Linux arm64's `TASK_SIZE` for 48-bit VAs, where x86_64's stop one page below
2^47 (`USER_MAP_END`, ROADMAP §10.6); each architecture's limit is a constant of its port (§11.1).
§4.1's fixed regions sit inside the TTBR1 range unchanged, and the physmap base is Limine's HHDM
offset, as on x86_64 (§4.1).
The low identity window and the AP trampoline page have no aarch64 counterpart: cores start through
PSCI on a temporary TTBR0 identity map that is dropped once they run in the kernel half (ROADMAP
§11.4). Memory attributes come from MAIR, and MMIO is Device-nGnRE through `ioremap` (ROADMAP §11.1).

## 11.3 Adding an architecture

A port adds `arch/<name>/` implementing every §11.1 trait, its rows in `docs/ARCH.md`, a harness
profile, and its marker list (ROADMAP §11.7). It changes no portable module. ROADMAP §11.8's riscv64
stretch measures that: a port that needs a change outside `arch/` has found a seam defect, and the
fix lands in the seam, not as a special case in portable code.
