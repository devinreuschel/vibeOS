# VMCOREINFO

The kernel describes itself to whoever reads a dump of its memory with one VMCOREINFO note
([ROADMAP §10.7](ROADMAP.md#107-forensics-and-tracing)). This file is the note's format: one build writes it and
another build's tools read it, so a key keeps its meaning across builds, as vibefs's format does
([VIBEFS.md](VIBEFS.md)).

Sources, all documentation, never GPL source (DESIGN §1.5): the note layout is the System V gABI's
"Note Section"; the device and its payload are QEMU's `docs/specs/vmcoreinfo.rst`, written through
the DMA interface of QEMU's `docs/specs/fw_cfg.rst`; the key shapes and Linux's own keys are Linux's
`Documentation/admin-guide/kdump/vmcoreinfo.rst`.

## Readers

- QEMU's `dump-guest-memory`, which copies the note into every ELF core it writes when the guest
  has given the `vmcoreinfo` device the note's address.
- The hostlib core tool (ROADMAP §10.7, P10-S85) and the `make debug` script it feeds, which check
  `BUILD-ID` against the kernel ELF and reach the kernel's tables through the roots below.
- §25.4's capture kernel, which finds the crashed kernel's note through the crash handover and puts
  it among the vmcore's notes.

## Note layout

One ELF note, little-endian, each part padded with zero bytes to a multiple of 4:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | `namesz`: 11 |
| 4 | 4 | `descsz`: the text's length, its last `\n` included |
| 8 | 4 | `type`: 0 |
| 12 | 12 | `VMCOREINFO\0`, padded to 12 |
| 24 | `descsz` | the text, padded to 4 |

The text is ASCII lines of `KEY=value`, each ending in `\n`. The kernel renders the note into one
page-aligned 4 KiB page of its image (`log::vmcoreinfo_init`, with the portable
`vibeos::log::vmcoreinfo::render`), so the note is physically contiguous and at most 4096 bytes.

## Publication

`vmcoreinfo_init::publish` runs once at boot, on the BSP, after the per-CPU array and the bootstrap
thread exist and before `irq: enabled`. It renders the note, translates its address, and then, when
QEMU's fw_cfg is present (probed only when CPUID reports a hypervisor, ROADMAP §10.2, so bare metal
never sees a write to port `0x510`) and lists `etc/vmcoreinfo`, reads that file's 16 bytes:

| Offset | Size | Field |
|---|---|---|
| 0 | 2 | `host_format`: the formats the host accepts |
| 2 | 2 | `guest_format`: the format the guest supplies; 1 is an ELF note |
| 4 | 4 | `size`: the note's length |
| 8 | 8 | `paddr`: the note's physical address |

When `host_format` has the ELF format (bit value 1), `publish` writes `{host_format as read,
guest_format = 1, size, paddr}` back through fw_cfg's DMA interface. Otherwise, and when fw_cfg or
the file is absent or the write fails, it records why and says so in its one `vmcoreinfo:` log line;
the note still exists in memory. A panic before `publish` leaves a core without the note.

The crash-path rule: a kernel entered through the crash path (ROADMAP §25.4) builds its own note
but never writes the device, as Linux's fw_cfg driver skips it in a kdump kernel, so a core taken
after a crash jump still describes the crashed kernel. No crash path exists before §25.4, and
`publish` is the only code that writes the device.

## Keys

`SYMBOL` values are virtual addresses in lowercase hex without a prefix; `NUMBER` and `LENGTH`
values are decimal. These are Linux's shapes. The kernel emits the keys in this order:

| Key | Value |
|---|---|
| `OSRELEASE` | the kernel crate's version (Linux's key) |
| `BUILD-ID` | 40 lowercase hex digits: the descriptor of the kernel ELF's `NT_GNU_BUILD_ID` note (Linux's key) |
| `PAGESIZE` | `4096` (Linux's key) |
| `NUMBER(vibeos_pgt_root)` | physical address of the kernel's x86_64 PML4 (`paging_init::kernel_cr3`) |
| `NUMBER(vibeos_pgt_levels)` | `4`: the kernel never sets `CR4.LA57` |
| `SYMBOL(vibeos_log)` | virtual address of the log ring's static, a `vibeos::log::KernelLog` |
| `SYMBOL(vibeos_tcbs)` | virtual address of the TCB slot array; each slot is a `TcbSlot`, null or a `Tcb` pointer |
| `LENGTH(vibeos_tcbs)` | slots in that array |
| `SYMBOL(vibeos_cpus)` | virtual address of the `PerCpu` array |
| `LENGTH(vibeos_cpus)` | entries in that array |

Naming rule: a key Linux defines keeps Linux's name and meaning; an entry only vibeOS defines is
`vibeos_*` inside Linux's `SYMBOL()`, `NUMBER()` and `LENGTH()` shapes (ROADMAP, How to read this,
Linux interfaces). Compatibility: readers ignore keys they do not know, and a key never changes
meaning; a new meaning takes a new key.

