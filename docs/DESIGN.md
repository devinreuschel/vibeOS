# Design

vibeOS is a monolithic kernel in Rust for x86_64 and, from ROADMAP Phase 11, aarch64 as a peer
([§11](PORTABILITY.md#11-portability)). `no_std`, `alloc` enabled once the heap is up. Limine boots the ELF in long
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

This file and the topic files its Contents table names record decisions. [ROADMAP.md](ROADMAP.md)
records what has landed. Present tense describes the code at the commit that last changed the sentence.
A rule the code does not meet yet says "Rule; not yet enforced" or "Planned" and names the ROADMAP line
that lands it. An `Fnnn` id names a finding in the kernel review
([reviews/KERNEL_REVIEW.md](reviews/KERNEL_REVIEW.md)); the ROADMAP section cited with it lands the
fix. A design-review id (`Gnnn`, `Hnnn`, `Jnnn`) names a row of
[reviews/DESIGN_REVIEWS.md](reviews/DESIGN_REVIEWS.md). When code and these files disagree, one of them
is a bug ([§1.4](#14-documentation-rules)). §2's invariants bind every ROADMAP box, and a box that
changes other text here changes it in the same commit (ROADMAP, How to read this). The numbers are
load-bearing. Change them deliberately and update the doc in the same commit. An agent implementing a
subsystem does not get to re-litigate the address map, the vector numbers, or the lock order halfway
through.

## Contents

| | Section | File | Covers |
|---|---------|------|--------|
| 1 | [Overview](#1-overview) | this file | Constraints, layers, module map, sources and licenses |
| 2 | [Invariants](INVARIANTS.md#2-invariants) | [INVARIANTS.md](INVARIANTS.md) | Lock order, handler rules, panic policy, markers, invariant register, publish last, preemption, trust boundaries, object lifetimes, RCU |
| 3 | [Boot](BOOT.md#3-boot) | [BOOT.md](BOOT.md) | Toolchain, Limine, `_start` order, linker |
| 4 | [Memory](MEMORY.md#4-memory) | [MEMORY.md](MEMORY.md) | Address map, buddy allocator, paging, heap, firmware runtime services |
| 5 | [Interrupts](INTERRUPTS.md#5-interrupts) | [INTERRUPTS.md](INTERRUPTS.md) | GDT/IDT, exception policy, vector map, PIC and APIC, privilege transitions |
| 6 | [Time](TIME.md#6-time) | [TIME.md](TIME.md) | Clock sources, calibration, timekeeping, timers |
| 7 | [SMP](SMP.md#7-smp) | [SMP.md](SMP.md) | ACPI, AP bring-up, per-CPU, IPIs, shootdown, offline and online |
| 8 | [Testing](TESTING.md#8-testing) | [TESTING.md](TESTING.md) | Tiers, marker contract, QEMU flags, CI |
| 9 | [Pitfalls](PITFALLS.md#9-pitfalls) | [PITFALLS.md](PITFALLS.md) | Bugs already paid for once |
| 10 | [Block I/O](BLOCK.md#10-block-io) | [BLOCK.md](BLOCK.md) | Requests, ordering and flush, ramdisk, virtio-blk, partitions, cache |
| 11 | [Portability](PORTABILITY.md#11-portability) | [PORTABILITY.md](PORTABILITY.md) | The architecture seam and its inventory, the aarch64 address space, adding a port, the EL0 and ring-3 environment, aarch64 exceptions and privilege transitions |
| 12 | [Device model](DEVICES.md#12-device-model) | [DEVICES.md](DEVICES.md) | Devices, parents and suppliers, states, probe order, resources, removal |

Each section lives in the file its row names and keeps its number, so `DESIGN §x.y` names §x.y wherever it is. `python3 scripts/doc_refs.py --where x.y` prints the file, and `make check` runs `scripts/doc_refs.py`, which fails on a `DESIGN §` or `ROADMAP §` citation that names no heading.

On-disk filesystem formats live in their own docs, not here ([§1.4](#14-documentation-rules)): [VIBEFS.md](VIBEFS.md) (vibefs **version 1**, CoW metadata + atomic superblock switch). Syscall ABI: [SYSCALL.md](SYSCALL.md). The Linux baseline, deliberate differences from it, and native interfaces: [LINUX.md](LINUX.md).

---

# 1. Overview

## 1.1 Design constraints

These are not style preferences. They shape every subsystem.

1. **Concrete over generic.** One PMM, one scheduler, one page-table layout per architecture. No
   trait soup to support the second implementation nobody is writing. Traits appear where there are
   genuinely N backends: console output, block devices, filesystems, and the §11.1 architecture
   ports.
2. **Test the algorithm on the host.** Anything that is pure logic (parsers, allocators, state
   machines, encodings) lives in `vibeos-core` so `make test-unit` covers it. Hardware pokes stay
   in the kernel half. This split is the single biggest lever on iteration speed.
3. **Serial is the ground truth.** Each step of the §3.3 boot order prints the marker its row lists,
   and a row with none says why. Those lines are a contract enforced by the e2e harness, not debug
   noise.
4. **Fail loud, fail early.** Assert invariants at boot. Bounded spins everywhere so a wedged device
   produces a diagnosable hang instead of a silent one.
5. **No unbounded loops against hardware.** Every poll gets an iteration cap and a failure path.
6. **Layering is enforced by dependency direction**, not by wishful thinking. Lower layers do not
   call up, except through a hook an upper layer installs at init, and §1.2 lists each one. The
   buddy and the heap never call up, directly or by hook, but for the TLB shootdown a kernel-half
   mapping change makes (§4.3): an allocation reaches reclaim, the writeback wait, and the OOM
   killer only through the allocation entry's hooks (§4.4).
7. **The portable crate is stable Rust.** `vibeos-core` enables no `#![feature]` and builds with its
   MSRV (§3.1), the oldest Rust that Kani (ROADMAP §10.8) and Verus (Phase 38) use: each pins its
   own toolchain, older than the kernel's nightly, and must build the code the kernel links. Nightly
   features stay in the kernel binary. `scripts/check_core_stable.py` in `make check` finds no
   feature attribute in `src/lib.rs`. Rule; not yet enforced: nothing builds the crate with its
   MSRV, which would reject a feature attribute however it is formatted and any API or syntax newer
   than the MSRV (ROADMAP §10.1).

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
| fs | `fs/mod.rs`, `fs/kernfs.rs`, `fs/ramfs.rs`, `fat.rs`, `vibefs.rs` | `fs_init.rs`, `fat_init.rs`, `vibefs_init.rs`, `file_init.rs` |
| entropy | `entropy.rs` | `entropy_init.rs` |
| proc | `addr_space.rs`, `elf.rs`, `proc.rs`, `syscall.rs` | `addr_space_init.rs`, `user_init.rs`, `proc_init.rs`, `syscall_init.rs` |
| limits | `limits.rs` (every table and resource cap) | — |
| kalloc | `kalloc.rs` (fallible heap types; stub until ROADMAP §10.4) | — |
| fpu | `fpu.rs` (FP register binding; stub until ROADMAP §10.6) | — |
| trap | `trap.rs` (portable trap kinds and ring-3 actions; stub until ROADMAP §10.6) | — |
| ktest | — | `ktest.rs`, `ktest/p10_s*.rs` (`kernel_tests` only) |

## 1.4 Documentation rules

- Durable intent goes in this file and the topic files [Contents](#contents) names. Ephemeral "fixed
  X" notes go in `CHANGELOG.md` or nowhere.
- No code review writeups, no phase retrospectives, no status reports. The roadmap checkboxes are the
  status. `git log` is the history. The one status these files state is the gap between a rule and the
  code, written as the header says ("Rule; not yet enforced" with the ROADMAP line that closes it),
  so a rule is never read as a description of the code.
- "How does this function work" goes in a doc comment. "Why is this line here at all" goes in a short
  comment on the line. Nothing goes in a comment that describes a past state of the code.
- If a design doc describes behavior the code contradicts, one of them is a bug. Say which in the commit
  that fixes it.
- Constants appear once, in the design docs, and are cross-referenced rather than restated: the address map in
  [section 4.1](MEMORY.md#41-virtual-address-map), vector numbers in [section 5.3](INTERRUPTS.md#53-vector-map), and MSR
  numbers and values in [section 7.2](SMP.md#72-lapic). A number used as a name beside its name, such as a
  vector or an MSR number, may repeat; a size, mask, or value is cited, not copied.
- This file outgrew one page per subsystem, so §2 to §12 live in the `docs/<topic>.md` files
  [Contents](#contents) names. They keep DESIGN's section numbers, and this file is their index
  (ROADMAP §10.3, DOC2). A topic file that outgrows one page per subsystem splits the same way,
  keeping its numbers. On-disk formats are a split of the same kind: [VIBEFS.md](VIBEFS.md), not a
  novel in this file. Syscall ABI: [SYSCALL.md](SYSCALL.md). The Linux baseline, deliberate
  differences from it, and native interfaces: [LINUX.md](LINUX.md).

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
