# 5. Interrupts

Index: [DESIGN.md](DESIGN.md). This file holds DESIGN §5, and its headings keep DESIGN's numbers.

Descriptor tables, the vector map, and the migration from the legacy 8259 to the APIC. Handler rules
are in [section 2.2](INVARIANTS.md#22-interrupt-handler-rules); this is the mechanism. §5.1 to §5.7 are x86_64's
mechanism; aarch64's vector table, exception entry, and interrupt controller are
[§11.5](PORTABILITY.md#115-aarch64-exceptions-and-privilege-transitions)'s. §5.8 and §5.10 hold on both architectures.

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
([section 7.5](SMP.md#75-per-cpu-data)), since ring 3 can load `0x2b` or 0 itself. Slot `0x20` stays null
because it is Linux's compat code segment, and a far transfer or `rt_sigreturn` to `0x23` gets
`SIGSEGV` (`docs/LINUX.md`, `no-compat-cs`). The kernel selectors are invisible to user code and keep
their places. `vibeos::desc` holds the layout, `desc::star_value()` the STAR value both
`syscall_init::init_cpu` and the in-guest `star_sysret_layout` use. The null DS, ES, FS, and GS are a
rule not yet enforced: ROADMAP §10.6. Today `syscall_init::first_return` loads `0x2b` into DS, ES,
FS, and GS.

Syscalls reach user memory through `vibeos::proc::uaccess` and its kernel half
`proc::uaccess_init` (ROADMAP §10.6): `copy_from_user`, `copy_to_user`, their `_partial` forms,
which return the bytes copied, and `strncpy_from_user`, which reads in chunks that stop at each page
boundary and at `USER_MAP_END`. Each copy runs `user_range_ok` first, a pure check with no
page-table walk: a non-empty range from `NULL_GUARD_LEN` up to at most `USER_MAP_END` with no
overflow, or an empty range below `USER_MAP_END`. The check stays because inside `stac`/`clac` the
MMU still lets the kernel reach its own pages. The port's `UserAccess` methods
(`arch::x86_64::uaccess`) then run one `rep movsb` on the user VA inside `stac`/`clac`, or without
them where SMAP is missing, since both raise `#UD` there; so a not-present page faults, and
`CR0.WP` faults a write to a read-only one. Each such instruction has one `__ex_table` record,
`uaccess::ExEntry`: the instruction and its fixup as offsets from each field's own address, and the
kind in bit 0 of `data`. `linker.ld` keeps the section whole inside `__rodata_*`. A CPL-0 `#PF` at
a recorded instruction with CR2 below `USER_MAP_END` resumes at its fixup, the `clac`, with RCX
holding the bytes left, and the accessor reports the bytes it copied ([§5.2](#52-idt-and-exceptions)).
Until ROADMAP §10.6's identity-teardown box removes the GLOBAL identity map of VA 0 to 512 MiB, the
x86_64 methods also refuse a range that starts below 512 MiB. The ELF loader and `clone_anon` still
copy through the HHDM physmap with `AddressSpace::read_bytes`/`write_bytes` after
`check_user_range`, which ignore the PTE's `WRITABLE` bit, until ROADMAP §10.6's fill-API box.
`arch::cpu::init_control_regs` sets `CR0.WP`, and `CR4.SMEP`, `SMAP` and `UMIP` where CPUID reports
them, on every CPU; `stac`/`clac` are no-ops when SMAP is missing.

Two kinds of accessor are told apart by that kind bit. Every entry today is faulting (bit 0 clear):
its fault resumes at the fixup, and the call returns `EFAULT` or a short count. Planned (ROADMAP
§12.2, §12.5): from §12.2 a faulting accessor (`copy_from_user`, `copy_to_user`, and their string
and vector forms) handles a fault through the region fault handler first, which may sleep, and
returns `EFAULT` only when that handler cannot resolve the fault. It runs with IF=1, no
spinlock held, and no sleeping lock of §2.1 levels 2 to 4 held (§2.9 rule 4). A non-faulting
accessor's fault goes straight to the fixup and returns a short count: no region lookup, no lock, no
sleep, and IF left as it was. It may run anywhere, under a busy page, a spinlock, or IF=0. The
buffered `write` path (§2.1) and the futex word read (ROADMAP §13.5) use it; after a short count they
release what they hold, fault the page in, and retry. The exception table lists accessor
instructions only, and aarch64's table carries the same kind bit. Rejected: a per-thread no-fault
count (Linux's `pagefault_disable`), which adds per-thread state to the fault path and puts the
choice away from the instruction that faults.

Each CPU gets its own GDT and TSS (`gdt::CpuTables`): the BSP's lives in a `BootCell`, filled for
its final address before the cell is set, and `gdt::alloc_ap_tables` allocates each AP's and keeps
it as the pointer `TryBox::into_raw` returns, which `free_ap_tables` hands back to
`TryBox::from_raw`. TSS.RSP0 is the stack an interrupt or exception from ring 3 lands on. `syscall`
does not read the TSS; its entry loads `PerCpu.kernel_rsp0`. `syscall_init::set_rsp0_for` sets both
to the incoming thread's stack top on every switch (the per-CPU `fallback_rsp0` for a thread with no
stack of its own); the CPU never writes RSP0. The TSS sits in an `UnsafeCell` inside `CpuTables`,
and `CpuTables::set_rsp0`, an `unsafe fn` that only the owning CPU calls with IF=0, is its one
writer after `load`: `set_rsp0_for` reaches it through `PerCpu.tables`, which `syscall_init::init_bsp`
and `init_ap` set (ROADMAP §10.3, F089). The TSS also holds the IST array.

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

One IDT of 256 gates (`arch::idt::IDT`) is shared by every CPU. `arch/x86_64/idt.rs` generates one entry
stub per vector from one `const` table, `ROWS`, whose rows give each vector's error-code flag
(`vectors::pushes_error_code`, read in a `const` context), IST slot, and gate DPL; `idt::init`
points every gate at its stub. The stub builds an `arch::idt::TrapFrame`: the vector, the error
code, CR2 for `#PF` and DR6 for `#DB`, then Linux's `user_regs_struct` (`syscall::UserFrame`),
whose last five words are the hardware frame. It makes the GS decision
([section 5.10](#510-privilege-transitions) rules 1 to 3) and calls one dispatcher, which runs the
vector's body: a plain `fn(&mut TrapFrame)` that `idt::set_handler` registers, or a default body
that passes a ring-3 fault to `proc_init::try_user_fault` and otherwise dumps and halts. `init`
registers the named exception, PIC, LAPIC, and IPI bodies; `irq_init` (`0x31`–`0x7F`) and
`kbd_init` (`0x21`, `0x30`) register theirs the same way, so their entries take the same GS step.
No body is an `extern "x86-interrupt"` function. Every gate is an interrupt gate
(`IdtEntry::interrupt`), so it clears IF on entry. Every gate but `#BP`'s is DPL 0 (type `0x8E`), so
`int n` from ring 3 raises `#GP`; `#BP`'s is DPL 3 (type `0xEE`), so a user `int3` reaches its body
and gets `SIGTRAP`.
`#DF`, NMI, `#MC`, and `#DB` run on IST stacks (§5.1).

`catch::intercept`, the in-guest test registry's exception catcher, compiles only under `kernel_tests`
(`arch::catch` and the dispatcher's intercept hook both), where it runs in the dispatcher before the
body of every vector below `0x20`; a production build runs every body directly (ROADMAP §10.2,
F146). It acts only on a CPL-0 frame on the CPU that armed the catch, so a fault on another CPU, or
one raised by ring 3, takes its normal path ([§2.7](INVARIANTS.md#27-invariant-register) row I29).

Rule: ring 3 never halts the kernel. Each exception vector has one row below. The Ring 3 column is
the rule; the last column says what the code does where it differs. The table lives in
`vibeos-core` (`vibeos::trap`), and `proc_init::sig_for_vec` reads each ring-3 answer from it,
after refining the cause from the frame's DR6 (`#DB`) or the thread's FSW and MXCSR (`#MF`,
`#XM`). The x86_64 port decodes each vector and error code into a portable
`TrapKind`, as the aarch64 port decodes its exception classes
([§11.5](PORTABILITY.md#115-aarch64-exceptions-and-privilege-transitions)), and one table gives each `TrapKind` its ring-3
action, the signal and the `si_code` Linux sends; a host test runs each vector `0x00`–`0x1F` through
decode and table and fails on any without a ring-3 row. `TrapKind` names what the CPU reported;
ROADMAP Phase 12's fault kinds (demand-zero, COW, file, stack) name how the fault path resolves a page
fault, downstream of it.

| Vector | Name | Ring 0 | Ring 3 | Ring 3, as built |
|--------|------|--------|--------|------------------|
| `0x00` | `#DE` | dump, halt | `SIGFPE` | as the rule |
| `0x01` | `#DB` | dump on IST, halt. Planned (ROADMAP §17.4, §18.4): three cases continue instead. A hit whose saved DR6 names only slots the current thread's tracer armed is dropped, as Linux drops a kernel-mode hit of a ptrace breakpoint; DR6.BS clears TF in the saved frame and logs once, as Linux does; in the ROADMAP §18.4 detector build a hit on a detector slot is reported | `SIGTRAP` (RFLAGS.TF, `int1`, a breakpoint or watchpoint the tracer armed); in the ROADMAP §18.4 detector build a hit on detector slots alone resumes with no signal and is counted | as the rule: the stub moves a CPL-3 frame off its IST stack, and the body runs with IF=1 and kills the process with `SIGTRAP` (§5.10 rule 3) |
| `0x02` | NMI | the handler first swaps its CPU's stop request word to 0 (`ipi_init::nmi_stop`, [§2.5](INVARIANTS.md#25-panic-policy) step 1, ROADMAP §10.7, F135), before any write or lock: a CPU already stopping or stopped halts again at once, an NMI on the dump owner returns at once, and STOP stops the CPU (`stopped (nmi)`); any other NMI dumps on IST and halts. Planned (ROADMAP §25.5): a backtrace or lockup request, and an external NMI on a CPU that is neither stopped nor the dump owner, are handled and return | not a ring-3 fault: the Ring 0 column applies | as the rule |
| `0x03` | `#BP` | log, continue | `SIGTRAP` (`int3`) | as the rule |
| `0x04`, `0x05`, `0x07`, `0x0A` | `#OF`, `#BR`, `#NM`, `#TS` | dump, halt | `SIGSEGV` | as the rule |
| `0x06` | `#UD` | dump, halt | `SIGILL` | as the rule |
| `0x08` | `#DF` | dump on IST, halt | not a ring-3 fault: the Ring 0 column applies | as the rule |
| `0x0B`, `0x0C` | `#NP`, `#SS` | dump, halt | `SIGBUS`; `SIGSEGV` for a fault on the return-to-user `iretq` (§5.10 rule 2) | as the rule |
| `0x0D` | `#GP` | dump with error code, halt | `SIGSEGV`, including a fault on the return-to-user `iretq` (§5.10 rule 2) | as the rule |
| `0x0E` | `#PF` | a fault on a user accessor's copy instruction with CR2 below `USER_MAP_END` resumes at its exception-table fixup and ends in `EFAULT` or a short count (§5.1); `catch::intercept` runs first. Any other: dump with CR2, halt. Planned (ROADMAP §12.2, §12.5): the region fault handler resolves a faulting accessor's fault before its fixup, and a non-faulting accessor's goes straight to the fixup | `SIGSEGV`. Planned (ROADMAP §12.2): a fault on a page that a region reserves is resolved first, and one through a file mapping on a page wholly past EOF, or on a page whose fill fails, gets `SIGBUS`, and so does a store through a shared file mapping whose space reservation fails (§4.3) | as the rule |
| `0x10` | `#MF` | dump, halt | `SIGFPE` | as the rule |
| `0x11` | `#AC` | dump, halt | `SIGBUS`, for a misaligned access while ring 3 has set RFLAGS.AC; `CR0.AM` is set on every CPU, as Linux sets it | as the rule |
| `0x12` | `#MC` | dump on IST, halt. Planned (ROADMAP §25.1, §25.3): only a fatal machine check, or an action-required error in kernel memory, halts; a lower severity is recorded and the CPU continues | not a ring-3 fault: the Ring 0 column applies. Planned (ROADMAP §25.3): an action-required error that ring-3 code consumed is recorded by the handler and recovered in exit work (§5.10 rule 11), which sends `SIGBUS` with `BUS_MCEERR_AR` | as the rule |
| `0x13` | `#XF` | dump, halt | `SIGFPE` | as the rule |
| `0x09`, `0x0F`, `0x14`–`0x1F` | reserved, `#VE`, `#CP`, `#HV`, `#VC`, `#SX` | dump, halt | `SIGSEGV` | as the rule |
| `0x20`–`0xFF` | IRQs and IPIs | handle, return. An interrupt no handler owns is counted per vector and per CPU, EOIed at the controller that delivered it (the LAPIC when its in-service bit for the vector is set, else the 8259), logged at most once a second per vector, and ignored; §5.5 gives the 8259 lines. Rule; not yet enforced: a pool vector (`0x31`–`0x7F`) with no handler is EOIed and ignored with no count, a vector in `0x80`–`0xEF` or `0xF3`–`0xFA` dumps and halts, and an 8259 line with no handler other than IRQ7 and IRQ15 prints `irq: unexpected` and halts the CPU that took it (ROADMAP §10.6) | handle, return to ring 3 | as the rule |

A halting handler prints the interrupt frame (RIP, CS, RFLAGS, RSP, SS), the error code where the
vector pushes one, and, for `#PF`, the CR2 the stub saved (§5.10 rule 9), then the common dump
(§2.5). A halt with no register dump is a wasted crash. A ring-3 fault whose signal takes its default
action (§2.5), the only action before ROADMAP §13.8, prints
`user: pid N killed SIG<name> rip=0x<rip> err=0x<err>`, plus
` cr2=0x<addr>` for `#PF` (a `user:` line, not a `vibeOS:` marker), and ends the process through
`finish_exit`. Every entry to ring 3 belongs to a spawned process, the in-guest tests' included
(ROADMAP §10.6), so a ring-3 fault always has a process to end.

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

The table has three registration rules. `vectors::NAMED` lists every named vector, `MC` (`0x12`)
included, and the host tests `named_vectors_are_unique` and `named_includes_mc` check it.
`idt::set_handler` asserts, as its first statement and before any guard, that its vector is `0x20` or
above and not one the table gives a fixed owner (`idt::fixed_owner`: the LAPIC timer, error, thermal
and spurious vectors and the four IPIs); callers pass constants or pool vectors, never input, so the
assertion guards a kernel invariant (AGENTS.md rule 4), and `idt::init` installs the exception and
fixed-owner bodies itself. A `const` block beside `idt.rs`'s row table fails the build unless the
table has exactly one row for each vector 0 to 255. The in-guest `idt_set_handler_refuses_fixed`
checks the assertion.

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
| 2 | Panic stop ([§2.5](INVARIANTS.md#25-panic-policy)) | `0xFE` |
| 3–7 | Free | |

No SGI does TLB shootdown: aarch64 broadcasts its TLB maintenance (ROADMAP §11.2).

## 5.4 IRQ registration

Drivers do not write to the IDT. They ask for a vector:

```rust
let vec = irq::allocate_vector(cpu)?;     // from the §5.3 device pool
irq::set_handler(vec, my_handler);
// or: irq::set_threaded(vec, Some(top_half), thread_fn, Some(inst));
```

Both threaded halves get the vector's context, a driver instance (`inst`, a `dev::Instance`) the vector
holds a reference to until it is set again or freed, so a driver with several devices keeps no table
of them (DEVICES.md §12.1 rule 1).

The kernel binary exposes this as `irq_init::allocate_vector`. Allocate is refused
inside a device hard-IRQ: `irq_init::dispatch` sets a per-CPU `hardirq::IN_ISR` flag around the handler. The
timer, IPI, and keyboard ISRs do not set it, and `park`, `begin_wait` and a voluntary `schedule` assert that it is clear
([INVARIANTS.md §2.2](INVARIANTS.md#22-interrupt-handler-rules)). That flag is not the `InterruptGuard` nest: `allocate_vector` takes only spinlocks, so a
caller with IF off may allocate, and only a device hard-IRQ is refused.

Planned (ROADMAP §11.3, on x86_64 before the GIC): drivers name an interrupt by an `IrqId`, a `u32`
the IRQ layer allocates, never a hardware number. It indexes the handler table, the interrupt's
bottom-half thread, and `free_vector`'s wait. Each interrupt controller is an `IrqChip` object, and
an `IrqId` records its chip and its hardware number on that chip (its hwirq):

```rust
let irq = irq::map_wired(&spec)?;      // a device-tree `interrupts` specifier or an ACPI GSI
let irqs = irq::alloc_msi(&dev, n)?;   // MSI or MSI-X, through the device's MSI parent
let tick = irq::map_percpu(&spec)?;    // a LAPIC LVT or a GIC PPI: one IrqId on every CPU
irq::set_threaded(irq, Some(top_half), thread_fn, Some(inst));
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
one chip object in a static `BootCell`, reached as `&'static dyn IrqChip`, as a driver's static
operations object is (§12.1); the indirect call is a branch beside an interrupt entry. A controller
a driver brings, such as a cascaded one, would be a counted device (§12.1), and the line that adds
one extends this. The seam's zero-sized port ([§11.1](PORTABILITY.md#111-the-seam)) keeps only the vector entry,
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
closed enum of each port's chips (no controller a driver brings could join); and waiting for ROADMAP §27.1
(every driver of Phases 12 to 20 written twice).

The allocator records the dest CPU. `set_affinity` updates that binding. I/O APIC
routes are rewritten immediately; MSI/MSI-X callers reprogram the message from
`cpu_of`. The ROADMAP §19.5 rebalance uses this table rather than a second map.

MSI message address is `0xFEE0_0000 | (apic_id << 12)` (physical dest, RH=0). Data is
the vector (fixed, edge). MSI-X table entries live in a BAR (BIR + offset from the
capability). The dispatcher writes mask, then addr/data, then the caller's mask bit,
and enables MSI-X; it writes no `COMMAND` bit, since the driver sets INTx disable and
bus mastering itself ([§12.3](DEVICES.md#123-resources)). Leaving INTx unmasked while
MSI-X is armed duplicates IRQs.

INTx remains the fallback when a function has neither MSI nor MSI-X: route the GSI
through the I/O APIC (PCI is level, active low). Keyboard keeps hardcoded vector
`0x30`; the pool starts handing out `0x31`. `free_vector` masks that GSI before it
clears the handler and forgets a `Route::IoApic` record. It also zeros threaded
`top`/`work`/`pending` so a recycled vector cannot keep the old bottom half. MSI
and MSI-X are message-based and do not need an I/O APIC mask on free. Clearing
first would let a still-asserted level line storm empty `dispatch` calls, and a
later `allocate_vector` could take IRQs from the old device.

Rule: `free_vector` masks the vector, sets its quiesce flag ([section 10.3](BLOCK.md#103-failure)), wakes
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
quiesce flag is set ([section 10.3](BLOCK.md#103-failure)). It takes no lock of §2.1's sleeping tier,
allocates without direct reclaim, in §4.4's atomic class, and leaves completion work beyond waking
waiters and settling page state to stage 2 ([section 10.1](BLOCK.md#101-completions)). In debug builds,
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
`set_threaded(vec, None, work, ctx)` is accepted, and `dispatch` EOIs before the bottom half runs, so on a
level-triggered INTx route a device that nothing quiets raises the line again at once (ROADMAP §15.2,
F099). The softirq stand-in is the high-prio workqueue: IRQ context enqueues a `fn(usize)` and
wakes workers. Planned (ROADMAP §19.4): its workers, one per CPU, run in the fair class at nice -20,
and bottom-half threads at `SCHED_FIFO` 50 ([§7.8](SMP.md#78-per-cpu-scheduling)).

## 5.5 8259 PIC

The PIC is a bootstrap artifact and a fallback, nothing more.

- Remap master to `0x20`, slave to `0x28`. Firmware may leave them at vectors `0x08`–`0x0F`, which
  collide with CPU exceptions (`#DF` at `0x08`, `#TS` through `#PF` at `0x0A`–`0x0E`), so a spurious
  IRQ before remap looks like a CPU exception.
- Mask everything (`0xFF` to both data ports) immediately after remap.
- After TSC calibration ([section 3.3](BOOT.md#33-_start-order) step 13b), `apic_init::prove` arms the LAPIC
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

Full register list and IPI encoding in [section 7.2](SMP.md#72-lapic). The parts that matter for interrupt
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
`GS_BASE` holds the user base (0; `syscall_init::first_return` writes it) and `KERNEL_GS_BASE`
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
through the seam's user-context trait ([§11.1](PORTABILITY.md#111-the-seam)): signal delivery and `rt_sigreturn`,
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
sets `x0` from `orig_x0` and moves `pc` back 4, as Linux does. Built so on x86_64: the frame is
`vibeos::arch::x86_64::trap::UserFrame` (`vibeos::syscall` re-exports it), which portable code reaches
through `vibeos::trap::SyscallAbi` (`restart` is the rewind); the syscall entry pushes it below
`PerCpu.kernel_rsp0` with a pad word under it, and a generated stub for a CS.RPL 3 frame builds
`arch::idt::TrapFrame`, whose last 21 words are the same frame at the same place, since the CPU
pushes its `iretq` frame at TSS.RSP0. `thread_init::spawn_user` writes a new thread's frame there,
and `syscall_init::first_return` enters the syscall exit at `vibeos_syscall_return` over it.

| Point | CPL, stack | GS | IF | AC |
|-------|------------|----|----|----|
| `vibeos_syscall_entry`, before its `swapgs` | 0, user RSP | user | 0 (FMASK) | 0 (FMASK) |
| syscall entry after its `swapgs`, and the syscall body | 0, `PerCpu.kernel_rsp0` | kernel | 0 until the stub has moved the user RSP out of the scratch and runs `sti`; 1 in the body ([§2.9](INVARIANTS.md#29-preemption-and-interrupt-state) rule 3) | 0 |
| syscall exit, from the return of `vibeos_syscall_stub` to `sysretq` or `iretq` | 0, kernel stack, then the user RSP | kernel; user after `swapgs` | 0 (rule 4) | 0 |
| `syscall_init::first_return`, from its `cli` to its jump into the syscall exit | 0, kernel stack | kernel, except from `mov gs` to its `GS_BASE` write, where the base is 0 and `KERNEL_GS_BASE` already holds `PerCpu` | 0 (rule 4) | 0 |
| non-IST vector taken at CPL 3 | 0, TSS.RSP0 | user until the stub's `swapgs` | 0 (interrupt gate); a fault or trap body then runs with IF=1, after the stub has saved the syndrome (rule 9), an interrupt's top half with IF=0 (§2.9 rule 3) | ring 3's until the stub's `clac` (rule 5) |
| non-IST vector taken at CPL 0 | 0, interrupted stack | kernel, except rule 2's case | 0 (interrupt gate) | the interrupted value until the stub's `clac` (rule 5) |
| IST vector taken at CPL 0 (`#DB`, NMI, `#MC`), and `#DF` | 0, its IST stack | whatever the interrupted point held (rule 3) | 0 | the interrupted value until the stub's `clac` (rule 5) |
| IST vector taken at CPL 3 (`#DB`, NMI, `#MC`) | 0, its IST stack, then the thread's kernel stack once the stub has copied its frame into the user frame (rule 3) | user until the stub's `swapgs` | 0; a `#DB` body then runs with IF=1 as any trap taken at CPL 3 does (§2.9 rule 3), an NMI's and a `#MC`'s with IF=0 | ring 3's until the stub's `clac` (rule 5) |
| vector exit to CPL 3 | 0, then 3 at `iretq` | user after `swapgs` | 0 until `iretq` restores ring 3's | `iretq` restores ring 3's |

On aarch64 the boundary is between EL0 and the kernel's level, EL1 or EL2 with VHE. The table below
is the required state; [§11.5](PORTABILITY.md#115-aarch64-exceptions-and-privilege-transitions) gives the mechanism and the
aarch64 counterparts of rules 1, 4, and 5. Rules 2 and 3 have none, because EL0 cannot reach the
per-CPU base register, so nothing is swapped at the boundary. Rules 9 onward hold on both
architectures. Planned (ROADMAP §11.3, §11.6): the aarch64 port does not exist.

| Point | Level, stack | DAIF | PAN | `SP_EL0` |
|-------|--------------|------|-----|----------|
| `svc` or exception taken from EL0, until the stub has saved the user frame | EL1 (EL2 with VHE), `SP_ELx` at the top of the thread's kernel stack, where the last return to EL0 left it | all set by the exception; the stub clears `MDSCR_EL1.SS` for a thread being stepped before any DAIF bit is cleared (§7.5, Debug state) | set by the exception (`SCTLR_EL1.SPAN` clear) | the user SP, until the stub saves it and loads `current` ([§2.9](INVARIANTS.md#29-preemption-and-interrupt-state) rule 5) |
| the syscall body, and the body of a fault or trap taken from EL0 | the kernel's level, the thread's kernel stack | all clear once the frame is saved (§2.9 rule 3) | set; clear only inside the user-memory accessors | `current` |
| IRQ taken from EL0, its top half | the kernel's level, the thread's kernel stack | D and A clear; I and F set | set | `current` |
| exception or IRQ taken at the kernel's level | the kernel's level, the interrupted stack, or this CPU's overflow stack when the entry's stack test finds it overflowed ([§11.5](PORTABILITY.md#115-aarch64-exceptions-and-privilege-transitions) rule 6) | all set by the exception; D and A clear once the frame is saved | set by the exception; the interrupted value returns with `SPSR_EL1` | `current`, untouched |
| return to EL0, from the first write of `ELR_EL1`, `SPSR_EL1`, or `SP_EL0` to `eret` | the kernel's level, then EL0 at `eret` | all set, until `eret` loads EL0's from `SPSR_EL1`; `MDSCR_EL1.SS` set after the last exit-work check, for a thread being stepped only (§7.5) | EL0 does not use it | the user SP, restored from the frame |

1. One entry stub per vector, owned by `arch/x86_64/idt.rs` and generated from one table. The stub runs
   `cld`, `clac` when SMAP is live, the GS decision, and rule 9's syndrome save, calls a body
   function, and mirrors the GS decision on exit. `idt::set_handler` takes a body function, never a
   gate, and no `extern "x86-interrupt"` function exists outside `src/arch/`; `scripts/check_entry.py`
   in `make check` enforces that last clause.
2. A non-IST vector decides `swapgs` from the saved CS.RPL (`arch::gs::from_user`), with one
   exception: a `#GP`, `#NP`, or `#SS` whose saved RIP is a return-to-user `iretq` arrives with the
   kernel CS and the user GS, and its handler swaps GS and sends the process `SIGSEGV`. User
   mappings end at `USER_MAP_END` (`0x0000_7FFF_FFFF_F000`), so a `syscall` in the last user page
   cannot leave a non-canonical return RIP. Built so: the labeled return-to-user `iretq`s are the
   syscall slow path's (`vibeos_syscall_iretq`, which a new thread's first return also takes), and
   the vector exits' (`vibeos_trap_iret`, `vibeos_trap_iret_ist`). The dispatcher runs
   `idt::user_return_fault` for `#GP`, `#NP`, and `#SS` before anything that reads `gs:`: when
   the saved RIP is one of those labels and the `iretq` frame at the saved RSP has CS.RPL 3, it
   loads `GS_BASE` from `KERNEL_GS_BASE` if `GS_BASE` is not a kernel (negative) address, and
   kills the process with `SIGSEGV`. The syscall exit tests the saved RIP after its `cli` and
   sends a non-canonical one to `vibeos_syscall_bad_rip`, which kills the process on the kernel
   GS before any `swapgs`, so it reaches neither `sysretq` nor `iretq`.
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
   §18.3). Linux's x86_64 entry splits the IST vectors the same way. The CPL-0 and `#DF` sign test
   is built: the IST entry path reads `GS_BASE` with `rdmsr` and keeps its decision in `ebx` for the
   exit. The CPL-3 path is built too: `vibeos_trap_entry_ist` swaps by CS.RPL, saves DR6 for `#DB`
   and resets it, copies the whole `TrapFrame` to `PerCpu.kernel_rsp0` minus its size, switches RSP
   there, and joins the non-IST entry's call; the dispatcher turns IF on for a CPL-3 `#DB` body, and
   the frame leaves through the non-IST exit (`vibeos_trap_iret`). The sign test fails once FSGSBASE lets userspace write a kernel-half GS base. Planned
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
   `syscall_init::first_return` runs `cli` before `mov gs`; each checks IF=0 in debug builds;
   `iretq` restores ring 3's IF from the frame. Both are built so. The syscall exit's `cli` follows
   the call, and in debug builds each exit path checks IF before its `swapgs` and faults at
   `vibeos_exit_if_set`; `console_init::wait_key` returns with the IF it was entered with. A
   spawned or forked thread's first return to ring 3 is the ordinary syscall exit over the user
   frame its creator wrote (`thread_init::spawn_user`): `first_return` runs `cli`, checks IF in
   debug builds (faulting at `vibeos_enter_if_set`), writes `KERNEL_GS_BASE` = `PerCpu`, loads the
   data selectors, writes `GS_BASE` = `PerCpu`, `KERNEL_GS_BASE` = 0, and `FS_BASE`, and jumps to
   `vibeos_syscall_return`, all in one asm block. The syscall exit stores the
   return value in the user frame's `rax` slot and reads everything it restores from the frame; no
   exit instruction writes a `gs:` operand.
5. Every interrupt and exception entry clears RFLAGS.AC before any other code: an interrupt gate
   clears IF and TF but not AC, and ring 3 can set AC with `popf`. The syscall entry clears AC
   through FMASK (bit 18). Every generated stub starts with `clac`, and `idt::init` points each gate
   at the stub when CPUID reports SMAP and 3 bytes in, past the `clac`, where it would be `#UD`
   otherwise; the stub's `iretq` restores the interrupted AC. Planned (ROADMAP §10.6):
   `stac` appears only inside the user-memory accessors.
6. A handler on an IST stack does not block and does not switch threads: a nested entry of the same
   vector restarts at the top of that IST stack and overwrites the first frame. It also takes no
   lock ([§2.2](INVARIANTS.md#22-interrupt-handler-rules)'s last row). An IST vector taken at CPL 3 leaves the
   IST stack before its body runs (rule 3), so this rule binds CPL-0 frames and `#DF`. Once IST
   handlers return (ROADMAP §10.6's CPL-3 `#DB`, §25.3's machine-check recovery, §25.5's NMI
   requests), three more things hold: an NMI handler takes no fault and runs no `iretq` before its
   own, since either unblocks NMIs while its IST frame is live (ROADMAP §25.5, F139); NMI, `#MC`,
   and `#DB` entries save DR7 and clear it before anything else (ROADMAP §18.4); and `#DF` never
   returns. Holds today for CPL-0 frames because every IST handler halts on them, except that under
   `kernel_tests` an armed `catch` steps RIP and returns or longjmps off the IST stack. A CPL-3
   `#DB` calls `try_user_fault` (the ring-3 `#DB` kill §5.2 requires) only after rule 3's move, on
   the thread's kernel stack with IF=1, because `try_user_fault` ends in `finish_exit`, which can
   switch threads.
7. Every gate but `#BP` is DPL 0, so `int n` from ring 3 raises `#GP`; the `#BP` gate is DPL 3, so
   `int3` delivers `SIGTRAP`. RFLAGS.TF and `int1` (`0xF1`) reach `#DB` at any DPL. `ROWS` gives
   each gate its DPL, and `IdtEntry::interrupt` encodes it (type `0x8E`, or `0xEE` for `#BP`).
8. Ring 3 never halts the kernel: §2.5 states the rule, and the §5.2 table gives each vector's ring-3
   action, §11.5's each aarch64 exception class's. On x86_64 `proc_init::sig_for_vec` reads every
   ring-3 answer from that one table (`vibeos::trap`).
9. An entry stub saves the exception's syndrome into its frame before anything can turn IF on or
   raise another fault on that CPU, on both architectures: CR2 for `#PF`, and DR6 for `#DB`, which
   it then clears, as Linux does, before a CPL-3 `#DB` frame leaves the IST stack; ESR_EL1 and
   FAR_EL1 (ESR_EL2 and FAR_EL2 at EL2 with VHE, through the same encodings) for every synchronous
   exception on aarch64, before the vector unmasks any DAIF bit. A body reads them from the frame,
   never from the register: once IF is on, a switch to a thread that faults overwrites them
   ([§2.9](INVARIANTS.md#29-preemption-and-interrupt-state) rule 3). On x86_64 only an NMI, `#MC`, or `#DB` can
   run between the delivery and the save, and none of their handlers takes a page fault (ROADMAP
   §25.5 for the NMI handler, which a debug build checks by comparing CR2 at its exit with its value
   at entry). Built so on x86_64: the `#DB` body reads DR6 only from `TrapFrame.dr6`
   (`vectors::dr6_cause`), and the stub writes DR6 back to `vectors::DR6_RESET`. Rule; not yet
   enforced: ROADMAP §11.3 (the aarch64 vectors).
10. Return state, on both architectures. A saved user frame that anything other than an entry from
    user mode wrote (`rt_sigreturn`, ptrace's `SETREGS`, `SETREGSET` of `NT_PRSTATUS` or
    `NT_PRFPREG`, `POKEUSER`, and any later writer of a saved context) passes one validator per
    architecture before the thread next returns to user mode, so no writer can return a thread to
    ring 0, EL1, or EL2, or to user mode with interrupts masked. The validators are pure code in each
    port's `vibeos-core` half ([§11.1](PORTABILITY.md#111-the-seam)), host-tested field by field, and each writer
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
      ([§11.2](PORTABILITY.md#112-address-space-on-aarch64)).

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
    ([§7.5](SMP.md#75-per-cpu-data)), a write under the FP binding, so a handler that interrupts code
    running under `std` or with a changed MXCSR starts from the psABI's state. On aarch64: `x0`
    holds the signal number and, under `SA_SIGINFO` only, `x1` and `x2` the `siginfo` and
    `ucontext`; `sp` points at the frame and `x29` at its frame record; `x30` is `sa_restorer` under
    `SA_RESTORER` and otherwise the vDSO's `__kernel_rt_sigreturn` (ROADMAP §13.8, §13.10); `pc` is
    `sa_handler`; `PSTATE.BTYPE` is 0 (`BTYPE_C` once ROADMAP §18.9 turns BTI on) and `PSTATE.TCO`
    is 0; V0-V31, FPCR, and FPSR keep their interrupted values. `rt_sigreturn` restores the saved
    context, TF included, after ROADMAP §13.8's validation. A thread that a tracer is
    single-stepping reports a step stop at the handler's first instruction (ROADMAP §17.4). The
    frame write may fault and sleep, so it runs where [§2.9](INVARIANTS.md#29-preemption-and-interrupt-state)
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
