# D1 · Fixed-capacity tables: heap-allocate the growable ones before Phase 9

| | |
|---|---|
| **Area** | 4.3 Data model & state management |
| **Impact / Effort / Phase** | High / L / III |
| **Depends on** | Q3 (`BootCell` for late init), A3 (VFS owns files/fds) |
| **Blocks** | Phase 9 exit gate (processes × fds) |
| **Review** | [ARCHITECTURE_REVIEW.md §4.3](../ARCHITECTURE_REVIEW.md#43-data-model--state-management) |

## Problem

- 53 `MAX_*` constants size static arrays. The growable ones: `MAX_THREADS = 64` (`src/thread.rs`; `Sched.slots: [Option<Box<Tcb>>; MAX_THREADS]` at `src/thread_init.rs:46`), `MAX_FILES = 16`, `MAX_FDS = 16`, `MAX_INODES = 48`, `MAX_DENTRIES = 48`, `MAX_MOUNTS = 8`, `MAX_RAM_NODES = 64`, `MAX_KERN_NODES = 128` (`src/fs/mod.rs:16–27`, arrays at lines 599–604), `MAX_OPEN = 16` (`src/file_init.rs:27`, `static FILES`), `MAX_COMMANDS = 48` (`src/shell.rs:421`).
- Hardware-bounded ones are fine as they are: `MAX_CPUS`, `MAX_IOAPICS`, `MAX_ISOS`, `MAX_BARS`, `MAX_DEVICES`, `MAX_VENDOR_CAPS`, `MAX_ORDER`.
- vibefs `MAX_BLOCKS = 1024` (4 MiB volumes) and `MAX_INODES = 64` are on-disk format v1 limits (`docs/VIBEFS.md`), a separate question.
- The heap exists since Phase 1 and `Vec`/`Box` are already used in `per_cpu_init`, `thread_init`, `virtio_blk_init`; the portable tables just predate it.

## Decision (maintainer deferred)

Keep static tables where bounded by hardware. For threads, inodes, dentries, files, fds, mounts, open files, and commands, allocate at init from the heap (`Box<[T]>`) sized from one `limits` module, so raising a cap is a constant change rather than a type change. Keep the clock-eviction logic unchanged. A real slab stays at ROADMAP §19.9. Add a Phase 9 gate line stating the limits Phase 9 is tested against.

## Implementation plan

1. **`src/limits.rs`** (lib): `pub struct Limits { pub threads: usize, pub inodes: usize, pub dentries: usize, pub files: usize, pub fds: usize, pub mounts: usize, pub ram_nodes: usize, pub kern_nodes: usize, pub commands: usize }` with `pub const DEFAULT: Limits` (threads 256, inodes 256, dentries 256, files 128, fds 64, mounts 16, …) and `pub const SMALL: Limits` for host tests (today's numbers, so existing eviction tests keep their meaning).
2. **`Vfs`**: change the six arrays to `Box<[T]>`; `Vfs::new(limits: &Limits) -> Self` is no longer `const`. `fs_init` builds it in `init()` (heap is up) into a `BootCell` (Q3). Host tests call `Vfs::new(&Limits::SMALL)`.
3. **`Sched.slots`** → `Box<[Option<Box<Tcb>>]>` built in `thread_init::init_bootstrap` (runs after `heap ok`). `places` likewise. `ThreadId` stays `u32`.
4. **`file_init::FILES`** disappears into `Vfs::files` (A3 step 6); if A3 has not landed, convert it the same way.
5. **`shell::Registry.cmds`** → `Box<[Option<Command>]>` built in `shell_init::init`; `MAX_COMMANDS` becomes `limits.commands`.
6. **`FdTable`** (`src/fs/mod.rs:554`): `fds: Box<[u16]>` sized `limits.fds`; one per process in Phase 9.
7. **Diagnostics:** `meminfo` prints table sizes and occupancy so the caps are visible at runtime.
8. **ROADMAP:** add to the Phase 9 exit gate: "runs with 256 threads, 128 open files, 64 fds per process; `spawn_exit_thousands` still passes".
9. **vibefs limits:** leave for a format v2; add a line to `docs/VIBEFS.md §13` stating the v1 caps and that they are format limits, not kernel limits.

## Acceptance criteria

- `grep -rn 'MAX_THREADS\|MAX_INODES\|MAX_DENTRIES\|MAX_FILES\|MAX_FDS\|MAX_MOUNTS\|MAX_OPEN' src --include='*.rs' | grep -v limits.rs | grep -v vibefs` is empty.
- New ktest `limits_heap_backed`: spawn 200 threads that exit, open 100 files, assert no `NoSpace`; `spawn_exit_thousands` unchanged.
- Host `fs` eviction tests pass with `Limits::SMALL`.

## Tests

The ktest above; host tests parameterised by `Limits`.

## Risks and rollback

Init-order: every table must be built after `heap ok` and before first use (`thread_init::init_bootstrap` already is). A `BootCell::get` panic at boot is the failure mode, visible on serial. Revert per table.

## Out of scope

A slab allocator (§19.9); vibefs v2.
