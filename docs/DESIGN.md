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
   killer only through the allocation entry's hooks (§4.4). `scripts/check_cycles.py`, which `make
   check` runs, enforces it on the module graph of both crates: it fails on two modules that use
   each other, on an edge from `heap`, `heap_init`, `pmm` or `pmm_init` to a scheduler, VFS,
   block-cache or process module, and on any reference from the raw serial layer (`serial::raw`)
   to a kernel module but `arch::cpu`. Cycles of three or more modules it prints and does not fail.
7. **The portable crate is stable Rust.** `vibeos-core` enables no `#![feature]` and builds with its
   MSRV (§3.1), the oldest Rust that Kani (ROADMAP §10.8) and Verus (Phase 38) use: each pins its
   own toolchain, older than the kernel's nightly, and must build the code the kernel links. Nightly
   features stay in the kernel binary. `make check` builds the crate with its MSRV (§8.5), which
   rejects a feature attribute however it is formatted and any API or syntax newer than the MSRV.

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
- `x86::set_per_cpu_hooks` (`InterruptGuard`'s nesting count and `x86::cpu_index`), set by
  `per_cpu_init::init_bsp` before it marks the per-CPU area live.
- `serial::set_capture_hook` (the log ring's serial capture), set by `log_init::init` right after
  `Serial::init`, before the first marker.
- Planned (ROADMAP §10.7): `serial::raw::set_stop_hook`, set by the stop primitive.
- `sync_init::set_spin_poll` (`SpinMutex::lock`'s spin runs `service_incoming`), set by
  `ipi_init::init` before the first AP starts (§7.9).
- `idt::set_intercept_hook` (the exception intercept for vectors 0 to 31), set by `catch::init` right
  after `idt::init`.
- `idt::set_user_fault_hook` (a ring-3 fault's signal, `proc_init::try_user_fault`), set by
  `proc_init::init` right after `syscall_init::init_bsp`, before the first ring-3 entry.
- `idt::set_user_return_hook` (the FP binding check on a return to ring 3, §7.5), set by
  `syscall_init::init_bsp` before the first ring-3 entry.
- `thread_init::set_switch_hooks` (a context switch's hardware side, `syscall_init::on_switch`, and
  a new thread's FP image), set by `syscall_init::init_bsp` before the scheduler starts.
- `thread_init::set_kick_hook` (wake a CPU's workqueue worker to free its dead stacks), set by
  `work_init::init` before it starts the workers.
- `syscall_init::set_syscall_handler` (the syscall entry's handler, `proc_init::syscall`), set by
  `proc_init::init` before the first ring-3 entry; unset, a syscall returns `-ENOSYS`.
- `fs_init::set_test_hooks` (a `kernel_tests` build's File API hooks, `file_init::testing`), set by
  `file_init::init` before it brings the filesystems up.
- `ipi_init::set_reschedule_hook` (a reschedule IPI's preemption point), set by `sched_init::init`
  before the scheduler goes live.
- `ipi_init::set_slot_tid_hook` (the wake-inbox drain's slot-to-tid lookup,
  `thread_init::tid_of_slot`), set by `thread_init::init_bootstrap` before a second thread exists;
  unset, a drain leaves the inbox as it is.
- Planned (ROADMAP §12.6): the allocation entry's hooks, set by the page cache (clean-page reclaim),
  the writeback threads (their wake and bounded wait), and the process layer (the OOM killer).

A hook not yet set is skipped, so host tests and early boot run a lower layer alone: before the
reclaim hooks are set, an allocation goes from the reserve straight to failure.

## 1.3 Module map

As of 2026-09-28 (Phase 10). Files that exist; `scripts/check_module_map.py` in `make check` fails
when this table and the tree differ.

**Layout.** One directory per subsystem in each crate, with the same name in both. The portable half
of subsystem `<s>` is `crates/core/src/<s>/` (`vibeos-core`, host-tested by `make test-unit`), and its
kernel half is `src/<s>/` (the kernel binary, rooted at `src/main.rs`). A kernel module keeps the
`_init` suffix of its portable pair and sits in the directory that mirrors it (`mm/pmm.rs` and
`mm/pmm_init.rs`). A module named like its directory is that directory's `mod.rs`. Each crate root
re-exports its modules under their pre-move names (`vibeos::pmm`, `crate::pmm_init`, `crate::x86`).
`arch/x86_64/` is the x86_64 port: its pure half (encodings, trap decode) in `vibeos-core`, and its
hardware half in the kernel (DESIGN §11.1). The cells are portable (`cell.rs`, over the port's seam),
and the kernel's `cell.rs` names them over its port. `user/` holds user programs, not kernel modules: the
Rust user runtime `vibeos-user` (`user/src/`, its programs in `user/src/bin/`, and everything that names an
architecture in `user/src/arch/<arch>/`), its `#![no_builtins]` memory crate `vibeos-user-mem`
(`user/mem/`), both workspace members that `make user` builds for the user triple (BOOT.md §3.1), and,
until ROADMAP §10.5 ports them, the assembly programs `user/*.asm` of the initrd.

**Reading the table.** Paths are relative to `crates/core/src/` (Portable) and `src/` (Kernel).
`{a,b}` lists files of one directory, and `*` matches within one. Every listed path exists, and every
`.rs`, `.S` and `.asm` file under the two roots is listed once. Outside the `crate` row, a row's paths
lie in its subsystem's directory. Test bodies in `src/<s>/ktest.rs`, and its `src/<s>/ktest/<topic>.rs`
children, need no row.

| Subsystem | Portable (`crates/core/src/`) | Kernel (`src/`) |
|---|---|---|
| crate | `lib.rs`, `marker.rs`, `fmt_util.rs`, `symtab.rs`, `limits.rs`, `kalloc.rs`, `kerror.rs`, `trap.rs`, `atomic.rs`, `cell.rs` (`BootCell`, `IrqCell`, `CellHooks`) | `main.rs`, `cell.rs` (the kernel's names for the cells and its `CellHooks` impl) |
| boot | `boot/{mod,cmdline}.rs` | `boot/{mod,fw_cfg_init}.rs` (`BootInfo`, Limine requests, fw_cfg) |
| arch | `arch/{mod,stub}.rs`, `arch/x86_64/{mod,apic,desc,pic,trap,uart,vectors}.rs` | `arch/{mod,current}.rs`, `arch/x86_64/{mod,apic_init,catch,cpu,gdt,gs,idt,percpu,pic,switch,trampoline,uaccess}.rs`, `arch/x86_64/trampoline.S` |
| mm | `mm/{mod,pmm,paging,heap,kva}.rs` | `mm/{mod,pmm_init,paging_init,heap_init,kva_init}.rs` |
| time | `time/mod.rs` | `time/{mod,time_init}.rs` |
| acpi | `acpi/mod.rs` | `acpi/{mod,acpi_init}.rs` |
| irq | `irq/{mod,ipi}.rs` | `irq/{mod,irq_init,ipi_init,hardirq}.rs` |
| smp | `smp/{mod,per_cpu}.rs` | `smp/{mod,smp_init,per_cpu_init}.rs` |
| sched | `sched/{mod,thread,wait,work,fpu,stack_depth}.rs` | `sched/{mod,sched_init,work_init}.rs`, `sched/thread_init/{boot,mod,testing}.rs` |
| sync | `sync/{mod,lock}.rs` | `sync/{mod,sync_init,blocking_init}.rs` |
| log | `log/{mod,line,vmcoreinfo}.rs`, `log/trace/{mod,tests}.rs` | `log/{mod,log_init,panic,diag,ksyms,trace_init,vmcoreinfo_init}.rs`, `log/serial/{mod,raw}.rs` |
| console | `console/{mod,kbd,fb,font}.rs` | `console/{mod,console_init,kbd_init,fb_init}.rs` |
| shell | `shell/mod.rs` | `shell/{mod,shell_init,complete}.rs`, `shell/cmds/{mod,blk,dev,fs,sys}.rs` |
| dev | `dev/{mod,pci,dma,virtio,entropy}.rs` | `dev/{mod,dev_init,pci_init,dma_init,virtio_init,entropy_init}.rs` |
| drivers | `drivers/{mod,virtio_blk}.rs` | `drivers/mod.rs`, `drivers/virtio_blk_init/{mod,vq,issue,irq}.rs` |
| block | `block/{mod,part,cache,blockdev}.rs` | `block/{mod,block_init,blockdev_init,part_init,cache_init}.rs` |
| fs | `fs/{mod,inode,mount,walk,file,ramfs,testfs,tests}.rs`, `fs/kernfs/{mod,node,devfs,tmpfs,procfs,sysfs,tests}.rs`, `fs/vibefs/{mod,disk,layout,vol,ops,commit,mkfs,fsck,tests}.rs`, `fs/fat/{mod,vol,rw,dirent,chain,mkfs,tests}.rs` | `fs/{mod,fs_init,fat_init,vibefs_init,vibefs_crash,file_init}.rs` |
| proc | `proc/{mod,elf,pid,syscall,syscall_table,uaccess}.rs`, `proc/addr_space/{mod,tests}.rs` | `proc/{mod,addr_space_init,user_init,syscall_init,uaccess_init}.rs`, `proc/proc_init/{mod,fd,exec,exit}.rs` |
| ktest | `ktest/mod.rs` (selection by `vibeos.ktest=`, run counts, deadlines) | `ktest/{mod,user}.rs` (`kernel_tests` only) |

**In-guest tests.** A `kernel_tests` build's test bodies live beside the code they test: each kernel
subsystem directory holds a `ktest.rs` (`src/mm/ktest.rs`, `src/sched/ktest.rs`, and so on;
`src/arch/ktest.rs` for the x86_64 port), declared `#[cfg(feature = "kernel_tests")] pub mod ktest;` in
that directory's `mod.rs`, so no other build compiles it. Each subsystem's `ktest.rs` exports its
rows in run order as `pub(crate) const TESTS: &[Test]`, and `src/ktest/mod.rs` holds the runner, the
helpers that tests of more than one subsystem share, and the ordered list of those lists, `GROUPS`
([TESTING.md §8.2](TESTING.md#82-in-guest-tests)); `src/ktest/user.rs` builds the ring-3 programs tests
spawn. A new test goes into its subsystem's `ktest.rs`, and its row into that file's `TESTS`. Planned
(ROADMAP §10.2, Q2): the `kernel_tests` hooks still in production modules move into these files.

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
  the notice. The header sits within the file's first 40 lines, in one comment block (`//`, `#` or
  `;` lines, or one `/* */` block with ` * ` prefixes), and runs to the end of that block:

  ```
  <c> Provenance: <https repository URL> <path in that repository> @ <tag or 40-hex commit>
  <c> Upstream-License: <SPDX expression, as upstream states it>
  <c> <upstream copyright line(s) and permission notice, verbatim>
  ```

  `scripts/check_provenance.py`, in `make check`, fails when a source file outside `third_party/`
  has a copyright line or an SPDX line and no header, when a header lacks a field or its notice,
  and when `Upstream-License` has no option made only of the notice-only ids above.
  `check_provenance.py --fetch`, on the nightly job, fetches each recorded file at its pinned
  revision and fails unless its SPDX line names the recorded license or, where it has none, its
  first comment block holds that license's text. Code under Apache-2.0 alone, or under any license with a further condition, is never
  adapted into a vibeOS file: it enters as a crate under ROADMAP §10.9's `deny.toml` policy or as a
  port under §14.10, keeping its own license and NOTICE. That policy, which `make check` runs as
  `cargo deny check licenses bans sources`, admits crates from crates.io only, under the notice-only
  licenses above or Apache-2.0 or Unicode-3.0, and only when `deny.toml`'s `[bans]` allow list names
  the crate with the reason it is in the graph; another license needs an edit here first.
- Cryptographic primitives and the TLS state machine are depended on, never written in-tree: pinned,
  widely reviewed crates behind one facade, `vibeos-crypto`, under ROADMAP §10.9's `deny.toml`
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
  require. Every ISO carries `/LICENSES/LICENSE`, vibeOS's own, and
  `/LICENSES/THIRD-PARTY-NOTICES.txt`, which `scripts/mkiso.sh` generates with
  `scripts/gen_notices.py` (ROADMAP §10.9): Limine's license and the texts of the projects its
  `3RDPARTY.md` lists, kept in `third_party/limine/` with the release they came from; the license
  files of every crate in the normal dependency graph of each shipped binary, from the registry or
  `third_party/crates/`; Rust's `COPYRIGHT-library.html` notice for the standard library, with the
  license texts it names; and the notice of each file a provenance header marks as adapted. Host
  tests fail when an entry is missing or `third_party/limine/` names a release other than the one
  `setup.sh` pins. Planned (ROADMAP §10.9): the same file as an asset beside the image in each
  release. A copyleft binary also carries its source offer (ROADMAP §14.10).

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
