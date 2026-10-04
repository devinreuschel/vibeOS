# Architecture seam map and x86 audit

Index: [DESIGN.md](DESIGN.md). This file holds the audit ROADMAP §10.3 asks for: each row of
[PORTABILITY.md §11.1](PORTABILITY.md#111-the-seam)'s seam table, in order, with the modules that
implement it, and every place outside `src/arch/` and `crates/core/src/arch/` that keeps x86_64 code behind
`#[cfg(target_arch = "x86_64")]`. `scripts/check_arch.py` checks both against the tree, so a slice
that moves a seam row's code or adds a file under `src/arch/x86_64/` or
`crates/core/src/arch/x86_64/` edits its row here in the
same commit.

## Seam rows

Portable side: the port-independent code that reaches the concern (in `vibeos-core`, or the kernel
crate's shared modules). x86_64 hardware half: the kernel crate's `src/arch/x86_64/` modules, and
fenced sites in shared modules (below) that Phase 11 splits. x86_64 pure half: `vibeos-core`'s
`crates/core/src/arch/x86_64/`, compiled and host-tested on every host. Stub: the stub port that
host tests run against. A trait row's trait is declared in `crates/core/src/arch/mod.rs` (the
`PageTable` and `SyscallAbi` rows' in `crates/core/src/mm/paging.rs` and `crates/core/src/trap.rs`,
re-exported there), and the kernel names its port as `arch::current::Arch` in
`src/arch/current.rs`, which also re-exports the port-neutral names shared kernel code calls.

`PageFlags` keeps x86_64's bit values, so that port's `PageTable::make_entry` and `entry_flags` are
the identity on them; a second port maps them to its own encoding (ROADMAP §11.2).

| Concern | Seam | Portable side | x86_64 hardware half | x86_64 pure half | Stub |
|---|---|---|---|---|---|
| Boot handover | trait (`BootHandover`) | `src/boot/mod.rs`, `src/main.rs` | `src/arch/x86_64/boot.rs` | none yet (ROADMAP §11.1) | `crates/core/src/arch/stub.rs` |
| Early console | port module | `src/log/serial/mod.rs` | `src/log/serial/raw.rs` | `crates/core/src/arch/x86_64/uart.rs` | none yet (ROADMAP §11.1) |
| Exception entry and exit | port module | `src/irq/irq_init.rs` | `src/arch/x86_64/idt.rs`, `src/arch/x86_64/gdt.rs`, `src/arch/x86_64/gs.rs`, `src/proc/syscall_init.rs` | `crates/core/src/arch/x86_64/desc.rs`, `crates/core/src/arch/x86_64/vectors.rs` | none yet (ROADMAP §11.3) |
| Trap decode | pure half | `crates/core/src/trap.rs`, `src/proc/proc_init/mod.rs` | `src/arch/x86_64/idt.rs` | `crates/core/src/arch/x86_64/trap.rs` | none yet (ROADMAP §11.3) |
| Kernel stack-overflow report | port module | none yet (ROADMAP §11.3) | `src/arch/x86_64/gdt.rs`, `src/arch/x86_64/idt.rs` | `crates/core/src/arch/x86_64/desc.rs` | none yet (ROADMAP §11.3) |
| Interrupt mask | trait (`InterruptMask`) | `crates/core/src/cell.rs`, `src/cell.rs` | `src/arch/x86_64/mod.rs`, `src/arch/x86_64/cpu.rs` | none yet (ROADMAP §11.3) | `crates/core/src/arch/stub.rs` |
| Interrupt controller and IRQ identity | port module | `crates/core/src/irq/mod.rs`, `src/irq/irq_init.rs` | `src/arch/x86_64/pic.rs`, `src/arch/x86_64/apic_init.rs` | `crates/core/src/arch/x86_64/pic.rs`, `crates/core/src/arch/x86_64/apic.rs` | none yet (ROADMAP §11.3) |
| IPI send and its ordering | trait (`IpiSend`) | `crates/core/src/irq/ipi.rs`, `src/irq/ipi_init.rs` | `src/arch/x86_64/ipi.rs`, `src/arch/x86_64/apic_init.rs` | `crates/core/src/arch/x86_64/apic.rs`, `crates/core/src/arch/x86_64/vectors.rs` | `crates/core/src/arch/stub.rs` |
| Timer and cycle counter | trait (`CycleCounter`) | `crates/core/src/time/mod.rs`, `src/time/time_init.rs` | `src/arch/x86_64/mod.rs`, `src/arch/x86_64/apic_init.rs`, `src/time/time_init.rs` | none yet (ROADMAP §11.3) | `crates/core/src/arch/stub.rs` |
| Page-table format and attributes | trait (`PageTable`) | `crates/core/src/mm/paging.rs`, `crates/core/src/proc/addr_space/mod.rs`, `src/mm/paging_init.rs` | `src/arch/x86_64/mmu.rs` | `crates/core/src/arch/x86_64/paging.rs` | `crates/core/src/arch/stub.rs` |
| TLB maintenance and address-space ids | trait (`PageTable`) | `src/irq/ipi_init.rs`, `src/proc/addr_space_init.rs`, `src/mm/paging_init.rs` | `src/arch/x86_64/mmu.rs` | none yet (ROADMAP §11.2) | `crates/core/src/arch/stub.rs` |
| Cache maintenance and DMA coherence | trait (`Barriers`) | `crates/core/src/dev/dma.rs`, `src/dev/dma_init.rs` | `src/arch/x86_64/mod.rs` | none yet (ROADMAP §11.2) | `crates/core/src/arch/stub.rs` |
| Barriers (`dma_wmb`, `dma_rmb`, `dma_mb`) and MMIO accessors | trait (`Barriers`) | `crates/core/src/dev/virtio.rs` | `src/arch/x86_64/mod.rs` | none yet (ROADMAP §11.2) | `crates/core/src/arch/stub.rs` |
| Atomics | module selected by `cfg(loom)` | `crates/core/src/atomic.rs` | none yet (ROADMAP §10.8) | none yet (ROADMAP §10.8) | none yet (ROADMAP §10.8) |
| Per-CPU base and current-thread registers | trait (`PerCpuBase`) | `crates/core/src/smp/per_cpu.rs`, `src/smp/per_cpu_init.rs` | `src/arch/x86_64/percpu.rs`, `src/arch/x86_64/gs.rs` | none yet (ROADMAP §11.4) | `crates/core/src/arch/stub.rs` |
| Syscall instruction, user frame's layout (§5.10), numbers and argument order | trait (`SyscallAbi`) | `crates/core/src/proc/syscall_table.rs`, `crates/core/src/proc/syscall.rs`, `crates/core/src/proc/uabi.rs`, `src/proc/proc_init/mod.rs` | `src/arch/x86_64/mod.rs`, `src/proc/syscall_init.rs` | `crates/core/src/arch/x86_64/trap.rs`, `crates/core/src/arch/x86_64/syscall.rs`, `crates/core/src/arch/x86_64/stat.rs` | `crates/core/src/arch/stub.rs` |
| User-memory accessors | trait (`UserAccess`) | `crates/core/src/proc/uaccess.rs`, `src/proc/uaccess_init.rs` | `src/arch/x86_64/uaccess.rs` | none yet (ROADMAP §11.6) | `crates/core/src/arch/stub.rs` |
| FP and SIMD state | port module | `crates/core/src/sched/fpu.rs` | `src/proc/syscall_init.rs` | none yet (ROADMAP §11.6) | none yet (ROADMAP §11.6) |
| User TLS register | port module | `src/proc/proc_init/exec.rs` | `src/arch/x86_64/cpu.rs` | none yet (ROADMAP §11.6) | none yet (ROADMAP §11.6) |
| Context switch | trait (`ContextSwitch`) | `crates/core/src/sched/thread.rs`, `src/sched/thread_init/mod.rs` | `src/arch/x86_64/switch.rs`, `src/arch/x86_64/mod.rs` | none yet (ROADMAP §11.4) | `crates/core/src/arch/stub.rs` |
| Secondary-CPU bring-up | port module | none yet (ROADMAP §11.4) | `src/smp/smp_init.rs`, `src/arch/x86_64/trampoline.rs`, `src/arch/x86_64/trampoline.S` | none yet (ROADMAP §11.4) | none yet (ROADMAP §11.4) |
| CPU identity, topology, and features | port module | `src/smp/per_cpu_init.rs` | `src/arch/x86_64/cpu.rs`, `src/arch/x86_64/percpu.rs` | none yet (ROADMAP §11.4) | none yet (ROADMAP §11.4) |
| Idle | port module | `src/sched/thread_init/idle.rs`, `src/console/console_init.rs` | `src/arch/x86_64/cpu.rs` | none yet (ROADMAP §11.3) | none yet (ROADMAP §11.3) |
| Power-off and reset | port module | `src/shell/cmds/sys.rs`, `src/proc/proc_init/floor.rs` | `src/arch/x86_64/power.rs`, `src/arch/x86_64/cpu.rs` | none yet (ROADMAP §11.4) | none yet (ROADMAP §11.4) |
| Machine description | pure half | `crates/core/src/machine/mod.rs`, `crates/core/src/machine/fdt.rs`, `crates/core/src/acpi/mod.rs` | `src/machine/machine_init.rs`, `src/acpi/acpi_init.rs` | none yet (ROADMAP §11.5) | none yet (ROADMAP §11.5) |
| PCI configuration access | port module | `crates/core/src/dev/pci.rs`, `src/dev/pci_init.rs` | `src/dev/pci_init.rs` | none yet (ROADMAP §11.5) | none yet (ROADMAP §11.5) |
| Hardware RNG | port module | `src/dev/entropy_init.rs` | `src/arch/x86_64/cpu.rs` | none yet (ROADMAP §11.5) | none yet (ROADMAP §11.5) |
| Debug and single-step state | port module | none yet (ROADMAP §17.4) | `src/arch/x86_64/idt.rs` | `crates/core/src/arch/x86_64/vectors.rs` | none yet (ROADMAP §17.4) |
| Panic stop | port module | `src/log/panic.rs` | `src/irq/ipi_init.rs`, `src/arch/x86_64/ipi.rs` | none yet (ROADMAP §10.7) | none yet (ROADMAP §10.7) |
| Unwinder | port module | `src/log/panic.rs`, `src/log/ksyms.rs` | `src/arch/x86_64/cpu.rs` | none yet (ROADMAP §10.7) | none yet (ROADMAP §10.7) |
| Signal frame, sigreturn trampoline, vDSO counter read, ELF machine, TLS variant, and HWCAP | pure half | `crates/core/src/proc/mod.rs` | none yet (ROADMAP §11.6) | none yet (ROADMAP §13.8) | none yet (ROADMAP §13.8) |
| Crash-dump page-table publication | port module | `crates/core/src/log/vmcoreinfo.rs` | `src/log/vmcoreinfo_init.rs` | none yet (ROADMAP §10.7) | none yet (ROADMAP §10.7) |
| Hypervisor | port module | none yet (ROADMAP §21.1) | none yet (ROADMAP §21.1) | none yet (ROADMAP §21.1) | none yet (ROADMAP §21.1) |

The x86_64 port's other files: `src/arch/x86_64/catch.rs` (the in-guest tests' fault catcher,
`kernel_tests` only), `crates/core/src/arch/x86_64/mod.rs` and `src/arch/x86_64/mod.rs` (the
module roots; the latter holds the zero-sized `Arch` and its `InterruptMask`, `CycleCounter`,
`ContextSwitch`, `Barriers`, `PerCpuBase`, `SyscallAbi` and `Port` impls).

