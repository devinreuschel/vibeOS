# vibeOS Roadmap Review

**Date:** 2026-09-22 · **HEAD reviewed:** `767c19f` (architecture review) · **Subject:** [docs/ROADMAP.md](../ROADMAP.md) · **Mode:** read-only

Companion to [ARCHITECTURE_REVIEW.md](ARCHITECTURE_REVIEW.md), which reviewed the code. This reviews the plan: whether it is clear, complete, in dependency order, and sized honestly, now that Era II is closed and the harder phases are next.

Line references and phase numbers are into `docs/ROADMAP.md` at the reviewed HEAD. The recommendations
were applied on 2026-09-22; §7 records what changed and the phase renumbering.

---

## 1. Summary

**Verdict: the roadmap is well made and should be kept. It needs one new phase, two splits, four dependency fixes, and a status-hygiene pass.**

What is right is structural and rare: every phase has a goal, an unlock, and an exit gate that a program can check; standing gates apply everywhere; non-goals are written down; ordering is explicitly by dependency. That discipline is why ten phases landed in six days without the tree rotting.

What is wrong is concentrated at the Era II to Era III boundary and in Era IV:

1. **Phases 10 and 11 cannot be tested with the userspace that exists.** Userspace is four hand-written nasm programs totalling 466 lines, the ELF loader refuses anything over 64 KiB, and there is no `brk`, `mmap`, or thread creation syscall. The Phase 11 gate literally requires `ls`, `grep`, and `wc`, which are Phase 12 deliverables. A small user runtime has to land before Phase 10's gate can be run. (§3.1)
2. **Four forward dependencies point the wrong way.** Phase 15 needs TLS from Phase 16, hardware from Phase 18, and perf counters from Phase 17. SMEP and SMAP sit in Phase 16 but belong before the first ring-3 instruction, which already happened. (§3.2)
3. **Phase 10 and Phase 18 are each two or three phases.** Phase 10 mixes what Phases 11 and 12 need (demand paging, copy on write, `mmap`) with what nothing needs until Phase 17 (slab, swap). Phase 18 bundles bare-metal x86, an AML interpreter, a USB stack, an architecture abstraction, aarch64, riscv64, and hardware CI. (§3.3, §3.4)
4. **The architecture review's findings are not in the roadmap.** The standing gate says correctness gaps become lines in this file. Thirty-six review items live only under `docs/reviews/`, and several of them are hard prerequisites for Phase 10 through 12: sixteen-entry process and descriptor tables, a VFS that real filesystems bypass, host tests that do not build on the maintainer's machine. A short consolidation phase at the era boundary would absorb them. (§3.5)
5. **Status hygiene.** One undefined checkbox state, three deferrals that are nine phases stale, a checked box that contains unfinished work, stretch goals mixed into gates, and two standing gates that nothing enforces. (§4)

None of this changes the destination. All of it changes what the next three agents should be told to do first.

---

## 2. Status, corrected

The question came in as "7 of 20 phases". The document has twenty-one phases, numbered 0 to 20.

| | |
|---|---|
| Phases with the exit gate closed | 10 (Phases 0 through 9) |
| Phases with every box ticked | 6 (1, 2, 3, 4, 8, 9) |
| Leftover open boxes in closed phases | 16 (Phase 0: 3, Phase 5: 2, Phase 6: 1, Phase 7: 10) |
| Boxes ticked / open overall | 467 / 553 |

Phase 7's ten open boxes are all NVMe (§7.3) and AHCI (§7.4). Nothing before Phase 18 depends on either, so Phase 7 is done for every purpose the roadmap cares about until then. §4.3 says where they should live.

Box counts per phase, for sizing:

