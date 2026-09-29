# 3. Boot

Index: [DESIGN.md](DESIGN.md). This file holds DESIGN §3, and its headings keep DESIGN's numbers.

Power-on to `sti`. Limine does the ugly part (real mode, A20, long mode, ELF loading) and hands us a
64-bit kernel with paging already on. Everything after that is ours.

## 3.1 Toolchain

| Piece | Value |
|-------|-------|
| Channel | dated nightly in `rust-toolchain.toml` (bump with CI in one PR) |
| Why nightly | The kernel binary's `alloc_error_handler`; the flags `-Zsanitizer` (ROADMAP §12.1), `-Zretpoline-external-thunk` and `-Zfunction-return` (§18.3), and `-Zub-checks` (§18.4). `vibeos-core` uses none (§1.1 constraint 7). |
| MSRV | `rust-version` in `crates/core/Cargo.toml`, for `vibeos-core` only (§1.1 constraint 7): the older of the last stable release before the nightly Kani pins (ROADMAP §10.8) and the Rust release Verus requires (the latest Verus release's, until ROADMAP §38.1 pins one). Before §10.8 lands, the stable release current on the pinned nightly's date. A bump of the nightly, Kani, or Verus re-derives it. `1.98`, the stable release current on 2026-09-22, which `make check`, `setup.sh` and the `check` job read from the manifest |
| Components | `llvm-tools` (objdump/nm/size), `rustfmt`, `clippy`; `rust-src` for rust-analyzer |
| Target | built-in `x86_64-unknown-none` (`rust-toolchain.toml` `targets`) |
| Build | `cargo build` (default target in `.cargo/config.toml`) |
| Panic | kernel target `abort`; host tests `unwind` (`profile.dev`) |
| Extra host tools | `xorriso`, `nasm` (`user/*.asm`), `qemu-system-x86_64`, `python3`, `dosfstools` (`fsck.fat`; the host FAT tests fail without it unless `VIBEOS_ALLOW_MISSING_TOOLS=1`), `ruff` and `mypy` (`make check`, at the versions the `check` job pins) |

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
  would need a save around each use, and none exists. Nothing traps a kernel FP use, since `CR0.TS`
  stays clear, so `make` runs `scripts/check_kernel_fp.py` on each linked kernel ELF: with the
  toolchain's `llvm-objdump` (the `llvm-tools` component) it fails the build, and deletes the ELF,
  on any x87, MMX, SSE, or AVX instruction outside `syscall_init::fp_save`, `fp_load`, and
  `fp_init_template`. User code gets SSE: `arch::cpu::init_control_regs` clears `CR0.EM` and
  `CR0.TS` and sets `CR0.MP`, `CR0.NE`, `CR4.OSFXSR` and `CR4.OSXMMEXCPT` on every CPU, so x87 and
  SSE floating-point errors reach `#MF` and `#XF` (§5.2), and each thread's 512-byte FXSAVE image
  (`Tcb.fpu`) follows the FP binding (§7.5).
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
| Base revision | Protocol version handshake: revision 3 today; ROADMAP §11.1 moves both architectures to the one revision the pinned Limine accepts on aarch64. Halt with a serial line if unsupported. |
| Framebuffer | Linear BGRX8888, 32 bits per pixel. Row stride is `pitch` bytes, which may exceed `width * 4`. |
| Memory map | Physical regions and types. Only `USABLE` feeds the buddy allocator. |
| HHDM | Higher-half direct map offset. `virt = phys + offset` for any physical access before our own tables exist. |
| Executable address | Physical and virtual base of the loaded kernel, so we can map ourselves and exclude ourselves from the allocator. |
| RSDP | Physical pointer to the ACPI RSDP. Gates all of ACPI, APIC, HPET, SMP. |
| Executable command line | The `limine.conf` entry's `cmdline:`, read as raw bytes up to the NUL (at most 2048), never through the crate's `cmdline()`, which unwraps non-UTF-8. Optional: absent means empty. |
| SMP (optional) | Limine can bring up APs for us. We do it ourselves; see [section 7](SMP.md#7-smp) for why. |

Firmware reclaimable regions stay out of the free lists. Reclaiming them is a few megabytes for a
nonzero chance of stomping something ACPI still points at.

Kernel command line (ROADMAP §10.2). `boot::capture` keeps it in `BootInfo`: the `limine.conf`
entry's `cmdline:` (`vibeos.strace=0` in the shipped entry), then, on x86_64 when CPUID.1:ECX[31]
reports a hypervisor and QEMU's fw_cfg lists `opt/vibeos/cmdline`, one space and that file's text,
trailing NULs and whitespace stripped (`boot::fw_cfg_init`, invariant I244), so the harness sets
options on the unmodified ISO (`VIBEOS_CMDLINE`, [§8.4](TESTING.md#84-qemu-flags)). At most 2048 bytes
are kept, Linux's x86 `COMMAND_LINE_SIZE`; the rest is dropped with one log line. The kernel prints
it once as `vibeOS: boot: cmdline: <text>`, a byte outside 0x20 to 0x7E as `?`. The portable
`vibeos::boot::cmdline` parses it as Linux's `kernel-parameters.rst` describes: words split at ASCII
whitespace outside double quotes, which are removed from the word or value they enclose; `-` and `_`
are equal in a name; the last occurrence wins; `--` ends the kernel's words. Its option names follow
the Linux-interfaces rule. An option Linux defines keeps Linux's name and meaning
(`root=`, `init=`, `ro`, `rw`, `console=`, `loglevel=`, `panic=`, `mitigations=`, `crashkernel=`). An
option only vibeOS defines is `vibeos.<name>=`, the `module.parameter` form Linux's parser gives a
module's options, so no later Linux option can take its name. `sysctl.<path>=` sets a sysctl vibeOS
implements, and an unknown path is logged and ignored, as on Linux. A word the kernel does not
recognize reaches init as Linux passes it: an undotted `name=value` into init's environment, any other
undotted word, and every word after `--`, as an argument; an unrecognized dotted word is dropped with
a log line. Init gets at most 8 argv entries (`argv[0]` included) and 8 environment strings, the
initial stack's capacity until ROADMAP §10.6 raises it; a word past either is dropped with one log
line. No sysctl exists yet, so every `sysctl.<path>=` word (`.` or `/` separators) is logged and
ignored. The parser's `OPTIONS` lists each option as it lands, and this table lists it with its
ROADMAP §39.1 class: `internal` for an option only the harness or a test sets, such as
`vibeos.ktest=`, and `stable` or `unstable` for the rest. The host test `cmdline_options_documented`
fails when an `OPTIONS` row has no row here with its class.

| Option | Defined by | Class | Meaning | Box |
|--------|------------|-------|---------|-----|
| `vibeos.strace` | vibeOS | unstable | `vibeos.strace=1` (or bare, `y`, `Y`, `on`) prints one `user: syscall` line per syscall that returns ([SYSCALL.md §6](SYSCALL.md#6-tracing-and-counters)); anything else leaves it off | ROADMAP §10.7 |

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
`boot_contract_markers()` in `tests/harness/harness.py` ([section 8.3](TESTING.md#83-end-to-end)). DOC2 (ROADMAP
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
| 9 | Kernel heap | `heap ok` | `alloc` becomes legal. Until `irq: enabled` (step 15) boot may use its infallible API; from then on every allocation is fallible ([§4.4](MEMORY.md#44-kernel-heap)). |
| 10 | Kernel VA allocator | `kva: ready` | Guarded stacks need it, so threads need it. |
| 11 | Per-CPU area for the BSP, bootstrap TCB, syscall MSRs | `per_cpu: bsp ready` | `GS_BASE` must be valid before any `per_cpu!` access, including from ISRs. Then `thread_init::init_bootstrap` makes `_start`'s context the bootstrap thread, and `syscall_init::init_bsp` programs STAR, LSTAR, FMASK (§7.2), and `EFER.SCE`, enables SSE for user code (§3.1), and wires TSS.RSP0. `syscall_init::init_bsp` first runs `arch::cpu::init_control_regs`, which writes CR0 and CR4 whole (§11.4). |
| 12 | ACPI tables | `acpi: xsdt N tables` | MADT drives APIC and SMP, HPET drives calibration. |
| 13 | Time: HPET or PIT, TSC calibration | `time: tsc N/ms` | The scheduler needs a tick, and AP bring-up needs `busy_wait_ms`. |
| 13b | BSP LAPIC, I/O APIC, LAPIC timer | `time: lapic_timer ok (<mode>)` | After TSC calib. Prove a tick (TSC-deadline → periodic → PIT), then mask PIC + PIT GSI if LAPIC owns it. |
| 14 | Scheduler, idle thread on BSP | `sched: cpu0 ready` | Preemption target must exist before the timer starts firing into it. |
| 15 | Arm scheduler; emit `irq: enabled` | `irq: enabled` | Scheduler is live. The timer already ticks from steps 13/13b; this marker is post-sched arming (IF on, preemption live), not the first STI. IRQ1 stays masked until the keyboard driver (step 17). |
| 16 | APIC + SMP bring-up | `smp: done` | Needs time (delays), heap (per-CPU allocation), scheduler (AP entry point). Live Phase 4 order: SMP before console. |
| 17 | Framebuffer console, PS/2, mux | `console ok` | After `smp: done`. Install the IRQ1 / keyboard GSI handler, init the 8042, then unmask. Replay the pre-FB log ring onto the framebuffer. |
| 17b | PCI enum + device registry | `pci: N devices` | After `console ok`. ECAM for the buses the first MCFG allocation covers (`acpi::parse_mcfg` reads no other entry; F045); otherwise `0xCF8`/`0xCFC`, which the kernel uses only for bus 0 (a kernel limit: configuration mechanism #1 addresses any bus; ROADMAP §20.1, F114). Scan builds a device list. Workqueue + threaded IRQ start, then drivers bind by id. Memory BARs are mapped through ioremap or the capped physmap; sizes above 32 MiB are recorded and skipped (§9.2). |
| 17c | Block layer + ramdisk + virtio-blk + partitions | `block: <name> <n> sectors` | After bind. One line per device. virtio-blk (`vda`) emits during probe; ramdisk (`ram0`) follows in `block_init`; partition children (`<parent>p<N>`) after that. |
| 17d | VFS + FAT initrd root + pseudo mounts + vibefs | (none) | After block. Makefile FAT32 initrd at `/` when live, else dummy ramfs. Then devfs/procfs/tmpfs/sysfs on `/dev` `/proc` `/tmp` `/sys`. BSS vibefs at `/vibe` (Phase 8D). No serial marker: a root without `/sbin/init` shows as `user: init failed` and no `shell ready`. Syscalls do not reach the VFS or kernfs: `file_init` resolves paths through its own FAT and vibefs route tables (ROADMAP §10.4, F086). |
| 18 | `/hello`, builtins, `/sbin/init` as pid 1 | `shell ready` | Last marker. The bootstrap thread spawns `/hello` and waits for it (`proc_init::spawn_elf`, `proc_init::wait_kernel`), `shell_init::init` registers the builtins, and `proc_init::start_init` spawns `/sbin/init` pinned to the BSP. `/sbin/init` forks `/bin/tests` and waits for it without reading its exit status, then forks `/bin/sh`, which writes `shell ready` from ring 3 (`user/sh.asm`); the marker is not kernel-emitted and does not show that `/bin/tests` passed (ROADMAP §10.5, F073). A `kernel_shell` build instead spawns the kernel `shell` thread, which prints `shell ready`; a `kernel_tests` build runs the in-guest registry. |

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
Partition scan stamps an MBR on `ram0`. Only a `kernel_tests` build stamps a GPT, and only on a `vda`
whose table fails to parse or has no entries and whose LBA 0–33 and last 33 sectors all read back as
zeros; the production kernel never writes `vda` here ([section 10.5](BLOCK.md#105-partitions); ROADMAP
§10.11, F003). It emits
`block: <parent>p<N> <n> sectors` per child. A writeback cache thread starts
before the scan. `file_init::init` then makes the FAT initrd `/` (a ramfs root only when the initrd is not
live) and mounts devfs / procfs / tmpfs / sysfs on `/dev` `/proc` `/tmp` `/sys` and vibefs at
`/vibe`, with no serial marker. Step 18 runs `/hello`, then starts `/sbin/init`; the trailing
contract line is `shell ready`, from `/bin/sh` (row 18).
The harness's `boot_contract_markers()` asserts the live order ([section 8.3](TESTING.md#83-end-to-end)).

Planned (ROADMAP §25.4, §26.4): a boot without Limine starts in the image's direct entry
([§4.1](MEMORY.md#41-virtual-address-map)), which runs before step 1 and enters the kernel in the state Limine
would leave it, with a `BootInfo` in place of Limine's responses. Step 2's base-revision check runs
only on a Limine boot; a kernel started by kexec checks the handover format's version there instead
(ROADMAP §25.4). It prints `vibeOS: boot: <path> entry ok` in place of `limine: rev <n> ok`
([section 8.3](TESTING.md#83-end-to-end)).

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
`.text`, `.rodata`, `.data`, `.bss`, and `__ksyms_start` and `__ksyms_end` around the `.ksyms`
section, which sits after `.rodata` and before `__rodata_end`, so paging maps it (§2.5).

## 3.5 Profiles

Two Cargo profiles, one shipped. `dev` (`opt-level = 1`, debug assertions on) is what `make` builds
by default and what every per-push tier runs. `release` (`opt-level = 3`, debug assertions off) is
what v* releases and 1.0 ship, and a gate that compares with Linux or states a rate or throughput is
measured on it (ROADMAP, How to read this). Both set `overflow-checks = true`, so an arithmetic
overflow panics in shipped images as it does in tests, and AGENTS.md rule 4's `checked_*` rule holds
in both. What must hold in release is an `assert!` (§9.4). Rule; not yet enforced: nothing builds or
boots the release profile, and releases ship the dev profile (ROADMAP §10.2, F137).

`opt-level = 1` for the dev profile. At `opt-level = 0` the page table setup function's stack frame is
large enough to overflow the boot stack Limine provides, and it faults on entry before printing
anything. If a boot function needs a big frame, box it or move it to a thread with a real stack; do
not rely on the optimizer. Planned (ROADMAP §10.2): a `make check` script bounds each function's
frame at a value recorded here. The bound is a screen for one oversized frame; §4.5's measured
budget is what bounds a whole path.

Neither profile writes a host path into what ships. Every cargo build the Makefile runs for a shipped
artifact goes through `CARGO_SHIP`, which sets Cargo's `trim-paths = "all"` for the profile being
built (`-Ztrim-paths --config 'profile.<name>.trim-paths="all"'`). Cargo then passes rustc a
`--remap-path-prefix` for the checkout, the sysroot and `$CARGO_HOME`, so panic `Location` strings,
DWARF, and the ThinLTO `.llvm.<hash>` names `gen_ksyms.py` copies into the ksyms table carry none.
Trim-paths is unstable on the pinned nightly, so the setting lives on the command line: a manifest's
`cargo-features = ["trim-paths"]` would stop every stable cargo, the MSRV check's included, from
reading the workspace. It moves into `Cargo.toml`'s profiles once Cargo stabilizes it. Host tools do
not ship and build without it.

## 3.6 ISO and QEMU

`make` builds every variant in the one `target/`, copies each variant's ELF to
`build/kernels/vibeos-<variant>.elf`, and writes `build/vibeos.iso` and `build/vibeos-<variant>.iso`;
`make isos` builds them all. Each ISO recipe reads only its own named ELF, so a test build cannot be
packaged as production. The repository root holds no build product.

`make` stages `build/iso_root_<variant>/` with the kernel ELF, `limine.conf`, and the Limine BIOS
and UEFI artifacts, then builds a hybrid ISO with `xorriso` and runs `limine bios-install`. Hybrid
means the same image boots BIOS and UEFI, which matters for real hardware later.

The Makefile lists every `.rs` and `.asm` under `src/` and `crates/core/src/` as a prerequisite. A hand-maintained short list
produced stale ISOs when new subsystem directories appeared. The host tools (`mkfs-vibefs`,
`fsck-vibefs` and the other hostlib binaries) and `build/initrd.fat` build from `vibeos-core` too, so
their rules list `$(HOSTLIB_DEPS)`: every kernel source, `Cargo.lock`, the manifests, and
`tests/hostlib/src/bin/*.rs`.

Builds are reproducible: two builds of one commit give byte-identical kernels, initrd and ISOs
(ROADMAP §10.2, F151, F152). No build time or builder identity lands in them. The Makefile exports
`SOURCE_DATE_EPOCH`: the caller's value, else the commit's time (`git log -1 --format=%ct`), else
`1262304000`. `mkinitrd` stamps the files it adds with that time, in destination order whatever the
`--add` order. `mkiso.sh` stages every file above its time pin, which sets each staged path's times to
the epoch, and runs `xorriso` with `-r`, so Rock Ridge records uid and gid 0, and with
`--modification-date` and `--set_all_file_dates` at the epoch's UTC time, which also fixes the volume
UUID. No identifier is random either: `--gpt_disk_guid` is a constant in `mkiso.sh`, from which xorriso
derives the partition GUIDs, and after `limine bios-install`, which seeds the MBR disk signature at
`0x1B8` from `time(NULL)`, `scripts/iso_disk_id.py` overwrites it with the first 4 bytes of the SHA-256
of the image with those bytes zeroed. That is safe because `limine.conf` names its files with `boot():`,
never by disk signature. The xorriso version lands in the volume descriptor, so `mkiso.sh` records it
beside each ISO as `<iso>.xorriso-version`, which a release publishes. An incremental build keeps the
epoch of the commit it last rebuilt a file at; compare clean builds.

`make repro` does (`scripts/repro_build.py`, run by a scheduled job): it clones the commit twice, at
checkout paths of different lengths, each with its own `CARGO_HOME` and `RUSTUP_HOME`, a copy of
`limine/` and no `CARGO_TARGET_DIR`, runs `./setup.sh` and `make isos` in each, and fails unless every
`build/kernels/*.elf`, `build/*.iso` and `build/initrd.fat` matches byte for byte and holds none of the
checkouts, `$HOME`, or either `CARGO_HOME` or `RUSTUP_HOME`. `REPRO_ARGS=--share-rustup` reuses the
caller's toolchain for a local run; `REPRO_ARGS=--scan-only` only scans this checkout's `build/`.

`make run` boots with COM1 on stdio and more than one CPU, so the default developer loop exercises SMP
rather than discovering AP bugs only in CI. Full flag set in [section 8.4](TESTING.md#84-qemu-flags).

Interactive input, two paths (not USB HID):

| Where you type | What the guest sees |
|----------------|---------------------|
| QEMU window (focused) | PS/2 i8042 → IRQ1/GSI → `kbd_init` ring |
| Controlling terminal | COM1 (`-serial stdio`), polled after the PS/2 pop |

Many IDE-embedded terminals do not forward keystrokes to `-serial stdio`. Output
appears, input goes nowhere. Type in the QEMU window, or run from a real terminal.
QEMU monitor `sendkey` hits the same i8042 as the window; `make test-e2e` and
`make test-ps2` use that as the TCG stand-in.
