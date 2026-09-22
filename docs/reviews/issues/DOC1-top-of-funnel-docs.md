# DOC1 · Fix the top of the documentation funnel: README, DESIGN header, module map, MIT LICENSE

| | |
|---|---|
| **Area** | 4.12 Documentation |
| **Impact / Effort / Phase** | High / S / I |
| **Depends on** | — |
| **Blocks** | — (agents read these first) |
| **Review** | [ARCHITECTURE_REVIEW.md §4.12](../ARCHITECTURE_REVIEW.md#412-documentation) |

## Problem

- `README.md` "Status" says "Phase 0 (Ignition) is mostly landed" and links to the Phase 0 checklist; eight phases have landed since.
- `docs/DESIGN.md:9` opens with "**The tree is new. Almost nothing described here exists as code yet.**"
- `docs/DESIGN.md §1.3` documents a layout (`src/mm/`, `src/arch/x86_64/`, `src/drivers/`, `src/ktest/`) that has never existed.
- No `LICENSE` file; `Cargo.toml` says `UNLICENSED`; README says "all rights reserved for now". The maintainer has chosen **MIT** (2026-09-22).

## Recommended fix

One documentation PR that makes the first three things an agent reads true, plus the license.

## Implementation plan

1. **`README.md`:**
   - Status: "Phases 0–8 landed (boot, memory, traps/ACPI/time, threads, SMP, console/log/shell, PCI/virtio, block storage, filesystems incl. vibefs). Phase 9 (user mode) in progress." Link to ROADMAP "The arc" table instead of Phase 0.
   - Quickstart: keep, add `make check` (after Q1) and a "macOS" line (`brew install qemu xorriso nasm python`; host tests supported after A2).
   - Stack: update if B2 lands (built-in target).
   - License section: "MIT, see LICENSE".
   - Keep the voice; it is good.
2. **`docs/DESIGN.md` header:** replace the bold sentence with "This file records decisions. `ROADMAP.md` records what has landed; as of Phase 8 nearly everything in §2–§10 exists as code. When code and this file disagree, one of them is a bug (§1.4)."
3. **`docs/DESIGN.md §1.3`:** replace the aspirational table with the real one: two columns (portable module / kernel module) per subsystem, listing today's `foo.rs` / `foo_init.rs` pairs, and a "Naming" paragraph. When A1 lands, replace it again with the directory map.
4. **`LICENSE`:** MIT text, "Copyright (c) 2026 Devin Reuschel". `Cargo.toml` and `tests/hostlib/Cargo.toml`: `license = "MIT"`. Note in the changelog "Added: MIT license".
5. **`.cursor/rules/general.mdc` / `AGENTS.md`:** point at the README status section (DOC3).

## Acceptance criteria

- `grep -n 'Phase 0 (Ignition) is mostly' README.md` empty; `grep -n 'Almost nothing described here exists' docs/DESIGN.md` empty.
- `LICENSE` exists; `grep -n 'license = "MIT"' Cargo.toml tests/hostlib/Cargo.toml` matches both.
- DESIGN §1.3 lists only files that exist (`scripts/check_module_map.py` from A1 can verify the interim table too).

## Tests

None; a doc PR.

## Risks and rollback

None.
