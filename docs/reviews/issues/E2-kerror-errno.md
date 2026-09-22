# E2 · One `KError` (errno-shaped) ahead of the syscall boundary

| | |
|---|---|
| **Area** | 4.4 Error handling, logging & observability |
| **Impact / Effort / Phase** | Medium / M / III |
| **Depends on** | A3 (file ops through `Vfs`) |
| **Blocks** | Phase 9.3 syscall ABI |
| **Review** | [ARCHITECTURE_REVIEW.md §4.4](../ARCHITECTURE_REVIEW.md#44-error-handling-logging--observability) |

## Problem

14 per-module error enums (`AcpiError`, `MapError`, `IrqError`, `DmaError`, `VirtioError`, `BlockError`, `PartError`, `FatError`, `vibefs::Error`, `FsError`, `ProbeError`, `ClaimError`, `IpiError`, `TokenError`) with no `From` conversions and no numeric mapping. `FsError` has 13 variants (`src/fs/mod.rs:59`) that already look like errno (`NotFound`, `Exists`, `NotDir`, `IsDir`, `Inval`, `NoSpace`, `Loop`, `NameTooLong`, `NotEmpty`, `Busy`, `Badf`, `NotSupp`, `Io`). `src/file_init.rs:351 err_line` prints names to the console. Phase 9.3 needs a negative errno for every path that can reach userspace.

## Recommended fix

A small `KError` in the portable crate with errno values, `From` impls from the enums that can reach userspace (`FsError`, `BlockError`, `MapError`, later `ProcError`), and `as_errno()`. Module enums stay for internal precision.

## Implementation plan

1. **`src/kerror.rs`** (lib):
   ```rust
   #[repr(i32)] #[derive(Clone, Copy, Debug, PartialEq, Eq)]
   pub enum KError { Perm = 1, NoEnt = 2, Io = 5, Badf = 9, Again = 11, NoMem = 12, Fault = 14, Busy = 16,
       Exist = 17, NotDir = 20, IsDir = 21, Inval = 22, NFile = 23, MFile = 24, NoSpc = 28, Range = 34,
       NameTooLong = 36, NotEmpty = 39, Loop = 40, NotSup = 95 }
   impl KError { pub const fn errno(self) -> i32 { self as i32 } pub const fn as_str(self) -> &'static str { … } }
   impl From<FsError> for KError { … }   // exhaustive match, no wildcard
   impl From<BlockError> for KError { … }
   impl From<MapError> for KError { … }
   ```
2. **Host test** `every_fs_error_maps`: iterate all `FsError` variants (add a `const ALL: [FsError; 13]`) and assert each maps to a distinct, non-zero errno; same for `BlockError`.
3. **File API** returns `Result<_, KError>` once A3 has made it thin; the shell's `err_line` prints `KError::as_str`.
4. **Syscall dispatch** (Phase 9.3) returns `Result<usize, KError>`; the entry stub converts to `-errno`. Add `KError` to the ROADMAP §9.3 task list.
5. **Docs:** a short table in DESIGN (new §11 "Errors") listing which module enums cross the user boundary.

## Acceptance criteria

- `KError` exists with the test above; `file_init` (or `fs::api`) signatures use it.
- No `match` on `FsError` outside `fs`/`kerror` to produce numbers.

## Tests

The mapping test; existing shell/file ktests.

## Risks and rollback

Purely additive until the File API signature changes; that change is mechanical.

## Out of scope

Signal-related errors (`EINTR`) until Phase 9.7.