ROADMAP §11.4's aarch64 files, named here so `check_arch.py` can see them without a new
seam-table column: `src/arch/aarch64/secondary.rs` (PSCI stub, identity TTBR0, EL2 list),
`src/arch/aarch64/switch.rs` (x19–x29, SP, LR, DAIF), `src/arch/aarch64/percpu.rs`
(`TPIDR_EL1` or `TPIDR_EL2`), `src/arch/aarch64/power.rs` (`SYSTEM_OFF`/`SYSTEM_RESET`,
`CPU_ON`), `crates/core/src/arch/aarch64/psci.rs` (IDs and `SecondaryParam`). Bring-up
lives in `src/smp/smp_init.rs` beside the x86 path.

`vibeos-core` names a few of the reference port's pure-half items directly: `crates/core/src/arch/mod.rs`
re-exports the x86_64 descriptor, vector, 8259, APIC and UART encodings. Host tests keep the
x86_64 `UserFrame` and `SYS_*` numbers; the kernel picks the running port's in
`src/arch/current.rs` (ROADMAP §11.6).

## Fenced sites

Each file outside `src/arch/` and `crates/core/src/arch/` where the audit grep below still finds
x86_64 code, all
of it under `#[cfg(target_arch = "x86_64")]` or `#[cfg(target_arch = "aarch64")]` (an item, a
statement, an array element, a `use` or `mod` line, or a whole module). A fenced site in a shared
module is a seam defect: the port that needs the concern moves it behind the seam
(PORTABILITY.md §11.3).

