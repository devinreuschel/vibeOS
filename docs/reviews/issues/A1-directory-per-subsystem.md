# A1 · Directory per subsystem, matching a corrected module map

| | |
|---|---|
| **Area** | 4.1 Architecture & module boundaries |
| **Impact / Effort / Phase** | High / L / III |
| **Depends on** | Q1 (format first so the move is a pure move), A2 (two crates give each half its own tree) |
| **Blocks** | Q5, D2, DOC2 (final module map) |
| **Review** | [ARCHITECTURE_REVIEW.md §4.1](../ARCHITECTURE_REVIEW.md#41-architecture--module-boundaries) |

## Problem

- 84 Rust files sit flat in `src/`, mostly as `foo.rs` / `foo_init.rs` pairs; `src/arch/` holds 4 files and `src/fs/` 2.
- `docs/DESIGN.md §1.3` documents a nested layout (`src/mm/pmm/`, `src/arch/x86_64/`, `src/interrupts/`, `src/drivers/`, `src/ktest/`) that has never existed. The map was written on 2026-08-01 (`4e51646`); the flat pairing began with the first code on 2026-09-16 (`1db35eb`, `d4d0b6f`).
- `src/x86.rs:1` says "real CPU state work lives under `src/arch/`" while holding CR3/MSR/CPUID/`lgdt` primitives.
- `src/file_init.rs` (1,516 lines) mixes the kernel File API, twelve shell commands (`cmd_ls` … `cmd_pwd`), and tab completion. `src/block_init.rs` (`cmd_blk`) and `src/dev_init.rs` also register shell commands.

## Decision

Keep the lib/bin pairing (the later, working convention; the maintainer leans that way) and nest it: one directory per subsystem in each crate, portable module and kernel counterpart sharing the subsystem name. Rewrite DESIGN §1.3 to describe exactly that.

## Target layout

After A2 has produced `crates/core` and `crates/kernel`. (The same directories could be created under the single `src/` first with both crate roots using `#[path]`, but doing it once after A2 is less churn.)

```
crates/core/src/                         crates/kernel/src/
  lib.rs  marker.rs  symtab.rs  fmt_util.rs   main.rs                (_start only)
  mm/{pmm,paging,heap,kva}.rs                 boot/mod.rs            (BootInfo, Limine requests; D3)
  platform/{acpi,apic,pci,virtio,virtio_blk,  arch/x86_64/{cpu,gdt,idt,pic,catch,switch}.rs + trampoline.asm
    dma,irq,vectors,desc,uart,pic,smp,        mm/{pmm,paging,heap,kva}.rs
    per_cpu,ipi}.rs                           interrupts/{irq,apic,ipi}.rs
  sched/{thread,sched,wait,sync,lock,work}.rs time/mod.rs
  time.rs  log.rs                             smp/{smp,per_cpu}.rs
  block/{block,part,cache}.rs                 sched/{thread,sched,sync,work}.rs
  fs/{mod,kernfs,fat,vibefs}.rs               log/{log,serial,panic,diag}.rs
  ui/{shell,console,kbd,fb,font}.rs           dev/{pci,dev,dma,virtio}.rs
                                              drivers/{virtio_blk,ramdisk,kbd,fb}.rs
                                              block/{block,part,cache}.rs
                                              fs/{fs,fat,vibefs,file}.rs
                                              shell/{mod,complete}.rs  shell/cmds/{fs,blk,dev,sys}.rs
                                              console/mod.rs
                                              ktest/{mod,mm,traps,...}.rs   (T1)
```

## Implementation plan

1. **Freeze the map.** Put the table above (adjusted after review) into DESIGN §1.3, marked "as of <date>", in the same PR as step 2. Every later step keeps it true.
2. **Root re-exports so paths keep working during the move.** In `core/src/lib.rs`: `pub mod mm; pub use mm::{pmm, paging, heap, kva};` and so on. Callers keep writing `vibeos::pmm::…` until the last step, so each PR stays small.
3. **Move one subsystem per PR with `git mv`** (history follows). Order: `mm` → `arch` → `interrupts`/`time`/`smp` → `sched`/`log` → `dev`/`drivers`/`block` → `fs` → `ui`/`shell`/`console`. In each PR: `git mv`, update `mod` declarations, `cargo fmt`, `make test`. No logic changes; the diff is paths only.
4. **Pull shell commands out of subsystem modules.** Move `cmd_*` functions from `file_init.rs` to `shell/cmds/fs.rs`, `block_init::cmd_blk` to `shell/cmds/blk.rs`, the `dev_init` commands to `shell/cmds/dev.rs`, and the builtins in `shell_init.rs` to `shell/cmds/sys.rs`. Add `shell::cmds::register_all()` called once from `shell::init()`. Tab completion (`file_init::{complete_line, vfs_complete_names, complete_cmd, common_prefix, apply_word}`) moves to `shell/complete.rs`. This also removes the `file_init ↔ shell_init` cycle (A4).
5. **Rename `x86.rs`** to `arch/x86_64/cpu.rs`; `arch/pic.rs` stays the driver and `core::platform::pic` the constants (Q4).
6. **Drop the root re-exports** in one final PR and fix the `use vibeos::…` lines with a sed map (put the map in the PR description).
7. **Update** `tests/hostlib` (if still `#[path]`-based), the `//!` headers that cite DESIGN sections, and `.cursor`/`AGENTS.md` paths.

## Acceptance criteria

- `find crates/*/src -maxdepth 1 -name '*.rs'` lists only crate roots and the three utility modules.
- DESIGN §1.3 equals the directory tree; `scripts/check_module_map.py` diffs them in `make check`.
- `grep -rn 'fn cmd_' crates/kernel/src --include='*.rs' | grep -v shell/cmds` is empty.
- All tiers green after every PR; host test count (367) and in-guest count (103) unchanged.

## Tests

No new behaviour; the existing tiers are the regression net. Add the module-map check script.

## Risks and rollback

Large path diffs conflict with concurrent Phase 9 work: merge each subsystem PR within a day and rebase in-flight branches (`git log --follow` still works). Rollback is `git revert` of one move PR.

## Out of scope

Splitting large files by responsibility (Q5); converting singletons to instances (D2).
