# B2 · Replace the custom target JSON with built-in `x86_64-unknown-none`

| | |
|---|---|
| **Area** | 4.10 Build |
| **Impact / Effort / Phase** | Medium / M / II |
| **Depends on** | C1 (pinned nightly) |
| **Blocks** | A2 step 5 (workspace), P1 |
| **Review** | [ARCHITECTURE_REVIEW.md §4.10](../ARCHITECTURE_REVIEW.md#410-build-cicd--release-process) |

## Problem

`x86_64-unknown-none-executable.json` matches `rustc --print target-spec-json --target x86_64-unknown-none` in `code-model` (kernel), `features` (`-mmx,-sse,…,+soft-float`), `disable-redzone`, `panic-strategy`, `linker` (`rust-lld`), `linker-flavor` (`gnu-lld`), `rustc-abi` (softfloat), `max-atomic-width`, `stack-probes`. The differences are `frame-pointer: always`, `relocation-model: static` / non-PIE, and `pre-link-args` (`--script=linker.ld`, `-z max-page-size=0x1000`, `-z norelro`), all expressible as rustflags or `build.rs` link args. The custom JSON is the sole reason for `-Zjson-target-spec`, `-Zbuild-std`, `-Zbuild-std-features`, the `.cargo/config.toml` vs `Makefile` split ("build-std here makes `tests/hostlib` compile a second `core`"), and the DESIGN §9.1 pitfall "build works from the repo root and fails from anywhere else" (relative `linker.ld` path). The maintainer confirms the custom target was the agents' choice, not a requirement.

## Recommended fix

Proceed: a one-day spike on a branch, then delete the JSON if the ISO boots identically on both firmware paths and the panic backtrace still symbolizes.

## Implementation plan

1. **Toolchain:** `rustup target add x86_64-unknown-none` (add to `setup.sh` and `rust-toolchain.toml` `targets = ["x86_64-unknown-none"]`).
2. **`.cargo/config.toml`:**
   ```toml
   [build]
   target = "x86_64-unknown-none"
   [target.x86_64-unknown-none]
   rustflags = ["-C", "force-frame-pointers=yes", "-C", "relocation-model=static",
                "-C", "link-arg=-zmax-page-size=0x1000", "-C", "link-arg=-znorelro"]
   ```
   (Keep `-D warnings` from Q1 here too.)
3. **Linker script via `build.rs`:** `println!("cargo:rustc-link-arg-bins=-T{}/linker.ld", manifest_dir)` — absolute path, so builds work from any directory (closes the §9.1 pitfall). Keep `rerun-if-changed=linker.ld`.
4. **Makefile:** `CARGO := cargo` (drop the three `-Z` flags); `TARGET := x86_64-unknown-none`; delete `TARGET_JSON` from `KERNEL_DEPS`.
5. **Verify layout:** `make layout` before and after; compare section addresses, PHDR flags, `__kernel_*` symbols, `.got` placement (DESIGN §3.4), and `readelf -h` type (must be `EXEC`, not `DYN`; if `DYN`, add `-C link-arg=-no-pie`).
6. **Verify behaviour:** `make test` (all variants), including `test-e2e-uefi` with OVMF and `test-e2e-panic` (needs `rust_begin_unwind` in the dump: frame pointers must be present; check `objdump -d | grep 'push rbp'` density or the backtrace depth).
7. **Verify `compiler_builtins` mem intrinsics:** the prebuilt `x86_64-unknown-none` `core` ships `memcpy`/`memset`; confirm no duplicate-symbol or missing-symbol link errors (this was the reason for `-Zbuild-std-features=compiler-builtins-mem`).
8. **Delete** `x86_64-unknown-none-executable.json`; update `README.md` (Stack), `docs/DESIGN.md §3.1`, `.cursor` rules/`AGENTS.md`, and the `.cargo/config.toml` comment about hostlib. `tests/hostlib/.cargo/config.toml` can now be deleted (A2 step 3).
9. **Fallback:** if `code-model=kernel` with static relocation needs the linker script's `KERNEL_VMA` and `rust-lld` complains about `--script` ordering, pass `-C link-args=--script=…` as the first link arg via `cargo:rustc-link-arg-bins`.

## Acceptance criteria

- No `-Z` flags in the Makefile except none; `grep -rn 'unknown-none-executable' . --exclude-dir=target` is empty.
- All five ISOs boot; full ladder green; panic dump shows symbolized frames.
- A kernel build from a subdirectory (`cd src && cargo build`) works.

## Tests

Full ladder; the layout diff kept in the PR.

## Risks and rollback

If the built-in target's PIE default cannot be overridden cleanly, keep the JSON and instead apply only step 3 (absolute linker path). Rollback is `git revert` of the spike PR.