| File | Items | §11.1 row | Why fenced, not moved |
|---|---|---|---|
| `src/main.rs` | the root `x86` alias and `apic_init` use line; `gp_test_trip` | Exception entry and exit | the alias names the x86_64 port for the modules below; `gp_test_trip` is the `gp_test` build's `#GP` |
| `src/boot/fw_cfg_init.rs` | the port I/O `select` / `read_bytes` / `transfer` halves | Machine description | QEMU's fw_cfg is port I/O on x86_64 and MMIO on aarch64 (ROADMAP §11.5); both live in this module |
| `src/console/kbd_init.rs` | the whole module (its `mod` line in `src/console/mod.rs`) | Interrupt controller and IRQ identity | the i8042 PS/2 driver is PC hardware (ROADMAP Phase 11 splits it) |
| `src/console/console_init.rs` | `wait_key_loop`'s `cli`, `sti` and `sti; hlt` statements | Idle | ROADMAP §10.3's compiler-barrier box names these raw statements; the idle wait moves with the port's `wfi` sequence (§11.3) |
| `src/sched/thread_init/idle.rs` | `halt_if_idle`'s `cli`, `sti` and `sti; hlt` statements | Idle | as for `wait_key_loop` |
| `src/dev/pci_init.rs` | the `x86` use line; `cf8_read32`, `cf8_write32` | PCI configuration access | the `0xCF8`/`0xCFC` mechanism exists only on x86; ECAM is the portable path |
| `src/log/serial/raw.rs` | the `x86` use line; `init`, `write_byte`, `try_read_byte`, `this_cpu`, `stop_if_halting` | Early console | the 16550's port I/O; `check_cycles.py`'s `raw` rule keeps this module on the kernel root's `x86` alias alone |
| `src/log/panic.rs` | the `x86` use line; `dump_regs`, the panic handler's register capture, `frame_fields`, `exception_halt`, `exception_vec` | Unwinder | the register dump and the x86 exception frame; ROADMAP §10.7's dump owns the file's rework |
| `src/log/pvpanic_init.rs` | the whole module (its `mod` line in `src/log/mod.rs`) | Machine description | the ISA pvpanic device is port I/O, found through x86_64's port-I/O fw_cfg; aarch64's `pvpanic-pci` comes with that port (ROADMAP §11.7) |
| `src/proc/syscall_init.rs` | the whole module (its `mod` line in `src/proc/mod.rs`) | Syscall instruction, user frame's layout (§5.10), numbers and argument order | the `syscall`/`sysretq` entry, its MSRs, FPU state and GS/FS bases (ROADMAP Phase 11 splits it) |
| `src/proc/syscall_init_aarch64.rs` | the whole module (its `mod` line in `src/proc/mod.rs`) | Syscall instruction, user frame's layout (§5.10), numbers and argument order | the `svc`/`eret` entry, TTBR0, `TPIDR_EL0`, and V0–V31 save and load (ROADMAP §11.6) |
| `src/proc/proc_init/mod.rs` | the trap-decode use line; `sig_for_vec`, `try_user_fault`'s `#PF` kill line, the `kill_line_yield` and APIC-id test hooks | Trap decode | they read the x86 frame's vector, CR2, DR6, FSW and MXCSR; a second port refines its own `TrapKind` (ROADMAP §11.3) |
| `src/smp/smp_init.rs` | the `x86` use line; `patch_params`, `start_one`, `ap_entry` | Secondary-CPU bring-up | INIT-SIPI and the real-mode trampoline (ROADMAP Phase 11 splits the module) |
| `src/time/time_init.rs` | the `x86` and TSC use lines; `io_wait`, `calibrate_pit`, `program_pit_ch0`, `rtc_reg`, `eoi_pit`; `init`'s CMOS write and TSC publication | Timer and cycle counter | the PIT, the CMOS RTC and TSC calibration are PC platform timers (ROADMAP Phase 11 splits them) |
| `src/ktest/mod.rs` | `Fault.cr2` and its `catch_fault` initializer; `on_kvm` | Trap decode | the in-guest tests' `#PF` address and CPUID's hypervisor leaf |
| `src/ktest/user.rs` | `user_code!`'s `global_asm!` | Syscall instruction, user frame's layout (§5.10), numbers and argument order | the tests' ring-3 / EL0 code is assembled per architecture |
| `src/mm/paging_init.rs` | `physmap_slot`'s x86_64 slot; `have_1g_pages`'s CPUID `pdpe1gb` check | Page-table format and attributes | the slot constant and 1 GiB pages are this port's; the aarch64 hardware half is ROADMAP §11.2 |
| `src/mm/ktest.rs` | `test_nx_enforcement`, `test_stack_guard` and their `TESTS` rows; `va0_probe` | Page-table format and attributes | they read the `#PF` error code and CR2, and probe VA 0 with an x86 load |
| `src/proc/ktest/entry.rs` | the `x86` use line; `test_addrspace_map_unmap_teardown` and its `TESTS` row | TLB maintenance and address-space ids | it runs `invlpg` and opens the SMAP window by hand |
| `src/proc/ktest/hooks.rs` | the `x86` use line; `star_configured` | Syscall instruction, user frame's layout (§5.10), numbers and argument order | STAR and EFER.SCE are the `syscall` instruction's MSRs |
| `src/sched/ktest.rs` | the `x86` use line; `test_sched_lock_timer_irq` and its `TESTS` row; `test_fp_no_leak`'s CR0.TS check | Context switch | a software `int` to the timer vector, and x86's lazy-FPU bit |