The three roots exist so the core tool reaches the tables through the types below, which are the
portable crate's, instead of through kernel-half containers (`SpinMutex<Sched>`,
`BootCell<Box<[PerCpu]>>`) whose layouts are not.

Planned keys, not yet emitted:

- `KERNELOFFSET` (ROADMAP §18.2): the KASLR slide, Linux's key.
- aarch64's TTBR1 root table (ROADMAP §11.7).
- `LAYOUT-VERSION` and the `SIZE`, `OFFSET` and `NUMBER` entries §25.4's dump filter reads.

`vibeos::log::vmcoreinfo::KEYS` lists the emitted keys in order; its host test `keys_documented`
fails unless each appears in the table above and `render` emits exactly those keys in order.

## Types the core tool reads

The tool reads these types from a core with the layouts vibeos-core gives them, so they are
`#[repr(C)]` (`ThreadState` is `#[repr(u32)]`: its tag is a `u32` at offset 0, 0 to 4 in
declaration order: `Ready`, `Running`, `Sleeping`, `Blocked`, `Dead`). A
`#[cfg(not(loom))] const _` block beside each type asserts its size, its alignment and the offset of
every field listed here, as literals, and compiles into the kernel and into every hostlib build, so
the kernel and the tool, built on Linux or macOS, cannot disagree. A change to one of these fields
changes its assertion in the same commit, and the tool with it.

Debug assertions change two of these layouts: a frame token (`pmm::Frames`) records its allocation
site with them, so `Tcb`'s fields after `stack`, and `PerCpu`'s `remote`, sit at other offsets in a
debug build than in a release build. The assertions pin both, and the tool reads a core with the
layout of the profile the kernel was built with.

| Type | File | Fields the tool reads | Assertions |
|---|---|---|---|
| `KernelLog` (`IrqCell<KernelLogger, _>`) | `crates/core/src/cell.rs` | `data` (at 0), `owner` (right after `data`) | size in `log/mod.rs`; field order in `cell.rs` |
| `KernelLogger` (`Logger<RING_CAP, MSG_CAP>`) | `crates/core/src/log/mod.rs` | `ring`, `filter` | `log/mod.rs` |
| `Ring<RING_CAP, MSG_CAP>` | `crates/core/src/log/mod.rs` | `recs`, `head`, `len`, `dropped`, `written` | `log/mod.rs` |
| `Record<MSG_CAP>` | `crates/core/src/log/mod.rs` | `timestamp`, `cpu_id`, `level`, `len`, `msg` | `log/mod.rs` |
| `Filter` | `crates/core/src/log/mod.rs` | `max` | `log/mod.rs` |
| `TcbSlot` | `crates/core/src/sched/thread.rs` | pointer-sized: null, or a `Tcb` pointer | `sched/thread.rs` |
| `Tcb` | `crates/core/src/sched/thread.rs` | `id`, `state`, `context`, `cpu`, `pid` | `sched/thread.rs` |
| `ThreadState` | `crates/core/src/sched/thread.rs` | the tag; `Sleeping`'s deadline and `Blocked`'s queue at offset 8 | `sched/thread.rs` |
| `CpuContext` | `crates/core/src/sched/thread.rs` | every register slot | `sched/thread.rs` |
| `PerCpu` | `crates/core/src/smp/per_cpu.rs` | `cpu_id`, `current`, `idle`, `runq`, `remote` | `smp/per_cpu.rs` (the pin block for `cpu_id`, `current` and `idle`) |
| `PerCpuRemote` | `crates/core/src/smp/per_cpu.rs` | `apic_id` | `smp/per_cpu.rs` |
| `ReadyQueue` | `crates/core/src/sched/mod.rs` | `buf`, `head`, `len` | `sched/mod.rs` |

The APIC id is `PerCpuRemote.apic_id`, which the tool reaches through `PerCpu.remote`; `PerCpu`
has no `apic_id` field of its own. `KernelLog`'s port parameter is only named, so its layout is the
same for every port.

## Build id

The kernel link passes `--build-id=sha1` (`.cargo/config.toml`), so lld writes a 20-byte
`NT_GNU_BUILD_ID` note; its bare `--build-id` would write an 8-byte fast hash. `linker.ld` keeps
`.note.gnu.build-id` in the read-only segment between `__build_id_start` and `__build_id_end`, ahead
of the `/DISCARD/` rule that drops every other note, and the kernel reads its own id there. The
two-pass ksyms link changes the id between passes; only the final ELF's counts, and the kernel reads
its in-memory copy, so the note and the ELF agree.
