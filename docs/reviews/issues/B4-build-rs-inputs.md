# B4 · Simplify `build.rs` inputs and the initrd path

**Superseded in part 2026-09-24:** the acceptance's `nasm` grep; `nasm` stays for the assembly user programs until ROADMAP §10.5 deletes them.

| | |
|---|---|
| **Area** | 4.10 Build |
| **Impact / Effort / Phase** | Low / S–M / II |
| **Depends on** | B1 |
| **Blocks** | R1 (no build product in the root) |
| **Review** | [ARCHITECTURE_REVIEW.md §4.10](../ARCHITECTURE_REVIEW.md#410-build-cicd--release-process) |
| **Status** | In the [index](README.md). #89: hostlib `mkinitrd` → `build/initrd.fat`; `build.rs` copies `VIBEOS_INITRD`. Trampoline is `global_asm!` (`src/arch/trampoline.S`), byte-identical to the old nasm blob. `nasm` remains for `user/*.asm`. |

## Problem

- `build.rs` shells out to `nasm` (AP trampoline) and, if `initrd.fat` is not staged in the repo root, to `python3 scripts/mkinitrd.py`; the `Makefile` has its own `initrd.fat:` rule; and `src/fat.rs:1690 fat::mkinitrd` is a second, Rust implementation of the same image, used by `src/fat_init.rs:208` as a fallback. Three generators, two code paths.
- `initrd.fat` is both a build product and a listed prerequisite (`KERNEL_DEPS`) living at the repo root (gitignored).
- `src/trampoline.asm` is 90 lines of straightforward 16/32/64-bit code.

## Recommended fix

One initrd generator (the host-tested Rust one) producing into `build/`; `build.rs` only consumes staged inputs via env vars like it already does for ksyms; assemble the trampoline with `global_asm!` to drop `nasm`.

## Implementation plan

1. **hostlib bin `mkinitrd`:** `tests/hostlib/src/bin/mkinitrd.rs` calling `vibeos::fat::mkinitrd(&mut buf)` and writing the file. Makefile: `build/initrd.fat: src/fat.rs tests/hostlib/src/bin/mkinitrd.rs` → `cd tests/hostlib && cargo run --bin mkinitrd -- $(abspath $@)`.
2. **`build.rs`:** replace the copy-or-run logic with `VIBEOS_INITRD` (path, `rerun-if-env-changed` + `rerun-if-changed`), copying into `OUT_DIR/initrd.fat`; panic with a clear message if unset ("run `make`, not bare `cargo build`", or fall back to an empty 64 KiB image so `cargo check` still works). Makefile passes `VIBEOS_INITRD=$(CURDIR)/build/initrd.fat` alongside `VIBEOS_KSYMS`.
3. **Delete `scripts/mkinitrd.py`** and the Makefile `initrd.fat` rule; `.gitignore` drops `/initrd.fat`.
4. **Trampoline via `global_asm!`:** `src/arch/trampoline.rs` with `global_asm!(include_str!("trampoline.S"))` where `trampoline.S` is the GNU-as translation (`.code16`, `.code32`, `.code64`; `org` becomes a `.set BASE, 0x8000` and absolute operands; `times` padding becomes `.fill`). Place it in a dedicated section (`.trampoline`) and export `__trampoline_start/__trampoline_end` from `linker.ld`; `smp_init` copies that byte range to 0x8000 instead of `include_bytes!(OUT_DIR/trampoline.bin)`. Verify the blob is byte-identical to the nasm output on first landing (`cmp` the two), then remove `nasm` from `build.rs`, `setup.sh`, CI, and the cloud env.
5. **Docs:** DESIGN §7.3 (trampoline build), §3.1 (host tools), README quickstart.

## Acceptance criteria

- `grep -rn 'nasm\|mkinitrd.py' build.rs Makefile setup.sh .github .cursor` is empty.
- `ls` at the repo root shows no `initrd.fat`; `make` produces `build/initrd.fat`.
- `trampoline_page`, `failed_ap_cleanup`, and `-smp 4` ktests green; `fat_initrd` ktest green.

## Tests

The byte-compare step during migration; existing ktests.

## Risks and rollback

The asm translation is the only risky step; land it separately from the initrd change and keep the nasm path for one commit to compare outputs.
