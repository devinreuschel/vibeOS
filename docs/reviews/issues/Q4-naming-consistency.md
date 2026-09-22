# Q4 · Naming and feature-flag consistency

| | |
|---|---|
| **Area** | 4.2 Code quality & consistency |
| **Impact / Effort / Phase** | Low / S / II |
| **Depends on** | — (feature rename); A1 (module naming) |
| **Blocks** | — |
| **Review** | [ARCHITECTURE_REVIEW.md §4.2](../ARCHITECTURE_REVIEW.md#42-code-quality--consistency) |

## Problem

- Cargo features mix styles: `panic-test` (8 `cfg` sites), `gp-test` (2) versus `kernel_tests` (83), `panic_exit` (3), `vibefs_crash` (4).
- `src/pic.rs` (constants, lib) and `src/arch/pic.rs` (driver, bin) share a name while every other pair uses `_init`; `src/arch/` holds only `gdt`, `idt`, `pic`, `catch` while `apic`, `acpi`, `smp`, `x86` sit at the root.
- `src/serial.rs` defines `print!` / `println!` macros that have zero call sites.
- Markers are emitted through `serial::line(marker::X)` (40 sites) and logs through `klog!`, with the rule ("markers bypass the filter") implicit.

## Recommended fix

Underscore feature names everywhere; one pairing convention (settled by A1); delete the unused macros; make the marker rule explicit (E3).

## Implementation plan

1. **Feature rename.** `Cargo.toml`: `panic_test`, `gp_test`. Update `src/main.rs` `cfg`s, `Makefile` (`--features panic-test` → `panic_test`, `gp-test` → `gp_test`), `.github/workflows/ci.yml` step names, `README.md`, `docs/DESIGN.md` §8, `.cursor` rules. One sed, one commit.
2. **Delete `print!` / `println!`** from `src/serial.rs` (no call sites).
3. **Module naming** waits for A1; in the meantime add a "Naming" paragraph to DESIGN §1.3: "`<name>.rs` is the portable half, `<name>_init.rs` the kernel half; `arch/` holds only what touches privileged CPU state."
4. **`pic` pair:** rename `src/arch/pic.rs` to `src/pic_init.rs` now (matches the convention) or leave until A1 moves it to `arch/x86_64/pic.rs` with the constants in `core::platform::pic`. Recommend waiting for A1 to avoid a double rename.

## Acceptance criteria

- `grep -rn 'feature = "[a-z]*-' src Cargo.toml Makefile .github` is empty.
- `grep -rn 'macro_rules! print' src` is empty.
- `make test` green (feature rename covers all five builds).

## Tests

None new.

## Risks and rollback

Trivial; a missed rename shows up as an unknown-feature error at build time.
