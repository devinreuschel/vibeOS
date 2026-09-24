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
section cited with it lands the fix. A design-review id (`Gnnn`, `Hnnn`, `Jnnn`) names a row of
[reviews/DESIGN_REVIEWS.md](reviews/DESIGN_REVIEWS.md). When code and this file disagree, one of them
is a bug ([§1.4](#14-documentation-rules)). §2's invariants bind every ROADMAP box, and a box that
changes other text here changes it in the same commit (ROADMAP, How to read this). The numbers are
load-bearing. Change them deliberately and update this doc in the same commit. An agent implementing a
subsystem does not get to re-litigate the address map, the vector numbers, or the lock order halfway
through.

## Contents

| | Section | Covers |
|---|---------|--------|
| 1 | [Overview](#1-overview) | Constraints, layers, module map, sources and licenses |
| 2 | [Invariants](#2-invariants) | Lock order, handler rules, panic policy, markers, invariant register, publish last, preemption, trust boundaries, object lifetimes, RCU |
| 3 | [Boot](#3-boot) | Toolchain, Limine, `_start` order, linker |
| 4 | [Memory](#4-memory) | Address map, buddy allocator, paging, heap, firmware runtime services |
| 5 | [Interrupts](#5-interrupts) | GDT/IDT, exception policy, vector map, PIC and APIC, privilege transitions |
| 6 | [Time](#6-time) | Clock sources, calibration, timekeeping, timers |
| 7 | [SMP](#7-smp) | ACPI, AP bring-up, per-CPU, IPIs, shootdown, offline and online |
| 8 | [Testing](#8-testing) | Tiers, marker contract, QEMU flags, CI |
| 9 | [Pitfalls](#9-pitfalls) | Bugs already paid for once |
| 10 | [Block I/O](#10-block-io) | Requests, ordering and flush, ramdisk, virtio-blk, partitions, cache |
| 11 | [Portability](#11-portability) | The architecture seam and its inventory, the aarch64 address space, adding a port, the EL0 and ring-3 environment, aarch64 exceptions and privilege transitions |
| 12 | [Device model](#12-device-model) | Devices, parents and suppliers, states, probe order, resources, removal |

On-disk filesystem formats live in their own docs, not here ([§1.4](#14-documentation-rules)): [VIBEFS.md](VIBEFS.md) (vibefs **version 1**, CoW metadata + atomic superblock switch). Syscall ABI: [SYSCALL.md](SYSCALL.md). The Linux baseline, deliberate differences from it, and native interfaces: [LINUX.md](LINUX.md).

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
   call up, except through a hook an upper layer installs at init, and §1.2 lists each one. The
   buddy and the heap never call up, directly or by hook, but for the TLB shootdown a kernel-half
   mapping change makes (§4.3): an allocation reaches reclaim, the writeback wait, and the OOM
   killer only through the allocation entry's hooks (§4.4).
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

Cross-cutting and allowed from anywhere: `serial`, `panic`, `sync`, `log`, and the allocation entry
(§4.4), which calls down into the heap and the buddy.

Upward calls go only through hooks an upper layer installs at init, each listed here with the layer
that sets it:

- `paging::tlb_shootdown_others`, set by `ipi_init::init` before the first AP starts (§4.3).
- Planned (ROADMAP §10.3, A4): the spin-poll hook in `sync_init`, set by `ipi_init::init`, and the
  scheduler hooks in `ipi_init`, set by `sched_init::init`.
- Planned (ROADMAP §12.6): the allocation entry's hooks, set by the page cache (clean-page reclaim),
  the writeback threads (their wake and bounded wait), and the process layer (the OOM killer).

A hook not yet set is skipped, so host tests and early boot run a lower layer alone: before the
reclaim hooks are set, an allocation goes from the reserve straight to failure.

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
  file. Syscall ABI: [SYSCALL.md](SYSCALL.md). The Linux baseline, deliberate differences from it,
  and native interfaces: [LINUX.md](LINUX.md).

## 1.5 Sources and licenses

vibeOS is MIT ([LICENSE](../LICENSE)). What goes into the tree comes from specifications, manuals,
and measured behaviour, or from sources whose license lets it into an MIT tree:

- Nothing is copied or translated from a file whose only licenses are the GPL or LGPL: most of
  Linux, glibc, GNU tools, and QEMU's GPL parts. The unit is the file, since the Linux tree mixes
  licenses: a Linux file whose SPDX line or notice text offers a notice-only license (next bullet),
  alone or as one option of a dual license such as `GPL-2.0 OR MIT`, falls under the next bullet.
  The Linux-interfaces rule asks for Linux's behaviour, which vibeOS learns from man pages,
  specifications, and running Linux, the oracle kernel of ROADMAP §12.4, never by porting Linux's
  code. Numeric constants, struct layouts, and `ioctl` numbers that an interface defines are facts;
  each is written down with a citation of where it is defined. The facts clause covers layouts and
  constants, not the algorithm that keeps a format consistent. A format that is valid only when such
  an algorithm maintains it (a B-tree, a space map, a commit order) and that only GPL code defines is
  written up in `docs/` from what Linux writes and what Linux's own tools accept or reject, before
  any code, and its GPL sources are not read for it (ROADMAP §29.6). A ROADMAP line may name a Linux
  function or file to identify a behaviour, but a GPL-only implementation is never read while
  writing the vibeOS code that matches it: code written with its source open is a translation
  however its text differs. The facts clause reads a header or a format's definition (a uapi header,
  `md_p.h`, `md-bitmap.h`) for its constants and layouts only.
- Code under a notice-only license, one whose only condition is keeping its copyright and permission
  notice or that has none (MIT, X11 and HPND, BSD-2-Clause and BSD-3-Clause, ISC, zlib, 0BSD, or
  that option of a dual license), may be adapted into a vibeOS file. The file keeps the notice and
  carries a provenance header naming the upstream project, the file's path, the pinned tag or
  commit, and the upstream license as an SPDX identifier; ROADMAP §10.9 checks the header and ships
  the notice. Code under Apache-2.0 alone, or under any license with a further condition, is never
  adapted into a vibeOS file: it enters as a crate under ROADMAP §10.9's `cargo deny` policy or as a
  port under §14.10, keeping its own license and NOTICE.
- Cryptographic primitives and the TLS state machine are depended on, never written in-tree: pinned,
  widely reviewed crates behind one facade, `vibeos-crypto`, under ROADMAP §10.9's `cargo deny`
  policy, with RustCrypto and dalek for the primitives and rustls for TLS (ROADMAP §13.10, §14.7,
  §15.11). In-tree crypto code is the entropy pool, the CSPRNG's construction, the signature
  formats, and glue.
- Third-party sources the tree builds rather than copies follow ROADMAP §14.10's port policy. A test
  input taken from a GPL-only file, such as a GPL-2.0-only Linux device tree or QEMU's ACPI test
  tables, is fetched at test time and never committed; one that offers a notice-only license is
  committed under it with its header (ROADMAP §20.7). Output a program generates that holds none of
  the program's own code, such as a device tree QEMU dumps with `-machine dumpdtb=`, is data and may
  be committed (ROADMAP §11.5); AML that QEMU generates carries methods QEMU's authors wrote, so
  QEMU's ACPI tables stay fetched. Test media and web pages are generated at test time by pinned
  tools or are free-licensed and fetched by SHA-256; copyrighted media is never committed.
- A binary the project publishes (an image, a package, a release asset, or a workflow artifact
  anyone can download) carries the copyright and license notices its third-party code's licenses
  require: `/LICENSES/` on an image, and the same file as an asset beside it in a release, generated
  by ROADMAP §10.9's notices script. A copyleft binary also carries its source offer (ROADMAP
  §14.10). Rule; not yet enforced: today's ISO carries Limine's binaries, the `limine` crate, and
  Rust's `core` and `alloc` with no notice (ROADMAP §10.9).

Why: one function derived from GPL code would put the kernel under the GPL, against the project's
license. The kernel review's spot check found no such copy, but no rule said so. Adapted code is
notice-only so that every vibeOS file stays MIT, as `Cargo.toml` and the notices file (ROADMAP
§10.9) say: an Apache-2.0 file would add its §4(b) notice of changes and its §4(d) NOTICE to every
binary, and would keep vibeOS code out of GPLv2 projects, whose license Apache-2.0 is incompatible
with. Crypto written
in-tree fails where test vectors do not look (timing, nonce reuse, signature malleability), and one
such bug forges a package; the crates have had the review this project cannot give them.

**Published data.** The repository is public, and so is everything its workflows log, upload,
commit, or file: artifacts, issues, the `ci-history` branch, and release assets. None of it carries
a person's data or a secret:

- A CI guest holds no personal data and no secret but the public test keys (ROADMAP §14.3), and a
  job that holds a secret uploads no core (ROADMAP §10.7), so a guest's core, log, and report are
  published as they are. An issue a workflow files carries the ROADMAP §10.7 core tool's text report
  and a link to the run's artifact, never an attached dump. A dump needed after its artifact expires
  is regenerated from its commit and seed. From ROADMAP §22.5 a fuzz job's crash record is the
  exception: it is sealed to the triage key, and its issue names only a crash id until the fix is
  published (§8.6).
- A physical machine's memory dump and firmware tables (`acpidump`, SMBIOS) are never committed or
  published: the AML in them is the vendor's proprietary code (ROADMAP §14.10), and an OEM's MSDM
  table holds a Windows product key. Host tests read them on the rig host from a store no workflow
  uploads, the tree commits only results derived from them (method results, `_PRT` routes, hotkey
  lists), and a crash leaves the rig host only as its text report.
- Captures are clean where they are taken, so they replay unmodified: the rig's access points use
  locally administered BSSIDs, its test stations randomized addresses, and its Bluetooth pairings
  its own peripherals, and a monitor-mode capture keeps only frames to or from those addresses as it
  is taken. A text record from a physical machine (`lspci -vv`, a kernel log, a test result) leaves
  the rig host only through a host-tested scrub step that replaces serial numbers, MAC addresses,
  UUIDs, and hostnames with fixed placeholders, as ROADMAP §10.9 keeps hostnames and home paths out
  of dev-host records.

ROADMAP §37.4's crash-report record decides what may leave a user's machine.

---

# 2. Invariants

Break these and the failure shows up somewhere else, hours later.

## 2.1 Lock order

Acquire in this order, release in reverse. Never take a lower number while holding a higher one, and
take a second lock of a number already held only through §2.3's nested acquire.

1. heap
2. page tables: the kernel's lock, and from ROADMAP §12.1 one lock per address space
3. physical allocator (buddy)
4. scheduler (also: wait-queue lists and blocking-primitive predicates)
5. device / driver locks
6. serial

Serial is last so any lock holder can still log. Heap is first because growing it takes PT and then
BUDDY, and nothing allocates from or frees to the heap while holding either. Page tables come before
the buddy because mapping a page allocates its table frames. Blocking `WaitQueue`s are serialized by
the scheduler lock: the predicate check and the enqueue happen under that same lock (DESIGN
[§9.4](#94-concurrency)).

Planned (ROADMAP §13.3): a seventh rank, SOCK, ahead of all six, for socket state (below). Planned
(ROADMAP §19.4): a TIMER rank just before SERIAL, for the per-CPU timer bases
([§6.5](#65-timers-and-timeouts)), so code under any other spinlock may arm, re-arm, or cancel a
timer.

The six ranks order spinlocks: `SpinMutex`, and the cross-CPU `IrqCell`s that ROADMAP §10.3 turns
into ranked `SpinMutex`es. Sleeping locks form a tier outside all six: `BlockingMutex`, `RwLock`,
`Semaphore`, and waiting for a page or buffer to finish I/O. A thread takes a sleeping lock only with
IF=1 and no spinlock held ([§2.9](#29-preemption-and-interrupt-state) rule 4), so every sleeping lock
ranks before every spin rank, and no spinlock is ever held across a sleep. Within the sleeping tier,
outermost first:

1. the locks a call may hold across a copy to or from user memory, outermost first: an open file
   description's position lock; then a stream's lock (a pipe's lock, a socket's owner lock, a TTY's
   read lock or write lock); then the filesystem namespace and inode locks: the mount table, then a
   directory, then an inode in it; a parent directory before its child. A `rename` takes the
   volume's rename lock, then its two directories: an ancestor before its descendant, and two
   directories neither of which contains the other in address order
2. the address-space lock (ROADMAP §13.1: `mmap`, `munmap`, and `mprotect` take it for writing, the
   fault path for reading). It guards the region tree only. A page-table entry changes under its
   space's page-table spinlock (the PT rank above), so the reverse-map unmap that direct reclaim
   does ([§4.4](#44-kernel-heap)) takes no address-space lock; it takes the reverse-map lock
   (level 3b) only by try-lock. The fault path never waits while it holds it (below)
3. waits on a page-cache page, in a file's mapping or a block device's
   ([§10.6](#106-block-cache); ROADMAP §12.5)

   3b. the reverse-map lock of a file mapping or an anonymous object ([§4.6](#46-what-comes-later)),
   a sleeping `RwLock`. A walk that finds every PTE mapping a page takes it for reading, possibly
   with that page held busy; linking or unlinking a region, or changing a linked region's range,
   takes it for writing, possibly under the address-space lock. No level-4 lock is held when it is
   taken: writeback's clean step walks before writeback takes the filesystem's locks. Code holding
   it takes no lock of levels 1 to 4 and no second reverse-map lock, except that for a region linked
   into two objects the file mapping's is taken before the anonymous object's; spinlocks may follow
   it, as after any sleeping lock. It is numbered 3b so that level 4 keeps its number.
4. a filesystem's block-mapping and volume I/O locks, which its page-fill and writeback paths take
   and which are never held across a user copy. Writeback and a filesystem commit take no lock of
   levels 1 and 2 and take a page busy only by try-lock, coming back later for a page they find
   busy, so a thread that holds an inode lock, its address-space lock, or a busy page can always
   wait for writeback progress ([§4.4](#44-kernel-heap) rule 3). A thread that has joined an open
   vibefs v2 transaction, which a commit waits for, holds it as a level-4 lock
   ([VIBEFS.md](VIBEFS.md) §15; ROADMAP §14.8)

A copy through a faulting accessor (§5.1) may fault, and the fault path takes the address-space lock
for reading, then page waits, then, to fill a file page, the filesystem's level-4 locks. So such a
copy is allowed while level-1 locks are held, as `write` needs, and under no lock of levels 2 to 4,
level 3b included. The reverse is forbidden: code that holds the address-space lock takes no level-1
lock. A non-faulting accessor (§5.1) takes no fault path and no lock, so it may run under any of
them.

The position lock serializes `read`, `readv`, `write`, `writev`, `lseek`, and `getdents64` on one
open file description of a regular file or directory, so threads and processes that share the
description through `CLONE_FILES`, `fork`, or `dup` each see a whole offset update (Linux's
`f_pos_lock`). Every such call takes it. `pread64` and `pwrite64` take none, and neither does a
description of a pipe, socket, TTY, or character device, whose I/O does not use the offset. The
file's size belongs to its inode and changes under the inode lock. A stream's lock guards the
stream's buffer and is held across its user copy, never across a wait for a party that needs it: a
pipe's lock and a socket's owner lock, which readers and writers both take, are dropped before the
caller sleeps on the stream's wait queue, while a TTY's read lock, which only readers take, may be
held while a reader waits for input (Linux's `atomic_read_lock`). So a `read` blocked on a shared
pipe, socket, or TTY never holds off a `write` through the same description. Two stream locks nest
only in address order, under a ROADMAP §13.12 subclass, as a pipe-to-pipe `splice` needs. A TTY's
input queue, its line and echo state, and the termios settings its input side reads are under a
spinlock, because the line discipline runs in the input device's bottom half
([§5.4](#54-irq-registration)), which takes no sleeping-tier lock. Session, process-group, and
process-table state is under spinlocks (ROADMAP §10.3 makes the process table a ranked `SpinMutex`),
which a holder of any level-1 lock may take.

A buffered `write` whose user buffer maps the very page it writes would fault on that page while it
holds the page busy (level 3) and wait on itself. So the write path copies into a busy page only
through a non-faulting accessor (§5.1), which returns a short count instead of taking the fault. On
a short count it releases the page, faults the rest of the source in with no page held, and retries.
This is Linux's `fault_in_iov_iter_readable` loop. A `read` into a buffer that maps the page it
reads needs no such loop, because it copies from a page that is up to date and not busy.

The fault path meets these levels in that order, but from ROADMAP §13.1 it never waits while it
holds the address-space lock. It holds the lock for reading only to find the region and to install
the PTE, and takes a page busy only by try-lock. When the page is busy, not yet up to date, or under
a writeback that a store must wait for, the fault takes a counted reference to the page, drops the
address-space lock, and then waits for the page or fills it, taking the filesystem's level-4 locks
with no level-2 lock held. It then drops the reference and restarts from the region lookup, since
the region may have been split, moved, or unmapped meanwhile. This is Linux's `VM_FAULT_RETRY`.
Within one attempt the fault still installs only against the PTE it read (ROADMAP §12.2). An
allocation with reclaim may run under the lock, because reclaim takes a sleeping lock only by
try-lock and bounds each of its waits ([§4.4](#44-kernel-heap) rule 3). So a fault that waits for a
disk holds up no `mmap`, `munmap`, `mprotect`, or `fork` in its process, no fault queued behind
them, and no OOM reaper's try-lock.

Every wait on the fault path ends early when the thread's process has a fatal signal pending: the
address-space lock's acquire, a page wait, the fill's acquire of level-4 locks and its wait for the
page's read, direct reclaim's wait for writeback, and the OOM killer's wait for its victim.
`BlockingMutex` and `RwLock` gain killable acquires for this; they are not new lock types (AGENTS.md
rule 10). A read whose waiter leaves keeps running, and its completion finishes the fill and
releases the page. A user fault then returns to the signal, and a fault inside a user-memory
accessor takes the exception-table fixup. The buffered `write` loop above checks for a fatal signal
on each pass and returns the bytes written so far, or `EINTR` if there are none, and `fork` checks
between the regions it copies and again before it makes the child runnable, and fails if one is
pending, as Linux's `dup_mmap` and `copy_process` do, so an OOM victim cannot finish cloning itself.
Other sleeping locks and waits stay uninterruptible, as most of Linux's are. Planned (ROADMAP §12.6,
§13.1).

The rename order is Linux's too: `rmdir` holds a parent and then the child it removes, so a
`rename` whose directories were an ancestor and its descendant, taken in address order, could hold
the child and wait for the parent. The rename lock serializes cross-directory renames, which is why
unrelated directories may go in any fixed order. `mmap` of a file takes a counted reference to the file's
page-cache object before it takes the address-space lock, never the inode lock inside it. A region's
references go the other way: they are dropped after the address-space lock is released. `munmap`, a
`MAP_FIXED` replacement, `mremap`, `mprotect`'s merge, `execve`'s release of the old image, and
exit's teardown move each removed region onto a local list under the lock and drop the list after
unlocking, since a last reference can release an unlinked inode, which takes that inode's lock and
the filesystem's block-mapping locks (Linux defers `fput` for the same reason). `msync` and an
`fsync` of a mapped range take counted references to the files of the regions they cover under the
lock for reading, release it, and then write back and wait (Linux releases `mmap_lock` before
`vfs_fsync_range`). Holding the lock across that writeback deadlocks three threads: an `msync`
holding it for reading waits for the inode lock of a `write` whose user buffer faults, the fault's
read request parks behind an `mmap` queued for writing, and the `mmap` waits for the `msync`. A
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
takes FAT's `fat_lock`, and contention never fails an operation; from ROADMAP §12.6 only a page
fill's acquire of it is killable, and ends early on a fatal signal (above).

Why: a lock that every file operation takes and that backends block under serializes all file I/O
behind one block wait, deadlocks a named-pipe read against its writer, and leaves the fault path,
which holds the page it fills busy (level 3), no legal way to fill a file page. Rule; not yet
enforced: the VFS lock is a `RANK_DEVICE` spinlock that the File API drops before any FAT or block
wait, and each FAT and vibefs volume sits behind a busy flag whose waiter yields and fails the
operation with `EIO` after 1,000,000 yields, and which `drop_slot` force-clears (ROADMAP §10.4,
F060).

A socket has two locks, as Linux's `lock_sock` and `bh_lock_sock` do. Its spinlock, at the SOCK
rank, guards the protocol state and the socket's queues; network receive and timer callbacks
([§2.2](#22-interrupt-handler-rules)) take it, and code under it may allocate fallibly and wake. Its
owner lock is a flag that changes only under that spinlock, with waiters on the socket's wait queue:
a syscall holds it across its user copies, as a level-1 stream lock, and releases it before it
sleeps for data or buffer space. A receive or timer callback that finds the socket owned appends its
packet or event to the socket's bounded backlog, dropping and counting it when the backlog is full;
the owner drains the backlog as it releases the owner lock and clears the flag only once the backlog
is empty. Two sockets' spinlocks nest only through `lock_nested` (§2.3), in address order, as a
socket pair needs. ROADMAP §13.3 adds the lock pair and the rank, §15.5 the backlog.

## 2.2 Interrupt handler rules

An interrupt handler must not:

- allocate or free (no heap, no buddy, no `Vec`, no `format!`)
- take any lock that is ever held with interrupts enabled
- log at anything but the most extreme failure path
- run unbounded loops

An interrupt handler must:

- EOI before it can possibly context switch, so the controller is not held across a switch
- rearm its own one-shot timer source before doing anything else that can yield
- keep its stack frame small: only `#DF`, NMI, `#MC`, and `#DB` enter on dedicated IST stacks
  (§5.1), and NMI, `#MC`, and `#DB` move to the thread's kernel stack when they interrupt ring 3
  (§5.10 rule 3); every other vector runs on the interrupted kernel stack, or on TSS.RSP0 when it
  interrupts ring 3; on aarch64 every vector runs on the interrupted kernel stack, or on the thread's
  kernel stack when it interrupts EL0, after an entry test that moves it to this CPU's overflow stack
  when that stack has overflowed (§11.5 rule 6); on either architecture, a handler on the
  interrupted kernel stack runs inside the 4 KiB that §4.5's stack budget leaves above the deepest
  path

Both of the "must" rules are expanded in [section 5.8](#58-handler-ordering-rules), because both are
easy to violate and expensive to debug.

Blocking and allocation are a class of bug, not an instance. Context rules:

| Context | May block? | May alloc? | Scheduling class ([§7.8](#78-per-cpu-scheduling)) |
|---------|------------|------------|---|
| Hard IRQ / MSI handler | No | No | None: it runs on the thread it interrupted |
| Softirq equivalent (high-prio workqueue) | No | Fallible only, without direct reclaim, down to half of the reserve ([§4.4](#44-kernel-heap)); the hard IRQ only enqueues | Its CPU's worker: fair, nice -20 |
| RCU read-side section ([§2.12](#212-rcu)) | No; it may be preempted | Fallible only, without direct reclaim, down to half of the reserve ([§4.4](#44-kernel-heap)) | Its thread's |
| Threaded IRQ bottom half | Only for its own device's resources, with a deadline; never for an I/O completion ([§5.4](#54-irq-registration)) | Fallible only, without direct reclaim, down to half of the reserve ([§4.4](#44-kernel-heap)) | `SCHED_FIFO` 50 |
| Block error handler ([§10.3](#103-failure)) | Yes, with IF=1 and no spinlock held | Fallible only, without direct reclaim ([§4.4](#44-kernel-heap)) | As a threaded bottom half ([§7.8](#78-per-cpu-scheduling)) |
| Workqueue worker | Yes | Yes, fallible only ([§4.4](#44-kernel-heap)); without direct reclaim while it runs a softirq-equivalent item (row above) | Fair, nice 0 |
| Driver `probe` | Yes | Yes, fallible only ([§4.4](#44-kernel-heap)); a failed probe leaves its device unbound and logs why | Fair, nice 0 |
| Syscall body, fault handler for a CPL-3 fault | Yes ([§2.9](#29-preemption-and-interrupt-state)) | Yes, fallible only ([§4.4](#44-kernel-heap)) | The calling thread's |
| CPL-0 fault in a faulting user accessor (§5.1; may sleep from ROADMAP §12.2) | Yes ([§2.9](#29-preemption-and-interrupt-state) rule 3); may take the address-space lock (§2.1) | Yes, fallible only ([§4.4](#44-kernel-heap)) | The calling thread's |
| CPL-0 fault in a non-faulting user accessor (§5.1) | No: it goes straight to the fixup | No | The calling thread's |
| Network receive: a queue's threaded bottom half ([§5.4](#54-irq-registration)) | No: it takes no sleeping lock and never waits for a socket's owner; a packet for an owned socket goes on its backlog (§2.1) | Fallible only, without direct reclaim, down to half of the reserve ([§4.4](#44-kernel-heap)) | `SCHED_FIFO` 50; fair, nice 0 past its budget |
| Timer callback: a timeout-wheel callback ([§6.5](#65-timers-and-timeouts)), run as a softirq-equivalent item on the CPU whose wheel fired it | No | Fallible only, without direct reclaim, down to half of the reserve ([§4.4](#44-kernel-heap)) | Its CPU's worker: fair, nice -20 |
| NMI and `#MC` at any CPL, `#DB` at CPL 0; shootdown and call-function work run from a spin | No | No | None: it runs inside whatever it interrupted |

Code in the last row runs inside whatever IF=0 section its CPU was in, locks included: an NMI,
`#MC`, or `#DB` interrupts it, and a CPU in a serviced spin (§2.3) runs incoming shootdown and
call-function work there. So that code takes no lock of any kind (a `SpinMutex`, an `IrqCell`, or a
TAS, the log ring's TAS and the serial TX lock included), since the section it interrupted may hold
that very lock. It allocates nothing, does a bounded amount of work, and writes only per-CPU state
and lock-free rings. A call-function closure that needs a lock queues a work item on its CPU
instead. The panic path (§2.5) is the one exception: it reads the log ring and writes COM1 without
taking their locks.

The hard-IRQ top half acknowledges and wakes. Work that allocates or blocks runs on a kernel thread
([section 5.4](#54-irq-registration), ROADMAP §6.6). The timer interrupt's top half also expires
deadline timers whose action is a wake or a signal, at most 32 wakes per interrupt
([§6.5](#65-timers-and-timeouts)). A last put of a counted object runs the object's release in place
only where [§2.11](#211-object-lifetimes) rule 6 allows it; anywhere else the release is deferred to
a workqueue worker.

Network receive polls its queue for at most a budget of packets per wake, so one flooded queue
cannot hold its CPU; [§7.8](#78-per-cpu-scheduling) gives the budget and what runs past it. Loopback
has no interrupt: its transmit queues the packet, and receive runs as a softirq-equivalent item on
the sending CPU. A timer callback (TCP's retransmit and delayed-ACK timers) runs on the CPU whose
timeout wheel fired it; a POSIX timer's or a `timerfd`'s expiry is a deadline timer and runs in the
timer interrupt's top half instead ([§6.5](#65-timers-and-timeouts)).
`cancel_sync` returns only once the callback runs on no CPU, as `free_vector` does for a handler
(§5.4), and a pending timer holds counted references to what its callback touches
([§2.11](#211-object-lifetimes) rule 5).

## 2.3 Locking with interrupts

Every spinlock that is taken from both an ISR and normal context disables interrupts for the whole
critical section. That is the default: `SpinMutex` is IRQ-aware, and the non-IRQ-aware variant does
not exist. The scheduler lock, the input ring, the buddy allocator, and the heap all qualify.

Pick one spinlock implementation and use it everywhere. The old tree ended up with two (a ticket lock
in one design doc, an IRQ-guarded spin mutex in the code) and the mismatch was a source of confusion
for weeks.

`SpinMutex` is that implementation: a compare-and-swap lock until ROADMAP §27.5 makes it a queued
(MCS) lock, for every lock in one change. A waiter spins with IF=0, and nothing that can run inside
another holder's IF=0 section takes a lock (the serviced-spin row below, and
[§2.2](#22-interrupt-handler-rules)'s last row), so a CPU waits for at most one `SpinMutex` at a
time, and the queued lock needs one queue node per CPU. From ROADMAP §21.4, a waiter under KVM on
x86_64 that has spun a bound halts with IF=0 until it is kicked. It still services incoming work, as
§2.9 rule 2 requires: every publisher of work that `service_incoming` serves kicks a target it finds
halted, after its Release store and a full fence.

The lock, the serviced spins, and the two cells:

| Primitive | Use |
|------|-----|
| `SpinMutex` | Shared across CPUs. IRQ-aware. Ranked (§2.1): `lock` refuses a lock whose rank, or a later one, this CPU already holds, and `lock_nested` takes a second lock of a held rank (below). Its spin is a serviced spin. |
| Serviced spin | `SpinMutex::lock`, `ipi_init::wait_acks`, and the call-function slot wait each call `ipi_init::service_incoming` on every iteration, so a CPU that waits on another with IF=0 still acknowledges shootdowns and runs call-function work ([§2.9](#29-preemption-and-interrupt-state) rule 2). That work runs inside whatever the spinning CPU holds, so, like an NMI, `#MC`, or CPL-0 `#DB` handler, it takes no lock ([§2.2](#22-interrupt-handler-rules)'s last row). Planned (ROADMAP §10.7, F135): `service_incoming` first reads this CPU's stop request word, and STOP runs §2.5's stop routine before any slot is served, so a serviced spin stops for a panic with no interrupt, on either architecture and GIC version. On aarch64 a serviced spin never executes WFE and waits with `core::hint::spin_loop`: a masked interrupt is a wake-up event for WFI but not for WFE (Arm ARM DDI 0487, the WFE and WFI wake-up events), so a WFE spinner with IRQs masked neither takes an SGI nor wakes to poll. A line that puts WFE in a serviced spin also makes every publisher of serviced work, the stop word included, issue `sev` after its Release store, and says so. |
| `IrqCell` | IRQ-off exclusive access: `with` takes IRQs off, panics on same-CPU re-entry, and spins while another CPU holds it. Used for CPU-local and boot-only state and as an unranked cross-CPU lock (among them `proc_init::TABLE`, `kva_init::KVA` and `DEFERRED`, `work_init::ST`, `irq_init::IRQ`, `file_init::CWD`, and `log_init::LOG`). Its `Sync` impl has no `T: Send` bound, and `force_unlock` is a safe fn (ROADMAP §10.3, F017). Its spin does not service IPIs, so a cross-CPU cell held across a wait on another CPU can stall a shootdown. Planned (ROADMAP §10.3, F108): every cross-CPU `IrqCell` but the log ring becomes a ranked `SpinMutex`; the log ring's holders never wait on another CPU (§2.5), and ROADMAP §19.5 replaces it with §2.5's lockless ring. |
| `BootCell` | Write once before `smp: done`, then shared `&T`. State written after publication sits behind its own `UnsafeCell` inside `T`. Not yet enforced: every switch writes the BSP's TSS through a pointer cast from `&Bsp` (ROADMAP §10.3, F089). The set-once check is a `debug_assert!` (ROADMAP §10.2, F137), and the `Sync` impl has no `T: Send + Sync` bound, so `per_cpu_init::CPUS` shares the non-`Sync` `PerCpu` (ROADMAP §10.3, F017, F039). |

Two locks of one rank nest only through `lock_nested`, in a pair order the call site's comment
names, and a per-rank count keeps the outer rank held when the inner lock drops. ROADMAP §12.1 adds
`fork`'s pair, the parent's page-table lock before the child's. ROADMAP §13.12's lock classes add
address order for a socket pair and check every pair order. Rule; not yet enforced: `lock` lets a
lock of a held rank nest, the inner release clears the rank bit the outer lock still holds, and
nothing stops the code in §2.2's last row from taking a lock (I1; ROADMAP §10.3, F108).

A wake takes the scheduler lock, which ranks before device and serial locks. So code holding one of
those records the wake and performs it after dropping the lock, as §10.1's completion does, and a
top half wakes its bottom half with no device lock held; the rank check fails the other order on
every call.

They live in `src/cell.rs` (`BootCell`, `IrqCell`) and `src/sync_init.rs` (`SpinMutex`). Do not add another `UnsafeCell` + `unsafe impl<T> Sync` wrapper. Planned (ROADMAP §10.3, F017): `scripts/check_cells.py` reads each impl header whole and allows a generic `unsafe impl` of `Send` or `Sync` only in these two files and only with AGENTS.md rule 6's bounds; a concrete type that holds an `UnsafeCell` or a raw pointer may carry its own impl, whose `// SAFETY:` line names the invariant. `static mut` is only the asm-owned `vibeos_jmpbuf` in `arch/catch.rs`. Accessors do not return `&'static mut`.

Cross-CPU rule: a CPU never touches another CPU's run queue directly. Work is handed over through a
per-CPU inbox plus a reschedule IPI. More SMP-specific rules in [section 7.7](#77-locking-with-more-than-one-cpu).

## 2.4 Memory invariants

- The buddy takes only memory the boot memory map marks usable, less physical page 0, the AP
  trampoline page (§7.3), and the kernel image, framebuffers, and boot modules wherever they overlap
  usable memory. Limine keeps usable entries clear of every other entry, so the last three are
  defensive; `pmm_init` clips each usable range against all of them as it reads `BootInfo`, with no
  fixed-size list, so no exclusion is ever dropped. Rule; not yet enforced: `pmm_init` keeps an
  8-entry `Excludes` list, admits a range past the eighth with a `pmm: excludes overflow` line, and
  excludes the fixed page `0x8000` (ROADMAP §10.6).
- Buddy free list nodes live inside the free pages themselves. A stray write into freed memory
  corrupts the allocator, so guard pages on stacks are not optional. One stack has none: Limine's
  boot stack (at least 64 KiB, no guard page, in bootloader-reclaimable memory), which all of boot
  and, in `kernel_tests` builds, the in-guest test registry run on, so an overflow there corrupts
  memory silently (ROADMAP §10.6, F072).
- A PTE change that removes or narrows a translation takes effect only when every CPU that could hold
  the old translation has invalidated it and acknowledged. Such changes are unmapping, making a PTE
  not-present, read-only, or NX, and clearing its dirty bit. The rule covers kernel and user
  mappings, CPU TLBs and paging-structure caches, and, from ROADMAP §18.1, the IOTLB. On aarch64 the
  acknowledgement is the completion of the broadcast TLBI's `dsb ish`. Until then nothing relies on
  the change: no frame or page-table page is freed or reused, no virtual address is reused,
  `mprotect`, `munmap`, `mremap`, and `madvise(MADV_DONTNEED)` do not return, `fork` does not make
  the child runnable, and no page counts as clean. Clearing only the accessed bit, as LRU aging does,
  needs no completed invalidation: a stale accessed bit only misjudges how recently a page was used.
  Kernel mappings are `GLOBAL`, so every online CPU could hold them ([§7.9](#79-tlb-shootdown)).
  Rule; not yet enforced for user mappings: `addr_space_init::shootdown_user` invalidates only on the
  calling CPU, which is enough only while one thread owns each address space (I8; ROADMAP §12.3).
- MMIO pages are mapped uncacheable. QEMU tolerates write-back MMIO; real hardware does not. On
  aarch64 they are Device-nGnRE (ROADMAP §11.1), and a device access is ordered against Normal
  memory only by [§4.7](#47-dma)'s accessors.
- Every mapping is `NO_EXECUTE` unless it holds code that is fetched. Exception: the low identity
  window's first 2 MiB is executable, though only the trampoline page (§7.3) is fetched, and only
  during AP bring-up (ROADMAP §10.6, F085).
- A value copied to user memory has no padding and no uninitialized bytes. Reading a padding byte is
  undefined behaviour in Rust, and copying one out leaks kernel stack or heap. A typed copy-out takes
  only a type whose bytes are all initialized, proved at compile time (ROADMAP §10.6); a byte slice
  passes as it is. Holds today only because every copy-out takes a byte slice built by hand; nothing
  checks it until that box lands.
- A second-level translation of guest memory (an EPT or NPT entry on x86_64, a stage-2 entry on
  aarch64) is a mapping of the host frame behind it, since ROADMAP §21.2 backs guest RAM with the
  VMM's address space. Every change or removal of a user PTE (`munmap`, a COW write-protect or
  break, a permission reduction, a reverse-map unmap, a migration, `MADV_DONTNEED`) invalidates the
  second-level translations of that range on every CPU that may hold them before the frame's count
  drops. On x86_64 every vCPU of the VM flushes the VM's translations before it next enters the
  guest (`INVEPT` on VMX, a flush of the guest's ASID through the VMCB on SVM), a vCPU running in
  guest mode is kicked out first, and a vCPU that enters on a CPU other than the one it last ran on
  flushes there too. On aarch64 the host issues `TLBI IPAS2E1IS` for the range, `DSB ISH`,
  `TLBI VMALLE1IS`, and `DSB ISH` under the VM's VMID. A second-level fault reads the address
  space's invalidation sequence before it looks up the host PTE, and retries if an invalidation ran
  in between. A vhost worker reaches guest memory as a thread of the VMM's address space does: it
  holds a `users` reference ([§2.11](#211-object-lifetimes)) from the VMM's `VHOST_SET_OWNER` until
  its device is released, and it copies only through the ROADMAP §10.6 accessors, never through a
  frame or a kernel mapping it caches. This is Linux's `mmu_notifier` with KVM's invalidation
  sequence. Planned (ROADMAP §21.2): no hypervisor exists yet.

## 2.5 Panic policy

Binding order (do not invert):

1. Stop every other CPU first. Today `ipi_init::halt_others` sets `HALTING`, broadcasts the halt IPI
   `0xFE` (Fixed delivery), and returns without waiting. A CPU spinning with IF=0 does not take the
   IPI, and once `HALTING` is set its serial writes skip the TX lock, so it can write COM1 during the
   dump; a second panicking CPU re-runs `Serial::init` mid-dump. Planned (ROADMAP §10.7, F135; §11.3
   on aarch64): one stop primitive, which every path that stops the other CPUs uses, ROADMAP §25.4's
   capture jump included:
   - The first CPU into `begin_dump` claims the dump (the `DUMPING` swap) before it stops anyone. A
     CPU that finds the dump claimed by another CPU sets no request and runs the stop routine itself;
     the owner re-entering prints `vibeOS: panic: reentered` and halts, as today.
   - The owner sets `HALTING`, then for each other online CPU sets STOP in that CPU's request word
     and sends it the stop IPI (`0xFE`, Fixed, on x86_64; the stop SGI on aarch64). A CPU stops at
     the first of: taking the IPI; its next `service_incoming` poll, which reads the request word
     before any slot, so a CPU in a serviced spin (§2.3) stops with IF=0 and no interrupt, on either
     GIC version; its next serial write or log append; or the NMI below. The request stays set, so a
     CPU that reaches a poll late still stops.
   - After 100 ms of counter time the owner sends NMI to each CPU that has not acknowledged (the NMI
     IPI on x86_64; on aarch64 the GICv3 pseudo-NMI from ROADMAP §25.5, and nothing on GICv2) and
     waits 10 ms more. For each other online CPU it prints
     `vibeOS: panic: cpu N stopped (ipi|poll|nmi|panic)`, where `panic` names a CPU that stopped in
     its own `begin_dump`, or `vibeOS: panic: cpu N not stopped`. A CPU left `not stopped` loops
     without polling, which §2.9 rule 2 already makes a bug. Planned (ROADMAP §20.9): a CPU inside a
     firmware call is the exception, since SMM holds off even an NMI; while its firmware record
     ([§4.8](#48-firmware-runtime-services)) is set it is printed
     `vibeOS: panic: cpu N not stopped (in firmware)`, and every CPU found in firmware also gets a
     `vibeOS: panic: cpu N in firmware: <service>` line and a backtrace from the record's saved
     frame.
   - The stop routine saves its CPU's interrupted registers and frame pointer in a per-CPU
     crash-register slot, sets the CPU's `stopped` flag, acknowledges, and halts with every interrupt
     masked (on aarch64, a `wfi` loop with DAIF set). The dump prints each saved slot.
   - The NMI handler first reads and clears its CPU's request word. STOP runs the stop routine; on a
     CPU whose `stopped` flag is set the handler halts again and does nothing else; an NMI with no
     request on the dump owner returns at once, so the dump completes. Any other NMI with no request
     dumps and halts, as today, until ROADMAP §25.5 makes it an all-CPU backtrace.
   - Only the owner writes COM1. A4's raw serial layer (ROADMAP §10.3) holds `HALTING`, the owner's
     CPU id, and a write that takes no lock and no `InterruptGuard` and writes only on the owner;
     once `HALTING` is set, a serial write or log append on any other CPU runs the stop routine
     instead.

   Why one primitive: an IPI misses a CPU spinning with IF=0, and aarch64 has no NMI before ROADMAP
   §25.5 and none on GICv2, but the commonest such CPU, a waiter on a lock the panicking CPU holds,
   already polls `service_incoming` on every iteration. Rejected: an NMI-only stop, which leaves
   IRQ-masked aarch64 waiters running into the dump and the capture kernel; a separate stop path for
   the capture jump, a second implementation (AGENTS.md rule 10) under which only capture panics
   record the other CPUs' registers; keying the NMI handler on the global `HALTING` flag, under which
   any NMI halts the dumping CPU mid-dump; and dropping other CPUs' writes after `HALTING`, which
   leaves the writer running.
2. Re-initialize serial from scratch (the panic may be *in* the serial path).
3. Print location and message; dump registers, the current thread, and the last N log records.
   From ROADMAP §19.5, every record serial has not printed goes out first (the log contract below),
   unless a capture kernel is loaded (step 6), whose vmcore holds the ring.
4. Symbolized backtrace when frame pointers exist (in-image sorted table, binary search, no alloc).
5. Encode the panic record (ROADMAP §20.1) in a fixed buffer, and copy it to §20.1's reserved RAM
   region when one is configured. Memory stores only: no lock, no firmware call, nothing that waits.
   Planned (ROADMAP §20.1); today there is no record.
6. If a capture kernel is loaded (ROADMAP §25.4), jump to it. Before the jump the panicking CPU does
   only this: it takes §20.9's runtime-services lock and the ERST backend's lock (ROADMAP §25.6) each
   with a trylock and never releases either; it sets an armed watchdog (ROADMAP §20.6) to its longest
   timeout and feeds it once; and it writes pvpanic's crash-loaded event (bit 1), which a host
   records without stopping the guest, where the kernel found a pvpanic device (ROADMAP §10.7,
   §11.7) that lists that event. It flushes no log backlog, sends nothing over netconsole, and calls
   no firmware. The crash handover passes runtime services, and ERST, on to the capture kernel only
   where that lock's trylock succeeded, and names the CPU the capture kernel starts on and the
   physical address of step 5's record. The capture kernel writes the vmcore, feeding that watchdog
   after each chunk, so a dump that takes longer than the timeout survives and a capture that stops
   making progress is still reset; then it writes that record through each store its handover passed
   on (ROADMAP §25.6), and then resets through step 7's ACPI or PSCI path; where no store was
   passed, the copy in reserved RAM is the record. It never calls firmware its handover withheld,
   which a stopped CPU, or on GICv2 a CPU still running, may have been inside. Planned (ROADMAP
   §25.4, §25.6).
7. Otherwise, halt or reset. Today: a `cli; hlt` loop, or QEMU `isa-debug-exit` under the
   `panic_exit` test feature. Planned, in this order: send the dump over netconsole where one is
   configured (ROADMAP §25.6); write the record to an EFI variable or to ERST under that store's
   trylock (ROADMAP §25.6), last among the writes, since a firmware call has no time bound; write
   pvpanic's panicked event (bit 0) where the kernel found the device, after which the host may
   pause or end the guest; then, with `panic=<seconds>` (ROADMAP §22.2), wait and reset through the
   ACPI or PSCI path of the `reboot` call, and otherwise `cli; hlt`.

   Why two branches: a capture kernel exists to take the one complete dump, so nothing that can
   wait, or that lets a host stop the guest, runs before the jump. pvpanic's panicked event pauses
   the harness's QEMU (`-action panic=pause`) and lets a host's crash policy end the guest; its
   crash-loaded event does neither. Linux likewise runs no panic notifier before its crash jump
   unless `crash_kexec_post_notifiers` is set. Rejected: one linear order (record, pvpanic, jump,
   reset, halt), which pauses the guest and calls firmware before the capture kernel runs; an EFI
   write before the jump whenever the trylock succeeds, which puts an unbounded call ahead of the
   vmcore when the capture kernel can write the same record after it; and a switch like Linux's,
   which is two orders to specify and test.

No unwinding: `panic = "abort"`. Serial TX in this path is a bounded THRE poll; drop the byte on
timeout (see [§9.6](#96-hardware-polling)). Do not take SCHED. Allocate nothing, the panic record
(ROADMAP §20.1) included: the dump, the backtrace, and the record use fixed buffers and reserved
memory, since the panicking CPU or a stopped one may hold the heap lock. The log ring is readable
after the halt IPI without taking its TAS (force-unlock if the panicking CPU held it).

The panic record outlives the reset that follows a panic (ROADMAP §20.1, §25.6). Planned: every
store uses one format, a header holding a magic and a format version, a sequence number, and a
checksum, which every release of a major version reads (ROADMAP §22.1). Reserved RAM holds one
record; from ROADMAP §25.6 an EFI variable store or ERST holds at most two, the newest and the one
before, and an EFI record is at most 1 KiB. The kernel writes an EFI variable only when
`QueryVariableInfo` reports room for it with 5 KiB to spare, as Linux's x86 EFI code requires, since
some firmware stops booting with a full variable store; otherwise it writes the next store in line.
The next boot logs each record it finds into the kernel log and then deletes it, so a panic loop
cannot fill the store and ROADMAP §22.2's `BootNext` write keeps working. The retired-frame list
(ROADMAP §25.3) follows the same rules: one variable of at most 64 entries, applied only on the
machine and memory map it was written for.

Log ring (ROADMAP §5.5):

- 256 records of up to 96 message bytes; a wrap drops the oldest record and counts it, and the panic
  dump prints the count (`last N (M dropped)`).
- Compile-time maximum level: `trace` in debug builds, `debug` in release. Runtime filter: an
  `AtomicU8`, default `info`.
- The panic dump prints the last 24 records.
- The ring's lock (`log_init::LOG`, an `IrqCell`) is IRQ-off and outside the §2.1 rank order; never
  hold it across serial TX.
- `klog!` and formatted `Serial` writes keep IF off for the whole emit, so the per-CPU capture stage
  cannot interleave with a preempting thread.
- `dmesg` prints through a plain serial path that does not re-capture.
- Host tests cover overflow, filtering, and symbol lookup.
- One global IRQ-safe ring and a serial try-lock sink. The sink drops its copy of a record when
  another CPU holds the TX lock, and `log_fmt` drops a record its CPU emits while already inside
  `log_fmt`; neither drop is counted. Planned: a log record reaches serial whole (ROADMAP §10.2,
  F138); the log contract below (ROADMAP §19.5).

Planned (ROADMAP §19.5), the log contract:

- One store: a lockless ring of fixed-size slots, each with a state word. A writer takes the
  record's sequence number from one 64-bit counter with a `fetch_add`, claims slot `seq mod N` with
  a compare-and-swap on its state word, fills it, and commits it with a Release store; a writer that
  finds its slot still being written drops its record and counts it. A reader copies a slot and
  checks its state word before and after, as a seqlock reader does. So any context may append, NMI
  and `#MC` included, and the ring is readable after a panic with no lock. A record carries its
  sequence number, CPU id, level, and timestamp. Its timestamp is `now_ns`, which any context may
  read (§6.4).
- One printer thread per console prints records in sequence order, so an emitter never waits for
  the UART. Where records were overwritten or dropped before it reached them, it prints
  `vibeOS: log: N records dropped`, and `/dev/kmsg` returns `EPIPE` there (ROADMAP §13.9). Until a
  console's printer thread runs, records print synchronously as they are appended; after `HALTING`
  only the dump owner prints, synchronously, through the owner write (step 1).
- `marker!` writes its line to serial under one TX hold before it returns, then appends its record
  marked printed on serial. The mark is per console, so the framebuffer printer still prints it.
  Markers therefore keep program order among themselves whatever the printer does; a `klog!` record
  the serial printer has not reached yet can print after a later marker.
- An emitter in NMI or `#MC` context only appends and takes no console lock (§2.2); its records
  print when a printer reaches them.
- The panic dump first prints, through the owner write, every record serial has not printed, unless
  a capture kernel is loaded (step 6).

Why: per-CPU buffers have no total order without a timestamp merge, `/dev/kmsg` exposes one
sequence number per record, and ROADMAP §25.5 logs from NMI context, where the `IrqCell` ring panics
on re-entry. Linux's printk ended in the same shape: a lockless ring from 5.10, with its per-CPU
safe and NMI buffers removed in 5.15. Rejected: per-CPU staging drained by a printer, which has no
total order; printing every record synchronously, which at 115,200 baud (about 11.5 KB/s) stalls
every emitter; markers through the printer, which lose a marker whose CPU then hangs and make the
harness's order depend on scheduling; and Linux's variable-length descriptor and data rings, which
96-byte records do not need and which loom models less easily.

Symbols come from a two-pass link (Makefile `KERNEL_VARIANT`): `nm` output from the first link fills
an in-image `.rodata` table (`KSYMS`) that the second link builds in. Rule: every function has the same address
in both links. Not yet enforced: nothing compares them, and in the panic-test build the
reference to the filled table compiles larger than the empty one and shifts every later function, so
that ISO's table mis-names frames (ROADMAP §10.2, F084). Frame pointers come from
`-C force-frame-pointers=yes` in `.cargo/config.toml` (§3.1); there is no target JSON.

Rule: `vibeos-core` (`src/lib.rs`) does not panic on data; its parsers and table walks return the
module error. Enforced only for `unwrap`, `expect`, and `panic!`: clippy denies `unwrap_used`,
`expect_used`, and `panic` on that crate (allowed in `#[cfg(test)]`). Neither `indexing_slicing` nor
`arithmetic_side_effects` is enabled, and `make` ships the dev profile, whose `overflow-checks = true`
turns an arithmetic overflow into a panic. Planned (ROADMAP §10.1): both lints are denied in every
byte parser ROADMAP §10.2's fuzzers cover, vibefs v1 excepted until ROADMAP §14.8 retires it, and
the kernel binary denies `unwrap_used`, `expect_used`, `panic`, `unreachable`, `todo`, and
`unimplemented` crate-wide, where a site a kernel invariant bounds keeps an `#[allow]` that names the
invariant (§9.4). Crafted input panics portable code: a FAT BPB whose
`rsvd + num_fats * FATSz32` overflows in `parse_bpb` (ROADMAP §10.2, F064); a vibefs write near file
offset 2^44, which overflows `map_block` (ROADMAP §10.11, F008); a CRC-valid vibefs leaf whose count
exceeds the per-leaf maximum (F061; ROADMAP §14.8 retires v1 for a v2 that validates every block it reads); a vibefs truncate-grow that keeps `F_INLINE`
past 128 bytes (ROADMAP §13.9, F062). Panics in the kernel binary end in the binding order above.

Exceptions follow the per-vector table in [section 5.2](#52-idt-and-exceptions), whose Ring 0 column
names each ring-0 case that continues instead of halting. A kernel `#BP`
logs and continues. Planned (ROADMAP §17.4, §18.4): so do three ring-0 `#DB` cases, which §5.2's row
lists: a hit on a debug slot the current thread's tracer armed, a stray single step, and, in the
data-race detector's build, a hit on its own slots. Every other exception taken in ring 0 dumps and
halts in the same order as `#[panic_handler]`. Planned (ROADMAP §10.6): a `#PF` (on aarch64, a data
abort) at CPL 0 whose faulting instruction has an exception-table entry is not a kernel fault; only
the §5.1 user-memory accessors have entries, and §5.1 says how each kind ends. Rule: ring 3 never
halts the kernel. An exception raised by ring-3 code, or by a return to ring 3, sends that process
the signal §5.2 gives the vector (§11.5 the exception class, on aarch64), and the kernel keeps
running. The signal's action then applies, as on Linux: from ROADMAP §13.8 a handler may catch it,
from §17.4 a tracer sees it first, and a fault signal the process blocks or ignores still takes its
default action. The default action, the only one today, ends the process and prints
`user: pid N killed SIG<name>`. Not yet enforced: ring-3 `#DB`, and `#AC` when `CR0.AM` is set, halt
the kernel (ROADMAP §10.6, F005), and so do the entry-path windows of §5.10 (ROADMAP §10.6, F004,
F006, F007); §5.2's last column lists every vector whose ring-3 action differs from the rule. An NMI
dumps and halts on its IST stack; from ROADMAP §10.7 the NMI handler first reads its CPU's stop
request word (step 1).

Rule: nothing is silently swallowed. An error is returned to its caller, or handled where it arises
in one of three ways: a counter plus a log line at most once a second; an error state recorded on
the device, volume, or file, which a later call reports, as Linux's `errseq_t` does for writeback
errors; or a retry whose bound and give-up path its comment names. A result may be dropped only when
it carries no failure anyone could act on, such as cleanup after an earlier error that was already
returned, or the `fmt::Result` of a write to `Serial`, which cannot fail. Clippy's
`let_underscore_must_use` and `unused_result_ok` catch a dropped `#[must_use]` value, and a kept drop
carries `#[expect(clippy::let_underscore_must_use, reason = "...")]` naming the case above, so an
exemption whose drop goes away fails the build; `#[cfg(test)]` code and the `kernel_tests`-only
`ktest` module are exempt. An `if let Ok` with no `else`, and `let _ = f().ok()`, are review items,
since no lint sees them. Not yet enforced: neither lint runs, `let _ =` drops a `Result` in more than
40 files, and the kernel review found dropped errors that ROADMAP §10.2 (F080), §10.11 (F051, F063,
and the tmpfs readahead eviction), §10.12 (F115), and §13.9 (F124) fix; ROADMAP §10.1 lands the
lints and audits every site.

Hardware events are also lost in four cases. An exception before `idt::init` (PMM, the CR3 switch,
ACPI discovery, heap, KVA, GDT, PIC) goes to whatever IDT Limine left and resets or hangs with no
output (ROADMAP §11.1, F136). LINT1 is masked on every CPU and MADT NMI entries (types 3 and 4) are
not parsed, so a chipset or external NMI never reaches the NMI handler (ROADMAP §20.1, F096).
`CR4.MCE` and `CR0.NE` are clear on every CPU, so a machine check shuts the CPU down with no dump,
and an x87 floating-point error raises the masked IRQ13 and is lost (ROADMAP §10.6, F026). An
interrupt on a pool vector no handler owns is EOIed and ignored with no count (ROADMAP §10.6).

## 2.6 Serial markers

Every boot line is `vibeOS: <subsystem>: <state>`, lowercase, no punctuation at the end. Success
markers are asserted by the e2e harness in order. Adding a marker means updating the contract in
[section 8.3](#83-end-to-end) in the same commit.

The markers are a contract with the harness, not an interface for software outside the tree: ROADMAP
§39.1 classes them `internal`, so a release may change one, with section 8.3's contract in the same
commit.

`marker!` for contract lines (never filtered, always captured); `klog!` for everything else;
`PlainSerial` only for `dmesg` and panic dumps. `marker!` writes serial before it returns; from
ROADMAP §19.5 a `klog!` line reaches serial when a printer thread gets to it
([§2.5](#25-panic-policy)).

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

Every line the kernel writes to its console UART starts with the byte 0x1E (ASCII RS), which
terminals ignore: `marker!` and `klog!` lines, ktest verdicts, and the panic dump alike. The harness
takes only framed lines as the kernel's (§8.3), so no user byte may produce the frame. The kernel
writes one line per call, and a `\r`, `\n`, or 0x1E inside a line prints as `?`, so a string a user
chose (a path, a thread name) cannot start a line of its own. The console UART's user write path (the
console `write` today, the ROADMAP §13.7 serial TTY and its echo later) prints a 0x1E in user bytes
as `?`, and a record a user writes through `/dev/kmsg` prints unframed. Before a framed line, the
kernel writes a newline when the last byte on that UART was user output that did not end one. The
framebuffer console never draws the frame, and a UART that is not the console carries neither frame
nor escape. Rule; not yet enforced: nothing is framed, and the harness matches every line (ROADMAP
§10.2).

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
| I6 | Ring 3 never halts the kernel (§2.5, §5.2, §11.5) | `proc_init::try_user_fault` | documented | No: ring-3 `#DB`, and `#AC` when `CR0.AM` is set, halt (F005), and so do the I4 windows (ROADMAP §10.6) |
| I7 | The kernel reads or writes user memory only through the §5.1 user-memory accessors, and writes an address space that is not running only through the fill API (ROADMAP §10.6) | `addr_space.rs`; the arch accessors from ROADMAP §10.6 | enforced by SMAP where the CPU has it (PAN on aarch64, ROADMAP §11.6); the fill-API rule is documented | Partly: today's accessors copy through the physmap after `check_user_range`, and `write_bytes` ignores the PTE's `WRITABLE` bit (ROADMAP §10.6, F023) |
| I8 | One thread per address space changes its regions, and another CPU changes its page tables only under its page-table lock (§2.11) | process model | assumed | Yes: only the owning thread touches a space. Lock-free user copies, local-only `invlpg`, and `&'static AddressSpace` depend on it. ROADMAP §10.6 replaces `&'static` with a counted object, §12.1's reverse map changes page tables from other CPUs under the space's page-table lock, §12.3 shoots down every CPU in the space's set, and §13.1's threads bring the address-space lock |
| I9 | TCBs are never freed, so a `*mut Tcb` stays valid | 64-slot table, `thread_init` | assumed | Yes, but `spawn_inner` can reuse a Dead slot whose thread is still switching out (ROADMAP §10.10, F012) |
| I10 | A dead thread's stack is freed only after its CPU has switched off it (§2.8, §4.5) | `kva_init::DEFERRED` | documented | No: any CPU drains the global list (F012), and the 8-slot list panics when full (F010) (ROADMAP §10.10) |
| I11 | A completer's publishing store is its last access to the waiter (§2.8) | `block_init::IoWaiter` | documented | No: `IoWaiter::finish` runs `wake_all` after it stores `done` (ROADMAP §10.10, F002) |
| I12 | Every kernel PML4 slot exists before the first user address space | `AddressSpace::new` copies PML4[256..512) once | assumed | Yes, by boot order only: `paging_init::install` creates none of the heap, KVA, and `ioremap` PML4 slots; each appears on its region's first mapping, and no current path makes a first mapping after `/hello` (ROADMAP §12.1, F101) |
| I13 | The low identity window is removed after `smp: done` (§4.1) | none yet | documented | No: it stays mapped and GLOBAL, VA 0 included (ROADMAP §10.6, F085) |
| I14 | Every buddy frame and page table lies inside the physmap (§4.1) | `pmm_init::init`, `paging_init::physmap_extent` | enforced | Yes; a framebuffer above the 8 GiB cap is not covered (ROADMAP §11.2, F020) |
| I15 | Frame 0, the trampoline page, and the kernel image, framebuffers, and boot modules never enter the buddy (§2.4) | `pmm_init::init`; `Buddy::insert_region` skips frame 0 | enforced | Partly: the trampoline page is the fixed `0x8000`, which boot uses whatever the memory map says there (bootloader-reclaimable under SeaBIOS); `Excludes` holds 8 ranges, and a range past the 8th stays in the buddy with a `pmm: excludes overflow` line, which Limine's rule that usable entries overlap no other entry leaves unreachable (ROADMAP §10.6) |
| I16 | The kernel PML4 lies below 4 GiB, because the trampoline loads a 32-bit CR3 | `smp_init::start_one` | enforced by skipping every AP | Not guaranteed: the PML4 frame has no address limit, and above 4 GiB every AP is skipped with a `smp: cr3 above 4GiB` line (ROADMAP §20.1) |
| I17 | MMIO is UC, RAM is WB, and no frame has both (§2.4) | `acpi_init`, `Mapper::patch_physmap_uc` | documented | Partly: a whole 2 MiB leaf goes UC with no RAM check, and a trailing leaf can be skipped (ROADMAP §11.2, F104) |
| I18 | EOI before any switch; a one-shot timer is rearmed before yielding (§5.8) | timer ISRs | documented | Yes; no test tier runs the TSC-deadline timer, the only one-shot source, so nothing exercises the rearm (ROADMAP §10.1, F078) |
| I19 | I/O APIC high dword written before the low; IST index zero-based in software, one-based in the gate (§5.1, §5.6) | `apic.rs`, `desc.rs` | enforced, host-tested | Yes |
| I20 | `now_ns` is monotonic | seqlock plus `time::monotonic_max` over `time_init::LAST_NS` | enforced | Yes, by construction, so the monotonicity tests cannot fail (ROADMAP §10.2, F100) |
| I21 | A run queue is touched only by its owner CPU with IF=0 (§2.3) | `per_cpu_init::with_current` | enforced (busy flag) | Partly: only the owner writes it, but `diag::cpus_to` (the shell `cpus` command) and in-guest tests read another CPU's `runq` length with no lock, and `&'static PerCpu` aliases the `&mut` (ROADMAP §10.3, F039) |
| I22 | A `BootCell` is set once, before SMP, and holds `Sync` data (§2.3) | `cell.rs` | documented | No: `per_cpu_init::CPUS` holds the non-`Sync` `PerCpu`, which the unbounded `Sync` impl allows (ROADMAP §10.3, F017, F039); the set-once check is a `debug_assert!` (ROADMAP §10.2, F137); every switch writes the BSP's TSS after publication through a pointer cast from `&Bsp` (ROADMAP §10.3, F089) |
| I23 | The block layer orders only overlapping writes and a sequential zone's writes; a `Flush` makes durable every write completed before it was submitted, and a `Fua` write is durable when it completes (§10.2) | `block.rs` | documented | No: C-LOOK can reorder overlapping writes, a block-cache flush misses writeback already in flight, and `Fua` does not exist (ROADMAP §10.11, F043) |
| I24 | vibefs never overwrites a live block before the newer superblock is durable, and from v2 reuses a block a commit freed only after the next commit's superblock is durable, so the older slot's tree stays whole; a v2 NOCOW file's data blocks are the one exception, overwritten in place ([VIBEFS.md](VIBEFS.md) §15) | vibefs commit | documented | No after a failed commit: the in-memory generation advances before the superblock write, so the retry writes the slot that holds the only valid superblock (ROADMAP §12.5, F050). Otherwise it rests on v1's on-disk refcounts, which its mount does not check (F061); v2 keeps no per-block count and checks its pointers and allocation map as it reads each block (VIBEFS.md §15; ROADMAP §14.8) |
| I25 | Per-thread CPU state is saved and restored in full (§7.5) | `syscall_init::on_switch`, `thread::switch_context` | documented | No: `FS_BASE` is not switched (ROADMAP §11.6, F022); `fork` and `execve` get the FPU state wrong (ROADMAP §10.6, F069); no entry from ring 3 saves a complete user frame, so the user GPRs of a thread preempted in ring 3 are at no known place, and a context whose RCX and R11 differ from its RIP and RFLAGS cannot be returned to (ROADMAP §10.6) |
| I26 | Every kernel stack has a guard page (§2.4) | `kva_init::alloc_guarded_stack` | documented | No: boot runs on Limine's unguarded stack (ROADMAP §10.6, F072) |
| I27 | `vibeos-core` does not panic on data (§2.5) | clippy deny on `unwrap`, `expect`, `panic` | enforced in part | No: indexing and overflow checks panic on crafted input; §2.5 lists the cases and their ROADMAP lines |
| I28 | A line the harness takes as the kernel's is framed, and no user byte can produce the frame (§2.6) | `serial::Serial`, `console_init::write` | documented | No: nothing is framed, and the harness matches every line, so ring 3 can print a contract line (`/bin/sh` prints `shell ready`) or fail a run with `panicked at` (ROADMAP §10.2) |
| I29 | A catch hook intercepts only a CPL-0 fault on the CPU that armed it, inside an in-guest test's catch window | `arch::catch` | assumed | Partly: production never arms it, but `intercept` runs first in every exception handler of every build, and its armed state is global, so in a `kernel_tests` build a fault with the armed vector on any CPU, at any CPL, is caught (ROADMAP §10.2, F146) |
| I30 | Interrupt and exception handlers run with RFLAGS.AC=0 (§5.10 rule 5) | none | documented | No: the gates keep ring 3's AC (ROADMAP §10.6, F088) |
| I31 | Every IF=0 stretch outside §2.9 rule 2's exemptions retires at most 100,000 instructions ([§2.9](#29-preemption-and-interrupt-state) rule 2) | §2.9; ROADMAP §10.3's IF-off tracer | documented | No: syscall bodies run with IF=0 until they block, the in-guest test runner holds IF off for the whole run, and a console `write` scrolls the framebuffer once per newline with IF=0 (ROADMAP §10.6, F044; ROADMAP §10.2, F075); the heap's first-fit `alloc`, its address-ordered insertion on `dealloc`, and a moving `realloc`'s copy run under the IRQ-off HEAP lock over a free list whose length user churn sets (ROADMAP §12.6); the buddy's double-free check walks the free lists (ROADMAP §12.1, F029); a `klog!` emit waits on the UART with IF off, about 8 ms per 96-byte line on a 115200-baud 16550, which no QEMU tier paces (ROADMAP §19.5); ROADMAP §10.10 makes a shootdown survive a violation (F011) |
| I32 | A handler on an IST stack never blocks, switches threads, or takes a lock, and an IST vector taken at CPL 3 leaves the IST stack before its body runs (§5.10 rules 3 and 6) | the IST entry stubs and handlers | documented | Partly: every IST handler halts, so none blocks or switches, except that under `kernel_tests` an armed `catch` steps RIP and returns or longjmps off the IST stack; no IST entry leaves the IST stack yet (ROADMAP §10.6, F005, F007) |
| I33 | A fault body reads CR2, DR6, ESR, and FAR from its frame, where the entry stub saved them before IF could turn on (§5.10 rule 9) | the `arch/idt.rs` stubs; the aarch64 vectors (ROADMAP §11.3) | documented | Yes, only because every fault body runs with IF=0 and reads CR2 before anything else can fault (`arch::idt::page_fault`); ROADMAP §10.6's IF=1 bodies need the stub save (its syscall-body and generated-stub boxes) |
| I34 | A PTE change that removes or narrows a translation takes effect only after every CPU that could hold the old one has invalidated and acknowledged; until then no frame, table page, or VA is reused and no page counts as clean (§2.4) | `kva_init::unmap_shootdown` (kernel); `addr_space_init::shootdown_user` (user) | documented | Partly: kernel unmaps free frames and VA only after `wait_acks`; a user change invalidates only on the calling CPU, enough only while I8 holds, and nothing yet clears a dirty bit (ROADMAP §12.3) |
| I35 | A user PTE change invalidates the second-level translations (EPT, NPT, stage-2) of its range on every CPU that may hold them before the frame's count drops (§2.4) | none yet | documented | Not relied on yet: no hypervisor exists until ROADMAP §21.2, which lands it |
| I36 | `current` is read in one instruction, and every other per-CPU access but the CPU-id hint runs with IF=0 ([§2.9](#29-preemption-and-interrupt-state) rule 5) | `per_cpu_init`; the syscall stub's `gs:[current]` load | documented | Partly: the syscall stub reads `current` in one load, but `current_thread`, `current_id`, `current_pid`, and `per_cpu!` load `gs:[0]` and then the field, and `current()` hands out `&'static PerCpu` at any IF; no preempted thread changes CPU yet (ROADMAP §10.3, F039) |
| I37 | Nothing is silently swallowed: an error is returned to its caller, or handled where it arises by a counter and a rate-limited line, a recorded error state, or a bounded retry (§2.5) | every module; ROADMAP §10.1's lints | documented | No: nothing checks a discard, and the kernel review's dropped errors remain (ROADMAP §10.1 audit; §10.2, F080; §10.11, F051, F063; §10.12, F115; §13.9, F124) |
| I38 | A return to user mode restores only what the §5.10 rule 10 validator accepted from any writer of the saved frame, and its last check for pending work runs with IF=0 (§5.10 rule 11) | the validators in each port's pure half; the exit paths | documented | Rule 10 holds vacuously: no writer of a saved user context exists before ROADMAP §13.8 and §17.4. Rule 11 does not: pending signals are acted on only at syscall entry and after the `wait4` sleep (ROADMAP §10.6, F033) |
| I39 | On aarch64, an ASID a CPU has used since its last local TLB flush names one address space on that CPU ([§11.2](#112-address-space-on-aarch64)) | the ASID allocator (ROADMAP §11.2) | documented | Not relied on yet: the aarch64 port does not exist; ROADMAP §11.2's host tests and loom model enforce it when it lands |

## 2.8 Publish last

The rule for every completion, hand-off, and deferred reclaim:

1. In a completion or hand-off, the store that lets the other side return, free, or reuse an object
   is the publisher's last access to that object; after it the publisher touches only its own stack
   and statics. The other side can see the store on a lock-free path (`IoWaiter::poll`, any Acquire
   load of a done flag) and return before the publisher's next instruction, so a lock taken after
   the store does not make a later access safe. The store is a Release store, a Release
   read-modify-write, or the unlock of a lock that the other side takes before it frees or reuses
   the object, and the other side reads it with Acquire. Program order alone does not make a store
   the last access: on a weakly ordered CPU, a load or store the publisher made to the object
   earlier can still be performed after a Relaxed store is visible.
2. An object that its owning CPU may still use (the kernel stack it runs on, a TCB that is switching
   out, an AP's stacks and tables during bring-up, and the old kernel's code, tables, and stacks
   when a planned kexec hands its memory to the new kernel, ROADMAP §25.4) is freed only after that
   CPU has passed a point the reclaimer can observe: its `switch_context` away from the object has
   returned, or INIT has stopped the AP. A global list that any CPU drains does not meet this rule.

Rule; not yet enforced. The violations, and the ROADMAP lines that fix them:

- `IoWaiter::finish` stores `done`, then takes SCHED and runs `wake_all` on the `WaitQueue` inside
  the waiter, which lives on the submitter's stack (ROADMAP §10.10, F002).
- `thread_exit` puts its own stack on the global `kva_init::DEFERRED` list, and any CPU's
  `reap_zombies` can unmap it before the exiting CPU has finished `switch_context` (ROADMAP §10.10,
  F012).
- `spawn_inner` can reuse a Dead TCB slot while its thread is still switching out on another CPU
  (ROADMAP §10.10, F012).
- On the bring-up timeout, `smp_init::start_one` frees an AP's kernel stack, GDT/TSS, and IST and
  RSP0 stacks without an INIT, so an AP that is still running uses freed memory (ROADMAP §11.4,
  F032).

## 2.9 Preemption and interrupt state

IF here means this CPU's maskable-interrupt enable: RFLAGS.IF on x86_64, and PSTATE.I clear on
aarch64, where the kernel masks and unmasks F with I
([§11.5](#115-aarch64-exceptions-and-privilege-transitions)) and ROADMAP §25.5's pseudo-NMIs later
make `InterruptGuard` mask by priority instead. The rules below hold on both architectures.

The kernel is preemptible wherever IF=1. The timer tick and the reschedule IPI call
`schedule_preempt`, which may switch away from any thread whose `irq_nest` is 0: kernel threads,
syscall bodies, and fault handlers alike. Planned (ROADMAP §20.9): one exception, `efi_rt` inside a
firmware call, which runs with IF=1 and is not switched away from until the call returns
([§4.8](#48-firmware-runtime-services)). Turning interrupts off is how code says "not now", so every
IF=0 stretch has a reason from the list below and a bound.

1. IF=0 only in: an interrupt or exception entry or exit stub; a hard-IRQ top half (§2.2); a
   spinlock or `IrqCell` critical section (§2.3); an `InterruptGuard` section that must not be
   preempted or moved to another CPU: a per-CPU access (rule 5), a change to this CPU's registers
   that must match the running thread (FP state, `FS_BASE`), the [§7.9](#79-tlb-shootdown)
   shootdown wait, the [§7.6](#76-ipis) call-function wait, or the
   [§7.11](#711-cpu-offline-and-online) offline rendezvous; the scheduler's switch path; the
   return-to-user sequences of [§5.10](#510-privilege-transitions) rule 4, which begin at rule 11's
   last exit-work check; the panic and halt paths
   (§2.5); and a CPU's bring-up before its first `sti` (boot before `irq: enabled`, an AP before it
   enters idle).
2. An IF=0 stretch does a bounded amount of work: at most 100,000 instructions from the instruction
   that turns IF off to the one that turns it back on, tens of microseconds on a current core. The
   panic, halt, and bring-up paths of rule 1, the [§7.9](#79-tlb-shootdown) shootdown and
   call-function waits, and the time a spinlock acquire spends spinning, which the holders' own
   bounds limit, are exempt. No loop whose trip count a user, a device, or a disk image controls
   runs with IF=0, and nothing waits for another CPU with IF=0 without servicing incoming IPIs
   ([§7.9](#79-tlb-shootdown)). A TLB invalidation is such a wait on x86_64, so on both
   architectures it is called only where §7.9's calling contract allows. A long job holds its lock for one bounded chunk at a time and turns
   IF back on between chunks; a walk over a user address space's page tables holds the space's
   page-table lock for at most one leaf table (512 entries) at a time. ROADMAP §10.3's IF-off tracer
   measures every stretch, and the bound is checked under TCG with `-icount shift=0` on one CPU,
   where guest time advances 1 ns per instruction retired, so 100 µs of guest time is exactly the
   bound whatever the host's load. Rule; not yet enforced: ROADMAP §12.6 turns the check on, and I31
   lists the violations.
3. A syscall body runs with IF=1. After `swapgs`, the entry stub copies the user RSP from
   `PerCpu.syscall_scratch` into its frame on the thread's kernel stack, then runs `sti`; from there
   on the scratch belongs to whichever thread next enters on this CPU. The exit stub runs `cli`
   before it loads the user RSP (§5.10 rule 4), and keeps its state in the thread's user frame
   (§5.10), not in the scratch. A fault or trap taken at CPL 3 runs its body with IF=1 once its
   frame is on the thread's kernel stack with the fault's syndrome saved in it (§5.10 rule 9), and
   so does a `#PF` taken at CPL 0 inside a faulting user-memory accessor (§5.1) once ROADMAP §12.2
   lets it sleep, since the code it interrupted ran with IF=1; a fault inside a non-faulting
   accessor runs no body: the handler finds its exception-table entry before it touches IF and goes
   to the fixup; a hardware interrupt's top half keeps IF=0. Rule; not yet enforced: FMASK clears
   IF at `syscall` and nothing sets it again, so a syscall body runs with IF=0 until it blocks
   (ROADMAP §10.6).
4. Code that may sleep (waits on a wait queue, takes a sleeping lock (§2.1), allocates with
   reclaim (ROADMAP §12.6), or copies through a faulting user-memory accessor (§5.1) once ROADMAP
   §12.2 lets its fault sleep) runs with IF=1, no spinlock held, and outside any RCU read-side section
   ([§2.12](#212-rcu)), and asserts each in debug builds (ROADMAP §10.3; the read-side check from
   §19.5).
5. `current`, the running thread's TCB pointer, is read only through `arch::current_tcb()`: one
   instruction that preemption cannot split, whose answer the switch keeps right on every CPU. On
   x86_64 it is one `gs`-relative load of `PerCpu.current` (`mov reg, gs:[offset]`), never `gs:[0]`
   followed by a field load; on aarch64 it reads `SP_EL0`, which holds the running thread's TCB
   pointer whenever the CPU runs at EL1 (EL2 under VHE), as on Linux arm64 (ROADMAP §11.6).
   `arch::cpu_id_hint()` is the one other per-CPU read allowed with IF=1, for callers that tolerate
   an answer that is already stale, such as the log prefix and a queue choice: it loads the
   immutable `PerCpu.cpu_id` through this CPU's per-CPU base and returns the value, never a
   reference. Every other per-CPU access, taking a `&PerCpu` and indexing a per-CPU table by CPU id
   included, happens with IF=0 (`with_current`, `IrqCell`), and the reference does not outlive that
   stretch, because a preemption with IF=1 can move the thread to another CPU between the lookup and
   the use. Rule; not yet enforced: ROADMAP §10.3 (F039). `per_cpu_init::current_thread`,
   `thread_init::current_id` and `current_pid`, and the `per_cpu!` macro load `gs:[0]` and then the
   field, and `per_cpu_init::current()` and `try_current()` return a `&'static PerCpu` at any IF.
   The split read is latent while syscall bodies run with IF=0 (rule 3) and no preempted thread
   changes CPU; ROADMAP §10.6 makes syscall bodies preemptible, and §13.10's `sched_setaffinity`,
   §19.4's balancing, and §19.6's offlining move preempted threads.

Why this model: a syscall body that runs with IF=0 cannot acknowledge a TLB shootdown (F011) or
take the tick (F044), so every long syscall (`fork`'s copy, `execve`'s load, a large `read`)
would need its own IF-on window, and ROADMAP §12.2's fault path must sleep. Linux runs syscalls
with interrupts on and preempts wherever no lock is held, so its behaviour settles edge cases.
Rejected: keeping syscall bodies at IF=0 and adding a polling window to each long call, which is
how ROADMAP §10.6 first fixed console `write`; it has to be repeated in every call that loops and
it still starves the tick. The bound counts instructions, not time, so its check gives the same
answer on every run. Rejected: a time bound checked on the §10.1 KVM leg, whose hosted runner is
itself a VM that can deschedule a vCPU mid-stretch.

## 2.10 Trust boundaries

Whom the kernel trusts, and the ROADMAP line where each boundary hardens. Until ROADMAP §18.8's
`docs/THREAT_MODEL.md` exists, this table is the threat model, and that document grows from it.
"Untrusted" means the source may send any bytes, at any time, as often as it likes, and the kernel
must neither halt nor corrupt memory it has not given to that source (AGENTS.md rule 4).

| Principal | Trusted for | Can do today what a hardened kernel stops | Hardens in |
|---|---|---|---|
| Ring-3 code | Nothing: it must not halt or corrupt the kernel (I6) | Halt the kernel (F004 to F010); every process is root, so it can read any file and signal any process | ROADMAP §10.6 and §10.10 (halts), §13.9 (uids), §18.6 (capabilities, `seccomp`) |
| Disk images and partition tables | Nothing: a parse returns `Corrupt` | Panic the kernel with a crafted image or table that root mounts or attaches (F061, F064, F117) | ROADMAP §10.2 (FAT BPB), §13.9 (partition tables), §14.8 (vibefs v2 validates every block it reads; v1 is retired); §18.7 (a LUKS2 header is parsed by the initrd's unlock tool, never by the kernel) |
| Devices: config space, rings, registers, interrupts | Nothing for halts (rule 4); everything for DMA | Read or write any physical memory by DMA; forge an MSI, and so halt the kernel with an interrupt on a vector no handler owns (ROADMAP §10.6) | ROADMAP §10.6 (stray interrupts), §18.1 (IOMMU, interrupt remapping, used-ring checks, F048) |
| Firmware tables: ACPI, device tree, SMBIOS, the memory map | What they describe, but not their bounds: a malformed table is refused, never followed out of range | Halt boot with a malformed table before the IDT exists (F136) | ROADMAP §11.1 (early exceptions report themselves), §20.1 (table bounds) |
| The network | Nothing, from the first packet | Not reachable yet | ROADMAP §15.10 fuzzes every parser from the start; §15.4, §15.6, and §15.8 defend against off-path guessing (keyed sequence numbers, ports, and IP IDs, RFC 5961, SYN cookies, checked PMTU messages, randomized DNS ids), which §15.10's simulated attacker checks |
| Speculation and timing side channels | Out of scope: no KPTI and no Spectre or MDS mitigations; the kernel half is mapped in every user address space (F024, F025, F131, F132) | Read kernel and other processes' memory on an affected CPU | ROADMAP §18.3 |
| Limine, the firmware, the CPU, and, under a VM, the VMM that provides them | Everything | Not applicable | ROADMAP §18.7 measures and verifies the boot chain: the signed Limine binary, its enrolled configuration and command line, the kernel, the initrd, and a kernel `kexec_file_load` starts (§25.4). Each slot's root, the state partition, and the key-set root record on them are outside it (§18.7's owner decision) |
| The host running QEMU, the harness, and CI | Everything; they are the test oracle | Not applicable | Never |
| Agent sessions, and the accounts they act through | Writing code, and opening and merging pull requests through the repository's rulesets; nothing else: no release or phase tag, deployment approval, repository setting, ruleset, environment, or secret | Act as the owner on GitHub: agents run with the owner's account and token, so every owner-only control in the release chain (the `release` environment's approval, the tag rules, ROADMAP §22.5's setting) is one prompt injection away | The agent-boundary decision below, before ROADMAP §14.6's first key |
| Code under test: candidate commits, agent-written code and tools, guests, third-party build systems, as seen by the machines that run them | Nothing: on a CI runner or a rig VM it reaches no secret, credential, or host service beyond its job; on the dev host, no credential beyond the agent account's own | On the dev host, read every credential of the owner's account: the `gh` token, git's credentials, SSH keys, a signed-in browser | The agent-boundary decision below (dev host); ROADMAP §10.1 and §14.6 (release jobs); ROADMAP Funded goals, Self-hosted runners (rig) |

Consequence: until Phase 18 closes, vibeOS stops a process from crashing the kernel, not from reading
another process's data. README says not to run untrusted code on it or keep secrets on it.

**Interim posture (owner decision, 2026-09-23, [design review G006](reviews/DESIGN_REVIEWS.md)).** The
owner accepted the open gaps in the table above until the ROADMAP lines that close them, the last in
Phase 18: speculation side channels (with no KPTI, a user process on a Meltdown-affected Intel CPU,
bare metal or under KVM, can read all RAM through the physmap), DMA that no IOMMU confines (§18.1),
every process running as root (§13.9), and root-mounted crafted images that panic the kernel. Why: the
kernel has no users and no secrets; QEMU's TCG, the harness default, does not model the speculation
Meltdown needs, and under KVM the exposure depends on the host CPU; and ROADMAP §10.6 rewrites the
entry path as one generated stub per vector, which keeps a later KPTI CR3 switch local. Rejected:
moving KPTI and syscall-index masking (§18.3's F024 and F025 boxes) into Phase 13, which costs a slice
of entry-path work, a CR3 switch on every entry, and a measurable syscall slowdown on affected CPUs;
and moving all of §18.3 before Phase 14's `login`, which costs most of a phase ahead of the
self-hosting work.

The acceptance assumes one user. It goes back to the owner before `login` lands (ROADMAP §14.3), when
a second user can share the machine. Keeping any gap past the line that closes it, or adding a gap,
is likewise the owner's decision, not an agent's.

**Agent boundary.** Rule: agents never act with the owner's credentials, and nothing agents write
runs where the root key is made or used (ROADMAP §14.6). Rule; not yet enforced: agents run under the
owner's macOS account and GitHub identity, as the two rows above say. How the boundary is set up is
the owner's decision, in the block below; ROADMAP §14.6's custody box waits for the answer, so no key
exists before it.

> **OWNER DECISION NEEDED (review J002)**: agents act as you. They run shell commands under your
> macOS account and push, open, and merge pull requests with your GitHub token, and GitHub checks
> every owner-only step by account: the `release` environment's approval (H007), who may create
> release and phase tags, rulesets, and the private-reporting setting (ROADMAP §22.5). A prompt
> injection in an issue, a pull request comment, or a web page an agent reads could push a release
> tag and approve its own release run in your name. The root key, as ROADMAP §14.6 wrote the
> procedure, would be made on that same account with a tool agents wrote, so anything an agent runs
> could read it, and it is the one key an installed system cannot recover from.
>
> Recommended, at $0: (1) Agents get a GitHub machine account (GitHub's terms allow one beside your
> personal account) with this repository's Write role only: no Maintain or Admin role, no
> environment reviewer, no ruleset bypass. It uses a fine-grained token if GitHub issues one to a
> collaborator on a personal-account repository, and otherwise a classic token with the `repo` and
> `workflow` scopes, whose reach is that Write role; moving the repository to a free organization is
> the other way. None of your own GitHub credentials stays on the macOS account agents run under: not
> the `gh` token, git's keychain entries, SSH keys registered to your account, or a github.com
> session in a browser an agent tool can drive. You push tags and approve release runs from a second
> macOS account or GitHub Mobile. A GitHub App in place of the machine account gives the same
> boundary, with its private key where agents mint tokens. (2) The root key is made and used only in
> that second macOS account, which holds no agent tooling, with macOS's own `/usr/bin/ssh-keygen`,
> which System Integrity Protection keeps agents from changing; it is written passphrase-encrypted
> to removable media and used only while no agent session runs. Cost: two accounts, moving your
> credentials once, and commits and pull requests that show the agent account as their author.
>
> If you decline (1), the H007 record says that anyone holding your token, agent sessions included,
> can approve a release run, and AGENTS.md's maintainer-only steps stay policy that nothing
> enforces. If you decline (2), the root key is made where you choose, and THREAT_MODEL (ROADMAP
> §18.8) lists a root key on a machine agents use as a known escalation path. No key exists before
> `v0.14.0`, and ROADMAP §14.6's custody box waits for this answer.

## 2.11 Object lifetimes

How an object that more than one thread or CPU can reach is created, shared, and destroyed. §2.8
covers the last store of a hand-off; these rules cover the rest.

1. One owner, or a count. An object one thread uses is owned by it (a stack value, a `Box`). An
   object that more than one thread or CPU can reach (an address space, an open file description, an
   inode, a mount, a device instance, a pipe) is reference-counted, and its last put ends it
   (rule 3). A mount and the filesystem instance it shows, its superblock, are separate counted
   objects: several mounts (another namespace's copy, a bind mount, a second mount of the same
   device) can show one superblock, which lives while any mount, inode, or open file holds it. A
   process's working directory and root are counted references to a directory (a mount and a
   dentry), never path strings: a relative walk starts from the working directory and an absolute
   one from the root, `fork` copies both references, rename moves the dentry a reference holds, and
   `chroot` (ROADMAP §14.9) and `pivot_root` (ROADMAP §18.6) replace them. The count is `kalloc`'s
   `TryArc` (increment `Relaxed`, decrement `Release`, and an `Acquire` fence before the release, as
   `alloc::sync::Arc` does), a table slot's own count (rule 2), or a frame's count in
   ROADMAP §12.1's frame array. A count with other rules, such as a get-unless-zero count whose last
   put runs a teardown before the memory goes, rule 3's operation gate, or a per-CPU count, is
   written once as a shared type, beside `TryArc` in `kalloc` or beside `BlockingMutex` in `sync`,
   with host tests and a loom model (ROADMAP §10.8), and then reused; no subsystem writes its own
   (AGENTS.md rule 10). `&'static` refers only to what lives for the whole run: a static item, a
   string literal, the contents of a `BootCell`, or memory allocated at boot and never freed. It
   never refers to heap memory a table owns, and it is never built from a raw pointer
   (AGENTS.md rule 6).
2. Tables hold references or quiescent slots. A lookup structure (the process table, the TCB table,
   the dentry cache, the device registry) holds a counted reference, or a slot it reuses only once
   the object's count is zero and no CPU still runs on it or through it (a TCB's `on_cpu` flag,
   ROADMAP §10.10, which a tracer and the core-dump writer also wait on before they touch the
   thread's saved state, [§7.5](#75-per-cpu-data)).
3. Two teardowns. An object whose life ends when its users are done (an address space, a pipe, an
   unlinked inode, a mount after a lazy unmount) is released by the last put; when RCU readers can
   also find it, it is unpublished first, and its memory is freed a grace period after that put
   ([§2.12](#212-rcu)). A lookup structure that can still find it without holding a count loses it
   first: the release unpublishes it and waits for the lookups in progress, under that structure's
   lock or by rule 2's quiescent slot, before it frees anything. Until the last put, a reference
   still held keeps working. An object removed while references remain, because what backs it is
   gone or has been told to go (a device, a network interface), is killed in this order: unpublish
   it from every lookup structure, so no new reference can be taken; close its gate, the count of
   operations in progress that every operation through a reference enters and leaves, so that every
   new operation fails at once with the object's error, and wake every thread sleeping on the
   object, so that each one rechecks the gate and fails; wait only for the operations already inside
   (an active-operation count, or, for lockless readers, an RCU grace period, §2.12), each of which
   ends within the object's own bound, such as a request's deadline ([§10.3](#103-failure)); release
   what it holds (frames, vectors, DMA buffers, stopping the device first,
   [§5.4](#54-irq-registration)); and free its memory at the last put, whenever that comes, and a
   grace period later when RCU readers could find it (§2.12). No step waits for a reference to drop,
   or for a lock that an operation in progress needs in order to finish.
4. Ids are not pointers. A pid, tid, descriptor, or device id that crosses the syscall boundary or
   sits in a table is looked up on each use, never cached as a pointer. Pids and tids share one id
   space and one allocator, and a process's pid is the tid of its first thread. They are allocated
   in increasing order up to `pid_max` (ROADMAP §10.4 gives its value) and then wrap to 300, skipping
   every id in use, as Linux does with its `RESERVED_PIDS`, so a freed id is not handed out again at
   once. An id is in use while a process or thread, a process group, or a session carries it, as
   Linux keeps its `struct pid`. A
   pidfd (ROADMAP §23.1) refers to that id object, not to the number, so once the process is reaped
   it answers `ESRCH` and never reaches a later holder of the number.
5. Asynchronous work owns what it touches. A request, timer, work item, or completion that outlives
   the call that started it holds counted references to every object it will touch (ROADMAP §12.5's
   owned block submission). It drops each one after its last access to the object
   ([§2.8](#28-publish-last)) and outside any spinlock section: [§10.1](#101-completions)'s
   completer drops its waiter and buffer references after it leaves SCHED.
6. Release runs only where it may free and sleep. A last put runs the object's release, which frees
   memory and may sleep or take a sleeping lock. It runs in place only where direct reclaim may run:
   IF=1, this CPU's `HELD` rank mask empty, and a thread that is not a no-reclaim thread and is
   outside any RCU read-side section ([§4.4](#44-kernel-heap) rule 1). Anywhere else (a hard-IRQ top
   half, a timer or IPI callback, a spinlock section, a threaded bottom half or softirq-equivalent
   item, a writeback thread, a thread already in reclaim, an RCU read-side section), the put defers:
   it links the object onto this CPU's deferred-release list through a node the object's allocation
   carries, so it allocates nothing, and queues a work item that runs the release with IF=1.
   `TryArc`'s drop makes this check and defers by itself, because a completion or a timer cannot
   know that its put is the last and Rust drops values implicitly; `put_deferred` is the explicit
   form, for code that knows its put may be the last. A count whose release only returns memory to
   an allocator (a frame's, ROADMAP §12.1) follows that allocator's lock rank instead (§2.1). Any
   other count type says which rule it follows, and in debug builds its last put asserts that it may
   release where it is. Linux defers the same way (`fput` through `delayed_fput`, and
   `mmput_async`).

An address space has two counts, as Linux's `mm_users` and `mm_count`. `users` counts the threads
that run in it and a remote accessor's temporary pin (ptrace, `/proc/<pid>/mem`, `process_vm_readv`,
procfs `maps` and `smaps`). A pin is taken only by get-unless-zero, so it fails once the last thread
has left, and it lasts one access, so no remote reader keeps a dead process's memory. The last
`users` put runs the teardown. The core is a `TryArc` that each region and each `users` holder
references, and so does a CPU that keeps the root loaded after its thread leaves; it holds the root
table, the page-table lock, and the CPU set. Freeing the core frees the root and the struct and runs
no teardown. Teardown runs in one order over the whole space: zap every region's PTEs, keeping the
frames they drop until the shootdown completes (ROADMAP §12.3); unlink every region from its
reverse-map object, taking that object's lock for writing; free the page-table pages below the root,
clearing each parent entry under the space's page-table lock; then drop the regions and their core
references. `munmap` follows the same order for its range and frees only the table pages that no
remaining region covers. The reverse map is a lookup structure in rule 3's sense, and unlinking
under its write lock is rule 3's unpublish together with its wait for operations in progress. So a
reverse-map walker takes no count: it holds the object's reverse-map lock for reading while it
borrows each region's core and takes that core's page-table lock, and a region it can see still has
its core and its page tables. A walker that took `users` could drop the last one and run the
teardown inside direct reclaim ([§4.4](#44-kernel-heap) rule 2). Code running inside direct reclaim
or the OOM killer releases nothing: a put there that may be the last hands the release to a
workqueue worker. Rule; not yet enforced: an address space has one owner, its process-table slot,
and is reached through `&'static` references (ROADMAP §10.6, F019), and one global `PT` lock serves
every space until ROADMAP §12.1.

Why: the kernel review's CRITICAL and HIGH lifetime findings (F002, F012, F019) each came from an
object that one subsystem freed or reused while another could still reach it, under a scheme that
subsystem invented. A removal waits for the operations in progress and not for every reference,
because a mount or an open descriptor keeps its reference for as long as it likes; Linux kills a
block queue or a network device the same way. A release in atomic context is deferred, not
forbidden, because a completion or a timer cannot know that its put is the last. The check sits in
`TryArc`'s drop because Rust drops values implicitly, and it is §4.4 rule 1's test, so one predicate
decides where reclaim and release may run. Rejected: keeping one scheme per subsystem, which is how
those bugs arose; never freeing (today's TCBs), which aliases a dying object as soon as its slot is
reused; a removal that waits for the count to reach zero, which hangs behind any holder; and
deferral only at call sites marked by hand, which misses an implicit drop. RCU (§2.12) is kept for
lockless readers only, and it adds a grace period to their objects' counts rather than replacing
them; everything else uses counts alone.

Today the code breaks rules 1, 2, 4, and 5: TCBs are never freed and their slots are rewritten in
place (I9; ROADMAP §10.10, F012), address spaces are reached through `&'static` references built
from table-owned boxes (ROADMAP §10.6, F019), block completions point into stack frames (ROADMAP
§10.10, F002; §12.5, F042), a pid is the index of its process-table slot, handed out lowest first
(ROADMAP §10.4, F127), and a tid is its TCB slot index, in a space of its own. Nothing implements
rule 3's operation gate or rule 6's deferred release yet (ROADMAP §10.4). A process's working
directory is a path string: one kernel-global `file_init::CWD` serves every process, and `Proc::cwd`
is a buffer nothing reads (ROADMAP §10.4, F057).

## 2.12 RCU

Lockless readers (ROADMAP §19.5: the dentry cache, the mount table, the routing table) find objects
without a lock or a count, so an object they can reach is freed only after every reader that might
hold it has finished. RCU is how the kernel knows. vibeOS's RCU is preemptible, as Linux's is in a
fully preemptible kernel.

1. A read-side section increments a nesting count in the TCB on entry and decrements it on exit. A
   reader runs with IF=1 and may be preempted, but it never sleeps: it takes no sleeping lock
   (§2.1), waits on no queue, copies no user memory, and allocates only without reclaim
   ([§4.4](#44-kernel-heap) rule 1). Code running with IF=0 is a reader too, because a CPU with IF=0
   passes no quiescent state except at the switch point of a context switch.
2. A CPU passes a quiescent state at a context switch whose outgoing thread's count is zero, in its
   idle loop, on a return to user mode, and at a tick that interrupts a thread whose count is zero.
   A CPU in tickless idle or offline (ROADMAP §19.6) is quiescent throughout.
3. A thread switched out with a nonzero count joins the blocked-reader list and leaves it at its
   outermost exit, on whatever CPU it then runs. The list has its own spinlock, ranked after SCHED
   because the switch path takes it inside SCHED; the rank enters §2.1 with the code. It is one list
   until ROADMAP §27.5's tree gives each leaf its own. A grace period ends when every CPU has passed
   a quiescent state since it began and no reader that entered its section before it began is still
   on the list.
4. A preempted reader holds every grace period open until it runs again. A reader that has held the
   current grace period open past a bound is boosted through ROADMAP §19.4's priority inheritance to
   the priority of RCU's grace-period thread until its outermost exit, as Linux's RCU priority
   boosting does. Real-time threads above that priority can still hold a grace period open for as
   long as they run, as on Linux.
5. An object that RCU readers can find and that is counted (§2.11 rule 1) is freed only after it is
   unpublished and its count has reached zero, and then a grace period has passed. A reader takes a
   count only with get-unless-zero, an increment that fails when the count is already zero, so it
   never revives an object whose last reference is gone.
6. An RCU path walk never waits on I/O or a sleeping lock. On a dentry-cache miss, a mount it cannot
   pin, or a sequence count that changed under it, it takes counted references with get-unless-zero
   to the last dentry and mount it validated, leaves the read side, and continues as a reference
   walk under the sleeping locks, as Linux's `LOOKUP_RCU` walk falls back. When get-unless-zero
   fails, the walk restarts from its starting point as a reference walk.

Why: §2.9 has one way to say "not now", IF=0, and its rule 2 forbids an IF=0 path walk whose length
a user sets, so readers run with IF=1 and can be preempted. The nesting count does not disable
preemption; it only tells the grace-period machinery whom to wait for, so it is not a second "not
now". Linux's preemptible RCU has this shape, and its behaviour settles edge cases. Rejected:
classic RCU with IF=0 readers (rule 2); a preempt-disable count beside IF, which every per-CPU rule
would then have to account for; epoch-based reclamation, which scans every thread's epoch for each
grace period and stalls on a preempted reader the same way; and sleepable RCU alone, which has no
fast read side for the dentry cache.

Cost: the blocked-reader list and boosting make RCU larger than a classic implementation, and a
preempted reader lengthens grace periods, so memory waiting on them grows under load
([§4.4](#44-kernel-heap) rule 3 counts it as freeable).

Planned (ROADMAP §19.5): nothing uses RCU yet.

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
- User code never builds for a bare target. `x86_64-unknown-none` has the soft-float Rust ABI: an
  `f64` multiply compiles to a call to `__muldf3`, `f64` arguments pass in integer registers, and
  rustc warns that enabling SSE there breaks the target's ABI. A user program built for it would use
  no SSE and could not call C built by ROADMAP §14.1's clang, which passes `f64` in XMM registers,
  and `aarch64-unknown-none` differs again (hard-float, strict alignment). Planned (ROADMAP §10.5,
  §11.1): the `no_std` user runtime builds for `<arch>-unknown-linux-musl`, the triple `std` user
  code uses (ROADMAP §24.3), and links with `rust-lld` as a static `ET_EXEC` with no crt objects, so
  no host needs a C compiler for it.
- `build.rs` passes the linker script as an absolute `-T` so the link does not depend on cwd.
- Each kernel target has an ISA floor, and a CPU feature above it is used only where CPUID or an ID
  register reports it. x86_64 builds for x86-64-v1, the target's default CPU, and also needs NX,
  which `paging_init` sets in EFER without a CPUID check; SMEP, SMAP, UMIP, RDRAND, RDTSCP, and the
  TSC-deadline timer are each used only where CPUID reports them. Planned (ROADMAP §11.1): aarch64
  builds with `+lse` and needs FEAT_LSE and FEAT_PAN, both mandatory from Armv8.1, and a CPU without
  them is refused at boot with a named line before any code that needs them runs. User programs
  build for each architecture's Linux baseline (x86-64-v1, Armv8.0) and find anything newer through
  CPUID or `AT_HWCAP`.

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

Kernel command line. Planned (ROADMAP §10.2): the kernel reads a Linux-style command line, and its
option names follow the Linux-interfaces rule. An option Linux defines keeps Linux's name and meaning
(`root=`, `init=`, `ro`, `rw`, `console=`, `loglevel=`, `panic=`, `mitigations=`, `crashkernel=`). An
option only vibeOS defines is `vibeos.<name>=`, the `module.parameter` form Linux's parser gives a
module's options, so no later Linux option can take its name. `sysctl.<path>=` sets a sysctl vibeOS
implements, and an unknown path is logged and ignored, as on Linux. A word the kernel does not
recognize reaches init as Linux passes it: an undotted `name=value` into init's environment, any other
undotted word, and every word after `--`, as an argument; an unrecognized dotted word is dropped.
ROADMAP §10.2's parser lists each option here as it lands, with its ROADMAP §39.1 class: `internal`
for an option only the harness or a test sets, such as `vibeos.ktest=`, and `stable` or `unstable`
for the rest.

Planned (ROADMAP §18.7, §22.2): under Secure Boot the kernel command line is the `cmdline:` of the
Limine configuration enrolled into the signed Limine binary, which sets `editor_enabled: no`, so
neither the boot menu nor a file on the disk can change it. An installed slot names its root with
Linux's `root=PARTLABEL=vibeos-root-a` (or `-b`), which the kernel matches only on the disk Limine's
executable file response names and only on a partition of the vibeOS root type, refusing the boot
with a named reason when two match, so the configuration holds no per-install value. Under a VM, the
VMM can append options through fw_cfg (ROADMAP §10.2); the VMM is trusted for everything (§2.10),
and measured boot records the appended text in PCR 12.

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
| 9 | Kernel heap | `heap ok` | `alloc` becomes legal. Until `irq: enabled` (step 15) boot may use its infallible API; from then on every allocation is fallible ([§4.4](#44-kernel-heap)). |
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
  line before its driver halts, with no useful backtrace; ROADMAP §10.6 masks,
  counts, and logs it instead (§5.5).
- `smp: done` precedes `console ok`, `pci: N devices`, and `shell ready`. The e2e harness enforces
  it. If SMP moves after the shell, AP failures become invisible in CI.
- ACPI discovery for the step-8 UC patch may run immediately after CR3 (alongside `paging: mmio uc`).
  The `acpi: xsdt N tables` marker stays at step 12. Do not "fix" that by moving the walk after the
  heap: first touch of LAPIC/IOAPIC/HPET would then be cacheable.
- In the ROADMAP §12.1 KASAN build, `_start` maps the early shadow (§4.1) before step 1, since every
  instrumented function reads the shadow, the buddy at step 6 included.

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
default PIC handler still halts on an unexpected line (§5.5 gives ROADMAP §10.6's change). The timer path re-runs the
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

Planned (ROADMAP §25.4, §26.4): a boot without Limine starts in the image's direct entry
([§4.1](#41-virtual-address-map)), which runs before step 1 and enters the kernel in the state Limine
would leave it, with a `BootInfo` in place of Limine's responses. Step 2's base-revision check runs
only on a Limine boot; a kernel started by kexec checks the handover format's version there instead
(ROADMAP §25.4). It prints `vibeOS: boot: <path> entry ok` in place of `limine: rev <n> ok`
([section 8.3](#83-end-to-end)).

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
not rely on the optimizer. Planned (ROADMAP §10.2): a `make check` script bounds each function's
frame at a value recorded here. The bound is a screen for one oversized frame; §4.5's measured
budget is what bounds a whole path.

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
| Limine's HHDM offset +, inside the slot `0xFFFF_8000_0000_0000` – `0xFFFF_C000_0000_0000` | 64 TiB slot; today `map_end` ≤ 8 GiB, plus leaves added above it | Physmap, `virt = phys + ` the HHDM offset, discovered at boot (below the table); today the constant `HHDM_BASE`, which `boot::capture` asserts Limine's offset equals. 2 MiB pages up to `map_end`. Above it: 4 KiB leaves from `acpi_init::map_gap` (no cap), and write-back leaves for a display BAR0 from `paging_init::ensure_physmap_wb` (below `PHYSMAP_CAP`). The physmap never leaves its slot (below the table). |
| `0xFFFF_C000_0000_0000` – `0xFFFF_C000_0400_0000` | 64 MiB | Kernel heap. Starts at 1 MiB mapped and grows. Planned (ROADMAP §12.6): the region's size is set at boot from installed memory, up to the 16 TiB below the KVA region, so the heap can grow as far as RAM does ([§4.4](#44-kernel-heap)). |
| `0xFFFF_D000_0000_0000` – `0xFFFF_D010_0000_0000` | 64 GiB | Kernel VA allocator: guarded stacks, `vmap`, large transient mappings. |
| `0xFFFF_E000_0000_0000` – `0xFFFF_E000_1000_0000` | 256 MiB | `ioremap` window for device MMIO that should not be reached through the physmap. Today a bump allocator that never frees; planned (ROADMAP §20.1): §4.5's range allocator over its slot, with `iounmap`. Its slot ends at `0xFFFF_EA00_0000_0000` (10 TiB); ROADMAP §20.1 sizes the window inside it. |
| `0xFFFF_EA00_0000_0000` – `0xFFFF_EB00_0000_0000` | 1 TiB | Frame metadata (ROADMAP §12.1): one `Frame` per 4 KiB of physical memory up to 64 TiB, indexed by physical frame number, virtually contiguous, and populated one memory section at a time, so a hole costs page tables only. A const assertion holds `size_of::<Frame>()` to 64 bytes. |
| `0xFFFF_EC00_0000_0000` – `0xFFFF_FC00_0000_0000` | 16 TiB | KASAN shadow, in the ROADMAP §12.1 KASAN build only: one shadow byte per 8 bytes of the kernel half, at LLVM's x86_64 kernel-address offset `0xDFFF_FC00_0000_0000` (aarch64: §11.2). |
| `0xFFFF_FFFF_8000_0000` – `0xFFFF_FFFF_FFFF_FFFF` | 2 GiB | Kernel image. Matches the `kernel` code model so `.text` relocations fit in 32-bit displacements. |

Regions must not overlap and every one asserts that its range is unmapped before claiming it. This is
a real failure mode: two subsystems in the old tree were both designed at `0xFFFF_C000_*` and only one
noticed.

The KASAN build passes each architecture's shadow offset to LLVM explicitly
(`-Cllvm-args=-asan-mapping-offset=`), never LLVM's default: LLVM picks its Linux kernel offset only
for a Linux x86_64 triple, and for both kernel targets its default is a user-space offset whose
shadow of the kernel half is non-canonical. In that build every kernel address's shadow reads as
accessible from the first instruction: before any Rust runs, `_start` points the whole shadow range
at one read-only zero page through three shared table pages, in uninstrumented `arch` asm, as
Linux's early shadow does, and each direct entry (ROADMAP §25.4) does the same.
`paging_init::install` carries these entries into the kernel's tables, and since it gives every
kernel-half PML4 slot its own PDPT (the next paragraph), each PML4 slot of the heap's and the KVA
region's shadow has a PDPT of its own, so real shadow later changes no PML4 entry (I12). From
`kva: ready`, heap growth and every KVA map allocate and map their own shadow in the same operation,
under the locks that operation already takes, and fail with `ENOMEM` when they cannot; a KVA free
poisons its shadow and unmaps it after the range's shootdown. The heap region's shadow grows with
the heap. The physmap and the image keep the zero page, so their accesses are not checked.

Every kernel-half PML4 slot exists before the first user address space (I12): `AddressSpace::new`
copies PML4[256..512) once, so a slot added later is missing from every address space made before
it. Planned (ROADMAP §12.1): on x86_64 `paging_init::install` allocates all 256 kernel-half PDPTs,
1 MiB, so no region, randomized base, shadow, or hot-added range ever needs a PML4-level table, and
the kernel mapper panics if it would change a kernel-half PML4 entry once an `AddressSpace` exists.
aarch64 needs no counterpart, since no address space copies TTBR1's tables.

The physmap base is the one region the kernel discovers. It is Limine's HHDM offset, which the Limine
protocol says "may vary between boots, including for randomisation", and which an executable "must
not assume". The kernel adopts that offset as its own physmap base, so a physical address has the
same alias before and after its `mov cr3`, and reads it once into `BootInfo`; every physical-to-virtual
translation uses that one value. `boot::capture` checks that the HHDM offset lies in the physmap's
slot (below) and halts with a named reason if it does not. Rule; not yet enforced:
`paging_init::HHDM_BASE` is a constant, and `boot::capture` asserts that Limine's offset equals it
(ROADMAP §11.1). ROADMAP §18.2 later draws the other bases from entropy too.

Planned (ROADMAP §25.4, §26.4): the kernel base and the HHDM offset have two sources, Limine's
responses and the image's own direct entry, which every boot without Limine takes. The direct entry,
one per architecture, draws both inside this table's slots, applies the image's relocations, builds
the tables the kernel starts on, and records the base, the offset, and a seed for the other bases in
`BootInfo`, where `boot::capture` reads them on every path. A loader that starts another kernel,
kexec's included, places bytes and writes a serialized `BootInfo`, and draws no layout: it runs the
release before, which does not know the next release's table. So this table may change between
releases without breaking an update reboot (ROADMAP §30.4). Rejected: the loader drawing the layout,
which freezes the old release's table into every later kernel.

The physmap's slot is `0xFFFF_8000_0000_0000` – `0xFFFF_C000_0000_0000` on x86_64 (aarch64: §11.2).
Limine's `randomise_hhdm_base` (ROADMAP §18.2) raises the offset above the slot's base by a draw in
1 GiB steps below 2^(VA bits − 3), 32 TiB with 48-bit VAs, so RAM that ends at or below 32 TiB fits
under every draw and RAM up to 64 TiB fits as the draw allows. Planned (ROADMAP §11.1, §11.2): the
physmap builder leaves a RAM-typed range whose alias would pass the slot's end out of the physmap and
the buddy, with the registered marker `vibeOS: pmm: <n> MiB past the physmap slot ignored`, as Linux
drops RAM past `MAXMEM`, and hot-added RAM past it is refused the same way (ROADMAP §27.6). One
`vibeos-core` function answers whether a physical range lies in the physmap; the builder and every
`HhdmPhys` translation use it (ROADMAP §20.1, F136).

The low identity window exists for one reason: an AP starting from SIPI runs in real mode and then
32-bit protected mode in the trampoline page below 1 MiB (§7.3), so that page must be identity
mapped and executable. All 512 MiB stay mapped and GLOBAL for the life of the kernel CR3, so a
NULL-plus-offset access from a kernel thread reads low RAM instead of faulting, and buddy frames
below 2 MiB have a supervisor writable, executable alias. Planned (ROADMAP §10.6, F085): the window
is torn down after `smp: done`, keeping only the trampoline page (4 KiB, read-only, executable, not
global).

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
reclaimable, ACPI NVS), inside its slot, and nothing else. It maps the kernel image's physical span
at 4 KiB on both architectures, since ROADMAP §18.1 gives that span per-section permissions and a
live aarch64 block is never split (ROADMAP §11.2); every other range takes the largest page its
alignment allows. After boot the physmap changes only in ROADMAP §12.1's `debug_mm` build, which maps
it at 4 KiB and unmaps or remaps a frame by one atomic exchange of its existing leaf. Device MMIO is
reached only through `ioremap`. Memory the kernel does not own that is not device MMIO (a firmware
table or an AML `SystemMemory` region outside the RAM-typed ranges, a framebuffer, and a capture
kernel's view of the crashed kernel's RAM, ROADMAP §25.4) is reached through `memremap`, which maps
the range in the KVA region through §4.5's allocator with the memory type the range needs: write-back
on x86_64, where MTRRs keep device memory uncached whatever the page attribute says; on aarch64 the
EFI memory map's attribute for the range (ROADMAP §20.9), and Device through `ioremap` where no map
describes it, as Linux's `acpi_os_ioremap` does; for a framebuffer, the type ROADMAP §11.1 gives its
location; for the crashed kernel's RAM, write-back. `memunmap` frees the range after §4.5's
shootdown. A firmware region described as terabytes of MMIO then costs nothing, which
removes the reason for the cap, so the cap goes and RAM above 8 GiB joins the buddy. Rejected:
keeping x86_64's whole-range physmap with in-place UC patches beside aarch64's RAM-only one, which
would leave the portable page-table code two policies for one primitive (AGENTS.md rule 10) and keeps
the UC-alias bug class (F104) alive. Rejected: a physmap leaf added when a firmware table is first
read, which allocates page tables at run time under `PT`, is always write-back, and aliases another
region for an address past the slot.

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
- the AP trampoline page, `0x8000` today (§7.3)
- the framebuffer
- anything not marked `USABLE`, including bootloader and ACPI reclaimable
- anything above the 8 GiB physmap cap (§4.1)

Planned (ROADMAP §10.6): the exclusions are §2.4's, clipped from each usable range as `BootInfo` is
read, with no fixed-size list.

`stats().free_frames` is a running counter, so `meminfo` costs O(1). `deallocate` does not: its
double-free check and its buddy lookup walk the free lists, O(`MAX_ORDER` × list length) per call, so
tearing down a large address space holds PT with IRQs off for that long (ROADMAP §12.1, F029).

Planned (ROADMAP §12.1): physical memory is counted in sections of 128 MiB (2^27 bytes, 32,768
frames) on both architectures, as on Linux x86_64 and arm64 with 4 KiB pages. At 64 bytes per frame
([§4.6](#46-what-comes-later)'s frame model), one section's metadata is exactly one 2 MiB leaf, and no
buddy block (at most 4 MiB, `MAX_ORDER` 10) or its buddy crosses a section boundary.

Planned (ROADMAP §27.3): a section joins its node's buddy on demand rather than at boot, so the
initialization above runs a section at a time and boot touches only the memory it uses.

- Metadata. Before the buddy is built, boot reserves each node's frame metadata from that node's own
  RAM, one 2 MiB-aligned run per section that holds RAM, packed from the top of the node's RAM
  downward, and maps all of it then, with 2 MiB leaves, in a frame-metadata region §4.1 reserves.
  Nothing writes it until its section joins, so the reservation touches no memory, a join writes no
  page table, and low memory and long free runs stay for DMA ([§4.7](#47-dma)) and huge pages. The
  runs are recorded per node, and a read of a frame's metadata (ROADMAP §12.1) reports a frame in
  them as kernel memory whatever its section's state. Hot-added memory (ROADMAP §27.6) takes its
  metadata from its own first 2 MiB and maps it under PT from a context that may sleep.
- Who joins. Before `irq: enabled`, boot joins the sections its allocations reach, as bring-up
  ([§2.9](#29-preemption-and-interrupt-state) rule 1). After it, an allocation outside §4.4's atomic
  class that would take free memory below the reserve joins the next section of a node it may use
  and retries (§4.4). An atomic-class allocation never joins: the background reclaim thread
  (ROADMAP §19.10), one per node, joins a section of its node before it reclaims anything once free
  memory falls below the low watermark. A join takes no sleeping lock and allocates nothing, so a
  no-reclaim thread may join without recursing into reclaim.
- How. One compare-and-swap of the section's state, from not joined to joining, gives one caller
  the join; a caller that finds only sections being joined waits for one of those joins to finish.
  The joiner writes the section's `Frame` entries with IF=1 and no lock held, every frame marked in
  use, and marks the section joined, after which reads of the section's metadata use its entries.
  Then it frees the section's RAM into the buddy a bounded chunk per buddy-lock hold
  ([§2.9](#29-preemption-and-interrupt-state) rule 2), skipping the metadata runs and the frames
  ROADMAP §25.3 has retired, and wakes any waiter. No free bit is set before its block is on a free
  list, so a merge never meets a half-joined buddy, and a join takes no lock but BUDDY and writes no
  page-table entry.
- Counting. The reserve and the watermarks (§4.4) count joined free frames, and R is sized from all
  RAM, joined or not. `meminfo` counts the frames of sections not yet joined as free, except those
  in metadata runs.

Rejected: joining inside the buddy under its lock, which takes PT under BUDDY against §2.1, calls up
from the buddy against §1.1 constraint 6, and writes 2 MiB of metadata with IF=0; carving each
section's metadata from the section itself, which leaves a metadata block inside every section,
needs a boot pool for sections with holes, and writes a kernel-half leaf outside PT at run time;
Linux's deferred-init threads, which join everything during boot and so write 16 GiB of metadata in
a 1 TiB guest; a joiner thread per node beside the background reclaim thread, which is a second
thread and a second threshold for one job.

Ownership. `PhysAddr` is an address: `Copy`, and it owns nothing. It carries PTE contents, DMA
addresses, and arithmetic. What owns free-list memory is `Frames`, a base and an order with private
fields, neither `Copy` nor `Clone`, and `#[must_use]`. Only `Buddy::alloc(order)` and
`Buddy::alloc_constrained` build one, and §4.6's frame metadata does when a unit's last count drops.
`Buddy::free(Frames)` is a safe fn, and `deallocate(PhysAddr, order)` is private to the `pmm` module.
Dropping a `Frames` never frees it: a free on drop would take BUDDY at whatever rank the drop site
holds, which §2.1 forbids under HEAP, SCHED, or DEVICE, and could free a frame before its TLB
invalidation. A dropped `Frames` leaks, `meminfo` counts it as leaked, and a debug build panics
naming the allocation site. `GuardedStack`, `DmaBuffer`, the `vmap` handle, and heap growth hold the
`Frames` they were built from. A frame that a page-table entry maps, a user leaf or a table page, is
consumed into that entry, which is its owner record until §4.6's frame metadata exists, and only the
page-table code that removes the entry takes it back, through an `unsafe fn` whose safety comment
names the entry. Rule; not yet enforced: the API above hands out and takes back `PhysAddr`, so safe
code can free a frame it does not own (ROADMAP §10.3, F018).

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
memory, and this patch and `map_gap` are deleted (§4.1).

### PTE flag policy

| Mapping | Flags |
|---------|-------|
| Kernel `.text` | present, global, read-only, executable |
| Kernel `.rodata` | present, global, read-only, NX |
| Kernel `.data` / `.bss` | present, global, writable, NX |
| Physmap | present, global, writable, NX |
| Heap | present, global, writable, NX |
| Kernel stacks | present, global, writable, NX, guard unmapped below (§4.5) |
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
  before the VA or the frame behind it can be reused (§2.4). See [section 7.9](#79-tlb-shootdown).
- A kernel-half edit runs a local `invlpg`, drops PT, then calls `paging::tlb_shootdown_others(va)`,
  a hook that `ipi_init::init` points at `ipi_init::shootdown_va` before the first AP starts (§7.9).
  Host tests, and boot before `ipi_init::init`, leave the hook unset; the local `invlpg` is enough
  there.
- Frames and page-table pages that a PTE change drops go into a per-operation gather, Linux's
  `mmu_gather` shape. The gather owns them (`Frames` or `FrameRef`, §4.2, §4.6) and releases them
  only after the invalidation round that covers the change completes. It holds at most 64 units, in
  storage on the operation's kernel stack, and never allocates. When it is full, the walk records
  its position as a virtual address, drops the page-table lock, completes a round for the units it
  holds (with §7.9's freed-tables flag when a page-table page is among them), releases them, and goes
  on from its position, rereading each PTE there. So an operation sends one round for every 64 units
  it drops, and freeing memory never needs memory (§4.4). Planned (ROADMAP §12.3): user unmaps use
  it; `kva_init::unmap_shootdown` already frees its frames after `wait_acks`.
- A page's dirty state lives in its PTEs as well as in the page. A page counts as clean only after
  every PTE that maps it has been write-protected or had its dirty bit cleared, each old dirty bit
  has been read by atomic exchange through the ROADMAP §10.3 page-table seam and folded into the
  page, and the invalidation has completed (§2.4). Writeback then writes it, and a later store either
  faults on the write-protected PTE or sets the dirty bit again. Reclaim harvests dirty bits the same
  way before it decides (§4.4). The MMU sets accessed and dirty bits in live entries: always on
  x86_64, and on aarch64 when ROADMAP §12.2 sets `TCR_EL1.HA` and `HD`, where a write through a leaf
  with `DBM` set clears its AP[2]. So every software store to a live PTE is an atomic exchange or
  compare-and-swap, and an operation that removes write permission or the mapping (`mprotect`,
  `munmap`, reclaim) folds the old dirty bit into the page as cleaning does. `DBM` is set only on a
  user leaf its process may write and that is not COW-shared, never on a kernel leaf. Planned (ROADMAP
  §12.2, §12.4, §12.6).
- Write-notify. A writable `MAP_SHARED` mapping of a file whose pages are written back to a device
  (a vibefs or FAT file, or a block device) maps a clean page read-only on both architectures,
  whatever the hardware's dirty management. The first store faults. The fault takes the page busy,
  waits for any writeback of the page when the file's data is checksummed, reserves the page's
  space with its filesystem, marks the page dirty and counts it against ROADMAP §12.5's dirty limit,
  updates the file's mtime and ctime, and only then makes the PTE writable. This is Linux's
  `page_mkwrite` path. None of these steps waits while the fault holds the address-space lock
  (§2.1): one that must wait, such as a reservation that waits for a commit to free space, runs
  after the lock is dropped, and the fault restarts. A failed reservation ends a user store with
  `SIGBUS` (§5.2) and a store inside a user-memory accessor with `EFAULT`. Cleaning such a page
  write-protects each PTE that maps it, never only clears its dirty bit, so the next store faults
  again. A shared mapping of tmpfs, which backs anonymous shared memory (§4.6), keeps dirty bits
  instead, charges tmpfs's size limit when the fault allocates a page, and never counts against the
  dirty limit, since nothing writes it back before swap; Linux leaves shmem out of write-notify for
  the same reason. Planned (ROADMAP §12.2, §12.4).

## 4.4 Kernel heap

A free-list heap at `HEAP_START`, backed by buddy frames mapped writable + NX. Initial mapping is
1 MiB; the allocator grows in page-sized increments up to the 64 MiB region limit. It never returns
a page to the buddy, so it reports the pages it holds and its bytes in use apart, and a leak check
reads the bytes in use (ROADMAP §12.1). `GlobalAlloc` takes the heap lock, a `SpinMutex`, so
interrupts are off for each `alloc` and `dealloc`, and the rank check refuses an allocation or a
free made while a spinlock ranked after the heap is held (§2.1). Planned (ROADMAP §12.6): the free
list becomes a two-level segregated-fit allocator (TLSF), whose `alloc` and `free` take constant
time with boundary-tag coalescing, and a moving `realloc` copies with the heap lock dropped, so
every HEAP hold is bounded however fragmented the heap is
([§2.9](#29-preemption-and-interrupt-state) rule 2). Today `alloc` walks the address-ordered free
list first-fit, `dealloc` walks it to insert, and `realloc` copies under the lock.

Planned (ROADMAP §12.6): the heap region is sized at boot from installed memory, so a heap allocation
fails only when frames run out. The two limits must be one because the failure policy below treats
every failure as a shortage of memory. A fixed region smaller than RAM would fail allocations while
frames are free, and reclaim and the OOM killer would then kill processes to make room that
memory already had.

Allocation failure has two policies, chosen by when it happens:

- After `irq: enabled`, allocation is fallible on every path, not only on those that untrusted input
  reaches (a syscall, device data, a disk image, a network packet): through `vibeos::kalloc`'s
  owning types, whose failure becomes `ENOMEM` (or the errno Linux returns there, such as `EAGAIN`
  from `fork`). Where the context may sleep ([§2.9](#29-preemption-and-interrupt-state) rule 4),
  ROADMAP §12.6's direct reclaim and OOM killer run before the allocation reports failure. A user
  who exhausts memory gets an errno or the OOM killer's verdict, never a kernel halt. A bound on an
  allocation's size and count does not make its failure a broken invariant: any process can exhaust
  memory first, and the heap fails when frames do (ROADMAP §12.6), so after boot a failure means
  memory is short.
- Before `irq: enabled`, while no process exists and no device is bound, the infallible `alloc` API
  (`Box::new`, `Vec::push`, `vec!`, `format!`, `String` growth, `Arc::new`) is allowed for a size
  that no device or disk image supplies. Its failure reaches `#[alloc_error_handler]`, which panics
  with the requested layout: there, a failure means the machine has too little memory to boot or a
  kernel invariant is false, and a halt that names the layout says which. Reaching the handler after
  `irq: enabled` means an infallible call escaped the lints below.

Fallibility is carried by type, not by a list of methods. The infallible surface of `alloc` is
large: `BTreeMap::insert`, `extend`, `collect`, `clone`, `to_vec`, `String::from`, and every other
growing call. On stable Rust, which `vibeos-core` uses (§1.1), `Box`, `Arc`, and `BTreeMap` have no
fallible constructor at all. So `vibeos-core` has a `kalloc` module of owning types (`TryBox`,
`TryVec`, `TryString`, `TryArc`, and an ordered map) whose every growing operation returns
`Result`. They are built on stable Rust: `alloc::alloc::alloc` with a null check for boxes, and
`try_reserve` for vectors. Clippy's `disallowed-types` denies `alloc`'s owning types (`Box`, `Vec`,
`String`, `Arc`, `Rc`, and the `alloc::collections` types) in both crates outside `kalloc`, and
`disallowed-macros` denies `vec!` and `format!`. A boot-time site that keeps an `alloc` type carries
an `#[allow]` whose comment names the boot step that runs it, and what it builds does not grow after
`irq: enabled`. This is the shape Rust-for-Linux settled on (`KBox`, `KVec`) after starting from
`alloc`'s collections. Rejected: a `disallowed-methods` list of infallible constructors, which
misses the calls it does not name and leaves `Box` and `Arc` with no fallible path on stable Rust.

Rule; not yet enforced: syscall paths, driver probes (`virtio_blk_init`'s `Box::new`), and
kernel-thread creation use the infallible API today, and a `fork` near exhaustion panics in
`spawn_inner` (F010). ROADMAP §10.4 lands `kalloc` and the lints. Rejected: making small allocations never fail by having the allocator wait
until the OOM killer frees memory (Linux's "too small to fail"), because an allocation made with a
spinlock held, or on a path the OOM victim needs in order to exit, cannot wait, and a failed
`Box::new` cannot be handled by its caller; AGENTS.md rule 4 forbids a user-triggerable panic.

Planned (ROADMAP §12.6): one allocation entry applies the rules below. It is the `#[global_allocator]`
that `kalloc`'s types allocate through and the frame entry that the fault path, page tables, kernel
stacks, and DMA use. It is a module of its own, not `heap_init` or `pmm_init`: it calls down into
the heap and the buddy, which report failure and never reclaim, wait for memory, or call up beyond
the heap's §4.3 shootdown when it grows, and it reaches reclaim, the writeback wait, and the OOM
killer through the hooks §1.2 lists. Where §27.3's joining applies, it joins a memory section before
it goes below the reserve (§4.2). Today the `#[global_allocator]` is `heap_init::KernelAlloc`, and
`kva_init`, `dma_init`, and `addr_space_init` take frames from `pmm_init::with_buddy` directly.

Direct reclaim (ROADMAP §12.6) runs inside an allocation that failed, on the allocating thread, so it
must need nothing that thread might hold. The thread may hold a filesystem's inode or block-mapping
lock, its own address-space lock, or a busy page-cache page (§2.1's sleeping tier). Rules:

1. Direct reclaim runs only for an allocation that began with IF=1, this CPU's `HELD` rank mask
   empty, and a calling thread that is not a no-reclaim thread and is outside any RCU read-side
   section ([§2.12](#212-rcu)). The allocation entry reads all four at entry, before it takes the heap
   lock. Any other allocation draws on the reserve below, as deep as its class allows, and then
   fails.
2. It frees clean pages, and from ROADMAP §19.9 on the unused slab objects that a shrinker meeting
   this rule gives up, and nothing else. It drops clean page-cache pages that nothing maps. It
   unmaps a mapped one through the reverse map, taking only each address space's page-table spinlock
   and try-locks of the page and of its reverse-map lock (§4.6), and no count on the space
   ([§2.11](#211-object-lifetimes)): it exchanges each PTE to empty, folds each old dirty bit into
   the page, and completes the invalidation (§2.4) before it decides. A page found dirty stays in
   the cache, unmapped and dirty, for the writeback threads; a clean page's count drops. It skips
   any page it cannot take at once. It skips a page that `mlock` holds, which is off the LRU
   (ROADMAP §12.4), and a page that a device has pinned (ROADMAP §19.8). It takes a sleeping lock
   only by try-lock, which never waits, and never takes the address-space lock.
3. It writes no page. The ROADMAP §12.5 writeback threads write dirty file pages, and ROADMAP
   §12.7's swap-out thread writes anonymous pages. When a pass frees too little, direct reclaim
   wakes those threads, waits up to 100 ms for writeback progress (any page cleaned or freed), and
   reclaims again. A pass that frees nothing counts toward a limit of 16 in a row, and one that
   frees anything resets the count. After 16 the OOM killer runs, whatever writeback is still
   pending or in flight, so an allocation waits at most about 1.6 s for that decision, and a device
   that has stopped completing delays it no longer than a slow one. These are Linux's figures
   (`MAX_RECLAIM_RETRIES` and its 100 ms reclaim throttle). A thread that holds a lock writeback may
   need, a level-4 lock or a reverse-map lock (§2.1), runs the same passes, but each waits only
   until a write already submitted to a device completes or 100 ms pass, and after 16 its
   allocation fails with `ENOMEM` instead of running the OOM killer, since the writeback it would
   wait for may need that lock. The allocation entry reads the thread's count of such locks at entry, with
   rule 1's conditions; each of those locks raises the count when it is taken and lowers it when it
   is released. That wait stands outside §2.1's order, though the reclaiming thread may hold a
   block-mapping or volume lock: the completion that ends it takes no sleeping-tier lock, in the
   device's bottom half ([§5.4](#54-irq-registration)) or in any later completion stage, and a
   filesystem's end-of-write work that needs its own locks runs in its writeback thread after the
   completion, never in it. Any wait for further writeback progress is bounded by the deadline.
   ROADMAP §13.12's lock-dependency build gives the wait a class of its own. Memory that waits only
   for an RCU grace period counts as freeable: before the OOM killer runs, reclaim waits, with the
   same deadline, for the grace period in progress to end and for the RCU callbacks it made ready to
   run (§2.12).
4. It never recurses. These are no-reclaim threads, whose allocations draw on the reserve below,
   as deep as their class allows, and then fail: the writeback threads, the swap-out thread,
   threaded interrupt bottom halves (§5.4), the block error handlers (§10.3), a workqueue worker
   while it runs a softirq-equivalent item (§2.2), and a thread already in reclaim.

Why: writeback or an unmap that needed a lock the allocating thread holds would deadlock that thread
on itself. A try-lock never waits, so reclaim may try a page and its reverse-map lock and skip what
it cannot take. For example, a filesystem that allocates while holding its volume lock would reach
writeback of its own dirty pages. A bottom half that waited on reclaim could wait for an I/O
completion that only it can deliver. A thread inside an RCU read-side section may not sleep (§2.12),
and reclaim's wait for a grace period (rule 3) would wait on that thread itself.

Linux guards the same recursion with per-call `GFP_NOFS` and `GFP_NOIO` flags and has moved page
writeback out of direct reclaim. These rules take the second route everywhere, so no call site
carries a flag. Where a thread must not wait for writeback, rule 3 decides from the locks it holds,
as Linux's scoped `memalloc_nofs_save` does, not from a flag at the call. Rejected:
- per-call reclaim flags, the shape of Rust-for-Linux's `KBox::new(x, flags)`, which add one more
  decision to every allocation;
- writeback from direct reclaim, which needs those flags.

Cost: when most reclaimable memory is dirty, an allocation waits for writeback progress rather than
writing itself, up to 16 passes of 100 ms before the OOM killer runs, and a thread that holds a
level-4 or reverse-map lock gets `ENOMEM` after its 16 passes, where Linux retries a small
`GFP_NOFS` allocation without end. ROADMAP §12.5's dirty limit throttles writers before it comes to
that. The limit counts pages dirtied through shared file mappings as well as by `write`, since the
first store to a clean page faults (§4.3).

The reserve (ROADMAP §12.6) is R frames of the buddy's free count: a level of that count, not a
separate pool. R is sized at boot as Linux sizes `min_free_kbytes`: the square root of 16 times the
memory the buddy manages, both in KiB, clamped to Linux's 128 KiB to 256 MiB, which gives 4 MiB in
a 1 GiB guest. An allocation that takes frames from the buddy, directly or by growing the heap, goes
below R only as far as its class allows. The class comes from context, as rule 1's reclaim decision
does, never from a flag:

- General: every allocation not named below. It stops at R. One that may sleep then runs direct
  reclaim and the OOM killer (rules 1 to 3); any other fails.
- Atomic: an allocation made with IF=0, with a spinlock held, or inside an RCU read-side section
  ([§2.12](#212-rcu)), and any allocation by a softirq-equivalent item or a threaded bottom half.
  It may go down to R/2, then fails.
- Progress: the writeback threads, the swap-out thread, a thread while it runs direct reclaim, and
  ROADMAP §19.10's background reclaim thread, whose running frees memory. It may use all of R, then
  fails.

Where two classes apply, as for a progress thread holding a spinlock, the deeper depth does.
Softirq-equivalent items and bottom halves run on their own threads (§2.2, §5.4), so none of them is
ever a progress thread. An OOM victim's threads, while they exit, may also go down to R/2, as
Linux's `ALLOC_OOM` gives a victim half of the min reserve and keeps the rest for reclaim. The OOM
reaper below is in the progress class. So a flood of network receive, which allocates in the atomic
class, can take at most half of R, and the rest stays for the threads that clean and free pages.
This is Linux's split: `GFP_ATOMIC` allocations may dip part of the way below the min watermark, and
`PF_MEMALLOC` reclaimers all the way. Each dedicated progress-class thread (the writeback threads,
the swap-out thread, the background reclaim thread, and the OOM reaper) names the most it allocates
for one unit of its work, such as one writeback pass or one reap step, or zero where it allocates
nothing, and ROADMAP §12.6 lists these bounds. R is the larger of the size above and twice the sum
of those bounds, so the half of R that no other class reaches holds one unit of work for each such
thread. R is recomputed when one starts or stops. A thread in direct reclaim is not in the sum,
because any number of threads may reclaim at once. `meminfo` shows R and, for each class, the lowest
free count one of its allocations has left since boot. Two rules keep ordinary work out of the
atomic class. A block completion allocates nothing, because what it needs was allocated at
submission (ROADMAP §12.5's owned submission); only a stage-2 item that submits more I/O allocates,
its new request, fallibly ([§10.1](#101-completions)). A fault or `mmap` allocates the page-table
pages it may need before it takes the page-table spinlock, with reclaim allowed, and frees those it
did not use, as Linux's `pte_alloc` does. Rejected: two pools with a refill order between them,
which adds machinery and leaves open which pool a progress thread holding a spinlock uses.

Planned (ROADMAP §27.3): while a node has sections not yet joined
([§4.2](#42-physical-memory-buddy-allocator)), joining one comes before the reserve. An allocation
outside the atomic class that would take free memory below R joins a section of a node it may use
and retries, so it goes below R, and direct reclaim and the OOM killer run, only once no section on
those nodes is left to join. The atomic class never joins; the background reclaim thread joins for
it.

The OOM killer (ROADMAP §12.6) chooses among the user processes of one scope, the machine and later
a cgroup, and never chooses pid 1, whose exit panics the kernel (ROADMAP §10.5), or a kernel thread.
A scope has at most one victim at a time: while that victim's memory is still to be released, the
killer chooses no other, and the allocation that ran it waits, with a deadline, then tries once
more. An OOM reaper thread unmaps the victim's private memory without waiting for it to exit, taking
the lock on the victim's region table only by try-lock, so a victim stuck in an uninterruptible wait
still gives its memory back. With no eligible process the allocation fails with `ENOMEM`, and a user
fault is retried. Linux panics when nothing is killable; here nothing on this path panics (AGENTS.md
rule 4).

Where reclaim is not allowed, a failed allocation must not lose an obligation. An allocation that
may not reclaim (rule 1) can fail at any time, driven by input alone, and its caller keeps what it
owes without it:

- Received data that cannot be buffered is dropped and counted, never half-processed. A TCP segment
  dropped this way is answered with an acknowledgement that advertises a zero window, as Linux does,
  so the sender probes instead of backing off its retransmission timer, and the window update after
  the reader drains restarts it at once.
- A driver that cannot refill a receive ring from its bottom half queues a refill item on an
  ordinary workqueue worker, which allocates with reclaim and retries with backoff until the ring is
  back above its low mark. A ring below that mark always has a refill pending, since an empty ring
  raises no interrupt that would retry. This is Linux virtio-net's `refill_work`.
- A timer callback that cannot allocate what a pending obligation needs re-arms itself instead of
  returning with nothing armed: after 500 ms for a retransmission, a zero-window probe, or a
  keepalive (Linux's `TCP_RESOURCE_PROBE_INTERVAL`), and after 200 ms for an acknowledgement
  (`TCP_DELACK_MAX`).
- Received data that a protocol has acknowledged cumulatively, and sent data not yet acknowledged,
  are never freed to relieve memory. Under pressure a receive queue is collapsed into fewer, fuller
  buffers, and the out-of-order queue, which no cumulative acknowledgement covers, may be pruned,
  reneging any block it had selectively acknowledged, as RFC 2018 §8 allows. These are Linux's
  `tcp_collapse` and `tcp_prune_ofo_queue`.

Why: each obligation has one owner, and nothing else retries it. An empty receive ring raises no
interrupt, so a refill left for the next interrupt never runs, and the queue, with every flow its
hash sends there, is dead until reboot. A retransmit timer is its connection's only liveness once
the peer's acknowledgement is lost. Freeing acknowledged data hands the reader a stream with a hole
and no error. Rejected: a reserve large enough that these allocations never fail, since untrusted
input sets the demand; a refill only at the next interrupt; and reclaim in bottom halves, which
rule 4 forbids.

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
  holding both images' page tables until the swap. `docs/LINUX.md` lists the difference
  (`execve-late-errno`).
- Exit has no caller to return an errno to, so its point of no return is its first release. Thread
  and process exit, an OOM victim's included, and the reap of a zombie by `wait4` allocate nothing
  from there on. What they need, such as a zombie's exit status and the `SIGCHLD` its parent gets,
  lives in memory allocated when the process or thread was created, where a failure made `fork` or
  `clone` return an errno. An exiting thread's puts go through `put_deferred`
  ([§2.11](#211-object-lifetimes) rule 6), so a release that needs memory, such as freeing an
  unlinked file's blocks at its last close, runs on a workqueue worker. Rejected: a reserve set
  aside at boot for each teardown path, in the shape of Linux's mempool, whose bound would scale
  with the process table.

A path that frees memory does not need memory to finish. `munmap` and a `MAP_FIXED` replacement
make the allocations they may need, for a region split and the new region, before they change their
first PTE; such an allocation may fail with `ENOMEM`, as on Linux, and leaves the mappings as they
were. From its first PTE change on, a freeing path completes even if every allocation fails, because
it collects what it drops in §4.3's gather, which never allocates. Exit and `execve` teardown,
truncate's unmap, the OOM reaper, and reclaim's reverse-map unmap allocate nothing at all. Rejected:
sizing the collection before the walk, since only the walk finds how much it drops, and for exit
that is the resident memory that has run short; and letting the gather grow by allocations that
may fail, as Linux's `mmu_gather` does, since under the page-table spinlock such an allocation is in
the atomic class above and would draw on the reserve that the threads freeing memory need.

A kernel thread or work item has no caller either, a release deferred to one included. When one of
its allocations fails, it records the error where a later call reports it, retries with a named
bound, or drops that work with a counter and a log line at most once a second, as
[§2.5](#25-panic-policy)'s rule that nothing is silently swallowed requires. A driver probe that
fails leaves its device unbound and logs why. A CPU whose idle thread or workers cannot be allocated
stays offline (ROADMAP §10.4, F037).

A slab allocator for hot object types (TCBs, file descriptors, inodes, network buffers) lands in
ROADMAP §19.9; general allocation stays on the heap, which ROADMAP §12.6 makes a constant-time TLSF
allocator, and slab does not replace it. A typed cache has no constructor: an object is initialized
on allocation and dropped on free. It is an allocator for `kalloc`'s owning types, which gain an
allocator parameter defaulting to the heap, not a second family of owning types. Direct reclaim
runs only the shrinkers that take no sleeping lock except by try-lock (rule 2); one that must wait
for a lock runs from ROADMAP §19.10's background reclaim thread.

Planned (ROADMAP §12.1): one set of allocation hooks for the sanitizer builds. Every kernel allocator
(the buddy, the heap, the KVA allocator, and ROADMAP §19.9's slab) calls the same hooks when it hands
memory out, with the span the caller may use, and when it takes memory back, and it holds freed
memory back from reuse for as long as the hooks' quarantine asks. The KASAN build marks red zones
and freed memory in its shadow, the `debug_mm` build fills freed memory with poison and checks it on
reuse, and the MTE build (ROADMAP §18.4) tags each allocation and retags it on free; in the default
build the hooks are empty. A new allocator calls them in the commit that adds it. Why: a sanitizer
sees only what an allocator reports to it, and with one set of hooks an allocator that lands after a
sanitizer build, or a build that lands after an allocator, needs no change to the other, whatever
order ROADMAP Phases 18 and 19 close in. Rejected: each sanitizer build patching each allocator,
which leaves an allocator that lands later, such as the slab after the KASAN build, invisible to it.

## 4.5 Kernel virtual address allocator

The heap answers "give me 40 bytes". The KVA allocator answers "give me 16 KiB of contiguous virtual
address space with an unmapped guard below it". Guarded kernel stacks are the motivating case, `vmap` of
non-contiguous frames is the second.

- A guarded stack is a power-of-two size *S* and starts at an address aligned to *2S*, and the *S*
  bytes of VA below it stay unmapped, so overflow takes a page fault instead of quietly eating
  whatever is below. Every address in the stack then has bit log2(*S*) clear and every address in its
  guard has it set. aarch64 relies on that: an exception taken at the kernel's level runs on the stack
  that overflowed, so each vector entry tests the bit and moves to a per-CPU overflow stack when it is
  set ([§11.5](#115-aarch64-exceptions-and-privilege-transitions) rule 6), which needs every stack the
  aarch64 kernel runs on (thread, idle, and overflow stacks) to have one size, 16 KiB. x86_64 needs no
  test, since `#DF` switches to its IST stack (§5.1), and its `#DF` handler reports stack overflow
  when CR2 lies in the guard of the stack the interrupted code ran on; it uses the same layout, so
  stack allocation has one path. Rule; not yet enforced: ROADMAP §11.3. Today a guarded stack of *n*
  pages reserves *n+1* pages of VA, maps the upper *n*, and has no alignment.
- Stack frames are allocated as *n* separate order-0 frames, not one order-*k* block. Stacks do not
  need physical contiguity and requesting it fragments the buddy allocator for nothing. Planned
  (ROADMAP §10.3): the `GuardedStack` holds each frame's `Frames` (§4.2).
- A freed VA range returns to the free list only after its shootdown completes (§2.4;
  `kva_init::unmap_shootdown`). Freed ranges go to the tail of the free list, so a stale pointer into
  one keeps faulting for as long as possible instead of reaching the range's next owner; that is a
  debugging aid, not the ordering.
- Freeing the stack you are running on does not work. Rule: a dead thread's stack is freed only
  after the CPU that ran it has switched off it (§2.8). Not yet enforced: `thread_exit` puts the
  stack on the global 8-slot `kva_init::DEFERRED` list, and any CPU's `reap_zombies` can drain it
  while the exiting CPU is still between `defer_free` and `switch_context` on that stack
  (ROADMAP §10.10, F012); `defer_free` panics when all 8 slots are full (ROADMAP §10.10, F010).
  Planned (ROADMAP §10.10): the switch tail never unmaps. It moves the dead stack into a per-CPU
  cache of at most two stacks, which the next spawn on that CPU reuses zeroed and still mapped; a
  stack the cache cannot take goes on a per-CPU list that a workqueue worker on that CPU unmaps and
  frees with IF=1, so no shootdown runs on the scheduler path. A CPU going offline (ROADMAP §19.6)
  frees its cache.

Default kernel stack is 4 pages (16 KiB) plus its 16 KiB guard. Budget: the deepest use observed on
a kernel stack, interrupts that landed on it included, stays at or below the stack's size minus
4 KiB, which is 12 KiB of a 16 KiB thread stack and 60 KiB of a 64 KiB one. The 4 KiB is the margin
for a hard-IRQ entry frame and top half (§2.2) that no run happened to land at the deepest point.
The size goes up only when a measurement names the path that needs more, and that path and its depth
are recorded here in the same commit; on aarch64 the size is one constant for every stack, and the
entry test's bit follows it. If the measurement shows a top half with its entry frame above 4 KiB,
per-CPU interrupt stacks, as Linux has, come before a larger thread stack.

Why: an overflow hits the guard page and halts the kernel (on x86_64 `#PF` has no IST stack, so the
fault becomes `#DF`; on aarch64 the entry test reports it from the overflow stack), so an overflow
on a path ring 3 drives breaks AGENTS.md rule 3. A path's depth is a sum of frames, a top half's
included, which no per-function bound sees. Rejected: raising the size when a path turns out to be
tight, which nobody learns until the overflow halts the kernel; a static whole-call-graph bound,
which loses the path at every indirect call (`dyn InodeOps`, IRQ handler tables, function pointers);
and 8-page stacks everywhere, 16 MiB at the 1024-thread Phase 10 limit, which hide the regressions a
measurement shows. Rule; not yet enforced: nothing measures stack depth (ROADMAP §10.2).

## 4.6 What comes later

The roadmap covers these in detail. Listed here so the interfaces above are designed with them in
mind:

- Demand paging (ROADMAP §12.2). `map_page` gains a "reserve VA, populate on fault" mode, and `#PF`
  becomes a recoverable exception with a real fault handler rather than a halt.
- Copy on write (ROADMAP §12.3). `fork` clones an address space by sharing frames read-only with a
  refcount; the write fault does the copy. Needs the frame metadata below (ROADMAP §12.1).
- Slab caches with per-CPU magazines, so hot object types avoid the global heap lock (ROADMAP §19.9);
  per-CPU free-frame lists do the same for the buddy lock (ROADMAP §27.3).
- One page cache of mappings (§10.6), which serves `mmap`, file I/O, and the block layer, and whose
  pages share one LRU with anonymous pages (ROADMAP §12.5).
- Swap, which finds every PTE mapping an anonymous page through the reverse map below (ROADMAP
  §12.7).

The per-frame metadata array is the pivot. Refcounting, reverse mapping, and page cache all need it,
so the PMM should be built expecting it to appear.

Frame model (ROADMAP §12.1). The metadata is kept per allocation unit: a naturally aligned block of
2^k frames from one buddy allocation, which Linux calls a folio. The unit's head `Frame` holds its
count, pin count, flags, owner, index, and LRU link; each tail `Frame` names its head and keeps only
per-frame flags, such as ROADMAP §25.3's poison bit; every `Frame` fits 64 bytes. Units are order 0
until ROADMAP §27.4 adds huge pages, so that phase changes no counting, reverse-map, page-cache, or
pin rule. `FrameRef` is one counted reference to a unit. It is not `Copy`; its `try_clone`
saturates, as Linux's `refcount_t` does, and never wraps; the last `put` hands the unit back as a
`Frames` (§4.2) to be freed after its TLB invalidation, and nothing frees implicitly.

- Every present user PTE holds one count on its unit, whatever the backing: anonymous, COW-shared,
  or page cache. The page cache holds one count of its own, and a pin (ROADMAP §19.8) holds one. A
  unit returns to the buddy only when its count reaches zero.
- A kernel-owned unit (the shared zero page, and later the vDSO pages, packet rings, and dumb
  buffers) carries a kernel-owned flag and takes no PTE count. Teardown recognizes the flag and
  drops only the mapping, as Linux's special zero-page PTEs do; counting them would put every CPU's
  demand-zero read fault on one contended atomic.
- A write fault reuses a private page in place only when the page is anonymous and its count is 1,
  so this PTE is its only holder. A file page mapped privately is always copied, since the cache's
  own count keeps it above 1. ROADMAP §19.8 adds that a pinned anonymous page, which is always
  exclusive to one address space, is reused although its pins raise its count.
- A unit splits (ROADMAP §27.4) only after it is unmapped everywhere through the reverse map, with
  migration entries in place of its PTEs, and its count then freezes at the cache's reference plus
  the caller's. A higher count means another holder, so the split remaps the unit and fails, and the
  caller falls back to 4 KiB handling. The unit keeps no map count; ROADMAP §23.4 adds one for
  `smaps`.

Reverse map (ROADMAP §12.1). The map is by object, not by PTE. Each unit's owner and index (the
frame model above) name where its page lives: a file page's owner is its file's mapping (§10.6) and
its index the file page index; an anonymous page's owner is the anonymous object of the region it
was first faulted in, and its index its page index in that object. A file mapping keeps an interval
tree of the regions that map it, and an anonymous object keeps a list of the regions that may map
its pages. A region records its page offset in its object, kept across `mremap` and a split, so a
page's address in a region is the region's start plus (index − offset) × 4 KiB. A region's list and
tree nodes live in the region, so linking allocates nothing; a region's first anonymous fault
allocates its anonymous object, fallibly, before it takes the page-table lock.

- A shared anonymous region (`MAP_SHARED | MAP_ANONYMOUS`) is backed by an unlinked tmpfs file, as
  Linux's shmem is. Its pages are file pages to the reverse map and to the fault path.
- A walk read-locks the object's reverse-map lock and visits its regions in the object's order. For
  each, it takes that address space's page-table spinlock, finds the PTE by address, acts, and drops
  the lock, so a walk holds at most one page-table lock. A walker takes no count. It holds the
  object's reverse-map lock for reading while it borrows each region's core and takes that core's
  page-table lock. Unlinking needs the lock for writing, so a region the walker can see has live
  page tables and a live core.
- The reverse-map lock is a sleeping `RwLock`, one per file mapping and one per anonymous object, at
  §2.1's level 3b. A walk visits as many regions as programs create, so a spinlock would hold IF=0
  for a time nothing bounds (§2.9 rule 2). Direct reclaim takes it only by try-lock and skips the
  page when that fails (§4.4 rule 2).
- A region made from another (`fork`'s copy, an `mremap` destination, a split) is placed after its
  source in the object's order, so a walk that has passed the source also reaches the copy after the
  PTE is copied or moved. When `mremap` cannot keep that order, the PTE move holds each object's
  reverse-map lock for writing, as Linux's `move_ptes` does under `need_rmap_locks`.
- A linked region's start, end, and offset change only while the write lock of every object it is
  linked into is held, since a walker reads them without the address-space lock. A private file
  region with anonymous pages is linked into two objects; the file mapping's lock is taken before
  the anonymous object's, as Linux takes `i_mmap_rwsem` before the `anon_vma` lock.
- `fork` links each child region into its objects, right after its parent region, before it copies
  any PTE, then copies holding the parent's page-table lock and then the child's. It is the one path
  that holds two page-table locks, which is safe because a walk holds one. It copies no PTE of a
  shared file region, a shared anonymous region, or a private file region with no anonymous page;
  the child faults those pages in from their mapping, as on Linux, so `fork`'s time goes to
  anonymous memory.
- `munmap` and exit zap a region's PTEs through the gather (§4.3), then unlink the region under each
  object's write lock.

Known limit: a region `fork` copies from a parent region joins the parent's anonymous object, so a
long-lived parent with many children makes that object's list long, and a walk of any page in it, a
child's private copy included, visits every child. That is Linux's `anon_vma` before 2.6.34, which
then added `anon_vma_chain` to bound walks. ROADMAP §19.10 records the regions each walk visits and
adopts the chained design when the 99th percentile passes 64.

## 4.7 DMA

`DmaBuffer` is physically contiguous (buddy `allocate_constrained`: size, alignment, and an optional
power-of-two boundary the buffer must not cross). Planned (ROADMAP §10.3): it holds the `Frames`
that `alloc_constrained` returns (§4.2). A boundary is not an address limit:
`DmaAlloc::dma32` sets a 4 GiB boundary, so its buffer never crosses a 4 GiB line, but the buffer can
lie above 4 GiB once RAM extends there, and no allocator keeps a 32-bit device's buffer below 4 GiB
(ROADMAP §20.6, F030). The device-visible address is `dma_to_device(phys)` (identity until an IOMMU
exists), never a physmap virtual address. A kernel never assumes that DMA is stopped when it
starts. After a planned kexec, Bus Master Enable is clear on every PCI function (ROADMAP §25.4); a
capture kernel entered from a crash clears it on every function, and aborts SMMUv3 streams, before
it touches a device, routes an interrupt, or turns off an IOMMU translation it found enabled.
Clearing Bus Master Enable also stops a device's MSIs, which are memory writes.

`sync_for_device` / `sync_for_cpu` always run at the API boundary. On x86 they are `fence(Release)` +
`sfence` and `fence(Acquire)` + `lfence`. Descriptor publish stores the index after that store-side
barrier, not a bare `compiler_fence`.

Neither barrier orders a store before a later load from another address. After the driver stores
`avail.idx`, it loads `avail_event` (EVENT_IDX) or `used.flags` to decide whether to kick; virtio 1.2
§2.7.13.4.1 requires a full barrier (`mfence`) between the two, and `SplitQueue::get_used` needs one
after its `used_event` store. Without them the driver and the device can each miss the other's
update and the queue stops. `SplitQueue::should_kick` and `get_used` have neither (ROADMAP §10.3,
F016).

Device ordering, on both architectures. An MMIO write through the §11.1 accessors is ordered after
every earlier store to memory, so a driver that stores descriptors and an index and then writes a
doorbell adds no barrier, as Linux's `writel` promises. An MMIO read completes before any later load
from memory, so a status read and then a buffer read see the buffer the status describes, as `readl`
promises. On x86_64 each accessor is a volatile access with a compiler barrier, since an uncached
access is not reordered with earlier stores or later loads. On aarch64, where a Device-nGnRE store
can be observed before an earlier Normal store, `mmio_write` runs `dmb oshst` before its store and
`mmio_read` runs `dmb oshld` after its load, as Linux's arm64 `writel` and `readl` do. Neither
orders an earlier load before a device write: a driver that reads a buffer and then writes a
doorbell that hands the buffer back runs `dma_mb` first. A `_relaxed` accessor carries no barrier
and is used only where a comment says why no ordering is needed. Rule; not yet enforced: drivers
write MMIO with `write_volatile` directly, and the accessors arrive with ROADMAP §10.3's seam and
§11.2.

Planned (ROADMAP §11.2): DMA coherence is a property of each device, from firmware: the device-tree
`dma-coherent` property on the device or a parent bus (ROADMAP §11.5), or ACPI `_CCA` and the IORT
node's coherency attribute (ROADMAP §20.7). A device with neither is non-coherent, as Linux treats
it. The device's registry entry records it ([§12.1](#121-devices)). For a coherent device,
`sync_for_device` and `sync_for_cpu` are `dma_wmb` and `dma_rmb`. For a non-coherent device,
`sync_for_device` cleans the buffer to the Point of Coherency (`dc cvac` for each line, then
`dsb sy`) whatever the transfer's direction, so no dirty line is written back later over what the
device writes, and `sync_for_cpu` invalidates it (`dc ivac` for each line, then `dsb sy`) before the
CPU reads what the device wrote, as Linux's arm64 DMA sync does. Maintenance works on whole cache
lines, so a non-coherent device's DMA region shares no cache writeback granule (`CTR_EL0.CWG`; 2
KiB, the architectural maximum, when it reads 0) with other data: a `DmaBuffer` is whole pages, a
region smaller than a page is aligned to and sized in granules, and boot asserts that the granule is
no larger than a page. Every device x86_64 drives is coherent, so its sync calls stay the barriers
above.

Why: on a weakly ordered CPU a doorbell can overtake the index it announces, and a completion read
can overtake the status read that announced it; ordering in the accessors means no driver works it
out again, the class of bug F016 was. Coherence is per device because QEMU's `virt` and servers
snoop the CPU caches while boards mark devices one by one, and a board tree that omits
`dma-coherent` needs the maintenance. Rejected: relaxed accessors with barriers at each call site;
`dmb osh` in every `mmio_write`, which orders earlier loads too, at the cost of a full barrier on
every device write for the few hand-back paths that need it; treating every aarch64 device as
non-coherent (maintenance on every transfer where the hardware snoops); and mapping non-coherent
buffers non-cacheable (slower CPU access, and an attribute that disagrees with the physmap's
cacheable alias, which ROADMAP §11.1 forbids for MMIO for the same reason).

## 4.8 Firmware runtime services

Planned (ROADMAP §20.9). UEFI's runtime services (variables, time, reset) are firmware code the
kernel calls after boot.

- One kernel thread, `efi_rt`, makes every call, one at a time, holding the runtime-services lock
  for each. A caller queues a request and sleeps on its completion, so no call runs in a caller's
  context or address space.
- `efi_rt`'s address space maps the runtime code, data, and MMIO regions of `BootInfo`'s EFI memory
  map at their physical addresses, beside the kernel half. The kernel never calls
  `SetVirtualAddressMap`: every call is a physical-mode call through that 1:1 map, so a kernel
  started by kexec (ROADMAP §25.4, §30.6) calls firmware as the first did, and no firmware virtual
  layout crosses a handover. A runtime region the lower half cannot hold leaves runtime services
  off, with a line that says so.
- A call runs with IF=1, so the tick, IPIs, and shootdown acknowledgements are taken while firmware
  runs, and §2.9 needs no new reason for IF=0. The scheduler does not switch away from `efi_rt`
  until the call returns, so firmware sees only the pauses an interrupt makes, and its FP and SIMD
  use needs no per-thread state: before the call the live user state is saved and this CPU's FP
  owner emptied ([§7.5](#75-per-cpu-data)).
- While a call runs, the wrapper keeps a per-CPU firmware record: the service, the requesting
  thread, and its own frame pointer, stack pointer, and return address, saved before entry and
  cleared after return. The panic stop (§2.5 step 1), the NMI backtrace, and the core tool (ROADMAP
  §10.7, §25.5) read it: a CPU whose record is set, or whose interrupted PC lies in a runtime
  region, is reported `in firmware: <service>` and walked from the saved frame, never from
  firmware's frame pointer, and one the stop does not reach while its record is set is
  `not stopped (in firmware)`, which tells a wait in SMM from a kernel spin.
- Only the panic path calls firmware outside `efi_rt`: on the panicking CPU, with IF=0, through the
  same map, and only if its trylock of the runtime-services lock succeeds (§2.5; ROADMAP §25.6). It
  sets the record too.

Why: a runtime call has no time bound (a `SetVariable` may erase flash, through SMM in `q35`'s
Secure Boot OVMF), so a call with IF=0 would break §2.9 rule 2, hold off the tick and shootdown
acknowledgements, and trip ROADMAP §25.5's lockup detector. UEFI allows an interrupt during a
runtime call, and Linux makes its calls with interrupts on from one worker thread and does not
preempt a call. `SetVirtualAddressMap` can be called once per boot, so using it would tie every
kexec and live update to carrying the firmware's virtual layout across kernel versions; FreeBSD's
`efirt` likewise calls through a 1:1 map of the runtime regions. Rejected: calls with IF=0; a
preemptible call, which would make firmware's FP state and address space per-thread state and give
firmware pauses no interrupt makes; calls from the caller's own context, which would switch the
caller's page table and FP state in place; `SetVirtualAddressMap` with a fixed layout passed across
kexec, as Linux does on x86_64; and unwinding firmware frames, which carry no unwind data the kernel
reads.

---

# 5. Interrupts

Descriptor tables, the vector map, and the migration from the legacy 8259 to the APIC. Handler rules
are in [section 2.2](#22-interrupt-handler-rules); this is the mechanism. §5.1 to §5.7 are x86_64's
mechanism; aarch64's vector table, exception entry, and interrupt controller are
[§11.5](#115-aarch64-exceptions-and-privilege-transitions)'s. §5.8 and §5.10 hold on both architectures.

## 5.1 GDT and TSS

Flat segmentation. Segments exist because the CPU requires them, not because we use them.

| Selector | Descriptor |
|----------|------------|
| `0x00` | null |
| `0x08` | kernel code, 64-bit, ring 0 |
| `0x10` | kernel data, ring 0 |
| `0x18` | null |
| `0x20` | null: Linux's compat user code (`__USER32_CS`); 32-bit user code is a non-goal |
| `0x28` | user data, ring 3 (selector `0x2b`) |
| `0x30` | user code, 64-bit, ring 3 (selector `0x33`) |
| `0x38` | TSS (16 bytes, two GDT entries) |

User selectors go in from the start even before ring 3 exists. `syscall`/`sysret` reads segment
selectors out of `IA32_STAR` with a fixed layout: `STAR.SYSCALL_CS = 0x08`, so kernel SS is CS+8
(`0x10`), and `STAR.SYSRET_CS = 0x23`, so a 64-bit `sysret` loads SS from +8 (`0x2b`) and CS from +16
(`0x33`), as Linux programs STAR. User *data* therefore sits before user *code*. Getting the order
right up front avoids a rebuild of the GDT later.

The user selectors are ABI, so they take Linux's values: `mov %cs`, the signal `ucontext`, `ptrace`'s
`user_regs_struct`, and a core's `NT_PRSTATUS` expose them, and gdb's native Linux target treats a
process as 64-bit only when CS is `0x33`, and as x32 when DS is `0x2b`. Ring 3 runs with CS `0x33`, SS
`0x2b`, and the null selector in DS, ES, FS, and GS, whose bases come from the MSRs, as a 64-bit Linux
process does. `execve` and a new process's first entry load the null selector into those four, `fork`
and `clone` copy the parent's, and the context switch keeps each thread's
([section 7.5](#75-per-cpu-data)), since ring 3 can load `0x2b` or 0 itself. Slot `0x20` stays null
because it is Linux's compat code segment, and a far transfer or `rt_sigreturn` to `0x23` gets
`SIGSEGV` (`docs/LINUX.md`, `no-compat-cs`). The kernel selectors are invisible to user code and keep
their places. Rule; not yet enforced: ROADMAP §10.6. Today user data is `0x18`, user code `0x20`, and
the TSS `0x28`, and `STAR.SYSRET_CS` is `0x10`, so ring 3 runs with CS `0x23`, which is Linux's compat
code selector, and SS `0x1B`, and `enter_user` and `enter_user_full` load `0x1B` into DS, ES, FS, and
GS.

User-memory access will go through `copy_from_user`/`copy_to_user`, which dereference the user VA
inside `stac`/`clac` (ROADMAP §10.6); pointer ranges are validated before use (ROADMAP §9.3). Today it
is `AddressSpace::read_bytes`/`write_bytes` after `check_user_range`, copying through the HHDM physmap
(a supervisor mapping, so SMAP does not apply until a user-VA accessor exists), and `write_bytes` does
not check the PTE's `WRITABLE` bit. `arch::cpu::harden()` sets `CR4.SMEP|SMAP|UMIP` where CPUID allows
and asserts `CR0.WP` on every CPU; `stac`/`clac` are no-ops when SMAP is missing.

Planned (ROADMAP §10.6, §12.2, §12.5): two kinds of accessor, told apart by a kind bit in each
exception-table entry. A faulting accessor (`copy_from_user`, `copy_to_user`, and their string and
vector forms) handles a fault through the region fault handler from ROADMAP §12.2 on, which may
sleep, and returns `EFAULT` only when that handler cannot resolve the fault. It runs with IF=1, no
spinlock held, and no sleeping lock of §2.1 levels 2 to 4 held (§2.9 rule 4). A non-faulting
accessor's fault goes straight to the fixup and returns a short count: no region lookup, no lock, no
sleep, and IF left as it was. It may run anywhere, under a busy page, a spinlock, or IF=0. The
buffered `write` path (§2.1) and the futex word read (ROADMAP §13.5) use it; after a short count they
release what they hold, fault the page in, and retry. The exception table lists accessor
instructions only, and aarch64's table carries the same kind bit. Rejected: a per-thread no-fault
count (Linux's `pagefault_disable`), which adds per-thread state to the fault path and puts the
choice away from the instruction that faults.

Each CPU gets its own GDT and TSS (`gdt::CpuTables`): the BSP's lives in a `BootCell`, and
`gdt::alloc_ap_tables` allocates each AP's. TSS.RSP0 is the stack an interrupt or exception from
ring 3 lands on. `syscall` does not read the TSS; its entry loads `PerCpu.kernel_rsp0`.
`syscall_init::set_rsp0_for` sets both to the incoming thread's stack top on every switch (the
per-CPU `fallback_rsp0` for a thread with no stack of its own); the CPU never writes RSP0. Planned
(ROADMAP §10.3, F089): the TSS sits in an `UnsafeCell` inside `CpuTables`, and `CpuTables::set_rsp0`,
an `unsafe fn` that only the owning CPU calls with IF=0, is its one writer after `load`; today
`set_rsp0_for` writes through `gdt::bsp_tss_ptr`, a `*mut Tss` cast from a shared reference. The TSS
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
the table lives in `vibeos-core`. The x86_64 port decodes each vector and error code into a portable
`TrapKind`, as the aarch64 port decodes its exception classes
([§11.5](#115-aarch64-exceptions-and-privilege-transitions)), and one table gives each `TrapKind` its ring-3
action, the signal and the `si_code` Linux sends; a host test runs each vector `0x00`–`0x1F` through
decode and table and fails on any without a ring-3 row. `TrapKind` names what the CPU reported;
ROADMAP Phase 12's fault kinds (demand-zero, COW, file, stack) name how the fault path resolves a page
fault, downstream of it.

| Vector | Name | Ring 0 | Ring 3 | Ring 3, as built |
|--------|------|--------|--------|------------------|
| `0x00` | `#DE` | dump, halt | `SIGFPE` | as the rule |
| `0x01` | `#DB` | dump on IST, halt. Planned (ROADMAP §17.4, §18.4): three cases continue instead. A hit whose saved DR6 names only slots the current thread's tracer armed is dropped, as Linux drops a kernel-mode hit of a ptrace breakpoint; DR6.BS clears TF in the saved frame and logs once, as Linux does; in the §18.4 detector build a hit on a detector slot is reported | `SIGTRAP` (RFLAGS.TF, `int1`, a breakpoint or watchpoint the tracer armed); in the §18.4 detector build a hit on detector slots alone resumes with no signal and is counted | halts the kernel. Rule; not yet enforced: ROADMAP §10.6 (F005) |
| `0x02` | NMI | dump on IST, halt. Planned (ROADMAP §10.7, F135): the handler first reads and clears its CPU's stop request word (§2.5 step 1): STOP stops the CPU, a CPU already stopped halts again at once, and an NMI with no request on the dump owner returns at once. Planned (ROADMAP §25.5): a backtrace or lockup request, and an external NMI on a CPU that is neither stopped nor the dump owner, are handled and return | not a ring-3 fault: the Ring 0 column applies | as the rule |
| `0x03` | `#BP` | log, continue | `SIGTRAP` (`int3`) | `int3` hits the DPL-0 gate, raises `#GP`, and gets `SIGSEGV`. Rule; not yet enforced: ROADMAP §10.6 (F148) |
| `0x04`, `0x05`, `0x07`, `0x0A` | `#OF`, `#BR`, `#NM`, `#TS` | dump, halt | `SIGSEGV` | `sig_for_vec` has no row, so one would halt the kernel. Rule; not yet enforced: ROADMAP §10.6 (F005) |
| `0x06` | `#UD` | dump, halt | `SIGILL` | as the rule; an SSE floating-point error also arrives here (row `0x13`) |
| `0x08` | `#DF` | dump on IST, halt | not a ring-3 fault: the Ring 0 column applies | as the rule |
| `0x0B`, `0x0C` | `#NP`, `#SS` | dump, halt | `SIGBUS`; `SIGSEGV` for a fault on the return-to-user `iretq` (§5.10 rule 2) | the `iretq` case halts. Rule; not yet enforced: ROADMAP §10.6 (F007) |
| `0x0D` | `#GP` | dump with error code, halt | `SIGSEGV`, including a fault on the return-to-user `iretq` (§5.10 rule 2) | the `iretq` case halts. Rule; not yet enforced: ROADMAP §10.6 (F007) |
| `0x0E` | `#PF` | dump with CR2, halt. Planned (ROADMAP §10.6, §12.2): a fault inside a user-memory accessor is handled by that accessor's kind (§5.1) and ends in `EFAULT` or a short count | `SIGSEGV`. Planned (ROADMAP §12.2): a fault on a page that a region reserves is resolved first, and one through a file mapping on a page wholly past EOF, or on a page whose fill fails, gets `SIGBUS`, and so does a store through a shared file mapping whose space reservation fails (§4.3) | as the rule |
| `0x10` | `#MF` | dump, halt | `SIGFPE` | cannot fire: `CR0.NE` is clear, so an x87 error raises the masked IRQ13 and is lost. Rule; not yet enforced: ROADMAP §10.6 (F026) |
| `0x11` | `#AC` | dump, halt | `SIGBUS`, for a misaligned access while ring 3 has set RFLAGS.AC; `CR0.AM` is set on every CPU, as Linux sets it | halts the kernel if `CR0.AM` is set (INIT clears it on each AP and no kernel code sets it; the BSP keeps Limine's value), and while it is clear ring 3's AC raises nothing. Rule; not yet enforced: ROADMAP §10.6 (F005) |
| `0x12` | `#MC` | dump on IST, halt; `CR4.MCE` is clear, so a machine check shuts the CPU down with no dump (ROADMAP §10.6, F026). Planned (ROADMAP §25.1, §25.3): only a fatal machine check, or an action-required error in kernel memory, halts; a lower severity is recorded and the CPU continues | not a ring-3 fault: the Ring 0 column applies. Planned (ROADMAP §25.3): an action-required error that ring-3 code consumed is recorded by the handler and recovered in exit work (§5.10 rule 11), which sends `SIGBUS` with `BUS_MCEERR_AR` | as the rule |
| `0x13` | `#XF` | dump, halt | `SIGFPE` | arrives as `#UD` and gets `SIGILL`: `CR4.OSXMMEXCPT` is clear. Rule; not yet enforced: ROADMAP §10.6 (F026) |
| `0x09`, `0x0F`, `0x14`–`0x1F` | reserved, `#VE`, `#CP`, `#HV`, `#VC`, `#SX` | dump, halt | `SIGSEGV` | `sig_for_vec` has no row, so one would halt the kernel. Rule; not yet enforced: ROADMAP §10.6 (F005) |
| `0x20`–`0xFF` | IRQs and IPIs | handle, return. An interrupt no handler owns is counted per vector and per CPU, EOIed at the controller that delivered it (the LAPIC when its in-service bit for the vector is set, else the 8259), logged at most once a second per vector, and ignored; §5.5 gives the 8259 lines. Rule; not yet enforced: a pool vector (`0x31`–`0x7F`) with no handler is EOIed and ignored with no count, a vector in `0x80`–`0xEF` or `0xF3`–`0xFA` dumps and halts, and an 8259 line with no handler other than IRQ7 and IRQ15 prints `irq: unexpected` and halts the CPU that took it (ROADMAP §10.6) | handle, return to ring 3 | `0x21`, `0x30`, and `0x31`–`0x7F` run on the user GS base and halt the kernel. Rule; not yet enforced: ROADMAP §10.6 (F004) |

A halting handler prints the interrupt frame (RIP, CS, RFLAGS, RSP, SS), the error code where the
vector pushes one, and, for `#PF`, the CR2 the stub saved (§5.10 rule 9), then the common dump
(§2.5). A halt with no register dump is a wasted crash. A ring-3 fault whose signal takes its default
action (§2.5), the only action before ROADMAP §13.8, prints
`user: pid N killed SIG<name> rip=0x<rip> err=0x<err>`, plus
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
| `0xFE` | Panic stop IPI (§2.5 step 1) |
| `0xFF` | LAPIC spurious vector |

Vector numbers live in one module as named constants, with a host unit test asserting that no two are
equal. That test costs nothing and catches the copy-paste that assigns two subsystems the same vector.

Planned (ROADMAP §11.3): the device vectors above (the keyboard's and the pool's) are the x86_64
chip's hardware numbers ([§5.4](#54-irq-registration)), and the device pool is that chip's to
allocate. On aarch64 the GIC sets priority per interrupt, by class: the top class is reserved for
ROADMAP §25.5's pseudo-NMI, then come IPIs and the tick (the generic timer's PPI), then devices, so
a busy device does not starve a reschedule there either. IPIs are SGIs 0 to 7 only, as Linux uses
them, since Arm recommends leaving SGIs 8 to 15 to the Secure world. The kernel therefore has at
most eight IPI kinds on both architectures, and a line that adds one takes a free SGI here.

| SGI | Purpose | x86_64 vector |
|-----|---------|---------------|
| 0 | Reschedule | `0xFD` |
| 1 | Call function | `0xFB` |
| 2 | Panic stop ([§2.5](#25-panic-policy)) | `0xFE` |
| 3–7 | Free | |

No SGI does TLB shootdown: aarch64 broadcasts its TLB maintenance (ROADMAP §11.2).

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
F110). That flag is not the `InterruptGuard` nest: `allocate_vector` takes only spinlocks, so a
caller with IF off may allocate, and only a device hard-IRQ is refused. The in-guest registry also
holds a guard on the BSP until ROADMAP §10.2 moves it to a thread (F075), so a nest check would
refuse every allocate from `ktest`.

Planned (ROADMAP §11.3, on x86_64 before the GIC): drivers name an interrupt by an `IrqId`, a `u32`
the IRQ layer allocates, never a hardware number. It indexes the handler table, the interrupt's
bottom-half thread, and `free_vector`'s wait. Each interrupt controller is an `IrqChip` object, and
an `IrqId` records its chip and its hardware number on that chip (its hwirq):

```rust
let irq = irq::map_wired(&spec)?;      // a device-tree `interrupts` specifier or an ACPI GSI
let irqs = irq::alloc_msi(&dev, n)?;   // MSI or MSI-X, through the device's MSI parent
let tick = irq::map_percpu(&spec)?;    // a LAPIC LVT or a GIC PPI: one IrqId on every CPU
irq::set_threaded(irq, Some(top_half), thread_fn);
irq::set_affinity(irq, cpu)?;
```

A chip translates a firmware specifier (a device-tree `interrupts` specifier, an ACPI GSI with its
trigger and polarity) to a hwirq; masks, unmasks, and ends an interrupt (EOI); sets its affinity;
allocates MSI hwirqs for a device; composes the MSI address and data for one; and frees them. IPIs
are not `IrqId`s: the port sends them on §5.3's fixed vectors or SGIs.

On x86_64 the chips are the 8259, the I/O APICs, and the LAPIC's MSI domain. A hwirq there is an IDT
vector from §5.3's pool, allocated inside the chip, so ROADMAP §27.1's per-CPU vectors change the
chip and no driver. On GICv3 a hwirq is an SPI or a PPI of the distributor and redistributors, or an
LPI from the ITS chip's allocator. The ITS maps a device by its DeviceID, from the device tree's
`msi-map` or `msi-parent` (ROADMAP §11.5) and later ACPI IORT (ROADMAP §20.7). GICv2m turns an MSI
into an SPI.

No driver composes or rewrites an MSI message. The PCI layer writes MSI and MSI-X entries from the
chip's `compose_msi`, in the order below. `set_affinity` asks the chip: the x86 chip rewrites the
I/O APIC route, or recomposes the message and has the PCI layer rewrite the entry; the ITS sends
`MOVI` to the target CPU's collection, then `SYNC`, and the entry stays as it was; GICv2m changes
the SPI's target. Priority is the chip's as well (§5.3).

The ITS tables and the redistributors' LPI property and pending tables are allocated once at boot
and never freed or moved, and the kernel never clears `GICR_CTLR.EnableLPIs`, which some GICs cannot
clear once it is set. ROADMAP §25.4's handover carries the tables' ranges, and a kernel that finds
EnableLPIs set reuses them, as Linux does.

`IrqChip` is an object-safe trait in the portable half. Each controller kind a port finds at boot is
one chip object in a static `BootCell`, reached as `&'static dyn IrqChip`, as §6.1's registry
reaches drivers; the indirect call is a branch beside an interrupt entry. A controller a driver
brings, such as a cascaded one, would be a counted device (§12.1), and the line that adds one
extends this. The seam's zero-sized port ([§11.1](#111-the-seam)) keeps only the vector entry,
finding the root controller, and the IPI send. The MSI paragraph below becomes the x86 chip's
`compose_msi`. Until the ROADMAP §11.3 box lands, the rest of this section describes the x86 vector
API as built. Each of its rules then holds for an `IrqId`: `set_affinity`, `set_threaded`, and
`free_vector` keep their names and take an `IrqId`, and a vector in ROADMAP §12.5 and §20.9 means an
`IrqId`.

Why: a GIC names a wired interrupt by an SPI the firmware fixes, delivers the timer as a per-CPU
PPI, and moves an ITS interrupt with `MOVI` while the device's message stays the same, so an API of
pool vectors and messages built from APIC IDs fits x86 alone. Kept, it would have every driver of
ROADMAP Phases 12 to 20 written against one architecture, and `set_affinity` on the ITS would
rewrite an entry the ITS ignores, so the interrupt would never move. An `IrqId` also keeps its
identity when ROADMAP §27.1 makes x86 vectors per CPU. Controllers are runtime objects because a
port runs several at once and firmware decides which exist (§1.1's N backends), as Linux's
`irq_chip` and irq domains are. Rejected: keeping `u8` vectors and mapping them to INTIDs inside the
GIC driver (79 device interrupts on both architectures, a lookup on every interrupt, and MSI
composition left in drivers); widening the vector to a `u32` hardware number (not unique once x86
vectors are per CPU, or where GICv2m's SPIs sit beside the distributor's); a seam trait on the
zero-sized port (one implementation per port, where each port runs several controllers at once); a
closed enum of each port's chips (no controller a driver brings could join); and waiting for §27.1
(every driver of Phases 12 to 20 written twice).

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

Rule: `free_vector` masks the vector, sets its quiesce flag ([section 10.3](#103-failure)), wakes
its bottom-half thread, and returns only after no CPU is running the vector's top half and that
thread has exited, so a driver may free the state its handlers touch as soon as it returns. A
bottom half that wakes to its quiesce flag returns without touching the device, so a remove that
has already stopped its device never waits on it. Teardown order for a device
is: stop the device (status 0, bus mastering off), free its vectors, detach its
IOMMU domain and complete the invalidation, then free its DMA memory and handler
state (§2.11 rule 3, §12.4). Not yet enforced: `free_vector` does not wait for a
handler already running on another CPU (ROADMAP §20.9). With interrupt remapping
(ROADMAP §18.1), `free_vector` also frees the vector's remapping entry and waits
until the interrupt entry cache invalidation completes, so a device that still
writes its old message reaches no vector that a later `allocate_vector` hands out.

On the GICv3 ITS, freeing a device's LPIs, at `free_vector` or at removal, sends `DISCARD` for each
of its events and `MAPD` with V=0 for its DeviceID, then `SYNC`, and waits up to 1 s, as Linux
waits for its ITS command queue, until the ITS has consumed the commands. Only then are the
device's ITT freed and its LPIs returned to the allocator. If the wait expires, both stay reserved
and the event is logged. The ITS reads each device's ITT from memory, so an ITT freed earlier would
let a late MSI translate through reused memory into another device's interrupt. This is the
aarch64 form of §12.4's interrupt-remapping rule. This is the ITS chip's free operation.

EOI is the dispatcher's job, not the driver's. The dispatch layer knows whether a
vector arrived via PIC or LAPIC and signals the right controller. A vector with no handler still
gets its EOI; ROADMAP §10.6 also counts and logs it (§5.2's `0x20`–`0xFF` row).

A threaded handler's top half runs in `dispatch` (ack / mask / wake only). Today one
kernel thread, pinned to the last online CPU, runs every threaded vector's bottom half.
Planned (ROADMAP §12.5): each threaded vector gets its own bottom-half thread, pinned to the
CPU the vector is routed to and moved by `set_affinity`, so a device with one queue per CPU
gets one thread per queue. A bottom half never waits for an I/O completion, its own device's
included, since it may be the thread that delivers it. It waits only for a resource of its own
device, such as a free descriptor. Each such wait has a deadline and also ends when the vector's
quiesce flag is set ([section 10.3](#103-failure)). It takes no lock of §2.1's sleeping tier,
allocates without direct reclaim, in §4.4's atomic class, and leaves completion work beyond waking
waiters and settling page state to stage 2 ([section 10.1](#101-completions)). In debug builds,
`IoWaiter::wait` and every page or buffer wait assert that the caller is neither a bottom half nor
a softirq-equivalent item. `free_vector` ends the vector's thread before it returns.

Why: one shared thread puts every device's completions on one CPU, although multi-queue
devices (virtio-blk today, NVMe in ROADMAP §20.4, RSS in Phase 28) spread them across CPUs
on purpose. One bottom half that blocks also stalls every other device, including the one
whose completion it waits for. One thread per threaded interrupt is Linux's model.
Per-vector threads end the stall across devices, not within one vector: a bottom half that waits
for its own device's completion waits for itself. So it may wait for resources only, and the debug
assertion checks the promise that the rejected shared thread could only make.
Rejected: one shared thread whose handlers promise never to block, which nothing checks;
and one bottom-half thread per CPU, in which one device's blocked handler still stalls
another device's on that CPU.

`set_threaded` is refused inside a hard-IRQ. The top half is optional:
`set_threaded(vec, None, work)` is accepted, and `dispatch` EOIs before the bottom half runs, so on a
level-triggered INTx route a device that nothing quiets raises the line again at once (ROADMAP §15.2,
F099). The softirq stand-in is the high-prio workqueue: IRQ context enqueues a `fn(usize)` and
wakes workers. Planned (ROADMAP §19.4): its workers, one per CPU, run in the fair class at nice -20,
and bottom-half threads at `SCHED_FIFO` 50 ([§7.8](#78-per-cpu-scheduling)).

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
  and the PIT owns the tick. The default PIC handler reads the ISR for IRQ7 and IRQ15: a clear bit
  means a spurious IRQ, which gets no EOI on that PIC (a spurious IRQ15 still EOIs the master's
  cascade line), and a real IRQ7 or IRQ15 with no driver is EOIed and ignored. Any other line with
  no handler prints `irq: unexpected N` and halts the CPU that took it. Planned (ROADMAP §10.6): a
  spurious IRQ is counted, and any other line no driver claims is masked at the PIC, EOIed,
  counted, and logged at most once a second, and the kernel goes on.
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
  already exist; what is left is a policy that calls `set_affinity` when a queue saturates one
  core, which each chip carries out its own way (§5.4).

## 5.10 Privilege transitions

Every entry to and exit from ring 3, and the state each boundary must hold. "User GS" means
`GS_BASE` holds the user base (0; `enter_user` and `enter_user_full` write it) and `KERNEL_GS_BASE`
holds this CPU's `PerCpu`. "Kernel GS" means `GS_BASE` holds this CPU's `PerCpu`; `KERNEL_GS_BASE`
then holds the user base after a `swapgs`, or the `PerCpu` address after `per_cpu_init::init_bsp`,
`install_gs`, or `arch::gs::force_kernel` (§7.5). The table is the required state. A rule the
code does not meet yet says "Rule; not yet enforced", names the ROADMAP line that lands it, and then
describes the current code.

Every entry from user mode saves the thread's user frame at the top of its kernel stack before it
calls a body: the syscall entry, every generated stub for a CPL-3 frame (rule 1), and on aarch64
`svc` and every exception taken from EL0 (ROADMAP §11.6). On x86_64 the frame is the first 21 words
of Linux's `struct user_regs_struct` (`arch/x86/include/asm/user_64.h`), the register set
`NT_PRSTATUS` and `PTRACE_GETREGS` use: `r15`, `r14`, `r13`, `r12`, `rbp`, `rbx`, `r11`, `r10`,
`r9`, `r8`, `rax`, `rcx`, `rdx`, `rsi`, `rdi`, `orig_rax`, then `rip`, `cs`, `rflags`, `rsp`, and
`ss`, which is the frame `iretq` pops. The syscall entry stores RCX in both the `rcx` and `rip`
slots, R11 in both `r11` and `rflags`, the user selectors in `cs` and `ss`, and the syscall number
in `orig_rax`; every other entry leaves -1 in `orig_rax`, after it has passed the body any error
code the CPU pushed into that slot, as Linux does. On aarch64 the frame is Linux's
`struct user_pt_regs` (`arch/arm64/include/uapi/asm/ptrace.h`: `x0`-`x30`, then `sp` from SP_EL0,
`pc` from ELR_EL1, and `pstate` from SPSR_EL1), followed by `orig_x0` and the syscall number, -1 on
an entry that is not a syscall. Everything that reads or writes a user context uses this frame,
through the seam's user-context trait ([§11.1](#111-the-seam)): signal delivery and `rt_sigreturn`,
`ptrace`, core dumps, `fork`, `execve`, syscall restart, and the ROADMAP §10.7 forensics; a spawned
or forked thread's first return to user mode is the ordinary exit over a frame its creator wrote.
The x86_64 syscall exit stores the return value in the `rax` slot and uses `sysretq` only when
`rip` equals `rcx`, `rflags` equals `r11`, `cs` and `ss` are the user selectors, `rip` is below
`USER_MAP_END`, and RF, TF, and VM are clear in `rflags`, which is Linux's test; otherwise it
restores every register from the frame and runs `iretq` on the frame's tail. A non-canonical `rip`
reaches neither instruction: it becomes `SIGSEGV` (AGENTS.md rule 1; ROADMAP §10.6). SYSRET loads
RIP from RCX and RFLAGS from R11, so only a context that `syscall` created can take it, and with TF
set it traps before the next user instruction runs, where `iretq` lets that instruction run first.
A syscall that must restart sets `rax` from `orig_rax` and moves `rip` back 2 bytes, or on aarch64
sets `x0` from `orig_x0` and moves `pc` back 4, as Linux does. Rule; not yet enforced: ROADMAP
§10.6. The syscall entry saves 16 words with no RIP, RFLAGS, CS, SS, or syscall-number slot, and
its exit reads RIP and RFLAGS from the RCX and R11 slots, so a context whose RCX and R11 differ from
its RIP and RFLAGS cannot be returned to; an `x86-interrupt` handler saves only the registers it
clobbers; and `enter_user_full` takes a second format, `UserRegs`.

| Point | CPL, stack | GS | IF | AC |
|-------|------------|----|----|----|
| `vibeos_syscall_entry`, before its `swapgs` | 0, user RSP | user | 0 (FMASK) | 0 (FMASK) |
| syscall entry after its `swapgs`, and the syscall body | 0, `PerCpu.kernel_rsp0` | kernel | 0 until the stub has moved the user RSP out of the scratch and runs `sti`; 1 in the body ([§2.9](#29-preemption-and-interrupt-state) rule 3) | 0 |
| syscall exit, from the return of `vibeos_syscall_stub` to `sysretq` or `iretq` | 0, kernel stack, then the user RSP | kernel; user after `swapgs` | 0 (rule 4) | 0 |
| `enter_user` and `enter_user_full`, from `mov gs` to `iretq` | 0, kernel stack | user | 0 (rule 4) | 0 |
| non-IST vector taken at CPL 3 | 0, TSS.RSP0 | user until the stub's `swapgs` | 0 (interrupt gate); a fault or trap body then runs with IF=1, after the stub has saved the syndrome (rule 9), an interrupt's top half with IF=0 (§2.9 rule 3) | ring 3's until the stub's `clac` (rule 5) |
| non-IST vector taken at CPL 0 | 0, interrupted stack | kernel, except rule 2's case | 0 (interrupt gate) | the interrupted value until the stub's `clac` (rule 5) |
| IST vector taken at CPL 0 (`#DB`, NMI, `#MC`), and `#DF` | 0, its IST stack | whatever the interrupted point held (rule 3) | 0 | the interrupted value until the stub's `clac` (rule 5) |
| IST vector taken at CPL 3 (`#DB`, NMI, `#MC`) | 0, its IST stack, then the thread's kernel stack once the stub has copied its frame into the user frame (rule 3) | user until the stub's `swapgs` | 0; a `#DB` body then runs with IF=1 as any trap taken at CPL 3 does (§2.9 rule 3), an NMI's and a `#MC`'s with IF=0 | ring 3's until the stub's `clac` (rule 5) |
| vector exit to CPL 3 | 0, then 3 at `iretq` | user after `swapgs` | 0 until `iretq` restores ring 3's | `iretq` restores ring 3's |

On aarch64 the boundary is between EL0 and the kernel's level, EL1 or EL2 with VHE. The table below
is the required state; [§11.5](#115-aarch64-exceptions-and-privilege-transitions) gives the mechanism and the
aarch64 counterparts of rules 1, 4, and 5. Rules 2 and 3 have none, because EL0 cannot reach the
per-CPU base register, so nothing is swapped at the boundary. Rules 9 onward hold on both
architectures. Planned (ROADMAP §11.3, §11.6): the aarch64 port does not exist.

| Point | Level, stack | DAIF | PAN | `SP_EL0` |
|-------|--------------|------|-----|----------|
| `svc` or exception taken from EL0, until the stub has saved the user frame | EL1 (EL2 with VHE), `SP_ELx` at the top of the thread's kernel stack, where the last return to EL0 left it | all set by the exception; the stub clears `MDSCR_EL1.SS` for a thread being stepped before any DAIF bit is cleared (§7.5, Debug state) | set by the exception (`SCTLR_EL1.SPAN` clear) | the user SP, until the stub saves it and loads `current` ([§2.9](#29-preemption-and-interrupt-state) rule 5) |
| the syscall body, and the body of a fault or trap taken from EL0 | the kernel's level, the thread's kernel stack | all clear once the frame is saved (§2.9 rule 3) | set; clear only inside the user-memory accessors | `current` |
| IRQ taken from EL0, its top half | the kernel's level, the thread's kernel stack | D and A clear; I and F set | set | `current` |
| exception or IRQ taken at the kernel's level | the kernel's level, the interrupted stack, or this CPU's overflow stack when the entry's stack test finds it overflowed ([§11.5](#115-aarch64-exceptions-and-privilege-transitions) rule 6) | all set by the exception; D and A clear once the frame is saved | set by the exception; the interrupted value returns with `SPSR_EL1` | `current`, untouched |
| return to EL0, from the first write of `ELR_EL1`, `SPSR_EL1`, or `SP_EL0` to `eret` | the kernel's level, then EL0 at `eret` | all set, until `eret` loads EL0's from `SPSR_EL1`; `MDSCR_EL1.SS` set after the last exit-work check, for a thread being stepped only (§7.5) | EL0 does not use it | the user SP, restored from the frame |

1. One entry stub per vector, owned by `arch/idt.rs` and generated from one table. The stub runs
   `cld`, `clac` when SMAP is live, the GS decision, and rule 9's syndrome save, calls a body
   function, and mirrors the GS decision on exit. `idt::set_handler` takes a body function, never a
   gate, and no `extern "x86-interrupt"` function exists outside `src/arch/`. Rule; not yet
   enforced: ROADMAP §10.6 (F004). Each `arch/idt.rs` handler calls `gs_enter` and `gs_leave`
   itself, and `irq_init::device_irq::<N>` (`0x31`–`0x7F`), `kbd_init::kbd_ioapic` (`0x30`), and
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
3. An IST vector taken at CPL 0 decides `swapgs` from the sign of `GS_BASE` (`rdmsr`; a kernel base
   is negative), because it can interrupt CPL-0 code that has the user GS loaded (the rows above
   whose GS column says user), and on exit it restores the GS state it found. `#DF` does the same at
   any CPL, since the CS it saves is undefined (Intel SDM Vol. 3A, interrupt 8). An IST vector taken
   at CPL 3 (`#DB`, NMI, `#MC`) decides from CS.RPL like any other vector: CS.RPL 3 proves that
   `GS_BASE` holds the user base and `KERNEL_GS_BASE` this CPU's `PerCpu`, and user code cannot
   write `KERNEL_GS_BASE`. Its stub runs `swapgs`, copies the hardware frame from the IST stack into
   the thread's user frame (above), completes the frame, and continues on the thread's kernel stack.
   From there it is an ordinary entry from user mode: its body may block where §2.9 allows, and it
   returns through the common return to user mode, exit work included, except that an NMI keeps IF=0
   and skips the exit work. A `#MC` body keeps IF=0, takes no lock, and never blocks, from ring 3
   too, since a broadcast machine check holds every CPU in its rendezvous (ROADMAP §25.1); it
   records what it found in the thread and leaves the rest to the exit work of rule 11, where
   ROADMAP §25.3's recovery runs. The IST exit (restore the GS state found, then `iretq` on the IST
   stack) serves only CPL-0 frames and `#DF`. So while a user thread is in the kernel its user GS
   base is in `KERNEL_GS_BASE`, however it entered, and the context switch reads it there (ROADMAP
   §18.3). Linux's x86_64 entry splits the IST vectors the same way. Rule; not yet enforced: ROADMAP
   §10.6 (F005, F007); the IST handlers decide from CS.RPL at every CPL and run their bodies on the
   IST stack. The sign test fails once FSGSBASE lets userspace write a kernel-half GS base. Planned
   (ROADMAP §18.3, F133): with FSGSBASE on, a CPL-0 IST entry and `#DF` save `GS_BASE` with
   `rdgsbase`, load this CPU's `PerCpu` pointer, and restore the saved value on exit. A CPL-3 entry
   keeps its `swapgs`: the save-and-load protocol would leave `KERNEL_GS_BASE` holding the `PerCpu`
   address while the thread is in the kernel, and a switch from the moved body would save that
   address as the thread's GS base.
4. IF=0 from the first instruction of a return-to-user sequence to its `sysretq` or `iretq`: the
   sequence loads the user RSP before its last instruction, and the GS base is the user's for part
   of it. The exit keeps its own state in the thread's user frame (above), never in per-CPU scratch:
   `PerCpu.syscall_scratch` holds only the user RSP between `syscall` and the entry's stack switch,
   as in Linux. Required: the syscall exit runs `cli` right after `call vibeos_syscall_stub`;
   `enter_user` and `enter_user_full` run `cli` before `mov gs`; each checks IF=0 in debug builds;
   `iretq` restores ring 3's IF from the frame. Rule; not yet enforced: ROADMAP §10.6 (F001, F006).
   The syscall exit has no `cli`, and `console_init::wait_key` returns with IF=1 from its `sti; hlt`
   (F001); `enter_user_full` runs with IF=1 (F006); `syscall_init::run_user` runs `cli` before
   `enter_user`. The syscall exit also stages the return value and the `iretq` frame in
   `PerCpu.syscall_scratch`, per CPU, not per thread (ROADMAP §10.6, the user-frame box).
5. Every interrupt and exception entry clears RFLAGS.AC before any other code: an interrupt gate
   clears IF and TF but not AC, and ring 3 can set AC with `popf`. The syscall entry clears AC
   through FMASK (bit 18). Rule; not yet enforced: ROADMAP §10.6 (F088); no interrupt or exception
   entry runs `clac`. Planned (ROADMAP §10.6):
   `stac` appears only inside the user-memory accessors.
6. A handler on an IST stack does not block and does not switch threads: a nested entry of the same
   vector restarts at the top of that IST stack and overwrites the first frame. It also takes no
   lock ([§2.2](#22-interrupt-handler-rules)'s last row). An IST vector taken at CPL 3 leaves the
   IST stack before its body runs (rule 3), so this rule binds CPL-0 frames and `#DF`. Once IST
   handlers return (ROADMAP §10.6's CPL-3 `#DB`, §25.3's machine-check recovery, §25.5's NMI
   requests), three more things hold: an NMI handler takes no fault and runs no `iretq` before its
   own, since either unblocks NMIs while its IST frame is live (ROADMAP §25.5, F139); NMI, `#MC`,
   and `#DB` entries save DR7 and clear it before anything else (ROADMAP §18.4); and `#DF` never
   returns. Holds today because every IST handler halts, except that under `kernel_tests` an armed
   `catch` steps RIP and returns or longjmps off the IST stack. Planned (ROADMAP §10.6, F005): a
   CPL-3 `#DB` calls `try_user_fault` (the ring-3 `#DB` kill §5.2 requires) only after rule 3's
   move, because `try_user_fault` ends in `finish_exit`, which can switch threads.
7. Every gate but `#BP` is DPL 0, so `int n` from ring 3 raises `#GP`; the `#BP` gate is DPL 3, so
   `int3` delivers `SIGTRAP`. RFLAGS.TF and `int1` (`0xF1`) reach `#DB` at any DPL. Rule; not yet
   enforced: ROADMAP §10.6 (F148). Every gate is DPL 0 today (`IdtEntry::interrupt`, type `0x8E`).
8. Ring 3 never halts the kernel: §2.5 states the rule, and the §5.2 table gives each vector's ring-3
   action, §11.5's each aarch64 exception class's. Rule; not yet enforced for each §5.2 row whose last column names a ROADMAP line.
9. An entry stub saves the exception's syndrome into its frame before anything can turn IF on or
   raise another fault on that CPU, on both architectures: CR2 for `#PF`, and DR6 for `#DB`, which
   it then clears, as Linux does, before a CPL-3 `#DB` frame leaves the IST stack; ESR_EL1 and
   FAR_EL1 (ESR_EL2 and FAR_EL2 at EL2 with VHE, through the same encodings) for every synchronous
   exception on aarch64, before the vector unmasks any DAIF bit. A body reads them from the frame,
   never from the register: once IF is on, a switch to a thread that faults overwrites them
   ([§2.9](#29-preemption-and-interrupt-state) rule 3). On x86_64 only an NMI, `#MC`, or `#DB` can
   run between the delivery and the save, and none of their handlers takes a page fault (ROADMAP
   §25.5 for the NMI handler, which a debug build checks by comparing CR2 at its exit with its value
   at entry). Rule; not yet enforced: ROADMAP §10.6 (the generated stubs) and §11.3 (the aarch64
   vectors). Today `arch::idt::page_fault` reads CR2 in its body, which is correct only because
   every fault body runs with IF=0.
10. Return state, on both architectures. A saved user frame that anything other than an entry from
    user mode wrote (`rt_sigreturn`, ptrace's `SETREGS`, `SETREGSET` of `NT_PRSTATUS` or
    `NT_PRFPREG`, `POKEUSER`, and any later writer of a saved context) passes one validator per
    architecture before the thread next returns to user mode, so no writer can return a thread to
    ring 0, EL1, or EL2, or to user mode with interrupts masked. The validators are pure code in each
    port's `vibeos-core` half ([§11.1](#111-the-seam)), host-tested field by field, and each writer
    gets Linux's outcome for a value it refuses.
    - x86_64: CS and SS reach `iretq` only with RPL 3. Ptrace returns `EIO` for a CS or SS that is
      zero or whose RPL is not 3, as Linux does; ROADMAP §13.8 says what `rt_sigreturn` does with a
      frame's CS and SS. A selector with RPL 3 whose descriptor `iretq` refuses raises `#GP` on the
      return-to-user `iretq`, which rule 2 turns into `SIGSEGV`, and `sysretq` runs only when CS and
      SS are the user selectors (the user frame, above). RFLAGS takes only the user-settable bits
      from a writer, so IF stays set and IOPL, NT, and VM stay clear. RIP may hold anything: the
      exit sends a non-canonical RIP, or one at or above `USER_MAP_END`, to `SIGSEGV` (rule 2).
      `fs_base` and `gs_base` stay below `USER_MAP_END` (ROADMAP §17.4), and the FP image passes
      ROADMAP §13.8's checks.
    - aarch64: SPSR keeps from a writer only N, Z, C, and V, the feature bits DIT, SSBS, TCO, and
      BTYPE, and the mode and D, A, I, and F fields, which it then checks; every other bit is
      cleared, so SS and IL never come from a writer, and only the kernel sets SS, for a thread it
      single-steps. A mode other than AArch64 EL0t, or any of D, A, I, or F set, is refused:
      `rt_sigreturn` delivers `SIGSEGV` and `SETREGSET` of `NT_PRSTATUS` returns `EINVAL`, as on
      Linux arm64. The reserved bits of FPCR and FPSR are cleared. PC and SP may hold anything: an
      EL0 fetch outside the user half, or from a kernel page, takes an instruction abort that
      becomes `SIGSEGV`, which rests on every kernel mapping carrying UXN
      ([§11.2](#112-address-space-on-aarch64)).

    Rule; not yet enforced: ROADMAP §13.8 and §17.4 build the writers and run the validators; no
    writer exists today.
11. Exit work, on both architectures. The last check for work pending on a return to user mode (a
    signal to act on, a reschedule, and any work a later ROADMAP line queues for that return, such
    as ROADMAP §25.3's machine-check recovery) runs with IF=0, and IF stays 0 from it to the
    `sysretq`, `iretq`, or `eret`. When the check finds work, the exit turns IF on, does the work,
    turns IF off, and checks again; rule 4's IF=0 stretch starts at the check that finds none. The
    §7.5 FP load runs after that check, still with IF=0. Whatever makes work pending for a thread
    that may be running on another CPU publishes the work first and then sends that CPU the
    reschedule IPI (an SGI on aarch64): the IPI either arrives before the target's last check, which
    then sees the work, or stays pending across the IF=0 exit and is taken in user mode at once,
    where its own exit runs the check. A debug build asserts IF=0 at the check. A check made with
    IF=1 and followed by the `cli` lets the IPI be taken between the two, and the thread returns to
    user mode with the work undone until the next tick. Rule; not yet enforced: ROADMAP §10.6
    (F033). Today pending signals are acted on only at syscall entry and after the `wait4` sleep.
12. Signal-handler entry, on both architectures. Delivery saves the interrupted context (the user
    frame above, after any syscall-restart rewind) and its FP state into Linux's signal frame on the
    user stack (ROADMAP §13.8), then rewrites the user frame so the return to user mode enters the
    handler. On x86_64: `rip` is `sa_handler`; `rsp` points at the frame, whose first word is
    `sa_restorer`, the handler's return address; `rdi` holds the signal number, `rsi` the frame's
    `siginfo`, `rdx` its `ucontext`, and `rax` 0; `cs` and `ss` are the user selectors; DF, TF, and
    RF are clear in `rflags`. The thread's FP state becomes the constant initial image
    ([§7.5](#75-per-cpu-data)), a write under the FP binding, so a handler that interrupts code
    running under `std` or with a changed MXCSR starts from the psABI's state. On aarch64: `x0`
    holds the signal number and, under `SA_SIGINFO` only, `x1` and `x2` the `siginfo` and
    `ucontext`; `sp` points at the frame and `x29` at its frame record; `x30` is `sa_restorer` under
    `SA_RESTORER` and otherwise the vDSO's `__kernel_rt_sigreturn` (ROADMAP §13.8, §13.10); `pc` is
    `sa_handler`; `PSTATE.BTYPE` is 0 (`BTYPE_C` once ROADMAP §18.9 turns BTI on) and `PSTATE.TCO`
    is 0; V0-V31, FPCR, and FPSR keep their interrupted values. `rt_sigreturn` restores the saved
    context, TF included, after ROADMAP §13.8's validation. A thread that a tracer is
    single-stepping reports a step stop at the handler's first instruction (ROADMAP §17.4). The
    frame write may fault and sleep, so it runs where [§2.9](#29-preemption-and-interrupt-state)
    rule 4 allows, and the exit's last check stays at IF=0 (rule 4). A signal that cannot be
    delivered, because its frame cannot be written or an x86_64 handler lacks `SA_RESTORER`, is
    dropped and replaced by a forced `SIGSEGV`, as Linux's `force_sigsegv` does: if the dropped
    signal is `SIGSEGV`, `SIGSEGV` is reset to its default action, unblocked, and delivered, so the
    process dies; otherwise `SIGSEGV` is unblocked, reset to its default action only if it was
    blocked or ignored, and sent, so a `SIGSEGV` handler on an alternate stack still runs. The exit
    work never retries a failed delivery. A synchronous fault signal (`SIGSEGV`, `SIGBUS`, `SIGILL`,
    `SIGFPE`, or `SIGTRAP` raised by the thread's own instruction) carries its `siginfo` in a
    per-thread slot filled at the fault, never in an allocated queue entry, so running out of memory
    cannot drop or delay it (AGENTS.md rule 4); Linux allocates one and can lose the `siginfo`, so
    the slot is stricter than Linux and the same while memory lasts. Planned (ROADMAP §13.8, §17.4):
    no handler is delivered today.

---

# 6. Time

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
| HPET main counter | Preferred TSC calibration reference. Monotonic, known frequency from the ACPI table. Planned (ROADMAP §10.3): the clocksource when the TSC is not invariant (§6.4). |
| ACPI PM timer | Planned (ROADMAP §10.3): the clocksource with neither an invariant TSC nor an HPET (§6.4). 3.579545 MHz, 24 or 32 bits, at the FADT's `X_PM_TMR_BLK`. |
| TSC | Sub-millisecond timestamps, `busy_wait_ms`, deadline arithmetic. Planned (ROADMAP §10.3): the clocksource when invariant (§6.4). |
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

Planned (ROADMAP §19.4): `sleep_ms` becomes a wrapper over a sleep to a nanosecond deadline on
[§6.5](#65-timers-and-timeouts)'s deadline timers, since every blocking primitive already takes a
nanosecond deadline.

`now_us` reads two values that an interrupt handler writes: the tick counter and the TSC snapshot at
that tick. Read them unprotected and you eventually get one from before an interrupt and one from
after, producing a timestamp that goes backwards. That is not hypothetical; it happened.

Publish them as a latched seqlock, which keeps two copies of the fields so that no reader waits for
the writer. The writer bumps the sequence and issues `fence(Release)`, stores copy 0, bumps the
sequence and issues `fence(Release)` again, and stores copy 1. The reader loads the sequence with
Acquire, loads the copy its low bit names (copy 1 while it is odd, when the writer is storing copy
0), issues `fence(Acquire)`, reloads the sequence, and retries only when it changed. A plain seqlock
reader retries while the sequence is odd, so one that interrupted the writer on its own CPU, in an
NMI, `#MC`, or `#DB` handler, a pseudo-NMI (ROADMAP §25.5), or the panic path, would spin forever;
the latched reader reads the copy the writer is not storing and returns. So `now_ns` may be read
from any context, a log record's timestamp included (§2.5). Linux's NMI-safe clock,
`ktime_get_mono_fast_ns`, is built the same way. Rule; not yet enforced: `TickClock` keeps one copy,
and `TickClock::write` has no release fence after the odd bump, and the release half of its
`fetch_add(AcqRel)` orders only earlier accesses. x86's locked `fetch_add` is a full barrier and
hides that; aarch64 with LL/SC atomics does not (ROADMAP §10.8, F098).

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

Option 2 is the destination, generalized: one clocksource, a free-running counter chosen at boot
that every CPU reads, and `now_ns = base_ns + ((read() - base_cycles) mod 2^width) × mult >> shift`.
The tick drives scheduling and timer expiry, and no count of timer interrupts enters the clock.
Candidates, best first:

| Clocksource | Architecture and condition | Width | A read |
|---|---|---|---|
| TSC | x86_64, when CPUID reports it invariant and ROADMAP §10.7's warp test saw no backward step | 64 | an instruction |
| KVM clock, Hyper-V reference page | x86_64 under that hypervisor (ROADMAP §21.4, §26.5) | 64 | a shared page and an instruction |
| HPET main counter | x86_64 with an ACPI HPET table | 32 or 64 | an MMIO load: an exit under KVM, QEMU's global lock under TCG |
| ACPI PM timer | x86_64 with the FADT's timer block | 24 or 32 | a port read: an exit under KVM |
| `CNTVCT_EL0` | aarch64, always | at least 56 | an instruction after `isb` |

A counter narrower than 64 bits is read at least once per half wrap. CPU 0's tick does it; once idle
stops the tick (§6.6), the CPU that holds that duty hands it on before it stops its own tick, and no
idle CPU sleeps past half the wrap, the bound Linux calls `max_idle_ns`. The boot marker
`time: clocksource <name>` names the choice. The HPET and the PM timer cost an exit per read on KVM
without an invariant TSC until the paravirtual clock lands; ROADMAP §19.3 measures the cost, and if
it shows, those two read once per tick and interpolate from the TSC, which still loses no time when
ticks coalesce. Planned: ROADMAP §10.3 (F027) moves x86_64 `now_ns` to this model, and §11.3 gives
aarch64 the same function. Until then option 1 stalls and lags: after a CPU 0 IF-off stretch or deep
idle, the `LAST_NS` clamp repeats one value until the interpolation catches up, and time then stays
behind the TSC by the lost ticks. Trace timestamps are separate: ROADMAP §10.7's records carry raw
cycle-counter reads, which order records across CPUs only when the TSC is invariant and the warp test
saw no backward step.

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
  reassembly timeouts, and block request deadlines ([§10.3](#103-failure)). Both structures are
  intrusive: a timer's links live in the object that owns it, so arming, re-arming, and cancelling
  allocate nothing.
- A deadline timer whose action is a wake or a signal expires in the timer interrupt's top half
  ([§2.2](#22-interrupt-handler-rules)). The expiry takes the timer off its base under the base's
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
  [§7.7](#77-locking-with-more-than-one-cpu)'s owner-only rule, because TCP re-arms a connection's
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
- Every blocking operation takes an optional deadline. A blocked thread with no timeout and no waker
  is a permanent leak, and the only way to find one is to have made timeouts mandatory from the start.
- The blocked-thread sweep in `schedule_inner` (every `SWEEP_TICKS` ticks, for timeouts at least
  `OVERDUE_NS`, 5 s, late) never reports: `pop_expired_into` has already drained every expired
  timeout when `overdue(now)` runs (ROADMAP §10.7, F111).

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

---

# 7. SMP

Discovering the other cores through ACPI, starting them, and then keeping the kernel correct with more
than one of them running. This is where every latent locking mistake turns into a hang.

We bring up APs ourselves rather than using Limine's SMP request. Limine's version works, but the
trampoline, the INIT/SIPI dance, and the per-CPU handoff are the interesting part of the problem, and
owning it is required for CPU offlining and for a future non-Limine boot path.

Not a goal: CPU hotplug. CPUs are enumerated once at boot. §7.1 to §7.4 are x86_64's discovery and
bring-up; aarch64 reads the device tree (ROADMAP §11.5) and starts cores through PSCI (ROADMAP §11.4),
and §7.3 gains its aarch64 paragraph there. §7.5 to §7.11 hold on both architectures, and §7.5's
per-thread table lists each one's state.

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

Planned (ROADMAP §11.5): what these tables describe fills the portable machine description
([§11.1](#111-the-seam)), which SMP bring-up, the IRQ layer, and the device registry read, and
aarch64's device tree fills the same description.

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
| `0xC000_0081` | `IA32_STAR`. `SYSCALL` loads CS `0x08`; `SYSRET` base `0x10` gives user SS `0x1B` and CS `0x23`. Planned (ROADMAP §10.6): base `0x23`, giving SS `0x2b` and CS `0x33`, as on Linux ([section 5.1](#51-gdt-and-tss)). |
| `0xC000_0082` | `IA32_LSTAR`. `vibeos_syscall_entry`. |
| `0xC000_0084` | `IA32_FMASK`. `0x47700`: `SYSCALL` clears TF, IF, DF, IOPL, NT, and AC. |
| `0xC000_0100` | `IA32_FS_BASE`. User TLS base, written by `enter_user`, `enter_user_full`, and `execve`; not switched per thread ([section 7.5](#75-per-cpu-data)). |
| `0xC000_0101` | `IA32_GS_BASE`. The `PerCpu` address in ring 0; the user GS base (0) in ring 3. |
| `0xC000_0102` | `IA32_KERNEL_GS_BASE`. The inactive GS base: the `PerCpu` address while the CPU runs ring 3, the user GS base after an entry `swapgs` ([section 7.5](#75-per-cpu-data)). |

## 7.3 AP trampoline

An AP comes out of SIPI in real mode at `CS:IP = vector<<8 : 0`, so the entry point must be a 4 KiB
aligned physical page below 1 MiB. `boot::capture` chooses the page from the memory map it already
reads: the lowest 4 KiB page above frame 0 and below 1 MiB that the map marks usable, recorded in
`BootInfo`, so `pmm_init` excludes it and `smp_init` starts APs on that one value. The SIPI vector
is its page number; vectors `0xA0` to `0xBF` are reserved, and no usable page lies there on a PC.
With no such page every AP is skipped with `vibeOS: smp: no trampoline page`, as the CR3 check below
skips them. Under SeaBIOS the pinned Limine types `0x1000`–`0x52000` bootloader-reclaimable and the
page is `0x52000`; under OVMF `0x0`–`0x87000` is usable and it is `0x1000`. Linux likewise reserves
its real-mode trampoline from memory below 1 MiB at boot. Rule; not yet enforced: `pmm_init` and
`smp` each define `0x8000`, and `smp_init` copies the blob there without reading the memory map,
which types that page bootloader-reclaimable under SeaBIOS (ROADMAP §10.6).

The trampoline is `src/arch/trampoline.S`, assembled with `global_asm!` into `.trampoline` (inside
`__rodata_start..__rodata_end` so the kernel map covers the copy source) and copied to the chosen
page. It goes: real mode, set up a GDT, enable protected mode, load CR3 from the param block, set
`EFER.LME` and `EFER.NXE`, enable paging, long jump to 64-bit, load the stack, call the Rust entry
point. On the way it clears `CR0.CD` and `CR0.NW` (then `wbinvd`), sets `CR4.PAE` and `CR4.PGE`, and
sets `CR0.PG` and `CR0.WP`. The blob runs at whichever page boot chose, from `0x1000` to `0x9F000`:
its real-mode code addresses itself through CS (DS = CS, offsets from the blob's start), and
`smp_init` patches its absolute operands (the protected-mode and long-mode entry addresses, the GDT
base, and the parameter block's address) from the page's base when it copies it. Rule; not yet
enforced: the blob sets DS to 0 and takes absolute addresses from `.set BASE, 0x8000`, which hold
only below 64 KiB (ROADMAP §10.6).

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

- Write each field with `write_volatile`, so the compiler neither merges nor drops a write, and start
  the AP only through the IPI send, which orders the writes before the SIPI in either APIC mode
  ([section 7.6](#76-ipis)). A plain `copy_nonoverlapping` followed by a send with no barrier lets the
  compiler move the copy past the write that starts the AP, and the AP then reads uninitialized
  parameters. This one is invisible in debug builds.
- The trampoline page must be identity mapped and executable. The AP starts in real mode and touches
  the page's physical address directly, so this cannot go through the physmap. Reserve the frame in
  the PMM forever, even after all APs are up.

## 7.4 AP bring-up sequence

The BSP starts APs one at a time. They share the trampoline page, its parameter block, and nothing
else, so two concurrent SIPIs corrupt each other's stack pointer.

For each enabled APIC ID that is not the BSP:

1. Allocate the AP's per-CPU area and its guarded stack. Publish the per-CPU table entry with a
   Release store before sending anything, so the AP can find itself; the INIT and SIPI sends order it
   ([section 7.6](#76-ipis)).
2. Patch the trampoline parameters ([section 7.3](#73-ap-trampoline)).
3. Send INIT. Wait 10 ms, the Intel-specified minimum.
4. Send SIPI. Wait ~1 ms. Send SIPI again. Some hardware needs the second one; sending two is harmless.
5. Wait for the AP to set its ready flag, with a 3 second timeout.
6. On timeout: send INIT to that APIC ID, clear its online bit, log the failure, and continue with the
   remaining CPUs. Leak the AP's stack, GDT/TSS and IST stacks, `PerCpu` slot, and idle TCB: an AP that accepted a
   SIPI and then stalled past 3 s can keep running on them, or read the next AP's parameter block, and
   the BSP cannot tell it from an AP that never started. `smp_init::start_one` breaks this rule: it
   frees the stacks and GDT/TSS and marks the idle TCB Dead, with no INIT, and does not clear the
   online bit (ROADMAP §11.4, F032).

On the AP side (`smp_init::ap_entry`), in order: `cli`; load the per-CPU GDT and TSS; set `GS_BASE`
and `KERNEL_GS_BASE` (`per_cpu_init::install_gs`); program the syscall MSRs and the FPU bits and set
RSP0 (`syscall_init::init_ap`); load the shared IDT; `arch::cpu::harden` (SMEP, SMAP, and UMIP where
CPUID allows); enable the LAPIC; copy the BSP's `tsc_per_ms` and timer mode into `PerCpu`; arm the
LAPIC timer with the BSP's calibration (`apic_init::arm_ap`); mark the CPU online and print
`vibeOS: sched: cpu<i> ready`; publish the ready flag; `sti`; enter the idle loop.

`GS_BASE` must be set before any `lidt` and before `sti`. NMI and timer IRQs both
read per-CPU state through `gs:[0]`. Setting it after `lidt` is a null dereference
waiting for a non-maskable interrupt, even with IF off.

Planned (ROADMAP §20.2): S3 resume reuses this sequence instead of keeping its own. The AP-side
setup above, from loading the per-CPU GDT and TSS through arming the LAPIC timer, is one routine.
Before S3, ROADMAP §19.6's offlining parks every CPU but the BSP, which points the FACS waking vector
at the trampoline page (§7.3) and saves its context. On wakeup the firmware enters the trampoline in
real mode on the BSP, which reaches long mode on the kernel CR3 and returns into that context, reruns
the routine, clearing the TSS descriptor's busy bit before `ltr` because the descriptor in memory is
still marked busy, re-bases the clocks (§6.6), reprograms the platform state the wakeup reset, and
brings each AP back through §19.6's online path, which is steps 2 to 5 above on the AP's existing
per-CPU area. Why: a wakeup loses every register the kernel set (QEMU resets the machine on a `q35`
wakeup, and firmware restores only what its own S3 script saved), and with one routine a register
added to bring-up, such as ROADMAP §18.3's mitigation MSRs or §20.1's x2APIC mode, is restored at
resume with no second edit. Rejected: a resume path with its own register list, which drifts from
bring-up.

## 7.5 Per-CPU data

One `PerCpu` struct per CPU. In ring 0, `GS_BASE` holds its address. While the CPU runs ring 3,
`KERNEL_GS_BASE` holds it and `GS_BASE` holds the user GS base (always 0). `enter_user` and
`enter_user_full` set that state, each `swapgs` exchanges the two, and `per_cpu_init::init_bsp`,
`per_cpu_init::install_gs`, and `arch::gs::force_kernel` set both MSRs to the `PerCpu` address.

`self_ptr` sits at offset 0 so `gs:[0]` yields the struct address, which is how a `&PerCpu` is obtained
without knowing which CPU you are on. Taking that reference is legal only with IF=0, and the
reference dies with the IF=0 stretch ([§2.9](#29-preemption-and-interrupt-state) rule 5). `current`
is never read through it: `arch::current_tcb()` loads `current` with one `gs`-relative instruction,
and `arch::cpu_id_hint()` loads `cpu_id` the same way for callers that tolerate a stale id. Rule;
not yet enforced: ROADMAP §10.3 (F039).

Contents (`src/per_cpu.rs`):

- `self_ptr`, logical CPU id, APIC id
- `current`, `idle`, and `idle_id`
- `runq`, this CPU's ready FIFO (owner only, IRQs off), and `ready_head`, a copy of its head that
  only an in-guest test reads (ROADMAP §10.7 deletes it, F111)
- `wake_inbox`, a `u64` `ThreadId` bitset: a remote CPU ORs in a thread's bit and sends IPI `0xFD` (planned: a bitmap sized from the limits, §7.6)
- `irq_nest`, tick and switch counts, `slice_tsc`, `idle_tsc`, and `switch_scratch`, a `CpuContext` that no code reads or writes
- `tsc_per_ms` (a copy of the BSP's value, [section 6.2](#62-calibrating-the-tsc)) and `timer_mode`
- `ready`, the flag an AP sets last in bring-up ([section 7.4](#74-ap-bring-up-sequence))
- `kernel_rsp0` and `as_cr3`, which the context switch updates; `tss`, through which it writes TSS.RSP0; and `fallback_rsp0`, the RSP0 it uses for a thread without `Tcb.stack` (below)
- `syscall_scratch`: the user RSP, the syscall return value, and the `iretq` RIP, RFLAGS, and RSP.
  It is per CPU, not per thread, so it is valid only while IF=0. The syscall exit breaks this: it
  runs without a `cli`, and a console `read` that waited in `sti; hlt` returns to it with IF=1
  (ROADMAP §10.6, F001). Planned (ROADMAP §10.6): it shrinks to one word, the user RSP between
  `syscall` and the entry's stack switch; the exit keeps the return value and its `iretq` frame in
  the thread's user frame ([section 5.10](#510-privilege-transitions)).

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
(F006). ROADMAP §10.6 closes these for the current kernel (an IST vector taken at CPL 0, and `#DF`,
decides from the sign of `GS_BASE`, one taken at CPL 3 swaps by CS.RPL and moves to the thread's
kernel stack, a `#GP`, `#NP`, or `#SS` on a labeled user-return `iretq` becomes `SIGSEGV`, and
`enter_user_full` runs `cli` before its `mov gs`) and §18.3 for FSGSBASE, where a user can load a
kernel-half base. See [section 9.3](#93-interrupts).

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
user-visible CPU state; the commit that adds it also adds its row here. The Arch column names the
port a row belongs to; the aarch64 rows are planned (ROADMAP Phase 11), and there `switch_now` calls
that port's `on_switch` and `switch_context`. A control that holds one value for every thread, such
as `CR4.TSD` or `SCTLR_EL1.UCT`, is a row of §11.4's table instead.

| Arch | State | Saved in | Switched by | Status |
|---|---|---|---|---|
| x86_64 | `rbx`, `rbp`, `r12`–`r15`, RSP, RIP | `Tcb.context` (`CpuContext`) | `switch_context` | switched |
| x86_64 | RFLAGS | `CpuContext.rflags`; IF comes from `irq_nest` (`apply_if_on_resume`) | `switch_context` | switched |
| both | `irq_nest` | `Tcb.irq_nest`, swapped with `PerCpu.irq_nest` | `switch_now` | switched |
| x86_64 | user GPRs, RIP, RSP, RFLAGS, CS, SS, and the original syscall number | the thread's user frame at the top of `Tcb.stack` ([section 5.10](#510-privilege-transitions)), saved by every entry from ring 3 | the RSP0 switch, which gives each thread its own entry stack | switched. Rule; not yet enforced: ROADMAP §10.6. The syscall entry saves a 16-word frame whose `rcx` and `r11` slots double as RIP and RFLAGS, and an interrupt or exception entry saves only what its `x86-interrupt` handler clobbers, so a thread preempted in ring 3 has `rbx`, `rbp`, and `r12`-`r15` in spill slots the compiler chose |
| x86_64 | x87, SSE, MXCSR | `Tcb.fpu`, a 512-byte FXSAVE image | the FP binding below: `fxsave64` at the switch away from a thread whose state is live, `fxrstor64` in the return to ring 3 when the registers hold another thread's state | switched. Rule; not yet enforced: the binding lands in ROADMAP §10.6. Today `switch_fpu` in `on_switch` runs `fxsave64` for the old thread and `fxrstor64` for the new on every switch, and the syscall entry and exit also save and restore it. `fork` gives the child `fpu_template()`, not the parent's image, and `execve` keeps the old image's registers (ROADMAP §10.6, F069). The template is captured after `fninit`, which resets only the x87 control, status, and tag words, so MXCSR and the XMM and ST registers hold whatever the loader left (ROADMAP §10.6, F129). FXSAVE covers no XSAVE state; `CR4.OSXSAVE`, `CR4.PKE`, and `EFER.FFXSR` are assumed clear and never asserted (ROADMAP §11.1, F130). |
| x86_64 | RSP0 | TSS.RSP0 and `PerCpu.kernel_rsp0`: the top of `Tcb.stack`, or `fallback_rsp0` for the bootstrap thread | `set_rsp0_for` in `on_switch` | switched |
| x86_64 | CR3 | `Tcb.as_cr3` (0 means the kernel PML4) | `switch_cr3_for` in `on_switch`, skipped when unchanged | switched; no PCID (ROADMAP §18.3 adds it with §7.9's flush generation) |
| x86_64 | FS_BASE (user TLS) | not saved | nothing | not switched. `enter_user`, `enter_user_full`, and `execve` write it; `force_kernel`'s `mov fs` zeroes it on every exit or kill; `fork` copies the live MSR, so a child can inherit another process's base (ROADMAP §11.6, F022). |
| x86_64 | user GS base | not saved; always 0 | nothing | holds while no `ARCH_SET_GS` or FSGSBASE exists (ROADMAP §18.3); from then on it is per thread, and while the thread is in the kernel it is in `KERNEL_GS_BASE` whichever vector it entered by, IST vectors included ([section 5.10](#510-privilege-transitions) rule 3), where the switch away reads it |
| x86_64 | DR0-DR3, DR7 | the thread's decoded debug slots, and the tracer's masked DR7 for `PEEKUSER` | the switch, by the Debug state paragraph below | not built: nothing arms them before ROADMAP §17.4 |
| x86_64 | DR6 | the thread's virtual DR6 | not switched: the `#DB` body writes the thread's copy from the DR6 its entry saved (§5.10) | not built: ROADMAP §17.4 |
| x86_64 | DS, ES, FS, and GS selectors | the thread's own four, saved at the switch away | `on_switch`, which loads the incoming thread's four before it writes `FS_BASE` and `GS_BASE`, since a selector load can clear the matching base | Rule; not yet enforced: ROADMAP §10.6 ([section 5.1](#51-gdt-and-tss)). `enter_user` and `enter_user_full` load `0x1B` into all four and nothing saves them, so a selector ring 3 loads with `mov` is lost at the next switch |
| x86_64 | `PerCpu.syscall_scratch` | per CPU | not switched | valid only while IF=0 (above; F001); one word, the user RSP at entry, once ROADMAP §10.6 keeps the exit's state in the user frame |
| aarch64 | `x19`-`x29`, SP, LR | `Tcb.context` | `switch_context` (ROADMAP §11.4) | not built |
| aarch64 | DAIF.I and F | come from `irq_nest`, as on x86_64 | `switch_context` | not built |
| aarch64 | user `x0`-`x30`, SP, PC, PSTATE, `orig_x0`, and the syscall number | the thread's user frame at the top of `Tcb.stack` ([section 5.10](#510-privilege-transitions)) | every entry from EL0 saves it, and each return to EL0 leaves `SP_ELx` at the top of the thread's stack for the next entry | not built: ROADMAP §11.6 |
| aarch64 | `SP_EL0` | at EL0 the user stack pointer, saved in the user frame by every EL0 entry; at EL1 or EL2 the running thread's TCB pointer ([§2.9](#29-preemption-and-interrupt-state) rule 5) | the EL0 entry stub, the return to EL0, and the switch (ROADMAP §11.6) | not built |
| aarch64 | V0-V31, FPCR, FPSR | `Tcb.fpu` | the FP binding below | not built: ROADMAP §11.6 |
| aarch64 | `TPIDR_EL0` (user TLS) | the thread's saved TLS base, as for `FS_BASE` | `on_switch` saves it for an outgoing user thread and loads the incoming one's; EL0 writes it with `msr` at any time, so only the live register is current | not built: ROADMAP §11.6, which switches x86_64's `FS_BASE` in the same commit (F022); the fatal stack-overflow path uses it as scratch before it halts (§11.5 rule 6) |
| aarch64 | `TPIDRRO_EL0` | not saved; 0 | every CPU writes 0 at bring-up, and only the fatal stack-overflow path, which halts, writes it again (§11.5 rule 6) | EL0 can read it, so it never holds a kernel value |
| aarch64 | `TTBR0_EL1` and its ASID | the address space | the `TTBR0` switch in `on_switch`, skipped when the address space is shared (ROADMAP §11.6) | not built; ASIDs from ROADMAP §11.2; the ASID rides in the same TTBR0 write (§11.2) |
| aarch64 | `DBGBVR`/`DBGBCR`, `DBGWVR`/`DBGWCR`, `MDSCR_EL1.MDE` | the thread's decoded debug slots | the switch, by the Debug state paragraph below | not built: ROADMAP §17.4 |
| aarch64 | `MDSCR_EL1.SS` | the thread's step flag, with `SPSR.SS` in its frame | set at the return to EL0 and cleared at entry from EL0, by the Debug state paragraph below | not built: ROADMAP §17.4 |
| aarch64 | SVE and SME state | not saved | nothing | not per-thread: every CPU clears `CPACR_EL1.ZEN` and `SMEN`, so EL0 use gets `SIGILL` (ROADMAP §11.6); §23.1 adds their rows |
| aarch64 | pointer-authentication keys, BTI, and MTE tag-check state | not saved | nothing | off in `SCTLR_EL1` until ROADMAP §18.9 (pointer authentication, BTI) and §18.4 (MTE), which add their rows |

The FP binding is one rule on both architectures. A user thread owns FP state from creation:
`Tcb.fpu` starts as the constant initial image (the psABI's FCW `0x037F` and MXCSR `0x1F80` with
every register zero on x86_64; V0-V31, FPCR, and FPSR zero on aarch64), `fork` copies the parent's,
and `execve` rewrites it. `PerCpu.fp_owner` names the thread whose state this CPU's FP registers
hold, and `Tcb.fp_cpu` names the CPU that last loaded or held the thread's state. The registers hold
thread T's live state only when this CPU's `fp_owner` is T and T's `fp_cpu` is this CPU. The switch
away from a thread whose state is live saves it into `Tcb.fpu` and loads nothing. The return to user
mode checks the binding with IF=0 ([§5.10](#510-privilege-transitions) rule 4) and, when the
registers hold another thread's state, loads `Tcb.fpu` and sets both fields. A new thread starts
with `fp_cpu` empty, so a TCB that reuses a freed one's address never matches a stale `fp_owner`.
Code that writes a thread's `Tcb.fpu` (`execve`, `rt_sigreturn`, ptrace) empties its `fp_cpu`, so
the next return to user mode loads what was written; code that reads the running thread's (`fork`,
signal delivery, a core dump) first saves the live registers inside an `InterruptGuard`. The kernel
is soft-float and never makes FP state live; a firmware call that may use the registers (ROADMAP
§20.9) saves the live state and empties this CPU's `fp_owner` first. No FP or SIMD access traps:
`CR0.TS` stays clear, and on aarch64 every CPU sets `CPACR_EL1.FPEN` (`CPTR_EL2`'s field under VHE)
to `0b11` at bring-up and never changes it, so FPEN is not per-thread state. SVE and SME are the
exception: they trap at a thread's first use to size and set up their state, as on Linux, and their
enable bits are per-thread rows (ROADMAP §23.1). Rule; not yet enforced: ROADMAP §10.6 for x86_64
and §11.6 for aarch64; the row above gives the code as built.

Why: every user thread uses FP (x86_64 user code passes floats and copies memory in XMM registers,
and every aarch64 compiler emits NEON), so a first-use trap sets up nothing that creation could not,
and trap-driven switching saves nothing and is the scheme LazyFP (CVE-2018-3665) retired. Deferring
the load to the return to user mode skips it when a thread blocks and resumes on the same CPU with
only kernel threads in between. Linux runs this model on both architectures: x86's
`fpregs_state_valid` compares the per-CPU owner and the context's `last_cpu`, and arm64 keeps
`fpsimd_last_state` per CPU and `fpsimd_cpu` per thread, with FP access enabled once per CPU.
Rejected: saving at every syscall entry and restoring at every exit, as x86_64 does today (a
512-byte save and restore per syscall that a soft-float kernel does not need, and a second mechanism
beside the switch); loading at switch-in (it loads for threads that switch away again before
reaching user mode, and makes `execve` load mid-syscall); a first-use FP trap on aarch64 (FPEN
becomes per-thread state, and the trap path saves nothing); a binding on `fp_owner` alone (a thread
that ran on another CPU and returns to one whose owner still names it would run with stale
registers).

**Debug state.** One owner per build holds the hardware breakpoint and watchpoint slots and their
enables: DR0-DR3 and DR7 on x86_64, and on aarch64 the `DBGBVR`/`DBGBCR` and `DBGWVR`/`DBGWCR` pairs
with `MDSCR_EL1.MDE` and `KDE`. In every build but ROADMAP §18.4's data-race detector build, user
threads own them: the switch loads a thread's slots, with the DR7 the kernel built from its tracer's
decoded writes or with `MDE` set, only for a thread with one armed, and clears DR7 or `MDE`
otherwise; `KDE` stays 0, so on aarch64 the kernel takes no hardware debug exception from its own
code (a `brk` always traps); a tracer's values are decoded, never loaded (ROADMAP §17.4). In the
detector build the detector owns them on every CPU: the switch never writes DR7, `MDE`, or `KDE`,
ptrace's debug-register writes return `ENOSPC`, and `PSTATE.D` is clear in the kernel except in the
debug-exception and SError handlers and the entry and exit sequences. Single step belongs to the
thread in every build: on x86_64, TF lives in the thread's saved RFLAGS, and FMASK and the interrupt
gates clear it in the kernel; on aarch64, the return to EL0 sets `MDSCR_EL1.SS` after the last
exit-work check, only for a thread with `PTRACE_SINGLESTEP` armed, and every entry from EL0 clears
it before it clears `PSTATE.D`, so `SS` is never 1 in EL1 while D is clear, nor at an `eret` to a
thread that is not being stepped. DR6 is per thread and virtual: ptrace reads and writes the
thread's copy, which the `#DB` body fills from the DR6 its entry stub saved (§5.10). ROADMAP §11.4
releases the OS Lock on every aarch64 core. Planned: nothing arms a debug slot before ROADMAP §17.4.

Another thread reads or writes a thread's saved per-thread state (any row of this table, the user
frame of [section 5.10](#510-privilege-transitions) included) only while that thread is held
stopped, in a ptrace stop (ROADMAP §17.4) or parked for a core dump (ROADMAP §13.8), and only after
an Acquire load has seen the thread's `on_cpu` flag (ROADMAP §10.10) clear. Stopping is not enough:
a stopped thread wakes its tracer before its CPU has switched away from it, and until then some rows
exist only in that CPU's registers (the FP state under the binding above, and the user FS and GS
bases under FSGSBASE, ROADMAP §18.3). The switch away finishes every save in this table before it
clears `on_cpu` with a Release store, its last access to the outgoing thread
([§2.8](#28-publish-last)). The reader holds the stop for the whole access, as Linux's ptrace does:
nothing resumes the thread meanwhile, and a `SIGKILL` that arrives takes effect when the access
ends, so the thread cannot exit and free the kernel stack that holds its user frame under the
reader. A write follows the binding's rule above, so the thread's next return to user mode loads
what was written. Planned (ROADMAP §13.8, §17.4): nothing reads another thread's saved state today.

## 7.6 IPIs

| Vector | Purpose |
|--------|---------|
| `0xFB` | Call function. Run a closure on a target CPU, optionally waiting for completion. |
| `0xFC` | TLB shootdown. |
| `0xFD` | Reschedule. Target CPU re-evaluates its run queue, waking from `hlt` if idle. |
| `0xFE` | Panic stop, Fixed delivery: the IPI of §2.5 step 1. Today `ipi_init::halt_others` broadcasts it and does not wait, and a CPU spinning with IF=0 (in `SpinMutex::lock` or `wait_acks`) never takes it, so another CPU can write to COM1 during the dump. Planned (ROADMAP §10.7, F135): the dump owner sets each CPU's stop request word before the IPI, and a CPU spinning with IF=0 stops at its next `service_incoming` poll. |

The reschedule IPI is what makes cross-CPU wakeups work without ever locking a remote run queue: push
onto the target's inbox, send `0xFD`, done. `0xFD` then takes SCHED IRQ-off via `schedule_preempt`.
Shootdown and call-function work take no lock at all, since a CPU in a serviced spin runs them inside
whatever it holds ([§2.2](#22-interrupt-handler-rules)). Call-function uses one global slot; the
initiator holds IF off from publish through reclaim, polling inbound work while it waits.

The IPI send is the publication point. `send_ipi`, the seam's IPI send ([§11.1](#111-the-seam)),
orders every store its CPU made before the call ahead of the interrupt's arrival, so a handler that
takes the IPI and then reads a slot, an inbox, or a parameter block sees what the sender wrote. With
xAPIC the ICR write is an uncached store, which x86 does not reorder with earlier stores, so the
send needs only a compiler barrier before it. With x2APIC the ICR is MSR `0x830`, and a `WRMSR` to
an x2APIC register is not serializing (Intel SDM Vol. 3A, MSR access in x2APIC mode), so the send
runs `mfence` then `lfence` before it, as Linux's `weak_wrmsr_fence` does. It does so on every
vendor, though Linux skips it on AMD. With GICv3 the send runs `dsb ishst` before the
`ICC_SGI1R_EL1` write and `isb` after it (ROADMAP §11.7 cites the Arm ARM rule). With GICv2 the SGI
is an MMIO store to `GICD_SGIR`, which goes through the ordered `mmio_write` ([§4.7](#47-dma)),
whose `dmb oshst` puts the CPU's earlier stores ahead of it, as the `dmb ishst` before Linux's GICv2
SGI write does. INIT and SIPI go through the same send. Callers publish with a Release store or a
locked read-modify-write and add no fence of their own. Rule; not yet enforced:
`apic_init::send_ipi` has no barrier of its own, and the callers that fence (`smp_init::start_one`,
`ipi_init::shootdown_va`) do so before their publishing store, which is not enough under x2APIC
(ROADMAP §20.1).

Planned (ROADMAP §10.4): the wake inbox (§7.5) is a per-CPU bitmap of `AtomicU64` words sized from
the `limits` thread count, with one summary bit per word. A push is a Release `fetch_or` of the
thread's bit and then of its word's summary bit, and sends `0xFD`; a drain swaps the summary to zero
with Acquire and then swaps each flagged word to zero with Acquire. A push allocates nothing and is
idempotent, so a thread woken from two CPUs at once is queued once. Push and drain live in
`vibeos-core` with ROADMAP §10.8's model, and the `0xFD` handler calls the drain. Rejected: an
intrusive MPSC list, which needs a queued flag in each TCB against double insertion and a larger
model.

## 7.7 Locking with more than one CPU

The global lock order is in [section 2.1](#21-lock-order) and the one-spinlock rule in
[section 2.3](#23-locking-with-interrupts). Additions specific to SMP:

- Never lock a remote CPU's per-CPU state. Per-CPU locks are taken only by the owning CPU, with
  interrupts off. `per_cpu_init::with_cpu` can reach any CPU's slot ([section 7.5](#75-per-cpu-data),
  F039). Planned (ROADMAP §19.4): the one exception is a CPU's timer base
  ([§6.5](#65-timers-and-timeouts)), whose lock any CPU takes to arm, re-arm, or cancel a timer on
  it.
- A thread moves between CPUs only through the target's inbox, pushed by the CPU that owns the
  thread. Load balancing, a `sched_setaffinity` whose new mask excludes the CPU a queued thread waits
  on (ROADMAP §13.10; that CPU moves it on a reschedule IPI), and CPU offlining (ROADMAP §19.6) all
  move threads this way. Work stealing, if ROADMAP §19.4's numbers keep it, takes threads from a
  lock-free deque with a loom model, never from a locked remote run queue. No code locks two CPUs'
  run queues. Planned (ROADMAP §10.7): `lock::cpu_lock_order`, which only its own test calls, is
  deleted.
- A lock taken from an ISR is taken with interrupts disabled in every other context too. The scheduler
  lock is the canonical case: the timer ISR calls into the scheduler, so any holder with interrupts
  enabled deadlocks the moment its own timer fires.
- Serial TX takes a lock so bytes from different CPUs do not interleave. `Serial::write_fmt` keeps
  IRQs off for the whole line but takes the TX lock once per `write_str` piece, so another CPU can
  write between two pieces of a formatted line, and `log_fmt` sends a record and its newline as two
  writes. The harness then misses a contract line split that way. Planned (ROADMAP §10.2, F138):
  each line is formatted, newline included, into one buffer and written under one TX hold.
- klog records go to one global IRQ-safe log ring and to a serial sink that only try-locks TX.
  Per-CPU serial capture assembles serial output into lines for the ring. Planned (ROADMAP §19.5):
  one lockless ring any context may append to, and a printer thread per console (§2.5).
- The global SCHED lock (ROADMAP §19.4 splits it), one `SpinMutex` on the block cache, one VFS
  lock over lookups and namespace changes (§2.1), the log-ring TAS, and virtio-blk bounce copies are
  known scale limits; see ROADMAP §19.4, §19.5, and §19.8. So is the one bottom-half thread for every threaded vector,
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

Kernel threads get their scheduling class when they are created, from this table. Planned (ROADMAP
§19.4): the classes exist from §19.4; until then every thread is scheduled round-robin.

| Kernel thread | Class | Linux's counterpart |
|---|---|---|
| The per-CPU stopper ([§7.11](#711-cpu-offline-and-online)) and ROADMAP §25.5's per-CPU watchdog thread | Stop class, above every real-time priority | the stop class's `migration/N` threads |
| Threaded interrupt bottom halves ([§5.4](#54-irq-registration)), network receive included, and block error handlers ([§10.3](#103-failure)) | `SCHED_FIFO` 50 | threaded interrupt handlers |
| RCU's grace-period and boost thread ([§2.12](#212-rcu)) | `SCHED_FIFO` 1 | RCU's kthreads with boosting on (`rcutree.kthread_prio` 1) |
| The softirq-equivalent workers, one per CPU, which also run timeout-wheel callbacks ([§6.5](#65-timers-and-timeouts)) | Fair, nice -20 | `WQ_HIGHPRI` workqueue workers |
| Every other kernel thread: writeback, swap-out, ROADMAP §19.10's reclaim thread, §19.5's log printer, ordinary workqueue workers, driver probes | Fair, nice 0 | the same |
| Idle, one per CPU | Runs only when nothing else can | the idle task |

vibeOS defers all interrupt work to threads, as Linux does under `PREEMPT_RT`, so these classes
decide device and audio latency and what a flood of untrusted input can take. A user real-time
thread above 50 preempts bottom halves, as on Linux. Two rules keep the table from starving anyone:

- Network receive is budgeted. A queue's bottom half processes at most 300 packets or 2 ms per wake,
  whichever comes first, in batches of 64 (Linux's `netdev_budget`, its `netdev_budget_usecs` at a
  1 kHz tick, and the NAPI weight). Past the budget it keeps its queue's interrupt masked and
  continues in the fair class at nice 0 until its ring is empty, then returns to `SCHED_FIFO` 50 and
  unmasks the interrupt. This is Linux's hand-off to `ksoftirqd` without a second thread, so a
  receive flood shares its CPU instead of owning it.
- Real-time throttling (ROADMAP §19.4) applies to user real-time threads only. A kernel thread in the
  real-time class is never throttled, and in the share that throttling keeps from user real-time
  threads it runs ahead of fair threads. Linux throttles its interrupt threads with everything else,
  but its block and network completions run in hard-IRQ and softirq context, which no real-time
  thread can starve. vibeOS's run in threads, so without this rule a user `SCHED_FIFO` 99 loop on
  every CPU stalls every device completion. `docs/LINUX.md` records the difference; no interface
  changes.

Why: a bottom half in the fair class waits a slice behind CPU-bound work, and any user real-time
thread preempts it outright, so completions and audio position updates lag with load. At the top
real-time priority, one flooded receive queue owns its CPU. Rejected: running softirq work at
hard-IRQ exit with IF=1, as Linux without `PREEMPT_RT` does, which needs a bottom-half-disable
count, a second "not now" beside IF ([§2.9](#29-preemption-and-interrupt-state)); a separate per-CPU
receive worker for work past the budget, which adds a hand-off and a context for nothing the
demotion does not do; and leaving each subsystem to choose. If ROADMAP §19.4's measurement shows
timeout-wheel callbacks starting late under load, they move to `SCHED_FIFO` 1, as `PREEMPT_RT`'s
`ktimers` threads run.

The timeout queue starts global with one lock. ROADMAP §19.4 makes it per CPU, as
[§6.5](#65-timers-and-timeouts)'s two structures. A timer stays on the base it was armed on when its
thread migrates: its expiry only wakes the thread, and the wake goes through the inbox like any
other. A CPU going offline hands its timers to an active CPU ([§7.11](#711-cpu-offline-and-online)).

## 7.9 TLB shootdown

Kernel mappings are `GLOBAL` and therefore live in every CPU's TLB. Unmapping one requires every CPU
to invalidate before the virtual address or the frame behind it is reused
([§2.4](#24-memory-invariants)).

Protocol: update the PTE, then broadcast `0xFC` with the target address, then wait for acknowledgement
from every online CPU. The initiator reads the online mask inside the IF=0 section it sends and
waits in, which [§7.11](#711-cpu-offline-and-online)'s offline rendezvous relies on.

Planned (ROADMAP §12.3): user address spaces are targeted. Each address space records the set of
CPUs that may hold its translations. A CPU sets its bit in the address space it switches to before
it loads the root, with a full barrier between that store and the first walk through the new root;
x86's CR3 write serializes, and aarch64 uses `dsb ish` and `isb`. The initiator of a round stores the
PTE, runs a full barrier, and then reads the set, so either it sees the bit or the switching CPU's
walk sees the new PTE. A CPU clears its bit in the address space it leaves once the new root is
loaded: without PCID that load flushes the old space's entries, and with PCID the flush generation
below catches them. A round carries its target (an address space, or the kernel), a start address, a
page count or "all", and a freed-tables flag, and above 32 pages the handler flushes the whole
target. A user round goes to the CPUs in the target's set, a kernel round to every online CPU. On
aarch64 a round is one broadcast `tlbi ...is` batch and its `dsb ish`, and the set only counts.

Planned (ROADMAP §18.3): with PCID, entries tagged for an address space survive a switch away from
it. Each address space carries a flush generation, which every round increments before it reads the
set, and each CPU records, for each PCID slot, the generation it last synced. A switch into an
address space whose generation is newer flushes that PCID before any user code runs, and a round's
handler brings the CPU's current address space up to its generation. IPIs still go only to the CPUs
in the set.

Planned (ROADMAP §27.5, kept only if its measurement there shows a saving): lazy TLB. A kernel
thread may keep the previous address space's root loaded. Its CPU stays in the set, marked lazy, and
holds a core reference to the address space ([§2.11](#211-object-lifetimes)) until it loads another
root; the switch that ends the hold releases it through the deferred put, since the switch path
frees nothing. A round that changes only leaf PTEs skips lazy CPUs, which catch up through the flush
generation on their next switch to a user address space, the same one included. A round that frees
page-table pages also sends lazy CPUs an IPI on x86_64, because a lazy CPU's paging-structure caches
and speculative walks can still read a freed table. aarch64 sends no IPI: its broadcast TLBI
(`vae1is` for a freed table) reaches every CPU, and the core reference keeps the root page and its
ASID until the lazy CPU loads another root. Until then, a switch to a kernel thread loads the kernel
root, and on aarch64 points TTBR0 at the empty user root.

Calling contract, on both architectures. On x86_64 an invalidation may wait for every online CPU
with IF=0, so the TLB-maintenance operation of the §11.1 seam is called only where
[§2.9](#29-preemption-and-interrupt-state) rule 2 allows a cross-CPU wait: never in NMI, `#MC`, or
`#DB` context, never from a serviced shootdown or call-function handler, and never under the log-ring
TAS, whose waiters service no IPIs. The aarch64 implementation asserts the same contexts in debug
builds, although its broadcast waits for no peer, so work run first on aarch64 cannot add a deadlock
that only x86_64 hits.

The initiator waits with interrupts disabled (`InterruptGuard` around publish → IPI → ack → clear
waiters). A waiter with IF off cannot take an incoming shootdown as an IRQ, which would deadlock two
CPUs shooting down at once. The wait loop therefore calls `service_incoming` and processes pending
slots so a spinning initiator still helps its peers. This is not optional; it is the difference
between working and a hang that only appears under load.

The wait is bounded: `ipi_init::wait_acks` panics after 1000 × `tsc_per_ms` TSC cycles (1 s) without
every acknowledgement. A target CPU that holds IF=0 that long without polling `service_incoming`
makes a shootdown panic the kernel. The in-guest test runner holds IF=0 for the whole run,
and a syscall body runs with the IF=0 that FMASK set until it blocks (ROADMAP §10.10, F011); a console
`write` with many newlines is the long case (ROADMAP §10.6, F044). As built, one round invalidates one VA
(`shootdown_va`) on every online CPU; ROADMAP §12.3 replaces it with the rounds above. `kva_init::unmap_shootdown` unmaps at most 32 pages (`MAX_UNMAP`) and leaves the
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

- x2APIC (ROADMAP §20.1). MSR-based register access, no MMIO, and APIC IDs beyond 255. Its ICR
  `WRMSR` orders nothing before it, so the IPI send fences first ([section 7.6](#76-ipis)).
- Topology awareness (ROADMAP §19.4): cores, threads, packages, and cache sharing from CPUID leaf
  0x1F, so the scheduler can prefer a sibling core over a remote package.
- NUMA (ROADMAP §19.7). SRAT and SLIT parsing, per-node buddy allocators, node-local allocation
  policy.
- CPU offline and online for power management (ROADMAP §19.6):
  [section 7.11](#711-cpu-offline-and-online).

## 7.11 CPU offline and online

Planned (ROADMAP §19.6). Offlining parks a core for power management, and S3 and kexec use it too
(ROADMAP §20.2, §25.4). It is the reverse of bring-up, not hotplug: the set of CPUs is still fixed
at boot.

- One offline or online runs at a time, under the hotplug lock, an `RwLock` that offline and online
  hold for writing. Code that walks the online CPUs and may sleep between them holds it for reading.
  A thread takes it with no other lock held, so it ranks ahead of everything in §2.1.
- CPU 0 never goes offline, on either architecture, and a request to offline it is refused. The boot
  CPU runs S3's resume and kexec's jump, and Linux has not let x86 offline CPU 0 since 6.5.
- A CPU is active while new work may be placed on it. An active mask sits beside the online mask,
  and placement, wake targets, affinity changes, vector routes, and timer moves pick only active
  CPUs.
- Offline and online run the steps of an ordered registry, a small version of Linux's `cpuhp`
  states. A subsystem that adds per-CPU state registers an offline step and an online step in the
  commit that adds the state. Each step runs either on the dying CPU before it parks, or on the
  control CPU after the dying CPU reports that it has parked. ROADMAP §19.6's test runs every
  registered step.

Offline, in order:

1. The control thread clears the CPU's active bit.
2. The dying CPU's stopper thread, in [§7.8](#78-per-cpu-scheduling)'s stop class, pushes each
   thread on its run queue to an active CPU's inbox ([§7.7](#77-locking-with-more-than-one-cpu)),
   with IF on between pushes. A user thread whose affinity names only this CPU gets every active CPU
   and a log line, as on Linux. The CPU's own kernel threads, such as its workers, park.
3. Every vector routed to the CPU moves to an active CPU through `set_affinity`
   ([§5.4](#54-irq-registration)), and each bottom-half thread moves with its vector. The steps that
   run on the dying CPU run now: for example, it leaves VMX or SVM operation (ROADMAP §21.1) and
   disarms its local timer.
4. A stop-machine rendezvous. Every online CPU's stopper reports in and waits with IF=1 until all
   have. Then each turns IF off and waits, servicing incoming IPIs ([§7.9](#79-tlb-shootdown)),
   while the dying CPU clears its online bit, pushes any thread its wake inbox holds to an active
   CPU, and raises again on its new CPU each moved vector still pending for the dying CPU (latched
   in its IRR on x86_64, pending for it in the GIC on aarch64), as Linux does when a CPU goes
   offline. Then every stopper turns IF back on and returns.
5. The dying CPU reports that it has parked, as its last store to shared state
   ([§2.8](#28-publish-last)), and parks.
6. The control CPU runs the steps that follow the park. They move the CPU's unpinned timers of both
   [§6.5](#65-timers-and-timeouts) structures to an active CPU with their deadlines kept, and cancel
   its pinned ones; requeue its queued softirq-equivalent and work items; drain its per-CPU log
   buffer; return its slab magazines, the kernel stacks it holds for reuse (ROADMAP §10.10), and its
   per-CPU free-frame lists; fold its per-CPU counters into a global offset; move its RCU callbacks
   ([§2.12](#212-rcu)); splice its per-CPU accept queues and receive-steering backlogs onto an
   active CPU's (ROADMAP §28.1, §28.5); and free what [§2.8](#28-publish-last) deferred until this
   CPU had switched away.

Why the rendezvous is enough: a broadcast initiator (a shootdown, a call-function, an NMI backtrace)
reads the online mask, sends, and waits inside one IF=0 section, and so does any code that picks a
CPU from a mask and then signals it, such as a waker or ROADMAP §25.5's buddy check. A stopper runs
only when its CPU is outside every such section, and while every CPU has IF=0 in step 4, no maskable
interrupt handler runs anywhere. So when the dying CPU clears its bit, no CPU holds a choice that
names it, and every later choice reads a mask without it. Step 6 waits for the park report because
it frees stacks the dying CPU deferred and moves timers it could arm until then, as Linux runs its
`DEAD` steps only after `cpu_wait_death`.

The parked state keeps what an interrupt can still reach. On x86_64 the CPU runs `cli; hlt` in a
loop with its `PerCpu`, GDT, TSS, IST stacks, and the IDT live, since an NMI or a broadcast `#MC`
still arrives. It returns to the loop from either without acting on it, clearing `MCG_STATUS` after
a machine check, as Linux does for an offline CPU. It comes back through INIT and SIPI on the
reserved trampoline ([§7.4](#74-ap-bring-up-sequence)), and INIT flushes its TLB. On aarch64 it
calls PSCI `CPU_OFF` and comes back through `CPU_ON` (ROADMAP §11.4), whose entry runs
`tlbi vmalle1` before it enables the MMU. Its `PerCpu`, stacks, and idle thread stay allocated for
the next online.

Online runs the online steps in the reverse order: the control CPU's steps, then the CPU's bring-up
([§7.4](#74-ap-bring-up-sequence)) up to its online bit, then the steps that run on the CPU itself.
A CPU that misses the bring-up timeout takes step 6 of §7.4 (on aarch64, ROADMAP §11.4's
`AFFINITY_INFO` path) and stays offline, and its `PerCpu`, stacks, and idle thread stay with it.
ROADMAP §10.7's TSC skew check runs again, and the active bit is set last.

Rejected: evacuating per subsystem as each lands, which left a dozen per-CPU structures with no
step; clearing the online bit with no rendezvous and having every broadcast re-check the mask, where
one missed re-check is a hang; refusing to offline a CPU that owns pinned threads, timers, or accept
queues, which rules out kexec on aarch64, since it needs every CPU but one offline; Linux's full
state machine of some two hundred states, for the dozen subsystems here; and offlining CPU 0, which
S3 and kexec would first have to move off. Cost: every CPU pauses for the rendezvous once per
offline, which power management does rarely.

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
access, no `unsafe` port I/O, no MMIO. The kernel half calls into it. Each port's pure half
([§11.1](#111-the-seam)) is part of it and runs on every host. Rule; not yet enforced: ROADMAP §10.3.
The x86-only pieces (`switch_context` in `thread.rs`, the fences in `dma.rs`) are
`cfg(target_arch = "x86_64")`, so an aarch64 host such as the dev Mac compiles them and their tests
out. ROADMAP §10.3 moves them to the kernel crate, and the host test that runs a port's switch
assembly lives in `tests/hostlib`, which includes that port's assembly when the host's architecture
matches (ROADMAP §10.2, §11.4).

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

The registry runs on the bootstrap thread under an `InterruptGuard`, so each test starts with IF
off, and `with_timer` turns interrupts on inside it. Planned (ROADMAP §10.2, F075): the registry
runs on a spawned kernel thread with IF on and `irq_nest` 0, on a guarded 64 KiB stack, and a test
that needs interrupts off takes its own guard.

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
the exit status. ROADMAP §10.2 makes it read each of these lines only when framed (§2.6).
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
can hide an intermittent hang or panic. Until then each retry goes to the job summary and to the
tier's results file, and a pull request that ticks a ROADMAP box on a run that retried fails
(ROADMAP §10.2, §10.9).

Planned (ROADMAP §10.2): `begin` carries the number of runs the boot will make, after the command
line's filter and repeat count, and `vibeOS: ktest: run <name> <deadline_ms>` precedes each run,
with the deadline the kernel enforces in the guest (10 s unless the registry sets another). The
harness requires one result line per run line and exactly that many results. It has no whole-run
deadline. `VIBEOS_TIMEOUT` bounds each stretch in which no test runs: from QEMU's start to `begin`,
and from `end` to QEMU's exit. From `begin` to `end`, each run gets its printed deadline plus 5 s,
and each gap between lines 5 s, all multiplied by `env_config`'s one timeout scale. That backstops
the in-guest deadline, which a CPU wedged with IF=0 never checks; a timeout names the test of the
last run line and prints the partial line the guest was writing. Adding tests changes no timeout,
and a test that needs longer carries a registry override, reviewed as code. A per-subsystem list
left out of the aggregate registry is unreferenced code, which the `kernel_tests` clippy run with
`-D warnings` rejects as dead; the rule against a blanket `allow(dead_code)` in production modules
(ROADMAP §10.2, Q2) keeps that true. The `utest_*` lines of ROADMAP §10.5 follow the same protocol.

Skips are first class and carry their reason on the `ktest: skip <name>: <reason>` line. Every skip
names what the configuration lacks: `no AP`, `no virtio-blk`, `no virtio-rng`, `no e1000e`, `no edu`,
`no smep/smap/umip`, `pit owns tick`, `pic fallback`, and `rtc unread` (the `Outcome::Skip` reasons
in `src/ktest.rs`). Destructive exception tests run inside `arch::catch` scopes, which longjmp out or
step RIP past the faulting instruction, instead of skipping.

Planned (ROADMAP §10.2): `tests/harness/skips.toml` lists each test allowed to skip, with its reason
and the configurations it skips in (architecture, accelerator, CPU model, CPU count, memory, machine
options, and the harness host). A run fails when its skipped set differs from the rows that match
its configuration, in either direction, so a lost `-device` or a regressed detection that turns
tests into skips fails the tier. Tests a run does not select print no run line and need no row, and
a test that `VIBEOS_KTEST` names without a glob must run whatever the file says (ROADMAP §12.3).

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
removed silently. The executable contract is `boot_contract_markers()` in `tests/harness/harness.py`;
the list below gives its order, the paragraphs after it add the lines that depend on the machine (the
calibration source, the LAPIC timer mode, the per-AP pairs, and the partition children), and the
`_start` table in [section 3.3](#33-_start-order) says why each step sits where it does. Every line in
it is the kernel's except `shell ready`, which `/bin/sh` prints in the production ISO. ROADMAP §10.2
makes the harness match the kernel's lines only when framed (§2.6), and a line a user program prints
(`shell ready`, the ROADMAP §10.5 `utest_*` lines, `user: tests ok`) only when unframed; today it
matches every line.

Planned (ROADMAP §10.2): one registry, `tests/contract/markers.toml`, holds every line the harness
knows (contract markers, diagnostics, failure lines and halt reasons, and the ktest and utest
protocol), each with its architecture, the program that prints it, and the configurations it holds in.
The harness builds this contract and the failing-fast list from it, `scripts/check_markers.py` fails on
a `marker!` line with no row, and this section then keeps the rules and links the file instead of
listing lines.

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
`block: vda <n> sectors` and `vdapN` when the ktest disk is present (not on the production e2e `pc`
set). The same blind spot follows the last marker: writeback, deferred reclaim, and vibefs commits
keep running after `shell ready`, and a panic there is invisible to a harness that stops reading at
it. Planned (ROADMAP §10.2): the console-input boot keeps reading serial for 3 s after its last
reply and fails on a panic signature in that window.

With `-smp N`, additionally:

- for each AP `i` in `1..N`, `vibeOS: sched: cpu<i> ready` then `vibeOS: smp: ap online`, in order,
  before `smp: done`. The harness requires at least these `N-1` pairs and does not reject an extra
  `ap online` line; ROADMAP §10.2 makes it count exactly `N-1` (F141)
- `vibeOS: sched: cpu<i> ready` for every `i` in `0..N`
- `vibeOS: time: lapic_timer ok (<mode>)` naming the selected timer path
  (`tsc-deadline`, `periodic`, or `pit`) rather than inferring it

The list above is the contract of a boot through Limine. Planned (ROADMAP §25.4, §26.4): a boot
through the image's direct entry prints `vibeOS: boot: <path> entry ok`, where `<path>` is `kexec`,
`crash`, or `pvh`, in place of `limine: rev <n> ok`. A crash entry, ROADMAP §25.4's capture kernel,
boots with `maxcpus=1` and so prints no `smp: ap online` line, and its list ends at
`vibeOS: vmcore: written <n> bytes` in place of `shell ready`, after which it resets.

### Failing fast

Scan for these (`PANIC_SIGNATURES` in `tests/harness/harness.py`) and, in a run that expects no
panic, fail immediately with the captured line rather than waiting out the timeout:

```
panicked at   vibeOS: panic:   #PF   #GP   #UD   #DF   double fault   stack overflow
```

Match the exception mnemonics, not the phrase "page fault". Shell help text and log messages contain
English words, and a substring match on prose produces false failures that erode trust in the suite.

Planned (ROADMAP §12.5): a registered failure line reports a failure the kernel recovered from, so
a run that shows one would otherwise pass. `vibeOS: block: <dev> timeout` and
`vibeOS: block: <dev> reset` are the first. A test that provokes one on purpose declares it; in any
other run it fails the run, since a recovery no test expected is a bug a timeout hides, such as a
lost kick ([section 10.4](#104-virtio-blk)) that shows only as a 30 s pause.

User programs print these strings too: the ROADMAP §10.5 runtime reports a panic as `panicked at` on
fd 2, and a fuzzer writes random bytes. ROADMAP §10.2 makes the harness scan framed lines only
(§2.6). Before the kernel's first framed line it fails fast on Limine's panic line, the one failure
that cannot be framed.

Expected-panic e2e waits for `vibeOS: panic: halted` so the dump (regs, thread, last log records,
backtrace) is in the captured log, then checks dump needles. Planned (ROADMAP §10.7, F135): before
`panic: halted` the dump prints one `vibeOS: panic: cpu N stopped (ipi|poll|nmi|panic)` or
`vibeOS: panic: cpu N not stopped` line for each other online CPU (§2.5 step 1), and the F135
variant checks them. `panic_exit` writes isa-debug-exit
`0x11` so QEMU leaves instead of sitting in `hlt`. The harness kills QEMU at `panic: halted` instead of
waiting for that exit, so it never checks status 35. It also matches boot markers on every line,
including the dump's `vibeOS: logrec:` replay of earlier records, so a marker printed out of order
before the panic can match again in the dump (ROADMAP §10.2, F141).

Planned (ROADMAP §10.7, §11.7), the event rule. The panic path signals pvpanic
([§2.5](#25-panic-policy) steps 6 and 7), QEMU runs with `-action panic=pause`, and the harness
reads QEMU's QMP events. Each run declares the end it expects: none, the default; a panic, for the
expected-panic e2e; `expect=reset`, for a line whose guest resets and boots again; or
`expect=capture`, for a line whose panic reaches a capture kernel. `GUEST_PANICKED` pauses the
guest: a run that expects no panic takes a guest core, quits, and fails; the expected-panic e2e
checks its dump needles and quits; an `expect=reset` run sends `cont`. QEMU reports
`GUEST_CRASHLOADED` without pausing: a run not declared `expect=capture` stops the guest, takes a
core, and fails, and an `expect=capture` run waits for the capture kernel's
`vibeOS: vmcore: written <n> bytes` line and its reset, which ends QEMU under `-no-reboot`. A panic
signature with no event, from a panic before the kernel has found its pvpanic device, fails a run
that expects no panic at once, and the harness takes the core after `vibeOS: panic: halted` or
10 s, whichever comes first. An `expect=reset` run boots without `-no-reboot`, fails on more QMP
`RESET` events than its line expects, and is judged by the markers its line names. `expect=reset`
and `expect=capture` runs take a core only when they fail: a core taken after a crash jump still
describes the crashed kernel, because a kernel entered through the crash path never writes QEMU's
`vmcoreinfo` device (ROADMAP §10.7).

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
(ROADMAP §10.2, F080). Planned (ROADMAP §10.2): the disk is served by the volatile-cache device,
an NBD server in hostlib that records every write and flush, and each round checks the images
rebuilt from its trace, since a kill loses no write QEMU received, under `cache=writeback` or
`cache=none` alike.

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
repeated boot attempts instead of stopping at the interesting one. Planned (ROADMAP §10.7): a run
declared `expect=reset` (§8.3) boots without it and counts QMP `RESET` events instead, so a reset
its line does not expect still fails the run.

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
| `VIBEOS_TIMEOUT` | `60` e2e/ps2, `90` ktest/crash; planned (ROADMAP §10.2): the §8.2 boot allowance, which bounds only the stretches of a boot in which no test runs | all drivers |
| `VIBEOS_QEMU_EXTRA` | empty | all drivers |
| `VIBEOS_TIER` | `adhoc`; each `make test-*` recipe sets its target name (planned, ROADMAP §10.9) | all drivers, which write `build/results/<arch>-<tier>.json` |
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

Two jobs run on every push and pull request, on Linux; the other rows below are scheduled,
dispatched, or run on a tag. `concurrency` cancels superseded runs for the same
branch (push and PR share one slot). The earlier one-ladder-job rule (runner queues) was lifted on
2026-09-22: the repo is public, so Actions minutes are free, and agents own the CI design. ROADMAP
§10.1 plans a build-once job plus a tier matrix per architecture; until that lands the ladder is one
job.

| Job | When | What |
|---|---|---|
| `check` | push / PR | `make check` (fmt, `vibeos-core` clippy `-D warnings`, host units, harness, ruff/mypy, `scripts/check_*.py`) then `cargo llvm-cov -p vibeos-core --lib --features std --target $HOST --fail-under-lines 87`. No QEMU, no `setup.sh`. HTML report is a 7-day `hostlib-coverage` artifact. |
| `phase 0 ladder` | push / PR, `needs: check` | Limine, QEMU/nasm/xorriso/OVMF, kernel clippy `-D warnings` with `--all-features`, `kernel_tests`, and `vibefs_crash` (never the default feature set that ships); ROADMAP §10.1 lints each feature set an image is built with, and `kernel_shell`, in place of `--all-features`; ISO, e2e (BIOS/UEFI/panic/#GP/PIT/9 GiB), in-guest at `-smp 2` and `-smp 4`, LAPIC fallback, vibefs crash. Green `main` uploads `vibeos.iso` (7 days). |
| `smp-stress` | weekly Monday 06:00 UTC + dispatch | `-smp 4`, longer timeout (`VIBEOS_TIMEOUT=180`); planned (ROADMAP §10.2): the §8.2 per-run deadlines, with no longer timeout |
| `nightly-canary` | same workflow, non-blocking | undated latest nightly, `make iso && make test-unit` |
| `release` | `v*` tags | `make test-e2e` (BIOS) only, then production + ktest ISO, changelog section, GitHub Release. It does not wait for `ci` at the tagged commit, and the ktest ISO writes fixed LBAs of any virtio-blk disk attached at boot (ROADMAP §10.1, F145). Planned (ROADMAP §10.1): dispatched from `main` with the release tag as input; a `build` job with `contents: read` and `actions: read`, no cache, and no persisted token, then a `publish` job that runs no repository script; from ROADMAP §14.6 a `sign` job in the `release` environment between them, and from §22.4 a keyless `verify` job on vibeOS. From ROADMAP §18.7 the `sign` job is two key jobs, `sign-files` and `sign-manifest`, with an unprivileged `assemble` job between them, since images hold the signed kernels and Limine binaries and the manifest lists the images (ROADMAP §22.1). |

Planned (ROADMAP §10.9): a `ticks` job on pull requests, after the jobs that run the tiers, runs
`scripts/check_ticks.py` against the `build/results/` files they upload.

Rule; not yet enforced: a job that holds a signing key or a write token runs no code from the
candidate commit, restores no cache, checks out nothing, and receives only artifacts and their
SHA-256 list (ROADMAP §10.1, §14.6). Today `release` builds, tests, and publishes in one job with
`contents: write`, a persisted checkout token, and restored caches.

**Runners.** Planned (ROADMAP §11.7): every job that boots an aarch64 guest runs on an arm64 runner
(`ubuntu-26.04-arm`, or the scheduled macOS job's arm64 image), never on an x86_64 one. TCG adds no
ordering to an aarch64 guest's loads and stores, so only an arm64 host lets a weak reordering reach
guest code; an x86_64 host runs them in its TSO order. `scripts/check_workflows.py` checks it. From
ROADMAP Phase 11 on, `make gate` also needs two dev-host records that loop the -smp 4 in-guest tier
and smp-stress under HVF for 30 minutes each (`tests/gates/common.toml`), the only gate that runs
those tests on a weakly ordered CPU directly. The weekly aarch64 smp-stress leg records whether TCG
there showed any weak outcome (`weak_order_probe`).

**Scheduled capacity.** Planned (ROADMAP §10.1): the Free plan's 20 concurrent jobs are the owner's
account's, shared with its other repositories, and scheduled and dispatched workflows hold 10 of
them as lanes. A lane is a job-level concurrency group, `sched-lane-<n>`, with `queue: max`: it runs
one job at a time and holds up to 100 waiting, first in first out. Without `queue: max` a group
keeps one waiting job and cancels it when another arrives, and GitHub runs no queue across
workflows, so lanes are how the split holds. This section will hold the lane map, which reserves for
jobs that end within 5.5 hours the lanes their cadence needs and names the lanes a release window
takes (ROADMAP §22.1), and a ledger row per workflow: cadence, jobs per run, job-hours per run
(estimated, then measured from `ci-history`), peak concurrent jobs, and lanes.
`ci_history.py --budget` holds every lane but the rebuilds' under 60% busy and every reserved-lane
wait under 12 hours. The 40% left absorbs GitHub's delays to scheduled runs and new workflows, and
keeps the account from running its share full around the clock, which GitHub's Actions terms count
against it when the burden is disproportionate to the benefits. The section also records each
per-push tier's median QEMU time, which `ci_history.py --tiers` keeps under 60 s, and the
`ci-history` branch's packed size (ROADMAP §10.9).

**Issues and crash records.** Planned (ROADMAP §14.10, §22.5): one `workflow_run` filer is the only
job with `issues: write`; it checks out nothing, runs no repository code, and opens or comments on
issues by kind, branch, and signature. From ROADMAP §22.5, fuzz jobs run in `fuzz.yml` under a
`fuzz-state` environment, encrypt the state their shards carry, seal each crash record to the triage
key, and publish only the target, the run, and a keyed crash id, so a crash's reproducer stays
private until its fix is published.

`-D warnings` reaches host builds through `[build] rustflags` and the kernel clippy steps through their own `-- -D warnings`. Kernel builds (`make iso` and
every ISO variant) run without it, because `[target.x86_64-unknown-none] rustflags` in
`.cargo/config.toml` replaces `[build] rustflags` (ROADMAP §10.1, F147).

GitHub Actions records per-step duration. Measured on `main` at `88370e5` (run 35796216463): `check`
53 s, then the ladder 160 s, serialized by `needs: check`. The ladder spends 58 s on setup, toolchain,
kernel clippy, and ISO build before the first QEMU step, then 98 s across nine QEMU steps (longest:
vibefs crash, 22 s); about **3m40s** end to end. Across ten green runs up to `90ce475` the ladder took
156-307 s (median about 206 s), because the harness retried a hung boot (ROADMAP §10.2). A fmt or
hostlib lint failure should go red in about a minute without starting QEMU. From ROADMAP §10.9's CI
history on, a measured number recorded in this document cites the commit it was measured at and the CPU
model or machine it ran on (ROADMAP, How to read this).

Hostlib line-coverage floor is **87%** (`--fail-under-lines 87` in `.github/workflows/ci.yml`).
Measured 87.60% on `nightly-2026-09-22` (`cargo llvm-cov --lib` in `tests/hostlib`; A2 runs the
same portable sources as `vibeos-core`). Ratchet the integer only upward. Coverage is still not a percentage target for the kernel: every bug that gets
fixed gets a test that would have caught it, in the cheapest tier that can catch it. Every entry in
[section 9](#9-pitfalls) names the rule that guards it, and where that rule is only an invariant in
code with no test, that is a weaker guarantee and should be visible as such.

Rule; not yet enforced: a pull request does not quietly change the gates that judge it. From ROADMAP
§10.9, `tests/gates/inputs.toml` lists the gate inputs (the check scripts and their tests, the gate
maps, every expected-failure and skip list, `deny.toml`, the workflows, the Makefile's `check` and
`gate` recipes, and KERNEL_REVIEW.md) and holds the floor above; `scripts/check_gate_inputs.py`
compares each pull request with its merge base and fails when it lowers the floor, adds an
expected-failure or skip entry or edits or removes another input without a `Gate-change:` trailer, or
edits a finding's heading or severity line; and rulesets require `check` and `ci-pass`, one job that
needs every per-push job, on `main`. Today the floor is a literal in the workflow a pull request can
edit, the check scripts judge the pull request that edits them, and `main` requires no check.

Planned (ROADMAP §38.1): `make verify` checks the Verus proofs and TLA+ specifications on every push
that changes `vibeos-core` or `docs/specs/`. From the `phase-38` tag, a change that adds an operation
or a layer to a proved structure, or reorders a modelled protocol, extends its proof or specification
in the same commit, or lists the property it leaves unproved in `docs/VERIFIED.md`'s `Not proved`
section with an open ROADMAP §38.6 box (ROADMAP Phase 38, Changing proved code).

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
Buddy free list nodes live inside free pages, and a kernel stack overflow wrote into one. Rule:
every kernel stack gets an unmapped guard below it, and stack overflow is a page fault, reported
from a stack known to be good, rather than silent corruption: x86_64's `#DF` runs on IST 1 (§5.1),
and aarch64's vector entries test the stack bit of §4.5's layout and move to a per-CPU overflow
stack (§11.5 rule 6; ROADMAP §11.3). The bootstrap thread breaks the rule (`stack: None`): `_start`,
all of boot, and the `kernel_tests` registry run on Limine's stack (at least 64 KiB, no guard page,
in bootloader-reclaimable memory). The kernel sends no stack size request (ROADMAP §10.6, F072).

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
On aarch64 the notify is a Device store, which can reach the device before the `avail.idx` store is
visible; `mmio_write`'s `dmb oshst` orders them ([§4.7](#47-dma)).

**Allocate or block in a hard-IRQ / MSI handler.**
The top half ran `Box` / `sleep` / `WaitQueue` wait. Rule: ack, set pending, wake the IRQ thread or
enqueue work. The thread may alloc and block ([section 2.2](#22-interrupt-handler-rules)).

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
with two CPUs sharing one `PerCpu`. Not yet enforced: the `arch/idt.rs` handlers, IST vectors
included, decide from CS.RPL, and the device pool stubs and keyboard ISRs make no GS decision at all
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
process; `exception_halt` is for faults in kernel code. Every x86_64 vector and every aarch64
exception class has a ring-3 row, in the [section 5.2](#52-idt-and-exceptions) and
[section 11.5](#115-aarch64-exceptions-and-privilege-transitions) tables, and a new ring-3 entry or
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
`IoWaiter` lives on the submitter's stack. `IoWaiter::finish` stores `done` with Release, then takes
SCHED and runs `wake_all` on the `WaitQueue` inside the waiter; `wait()` returns as soon as its
lock-free `poll()` sees `done`, so the wake can land in whatever frame reused that stack (ROADMAP
§10.10, F002). Rule: publish last ([section 2.8](#28-publish-last)). For `IoWaiter`: take SCHED,
wake, then store `done` with Release inside that section, which `poll()` loads with Acquire
([section 10.1](#101-completions)).

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
split a line and the harness then misses a contract line ([section 7.7](#77-locking-with-more-than-one-cpu),
ROADMAP §10.2, F138).

**Two CPUs performing a TLB shootdown at the same time hang.**
Both waited with interrupts disabled for acknowledgement the other could not send. Rule: the wait loop
also services incoming shootdown requests.

## 9.5 SMP bring-up

**An AP reads garbage parameters and dies.**
The trampoline parameter block was written with a non-volatile copy, and the compiler was free to
reorder it past the MMIO write that sent the SIPI. Rule: `write_volatile` per field, and start the AP
through the IPI send, which orders earlier stores for the APIC mode in use ([section 7.6](#76-ipis)).
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
tables, not a second CPU on the same memory ([section 7.4](#74-ap-bring-up-sequence)).
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
then start the AP through the IPI send, which orders the store ([section 7.6](#76-ipis)).

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
place, this file, and a change updates it in the same commit.

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
tier CI runs has it. Not yet enforced: the harness retries until ROADMAP §10.2 deletes the retries.

---

# 10. Block I/O

Phase 7. Portable types live in `src/block.rs`, `src/part.rs`, and
`src/cache.rs`. Kernel ramdisk, waiters, and the boot marker live in
`src/block_init.rs`. virtio-blk packing is `src/virtio_blk.rs`;
the driver is `src/virtio_blk_init.rs`. Partition children are
`src/part_init.rs`. The write-back cache is `src/cache_init.rs`.

## 10.1 Completions

A request carries waiter cookies, not a locked queue. Submit takes the per-device queue lock
(RANK_DEVICE), merges or enqueues, and drops the lock before any copy. The ramdisk pump runs after
that drop; virtio-blk submits to a virtqueue after the same drop and completes from a threaded IRQ.
Rule: completion begins with the claim ([§10.3](#103-failure) step 2), and only the party that
claimed a request touches it. It wakes the waiters under SCHED and then stores the status with
Release inside that section, as its last access to each waiter ([section 2.8](#28-publish-last));
`poll()` loads it with Acquire. `IoWaiter::finish` breaks this: it stores `done` with Release before
it takes SCHED, then wakes (ROADMAP §10.10, F002). Never hold the queue lock across I/O or across
that wake (DEVICE then SCHED is the wrong order). Hard IRQ must not run this path: enqueue work only
(DESIGN [§2.2](#22-interrupt-handler-rules)). Ramdisk backing is BSS, not a heap `Vec`: allocating
under RANK_DEVICE would take RANK_HEAP (lock order).

Blocking wait and async submit share the same cookie. The buffer and waiter must outlive the
completer's last access to them, which under the rule above is the status store. Once the waiter and
the buffer are counted (ROADMAP §12.5, F042), the completer drops its references to them after it
leaves SCHED, never inside it (§2.11 rules 5 and 6). The lock held across the wake and the status
store, SCHED today, lives outside the waiter, so its unlock after the store touches no waiter
memory. When wait queues get their own locks (ROADMAP §19.4 splits SCHED), a waiter that lives on a
stack, such as `IoWaiter`, is still woken under a lock outside it, or its `wait` takes that lock
before it returns and has no lock-free return.

Planned (ROADMAP §12.5): completion runs in two stages. Stage 1 runs in the party that claimed the
request ([section 10.3](#103-failure)), usually the bottom half. It records the status and does only
work that neither waits nor submits I/O: it wakes the waiters, clears a page's FILLING or WRITEBACK
state with its result (a failed fill leaves the page not up to date and wakes its waiters with the
error), records a writeback error on the mapping, and drops references through `put_deferred`
(§2.11). Everything else is stage 2: data-checksum verification, decryption, a stacked parent's
completion, and a failover or mirror retry. Stage 2 runs as a softirq-equivalent item (§2.2)
queued from the CPU that ran stage 1. The item is part of the request, so queuing it allocates
nothing and cannot fail. A stage-2 item takes no sleeping lock and never waits. Work that needs
more I/O (a read from another mirror, the rewrite of a bad copy, a stacked device's child request)
allocates its request fallibly and only enqueues it on the target's software queue, never waiting
for a descriptor, bounce slot, or tag, and that request has its own stage-2 item. If the allocation
fails, the original request completes with its own error. A page whose fill needs stage 2 stays
FILLING until stage 2 finishes. A stacked parent's completion runs in the item that completed its
member. A read loads the checksums of the blocks it reads before it submits them, in the
submitting thread, so verification at completion reads nothing.

Why: a bottom half that reads a checksum-tree block to verify a fill either waits for its own
device, whose completion only it delivers, or does not wait and leaves the page FILLING with no one
to finish it. Linux runs this work after completion in workqueues (btrfs's end-io workers,
dm-crypt's kcryptd) and looks checksums up at submission, as btrfs does. Rejected: a second,
block-only completion workqueue, which duplicates the softirq-equivalent queue (AGENTS.md rule 10);
a completion thread per queue that may block, which moves the blocked-handler problem up one level;
and Linux's concurrency-managed workqueues with rescuer threads, more machinery than a rule that
stage 2 never waits.

## 10.2 Ordering, flush, and FUA

Rule: the block layer does not order requests. Requests in flight complete in
any order, on any queue. A caller that needs one write durable, or visible to a
later read, before another starts waits for its completion before it submits the
other. The block layer keeps two orders of its own: it never dispatches a write
while an older write to an overlapping range is queued or in flight (ROADMAP
§10.11), and a zoned device has at most one write in flight per sequential zone
(ROADMAP §29.1).

`Flush` makes durable every write whose completion was reported before the
`Flush` was submitted, as virtio 1.2 §5.2.6.2's FLUSH, NVMe's Flush, SCSI's
SYNCHRONIZE CACHE, and Linux's `REQ_PREFLUSH` do. It neither waits for nor
covers a write still in flight, and it holds up no later request. Ramdisk flush
is a successful no-op (the backing is already memory).

A write may carry `Fua`: its completion means it is durable. A device that
offers FUA gets the flag with the write. For any other device the block layer,
not the driver, completes a `Fua` write: it waits for the write, sends a
`Flush`, and reports the write complete when that `Flush` completes, as Linux's
flush machinery does. A driver only says whether its device offers FUA
(AGENTS.md rule 10).

A filesystem commit is built from these: write the new blocks, wait for every
completion, `Flush`, then write the block that makes the commit visible with
`Fua`. A journaling filesystem does this with its commit record. vibefs is not
journaled: it uses CoW metadata plus an atomic superblock switch
([VIBEFS.md](VIBEFS.md) §2, §10), and a generation is durable when its
superblock write, carrying `Fua`, completes.

Adjacent read, write, and discard requests merge; a `Flush` merges with nothing.

Why: a fence that holds every later request until the earlier ones complete
serializes the device. With a queue per CPU ([section 10.4](#104-virtio-blk)),
partitions that share a device ([section 10.5](#105-partitions)), and stacked
devices over several members (ROADMAP §29.1), one `fsync` would drain every
queue, partition, and member, reads included. A filesystem needs only the
completions of its own writes, which it can wait for. Linux replaced its ordered
barriers with completion waits, `REQ_PREFLUSH`, and `REQ_FUA` in 2.6.37 for this
reason, and virtio keeps its barrier feature only in the legacy interface.
Rejected: a device-wide `Barrier` that every later request waits behind; and a
fence per queue, since one filesystem writes from every CPU's queue and would
wait for completions anyway.

Not yet: `src/block.rs` still defines `Barrier` and holds later requests behind
a queued `Barrier` or `Flush`, `try_merge` checks only the lowest queued fence,
a fence whose seq is `u32::MAX` reads as no fence, C-LOOK can reorder
overlapping writes whatever their seq, `Fua` does not exist, and the block
cache's `flush` does not wait for writeback already in flight
([section 10.6](#106-block-cache)). Only two in-guest tests call `barrier()`.
ROADMAP §10.11 lands all of it (F043).

## 10.3 Failure

Rule: an I/O error is retried within the request's retry budget, `DEFAULT_RETRY_BUDGET` (3) extra
attempts, which step 5 below shares with resets. A request that exhausts it completes with its
error, and the device stays `Bound`. `Inval` (range, size) is not retried and uses no budget. No
infinite retry loop. Submits to a `Failed` device return `Failed`. Not yet enforced: a virtio-blk
request that exhausts its budget makes the device `Failed` and fails every queued request with it
(ROADMAP §10.11, F046).

Rule: every request has a deadline, 30 s by default and settable per device, as Linux's block layer
has, and the device gives a request back before anything touches its buffer (§2.11 rule 3: stop the
device, then release). Error handling:

1. Deadline. The deadline starts when the driver dispatches the request to the device, as Linux's
   `blk_mq_start_request` starts it, and each dispatch gets a new one. A stacked device (md, dm,
   multipath, the ROADMAP §18.7 transform) has no deadline of its own. It acts on its members'
   completions, which their error handling bounds.
2. One claim. Exactly one party completes a dispatched request: the bottom half that harvests it,
   the error handler, or removal. That party claims the request before it touches the request's
   buffer or waiter, by a compare-and-swap on the request's state or by taking the request out of
   the driver's table under the queue lock. A harvest that finds its request already claimed leaves
   it alone. Completion ([section 10.1](#101-completions)) begins with the claim.
3. One error handler per resettable unit (a virtio function, an NVMe controller, an AHCI port): a
   kernel thread that sleeps until the unit's earliest deadline or a trigger. A trigger is a
   deadline passing, the device's needs-reset status (virtio's `DEVICE_NEEDS_RESET`), a used-ring
   entry that fails the transport's checks, an IOMMU translation fault, or a surprise-removal
   event. Hard IRQs, bottom halves, and waiters only wake the handler. It is a no-reclaim thread
   (§4.4 rule 4), and it never takes the device-model lock (§12.1), so removal may wait for it
   while holding that lock. `IoWaiter::wait` parks with its device's stall bound S (step 6) as its
   deadline. If S passes, the waiter logs a report naming the request and waits on. It never takes
   the buffer back itself.
4. Recovery. Where the device can abort one command (NVMe's Abort), the handler first aborts each
   timed-out command and waits for the abort. The command's own completion, with whatever status,
   is claimed as usual. Otherwise, or when an abort does not complete in its wait, the handler
   resets the unit:
   1. It marks the unit `Resetting`, so new requests stay in the software queue.
   2. It quiesces each queue: it masks the queue's interrupt through its interrupt controller
      ([section 5.4](#54-irq-registration)), sets the queue's quiesce flag, which ends any
      bottom-half wait on the device's resources, and waits until the queue's bottom half is idle.
      If every request whose deadline passed has now been claimed, it unquiesces and resets
      nothing.
   3. It resets the device (virtio's status 0, read back as 0; NVMe's `CC.EN` cleared and
      `CSTS.RDY` read back as 0; AHCI's port reset) with IF=1 and no spinlock held, sleeping
      between reads, for at most the unit's reset bound R (step 6).
   4. It claims every dispatched request not yet claimed.
   5. If the reset brought the unit back, it reinitializes the rings and the last-seen used index,
      resubmits the claimed requests in each queue's submission order, Flushes included, and
      unquiesces. Under [section 10.2](#102-ordering-flush-and-fua)'s ordering no request in
      flight depends on another, so the order carries no durability meaning.
   6. If it did not, it clears Bus Master Enable in the function's command register and reads it
      back, detaches the device's IOMMU domain when ROADMAP §18.1's IOMMU is on, completes every
      claimed request with `Io`, and marks the unit `Failed`. Where bus mastering cannot be
      cleared and read back (a transport without it, such as virtio-mmio or a platform device, or
      a function that is gone, whose config reads return all ones) and no IOMMU that translates
      the device blocks it, each request still completes with `Io`, but its buffer is quarantined:
      the driver keeps its frame references and counts them in `blk` and meminfo. A quarantined
      buffer is freed once the device provably cannot DMA: its downstream port reports the link
      down or its presence lost, the kernel has powered its slot off, or an IOMMU that translates
      it has its domain set to blocking and that invalidation has completed. Otherwise it is held
      for good.

   A trigger that arrives during a recovery joins it.
5. Budget. Every resubmission, after an I/O error or after a reset took the request back, uses one
   attempt of the request's retry budget. With none left, the request completes with its error,
   or with `Io` after a reset. A request whose own deadline passes a second time completes with
   `Io` at that recovery, whatever budget it has left, so a command the device never completes
   cannot hold the unit in a reset loop. A reset that brought the unit back leaves it `Bound`.
6. Stall bound. Each driver declares its reset bound R, the longest a recovery takes from the
   handler's first step to the unquiesce, the abort's wait included: virtio-blk's reset poll
   ([section 10.4](#104-virtio-blk)), NVMe's abort wait plus `CAP.TO`, AHCI's port reset. A
   request waits behind at most one recovery before its first dispatch, is dispatched at most
   1 + `DEFAULT_RETRY_BUDGET` times, and each dispatch ends within deadline + R: it completes, or
   its own deadline passes and its recovery ends, or a recovery that began before its deadline
   takes it back. So the device's stall bound S = R + (1 + `DEFAULT_RETRY_BUDGET`) × (deadline + R)
   bounds how long a request of a failing device stays incomplete and how long a thread waits for
   one: 125 s for virtio-blk at the defaults. A stacked device's S is the largest sum of member
   bounds over the members one request can try in turn: every copy of a mirror, every path of a
   multipath device, one member of a stripe. Time a request waits for a free descriptor behind
   requests the device is still completing is load, which S does not bound.
7. One quiesce. Removal, suspend (ROADMAP §20.2), and shutdown (ROADMAP §25.4) begin with step
   4.2's queue quiesce (AGENTS.md rule 10), and `free_vector` sets the same quiesce flag (§5.4).
   Removal marks the device `Removing` (§12.1), wakes the handler, and waits for the handler to
   exit before it frees the device's state. A handler that finds the device `Removing` finishes
   its current step and then does not reset: it quiesces, runs the driver's stop step (§12.2),
   claims every dispatched request, completes it with `Gone`, under step 4.6's quarantine rule
   when the stop step cannot clear bus mastering, and exits.
8. Terminal states. `Failed` and `Dead` (§12.1) are terminal for a registration: the device
   returns only as a new registration with a new id. A `Failed` device's consumers see what a
   removed device's see (below and §12.4): its filesystem never commits again, its dirty pages are
   dropped and their error is reported once to each open file description's next `fsync`, and
   `mount -o remount,rw` returns `EIO`.

A reset is not a power loss. Writes completed before it are assumed to stay in the device's volatile
cache until a later `Flush` makes them durable, as Linux's NVMe and virtio-blk drivers assume.

Planned (ROADMAP §22.2, §25.5): every hang detector waits longer than the stall bound of what it
may be waiting on, so a storage failure the kernel is handling is not taken for a hang. The
blocked-thread report waits at least the largest S among registered block devices, since a thread
waiting for a lock may wait behind another thread's I/O. The watchdog core keeps its timeout at or
above twice that S. Init's trial-boot health check and each critical service's heartbeat allowance
wait at least half the watchdog timeout, so at least that S. The kernel logs each block device's S
when it registers the device. Why: Linux's `i6300esb` driver defaults to a 30 s timeout, the block
deadline itself, so a watchdog left at such a value resets a machine whose RAID was about to absorb
a wedged disk, and a trial boot rolls back a good update because a disk stalled. Rejected: a
shorter block deadline, which times out slow but healthy devices and loaded TCG runners; relying
only on init locking its memory, which a distribution's init does not do; and leaving each timeout
to whoever implements it.

Not yet enforced: no request has a deadline, and `IoWaiter::wait` parks with `FAR_DEADLINE`, so a
lost completion, such as one after a missed kick ([section 10.4](#104-virtio-blk)), blocks its
submitter for good, and so does every thread that then waits for a lock the submitter holds
(ROADMAP §12.5).

Why: a lost completion is a device or driver bug the kernel must survive and report (§1.1
constraints 4 and 5, which already bound every poll). Freeing a timed-out request's buffer while
the device may still write it would turn a hang into memory corruption, so the device stops first.
A request has one claimant because several parties can see it end (the harvest, a timeout, a
needs-reset status, a ring violation, an IOMMU fault, and removal), and the status store of §10.1
is safe only as the last access of one of them. The handler is a thread of the unit's own because
the waiter cannot run it (writeback and readahead have none, and many waiters would race to reset
one device), a bottom half cannot (it may be the blocked party, and a device-wide reset stops every
queue's bottom half, its own included), a softirq-equivalent item may not block while an NVMe reset
poll may last `CAP.TO`'s 127.5 s, and a general workqueue item would stall unrelated items behind
that poll. Quiescing before the claim means a harvest never runs during a reset. Every
resubmission uses the one retry budget so that S exists. Adopted from Linux: the timer started at
dispatch, blk-mq's claim before completion, NVMe's reset work and its retry count for commands a
reset cancelled, and SCSI's error-handler thread per host. Rejected: a timeout that only reports
and keeps waiting, which leaves every waiter behind the request stuck, safe but not live; a
deadline on a stacked device, which would release pages a member may still write; separate stop
mechanisms for reset and for removal, two implementations of one primitive that a removal during a
reset needs both of; and freeing a removed device's buffers with no proof it cannot DMA.

A removed device is `Gone` (§12.4), and every request to it fails with `Gone`. Its consumers
see what Linux shows for a removed device. A filesystem maps `Gone` to `EIO`; it stops
committing and writing back, so its last committed generation stays the on-disk state
([VIBEFS.md](VIBEFS.md) §10), and later writes fail with `EIO`. A read of a page not in the
cache returns `EIO`, and a fault on an unpopulated page of a file mapping raises `SIGBUS`.
Dirty pages are dropped, and the error is reported once to each open file description's next
`fsync`, `fdatasync`, or `msync`. `umount` does not fail on the device's errors, and its busy
rules are unchanged: `umount2` with `MNT_DETACH` succeeds while files are open, and a plain
`umount` succeeds once they are closed. A device node's open descriptor fails as Linux's does
for a removed device of its class (`ENODEV` from an input node, for one), and it never reaches
a later device. Planned (ROADMAP §20.9): nothing is removed today.

Logical block size is per device. Do not assume 512. Capacity is in those
blocks. Discard on ramdisk validates the range and otherwise no-ops.

## 10.4 virtio-blk

Modern virtio-blk (`1af4:1042`, `VERSION_1` required) binds by id on the
Phase 6 transport. Config reads capacity (512-byte units), `blk_size` (512
if `F_BLK_SIZE` is absent), and topology when offered. Each request is a
descriptor chain: header + data (or discard range) + status. The status
byte is device-writable DMA, never a stack slot. Completions harvest the
used ring on the threaded IRQ and wake the same `IoWaiter` cookies as the
ramdisk. Its reset polls the status for at most 1 s, sleeping between reads, which is virtio-blk's
reset bound R ([section 10.3](#103-failure)); today `reset` spins for up to a million reads
(ROADMAP §12.5). The hard IRQ only acks ISR, reading it on every interrupt. The driver always
runs on MSI-X, where virtio 1.2 §4.1.4.5.2 says a driver should not read ISR (ROADMAP §26.4, F122).

`F_MQ`: one virtqueue per online CPU, capped by the device `num_queues` and by
`MAX_VQ` (8). Requests on different queues are not ordered against each other
([section 10.2](#102-ordering-flush-and-fua)). Without `F_MQ`, a single
request queue. Data goes through 16 bounce slots of 8 KiB shared by the
device's queues; a request over 8 KiB is
`Inval`, including one the block queue merged past that size (ROADMAP §12.5,
F119). Flush and discard go to the device when those features are negotiated;
flush without `F_FLUSH` is a successful no-op (nothing to make durable).
virtio-blk has no FUA, so the block layer completes a `Fua` write with a
`Flush` after it (§10.2); without `F_FLUSH` that `Flush` is the same no-op,
since every completed write is already durable.

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

Rule: a lookup of a block device by an identity it carries (a filesystem
UUID or label, a GPT disk GUID, or a partition's unique GUID or label) that
matches more than one device is refused with a log line naming every device
it matched. It is never settled by probe order. Naming a device by its path
always works. The rule binds every such lookup the kernel or the initrd
makes, from `root=` (ROADMAP §14.8, §22.2) to the assembly of stacked
volumes (ROADMAP §29.1). Why: a byte copy of a disk, such as a second
instance of a cloud image or a snapshot attached for rescue, carries its
source's ids, and probe order could mount the copy read-write as root. An
image's first boot replaces the ids it was built with (ROADMAP §26.2), so
only a copy of a disk that has booted can still collide, and `tune-vibefs`
changes a copy's filesystem id offline (VIBEFS.md §15).

## 10.6 Block cache

Page-granular (4 KiB), 16 pages (`cache::DEFAULT_PAGES`), keyed by
`(dev_id, page offset)`. Read-through, write-back, clock eviction,
sequential readahead, dirty-ratio writeback thread (`blk-wb`). `flush`
writes the pages marked dirty, waits for each write to complete, then sends
the device `Flush` (§10.2). `barrier` writes dirty pages without a device
flush and goes with `Barrier` (ROADMAP §10.11).

A slot has no filling or writeback state. `take_dirty` clears a page's dirty
bit before `blk-wb` writes it with the lock dropped, so while that write is in
flight `flush` skips the page and can send the device flush first, a page
dirtied again can have two writes to one LBA in flight, and the slot can be
evicted and later read back stale. `find()` matches only valid slots, so a
second reader of a page being filled does not wait for it (the writeback
state and the `flush` wait land with ROADMAP §10.11, F015 and F043; the
filling state with ROADMAP §12.5, F015).

The cache reaches the drivers by raw device id: `cache_init::raw_read`,
`raw_write`, and `raw_flush` match `DEV_RAM0` to `block_init` and `DEV_VDA`
to `virtio_blk_init`, and only in-guest tests call the `BlockDevice` trait
objects. Planned (ROADMAP §10.4, D2): the cache holds a counted `BlockRef`
from one registry of block devices, partitions included, and keys its pages
by the device's never-reused id ([§12.1](#121-devices)), instead of matching
device ids (F081). Hit/miss/device-request counters are in the `blk` shell
command. The cache lock is RANK_DEVICE and is dropped before blocking
device I/O.

Planned (ROADMAP §12.5): one page cache made of mappings, of which this cache becomes one kind.
Every cached page belongs to exactly one mapping at one page index, and the index is its key. A
mapping is a file's (file data, tmpfs files, and the ELF pages ROADMAP §12.2 maps) or a block
device's (filesystem metadata, and reads and writes of the device node). A page's device location is
filesystem block-mapping state and is never a cache key. One frame pool, one LRU, and one set of
writeback threads serve both kinds; do not grow a second private cache. Today's
`(dev_id, page offset)` cache becomes each block device's mapping and keeps the FILLING and
WRITEBACK states and the flush wait above (F015, F043).

- A mounted filesystem reads and writes its file data only through the file's mapping, and caches
  its metadata in the device mapping by LBA. Direct I/O (ROADMAP §19.8) bypasses the cache only
  after it writes back and drops the file mapping's pages over its range, as Linux's does. Reads of
  the device node while it is mounted see the device, not dirty file pages, as on Linux.
- Copy-on-write moves a file's block map, not its cache entry: vibefs writes a dirty file page back
  to a fresh block ([VIBEFS.md](VIBEFS.md) §10) and submits the page's own frame, so the page keeps
  its mapping, index, and frame while the block it lives in changes at every commit.
- A page filled from a filesystem that checksums its data becomes up to date only after the checksum
  that covers it verifies, and a write that changes part of a checksummed block fills and verifies
  the whole block first. A failed fill leaves the page not up to date and never written back: `read`
  and such a write return `EIO` and dirty nothing, and a fault on the page raises `SIGBUS`
  ([VIBEFS.md](VIBEFS.md) §15, Corrupt data). Writeback checksums whatever a page holds, so a merge
  into an unverified page would store the corruption under a valid checksum.
- A block the filesystem allocates loses any page the device mapping holds for its LBA before its
  first write through any path: the page is dropped with its dirty state discarded, and an in-flight
  writeback of the old contents completes before the new write is submitted. A reused LBA never
  returns an old block and is never overwritten by one, as Linux's `clean_bdev_aliases` ensures.
- Index: one radix tree per mapping, keyed by page index, with 64-way nodes (Linux's XArray shape).
  Nodes are allocated before the mapping's lock is taken and freed after it is dropped. The lock is
  a RANK_DEVICE spinlock, as today's cache lock is, and is never held across I/O.
- A page's owner and index in §4.6's frame metadata name its mapping and its index there.

Why: a vibefs file page's device location changes at every commit, and a hole or a page not yet
written back has none, so a key by location moves under readers and mapped PTEs. Linux keys the same
way, with an `address_space` per file and per block device. Rejected: keying file pages by
`(device, LBA)`; a per-file index and a device index over one frame, which gives a frame two owners with no
coherent eviction; a B-tree index, whose splits allocate on insert and remove, for dense keys that
gain nothing from it.

Planned (ROADMAP §12.5): the writeback contract.

- Errors follow Linux's `errseq_t` at the [LINUX.md](LINUX.md) baseline. An error in writeback, or
  in the commit that would make a file's data durable, is recorded on the file's mapping and on its
  volume. Each open file description's next `fsync`, `fdatasync`, `msync` with `MS_SYNC`, or
  `sync_file_range` with a wait flag returns it once; a description opened later returns an error
  that no description has seen yet; `syncfs` returns the volume's errors the same way.
- vibefs keeps a failed commit's file pages dirty and writes them again, to other blocks, at its
  next commit ([VIBEFS.md](VIBEFS.md) §10), where Linux marks them clean; `docs/LINUX.md` lists the
  difference. A volume whose commits fail `DEFAULT_RETRY_BUDGET` times in a row, or whose device is
  `Failed` or `Gone` (§10.3), goes read-only, drops its dirty pages with the error recorded on each
  mapping, and logs it.
- Stable pages. A page of a file whose data is checksummed (vibefs v1's extent CRCs, v2's block
  checksums) does not change while it is written back. Writeback write-protects every user mapping
  of the page through the reverse map and completes the TLB shootdown (ROADMAP §12.1, §12.3) before
  it checksums the page. A `write()`, a write fault, or a kernel write into the page, such as
  truncate zeroing a tail, waits until that writeback completes, a level-3 wait (§2.1), as Linux's
  stable pages do. A page a device holds pinned (ROADMAP §19.8) is written back from a copy,
  checksummed over the copy, and a direct-I/O write to a file whose data is checksummed copies the
  user buffer into a kernel buffer and checksums the copy. The checksum then covers exactly the
  bytes the device writes, with or without §10.4's bounce copy. NOCOW files and unwritten ranges
  ([VIBEFS.md](VIBEFS.md) §15) have no data checksum and need none of this.

Why: an error nobody reports is data a program believes durable, which is why PostgreSQL treats an
`fsync` error as fatal; and a checksum over bytes that changed while the device wrote them makes a
block read as `Corrupt` for good with no disk fault behind it. Rejected: marking failed pages clean,
which loses data a copy-on-write retry to fresh blocks can still save; retrying without a bound,
which pins dirty memory for good behind a device whose writes all fail; skipping checksums for pages
mapped writable, a gap in exactly the files that change most; and bouncing every write, which undoes
ROADMAP §19.8's zero-copy.

---

# 11. Portability

x86_64 and aarch64 are peers from ROADMAP Phase 11; x86_64 came first and is the reference where the two
ports disagree about the kernel's own behaviour; for anything a user program can observe, each
architecture's reference is Linux on that architecture (ROADMAP, How to read this). This section is the
contract for the seam between them. `docs/ARCH.md` (ROADMAP §10.3) maps each row of the §11.1 table to
the modules that implement it in each port. Planned: ROADMAP §10.3 builds the seam and Phase 11 the
aarch64 port. Today no seam trait, stub port, or `docs/ARCH.md` exists; `thread.rs` and `dma.rs` in
`vibeos-core` carry `cfg(target_arch)` and assembly (ROADMAP §10.3); and the one port is x86_64's, in
the kernel crate's `src/arch/` and in `vibeos-core`'s `desc.rs`, `pic.rs`, and `vectors.rs`. The rest of
this section is the design those lines build.

## 11.1 The seam

The table is the one inventory of what is architecture-specific, and ROADMAP cites it rather than
repeating it. The Seam column says how portable code reaches a concern: through a trait, which ROADMAP
§10.3 builds; through the port's pure half (below); or not at all, where a port module that only the
kernel crate's architecture code calls holds it. Everything the table does not name is shared. The
syscall table's semantics are shared too; only numbers and argument order differ per architecture
(ROADMAP §10.5), and user-visible structures keep their meaning while their layouts follow Linux's
per-architecture uapi (ROADMAP §13.10).

| Concern | Seam | x86_64 | aarch64 | ROADMAP |
|---|---|---|---|---|
| Boot handover: the machine state the boot handshake hands over, normalized into `BootInfo` | trait | Limine base revision 3, long mode; without Limine, the direct entry (§4.1): a PVH door and a 64-bit door into one body | Limine base revision 6, EL1, or EL2 with VHE; without Limine, the direct entry (§4.1) behind an arm64 `Image` header, entered with the MMU off | §10.3, §11.1, §25.4, §26.4 |
| Early console | port module | 16550 on COM1 | PL011 | §11.1 |
| Exception entry and exit | port module: generated entry code | one stub per IDT vector ([§5.10](#510-privilege-transitions) rule 1) | one 16-entry vector table ([§11.5](#115-aarch64-exceptions-and-privilege-transitions)) | §10.6, §11.3 |
| Trap decode | pure half: a trap to a `TrapKind` (§5.2) | vector and error code | vector slot and `ESR_EL1` (§11.5) | §10.6, §11.3 |
| Kernel stack-overflow report | port module | `#DF` on IST 1 (§5.1) | a stack test at every vector entry and a per-CPU overflow stack (§4.5, §11.5) | §11.3 |
| Interrupt mask | trait | RFLAGS.IF (`cli`, `sti`) | PSTATE.I and F (`msr daifset`, `msr daifclr`); priority masking from ROADMAP §25.5 | §10.3 |
| Interrupt controller and IRQ identity | port module: finding the root controller, with the vector entry and the IPI send in their own rows; each controller is an `IrqChip` object (§5.4), not a seam trait | 8259, I/O APIC, and LAPIC MSI chips; a hwirq is an IDT vector (§5.3) | GICv2 or GICv3 distributor and redistributor chips, ITS or GICv2m; a hwirq is an INTID | §11.3 |
| IPI send and its ordering | trait | LAPIC ICR write (§7.6) | SGI register write | §10.3, §11.3 |
| Timer and cycle counter | trait (`CycleCounter`) | TSC, or the HPET or ACPI PM timer as the clocksource (§6.4); LAPIC timer | `CNTVCT_EL0`; generic timer | §10.3, §11.3 |
| Page-table format and attributes | trait (`PageTable`); encodings in the pure half | 4-level tables, PAT bits | 4 KiB granule, 48-bit VA, MAIR, break-before-make | §10.3, §11.2 |
| TLB maintenance and address-space ids | trait (`PageTable`) | `invlpg` and the shootdown IPI (§7.9); no PCID | broadcast `tlbi ...is`; ASIDs from §11.2's generation allocator | §10.3, §11.2 |
| Cache maintenance and DMA coherence | trait (`Barriers`) | none: coherent | per-device coherence from `dma-coherent` or `_CCA`; `dc cvac` and `dc ivac` to the Point of Coherency for non-coherent devices (§4.7); `dc` and `ic` for code | §10.3, §11.2 |
| Barriers (`dma_wmb`, `dma_rmb`, `dma_mb`) and MMIO accessors | trait (`Barriers`) | `mfence`, `sfence`, `lfence`; plain loads and stores; accessors carry a compiler barrier (§4.7) | `dmb oshst`, `dmb oshld`, `dmb osh`; `dmb oshst` before an `mmio_write` and `dmb oshld` after an `mmio_read` (§4.7) | §10.3, §11.2 |
| Atomics | module selected by `cfg(loom)` (below) | `core::sync::atomic` | `core::sync::atomic`, with LSE instructions (`+lse`, §3.1's floor) | §10.8 |
| Per-CPU base and current-thread registers | trait | `GS_BASE` and `swapgs`; `current` by one `gs`-relative load (§2.9 rule 5) | `TPIDR_EL1`, or `TPIDR_EL2` at EL2; `current` in `SP_EL0` (§2.9 rule 5) | §10.3, §11.4, §11.6 |
| Syscall instruction, user frame's layout ([§5.10](#510-privilege-transitions)), numbers and argument order | trait | `syscall` and `sysretq`; the x86_64 table | `svc #0`; the asm-generic table | §10.3, §10.5, §10.6, §11.6 |
| User-memory accessors | trait | `stac` and `clac` (SMAP) | PAN | §10.3, §10.6, §11.6 |
| FP and SIMD state | port module, under §7.5's per-thread rules | FXSAVE image | V0-V31, FPCR, FPSR | §10.6, §11.6 |
| User TLS register | port module | `FS_BASE` | `TPIDR_EL0` | §11.6 |
| Context switch | trait | `switch_context`: callee-saved registers, RSP, RIP | `switch_context`: x19-x29, SP, LR | §10.3, §11.4 |
| Secondary-CPU bring-up | port module | INIT-SIPI and the trampoline page (§7.3) | PSCI `CPU_ON` | §11.4 |
| CPU identity, topology, and features | port module | APIC ID, CPUID | MPIDR, ID registers | §11.4 |
| Idle | port module | `sti; hlt` | `wfi` with IRQs masked (below) | §11.3, §19.6 |
| Power-off and reset | port module | ACPI, the 8042, `isa-debug-exit` | PSCI `SYSTEM_OFF` and `SYSTEM_RESET` | §11.4, §20.2 |
| Machine description | pure half: `MachineDesc` (below) | filled from ACPI (§7.1) | filled from the device tree; also from ACPI from ROADMAP §20.7 | §11.5, §20.7 |
| PCI configuration access | port module | `0xCF8`/`0xCFC` and ECAM | ECAM only | §11.5 |
| Hardware RNG | port module | `RDRAND`, `RDSEED` | `RNDR` | §11.5 |
| Debug and single-step state | port module | DR0-DR7, RFLAGS.TF | `MDSCR_EL1`, breakpoint and watchpoint registers, `brk` | §17.4 |
| Panic stop | port module | IPI `0xFE`, then NMI (§7.6) | SGI; pseudo-NMI from ROADMAP §25.5 | §10.7, §25.5 |
| Unwinder | port module | `rbp` chain | `x29` chain | §10.7, §11.7 |
| Signal frame, sigreturn trampoline, vDSO counter read, ELF machine, TLS variant, and HWCAP | pure half: layouts | `rt_sigframe`, `EM_X86_64`, TLS variant II | `rt_sigframe` and the trampoline page, `EM_AARCH64`, TLS variant I, `AT_HWCAP` | §11.6, §13.8, §13.10 |
| Crash-dump page-table publication | port module | none: QEMU walks x86_64 page tables | the TTBR1 root in a `VMCOREINFO` note | §11.7 |
| Hypervisor | port module | VMX or SVM | EL2 with VHE | Phase 21 |

The page-table format is the largest pure half. Descriptor encoding and decoding, the walk, `map`,
`unmap`, `protect`, and `translate`, and on aarch64 the break-before-make check, are `vibeos-core`
code, in each port's pure half and the portable `Mapper` over it; the hardware half keeps only the
root-register writes (CR3; TTBR0 and TTBR1) and the TLB and cache instructions. Why: ROADMAP Phase
11's gate host-tests the page-table code's break-before-make refusals on every host, and Phase 38
proves only `vibeos-core` source (ROADMAP §38.2), so page-table code in the kernel crate would have
to move before it could be proved.

Idle is the row most easily ported wrong. x86_64's idle checks for work with IF=0 and runs `sti; hlt`:
`sti` takes effect only after the next instruction, so an interrupt that arrives after the check wakes
the `hlt`. aarch64 has no such shadow. Its idle checks for work with IRQs masked, runs `wfi` with them
still masked, since a pending interrupt wakes `wfi` whatever PSTATE.I says, and unmasks afterwards so
the interrupt is taken. Unmasking before the `wfi` lets a reschedule IPI be taken between the unmask
and the `wfi`, which then sleeps until the next tick, and for good on a CPU whose tick is off (ROADMAP
§19.6).

The machine description is one portable struct, `MachineDesc` in `vibeos-core`, holding what boot
needs from firmware and nothing that needs AML: the CPUs with their hardware ids (APIC ID or MPIDR) and
enable method, the interrupt controllers, the timers, the consoles, the PCI host bridges (segment, bus
range, and ECAM base), and the memory that must stay out of the buddy (the device tree's
`/reserved-memory` and its header's reservation block). The ACPI tables ([§7.1](#71-acpi), and on
aarch64 from ROADMAP §20.7) and the device tree (ROADMAP §11.5) each fill it, and SMP bring-up, the IRQ
layer, and the device registry read only it, so a firmware format is one more producer, never another
consumer path. A device, its resources, and its INTx routing are not in it: a driver binds through
ROADMAP §6.1's registry to a firmware-node handle, a device-tree node or an ACPI namespace node, matched
by compatible string or `_HID`, whose resources ROADMAP §20.2's `_CRS` and `_PRT` fill at runtime and
hot removal can take away. Planned (ROADMAP §11.5); today the ACPI parser's results feed x86_64's
consumers directly.

The mechanism:

- A concern whose Seam is a trait is a trait in `vibeos-core` (`PageTable`, `Barriers`,
  `CycleCounter`, and so on). Each port has two halves. Its pure half lives in `vibeos-core` under
  `arch/<name>/` and holds the logic that touches no hardware: descriptor and page-table encodings,
  the trap decode, register-frame and uapi layouts, and interrupt-controller command encodings. It
  compiles and runs its host tests on every host with no `cfg`, so x86_64 CI and the dev Mac test
  both ports' encodings. Its hardware half lives in the kernel crate under `arch/<name>/`, the
  assembly and the system-register and port access, and implements the traits on one zero-sized
  type: `arch::x86_64::Arch` or `arch::aarch64::Arch`. `vibeos-core` carries a third implementation,
  `arch::stub::Arch`, which the host tests use.
- Portable code that needs the seam takes the port as one type parameter of the type that uses it,
  never as `dyn`, so every seam call is resolved at compile time and inlines. A leaf type bounds the
  parameter by the traits it calls (`Mapper<A: PageTable>`, `SplitQueue<A: Barriers>`), so its
  bounds say what it depends on and a host test can hand it a fake for that one concern. A type that
  holds several seam users still takes one parameter and bounds it by the umbrella trait `Port`,
  whose supertraits are the table's traits, so a second port parameter never spreads into the types
  that hold it. The kernel crate names the concrete types once, in `arch::current` (for example
  `type AddressSpace = vibeos::AddressSpace<Arch>`), so kernel code never spells the parameter.
- The kernel binary names its port once, `type Arch = arch::current::Arch;`, chosen by
  `cfg(target_arch)` in the kernel crate. `vibeos-core` contains no `cfg(target_arch)` and no
  assembly, test modules included (ROADMAP Phase 10 gate); `scripts/check_core_stable.py` enforces
  it from ROADMAP §10.3's A2 box.
- The atomics seam is the one exception: a module selected by `cfg(loom)` (ROADMAP §10.8), because
  loom replaces types, not functions.

Why: the stub port lets the portable crate build and run its tests on any host, which ROADMAP §10.3
calls the cheapest second architecture, and the compiler checks that a port implements every
function. A port's pure half sits in `vibeos-core` so that one host build tests both ports: the
kernel crate does not build for the host, and ROADMAP §11.2's break-before-make refusals and §11.5's
device-tree parser need host tests. One table keeps ROADMAP and this section from drifting apart, as
two concern lists had. §1.1's "concrete over generic" allows the traits because there are three
implementations. Rejected: `dyn` traits (an indirect call in the page-table and barrier hot paths,
and no inlining); `cfg`-selected modules inside `vibeos-core` (the portable crate would carry
`cfg(target_arch)` and could not be built against a stub on a third host); link-time `extern`
symbols (signatures the compiler does not check, and one implementation per test binary); every port
file in the kernel crate, which left aarch64's descriptor and trap-decode logic untestable on x86_64
CI and on the dev Mac; a parameter per concern on a type that holds several seam users, which
multiplies parameters along every type that holds it; and one description per firmware format
(ACPI-shaped on x86_64, device-tree-shaped on aarch64), which ROADMAP §20.7 would have to translate
between or add a third path to.

## 11.2 Address space on aarch64

The kernel half is TTBR1 with a 48-bit VA (`T1SZ` 16); user space is TTBR0, also 48-bit, so user
addresses run to 2^48, Linux arm64's `TASK_SIZE` for 48-bit VAs, where x86_64's stop one page below
2^47 (`USER_MAP_END`, ROADMAP §10.6); each architecture's limit is a constant of its port (§11.1).
§4.1's fixed regions sit inside the TTBR1 range unchanged, and the physmap base is Limine's HHDM
offset, as on x86_64 (§4.1). The physmap's slot (§4.1) is `0xFFFF_0000_0000_0000` –
`0xFFFF_6000_0000_0000`, where Limine's 48-bit HHDM starts; Limine's draw there is below 64 TiB, so
RAM that ends at or below 32 TiB fits under every draw. TTBR1's tables serve every address space, so
§4.1's PML4-slot rule has no aarch64 counterpart.
The low identity window and the AP trampoline page have no aarch64 counterpart: cores start through
PSCI on a temporary TTBR0 identity map that is dropped once they run in the kernel half (ROADMAP
§11.4). Memory attributes come from MAIR, and MMIO is Device-nGnRE through `ioremap` (ROADMAP §11.1).
Every kernel-half descriptor sets UXN, so EL0 can execute nothing in the kernel half whatever its
access permissions say; §5.10 rule 10 lets a writer set any PC because of it (ROADMAP §11.2).
In the KASAN build (§4.1) the shadow offset is `0xDFFF_8000_0000_0000`, Linux arm64's generic-KASAN
offset for 48-bit VAs, so the TTBR1 range's shadow is `0xFFFF_6000_0000_0000` –
`0xFFFF_8000_0000_0000`, below §4.1's fixed regions and just above the physmap's slot, so
`boot::capture`'s slot check and the physmap builder's bound (§4.1) keep the physmap out of it.

Planned (ROADMAP §11.1, §11.6): `TCR_EL1.TBI0` is 1 and `TBI1` is 0, as Linux arm64 sets them, so EL0
loads and stores ignore bits 63:56 of a user address and user code may keep a tag there, while a kernel
address is never tagged, except in ROADMAP §18.4's tag-based KASAN build. The syscall boundary follows
Linux's default untagged ABI: the ROADMAP §10.6 range check refuses a user pointer whose bits 63:56 are
not zero with `EFAULT`, as Linux does for a process that has not opted in, and the fault path clears
those bits from `FAR_EL1` before it uses the address, so neither the lookup nor a signal's `si_addr`
sees the tag. The opt-in, `PR_SET_TAGGED_ADDR_CTRL`, is ROADMAP §18.4's; until it lands that `prctl`
returns `EINVAL`, and `docs/LINUX.md` lists the difference.

Planned (ROADMAP §11.2): ASIDs, so a context switch changes TTBR0 without flushing the TLB. The
allocator is Linux arm64's generation scheme. An address space holds one 64-bit value, a generation
counter above its ASID bits, and each CPU holds an atomic `active_asid`. A switch into an address
space whose generation is current publishes that value with a compare-exchange against the CPU's
`active_asid` and loads TTBR0. Any other switch takes the allocator lock, keeps the space's old ASID
if it is free or reserved in the current generation, and otherwise takes a free one. When none is
free, the allocator rolls over under the same lock: it starts a new generation, exchanges each CPU's
`active_asid` for 0 and reserves the ASID it held (a CPU whose slot already reads 0 keeps the ASID
it had reserved), so an address space running at rollover keeps its number, and it marks every CPU
flush-pending. A compare-exchange that races a rollover finds 0, fails, and takes the lock. A
flush-pending CPU runs `tlbi vmalle1`, `dsb nsh`, and `isb` on itself before it loads any ASID of
the new generation, and no rollover broadcasts a flush: until a CPU has flushed, every ASID its TLB
may hold still names the one address space it named there. `TCR_EL1.A1` is clear, so one TTBR0 write
changes the root and the ASID together. ASIDs are 16 bits where `ID_AA64MMFR0_EL1.ASIDBits` reports
them, with `TCR_EL1.AS` set to match, and 8 bits otherwise. ASID 0 is reserved for the empty user
root and ROADMAP §11.4's bring-up identity map. Reservation needs more usable ASIDs than CPUs, so
where 2^bits − 1 does not exceed the possible-CPU count the port uses no ASIDs, runs `tlbi aside1`
for ASID 0 on every TTBR0 switch, and says so at boot. The allocator is portable code in
`vibeos-core`, which host tests and a loom model run (ROADMAP §10.8). x86_64's PCID (ROADMAP §18.3)
is a separate scheme, §7.9's flush generations.

Why: a rollover that only broadcasts a flush is not safe. A CPU still running an address space from
the old generation refills its TLB under that ASID after the flush, and the address space that
receives the ASID next reads and writes through those entries. Reserving the running ASIDs, and
flushing each CPU before it uses the new generation, closes that with no IPI. Rejected: no ASIDs (a
full refill on every switch; kept only as the fallback above); per-CPU ASID spaces (an address space
would carry a different ASID on each CPU, so a broadcast `tlbi aside1is` could not name it, and user
unmaps would need IPIs again); and one allocator shared with x86_64, whose PCIDs are per-CPU slots
because x86 has no broadcast invalidation.

## 11.3 Adding an architecture

A port adds `arch/<name>/` in both crates, its pure half in `vibeos-core` and its hardware half,
implementing every trait the §11.1 table names, in the kernel crate; its column in that table; its
rows in `docs/ARCH.md`; a harness profile; and its marker list (ROADMAP §11.7). It changes no shared
module. ROADMAP §11.8's riscv64 stretch measures that: a port that needs a change outside the two
`arch/` directories, or a concern the table lacks, has found a seam defect, and the fix lands in the
seam (a new row or a changed trait), not as a special case in portable code.

## 11.4 EL0 and ring-3 environment

The CPU controls that change what an instruction does at EL0 or at CPL 3, with the value every CPU
holds and what user code sees. Every CPU writes `SCTLR_EL1`, `CNTKCTL_EL1`, `PMUSERENR_EL0`, and CR4
whole at bring-up, each from one value the port computes, never read-modify-write, so nothing firmware
or Limine left reaches user code. An MSR carries model-specific bits the kernel does not own, so of an
MSR only the bits this table names are written. The values are Linux's on each architecture, so
software that runs on Linux sees the same machine (ROADMAP, How to read this). A control here holds one
value for every thread. A line that makes one per-thread (Linux's `PR_SET_TSC` for `CR4.TSD`,
`ARCH_SET_CPUID`, `perf_user_access` for `PMUSERENR_EL0`) moves it to §7.5's per-thread table under
AGENTS.md rule 8, and a line that changes a value changes its row in the same commit. Rule; not yet
enforced: ROADMAP §11.6. Today `arch::cpu::harden` and `syscall_init::init_fpu` set bits in the CR4
they read, and the aarch64 port does not exist.

| Architecture | Control | Value | What user code sees |
|---|---|---|---|
| aarch64 | `SCTLR_EL1.UCT` | 1 | EL0 reads `CTR_EL0`, as a JIT's `__clear_cache` does to size its cache maintenance |
| aarch64 | `SCTLR_EL1.UCI` | 1 | `dc cvau`, `dc cvac`, `dc civac`, and `ic ivau` run at EL0, so a JIT makes its own code coherent |
| aarch64 | `SCTLR_EL1.DZE` | 1 | `dc zva` runs at EL0 and `DCZID_EL0.DZP` reads 0; glibc's `memset` uses it |
| aarch64 | `SCTLR_EL1.UMA` | 0 | an EL0 access to `DAIF` (`msr daifset`, `msr daifclr`, `mrs`) traps and gets `SIGILL`, so user code never masks an interrupt (§2.5) |
| aarch64 | `SCTLR_EL1.nTWE` | 1 | `wfe` runs at EL0 |
| aarch64 | `SCTLR_EL1.nTWI` | 0 | an EL0 `wfi` traps, and the exception handler steps over it, so it returns at once with no signal, as on Linux arm64 |
| aarch64 | `SCTLR_EL1.SA0` | 1 | an EL0 load or store through a misaligned SP raises an SP alignment fault and gets `SIGBUS` |
| aarch64 | `SCTLR_EL1.A` | 0 (ROADMAP §11.1) | an EL0 load or store to Normal memory may be unaligned, as on Linux arm64; the user crate's `aarch64-unknown-linux-musl` code is not built for strict alignment and relies on it |
| aarch64 | `SCTLR_EL1.E0E` | 0 | EL0 is little-endian |
| aarch64 | `SCTLR_EL1.SPAN` | 0 (FEAT_PAN is in the §3.1 floor) | nothing directly; every exception entry to EL1 sets PAN (ROADMAP §11.6) |
| aarch64 | pointer authentication (`EnIA`, `EnIB`, `EnDA`, `EnDB`), BTI (`BT0`), and MTE (`ATA0`, `TCF0`) in `SCTLR_EL1` | 0 | off until ROADMAP §18.9 (pointer authentication, BTI) and §18.4 (MTE) turn them on and change this row |
| aarch64 | `TCR_EL1.TBI0` (`TBI1` 0) | 1 | EL0 loads and stores ignore bits 63:56 of the address, so a program may keep a tag there; a system call refuses a tagged pointer ([§11.2](#112-address-space-on-aarch64)) |
| aarch64 | every other `SCTLR_EL1` field | the port's constant, with each field's reason beside it | no EL0-visible effect |
| aarch64 | `CNTKCTL_EL1.EL0VCTEN` | 1 | EL0 reads `CNTVCT_EL0` and `CNTFRQ_EL0`, which the ROADMAP §13.10 vDSO clock reads |
| aarch64 | `CNTKCTL_EL1.EL0PCTEN` | 0 | an EL0 read of `CNTPCT_EL0` traps and gets `SIGILL`; Linux arm64 also leaves this bit clear |
| aarch64 | `CNTKCTL_EL1.EL0VTEN`, `EL0PTEN` | 0 | an EL0 access to the virtual or physical timer registers gets `SIGILL`; at EL1 entry the virtual timer is the kernel's tick (ROADMAP §11.3), so a user write to it would stop preemption |
| aarch64 | `CNTKCTL_EL1.EVNTEN`, `EVNTI` | 1, about 10 kHz from `CNTFRQ_EL0` | an EL0 `wfe` wakes at least that often, and `AT_HWCAP` carries `HWCAP_EVTSTRM`, as on Linux arm64 |
| aarch64 | `PMUSERENR_EL0` | 0 | every EL0 PMU register access gets `SIGILL`, as under Linux's default `perf_user_access` of 0 |
| aarch64 | EL0 `mrs` of an ID register | traps | `SIGILL`, and `AT_HWCAP` carries no `HWCAP_CPUID`, until ROADMAP §23.1 emulates the sanitized fields |
| x86_64 | `CR4.TSD` | 0 | `rdtsc` and `rdtscp` run at CPL 3, as the ROADMAP §13.10 vDSO clock needs |
| x86_64 | `CR4.PCE` | 0 | `rdpmc` at CPL 3 raises `#GP` and gets `SIGSEGV` |
| x86_64 | CPUID faulting (`MSR_MISC_FEATURES_ENABLES` bit 0, where `MSR_PLATFORM_INFO` bit 31 enumerates it) | 0 | `cpuid` runs at CPL 3 |
| x86_64 | `CR4.UMIP` | 1 where CPUID enumerates it (§5.1) | `sgdt`, `sidt`, `sldt`, `smsw`, and `str` at CPL 3 raise `#GP` (§5.2) |
| x86_64 | `CR4.OSXSAVE`, `CR4.PKE` | 0 (ROADMAP §11.1, F130) | `xgetbv`, `rdpkru`, and `wrpkru` raise `#UD` and get `SIGILL`; ROADMAP §13.8 changes the `OSXSAVE` row if it chooses XSAVE |
| x86_64 | `CR4.FSGSBASE` | 0 until ROADMAP §18.3 | `rdfsbase`, `wrfsbase`, `rdgsbase`, and `wrgsbase` raise `#UD` |
| x86_64 | `CR0.AM` | 1 (ROADMAP §10.6) | a misaligned access while ring 3 has set `RFLAGS.AC` raises `#AC` and gets `SIGBUS` (§5.2), as on Linux |

At EL2 with VHE, `CNTKCTL_EL1` names `CNTHCTL_EL2`, whose EL0 fields sit at the same bits. The boot
CPU computes that register's whole value with the EL0 fields above, which clears the `EL0PCTEN` bit
Limine sets at EL2 entry, and ROADMAP §11.4's stub writes the same value on every core, as it writes
the boot CPU's `SCTLR_EL1`.

Why: user code depends on these values (a JIT's cache maintenance, glibc's `memset`, the vDSO's clock),
and the kernel's own safety depends on others (`UMA`, `EL0VTEN`). Their reset values are UNKNOWN.
Limine's base revision 6 enters with `UCT`, `UCI`, and `DZE` clear and `SPAN` set, gives no value for
`CNTKCTL_EL1` or `PMUSERENR_EL0`, and at EL2 sets `CNTHCTL_EL2.EL0PCTEN`; QEMU's zero reset values would
hide a missing write from every TCG test. Rejected: keeping what firmware or Limine left; setting bits
in the value read at boot, which keeps any bit this table does not name; and giving EL0 the physical
counter or a timer, which Linux does not, and which at EL1 entry would let user code reprogram the
kernel's tick.

## 11.5 aarch64 exceptions and privilege transitions

aarch64's counterpart of §5.1 to §5.3 and of the x86_64 mechanism behind §5.10; §5.10's aarch64 table
gives the required state at each boundary, and its rules 9 onward hold on both architectures. The
kernel runs at EL1, or at EL2 with VHE when Limine entered there (ROADMAP §11.1); at EL2 the `_EL1`
names below reach their `_EL2` registers through VHE's redirection, and `VBAR_EL2`, `ESR_EL2`,
`FAR_EL2`, `ELR_EL2`, and `SPSR_EL2` take the roles given here to the `_EL1` ones. Planned (ROADMAP
§11.3, §11.6): the aarch64 port does not exist.

1. One vector table, generated by the port from one list, as §5.10 rule 1 generates x86_64's stubs.
   Of its 16 entries (synchronous, IRQ, FIQ, and SError, each taken at the current level on
   `SP_EL0`, at the current level on `SP_ELx`, from EL0 in AArch64, and from EL0 in AArch32), the
   kernel expects only the `SP_ELx` and AArch64-EL0 ones, since it always runs on `SP_ELx` and EL0
   has no AArch32; each of the other eight dumps and halts. Every entry runs rule 6's stack test
   before its first store. An entry from EL0 saves the user frame (§5.10), `ELR_EL1` and `SPSR_EL1`
   included, and every entry saves the syndrome (§5.10 rule 9), before it clears any DAIF bit.
2. DAIF. Every exception sets all four bits. Once its frame is saved, an entry clears D and A; a
   syscall or a fault or trap body then clears I and F too (§2.9 rule 3), and an IRQ's top half
   keeps them set. The kernel masks and unmasks I and F together, since nothing routes a FIQ to it.
   Outside the entry and exit sequences D and A are clear, so a debug exception or an SError is
   taken where it is raised. With A set in the kernel, a pending SError would be taken right after
   the next `eret` to EL0 and charged to whichever process was returning.
3. The return to EL0 sets all of DAIF before it writes `ELR_EL1`, `SPSR_EL1`, or `SP_EL0`, and keeps
   it set until `eret` loads EL0's DAIF from `SPSR_EL1`; §5.10 rule 11's last exit-work check comes
   before those writes. A debug exception or an SError taken between the writes and the `eret` would
   overwrite `ELR_EL1` and `SPSR_EL1`, and the `eret` would then return to the kernel PC that
   exception saved, at the kernel's level. Masking only I, as x86_64's IF alone suggests, leaves
   that window open once rule 2 unmasks A or ROADMAP §18.4 arms a kernel watchpoint. A debug build
   checks DAIF before each `eret` to EL0.
4. PAN. `SCTLR_EL1.SPAN` is clear, so every exception entry sets PSTATE.PAN, and only the
   user-memory accessors clear it (ROADMAP §11.6), as §5.10 rule 5 opens SMAP only inside the
   accessors.
5. No GS swap. EL0 cannot access `TPIDR_EL1` or `TPIDR_EL2`, so the per-CPU base never changes at
   the boundary, and §5.10 rules 2 and 3 have no counterpart. At the kernel's level `SP_EL0` holds
   the running thread's TCB pointer ([§2.9](#29-preemption-and-interrupt-state) rule 5): every entry
   from EL0 saves the user's `SP_EL0` into the frame and loads `current`, and the return to EL0
   restores the user's after it sets DAIF (rule 3).
6. Stack overflow. aarch64 has no IST: an exception taken at the kernel's level runs on the stack it
   interrupted, so on an overflowed stack the entry's first store faults again, and each nested entry
   moves SP further down, past the guard and into whatever mapping lies below. Every vector entry
   therefore tests, before its first store, bit log2(*S*) of SP less its frame, which §4.5's layout
   sets exactly when that address is in a guard. It exchanges SP and `x0` by adding and subtracting,
   so the test touches no memory and needs no scratch register. When the bit is set, the entry
   stashes `x0` and the faulting SP in `TPIDR_EL0` and `TPIDRRO_EL0`, switches to this CPU's overflow
   stack (16 KiB, allocated at bring-up in §4.5's layout, so an exception taken on it passes the
   test), and prints `vibeOS: panic: stack overflow` with `FAR_EL1`, `ESR_EL1`, `ELR_EL1`, the
   stashed SP, and the current thread, then halts through §2.5. It keeps DAIF set until it halts, so
   it reads the syndrome from the registers rather than from a frame (§5.10 rule 9). The two EL0
   registers are free there because the path never returns to EL0 (§7.5). The full vector table goes
   live only once the bootstrap thread runs on a stack in §4.5's layout (ROADMAP §10.6), so Limine's
   stack meets only ROADMAP §11.1's early vector table. This is the check Linux arm64 makes for its
   virtually mapped stacks.

Exception classes. Each port decodes a trap into a portable `TrapKind` in its `vibeos-core` half, and
one table there gives each `TrapKind` its ring-3 action, the signal and the `si_code` Linux sends, or
says it is not a ring-3 fault (§5.2 for x86_64's vectors). On aarch64 the decode reads the vector
slot and, for a synchronous exception, the exception class (EC) and ISS of `ESR_EL1` and an abort's
fault status code. The Ring 3 column is Linux arm64's. A host test runs every EC from `0x00` to
`0x3F`, and the IRQ, FIQ, and SError slots, through decode and table, and pins each row below.
Planned (ROADMAP §11.3, §11.6).

| EC | Class | Ring 0 (at the kernel's level) | Ring 3 (from EL0) |
|----|-------|--------------------------------|-------------------|
| `0x00` | unknown, such as `udf` | dump, halt | `SIGILL`, `ILL_ILLOPC` |
| `0x01` | trapped `wfi` or `wfe` | dump, halt | no signal: the handler steps over the instruction |
| `0x07` | FP or SIMD access | dump, halt | cannot occur while `CPACR_EL1.FPEN` stays on (§7.5); `SIGILL`, `ILL_ILLOPC`, with an `fp: unexpected trap` log line |
| `0x0D` | branch target (BTI) | dump, halt | `SIGILL`, `ILL_ILLOPC`; BTI is off until ROADMAP §18.9 |
| `0x0E` | illegal execution state | dump, halt | `SIGILL`, `ILL_ILLOPC` |
| `0x15` | `svc` | not taken: the kernel makes no `svc` | the syscall path |
| `0x18` | trapped `mrs`, `msr`, or system instruction | dump, halt | `SIGILL`, `ILL_ILLOPC`, unless a ROADMAP line emulates the register (§23.1) |
| `0x19`, `0x1D` | SVE, SME access | dump, halt | `SIGILL`, `ILL_ILLOPC` (ROADMAP §11.6), until §23.1 gives their state a first-use setup |
| `0x1C` | pointer-authentication failure | dump, halt | `SIGILL`, `ILL_ILLOPN`; pointer authentication is off until ROADMAP §18.9 |
| `0x20`, `0x21` | instruction abort (`0x20` from EL0, `0x21` at the kernel's level) | dump with `FAR_EL1`, halt | the fault path (ROADMAP §12.2): an unresolved translation, access-flag, or permission fault gets `SIGSEGV`, `SEGV_MAPERR` or `SEGV_ACCERR`; an alignment fault `SIGBUS`, `BUS_ADRALN`; a synchronous external abort `SIGBUS`, `BUS_OBJERR`, until ROADMAP §25 classifies it |
| `0x24`, `0x25` | data abort (`0x24` from EL0, `0x25` at the kernel's level) | dump with `FAR_EL1`, halt; a fault inside a user-memory accessor is handled by that accessor's kind (§5.1) and ends in `EFAULT` or a short count (ROADMAP §11.6) | as `0x20` |
| `0x22` | PC alignment | dump, halt | `SIGBUS`, `BUS_ADRALN` |
| `0x26` | SP alignment | dump, halt | `SIGBUS`, `BUS_ADRALN` |
| `0x2C` | trapped floating-point exception | dump, halt | `SIGFPE`, with the `si_code` of the exception `FPSR` reports |
| `0x30`, `0x31` | breakpoint | as §5.2's `#DB` row | `SIGTRAP`, `TRAP_HWBKPT` |
| `0x32`, `0x33` | software step | as §5.2's `#DB` row | `SIGTRAP`, `TRAP_TRACE` |
| `0x34`, `0x35` | watchpoint | as §5.2's `#DB` row | `SIGTRAP`, `TRAP_HWBKPT` |
| `0x3C` | `brk` | dump, halt | `SIGTRAP`, `TRAP_BRKPT` |
| every other class | AArch32, reserved, or a feature the kernel leaves off | dump, halt | `SIGILL`, `ILL_ILLOPC`, as Linux sends for a class it does not handle |
| IRQ slot | | handle, return | handle, return to EL0 |
| FIQ slot | | dump, halt: nothing routes a FIQ to the kernel | not a ring-3 fault: the Ring 0 column applies |
| SError slot | | dump with `ESR_EL1`, halt, until ROADMAP §25 classifies RAS errors | not a ring-3 fault: the Ring 0 column applies, as for `#MC` |

A ROADMAP line that maps device memory into EL0 or into a guest, such as §28.4's VFIO, states how an
SError that such an access raises is charged, since the SError row would otherwise let that access
halt the kernel.

Why: these are the rules §5.10's x86_64 rows exist for, restated for a machine with no IST, no GS
swap, and four interrupt masks instead of one. Linux arm64 enters and leaves EL0 the same way: all of
DAIF set on the way out, D and A cleared once the frame is saved, and `current` in `SP_EL0`.
Rejected: interleaving aarch64 paragraphs through §5.1 to §5.7, which would double each section and
mix two machines' registers; a separate `docs/ARCH_AARCH64.md`, which would split the invariant
tables that AGENTS.md rules 1, 2, and 8 point at; and masking only I on the way out (rule 3).

---

# 12. Device model

A device is what a driver binds to: a PCI function, a device the device tree or ACPI describes, a
USB device, a partition, a stacked block device. [§2.11](#211-object-lifetimes) gives every device
its lifetime rules. This section adds the tree the devices form, the order their callbacks run in,
and who owns a device's resources. It is Linux's driver model (a device tree with supplier links,
deferred probe, one lock per device) without kobjects: ROADMAP §23.3's sysfs renders this tree and
does not own it.

## 12.1 Devices

1. A device is a counted object (§2.11 rule 1) in one registry. A lookup by bus address, name, or
   device number returns a `DevRef`, a counted reference, and a block device's handle is a
   `BlockRef`. Nothing hands out `&'static` to a device or to a driver's per-device state, and no
   device record is `Copy`. A driver's static operations object may be `&'static`; the state for
   each device it binds is owned by that device's registry entry.
2. Every device but a root has a parent: a PCI function its bridge or root port, a USB device its
   hub, a partition its disk. A device may also name suppliers outside the tree: the IOMMU that
   translates it, the ITS its MSIs go through, and the members of a dm or md device, which list that
   device as a holder.
3. A device is `Present`, `Probing`, `Bound`, `Resetting`, `Failed`, `Suspended`, `Removing`, or
   `Dead`. `Resetting` and `Failed` are [§10.3](#103-failure)'s error-handling states. `Failed` and
   `Dead` are terminal for a registration. One sleeping lock per device serializes `probe`,
   `remove`, `suspend`, `resume`, and `shutdown` on it, and the registry's own lock is never held
   across a driver callback.
4. A block device's id is a 64-bit sequence number never reused within a boot, as Linux's `diskseq`
   is, so the block cache ([§10.6](#106-block-cache)) and any other table keyed by it cannot alias a
   later device. A device's name is owned by its registry entry.

## 12.2 Order

5. Probe and resume run a device's parent and suppliers before it; remove, suspend, and shutdown run
   its children and consumers before it. Removing a device removes its subtree, deepest device
   first. The order is per device, not per driver, because one driver sits at several depths: nested
   USB hubs, a chain of PCI bridges, dm on dm.
6. A probe whose supplier is not bound returns `Defer`. The binder retries deferred devices after
   each successful bind, and at the end of boot it logs each device still deferred with the supplier
   it waits for, as Linux's deferred probe does. A device that an IOMMU translates gets its domain
   before its first probe: when it is added if the IOMMU is registered, and otherwise when the IOMMU
   registers.
7. A driver's `shutdown` and the stop step of its `remove` are one quiesce function
   (AGENTS.md rule 10). Both begin with §10.3's queue quiesce, which suspend and a reset begin with
   too.

## 12.3 Resources

8. A probe owns its device's resources. It maps only the MMIO and I/O ranges it holds a claim for: a
   PCI BAR, a device-tree `reg` entry, or an ACPI `_CRS` range. A claim is not `Copy`, and it is the
   only way to map a range. The registry refuses a claim that overlaps another claim or a RAM-typed
   range of the boot memory map. The driver enables its device's memory decode before it touches the
   device and bus mastering after it resets it; the binder enables neither. A failed probe, and
   `remove`, reset the device and clear bus mastering before they free memory the device was given
   ([§5.4](#54-irq-registration)).

Why: a device is freed at its last put and never before, so a hot removal, a partition-table reread,
or a USB unplug cannot leave a handle to freed memory, and an id that is never reused cannot hand an
old device's cached pages to a new one. S3 (ROADMAP §20.2), subtree removal (§20.9), USB hubs
(§20.3), and stacked block devices (§29.1) each need an order between devices, which a flat list
cannot give. A driver that can map a range only through a claim cannot forget to check the range for
an overlap or a RAM alias. Rejected: `&'static` devices that are never freed, which leak a device
per hotplug or reread and which §2.11 rejects for every object; a flat registry ordered by a
per-driver number, which cannot express a hub tree, subtree removal, or an IOMMU before the devices
it translates; and Linux's kobject core, which is more than this needs.

Rule; not yet enforced: the registry is a fixed array of `Copy` PCI records with no parent or state,
bound in a per-driver `order()` (ROADMAP §6.1). `dev_init::bind_all` probes a copy of each record
and writes it back, and it turns on memory decode and bus mastering before `probe`, as `irq_init`'s
MSI and MSI-X setup does again. `pci_init` maps every memory BAR of every function at enumeration,
before any claim ([§3.3](#33-_start-order)). Block devices are named by `&'static str` and reached
by fixed ids (§10.6). ROADMAP §10.4 (D2) and §10.12 land rules 1 to 4 and 8 for PCI and block
devices, §11.5 and §20.7 apply rule 8 to device-tree and ACPI devices, §18.1 lands rule 6, and
§20.2, §20.3, §20.9, and §25.4 land rules 5 and 7 for suspend, hubs, removal, and shutdown.

## 12.4 Removal

9. Removal kills a device in §2.11 rule 3's order. It unpublishes the device from the registry, its
   `/dev` node, and its `/dev/disk/by-id` link, and from ROADMAP §23.3 on it sends a `remove`
   uevent. It closes the device's gate, so every later operation through a `DevRef` or `BlockRef`
   still held fails with `Gone`, and threads sleeping on the device wake and fail. It waits for the
   operations already inside, whose requests complete, fail at their deadline
   ([§10.3](#103-failure)), or fail at once on a disconnected function. It stops the device and
   fails what the device still holds through the error handler (§10.3 step 7), and frees its
   vectors ([§5.4](#54-irq-registration)). It then detaches the device's IOMMU domain and completes
   the IOTLB and device-TLB invalidation, and only then frees the device's DMA buffers. A
   device-TLB invalidation is skipped for a function marked disconnected, as Linux's VT-d driver
   skips it, since a gone function never answers. Every IOMMU invalidation wait (VT-d's wait
   descriptor, SMMUv3's `CMD_SYNC`) has a bound, 1 s as Linux's SMMUv3 driver uses. When it
   expires, the device's domain is set to blocking, its IOVAs and the frames behind them stay
   reserved, and the event is logged. Its memory goes at the last put. A device's subtree is
   removed first (rule 5).
10. A surprise removal, which the slot reports through its presence-detect or link-down interrupt,
    first marks the function disconnected: its driver fails I/O at once without resetting it, and a
    register read that returns all ones ends any poll.
11. A network interface being removed also deletes its routes and neighbour entries, leaves any
    bridge, and fails sends on sockets bound to it with `ENODEV`. Its ifindex is allocated in
    increasing order and not reused at once, as a pid is (§2.11 rule 4).

Why: a surprise removal cannot be refused, and a removal that waited for every holder to close would
hang behind a shell's open descriptor, so removal fails the holders' operations and lets them close
when they will, as Linux does. The IOMMU and interrupt-remapping steps come before memory and
vectors are reused, because a device that is stopped but still translated can write a freed buffer,
and a remapping entry left behind lets it raise the next owner's vector. Rejected: refusing removal
while a filesystem is mounted, which a surprise removal cannot honor; forcing an unmount at removal,
which races open descriptors and hides the error from the programs that hold them.

Planned (ROADMAP §15.1, §20.3, §20.9): nothing is removed today.
