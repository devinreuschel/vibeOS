# 7. SMP

Index: [DESIGN.md](DESIGN.md). This file holds DESIGN §7, and its headings keep DESIGN's numbers.

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
| MCFG | `MCFG` | PCIe ECAM base. ECAM reaches extended config space (offsets `0x100` and up); below that offset, `0xCF8`/`0xCFC` reaches every bus ([section 9.2](PITFALLS.md#92-memory)) |

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
([§11.1](PORTABILITY.md#111-the-seam)), which SMP bring-up, the IRQ layer, and the device registry read, and
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
| `0xC000_0081` | `IA32_STAR`. `SYSCALL` loads CS `0x08`; `SYSRET` base `0x10` gives user SS `0x1B` and CS `0x23`. Planned (ROADMAP §10.6): base `0x23`, giving SS `0x2b` and CS `0x33`, as on Linux ([section 5.1](INTERRUPTS.md#51-gdt-and-tss)). |
| `0xC000_0082` | `IA32_LSTAR`. `vibeos_syscall_entry`. |
| `0xC000_0084` | `IA32_FMASK`. `0x47700`: `SYSCALL` clears TF, IF, DF, IOPL, NT, and AC. |
| `0xC000_0100` | `IA32_FS_BASE`. User TLS base, written by `syscall_init::first_return` and `execve`; not switched per thread ([section 7.5](#75-per-cpu-data)). |
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
page is `0x52000`; under OVMF the first usable page is `0x1000` (the edk2 build `make test-e2e-uefi`
boots types `0x0`–`0x1000` bootloader-reclaimable and `0x1000`–`0xA0000` usable; older builds type
`0x0`–`0x87000` usable). Linux likewise reserves its real-mode trampoline from memory below 1 MiB at
boot. `vibeos::pmm::choose_trampoline_page` makes the choice, and `smp_init` prints
`vibeOS: smp: trampoline page <addr>` before the first AP starts.

The trampoline is `src/arch/x86_64/trampoline.S`, assembled with `global_asm!` into `.trampoline` (inside
`__rodata_start..__rodata_end` so the kernel map covers the copy source) and copied to the chosen
page. It goes: real mode, set up a GDT, enable protected mode, load CR3 from the param block, set
`EFER.LME` and `EFER.NXE`, enable paging, long jump to 64-bit, load the stack, call the Rust entry
point. On the way it clears `CR0.CD` and `CR0.NW` (then `wbinvd`), sets `CR4.PAE` and `CR4.PGE`, and
sets `CR0.PG` and `CR0.WP`. The blob runs at whichever page boot chose, from `0x1000` to `0x9F000`:
its real-mode code addresses itself through CS (DS = CS, offsets from the blob's start), and
`smp_init` patches its absolute operands (the protected-mode and long-mode entry addresses, the GDT
base, and the parameter block's address) from the page's base when it copies it. Each is assembled
as if the blob sat at 0, and `.org` pins it at an offset `vibeos::smp::PATCH_SITES` lists;
`vibeos::smp::patch_blob` adds the base to a copy of the blob, and `smp_init` asserts at boot that
the blob's exported patch labels sit at those offsets. The jump out of real mode is the 32-bit
offset form (`66 EA imm32 imm16`), since a 16-bit offset cannot reach a page above 64 KiB, and long
mode loads the stack and entry RIP-relative. INIT leaves SP at 0, so nothing pushes before long
mode.

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

Two correctness requirements:

- Write each field with `write_volatile`, so the compiler neither merges nor drops a write, and start
  the AP only through the IPI send, which orders the writes before the SIPI in either APIC mode
  ([section 7.6](#76-ipis)). A plain `copy_nonoverlapping` followed by a send with no barrier lets the
  compiler move the copy past the write that starts the AP, and the AP then reads uninitialized
  parameters. This one is invisible in debug builds.
- The trampoline page must be identity mapped and executable. The AP starts in real mode and touches
  the page's physical address directly, so this cannot go through the physmap. The BSP writes the
  blob and its parameters only through the physmap. Reserve the frame in the PMM forever, even after
  all APs are up.

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
and `KERNEL_GS_BASE` (`per_cpu_init::install_gs`); write CR0 and CR4 whole (`arch::cpu::init_control_regs`), program the
syscall MSRs and the FPU, and set RSP0 (`syscall_init::init_ap`); load the shared IDT; enable the LAPIC; copy the BSP's timer mode into `PerCpu`; arm the
LAPIC timer with the BSP's calibration (`apic_init::arm_ap`); run the TSC warp test against the
BSP; mark the CPU online and print
`vibeOS: sched: cpu<i> ready`; publish the ready flag; `sti`; enter the idle loop.

The TSC warp test (`smp_init::tsc_warp_source` on the BSP, `tsc_warp_target` on the AP) measures
the AP's TSC against the BSP's over one shared cache line, `trace::WarpLine`. The BSP joins it just
before step 5, with IF=1; the AP joins after arming its timer, with IF=0 before its first `sti`. Each
side waits at a barrier that spins on its own cycle counter for at most the 3 s of step 5 and skips
the test when left alone, then reads its counter for 2 ms, at most 200,000 times: each read is
compared with the largest read either side has published, and one below it is a backward step.
The skew is the largest backward step, in cycles, 0 for none (`time_init::note_tsc_warp`,
`tsc_max_skew`); any backward step makes the TSC unfit to order a trace across CPUs, Linux's
`check_tsc_warp` rule ([DESIGN §6.4](TIME.md#64-timekeeping-api)). Once at least one AP ran
the test, the BSP prints `vibeOS: smp: tsc skew <n> cycles` before `vibeOS: smp: done`, and it
always publishes the calibration, the invariant bit and the warp result into the flight recorder's
header (`trace_init::publish_clock`), where the core tool reads them.

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
brings each AP back through ROADMAP §19.6's online path, which is steps 2 to 5 above on the AP's existing
per-CPU area. Why: a wakeup loses every register the kernel set (QEMU resets the machine on a `q35`
wakeup, and firmware restores only what its own S3 script saved), and with one routine a register
added to bring-up, such as ROADMAP §18.3's mitigation MSRs or §20.1's x2APIC mode, is restored at
resume with no second edit. Rejected: a resume path with its own register list, which drifts from
bring-up.

## 7.5 Per-CPU data

One `PerCpu` struct per CPU. In ring 0, `GS_BASE` holds its address. While the CPU runs ring 3,
`KERNEL_GS_BASE` holds it and `GS_BASE` holds the user GS base (always 0).
`syscall_init::first_return` sets that state for a new thread, each `swapgs` exchanges the two, and `per_cpu_init::init_bsp`,
`per_cpu_init::install_gs`, and `arch::gs::force_kernel` set both MSRs to the `PerCpu` address.

`self_ptr` sits at offset 0 so `gs:[0]` yields the struct address, which is how a `&PerCpu` is obtained
without knowing which CPU you are on. Taking that reference is legal only with IF=0, and the
reference dies with the IF=0 stretch ([§2.9](INVARIANTS.md#29-preemption-and-interrupt-state) rule 5). `current`
is never read through it: `arch::current_tcb()` loads `current` with one `gs`-relative instruction,
and `arch::cpu_id_hint()` loads `cpu_id` the same way for callers that tolerate a stale id (the log
prefix, virtio-blk's queue choice, the panic dump, a new thread's placement). Both live in
`arch::x86_64::percpu`, with the per-CPU-live flag, and return null or 0 before it is set. Debug
builds assert IF=0 in `per_cpu_init::current()` and `try_current()` from `irq: enabled` on, and
`scripts/check_current.py` rejects a read of `PerCpu.current` outside `src/arch/` (ROADMAP §10.3,
F039).

Contents (`crates/core/src/smp/per_cpu.rs`):

- `self_ptr` and logical CPU id
- `current`, `idle`, and `idle_id`
- `runq`, this CPU's ready FIFO (owner only, IRQs off)
- `irq_nest`, `slice_tsc`, `idle_tsc`, and `switch_scratch`, a `CpuContext` that no code reads or writes
- `timer_mode`
- `kernel_rsp0`, which the context switch updates; `tables`, this CPU's `gdt::CpuTables` (opaque in `vibeos-core`), through whose `set_rsp0` it writes TSS.RSP0; and `fallback_rsp0`, the RSP0 it uses for a thread without `Tcb.stack` (below)
- `syscall_scratch`: one word, the user RSP between `syscall` and the entry's stack switch, which
  copies it into the user frame. It is per CPU, not per thread, so it is valid only while IF=0; the
  entry's `sti` follows the copy. No exit writes it: the exit keeps the return value and its
  `iretq` frame in the thread's user frame ([section 5.10](INTERRUPTS.md#510-privilege-transitions)). It sits
  at the end of the owner-only part.
- `remote`, this CPU's `PerCpuRemote` in a separate per-CPU array: `ticks`, `switches`, `runq_len`,
  `ready`, `wake_inbox`, `apic_id`, and `as_cr3`, the root this CPU last loaded: an `AtomicU64` its
  owner stores after each CR3 write and `addr_space_init::teardown` reads. All are atomics; it is the
  only per-CPU state another CPU reads.
  `wake_inbox`, a slot bitmap with a summary word (§7.6): a remote CPU sets a thread's bit and sends
  IPI `0xFD`. `ready` is the flag an AP sets last in bring-up
  ([section 7.4](#74-ap-bring-up-sequence)).

`PerCpu` is `#[repr(C)]`; its size and the offsets the core tool reads are const-asserted in
vibeos-core outside `cfg(loom)`, and [VMCOREINFO.md](VMCOREINFO.md) lists them.

`per_cpu_init::init_bsp` allocates one `PerCpu` per MADT CPU in a heap array, not a static array
sized by a `MAX_CPUS` guess, and installs the BSP at slot 0; each AP installs its own slot with
`per_cpu_init::install_gs`. `current` and `idle` are `*mut Tcb`. The other per-CPU tables are static
and cap the CPU count at 64: the MADT `apic_ids` array (`acpi::MAX_CPUS`), `hardirq::IN_ISR`,
`ipi_init::SHOOT`, `per_cpu_init::WITH_BUSY`, `sync_init::HELD`, `log_init::EMITTING` and `log_init::STAGE`, and the `u64` online mask. The thread table limits it further: boot takes 2N+3 of
its 1024 slots (`limits::MAX_THREADS`) at `-smp N`. `smp_init::alloc_ap_resources` creates an AP's idle
thread and its per-CPU workers (the `wq` worker, pinned and parked by
`work_init::spawn_cpu_workers`) before it starts the AP, and makes the workers runnable only after the
AP reports `ready`. When one of them finds no slot or no memory it releases what it took, the AP
stays offline with `vibeOS: smp: apic N alloc failed` and a warning naming the `SpawnError`, and a
bring-up that times out retires the parked workers the same way; `thread_init::adopt_ap_idle` drops
an unplaced `Tcb` box only after `SCHED` is released (ROADMAP §10.4, F037).

`per_cpu_init::current()` is valid only after the entry path has put the kernel base in `GS_BASE`. The
`swapgs` instructions are the three in `vibeos_syscall_entry` (entry, `sysretq` exit, `iretq` exit)
and the entry and exit ones in the `arch/x86_64/idt.rs` entry paths that every generated stub jumps to
([section 5.10](INTERRUPTS.md#510-privilege-transitions) rule 1).

The CS.RPL rule is also wrong wherever CS is the kernel's while `GS_BASE` holds the user base: NMI,
`#MC`, and `#DB` in the one-instruction windows between `syscall` and the entry `swapgs` or between
the exit `swapgs` and `sysretq`/`iretq`; a `#GP` raised by the user-return `iretq` (F007); and an
interrupt taken inside a new thread's first return (`syscall_init::first_return`) while IF=1
between its `mov gs` and its `iretq` (F006). ROADMAP §10.6 closes these for the current kernel (an IST vector taken at CPL 0, and `#DF`,
decides from the sign of `GS_BASE`, one taken at CPL 3 swaps by CS.RPL and moves to the thread's
kernel stack, a `#GP`, `#NP`, or `#SS` on a labeled user-return `iretq` becomes `SIGSEGV`, and
`syscall_init::first_return` runs `cli` before its `mov gs`) and §18.3 for FSGSBASE, where a user can load a
kernel-half base. See [section 9.3](PITFALLS.md#93-interrupts).

Which accessor may alias which. `cpu(id)` returns `&'static PerCpuRemote`, which is never taken
`&mut`, so it may alias anything; the owner reaches its own view through `PerCpu.remote`.
`with_current` and `with_current_switch` give this CPU's `&mut PerCpu` with IF=0 and the busy flag
(`WITH_BUSY`), so neither nests in the other or in itself; `with_current_switch` takes no
`InterruptGuard` of its own, needs the caller's, and returns before `switch_context`, so the
incoming thread can take IRQs and `with_current`. `with_cpu` is an `unsafe fn` for a CPU that is not
running: `smp_init` uses it before an AP's SIPI, and after a SIPI it clears only the view.
`ap_entry` holds its slot's `&mut` from `STARTING` until it publishes `ready`. `current()` and
`try_current()` return `&'static PerCpu`, which must not be live across a `with_current*` scope or a
preemption point; both assert IF=0 in debug builds from `irq: enabled` on (ROADMAP §10.3, F039).

### Per-thread CPU state

`thread_init::switch_now` swaps `irq_nest` between the TCB and `PerCpu` and calls
`syscall_init::on_switch` (FPU, RSP0, CR3) inside `with_current_switch`, then calls
`arch::x86_64::switch::switch_context` (callee-saved GPRs, RSP, RIP, RFLAGS) after that `&mut` has ended. AGENTS.md rule 8 governs adding
user-visible CPU state; the commit that adds it also adds its row here. The Arch column names the
port a row belongs to; the aarch64 rows are planned (ROADMAP Phase 11), and there `switch_now` calls
that port's `on_switch` and `switch_context`. A control that holds one value for every thread, such
as `CR4.TSD` or `SCTLR_EL1.UCT`, is a row of §11.4's table instead.

| Arch | State | Saved in | Switched by | Status |
|---|---|---|---|---|
| x86_64 | `rbx`, `rbp`, `r12`–`r15`, RSP, RIP | `Tcb.context` (`CpuContext`) | `switch_context` | switched |
| x86_64 | RFLAGS | `CpuContext.rflags`; IF comes from `irq_nest` (`apply_if_on_resume`) | `switch_context` | switched |
| both | `irq_nest` | `Tcb.irq_nest`, swapped with `PerCpu.irq_nest` | `switch_now` | switched |
| x86_64 | user GPRs, RIP, RSP, RFLAGS, CS, SS, and the original syscall number | the thread's user frame at the top of `Tcb.stack` ([section 5.10](INTERRUPTS.md#510-privilege-transitions)), saved by every entry from ring 3 | the RSP0 switch, which gives each thread its own entry stack | switched: the syscall entry and every generated stub for a CS.RPL 3 frame save all 21 words of `UserFrame` at the top of the thread's kernel stack, and every return to ring 3 restores from it; `thread_init::spawn_user` writes a new thread's |
| x86_64 | x87, SSE, MXCSR | `Tcb.fpu`, a 512-byte FXSAVE image | the FP binding below: `fxsave64` at the switch away from a thread whose state is live, `fxrstor64` in the return to ring 3 when the registers hold another thread's state | switched, by the binding as built: `PerCpu.fp_owner` and `Tcb.fp_cpu`, whose transitions are `vibeos::fpu`. `syscall_init::switch_fpu` in `on_switch` saves a live state with `fp_save` and loads nothing; `vibeos_fp_user_return` runs with IF=0 after the syscall exit's `cli`, in `idt::exit_to_user` after a `cli`, and in a new thread's first return, which enters the syscall exit after its `cli`, and loads with `fp_load` when the registers hold another thread's state. The syscall entry and exit neither save nor restore it. A new TCB, and one `fill_tcb` reuses, starts with `fp_cpu` empty, and `thread_init::fp_invalidate` empties it for a write to `Tcb.fpu`. `fork` gives the child `fpu_template()`, not the parent's image, and `execve` keeps the old image's registers (ROADMAP §10.6, F069). The template is captured after `fninit`, which resets only the x87 control, status, and tag words, so MXCSR and the XMM and ST registers hold whatever the loader left (ROADMAP §10.6, F129). FXSAVE covers no XSAVE state; `CR4.OSXSAVE`, `CR4.PKE`, and `EFER.FFXSR` are assumed clear and never asserted (ROADMAP §11.1, F130). |
| x86_64 | RSP0 | TSS.RSP0 and `PerCpu.kernel_rsp0`: the top of `Tcb.stack`, which every thread has, the bootstrap thread included (`thread_init::init_bootstrap`); `fallback_rsp0` only for a thread without one | `set_rsp0_for` in `on_switch`, through `CpuTables::set_rsp0` on `PerCpu.tables` | switched |
| x86_64 | CR3 | `Tcb.as_cr3` (0 means the kernel PML4) | `switch_cr3_for` in `on_switch`, skipped when unchanged | switched; no PCID (ROADMAP §18.3 adds it with §7.9's flush generation) |
| x86_64 | FS_BASE (user TLS) | not saved | nothing | not switched. `syscall_init::first_return` (from `Proc.fs_base`) and `execve` write it; `force_kernel`'s `mov fs` zeroes it on every exit or kill; `fork` copies the live MSR, so a child can inherit another process's base (ROADMAP §11.6, F022). |
| x86_64 | user GS base | not saved; always 0 | nothing | holds while no `ARCH_SET_GS` or FSGSBASE exists (ROADMAP §18.3); from then on it is per thread, and while the thread is in the kernel it is in `KERNEL_GS_BASE` whichever vector it entered by, IST vectors included ([section 5.10](INTERRUPTS.md#510-privilege-transitions) rule 3), where the switch away reads it |
| x86_64 | DR0-DR3, DR7 | the thread's decoded debug slots, and the tracer's masked DR7 for `PEEKUSER` | the switch, by the Debug state paragraph below | not built: nothing arms them before ROADMAP §17.4 |
| x86_64 | DR6 | the thread's virtual DR6 | not switched: the `#DB` body writes the thread's copy from the DR6 its entry saved (§5.10) | not built: ROADMAP §17.4 |
| x86_64 | DS, ES, FS, and GS selectors | the thread's own four, saved at the switch away | `on_switch`, which loads the incoming thread's four before it writes `FS_BASE` and `GS_BASE`, since a selector load can clear the matching base | Rule; not yet enforced: ROADMAP §10.6 ([section 5.1](INTERRUPTS.md#51-gdt-and-tss)). `syscall_init::first_return` loads `0x1B` into all four and nothing saves them, so a selector ring 3 loads with `mov` is lost at the next switch |
| x86_64 | `PerCpu.syscall_scratch` | per CPU | not switched | valid only while IF=0 (above); one word, the user RSP from `syscall` to the entry's stack switch; the exit keeps its state in the user frame |
| aarch64 | `x19`-`x29`, SP, LR | `Tcb.context` | `switch_context` (ROADMAP §11.4) | not built |
| aarch64 | DAIF.I and F | come from `irq_nest`, as on x86_64 | `switch_context` | not built |
| aarch64 | user `x0`-`x30`, SP, PC, PSTATE, `orig_x0`, and the syscall number | the thread's user frame at the top of `Tcb.stack` ([section 5.10](INTERRUPTS.md#510-privilege-transitions)) | every entry from EL0 saves it, and each return to EL0 leaves `SP_ELx` at the top of the thread's stack for the next entry | not built: ROADMAP §11.6 |
| aarch64 | `SP_EL0` | at EL0 the user stack pointer, saved in the user frame by every EL0 entry; at EL1 or EL2 the running thread's TCB pointer ([§2.9](INVARIANTS.md#29-preemption-and-interrupt-state) rule 5) | the EL0 entry stub, the return to EL0, and the switch (ROADMAP §11.6) | not built |
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
mode checks the binding with IF=0 ([§5.10](INTERRUPTS.md#510-privilege-transitions) rule 4) and, when the
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
enable bits are per-thread rows (ROADMAP §23.1). Built on x86_64 (the row above). Rule; not yet
enforced on aarch64: ROADMAP §11.6.

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
frame of [section 5.10](INTERRUPTS.md#510-privilege-transitions) included) only while that thread is held
stopped, in a ptrace stop (ROADMAP §17.4) or parked for a core dump (ROADMAP §13.8), and only after
an Acquire load has seen the thread's `on_cpu` flag clear. Stopping is not enough:
a stopped thread wakes its tracer before its CPU has switched away from it, and until then some rows
exist only in that CPU's registers (the FP state under the binding above, and the user FS and GS
bases under FSGSBASE, ROADMAP §18.3). The switch away finishes every save in this table before
`thread_init::finish_switch`, on that CPU once `switch_context` has returned, clears `on_cpu` with
a Release store, its last access to the outgoing thread ([§2.8](INVARIANTS.md#28-publish-last)). The reader holds the stop for the whole access, as Linux's ptrace does:
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
| `0xFE` | Panic stop, Fixed delivery: the IPI of [§2.5](INVARIANTS.md#25-panic-policy) step 1 (ROADMAP §10.7, F135). The dump owner (`ipi_init::stop_others`) sets each other online CPU's stop request word, then sends it this IPI to its APIC id, through `apic_init::send_ipi`, which records no trace event; the body (`ipi_init::on_stop_ipi`) saves the interrupted registers and stops (`stopped (ipi)`). A CPU spinning with IF=0 (in `SpinMutex::lock` or `wait_acks`) stops at its next `service_incoming` poll instead, and one that neither polls nor writes gets NMI after 100 ms. |

The reschedule IPI is what makes cross-CPU wakeups work without ever locking a remote run queue: push
onto the target's inbox, send `0xFD`, done. `0xFD` then takes SCHED IRQ-off via `schedule_preempt`.
A wake's push goes out after its waker has dropped SCHED (`thread_init::with_sched` delivers the
places a closure made once the lock is released), so it can reach the CPU after the thread has
already resumed there through its own `schedule`, which found it `Ready`, and has since blocked
again or exited. A run-queue entry is therefore only a hint: `thread_init::schedule_inner` runs a
dequeued thread only while SCHED shows it `Ready` and placed on that CPU, and drops any other entry.
Shootdown and call-function work take no lock at all, since a CPU in a serviced spin runs them inside
whatever it holds ([§2.2](INVARIANTS.md#22-interrupt-handler-rules)). Call-function uses one global slot; the
initiator holds IF off from publish through reclaim, polling inbound work while it waits.

The IPI send is the publication point. `send_ipi`, the seam's IPI send ([§11.1](PORTABILITY.md#111-the-seam)),
orders every store its CPU made before the call ahead of the interrupt's arrival, so a handler that
takes the IPI and then reads a slot, an inbox, or a parameter block sees what the sender wrote. With
xAPIC the ICR write is an uncached store, which x86 does not reorder with earlier stores, so the
send needs only a compiler barrier before it. With x2APIC the ICR is MSR `0x830`, and a `WRMSR` to
an x2APIC register is not serializing (Intel SDM Vol. 3A, MSR access in x2APIC mode), so the send
runs `mfence` then `lfence` before it, as Linux's `weak_wrmsr_fence` does. It does so on every
vendor, though Linux skips it on AMD. With GICv3 the send runs `dsb ishst` before the
`ICC_SGI1R_EL1` write and `isb` after it (ROADMAP §11.7 cites the Arm ARM rule). With GICv2 the SGI
is an MMIO store to `GICD_SGIR`, which goes through the ordered `mmio_write` ([§4.7](MEMORY.md#47-dma)),
whose `dmb oshst` puts the CPU's earlier stores ahead of it, as the `dmb ishst` before Linux's GICv2
SGI write does. INIT and SIPI go through the same send. Callers publish with a Release store or a
locked read-modify-write and add no fence of their own. Rule; not yet enforced:
`apic_init::send_ipi` has no barrier of its own, and the callers that fence (`smp_init::start_one`,
`ipi_init::shootdown_ranges`) do so before their publishing store, which is not enough under x2APIC
(ROADMAP §20.1).

The wake inbox (§7.5) is `vibeos::irq::ipi::WakeInbox`, a per-CPU bitmap of `AtomicU64` words
with one bit per thread-table slot, sized from `limits::MAX_THREADS`, and a summary word with one
bit per word. A push is a Release `fetch_or` of the slot's bit and then of its word's summary bit,
and sends `0xFD`; a drain swaps the summary to zero with Acquire, then swaps each flagged word to
zero with Acquire and takes its set bits in ascending order. A push allocates nothing and is
idempotent, so a thread woken from two CPUs at once is queued once. A bit names a slot, not a tid:
the `0xFD` handler's drain maps each slot to its tid through `thread_init`'s slot table, which spawn
publishes with Release under `SCHED` whenever a slot takes a TCB. A bit is set only for a Ready
thread, which cannot die before it runs, so a slot is not reused while its bit is set. Rejected: an
intrusive MPSC list, which needs a queued flag in each TCB against double insertion and a larger
model. Planned: ROADMAP §10.8's loom model of push and drain.

## 7.7 Locking with more than one CPU

The global lock order is in [section 2.1](INVARIANTS.md#21-lock-order) and the one-spinlock rule in
[section 2.3](INVARIANTS.md#23-locking-with-interrupts). Additions specific to SMP:

- Never lock a remote CPU's per-CPU state. Per-CPU locks are taken only by the owning CPU, with
  interrupts off. `per_cpu_init::with_cpu`, an `unsafe fn`, reaches another CPU's slot only while that CPU
  is not running ([section 7.5](#75-per-cpu-data)). Planned (ROADMAP §19.4): the one exception is a CPU's timer base
  ([§6.5](TIME.md#65-timers-and-timeouts)), whose lock any CPU takes to arm, re-arm, or cancel a timer on
  it.
- A thread moves between CPUs only through the target's inbox, pushed by the CPU that owns the
  thread. Load balancing, a `sched_setaffinity` whose new mask excludes the CPU a queued thread waits
  on (ROADMAP §13.10; that CPU moves it on a reschedule IPI), and CPU offlining (ROADMAP §19.6) all
  move threads this way. Work stealing, if ROADMAP §19.4's numbers keep it, takes threads from a
  lock-free deque with a loom model, never from a locked remote run queue. No code locks two CPUs'
  run queues.
- A lock taken from an ISR is taken with interrupts disabled in every other context too. The scheduler
  lock is the canonical case: the timer ISR calls into the scheduler, so any holder with interrupts
  enabled deadlocks the moment its own timer fires.
- Serial TX takes a lock so bytes from different CPUs do not interleave. Each kernel line is
  formatted, newline included, into one stack buffer and written under one TX hold, so another CPU
  cannot split it (ROADMAP §10.2, F138).
- klog records go to one global IRQ-safe log ring and to a serial sink that only try-locks TX.
  Per-CPU serial capture assembles serial output into lines for the ring. Planned (ROADMAP §19.5):
  one lockless ring any context may append to, and a printer thread per console (§2.5).
- The global SCHED lock (ROADMAP §19.4 splits it), one `SpinMutex` on the block cache, one VFS
  lock over lookups and namespace changes (§2.1), the log-ring TAS, and virtio-blk bounce copies are
  known scale limits; see ROADMAP §19.4, §19.5, and §19.8. So is the one bottom-half thread for every threaded vector,
  until ROADMAP §12.5 gives each vector its own ([§5.4](INTERRUPTS.md#54-irq-registration)).

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
| Threaded interrupt bottom halves ([§5.4](INTERRUPTS.md#54-irq-registration)), network receive included, and block error handlers ([§10.3](BLOCK.md#103-failure)) | `SCHED_FIFO` 50 | threaded interrupt handlers |
| RCU's grace-period and boost thread ([§2.12](INVARIANTS.md#212-rcu)) | `SCHED_FIFO` 1 | RCU's kthreads with boosting on (`rcutree.kthread_prio` 1) |
| The softirq-equivalent workers, one per CPU, which also run timeout-wheel callbacks ([§6.5](TIME.md#65-timers-and-timeouts)) | Fair, nice -20 | `WQ_HIGHPRI` workqueue workers |
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
count, a second "not now" beside IF ([§2.9](INVARIANTS.md#29-preemption-and-interrupt-state)); a separate per-CPU
receive worker for work past the budget, which adds a hand-off and a context for nothing the
demotion does not do; and leaving each subsystem to choose. If ROADMAP §19.4's measurement shows
timeout-wheel callbacks starting late under load, they move to `SCHED_FIFO` 1, as `PREEMPT_RT`'s
`ktimers` threads run.

The timeout queue starts global with one lock. ROADMAP §19.4 makes it per CPU, as
[§6.5](TIME.md#65-timers-and-timeouts)'s two structures. A timer stays on the base it was armed on when its
thread migrates: its expiry only wakes the thread, and the wake goes through the inbox like any
other. A CPU going offline hands its timers to an active CPU ([§7.11](#711-cpu-offline-and-online)).

## 7.9 TLB shootdown

Kernel mappings are `GLOBAL` and therefore live in every CPU's TLB. Unmapping one requires every CPU
to invalidate before the virtual address or the frame behind it is reused
([§2.4](INVARIANTS.md#24-memory-invariants)).

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
holds a core reference to the address space ([§2.11](INVARIANTS.md#211-object-lifetimes)) until it loads another
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
[§2.9](INVARIANTS.md#29-preemption-and-interrupt-state) rule 2 allows a cross-CPU wait: never in NMI, `#MC`, or
`#DB` context, never from a serviced shootdown or call-function handler, and never under the log-ring
TAS, whose waiters service no IPIs. The aarch64 implementation asserts the same contexts in debug
builds, although its broadcast waits for no peer, so work run first on aarch64 cannot add a deadlock
that only x86_64 hits.

The initiator waits with interrupts disabled (`InterruptGuard` around publish → IPI → ack → clear
waiters). A waiter with IF off cannot take an incoming shootdown as an IRQ, which would deadlock two
CPUs shooting down at once. The wait loop therefore calls `service_incoming` and processes pending
slots so a spinning initiator still helps its peers. This is not optional; it is the difference
between working and a hang that only appears under load. `SpinMutex::lock`'s spin polls it too: it
reaches `service_incoming` through `sync_init::set_spin_poll`, which `ipi_init::init` sets before the
first AP starts.

The wait never panics, because `shootdown_ranges` and `call_mask` free frames and reuse their slot as
soon as `ipi_init::wait_acks` returns. After each second (1000 × `tsc_per_ms` cycles) without every
acknowledgement, it logs `vibeOS: ipi: wait_acks late <n> s: cpu<i> …` and counts it in
`ipi_init::ack_late_count`. Without a TSC it logs every 50,000,000 polls. A CPU at IF=0 that does not
poll `service_incoming` delays every shootdown until it does. One that never does leaves a hang for
ROADMAP §10.7's forensics to report. `lifetime_shootdown_ack_late` holds IF off for 3 s on one CPU
while another unmaps. As built, a round is kernel-only and goes to every online CPU: it carries up
to 16 ranges (`vibeos::ipi::SHOOT_RANGES`) of 1 to 32 pages each (`ShootRange`, a page-aligned start
with the page count in its low 12 bits), and each receiver runs `invlpg` on every page of them
(`ipi_init::shootdown_ranges`, which sends one round per 16 ranges; `paging::tlb_shootdown_others(va)`
is the one-page case). A KVA unmap of up to 32 pages sends one round, and a worker frees its CPU's
dead stacks 16 to a round ([§4.5](MEMORY.md#45-kernel-virtual-address-allocator)); ROADMAP §12.3
adds the target, the "all" count, and the freed-tables flag of the rounds above. `kva_init::unmap_shootdown` unmaps at most 32 pages (`MAX_UNMAP`) and leaves the
rest mapped with no error.

The shootdown handler allocates nothing and takes no lock ([§2.2](INVARIANTS.md#22-interrupt-handler-rules)). It
reads a request slot and executes `invlpg` on each page it names, at most 512.

## 7.10 Verification and later work

SMP bugs are timing dependent, so the tests matter more than usual:

- `-smp 2` is the default for every QEMU invocation including the normal boot test. One AP catches most
  bring-up bugs.
- `-smp 4` as a separate target to shake out sequencing assumptions that hold for exactly one AP.
- `-cpu qemu64,-tsc-deadline` to force the LAPIC periodic path under KVM. Under TCG every tier but `make test-e2e-pit` (PIT) takes
  the periodic path already, and no CI tier runs the TSC-deadline path ([section 6.3](TIME.md#63-the-tick),
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
   ([§5.4](INTERRUPTS.md#54-irq-registration)), and each bottom-half thread moves with its vector. The steps that
   run on the dying CPU run now: for example, it leaves VMX or SVM operation (ROADMAP §21.1) and
   disarms its local timer.
4. A stop-machine rendezvous. Every online CPU's stopper reports in and waits with IF=1 until all
   have. Then each turns IF off and waits, servicing incoming IPIs ([§7.9](#79-tlb-shootdown)),
   while the dying CPU clears its online bit, pushes any thread its wake inbox holds to an active
   CPU, and raises again on its new CPU each moved vector still pending for the dying CPU (latched
   in its IRR on x86_64, pending for it in the GIC on aarch64), as Linux does when a CPU goes
   offline. Then every stopper turns IF back on and returns.
5. The dying CPU reports that it has parked, as its last store to shared state
   ([§2.8](INVARIANTS.md#28-publish-last)), and parks.
6. The control CPU runs the steps that follow the park. They move the CPU's unpinned timers of both
   [§6.5](TIME.md#65-timers-and-timeouts) structures to an active CPU with their deadlines kept, and cancel
   its pinned ones; requeue its queued softirq-equivalent and work items; drain its per-CPU log
   buffer; return its slab magazines, the kernel stacks it holds for reuse (ROADMAP §10.10), and its
   per-CPU free-frame lists; fold its per-CPU counters into a global offset; move its RCU callbacks
   ([§2.12](INVARIANTS.md#212-rcu)); splice its per-CPU accept queues and receive-steering backlogs onto an
   active CPU's (ROADMAP §28.1, §28.5); and free what [§2.8](INVARIANTS.md#28-publish-last) deferred until this
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