| Phase | Done | Open | Phase | Done | Open |
|---|---|---|---|---|---|
| 0 Ignition | 47 | 3 | 11 IPC | 0 | 55 |
| 1 Memory | 42 | 0 | 12 Userspace | 0 | 55 |
| 2 Traps, ACPI, Time | 54 | 0 | 13 Networking | 0 | 67 |
| 3 Threads | 41 | 0 | 14 Graphics | 0 | 52 |
| 4 SMP | 73 | 0 | 15 Self-hosting | 0 | 34 |
| 5 Console | 37 | 2 | 16 Hardening | 0 | 52 |
| 6 Devices | 41 | 1 | 17 Performance | 0 | 53 |
| 7 Block | 30 | 10 | 18 Real hardware | 0 | 51 |
| 8 Filesystems | 45 | 0 | 19 Virtualization | 0 | 34 |
| 9 User mode | 57 | 0 | 20 Distribution | 0 | 28 |
| 10 Advanced memory | 0 | 56 | | | |

The box counts are roughly even across phases. The work behind them is not. A box in Phase 4 was "route a GSI"; a box in Phase 13 is "the full TCP state machine" and a box in Phase 15 is "then clang and LLVM". The second half of the roadmap is a different project wearing the same checklist format, which is fine as long as nobody reads "Phase 13 has 67 boxes, Phase 4 had 73" as a comparable estimate.

---

## 3. Findings, by impact

### 3.1 A user runtime is missing between Phase 9 and Phase 10

**Evidence.**