## Audit

The audit grep covers `src/**/*.rs` and `crates/core/src/**/*.rs` outside `src/arch/` and
`crates/core/src/arch/`, with comments and the contents of string, raw-string and char literals
stripped. It matches `asm!`, `global_asm!` and `naked_asm!`; the words `x86` and `x86_64`; and the
whole words `cr0` to `cr4` and `cr8` in either case, `rdmsr`, `wrmsr`, `MSR_*`, `IA32_*`, `EFER`,
`STAR`, `LSTAR`, `FMASK`, `FS_BASE`, `GS_BASE` and `KERNEL_GS_BASE`, word boundaries as `grep -w`
has them (`_` is a word character, so `read_cr3` is no hit). Each hit is either moved (through a
seam trait on `arch::current::Arch`, or a port-neutral name `src/arch/current.rs` re-exports) or
fenced (above); a hit in `vibeos-core` can only be moved, since the crate carries no
`cfg(target_arch)` (ROADMAP §10.3).

It last ran at P10-S38's tick commit, over the tree of its step 9 (`20bb0da`): 168 hits in the
19 files above, every one fenced. `python3 scripts/check_arch.py --list` prints each hit with its
fence status; `python3 scripts/check_arch.py` fails on any hit that is neither moved nor fenced,
and on a file listed above with no hit left.
