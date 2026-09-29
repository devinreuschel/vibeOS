# 11. Portability

Index: [DESIGN.md](DESIGN.md). This file holds DESIGN §11, and its headings keep DESIGN's numbers.

x86_64 and aarch64 are peers from ROADMAP Phase 11; x86_64 came first and is the reference where the two
ports disagree about the kernel's own behaviour; for anything a user program can observe, each
architecture's reference is Linux on that architecture (ROADMAP, How to read this). This section is the
contract for the seam between them. `docs/ARCH.md` (ROADMAP §10.3) maps each row of the §11.1 table to
the modules that implement it in each port. Planned: ROADMAP §10.3 builds the seam and Phase 11 the
aarch64 port. Built so far: the seam traits and `Port` in `vibeos-core`'s `arch/mod.rs`, the stub port
in `arch/stub.rs`, and the x86_64 port's zero-sized type with its `CycleCounter`, `InterruptMask`,
`PerCpuBase`, and `SyscallAbi` impls, which kernel code names as `arch::current::Arch`; the other impls,
`impl Port` for it, and `docs/ARCH.md` are planned in ROADMAP §10.3. `thread.rs` and `dma.rs` in
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
| Boot handover: the machine state the boot handshake hands over, normalized into `BootInfo` | trait (`BootHandover`) | Limine base revision 3, until ROADMAP §11.1's bump moves it to aarch64's; long mode; without Limine, the direct entry (§4.1): a PVH door and a 64-bit door into one body | Limine base revision 6, EL1, or EL2 with VHE; without Limine, the direct entry (§4.1) behind an arm64 `Image` header, entered with the MMU off | §10.3, §11.1, §25.4, §26.4 |
| Early console | port module | 16550 on COM1 | PL011 | §11.1 |
| Exception entry and exit | port module: generated entry code | one stub per IDT vector ([§5.10](INTERRUPTS.md#510-privilege-transitions) rule 1) | one 16-entry vector table ([§11.5](#115-aarch64-exceptions-and-privilege-transitions)) | §10.6, §11.3 |
| Trap decode | pure half: a trap to a `TrapKind` (§5.2) | vector and error code | vector slot and `ESR_EL1` (§11.5) | §10.6, §11.3 |
| Kernel stack-overflow report | port module | `#DF` on IST 1 (§5.1) | a stack test at every vector entry and a per-CPU overflow stack (§4.5, §11.5) | §11.3 |
| Interrupt mask | trait (`InterruptMask`) | RFLAGS.IF (`cli`, `sti`) | PSTATE.I and F (`msr daifset`, `msr daifclr`); priority masking from ROADMAP §25.5 | §10.3 |
| Interrupt controller and IRQ identity | port module: finding the root controller, with the vector entry and the IPI send in their own rows; each controller is an `IrqChip` object (§5.4), not a seam trait | 8259, I/O APIC, and LAPIC MSI chips; a hwirq is an IDT vector (§5.3) | GICv2 or GICv3 distributor and redistributor chips, ITS or GICv2m; a hwirq is an INTID | §11.3 |
| IPI send and its ordering | trait (`IpiSend`) | LAPIC ICR write (§7.6) | SGI register write | §10.3, §11.3 |
| Timer and cycle counter | trait (`CycleCounter`) | TSC, or the HPET or ACPI PM timer as the clocksource (§6.4); LAPIC timer | `CNTVCT_EL0`; generic timer | §10.3, §11.3 |
| Page-table format and attributes | trait (`PageTable`); encodings in the pure half | 4-level tables, PAT bits | 4 KiB granule, 48-bit VA, MAIR, break-before-make | §10.3, §11.2 |
| TLB maintenance and address-space ids | trait (`PageTable`) | `invlpg` and the shootdown IPI (§7.9); no PCID | broadcast `tlbi ...is`; ASIDs from §11.2's generation allocator | §10.3, §11.2 |
| Cache maintenance and DMA coherence | trait (`Barriers`) | none: coherent | per-device coherence from `dma-coherent` or `_CCA`; `dc cvac` and `dc ivac` to the Point of Coherency for non-coherent devices (§4.7); `dc` and `ic` for code | §10.3, §11.2 |
| Barriers (`dma_wmb`, `dma_rmb`, `dma_mb`) and MMIO accessors | trait (`Barriers`) | `mfence`, `sfence`, `lfence`; plain loads and stores; accessors carry a compiler barrier (§4.7) | `dmb oshst`, `dmb oshld`, `dmb osh`; `dmb oshst` before an `mmio_write` and `dmb oshld` after an `mmio_read` (§4.7) | §10.3, §11.2 |
| Atomics | module selected by `cfg(loom)` (below) | `core::sync::atomic` | `core::sync::atomic`, with LSE instructions (`+lse`, §3.1's floor) | §10.8 |
| Per-CPU base and current-thread registers | trait (`PerCpuBase`) | `GS_BASE` and `swapgs`; `current` by one `gs`-relative load (§2.9 rule 5) | `TPIDR_EL1`, or `TPIDR_EL2` at EL2; `current` in `SP_EL0` (§2.9 rule 5) | §10.3, §11.4, §11.6 |
| Syscall instruction, user frame's layout ([§5.10](INTERRUPTS.md#510-privilege-transitions)), numbers and argument order | trait (`SyscallAbi`) | `syscall` and `sysretq`; the x86_64 table | `svc #0`; the asm-generic table | §10.3, §10.5, §10.6, §11.6 |
| User-memory accessors | trait (`UserAccess`) | `stac` and `clac` (SMAP) | PAN | §10.3, §10.6, §11.6 |
| FP and SIMD state | port module, under §7.5's per-thread rules | FXSAVE image | V0-V31, FPCR, FPSR | §10.6, §11.6 |
| User TLS register | port module | `FS_BASE` | `TPIDR_EL0` | §11.6 |
| Context switch | trait (`ContextSwitch`) | `switch_context`: callee-saved registers, RSP, RIP | `switch_context`: x19-x29, SP, LR | §10.3, §11.4 |
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
| Signal frame, sigreturn trampoline, vDSO counter read, ELF machine, TLS variant, and HWCAP | pure half: layouts | `rt_sigframe`, `EM_X86_64`, TLS variant II | `rt_sigframe` and the vDSO's `__kernel_rt_sigreturn`, `EM_AARCH64`, TLS variant I, `AT_HWCAP` | §11.6, §13.8, §13.10 |
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
`/reserved-memory` and its header's reservation block). The ACPI tables ([§7.1](SMP.md#71-acpi), and on
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
  `arch::stub::Arch` in `arch/stub.rs`, which the host tests use. The stub is compiled in host builds
  only (`cfg(any(test, feature = "std"))`); its state is per host thread, so parallel tests never
  share it, and settable (the counter and its step, the frequency, the CPU id, IPI refusal, a user-copy
  fault address); and it records each seam call in a bounded event log, the first 256 events and a
  count of the rest, which a test reads back.
- Portable code that needs the seam takes the port as one type parameter of the type that uses it,
  never as `dyn`, so every seam call is resolved at compile time and inlines. A leaf type bounds the
  parameter by the traits it calls (`Mapper<A: PageTable>`, `SplitQueue<A: Barriers>`), so its
  bounds say what it depends on and a host test can hand it a fake for that one concern. A type that
  holds several seam users still takes one parameter and bounds it by the umbrella trait `Port`,
  whose supertraits are the table's traits, so a second port parameter never spreads into the types
  that hold it. The kernel crate names the concrete types once, in `arch::current` (for example
  `type AddressSpace = vibeos::AddressSpace<Arch>`), so kernel code never spells the parameter.
- The kernel binary names its port once, as `arch::current::Arch` in `src/arch/current.rs`, a type
  alias chosen by `cfg(target_arch)` in the kernel crate, where a compile-time item checks that the
  port implements the seam traits built so far. `vibeos-core` contains no `cfg(target_arch)` and no
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
enforced: ROADMAP §11.6. On x86_64 `arch::cpu::init_control_regs` writes CR0 and CR4 whole on every
CPU, and the aarch64 port does not exist.

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
| x86_64 | `CR0.AM` | 1 | a misaligned access while ring 3 has set `RFLAGS.AC` raises `#AC` and gets `SIGBUS` (§5.2), as on Linux |
| x86_64 | `CR0.EM`, `CR0.TS` | 0 | x87, MMX and SSE instructions run at CPL 3 with no `#NM` |
| x86_64 | `CR0.MP` | 1 | with `TS` clear, no visible effect; set as Linux sets it |
| x86_64 | `CR0.NE` | 1 | an unmasked x87 exception raises `#MF` at the next waiting x87 instruction and gets `SIGFPE` (§5.2), as on Linux |
| x86_64 | `CR4.OSFXSR` | 1 | SSE runs at CPL 3, and `fxsave`/`fxrstor` include the XMM registers |
| x86_64 | `CR4.OSXMMEXCPT` | 1 | an unmasked SIMD floating-point exception raises `#XM` and gets `SIGFPE` rather than `#UD` and `SIGILL` |

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
   the running thread's TCB pointer ([§2.9](INVARIANTS.md#29-preemption-and-interrupt-state) rule 5): every entry
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

A ROADMAP line that maps device memory into EL0 or into a guest, such as ROADMAP §28.4's VFIO, states how an
SError that such an access raises is charged, since the SError row would otherwise let that access
halt the kernel.

Why: these are the rules §5.10's x86_64 rows exist for, restated for a machine with no IST, no GS
swap, and four interrupt masks instead of one. Linux arm64 enters and leaves EL0 the same way: all of
DAIF set on the way out, D and A cleared once the frame is saved, and `current` in `SP_EL0`.
Rejected: interleaving aarch64 paragraphs through §5.1 to §5.7, which would double each section and
mix two machines' registers; a separate `docs/ARCH_AARCH64.md`, which would split the invariant
tables that AGENTS.md rules 1, 2, and 8 point at; and masking only I on the way out (rule 3).