- Userspace is `user/hello.asm`, `user/init.asm`, `user/sh.asm`, `user/tests.asm`: 466 lines of nasm assembled with `nasm -f bin` and wrapped into an ELF by `scripts/mkuserelf.py` ([Makefile:92-105](../../Makefile#L92)). No libc, no allocator, no Rust.
- `src/user_init.rs:22` caps loadable binaries at 64 KiB (`MAX_ELF`). Any real program exceeds that.
- The syscall table ([docs/SYSCALL.md](../SYSCALL.md) §3) has 17 entries. There is no `brk`, `mmap`, `clone`, `nanosleep`, `stat`, or `getdents`.
- Phase 10's gate needs a user program that forks a 100 MB process (line 832), maps a file and calls `msync` (834), and allocates past physical memory (835).
- Phase 11's gate needs `ls | grep foo | wc -l` (915), a futex mutex under contention (919), and `poll` on 100 descriptors (918). `ls`, `grep`, `wc` are §12.4. A futex test in assembly is possible and nobody should do it.

**Why it matters.** The roadmap's own rule (line 18) is that a gate must be checkable by running something. As written, Phase 10 and 11 gates are checkable only by writing every test program in assembly under a 64 KiB cap, or by pulling half of Phase 12 forward silently. The agents will do one of those two things, and the second is worse because it happens undocumented.

**Recommendation.** Add a first subsection to Phase 10 (or the last to Phase 9), "User runtime", with its own gate:

- a Rust `no_std` userspace crate under `user/` as a workspace member, built for a user-mode x86_64 target, statically linked, with its own `_start`, stack argument parsing, and `panic` reporting through `write(2)`
- syscall stubs generated from one table shared with the kernel; §12.1 already asks for this, and it is cheaper before the table grows
- `brk` and `mmap`-backed allocator, which means `brk` and anonymous `mmap` land here, not in §10.4
- a `utest_ok` / `utest_fail` serial protocol that `tests/harness` recognizes, mirroring `ktest`, so user tests fail CI the same way kernel tests do
- the four assembly programs rewritten in the crate, plus `ls`, `cat`, `grep`, `wc`, `true`, `false`, `sleep`, and `yes`, which is exactly the set Phase 11's gate consumes
- `MAX_ELF` removed; the loader streams segments from the file instead of reading it whole

This also settles the first box of §12.1 (line 1010, "decide: write our own or port musl"). The answer falls out: our own runtime in Rust for everything vibeOS ships, and a musl port later for the C surface that Phase 15 needs anyway. Record that decision in §12.1 now instead of deferring it.

### 3.2 Dependencies that point forward

Line 11 says ordering is by dependency. These are the places where a phase needs something from a later phase.

| Where | Needs | Which is in | Fix |
|---|---|---|---|
| §15.3 line 1281, `cargo` "requires networking, TLS, and git" | TLS, a CSPRNG, hashes | §16.7 (lines 1364-1372) | Move entropy pool, CSPRNG, `getrandom`, SHA-2, AES-GCM and ChaCha20-Poly1305, Ed25519 and X25519, and TLS-in-userspace into Phase 13 as a new §13.11 "Crypto for the network". §12.6 (signed repositories) and §13.8 (`sshd`) need the same things. Leave secure boot, measured boot, TPM, and full-disk encryption in §16.7. |
| Phase 15 gate line 1258, "the test suite runs on vibeOS, on hardware" | A physical machine and hardware CI | §18.1, §18.8 | Phase 15's gate is "under QEMU with KVM". The on-hardware run belongs in Phase 18's gate, or in Phase 20's, where the self-hosted CI already lives. |
| §15.4 line 1289, "a profiler using the perf counters from phase 17" | PMU sampling | §17.2 | Move the line to §17.2, or mark it as not gating Phase 15. |
| §16.3 line 1333, SMEP; also SMAP, UMIP, `CR0.WP` | Nothing. These should have preceded §9.1. | Already late | Land now, before Phase 10 touches the page fault path. `grep -rn SMEP src` is empty; the Phase 9 gate "a user process faulting on a bad pointer does not take the kernel with it" is weaker than it reads while the kernel can still dereference user pointers outside the accessor unnoticed. The architecture review's S1 has the plan. |
| §14.4 line 1210, "USB HID once phase 18 lands xHCI" | xHCI | §18.3 | Already phrased as deferred. Fine. Listed for completeness. |
| Phase 19 gate, a Linux guest under a vibeOS hypervisor | Nested VMX in the test environment | Unstated | GitHub-hosted runners expose KVM but nested VMX is not guaranteed. Phase 19's gate implicitly depends on §18.8 hardware CI. Say so in the Phase 19 preamble. |

### 3.3 Phase 10 is two phases

Phase 10 has two chains that share nothing but a frame array.

**Chain A, needed by Phases 11 and 12:** §10.1 frame metadata, §10.2 demand paging, §10.3 copy on write, §10.4 `mmap` and `brk`, §10.6 unified page cache (the dynamic linker in §12.2 needs file-backed private mappings), and enough of §10.8 that a fork bomb kills a process instead of panicking the kernel.

**Chain B, needed by nothing before Phase 17:** §10.5 slab allocator, §10.7 swap, and the full watermark and reclaim machinery. The slab's own gate (line 837) is "the buddy lock is measurably less contended", which is a Phase 17 measurement. Swap's gate (line 835) is a capability nothing in Phases 11 through 15 exercises; a self-hosting build is given enough RAM, not swap.

**Recommendation.** Make Phase 10 chain A and rename it "Fault-driven memory". Move §10.5 to §17.5 alongside RCU and per-CPU counters, where the architecture review's P2 note already wants the buddy hotspot recorded. Move §10.7 and the full §10.8 reclaim to a new phase after 15, or to §17.8. If renumbering is unwelcome, the minimum is a sentence in the Phase 10 preamble stating that §10.5 and §10.7 do not gate Phase 11, so an agent can close the gate without them.

### 3.4 Phase 18 is three projects

§18.1 through §18.4 plus §18.8 are "boot on real x86 hardware": firmware quirks, an AML interpreter (which line 1479 correctly calls "a large and genuinely unpleasant subproject"), USB from xHCI through HID and mass storage, real NIC and audio drivers, and a netbooting CI machine. §18.5 through §18.7 are "make the kernel portable": an architecture abstraction, aarch64, riscv64. They share almost no code and no test environment.

**Recommendation.** Split into "18: Real hardware" (18.1-18.4, 18.8) and "19: Portability" (18.5-18.7), renumbering 19 and 20 to 20 and 21. The AML interpreter alone deserves a subsection with its own gate (a real DSDT from a named machine parses and `_PRT` resolves interrupt routing), since it is where a naive agent will spend a month.

One consideration, not a recommendation. The dev host is an Apple Silicon Mac, so every local run is under TCG. The changelog already carries three TCG-only flakes fixed in one week (`now_us` going backwards under `hlt`, `sleep_ms_50` and `tsc_calib_source` under `-smp 4`, and a `wait4` hang under the periodic LAPIC). An aarch64 port would make local runs native under HVF. That is a real argument for doing §18.5 and §18.6 earlier than their number suggests. It is also a very large detour. It should be a deliberate choice, recorded either way.

### 3.5 The architecture review is not in the roadmap

Line 30: "no `TODO` describing a correctness gap. Those become lines in this file." The code has zero `TODO`s, which is honoured. The gaps instead live as constants and as 36 review items under [issues/](issues/README.md), none of which the roadmap references.

The ones that block the next three phases:

| Review item | Why it blocks | Roadmap line that assumes it |
|---|---|---|
| D1 heap-sized tables | `MAX_PROCS = 16`, `MAX_FDS = 16` (`src/proc.rs:8-9`), `MAX_THREADS = 64` (`src/thread.rs:12`), `MAX_REGIONS = 32` (`src/addr_space.rs:14`), `MAX_OPEN = 16`. A shell pipeline of three processes plus a test runner plus init is already a third of the process table. | 915 (pipelines), 918 (100 descriptors), 832 (100 MB process with more than 32 regions) |
| A3 VFS single dispatch | Per-process descriptor tables must work over every mount. Today FAT and vibefs bypass `InodeOps`. | §11.1 named pipes through the VFS, §11.3 `shm_open` on tmpfs, §10.6 one cache per inode |
| E2 one errno-shaped error | Every path that can reach a syscall needs a Linux errno. Fourteen unrelated error enums today. | §9.3 (already ticked), every Phase 11 syscall |
| S1 SMEP, SMAP, UMIP, entropy | See §3.2. `/dev/random` is xorshift. | Phase 9 gate, §16.2 KASLR, `AT_RANDOM` |
| A2, B2 portable host tests | `make test-unit` fails on the maintainer's machine. The cheapest tier is unavailable where bugs get reported from. | Standing gate line 23 |
| Q1, C1 lint gates and pinning | See §4.6. | Standing gate line 22 |

**Recommendation.** Add a **Phase 9.5, "Consolidation"**, between the eras. The document's own framing invites it: Era II is "known answers", Era III is "where it stops being clear", and the boundary is the right place to pay down what Phase 0 laid down in a hurry. Its exit gate should be as checkable as any other:

- `make check` (fmt, clippy with warnings denied, host unit, harness unit) passes on macOS and Linux and gates CI ahead of the QEMU ladder
- the nightly is pinned by date; Limine and the GitHub actions are pinned by hash
- the growable tables are heap-sized at init from one `limits` module, and the gate states the limits Phase 10 is tested against
- FAT and vibefs implement `InodeOps`; the `Back` enum in `src/file_init.rs` is gone
- one errno-shaped kernel error type; `docs/SYSCALL.md` §2 generated from it
- SMEP, SMAP, UMIP, and `CR0.WP` set on every CPU with an in-guest test; `/dev/random` fed by virtio-rng or `RDRAND`
- `BootInfo` landed (line 116, deferred since Phase 0)
- `AGENTS.md` and an MIT `LICENSE` in the tree; README status current
- the review directory's index marks each item done or explicitly declined

Also write down the **slice convention** in "How to read this". Every phase since Phase 1 landed as two to four slices (A, B, C, D), each its own PR with its own in-guest tests. The agents invented it and it works. The document should describe it so the next agent does not invent a different one.

### 3.6 Items no phase provides

Things a later line assumes and no earlier line creates.

- **User threads.** §11.4 futex, §11.7 line 976 "per-thread signal masks", and §12.1 line 1017 "pthreads over kernel threads" all assume a process can have more than one thread. No line creates one: no `clone` with `CLONE_VM | CLONE_THREAD`, no `exit_group`, no `arch_prctl(ARCH_SET_FS)` or `set_tid_address`, no per-thread user TLS. §9.5 says "a process is one or more threads" and the code has only the one. Add a §11.0 "Threads" with those four syscalls and a gate: N user threads increment a counter under a futex mutex, correct count, `exit_group` tears all of them down.
- **`brk`.** Every allocator wants it. Only `mmap` appears (§10.4). Add it, and put it in the user-runtime subsection from §3.1.
- **The `stat` family, `getdents64`, `readlink`, `pread`, `pwrite`, `readv`, `writev`, `fstatat`, `openat`.** §11.8 lists `getcwd`, `chdir`, `access`, `chmod`, and the rest, but not the calls every coreutil makes first. Add them to §11.8.
- **procfs wired to real processes.** Line 717 ticks "per-process directories" but the changelog for Phase 8C says it is a pid-1 stub, and `ps` uses a vibeOS-only syscall 500 "until procfs". Add "procfs backed by the process table; retire syscall 500" to §11.8.
- **SNTP.** DESIGN §6.6 promises "NTP over the network eventually". §13.8 never lists it; §12.4 `date` wants a correct clock. Add an SNTP client to §13.8.
- **Login.** §16.6 enforces uids and §20.2 creates users, but no line adds `login`, a getty, `passwd`, or `su`. Add to §12.3 or §16.6.
- **Loadable kernel modules.** Line 1371 verifies signatures "on module loading" and line 1426 keeps "the module list" under RCU. No phase builds modules, and the "monolithic" non-goal does not settle it, since a monolithic kernel can load modules. Decide. Either add them (probably §18.1, where real hardware makes out-of-tree drivers plausible) or strike both lines and add "no loadable modules" to the non-goals.
- **x2APIC.** DESIGN §5.9 and §7.10 list it as later. The roadmap never mentions it. Some real machines and most large VMs expose only x2APIC. Add to §18.1.
- **CPU offlining.** DESIGN §7.10 lists it for power management. The roadmap's non-goals say "CPU hotplug". Offlining a core is not hotplug, but the two documents should agree. Add to §17.6 or delete from DESIGN.
- **A KVM leg in CI.** The harness defaults to TCG because "KVM on a loaded host makes PIT/sleep tests flake" ([Makefile:35](../../Makefile#L35)). Three TCG-only bugs were fixed in one week. Both accelerators find bugs the other hides, and GitHub's Linux runners have KVM. Add a KVM run of `make test-kernel` to §4.11 or to the consolidation phase.
- **A kernel debugging workflow.** QEMU `-s -S`, `gdb` with the kernel ELF, a `make debug` target, and a paragraph in DESIGN §8.4. Cheap, and the agents debugging Phase 10 page faults will want it. Put it in the consolidation phase.

---

## 4. Clarity and status hygiene

Cheap, and all of it in one commit.

### 4.1 An undefined checkbox state

Line 117 uses `[~]`. "How to read this" defines `[ ]` and `[x]` only. Define it as "partially landed; the trailing note says what is missing", or resolve the line.

### 4.2 Stale deferral notes

- Line 123, "polled RX on the data-ready bit (input arrives in phase 5)". §5.3 shipped serial RX and is ticked. Tick this one.
- Line 150, clippy and fmt gates "(deferred: clippy/fmt gates land with phase 1)". Phase 1 closed eight phases ago. See §4.6.
- Line 116, `BootInfo` "(deferred: ...)". Review item D3 has a one-day plan.

Suggested rule for the preamble: a deferral names the phase that will land it, and that phase's gate cannot close while the item is open. Lines 503 and 536 ("parked, Design ACK") already follow a better convention; make it the only one.

### 4.3 Boxes that live in the wrong phase

- §7.3 NVMe and §7.4 AHCI (lines 641-653, ten boxes). Nothing before Phase 18 needs them and §18.4 already says "AHCI and NVMe validated on real drives". Move both subsections to Phase 18. If the QEMU NVMe depth test is wanted earlier, keep §7.3 and move only §7.4.
- Line 600, packed virtqueue "as a later optimization". Move to §17.8.

### 4.4 A ticked box containing unfinished work

Line 776 is `[x]` and ends "`stat`/`fstat`/`nanosleep`/`brk`/`mmap`/`munmap` still later". Split it: one ticked line for what landed, one open line pointing at §10.4 and §11.8.

### 4.5 Stretch goals mixed into gates

Lines 600, 1151 (`sshd` "eventually"), 1196 (virtio-gpu 3D "as a much later stretch"), 1224 (Wayland "as a stretch"), 1337 (CET "as a stretch"), and 1521 (riscv64 "a stretch") are checkboxes that are not expected to be ticked when the phase closes. That makes "phase complete" non-binary. Give each phase a "Stretch" subsection that the exit gate excludes, or collect them in a "Beyond" section after Phase 20.

### 4.6 Standing gates that nothing enforces

- Line 22, "`make` builds clean with warnings denied". No `-Dwarnings`, `clippy`, or `fmt --check` exists in the Makefile, `.cargo/`, or CI. Either land review item Q1 (about a day) or stop claiming it.
- Line 23, "`make test` green, all tiers". `make test-unit` cannot run on the maintainer's macOS host (review A2). The gate is true only on Linux, which the document does not say.

A standing gate that is not enforced teaches agents that gates are aspirational. That is the one thing this document cannot afford.

### 4.7 Gates that are not measurable

The preamble's rule is line 18. These break it:

| Line | Wording | Suggested shape |
|---|---|---|
| 1083 | "reasonable throughput" (TCP client) | a number in MB/s under KVM with virtio-net, recorded in the gate |
| 1182 | "steady frame rate" (compositor) | a target frame rate at a stated resolution and window count, with a maximum dropped-frame fraction |
| 1389 | "within a stated bound" (scheduler latency) | state the bound |
| 1391 | "the top contended lock addressed" | before and after contention numbers, and a threshold |
| 1540 | "reasonable throughput" (guest virtio) | a fraction of the host's native number |
| 1618 | "an honest known-issues list" | the list exists and every open roadmap box in the release's phases is in it |

The numbers are the maintainer's to pick. The point is that the gate names one, so an agent cannot close it by assertion.

### 4.8 Small consistency items

- Phases 0 through 15 have an **Unlocks** line. Phases 16 through 20 do not.
- §17.4 line 1422 "cgroup-style group scheduling" and §19.5 line 1577 "cgroup-equivalent: CPU, memory, and I/O limits" are the same feature. Keep one and cross-reference.
- §10.6 line 887 and §8.4 line 716 both describe tmpfs on the page cache. §8.4 is ticked but the changelog says tmpfs sits on the Phase 7 block cache plus a fixed ramdisk. That is fine as long as §10.6 says it re-does §8.4's version rather than adding to it.
- "How to read this" says "Twenty one phases". The README says the same. The arc table numbers them 0 to 20. All consistent; noted only because the question arrived as "7 of 20".

---

## 5. A proposed shape

If every recommendation above is accepted, the arc becomes:

| Era | Phase | Change |
|---|---|---|
| II | 9.5 Consolidation | new (§3.5) |
| II | 10 Fault-driven memory | §10.1-10.4, §10.6, minimal OOM, plus the user runtime (§3.1); slab and swap out |
| II | 11 Threads, IPC, signals, TTY, POSIX floor | plus §11.0 threads, `brk`, the `stat` family, real procfs (§3.6) |
| III | 12 Userspace | libc decision recorded (§3.1); login (§3.6) |
| III | 13 Networking | plus §13.11 crypto and TLS, SNTP (§3.2, §3.6) |
| III | 14 Graphics ∥ 15 Self-hosting | independent tracks; say so. Phase 15's gate under QEMU with KVM (§3.2) |
| IV | 16 Hardening | minus the crypto that moved to 13 |
| IV | 17 Performance | plus slab, swap or a reclaim phase, packed virtqueue, the review's P2 hotspots |
| IV | 18 Real hardware (x86_64) | plus NVMe and AHCI from Phase 7, x2APIC, the modules decision |
| IV | 19 Portability | §18.5-18.7 split out (§3.4) |
| IV | 20 Virtualization | nested-VMX test environment dependency stated |
| IV | 21 Distribution | unchanged |

Phase 14 before 15 in the numbering is harmless, but a reader assumes self-hosting waits for a compositor. A sentence in the Era III preamble that 13, 14, and 15 fan out from 12 would fix that without renumbering.

---

## 6. What to do first

In order, each one a small PR:

1. The status-hygiene commit (§4): define `[~]`, tick line 123, split line 776, move §7.3 and §7.4, add the Stretch convention, add Unlocks to Phases 16 through 20, dedupe cgroups.
2. Add Phase 9.5 with the gate in §3.5 and cross-reference the review issue documents from it.
3. Add the user-runtime subsection (§3.1), `brk`, and §11.0 threads, and split Phase 10 (§3.3).
4. Move crypto into Phase 13 and fix Phase 15's gate (§3.2).
5. Split Phase 18 (§3.4) and decide loadable modules and x2APIC (§3.6).
6. Put numbers on the six gates in §4.7.

Items 1, 2, and 3 are the ones that change what the next agent builds. The rest can wait for the phase that reaches them.

---

## 7. Applied (2026-09-22)

The maintainer accepted the recommendations and asked for both architectures to be first class. The
roadmap now has 23 phases. Old numbers in this document map as follows.

| Old | New | Change |
|---|---|---|
| | 10 Consolidation | new: the review items as boxes, the architecture seam, the user runtime |
| | 11 Portability | new: aarch64 to Phase 9 parity, first class; riscv64 as a stretch |
| 10 Advanced memory | 12 Fault-driven memory | slab to 19.9, swap to a stretch subsection, `brk` added |
| 11 IPC | 13 Threads, IPC, signals, POSIX | §13.1 threads added; `stat` family, `getdents`, real procfs in §13.9 |
| 12 Userspace | 14 Userspace | libc decision recorded; login added |
| 13 Networking | 15 Networking | §15.11 crypto and TLS moved in from hardening; SNTP; throughput numbers |
| 14 Graphics | 16 Graphics | frame-rate numbers; 3D and Wayland to Beyond |
| 15 Self-hosting | 17 Self-hosting | gate under QEMU/KVM; both architectures; virtio-fs |
| 16 Hardening | 18 Hardening | crypto out; aarch64 equivalents in; Unlocks |
| 17 Performance | 19 Performance | slab, offlining, packed virtqueue in; latency and contention numbers |
| 18 Real hardware and portability | 20 Real hardware | portability out; NVMe, AHCI, x2APIC, an aarch64 board in |
| 19 Virtualization | 21 Virtualization | nested-virt dependency stated; aarch64 EL2 |
| 20 Distribution | 22 Distribution | both architectures; known-issues list generated |
| | Beyond | stretch items collected, plus new hard things |

Also applied: the `[~]` state removed, stale deferrals resolved, line 776 split, the slice and stretch
conventions written down, the `unsafe` documentation standing gate, loadable modules made a non-goal,
and the cross-references in `DESIGN.md`, `README.md`, `CHANGELOG.md`, `SYSCALL.md`, source comments, and
the open review issue documents updated to the new numbers.

Decisions taken in the edit that the maintainer may want to revisit: the placement of Portability at
Phase 11 rather than after Phase 13; the table limits in the Phase 10 gate; the throughput, frame-rate,
and latency numbers in the Phase 15, 16, and 19 gates; and the musl-for-C decision recorded in §14.1.

*Review written read-only; the edits in §7 were applied afterwards. Nothing is committed.*
