# D1 · Fixed-capacity tables: heap-allocate the growable ones (ROADMAP Phase 10)

**ROADMAP:** the §10.4 boxes that cite D1. Where this plan and those boxes differ, the boxes decide. Superseded here: the acceptance's `NoSpace` one past each limit; `limits_heap_backed` expects the error Linux returns at each limit, as the D1 box that allocates the tables at init says, and `scripts/check_limits.py` replaces the acceptance grep.

| | |
|---|---|
| **Area** | 4.3 Data model & state management |
| **Impact / Effort / Phase** | High / L / III |
| **Depends on** | Q3 (`BootCell` for late init), A3 (VFS owns files/fds) |
| **Blocks** | ROADMAP Phase 10 exit gate (the limits line), and through it Phases 12 and 13 |
| **Review** | [ARCHITECTURE_REVIEW.md §4.3](../ARCHITECTURE_REVIEW.md#43-data-model--state-management) |

## Problem

- 53 `MAX_*` constants size static arrays. The growable ones: `MAX_THREADS = 64` (`src/thread.rs`; `Sched.slots: [Option<Box<Tcb>>; MAX_THREADS]` at `src/thread_init.rs:46`), `MAX_FILES = 16`, `MAX_FDS = 16`, `MAX_INODES = 48`, `MAX_DENTRIES = 48`, `MAX_MOUNTS = 8`, `MAX_RAM_NODES = 64`, `MAX_KERN_NODES = 128` (`src/fs/mod.rs:16–27`, arrays at lines 599–604), `MAX_OPEN = 16` (`src/file_init.rs:27`, `static FILES`), `MAX_COMMANDS = 48` (`src/shell.rs:421`).
- Per process and per address space: `MAX_PROCS = 16` and the per-process `MAX_FDS = 16` (`src/proc.rs:8-9`; `procs` in `src/proc_init.rs`, `FdTable.slots` at `src/proc.rs:111`), `MAX_REGIONS = 32` (`src/addr_space.rs:14`). The per-CPU `ReadyQueue.buf` (`src/sched.rs:30`) is sized by `MAX_THREADS`.
- Hardware-bounded ones are fine as they are: `MAX_CPUS`, `MAX_IOAPICS`, `MAX_ISOS`, `MAX_BARS`, `MAX_DEVICES`, `MAX_VENDOR_CAPS`, `MAX_ORDER`.
- vibefs `MAX_BLOCKS = 1024` (4 MiB volumes) and `MAX_INODES = 64` are on-disk format v1 limits (`docs/VIBEFS.md`), a separate question.
- The heap exists since Phase 1 and `Vec`/`Box` are already used in `per_cpu_init`, `thread_init`, `virtio_blk_init`; the portable tables just predate it.

## Decision (maintainer deferred)

Keep static tables where bounded by hardware. For threads, processes, per-process fds, regions, inodes, dentries, files, mounts, open files, and commands, allocate at init from the heap (`Box<[T]>`) sized from one `limits` module, so raising a cap is a constant change rather than a type change. Keep the clock-eviction logic unchanged. A real slab stays at ROADMAP §19.9. Add a gate line stating the limits the next phase is tested against (landed as a ROADMAP Phase 10 gate line: the limits Phase 12 is tested against).

## Implementation plan

1. **`src/limits.rs`** (lib): `pub struct Limits { pub threads: usize, pub procs: usize, pub fds: usize, pub regions: usize, pub inodes: usize, pub dentries: usize, pub files: usize, pub mounts: usize, pub ram_nodes: usize, pub kern_nodes: usize, pub commands: usize }` (`fds` is per process) with `pub const DEFAULT: Limits` (threads 1024, procs 256, fds 256, regions 256, files, inodes and dentries at least 1024 so one process can hold 256 distinct open files, mounts 16, …) and `pub const SMALL: Limits` for host tests (today's numbers, so existing eviction tests keep their meaning). The Phase 10 gate states these limits; change the gate and this struct together.
2. **`Vfs`**: change the six arrays to `Box<[T]>`; `Vfs::new(limits: &Limits) -> Self` is no longer `const`. `fs_init` builds it in `init()` (heap is up) into a `BootCell` (Q3). Host tests call `Vfs::new(&Limits::SMALL)`.
3. **`Sched.slots`** → `Box<[Option<Box<Tcb>>]>` built in `thread_init::init_bootstrap` (runs after `heap ok`). `places` and the per-CPU `ReadyQueue.buf` likewise. `ThreadId` stays `u32`.
4. **Wake inbox:** the cross-CPU wake inbox (`src/ipi.rs:11-18`, `inbox_push` at `src/ipi_init.rs:174`) is a `u64` bitset indexed by `ThreadId`, so ids of 64 or more are dropped silently once `MAX_THREADS` goes. Replace it with a per-CPU bitmap of `[AtomicU64; threads/64]` sized from `Limits`, with one summary bit per word (DESIGN §7.6). Update the DESIGN §7 inbox text in the same commit.
5. **KVA free-list pool:** the node pool (`MAX_RANGES = 128` at `src/kva.rs:20`, `expect("kva: free-list")` in `src/kva_init.rs`) is sized from `limits.threads`, or grows, so that out-of-order exits of many thread stacks cannot exhaust it.
6. **`file_init::FILES`** disappears into `Vfs::files` (A3 step 6); if A3 has not landed, convert it the same way.
7. **`shell::Registry.cmds`** → `Box<[Option<Command>]>` built in `shell_init::init`; `MAX_COMMANDS` becomes `limits.commands`.
8. **Per-process tables:** the `FdTable` in `src/proc.rs` (`slots`, line 111) becomes `Box<[Fd]>` sized `limits.fds`; `procs` in `src/proc_init.rs` is sized `limits.procs` and `AddressSpace.regions` (`src/addr_space.rs:108`) `limits.regions`. Fold the `src/fs/mod.rs` `FdTable` into the `src/proc.rs` one, or delete it under A3.
9. **Diagnostics:** `meminfo` prints table sizes and occupancy so the caps are visible at runtime.
10. **ROADMAP:** the Phase 10 exit gate states the limits Phase 12 is tested against (landed: 256 processes, 256 descriptors per process, 1024 threads, 256 regions per address space).
11. **vibefs limits:** leave for a format v2; add a line to `docs/VIBEFS.md §13` stating the v1 caps and that they are format limits, not kernel limits.

## Acceptance criteria

- `grep -rn 'MAX_THREADS\|MAX_PROCS\|MAX_REGIONS\|MAX_INODES\|MAX_DENTRIES\|MAX_FILES\|MAX_FDS\|MAX_MOUNTS\|MAX_OPEN' src --include='*.rs' | grep -v limits.rs | grep -v vibefs` is empty.
- New ktest `limits_heap_backed` fills each table to its `Limits::DEFAULT` value under the default harness guest (TCG, 128M): threads alive at once up to the limit, 256 processes, 256 distinct files open in one process, 256 regions in one address space; it asserts `NoSpace` only one past each limit. `spawn_exit_thousands` unchanged.
- A cross-CPU wake of a thread whose id is 64 or more is delivered.
- Host `fs` eviction tests pass with `Limits::SMALL`.

## Tests

The ktest above; host tests parameterised by `Limits`.

## Risks and rollback

Init-order: every table must be built after `heap ok` and before first use (`thread_init::init_bootstrap` already is). A `BootCell::get` panic at boot is the failure mode, visible on serial. Revert per table.

## Out of scope

A slab allocator (§19.9); vibefs v2.
