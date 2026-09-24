# B1 · Parametrize the Makefile's ISO recipes; add `make check`

**Status:** in the [index](README.md). #76, with DX1.

| | |
|---|---|
| **Area** | 4.10 Build |
| **Impact / Effort / Phase** | Medium / S / I |
| **Depends on** | — |
| **Blocks** | P1, T3, R1 (build products location) |
| **Review** | [ARCHITECTURE_REVIEW.md §4.10](../ARCHITECTURE_REVIEW.md#410-build-cicd--release-process) |

## Problem

`Makefile` contains five nearly identical ISO recipes (`$(ISO)`, `$(ISO_PANIC)`, `$(ISO_GP)`, `$(ISO_KTEST)`, `$(ISO_VIBEFS_CRASH)`: `rm -rf`, `mkdir`, six `cp`s, `xorriso`, `limine bios-install`) and five copies of the two-pass build + `gen_ksyms.py` sequence, differing only in target dir, feature list, and output name. A comment (line 57–59) notes `$(call …)` cannot wrap `$(CARGO)` because the flags contain commas. There is no fast local gate (`make check`).

## Recommended fix

One shell script for ISO assembly, one Make template for the two-pass build, variants declared as one-liners. Add `make check`.

## Implementation plan

1. **`scripts/mkiso.sh <kernel-elf> <out.iso> <staging-dir>`:** the `rm/mkdir/cp/xorriso/bios-install` sequence with `set -euo pipefail`; `LIMINE_DIR` from env with default `./limine`.
2. **Comma-safe Make template:**
   ```make
   comma := ,
   BUILD_STD := core$(comma)compiler_builtins$(comma)alloc
   CARGO = cargo -Zjson-target-spec -Zbuild-std=$(BUILD_STD) -Zbuild-std-features=compiler-builtins-mem
   # $(1)=variant name  $(2)=target dir  $(3)=feature flags  $(4)=iso file
   define KERNEL_VARIANT
   $(2)/$(TARGET)/$(PROFILE_DIR)/vibeos: $(KERNEL_DEPS)
   	CARGO_TARGET_DIR=$(2) $$(CARGO) build $$(CARGO_FLAGS) $(3)
   	python3 scripts/gen_ksyms.py --nm "$$(NM)" $$@ $(2)/vibeos-ksyms.rs
   	VIBEOS_KSYMS=$(2)/vibeos-ksyms.rs CARGO_TARGET_DIR=$(2) $$(CARGO) build $$(CARGO_FLAGS) $(3)
   $(4): $(2)/$(TARGET)/$(PROFILE_DIR)/vibeos limine.conf $(LIMINE_BIN)
   	scripts/mkiso.sh $$< $$@ build/iso_root_$(1)
   endef
   $(eval $(call KERNEL_VARIANT,prod,$(CURDIR)/target,,vibeos.iso))
   $(eval $(call KERNEL_VARIANT,panic,$(CURDIR)/target-panic,--features panic_test --features panic_exit,vibeos-panic.iso))
   $(eval $(call KERNEL_VARIANT,gp,$(CURDIR)/target-gp,--features gp_test --features panic_exit,vibeos-gp.iso))
   $(eval $(call KERNEL_VARIANT,ktest,$(CURDIR)/target-kernel-tests,--features kernel_tests,vibeos-ktest.iso))
   $(eval $(call KERNEL_VARIANT,vibefs_crash,$(CURDIR)/target-vibefs-crash,--features vibefs_crash,vibeos-vibefs-crash.iso))
   ```
   (Because `$(CARGO)` is referenced as `$$(CARGO)` inside `define`, the comma problem disappears.) After P1 the target-dir argument collapses to one value.
3. **`make check`:** `cargo fmt --check` (both packages), clippy (both), `make test-unit`, `make test-harness`, and the `scripts/check_*.py` guards as they arrive (A4, Q5, A1).
4. **`make help`:** print the target inventory (replaces DESIGN §8.5's hand-maintained list, which can then link to `make help`).
5. **Byte-for-byte check:** before merging, build all five ISOs with the old and new Makefile and compare `nm` of each kernel ELF and `xorriso -indev … -find` listings.

## Acceptance criteria

- `wc -l Makefile` roughly halves; `grep -c xorriso Makefile` = 0 (only in the script).
- Five ISOs identical in content to the pre-change build.
- `make check` runs in under a minute on a warm cache.

## Tests

The comparison in step 5; full ladder.

## Risks and rollback

`$(eval $(call …))` templates are harder to read than five copies for some; mitigate with the comment block above each variant line. Rollback is the previous Makefile.
