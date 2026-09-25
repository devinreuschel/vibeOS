# vibeOS Kernel Review: Correctness, Safety and Privilege Boundary

**Date:** 2026-09-23 · **HEAD reviewed:** `e929a7c` (D3 follow-up, #91) · **Mode:** read-only review of the code; the build, the full test ladder and host-side reproductions were run.

This review follows [ARCHITECTURE_REVIEW.md](ARCHITECTURE_REVIEW.md), which covered structure, build and process. This one covers whether the kernel is correct and safe at the privilege boundary, under SMP, against devices, and on disk. Where the two overlap, this document is the more recent reading of the code.

**Tracking.** This file is a snapshot of `e929a7c` and is not edited as work lands.
- **Where each finding lives.** Every finding id (`Fnnn`) is cited by at least one line of [ROADMAP.md](../ROADMAP.md): a box in Phase 10, or in the later phase that first needs the fix, for live defects, or a box in the phase a LATENT finding's milestone names.
- **Enforcement.** `scripts/check_review_refs.py` in `make check` fails on an id no ROADMAP box cites, and on one whose boxes sit later than ROADMAP's placement allows (§10.9): a CRITICAL or HIGH finding without a LATENT tag in Phase 10, a LATENT one no later than its milestone's phase. Its `--closed` mode fails while a CRITICAL or HIGH finding without a LATENT tag is cited by an open box in Phases 0 to 10.
- **Corrections.** A finding's `####` heading and `**Severity:**` line are never edited. A correction (a severity, a LATENT tag, a location) is a dated entry under a `## Errata` heading at the end of this file, naming the finding and what changed, with a `Gate-change:` trailer on its commit; ROADMAP §10.9 has `scripts/check_review_refs.py` read an erratum before the finding's own line.
- **Where the rules now live.** The guardrails in §8 are in [AGENTS.md](../../AGENTS.md). The invariants in §3.9 are in DESIGN §2.

---

## 1. Verdict

This is not yet a foundation to build Phases 12–13 on, but it does not need to be thrown away either.

**What is sound.** The pieces that one session can hold in its head are mostly sound, and many are well tested on the host: the allocators, the parsers, the APIC/IOAPIC programming, the marker contract.

**Where it is broken.** The failures sit at the seams, where two sessions' assumptions meet:
- the ring-3/ring-0 boundary;
- cross-CPU object lifetimes;
- the storage durability contract;
- a test system that has been tuned, one retry rule at a time, to report green.

**Concrete consequences today:**
- **Ring 3 can halt the whole kernel.** A user process can do it with a single-step trap, with an `lseek`+`write` on `/vibe`, or by exhausting kernel resources. It also happens if someone presses a key in the QEMU window while any program is computing.
- **The syscall exit path runs with interrupts enabled** after a console read. On KVM or real hardware, an interrupt landing there executes at CPL0 on a stack pointer that user code chose.
- **Block-I/O completion can write into a freed stack frame.**
- **Attaching a whole-disk image and booting destroys it.**
- **vibefs silently stops committing** after 57 generations, while its crash-consistency test keeps passing.

**What must go before building further:**
- the harness rules that retry kernel panics and timeouts;
- the production GPT auto-stamp;
- the legacy "bound `run_user`" execution model, with its system-wide `CURRENT_AS`/`USER_JMP`/`STDOUT` globals;
- one of the two file stacks: the unreachable VFS, or the route-table File API.

**What to rewrite rather than patch.** Rewrite the entry/exit path as one mechanism; do not patch it a seventh time.

**The outlook.** Fix the five items in §6 during Phase 10 and this becomes a reasonable base. Skip them, and Phase 12 (COW) and Phase 13 (threads) will turn several of today's LATENT findings into cross-process memory corruption.

---

## 2. Scope and assumptions

**Context.** The brief left these blank, so they were inferred from the code and docs.

| | Value |
|---|---|
| Architecture | x86_64 only. aarch64 is ROADMAP Phase 11 (Portability). |
| Language / toolchain | Rust `nightly-2026-09-22` (`rust-toolchain.toml`), edition 2024, built-in `x86_64-unknown-none` (kernel code model, no red zone, soft-float). Unstable features: `abi_x86_interrupt`, `alloc_error_handler`. |
| Kernel design | Monolithic |
| Boot path | Limine v9.6.7 (binary branch, commit-pinned), base revision 3, hybrid BIOS/UEFI ISO |
| Stage | Boots in QEMU TCG with SMP (tested at `-smp 2` and `-smp 4`). Ring-3 userspace with `fork`/`execve`/`wait4`, 17 syscalls, FAT32 initrd (writable, in RAM), in-memory vibefs at `/vibe`, PCI + MSI-X, virtio-rng and virtio-blk, block cache, partitions. Never run on real hardware. |
| Roadmap next | Phase 10 Consolidation (in progress). Then 11 Portability (aarch64), 12 Fault-driven Memory (demand paging, COW), 13 Threads/IPC/Signals/POSIX, 14 Userspace, 15 Networking … 18 Hardening, 20 Real Hardware. |
| Security goals | Hobby/research ("toy kernel", README). No permission model: every process is root. Treated as: ring 3 must not be able to corrupt or halt the kernel, and devices and disks are untrusted at the milestones that introduce them. |
| Agent instructions / design docs | `AGENTS.md` (via `CLAUDE.md`), `docs/DESIGN.md`, `docs/SYSCALL.md`, `docs/VIBEFS.md`, `docs/ROADMAP.md`, plus the earlier `docs/reviews/ARCHITECTURE_REVIEW.md` |

**What was reviewed.** HEAD `e929a7c` on branch `claude/ai-kernel-review-e5a10d`:
- about 53k lines of Rust in `src/` (4.65k of them in `src/ktest.rs`)
- the `user/*.asm` programs
- the Python harness
- the Makefile, `build.rs`, the linker script and the CI workflows

There are 651 `unsafe {}` blocks in `src/`, and exactly one `// SAFETY:` comment.

**How.**
1. **Mapping.** Eight parallel readers mapped the subsystems: mechanisms, invariants, trust boundaries and leads.
2. **Bug hunting.** Eleven area reviewers turned the leads into 159 raw findings, merged to 119 after de-duplication.
3. **Round 2.**
   - A second filesystem pass: 27 findings.
   - A completeness critic that picked four uncovered areas (speculation posture, process lifecycle, FPU state across fork/exec, build reproducibility), and gap reviewers for them: 13 findings.
4. **Adversarial verification.** Every finding was checked by reviewers told to refute it:
   - CRITICAL/HIGH: three independent verifiers (correctness, reachability, spec/severity).
   - MEDIUM: one verifier, escalated to three if it argued for HIGH.
   - LOW: verified in batches.

   A finding was dropped when a majority refuted it; two were. Severity is the verifiers' median. Every entry below has the verifiers' corrections applied: line numbers, mechanisms, and reachability.
5. **Final set.** 152 findings after cross-round merging.
6. **Spec checks.** Verifiers checked hardware and spec claims against primary sources. These include the Intel SDM, the virtio 1.2 spec, ACPI 6.5, PCI Local Bus 2.3 / PCI Firmware 3.2, and the Limine PROTOCOL.md. Where no citation is given, the entry says Suspected.

**What was built and run** (macOS 26, Apple Silicon host, QEMU TCG):
- `./setup.sh`: ok (pinned Limine verified).
- `make check`: **green**. ruff and mypy are not installed, and the gate skips them with only an echo.
- `make` from clean: builds in about 7 s.
- `make test`: **red** on this supported dev host.
  - `test-e2e-uefi` claims to skip when OVMF is missing, but runs anyway and fails. With Homebrew's `edk2-x86_64-code.fd`, the file AGENTS.md points to, it still fails: QEMU refuses it as `-bios`, and it only boots as pflash.
- `make -k test` (all remaining tiers):
  - BIOS e2e, panic, #GP, PIT and 9 GiB e2e all pass.
  - `test-kernel` (`-smp 2`) **fails**: `ktest: FAIL reap_many_via_idle: reap did not restore frames`. This is not on the retry list.
  - `test-kernel-smp4`: 120/120.
  - `test-lapic-fallback` **hung for 90 s** after `ktest: ok syscall_dispatch`. The harness retried, and the second boot passed.
  - `test-vibefs-crash` passes 8/8, but every round reports `wr=0`, and from round 2 on fsck shows the same `gen 57 errors 0 warnings 58`. Root cause: vibefs leaks a block per commit. The 64-block volume fills at generation 57, and every later commit fails with an error the test ignores. The crash test has been passing without exercising a single commit.
- Host-side reproductions: reviewers ran unmodified copies of `src/block.rs`, `src/cache.rs` and the vibefs commit path in scratch crates. That confirmed the block-queue ordering bugs, the duplicate-cache-slot bug and the vibefs leak.

**What was not run.**
- **Malformed syscall arguments in the guest.** I did not boot a hostile ring-3 program. Every "reachable from userspace" claim below comes from tracing the code from the syscall or exception entry, and each is marked Confirmed or Likely on that basis.
- **Malformed disk images in the guest.**
- **Real hardware or KVM.** Everything was TCG, which never produces the TSC-deadline timer path and delivers interrupts only at translation-block boundaries. Several races are therefore narrower under TCG than under KVM.
- **Miri and sanitizers.** The `miri` component is not installed, and ASan was not attempted.
- **The release profile.** It was not built.
- **aarch64.** Not applicable yet.

---

## 3. Architecture summary

### 3.1 Boot sequence and init ordering (`src/main.rs:97-289`)

1. **Initial state.** Limine enters `_start` in long mode with IF=0, on Limine's 64 KiB bootloader-reclaimable stack. That stack has no guard page, and the bootstrap thread keeps running on it forever. At this point the CPU has Limine's GDT and CR3, and **no usable IDT**.
2. **Serial.** COM1 comes up and the base revision is checked.
3. **`boot::capture`.** Takes the HHDM (asserted to equal `0xFFFF_8000_0000_0000`), the memory map, the executable address, the RSDP and the framebuffers, into a `BootCell<BootInfo>`.
4. **Buddy PMM.** Built from USABLE ranges below the 8 GiB physmap cap, excluding frame 0, the kernel, `0x8000` and the framebuffers.
5. **Our own PML4, then `mov cr3`:**
   - kernel sections with per-section permissions;
   - the physmap `[0, map_end)`: RW, NX, GLOBAL, 2 MiB pages;
   - a **permanent low identity map of 0–512 MiB**: GLOBAL, supervisor, with the first 2 MiB RWX.
6. **ACPI.** Parsed from firmware memory, and the LAPIC/IOAPIC/HPET pages are patched to UC. This still happens before any IDT is installed.
7. **Heap and KVA.** Guarded kernel stacks become available.
8. **GDT/TSS/IST, then IDT.** The GDT/TSS/IST are loaded (IST stacks from KVA) and the PIC is remapped and masked. The IDT gets 256 DPL0 interrupt gates, with IST for #DF, NMI, #MC and #DB. CR4.SMEP, SMAP and UMIP are set when CPUID offers them, and CR0.WP is set.
9. **Per-CPU and syscall setup.** The per-CPU area is set up via GS_BASE and KERNEL_GS_BASE, and the bootstrap TCB is created. The syscall MSRs are programmed (STAR, LSTAR, FMASK=`0x47700`, EFER.SCE) and the FXSAVE template is captured.
10. **Timekeeping and interrupts.** HPET or PIT calibration of the TSC, then LAPIC and IOAPIC. The **first `sti`** comes here, after which the LAPIC timer is proved (TSC-deadline, then periodic, then PIT).
11. **Scheduler and SMP.** Idle threads are created and the IPIs installed. APs are started one at a time through the `0x8000` trampoline (INIT, SIPI, SIPI).
12. **Devices.** Console (framebuffer and PS/2), PCI scan, workqueue, virtio-rng and virtio-blk binding, and entropy.
13. **Block layer.** Ramdisk, cache, partition scan. The scan **stamps a GPT onto any `vda` without a parsable table**.
14. **Filesystems:**
   - the FAT initrd copied into BSS as `/`;
   - kernfs (devfs, procfs, tmpfs, sysfs) mounted in the VFS;
   - vibefs, built in memory, at `/vibe`.
15. **Userspace.** `boot_hello` runs `/hello` bound to the bootstrap thread. Then `/sbin/init` becomes pid 1, pinned to the BSP, and the bootstrap thread parks.

### 3.2 Address space

| PML4 | Range | Contents | Leaf flags |
|---|---|---|---|
| 0 | `0–512 MiB` | Identity map. **Kernel CR3 only.** Never torn down. | GLOBAL, supervisor. First 2 MiB RWX; the rest RW NX. |
| 256+ | `0xFFFF_8000_…` | Physmap (HHDM) `[0, min(max(usable, fb, kernel end), 8 GiB))`. `map_gap` can add more later, with no cap. | RW, NX, GLOBAL, WB. MMIO leaves are patched to UC in whole 2 MiB units. |
| 384 | `0xFFFF_C000_…` | Heap, 64 MiB window | RW, NX, GLOBAL |
| 416 | `0xFFFF_D000_…` | KVA: guarded stacks and vmap | RW, NX, GLOBAL. Guard page unmapped. |
| 448 | `0xFFFF_E000_…` | ioremap. A 256 MiB bump window that is never freed, created lazily. | UC |
| 511 | `0xFFFF_FFFF_8000_…` | Kernel image | `.text` RX, `.rodata` R NX, `.data`/`.bss` RW NX. **The physmap also maps the image RW.** |
| user | `[0x1000, 0x8000_0000_0000)` | A fresh PML4 per process. PML4[256..512) is copied **once** from the kernel root at creation; PML4[0] is not copied. | P\|U (+W, +NX unless executable), non-global. Interior tables are always P\|W\|U, kernel half included. |

There is no KPTI and no PCID. User memory is never dereferenced directly. Every copy goes `check_user_range` (canonical, below `USER_END`, above the null page, present with U at every leaf), then the translated frame is accessed through the HHDM. SMAP therefore protects nothing on the copy path, and `stac`/`clac` are dead code.

### 3.3 Allocators

- **Buddy** (`pmm.rs`). Intrusive doubly-linked free lists live inside the free frames, reached through the HHDM. `MAX_ORDER` 10. The double-free check and the coalescing are O(list length). Guarded by a global IRQ-off `SpinMutex` at rank 2.
- **Heap** (`heap.rs`). Address-ordered first fit. It grows page by page, taking PT and then BUDDY, up to 64 MiB. OOM panics.
- **KVA** (`kva.rs`). A 128-node free list. Guarded stacks are 4 pages plus a guard. Dead stacks go on a **global 8-slot DEFERRED list** that any CPU drains, and it panics when full.
- **DMA** (`dma_init.rs`). Buddy `allocate_constrained`. The device address is the physical address; there is no IOMMU.

### 3.4 Interrupt and exception flow

- **Gates.** All gates are `extern "x86-interrupt"` functions in `src/arch/idt.rs`, except the device pool stubs (`irq_init.rs`) and the keyboard ISRs (`kbd_init.rs`), which are installed at runtime.
- **Handler entry.** The idt.rs handlers do `swapgs` only when the interrupted CS.RPL is 3. Every exception handler calls the test hook `catch::intercept` first, including in production.
- **Ring-3 faults.** `try_user_fault` kills the process for #DE/#MF/#XM (SIGFPE), #UD (SIGILL), #NP/#SS (SIGBUS) and #GP/#PF (SIGSEGV). A user `int3` hits a DPL0 gate, so it becomes #GP and SIGSEGV, not the "log, continue" DESIGN §5.2 promises. Every other vector raised from ring 3 halts the whole kernel, #DB and #AC included.
- **Device IRQs.** They come from a pool of vectors 0x30–0x7F. `dispatch` runs the top half, wakes a threaded bottom half pinned to the last CPU, then EOIs.
- **Timers and IPIs.** The timer ISRs EOI, re-arm, then call the scheduler tick. The IPIs are 0xFB call-function, 0xFC shootdown, 0xFD reschedule and 0xFE halt. Initiators of the first two spin with IF off and panic after 1 s without acks.

### 3.5 Scheduler and context switch

- **Thread table.** A global table of 64 `Box<Tcb>` slots under `SCHED` (a ranked SpinMutex). TCBs are never freed; dead slots are rewritten in place.
- **Run queues.** Each CPU has a FIFO run queue, touched only by its owner with IF off. Cross-CPU wakes go through an atomic inbox plus a 0xFD IPI. There is no migration, and user threads are pinned to the CPU that spawned them, in practice the BSP.
- **What `switch_now` saves and restores.** Callee-saved GPRs, RFLAGS (with the delayed-`sti` IF policy) and FXSAVE, plus TSS.RSP0/`kernel_rsp0` and CR3. It **does not** save or restore FS_BASE. The one global `USER_JMP`/`CURRENT_AS` set belongs to a second, "bound" user-execution model that is still in production code.
- **Timeouts.** A single global sorted list.
- **Thread exit.** `thread_exit` marks the thread Dead without taking SCHED, defers its own stack, and schedules away.

### 3.6 Syscall entry and exit (`src/syscall_init.rs:37-208`)

**Entry:**
1. `syscall`, then `swapgs`.
2. Save the user RSP to per-CPU scratch and load `kernel_rsp0`.
3. Push a 128-byte frame.
4. `fxsave` into the current TCB.
5. Call `proc_init::syscall`, with IF=0 because FMASK clears it.

**Dispatch.** A `match` on the Linux syscall number. `syscall.rs`'s metadata table is not used.

**Exit:**
1. `fxrstor`.
2. If RCX is canonical and neither RF nor VM is set: restore the GPRs, `mov rsp, user_rsp`, `swapgs`, `sysretq`.
3. Otherwise: an `iretq` frame built from per-CPU scratch, then `swapgs`, `iretq`.

**First entry to ring 3.** `enter_user` and `enter_user_full` get there with `iretq`, after loading the user data selector into GS and writing GS_BASE=0.

### 3.7 Driver model

- **Discovery and binding.** PCI enumeration (CF8 for bus 0, ECAM from MCFG otherwise) fills a 64-slot registry. Drivers register and bind by ID, and `probe` runs on a copy of the device record.
- **Before probe.** MEM and MASTER are enabled before `probe` for every device.
- **Interrupts.** MSI-X where the device supports it, INTx through the IOAPIC otherwise.
- **Virtio.** Modern devices only. The split virtqueue keeps its free list inside device-visible descriptor memory.
- **virtio-blk.** Uses 8 KiB bounce slots, one queue per CPU (capped at 8), completion on the threaded bottom half, and stack `IoWaiter` cookies.
- **Block stack.** Block requests go through a merging C-LOOK queue with fence sequence numbers. On top sits a 16-page write-back cache with a `blk-wb` thread.

### 3.8 Where untrusted data enters

| Source | Entry points |
|---|---|
| Userspace | Syscall registers. Buffers for `read`/`write`/`psinfo`. C strings for `open`/`execve`. argv. The `wait4` status pointer. fd/flags/whence/offset/signal numbers. **ELF files from any writable FS**, which reach `elf::parse` and `user_init::map_loads` through `execve`. User RFLAGS (TF, AC, IOPL is ignored). Exception frames from CPL3. Console output bytes going to the framebuffer. |
| Devices | Virtio used-ring id/idx/len, config space, capability offset/length/multiplier, the status byte and payload. PCI config space, BAR addresses and sizes. MSI-X table geometry. PS/2 and COM1 bytes. virtio-rng bytes, which go into `/dev/random`. |
| Firmware | Limine memory map, framebuffer and executable address. ACPI RSDP/XSDT/MADT/HPET/FADT/MCFG, parsed before any IDT exists. MADT APIC IDs, the LAPIC/IOAPIC bases and ISO flags. FADT reset and sleep registers. |
| On disk | MBR/EBR/GPT on `ram0` and `vda` at every boot. FAT32 and vibefs images, reachable only through the debug-shell `mount` (feature `kernel_shell`) and the `vibefs_crash` build. The initrd and the `/vibe` volume are generated by the build. |

### 3.9 Invariants the code relies on

Status key:
- *documented*: written in DESIGN or a comment.
- *enforced*: checked by code, types or tests.
- *assumed*: neither.

"Holds?" is what verification found.

| # | Invariant | Established at | Status | Holds? |
|---|---|---|---|---|
| I1 | Lock rank PT < BUDDY < HEAP < SCHED < DEVICE < SERIAL | `lock.rs`, `sync_init.rs:150-187` | documented + enforced at runtime, per CPU | Partly. Same-rank nesting clears the shared bit. `IrqCell` is unranked. Heap growth under a lock panics only when growth actually happens (load-dependent). |
| I2 | Hard-IRQ context never blocks, allocates or schedules | DESIGN §2.2 | documented | Not enforced. `schedule()` has no in-IRQ assertion. |
| I3 | IF stays 0 from `syscall` until `sysretq`/`iretq`, because the per-CPU syscall scratch is shared | FMASK `x86.rs:106` | assumed | **Broken.** `console_init::wait_key` turns IF back on inside `sys_read`. |
| I4 | While kernel code runs, GS_BASE = this CPU's `PerCpu` | `per_cpu_init.rs:45-72`, `arch/gs.rs` | documented | **Broken.** The device and keyboard ISRs never `swapgs`. `enter_user_full` has an IF=1 window. Nothing handles the swapgs/sysret windows or an `iretq` that faults. |
| I5 | `swapgs` iff CS.RPL==3, done "in exactly one place" | DESIGN §7.5, `arch/gs.rs` | documented | Not uniformly applied (see I4). |
| I6 | Ring-3 faults kill the process, never the kernel | DESIGN §2.5 | documented | **Broken.** Contradicted by DESIGN §5.2 and by the code for #DB and #AC. |
| I7 | The kernel never dereferences user VAs; all copies go through HHDM after `check_user_range` | `addr_space.rs:267-374` | documented + enforced | Holds, but the W bit is never checked. |
| I8 | One thread per address space; nothing mutates an AS concurrently | process model | assumed | Holds today. It carries a lot: lock-free user copies, local-only invlpg, `&'static AddressSpace`. |
| I9 | TCBs are never freed, so a raw `*mut Tcb` stays valid forever | `thread_init.rs:388,487,594` | assumed | Holds, but in-place slot reuse aliases a dying thread. |
| I10 | A dead thread's stack is not unmapped while any CPU still runs on it | DESIGN §4.5/§9.2, `thread_init.rs:151-167` | documented | **Broken across CPUs.** DEFERRED is global. |
| I11 | A `WaitQueue` outlives its waiters, and wakers don't touch it after completion is published | DESIGN §9.4 | documented | **Broken** by `IoWaiter::finish`. |
| I12 | Every kernel PML4 slot exists before the first user AS is created | `paging.rs:602-611` | assumed | Holds by boot order only. The ioremap slot is created lazily. |
| I13 | The low identity map is torn down once the APs are up | DESIGN §4.1 | documented ("should be") | **Not done.** It is also GLOBAL and maps VA 0, which defeats `try_current`'s null check. |
| I14 | All buddy frames and page tables lie inside the physmap (≤ 8 GiB) | `pmm_init.rs:119-124`, `paging_init.rs:584-591` | documented + enforced | Holds. The framebuffer is not covered by this. |
| I15 | Frame 0, the kernel, `0x8000` and the framebuffers never enter the buddy | `pmm_init.rs:100-127` | enforced | Holds. The exclusion list silently drops entries past 8. |
| I16 | The kernel PML4 is below 4 GiB, because the trampoline loads a 32-bit CR3 | `smp_init.rs:166` | enforced by skipping APs | Allocation does not guarantee it. With more than 4 GiB of RAM the APs may be silently skipped. |
| I17 | MMIO is mapped UC, RAM WB, with no aliases | DESIGN §2.4 | documented | Partly. The UC patch covers whole 2 MiB leaves, skips a trailing leaf, and patch failures are ignored. |
| I18 | EOI before any context switch; the one-shot timer is re-armed before yielding | DESIGN §5.8 | documented + enforced | Holds. |
| I19 | IOAPIC high dword written before low; IST index 0-based in software, 1-based in the TSS | `apic.rs:251`, `desc.rs:44` | enforced + host-tested | Holds. |
| I20 | `now_ns` is monotonic (seqlock plus `fetch_max`) | `time.rs` | enforced | Holds by construction, which is why the monotonicity ktests cannot fail. |
| I21 | A per-CPU run queue is touched only by its owner CPU with IF off | `per_cpu_init.rs` | enforced (busy flag) | Holds at runtime. `&'static PerCpu` aliases `&mut`, which is formal UB. |
| I22 | `BootCell` is set once, before SMP | `cell.rs:50-62` | documented | Checked only by `debug_assert`. The unbounded `Sync` impl lets `!Sync` data be shared. |
| I23 | Barrier: everything before it completes before anything after it starts. Flush: durable. | DESIGN §10.2 | documented | **Not implemented** for virtio-blk. The cache does not cover in-flight writes. |
| I24 | vibefs never overwrites a live block before the newer super is durable | VIBEFS.md §6/§10 | documented | Depends on the on-disk refcounts being correct, which mount never checks. The in-memory generation is bumped before the super write. |
| I25 | Per-thread CPU state is complete: GPRs, FPU, RSP0, CR3 | `syscall_init::on_switch` | assumed | **FS_BASE is missing.** |
| I26 | Every kernel stack has a guard page | DESIGN §2.4/§9.2 | documented | The boot stack does not, and it runs all of boot plus every ktest. |
| I27 | The portable half never panics on data | DESIGN §2.5, clippy denies on `unwrap`/`expect`/`panic` | documented + partly enforced | Indexing and arithmetic are not covered, and the shipped dev profile has `overflow-checks = true`. |
| I28 | Contract markers are kernel-emitted | DESIGN §2.6 | documented | The final production marker, `shell ready`, is printed from ring 3. |
| I29 | A catch hook only intercepts inside ktest catch windows | `arch/catch.rs` | assumed | Holds only because nothing in production arms it. It is compiled into every exception handler, with global (not per-CPU) state. |

The undocumented invariants that matter most are I3, I8, I9, I12 and I25. Each is load-bearing, none is written down, and each is violated or about to be.

---

## 4. Findings

152 verified findings: 3 CRITICAL, 17 HIGH, 66 MEDIUM, 66 LOW. 77 of them carry a LATENT tag. They are ordered by severity, and within each severity by impact, then by area. Line numbers are for HEAD `e929a7c`. IDs are stable within this document.


### Critical


#### F001 · Syscall exit runs with IF=1 after a blocking console read: an interrupt between loading the user RSP and SYSRET runs at CPL0 on a user-chosen stack pointer

**Severity:** CRITICAL (window open under KVM and on real hardware; closed under the default TCG, which only delivers interrupts at translation-block boundaries) · **Confidence:** Confirmed (IF=1 at exit); Likely (interrupt landing in the window)

**Location:** src/console_init.rs:91-119, src/proc_init.rs:602-621, src/syscall_init.rs:72-141, src/syscall_init.rs:637-640

**What's wrong:** The syscall exit asm assumes the IF=0 that FMASK set at entry lasts until `sysretq`/`iretq`. It stores the return value in per-CPU `gs:[retval]` (line 73), stashes iret state in `gs:[iret_*]`, and runs `mov rsp,[rsp]; swapgs; sysretq` with no `cli`. Console `sys_read` calls `wait_key`, whose `sti; hlt` (line 117) leaves IF=1. Later `read()` InterruptGuards restore IF=1, the `sti` at line 105 is a second IF=1 return, and nothing clears IF before the exit asm.

**Evidence:**
```
call vibeos_syscall_stub
mov qword ptr gs:[{retval}], rax    // :73
...
mov rsp, [rsp]                      // :107
swapgs
sysretq
```
Any console read that had to `hlt` triggers it, e.g. every `sh` keystroke read. The OS must keep interrupts off between loading the user RSP and SYSRET (Intel SDM Vol. 2B, SYSRET); a same-CPL interrupt does not switch stacks (Vol. 3A §6.12.1).

**Impact:** A preemption between lines 73 and 92 lets another same-CPU process's syscall overwrite `gs:[retval]`, returning the wrong value to user mode. This needs a second syscall-making process, which fork provides. An IRQ after line 107 pushes its frame at whatever RSP user code passed to SYSCALL. That is a kernel write for a kernel address, #PF→#DF otherwise. After `swapgs`, per-CPU reads use the user GS base. These windows are probably closed under TCG but open under KVM/HVF and on hardware.

**Fix:** Add a `cli` right after `call vibeos_syscall_stub`, before line 73. Make `wait_key` restore the caller's IF state instead of leaving it set. Add a debug assert that IF=0 on the exit path.


#### F002 · IoWaiter::finish touches the waiter's WaitQueue after the waiter may have returned and reused its stack frame

**Severity:** CRITICAL · **Confidence:** Confirmed

**Location:** src/block_init.rs:98-121, src/block_init.rs:124-133, src/block_init.rs:273-277, src/virtio_blk_init.rs:1193-1209, docs/DESIGN.md:1847-1855

**What's wrong:** `IoWaiter` lives on the submitter's stack. `finish()` publishes `done` with a Release store outside SCHED, then takes SCHED and runs `wake_all` on the embedded `WaitQueue`. `wait()` returns as soon as it sees `done`, either through the lock-free `poll()` (line 100) or the re-check under SCHED (104-105). Nothing keeps the waiter alive until `finish` stops touching `self`; DESIGN.md:1848 documents this order.

**Evidence:**
```rust
fn finish(&self, res: Result<(), BlockError>) {
    self.done.store(pack(res), Ordering::Release);
    thread_init::with_sched(|s| {
        s.wake_all(unsafe { &mut *self.wq.get() });
    });
}
```
The trigger is a completion that lands before the waiter parks while the completer (irqth, or a pump running in another thread) is delayed between the store and `SCHED.lock()`. SCHED contention or a timer preemption can cause that delay, because IF is on at that point.

**Impact:** `wake_all` → `ReadyQueue::pop_front` (sched.rs:99-108) reads and writes `head`/`len`/`buf` in whatever frame now occupies that stack. Possible results: stray writes into a live stack, a bounds-check panic under SCHED, a long spin with IF off, or arbitrary TCBs marked Ready and queued twice. The window is narrow. The boot-time vda probe, kernel_shell mounts, vibefs_crash and ktests all reach it.

**Fix:** Make the `done` store the last access to `*self`: `with_sched(|s| { s.wake_all(wq); self.done.store(..) })`. Moving the store under SCHED but before `wake_all` is not enough, because `poll()` is lock-free. Alternatively, have the waiter take and drop SCHED after it sees `done` (Linux `completion_done`). Add a stress ktest and correct DESIGN.md:1848.


#### F003 · Every boot writes a GPT over a vda with no parsable table, destroying whole-disk vibefs/FAT32 images

**Severity:** CRITICAL · **Confidence:** Confirmed

**Location:** src/part_init.rs:429-453, src/part_init.rs:173-263, src/main.rs:257, docs/DESIGN.md:412, tests/harness/run_vibefs_crash.py:31

**What's wrong:** `part_init::init` runs in every build. It calls `stamp_vda_gpt()` whenever `parse_dev(DEV_VDA)` fails or returns zero entries, and the only guard is `bs == 512 && cap >= 1024`. The stamp writes a protective MBR, GPT header and entries to LBA 0-33, writes the backup to the last 33 sectors, then flushes. DESIGN.md:412 says "(if empty)", but nothing checks that the disk is empty.

**Evidence:**
```rust
if !ok
    && stamp_vda_gpt().is_ok()
    && let Ok(t) = parse_dev(DEV_VDA)
```
Reproduced on the host with the project's own mkfs/parse code: a whole-disk `mkfs-vibefs` image (no 0x55AA) parses as `Err(Empty)`, and a `mkfs.fat` FAT32 image (0x55AA, empty partition table) as `Ok(n=0)`. Both are stamped at 512 KiB or more, as is any disk whose LBA 0 read fails.

**Impact:** The stamp permanently destroys both vibefs super slots, the alloc map and the first inode block, or the FAT32 VBR, FSInfo, backup boot sector and start of FAT#1. `mount_dev` accepts only `ram0`/`vda`, so a whole-disk image is the only supported persistent layout, and that is exactly what gets destroyed. It needs an operator-attached virtio-blk disk (`make run` attaches none); the crash harness escapes only because its image is 512 sectors.

**Fix:** Put the stamp behind `cfg(feature = "kernel_tests")`, or pre-stamp the ktest image with a hostlib tool that uses the `part::pack_*` helpers. If auto-stamping stays, require LBA 0-33 and the last 33 sectors to be all zeros, and never stamp after a read error. Add an e2e test with a 1 MiB vibefs image, and correct DESIGN.md:412.


### High


#### F004 · Device IRQ pool stubs and keyboard ISRs never swapgs, so an interrupt taken at CPL3 reads gs:[0] at address 0 and halts the kernel

**Severity:** HIGH · **Confidence:** Confirmed

**Location:** src/irq_init.rs:92-101, src/irq_init.rs:411-421, src/kbd_init.rs:44-52, src/kbd_init.rs:94-95, src/kbd_init.rs:128, src/per_cpu_init.rs:192-202, src/syscall_init.rs:369-380, src/syscall_init.rs:393-404, docs/DESIGN.md:1161

**What's wrong:** Every handler in src/arch/idt.rs wraps its body in gs_enter/gs_leave. Three handlers are installed from outside idt.rs and do neither: the pool stubs `device_irq::<N>` (vectors 49-127), `kbd_ioapic` and `kbd_pic`. `kbd_pic` also overwrites the gs-aware IRQ1 handler that idt.rs installed. When one of these fires at CPL3 it runs with the user GS base, and enter_user always sets that to 0.

**Evidence:**
```rust
extern "x86-interrupt" fn device_irq<const N: u8>(_frame: InterruptFrame) {
    dispatch(N); // set_in_isr -> try_current -> mov gs:[0]
}
extern "x86-interrupt" fn kbd_ioapic(_frame: ..) { on_irq(); apic_init::eoi(); }
```
Trigger: a PS/2 keypress (IOAPIC destination cpu(0)) arrives while a user process is running ring-3 code with IF=1. Every user process is pinned to CPU0. With virtio-blk attached, a queue-0 completion to CPU0 does the same. Interrupt delivery never changes the GS base (Intel SDM Vol. 3A, "Exception and Interrupt Handling in 64-bit Mode"; SWAPGS, Vol. 2B).

**Impact:** gs:[0] reads linear address 0, which is never mapped in a user address space. The CPL0 #PF goes to exception_halt, so the whole kernel halts when someone types in the QEMU window while a program runs. If user code ever gains control of the GS base (ROADMAP §18.3 FSGSBASE), gs:[0] becomes a user-chosen PerCpu pointer. The irq_nest and IN_ISR writes then become attacker-directed kernel writes, which is CRITICAL. No ktest catches this, because ktests enter ring 3 with IF=0.

**Fix:** Change idt::set_handler to take a body fn and install it behind an idt.rs trampoline that does gs_enter/gs_leave, so no raw x86-interrupt fn can bypass the swap. Add a ktest that enters ring 3 with IF=1 and self-IPIs a pool vector. Correct DESIGN.md:1161.


#### F005 · Ring-3 #DB (TF single-step or INT1) halts the whole kernel; #AC has no signal mapping

**Severity:** HIGH · **Confidence:** Confirmed

**Location:** src/arch/idt.rs:98-105, src/arch/idt.rs:277, src/arch/idt.rs:50-65, src/proc_init.rs:1260-1279, src/panic.rs:227-256, docs/DESIGN.md:240-245, docs/DESIGN.md:704

**What's wrong:** `debug_ex` has no user-mode branch. Outside ktest `catch()` scopes it always calls `exception_halt`, which IPIs every CPU to halt. `sig_for_vec` has no DB arm, so routing the fault to `try_user_fault` alone would still fall through to the halt. DESIGN §5.2 row "halt unless mapped above" (line 704) writes this policy down, and it contradicts §2.5, which says ring-3 faults kill only the process.

**Evidence:**
```rust
extern "x86-interrupt" fn debug_ex(mut frame: InterruptFrame) {
    let user = gs_enter(&frame);
    if catch::intercept(vectors::DB, &mut frame, 0) { gs_leave(user); return; }
    crate::panic::exception_halt(b"#DB", &frame, None, None);
}
```
User code can set RFLAGS.TF with POPF. The single-step trap is a processor exception, so the gate DPL does not block it (Intel SDM Vol. 3A §6.12.1.1; Vol. 3B §18.3.1.4). INT1 (0xF1) also skips the DPL check (SDM Vol. 2A, "INT n/INTO/INT3/INT1").

**Impact:** Any unprivileged process can halt every CPU. #AC (vector 17) also has no `sig_for_vec` mapping, so it ends in `exception_vec` and halts. That part is LATENT (real hardware / other bootloaders): the AP trampoline clears CR0.AM, and on the BSP it comes from Limine and firmware and is clear in practice.

**Fix:** Add `vectors::DB => Some(SIGTRAP)` and `vectors::AC => Some(SIGBUS)` to `sig_for_vec`, and call `try_user_fault` from `debug_ex` when `user` is true. `debug_ex` runs on the Debug IST stack, so either confirm the kill path never sleeps before it switches away, or stop using an IST for #DB. Clear CR0.AM during CPU init, and update DESIGN §5.2.


#### F006 · enter_user_full zeroes GS_BASE at CPL0 with IF=1; an interrupt in that window faults on gs:[0] and halts the kernel

**Severity:** HIGH · **Confidence:** Confirmed

**Location:** src/syscall_init.rs:389-406, src/proc_init.rs:301-315, src/thread.rs:234-240, src/syscall_init.rs:607-609, src/ktest.rs:824-826

**What's wrong:** Spawned and forked processes enter ring 3 through trampoline → `user_thread_entry` → `enter_user_full`. A new TCB has `irq_nest=0`, so `apply_if_on_resume` sets IF, and the `InterruptGuard` in `with_table` restores IF=1. `enter_user_full` then loads USER_DS into GS, writes GS_BASE=0 and FS_BASE, and runs the iret stub, all without a `cli`. `run_user` executes `cli` before `enter_user` (line 609), but this path does not. ktest.rs:825 also calls `enter_user` without one.

**Evidence:**
```rust
core::arch::asm!("mov ds, {0:x}", /* es, fs */ "mov gs, {0:x}", in(reg) USER_DS_RPL, ..);
x86::wrmsr(IA32_GS_BASE, 0);
x86::wrmsr(IA32_FS_BASE, regs.fs_base);
vibeos_iret_user_full(regs as *const UserRegs);
```
A LAPIC tick or IPI that arrives after `mov gs` is taken at CPL0. `gs_enter` therefore skips `swapgs`, and the per-CPU read resolves `gs:[0]` to linear address 0. Only MOV SS and POP SS inhibit interrupts for one instruction; MOV GS and WRMSR do not (Intel SDM Vol. 3A §6.8.3).

**Impact:** CR3 is already the process's address space, and VA 0 is unmapped there. The load raises #PF at CPL0, and `exception_halt` stops the whole kernel. This is a crash, not memory corruption, because user code cannot control VA 0. The window is small, but a process can loop fork/exit to widen its chances. Exec is not affected.

**Fix:** Execute `cli` at the top of `enter_user_full`; iretq restores the user IF from `regs.rflags`. Add a `cli` or a debug assert that IF=0 in `enter_user`. Consider doing the segment loads, MSR writes and iretq in a single asm block.


#### F007 · A fault on the user-return iretq runs on the user GS base with no fixup; a syscall at the top user page crashes the kernel silently on hardware/KVM

**Severity:** HIGH · LATENT (real hardware / KVM) · **Confidence:** Confirmed

**Location:** src/arch/gs.rs:19-22, src/arch/idt.rs:107-117, src/arch/idt.rs:216-220, src/syscall_init.rs:80-86, src/syscall_init.rs:111-141, src/syscall_init.rs:176, src/syscall_init.rs:378, src/syscall_init.rs:402, src/elf.rs:162, src/addr_space.rs:410

**What's wrong:** The GS decision reads only the saved CS.RPL, and no handler recognises a fault at a user-return `iretq`. Such a #GP arrives with KERNEL_CS after the exit `swapgs`. It runs on the user GS base (0) and goes straight to `exception_halt`, never `try_user_fault`. Userspace can reach it: `check_user_va`/`check_map_range` accept a mapping that ends exactly at USER_END, so RCX can be non-canonical after SYSCALL.

**Evidence:**
```rust
pub fn from_user(cs: u64) -> bool { cs & 3 == 3 }   // gs.rs:20
if user { try_user_fault(...) }                      // idt.rs:113-115
exception_halt(b"#GP", &frame, Some(err), None);     // kernel CS: always
```
Trigger: a SYSCALL in the last two bytes of a mapped top user page sets RCX=0x0000_8000_0000_0000, and the exit path sends that through `swapgs; iretq` (:140-141). A fork from that syscall faults at :176. SDM Vol. 2B, SYSCALL: RCX ← RIP, no canonical check. SDM Vol. 2A, IRET 64-bit mode exceptions: #GP(0) when the return RIP is non-canonical.

**Impact:** On KVM or real hardware, `exception_halt` reaches `gs_self`, which reads `gs:[0]` at unmapped VA 0. That recurses through #PF to #DF, then a silent hang or triple fault. QEMU TCG's `helper_ret_protected` skips the canonical check, so default tests get SIGSEGV instead. NMI or #MC in the exit window only loses diagnostics (LOW).

**Fix:** Cap user mappings at USER_END − 4 KiB, like Linux's TASK_SIZE_MAX. Add a bad-iret fixup too: label the `iretq`s at syscall_init.rs:141/151/176. If a #GP/#NP/#SS frame.rip matches one, swapgs and deliver SIGSEGV.


#### F008 · vibefs truncates file block numbers to u32; a write just below offset 2^44 makes map_block overflow and panic the kernel

**Severity:** HIGH · **Confidence:** Confirmed

**Location:** src/vibefs.rs:1298, src/vibefs.rs:1330, src/vibefs.rs:1410, src/vibefs.rs:1433, src/file_init.rs:599-615, src/file_init.rs:589-590, src/file_init.rs:106

**What's wrong:** file_init::seek accepts any offset in [0, i64::MAX]. Vol::read and Vol::write compute `fblk = (pos / 4096) as u32` with no file-size cap. A write at block 0xFFFF_FFFF records an extent with log = u32::MAX. The next map_block over that inode then computes `e.log + e.len`, which overflows u32. Offsets at or above 2^44 wrap modulo 2^44 and alias the low blocks of the same file.

**Evidence:**
```rust
let fblk = (pos / BLOCK as u64) as u32;            // vibefs.rs:1410
...
if file_blk >= e.log && file_blk < e.log + e.len { // vibefs.rs:1298
```
Trigger: write into a /vibe file at an offset in [2^44-4096, 2^44), then make any read or write that maps the same block. A single write of 257 bytes or more is enough, because sys_write splits writes into 256-byte chunks. The default build uses the dev profile, which sets overflow-checks = true (Makefile:14, Cargo.toml:44). The host probe panicked at vibefs.rs:1298:48.

**Impact:** Any process can panic and halt the kernel through /vibe, which is always mounted. In every build profile, a write at or above 2^44 silently overwrites the start of the writer's own file (the probe read back "ZZAA"). In release builds the top block drops writes, and after MAX_EXT duplicate extents writes fail with NoSpace.

**Fix:** Cap vibefs file size so that the last block index is at most u32::MAX-1. Reject larger offsets in Vol::read, Vol::write and file_init::seek: EINVAL for lseek, and EFBIG (needs a new FsError variant) or ENOSPC for write. As hardening, use checked or u64 arithmetic in map_block, split_replace_extent (1460-1463) and truncate (1534). Widen OpenFile.size and vibefs Node.size to u64 so SEEK_END and O_APPEND stay correct above 4 GiB.


#### F009 · Unbounded ELF p_memsz drains all physical memory under the PT lock and releases it with the buddy empty

**Severity:** HIGH · **Confidence:** Confirmed

**Location:** src/elf.rs:154-169, src/elf.rs:252-259, src/user_init.rs:22, src/user_init.rs:93-115, src/user_init.rs:126-137, src/addr_space_init.rs:39-51, src/addr_space.rs:190-230, src/heap_init.rs:183-191, src/thread_init.rs:528

**What's wrong:** `elf::parse` checks `p_memsz` only against USER_END. MAX_ELF limits only the file size. `map_loads` passes `page_up(memsz)` to `addr_space_init::map_anon`, which holds the PT SpinMutex for a loop that allocates one buddy frame per page until the buddy is empty. On leaf-frame OOM it returns through `?` without the rollback the `map_page` error path does. PT_TLS memsz is also uncapped, up to about 2 GiB through `setup_tls`.

**Evidence:**
```rust
let end = elf::page_up(seg.vaddr.saturating_add(seg.memsz));   // user_init.rs:99
match unsafe { addr_space_init::map_anon(space, start, len, perms) } { .. }
// addr_space.rs:198, inside the per-page loop:
c.alloc_frame().ok_or(AsError::OutOfFrames)?
```
Any process can write a small ELF with one huge PT_LOAD and execve it; a host test confirmed `parse` accepts memsz 0x7000_0000_0000.

**Impact:** The CPU running execve keeps IF off (FMASK) and holds PT until RAM runs out. That is under a second at the default 128 MiB and much longer near the 8 GiB physmap cap, where teardown's free-list scans are quadratic. PT is then released with the buddy empty until teardown runs. A CPU queued on PT for heap growth or a thread stack can grab it first and panic (Likely; race). Sizing memsz to leave almost nothing free makes later forks panic.

**Fix:** Cap the total page-rounded PT_LOAD and TLS memsz in `elf::parse`/`load_path` and return ENOMEM above it. Roll back already-mapped pages when a leaf allocation fails in `AddressSpace::map_anon`. Do not hold PT across a user-sized loop: chunk it or reserve frames up front.


#### F010 · Userspace fork/exit pressure panics the kernel: the 8-slot deferred-stack list overflows, and spawn_inner uses expect() instead of returning an errno

**Severity:** HIGH · **Confidence:** Likely

**Location:** src/kva_init.rs:35-36, src/kva_init.rs:122-134, src/thread_init.rs:152-166, src/thread_init.rs:268-277, src/thread_init.rs:528, src/proc_init.rs:856-857, src/heap_init.rs:183-192, src/thread_init.rs:590

**What's wrong:** Every exiting thread parks its kernel stack on a global 8-entry DEFERRED list, and `defer_free` panics when that list is full. The list drains only on voluntary schedule returns, trampolines and idle. A thread resumed from timer preemption (`from_irq=true`) skips `reap_zombies`. Separately, `sys_fork` calls `spawn_user` after `clone_full` succeeds, and `spawn_inner` uses `expect()` on the stack allocation, so running out of buddy frames there panics instead of returning ENOMEM. Refuted for userspace: KVA node exhaustion and `bind_current`'s `expect("proc table")`. `expect("thread table full")` (:590) is LATENT and needs about 23 or more CPUs.

**Evidence:**
```rust
const MAX_DEFERRED: usize = 8;               // kva_init.rs:35
panic!("kva: deferred free list full");      // kva_init.rs:131
if !from_irq { reap_zombies(); }             // thread_init.rs:275
alloc_guarded_stack(..).expect("thread stack") // thread_init.rs:528
```
Trigger: more than 8 user processes exit back to back, each handing off to a preempt-resumed sibling, before any drain. MAX_PROCS=16 allows this, since user threads are pinned to one CPU with a FIFO runq.

**Impact:** An ordinary unprivileged exit burst, or a fork near memory exhaustion, panics the kernel. On SMP, drains on other CPUs make the overflow timing-dependent but do not prevent it.

**Fix:** Defer frees per TCB (free on Dead-slot reuse or reap), or drain on every switch, including the preempt path. Never panic, and never free a stack whose CPU has not switched away. Make `spawn_inner` return `Result` and allocate the stack before committing the pid and AS. Then return ENOMEM/EAGAIN from `sys_fork` and `spawn_elf`.


#### F011 · wait_acks panics after about 1 s without an ack, while syscall bodies and the ktest runner hold IF off with no time bound

**Severity:** HIGH · **Confidence:** Likely

**Location:** src/ipi_init.rs:123-150, src/ipi_init.rs:157-172, src/ipi_init.rs:257-288, src/x86.rs:106, src/syscall_init.rs:226, src/proc_init.rs:526-573, src/ktest.rs:205, tests/harness/harness.py:837-898

**What's wrong:** `shootdown_va` and `call_mask` wait on every online CPU, and `wait_acks` panics when any of them has not acked within about 1 s. A CPU with IF=0 acks only when it polls `service_incoming`. Every syscall body runs with IF masked (FMASK 0x47700 includes IF, and no `sti` follows), and `sys_write` loops over an uncapped length without polling. The finding's original causes are refuted: IrqCell sections and `switch_now` are short, and `halt_if_idle` ends in `sti; hlt`.

**Evidence:**
```rust
if cap != 0 && time_init::read_tsc().wrapping_sub(start) > cap {
    panic!("ipi: ack timeout waiters={waiters:#x}");  // ipi_init.rs:143
}
```
CI run 35789075345 hit this panic (AP idle → drain_deferred → shootdown_va) while `ktest::run` held IF off on the BSP (ktest.rs:205). harness.py retries it as a flake. SDM Vol. 2B, SYSCALL: RFLAGS &= ~IA32_FMASK.

**Impact:** If any CPU stays IF-off for more than about 1 s, a routine TLB shootdown on another CPU panics the kernel. User processes are pinned to CPU0, so today this needs an AP-side shootdown during a large console write. It gets easy with user threads on APs, or on real hardware, where 12 KB at 115200 baud takes about 1 s.

**Fix:** Have `wait_acks` keep waiting and log a rate-limited klog. It must never return early, because callers free frames right after the shootdown. Enable IF inside syscall bodies, or poll `service_incoming` once per `sys_write` chunk. Stop running ktests with IF off, then remove both SMP4 IPI signatures from the retry allow-list.


#### F012 · An exiting thread's kernel stack can be unmapped by another CPU before the exiting CPU switches off it

**Severity:** HIGH · **Confidence:** Confirmed

**Location:** src/thread_init.rs:151-167, src/thread_init.rs:333-337, src/thread_init.rs:141, src/thread_init.rs:269-276, src/thread_init.rs:542-558, src/kva_init.rs:120-154, src/kva_init.rs:196-240, src/sched_init.rs:52-60, docs/DESIGN.md:609, docs/DESIGN.md:1604

**What's wrong:** thread_exit() puts its own kernel stack on the single global DEFERRED list, then keeps running schedule() on that stack until switch_context. reap_zombies() drains every entry on whichever CPU calls it (trampoline, non-IRQ schedule tails, switch_to, idle_loop). Nothing records whether the owning CPU has switched away yet. So free_stack_shootdown's "not the running stack" contract and DESIGN §9.2's "drained by a thread that is not on them" hold only for the local CPU.

**Evidence:**
```rust
(*p).state = ThreadState::Dead;
if let Some(ks) = (*p).stack.take() {
    kva_init::defer_free(GuardedStack { guard: VirtAddr(ks.guard), pages: ks.pages });
}
schedule(); // still executing on the deferred stack
```
Trigger: a user process exits on CPU0 while an AP thread (blk-wb's 50 ms loop, irqth or a wq worker) passes a schedule tail during the defer_free→switch_context window and unmaps the stack. The exiting CPU faults as soon as its TLB entry is gone. On QEMU TCG, the CR3 write in on_switch flushes even global entries. With 3 or more CPUs, the invlpg serviced inside SpinMutex::lock has the same effect.

**Impact:** The #PF is delivered onto the unmapped stack and escalates to #DF (Intel SDM Vol. 3A, Interrupt 8, Table 6-5), which panics on its IST. The result is a rare, timing-dependent kernel panic from ordinary fork/exit activity at the default SMP=2. Frames are never reused while the CPU still runs on them, because unmap_shootdown frees them only after every CPU has acked the invlpg. A related latent issue: spawn_inner's Dead-slot scan (line 544) excludes only the local CPU's threads, and the Dead state is stored without SCHED held. A spawn on another CPU can therefore rewrite a TCB that is still switching out. Only ktests reach this today. It becomes CRITICAL once Phase 13 threads run user work on several CPUs.

**Fix:** Make reclaim owner-scoped, as Linux's finish_task_switch does: park the dying stack in a per-CPU slot, and have that same CPU free it after switch_context returns. Let spawn reuse a Dead slot only after an on_cpu flag, cleared after the switch, shows the switch-out has finished.


#### F013 · Each FAT open file keeps its own copy of first cluster, size and dirent slot; a second descriptor on the same file makes that copy stale, which leaks or cross-links chains and loses data

**Severity:** HIGH · **Confidence:** Confirmed

**Location:** src/file_init.rs:95-126, src/file_init.rs:485-521, src/file_init.rs:557-559, src/file_init.rs:574-587, src/file_init.rs:605, src/fat.rs:440-466, src/fat.rs:1118-1186, src/fat.rs:779-811, src/fat_init.rs:353-356

**What's wrong:** Each open() copies clu, size, dir_clu and dir_off into its own OpenFile. FAT read, write and truncate work from that copy and write it back to the dirent through update_short. Nothing refreshes other descriptors open on the same file. dup and fork share one slot, so they are not affected. The put_size ino table is read only by by_ino, which has no callers. On both FAT and vibefs, O_APPEND and SEEK_END use the cached size.

**Evidence:**
```rust
Back::Fat => fat_init::write(f.vol, f.dir_clu, f.dir_off, f.ino,
    &mut f.clu, &mut f.size, f.offset, buf)?,
// fat.rs:460
if need > *size { *size = need;
    self.update_short(d, dir_clu, dir_off, *first, *size)?;
```
Host probes confirmed three triggers:
- Two descriptors on an empty file both write. Each allocates its own chain, the last dirent update wins, and the other chain is orphaned.
- A stale write that ends past its cached size but before the real EOF shrinks the dirent size.
- A second fd truncates the file with O_TRUNC and the next-fit allocator reuses the freed cluster, which is easy on the 64 KiB initrd. The stale fd then reads and writes another file's clusters, and an extending write cross-links the two chains.

**Impact:** Any process can leak clusters, lose data and cross-link chains on the writable initrd FAT using ordinary open, read, write and lseek. This breaks ROADMAP §8.1 and §8.3. The damage is RAM-only today, because vda FAT mounts exist only in the kernel_shell build, which starts no userspace. It becomes persistent (CRITICAL) once userspace can reach a disk-backed FAT mount.

**Fix:** Add a refcounted in-core FAT inode, keyed by dirent location, that owns the first cluster and size. OpenFile then holds only a reference, offset and flags, and every operation updates the inode under the volume lock. Defer free_chain to the last close and re-key the inode on rename. get_file/put_file (file_init.rs:343-361) copy refs back the same way and can race on shared fids (Likely), so fix them in the same change.


#### F014 · vibefs commit never re-marks the directory blocks it writes, so each later commit leaks them; volumes stop committing after about 57 syncs or become unmountable past MAX_META

**Severity:** HIGH · **Confidence:** Confirmed

**Location:** src/vibefs.rs:1782-1841, src/vibefs.rs:1916-1950, src/vibefs.rs:2113-2139, src/vibefs.rs:833-840, src/vibefs.rs:39, src/vibefs_init.rs:594, tests/harness/run_vibefs_crash.py:97-102

**What's wrong:** On every commit, commit allocates new DIR_LEAF and DIR_INT blocks for each non-empty directory. After the super flush it rebuilds `meta` from the alloc block and the inode blocks only. The next commit releases only `meta` and the drop list, so the previous generation's directory blocks keep refc 1 forever. mount, by contrast, calls mark_meta on every directory block and fails at MAX_META = 48. commit never checks that cap.

**Evidence:**
```rust
self.nmeta = 0;
self.mark_meta(alloc_bno)?;
while li < ileaves { self.mark_meta(ileaf[li])?; li += 1; }
if iints > 0 && iroot != ileaf[0] { self.mark_meta(iroot)?; }
```
Host probe on a 64-block volume with one file: free blocks fell from 59 to 49 over 10 syncs, and the first NoSpace came at sync 58 (fsck `errors 0 warnings 59`). A chain of 44 nested directories syncs fine, but the remount returns NoSpace. The crash harness froze at `gen 57 … warnings 58` in rounds 2-8 and still passed, because crash_loop ignores the sync_fs error and the harness checks only `errors 0`.

**Impact:** From the second dirty commit in a mount session on, each commit leaks the previous commit's directory blocks. A 64-block volume stops committing after about 57 syncs. umount (`let _ = sync(id)`) then drops uncommitted data silently. Today commit is reachable only from kernel-shell sync and umount, from ktests and from the vibefs_crash build; there is no sync syscall, and /vibe is re-formatted at every boot. Even so, the harness behind the Phase 8 exit gate has not tested commits past generation 57.

**Fix:** Call mark_meta on the dleaf and droot blocks. Size MAX_META for the worst case (about 72 blocks), or check capacity before the super write, so that mark_meta cannot fail after the commit is on disk. Make the harness fail when warnings grow or the generation stops advancing. Add a host test that free_count stays constant across commits and remounts; FS-04 is a second leak that only a remount exposes.


#### F015 · Block cache has no fill/writeback state: flush misses in-flight writes, same-LBA writes overlap, and stale or duplicate slots are possible

**Severity:** HIGH · LATENT (Phase 12.5 unified page cache / production disk mounts) · **Confidence:** Confirmed

**Location:** src/cache.rs:151-161, src/cache.rs:218-230, src/cache.rs:250-257, src/cache.rs:266-278, src/cache.rs:320-325, src/cache.rs:384-408, src/cache.rs:426-433, src/cache_init.rs:125-129, src/cache_init.rs:339-371, src/cache_init.rs:390-400, src/block.rs:445-456

**What's wrong:** Cache slots have no FILLING or WRITEBACK state.
- take_dirty clears F_DIRTY under the lock, and writeback_dev then writes the page with the lock dropped. A concurrent flush(dev) skips that page and sends the device Flush while the write is still in flight.
- If the page is dirtied again in that window, two writes to the same LBA can be in flight at once.
- The slot looks clean while its write is in flight, so it can be evicted without writeback and read back stale from the device.
- In-progress slots carry only F_FILL, and find() matches only F_VALID. The "wait for filler" paths (cache.rs:223, 271; cache_init.rs:232-239, 294-301) are therefore dead code.
- A dirty victim is re-keyed before its writeback lands.

**Evidence:**
```rust
self.meta[s].flags = f & !F_DIRTY;   // take_dirty
// flush():
writeback_dev(Some(dev))?;           // skips pages blk-wb already took
raw_flush(dev)?;
```
Trigger: blk-wb runs `writeback_dev(None)` once more than 8 of 16 pages are dirty, while the FS thread syncs or re-dirties pages on the same device. virtio v1.2 §5.2.6.2 makes a write stable only if a FLUSH is sent after that write completes.

**Impact:** vibefs commits in the order meta → flush → super → flush (vibefs.rs:1907-1914). The super can become durable before metadata that blk-wb is still writing. An older same-LBA write can also land last while the cache holds the newer page as clean. Either way, updates on vda are lost persistently. Today only the kernel_shell `mount` command and vibefs_crash send I/O through the cache. Duplicate keys and the restore_evict clobber need two concurrent FS callers, which no build has yet. Phase 12.5 builds the unified page cache on these slots.

**Fix:** Add explicit FILLING state (key visible, readers wait) and WRITEBACK state (key visible and readable, no second write), and keep the old key visible until its writeback completes. flush(dev) must wait for the device's WRITEBACK pages before calling raw_flush. Replace yield-spinning with a per-slot wait queue, and add a host test that interleaves plan/install calls.


#### F016 · No full barrier between the avail.idx store and the avail_event load, so a lost kick can permanently wedge a virtio-blk queue

**Severity:** HIGH · LATENT (userspace disk mounts) · **Confidence:** Likely

**Location:** src/virtio.rs:504-511, src/virtio.rs:551-559, src/virtio.rs:535-549, src/dma.rs:184-194, src/virtio_blk_init.rs:482-485, src/virtio_blk_init.rs:604-631, src/block_init.rs:98-114, docs/DESIGN.md:636-638, docs/DESIGN.md:1644-1645, docs/DESIGN.md:1895

**What's wrong:** issue() calls publish() (dma_wmb, then the avail.idx store), then sync_for_device() (dma_wmb), then should_kick(), which loads avail_event or used.flags. On x86, dma_wmb is `fence(Release)`, which emits no instruction, plus `sfence`, which does not order loads. So the load can complete before the buffered index store is visible. Intel SDM Vol. 3A ("Loads May Be Reordered with Earlier Stores to Different Locations") allows this. It violates virtio v1.2 §2.7.13.4.1, which requires a suitable barrier "before reading flags or avail_event". DESIGN.md:1895 states that dma_wmb is sufficient.

**Evidence:**
```rust
dma::dma_wmb();                                    // Release + sfence
store_u16(self.base, self.layout.avail_idx(), new);
// ...
let event = load_u16(self.base, self.layout.avail_event());
```
Trigger: QEMU's virtqueue handler, running on its own host thread via ioeventfd, re-arms avail_event, runs smp_mb, and re-checks avail.idx while the guest publishes to the same queue at the same moment.

**Impact:** With EVENT_IDX, one lost kick leaves avail_event stuck, and need_event never fires again for that queue. IoWaiter::wait has no timeout (FAR_DEADLINE), so every request on the queue hangs, and eventually all 16 slots are pinned. Today only ktest, kernel_shell and vibefs_crash builds do vda I/O: there is no mount syscall, and /dev block nodes return NotSupp. So the current exposure is a rare CI hang on x86 hosts. TCG on an arm64 host likely masks it. On the used_event side, the only barrier is an accidental one: the `lock`-prefixed COMPLETIONS.fetch_add in harvest (virtio_blk_init.rs:630).

**Fix:** Add a `dma_mb()` (`fence(SeqCst)`/mfence or `lock or [rsp],0`). Call it in should_kick between the avail.idx store and the avail_event/used.flags load, and in get_used after the used_event store (Linux uses virtio_mb and virtio_store_mb at these points). Update DESIGN §4.7, the §9 pitfall and line 1895 to require the full barrier.


#### F017 · BootCell and IrqCell are Sync/Send for every T and IrqCell::force_unlock is a safe fn, so the mandated core cells are unsound

**Severity:** HIGH · **Confidence:** Confirmed

**Location:** src/cell.rs:36-38, src/cell.rs:94-95, src/cell.rs:137-141, src/log_init.rs:82-84, src/per_cpu_init.rs:23, src/proc_init.rs:124, src/smp_init.rs:49, src/diag.rs:60-81, src/sched_init.rs:42, docs/DESIGN.md:189-195, scripts/check_cells.py:25

**What's wrong:** The `unsafe impl<T> Sync/Send` for BootCell and for IrqCell have no bounds. IrqCell hands `&mut T` to whichever CPU wins the CAS, so it needs `T: Send`, as Mutex does. BootCell hands `&T` to every CPU, so it needs `T: Send + Sync`, as OnceLock does. Today that requirement exists only as a comment. `IrqCell::force_unlock` and `log_init::with_logger_unlocked` are safe fns that break the cell's exclusivity. The safe `as_ptr` is not a hole: it only returns a raw pointer.

**Evidence:**
```rust
// After `set`, `&T` is shared. Caller puts a `Sync` `T` in the cell.
unsafe impl<T> Sync for BootCell<T> {}
unsafe impl<T> Sync for IrqCell<T> {}
pub fn force_unlock(&self) { self.owner.store(0, Ordering::Release); }
```
With std-equivalent bounds, the build fails at exactly three statics: CPUS (PerCpu), TABLE (Proc) and STARTING. A host copy of cell.rs shows that safe code can corrupt a non-atomic refcount held in an IrqCell static.

**Impact:** These are the only Sync wrappers AGENTS.md and DESIGN §2.3 allow, so the compiler's Send/Sync checks are silently off for every static that uses them. The one concrete in-tree race is in diag::cpus_to. It reads another CPU's `ticks`, `switches` and `runq` through BootCell's `&PerCpu`, while the owning CPU writes them through `&mut` (sched_init.rs:42). That is formal UB, but benign on x86 today. check_cells.py only greps for the literal text `unsafe impl<T> Sync`, so it cannot catch any of this.

**Fix:** Bound BootCell as `Sync where T: Send + Sync` and `Send where T: Send`, and IrqCell as `Send + Sync where T: Send`. Make force_unlock and with_logger_unlocked `unsafe fn`. For the three statics that then fail to compile, add a justified `unsafe impl Send` on the containing type. Make the PerCpu stats that other CPUs read into atomics. Fix DESIGN §2.3 and cell.rs:5: they call IrqCell CPU-local, but TABLE, KVA, LOG and other statics use it as a cross-CPU lock.


#### F018 · Safe pub fns and Copy ownership tokens let safe code free arbitrary frames, unmap arbitrary kernel VA and load arbitrary CR3

**Severity:** HIGH · **Confidence:** Confirmed

**Location:** src/dma.rs:97-116, src/dma.rs:224-227, src/dma_init.rs:21-23, src/kva_init.rs:19-23, src/kva_init.rs:116-134, src/kva_init.rs:186-194, src/pmm.rs:89-94, src/addr_space_init.rs:64-69, src/addr_space_init.rs:94-102, src/syscall_init.rs:317-329, src/thread.rs:124-153, src/paging_init.rs:179-189, src/paging.rs:602-611

**What's wrong:** Several safe functions discharge `unsafe` memory-management contracts without checking them, and the ownership handles `DmaBuffer` (safe `from_phys`) and `GuardedStack` are `Copy` with pub fields. The safe `free_to_buddy`/`dma_init::free`/`free_stack`/`defer_free` free whatever the handle names, and `vunmap` unmaps any VA into the KVA free list (`Kva::free` checks alignment and `used >= len`, not the window). `Buddy::set_hhdm_offset` is safe, `load_cr3_u64` and `switch_cr3_for` (via pub `Tcb.as_cr3`) load any value into CR3, `teardown` never checks that the root is no longer loaded, and `current_mapper()` mints unlocked `Mapper`s over the shared kernel PML4.

**Evidence:**
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaBuffer { pub virt: u64, pub phys: u64, /* .. */ }
pub fn free_to_buddy(buddy: &mut Buddy, buf: DmaBuffer) {
    buf.sync_for_cpu();
    unsafe { buddy.deallocate(buf.phys, buf.order) };
}
```
Freeing `DmaBuffer::from_phys(0x1000, ..)` into an empty `Buddy` from a `#![forbid(unsafe_code)]` host crate segfaults. Every current caller is correct.

**Impact:** One mistaken safe call frees live or never-owned frames (`deallocate` only catches blocks still on a free list, so a reallocated frame is handed out twice), unmaps live kernel VA, or loads a freed page table. Each new driver builds on these APIs.

**Fix:** Make `DmaBuffer` and `GuardedStack` move-only with private fields and allocator-only constructors. Mark the raw-address functions `unsafe fn` (or have `current_mapper` return a PT-lock guard), make the HHDM offset a `Buddy::new` argument, and assert in `teardown` that no CR3 or `as_cr3` holds the root. `vmap` and `ensure_physmap_wb` only add mappings and need no change.


#### F019 · AddressSpace references escape as &'static from the proc-table Box and from untied raw pointers (Proc.borrowed, CURRENT_AS)

**Severity:** HIGH · LATENT (threads/mmap) · **Confidence:** Confirmed

**Location:** src/proc_init.rs:183-205, src/proc_init.rs:318-329, src/proc_init.rs:919-951, src/proc_init.rs:998-1029, src/syscall_init.rs:433, src/syscall_init.rs:525-550, src/syscall_init.rs:612, src/paging.rs:212-220, src/addr_space.rs:306-374

**What's wrong:** `current_space()`/`space_of()` return `&'static AddressSpace` built from the `Box` in the proc table, which `sys_execve` and `finish_exit` later tear down. The safe `bind_current(&mut AddressSpace)` and `set_user_as(&AddressSpace)` store raw pointers (`Proc.borrowed`, the global `CURRENT_AS`) with no lifetime tie, and `current_as()`/`peek_user_as()` hand them back as `&'static`. Every syscall user copy goes through these accessors. The `Mapper` comment "Not `Sync`" is also false (it is auto Send+Sync), though the escape goes through raw pointers, not `Sync`.

**Evidence:**
```rust
fn space_of(pid: u32) -> Option<&'static AddressSpace> {
    with_table(|t| { /* .. */ Some(&**s as *const AddressSpace) /* .. */ })
    .map(|p| unsafe { &*p })
}
pub fn set_user_as(space: &AddressSpace) {
    CURRENT_AS.store(space as *const AddressSpace as *mut AddressSpace, Ordering::Release);
}
```
Binding or setting a space and then dropping it without unbind/clear leaves a dangling `&'static` that safe code can read.

**Impact:** Nothing dangles today: each process has one thread, only that thread tears down its space, and all three teardown paths clear or replace the pointers first. Once threads, munmap or cross-process access exist, syscalls copy through freed page tables or reused frames (CRITICAL at that milestone), and the unlocked `read_bytes`/`write_bytes` lose their "All-or-nothing" guarantee.

**Fix:** Replace the `&'static` accessors with a scoped guard (`with_current_space(|as| ..)`) over a per-process lock or `Arc`, and make `bind_current` take ownership or become `unsafe`. Delete `CURRENT_AS`, `set_user_as`/`peek_user_as` and the pid-0 fallback, whose read path has no live caller. Fix the "Not `Sync`" comment.


#### F020 · Framebuffer above the 8 GiB physmap cap is written through an unmapped HHDM address, so boot halts at console init

**Severity:** HIGH · LATENT (Phase 20 Real Hardware) · **Confidence:** Confirmed

**Location:** src/fb_init.rs:57-72, src/fb_init.rs:76-103, src/boot.rs:76-90, src/paging_init.rs:52, src/paging_init.rs:584-591, src/paging_init.rs:512

**What's wrong:** Fb.base is Limine's HHDM virtual address (`fb.address()`). After paging_init::install switches to the kernel's own PML4, the physmap covers only `[0, min(align2M(max(RAM end, fb end, kernel end)), 8 GiB))`. A framebuffer at or above PHYSMAP_CAP is left unmapped, and one that straddles the cap is only partly mapped. Fb::new checks bpp, width, height and pitch, but never compares the framebuffer against paging_init::map_end().

**Evidence:**
```rust
.chain(info.framebuffers().map(|fb| fb.phys + fb.size))
.fold(info.kernel_phys.end, u64::max);
paging_align_up(hi, PAGE_SIZE_2M).min(PHYSMAP_CAP)
// fb_init: Some(Self { base: i.virt, .. })  -- no map_end check
```
Trigger: the UEFI GOP framebuffer sits in a 64-bit BAR above 8 GiB. Examples are an Intel iGPU aperture at 0x40_0000_0000, or a discrete GPU with Above-4G decoding or ReBAR. The Limine protocol (HHDM feature, base revision ≥ 3) guarantees the framebuffer is mapped only under Limine's own page tables.

**Impact:** `fb.fill(BG)` runs from console_init (main.rs:245) and takes a kernel-mode #PF. exception_halt then stops boot right after `smp: done`. QEMU's std-VGA BAR is below 4 GiB, so neither CI nor the ktest guard (ktest.rs:1058-1060) can catch this. A high framebuffer also pushes map_end up to the full 8 GiB cap, which maps non-RAM holes as WB. This becomes CRITICAL once real UEFI hardware is a target.

**Fix:** In paging_init::install, map each framebuffer range at HHDM_BASE+phys independently of PHYSMAP_CAP, and leave framebuffers out of physmap_extent. At minimum, Fb::new should reject a framebuffer whose phys+size exceeds map_end() and fall back to serial. A WC ioremap would need PAT support first, because the ioremap window is UC-only today.


### Medium


#### F021 · Harness retry rules turn intermittent kernel hangs and SMP4 panics into passes

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** tests/harness/run_ktest.py:76-102, tests/harness/harness.py:859-895, tests/harness/harness.py:804-808, tests/harness/harness.py:1003-1011, tests/harness/run_e2e.py:58-67, .github/workflows/smp-stress.yml:47-48, CHANGELOG.md:54-57, docs/DESIGN.md:1828-1831

**What's wrong:** ktest attempt 0 retries any error containing "timed out", at any SMP count and accelerator, on both boots, and attempt 1 retries again if the tail ends in `user: dup ok`. At SMP4, any Rust panic (not CPU exception dumps) is retried if its 40-line tail contains `ipi: ack timeout waiters=`, the `ipi_init::wait_acks` frame (which also covers the IF-on assertion and any IPI callback panic), or a banner glued to a `ktest: ok` line. e2e retries "timed out" and "no shell ready", which `run_qemu_console_input` raises on any timeout, even after shell ready. Four commits widened the rules (#75, #79, #80, #82); no ROADMAP line tracks any root cause, as DESIGN §9.8 requires.

**Evidence:**
```python
timed_out = "timed out" in str(e)
if (attempt == 0 and (timed_out or retryable)) or (
    attempt == 1 and extra_dup_ok
):
    continue
```
In a review run, a test-lapic-fallback boot hung after `user: dup ok`, was retried, and the tier went green.

**Impact:** An intermittent hang or IPI panic with per-boot probability p fails CI with about p² (p³ for the dup-ok class); deterministic failures still fail. The weekly smp-stress job uses the same rules. The CHANGELOG claims that "Other assertions stay hard" and that the fork-bomb yields stop the wait4 hang are false; the observed hang precedes the fork bomb.

**Fix:** Remove the blanket timeout retry and the frame and same-line needles. Fix each known flake, or add a ROADMAP line plus a quarantine naming one test and one exact message that prints FLAKY and fails on repeat. Add per-test ktest deadlines.


#### F022 · FS_BASE (user TLS base) is not saved or restored on context switch, so a TLS process can resume with base 0

**Severity:** MEDIUM · LATENT (TLS userspace: Phase 13 libc, CLONE_SETTLS threads) · **Confidence:** Confirmed

**Location:** src/syscall_init.rs:347-357, src/syscall_init.rs:379, src/syscall_init.rs:403, src/proc_init.rs:853-855, src/proc_init.rs:960, src/arch/gs.rs:58-85, src/proc_init.rs:1025, src/proc_init.rs:1030, src/proc_init.rs:1290, src/user_init.rs:126-137

**What's wrong:** IA32_FS_BASE is written only at first ring-3 entry and at execve, and `force_kernel()`'s `mov fs, KERNEL_DS` resets it to 0 on every process exit and user-fault kill. Neither `on_switch` (FPU, RSP0, CR3) nor `switch_context` saves or restores it, so processes pinned to one CPU run with whatever base was last set. `sys_fork` copies the live MSR, so a stale value becomes the child's permanent base.

**Evidence:**
```rust
pub fn on_switch(cpu: &mut PerCpu, old: *mut Tcb, new: *mut Tcb) {
    switch_fpu(old, new);
    if !new.is_null() {
        unsafe { set_rsp0_for(cpu, &*new); switch_cr3_for(cpu, &*new); }
    }
}
```
A PT_TLS process resumes with FS_BASE=0 after another process on its CPU exits, is killed, or starts or execs a non-TLS image. FS.base is a linear address resolved through the current CR3, and a selector load sets it from the descriptor base (Intel SDM Vol. 3A §3.4.4).

**Impact:** No cross-process access or disclosure: `setup_tls` gives every TLS process the same base (0x7FFD_FFF8), and CR4.FSGSBASE is off. The TLS process's next `%fs` access faults and it is killed with SIGSEGV. No shipped binary has PT_TLS, though ROADMAP §9.4 marks PT_TLS done.

**Fix:** Keep `fs_base` per TCB (or treat `Proc.entry.fs_base` as authoritative), rdmsr it for a user `old` and wrmsr it for a user `new` in `on_switch`, and have `sys_fork` copy the parent's saved value. Add a ktest: a PT_TLS process forks an exiting child, then reads back an `%fs`-relative value.


#### F023 · AddressSpace::write_bytes ignores the PTE W bit, so read/wait4/psinfo can write into read-only or executable user pages

**Severity:** MEDIUM · LATENT (Phase 12 COW / shared zero page: becomes CRITICAL) · **Confidence:** Confirmed

**Location:** src/addr_space.rs:316-324, src/addr_space.rs:266-304, src/addr_space.rs:347-374, src/proc_init.rs:608, src/proc_init.rs:627, src/proc_init.rs:1084-1090, src/proc_init.rs:1205-1214

**What's wrong:** `write_bytes` skips the writable check on purpose, so the ELF loader can fill RX pages, and copies through the supervisor-RW HHDM alias. `check_user_range` checks only that pages are present and USER. `sys_read`, the `wait4` status pointer and `sys_psinfo` use it to copy data out to user memory. So they can write into any present user page, including the RX text of every user image. CR0.WP does not help, because the write never goes through the user PTE.

**Evidence:**
```rust
/// Does not require the PTE to be writable (ELF load onto RX pages).
pub fn write_bytes(&self, dst: u64, src: &[u8]) -> Result<(), UserMemError> {
    ...
    self.check_user_range(dst, src.len() as u64)?;
```
A `read()` buffer or `wait4` status pointer aimed at the caller's own text succeeds. Linux returns EFAULT here.

**Impact:** Today a process can overwrite only its own read-only or executable pages. That defeats W^X inside the process but crosses no privilege boundary. Under ROADMAP 12.2/12.3 the same path writes into COW-shared frames or the global zero page, which is cross-process memory corruption.

**Fix:** Rename the W-ignoring copy and keep it for kernel-internal callers only: the ELF loader, `clone_anon` (addr_space.rs:488) and ktest. Add a syscall-facing `copy_to_user` that requires `PageFlags::WRITABLE` on every leaf and returns EFAULT otherwise; at Phase 12 that check becomes the COW-break point. Add a regression test for `read()` into a read-only page.


#### F024 · No KPTI: every user CR3 maps the whole kernel half (a GLOBAL, writable physmap, the heap and kernel stacks), and no KPTI decision is recorded

**Severity:** MEDIUM · LATENT (real hardware or KVM on a Meltdown-affected Intel host; untrusted users, Phase 18/22) · **Confidence:** Confirmed

**Location:** src/addr_space.rs:143, src/paging.rs:602-611, src/paging.rs:774-800, src/paging_init.rs:52, src/paging_init.rs:443-456, src/paging_init.rs:584-591, src/syscall_init.rs:42-72, src/arch/idt.rs:36-50, docs/DESIGN.md:481, docs/DESIGN.md:689-690, docs/ROADMAP.md:1566

**What's wrong:** Every user PML4 copies PML4[256..512) unchanged from the kernel root. While ring 3 runs, the physmap (all RAM the kernel uses, up to 8 GiB), the kernel image, the heap, the KVA kernel and IST stacks, and the ioremap window all stay mapped. Only U=0 protects them, and they are marked GLOBAL. There is no KPTI, no PCID and no RDCL_NO detection. ROADMAP §18.3 has only a generic "speculation mitigations" line and does not say that x86-interrupt handlers would need asm entry stubs for a CR3 switch.

**Evidence:**
```rust
while i < PTES_PER_TABLE {
    let e = unsafe { src_ptr.add(i).read_volatile() };
    unsafe { dst_ptr.add(i).write_volatile(e) };
```
physmap_flags() is PRESENT|WRITABLE|GLOBAL|NX. References: CVE-2017-5754; Intel SDM Vol. 4, IA32_ARCH_CAPABILITIES (MSR 10AH) bit 0 RDCL_NO.

**Impact:** On Intel CPUs with RDCL_NO=0, including a KVM guest on such a host, a user process can read kernel and other processes' memory through rogue data cache load. QEMU TCG is unaffected today. Planned KASLR is also weak without KPTI.

**Fix:** Record the decision next to DESIGN §5.2. Option one: declare affected CPUs out of scope and log RDCL_NO at boot, reading MSR 0x10A only when CPUID.(EAX=7,ECX=0):EDX[29] is set and treating non-Intel CPUs as unaffected. Option two: name KPTI in ROADMAP §18.3, covering asm stubs that switch CR3 and copy the frame, a shadow user PML4, PCID, and GLOBAL only on entry mappings. Either way, fix the stale DESIGN §4.1 row.


#### F025 · Spectre v1 at the syscall boundary: syscall nr, fd, pid and fid are bounds-checked but not masked against speculation, and user GPRs stay live into Rust

**Severity:** MEDIUM · LATENT (real hardware or KVM on a speculative x86 CPU; non-root users) · **Confidence:** Likely

**Location:** src/proc_init.rs:419-451, src/proc.rs:138-205, src/proc_init.rs:97-121, src/proc_init.rs:511-517, src/proc_init.rs:1143-1192, src/file_init.rs:343-361, src/syscall_init.rs:42-72, docs/ROADMAP.md:1566

**What's wrong:** Indices that come from syscalls are bounds-checked with a plain compare-and-branch and then used without masking. The kernel has no index-masking helper. The only lfences are the ones before rdtsc (x86.rs:464) and for DMA ordering (dma.rs:201). `match nr` on the raw u64 compiles to a jump table indexed by the user's value. vibeos_syscall_entry calls into Rust while rbx, rbp, rsi, rdx and r8-r15 still hold user values. ROADMAP §18.3 covers this only as a generic item.

**Evidence:**
```rust
let i = fd as usize;
if i >= MAX_FDS || !self.slots[i].is_open() {
    None
} else {
    Some(self.slots[i])
```
The dev build compiles the dispatch to `cmpq $0x6e,%r14; ja; jmpq *table(,%r14,8)`. References: CVE-2017-5753; Linux array_index_nospec and do_syscall_x64.

**Impact:** On speculative hardware, a user process could steer out-of-bounds loads that only execute speculatively. The reach is limited. fd, pid and fid are zero-extended and index upward from .bss or a stack copy, so they cannot reach the physmap. Only the nr jump table can target arbitrary addresses, and that window is very short. No exploit has been shown, and TCG does not speculate.

**Fix:**
- Add a portable nospec_index(i, len) helper with a host test: cmp/sbb/and on x86_64, csel+csdb on aarch64.
- Apply it to nr before dispatch, and to fd, pid and fid at the cited sites.
- Zero the user GPRs after the pushes in vibeos_syscall_entry.
- State the masking rule in DESIGN §2.


#### F026 · CR0.NE, CR4.MCE and CR4.OSXMMEXCPT are never set on any CPU, so the #MF/#XM/#MC paths the docs claim cannot fire; PGE is set on APs only

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/arch/trampoline.S:37-40, src/arch/trampoline.S:53-55, src/syscall_init.rs:264-270, src/arch/cpu.rs:26-46, src/proc_init.rs:1262, docs/DESIGN.md:700-702, docs/ROADMAP.md:822

**What's wrong:** No code sets CR0.NE, CR4.MCE or CR4.OSXMMEXCPT, and `src/x86.rs` does not define them. These bits are 0 on the BSP and on every AP:
- APs start from the INIT state.
- The BSP enters from pinned Limine v9.6.7 (base revision 3) with CR0=0x80010011 and CR4=0x20.

The actual BSP/AP divergence is PGE, which the trampoline sets on APs only. DESIGN.md:700-702 and ROADMAP.md:822 claim SIGFPE on #MF/#XF and log-and-halt on #MC, but those paths cannot fire.

**Evidence:**
```asm
    /* PAE + PGE. GLOBAL kernel mappings match the BSP tables. */
    mov eax, cr4
    or eax, 0xA0
```
`init_fpu` touches only EM, TS, MP and OSFXSR. Intel SDM Vol. 3A §2.5 defines NE, MCE and OSXMMEXCPT, and Table 10-1 (formerly 9-1) gives the INIT state (CR0=60000010H, CR4=0). Limine PROTOCOL.md leaves other CR bits undefined below base revision 5.

**Impact:**
- **NE=0:** under QEMU TCG, an unmasked user x87 exception raises the masked IRQ13, so it is silently dropped with no SIGFPE.
- **MCE=0:** a machine check shuts the CPU down without a diagnostic (Likely).
- **OSXMMEXCPT=0:** on hardware or KVM, SSE exceptions become #UD, so the process gets SIGILL instead of SIGFPE.

**Fix:**
- In one per-CPU routine, set CR0.NE|MP, clear EM/TS, set CR4.MCE|OSFXSR|OSXMMEXCPT, and set PGE uniformly on every CPU.
- Assert these bits in the per-CPU CR ktest (ktest.rs:2372), and add a user x87 divide-by-zero test that expects SIGFPE.


#### F027 · now_ns counts BSP timer interrupts, so it permanently loses time whenever ticks coalesce and runs slightly slow in TSC-deadline mode

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/time.rs:138-157, src/time_init.rs:315-321, src/time_init.rs:335-343, src/apic_init.rs:460-485, src/apic.rs:336-339, src/ktest.rs:1128-1141, src/ktest.rs:1225-1245, docs/DESIGN.md:902

**What's wrong:** Monotonic time is `ticks × 1 ms`, with `ticks` incremented once per BSP timer interrupt. The TSC only interpolates since the last tick, and `monotonic_max` (a `fetch_max` on LAST_NS) hides the jumps. The periodic LAPIC timer, the PIT and TSC-deadline one-shots all collapse into a single pending IRR bit, so any BSP IF=0 window longer than 1 ms loses the extra periods for good. In TSC-deadline mode, which only runs on KVM or real hardware because TCG boots use periodic, `rearm_deadline` arms from a TSC read taken inside the ISR. Each period is therefore 1 ms plus latency, but it is counted as exactly 1 ms.

**Evidence:**
```rust
let ticks = st.ticks.fetch_add(1, Ordering::Relaxed).wrapping_add(1); // time_init.rs:319
let d = tsc_deadline_value(time_init::read_tsc(), k);                  // apic_init.rs:469
```
IF=0 windows over 1 ms are common: every syscall (FMASK_SYSCALL clears IF), the IRQ-off VFS `SpinMutex`, `Serial::write_fmt`, and ktest. ktest.rs:1662 already notes that ticks coalesce. Intel SDM Vol. 3A §11.5.4.1 says TSC-deadline is one-shot and software rearms it.

**Impact:** After an N ms IF-off window, `now_ns` freezes for about N-1 ms and then stays that far behind. TSC-deadline mode also drifts about 0.1-0.5% slow. Sleeps, timeouts, `wall_unix_s` and log timestamps all drift. The tests cannot catch this: test_uptime_sides compares two tick-derived values, and test_pit_tick_rate accepts 40-160 ms for an 80 ms wait. Separately, DESIGN §6.2 claims per-CPU calibration, but each per-CPU `tsc_per_ms` is a copy of the BSP's one global value.

**Fix:** Derive `now_ns` from `(rdtsc - tsc0) * 1e6 / tsc_per_ms` in u128 (DESIGN §6.4 option 2) and use the tick only for scheduling. Test that uptime tracks the TSC to within about 1% over 1 s, and correct DESIGN §6.2.


#### F028 · enable_lapic clears EXTD without checking the current mode; an x2APIC-mode firmware handoff takes #GP and halts boot

**Severity:** MEDIUM · LATENT (real hardware, ROADMAP §20.1) · **Confidence:** Confirmed

**Location:** src/apic.rs:187-192, src/apic_init.rs:120-128, src/apic_init.rs:658-666, src/apic.rs:512-520

**What's wrong:** `apic_base_msr` clears EXTD (bit 10) and sets EN, and `enable_lapic` writes the result to IA32_APIC_BASE without checking the current mode. If firmware handed off in x2APIC mode, that write is an illegal x2APIC→xAPIC transition. Missing x2APIC support is already tracked (ROADMAP §20.1, DESIGN §5.9). The new defect is the unchecked write: it faults instead of taking the legal disable-then-enable path or failing with a marker. The host test only feeds EXTD=0, so the mask is untested.

**Evidence:**
```rust
let flags = (flags & !APIC_BASE_X2APIC) | APIC_BASE_ENABLE; // apic.rs:190
let cur = x86::rdmsr(IA32_APIC_BASE);                       // apic_init.rs:126
let next = apic::apic_base_msr(cur, phys);
unsafe { x86::wrmsr(IA32_APIC_BASE, next) };
```
Triggered when firmware leaves EXTD=1 (e.g. an APIC ID ≥ 255); pinned Limine v9.6.7 only touches x2APIC for an MP request, which vibeOS does not send. Intel SDM Vol 3A §11.12.5 (§10.12.5 in older editions): a WRMSR from x2APIC mode to xAPIC mode raises #GP. The only legal exit from x2APIC mode is to disabled.

**Impact:** The IDT is live (main.rs:194) before `apic_init::init` (main.rs:221), so the BSP halts in `exception_halt(b"#GP")`. QEMU/SeaBIOS and small-SMP OVMF hand off in xAPIC mode, so they are unaffected today.

**Fix:** Handle EXTD=1 in `enable_lapic`, so APs get the fix too. If x2APIC is not locked and every MADT APIC ID is ≤ 254, write EN=0,EXTD=0 and then EN=1 (Linux `__x2apic_disable`). Otherwise emit a marker and return None, which already falls back to the PIT. Add a host test with EXTD=1 input.


#### F029 · Buddy deallocate walks every free list per frame; tearing down a large address space holds PT with IRQs off for seconds on multi-GiB RAM

**Severity:** MEDIUM · LATENT (multi-GiB RAM / real hardware) · **Confidence:** Confirmed

**Location:** src/pmm.rs:174-204, src/pmm.rs:351-376, src/addr_space_init.rs:20-25, src/addr_space_init.rs:64-69, src/ipi_init.rs:123-150

**What's wrong:** Each `deallocate` calls `covered_by_free_block`, which walks every free list from the freed order up to MAX_ORDER. On a legitimate free nothing matches, so each walk reaches the end of its list. The order-10 list alone has about (free RAM / 4 MiB) nodes. Teardown frees each user and page-table frame on its own, inside `with_pt`, which runs with IRQs off. The cost is therefore about frames × (free RAM / 4 MiB). The IRQ-off window is held by PT, not BUDDY. BUDDY is taken and released once per frame.

**Evidence:**
```rust
for k in (at_least_order as usize)..=MAX_ORDER {
    let block_start = phys & !((PAGE_SIZE << k) - 1);
    if unsafe { self.in_free_list(block_start, k as u8) } {
```
A host benchmark of pmm.rs, copied verbatim, freed a process in 9 ms on a 128 MiB pool, 2.1 s on a 2 GiB pool, and 7.1 s on an 8 GiB pool with only 256 MiB freed. The trigger is exit, exec or fork rollback of a process with a large bss.

**Impact:** At the default 128 MiB the cost is negligible. With multi-GiB RAM, a single teardown holds PT with IRQs off for seconds, which stalls every CPU that needs PT. A concurrent shootdown sender can also panic with "ipi: ack timeout" (Likely, because it depends on interleaving).

**Fix:** Make `deallocate` O(MAX_ORDER) with per-frame metadata (DESIGN §4.6 `struct Frame`) or a per-order bitmap. Re-acquiring BUDDY periodically would not help, because BUDDY is already released per frame. Any batching must drop PT and call `service_incoming()` between chunks.


#### F030 · dma32 only forbids crossing a 4 GiB boundary; it does not keep buffers below 4 GiB

**Severity:** MEDIUM · LATENT (Phase 20: Real Hardware, first 32-bit-only DMA device with >4 GiB RAM) · **Confidence:** Confirmed

**Location:** src/dma.rs:70, src/dma.rs:88-94, src/pmm.rs:243-276, src/pmm.rs:396-402, src/ktest.rs:3402, docs/DESIGN.md:632-634

**What's wrong:** `DmaAlloc::dma32` only sets `boundary = 1 << 32`, and `allocate_constrained` only rejects blocks that straddle a multiple of it. Nothing keeps the buffer below 4 GiB. With >4 GiB RAM, a dma32 buffer is usually above 4 GiB, because the LIFO free lists return the highest block first. The ktest's `phys < 4 GiB` assertion runs only at 128 MiB, and the host tests use pools at 0x400000, so neither can fail.

**Evidence:**
```rust
pub const fn dma32(size: u64) -> Self {
    Self { size, align: PAGE_SIZE, boundary: DMA32_BOUNDARY }
}
// pmm.rs:263
if ok_align && !crosses_boundary(phys, bytes, boundary) {
```
A block at 5 GiB does not straddle a 4 GiB multiple, so it is accepted. `make test-e2e-highmem` (9 GiB) already puts such frames in the buddy.

**Impact:** Nothing breaks today: the only users are modern virtio drivers, which program 64-bit queue addresses (virtio 1.x §4.1.4.3). The first 32-bit-limited device on a >4 GiB machine would DMA to a truncated address and corrupt unrelated memory. The EDU ktest's `as u32` address writes (src/ktest.rs:3476, 3490) already follow that pattern.

**Fix:** Give dma32 a real upper bound: a below-4 GiB buddy zone, or a free-list walk that finds a qualifying block. Adding `phys + bytes <= 1 << 32` to the 16-try loop is not enough, since every LIFO pop comes from high memory. Add a host test with a pool above 4 GiB, and correct DESIGN §4.7.


#### F031 · ELF loader treats AsError::Overlap as success, then zeroes a page shared by two PT_LOADs and wipes the earlier segment

**Severity:** MEDIUM · LATENT (multi-PT_LOAD ELFs whose segments share a page) · **Confidence:** Confirmed

**Location:** src/user_init.rs:93-115, src/addr_space.rs:176-179, src/elf.rs:171-175, src/elf.rs:267-273

**What's wrong:** `elf::parse` rejects PT_LOADs only when their bytes overlap, so two segments with disjoint bytes in the same 4 KiB page are accepted. `map_loads` rounds each segment out to whole pages and treats `AsError::Overlap` as success. It then zeroes the entire page-rounded range. If any page overlaps, `map_anon` rejects the whole request. None of the second segment is mapped, and the shared page keeps the first segment's permissions.

**Evidence:**
```rust
match unsafe { addr_space_init::map_anon(space, start, len, perms) } {
    Ok(()) | Err(AsError::Overlap) => {}
    Err(e) => return Err(LoadError::As(e)),
}
space.zero_bytes(start, len).map_err(LoadError::Mem)?;
```
The trigger is an ELF whose RX and RW PT_LOADs share a virtual page. Examples: lld with a minimal linker script that has no ALIGN between .text and .data (reproduced), `ld -n`, or a hand-built ELF passed to execve. Default lld/GNU ld layouts and today's single-PT_LOAD mkuserelf.py binaries are unaffected.

**Impact:** Suppose the second segment fits inside the shared page. The first segment's bytes get zeroed and .data is written into an RX page, so the program executes zeros or faults on its first store. If the second segment extends past the shared page, `zero_bytes` fails with Unmapped and exec rejects a valid ELF. Damage stays inside the exec'ing process.

**Fix:** Merge the page intervals shared by several PT_LOADs, using the union of their W/X flags. Map and zero each interval once, then write every segment's file bytes in a second pass. Alternatively, reject page sharing in `elf::parse` by comparing page-rounded ranges. Add a host test and a ktest for a page-sharing pair.


#### F032 · An AP that misses the 3 s bring-up timeout but keeps running uses its freed stack, GDT/TSS and PerCpu

**Severity:** MEDIUM · LATENT (real hardware / overcommitted hypervisors) · **Confidence:** Likely

**Location:** src/smp_init.rs:132-148, src/smp_init.rs:150-160, src/smp_init.rs:203-207, src/smp_init.rs:215-246, src/arch/trampoline.S:77-80, src/arch/gdt.rs:54-62, src/arch/gdt.rs:144-151, docs/DESIGN.md:1122, docs/DESIGN.md:1752-1753

**What's wrong:** On `wait_ready` timeout, `start_one` frees the AP's kernel stack, GDT/TSS and IST/RSP0 stacks, nulls PerCpu idle/current and marks the idle TCB Dead, with no INIT and no claim handshake. An AP that accepted a SIPI but has not stored `ready` within 3 s keeps running on those freed resources, or picks up the next AP's param-block RSP in the trampoline and the next AP's `STARTING` in `ap_entry`. `free_ap_resources` also never clears the ONLINE bit. DESIGN §7.4 step 6 and the §9.5 pitfall prescribe this free.

**Evidence:**
```rust
if !wait_ready(cpu_id) {
    crate::marker!("vibeOS: smp: apic {apic_id} timed out");
    free_ap_resources(a);
    return false;
}
```
The trigger is a live AP that stalls for more than 3 s after accepting a SIPI. An AP that never accepted one stays in wait-for-SIPI and is safe (Intel SDM Vol. 3A, MP initialization protocol).

**Impact:** Two CPUs share a stack, GDT/TSS or PerCpu, or the AP runs on unmapped stacks (the shootdown skips CPUs that are not yet online). `ltr` on a TSS the next AP already loaded raises #GP (Intel SDM Vol. 2A, LTR) before `ap_entry` loads the IDT, which likely triple-faults. Boot-time only; not reachable from userspace or devices.

**Fix:** On timeout, send INIT, clear the ONLINE bit, and deliberately leak that AP's stack, tables and idle TCB, then update DESIGN §7.4 and §9.5. A generation token only helps if the trampoline claims it with a CAS before loading RSP from the shared param block.


#### F033 · SIGCONT can be lost between apply_pending's Stop decision and wait_on(stop_wq), which never rechecks the state

**Severity:** MEDIUM · LATENT (user threads on more than one CPU, or preemptible syscalls) · **Confidence:** Confirmed

**Location:** src/proc_init.rs:453-502, src/proc_init.rs:497-500, src/proc_init.rs:1164-1170, src/proc_init.rs:1186-1194, src/thread_init.rs:747-751

**What's wrong:** `apply_pending` sets `state = Stopped` inside `with_table` and releases SCHED. It then calls `wait_on(stop_wq)`, which enqueues and schedules without rechecking the state. If a SIGCONT lands in that gap, `sys_kill` sets `Live` and calls `wake_queue` on a queue that is still empty. `sys_wait4` (proc_init.rs:1061-1076) avoids this lost wakeup by doing the check and `begin_wait` in one SCHED section. A separate defect is not covered here: signals are applied only at syscall entry and after wait4, so a process that never makes a syscall cannot be stopped or killed.

**Evidence:**
```rust
Pending::Stop => {
    let wq = unsafe { &mut (*TABLE.as_ptr()).procs[pid as usize].stop_wq };
    thread_init::wait_on(wq); // with_sched(begin_wait); schedule();
}
```
This cannot be reached today. All user threads are pinned to CPU 0 (`spawn_user` uses `Pinned(current_cpu())`), and syscalls run with IF=0, so nothing can run between the two SCHED sections. It becomes reachable once user threads run on several CPUs (DESIGN §7.8) or syscalls become preemptible.

**Impact:** The process stays blocked on stop_wq while `ps` reports it as Live. A second SIGCONT cannot wake it, because `was` is false. SIGKILL, a Term-class signal, or SIGSTOP followed by SIGCONT still recovers it through the `wake` branch. The impact is bounded to one process.

**Fix:** Mirror `sys_wait4`. Inside one `with_sched(|s| TABLE.with(...))`, check `state == Stopped` and call `s.begin_wait(&mut t.procs[pid].stop_wq, FAR_DEADLINE)`. Then schedule and loop. This also removes the unlocked `TABLE.as_ptr()` dereference at proc_init.rs:498.


#### F034 · Condvar::wait_until can strand the woken mutex waiter when preempted in with_sched's IF-on window

**Severity:** MEDIUM · LATENT (first non-test Condvar user: Phase 13 pipes/futex/threads) · **Confidence:** Confirmed

**Location:** src/sync_init.rs:515-534, src/thread_init.rs:661-676, src/x86.rs:299-305, docs/DESIGN.md:1672

**What's wrong:** `Condvar::wait_until` marks the caller Blocked and wakes the mutex waiter W inside one `with_sched` closure. `with_sched` delivers the recorded wakes (`place_ready`) only after dropping SCHED, and dropping the guard re-enables IF. If the caller is preempted in that window, it is switched out without being re-queued, and W is left Ready on no run queue, inbox or timeout queue until the caller resumes.

**Evidence:**
```rust
let (r, places, n) = {
    let mut s = SCHED.lock(); let r = f(&mut s);
    let n = s.place_n; let p = s.places; s.place_n = 0; (r, p, n)
};
while i < n { crate::ipi_init::place_ready(places[i].0, places[i].1); i += 1; }
```
An IRQ0 or 0xFD reschedule IPI that became pending during the closure fires right after the `sti` in `InterruptGuard::drop`.

**Impact:** With `Condvar::wait` (no deadline) and W as the only notifier, both threads hang permanently. With a deadline, W is only delayed. Only `kernel_tests` ktests use Condvar today, and none of them has a thread queued on the mutex when the waiter calls `wait()`, so production cannot hit this. DESIGN §9.4's "Condvar wait parks still holding the mutex" pitfall is only half fixed.

**Fix:** Take `InterruptGuard::enter()` at the top of `with_sched` and hold it through the `place_ready` loop (`place_ready`'s own guard nests). Add a ktest where the notifier is queued on the mutex when the waiter calls `wait()`.


#### F035 · A duplicate MADT APIC ID re-INITs a running AP and leaves a ghost CPU in the online mask, so the first TLB shootdown panics

**Severity:** MEDIUM · LATENT (real hardware: firmware MADT that repeats an enabled LAPIC ID) · **Confidence:** Confirmed

**Location:** src/acpi.rs:324-331, src/smp_init.rs:267-285, src/ipi_init.rs:157-172, src/ipi_init.rs:123-150

**What's wrong:** `parse_madt` appends every enabled type-0 LAPIC entry without checking whether its APIC ID was already seen. The SMP loop skips only the BSP's ID. If firmware lists ID X twice, the BSP sends INIT/SIPI to X a second time after logical CPU N is already running there. That CPU resets and comes back as logical M, but N stays in the online mask.

**Evidence:**
```rust
if flags & LAPIC_ENABLED != 0
    && let Some(slot) = info.apic_ids.get_mut(info.cpu_count)
{
    *slot = rec[3];
    info.cpu_count += 1;
```
ACPI 6.5 §5.2.12.2 requires one Processor Local APIC structure per processor. Intel SDM Vol. 3A §11.6.1 (ICR, INIT delivery mode) says an INIT resets the target CPU even while it runs.

**Impact:** No CPU acks N's shootdown bit any more, because acks come from `my_bit()`, which is now M's. The first TLB shootdown after bring-up therefore panics with "ipi: ack timeout" after 1 s. The stack free on the first thread exit is enough. Before that, round-robin placement assigns threads to N and none of them run. On such firmware this is effectively a boot failure. Under QEMU nothing happens.

**Fix:**
- In `parse_madt`, drop repeated IDs using a 256-bit seen-set and warn once.
- In `smp_init`, skip any APIC ID already assigned to a PerCpu.
- Also consider rejecting ID 0xFF, the xAPIC broadcast destination (Suspected).


#### F036 · RwLock writer that is woken, then hits its deadline after a reader barges, returns without waking parked readers

**Severity:** MEDIUM · LATENT (first non-test caller of `RwLock::write_until` with a finite deadline) · **Confidence:** Confirmed

**Location:** src/sync_init.rs:354-392 (early return at :364/:371, fixup only at :373-387), src/sync_init.rs:394-404, src/wait.rs:158-164, src/wait.rs:183-196

**What's wrong:** When a queued writer gives up, `after_writer_wait_timeout` wakes the readers parked behind it. That fixup runs only when `wait_resume()` returns `Timeout`. `drop_read` can instead wake the writer, which empties `write_wq`. A reader can then barge in before the writer runs. If the deadline has also passed, the writer takes the `past(d)` early return and skips the fixup. The barging reader's drop wakes only `write_wq`, so the readers parked in `read_wq` stay parked.

**Evidence:**
```rust
if st.try_write(me()) {
    return Ok(());
}
assert!(st.writer != me(), "rwlock: recursive write");
if past(d) {
    return Err(false);
}
```
The trigger is a writer's deadline expiring after `drop_read` has woken it but before it runs, while another reader acquires the lock. Timeouts are reaped only in `schedule_inner`, which widens that window.

**Impact:** This is a liveness bug, not a soundness hole: there is no UB and mutual exclusion still holds. Readers that called `read()` (FAR_DEADLINE) stay blocked until a later writer's drop or timeout wakes `read_wq`. If no writer ever comes, they block forever. It cannot be reached today, because RwLock has no users outside ktest and `write()` never times out.

**Fix:**
- Run the `after_writer_wait_timeout` wake logic before any `return Err(false)` that happens after a wait.
- Alternatively, have `RwLockReadGuard::drop` call `wake_all(read_wq)` when no writer holds the lock and none is queued.
- Add a host-model regression test for the woken-then-past-deadline sequence.


#### F037 · 64-slot thread table: boot panics at 31+ CPUs and fork panics instead of returning EAGAIN from 24 CPUs; adopt_ap_idle frees a Box under SCHED

**Severity:** MEDIUM · LATENT (24 or more CPUs: large VMs, real hardware, VIBEOS_SMP≥24) · **Confidence:** Confirmed

**Location:** src/thread.rs:12, src/thread_init.rs:585-590, src/thread_init.rs:458-491, src/ipi.rs:11, src/work_init.rs:79-87, src/proc_init.rs:821-870, src/acpi.rs:32, src/sync_init.rs:164-167

**What's wrong:** Boot uses 2N+3 of the 64 thread slots: bootstrap, N idle threads, N wq workers, irqth and blk-wb. When no slot is free, `spawn_inner` panics. `sys_fork` checks only the 16-entry process table before it calls the infallible `spawn_user`, so running out of threads panics the kernel instead of returning EAGAIN. Separately, `adopt_ap_idle` builds its `Box<Tcb>` before `with_sched`. On the no-slot path the Box is dropped inside the closure, which frees heap memory under SCHED and trips the lock-rank assert (HEAP rank 3 while SCHED rank 4 is held).

**Evidence:**
```rust
.position(|x| x.is_none())
.expect("thread table full");                 // spawn_inner, thread_init.rs:589-590
let slot = s.slots.iter().position(|x| x.is_none())?; // adopt_ap_idle:484, tcb moved in
```
With N=31 the panic hits at the blk-wb spawn, and with N=32 inside the wq loop. N=30 boots with the table full, so init's first fork panics. With 24 ≤ N ≤ 29, forking up to the process limit panics.

**Impact:** Boot panics with 31 or more CPUs. From 24 CPUs up, userspace can panic the kernel through fork. VIBEOS_SMP≥31 reproduces the boot panic today. The adopt_ap_idle path is reachable only at N=64, where it just changes the panic message.

**Fix:** Make `spawn_inner` fallible, so fork returns EAGAIN and per-CPU workers log and degrade. Cap AP bring-up so 2N+3 slots plus user headroom fit. Return an unplaced Box out of `with_sched` and drop it after the lock is released. Raising MAX_THREADS is blocked by the u64 wake-inbox assert (ipi.rs:11) until D1 lands.


#### F038 · SpinMutexGuard and BlockingMutexGuard are Sync when T is only Send, so safe code can share &T of a !Sync T across threads; InterruptGuard is Send

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/sync_init.rs:33-41, src/sync_init.rs:104-108, src/sync_init.rs:209-214, src/sync_init.rs:281-285, src/x86.rs:277-306

**What's wrong:** `SpinMutex<T>` and `BlockingMutex<T>` are `Sync` for any `T: Send`. Their guards derive `Send` and `Sync` automatically from their fields: a reference, integers, and `InterruptGuard { restore: bool }`. So `&SpinMutexGuard<Cell<u32>>` is `Send` and derefs to `&Cell<u32>` on several threads, which is a data race in safe code. std's `MutexGuard` is `!Send` and is `Sync` only when `T: Sync`; it closed the same hole in rust-lang/rust#41622. `InterruptGuard` is also auto-`Send`, so a different kernel thread can drop it.

**Evidence:**
```rust
unsafe impl<T: Send> Sync for SpinMutex<T> {}
pub struct SpinMutexGuard<'a, T> {
    mutex: &'a SpinMutex<T>, owner: usize, rank: u8, _irq: InterruptGuard,
}
```
The trigger is a guard leaked to `'static` and shared, or sent through `Channel`, which accepts it because it is `Send`. A rustc check on replicas of these exact struct shapes compiles. Only the std control fails the `Sync` bound.

**Impact:** This is a soundness hole in the core lock types. No current code shares or sends a guard, since spawn takes `fn()`. A foreign-thread `InterruptGuard` drop mostly ends in an `irq nest underflow` or lock-order panic, or leaves a thread stuck at IF=0. It does not silently alias. A drop on a different CPU by the same thread is intended, because `switch_now` swaps `irq_nest` per TCB.

**Fix:** Add `PhantomData<*const ()>` to `InterruptGuard`, which makes `SpinMutexGuard` `!Send` and `!Sync`. Then add `unsafe impl<T: Sync> Sync for SpinMutexGuard<'_, T>`. Use the same pattern for `BlockingMutexGuard`.


#### F039 · PerCpu hands out &'static PerCpu alongside &mut to the same slot; remote non-atomic reads race the owner CPU on SMP boot

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/per_cpu_init.rs:82-103, src/per_cpu_init.rs:109-156, src/cell.rs:36-37, src/diag.rs:60-81, src/main.rs:241-242, src/sched_init.rs:40-44, src/thread_init.rs:280-303, src/smp_init.rs:132-141, src/smp_init.rs:150-156, src/smp_init.rs:215-245, src/ktest.rs:1405-1418, src/shell_init.rs:259

**What's wrong:** `cpu(id)`, `current()` and `try_current()` return `&'static PerCpu`. PerCpu is not Sync, and it is stored in `BootCell`, whose `unsafe impl<T> Sync` has no `T: Sync` bound even though its comment requires one. Meanwhile `with_current`, `with_current_switch`, `with_cpu` (a safe `pub fn` that gives `&mut` to any CPU's slot) and `ap_entry` all create `&mut PerCpu` to the same memory. Other CPUs read non-atomic fields while the owner writes them. `with_current_switch` also keeps its `&mut` alive across `switch_context`, while the incoming thread and ISRs on that CPU take fresh `&mut`s.

**Evidence:**
```rust
// diag.rs:66-78, on the BSP
let Some(c) = per_cpu_init::cpu(i) ... c.ticks, c.switches, c.runq.len()
// sched_init.rs:41-42, in the AP timer ISR
per_cpu_init::with_current(|cpu| { cpu.ticks = cpu.ticks.wrapping_add(1);
```
main.rs:242 runs `diag::cpus()` right after the APs execute `sti` with their LAPIC timers armed (the default; PIT mode arms none). The shell `cpus` command, `wait_ready` and ktest repeat these remote reads.

**Impact:** These are data races and noalias violations, which are UB under the Rust memory model. The current x86 codegen keeps every per-CPU store before `switch_context` (checked in the built ELF), so nothing is miscompiled today. If a store to `current` or `slice_tsc` were moved past the switch, it would run when the outgoing thread resumes and overwrite newer values on the same CPU, breaking `current_thread()`. A late AP that overlaps `free_ap_resources` could also hit the WITH_BUSY re-entry panic.

**Fix:** Make the remotely read fields atomic and have `cpu(id)` return a view that exposes only those fields. End the closure's borrow before `switch_context`. Make `with_cpu` `unsafe`, or limit it to CPUs that have not started. Add a `T: Sync` bound to BootCell's Sync impl.


#### F040 · run_user calls a twice-returning setjmp from Rust, longjmp_user is a safe fn over global jump state, and run_user's contract omits the RSP0-stack precondition

**Severity:** MEDIUM · LATENT (second run_user caller: spawned thread or concurrent SMP session) · **Confidence:** Confirmed

**Location:** src/syscall_init.rs:430-448, src/syscall_init.rs:581-585, src/syscall_init.rs:603-634, src/syscall_init.rs:302-313, src/proc_init.rs:972-989, src/user_init.rs:269-279, src/arch/catch.rs:44-68, src/arch/catch.rs:172-193

**What's wrong:**
1. `run_user` calls `vibeos_user_setjmp` directly. rustc emits no returns_twice for it, so `if_on` and `t` survive the longjmp only because LLVM happens to keep them in callee-saved registers (rbx/r15 in debug, r12/r13 in release). `arch/catch.rs` avoids this by wrapping setjmp in asm (`vibeos_catch`).
2. `longjmp_user` is a safe fn that jumps to the global `USER_JMP`, which is zeroed or stale outside a session.
3. `USER_JMP`, `IN_USER`, `CURRENT_AS` and `EXIT_STATUS` are globals, and `finish_exit` longjmps whenever the global `in_user()` is set.
4. The Safety doc omits that the caller must not run on its own `kernel_rsp0` stack. Only the bootstrap thread, which uses `fallback_rsp0`, meets this.

**Evidence:**
```rust
let rc = unsafe { vibeos_user_setjmp(USER_JMP.as_ptr()) };          // :618
pub fn longjmp_user(status: i32) -> ! { ...
    unsafe { vibeos_user_longjmp(USER_JMP.as_ptr(), 1) }; }        // :581
```
`set_rsp0_for` points RSP0 at the TCB stack top for every spawned thread, and the safe `run_path` does not check which thread calls it. Today's callers (`boot_hello`, ktest) run one at a time on the bootstrap thread.

**Impact:** Kernel stack corruption, or two CPUs running on one stack, follows from any of: a spill-slot reuse after setjmp, `run_path` from a spawned thread, or a second concurrent session. The frames the longjmp skips hold no Drop guards today. catch.rs shares global state across CPUs too, but it is test-only (LOW).

**Fix:** Move setjmp into an asm trampoline like `vibeos_catch`. Make `longjmp_user` unsafe or private, and make the session state per-CPU or per-TCB. Assert `tcb.stack.is_none()` in `run_user`, or give the session a dedicated RSP0 stack.


#### F041 · 651 unsafe blocks carry one SAFETY comment and no lint enforces more, and several written safety claims are false

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/main.rs, src/lib.rs, src/per_cpu_init.rs:172, src/arch/gdt.rs:183-186, src/paging.rs:214-215, src/cell.rs:5, src/cell.rs:36-37, src/cell.rs:50-58, src/dev.rs:564-565, src/thread_init.rs:334-335, src/block_init.rs:72-73

**What's wrong:** `src` has 651 `unsafe {}` blocks and 149 unsafe fn/impl/extern items, but only two safety comments (thread_init.rs:131, dev.rs:564). `clippy::undocumented_unsafe_blocks` is not enabled, and `make check` runs clippy only on vibeos-core and hostlib. Several written claims are false:
- per_cpu_init.rs:172 says irq_nest_enter is a no-op if GS is 0, but `try_current` reads `gs:[0]` once LIVE is set.
- gdt.rs:183 says hardware updates RSP0. Software writes it, through a pointer derived from `&`.
- paging.rs:214 says `Mapper` is "Not `Sync`", but it is auto-`Sync`.
- cell.rs:5 says IrqCell is CPU-local, but TABLE, KVA, LOG, INITRD and IRQ are cross-CPU. This is still sound because of the owner CAS.
- cell.rs:36's "Caller puts a `Sync` `T`" is enforced by nothing.
- `BootCell::set` checks for a second set only with `debug_assert`.

**Evidence:**
```rust
/// InterruptGuard nesting. No-op before [`init_bsp`], or if GS is still 0.
pub fn irq_nest_enter() {
    if let Some(c) = try_current() {   // reads gs:[0]
```
In 64-bit mode the CPU only reads TSS.RSP0 (Intel SDM Vol. 3A, "Task Management in 64-bit Mode"). The thread_init.rs:131 comment holds for current callers; its `cookie(&self)` provenance is only a nit.

**Impact:** Nobody can tell which invariant a block relies on, and the wrong comments justify wrong code. Several items are defects in their own right and should be tracked separately: `unsafe impl<T> Sync for BootCell<T>` holds a !Sync PerCpu, DEFERRED stacks are drained by any CPU, and teardown leaves `mapper.root` dangling.

**Fix:** Enable `warn(clippy::undocumented_unsafe_blocks)` in main.rs and lib.rs. Write a SAFETY comment per block that names the invariant and who establishes it, and correct the claims above. Use `assert!` in `BootCell::set`, as DESIGN §9.4 requires.


#### F042 · Safe pub fns write through caller-supplied raw pointers or integer addresses (context switch, syscall entry, block submit, virtqueue, IDT)

**Severity:** MEDIUM · LATENT (Phase 12.5 unified page cache / async writeback) · **Confidence:** Confirmed

**Location:** src/syscall_init.rs:302-313, src/syscall_init.rs:331-357, src/per_cpu.rs:52, src/proc_init.rs:402-405, src/block.rs:590-619, src/block_init.rs:124-133, src/block_init.rs:232-271, src/virtio_blk_init.rs:208-231, src/virtio_blk_init.rs:1133-1191, src/virtio.rs:380-391, src/virtio.rs:578-584, src/arch/idt.rs:364-368, src/ktest.rs:3965-4015

**What's wrong:** Several safe `pub fn`s write through pointers that callers supply, and none has an `unsafe` contract:
- `switch_fpu`/`on_switch` (fxsave64/fxrstor64, `&*new`)
- `set_rsp0_for` (through the pub `PerCpu.tss` field)
- `proc_init::syscall` (`&mut *frame`)
- `RamDisk::apply` and the virtio bounce copies (through the pub `Seg.ptr: usize`)
- `SplitQueue::new` and its safe methods, and `write_indirect_write`

Both block `submit` fns keep the `&IoWaiter` address as a completion cookie after the borrow ends. `idt::set_handler` accepts error-code vectors for a no-error handler.

**Evidence:**
```rust
/// Async submit. `buf` must stay live until `w` completes. ...
pub fn submit(op: Op, lba: u64, nsect: u32, ptr: usize, len: usize, w: &IoWaiter)
    unsafe { &*(p as *const IoWaiter) }.finish(res); // complete_req
```
The compiler accepts a bogus `ptr`, or a return before the waiter completes. `test_block_vblk_deep` already returns with requests in flight on its failure paths. Vectors 8, 10-14, 17 and 21 push an error code (Intel SDM Vol. 3A §6.3 Table 6-1, §6.13), and so do 29 and 30 (AMD APM Vol. 2 §8.2). A no-error handler on one of them makes `iretq` pop a misaligned frame, which normally raises #GP.

**Impact:** Safe code can make the kernel write to arbitrary memory: copies into `ptr`, a wake into a dead stack frame, fxsave to any address, or a TSS write through `PerCpu::empty()` with a forged `tss`. Production callers pass valid pointers today. A separate race that affects correct IoWaiter users is its own entry.

**Fix:** Mark these `unsafe fn` with `# Safety` sections, or make `PerCpu.tss`, `Seg.ptr` and `Request.waiters` private. Give async block I/O owned buffers and a pinned or ref-counted waiter. Split `set_handler` into no-error and error-code variants.


#### F043 · Block Barrier/Flush contract not implemented for virtio-blk: fences only order dispatch, merges cross a second fence, seq wrap hides a fence

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/block.rs:7-14, src/block.rs:336-378, src/block.rs:427-471, src/virtio_blk_init.rs:299-302, src/virtio_blk_init.rs:489-527, src/virtio_blk_init.rs:564-579, docs/DESIGN.md:1857-1870, src/ktest.rs:3912-3920

**What's wrong:** block.rs and DESIGN §10.2 promise that a Barrier makes earlier requests complete before later ones start and that Flush covers prior writes, but `pick()` only orders dispatch:
- (a) virtio `issue()` completes a Barrier locally while earlier requests are still in flight.
- (b) Flush is sent once earlier writes are dispatched, not completed.
- (c) `try_merge` checks only the lowest queued fence, so a later write can merge across a second fence.
- (d) A fence with seq u32::MAX looks like the "no fence" sentinel. C-LOOK also reorders overlapping writes regardless of seq.

**Evidence:**
```rust
Op::Barrier => return Issued::Local(req, Ok(())),        // virtio_blk_init.rs:301
let after_fence = fence == u32::MAX || slot.seq > fence; // block.rs:365
```
Host repro on the real `Queue`: submitting F1, W(100), F2, W(101) dispatches `Flush 0; Write lba 100 nsect 2; Flush 2`. virtio v1.2 §5.2.6.2: a write is stable only once a FLUSH has been sent after the write completed.

**Impact:** The crash-consistency guarantees VIBEFS.md and DESIGN §10.2 rely on do not hold. All production callers block and nothing issues Barrier, so only (b) is live today: blk-wb writeback racing an FS flush, which matters only if the host crashes under QEMU. (a), (c) and (d) wait on async submit, a journaling FS, or more than 16 concurrent submitters.

**Fix:** Dispatch a fence only when nothing is in flight and hold later requests until it completes, or narrow the contract to Linux semantics and make `cache_init::flush` wait for in-flight writeback. Refuse merges across any fence, use a u64 seq, and never let `pick()` overtake or co-issue an older overlapping write (virtio v1.2 §6: in-order completion only with VIRTIO_F_IN_ORDER). Add host tests; the ktest only checks that `barrier()` returns Ok.


#### F044 · Console write() does a full-framebuffer memmove per newline with IRQs off, so userspace can stall CPU 0 without bound

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/fb.rs:125-134, src/fb_init.rs:171-213, src/fb_init.rs:219-243, src/console_init.rs:53-60, src/proc_init.rs:526-571, src/x86.rs:106, src/ipi_init.rs:123-150

**What's wrong:** Once the cursor is on the last row, every '\n' and every line wrap scrolls. Each scroll `ptr::copy`s (rows−2)·8·pitch bytes (about 3–4 MB, read back from the framebuffer) and clears a row, all under the IRQ-off FB SpinMutex. `sys_write` pushes the whole user buffer through this in 256-byte chunks and never opens an IF-on window. Syscalls enter with IF cleared (FMASK_SYSCALL 0x47700 includes IF), and nothing on the path executes `sti`.

**Evidence:**
```rust
CellAction::Scroll => self.scroll(),        // fb_init.rs:224, once per newline
ptr::copy(self.base.wrapping_add(src) as *const u8,
          self.base.wrapping_add(dst) as *mut u8, len as usize);
```
`write(1, buf, N)` with N newlines runs about N full-framebuffer memmoves with IF=0. 256 newlines copy roughly 0.8–1 GB. Triggering it needs a custom ring-3 binary, because the shipped sh and ps cannot emit enough newlines.

**Impact:** CPU 0 runs every user process, and it stays unpreemptible for seconds to minutes. The writer cannot be killed, IPIs are delayed and keyboard input is lost. The clock does not drift, because timekeeping is TSC-based. The 1 s `wait_acks` panic (ipi_init.rs:143) cannot be reached from ring 3 today, since all user threads are pinned to CPU 0. Once user threads run on APs, it becomes a user-triggerable panic (HIGH).

**Fix:** Coalesce scrolls so a write does at most one memmove of min(n, rows) rows, or keep a RAM shadow buffer and repaint dirty rows so VRAM is never read. Open an IF-on window between chunks in `sys_write`. Making all syscall bodies IF-on is a bigger, separate change, because the exit stub reuses per-CPU scratch.


#### F045 · ECAM address is computed relative to MCFG start_bus, but the MCFG base corresponds to bus 0; the unit test locks in the wrong formula

**Severity:** MEDIUM · LATENT (real hardware) · **Confidence:** Confirmed

**Location:** src/pci.rs:18-41, src/pci.rs:874-877, src/pci_init.rs:106-122, src/pci_init.rs:223-235, src/acpi.rs:461-477, docs/DESIGN.md:1581-1582

**What's wrong:** `ecam_phys` returns `base + ecam_off(bus - start_bus, ...)`, but the MCFG base address corresponds to bus 0 even when start_bus is nonzero. The doc comment at pci.rs:19 gives the same wrong formula and the unit test asserts it, while DESIGN §9 has the correct `base + (bus<<20)|...`. `parse_mcfg` also reads only the first allocation entry, and `pci_init::init` throws away its segment.

**Evidence:**
```rust
Some(base.wrapping_add(ecam_off(bus - start_bus, dev, func, offset))) // pci.rs:40
// pci.rs:874-877
ecam_phys(0xB000_0000, 1, 3, 2, 0, 1, 4) == Some(0xB000_0000 + ecam_off(1, 0, 1, 4))
```
This triggers on firmware whose first MCFG entry has start_bus ≠ 0 or segment ≠ 0. PCI Firmware Spec 3.2 §4.1.2 (cited in Linux Documentation/PCI/acpi-info.rst) says the base always corresponds to bus 0, and Linux's `pci_dev_base` uses the absolute bus number.

**Impact:** With start_bus = S, an access to bus B goes to base + (B-S)<<20. That is another bus's function, or, when B-S < S, an address below the ECAM range the firmware reserved. BAR-sizing writes, COMMAND bus-master enables and MSI-X programming all land there during the scan. Buses covered by later MCFG entries are silently never enumerated, because reads return all-ones. QEMU q35 has one entry with segment 0 and start_bus 0, so nothing goes wrong today.

**Fix:** Compute `base + ((bus << 20) | (dev << 15) | (fn << 12) | off)` and keep the [start, end] range check. Fix the doc comment and the test. Parse every MCFG allocation, keyed by (segment, bus range).


#### F046 · virtio-blk fails the whole device when one request exhausts its retries, and without F_RO a read-only vda is failed on its first write

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/virtio_blk_init.rs:564-591, src/virtio_blk_init.rs:544-562, src/virtio_blk.rs:22-32, src/part_init.rs:173-176, src/part_init.rs:216, src/part_init.rs:445-446, src/virtio_blk_init.rs:149-155, src/virtio_blk_init.rs:521-524

**What's wrong:** When one request uses up its 3 retries on a retryable error, `finish()` calls `fail_rest()`. That sets STATE=Failed for the whole device, and every read and write then fails until reboot, because virtio-blk has no reset path. `OFFER` deliberately leaves out F_RO, so on a read-only backend every write returns IOERR and ends in this path. At boot, an unpartitioned vda (512-byte sectors, at least 1024 sectors) gets a GPT stamp write, which kills it. A lesser, theoretical issue (LOW): `clamp_qsize` accepts queue sizes of 1 or 2, but read and write chains need 3 descriptors, so `issue` returns Full with nothing in flight and all I/O hangs.

**Evidence:**
```rust
/// Transport + blk features we will accept. Never [`F_RO`].   // virtio_blk.rs:22
Err(e) if e.retryable() => {
    block_init::complete_waiters(&req, Err(BlockError::Failed));
    fail_rest();                                                // virtio_blk_init.rs:585-588
}
```
This triggers on `-drive readonly=on` with an unpartitioned image, or on any persistent media error. Virtio v1.2 §5.2.6.1: the driver SHOULD accept F_RO. §5.2.6.2: the device MUST fail writes with IOERR when F_RO is offered.

**Impact:** A read-only vda becomes unusable, reads included. On real hardware, one bad sector takes out the whole disk. ROADMAP §7.1 asked for this device-wide policy on purpose, but Linux accepts F_RO and fails only the affected request. In-flight requests still complete after Failed, so a transient stall does recover.

**Fix:**
- Negotiate F_RO, return a read-only error for writes, and skip the GPT stamp on RO disks.
- After retries run out, fail only the request unless the device sets DEVICE_NEEDS_RESET.
- Reject qsize < 3 at probe.


#### F047 · virtio-blk doorbell always writes 0 instead of the virtqueue index, so multi-queue notifications name queue 0

**Severity:** MEDIUM · LATENT (real hardware / non-QEMU virtio) · **Confidence:** Confirmed

**Location:** src/virtio_blk_init.rs:286-291, src/virtio_blk_init.rs:536-540

**What's wrong:** The virtio-blk `kick()` writes 0 to every queue's notify address. With F_MQ negotiated and more than one CPU online, the driver sets up to 8 queues and `pick_vq` routes I/O to queue `cpu_id % nq`, so every notification for queues 1..nq-1 names queue 0. The spec requires the queue index whatever the address layout. A device can share one notify address across queues with multiplier 0 or with equal queue_notify_off values. The rng kick (virtio_init.rs:220-225) is correct, since rng uses only queue 0.

**Evidence:**
```rust
fn kick(doorbell: u64) {
    dma::dma_wmb();
    unsafe { core::ptr::write_volatile(doorbell as *mut u16, 0u16); }
}
```
F_NOTIFICATION_DATA is not offered, so virtio v1.2 §4.1.5.2 applies: the driver writes "the 16-bit virtqueue index of this virtqueue" to Queue Notify.

**Impact:** No effect on QEMU, whose MMIO notify handler takes the queue from the address and ignores the value. On devices that read the value (shared doorbell, vDPA, hardware virtio), requests on queues other than 0 are never processed and their waiters hang.

**Fix:** Store the queue index in `Vq`, or pass pump()'s loop index into `kick()`, and write `qi as u16`.


#### F048 · virtio transport trusts device-supplied used-ring ids, used.idx and notify/config capability bounds

**Severity:** MEDIUM · LATENT (untrusted devices / IOMMU) · **Confidence:** Confirmed

**Location:** src/virtio.rs:535-549, src/virtio.rs:480-496, src/virtio.rs:427-432, src/virtio.rs:239-251, src/virtio_blk_init.rs:478-480, src/virtio_blk_init.rs:602-631, src/virtio_init.rs:196, src/virtio_init.rs:104-114, src/virtio_blk_init.rs:137-147

**What's wrong:** 
1. `get_used()` calls `free_chain(id)` on the device's used id before anything checks it is below qsize and a live chain head. `free_chain` follows flags and next pointers from memory the device can see. `push_free` stores the unmasked id as `free_head` and uses `saturating_add` on `num_free`.
2. Harvest loops run with IRQs off under the BLK or rng lock, and `pending()` rereads `used.idx` on every pass, so a device that keeps advancing idx holds the loop with no bound.
3. `region()` checks only `cap.offset < BAR size`. `notify_addr` skips its bound entirely when `cap.length == 0` and ignores the 2-byte write width.

The ignored used `len` is not a defect: the 0xFF status sentinel already fails truncated completions.

**Evidence:**
```rust
let id = load_u32(self.base, off) as u16;
...
self.free_chain(id); // virtio.rs:541-544; blk validates only later, at 605-615
```
This triggers when a device posts a duplicate, non-head or out-of-range used id, or advertises a notify cap shorter than queue_notify_off × multiplier + 2.

**Impact:** None with conforming QEMU devices: virtio v1.2 §2.7.5.1 forbids device writes to the descriptor table, and §4.1.4.4.1 bounds cap.length. A buggy device could cause any of these:
- The same descriptor handed out twice, mixing data between I/O requests.
- A head ≥ qsize that the inflight guard skips, so its request hangs forever.
- An IRQ-off livelock.

A bad notify cap becomes a CPU store at a device-chosen physmap address, and an IOMMU would not contain that.

**Fix:** Keep a driver-private shadow (desc_state per head, with chain length). Reject ids ≥ qsize or non-head ids before freeing, and mark the device broken. Cap each harvest pass at qsize. Require offset + access length ≤ BAR size, cap.length ≥ 2, and delta + 2 ≤ cap.length.


#### F049 · vibefs commit writes the alloc map before applying its drops, and mount trusts that map, so every session that commits leaks its last-replaced blocks on disk

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/vibefs.rs:1903-1909, src/vibefs.rs:1916-1938, src/vibefs.rs:820-830, src/vibefs.rs:2077-2079, src/vibefs.rs:864-876

**What's wrong:** commit writes refc and the bitmap into the new alloc block before it decrements refc for old_meta and the drop list. Those decrements happen only in memory and reach disk only if another commit follows. mount loads refc unchanged through load_alloc and never rebuilds it from what is reachable. After a remount or a crash, the blocks replaced by the session's last commit therefore keep refc 1 with nothing pointing at them. fsck only checks and cannot repair (the repair mode described in VIBEFS.md §11 does not exist), so the leak is permanent.

**Evidence:**
```rust
write_alloc_into(self, &mut sbuf);
d.write_block(alloc_bno, &sbuf)?;
...
d.write_block(slot as u32, &sbuf)?;
d.flush()?;
// drop replaced committed meta   <- in memory only
```
Host probe on a 64-block image, with one 300-byte overwrite and one sync per mount session: each session lost about 4 blocks (the previous alloc block, inode leaf and dir leaf, plus the replaced data block). fsck warnings kept rising while errors stayed at 0, and NoSpace arrived after 12-14 sessions.

**Impact:** This leaks space only on disk-backed vibefs volumes and ends in a clean NoSpace; no data is corrupted. Today it can only be reached through the kernel-shell `mount vibefs vda|ram0` followed by sync or umount. The /vibe volume is formatted fresh at every boot and never remounted, so it is not affected. Sessions that never commit leak nothing.

**Fix:** Write a copy of refc and the bitmap that already has this commit's old_meta and drop decrements applied. This is crash-safe, because the old super still names the old alloc block. Keep the in-memory release after the super flush. A mount-time refcount rebuild would also repair images that have already leaked, but it must walk the snapshot trees as well. Update VIBEFS.md §10 step 7 to match.


#### F050 · vibefs commit advances the generation and roots in memory before the super is on disk; after a failed commit, the retry overwrites the slot holding the only valid super

**Severity:** MEDIUM · LATENT (real hardware I/O errors) · **Confidence:** Confirmed

**Location:** src/vibefs.rs:1903-1914, src/vibefs.rs:1916-1940, src/vibefs.rs:784-804, src/vibefs.rs:671, src/vibefs.rs:2058

**What's wrong:** commit assigns generation, inode_root and alloc_root before the alloc write, the first flush and the super write, and it picks the super slot by parity. If any of those three steps fails, commit returns Err with memory one generation ahead and nothing rolled back. The retry is generation G+2, and parity sends it to the slot that holds the only valid super, G. That breaks VIBEFS.md §10 step 5 ("Write the inactive super slot"). If only the second flush fails, the result is harmless apart from leaked blocks.

**Evidence:**
```rust
self.generation = self.generation.saturating_add(1);
self.inode_root = iroot;
self.alloc_root = alloc_bno;
...
let slot = (self.generation % 2) as u8;
```
Host probe with a disk set to fail: after the gen-3 commit failed, the retry wrote gen 4 over the live gen 2, leaving slots 0 and 1 holding gens 4 and 1. Tearing slot 0 made mount succeed on gen 1. The failed commit had reused gen 1's already-freed inode and alloc blocks, and mount never compares a meta block's header generation with the super's.

**Impact:** An I/O error during one sync, followed by a torn super write on the next commit, silently mounts a mix of old and uncommitted state: files a sync reported as durable are lost, unsynced files appear, or the alloc map misses reachable blocks. Blocks allocated by the failed commit also leak permanently. Today only the device backend (vda/ram0 through cache_init, mounted from the kernel shell) can fail; /vibe's memory backend cannot.

**Fix:** Compute the new generation and roots in local variables and assign them to self only after the super flush succeeds. Write to the slot opposite the live super on disk. On error, release the blocks this transaction allocated. Optionally, have mount reject meta blocks whose header generation is newer than the super's.


#### F051 · vibefs write leaks its newly allocated block when add_extent fails, so any process can fill /vibe by retrying a write past 16 KiB

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/vibefs.rs:1427-1434, src/vibefs.rs:1351-1362, src/vibefs.rs:842-876, src/vibefs.rs:1416-1426, src/vibefs.rs:1572

**What's wrong:** For an unmapped block, Vol::write allocates a block, writes it and computes its CRC before calling add_extent. add_extent returns NoSpace once the inode already has MAX_EXT = 4 extents. The allocated block (refc 1, in txn) is then never released. Kernel writes only create 1-block extents and add_extent never merges them, so every /vibe file hits this limit at 16 KiB, and each retry leaks another block. A multi-chunk write that fails part-way also returns Err without updating the size for the chunks already written.

**Evidence:**
```rust
let newp = self.alloc_block()?;
d.write_block(newp, &self.iobuf)?;
let crc = self.extent_crc(d, newp, 1)?;
let is = self.inode_slot(ino)?;
self.add_extent(is, fblk, newp, 1, crc)?;
```
Host probe on a 64-block image: 10 failing writes at offset 16384 took free space from 229376 to 188416 bytes. With enough retries free space reaches 0, and later writes and commits fail with NoSpace.

**Impact:** Any process can fill /vibe (about 60 free blocks) with open followed by repeated writes past 16 KiB. No inode references the leaked blocks, so the space stays lost until reboot; unlinking the file does not free them. On a disk-backed vibefs volume the leak becomes permanent at the next commit. The other leak paths are latent, because none of their triggers can happen on /vibe today:
- the overwrite path at 1416-1426 needs a device I/O error;
- pending_drop needs more than 96 drops in one transaction;
- split_replace_extent needs crafted multi-block extents;
- the ignored free_inode_data error in truncate (`let _`) needs an ftruncate syscall.

**Fix:** Check extent-slot capacity, including what a split needs, before calling alloc_block. On any error after alloc_block, call pending_drop(newp), which frees a txn block immediately. Return a short count with the size updated for completed chunks, and propagate free_inode_data errors.


#### F052 · A failed FAT extend leaks the clusters it already allocated: one sparse write can use up every free cluster, and ENOSPC is reported as EMFILE

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/fat.rs:1133-1138, src/fat.rs:1155-1162, src/fat.rs:453-458, src/fat_init.rs:273-283, src/proc_init.rs:152

**What's wrong:** `ensure_size` allocates and links clusters one at a time. When the volume runs out, it returns `NoSpace` from inside the loop and does not free the clusters this call already allocated. If the file was empty, `fat_init::write` then throws away the new `first`/`size`, so the dirent keeps clu 0 and nothing points to the partial chain. If the file was not empty, the extra clusters stay linked past its recorded size, and unlink or truncate can get them back. Separately, `fs_errno` maps `FsError::NoSpace` to `EMFILE`, and the code defines no `ENOSPC` constant (see also FS-12).

**Evidence:**
```rust
while have < need {
    let n = self.alloc_clu(d, clu)?;   // Err(NoSpace) returns here, no rollback
    self.zero_cluster(d, n)?;
    self.fat_set(d, clu, n)?;
    self.fat_set(d, n, EOC_MIN)?;
```
A write far past EOF on an empty file triggers it. In a host probe on the initrd image, free clusters went 89 → 0 while the dirent stayed at clu 0, size 0. Unlink did not recover the clusters, and the leak was still there after sync and remount.

**Impact:** One failed write from any process can leave the in-RAM root FS full until reboot. On a block-backed FAT mount the orphaned chain stays until fsck. Callers see "too many open files" instead of "no space".

**Fix:** Before allocating, check `self.free` against the number of clusters needed. Otherwise, on error, free the clusters allocated in this call and restore the old EOC and `*first`. Give FILES-table exhaustion its own error, add `ENOSPC` (28) to src/syscall.rs and docs/SYSCALL.md, and map `NoSpace` to it.


#### F053 · FAT dir_reserve grows the directory by a cluster on every create or rename that needs an LFN (any lowercase or long name)

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/fat.rs:944-996, src/fat.rs:961-975, src/fat.rs:982-994, src/fat.rs:539-546, src/fat.rs:646, src/fat.rs:1888

**What's wrong:** `dir_reserve` counts the first 0x00 entry as a single free slot and stops scanning there. A name that needs an LFN needs at least 2 slots, and `as_pure_83` rejects every lowercase name. So `run < slots` holds, and each such create adds a zeroed cluster to the directory, even though the entries are still written at `run_off` in the old cluster. Under the FAT spec, every entry after a 0x00 is free, so the extension is almost never needed. `rename` goes through the same path (fat.rs:646).

**Evidence:**
```rust
off += ENT as u32;
if ent[0] == ENT_FREE { break; }
...
// extend directory
let new = self.alloc_clu(d, last)?;
```
In a host probe on the 64 KiB initrd, each lowercase create used one cluster (89 → 83 free after 6 creates), and create #89 returned NoSpace. Uppercase 8.3 names used none. Microsoft FAT32 File System Specification v1.03, "FAT 32 Byte Directory Entry Structure": DIR_Name[0]==0x00 marks the entry and every entry after it as free.

**Impact:** Any process fills the root FS by creating about 89 files with lowercase names. Each create uses only 64-192 B of a 512 B cluster, and longer directory chains make scans slower. On block-backed FAT mounts the lost space is written to the device. On the RAM initrd it lasts until reboot.

**Fix:** When the scan reaches 0x00, count the rest of the chain as free, and return `run_off` if `run` plus the remaining entries is at least `slots`. Extend only when that tail really is too short, and move the `have + cb < need` check before the allocation. Add a host test that `free_bytes()` does not change across several lowercase creates and a rename.


#### F054 · FAT long names do not round-trip non-ASCII bytes; open(O_CREAT) of a UTF-8 name fails with ENOENT, leaves duplicate dirents, and '?' aliases names

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/fat.rs:1775-1777, src/fat.rs:1779-1812, src/fat.rs:1814-1847, src/fat.rs:813-829, src/fat.rs:397, src/file_init.rs:447-476

**What's wrong:** `fill_lfn` stores each UTF-8 byte as a separate UTF-16 unit, and `take_lfn` decodes every unit ≥ 0x80 as '?'. `lookup` compares against the decoded LFN, so it never matches a non-ASCII name that was just created. `file_init::open` with O_CREAT therefore creates the entry, walks the path again, and returns ENOENT, and each retry adds another dirent set under a new ~N short name. `check_name` also accepts '?' and the other characters the spec forbids in LFNs. As a result, the ASCII name "caf??" opens the file created as "café", and "ü1"/"é1" resolve to the same entry.

**Evidence:**
```rust
chars[i] = name[p] as u16;                          // fill_lfn, :1828
out[at] = if ch < 0x80 { ch as u8 } else { b'?' };  // take_lfn, :1803
```
In a host probe, create("café") returned Ok three times, every lookup returned NotFound, and readdir listed three "caf??" entries. Microsoft FAT32 spec v1.03, "FAT Long Directory Entries" and "Name Limits and Character Sets": long names are UCS-2/UTF-16, and `" * : < > ? \ |` are illegal in them.

**Impact:** A valid open(O_CREAT) fails with ENOENT and leaves duplicate dirents behind. These use directory slots, and the directory grows by a cluster once the slots are full. A kernel-internal mkdir of such a name leaks a cluster to a directory nothing can reach. Other OSes read the names vibeOS writes as mojibake, and here their non-ASCII names are unreachable or ambiguous.

**Fix:** Encode UTF-8 to UTF-16 in `fill_lfn`, with `utf16_len` counting code units, and decode UTF-16 back to UTF-8 in `take_lfn`. Until that lands, reject non-ASCII bytes in `check_name`. Always reject the spec-illegal LFN characters, '?' included.


#### F055 · Open-file entries shared through fork/dup are copied out and written back whole, with no lock held across the I/O

**Severity:** MEDIUM · LATENT (SMP userspace / Phase 13 threads) · **Confidence:** Confirmed

**Location:** src/file_init.rs:343-361, src/file_init.rs:549-615, src/file_init.rs:447-470, src/proc_init.rs:278-298, src/proc_init.rs:741-790

**What's wrong:** `read`, `write` and `seek` copy the `OpenFile` out of FILES and drop the lock for the I/O. `put_file` then writes the whole struct back and checks only `used`. fork, dup and dup2 share one fid through `addref`, so once two holders can interleave inside that window, one overwrites the other's offset, size, first-cluster and `refs` updates. The O_CREAT walk, parent lookup and create in `open` are also separate critical sections.

**Evidence:**
```rust
g[i] = f;          // put_file: whole snapshot, refs included
g[i].used = true;
```
This cannot happen today. User threads are pinned to the CPU that creates them (thread_init.rs:433-437), which is CPU 0. Syscalls run with IF clear (`FMASK_SYSCALL = 0x47700`), and the default FAT/vibefs volumes never block. Blocking device mounts exist only in kernel-shell builds, and those do not start userspace.

**Impact:** It becomes real once userspace runs on several CPUs, syscalls can sleep, or threads land. Then: writes and offsets are lost through an inherited fd; two first-cluster allocations on an empty FAT file orphan one chain; and racing non-O_EXCL creates fail with EEXIST. A lost `refs` update can free a slot that is still in use, so a stale fid would point at another file.

**Fix:** Hold a per-open-file or per-inode sleeping lock across each operation, or keep offset and size in the shared in-core inode (FS-01) under the volume lock. Never write `refs` back from a snapshot. Add a fid generation that `put_file` checks, and treat Exists as success on the non-O_EXCL create path.


#### F056 · Syscall paths are routed by raw byte prefix before normalization, and kernfs is unreachable from syscalls; the FAT directories under mount points are reachable and writable

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/file_init.rs:160-201, src/file_init.rs:281-324, src/fat_init.rs:359-390, src/vibefs_init.rs:347-380, src/fat.rs:341-380, src/vibefs.rs:977-1008, src/fs_init.rs:46-65, src/fs/mod.rs:1308-1316

**What's wrong:** `walk_abs` and `vol_parent` pick a volume by byte-prefix match on the raw joined path. Normalization happens only later, inside `FatVol::walk`, which skips `//` and `.`, resolves `..` lexically and compares case-insensitively. So `/./vibe/f`, `/dev/../vibe/f`, `//vibe/f` and `/VIBE/f` route to the FAT initrd and reach the FAT `vibe` directory hidden under the vibefs mount. Meanwhile vibefs rejects `/vibe/./f`, `/vibe//f` and `/vibe/..` with EINVAL. kernfs can only be reached through the VFS, and no file syscall goes through the VFS, so to userspace /dev, /proc, /tmp and /sys are plain FAT directories.

**Evidence:**
```rust
if (path == p || (path.len() > n && path[..n] == p[..] && path[n] == b'/')) && n >= best
```
From ring 3, open("/dev/null") returns ENOENT. With O_CREAT it creates a regular initrd file that receives every write. The kernel shell's `ls /dev` goes through `vfs_ls_snap`, lists kernfs null/zero/random, and never shows that file.

**Impact:** One logical path names two different files, data can land in a shadowed directory, and redirecting output to /dev/null fills the root FS. The devfs that ROADMAP §8.4 marks done (docs/ROADMAP.md:723) does not exist for userspace.

**Fix:** Canonicalize absolute paths first (collapse `//` and `.`, resolve `..`, clamp at root). Then route through the VFS mount table as the single source of truth, per docs/reviews/issues/A3-vfs-single-dispatch.md and the Phase 10 gate (docs/ROADMAP.md:852). Until then, refuse O_CREAT under kernfs mounts. Add ktests for `/./vibe/f`, `/VIBE/f` and `/dev/null`.


#### F057 · File API state is system-wide: one 16-entry open-file table and one CWD for all processes; disk-full is reported as EMFILE and O_TRUNC runs before slot allocation

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/file_init.rs:26, src/file_init.rs:128-129, src/file_init.rs:131-188, src/file_init.rs:326-341, src/file_init.rs:447-508, src/proc_init.rs:145-158, src/user_init.rs:75

**What's wrong:** FILES is a single `[OpenFile; 16]` shared by every process, with no per-process quota. A process that closes its stdio, or dup2s over it, can hold all 16 slots. CWD is one kernel-global `IrqCell`, and only the debug shell's `cd` changes it. `Proc::cwd` is copied on fork but never used, and there is no chdir syscall. `alloc_fid` and volume exhaustion both return `FsError::NoSpace`, which `fs_errno` maps to EMFILE, contradicting docs/SYSCALL.md:60. `open` also does O_CREAT/O_TRUNC before it calls `alloc_fid` (:508).

**Evidence:**
```rust
const MAX_OPEN: usize = 16;
static FILES: SpinMutex<[OpenFile; MAX_OPEN]> = ...;
FsError::NoSpace => EMFILE,   // proc_init.rs:152
```
execve needs a fid (`read_path`, user_init.rs:75), so while the table is full, exec fails for every process.

**Impact:** A single process holding 16 files makes every other open() and execve fail. Once non-root users exist, that is an unprivileged DoS. Relative paths in user programs resolve against the shell's cwd. A full disk is reported as "too many open files", and open(O_TRUNC) with a full table truncates the file and then fails.

**Fix:** Size the file table from a `limits` cap with per-process limits (D1, docs/ROADMAP.md:894). Resolve relative paths against `Proc::cwd` and add chdir/getcwd (docs/ROADMAP.md:1165). Return ENFILE (23) when the table is full and ENOSPC (28) when allocation fails, adding both to src/syscall.rs. Reserve the fid and fd before any create or truncate.


#### F058 · FAT block-device mount overflows the 16 KiB kernel thread stack: `mount fat32` always faults on the guard page

**Severity:** MEDIUM · LATENT (disk-backed FAT mount outside the kernel_shell REPL; HIGH once userspace I/O reaches a vda FAT) · **Confidence:** Confirmed

**Location:** src/fat_init.rs:455-516, src/fat_init.rs:472, src/fat_init.rs:490, src/fat.rs:249-258, src/kva.rs:18, src/thread_init.rs:528, src/file_init.rs:1465, src/shell_init.rs:66-69

**What's wrong:** `fat_init::mount_dev` moves `FatVol` by value: first the return of `FatVol::mount`, then an `Option` temporary. `FatVol` is 6,500 B (8 × 520 B FAT cache sectors plus 96 × 24 B ino entries). The frame of `mount_dev` alone is about 26 KiB in the dev profile and 13 KiB in release, and `FatVol::mount` adds about 7.5 KiB. Every kernel thread gets `DEFAULT_STACK_PAGES` = 4 (16 KiB). The only caller is the kernel_shell debug REPL's `mount fat32 ram0|vda <path>`. No syscall or production path reaches it.

**Evidence:**
```rust
let vol = FatVol::mount(&mut io).map_err(FatError::to_fs)?;   // :472
*SLOTS[id as usize].vol.get() = Some(vol);                     // :490
```
In the disassembly, the dev prologue is six `sub rsp,0x1000` probes plus `sub rsp,0x608`. In release, the prologue is 13,136 B and `FatVol::mount` adds 7,648 B. The probes run before the device-name match, so every invocation faults, including one with an invalid device.

**Impact:** A kernel_shell build crashes deterministically. #PF has no IST (src/arch/idt.rs:350-362), so the fault escalates to #DF, which dumps state and halts. There is no memory corruption. With the same pattern, a userspace write to a vda FAT would stack 4 KiB cluster buffers over the roughly 8 KiB frame of `virtio_blk_init::pump`. The finding estimates that path at about 20-22 KiB; the verifier did not re-check that total.

**Fix:** Build `FatVol` in place in its slot, as vibefs does with `Vol::clear` plus mount, or box it. Move the per-call cluster buffers into per-volume storage under the slot lock, like vibefs's `iobuf`. Add a static frame-size gate for syscall and shell entry points.


#### F059 · FAT case-only rename frees the file's cluster chain (dangling dirent or lost file); FAT and vibefs directory renames skip cycle checks, FAT skips the '..' fixup

**Severity:** MEDIUM · LATENT (rename syscall; CRITICAL on a vda FAT then) · **Confidence:** Confirmed

**Location:** src/fat.rs:616-663, src/fat.rs:624, src/fat.rs:627-634, src/fat.rs:656-662, src/vibefs.rs:1160-1189, src/file_init.rs:785-807, src/fs/mod.rs:940-942

**What's wrong:** The early return at fat.rs:624 compares names byte for byte, but `lookup` is ASCII case-insensitive (`eq_ci`). Renaming `a` to `A` in the same directory therefore resolves dst to src itself and unlinks it, which frees its chain. The new short entry is then copied from the deleted source dirent and names the freed clusters. If the new name needs LFN slots, `dir_reserve` reuses the slots just deleted, and the final `mark_deleted` (:662) deletes the new entry, so the file vanishes. Separately, neither FAT nor vibefs refuses to move a directory into its own subtree, and FAT never rewrites the moved directory's `..`.

**Evidence:**
```rust
if src_name == dst_name && src_dir == dst_dir { return Ok(()); }
let src = self.lookup(d, src_dir, src_name)?;
match self.lookup(d, dst_dir, dst_name) {       // finds src
    Ok(dst) => { /* ... */ self.unlink(d, dst_dir, dst_name, false)?; }
```
In host probes, after `a`→`A` the FAT marked cluster 4 free while `A` still named it. Once next-fit allocation wrapped around, `A` read another file's bytes. `hello.txt`→`Hello.txt` returned Ok and the file became NotFound. Moving `p` to `p/c/q` returned Ok on both filesystems, and vibefs fsck reported 0 errors.

**Impact:** On a disk FAT this causes a persistent cross-link or loses the file. On vibefs it leaves an unreachable directory cycle that leaks inodes and blocks. The stale `..` does not affect vibeOS, which resolves `..` itself, but external FAT tools see it.

**Fix:** When dst is the same dirent as src (same dir_clu/dir_off), skip the unlink and keep the write-new-then-delete-old path. Reject moving a directory under itself. Rewrite `..` in a moved FAT directory, using 0 for a root parent. Make vibefs fsck flag unreachable inodes. Add host regression tests.


#### F060 · Mount/umount lifecycle: vibefs `umount` skips sync and leaks its route and slot; slots are dropped on failed umount and under open fids; drop_slot force-clears busy after a grab timeout

**Severity:** MEDIUM · LATENT (umount/mount outside the kernel_shell REPL) · **Confidence:** Confirmed (spurious-EIO sub-claim Suspected)

**Location:** src/file_init.rs:1504-1515, src/fat_init.rs:518-538, src/vibefs_init.rs:539-559, src/fat_init.rs:430-453, src/vibefs_init.rs:432-444, src/fat_init.rs:143-165, src/vibefs_init.rs:176-198, src/fat_init.rs:34-41, src/vibefs_init.rs:36-43, src/fs/mod.rs:736-760

**What's wrong:** (a) `cmd_umount` calls `fat_init::umount` first. Its VFS umount also succeeds for `FsType::Vibe`, so `vibefs_init::umount` (sync, unregister_mnt, drop_slot) never runs. (b) Both umount functions unregister the route and call `drop_slot` even when the VFS umount fails. Neither checks `file_init::FILES`, and `Vfs::umount` scans only VFS files. (c) `drop_slot` ignores a `grab()` timeout. It then clears a busy flag it does not own and does not take ALLOC; the FAT version also overwrites `vol` while another thread may hold `&mut FatVol`. (d) `grab` returns Io after 1M yields.

**Evidence:**
```rust
let _ = grab(id);                          // fat_init.rs:438
unsafe { *SLOTS[i].vol.get() = None; }
SLOTS[i].used.store(false, Ordering::Release);
drop_busy(id);
```
Unmounting a FAT mount that has a child mount returns Busy, but only after the route has been removed and the slot dropped.

**Impact:** For vibefs, `umount` appears to succeed but leaves the volume unsynced and still routed. The leaked slot (MAX_VOLS=2) makes the next `mount vibefs` fail with NoSpace. Stale fids get Io. After the slot is reused, a write through a stale fid lands on a different volume at the old dir_clu/dir_off. (c) is aliasing UB behind a legacy `unsafe impl Sync for Slot` that has no SAFETY comment. No shown operation is confirmed to exceed the 1M-yield bound (`count_free` runs only before the slot is published).

**Fix:** Dispatch umount on the mount's fstype. Refuse umount while FILES holds fids on the volume, or refcount volumes from fids. Unregister and drop only after the VFS umount succeeds. Replace used/busy with `sync_init::BlockingMutex` owning the volume, and never force-clear it.


#### F061 · vibefs mount trusts on-disk record counts, block pointers, extents, F_INLINE, next_ino and refcounts: a crafted image panics the kernel or makes CoW overwrite live blocks

**Severity:** MEDIUM · LATENT (untrusted disks) · **Confidence:** Confirmed

**Location:** src/vibefs.rs:1955-1965, src/vibefs.rs:1967-1990, src/vibefs.rs:620-654, src/vibefs.rs:2058-2151, src/vibefs.rs:554-566, src/vibefs.rs:842-853, src/vibefs.rs:864-876, src/vibefs.rs:1150-1154, src/vibefs.rs:1916-1937, src/vibefs.rs:1324

**What's wrong:** `parse_meta` checks magic, CRC and kind, but does not check `count` against INODE_PER_LEAF (15) or DENT_PER_LEAF (56). Extent phys/len/log, `alloc_root`, `inode_root`, `dir_root` and INT child pointers are never range-checked against `nblocks` or MAX_BLOCKS (1024). The kernel Disk bounds-checks only against device capacity. `unpack_inode` accepts F_INLINE with size > 128. `next_ino` is not checked against the highest existing inode, and on-disk `refc` is never reconciled with reachability.

**Evidence:**
```rust
let (_lvl, count, _) = parse_meta(buf, META_INODE_LEAF)?;
while e < count as usize {
    let rec = unpack_inode(&buf[HDR + e * INODE_REC..])?;
```
Host probes with CRC-valid blocks panicked in five cases: an inode leaf with count 16 (:652), a dir leaf with count 57 (:1978), an extent with phys 5000 followed by unlink (:556), an F_INLINE inode with size 200 followed by read (:1324), and inode_root 1500 on a device larger than 4 MiB at commit (:1920). With refc 0 on a live block, `alloc_block` reused it. With next_ino 2, a new file aliased inode 2.

**Impact:** A crafted or corrupted image panics the kernel on mount, read, write, unlink, truncate or commit. Once the volume is mounted, any process can trigger the read panic. The same image can also make CoW overwrite live data in place. DESIGN §2.5 says the portable half never panics on data, though it also admits that vibefs is not yet `indexing_slicing`-clean.

**Fix:** Validate at mount: count within per-leaf capacity; every pointer in [2, nblocks); extent len ≥ 1 with checked phys+len ≤ nblocks; F_INLINE implies size ≤ 128; unique inos; next_ino greater than the highest ino; each meta block referenced once. Rebuild refcounts from reachability (see FS-04), or at least require refc > 0 on every reachable block.


#### F062 · vibefs truncate-grow keeps F_INLINE with size > 128; the next read, or a write that spills, indexes past inline_data and panics

**Severity:** MEDIUM · LATENT (truncate syscall, then HIGH; untrusted disks) · **Confidence:** Confirmed

**Location:** src/vibefs.rs:1498-1507, src/vibefs.rs:1321-1325, src/vibefs.rs:1248-1275, src/vibefs.rs:1508-1515, src/vibefs.rs:620-654, src/file_init.rs:834-858

**What's wrong:** When `new > INLINE` (128), the grow branch of `truncate` sets `size = new` and keeps F_INLINE without spilling. `read` then slices `inline_data[s..s+want]` and `spill_inline` slices `tmp[..size]`, both past 128 bytes. `unpack_inode` and fsck do not reject F_INLINE with size > 128, so a crafted image reaches the same state. Shrinks also leave stale bytes: the inline tail is zeroed only when new == 0, and the partial last extent block is never zeroed. A later grow therefore reads old data instead of zeros.

**Evidence:**
```rust
if self.inodes[is].flags & F_INLINE != 0 && new <= INLINE as u64 { /* stay inline */ return Ok(()); }
self.inodes[is].size = new;   // F_INLINE still set when new > 128
```
In a host probe, an inline 3-byte file was truncated to 256 bytes. `read` then panicked at :1324 ("range end index 256 out of range for slice of length 128"), and a write ending past byte 128 panicked at :1275. Writes that end at or below byte 128 succeed and leave the bad state behind, and fsck reports 0 errors.

**Impact:** This becomes a kernel panic as soon as a truncate syscall exists. Today `truncate_path` has one caller, a ktest that shrinks, and O_TRUNC only truncates to 0. It contradicts VIBEFS.md §7 (crossing 128 bytes clears the inline flag) and the "inline vs size" check that VIBEFS.md §11 lists for fsck. Shrink followed by grow breaks POSIX zero-fill.

**Fix:** In the grow branch, call `spill_inline` while `size` still holds the old value, then set `size = new`. Bound `read`/`spill_inline` by `inline_len` and return Corrupt on mismatch. Reject F_INLINE with size > 128 in `unpack_inode` and fsck. Zero the discarded tail on shrink.


#### F063 · vibefs CoW overwrite discards extent CRC failures and re-checksums corrupt data; on multi-block extents it merges into the wrong block and leaves stale CRCs

**Severity:** MEDIUM · LATENT (real hardware / persistent vda; foreign images for multi-block cases) · **Confidence:** Confirmed

**Location:** src/vibefs.rs:1416-1426, src/vibefs.rs:1191-1200, src/vibefs.rs:1474-1488, src/vibefs.rs:1534-1541, src/vibefs.rs:1341-1345

**What's wrong:** The overwrite path throws away the result of `check_extent`. It merges the new bytes into a block that may be corrupt, writes the result to a new block and stores a fresh CRC. `check_extent` → `extent_crc` reads every block of the extent into the shared `iobuf`, so in a multi-block extent the merge lands on the extent's last block, not the target. Split (:1474-1488, where a comment admits the stale CRC) and partial truncate (:1541) keep the old whole-extent CRC.

**Evidence:**
```rust
d.read_block(phys, &mut self.iobuf)?;
let old_e = self.inodes[is].extents[ei];
let _ = self.check_extent(d, old_e);   // error dropped; iobuf clobbered
self.iobuf[pin..pin + n].copy_from_slice(&buf[done..done + n]);
```
Host probe on a 1-block extent: after one byte of the data block was flipped, read returned Corrupt. A 1-byte overwrite then returned Ok. The next read returned the corrupted bytes under a valid CRC, and fsck reported 0 errors. In a 2-block extent, a write to block 0 stored block 1's contents.

**Impact:** Any write to a corrupted block permanently hides the corruption. This contradicts VIBEFS.md §9 and ROADMAP line 734 ("corruption reported rather than propagated"). The multi-block cases need an image this kernel did not write, since it only emits len=1 extents. On such an image they corrupt data silently or make good data read as Corrupt. The read path copies before it verifies, but that has no effect today because every caller discards the buffer on Err.

**Fix:** Propagate the error with `self.check_extent(d, old_e)?`, then re-read `phys` into `iobuf` or compute CRCs in a separate buffer. Recompute CRCs on split and partial truncate, or split multi-block extents at mount. In `read`, verify before copying.


#### F064 · FAT BPB arithmetic is unchecked: a crafted FATSz32 overflows data_lba and panics the kernel on mount

**Severity:** MEDIUM · LATENT (untrusted disks: a user-reachable mount of a foreign FAT image) · **Confidence:** Confirmed

**Location:** src/fat.rs:1558-1571, src/fat.rs:1522-1525, src/fat.rs:292-296, src/fat_init.rs:90-96

**What's wrong:** parse_bpb computes `data_lba = rsvd + num_fats * fatsz` from on-disk BPB fields. The only earlier check is `fatsz == 0`. The default kernel profile is dev with `overflow-checks = true` (Makefile:14, Cargo.toml:44), so a crafted FATSz32 panics the kernel inside FatVol::mount. In a release build the sum wraps silently. There is also no FAT32 maximum-cluster-count check.

**Evidence:**
```rust
let fatsz = le32(boot, 36);
if fatsz == 0 { return Err(FatError::Inval); }
...
let data_lba = rsvd + num_fats as u32 * fatsz;
```
In a host probe, fatsz=0x8000_0000 with num_fats=2 (or 0xFFFF_FFFF with num_fats=1) panics with an overflow at fat.rs:1569. Today the only way in is the debug-shell `mount fat32 ram0|vda` (file_init.rs:1465) and ktests. There is no mount syscall. Reference: Microsoft FAT32 spec v1.03, "Boot Sector and BPB" and "FAT Type Determination".

**Impact:** Mounting a crafted image panics the kernel. In a release build the FAT region overlaps the data region, and writes corrupt that volume. The `clu * 4` overflow in fat_loc is a second crafted-image case only: it needs nclus > 2^30 with clusters of at most 4 KiB, which spec-conforming volumes cannot have. The u32 truncation in Io::nsectors fails safe: on devices of 2 TiB or more it causes a spurious Corrupt.

**Fix:** Use checked_mul/checked_add at fat.rs:1569 and return Corrupt on overflow. Reject nclus > 0x0FFFFFF5, which also bounds fat_loc. Optionally check at mount that fatsz*128 >= nclus+2 and root_clus < nclus+2 (runtime guards at fat.rs:177/1354/1363/1383 already catch both later), and clamp capacity with `.min(u32::MAX as u64)`.


#### F065 · VFS dentry cache: evicting a directory leaves children linked to a reusable slot, aliasing lookups and hiding mounts; umount mutates before its last Busy check

**Severity:** MEDIUM · LATENT (A3: FAT and vibefs behind the VFS on the syscall path) · **Confidence:** Confirmed

**Location:** src/fs/mod.rs:419-430, src/fs/mod.rs:1618-1654, src/fs/mod.rs:1656-1669, src/fs/mod.rs:1671-1682, src/fs/mod.rs:1459-1473, src/fs/mod.rs:691-730, src/fs/mod.rs:736-790

**What's wrong:** Dentry.parent is a bare u16 slot with no refcount and no generation. dentry_evict frees an unpinned directory dentry without touching its children, and dentry_force_alloc then gives the slot to a new dentry. After that, dcache_find returns the old children under the new directory, and dotdot follows the reused slot. mount() calls dentry_force_alloc before it pins the mountpoint. umount unpins and evicts dentries before its final inode-refs Busy check.

**Evidence:**
```rust
if d.used && d.parent == parent && d.name.eq_bytes(name) {
```
dcache_find checks neither the mount nor a generation. Host probes showed three failures:
- When /c reuses /a's slot, stat("/c/x") returns the inode of /a/x.
- Evicting /a hides a mount on /a/m in 349 of 400 runs, and umount of it then fails.
- mount() once returned mp_dslot == root_dslot.

**Impact:** Path resolution can return the wrong inode, and a mount can end up hidden, stranded or on the wrong directory. Today only the kernel shell (kernfs trees and shell ramfs mounts) and ktests reach this: FAT/vibefs lookup returns NotSupp (mod.rs:1311), and syscalls do not walk the Vfs.

**Fix:**
- Keep a parent pinned or refcounted while any child exists, or evict whole subtrees; alternatively, add a generation to the parent link.
- Make dcache_find match the mount.
- Pin dir.dslot before calling dentry_force_alloc in mount().
- Do every umount check before changing any state.
- Add a host regression test for the hidden-mount case.


#### F066 · tmpfs extent relocation copies the stale backing store, then drops dirty cache pages, losing written data

**Severity:** MEDIUM · LATENT (kernfs/tmpfs file I/O reachable from syscalls) · **Confidence:** Confirmed

**Location:** src/fs/kernfs.rs:1364-1376, src/fs/kernfs.rs:1277-1303, src/fs/kernfs.rs:1387-1404, src/fs/kernfs.rs:1449-1450, src/cache.rs:446-452

**What's wrong:** tmpfs writes go only into the 4-page write-back tmp_cache, and tmp_back is updated only when a page is evicted. When tmp_ensure cannot grow an extent in place, it copies the old run from tmp_back, which may be stale, to the new run. tmp_free_run then invalidates the old pages, and Cache::invalidate drops dirty pages without writing them back.

**Evidence:**
```rust
    vfs.kern.tmp_back.copy_within(src..src + nbytes, dst);
}
tmp_free_run(vfs, old as u16, oldn as u16);
```
Host probe: write 8 bytes to /tmp/a, create /tmp/b right after it, then grow /tmp/a with a write or a truncate. Reading a[0..8] then returns zeros. If eviction is forced first, the data survives.

**Impact:** Recently written data is silently lost in any tmpfs file that relocates while growing, through write or truncate-grow. Today only the Vfs API reaches kernfs (host tests and ktests). The syscall file layer routes only to FAT and vibefs.

**Fix:** Before copy_within, write the old run's dirty pages back to tmp_back, or re-key them to the new run. Drop pages without writeback only when they are truly freed (unlink, shrink). Correct the Cache::invalidate contract comment. Add a regression test: write A, create B next to it, grow A, read A back.


#### F067 · docs/VIBEFS.md and checked ROADMAP boxes claim guarantees the code does not provide

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** docs/VIBEFS.md:53-58, 219-222, 230, 243-246, 257-260, 309-322, 330-345; docs/ROADMAP.md:703, 719, 724, 733, 734, 739; src/vibefs.rs:588-590, 923-933, 1420, 1505, 1829-1836, 2130, 2175-2226

**What's wrong:** VIBEFS.md calls itself normative, and AGENTS.md makes the ROADMAP checkboxes the status, but several statements are false:
- §8 puts the DIR_INT child pointer at offset 68 and nlen at offset 5, which overlaps the name bytes. The code writes and reads the child at 0 and nlen at 4.
- §8 says keys sort by length then bytes, but name_cmp is plain lexicographic.
- §2 describes snapshot delete, which does not exist.
- Parts of §7 (inline flag clearing), §9 (no corrupt payload) and §11 fsck are not implemented. The missing fsck checks are mode, inline vs size, duplicate names, regular-file nlink and the bitmap comparison.

**Evidence:**
```rust
put32(&mut self.iobuf, o, dleaf[k]);  // child at 0
self.iobuf[o + 4] = de.nlen;          // nlen at 4
```
ROADMAP boxes that are checked but false:
- 724: tmpfs is a static ramdisk with a private 4-page cache, not the page cache.
- 733: find_dent linearly scans a flat 96-entry table.
- 734: the write path ignores check_extent (vibefs.rs:1420).
- 719: FAT case-only rename writes a dirent to freed clusters.

Box 703 holds only in the VFS. Box 739's pass criterion ("errors 0") accepts a volume that has stopped committing.

**Impact:** Later agents will build on guarantees that do not exist. A tool written from §8 will misparse DIR_INT blocks, and the DIR_INT path (more than 56 entries) has no test.

**Fix:** Correct §8 and the key-order text to match the code. Uncheck 719, 724, 733 and 734, and qualify 703 as VFS-only. Implement the missing §11 checks, and add a progress or contents check to the §12 crash tests.


#### F068 · Any ring-3 process can kill pid 1; with init dead (or absent in test/shell builds), orphans become unreapable zombies until fork returns EAGAIN, silently

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/proc_init.rs:1143-1181, src/proc_init.rs:972-1019, src/proc_init.rs:1038-1051, src/proc_init.rs:1114-1136, src/proc_init.rs:215-243, src/proc_init.rs:840-843, src/proc_init.rs:331-345, docs/ROADMAP.md:807, docs/ROADMAP.md:817-818

**What's wrong:** sys_kill has no INIT_PID guard, so default-Term signals kill init and default-Stop signals stop it. A dead init becomes a Zombie with ppid 0 that nothing can reap. alloc_pid never reuses slot 1, and there is no respawn, panic or marker. reparent_children still sets `ppid = INIT_PID` unconditionally, and that is also true in kernel_tests/kernel_shell builds, where slot 1 is Unused. Bound run_path exits skip reparent_children entirely, so their children keep a dangling ppid that a recycled pid can inherit.

**Evidence:**
```rust
if t.procs[i].state != ProcState::Unused && t.procs[i].ppid == dead {
    t.procs[i].ppid = INIT_PID;   // init's state never consulted
    ...
    let _ = wq;                   // dead wake; the real one is at 995-997
```
Trigger: any ring-3 program sends pid 1 a signal whose default action is Term. find_zombie requires `p.ppid == parent` (1118), and wait4 from pid 0 returns ECHILD (1055-1057). kill(2) NOTES: PID 1 receives only signals it has handlers for.

**Impact:** The stock image has no path to this, so today it needs a custom binary. Once init is dead, /bin/sh and every later orphan stay zombies forever. The 14 usable slots fill up and fork returns EAGAIN for the rest of the boot, with nothing logged. SIGSTOP/SIGTSTP only delay reaping until SIGCONT. ROADMAP 9.5/9.6 tick orphan reaping, plus an in-guest orphan test (818) that does not exist.

**Fix:** Drop default-Term/Stop signals aimed at INIT_PID, and panic or emit a registered marker if init exits. Put reaper selection in a host-tested vibeos-core helper that returns INIT_PID only while slot 1 is Live or Stopped and otherwise frees zombie orphans. Call it from the bound-exit path too, delete the dead wake, and add a grandchild-orphan user test.


#### F069 · fork gives the child the boot FPU template instead of the parent's FXSAVE image; execve keeps the old image's XMM/x87 registers, MXCSR and FCW

**Severity:** MEDIUM · LATENT (Phase 14 C userspace / musl + libm; any SSE-built user runtime) · **Confidence:** Confirmed

**Location:** src/proc_init.rs:857-869, src/thread_init.rs:578, src/thread_init.rs:630, src/proc_init.rs:877-961, src/syscall_init.rs:66-78, src/syscall_init.rs:607-633, docs/SYSCALL.md:27, docs/ROADMAP.md:771

**What's wrong:** sys_fork builds the child with spawn_user, which initializes `tcb.fpu` from `fpu_template()`, and never copies the parent's saved image. The child therefore returns from fork with template XMM, ST, MXCSR and FCW values. sys_execve never touches `tcb.fpu`, so the exit stub's `fxrstor64` hands the new image the old image's registers, MXCSR (rounding, FTZ/DAZ, masks, sticky flags) and x87 CW/SW. run_user has the same gap between consecutive bound programs.

**Evidence:**
```rust
let h = thread_init::spawn_user("user", user_thread_entry, pid, cr3); // :857
// thread_init.rs:578 / :630
fpu: crate::syscall_init::fpu_template(),
```
Nothing between :857 and make_ready at :869 writes the child's fpu, and sys_execve has no fpu reference. psABI §3.2.1 makes the MXCSR control bits and x87 CW callee-saved. psABI §3.4.1 requires a new process to start with CW 0x037F and MXCSR 0x1F80. POSIX fork requires an exact copy.

**Impact:** No effect today, because user programs are FP-free asm. With a libc, a forked child silently loses fesetround/FTZ/DAZ settings and any XMM value that is live across the syscall. An exec'd image inherits unmasked exceptions: with CR4.OSXMMEXCPT clear these raise #UD, which becomes SIGILL (SDM Vol 3A §2.5; this needs KVM or hardware). Once non-root users exist, it also inherits the prior image's register contents.

**Fix:** Copy the parent's `tcb.fpu` into the child before make_ready. On execve, after the point of no return and with IRQs off, write an ABI initial image into `tcb.fpu` and fxrstor it. Force MXCSR to 0x1F80 there, because the template's MXCSR is whatever firmware left. Do the same in run_user. Add a tests.asm check of MXCSR, FCW and xmm0 across fork and exec, and document the rules in SYSCALL.md.


#### F070 · Exception backtraces start at the handler's own rbp: for error-code vectors the walk reads the interrupted RAX as a return address

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/panic.rs:255, src/panic.rs:283, src/panic.rs:90-92, src/panic.rs:153-178, src/arch/idt.rs:50-65, src/arch/idt.rs:107-139, tests/harness/run_e2e.py:90

**What's wrong:** exception_halt and exception_vec pass their own `x86::read_rbp()` to dump_backtrace. The walk prints the fault RIP, then the handler's call site, and then treats [handler_rbp+8] as a return address. For every error-code vector (#GP, #PF, #DF and the default_err handlers), LLVM's x86-interrupt prologue is `push rax; push rbp`, so that slot holds the interrupted RAX, not a RIP.

**Evidence:**
```rust
dump_common(frame.rip, x86::read_rbp(), frame.rsp, frame.rflags);
```
Disassembly of general_protection, page_fault and double_fault in the built kernels shows `push rax; push rbp; mov rbp, rsp`, with the error code at [rbp+0x10] and the frame at [rbp+0x18]. The extra push realigns the stack after the CPU's 48-byte push (Intel SDM Vol. 3A §6.14.2).

**Impact:** If RAX is outside the kernel image, the walk stops and the faulting function's callers never appear. If RAX points anywhere in the image (in_image covers .text through .bss), a bogus frame is printed before the real chain. No-error vectors print the fault RIP twice. The `regs: rbp=` line shows exception_halt's rbp, not the interrupted one. The e2e check matches only frame 0, so it passes either way.

**Fix:** Use naked asm entry stubs that save a full register frame and pass the interrupted rbp explicitly. As a stopgap, derive it inside exception_halt as `*(*(read_rbp() as *const u64) as *const u64)`. Add a ktest that faults two calls deep and asserts the caller's symbol appears.


#### F071 · Panic path writes through the asserting InterruptGuard: an irq_nest underflow panic recurses forever and hides the original panic

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/panic.rs:73-83, src/panic.rs:206, src/serial.rs:63, src/serial.rs:113, src/x86.rs:277-306, src/per_cpu_init.rs:179-184

**What's wrong:** Every serial write on the panic path, including begin_dump's "reentered" line, goes through Serial::write_bytes or write_fmt, which take an InterruptGuard. The guard's drop runs a release `assert!(old > 0, "irq nest underflow")`. If the original panic is that assertion, the counter has already wrapped to u32::MAX, so every later guarded write fails the assert again and the handler recurses without ever reaching finish().

**Evidence:**
```rust
let old = c.irq_nest.fetch_sub(1, Ordering::Relaxed);
assert!(old > 0, "irq nest underflow");
```
After the first failure each guard moves the counter MAX→0 on enter and 0→MAX on drop with old == 0. begin_dump's re-entry branch writes through the same guard, so the cycle never ends.

**Impact:** The banner prints, followed by endless "vibeOS: panic: reentered". The real panic's message and location are lost, and under panic_exit isa-debug-exit is never reached, so tests hang to timeout. The stack grows: on a KVA stack the guard-page #PF usually becomes #DF on IST1 (Intel SDM Vol. 3A §6.15, Interrupt 8) and keeps looping, though a fault inside the enter/leave window can halt cleanly. On the BSP bootstrap thread's unguarded Limine stack it likely overwrites adjacent memory. Nothing underflows irq_nest today; this only fires on top of the accounting bug the assert exists to report.

**Fix:** After begin_dump, write through a raw path (IF already off, write_bytes_raw, no InterruptGuard, capture or per-CPU access), including the reentered line. Alternatively, skip irq_nest accounting and the assert in InterruptGuard once ipi_init::is_halting() is set.


#### F072 · Boot and the ktest registry run on Limine's unguarded 64 KiB stack, with no StackSizeRequest

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/main.rs:97-124, src/main.rs:133-289 (fs_init at :258, ktest::run at :282), src/boot.rs:17-37, src/thread_init.rs:363-396, src/ktest.rs:202-227

**What's wrong:** `_start` and the bootstrap thread (`stack: None`) run all of boot on Limine's stack, and so does every in-guest test in `kernel_tests` builds. That stack is only guaranteed to be 64 KiB, and the kernel never requests a larger one. It has no guard page, and after `sti` (main.rs:223) IRQs push onto it too. DESIGN §9.2 and §2.4 require a guard page on every kernel stack, with no exception.

**Evidence:**
```rust
    let mut tcb = Box::new(Tcb {
        id: ThreadId::BOOTSTRAP,
        ...
        stack: None,
```
Limine PROTOCOL.md (x86-64 entry machine state, rsp) puts this stack in bootloader-reclaimable memory at "at least 64KiB" unless the Stack Size feature is used. Direct calls alone reach about 32 KB from `_start`: fs_init::init, then fat_init::init (13 KB frame), then FatVol::mount, then virtio_blk_init::pump.

**Impact:** Headroom is about 2x today and shrinks as boot code grows. After the CR3 switch the physmap maps everything below the stack, so an overflow does not fault. It silently corrupts that memory instead, likely the Limine memmap entries BootInfo still reads and then buddy RAM. Userspace cannot reach this stack, because syscalls run on `fallback_rsp0`.

**Fix:** Add a `StackSizeRequest` now (for example 256 KiB). Once KVA is up (main.rs:176), switch to a guarded KVA stack, or run the rest of boot and `ktest::run` on a thread with a guarded stack. Keep large-frame functions such as `fat_init::init` off the boot path (DESIGN §3.5).


#### F073 · e2e boot contract ends at a `shell ready` line printed from ring 3 and never checks the /bin/tests result on the production init path

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** tests/harness/harness.py:1142-1144, tests/harness/harness.py:654-664, tests/harness/harness.py:50-59, user/init.asm:22-28, user/tests.asm:197-205, user/sh.asm:15-19, docs/DESIGN.md:369

**What's wrong:** In production, /sbin/init forks /bin/tests and calls wait4 with no status pointer. It then execs /bin/sh no matter how the tests ended. /bin/sh writes the final contract marker `vibeOS: shell ready` with write(1), and the harness sends `quit` as soon as that line matches. Nothing in tests/harness matches `user: tests ok` or `user: tests fail`, and PANIC_SIGNATURES only covers panics and exceptions. DESIGN.md:369 still says a kernel shell thread prints `shell ready`.

**Evidence:**
```asm
.wait_tests:
    mov rdi, rax
    xor rsi, rsi        ; no status pointer
    ...
    mov eax, SYS_WAIT4
    syscall
.shell:
```
If /bin/tests prints `user: tests fail` and exits 1 without panicking, every e2e variant still passes.

**Impact:** The in-guest ktest `user_syscalls` (src/ktest.rs:944-975) runs /bin/tests through run_path with interrupts on and requires status 0. That covers the BIOS/TCG ktest configuration. Nothing checks the production path: spawn_elf, init as pid 1, and /bin/tests as a scheduled fork+execve child. Nothing checks the e2e-only configurations either (`test-e2e-uefi`, `test-e2e-pit`, `test-e2e-highmem`). release.yml:53 gates on `make test-e2e` alone.

**Fix:**
- Add `Marker("user: tests ok", ...)` before `shell_ready` in the non-gp contract.
- Register `user: tests fail` as a failure signature.
- Have init check the wait4 status and print a distinct failure line (or exit) when it is nonzero.
- Update step 18 at DESIGN.md:369.


#### F074 · Frame-accounting ktests assert exact global buddy counts without a quiescent baseline; reap_many_via_idle flakes and a per_cpu_bsp flake is masked by harness retry

**Severity:** MEDIUM · **Confidence:** Likely

**Location:** src/ktest.rs:234-236, src/ktest.rs:1756-1812, src/ktest.rs:2219-2222, src/kva.rs:149-173, src/paging_init.rs:302-307, src/thread_init.rs:540-583, src/ktest.rs:1370-1372, tests/harness/harness.py:826-832, tests/harness/harness.py:867-875

**What's wrong:** reap_many_via_idle compares the global buddy `free_frames()` count before and after the test, but that count also moves with permanent allocations whose number depends on timing:
- `Kva::free` appends freed VA to the tail, so until the first coalesce each spawn maps fresh VA. That can allocate a page-table page, and `unmap_4k_locked` never frees it.
- `spawn` Box-allocates a new Tcb when no Dead slot is free, which grows the heap for good.

The reviewer's original explanation does not hold at -smp 2. It claimed an AP was holding drained DEFERRED stacks, but an idle AP resumes via from_irq inside `halt_if_idle` and never reaches reap, and no thread homed on an AP runs during the test. Separately, per_cpu_bsp's check that `ready_head` is null is not an invariant. The value is a stale snapshot from the BSP's last relink.

**Evidence:**
```rust
// ktest.rs:1804-1807
let after = free_frames();
if after != before {
    return Outcome::Fail("reap did not restore frames");
```
The failure was observed at -smp 2. spawn_exit_thousands (ktest.rs:2220-2222) documents the page-table-page leak and warms up with 256 spawns first. reap_many_via_idle runs earlier and has no warm-up. The harness retries the per_cpu_bsp failure, matched by its exact message string.

**Impact:** `make test` fails at random, and the per_cpu_bsp flake is retried instead of fixed. The originally proposed in-flight counter on drain_deferred would fix neither.

**Fix:** Take a quiet baseline first: in a shared setup step, pre-warm KVA past one coalesce and pre-populate Dead TCB slots. Alternatively, assert that the specific stack frames and the KVA `used` count come back, not the global buddy count. Remove the `ready_head` assertion together with its harness retry. Fix the "idle loop must drain" comments (ktest.rs:1759-1760, sched_init.rs:53-54).


#### F075 · ktest runs the registry, ring 3 and fork children with IF=0, an interrupt context production does not use, and ktest workarounds have leaked into production code

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/ktest.rs:202-206, src/ktest.rs:1115-1126, src/ktest.rs:944-947, src/syscall_init.rs:607-634, src/proc_init.rs:852, src/proc_init.rs:870-873, src/proc_init.rs:384, src/thread_init.rs:406-420, src/ipi_init.rs:122-149, src/kva_init.rs:136-154

**What's wrong:** `ktest::run` holds an InterruptGuard across the whole registry, so the BSP runs tests with IF=0. `with_timer` then does `sti` while that guard is still held, a state production never reaches. `run_user` enters ring 3 with IF=0. `sys_fork` copies the parent's saved r11, so under ktest `/bin/tests` and its fork children stay non-preemptible until they exec. That contradicts the ktest.rs:945 comment "Spawned fork children enter with IF on". `spawn_here` copies `irq_nest`, so ktest workers also run with IF=0.

**Evidence:**
```rust
let _cli = x86::InterruptGuard::enter();                 // ktest.rs:205
unsafe { enter_user(rip, rsp, RFLAGS_RESERVED1, fs_base) }; // syscall_init.rs:633
entry.rflags = RFLAGS_RESERVED1 | RFLAGS_IF;             // proc_init.rs:384
```
`sys_fork` calls `yield_now()` unconditionally, and its comment names "ktest's IF-off registry" as the reason. That is a ktest workaround in production code.

**Impact:** In-guest SMP and user-mode tests exercise an interrupt context that production does not use. One possible contributor to the retried SMP4 `ipi: ack timeout` panics is a BSP sitting at IF=0 (Suspected). The panic does not name the CPU that failed to ack, and the BSP still services IPIs while it spins. The `user: dup ok` wait4 stall is not explained by this, because it also occurs in production e2e with IF=1 (commit 92cc152).

**Fix:** Run the registry in a kernel thread with IF=1 and `irq_nest=0`, and disable interrupts only inside the tests that need it. Have `run_user` enter ring 3 with `RESERVED1|IF`. Then re-evaluate the SMP4 IPI retry. The dup-ok retry needs its own root cause.


#### F076 · ktest first-boot retries reuse the disk the failed attempt already wrote, so FAIL-line retries cannot pass and are reported as 'missing persist wrote'

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** tests/harness/run_ktest.py:45-104, tests/harness/run_ktest.py:107-114, src/ktest.rs:202-227, src/ktest.rs:4112-4151, tests/harness/test_harness.py:507-535

**What's wrong:** `main()` creates the virtio disk once, and every `_ktest_boot` attempt reuses it. `ktest::run` keeps going after a FAIL, so a failing attempt still runs `block_persist`, which writes the persist pattern. `block_persist` is test #112 of 120, after `per_cpu_bsp` (#40) and `msix_cpu` (#95). On the retry, `block_persist` finds the magic and prints `persist: intact`. The first-boot check requires `persist: wrote`, and that error is not retryable. The unit test mocks a retry that prints `persist: wrote`, which a real disk cannot produce.

**Evidence:**
```python
_require_line(raw.lines,
    lambda ln: ln == "vibeOS: persist: wrote",
    "missing persist wrote")
```
The trigger is any first-boot retry where the failed attempt got past `block_persist`. A host-side replay with realistic mocked output raises `missing persist wrote` after two calls.

**Impact:** The SMP2 `per_cpu_bsp` and SMP4 `msix_cpu` FAIL retries are dead code, and so is any retry of a timeout or panic that happens after `block_persist`. The final `[ktest] FAIL:` names the wrong cause, although the real error is printed on the "retry after" line just before it. The dup-ok retry (#23) and IPI panics that happen before #112 still retry correctly.

**Fix:** Create a fresh zero-filled disk for each first-boot attempt, and pass only the successful attempt's disk to the persist reboot. Make the mocks realistic: `persist: wrote` in the failed run and `persist: intact` in the retry.


#### F077 · Phase 9 ROADMAP checkboxes claim per-syscall error, orphan, exec-chain and wait-ordering tests that do not exist, and the fork bomb never asserts the limit

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** docs/ROADMAP.md:761, docs/ROADMAP.md:805, docs/ROADMAP.md:818, docs/ROADMAP.md:829, user/tests.asm:153-186, src/ktest.rs:896-942, src/proc_init.rs:47

**What's wrong:** These boxes were checked in #74 together with user/tests.asm, the only userspace test program. EFAULT and huge-length cases are tested only for `write`. No test calls read, open, lseek, dup2, fcntl, kill, getppid or psinfo, or passes a bad execve argv/envp or wait4 status pointer. The kernel does validate these; only the tests are missing. There are no orphan-reparenting, exec-chain or wait-ordering tests. Line 805 ("a process is one or more threads") is false: `Proc` has a single `tid` and no thread syscall exists.

**Evidence:**
```asm
.bomb_done:
    cmp rax, -EAGAIN
    je .reapb
    test r13, r13
    jz fail
```
The bomb passes after 32 successful forks (rax = 0 from sched_yield) and on any non-EAGAIN error after the first fork. EAGAIN is reached today (MAX_PROCS = 16), but the test does not assert it, so a raised or removed limit would go unnoticed.

**Impact:** The hostile-userspace surface is recorded as verified, and agents treat checkboxes as status. Real gaps are already hidden: `sys_read` and `wait4` check destinations only with `check_user_range`, which does not require a writable PTE. They can therefore write into read-only user pages, and no test would notice.

**Fix:** Uncheck 761, 805, 818 and 829, or back them with a table-driven user test. It should call every pointer-taking syscall with NULL, kernel, unmapped, USER_END-straddling, read-only-page and huge-length arguments. Make the bomb require `-EAGAIN`, and add an orphan test (`getppid() == 1`) and SIGKILL/SIGSTOP tests.


#### F078 · The TSC-deadline timer path never runs in any test tier; under TCG, test-lapic-fallback takes the same LAPIC path as test-kernel

**Severity:** MEDIUM · LATENT (KVM / bare-metal Intel) · **Confidence:** Confirmed

**Location:** tests/harness/harness.py:496-513, Makefile:39, Makefile:248-249, .github/workflows/ci.yml:150-157, src/ktest.rs:1264-1288, src/apic_init.rs:414-423, src/apic_init.rs:460-470, src/apic_init.rs:669-683, docs/ROADMAP.md:404, docs/DESIGN.md:925-926

**What's wrong:** Every tier runs `-accel tcg`, and QEMU's full-system TCG never advertises CPUID.01H:ECX[24] (TSC-deadline), even with `-cpu max`. The kernel therefore always uses the periodic LAPIC timer, and arm_tsc_deadline, rearm_deadline and the TscDeadline arm of arm_ap have never run. test-lapic-fallback only changes the CPU to `qemu64,-tsc-deadline`, so under TCG it takes the same periodic branch as test-kernel. The only extra coverage it adds is outside the LAPIC timer: no SMEP/SMAP/UMIP, and the lfence;rdtsc fallback. Even so, ROADMAP.md:404 counts it as a Phase 4 exit gate, and DESIGN.md:925-926 says "CI runs both".

**Evidence:**
```python
if accel_s == "tcg" or "-tsc-deadline" in parts:
    return "periodic"
return "tsc-deadline"
```
Makefile:39 sets `VIBEOS_QEMU_ACCEL ?= tcg`, and no workflow overrides it. QEMU's target/i386/cpu.c leaves CPUID_EXT_TSC_DEADLINE_TIMER out of the TCG feature set except in user-mode builds.

**Impact:** The first KVM or real-Intel boot runs MSR arming and rearm code that has never executed. If the boot-time timer check (prove()) fails, the kernel falls back to periodic, but bugs that only appear later (AP arming, per-tick rearm) would go unnoticed. The gap also hides a related issue, reported separately (Suspected): disarm_timer (apic_init.rs:437-438) always writes IA32_TSC_DEADLINE, including on the periodic-failure path. That MSR exists only when CPUID.01H:ECX[24]=1 (Intel SDM Vol. 4, Table 2-2), so the write may #GP.

**Fix:** Add the KVM CI leg tracked at ROADMAP.md:867, on a runner that has /dev/kvm; the harness already expects `tsc-deadline` in that configuration. Until then, qualify ROADMAP.md:404 and DESIGN.md:925-926. Also add an `-smp 1` e2e run.


#### F079 · test-e2e-uefi's skip never skips, and the documented Homebrew OVMF file cannot be loaded through the harness's -bios

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** Makefile:218-225, Makefile:263, tests/harness/harness.py:561-563, tests/harness/harness.py:673-690, README.md:31-33, AGENTS.md:51-53, docs/reviews/issues/I1-macos-job-ovmf.md:13

**What's wrong:** The OVMF check and the harness run are separate recipe lines, so each runs in its own shell. `exit 0` therefore ends only the check, and the harness always runs. The harness passes firmware with `-bios`, and QEMU's ROM path rejects any image whose size is not a multiple of 64 KiB. The Homebrew `edk2-x86_64-code.fd` that README and AGENTS.md recommend is a 0x37C000-byte code-only image (remainder 49152), so it is rejected. The harness ignores QEMU's exit code and reports `missing marker 'serial_online'` without QEMU's stderr. I1 wrongly says the target "exits 0".

**Evidence:**
```make
	@if [ ! -f "$(OVMF)" ]; then \
	    echo "...skipping"; \
	    exit 0; \
	fi
	VIBEOS_ISO=$(ISO) VIBEOS_BIOS=$(OVMF) python3 tests/harness/run_e2e.py
```
QEMU v10.2.x hw/i386/x86-common.c:1043-1045 has `if (bios_size <= 0 || (bios_size % 65536) != 0) goto bios_error;`. That path runs when no pflash drive is given (hw/i386/pc_sysfw.c:248-254).

**Impact:** With the documented macOS setup, `test-e2e-uefi` always fails. It is the fourth prerequisite of `test:`, so make stops before the panic, gp, pit, highmem, ktest and vibefs-crash tiers. The pre-PR gate cannot go green on the owner's host. CI is unaffected, because apt's `/usr/share/ovmf/OVMF.fd` is a combined image.

**Fix:** Put the check and the run in one shell line, with a strict mode that fails in CI. Attach code-only images as `-drive if=pflash,format=raw,unit=0,readonly=on,file=…` plus a writable VARS copy on unit=1. When output ends before the first marker, report QEMU's exit status and stderr.


#### F080 · The vibefs QEMU-kill crash gate is mostly vacuous: no committed-prefix check, guest commit errors discarded, and most rounds kill a full volume that no longer changes

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** tests/harness/run_vibefs_crash.py:49-52, tests/harness/run_vibefs_crash.py:83-103, src/vibefs_init.rs:568-597, tests/harness/harness.py:452-461, tests/harness/harness.py:477-479, tests/harness/harness.py:1000-1016, src/vibefs.rs:153-209, docs/ROADMAP.md:691, docs/VIBEFS.md:330-347

**What's wrong:** Passing only requires fsck to print `errors 0`. The image is never mounted and /crash/w is never checked for a committed prefix, which docs/VIBEFS.md §12 requires. `wr` is parsed from the first marker only, so it is always 0. The guest discards `sync_fs()` errors (vibefs_init.rs:594), so after the dir-block leak fills the image (around iteration 56) every commit fails and later kills hit a volume that no longer changes. An early QEMU exit or a hang after `wr 0` also passes.

**Evidence:**
```python
if code != 0:
    raise HarnessError(f"fsck errors after kill wr={wr}: {out}")
if "errors 0" not in out:
    raise HarnessError(f"fsck did not report errors 0: {out}")
```
Observed run: 8/8 ok, wr=0 in every round, and rounds 2-8 identical at `gen 57 errors 0 warnings 58`. Kills are uniform over 0-0.18 s, so about two-thirds of rounds test nothing.

**Impact:** ROADMAP.md:691 marks power-loss survival as verified, yet this gate already let a persistent leak through. A missing vibefs barrier is caught only by chance, through the guest cache. A device-level flush bug is never caught, because QEMU's finished writes survive SIGKILL (QEMU docs, `-drive cache=writeback`). CrashDisk drops only an in-order suffix, so it cannot catch a missing barrier either.

**Fix:** Fail on guest commit errors, and make the image large enough never to fill. After the kill, mount and require /crash/w to match iteration N or N-1 of the last `wr N`. Require that the harness itself killed QEMU and that some minimum number of commits ran. Check that warnings do not grow with the generation. Let CrashDisk drop or reorder writes since the last flush.


#### F081 · BlockDevice trait is bypassed in production: cache, filesystem and partition code hard-code ram0/vda, so partitions cannot be mounted

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/block.rs:119-130, src/cache_init.rs:20-72, src/part_init.rs:65-112, src/fat_init.rs:89-96, src/fat_init.rs:455-470, src/vibefs_init.rs:92-165, src/vibefs_init.rs:336-344, src/vibefs_init.rs:469-484, src/fs_init.rs:67-86, src/fs/kernfs.rs:1018, src/fs/kernfs.rs:1053, docs/DESIGN.md:1915

**What's wrong:** DESIGN §1.1 justifies the BlockDevice trait by "genuinely N backends", and DESIGN.md:1915 says the cache sits above it. In production, though, the cache, partition, FAT, vibefs and devfs code all dispatch with `match dev { DEV_RAM0 => block_init::.., DEV_VDA => virtio_blk_init::.., _ => .. }`. vibefs_init even treats any unknown device id as vda when it looks up the block size. The trait objects (`block_init::device`, `virtio_blk_init::device`, `part_init::device`) are called only from ktest.rs. A module-level `allow(dead_code)` outside kernel_tests hides this. Partition I/O does go through the parent's cache. However, both mount_dev functions accept only "ram0" and "vda", and kernfs Block nodes return NotSupp for read and write.

**Evidence:**
```rust
fn raw_flush(dev: u32) -> Result<(), BlockError> {
    match dev {
        DEV_RAM0 => block_init::flush(),
        DEV_VDA => virtio_blk_init::flush(),
        _ => Err(BlockError::Inval),
```
The shell's `mount fat32 vdap2 /x` returns Inval. /dev/vdap1 exists but cannot be read.

**Impact:** Filesystems can use only the whole ram0 or vda device. Adding a second disk, NVMe or AHCI means editing every hard-coded match, across about six modules. ROADMAP.md:670 (marked done) and DESIGN.md:1915 describe a design the code does not follow. This is tracked as D2 (ROADMAP.md:897).

**Fix:** Keep a registry of `&'static dyn BlockDevice`, keyed by name and including partitions. Key the cache by device handle, have the partition, FAT and vibefs code take that handle, and let mount_dev resolve any registered name. Then drop the dead_code allows. Separately: part_init::init (part_init.rs:436-450) stamps a GPT onto any vda of at least 1024 sectors that has no partition table, which would overwrite a filesystem written to the whole disk. That should be its own finding.


#### F082 · setjmp/longjmp is implemented twice, and the legacy bound user model still runs beside spawned processes

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/arch/catch.rs:23-33, src/arch/catch.rs:70-127, src/syscall_init.rs:419-488, src/syscall_init.rs:606-632, src/user_init.rs:270-304, src/proc_init.rs:439, src/proc_init.rs:972-993, src/main.rs:260

**What's wrong:** catch.rs and syscall_init.rs each define the same `#[repr(C)] JmpBuf` and instruction-identical setjmp/longjmp asm under different symbols. The older "bound" model (run_user with global USER_JMP, IN_USER, EXIT_STATUS, CURRENT_AS) still runs at boot through boot_hello, next to the spawned-process model, and finish_exit and SYS_EXIT dispatch carry special cases for it. run_user calls setjmp directly from Rust, which Rust does not support (it has no returns_twice); catch.rs calls it only from an asm trampoline.

**Evidence:**
```rust
// src/syscall_init.rs:618
let rc = unsafe { vibeos_user_setjmp(USER_JMP.as_ptr()) };
```
main.rs:260 calls boot_hello (except under vibefs_crash), which binds a `bound` process to the bootstrap thread and enters ring 3 through run_user. The bound /bin/tests ktest forks spawned children, so both models run in one test.

**Impact:** Two copies of subtle asm and two exit paths to keep in sync. Nothing fails today: the only caller runs on the pinned, stackless bootstrap thread, and the dev-profile build keeps values live across setjmp in callee-saved registers. A second caller, a caller with its own KernelStack (RSP0 would then point at its live frames), or a codegen change would turn the global jmpbuf and the Rust-level setjmp into real bugs.

**Fix:** Keep one setjmp/longjmp in src/arch, reached only through an asm trampoline. Retire the bound model: run /hello and the ring-3 ktests as spawned processes plus wait, then remove the `p.bound` and `in_user()` branches.


#### F083 · errno mapping contradicts Linux: a full disk or full global file table returns EMFILE, and on-disk corruption returns EINVAL

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/proc_init.rs:145-159, src/proc_init.rs:737, src/file_init.rs:326-341, src/fat.rs:38-50, src/fat.rs:70-84, src/vibefs.rs:48-60, src/vibefs.rs:79-93, src/fat.rs:2428, src/syscall.rs:9-48, docs/ROADMAP.md:788, docs/SYSCALL.md:60

**What's wrong:** fs_errno maps FsError::NoSpace to EMFILE (24), but NoSpace has several sources:
- running out of space on the filesystem: vibefs block, inode or dirent allocation; FAT clusters; FAT short names
- FAT's 4 GiB file-size limit (Linux: EFBIG)
- a full global open-file table in alloc_fid (Linux: ENFILE)

FatError and vibefs::Error are duplicate 11-variant enums. Both map Corrupt to FsError::Inval, which becomes EINVAL. lseek on the console returns EINVAL where Linux returns ESPIPE. ENOSPC, ENFILE, EFBIG, ESPIPE, ENOTEMPTY and ELOOP are not defined. The Loop, NotEmpty and NotSupp mappings to EINVAL are also wrong, but no current syscall can reach them.

**Evidence:**
```rust
FsError::NoSpace => EMFILE,                                     // proc_init.rs:152
FsError::Loop | FsError::NotEmpty | FsError::NotSupp => EINVAL, // :157
Error::Inval | Error::Corrupt => FsError::Inval,                // vibefs.rs:81
```
Writing to /vibe until it is full returns -24. A read that hits an extent CRC mismatch returns EINVAL.

**Impact:** These wrong values reach userspace today. Nothing breaks yet because the in-tree assembly programs ignore errno. The code contradicts ROADMAP.md:788 (marked done: "errno values matching Linux") and SYSCALL.md:60 (EMFILE only for a full per-process table). A host test (fat.rs:2428) pins the Corrupt-to-Inval mapping. Once a ported libc depends on these values, fixing them is an ABI change.

**Fix:**
- Add the missing constants.
- Split NoSpace into NoSpace (ENOSPC), TableFull (ENFILE) and TooBig (EFBIG).
- Map Corrupt to EIO, and console lseek to ESPIPE.
- Merge the two FS error enums and move the errno table into portable code with a host test per variant (E2, ROADMAP.md:898).


#### F084 · Two-pass ksyms build moves .text; the panic-test ISO's symbol table is shifted and mis-names frames, and nothing checks it

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** Makefile:66-78, build.rs:28-40, src/panic.rs:133-151, tests/harness/run_e2e.py:94-100, docs/DESIGN.md:228-230, docs/DESIGN.md:1551-1554

**What's wrong:** DESIGN says twice that ".text must not move" between the empty-table link and the filled-table link, and nothing checks it. In pass 1 `KSYMS = &[]`, and the `symtab::lookup(KSYMS, addr)` call in `print_frame_addr` encodes the fat pointer as short immediates (`movl $0x8,%edi; xorl %esi,%esi`). Pass 2 needs a 64-bit address and a length, so the function grows from 0x2a2 to 0x2b2 bytes and every later function shifts. In the release profile, pass 1 also inlines `print_frame_addr` into `dump_common` and pass 2 does not.

**Evidence:**
```make
	... $$(CARGO) build $$(CARGO_FLAGS) $(3)
	python3 scripts/gen_ksyms.py --nm "$$(NM)" $$@ $(2)/vibeos-ksyms.rs
	... VIBEOS_KSYMS=$(2)/vibeos-ksyms.rs ... $$(CARGO) build $$(CARGO_FLAGS) $(3)
```
Regenerating the table from the final panic-variant ELF gives 36 differing entries from `vibeos::panic::finish` onward (table 0x…2370, ELF 0x…2380). The dev prod, gp, ktest and vibefs-crash builds match only because padding absorbs the growth. With `CARGO_PROFILE=release`, 499 of 1106 prod entries are wrong.

**Impact:** The ISO that verifies backtraces prints wrong names: the `panic_fmt` return address shows as `core::str::count::do_count_chars+0xc`. The e2e needles require only `rust_begin_unwind`, which comes before the shift, so the test cannot fail. In release builds, most of the table is wrong.

**Fix:** After pass 2, rerun gen_ksyms on the final ELF and fail on any difference. Make the reference size-invariant, for example `core::ptr::read_volatile(&KSYMS)` or linker-script bounds for a dedicated section. `black_box` alone is not reliable.


#### F085 · The low 512 MiB identity map (VA 0 mapped, first 2 MiB supervisor W+X, all GLOBAL) is never torn down, and no ROADMAP line tracks removing it

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/paging_init.rs:42-47, src/paging_init.rs:458-485, src/pmm_init.rs:99-120, src/addr_space.rs:4-6, docs/DESIGN.md:209, docs/DESIGN.md:494-497, docs/DESIGN.md:563-564, docs/ROADMAP.md:1543

**What's wrong:** The kernel CR3 maps physical [0, 2 MiB) at VA 0 as supervisor writable, executable and GLOBAL, and [2 MiB, 512 MiB) as writable GLOBAL NX. Nothing ever unmaps either range. DESIGN §4.1 says the window "eventually should be" torn down. The ROADMAP tracks only the W^X audit (Phase 18.1), not removing the window, the NULL page or the GLOBAL entries. DESIGN.md:209 (NX unless code is fetched) stops holding after `smp: done`, and the §4.3 flag table leaves out GLOBAL.

**Evidence:**
```rust
.map_page(
    VirtAddr(LOW_ID_BASE),
    PhysAddr(LOW_ID_BASE),
    PageFlags(PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::GLOBAL),
    PageSize::Size2M, MapMode::Fresh, &mut alloc)
```
Kernel threads (idle, shell, drivers, ktests) run on this CR3. Usable frames in [0x1000, 2 MiB), apart from 0x8000, go into the buddy.

**Impact:** On kernel threads, NULL-plus-offset reads and writes hit physical low RAM instead of faulting. Buddy frames below 2 MiB, which may be user pages or page tables, get a supervisor W+X alias. The GLOBAL part is latent: once user code runs in the window on a CPU with CR4.PGE=1, a stale entry causes a spurious SIGSEGV (Intel SDM Vol. 3A §4.10.4.1), not silent corruption.

**Fix:** After `smp: done`, unmap the window except the trampoline page. First check the bootstrap RSP and the post-boot read of 0x8000 at src/smp_init.rs:300-302. Add a ROADMAP line (addr_space.rs:6 already assumes a teardown), add GLOBAL to the DESIGN table, and set CR4.PGE the same way on the BSP and the APs.


#### F086 · VFS and kernfs are unreachable from syscalls; the File API resolves paths through its own FAT/vibefs mount tables and a kernel-global CWD

**Severity:** MEDIUM · **Confidence:** Confirmed

**Location:** src/proc_init.rs:687-712, src/file_init.rs:447-525, src/file_init.rs:281-300, src/fat_init.rs:359-380, src/vibefs_init.rs:347-370, src/file_init.rs:142-188, src/proc_init.rs:50, src/proc_init.rs:833-862, src/fs_init.rs:57-65, src/fs/mod.rs, src/fs/kernfs.rs

**What's wrong:** sys_open calls file_init::open, which goes through vol_walk and join_cwd to walk_abs. walk_abs picks between two separate byte-prefix mount tables (vibefs_init::route and fat_init::route) and never calls fs_init or Vfs. No syscall reaches the VFS, so about 4.1k non-test lines of src/fs are unreachable from userspace. The kernfs mounts on /dev, /proc, /tmp and /sys exist only in the VFS. The shell can list them, but its cat, cp and stat use file_init::open, so only ktests can read kernfs files. Relative paths resolve against a kernel-global CWD that the shell's `cd` writes. Proc.cwd is copied on fork but never read.

**Evidence:**
```rust
fn walk_abs(pb: &[u8]) -> Result<Walked, FsError> {
    let (vv, vs) = vibefs_init::route(pb);
    let (fv, fs) = fat_init::route(pb);
```
A userspace open("/dev/null") walks the empty FAT /dev directory that attach_pseudo_dirs created and gets ENOENT. With O_CREAT, it creates a real FAT file that the kernfs mount then hides.

**Impact:** Processes cannot use devfs null/zero/random (ROADMAP.md:723, marked done) or the VFS descriptor table (ROADMAP.md:702, marked done). A process's relative open() depends on where the debug shell last ran `cd`. The two path resolvers keep diverging, and every new filesystem needs another routing arm.

**Fix:** Complete A3 (ROADMAP.md:896, docs/reviews/issues/A3-vfs-single-dispatch.md): route file_init through the Vfs InodeOps and delete route/routed_rest. Resolve relative paths against Proc.cwd, with chdir from ROADMAP.md:1165. This debt is already tracked; the concrete consequences above are what is new.


### Low


#### F087 · execve from a bound (run_path) process installs an AddressSpace that is never torn down, leaking page tables and frames

**Severity:** LOW · LATENT (first run_path-bound program that calls execve directly) · **Confidence:** Confirmed

**Location:** src/proc_init.rs:919-951, src/proc_init.rs:331-349, src/proc_init.rs:980-989, src/user_init.rs:270-279

**What's wrong:** `sys_execve` has no case for bound processes. A process started by `bind_current` has `space = None` and `borrowed = ptr`, so execve takes `old = None`, installs the new image as `p.space`, and tears nothing down. On exit, the bound path longjmps back to `run_user`. `unbind_current` then overwrites the slot with `Proc::empty()`, which drops the `Box<AddressSpace>` without calling `addr_space_init::teardown`, and `run_path` tears down only the caller-owned pre-exec space.

**Evidence:**
```rust
let old = p.space.take();   // None for a bound proc (proc_init.rs:928)
p.space = boxed.take();
...
*p = Proc::empty();         // unbind_current (proc_init.rs:340)
```
`AddressSpace` has no `Drop` impl, so frames are released only by explicit `teardown` calls. The leak happens when a program started through `user_init::run_path` calls execve itself. Fork creates an owned, unbound space, so an exec from a forked child is not affected.

**Impact:** Each such exec leaks every user frame and page-table frame of the new image. There is no use-after-free: CR3 keeps pointing at the leaked tables until `run_path` loads the kernel CR3. Nothing reaches this today: `/hello` never execs, and `/bin/tests` execs only from forked children.

**Fix:** In `unbind_current`, take `p.space` if it is present, load the kernel CR3 and call `teardown` on it. Alternatively, reject or explicitly handle execve for bound processes.


#### F088 · RFLAGS.AC from ring 3 is not cleared on interrupt or exception entry, so SMAP is off inside those handlers

**Severity:** LOW · **Confidence:** Confirmed

**Location:** src/arch/idt.rs:38-214, src/x86.rs:105-106, src/x86.rs:149-157

**What's wrong:** SYSCALL entry clears RFLAGS.AC through FMASK. Interrupt and exception delivery never clears AC, and no IDT handler executes `clac`. Ring 3 can set AC with `popf`, so any IRQ or exception taken from CPL3 runs its handler with SMAP disabled. `x86::clac()` exists, but only ktest calls it.

**Evidence:**
```rust
/// TF|IF|DF|IOPL|NT|AC. Cleared on `syscall`.
pub const FMASK_SYSCALL: u64 = 0x47700;
```
Every idt.rs handler starts with `gs_enter` and none executes `clac`. SDM Vol 2A, INT n/INTO/INT3/INT1 Operation: interrupt-gate delivery clears TF, VM, RF, NT and IF, but never AC. See also SDM Vol 3A §6.12.1.3 (§7.12.1.3 in newer editions). SDM Vol 3A §4.6: with CR4.SMAP=1, supervisor accesses to user pages are allowed while EFLAGS.AC=1.

**Impact:** This only weakens defense in depth. The kernel reads user memory through `AddressSpace` page walks (for example `copy_user_str`), not by dereferencing user virtual addresses directly, so nothing is exposed today. An accidental direct user dereference in a handler, including the `try_user_fault` → `finish_exit` path, would not trap. ROADMAP.md:772 and :855 mark SMAP hardening done, but the entry-path half is missing.

**Fix:** Call `x86::clac()` (gated on `smap_live`) at the start of every IDT handler reachable from CPL3. `gs_enter` is a natural place when the frame's CS RPL is 3. This matches Linux's `ASM_CLAC` in `idtentry`.


#### F089 · TSS pointers (BSP and AP) come from shared references but get written on every context switch; doc comment wrongly says hardware updates RSP0

**Severity:** LOW · **Confidence:** Confirmed

**Location:** src/arch/gdt.rs:183-186, src/syscall_init.rs:304-314, src/smp_init.rs:182, src/smp_init.rs:209-210, src/smp_init.rs:223, src/wait.rs:48-50, src/thread_init.rs:131-132

**What's wrong:** `bsp_tss_ptr` takes a pointer from `&Bsp` (via `BootCell::get`) and casts it to `*mut Tss`. `Tss` holds only plain integer fields and no `UnsafeCell`, so the pointer has read-only provenance, yet `set_rsp0_for` writes through it. The AP path has the same problem: smp_init.rs:182 casts `a.tables.tables.as_ref()` to `*mut CpuTables`, `ap_entry` turns it into `&mut`, and the Box is later moved into `LIVE_TABLES`. `WaitQueue::cookie` (taken from `&self`) is turned back into `&mut WaitQueue` at thread_init.rs:132, which is the same class of bug through exposed provenance. The doc comment is also wrong: the CPU only reads RSPn/ISTn from a 64-bit TSS and never writes them (Intel SDM Vol. 3A §8.7, "Task Management in 64-bit Mode"). Software writes RSP0.

**Evidence:**
```rust
/// TSS for the BSP. Call after [`init_bsp`]. Hardware updates RSP0.
pub fn bsp_tss_ptr() -> *mut Tss {
    core::ptr::addr_of!(BSP.get().tables.tss) as *mut Tss
}
// syscall_init.rs:312
unsafe { (*cpu.tss).set_rsp0(top) };
```
Every context switch after `init_bsp` writes through this pointer.

**Impact:** This is UB under both Stacked Borrows and Tree Borrows. No miscompile is known: the shared reference is short-lived and is never a `noalias` argument that stays live across the writes. The comment misleads readers into treating the TSS as hardware-owned.

**Fix:** Take the pointer from `UnsafeCell` provenance: `addr_of_mut!((*BSP.as_ptr()).tables.tss)`. For APs, take it from the owned Box (`addr_of_mut!(*a.tables.tables)` or `Box::into_raw`/`leak`). Derive the WaitQueue cookie from a `*mut`, and fix the doc comment.


#### F090 · EFER.NXE is set without checking CPUID NX support

**Severity:** LOW · LATENT (real hardware) · **Confidence:** Confirmed

**Location:** src/paging_init.rs:505-508, src/arch/trampoline.S:58-61

**What's wrong:** The BSP sets EFER.NXE, and the AP trampoline ORs in 0x900 (LME|NXE), without checking CPUID.80000001H:EDX[20]. Limine already enables NX when it is available, so the BSP write runs only when NX is absent. That is exactly the case where WRMSR raises #GP.

**Evidence:**
```rust
let efer = x86::rdmsr(x86::IA32_EFER);
if efer & x86::EFER_NXE == 0 {
    unsafe { x86::wrmsr(x86::IA32_EFER, efer | x86::EFER_NXE) };
}
```
This fires on a CPU without NX: firmware with XD disabled (IA32_MISC_ENABLE[34] clears the CPUID bit), or a VM CPU model started with `-nx`. SDM Vol 3A §4.1.4: NXE may be set only if CPUID.80000001H:EDX.NX[20]=1. SDM Vol 2B WRMSR: setting reserved bits raises #GP(0). AMD APM Vol 2 §3.1.7 says the same.

**Impact:** `paging_init::install` (main.rs:156) runs before `arch::idt::init` (main.rs:194). The #GP therefore has no handler, and the machine triple-faults and resets with no serial output. QEMU's default CPU models expose NX, so nothing hits this today.

**Fix:** Check CPUID.80000001H:EDX[20] before the write, and halt with a clear serial message if NX is absent. Running without NX instead would also require clearing bit 63 in every PTE the kernel builds, because that bit is reserved when NXE=0 (SDM Vol 3A §4.5), and the trampoline would need the same change.


#### F091 · Guard-exit sti, x86::sti/cli and raw cli/sti use options(nomem), so they are not compiler barriers

**Severity:** LOW · **Confidence:** Confirmed

**Location:** src/x86.rs:299-305, src/x86.rs:491-500, src/cell.rs:119-125, src/console_init.rs:101-117, src/thread_init.rs:344-354, src/syscall_init.rs:595-601

**What's wrong:** A `nomem` block lets the compiler assume it touches no memory, so loads and stores can move across it (Rust Reference, inline assembly options). Entering an IRQ-off region is safe: `InterruptGuard::enter` (x86.rs:284-290) has no options and keeps the default memory clobber. The guard's exit `sti`, `x86::sti`/`x86::cli`, and the raw `cli`/`sti` in `wait_key` and `halt_if_idle` are all `nomem`. No miscompile has been seen, but the binary has not been disassembled to check.

**Evidence:**
```rust
crate::per_cpu_init::irq_nest_leave();
if self.restore {
    unsafe { asm!("sti", options(nomem, nostack)) };
}
```
The compiler may legally move IrqCell's unlock `store(0, Release)` or the Relaxed `irq_nest` decrement to after the `sti`. It may also move the re-check that follows `cli` in `wait_key` and `halt_if_idle` to before the `cli`.

**Impact:** If that happens, an IRQ on the same CPU could see `owner == me` and hit a spurious "IrqCell re-entry" panic, or read a stale nest count. A re-check moved above `cli` reopens the lost-wakeup window, which lasts at most until the next timer tick.

**Fix:** Remove `nomem` from `x86::sti`, `x86::cli`, the guard's `sti`, and the raw `cli`/`sti`, but keep it on `sti; hlt`. This matches Linux's memory clobber on these operations. Also replace `flags_if_on` with `x86::interrupts_enabled()`: its `pushfq` is declared `nostack`, which breaks the asm! contract and is harmless only because the red zone is disabled.


#### F092 · disarm_timer writes IA32_TSC_DEADLINE unconditionally, including on the periodic-fallback failure path of CPUs without TSC-deadline

**Severity:** LOW · LATENT (real hardware without TSC-deadline) · **Confidence:** Confirmed

**Location:** src/apic_init.rs:437-445, src/apic_init.rs:595-601

**What's wrong:** `disarm_timer` always starts with `wrmsr(IA32_TSC_DEADLINE, 0)`. `prove()` also calls it on the periodic-mode failure path (line 600). That path runs when `want_td` is false, which means the CPU does not implement MSR 0x6E0. The call at line 583 is safe because its branch runs only when `want_td` is true.

**Evidence:**
```rust
fn disarm_timer(va: u64) {
    unsafe { x86::wrmsr(IA32_TSC_DEADLINE, 0) };
```
Trigger: the CPU lacks TSC-deadline and the periodic LAPIC timer does not fire within `PROVE_MS` (50 ms). Intel SDM Vol 4 lists MSR 6E0H only "If CPUID.01H:ECX.[24] = 1". Per SDM Vol 2B (WRMSR), a write to an unimplemented MSR raises #GP(0). `x86::wrmsr` (src/x86.rs:81) has no fault fixup. QEMU TCG ignores unknown MSR writes, and the `qemu64,-tsc-deadline` target (Makefile:248) only runs the success path, so CI cannot catch this.

**Impact:** Instead of falling back to the PIT, the kernel takes a kernel-mode #GP and halts in `exception_halt`. The trigger is rare and exists only on real hardware.

**Fix:** Remove the MSR write, or guard it on `want_td` / `mode == TscDeadline`. The LVT write that follows already disarms the timer, because leaving TSC-deadline mode disarms it (SDM Vol 3A §11.5.4.1).


#### F093 · The IDT error-code split is a hand-typed literal list; the host test pins vectors::pushes_error_code, which nothing uses, and set_handler accepts any vector

**Severity:** LOW · **Confidence:** Confirmed

**Location:** src/vectors.rs:55-59, src/vectors.rs:63-100, src/vectors.rs:128-136, src/arch/idt.rs:255-272, src/arch/idt.rs:366-368

**What's wrong:** `install_defaults` hardcodes the error-code and no-error vector lists as literals. `pushes_error_code` is referenced only by its own test, so `error_code_set_matches_sdm` compares a dead function against another literal and never checks the IDT. The public `set_handler`, called at runtime by `kbd_init` and the `irq_init` pool, always installs a no-error gate without checking the vector. `NAMED` also omits `MC` (0x12), so the uniqueness test does not cover it.

**Evidence:**
```rust
install_err!(8, 10, 11, 12, 13, 14, 17, 21, 29, 30);
...
pub fn set_handler(vec: u8, h: ...) { set_noerr(vec, h, 0); }
```
`grep pushes_error_code` finds only src/vectors.rs:57 and its test. The current lists are correct: they partition 0..=255, and the error set matches Intel SDM Vol 3A §6.15 Table 6-1 plus AMD #VC(29) and #SX(30).

**Impact:** There is no bug today. The test gives false assurance, and a future edit can install a no-error gate on an error-code vector, which misaligns the iret frame.

**Fix:** Generate both install lists from `pushes_error_code` (a const table, or a const assert over 0..=31). Add `debug_assert!(!pushes_error_code(vec))` in `set_handler`, and add `MC` to `NAMED`.


#### F094 · 8259 presence is read from FADT IAPC_BOOT_ARCH bit 0 (LEGACY_DEVICES) instead of MADT PCAT_COMPAT, which is parsed but never used

**Severity:** LOW · **Confidence:** Confirmed

**Location:** src/acpi.rs:414-419, src/acpi.rs:306, src/acpi.rs:870, src/pic.rs:91-97, src/arch/pic.rs:1-5, src/arch/pic.rs:17-38, src/main.rs:185-191, src/main.rs:214-216, docs/DESIGN.md:793-798, docs/DESIGN.md:1023, docs/ROADMAP.md:280

**What's wrong:** `legacy_8259()` returns `iapc_boot_arch & 1`, and `remap_and_mask` skips the ICW sequence when that bit is clear. In ACPI, bit 0 is LEGACY_DEVICES (user-visible LPC/ISA devices) and says nothing about an 8259. The dual-8259 indicator is MADT Flags bit 0, PCAT_COMPAT. It is parsed at acpi.rs:306 but only a test reads it. QEMU's PIIX and Q35 FADTs set only bit 1 (8042), so the boot remap is always skipped, and main.rs:216 calls `program()` unconditionally to make up for it. DESIGN §5.5, the DESIGN §7.1 table and ROADMAP §2.3 repeat the wrong meaning, and acpi.rs:870 pins it in a test.

**Evidence:**
```rust
/// DESIGN §7.1: `iapc_boot_arch` bit 0, used by §2.3 to skip the 8259.
pub fn legacy_8259(self) -> bool {
    self.iapc_boot_arch & 1 != 0
}
```
ACPI 6.5 §5.2.9.3 Table 5.11 (bit 0 LEGACY_DEVICES) and §5.2.12 Table 5.20 (MADT Flags bit 0 PCAT_COMPAT) say this. ACPICA `actbl.h`/`actbl2.h` agree.

**Impact:** No functional impact today. The unconditional `program()` runs before `sti` (main.rs:223). The documented "skip the PIC when absent" behaviour never happens, and the ports are always written. If someone makes `program()` honour the documented FADT check, QEMU would boot with an un-remapped 8259, and unmasking IRQ0 would deliver the PIT on vector 0x08 (#DF).

**Fix:** Gate `should_program` on `madt.pcat_compat` (no MADT means program the PIC). Delete `legacy_8259` and merge `remap_and_mask`/`program` into a single remap before `lidt`. Correct DESIGN §5.5 and §7.1, ROADMAP §2.3 and the acpi.rs:870 test.


#### F095 · IOAPIC VER is not validated: a non-decoding MADT IOAPIC gets max_index=255, can shadow a real IOAPIC's GSIs, and drives the redirection index past pin 119

**Severity:** LOW · LATENT (buggy firmware: MADT lists a non-decoding IOAPIC) · **Confidence:** Confirmed

**Location:** src/apic.rs:241-248, src/apic.rs:319-329, src/apic_init.rs:160-183, src/apic_init.rs:185-205, src/apic_init.rs:229-239

**What's wrong:** `enum_ioapics` takes `max_index = (VER >> 16) & 0xFF` without validation. An MADT IOAPIC address that nothing decodes reads VER=0xFFFFFFFF, which gives a phantom IOAPIC with `max_index = 255`. `find_ioapic` returns the first IOAPIC whose range covers a GSI, so a phantom listed before a real IOAPIC shadows the real one's GSIs. `ioapic_redir_regs` also wraps: pins 120..127 map to registers 0x00..0x0F, and pins 128+ alias back onto pins 0..119.

**Evidence:**
```rust
let low = IOAPIC_REDIR_BASE.wrapping_add(pin.wrapping_mul(2));
// mask_all_pins:
while pin <= io.max_index { ... write_redir(...) }
```
Per the Intel 82093AA datasheet §3.2, IOREGSEL is 8-bit, so no real IOAPIC can report 120 or more entries. The wrap onto ID/ARB therefore only touches an address with no registers behind it. The originally claimed ID/ARB corruption of a real IOAPIC is not reachable.

**Impact:** With a buggy MADT, `route_gsi`/`unmask_gsi` for the shadowed GSIs write into the void, and those device IRQs never arrive.

**Fix:** Treat VER==0xFFFFFFFF (or a version byte of 0xFF) as an absent IOAPIC and skip it. Clamp `max_index` to 119.


#### F096 · LINT1 masked on every CPU and MADT NMI entries (types 3/4) ignored, so platform/external NMIs are silently dropped

**Severity:** LOW · LATENT (real hardware) · **Confidence:** Confirmed

**Location:** src/apic_init.rs:148-150, src/acpi.rs:323-363, src/time_init.rs:458-459, docs/DESIGN.md:244

**What's wrong:** `enable_lapic` masks LINT1 on the BSP and on every AP, and no later code reprograms it. `parse_madt` drops type 3 (NMI Source) and type 4 (Local APIC NMI) entries through `_ => {}`. This contradicts the repo's own intent: `time_init` deliberately re-enables chipset NMI, and DESIGN §2.5 says NMI stays on IST for real NMIs and "Nothing is silently swallowed".

**Evidence:**
```rust
lapic_write(va, LAPIC_LVT_LINT0, LVT_MASKED);
lapic_write(va, LAPIC_LVT_LINT1, LVT_MASKED);
```
Intel SDM Vol 3A §11.5.1: an LVT mask bit of 1 inhibits reception. ACPI 6.5 §5.2.12.7 defines the Local APIC NMI structure that names the NMI pin, and QEMU's MADT declares LINT1. In QEMU, `apic_external_nmi` delivers through LINT1 and returns early when that LVT is masked (hw/intc/apic.c).

**Impact:** Chipset error NMIs (SERR/IOCHK, watchdog) and debug NMIs are silently discarded, and the IST `nmi()` handler never runs for them.

**Fix:** Program LINT1 with NMI delivery mode (0b100 << 8) on the BSP only, leaving APs masked (as Linux `setup_local_APIC` does). Parse MADT type 4/0x0A to validate or override the pin, polarity and trigger, and honour type 3.


#### F097 · Kernel-shell poweroff/reboot use QEMU-specific constants and ignore FADT PM1 control blocks and RESET_REG_SUP

**Severity:** LOW · LATENT (real hardware) · **Confidence:** Confirmed

**Location:** src/shell_init.rs:357-373, src/shell_init.rs:375-400, src/shell_init.rs:402-424, src/acpi.rs:421-449

**What's wrong:** `try_acpi_sleep_s5` writes a fixed SLP_TYP of 5 to SLEEP_CONTROL_REG, a register that exists only on HW-reduced platforms. SLP_TYPx must come from the `\_S5` AML package; 5 is simply QEMU GED's value. On every other system, poweroff falls back to writing 0x2000 to ports 0x604 and 0xB004, the QEMU/Bochs PM1a_CNT locations. It does this because `parse_fadt` never reads PM1a/PM1b_CNT_BLK, their X_ variants, or Flags (offset 112). `try_acpi_reset` never checks Flags bit 10 RESET_REG_SUP. It widens the write to 16 or 32 bits and ignores PCI-config reset registers. It writes memory-space reset registers at `HHDM_BASE + address`, which faults above `physmap_extent()`. Only `kernel_shell` debug builds can reach this code.

**Evidence:**
```rust
x86::outw(0x604, 0x2000);                                  // :369
x86::outw(0xB004, 0x2000);                                 // :370
write_gas(fadt.sleep_control, (5 << 2) | (1 << 5));        // :399
let va = paging_init::HHDM_BASE.wrapping_add(gas.address); // :418
```
On QEMU `pc` (FADT rev 1) and `q35` the ACPI paths never run; the fixed ports and the 8042 pulse do the work. See ACPI 6.5 §4.8.3.7 (SLP_TYPx comes from `\_Sx`). See also §5.2.9 Table 5.9 (RESET_REG is 8-bit at offset 0, in I/O, memory or PCI config space) and Table 5.10 (Flags bit 10).

**Impact:** On real hardware, poweroff does nothing useful and writes 0x2000 to two arbitrary I/O ports. Reboot may write a reset register the platform marks as unsupported, or page-fault on a high MMIO reset register. The 8042 and 0xCF9 fallbacks usually still reset the machine.

**Fix:** Parse Flags, PM1a/PM1b_CNT_BLK, the X_ variants and HW_REDUCED_ACPI. Put the fixed-port shutdown behind an explicit QEMU check until an AML `\_S5` lookup exists. Honour RESET_REG_SUP, always write 8 bits, support PCI config space, and map MMIO reset registers UC.


#### F098 · TickClock::write has no release fence after the odd sequence bump; the memory model allows torn (tick, tsc) reads outside x86

**Severity:** LOW · LATENT (aarch64 port, LL/SC codegen) · **Confidence:** Confirmed

**Location:** src/time.rs:190-201, src/time.rs:204-219, src/time.rs:230-251

**What's wrong:** The writer does `fetch_add(1, AcqRel)`, then two Relaxed payload stores, then `fetch_add(1, Release)`. The reader's `fence(Acquire)` can synchronize only with a release fence or release store placed before the payload stores it read. The AcqRel RMW is not a fence, and its release half orders only earlier accesses. So nothing forces the reader's second `seq` load to see the odd value, and `s1 == s2` with a new payload is allowed. The comment "Acquire keeps the payload stores after seq is odd" reasons only about the RMW's load half.

**Evidence:**
```rust
self.seq.fetch_add(1, Ordering::AcqRel);
self.tick.store(tick, Ordering::Relaxed);
self.tsc.store(tsc, Ordering::Relaxed);
self.seq.fetch_add(1, Ordering::Release);
```
The code is safe on x86 because `lock xadd` is a full barrier. On aarch64 LL/SC (`ldaxr`/`stlxr`), later plain stores can pass the store-exclusive. LSE `LDADDAL` is fully ordered, so the aarch64-apple-darwin host test cannot catch this. See C++ [atomics.fences], which Rust follows, and Boehm, "Can Seqlocks Get Along with Programming Language Memory Models?"

**Impact:** After an aarch64 port, a reader can accept a new tick with an old tsc, overshooting by about one tick (~1 ms). `monotonic_max` then holds published time flat until real time catches up, so the error is not permanent. The opposite tear is hidden by the negative-delta branch.

**Fix:** There is a single writer (the CPU-0 ISR), so use `seq.fetch_add(1, Relaxed); fence(Release);`, then the payload stores, then `seq.fetch_add(1, Release)`, and fix the comment. A separate aarch64 item: dma.rs:181-203 needs outer-shareable barriers.


#### F099 · set_threaded accepts top: None and dispatch EOIs without oneshot masking, so a level INTx route whose top half does not ack storms the destination CPU

**Severity:** LOW · LATENT (first INTx-only device with a threaded handler) · **Confidence:** Likely

**Location:** src/irq_init.rs:103-130, src/irq_init.rs:173-188, src/irq_init.rs:274-295

**What's wrong:** `set_threaded` accepts `top: None`. For threaded vectors, `dispatch` marks the vector pending and EOIs unconditionally without masking the GSI. DESIGN §5.4 says the top half does "ack / mask / wake", but nothing enforces an acking top half or provides oneshot masking for the natural "bottom half acks the device" pattern. Linux rejects this configuration: `__setup_irq` refuses a NULL handler without ONESHOT.

**Evidence:**
```rust
if top != 0 { h(); }
... s.th.pending[i] = true; ...
apic_init::eoi_for(vec);
```
On a `Trigger::Level` IOAPIC route, the EOI clears Remote IRR and the IOAPIC re-sends while the device still asserts the line (82093AA datasheet §3.2.4). MSI-X routes are edge-triggered and unaffected, so today's virtio-rng and virtio-blk (MSI-X, acking top halves) are safe, and no production caller uses `route_intx`.

**Impact:** The destination CPU re-enters `dispatch` after every `iretq` and near-livelocks. If `route_intx` targets `threaded_cpu()`, the bottom half that would ack the device never runs.

**Fix:** For `Route::IoApic` threaded vectors, mask the GSI in `dispatch` before EOI and unmask it after the bottom half completes. Alternatively, reject `top: None` for level routes.


#### F100 · now_us monotonicity ktests cannot fail: the LAST_NS fetch_max clamp makes now_us monotonic by construction

**Severity:** LOW · **Confidence:** Confirmed

**Location:** src/ktest.rs:1143-1178, src/ktest.rs:111-112, src/time.rs:159-162, src/time_init.rs:68-75, src/time_init.rs:331-343, docs/DESIGN.md:949-953

**What's wrong:** `test_now_us_monotonic` and `test_now_us_under_yields` assert that successive `now_us()` values never decrease. But `now_ns()` returns `LAST_NS.fetch_max(n).max(n)`, which can never be below a value it returned earlier. The fail branch is unreachable, so a seqlock tear or an interpolation regression cannot fail either test. The "under yields" variant also never yields: it calls `x86::hlt_once()` every 200 iterations on a single thread. It therefore does not provide the cross-thread coverage that DESIGN §6.4 calls "the real coverage".

**Evidence:**
```rust
pub fn monotonic_max(last: &AtomicU64, n: u64) -> u64 {
    last.fetch_max(n, Ordering::Relaxed).max(n)
}
```
`publish_ns` is the only writer of `LAST_NS` and every write is a `fetch_max`. By RMW coherence each later return on the same thread is ≥ the earlier one, so `if n < last` (ktest.rs:1148, 1165) is never taken.

**Impact:** Kernel-side regressions in the seqlock or TSC interpolation go undetected. Only host tests exercise tearing (`TickClock::now_with`, time.rs:231), and they bypass `LAST_NS`. Latching a forward overshoot into `LAST_NS` is documented intent (time_init.rs:69-70), not a separate defect.

**Fix:** Under `cfg(feature = "kernel_tests")`, expose the unclamped clock reading and assert on that, or compare `now_ns` against an independently published timestamp within a tolerance. Run the readers on several threads or CPUs, with explicit yields while the timer fires.


#### F101 · Kernel-half PML4 entries are copied once per address space; a top-level slot created later never reaches existing user PML4s

**Severity:** LOW · LATENT (first kernel-half PML4 slot created after boot_hello) · **Confidence:** Confirmed

**Location:** src/paging.rs:598-611, src/addr_space.rs:133-145, src/paging_init.rs:120-150, src/acpi_init.rs:51-69, src/paging_init.rs:387-521

**What's wrong:** `AddressSpace::new` copies PML4[256..512) from the kernel root once. `ioremap` and `map_gap` map through `current_mapper()`, which is the kernel root. `install` does not pre-create the heap (384), KVA (416) or ioremap (448) slots. So a top-level slot created after a user AS exists is missing from that AS. Today the rule "every kernel-half PML4 slot exists before the first AS" holds only because of boot ordering. DESIGN §4 does not state it, and no code enforces it.

**Evidence:**
```rust
let mut i = KERNEL_PML4_FIRST;
while i < PTES_PER_TABLE {
    let e = unsafe { src_ptr.add(i).read_volatile() };
    unsafe { dst_ptr.add(i).write_volatile(e) };
```
No current path triggers it. `map_gap` runs only from `acpi_init::init` (main.rs:164). Boot PCI enumeration has already ioremapped ECAM before any later access. Heap and KVA growth stays inside slots created at boot.

**Impact:** Suppose code later creates a new top-level kernel slot after boot_hello. Any kernel access to that region with a user CR3 loaded, from a syscall or an IRQ, takes a kernel #PF and panics.

**Fix:** In `install`, allocate zeroed PDPTs for every kernel-half slot. All of 256..511 costs about 1 MiB; covering only the slots the layout uses costs less. Document the invariant in DESIGN §4. Alternatively, make the kernel mapper panic if it allocates a PML4-level table after the first AS is created.


#### F102 · map_anon error path unmaps a pre-existing leaf on AlreadyMapped and leaks its frame

**Severity:** LOW · LATENT (user-half mappings installed outside the region table) · **Confidence:** Confirmed

**Location:** src/addr_space.rs:219-228

**What's wrong:** When `map_page` fails inside `map_anon`, the error path calls `self.mapper.unmap_page(page_va)`. On `AlreadyMapped`, that removes a pre-existing leaf this call never installed. It discards the returned PA, which leaks that frame, and it does no TLB invalidation. For every other error, `map_page` returns before writing the leaf, so the call does nothing. Freeing `pa` is correct, because that frame was never mapped.

**Evidence:**
```rust
if let Err(e) = map_rc {
    let _ = unsafe { self.mapper.unmap_page(page_va) };
    unsafe { alloc.free_frame(pa) };
    let _ = unsafe { self.unmap_free(va, mapped, alloc) };
```
In `MapMode::Fresh`, `map_page` returns `AlreadyMapped` when a present leaf exists (paging.rs:313-316). This path runs when a user-half leaf exists that no `Region` covers, because `overlaps()` (addr_space.rs:376) checks only the region table.

**Impact:** Nothing reaches this today. Every user-half leaf comes from `map_anon`, which records a Region, and its own rollback removes every leaf it installed. Once another path installs untracked user leaves, a `map_anon` that collides with one silently unmaps a live page and leaks its frame. Separately, the rollback's `unmap_free` decrements `user_frames` for pages that were never counted, since `+= pages` happens only on success. Only tests read that stat.

**Fix:** Delete the `unmap_page(page_va)` call and keep `free_frame(pa)` plus the rollback of `[va, va+mapped)`. Stop the rollback from decrementing `user_frames` for pages that were never counted.


#### F103 · Buddy::order_for: unchecked next_power_of_two wraps in release (panics in dev) for sizes above 2^63

**Severity:** LOW · LATENT (first caller passing an unvalidated u64 size, e.g. a user mmap or DMA length) · **Confidence:** Confirmed

**Location:** src/pmm.rs:220-241 (rounding at :228); Cargo.toml:38-47; Makefile:14

**What's wrong:** `order_for` rounds the request up with `next_power_of_two()` and never uses the checked variant. When bytes > 2^63 the rounding overflows. Under the default dev profile (`overflow-checks = true`) the kernel panics. Under the release profile `need_size` wraps to 0, and the function returns the alignment's order where it should return `None`. Sizes from 2^22 up to 2^63 are correctly rejected by the `MAX_ORDER` check.

**Evidence:**
```rust
let need_size = bytes.max(PAGE_SIZE).next_power_of_two();
let need_align = align.max(PAGE_SIZE);
let need = need_size.max(need_align);
```
In a release build, `order_for((1<<63)+1, 0)` returns `Some(0)`. No caller can produce such a size today. Callers pass virtio layout sizes (16-bit `queue_size`), `N_SLOTS*SLOT_STRIDE` or small constants (src/virtio_init.rs:301,305; src/virtio_blk_init.rs:772,883).

**Impact:** Harmless today. Once an unvalidated size reaches this path, a release kernel's `allocate_constrained` hands back a 4 KiB block for a huge request, and `DmaBuffer::from_phys` records the huge length against it. A dev kernel panics instead.

**Fix:** Use `checked_next_power_of_two()?`, or reject `bytes > PAGE_SIZE << MAX_ORDER` before rounding. Add host tests for `u64::MAX` and `(1<<63)+1`.


#### F104 · patch_physmap_uc marks whole 2 MiB physmap leaves UC with no RAM check, and its walk skips a trailing leaf on unaligned ranges

**Severity:** LOW · LATENT (real hardware) · **Confidence:** Confirmed

**Location:** src/paging.rs:440-487, src/paging_init.rs:159-174, src/pci_init.rs:83-104, docs/DESIGN.md:548-550 (§4.3), docs/DESIGN.md:1577 (§9.2)

**What's wrong:** Every physmap leaf below `map_end` is 2 MiB, and the routine ORs PCD|PWT into the whole leaf without checking that it holds only MMIO. Separately, `off` advances by the leaf size from an unaligned `phys`, so a range crossing a leaf boundary misses its last leaf. That breaks the doc comment's promise to patch "every leaf covering [phys, phys+len)". Whole-leaf patching is the documented DESIGN rule, which assumes the leaf holds no RAM.

**Evidence:**
```rust
let va = VirtAddr(hhdm_start.0 + phys.0 + off);
let patched = entry | PageFlags::PCD | PageFlags::PWT;
unsafe { entry_ptr.write_volatile(patched) };
off = off.saturating_add(size.bytes());
```
With phys=0x1F0000 and len=0x20000, only [0, 2M) is patched. No current caller reaches the skip: BARs are size-aligned (PCI Local Bus Spec 3.0 §6.2.5.1), and the ECAM/LAPIC/IOAPIC/HPET callers pass aligned 4 KiB pages. The RAM case needs usable RAM in the same 2 MiB frame as a BAR below `map_end`, for example a BAR at an odd-MiB TOUUD on a machine with 8 GiB or less.

**Impact:** In that rare layout, frames the buddy allocator hands out (and user PTEs map WB) also get a UC physmap mapping. Intel does not support one page mapped with different memory types (SDM Vol. 3A §12.12.4), so behaviour is undefined and a #MC is possible. QEMU ignores cache attributes, so tests show neither problem.

**Fix:** Walk leaves from align_down(phys, leaf) to align_up(phys+len, leaf), and add a host test with an unaligned range that crosses 2 MiB. Split a leaf to 4 KiB before patching if it overlaps usable RAM, and update DESIGN §4.3 and §9.2 in the same commit.


#### F105 · Physmap and low identity map give kernel .text/.rodata a writable alias, so W^X holds only per VA

**Severity:** LOW · LATENT (Phase 18.1 kernel memory protection) · **Confidence:** Confirmed

**Location:** src/paging_init.rs:443-456, src/paging_init.rs:473-484, src/paging_init.rs:395-430, src/paging_init.rs:584-591, src/paging.rs:774-786, docs/DESIGN.md:559

**What's wrong:** The image's high VA is mapped per section: .text is RO+X and .rodata is RO+NX. The physmap, however, maps [0, map_end) as one writable+NX range, and `physmap_extent` always includes `kernel_phys.end`. So code and rodata frames also get a writable alias at HHDM_BASE+phys. With the default 128 MiB guest, the image also sits in the low identity map's RW+NX tail, which exists only in the kernel CR3. No single mapping is both writable and executable. The gap is that code and rodata are writable through a second VA. DESIGN §4.3 documents this ("Physmap: present, global, writable, NX"), and ROADMAP 18.1 schedules the fix.

**Evidence:**
```rust
map_range(VirtAddr(HHDM_BASE), PhysAddr(0), map_end,
          paging::physmap_flags(), MapMode::Fresh, &mut alloc)
// physmap_flags     = PRESENT | WRITABLE | GLOBAL | NX
// kernel_text_flags = PRESENT | GLOBAL
```
User address spaces copy PML4[256..512), so this supervisor-only alias is present under every CR3. CR0.WP is set, which protects only the image VA.

**Impact:** This does nothing on its own. It lets any future kernel write bug patch code or rodata, which is what the 18.1 audit is meant to rule out. Today, DMA without an IOMMU already bypasses page tables, which limits the practical difference.

**Fix:** In the physmap and the low-identity tail, map the image's physical span with per-section permissions: .text, .rodata and limine_requests RO, while .data and .bss stay RW. Split to 4 KiB at the unaligned image edges. Update the DESIGN §4.3 flag table and add the 18.1 boot-time audit.


#### F106 · finish_exit frees the process PML4 but leaves tcb.as_cr3 pointing at the freed frame

**Severity:** LOW · LATENT (preemptible syscall/exit paths, or threads sharing an address space) · **Confidence:** Confirmed

**Location:** src/proc_init.rs:1024-1029, src/syscall_init.rs:317-329, src/thread_init.rs:738-745

**What's wrong:** `finish_exit` frees the process PML4 but never updates the exiting thread's `tcb.as_cr3`, which still points at the freed frame. Every other path that replaces or drops an address space keeps the field in sync. execve sets it to the new root before teardown (proc_init.rs:948), `run_user`'s return path zeroes it (syscall_init.rs:623), and `unbind_current` calls `set_pid_cr3(tid, 0, 0)` (proc_init.rs:347).

**Evidence:**
```rust
if let Some(space) = old {
    crate::arch::gs::force_kernel();
    addr_space_init::load_kernel_cr3();
    clear_as();
    addr_space_init::teardown(*space);
}
```
When a thread is switched in, `switch_cr3_for` writes any non-zero `tcb.as_cr3` straight into CR3.

**Impact:** This is harmless today. `finish_exit` always runs with IF=0 (syscall FMASK and interrupt gates), the Dead TCB is never re-enqueued, and `fill_tcb` overwrites `as_cr3` before the slot is reused. IF=0 is not a syscall invariant, though: console `sys_read` comes back from `wait_key` with IF=1 (console_init.rs:103-117). If a preemption ever lands between teardown and `exit_current`, switching back to the thread loads a freed frame as the PML4. If that frame has been reused as a user page, ring 0 ends up running on page tables that userspace controls.

**Fix:** In `finish_exit`, call `thread_init::set_pid_cr3(tid, 0, 0)` (or zero `as_cr3`) before `teardown`, as the other paths do.


#### F107 · vunmap frees the full VA span while unmap_shootdown silently unmaps at most 32 pages

**Severity:** LOW · LATENT (first vunmap caller outside ktest with nframes > 32 or a count different from the vmap'd one) · **Confidence:** Confirmed

**Location:** src/kva_init.rs:186-194, src/kva_init.rs:211-214, src/kva_init.rs:158-160

**What's wrong:** `unmap_shootdown` silently clamps its page count to `MAX_UNMAP` (32). `vunmap` still returns `nframes * PAGE_SIZE` of VA to the KVA free list. `vunmap` also keeps no record of the mapped length and trusts the caller's `nframes`. A count above 32, or below the count that was vmap'd, therefore leaves pages mapped under VA that has already been freed.

**Evidence:**
```rust
pub fn vunmap(va: VirtAddr, nframes: usize) {
    unmap_shootdown(va, nframes, false); // n.min(MAX_UNMAP) inside
    ... k.free(va.as_u64(), nframes as u64 * PAGE_SIZE)
```
`vmap` rejects more than 32 frames (kva_init.rs:159), and the only vmap/vunmap caller is a 2-frame ktest (src/ktest.rs:530,539), so the matched pair is safe today.

**Impact:** When the count does not match, stale mappings and TLB entries survive over VA that gets reissued. The next `alloc_va`, `vmap` or `alloc_guarded_stack` on that range then either fails with `AlreadyMapped` or maps a second owner onto the same VA.

**Fix:** Assert `nframes != 0 && nframes <= MAX_UNMAP` in `vunmap`, or unmap in 32-page chunks so the unmapped span always equals the freed span. Better still, have `vmap` return a handle that records its length. Replace the silent clamp in `unmap_shootdown` with an assert.


#### F108 · Lock-rank checker misses spinlocks held across a switch, same-rank nesting, and IrqCell ordering

**Severity:** LOW · **Confidence:** Confirmed

**Location:** src/sync_init.rs:151-187, src/sync_init.rs:81-91, src/lock.rs:24-34, src/cell.rs:107-126, src/thread_init.rs:274, docs/DESIGN.md:192

**What's wrong:** `HELD` is a per-CPU rank bitmask, and the checker has three gaps.
1. Nothing asserts that `HELD` is empty before `switch_now`, so a spinlock held across `schedule` goes undetected, and its stale bit blames the next thread on that CPU.
2. Equal ranks may nest, and `release_mask` clears a single bit. ABBA between two `RANK_DEVICE` locks is therefore invisible, and the inner release (or a failed `try_lock`) clears the outer lock's bit.
3. `IrqCell::with` is unranked, and its spin loop does not call `service_incoming`. Yet several IrqCells (proc TABLE, KVA, DEFERRED, work ST, IRQ state, LOG) act as cross-CPU locks, which contradicts DESIGN §2.3 ("CPU-local or boot-only").

**Evidence:**
```rust
pub const fn can_acquire(held: u8, rank: u8) -> bool {
    if rank == 0 { true } else { held >> rank == 0 }
}
pub const fn release_mask(held: u8, rank: u8) -> u8 { held & !rank_bit(rank) }
```
Apart from gap 1, per-CPU tracking is equivalent to per-thread tracking, because SpinMutex keeps IRQs off for the whole life of the guard.

**Impact:** The checker misses these classes of lock-order bug. Today the riskiest IrqCells (KVA, DEFERRED, TABLE) are only taken under a ranked outer SpinMutex (PT or SCHED), which limits the practical impact.

**Fix:** Assert `HELD[cpu] == 0` before `switch_now`, where the SCHED guard has already been dropped. Keep a per-rank nesting count, or give device locks distinct sub-ranks. Give cross-CPU IrqCells a rank or convert them to SpinMutex, and update DESIGN §2.3. Moving tracking into the TCB is unnecessary.


#### F109 · IPI slot reuse relies on x86 TSO: a responder can pair a new round's acked reset with the previous round's va/func

**Severity:** LOW · LATENT (Phase 11 Portability, aarch64) · **Confidence:** Likely

**Location:** src/ipi_init.rs:82-99, src/ipi_init.rs:101-121, src/ipi_init.rs:157-172, src/ipi_init.rs:257-288

**What's wrong:** The first round's publication is sound on any architecture. A Release store on `waiters` orders every earlier store, Relaxed ones included, so the `compiler_fence` adds nothing. The weak-memory hazard is slot reuse. A responder that acked round N can re-enter `service_*` from `wait_acks`, a SpinMutex spin or another IPI. There it can still read round N's `waiters`, then round N+1's Relaxed `acked = 0` reset, then a Relaxed `va`/`func` that still holds round N's value.

**Evidence:**
```rust
slot.va.store(va.as_u64(), Ordering::Relaxed);   // sender
slot.acked.store(0, Ordering::Relaxed);
if w & me != 0 && s.acked.load(Ordering::Relaxed) & me == 0 {  // responder
    let va = s.va.load(Ordering::Relaxed);
```
x86 keeps store-store and load-load order (Intel SDM Vol. 3A §8.2.2). ARMv8 permits both reorderings (Arm ARM DDI 0487, §B2.3).

**Impact:** None on x86. On an aarch64 port the responder could invalidate the old VA, or skip a null `func` (reset at :286), and still ack. The sender then returns believing the work was done. aarch64 would normally use broadcast `TLBI ...IS`, so most of the exposure is in `call_mask`, which only ktests call today (src/ktest.rs:2351-2487).

**Fix:** Store the payload first, then publish the reset with `acked.store(0, Release)`, and have responders load `acked` with Acquire. A per-round generation counter also works. Remove the redundant `compiler_fence`.


#### F110 · Blocking primitives do not assert they are outside a device hard-IRQ top half

**Severity:** LOW · **Confidence:** Confirmed

**Location:** src/thread_init.rs:170-176, src/thread_init.rs:643, src/thread_init.rs:748, src/sync_init.rs:197-200, src/irq_init.rs:83-97, src/irq_init.rs:103-130

**What's wrong:** DESIGN §2.2 forbids blocking in a hard-IRQ/MSI handler. Only the vector-management calls (irq_init.rs:133,140,174,246) and virtio_blk_init.rs:1142 check `in_hard_irq()`. `schedule`/`yield_now`, `park`, `wait_on` and the sync_init wait loops (`wait_resume`) do not. Scheduling from IRQ context is itself allowed: timer and reschedule IPIs call `schedule_preempt` by design. `IN_ISR` is set only by device `dispatch()`, so the guard belongs on the `from_irq=false` paths only.

**Evidence:**
```rust
pub fn dispatch(vec: u8) {
    set_in_isr(true);
    ... h();                     // top half
    apic_init::eoi_for(vec);
    set_in_isr(false);
```
The trigger is a device top half that calls any blocking primitive.

**Impact:** No current top half blocks, since `dispatch` only wakes threads. A future one would switch away with the vector still in service at the LAPIC, because EOI follows the handlers. `IN_ISR[cpu]` would also stay set, so the next thread on that CPU gets `InIrq` from `allocate_vector`, `set_threaded` or `set_affinity`, and `reap_zombies` runs in IRQ context. The bug would show up as a distant hang, not an assert.

**Fix:** Add `assert!(!irq_init::in_hard_irq())` on the `from_irq=false` path of `schedule_inner`, and in `park`, `wait_on` and `begin_wait`. Do not add it to `schedule_preempt`.


#### F111 · Overdue-waiter diagnostic can never fire: expired timeouts are drained before the overdue scan runs

**Severity:** LOW · **Confidence:** Confirmed

**Location:** src/thread_init.rs:199-229, src/thread_init.rs:247, src/sched.rs:18-21, src/sched.rs:223-259; dead scaffolding: src/thread_init.rs:263, src/thread_init.rs:305-331, src/thread_init.rs:720, src/per_cpu.rs:29-30, src/smp_init.rs:90, src/smp_init.rs:116, src/smp_init.rs:233, src/per_cpu_init.rs:212-214

**What's wrong:** `schedule_inner` first calls `pop_expired_into(now, ..)`. Its buffer has `MAX_THREADS` slots, the same as the queue's capacity, so the call drains every timeout with `deadline <= now`. `overdue(now)` then runs with the same `now` and looks for `now - deadline >= OVERDUE_NS`, a subset of the entries just removed. It can never return anything. The `SWEEP_TICKS` gate is not the cause. The `sched: overdue tid` marker is also not registered in the harness. Other dead scaffolding sits on hot paths. `relink()` rebuilds `Tcb.prev/next` and `PerCpu.ready_head` under SCHED on every schedule, and only one ktest reads the result (ktest.rs:1370). `PerCpu.tsc_per_ms` is written but never read. `PARAM_IDT` is written, but the trampoline never reads offset 0xE8.

**Evidence:**
```rust
let n = s.timeouts.pop_expired_into(now, &mut expired); // drains deadline <= now
...
if ticks.is_multiple_of(SWEEP_TICKS) {
    for t in s.timeouts.overdue(now) {                  // wants deadline <= now - 5 s
```
The host test `overdue_scan` calls `overdue()` on its own, so it cannot catch this.

**Impact:** The diagnostic meant to expose stuck or lost-wakeup waiters never fires. `relink` adds O(n) work under the global SCHED lock on every schedule for no consumer. Correctness is not affected.

**Fix:** Either delete the sweep, the marker and the constants, or detect lateness during the pop loop (`now - deadline >= OVERDUE_NS`) and register the marker. Delete `relink`, `prev/next`, `ready_head`, `PerCpu.tsc_per_ms` and `PARAM_IDT`, or wire them up.


#### F112 · send_ipi writes the two ICR halves with interrupts enabled; smp_init sends INIT/SIPI with IF=1

**Severity:** LOW · LATENT (CPU offlining, ROADMAP §19.6) · **Confidence:** Confirmed

**Location:** src/apic_init.rs:324-339, src/apic_init.rs:349-366, src/smp_init.rs:193-201, src/main.rs:223, src/main.rs:241

**What's wrong:** send_ipi and send_ipi_all_ex_self write ICR_HIGH and then ICR_LOW as two MMIO writes without masking interrupts, and neither documents an IF-off precondition. smp_init::start_one sends INIT/SIPI through send_ipi after the first sti. The ipi_init senders (place_ready, shootdown_va, call_mask) all hold an InterruptGuard; start_one does not.

**Evidence:**
```rust
lapic_write(va, LAPIC_ICR_HIGH, hi);
lapic_write(va, LAPIC_ICR_LOW, lo);
```
In xAPIC mode, writing the low dword sends the IPI to whatever destination ICR_HIGH holds at that moment (Intel SDM Vol. 3A, "Interrupt Command Register (ICR)", §11.6.1 in older editions). An IPI sent on the same CPU between the two writes therefore retargets the INIT or SIPI.

**Impact:** Nothing can trigger this today, on QEMU or on hardware. During smp_init only the bootstrap and idle threads exist, no timeouts are queued, and each CPU's ICR is local, so nothing on the BSP sends an IPI inside the window. If INIT/SIPI ever runs with a live scheduler (CPU offlining/onlining, or sleeping threads created before smp_init), an INIT could reach a running AP. It could also reach APIC ID 0 if the interleaved sender used the all-excluding-self shorthand, which leaves ICR_HIGH at 0.

**Fix:** Take an InterruptGuard inside send_ipi and send_ipi_all_ex_self, covering the pending poll and the ICR_HIGH/ICR_LOW pair, so the primitive is safe for every caller. Do not hold IF off across busy_wait_ms.


#### F113 · BAR sizing leaves I/O and memory decode enabled, and mis-sizes I/O BARs whose upper 16 bits read zero

**Severity:** LOW · LATENT (Phase 20: Real hardware) · **Confidence:** Confirmed

**Location:** src/pci.rs:293-314, src/pci.rs:225-235, src/pci.rs:448-461, src/pci_init.rs:54-65, src/pci_init.rs:164-195

**What's wrong:** `probe_bar` writes all-1s to each BAR and then restores it, but never clears COMMAND.IO/MEM first. Each config access is its own `with_cfg` critical section, so IRQ handlers and the already-running APs keep going while a BAR is relocated. Separately, `size_from_mask` inverts I/O masks in 32-bit space and does not ignore bits 31:16 when they read back zero. A 32-byte I/O BAR with hardwired upper bits therefore sizes as 0xFFFF0020.

**Evidence:**
```rust
let raw0 = cfg.read32(bdf, off);
cfg.write32(bdf, off, 0xFFFF_FFFF);
let mask0 = cfg.read32(bdf, off);
cfg.write32(bdf, off, raw0);
// size_from_mask, 32-bit/I/O path:
(!(m as u32)).wrapping_add(1) as u64
```
This runs once at boot, after SMP bring-up. A host repro of `decode_bar(0xC001, 0, 0x0000_FFE1, 0)` returns size 0xFFFF0020. PCI Local Bus Spec 3.0 §6.2.5.1 (Implementation Note, sizing a 32-bit BAR) says to disable decode through COMMAND before sizing, and to ignore the upper 16 bits of an I/O result when they read zero.

**Impact:** No effect seen on QEMU. On real chipsets, a device can briefly decode at the top of 32-bit space (the IOAPIC/HPET/firmware range), or move a framebuffer BAR away from the live console, while another CPU or an IRQ handler is using it. The wrong I/O size only affects the registry, lspci and claim-overlap bookkeeping today, because no driver maps I/O BARs.

**Fix:** Save COMMAND and clear IO|MEM. Probe all of the function's BARs under one CFG_LOCK hold, restore the BARs, then restore COMMAND. For I/O masks whose bits 31:16 are 0, OR in 0xFFFF0000 before inverting.


#### F114 · CF8/CFC restricted to bus 0 on the basis of a false hardware claim; devices behind bridges are invisible without MCFG

**Severity:** LOW · LATENT (no-MCFG platforms with PCI-PCI bridges) · **Confidence:** Confirmed

**Location:** src/pci_init.rs:3-4, src/pci_init.rs:175-178, src/pci_init.rs:192-194, docs/DESIGN.md:366, docs/DESIGN.md:406, docs/DESIGN.md:1580-1582, docs/ROADMAP.md:592

**What's wrong:** When ECAM does not cover a bus, `HwCfg::read32` returns 0xFFFF_FFFF for any bus other than 0, and `write32` drops the write. DESIGN §9 justifies this with the claim that legacy 0xCF8/0xCFC "only addresses bus 0 on typical host bridges", and the claim is false. It started in commit 0538d67 and spread to the module doc, DESIGN step 17b, DESIGN §9 and ROADMAP.

**Evidence:**
```rust
if bdf.bus == 0 {
    return with_cfg(|| cf8_read32(bdf, offset));
}
0xFFFF_FFFF
```
PCI Local Bus Spec 3.0 §3.2.2.3.2 (Configuration Mechanism #1): CONFIG_ADDRESS bits 23:16 select the bus, and the host bridge issues a Type 1 transaction for non-local buses. `cf8_addr` (src/pci.rs:10-16) already encodes the bus. The real limitation of mechanism #1 is the 256-byte config space.

**Impact:** `scan_bus` follows bridges, but without MCFG every device behind a PCI-PCI bridge reads as absent and is never bound. The harness uses `-machine pc` without bridges, so nothing is hidden today.

**Fix:** Fall back to CF8 for any bus that ECAM does not cover (offsets < 0x100). Correct the pci_init.rs module doc, DESIGN.md:366, :406 and :1580-1582, and ROADMAP.md:592.


#### F115 · 'Resource claims are exclusive' is not enforced for real drivers; BAR addresses are never checked against RAM or other BARs

**Severity:** LOW · LATENT (real hardware / buggy firmware) · **Confidence:** Confirmed

**Location:** src/dev.rs:1-4, src/dev.rs:405-441, src/dev_init.rs:44-50, src/dev_init.rs:96-97, src/virtio_blk_init.rs:990-1003, src/virtio_init.rs:379-392, src/pci_init.rs:83-104, src/pci_init.rs:127-151

**What's wrong:** The dev.rs module doc says "Resource claims are exclusive", but no driver claim goes through a check. `claim_bars` only sets `claimed = true` on the probe copy, and `dev_init::bind_all` writes that copy back into REG. `Registry::claim` is the only code that checks for overlap and records a claim, and only ktests reach it (via `dev_init::claim`). `map_mmio` does not check a BAR against usable RAM or other BARs before UC-patching the physmap or ioremapping it. The same doc names `Registry::bind_all`, but only host tests use that. The kernel uses `dev_init::bind_all`.

**Evidence:**
```rust
if i < MAX_BARS && !dev.resources[i].is_empty() {
    dev.resources[i].claimed = true;          // virtio_blk_init.rs:995
}
let _ = unsafe { paging_init::patch_physmap_uc(PhysAddr(phys), len) }; // pci_init.rs:99
```
This triggers when firmware or a device reports a BAR that overlaps RAM or another BAR. Discarding the `patch_physmap_uc` result is harmless today: it can only fail with NotMapped, and every BAR below `map_end` is mapped.

**Impact:** Overlapping or RAM-aliased BARs go undetected. A BAR inside RAM has its physmap leaf made UC and then receives driver MMIO writes. The claims table is effectively dead code. Once a driver has bound a BAR, a later `Registry::claim` on it returns `Already` and records nothing. Once the 64-entry ECAM cache is full, every miss remaps. On the ioremap path each miss leaks at least 4 KiB of window VA, which is never reclaimed. That needs an MCFG above `map_end` and more than 64 function pages, as on q35 with root ports but not the default `pc` machine. The loss is limited to boot-time config reads.

**Fix:** Route driver claims through `Registry::claim` under REG. Reject BARs that overlap usable RAM or an existing claim. Propagate `patch_physmap_uc` errors, let ECAM misses reuse or unmap window VA, and correct the dev.rs doc.


#### F116 · Bus mastering enabled before probe and never revoked; failed virtio probes free queue memory without resetting the device

**Severity:** LOW · LATENT (Phase 20: Real hardware) · **Confidence:** Confirmed

**Location:** src/dev_init.rs:41-42, src/pci_init.rs:254-258, src/virtio_blk_init.rs:1025-1028, src/virtio_blk_init.rs:783-941, src/virtio_blk_init.rs:190-198, src/virtio_init.rs:341-346, src/virtio_init.rs:157-161

**What's wrong:** `bind_all` sets COMMAND.MEM|MASTER before every probe and never clears it when the probe fails. That includes a second virtio-blk, which `probe` refuses through the LIVE check without touching the device. In both virtio drivers, the error paths after the queue addresses are programmed and QENABLE=1 return `qdma`/`slots`/`data` to the buddy allocator and then call `fail_armed`. `fail_armed` only disables MSI-X, frees the vectors and sets STATUS_FAILED. The device is never reset, so it still holds queue addresses that point at freed frames.

**Evidence:**
```rust
pci_init::enable_mem_master(dev.addr);
if let Ok(()) = drv.probe(&mut dev) {
// virtio_init.rs, after QENABLE:
dma_init::free(qdma);
dma_init::free(data);
fail_armed(dev, common, vec);
```
Any probe failure after QENABLE triggers this: a notify-address failure, or a vector allocation or MSI-X enable failure on a later queue. virtio 1.2 §2.1.2: the device MUST NOT consume buffers before DRIVER_OK. §2.1.1: after FAILED, the driver MUST reset the device before re-initializing.

**Impact:** No effect today, because a conforming device (QEMU) never DMAs before DRIVER_OK. A buggy or non-conforming device left with bus mastering could DMA into reused buddy memory, including free-list nodes. Bus Master Enable does not stop a malicious device. Only an IOMMU does, and IOMMU support is deferred.

**Fix:** Enable bus mastering inside the driver, after reset. On probe failure, reset the device (write status 0 and poll), clear COMMAND.MASTER, and only then free the DMA memory.


#### F117 · Partition layout trusted: overlapping entries accepted, MyLBA and usable range unchecked, extra partitions silently dropped

**Severity:** LOW · LATENT (writable/mounted partitions on untrusted disks) · **Confidence:** Confirmed

**Location:** src/part.rs:379-407, src/part.rs:447-484, src/part.rs:523-545, src/part.rs:586-627, src/part_init.rs:34-35, src/part_init.rs:116-150

**What's wrong:**
- `header_ok` never compares the header's MyLBA with the LBA it was read from, and it ignores FirstUsableLBA and LastUsableLBA.
- GPT entries and MBR primaries are checked only against the disk size. Entries that overlap each other or the partition table itself are accepted, for example an MBR entry starting at LBA 0.
- Up to 16 partitions are parsed, but only 4 names exist for vda and 5 for ram0. `register_table` stops at the first partition without a name and logs nothing.
- A related bug: `parse_mbr` reads its entries from `sector_buf`, and `parse_logical` overwrites that buffer with each EBR. Any primary listed after an extended entry is therefore read from the last EBR.

**Evidence:**
```rust
if start >= nsectors || end > nsectors {        // part.rs:611, the only MBR check
let Some(name) = name_for(dev, i) else { break; }; // part_init.rs:124
```
An MBR with entries (start 0, len 100) and (start 50, len 100) parses as two overlapping partitions, and the first one covers the MBR. UEFI 2.10 §5.3.2 defines MyLBA. §5.3.1 requires partitions to lie within the usable range and not to overlap.

**Impact:** Nothing corrupts data today. No filesystem mounts partitions, devfs block nodes return NotSupp for read and write, and only ktests write. Once partitions are writable, a write through an overlapping partition would corrupt the table or a neighbouring filesystem. Partitions past the name table, and primaries listed after an extended entry, are already invisible today.

**Fix:** Reject entries that overlap each other, the MBR, or the GPT header and entry arrays. Check MyLBA and the usable range, and log any partition that is dropped. Copy the four MBR entries into a local array before walking the EBR chain.


#### F118 · PCI capability walk ignores reserved pointer bits and does unbounded, unaligned dword reads

**Severity:** LOW · LATENT (Phase 20: Real hardware (ECAM / q35)) · **Confidence:** Confirmed

**Location:** src/pci.rs:346-374, src/virtio.rs:159-206, src/pci.rs:617-622, src/pci.rs:10-16, src/pci.rs:20-25, src/pci_init.rs:153-155

**What's wrong:** `walk_caps` and `read_modern_caps` use the Capabilities Pointer and every Next pointer without masking the two reserved low bits. `read_modern_caps` and `read_msix_cap` then call `read32` at `ptr+4/8/12/16` with no alignment or bounds check. Under CF8, `cf8_addr` masks the offset to 0xFC, so an offset past 0xFF wraps into the header dwords. Under ECAM, `ecam_off` keeps the low bits, so an unaligned pointer becomes an unaligned `*const u32` `read_volatile`, which is UB.

**Evidence:**
```rust
let mut ptr = read8(cfg, bdf, CFG_CAP_PTR);     // pci.rs:352, no & 0xFC
mult = cfg.read32(bdf, ptr as u16 + 16);        // virtio.rs:173
unsafe { (va as *const u32).read_volatile() }   // pci_init.rs:154
```
The trigger is a device whose capability pointer has low bits set, or whose vendor capability starts at 0xF0 or higher. PCI Local Bus Spec 3.0 §6.7: the bottom two bits of capability pointers are reserved and software must mask them off (Linux does `pos &= ~3`).

**Impact:** Capabilities get parsed wrong, for example a notify multiplier or BAR offset read from the header, and the driver then uses the wrong BAR or offset. Under ECAM, a dev-profile build would likely halt at boot on the `read_volatile` alignment precondition check. No effect today: the default `pc` machine has no MCFG, and QEMU devices use aligned capabilities.

**Fix:** Mask every pointer with `& 0xFC`. Reject any capability where `ptr + cap_len` exceeds 0x100 (0x1000 under ECAM). Issue dword reads only at 4-byte-aligned offsets, or compose them from `read8`/`read16`.


#### F119 · Block queue merges past virtio-blk's 8 KiB bounce and max_discard limits, failing every merged waiter with Inval

**Severity:** LOW · LATENT (Phase 12.5: Unified page cache) · **Confidence:** Confirmed

**Location:** src/block.rs:235-303, src/block.rs:351-378, src/virtio_blk_init.rs:319-324, src/virtio_blk_init.rs:357-361, src/virtio_blk_init.rs:564-591, src/virtio_blk_init.rs:1154-1167

**What's wrong:** `merge_into` limits only the segment and waiter counts (8 each) and checks for sector overflow. It has no byte limit and no discard-sector limit. virtio-blk `issue()` rejects any request larger than the 8 KiB bounce buffer, or above `max_discard_sectors`, with `Inval`. `finish()` treats `Inval` as non-retryable, so every waiter merged into an oversize request fails. `submit()` also accepts a single read/write over 8 KiB, which `issue()` then fails.

**Evidence:**
```rust
if (dst.nseg as usize) + (src.nseg as usize) > MAX_SEGS { return false; }
if (dst.nwait as usize) + (src.nwait as usize) > MAX_SEGS { return false; }
let nsect = dst.bio.nsect.checked_add(src.bio.nsect);
// issue():
if req.bytes() != want || want == 0 || want > BOUNCE {
```
Three adjacent 4 KiB writes queued before `pump()` issues them merge into one 12288-byte request (reproduced on the host against the real `vibeos-core`). Two merged 4 KiB writes come to exactly 8192 bytes, which still passes.

**Impact:** Every caller in the merged request gets `Inval` for I/O that was valid. This needs three concurrent submitters writing adjacent LBAs on vda. The page cache does not hold its lock across backend I/O, so this can already happen but is rare. It gets common once concurrent vda I/O grows.

**Fix:** Give `Queue` per-device `max_bytes` and `max_discard` limits (set from BOUNCE and MAX_DISCARD for vda) and refuse merges that would exceed them, or split oversize requests across slots in `issue()`. Reject a single oversize request in `submit()`.


#### F120 · Vacuous assertions in dma, virtio, virtio-blk and ECAM tests; the ECAM test pins the wrong formula

**Severity:** LOW · **Confidence:** Confirmed

**Location:** src/dma.rs:314-322, src/virtio.rs:833, src/ktest.rs:3915-3920, src/virtio_blk_init.rs:301, src/pci.rs:873-876, src/pci.rs:28-41

**What's wrong:** Four assertions cannot fail for the property they claim to check:
- `publish_uses_release_not_only_compiler_fence` checks that one thread reads back its own store. That holds with any fence or none.
- In `split_wrap_and_sim_device`, `assert!(q.should_kick(old) || n > 0)` only checks anything at n=0. The rest of that test is meaningful.
- `test_block_vblk_rw` asserts that `barrier()` succeeds, but `issue` completes `Op::Barrier` locally with `Ok(())`. Its `!has_flush()` branch contains only a comment. The test's round-trip and flush checks are real.
- The ECAM test asserts a start_bus-relative address, so it locks in the wrong formula in `ecam_phys`.

**Evidence:**
```rust
publish_index(&idx, 3);
assert_eq!(idx.load(Ordering::Acquire), 3);
// virtio_blk_init.rs:301
Op::Barrier => return Issued::Local(req, Ok(())),
```
The MCFG base address corresponds to bus 0 even when start_bus > 0 (PCI Firmware Spec 3.x §4.1.2; Linux Documentation/PCI/acpi-info.rst). A correct test therefore expects base + (bus << 20).

**Impact:** The ECAM formula bug passes CI, and so would a regression in the kick decision. The ECAM bug does no harm on QEMU pc or q35, where start_bus is 0.

**Fix:** Delete or rename the dma test. No single-threaded host test can detect a missing store-load fence. Drive `should_kick` with explicit avail_event values and assert both outcomes. Remove the empty branch and the barrier assertion; barrier ordering is already host-tested in block.rs:738-770. Change the ECAM test to start_bus=1 expecting base+(bus<<20), in the same change as the formula fix.


#### F121 · virtio-rng pool: lock-free take can race a refill and hand the same bytes to two readers; no single-device guard

**Severity:** LOW · **Confidence:** Confirmed

**Location:** src/virtio_init.rs:495-512, src/virtio_init.rs:177-188, src/virtio_init.rs:190-211, src/entropy_init.rs:13-23, src/virtio_init.rs:413-434, src/virtio_init.rs:351-366, src/fs/kernfs.rs:999-1015

**What's wrong:**
(a) `rng_take` claims an index with a CAS on `POOL_POS` and reads `POOL[pos]` without taking the Q lock. Meanwhile `publish_pool` (threaded IRQ, under the Q lock) rewrites `POOL[0..n]` before resetting `POOL_POS` to 0. A reader on another CPU that claims index k in that window gets the new byte k, and the next reader gets the same byte again after the reset.
(b) Hardening only: `hw_fill` calls `rng_request` only after a non-empty take, so a zero-length completion would stop refills permanently. A conforming device never sends one.
(c) `RngDriver::probe` has no BOUND guard, so a second virtio-rng overwrites `Q`/`ISR_VA` and orphans the first device's queue and vector.

**Evidence:**
```rust
if POOL_POS.compare_exchange(pos, pos + 1, AcqRel, Relaxed).is_ok() {
    buf[i] = POOL[pos as usize].load(Ordering::Relaxed);
```
The VFS lock serializes readers, so (a) needs a completion to land on another CPU while a reader is consuming. `hw_fill` issues a new request after every non-empty take, so that overlap happens in normal use. For (b), virtio 1.2 §5.4.6 (Entropy device, device requirements) says the device MUST write at least one byte.

**Impact:** Occasionally, duplicated bytes reach `/dev/random` readers unmixed. (c) needs two virtio-rng devices. The old queue memory is leaked rather than freed (DmaBuffer has no Drop), so there is no stray DMA.

**Fix:** Take the pool under the Q lock, or version `POOL_POS` with a generation counter. Issue `rng_request` whenever the pool is empty and nothing is in flight. Refuse a second probe once BOUND is set.


#### F122 · virtio drivers skip QMSIX read-back and config_generation loop, read ISR under MSI-X, and ignore negotiated SIZE_MAX

**Severity:** LOW · LATENT (Phase 20: Real hardware / non-QEMU virtio) · **Confidence:** Confirmed

**Location:** src/virtio_blk_init.rs:916-921, src/virtio_init.rs:332-333, src/virtio_blk_init.rs:128-130, src/virtio_blk_init.rs:709, src/virtio_blk_init.rs:646-652, src/virtio_init.rs:168-175, src/virtio_blk.rs:22-32, docs/DESIGN.md:1889

**What's wrong:** There are four deviations:
- Neither driver reads `queue_msix_vector` back after writing it, so a NO_VECTOR result goes unnoticed.
- The 64-bit `capacity` is read as two 32-bit reads with no `config_generation` loop.
- Both top halves read ISR on every interrupt, and MSI-X is always on for these drivers. DESIGN §10.4 describes this as intended.
- F_SIZE_MAX is negotiated, but `size_max` is never read, and the single data descriptor can be up to 8 KiB. F_SEG_MAX is ignored too, but one data descriptor always satisfies it.

**Evidence:**
```rust
w16(common, COMMON_OFF_QMSIX, if per_q_msix { qi as u16 } else { 0 });
w16(common, COMMON_OFF_QENABLE, 1);
let cap_512 = r64(cfg, CFG_CAPACITY); // two r32
```
virtio 1.2 §4.1.5.1.2.2 and §4.1.5.1.3: the driver MUST verify the vector by reading it back. §2.5.1: the generation loop (SHOULD). §4.1.4.5.2: with MSI-X enabled, the driver SHOULD NOT access ISR on a queue interrupt.

**Impact:** None on QEMU with in-range vectors. If a device refuses the vector, virtio-blk waiters park with FAR_DEADLINE and hang forever, and virtio-rng silently stays in flight. Other effects: a resize during probe can tear the capacity, the ISR read costs an extra MMIO trap per interrupt under KVM/HVF, and a device with `size_max` < 8192 may reject I/O.

**Fix:** Read QMSIX back and fail the probe on NO_VECTOR. Put multi-dword config reads in a generation loop. Drop the ISR read under MSI-X and update DESIGN §10.4. Either remove F_SIZE_MAX/F_SEG_MAX from OFFER or cap the transfer size at `size_max`.


#### F123 · FAT timestamps use the wrong epoch and leap-year rule, fat_to_unix is not the inverse, and the kernel never sets FatVol::now

**Severity:** LOW · **Confidence:** Confirmed

**Location:** src/fat.rs:1997-2039, src/fat.rs:2041-2054, src/fat.rs:303, src/fat.rs:556, src/fat.rs:1179, src/fat.rs:1700, src/fat.rs:728

**What's wrong:** fat_datetime takes unix seconds and counts years from 1970. It stores that count directly in date bits 15-9, which hold years since 1980, and it treats 1970 (y = 0) as a leap year. fat_to_unix has no 1980 offset, no leap days and uses 30-day months, so the two functions are not inverses. FatVol::mount initializes `now: 0`, and nothing in the kernel ever sets it, even though time_init::unix_time_s() exists.

**Evidence:**
```rust
let ly = if y.is_multiple_of(4) { 366 } else { 365 }; // y counts from 1970
let date = (y << 9) | ((m + 1) << 5) | ((days as u16) + 1);
// fat_to_unix:
y * 365 * 86400 + m.saturating_sub(1) * 30 * 86400 + ...
```
Microsoft FAT32 spec v1.03, "Date and Time Formats": bits 15-9 are the count of years from 1980.

**Impact:** Every file the kernel creates, writes or truncates is stamped 1980-01-01 on disk and reads back through stat as 1970-01-01. Initrd files (now = 2010-01-01, the same constant tests/hostlib mkinitrd uses) are stored as 2020-01-01 and stat as 2009-12-22. The only effect is wrong mtimes; nothing is corrupted.

**Fix:** Count years from 1980 with the Gregorian leap rule. Make fat_to_unix its exact inverse and add a host round-trip test. Feed the wall clock into FatVol::now (Vfs::now has the same gap).


#### F124 · FAT walk resolves '..' wrongly below depth 16, and read_dirent treats I/O and corruption errors as end-of-directory, so rmdir can free a non-empty directory

**Severity:** LOW · LATENT (untrusted disks / real hardware; mkdir/rmdir syscalls) · **Confidence:** Confirmed

**Location:** src/fat.rs:346-373, src/fat.rs:841-846, src/fat.rs:1029-1041, src/fat.rs:594-612, src/fat.rs:385-398

**What's wrong:** FatVol::walk keeps a 16-entry ancestor stack and silently drops pushes past it, so `..` below depth 16 pops the wrong ancestor. read_dirent maps FatError::Io and Corrupt to "no more entries". As a result, lookup returns NotFound, readdir stops early, and dir_empty returns true when an error occurs. The real end of a chain already comes back as `Ok(false)`, and sibling scanners (short_taken, dir_reserve) propagate errors, so this mapping is an inconsistency, not a design choice.

**Evidence:**
```rust
if sp < stack.len() { stack[sp] = node.clu; sp += 1; } // overflow dropped
...
Err(FatError::Corrupt) | Err(FatError::Io) => return Ok(None),
```
unlink with `rmdir = true` trusts dir_empty, then marks the entry deleted and frees the directory's chain.

**Impact:** On a device-backed FAT mount (vda/ram0), an I/O error or corruption during `rm -r` frees a non-empty directory. Its children's entries are orphaned and their clusters leak. A lookup error can also let create or O_CREAT add a duplicate name. Once a tree deeper than 16 levels exists, userspace open resolves `..`, and O_CREAT creates files, in the wrong directory. Today, deep trees and rmdir require the root debug shell or an external image.

**Fix:** Fail with NameTooLong/Inval when the stack overflows, or resolve `..` from the on-disk entry, where cluster 0 means root_clus. Propagate Io and Corrupt from read_dirent.


#### F125 · Stale bytes reappear after a shrink followed by a grow (FAT last cluster, vibefs inline tail and last extent block)

**Severity:** LOW · LATENT (ftruncate/truncate syscall) · **Confidence:** Confirmed

**Location:** src/fat.rs:486-522, src/fat.rs:1118-1160, src/fat.rs:458-459, src/vibefs.rs:1499-1516, src/vibefs.rs:1518-1545

**What's wrong:** A shrink never zeroes the tail of the last kept unit, and a later grow never zeroes the range [old_size, new). FAT truncate-shrink leaves the tail of the last kept cluster, and ensure_size zeroes only the clusters it allocates. vibefs inline shrink leaves inline_data past the new size, zeroing it only when `new == 0`. The vibefs extent path trims extents at keep_blks without zeroing the tail of the last kept block.

**Evidence:**
```rust
if self.inodes[is].flags & F_INLINE != 0 {
    self.inodes[is].size = new;
    self.inodes[is].inline_len = new as u8;
    if new == 0 { self.inodes[is].inline_data = [0; INLINE]; }
```
Host probes: on FAT, writing 400 B, truncating to 10, then truncating to 300 reads old data at byte 200. The same happens on vibefs inline files and on extent files (3000 → 200 → 2000).

**Impact:** Old bytes from the same file reappear where POSIX requires zeros after a truncate extension or a write past EOF. Nothing leaks across files. Non-zero truncate is reachable today only through the kernel_tests truncate_path; O_TRUNC to 0 is safe on both filesystems.

**Fix:** Zero on shrink: the FAT last cluster from `new % cb` to its end, vibefs `inline_data[new..]`, and a CoW rewrite of the last kept vibefs block with a zeroed tail. Also file separately: a vibefs truncate-grow of an inline file past INLINE (1504-1506) leaves F_INLINE set, and the next read panics at vibefs.rs:1324 (latent HIGH under the same milestone).


#### F126 · Debug-shell file commands: rm -r stack overflow and path-buffer panic, `ls` fails on FAT/vibefs subdirectories, repeated mount leaks pinned dentries

**Severity:** LOW · **Confidence:** Confirmed

**Location:** src/file_init.rs:725-782, src/file_init.rs:765-774, src/file_init.rs:232-240, src/file_init.rs:713-723, src/file_init.rs:684, src/fs/mod.rs:1208-1265, src/fs/mod.rs:1706-1721, src/fs/mod.rs:1311

**What's wrong:** rm_r recurses with frames of about 2 KiB, plus 5-6 KiB in the FAT leaf, on a 16 KiB thread stack. It writes its 256-byte `child` path buffer without length checks and collects only 16 children. vfs_ls_snap treats only NotFound/Io as "not kernfs", but ops_lookup returns NotSupp for Fat and Vibe, so `ls` of any FAT or vibefs subdirectory fails. Each repeated vfs_attach inserts another pinned dentry, because dcache_drop_name skips pinned entries.

**Evidence:**
```rust
let mut child = [0u8; MAX_PATH];
...
child[n] = b'/';
...
child[n..n + ln].copy_from_slice(&kids[i][..ln]);
```
This indexes out of bounds once a descendant's absolute path exceeds 256 bytes, which is reachable via `mv` of an ancestor or via long LFN names on a mounted image. A host probe hit NoSpace after 47 repeated attaches.

**Impact:** These shell commands can panic the kernel. `rm -r` hits the guard page and a #DF panic after about 4 levels (estimated from frame sizes), or panics on the slice bounds. `rm -r` on a directory with more than 16 entries deletes 16, then fails with NotEmpty. `ls /etc` fails. Re-running `mount` on one path exhausts dentries, which breaks kernfs lookups too. Only the `kernel_shell` build reaches any of this. unlink_path drops the name from "/" instead of the real parent, but that has no effect until FS-01. FS-01 also owns the FAT ino-table issues.

**Fix:** Make rm_r iterative, with a bounded explicit stack, length-checked path building and no 16-child limit. Treat NotSupp as "not kernfs", make vfs_attach idempotent, and drop names from the resolved parent.


#### F127 · When a bound (run_path) process exits, reparent_children never runs, so its children keep a stale ppid and the next process to get that pid adopts them

**Severity:** LOW · LATENT (a bound run_path program that exits or faults before reaping its children, for example a failing /bin/tests ktest) · **Confidence:** Confirmed

**Location:** src/proc_init.rs:980-994, src/proc_init.rs:331-349, src/user_init.rs:270-279, src/proc_init.rs:215-249, src/proc_init.rs:1011-1016, src/proc_init.rs:1114-1136, docs/ROADMAP.md:807

**What's wrong:** In `finish_exit`, the bound branch longjmps into `run_user` or diverges through `return_status_or_die`, so it never reaches `reparent_children` at :994. `run_path` then calls `unbind_current`, which resets the slot to `Proc::empty()` and leaves the children's ppid pointing at it. `alloc_pid` hands out the lowest free pid from 2 upward. The bound pid is normally 2, because boot_hello runs before start_init. So the next fork or spawn gets that pid, and since `find_zombie` and `has_child` match on ppid alone, it inherits the orphans. This contradicts the checked ROADMAP item "reparenting to init on parent death".

**Evidence:**
```rust
if bound {
    ...
    if syscall_init::in_user() {
        syscall_init::longjmp_user(syscall_init::exit_status());
    }
    return_status_or_die(wait_status);
}
```
This triggers when a bound process exits or dies before reaping its children. tests.asm's `fail:` path exits without reaping. Reference: POSIX.1-2017 _exit(), "Consequences of Process Termination".

**Impact:** When an orphan exits, `t.get_mut(ppid)` returns None, so no SIGCHLD is sent. The orphan stays a Zombie that init cannot reap, because its ppid is not 1. The next owner of that pid can reap zombies it never forked, block in wait4(-1) on unrelated live orphans, and show up as the parent in their getppid(). If the pid is never reused, the zombies occupy slots in the 15-pid table indefinitely. Memory safety is not affected.

**Fix:** In the bound path, run `reparent_children` and the init wake under with_sched+TABLE, either before the longjmp or in `unbind_current` before the slot is reset. Add a debug_assert in `alloc_pid` that no live slot has ppid equal to the new pid. Add a regression ktest in which a bound program forks and exits without waiting.


#### F128 · /sbin/init spins on wait4(-1) = -ECHILD once it has no children; a failed sh fork or exec leaves no shell and prints nothing

**Severity:** LOW · **Confidence:** Confirmed

**Location:** user/init.asm:45-52, user/init.asm:30-43, src/proc_init.rs:1068-1070

**What's wrong:** The `.reap` loop calls wait4(-1) and jumps back without checking rax. Once init has no children, `sys_wait4` returns -ECHILD before it reaches any blocking path, so init spins in ring 3. This happens when the sh fork fails (`js .reap`), when the sh execve fails (the child exits 127 and is reaped once), or when a fault kills sh. Init does not report the problem and does not retry.

**Evidence:**
```asm
.reap:
    mov rdi, -1
    ...
    mov eax, SYS_WAIT4
    syscall
    jmp .reap
```
`sys_wait4` checks `!has_child(t, self_pid, want)` and returns ECHILD before the nohang and sleep paths. sh.asm has no exit path, so today this needs an sh start failure (for example, /bin/sh missing from the initrd) or a user fault in sh.

**Impact:** init runs its full quantum whenever it is scheduled. It is pinned to the BSP, and every forked user process inherits that pin. The timer still preempts, so other processes slow down but do not starve. No shell comes up. Fork and exec failures log nothing; only a fault prints `user: pid N killed ...`. In e2e the only symptom is a `shell ready` timeout.

**Fix:** Check rax in `.reap`. On -ECHILD or a failed fork, write a diagnostic, call sched_yield, and fork and exec /bin/sh again, a minimal version of the ROADMAP Phase 14 restart item (ROADMAP.md:1194). If sh still cannot start after a bounded number of retries, exit so that the kernel's init-exit policy reports it. Add a harness case with /bin/sh missing that expects the diagnostic.


#### F129 · FPU_TEMPLATE captures the bootloader's leftover MXCSR and XMM/ST contents instead of a defined ABI initial state

**Severity:** LOW · LATENT (MXCSR: firmware or a loader that breaks the handoff spec; XMM contents: UEFI/OVMF boot; either matters once untrusted user binaries run) · **Confidence:** Likely

**Location:** src/syscall_init.rs:264-285, src/syscall_init.rs:287-299, src/thread_init.rs:380, src/thread_init.rs:479, src/thread_init.rs:578, src/thread_init.rs:630, src/main.rs:202-203

**What's wrong:** `init_fpu` runs `fninit` and then saves the result with `fxsave64` as the template for every TCB, including spawned processes and fork children. FNINIT resets only the x87 control, status and tag words. It leaves ST0-7 data unchanged and does not touch MXCSR or XMM0-15. Nothing in src/ executes `ldmxcsr` or clears the XMM registers. In addition, `init_bootstrap` (main.rs:202) builds a TCB before the template is captured. That TCB gets the `Fxsave::empty()` fallback: FCW=0 and MXCSR=0, which unmasks every x87 and SIMD exception. `seed_current_fpu` overwrites it today.

**Evidence:**
```rust
core::arch::asm!("fninit", options(nomem, nostack));
if FPU_TEMPLATE.try_get().is_none() {
    let mut tmpl = Fxsave::empty();
    // fxsave64 [tmpl]; FPU_TEMPLATE.set(tmpl)
```
The template is correct today only by accident. UEFI 2.x §2.3.4 requires FCW=0x037F and MXCSR=0x1F80 at loader handoff. SDM Vol. 3A Table 9-1 gives the same MXCSR value and zeroed XMM/ST registers at power-up. Whether OVMF's own SSE use leaves data in XMM is Suspected. See also SDM Vol. 2A, FINIT/FNINIT and FXSAVE.

**Impact:** The kernel itself is unaffected. With non-conforming firmware, every user process starts with an MXCSR that violates the psABI (§3.4.1). Any data the firmware leaves in XMM or ST registers can be read by every process through its own FXSAVE. No current user binary uses x87 or SSE, and every process runs as root.

**Fix:** Make the template a `const`: a zeroed Fxsave with FCW (bytes 0-1) = 0x037F and MXCSR (bytes 24-27) = 0x1F80. This also removes the `Fxsave::empty()` fallback and the dependency on init order. Add a host test on those bytes and on the zeroed ST/XMM regions. Separately, CR4.OSXMMEXCPT is never set (src/x86.rs:112-115), so an unmasked SIMD exception arrives as #UD instead of #XM (SDM Vol. 3A §2.5).


#### F130 · FXSAVE-only FP switching assumes CR4.OSXSAVE, CR4.PKE and EFER.FFXSR are clear, but no CPU clears or asserts them

**Severity:** LOW · LATENT (bootloader change: any loader or firmware that does not zero CR4/EFER at handoff) · **Confidence:** Confirmed

**Location:** src/thread.rs:110-121, src/syscall_init.rs:69, src/syscall_init.rs:78, src/syscall_init.rs:331-345, src/syscall_init.rs:226-228, src/syscall_init.rs:264-270, src/arch/cpu.rs:26-38, src/paging_init.rs:505-508, src/arch/trampoline.S:53-61, src/main.rs:90

**What's wrong:** Per-thread FP state is only the 512-byte legacy FXSAVE image, switched with fxsave64/fxrstor64 at CPL0. Every CR4 and EFER write in the kernel and the AP trampoline is a read-modify-write that only sets bits. Three inherited bits would break this model without anyone noticing: CR4.OSXSAVE with XCR0 enabling AVX or AVX-512 (FXSAVE does not cover that upper state), EFER.FFXSR (AMD fast FXSAVE skips XMM0-15 at CPL0 in long mode), and CR4.PKE (user-writable PKRU that is not switched). Nothing clears or asserts any of them.

**Evidence:**
```rust
static BASE_REV: BaseRevision = BaseRevision::with_revision(3); // main.rs:90
let cr4 = x86::read_cr4() | CR4_OSFXSR;                        // syscall_init.rs:269
x86::wrmsr(IA32_EFER, efer | EFER_SCE);                        // syscall_init.rs:228
```
Limine guarantees that other CR0/CR4/EFER bits are cleared only from base revision 5 onward. The pinned v9.6.7 zeroes CR4 on both boot paths but EFER only on UEFI; on BIOS boots EFER is whatever the firmware left, which is 0 under SeaBIOS. APs start from INIT with CR4=0. So today VEX-encoded instructions raise #UD and the process gets SIGILL (src/arch/idt.rs:84). The per-CPU CR snapshot ktest checks only SMEP, SMAP and UMIP. Specs: Limine PROTOCOL.md "x86-64 machine state at entry"; AMD APM Vol 2 §3.1.7 (EFER.FFXSR); Intel SDM Vol 1 ch. 13; SDM Vol 3A §2.5 (CR4.PKE).

**Impact:** Harmless with the pinned loader. If a loader or firmware leaves any of these bits set, vector-register state leaks between processes on the same CPU (the YMM/ZMM upper halves, or all XMM registers with FFXSR). That is a cross-process isolation break, and nothing would detect it.

**Fix:** In init_cpu, on every CPU, clear CR4.OSXSAVE, CR4.PKE and EFER.FFXSR (bit 14), assert them, and extend the CR snapshot ktest to cover them. Requesting base revision 5 or later helps but does not replace the assert. If XSAVE is adopted later, set OSXSAVE deliberately, program XCR0 to exactly the features being saved, and size the TCB area from CPUID leaf 0Dh.


#### F131 · No branch-target-injection mitigations: SPEC_CTRL/PRED_CMD never written, no retpoline or return thunks, no IBPB or RSB fill on address-space switch

**Severity:** LOW · LATENT (Phase 18 §18.3: KVM or bare metal on BTI/BHI/Retbleed-affected CPUs; the user-to-user parts matter once non-root users exist, §18.6) · **Confidence:** Confirmed

**Location:** src/arch/cpu.rs:16-57, src/x86.rs:161-168, .cargo/config.toml:9-16, src/syscall_init.rs:317-357, src/proc_init.rs:419-451, docs/ROADMAP.md:1566

**What's wrong:** `cpuid_leaf7` throws away CPUID.(7,0):EDX, which holds the IBRS/IBPB, STIBP, ARCH_CAPABILITIES and SSBD bits. The kernel never reads IA32_ARCH_CAPABILITIES and never writes IA32_SPEC_CTRL or IA32_PRED_CMD. It is built without retpolines or return thunks, and `switch_cr3_for` switches between user address spaces with no IBPB and no RSB fill. The syscall `match nr` compiles to a jump table indexed by the user-supplied number. That table is both a BTI/BHI target and a Spectre v1 (bounds-check bypass) site. The open ROADMAP line "speculation mitigations, measured before enabling" tracks this gap but never lists these items.

**Evidence:**
```rust
let (_, ebx, ecx, _) = cpuid(7, 0);
(ebx, ecx)
// switch_cr3_for
unsafe { x86::write_cr3(want) };
cpu.as_cr3 = want;
```
The tree has no SPEC_CTRL, PRED_CMD, IBRS, IBPB or retpoline anywhere. The debug ELF has 57 `jmp *`, 559 `call *` and no thunk symbols. Dispatch compiles to `cmpq $0x6e,%r14; ja; jmpq *table(,%r14,8)`. References: CVE-2017-5715, CVE-2022-0001/0002 and CVE-2022-29900/29901; Intel SDM Vol. 2A, CPUID leaf 07H EDX; Intel SDM Vol. 4, MSRs 48H, 49H and 10AH.

**Impact:** No exposure under the default TCG accelerator (Makefile:39), and every process runs as root today. Under KVM or on bare metal, user code can steer kernel indirect branches and returns during speculation, and one process can poison another's branch predictors across a context switch. On Meltdown-affected Intel CPUs these controls achieve nothing until KPTI exists, because the kernel half is mapped into every user PML4 (src/addr_space.rs:3).

**Fix:** Replace ROADMAP.md:1566 with named items and put KPTI first. On each CPU, read CPUID.7.EDX and ARCH_CAPABILITIES. If IBRS_ALL is set, set SPEC_CTRL.IBRS, plus BHI_DIS_S where the CPU enumerates it. Otherwise, build with the pinned nightly's `-Zretpoline-external-thunk` and `-Zfunction-return=thunk-extern` (which also removes the jump table), or mask the syscall index. Add a measured, conditional IBPB on user-to-user CR3 switches and an RSB fill. On Zen 1/2, Retbleed needs an untrained-return thunk or IBPB on entry, not RSB filling.


#### F132 · No VERW before return to ring 3 (MDS/TAA), and no LFENCE after the conditional swapgs in IDT entry (CVE-2019-1125 pattern)

**Severity:** LOW · LATENT (VERW part: KVM or bare metal on Intel CPUs with RDCL_NO=1 and MDS_NO=0 or TAA_NO=0. swapgs part: the ROADMAP §18.3 CR4.FSGSBASE item or any ARCH_SET_GS) · **Confidence:** Confirmed

**Location:** src/syscall_init.rs:79-109, src/syscall_init.rs:111-141, src/syscall_init.rs:143-151, src/syscall_init.rs:153-176, src/arch/gs.rs:36-51, src/arch/idt.rs:216-224, docs/ROADMAP.md:1563, docs/ROADMAP.md:1566

**What's wrong:** No exit to ring 3 executes VERW. That covers sysretq at :109, the iretq exits at :141, :151 and :176, and the x86-interrupt epilogues. MD_CLEAR (CPUID.7.EDX[10]) is never checked. Every IDT handler calls `gs::enter` through `gs_enter`, and `gs::enter` runs swapgs only when `CS & 3` is set, with no LFENCE on either path. The swapgs at syscall entry (:44) always runs, so it does not have this problem.

**Evidence:**
```rust
pub unsafe fn enter(from_user: bool) {
    if from_user {
        unsafe { do_swapgs() };
    }
}
```
The disassembly of ipi_reschedule shows `testb $0x3,%al; jne ...; swapgs` with no fence. The gs:[0] loads come later, in callees (drain_inbox, schedule_inner). The swapgs part cannot be exploited today:
- user GS_BASE is written as 0 before every entry to ring 3 (syscall_init.rs:378, 402);
- there is no FSGSBASE, ARCH_SET_GS or LDT;
- VA 0 can never be mapped (NULL_GUARD_LEN).

References: CVE-2018-12126/12127/12130, CVE-2019-11091 and CVE-2019-11135 (Intel MDS/TAA guidance on VERW with MD_CLEAR); CVE-2019-1125 (Intel SWAPGS guidance).

**Impact:** No exposure under TCG. On Intel CPUs affected by MDS or TAA, user code can sample stale kernel data from CPU buffers after a return to ring 3. Most of those CPUs are also affected by Meltdown, and without KPTI user code there can read kernel memory directly, so VERW alone only closes a leak on RDCL_NO=1 parts. Once user code can choose its GS base, a mispredicted CS check becomes a gadget that discloses kernel memory.

**Fix:** Add `lfence` after the swapgs check in `gs::enter` now, on both paths, and record it as a prerequisite of ROADMAP.md:1563. Gate VERW on MD_CLEAR and ARCH_CAPABILITIES. Put it before each sysretq/iretq to ring 3 and in `gs::leave` when `to_user`. Schedule it alongside KPTI in the expanded 1566 item. Disabling TSX is an alternative fix for TAA.


#### F133 · The speculation posture is one generic ROADMAP line, so three ordering dependencies go unrecorded: KPTI before KASLR, L1TF-safe PTEs before swap, and the swapgs fence before FSGSBASE

**Severity:** LOW · LATENT (Phase 12.7 swap, Phase 18.2 KASLR, Phase 18.3 FSGSBASE) · **Confidence:** Confirmed

**Location:** docs/ROADMAP.md:1566, docs/ROADMAP.md:1563, docs/ROADMAP.md:1548-1553, docs/ROADMAP.md:1065-1071, docs/ROADMAP.md:1599-1602, docs/DESIGN.md:474-481, src/arch/cpu.rs:52-56

**What's wrong:** ROADMAP.md:1566 sets a defer-and-measure posture but names no mitigation. Three mitigations are ordering constraints on other roadmap items:
- §18.2 KASLR is planned on a kernel half that is mapped into every user PML4, with no KPTI.
- §12.7 swap will define the swap-entry PTE format before anyone reads Phase 18, and L1TF needs inverted offsets in non-present PTEs.
- The FSGSBASE line (1563) does not say it depends on an LFENCE after the conditional swapgs.

DESIGN §4.1 never says the kernel half is present in every user CR3, and its user row still reads "Empty until ring 3 exists".

**Evidence:**
```
- [ ] `CR4.FSGSBASE` handled correctly with respect to `swapgs`
- [ ] speculation mitigations, measured before enabling, since some cost more than the risk
```
A grep of docs/ for spectre, meltdown, kpti, speculat, mitigat, l1tf, mds, verw and ibrs matches only line 1566. Only src/addr_space.rs:3 and src/paging.rs:45 say the kernel half is shared. References: L1TF is CVE-2018-3620; EntryBleed is CVE-2022-4543.

**Impact:** No runtime effect today: TCG is the default and every process runs as root. The risk is in planning. The §18.2 "KASLR active" gate could be checked off on a layout that timing side channels reveal even on CPUs not affected by Meltdown. The §12.7 swap-entry format could be designed without L1TF in mind. The Phase 18 threat-model gate (1538, §18.8) names the deliberate gaps only after this code exists.

**Fix:**
- Split line 1566 into named items: KPTI, or an explicit out-of-scope statement tied to RDCL_NO; index masking for syscall-derived indices; eIBRS or retpoline plus IBPB and RSB fill; VERW on exit.
- Add the swapgs LFENCE as a prerequisite of line 1563.
- Add a KPTI/KASLR limitation note to §18.2 and an L1TF PTE-inversion item to §12.7.
- Correct DESIGN §4.1.
- Optionally, log the CPUID.7.EDX and ARCH_CAPABILITIES bits at boot.


#### F134 · /dev/random xorshift fallback uses the same seed every boot (Vfs.now is never set) and mislabels partial hardware fills

**Severity:** LOW · LATENT (kernfs /dev reachable from userspace; Phase 15.11) · **Confidence:** Confirmed

**Location:** src/fs/kernfs.rs:999-1015, src/fs/kernfs.rs:1220-1233, src/fs/mod.rs:616, src/fs/mod.rs:632, src/entropy.rs:51-62, src/ktest.rs:3613-3635, src/file_init.rs:27-31, docs/ROADMAP.md:855

**What's wrong:** When `hw_fill` returns fewer bytes than requested, the read pads the rest with `mix_rng`, whose first seed is `vfs.now ^ 0x9E37_79B9_7F4A_7C15`. No kernel code writes `Vfs.now`: only host tests do, and fat.rs:1700 writes a different field, `FatVol.now`. So the fallback stream is the same on every boot, and the read still returns the full length. The ROADMAP:855 claim also rests on a weak test. `XorShift` is recorded only when `i == 0`, while `hw_fill` records VirtioRng or RdRand whenever `n > 0`, so a partial hardware fill padded with xorshift is reported as hardware.

**Evidence:**
```rust
if i == 0 {
    crate::entropy::set_last_source(crate::entropy::Source::XorShift);
}
// mix_rng:
x = vfs.now ^ 0x9E37_79B9_7F4A_7C15;
```
This happens on a CPU without RDRAND once the 32-byte virtio-rng pool runs dry, for example under the ktest `qemu64,-tsc-deadline` config (Makefile:249).

**Impact:** Today nothing can observe it. Userspace and the shell cannot open kernfs /dev/random, because file_init walks only FAT and vibefs, and no kernel code consumes random bytes. `test_dev_random_source` checks only `last_source()` and skips without virtio-rng, so it cannot catch this. Once /dev is reachable, the output is the same known sequence on every boot.

**Fix:** Return a short count (or EAGAIN) instead of padding, or land the §15.11 CSPRNG. Record `XorShift` whenever any padding happens. Add a ktest for a partial fill, and run it when only RDRAND is present.


#### F135 · Panic dump is not isolated from other CPUs: raw COM1 writes and a re-entrant Serial::init can corrupt the divisor latch or flush the FIFO mid-dump

**Severity:** LOW · LATENT (real hardware) · **Confidence:** Likely

**Location:** src/panic.rs:73-83, src/serial.rs:27-38, src/serial.rs:72-79, src/serial.rs:91-101, src/ipi_init.rs:244-247, src/ipi_init.rs:123-150, src/sync_init.rs:56-65

**What's wrong:** The panic dump does not keep other CPUs off COM1.
- `halt_others()` sends a Fixed 0xFE IPI and returns without waiting.
- CPUs spinning with IF=0 in `SpinMutex::lock` or `wait_acks` never take that IPI, and `service_incoming` does not check `HALTING`.
- Once `HALTING` is set, `write_bytes_plain` and `try_write_bytes` skip the TX lock, so those CPUs write COM1 raw while the dumper runs `Serial::init()`.
- A second CPU that enters `begin_dump` re-runs `Serial::init()` unconditionally in the middle of the first dump.

**Evidence:**
```rust
x86::outb(COM1_BASE + REG_LCR, LCR_DLAB);
x86::outb(COM1_BASE + REG_DLL, (BAUD_115200_DIVISOR & 0xFF) as u8);
x86::outb(COM1_BASE + REG_DLM, (BAUD_115200_DIVISOR >> 8) as u8);
x86::outb(COM1_BASE + REG_LCR, LCR_8N1);
x86::outb(COM1_BASE + REG_FCR, FCR_ENABLE);
```
A raw data write from another CPU between the DLL write and `LCR_8N1` lands in DLL and reprograms the baud divisor. The 0xC7 FCR write drops queued TX bytes, including up to 16 bytes queued before the panic (NS16550A datasheet: with DLAB=1, offsets 0/1 are DLL/DLM; FCR bit 2 resets the transmit FIFO). A CPU in `wait_acks` waiting on the halted dumper panics after about 1 s. Under `panic_exit` it then calls `finish()`, which exits QEMU.

**Impact:** On real UARTs, panic dumps come out garbled, interleaved or truncated. In `panic_exit` builds a dump can be cut short when two CPUs fail together, though this is unlikely under QEMU's unthrottled UART. Diagnostics only.

**Fix:** Only the first dumper re-initializes the UART, and only after the other CPUs acknowledge the halt. Use NMI for the stop IPI, or as a fallback after a short timeout. Later dumpers skip `Serial::init` and spin quietly, and once `HALTING` is set only the `DUMPING` owner writes to COM1.


#### F136 · No exception diagnostics until idt::init (main.rs:193), while paging, firmware-driven ACPI parsing, heap, KVA and GDT setup run with unbounded firmware inputs

**Severity:** LOW · LATENT (Phase 20 Real hardware) · **Confidence:** Likely

**Location:** src/main.rs:136-193, src/acpi_init.rs:21-36, src/acpi_init.rs:47-70, src/acpi.rs:107-125, src/acpi.rs:519-546, src/paging.rs:252-266

**What's wrong:** The kernel loads its own IDT only at main.rs:193. Before that, pmm, the paging install (new CR3), ACPI discovery, heap, KVA, GDT and PIC setup all run under whatever IDT Limine left behind. A CPU exception or NMI in that window produces no vibeOS output; panics still print. DESIGN §3.3 orders this on purpose, because the IST stacks come from KVA, but there is no fallback handler. The ACPI walk in the window also trusts firmware addresses and lengths without bounds.

**Evidence:**
```rust
// map_gap, reached from HhdmPhys::read for any firmware-supplied address
let va = VirtAddr(paging_init::HHDM_BASE.wrapping_add(p));
... paging_init::map_4k(va, PhysAddr(p), flags)
```
Three malformed-firmware inputs cause trouble. A table address at or above MAXPHYADDR yields a PTE with reserved bits set and a #PF(RSVD). An address of 64 TiB or more puts `HHDM_BASE + phys` inside the heap, KVA, ioremap or kernel-image windows. A corrupt `hdr.length` (u32) makes `checksum_range` map and read up to 4 GiB, MMIO included, as WB before any length check. References: the Limine protocol, x86-64 entry machine state ("The IDT is in an undefined state"); Intel SDM Vol.3A §4.5 (reserved bits M–51) and §4.7 (the RSVD error-code bit).

**Impact:** On real hardware, malformed ACPI tables or an early boot-path bug reset or hang the machine with nothing on serial, which leaves nothing to diagnose. QEMU's tables are well formed, so nothing fails today.

**Fix:** Right after the Limine handshake, install a minimal early IDT without IST whose handlers print the vector and RIP through raw serial and then halt. In `HhdmPhys`/`map_gap`, reject physical addresses at or above min(MAXPHYADDR, PHYSMAP_CAP). Reject table lengths above a sane bound, such as 1 MiB, before checksumming.


#### F137 · CARGO_PROFILE=release is a supported knob that no CI job builds or boots; debug_assert invariants and overflow checks vanish in it

**Severity:** LOW · **Confidence:** Confirmed

**Location:** Makefile:14-21, Cargo.toml:38-51, src/cell.rs:54-62, src/pmm.rs:313, src/heap.rs:227-229, docs/DESIGN.md:231, docs/DESIGN.md:1682-1684

**What's wrong:** The Makefile exposes `CARGO_PROFILE=release` (it maps to `--release`), but no workflow builds or boots that profile; ci.yml and release.yml both use dev. In release every `debug_assert!` compiles out, including BootCell's set-twice check, pmm's pop-from-empty-order check and the heap bounds checks. `[profile.release]` also leaves `overflow-checks` at its default of off, since only `[profile.dev]` sets it. This contradicts DESIGN §9.4, which says any invariant that must hold in release is an `assert!`. Separately, DESIGN §2.5 still says the target JSON keeps `frame-pointer: always`. B2 deleted the target JSON, and frame pointers now come from `-C force-frame-pointers=yes` in .cargo/config.toml.

**Evidence:**
```rust
// cell.rs:55
debug_assert_eq!(self.state.load(Ordering::Acquire), UNSET, "BootCell::set twice");
// pmm.rs:313
debug_assert!(head != NULL, "pmm: pop_head on empty order {k}");
```
`make CARGO_PROFILE=release` builds a kernel whose opt level, assertion set and overflow behaviour have never been run in CI.

**Impact:** Nothing breaks today. Every BootCell set site runs once, and `set` is an `unsafe fn` whose contract forbids a second call. But a release-only miscompile or invariant break would go unnoticed. DESIGN §9 already records one release-only double-`&mut` bug that came from exactly this pattern.

**Fix:** Add a release build and e2e job, or remove the release profile and the knob. Promote the load-bearing `debug_assert!`s (pmm, heap, BootCell) to `assert!`. Correct the stale target-JSON sentence in DESIGN §2.5.


#### F138 · marker! lines and klog records are not line-atomic across CPUs; another CPU's serial output can split a contract line

**Severity:** LOW · **Confidence:** Likely

**Location:** src/serial.rs:62-79, src/serial.rs:104-116, src/serial.rs:135-153, src/log_init.rs:114-142

**What's wrong:** `Serial::write_fmt` keeps IRQs off for the whole line, but `fmt::write` hands the line over as several `write_str` pieces, and each piece takes and releases the TX `SpinMutex` by itself. Another CPU's klog record or console write can take TX between two pieces. A formatted `marker!` emits three or more pieces. The expr form (`serial::line`) emits two: the message and `"\n"`. Only a literal-only `marker!("...")` goes out as one piece. klog is not line-atomic either: `log_fmt` sends the message and the trailing newline in two separate `try_write_bytes` calls, so either half can be dropped without the other.

**Evidence:**
```rust
// serial.rs:77, runs once per write_str piece
let _g = TX.lock();
Self::write_bytes_raw(bytes);
// log_init.rs:136-139
let _ = crate::serial::Serial::try_write_bytes(msg);
let _ = crate::serial::Serial::try_write_bytes(b"\n");
```
It triggers when a second CPU writes to serial between the pieces of a BSP contract line, after `smp: done` or during ktest. The harness matches markers by substring within a single line (tests/harness/harness.py:84-87), so a split line misses its marker.

**Impact:** Intermittent harness failures that report a missing marker. A klog line can also lose its newline and run into the next line, or leave a stray blank line. Kernel state is unaffected.

**Fix:** Format each line, including its `\n`, into a stack buffer (as `log_fmt`'s `StackBuf` already does) and emit it under a single TX hold. In `log_fmt`, append the newline to the buffer so the record goes out in one `try_write_bytes` call.


#### F139 · Backtrace filter admits the ioremap MMIO window and low user memory, and syscall entry leaks the user rbp into the frame chain

**Severity:** LOW · **Confidence:** Confirmed

**Location:** src/panic.rs:55-69, src/panic.rs:153-175, src/paging.rs:691-692, src/syscall_init.rs:42-72

**What's wrong:** `stackish()` accepts any aligned address in `0xFFFF_8000_0000_0000..0xFFFF_E000_1000_0000`. That range covers the whole 256 MiB ioremap window and the UC-patched LAPIC/IOAPIC/HPET pages in the physmap. It also accepts every address below `0x2000_0000`, which is user memory under a process CR3. `dump_backtrace` then calls `read_volatile` on `[rbp]` and `[rbp+8]`. For a CPL3 exception the walker stops safely, because the user RIP fails `in_image`. The exposure is in syscalls: `vibeos_syscall_entry` pushes the user rbp but never zeroes rbp before `call vibeos_syscall_stub`. A kernel panic inside a syscall therefore walks into the user's rbp.

**Evidence:**
```rust
if p < 0x2000_0000 {
    return true;
}
if (0xFFFF_8000_0000_0000..0xFFFF_E000_1000_0000).contains(&p) {
    return true;
}
```
`IOREMAP_BASE` is `0xFFFF_E000_0000_0000` and the window is 256 MiB. Triggering this needs a separate kernel panic during a syscall while the user's rbp holds an MMIO address or a user-mapped address.

**Impact:** The panic path can read device registers that have read side effects; for example, the virtio ISR status register clears on read (virtio 1.1 §4.1.4.5). It can also print frames forged by user code, or take a #PF that cuts the dump short through the re-entry path. Diagnostics only.

**Fix:** Accept only real stack ranges: KVA stacks, the recorded boot-stack bounds, and the per-CPU IST and RSP0 stacks. Add `xor ebp, ebp` in `vibeos_syscall_entry` before the call so the frame chain ends at the syscall boundary.


#### F140 · Weak entropy sources: AT_RANDOM is derived from one TSC read, RDRAND output is never health-checked, and a virtio-rng pool refill can hand out a byte twice

**Severity:** LOW · LATENT (Phase 14 Userspace; Phase 20 Real hardware) · **Confidence:** Confirmed

**Location:** src/user_init.rs:158-165, src/x86.rs:170-201, src/entropy_init.rs:13-39, src/virtio_init.rs:177-188, src/virtio_init.rs:495-512

**What's wrong:** (a) `at_random()` is one TSC read plus a fixed hash of that same value. It bypasses the entropy hook, and CR4.TSD is never set, so ring 3 can read the TSC as well. (b) `rdrand64` trusts any value returned with CF=1 and runs no self-test at init. It also executes CPUID on every call, which is a VM exit under KVM (a performance cost only; the default accel is TCG). (c) `rng_take` wins the CAS on `POOL_POS` and only then loads `POOL[pos]`, without the queue lock. If `publish_pool` rewrites the pool and resets `POOL_POS` to 0 in between, the byte returned comes from the new pool and is handed out again by a later take.

**Evidence:**
```rust
let t = x86::lfence_rdtsc();                           // user_init.rs:159
let mix = t.wrapping_mul(0x9E37_79B9_7F4A_7C15);
if ok != 0 { return Some(val); }                       // x86.rs:195
buf[i] = POOL[pos as usize].load(Ordering::Relaxed);   // virtio_init.rs:507
```
(c) needs only one reader: `hw_fill` re-arms `rng_request` after every non-empty take, so a refill on the IRQ-thread CPU can overlap the next take. For comparison, Linux `arch/x86/kernel/cpu/rdrand.c` takes 8 samples at init and disables RDRAND unless at least 5 of them differ, because some AMD parts return all-ones with CF=1.

**Impact:** Nothing reads AT_RANDOM yet, since userspace is hand-written asm. Once a libc seeds its canaries and pointer guard from it, those values become predictable. With a stuck RDRAND, `/dev/random` returns a constant stream, because kernfs copies `hw_fill` output without mixing. Repeated pool bytes can already happen at `-smp 2`.

**Fix:** Fill AT_RANDOM from the entropy hook or a kernel CSPRNG. Self-test RDRAND once at init and cache the feature bit. Take pool bytes under the virtio queue lock, or use an epoch-tagged position.


#### F141 · expect_panic e2e mode matches markers after the panic banner and ignores exit status; harness unit tests exercise an unused matcher

**Severity:** LOW · **Confidence:** Confirmed

**Location:** tests/harness/harness.py:595-598, tests/harness/harness.py:646-670, tests/harness/harness.py:108-139, tests/harness/harness.py:1105-1111, tests/harness/test_harness.py:37-190, src/log_init.rs:277-304, docs/DESIGN.md:1339-1342, docs/DESIGN.md:1413

**What's wrong:**
- The docstring says markers are checked only against lines before the panic banner. In fact the matcher runs on every line, including the panic dump's `vibeOS: logrec:` replay of the last 24 records, which repeats marker text verbatim.
- The panic_exit status (35) is never checked.
- The harness unit tests target `check_markers_in_order`, which no runner calls. `run_qemu_and_check` has no unit tests.
- DESIGN §8.2 lists two known skips (ud2, serial loopback) that no longer exist.
- DESIGN §8.3 says "exactly N-1" `ap online` lines. The harness actually enforces at least N-1, in order.

**Evidence:**
```python
if marker_idx < len(markers) and markers[marker_idx].matches(line):
```
This check follows the expect_panic branch and does not test `panic_seen`. `Marker.matches` is a substring test, so a marker the kernel printed out of order before the panic matches again when the dump replays it.

**Impact:** Marker-ordering regressions specific to the panic_test and gp_test builds can pass. Ordering on the shared boot path is still covered by the normal e2e tier. Regressions in the panic_exit feature go unnoticed, and the harness unit tests give confidence about code the runners never use.

**Fix:** Stop marker matching at the first panic signature. Count dump banners rather than signatures, because a correct dump contains several PANIC_SIGNATURES lines. Wait for QEMU to exit instead of killing it at PANIC_DONE, then assert status 35. Delete `check_markers_in_order`, unit-test `run_qemu_and_check` through a fake line source, and update DESIGN §8.2 and §8.3.


#### F142 · Several ktests and one host test check less than their names claim; mmio_uc_flags leaves a RAM physmap leaf UC

**Severity:** LOW · **Confidence:** Confirmed (the UC-aliasing sub-claim is Likely)

**Location:** src/ktest.rs:1616-1628, src/ktest.rs:977-980, src/ktest.rs:548-565, src/ktest.rs:1245-1262, src/ktest.rs:1690-1718, src/thread.rs:248-290, src/thread.rs:456-457

**What's wrong:**
- `lock_spins` prints counters and always returns Ok.
- `int3_roundtrip` fails only if the #BP path crashes. It never checks that the handler ran.
- In `rtc_offset`, the "rtc lost" failure can never happen once the boot offset exists. The test only re-checks that `now_ns` is monotonic, never sanity-checks the RTC value, and discards the result of `deadline_after`.
- `mmio_uc_flags` UC-patches the physmap leaf covering ordinary RAM at PA 0x200000 and never restores it.
- At `-smp 4`, round-robin spawn can put both `preempt_two_threads` workers on idle APs, so the test passes without any preemption. At SMP=2 it is sound.
- The host test `switch_context_roundtrip` runs a `#[cfg(test)]` copy of the switch asm that has no `cli` or delayed `sti`. On aarch64 hosts it is compiled out entirely.

**Evidence:**
```rust
fn test_lock_spins() -> Outcome {
    let c = crate::sync_init::spin_counts();
    crate::marker!(/* counters only */);
    Outcome::Ok
}
```
The kernel never programs IA32_PAT, so PCD|PWT selects the default PAT entry 3, which is UC. Frames in that range may also be mapped WB elsewhere. Intel SDM Vol. 3A, "Programming the PAT", says mixed memory-type aliasing is not supported.

**Impact:** The 119 registered tests overstate coverage. Preemption on APs and IF-shadow ordering in the real switch asm could regress while these tests stay green. A gross switch break still fails every boot. The UC alias is harmless under emulation.

**Fix:** Move lock_spins out of the ok count. Have int3_roundtrip assert a flag set by the handler, and have rtc_offset assert a plausible year. Restore the PTE in mmio_uc_flags. Pin both preempt workers to one AP with `spawn_on`. Host-test the real switch asm with cli/sti substituted by a macro instead of keeping a duplicate. `dev_random_source` accepting RdRand is the intended fallback and needs only a rename.


#### F143 · Host-tool and initrd Makefile rules omit shared vibeos-core sources, and the macOS setup line omits dosfstools

**Severity:** LOW · **Confidence:** Confirmed

**Location:** Makefile:256-258, Makefile:165-172, src/vibefs.rs:7-8, src/fat.rs:11, src/fat.rs:2095-2097, AGENTS.md:50

**What's wrong:** The `mkfs-vibefs`/`fsck-vibefs` rule lists `src/vibefs.rs` as its only kernel source. The host tools compile all of `vibeos-core`, though, and vibefs imports `crate::fs` and `crate::part::crc32_ieee`. The `$(INITRD)` rule has the same gap: it lists `src/fat.rs` but not `src/fs/*`, which fat.rs imports. Separately, the macOS `brew install` line in AGENTS.md omits `dosfstools`. On the supported Mac host the FAT host tests therefore skip their `fsck.fat` cross-check. They do print a skip message, and CI installs dosfstools.

**Evidence:**
```make
$(MKFS_VIBEFS) $(FSCK_VIBEFS): src/vibefs.rs tests/hostlib/src/bin/mkfs_vibefs.rs \
		tests/hostlib/src/bin/fsck_vibefs.rs tests/hostlib/Cargo.toml crates/core/Cargo.toml
```
An edit to `src/fs/*`, `src/part.rs`, `src/lib.rs` or `Cargo.lock` rebuilds the kernel, but make still treats the host tools and the initrd as up to date.

**Impact:** After a change to a shared module, `make test-vibefs-crash` and the initrd build can run against stale host binaries, and the results mislead until a clean build. A CRC mismatch in particular is unlikely, because a check-value test pins `crc32_ieee` (src/part.rs:739). On macOS, local `make check` is weaker than CI.

**Fix:** Make the host-tool and initrd rules depend on `$(KERNEL_SRCS)` plus `Cargo.lock`, or always invoke cargo, which builds incrementally. Add `dosfstools` to the brew line in AGENTS.md.


#### F144 · release.yml interpolates ${{ github.ref_name }} directly into a bash run step (script-injection hygiene, no added capability)

**Severity:** LOW · **Confidence:** Confirmed

**Location:** .github/workflows/release.yml:58-59

**What's wrong:** The changelog step substitutes `${{ github.ref_name }}` into the shell script before bash runs. Git ref names may contain `$`, `(`, `;` and backticks, so a crafted `v*` tag name would run as shell in a job that holds a `contents: write` token. GitHub's Actions security-hardening guide warns against this script-injection pattern.

**Evidence:**
```yaml
- name: changelog section
  run: python3 scripts/changelog_section.py --tag "${{ github.ref_name }}" > /tmp/release-body.md
```
It triggers when someone pushes a tag whose name contains shell syntax.

**Impact:** It gives no extra privilege. Anyone who can push a tag can also push and tag a commit that changes setup.sh, the Makefile or scripts/changelog_section.py. None of those are under .github/workflows, so this needs no workflow scope. The release job then runs that code with the same token. This is a hygiene issue, not an escalation path.

**Fix:** Pass the tag through the environment: set `env: TAG: ${{ github.ref_name }}` on the step and use `--tag "$TAG"`.


#### F145 · Release publishes the unguarded ktest ISO, which overwrites raw sectors of any attached virtio-blk disk, and gates tags only on BIOS e2e

**Severity:** LOW · **Confidence:** Confirmed

**Location:** .github/workflows/release.yml:3-5, .github/workflows/release.yml:49-68, src/ktest.rs:3888, src/ktest.rs:3905, src/ktest.rs:3945, src/ktest.rs:3988-3995, src/ktest.rs:4037, src/ktest.rs:4142, src/ktest.rs:4237, src/virtio_blk_init.rs:1106-1114, CHANGELOG.md:23

**What's wrong:** On a `v*` tag, release.yml runs only `make test-e2e` (BIOS) and then publishes `vibeos-ktest.iso`. It does not depend on ci, and ci does not run on tag pushes. The ktest ISO runs every test at boot. Its virtio-blk tests check only `virtio_blk_init::live()` and write fixed LBAs without confirming that the disk is a harness scratch disk. Shipping both ISOs was a deliberate B3 choice, but the artifact carries no warning and the kernel has no guard. The GPT stamp on unpartitioned disks comes from `part_init::init` and ships in both ISOs, so it is not part of this finding.

**Evidence:**
```yaml
- name: build ktest ISO
  run: make vibeos-ktest.iso
  files: |
    vibeos.iso
    vibeos-ktest.iso
```
The tests write LBA 1 (the primary GPT header), LBAs 2, 5-7 and 10-24, worker spans from LBA 32, LBA 2048 (the usual first-partition start), and sector 1 of vdap1. Booting the published ISO in a VM with an existing image attached as virtio-blk triggers these writes.

**Impact:** That image loses its partition table and the start of partition 1. Physical machines have no virtio-blk, and `make run` and the harness never attach a valuable disk, so this takes a hand-built QEMU command. A tag on a commit whose ktest or UEFI tiers fail would also publish, though CI on main makes that unlikely.

**Fix:** Drop `vibeos-ktest.iso` from the release files, or make ktest require a harness-written magic sector before any write test. In release.yml, run `make test-kernel` and the UEFI tier, or verify that ci passed for `github.sha`.


#### F146 · Test hooks ship in production exception handlers, the catch state is global across CPUs, and vblk_deep returns with I/O still in flight

**Severity:** LOW · **Confidence:** Confirmed

**Location:** src/arch/idt.rs:38-146, src/arch/catch.rs:6, src/arch/catch.rs:44-46, src/arch/catch.rs:172-193, src/block_init.rs:124-133, src/block_init.rs:145-156, src/ktest.rs:3995-4013

**What's wrong:** Three related test-hook problems. (1) `catch::intercept` runs first in every IDT exception handler with no `kernel_tests` gate, and the `FAIL_NEXT` fault-injection loop runs on every ramdisk request in production. The `on_panic` and `on_alloc_error` hooks are correctly gated. (2) `KIND`, `WANT` and `vibeos_jmpbuf` are single globals, and `intercept` checks neither which CPU armed the catch nor the frame's CPL. (3) `test_block_vblk_deep` returns on the first failed submit or wait while other requests still point at its stack buffers and stack `IoWaiter`s.

**Evidence:**
```rust
if catch::intercept(N, &mut frame, 0) {   // idt.rs:40, no cfg
...
ST_VECTOR if want == vector => {         // catch.rs:176
    record(vector, frame, err);
    KIND.store(ST_OFF, Ordering::Release);
    unsafe { vibeos_longjmp(core::ptr::addr_of_mut!(vibeos_jmpbuf), 1) };
```
While a ktest has a vector armed, the same vector on another CPU (for example a user #PF on an AP) longjmps onto the arming CPU's saved rsp/rip, with GS still swapped if the frame came from CPL3. In vblk_deep, a later completion calls `finish` through `complete_req` on an `IoWaiter` in a dead frame, and the device may still DMA to or from stale buffer addresses.

**Impact:** No production impact today. Only the gated ktest module writes `KIND`, so `intercept` is one atomic load that returns false. In ktest runs, a cross-CPU fault during a catch window, or a misbehaving virtio-blk driver, can corrupt another stack and hide the original failure. The `FAIL_NEXT` part is already tracked as Q2 (ROADMAP.md:877).

**Fix:** Gate the catch module, the `intercept` calls and the `FAIL_NEXT` check behind `kernel_tests`. Record the arming CPU and require CPL0 in `intercept` and `on_panic`. In `test_block_vblk_deep`, wait for every successfully submitted request before returning a failure.


#### F147 · Warning and lint gates are weaker than documented: kernel builds drop [build] -D warnings, CI never clippy-checks the shipped feature set, Python linters are unpinned

**Severity:** LOW · **Confidence:** Confirmed

**Location:** .cargo/config.toml:1-16, .github/workflows/ci.yml:46-49, .github/workflows/ci.yml:121-126, Makefile:135-148, scripts/check_changelog.py:12, CHANGELOG.md:30-33, docs/ROADMAP.md:35, docs/ROADMAP.md:170, docs/ROADMAP.md:862, AGENTS.md:22, src/main.rs:24, src/main.rs:270-288

**What's wrong:** (1) Cargo uses exactly one rustflags source, and `target.<triple>.rustflags` takes precedence over `build.rustflags`. So `-D warnings` never reaches any `x86_64-unknown-none` build, including `make iso`. (2) CI runs clippy with `--all-features`, `kernel_tests` and `vibefs_crash`, but never with the default features that ship. No CI clippy run sees code that needs both negations (main.rs:270-275, 284-288), and `--all-features` lints the least boot code: `panic_test` compiles out `normal_boot_tail` and allows `dead_code` crate-wide. (3) CI installs ruff and mypy unpinned, and `make check` only prints a message when they are missing. (4) `check_changelog` allows 3 lines, but AGENTS says 2 or fewer.

**Evidence:**
```toml
[build]
rustflags = ["-D", "warnings"]
[target.x86_64-unknown-none]
rustflags = ["-C", "force-frame-pointers=yes", ...]
```
`cargo check --bin vibeos -v` shows no `-D warnings` on the rustc line (The Cargo Book, Configuration, `build.rustflags`). Default-feature clippy with `-D warnings` is clean today, so a gate is missing but no warning exists yet.

**Impact:** New warnings, including lints that stand in for correctness checks, can land in the shipped kernel without failing CI. The deny-warnings claims in CHANGELOG and ROADMAP are false for kernel builds, and so is "pinned so CI cannot drift" as far as the Python linters go.

**Fix:** Add `"-D", "warnings"` to the target rustflags table, and add `cargo clippy --bin vibeos -- -D warnings` for the default features. Pin ruff and mypy, and fail in CI when they are absent. Set `MAX_LINES = 2`.


#### F148 · Load-bearing DESIGN and code-comment claims that the code contradicts (grouped)

**Severity:** LOW · **Confidence:** Confirmed

**Location:** docs/DESIGN.md:160, 230-231, 234, 665, 696, 1135-1136, 1161-1162, 1339-1342, 1415; src/paging_init.rs:11-12; src/paging.rs:202, 383; src/arch/gdt.rs:18, 183; src/per_cpu.rs:43; src/arch/trampoline.S:8; Makefile:32, 67

**What's wrong:** Design text and comments that agents rely on contradict the code.
- **DESIGN:**
  - #DF is not the only IST handler; #DB, NMI and #MC also have IST stacks.
  - No target JSON exists.
  - "Never panics on data" is false: vibefs `load_inode_leaf` and `load_dir_leaf` index past 4096 when a CRC-valid leaf's `count` exceeds the per-leaf maximum.
  - `copy_from_user`/`copy_to_user` with `stac`/`clac` do not exist.
  - A user `#BP` does not "log, continue".
  - "GS base never changes" is stale now that `swapgs` is live.
  - The "two known skips" are gone.
  - The harness checks at-least-N-1-in-order, not exactly N-1.
- **Comments:**
  - paging_init.rs says invlpg is baked into `Mapper`, but paging.rs:831 says it deliberately is not.
  - `tlb_invalidate_page` does not exist.
  - `patch_physmap_uc` does not use `MapMode::Remap`.
  - Hardware never updates RSP0.
  - `syscall_scratch` is in use, not future.
  - trampoline.S still says "nasm layout".
  - gdt.rs says DESIGN specifies one IST page; it specifies four.
  - The Makefile labels ROADMAP §0.5 and §5.6 as DESIGN.

**Evidence:**
```rust
ist_type: 0x8E00 | (ist as u16 & 7),   // src/desc.rs:163: every gate is DPL 0
```
INT3 at CPL3 through a DPL-0 gate raises #GP (Intel SDM Vol. 3A §6.12.1.1), which kills the process with SIGSEGV.

**Impact:** An agent that trusts DESIGN §7.5 could call `PerCpu::current()` in an ISR before `gs_enter`, or assume vibefs parsing cannot panic. The other items lead to wasted time and wrong edits.

**Fix:** Correct every line in one doc commit. File the vibefs count bound and the #BP gate DPL as separate bugs, and add a script that checks `DESIGN §x.y` references.


#### F149 · Linux syscall numbers with undocumented non-Linux semantics and no reserved-bit checks (kill, wait4, open)

**Severity:** LOW · LATENT (libc port) · **Confidence:** Confirmed

**Location:** src/proc_init.rs:1143-1160, src/proc_init.rs:1053-1060, src/proc_init.rs:1114-1133, src/proc_init.rs:445, src/proc_init.rs:687-697, src/syscall.rs:67, docs/SYSCALL.md:3-4

**What's wrong:** SYSCALL.md says numbers match Linux "where they exist", but several calls diverge from Linux and the doc does not say so.
- `kill(pid, 0)` returns EINVAL instead of probing for existence.
- `kill(0, s)` and `kill(-1, s)` return ESRCH instead of signalling the process group or broadcasting.
- `wait4(0)` returns ECHILD, and `pid < -1` means any child.
- Unknown `wait4` option bits are accepted silently, and `rusage` (r10) never reaches the handler.
- `open` ignores `mode`.
- `psinfo = 500` does not collide today, but it sits in the range Linux will allocate next.

**Evidence:**
```rust
    if sig == 0 || sig > 31 {           // sys_kill
        return syscall::neg(EINVAL);
    }
    let want = pid as i64;              // sys_wait4
    let nohang = options & WNOHANG != 0;
```
Linux kill(2) defines signal 0 as an existence and permission check. wait4(2) defines pid 0 and pid < -1 as process-group waits. wait4 reads the pid as 64 bits, while kill truncates it to 32. The two differ only for callers that zero-extend a 32-bit pid; libc wrappers sign-extend.

**Impact:** No user program calls kill today. Ported libc programs will misbehave. Because reserved option bits are not rejected, those bits cannot be given a meaning later without breaking existing callers.

**Fix:** Truncate pid_t to i32 in both calls. Implement signal 0, return EINVAL for unknown `wait4` option bits, and zero-fill `rusage` when the pointer is non-null. Document the process-group gaps in SYSCALL.md, and move psinfo out of the Linux number range or into procfs.


#### F150 · The syscall metadata table, validate_args, the tracing flag and the syscall counters are dead in production but documented and ticked as done

**Severity:** LOW · **Confidence:** Confirmed

**Location:** src/syscall.rs:200-374, src/proc_init.rs:402-450, src/syscall_init.rs:446, src/syscall_init.rs:491-510, src/syscall_init.rs:587-593, src/thread.rs:149-150, docs/SYSCALL.md:68-96, docs/SYSCALL.md:134-141, docs/ROADMAP.md:784, docs/ROADMAP.md:790-791

**What's wrong:** Dispatch is a plain `match nr`, and each handler validates its own pointers.
- The table's `arity`, `ptr_mask` and `len_arg` fields and `validate_args` are used only by host tests. Production reads only `.name`, for trace output.
- `set_trace` has no caller, so tracing can never be turned on.
- The per-TCB `syscall_count` and the global `SYSCALLS` are incremented on every syscall and never read. `syscall_count()` has no caller, and the per-process aggregation that the thread.rs comment promises does not exist.

SYSCALL.md §3 and §6, and three ticked ROADMAP §9.3 boxes, describe all of this as done.

**Evidence:**
```rust
#[cfg_attr(feature = "kernel_tests", allow(dead_code))] // tracing / procfs; parked
pub fn set_trace(on: bool) {
```
The table has already drifted. `WAIT4.ptr_mask` is 0, although SYSCALL.md documents wait4's optional `rsi` status pointer. SYSCALL.md §6 also names `syscall::set_trace`, but the function is `syscall_init::set_trace`.

**Impact:** The docs and the roadmap claim a table-driven validation policy, tracing and a procfs counter, none of which exist. Driving dispatch from the table as it stands would under-validate wait4. Every syscall also pays for a global `fetch_add` that other CPUs contend on.

**Fix:** Either drive dispatch from the table, fixing WAIT4 first and checking arity and pointers up front, or delete the unused fields and `validate_args`. Wire tracing to a boot parameter or a shell command, or remove it. Correct SYSCALL.md and untick the ROADMAP §9.3 items that are not reachable.


#### F151 · Kernel image embeds build-host paths: loaded .rodata varies with checkout directory and toolchain location, and local ISOs carry the builder's login

**Severity:** LOW · LATENT (Phase 22 release engineering: "byte-identical artifacts" exit gate, 22.1 "no paths") · **Confidence:** Confirmed

**Location:** .cargo/config.toml:9-16, Cargo.toml:45, Cargo.toml:51, rust-toolchain.toml:5, scripts/gen_ksyms.py:44-67, scripts/mkiso.sh:20, .github/workflows/release.yml:50

**What's wrong:** No path remapping is configured: there is no `--remap-path-prefix` and no `trim-paths`. Because rust-src is installed, panic `Location` strings from core/alloc generics instantiated in the kernel resolve to the local sysroot path, and limine's resolve to the `$CARGO_HOME` registry path, all in loaded .rodata. ThinLTO `.llvm.<hash>` suffixes are computed over bitcode that includes the DWARF comp_dir, and gen_ksyms.py copies them verbatim into KSYMS, which also lives in .rodata. With `debug = true`, the unstripped ELF inside the ISO also carries the checkout path in DWARF.

**Evidence:**
```
/Users/skid/.rustup/toolchains/nightly-2026-09-22-aarch64-apple-darwin/lib/rustlib/src/rust/library/core/src/...
vibeos::sched_init::idle_main (.llvm.4665469366746335357)
```
Building HEAD from two checkout paths gave 65 of 1482 KSYMS names that differed only in the hash, so the loaded .rodata differed. Whether .text immediates also shift depends on the hash-string lengths in that run. With `-Z trim-paths` and `profile.dev.trim-paths="all"`, both paths produced a byte-identical ELF containing no host paths.

**Impact:** The same commit is not reproducible across checkout directories, or between a macOS dev box and CI. The release ELF embeds /home/runner paths. Locally built ISOs shared in bug reports expose the builder's login in kernel memory and in DWARF. Symbolization is unaffected: .text addresses are stable and the ISO ships its own ELF. Shifting .data/.bss by a page would take a path-length difference of about 1.7 KB, and the macOS-to-CI difference is far smaller today.

**Fix:** Enable `trim-paths = "all"` (with `cargo-features = ["trim-paths"]`) in both profiles, or pass `-Z trim-paths` in the Makefile and CI. This was verified sufficient on the pinned nightly. Optionally strip `.llvm.<digits>` in gen_ksyms.py and add a CI check that builds from two directories and compares the output. ISO timestamps need a separate fix.


#### F152 · ISO assembly is nondeterministic: build-time dates, a time(NULL)-seeded MBR disk signature, random GPT GUIDs, and the builder's uid/gid in Rock Ridge

**Severity:** LOW · LATENT (Phase 22 release engineering: "byte-identical artifacts" exit gate, 22.1 "no timestamps") · **Confidence:** Confirmed

**Location:** scripts/mkiso.sh:20-32, Makefile:76-77, .github/workflows/release.yml:20, .github/workflows/release.yml:50, .github/workflows/release.yml:62-68, limine/limine.c:866-870 (cloned, pinned Limine; not repo source)

**What's wrong:** mkiso.sh stages files with plain `cp` and runs `xorriso -as mkisofs` with no SOURCE_DATE_EPOCH, no `-r` and no fixed file dates. As a result, volume and file dates are the build time, and Rock Ridge records the builder's uid/gid. `limine bios-install` then converts xorriso's GPT to MBR and writes a disk signature seeded from `time(NULL)`, and a leftover GPT entry array with random GUIDs survives the conversion. xorriso is not pinned, and its version string lands in the PVD preparer field.

**Evidence:**
```c
// Generate pseudorandom MBR disk ID.
srand(time(NULL));
for (size_t i = 0; i < 4; i++) { uint8_t r = rand(); device_write(&r, 0x1b8 + i, 1); }
```
Two mkiso.sh runs 2 s apart on the same ELF differed in 124 bytes: the MBR ID (0x1B8-0x1BB), the PVD dates (sector 16), directory-record dates (sectors 19 and 21-23) and the GPT GUIDs (sector 5979). SOURCE_DATE_EPOCH alone is not enough: xorrisofs then falls back to file mtimes, and limine's disk ID is written outside xorriso. Specs: ECMA-119 §8.4.20, §8.4.26-27, §9.1.5; UEFI §5.2.1 (disk signature at offset 440).

**Impact:** Every build of the same commit produces a different vibeos.iso hash, so releases, which ship without a checksum file, cannot be verified against their tag. Local ISOs embed the builder's uid/gid. There is no runtime effect.

**Fix:** In mkiso.sh, default SOURCE_DATE_EPOCH to `git log -1 --format=%ct` and pass `-r` plus `--set_all_file_dates` derived from it. After bios-install, overwrite the 4 bytes at 0x1B8 with a content-derived value; this is safe because limine.conf addresses the kernel as `boot():`, not by disk ID. Keep the GPT-to-MBR conversion and do not patch limine/. Record the xorriso version used for releases and publish SHA256SUMS.

---

## 5. AI-pattern analysis

Each verified finding was tagged with the AI failure mode it shows. The rough counts are in the table, and each pattern gets a paragraph after it. The counts overlap, because one finding can show several patterns.

| Pattern | Findings | Where it concentrates |
|---|---|---|
| Claimed-but-missing behaviour | ~25 | DESIGN/ROADMAP/VIBEFS claims, harness "skip", crash-consistency gate |
| Session drift / duplication | ~22 | Entry paths, two user-execution models, two file stacks, FS slot locks |
| Tutorial shortcut made permanent | ~19 | Identity map, boot stack, fixed global tables, auto-GPT, `expect()` on exhaustion |
| Tests that can't fail or mask failures | ~15 | Harness retry list, vibefs crash test, e2e contract, several ktests and host tests |
| Unverified SAFETY / soundness claims | ~12 | Cells, per-CPU, AddressSpace lifetimes, MM ownership tokens |
| Cargo-culted patterns | ~10 | Unbounded `unsafe impl Sync`, setjmp/longjmp across Rust frames, copied guards |
| Hallucinated hardware / tooling facts | 5 | Virtio barriers, CF8 bus-0 claim, FADT bit 0, OVMF `-bios`, "hardware updates RSP0" |
| Over-engineering | ~4 | Unreachable VFS + kernfs, a `BlockDevice` trait nobody uses, dead syscall metadata |

**Claimed-but-missing behaviour** is the dominant failure. The docs are long and confident, and a large share of their load-bearing sentences are false. Examples:
- The identity window is never torn down, although DESIGN says it should be.
- `invlpg` is not "baked into" `Mapper`.
- `copy_from_user` does not use `stac`/`clac`.
- The "GS base never changes" rule no longer holds.
- Block Barrier and Flush do not provide the semantics DESIGN §10.2 describes.
- vibefs directories are not B-trees.
- `-D warnings` does not apply to kernel builds.
- The ROADMAP marks "every syscall argument validated", "fork bomb bounded" and "PT_TLS" as done without the backing code or tests.
- The UEFI e2e "skip" does not skip.
- The crash-consistency gate cannot detect crash-consistency bugs.

The pattern is consistent. An agent writes the intent into a doc or a checkbox in the same change that implements part of it, and no later session checks the claim against the code.

**Session drift** is visible in the entry paths. Phase 9A added `gs_enter`/`gs_leave` to every handler in `idt.rs`, but the device-pool and keyboard stubs are installed from other files and were never updated. `enter_user_full` is a near-copy of `enter_user` without the `cli` its caller supplies. Other examples:
- The bound `run_user` model (Slice B) still runs beside the spawned-process model (Slice C), with its own global `CURRENT_AS`, `USER_JMP` and `STDOUT` state.
- setjmp/longjmp exists twice, byte for byte.
- `fat_init` and `vibefs_init` are copy-paste twins with diverging `drop_slot`.
- virtio-blk has two "running" flags.
- There are two complete file stacks. The VFS and kernfs (about 4.6k lines) cannot be reached from any syscall, and the File API resolves paths through its own route tables.

**Tutorial shortcuts made permanent.** Several early scaffolds were never retired:
- **Low identity map.** It is permanent, GLOBAL, and RWX over the first 2 MiB, and it maps VA 0 in the kernel CR3. As a result, `try_current`'s null check never fires.
- **Boot stack.** The unguarded 64 KiB Limine stack runs all of boot and every ktest.
- **Fixed tables.** The 64-slot thread table, the 16-entry global open-file table, the global CWD, and the 8-slot deferred-stack list that panics when full.
- **`expect()` on exhaustion.** Resource exhaustion on paths userspace can reach ends in `expect()` or `panic!` instead of an errno.
- **GPT auto-stamp.** It exists only so ktests find `vdap1`/`vdap2`, yet it runs in every build.

**Tests that can't fail, and a harness that hides failures.** This is the most damaging pattern, because it removes the feedback loop the project depends on.
- **Retries.** `retryable_ktest_failure` and `_retry_hang` retry any timeout, and on `-smp 4` they also retry kernel panics whose text matches `ipi: ack timeout` or `ipi_init::wait_acks`. Commit 92cc152 (#75) reverted a kernel fix and replaced it with a retry.
- **vibefs crash test.** It passed for 8 rounds while the filesystem it tests had filled up at generation 57 and was failing every commit.
- **e2e contract.** It ends at a `shell ready` line that a ring-3 program prints, and it never checks the `/bin/tests` result.
- **Host tests of the wrong code.**
  - The `switch_context` host test runs a different `#[cfg(test)]` asm body from the one the kernel runs.
  - The cell host tests use a stub `InterruptGuard`.
  - The ECAM unit test locks in the wrong addressing formula.
  - The `now_us` monotonicity ktests are monotonic by construction.

**Unverified safety claims.**
- **Unsafe blocks.** 651 `unsafe {}` blocks carry one `// SAFETY:` comment, and that comment is inaccurate.
- **Unbounded cells.** The two cell types AGENTS.md makes mandatory are themselves unbounded `unsafe impl<T> Sync`.
- **Fabricated lifetimes.** `&'static AddressSpace` is built from a `Box` owned by a table. `&'static PerCpu` aliases `&mut`.
- **False comments.** "Not `Sync`", "hardware updates RSP0" and "VA 0 is never mapped" are all false.

**Cargo-culted patterns:**
- `x86-interrupt` handlers with swapgs keyed only on CS, and no paranoid path for IST vectors or for `iretq` faults. That is the textbook shape without the surrounding invariants.
- setjmp/longjmp across Rust frames, skipping destructors.
- Mutex-style `Send`/`Sync` impls copied without the guard bounds.
- A hand-rolled busy-flag lock copied into two filesystems.

**Hallucinated hardware facts** are rarer than feared.
- **What checks out.** The register-level constants hold up against the SDM and specs: APIC offsets, the IOAPIC write order, the LVT→MFENCE→TSC_DEADLINE sequence, the trampoline CR0/CR4/EFER values, the IST numbering, and the error-code vector set.
- **What is wrong** is semantics, not constants:
  - "CF8 only reaches bus 0" is false.
  - The MCFG base is treated as `start_bus`-relative.
  - The 8259's presence is read from FADT bit 0 (LEGACY_DEVICES) instead of MADT PCAT_COMPAT.
  - `Release` + `sfence` is treated as a store→load barrier for virtio EVENT_IDX.
  - The virtio doorbell always writes 0 instead of the queue index.
  - The Homebrew OVMF image is documented as usable via `-bios`.

**Over-engineering:**
- the unreachable VFS/kernfs layer;
- a `BlockDevice` trait that production code bypasses with hard-coded device ids;
- a syscall metadata table with arity and pointer masks that dispatch never consults;
- `relink` maintaining a shadow run-list that only ktests read.

**License contamination.** One reviewer was asked to spot-check the most tutorial-shaped code (the trampoline, PIC remap, PIT, 8042, FAT LFN handling, CRC32) against GPL kernels. They reported no concrete match. No systematic similarity scan was run, so this is not a clearance. The project is MIT; blog_os (MIT/Apache) and xv6 (MIT) would be compatible sources anyway.

**Least trustworthy parts, in order:**
1. Privilege-transition code: `syscall_init.rs` entry/exit, `enter_user*`, every IDT handler installed outside `idt.rs`, and the ring-3 exception policy.
2. Thread exit and reaping, and every "publish, then keep touching" completion protocol (`IoWaiter`, deferred stacks, dead-slot reuse).
3. Block cache, block queue and virtio-blk completion, which are correct only for the synchronous ramdisk they were first written against.
4. vibefs, which disagrees with its own format doc and leaks on every commit.
5. The test harness, which in several places is tuned to report green.

The most trustworthy parts are the host-tested portable parsers and allocators, the IOAPIC/LAPIC programming, and the marker contract mechanics.

---

## 6. Top 5

If only five things get fixed, fix these, in this order.

**1. Rebuild the privilege-transition path as one audited mechanism.**
- **The defects.**
  - The device-pool and keyboard ISRs never `swapgs`.
  - `enter_user_full` zeroes the GS base with interrupts enabled.
  - The syscall exit path can run with IF=1 after a console read, and loads the user RSP before `sysretq`.
  - Nothing handles faults on `iretq` or IST vectors arriving in the swapgs windows.
  - Ring-3 #DB/#AC halt the machine.
  - RFLAGS.AC survives interrupt entry.
- **Why one fix.** These are one defect class: GS and IF state at the ring boundary is decided ad hoc in seven places.
- **The fix.**
  - A single asm entry stub per vector, generated from one table.
  - `cli` before every return-to-user sequence, backed by assertions.
  - A ring-3 exception policy table in which every vector kills the process, never the kernel.
  - A ktest that runs ring 3 with IF=1 and delivers every vector class.
- **Findings:** F001 (CRITICAL), F004, F005, F006, F007, F088.

**2. Fix "publish, then keep touching" lifetimes, and write the rule down.**
- **The defects.**
  - `IoWaiter::finish` wakes a wait queue on a stack frame that may already be gone (CRITICAL).
  - An exiting thread's stack can be unmapped by another CPU's reaper while the exiting CPU still runs on it.
  - A Dead TCB slot can be reused while its thread is still switching out.
  - Condvar wakeups placed after the lock drops can be stranded.
- **The fix.**
  - Make the completion store the last access to the waiter.
  - Tag deferred stacks with their owning CPU, and free them only after that CPU has switched.
  - Take SCHED when publishing death, and exclude slots that are still on a CPU.
  - Add the "publish last" rule to DESIGN §9.4 (guardrail 5).
- **Findings:** F002 (CRITICAL), F012, F034.

**3. Stop destroying and silently corrupting data.**
- **Remove the destructive defaults.**
  - Remove the vda GPT auto-stamp from production builds. It is a few lines, and it is the only CRITICAL here with a one-line trigger (attach a disk).
  - Fix the vibefs commit leak and the stale alloc map; the volume stops committing at generation 57.
  - Give FAT an in-core inode, so descriptors stop acting on stale cluster and dirent snapshots.
- **Before any disk-backed mount becomes reachable from userspace:**
  - implement the documented Barrier/Flush semantics for virtio-blk;
  - add in-flight state to the block cache;
  - bound-check vibefs logical block math.
- **Findings:** F003 (CRITICAL), F013, F014, F015, F043, F049.

**4. Make the test signal honest.** A kernel written by agents is only as good as the signal the agents get back.
- **Stop hiding failures.** Delete the harness rules that retry kernel panics and timeouts, then fix what they were hiding:
  - the smp4 IPI-ack panic;
  - the #75 wait4 stall;
  - the lapic-fallback hang;
  - the `reap_many_via_idle` frame-count race.
- **Make the tests check something.**
  - e2e asserts `/bin/tests`' result.
  - The vibefs crash test can fail: write-through cache, a kill point across the whole run, contents checked.
  - `make test` passes on macOS.
  - ROADMAP boxes are re-opened where no test backs them.
- **Findings:** F021, F073, F074, F077, F079, F080.

**5. Turn every user-reachable exhaustion panic into an error.**
- **Kernel-panic paths a process can drive:**
  - fork against a full thread table or KVA;
  - more than 8 exits between reaps, which overflows the DEFERRED list;
  - an ELF with a huge `p_memsz`, which drains all RAM under the PT lock and leaves heap growth to panic;
  - the 1 s IPI-ack timeout under IF-off load;
  - vibefs logical-block overflow via `lseek`.
- **The fix.** Cap `p_memsz` and total mapped pages per process. Return EAGAIN/ENOMEM from fork. Make the deferred list unbounded or drained on demand. Replace the ack-timeout panic with a diagnostic and continued waiting while the target is making progress. Deny `expect`/`panic` in syscall-reachable modules (guardrail 4).
- **Findings:** F008, F009, F010, F011.

---

## 7. Milestone outlook

What breaks, or gets expensive, at each upcoming ROADMAP phase if nothing changes.

**Phase 10: Consolidation (now).** This phase is the natural home for the Top 5 (§6). Do the soundness work below before Phase 12 builds on it:
- bounded cells, and `unsafe` MM APIs with non-`Copy` ownership tokens;
- `AddressSpace` lifetimes carried in types, not `&'static`;
- one user-execution model instead of two;
- one file stack instead of two.

Every later phase adds callers to these APIs, so each fix costs more the longer it waits.

**Phase 11: Portability (aarch64).**
- **Memory ordering.** Several protocols are correct only under x86 TSO. The IPI slot publication is Relaxed stores plus `compiler_fence`. The seqlock writer has no release fence after the odd bump. Virtio relies on `sfence`/`lfence`. With the unbounded cells, these become real races on a weakly ordered CPU.
- **x86 code in the "portable" crate.** `vibeos-core` contains x86 asm (`thread.rs` switch, `dma.rs` fences). `InterruptGuard`, `IrqCell` and `SpinMutex` all depend on `gs:[0]`. The catch mechanism is wired into x86 IDT handlers.
- **Where to start.** Split each of these along an arch seam before writing any aarch64 code.

**Phase 12: Fault-driven Memory (demand paging, COW).** This is where the latent memory findings turn critical:
- **`write_bytes` ignores the PTE W bit.** A `read()` into a COW-shared page would write the shared frame, which is a cross-process write. Fix it first.
- **No remote shootdown for user mappings.** User pages are never shot down on other CPUs, and `unmap` frees frames before `invlpg`.
- **`free_user_half` frees every leaf.** It assumes each user leaf is an exclusively owned buddy frame, so it will free shared or device frames. There is no per-frame metadata (refcounts) in the PMM yet.
- **Other gaps:**
  - kernel PML4 slots created later are never propagated to existing address spaces;
  - `clone_full` copies under the PT lock with IRQs off;
  - buddy frees are O(n²) under the BUDDY lock;
  - a kernel-mode #PF is a halt;
  - nothing recovers a fault taken during a user copy.

**Phase 13: Threads, IPC, Signals, POSIX.**
- **Per-thread CPU state.** FS_BASE is not per-thread, so TLS breaks as soon as two threads have different bases.
- **Single-thread assumptions:**
  - `current_space()` returns `&'static` into a table-owned Box;
  - the check/copy pair in `read_bytes`/`write_bytes` is a TOCTOU window once threads or munmap exist;
  - `CURRENT_AS`, `USER_JMP`, `STDOUT` and `IN_USER` are system-wide globals.
- **Signals.** They are delivered only at syscall entry. A compute-bound process can't be stopped or killed, and a SIGCONT can be lost.
- **Latent sync bugs go live.** The Condvar deferred-wakeup and RwLock writer-timeout bugs become reachable as soon as real callers exist.
- **Global file state.** The open-file table and the CWD are global. Fids shared across fork/dup are updated without a lock.

**Phase 14: Userspace (libc, real binaries).**
- **ELF loader.** It zeroes a page shared by two PT_LOADs and drops the second segment's permissions. Real linkers routinely emit that layout.
- **ABI semantics.**
  - errno values are wrong (EMFILE for a full disk, EINVAL for corruption);
  - several Linux syscall numbers carry non-Linux semantics (`wait4` ignores rusage, open flags truncated to u32, `psinfo` at 500);
  - no reserved bits are rejected, so the ABI cannot be extended later without breaking binaries.
- **Entropy.** AT_RANDOM is one TSC read.

**Persistent storage (whenever a disk-backed root or a mount syscall arrives).**
- **Destructive auto-format.** The GPT auto-stamp destroys whole-disk images.
- **Durability contract.** Barrier and Flush do not order completion. The cache has no in-flight state.
- **vibefs.**
  - It leaks a block per commit.
  - Its alloc map keeps counting replaced blocks.
  - Mount trusts every on-disk count and pointer.
- **FAT.** Open files cache cluster and dirent state that other descriptors invalidate. `mount fat32` overflows a 16 KiB kernel stack.

**Phase 15: Networking.**
- **Device trust.** The virtio transport trusts device-supplied used-ring ids, indices and capability bounds. DMA buffers are freed before the device is reset. Bus mastering is enabled before probe and never revoked.
- **Virtio conformance.** The EVENT_IDX kick path has no full barrier. The doorbell writes 0, which breaks multi-queue devices that share a notify address.
- **Shared-cost paths.** The IRQ-off console scroll and the single threaded-IRQ CPU become shared-cost paths.

**Phase 18: Hardening.**
- **Missing mitigations.**
  - There is no KPTI, which matters on Meltdown-affected CPUs.
  - SMEP, SMAP and UMIP are silently optional.
  - RFLAGS.AC is not cleared on interrupt entry.
- **W^X gaps.** The physmap gives kernel `.text` a writable alias. The low identity map is RWX and GLOBAL.
- **Test hooks in production.** `catch::intercept` sits in every exception handler of the production kernel.
- **Unsafe audit.** 650 unsafe blocks have no written justification. Retrofitting SAFETY comments at that point means re-auditing the whole tree.

**Phase 20: Real Hardware.** Expect boot failures or misbehaviour from:
- a GOP framebuffer above 8 GiB (the boot halts at console init);
- x2APIC-mode handoff (#GP in `enable_lapic`);
- CR0.NE, CR4.MCE and OSXMMEXCPT never being set;
- the kernel PML4 landing above 4 GiB, so every AP is silently skipped;
- duplicate MADT APIC IDs;
- an AP that starts after the timeout, on freed resources;
- BAR sizing with decode enabled;
- ECAM addressing with `start_bus != 0`;
- UC patches that alias RAM;
- QEMU-only poweroff and reboot constants;
- the untested TSC-deadline path;
- no diagnostics before the IDT exists.

**More than about 22 CPUs.** The 64-slot thread table is exhausted at boot. Every CPU bitset is limited to 64.

---

## 8. Guardrails for future agents

These are written to be pasted into `AGENTS.md`, or linked from it, as binding rules. Each maps to a class of bug found in this review.

### 8.1 Rules
1. **Privilege transitions have one implementation.** Every IDT vector enters through one asm stub generated from one table in `arch/`. The stub does `swapgs` iff CS.RPL==3, plus `cld` and `clac`. It also handles the IST vectors (NMI/#MC/#DB) by reading the GS MSR rather than trusting CS, and fixes up faults taken on `iretq`/`sysretq`. No raw `extern "x86-interrupt"` function may be installed from outside `arch/idt.rs`. `idt::set_handler` takes a body function, never a gate.
2. **IF=0 from the moment a return-to-user sequence starts.** Syscall exit, `enter_user*` and every iret-to-user path execute `cli` first and `debug_assert!(!interrupts_enabled())`. No syscall handler may leave IF in a different state than it found it.
3. **Ring 3 can never halt the kernel.** Every exception vector has an explicit ring-3 policy row (signal or kill) in a table that a host test checks against the full vector list. `exception_halt` must be unreachable when the interrupted CS is ring 3.
4. **No `expect`/`unwrap`/`panic!`/`assert!` on any path reachable from a syscall or a device.** Return an errno or error instead. Keep a list of syscall-reachable modules and deny `clippy::expect_used`, `unwrap_used` and `panic` there, as the portable half already does. Arithmetic on user-controlled values uses `checked_*`, because the shipped profile has overflow checks on.
5. **Publish last.** In any completion or handoff protocol, the store that lets the other side free or reuse an object must be the last access to that object. The same rule covers anything deferred for cross-CPU reclaim: it may be freed only after the owning CPU has passed a quiescent point (a context switch). Put this in DESIGN §9.4.
6. **Soundness is typed.** New `unsafe impl Send/Sync` must carry the bounds std's equivalent would (`T: Send` for mutex-like types, `T: Sync` for shared-reference types). Functions that can cause UB with bad arguments are `unsafe fn`. Ownership tokens (frames, stacks, DMA buffers, address spaces) are not `Copy`. No `&'static` may be manufactured from a raw pointer or a table-owned Box.
7. **Every `unsafe` block states its invariant.** Enable `clippy::undocumented_unsafe_blocks` (deny) and `unsafe_op_in_unsafe_fn`. A SAFETY comment names the invariant and where it is established (file:line). "Caller guarantees" is not an answer inside a safe function.
8. **Per-thread CPU state is enumerated in one place.** `on_switch` owns a documented list of what is switched: GPRs, RFLAGS, FPU, FS_BASE, RSP0, CR3. Adding user-visible CPU state (TLS, debug registers, XSAVE components) means extending that list and adding a ktest that switches between two processes that differ in it.
9. **Test hooks never ship.** Everything a test needs from production code (`catch::intercept`, fault injection, stdout capture, GPT stamping) sits behind `cfg(feature = "kernel_tests")`. CI builds the production kernel with `-D warnings` and fails if any test-only symbol is present (`nm | grep`).
10. **One implementation of each primitive.** Before adding a lock, ring buffer, setjmp, error enum, user-entry path or file stack, grep for the existing one and extend it. Deleting the duplicate is part of the change. This review found two of nearly everything.

### 8.2 Required checks (CI and `make test`)
- **No retries that hide failures.** Delete `retryable_ktest_failure`'s panic and timeout rules and `_retry_hang`. If a flake is truly environmental, it gets a ROADMAP line and an issue in the same commit, per DESIGN §9.8. A retry must never match a kernel panic.
- **`make test` is green on every supported host.** That includes macOS/Apple Silicon: fix the UEFI skip and use pflash for OVMF.
- **e2e asserts userspace results.** It requires `user: tests ok` and fails on `user: tests fail`. It checks QEMU exit codes in panic mode, and matches panic-mode markers only before the banner.
- **Negative-path syscall tests.** A user program in the initrd exercises the error path of every syscall and pointer argument: EFAULT, EBADF, EINVAL, E2BIG, ENOMEM, EAGAIN. It also triggers each user-mode exception, including TF single-step and alignment checks, and requires that only the process dies. The harness asserts its result.
- **The crash-consistency gate can fail.**
  - QEMU runs with `cache=none` or `writethrough` for that test.
  - The kill point is randomized across the whole run.
  - fsck warnings are errors.
  - File contents are checked against a committed prefix.
- **Core crate checks.** `cargo miri test -p vibeos-core` on the allocator, parser and ring modules. `cargo fuzz` targets for `elf`, `fat`, `vibefs`, `part`, `acpi` and `pci` parsing, run in the weekly job.
- **Coverage gaps.** A KVM leg (self-hosted, or nested where available) to exercise TSC-deadline and real interrupt timing. A `CARGO_PROFILE=release` build-and-boot leg.
- **Build hygiene.**
  - Two-pass ksyms: pass 2 must leave `.text` identical (`cmp` the section bytes, or diff `nm`).
  - `check_cells.py` becomes a real check: no `unsafe impl` of `Send`/`Sync` outside `cell.rs`, and bounds required inside it.

### 8.3 Documents to add or fix
- **`docs/INVARIANTS.md`** (the planned DOC2 split). Turn §3.9 of this review into the living list. Each invariant names where it is established, where it is relied on, and the test that enforces it.
- **A privilege-transition section** (DESIGN §5 or a new `docs/ENTRY.md`). It covers:
  - the GS, IF and AC state at every instruction boundary of entry and exit;
  - the IST vectors;
  - the ring-3 vector policy table;
  - the rules above.
- **Stale claims.** Fix the load-bearing false claims listed in the grouped "stale doc claims" finding. For each one, state whether the code or the doc was wrong (DESIGN §1.4).
- **ROADMAP truthfulness.** A checkbox may be ticked only when the commit names the test that proves it. Re-open the Phase 8/9 boxes this review found unbacked.
