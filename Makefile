# vibeOS Makefile. Hybrid BIOS+UEFI ISO, QEMU, and the test ladder.
#
# `make help` is the live target inventory (DESIGN §8.5).

TARGET := x86_64-unknown-none
CARGO  := cargo
export CARGO_TARGET_DIR := $(CURDIR)/target
# Host triple for vibeos-core tests and mkfs/fsck. Parent cargo config
# defaults to $(TARGET), so host recipes pass --target $(HOST_TRIPLE).
# VIBEOS_PREBUILT=1: the `make test-*` tiers use the files `make prebuilt`
# packed (a CI tier job, DESIGN §8.6), so nothing here runs rustc or cargo
# to build them: build/prebuilt.mk records the triple they were built for,
# and the ISO and host-tool rules below are not defined, so a missing file
# fails with "No rule to make target".
ifeq ($(VIBEOS_PREBUILT),1)
include build/prebuilt.mk
else
HOST_TRIPLE := $(shell rustc -vV | sed -n 's/^host: //p')
endif
# Script-style Python runners (`python3 tests/harness/run_e2e.py`) need the
# repo root on sys.path so `from tests.harness.harness import` resolves.
export PYTHONPATH := $(CURDIR)
CARGO_PROFILE ?= dev
ifeq ($(CARGO_PROFILE),release)
CARGO_FLAGS   := --release
PROFILE_DIR   := release
else
CARGO_FLAGS   :=
PROFILE_DIR   := debug
endif

ISO              := vibeos.iso
ISO_PANIC        := vibeos-panic.iso
ISO_GP           := vibeos-gp.iso
ISO_KTEST        := vibeos-ktest.iso
ISO_VIBEFS_CRASH := vibeos-vibefs-crash.iso

LIMINE_DIR := ./limine
LIMINE_BIN := $(LIMINE_DIR)/limine

# QEMU config. `-smp 2` from day one, ROADMAP §0.5. VIBEOS_SMP / VIBEOS_QEMU_CPU
# override so a single Makefile covers the SMP and LAPIC fallback variants.
VIBEOS_SMP      ?= 2
VIBEOS_QEMU_CPU ?= max
VIBEOS_MEM      ?= 128M
# TCG by default: KVM on a loaded host makes PIT/sleep tests flake.
# Override with VIBEOS_QEMU_ACCEL=kvm (or empty for QEMU's default).
VIBEOS_QEMU_ACCEL ?= tcg

QEMU_BASE = qemu-system-x86_64 \
    -cdrom $(ISO) \
    -m $(VIBEOS_MEM) \
    -smp $(VIBEOS_SMP) \
    -cpu $(VIBEOS_QEMU_CPU) \
    -accel $(VIBEOS_QEMU_ACCEL) \
    -no-reboot

# Prerequisites: everything under src/ and crates/core/src/, the linker
# script, the limine config, and this Makefile. A find(1) so newly added
# source dirs are not silently missed (DESIGN §9.1).
KERNEL_SRCS := $(shell find src crates/core/src -type f \( -name '*.rs' -o -name '*.asm' -o -name '*.S' \) 2>/dev/null)
USER_HELLO  := user/hello
USER_INIT   := user/init
USER_SH     := user/sh
USER_TESTS  := user/tests
INITRD := $(CURDIR)/build/initrd.fat
KERNEL_DEPS := $(KERNEL_SRCS) Cargo.toml crates/core/Cargo.toml build.rs linker.ld Makefile rust-toolchain.toml \
	scripts/gen_ksyms.py scripts/mkuserelf.py scripts/mkiso.sh \
	user/hello.asm user/init.asm user/sh.asm user/tests.asm user/sys.inc $(INITRD) \
	.cargo/config.toml Cargo.lock

ifneq ($(VIBEOS_PREBUILT),1)
LLVM_TOOL_DIR := $(shell rustc --print sysroot)/lib/rustlib/$(shell rustc -vV | sed -n 's/^host: //p')/bin
endif
OBJDUMP := $(if $(wildcard $(LLVM_TOOL_DIR)/llvm-objdump),$(LLVM_TOOL_DIR)/llvm-objdump,llvm-objdump)
NM      := $(if $(wildcard $(LLVM_TOOL_DIR)/llvm-nm),$(LLVM_TOOL_DIR)/llvm-nm,llvm-nm)

# Two-pass ksyms: first link has an empty table in .rodata, nm fills it,
# and the second link must not move .text (DESIGN §2.5; not yet enforced,
# ROADMAP §10.2, F084).
# $(1)=variant name  $(2)=target dir  $(3)=feature flags  $(4)=iso file
# Feature flags use repeated --features, never commas (those split $(call)).
# $$ so $(CARGO) is expanded when the recipe runs, not at $(eval) time.
define KERNEL_VARIANT
$(2)/$(TARGET)/$(PROFILE_DIR)/vibeos: $(KERNEL_DEPS)
	VIBEOS_INITRD=$(INITRD) CARGO_TARGET_DIR=$(2) $$(CARGO) build $$(CARGO_FLAGS) $(3)
	python3 scripts/gen_ksyms.py --nm "$$(NM)" $$@ $(2)/vibeos-ksyms.rs
	VIBEOS_INITRD=$(INITRD) VIBEOS_KSYMS=$(2)/vibeos-ksyms.rs CARGO_TARGET_DIR=$(2) $$(CARGO) build $$(CARGO_FLAGS) $(3)
	python3 scripts/check_kernel_fp.py --objdump "$$(OBJDUMP)" $$@ || { rm -f $$@; exit 1; }
$(4): $(2)/$(TARGET)/$(PROFILE_DIR)/vibeos limine.conf $(LIMINE_BIN)
	LIMINE_DIR=$$(LIMINE_DIR) scripts/mkiso.sh $$< $$@ build/iso_root_$(1)
endef

ifneq ($(VIBEOS_PREBUILT),1)
# prod: no extra features
$(eval $(call KERNEL_VARIANT,prod,$(CURDIR)/target,,$(ISO)))
# panic: deliberate panic-test dump
$(eval $(call KERNEL_VARIANT,panic,$(CURDIR)/target-panic,--features panic_test --features panic_exit,$(ISO_PANIC)))
# gp: deliberate #GP after IDT
$(eval $(call KERNEL_VARIANT,gp,$(CURDIR)/target-gp,--features gp_test --features panic_exit,$(ISO_GP)))
# ktest: in-guest registry; own dir so it cannot leak into production
$(eval $(call KERNEL_VARIANT,ktest,$(CURDIR)/target-kernel-tests,--features kernel_tests,$(ISO_KTEST)))
# vibefs_crash: write-loop kernel for QEMU-kill fsck
$(eval $(call KERNEL_VARIANT,vibefs_crash,$(CURDIR)/target-vibefs-crash,--features vibefs_crash,$(ISO_VIBEFS_CRASH)))
endif

KERNEL_ELF := $(CURDIR)/target/$(TARGET)/$(PROFILE_DIR)/vibeos
KERNEL_TESTS_DIR := $(CURDIR)/target-kernel-tests
KERNEL_VIBEFS_CRASH_DIR := $(CURDIR)/target-vibefs-crash

.PHONY: help check check-python check-msrv all kernel iso run run-panic clean distclean setup layout prebuilt \
        test-unit test-harness test-e2e test-e2e-panic test-e2e-gp test-e2e-mce test \
        test-e2e-pit test-e2e-highmem test-ps2 test-kernel test-kernel-smp4 test-lapic-fallback \
        test-smp-stress test-vibefs-crash test-e2e-uefi

help:
	@printf '%s\n' \
	  'vibeOS make targets:' \
	  '  check                 fast local gate (clippy/unit/harness/python)' \
	  '  check-python          ruff and mypy (VIBEOS_ALLOW_MISSING_TOOLS=1 skips a missing one)' \
	  '  check-msrv            vibeos-core with its MSRV toolchain (rust-version), host and kernel target' \
	  '  all / iso             kernel + vibeos.iso (hybrid BIOS/UEFI)' \
	  '  kernel                kernel ELF only' \
	  '  run                   boot production ISO in QEMU' \
	  '  run-panic             boot panic-test ISO' \
	  '  layout                objdump sections + __kernel_ symbols' \
	  '  test-unit             vibeos-core host tests (any host triple)' \
	  '  test-harness          python unit tests for the harness' \
	  '  test-e2e              boot contract on the production ISO' \
	  '  test-e2e-uefi         same, OVMF (prints a skip, then fails, if missing)' \
	  '  test-e2e-panic        panic-test dump contract' \
	  '  test-e2e-gp           #GP dump+halt contract' \
	  '  test-e2e-mce          injected #MC dump+halt contract' \
	  '  test-e2e-pit          PIT calibration fallback' \
	  '  test-e2e-highmem      boot contract with 9 GiB, past the physmap cap' \
	  '  test-ps2              QEMU sendkey echo (also part of test-e2e)' \
	  '  test-kernel           in-guest tests, -smp 2' \
	  '  test-kernel-smp4      in-guest tests, -smp 4' \
	  '  test-lapic-fallback   in-guest tests, TSC-deadline off' \
	  '  test-vibefs-crash     QEMU-kill + host fsck-vibefs' \
	  '  test-smp-stress       -smp 4, longer timeout (scheduled CI)' \
	  '  test                  all of the above except test-smp-stress and test-ps2' \
	  '  gate PHASE=N          phase exit gate: gate-map entries and box rules (RECORD=1: dev-host records)' \
	  '  prebuilt              every ISO and host tool a tier uses, as build/prebuilt.tar;' \
	  '                        VIBEOS_PREBUILT=1 make test-* then uses them (CI tier jobs)' \
	  '  clean / distclean     build products; distclean also drops limine/'

# VIBEOS_ALLOW_MISSING_TOOLS=1 is a gate switch (AGENTS.md How to run), not a
# QEMU override: `make check` and the FAT host tests fail when a tool they
# need is missing, unless it is set, when each skips that check and prints it.
# Exported so a command-line setting reaches `cargo test`. CI never sets it.
VIBEOS_ALLOW_MISSING_TOOLS ?=
export VIBEOS_ALLOW_MISSING_TOOLS
RUFF ?= ruff
MYPY ?= mypy
# $(call missing_tool,<tool>,<check>,<install hint>): shell text for a missing
# <tool>. It prints the skip line under the switch; otherwise it names the tool
# and the hint on stderr and exits 1. Arguments contain no commas.
missing_tool = if [ "$(VIBEOS_ALLOW_MISSING_TOOLS)" = 1 ]; then \
	    echo "check: skipped $(strip $(2)): $(strip $(1)) not installed (VIBEOS_ALLOW_MISSING_TOOLS=1)"; \
	else \
	    echo "check: $(strip $(1)) not installed; $(strip $(3)), or set VIBEOS_ALLOW_MISSING_TOOLS=1 to skip $(strip $(2))" >&2; \
	    exit 1; \
	fi
# $(call run_py_tool,<tool>,<args>): run <tool> as a command, else as
# `python3 -m <tool>`, else call missing_tool.
run_py_tool = if command -v $(1) >/dev/null 2>&1; then \
	    $(1) $(2); \
	elif python3 -m $(1) --version >/dev/null 2>&1; then \
	    python3 -m $(1) $(2); \
	else \
	    $(call missing_tool,$(1),$(strip $(1) $(2)),pip install the version the check job in .github/workflows/ci.yml pins); \
	fi

# vibeos-core's MSRV (ROADMAP §10.1, BOOT.md §3.1), read from its manifest;
# MSRV_TOOLCHAIN overrides the rustup toolchain check-msrv builds with.
MSRV := $(shell sed -n 's/^rust-version = "\(.*\)"$$/\1/p' crates/core/Cargo.toml)
MSRV_TOOLCHAIN ?= $(MSRV)

# Fast local / CI `check` job gate (T3). It lints the kernel with its default
# features and vibeos-core's no_std build for the kernel target, so kernel-target
# code compiles before every commit; CI's ladder lints each other ISO feature
# set and kernel_shell (ROADMAP §10.1, F147).
# Guard scripts (scripts/check_*.py) run when present (A4, Q5, A1). The
# `hookcheck` link gives check_test_hooks.py a production-feature ELF (Q2's
# nm check) without replacing the production kernel; it needs no initrd,
# whose bytes change no symbol name.
check:
	cargo fmt --check --all
	cargo clippy -p vibeos-core --all-targets --features std --target $(HOST_TRIPLE) -- -D warnings
	cargo clippy -p vibeos-hostlib-tests --all-targets --target $(HOST_TRIPLE) -- -D warnings
	cargo clippy -p vibeos-core --target $(TARGET) -- -D warnings
	$(MAKE) check-msrv
	cargo clippy --bin vibeos -- -D warnings
	$(MAKE) test-unit
	cargo test -p vibeos-core --lib --features std --target $(HOST_TRIPLE) --config 'profile.test.debug-assertions=false' -- release_assert_
	$(MAKE) test-harness
	$(MAKE) check-python
	$(CARGO) build --bin vibeos --profile hookcheck --config 'profile.hookcheck.inherits="dev"'
	@set +e; \
	for s in scripts/check_*.py; do \
	    if [ -f "$$s" ]; then \
	        python3 "$$s" || exit 1; \
	    fi; \
	done
	python3 scripts/doc_refs.py
	@echo "check: ok"

# ruff and mypy over tests/ and scripts/ (DX1, F147).
check-python:
	@$(call run_py_tool,$(RUFF),check tests scripts)
	@$(call run_py_tool,$(MYPY),)

# vibeos-core builds with its MSRV, for the host with std and for $(TARGET)
# without it. RUSTFLAGS replaces both .cargo/config.toml rustflags tables, so
# this proves only that the crate builds; clippy on the pinned nightly keeps
# the lints. RUSTUP_AUTO_INSTALL=0 and the toolchain check keep rustup from
# downloading a toolchain; the separate target dir keeps the nightly's cache.
check-msrv:
	$(if $(MSRV),,$(error crates/core/Cargo.toml sets no rust-version))
	@if command -v rustup >/dev/null 2>&1 \
	    && rustup toolchain list | cut -d' ' -f1 | grep -qxF '$(MSRV_TOOLCHAIN)-$(HOST_TRIPLE)' \
	    && rustup target list --installed --toolchain '$(MSRV_TOOLCHAIN)' 2>/dev/null | grep -qxF '$(TARGET)'; then \
	    set -ex; \
	    RUSTUP_AUTO_INSTALL=0 RUSTFLAGS=--cap-lints=warn CARGO_TARGET_DIR=$(CARGO_TARGET_DIR)/msrv \
	        cargo +$(MSRV_TOOLCHAIN) check -p vibeos-core --features std --target $(HOST_TRIPLE); \
	    RUSTUP_AUTO_INSTALL=0 RUSTFLAGS=--cap-lints=warn CARGO_TARGET_DIR=$(CARGO_TARGET_DIR)/msrv \
	        cargo +$(MSRV_TOOLCHAIN) check -p vibeos-core --target $(TARGET); \
	else \
	    $(call missing_tool,rust $(MSRV_TOOLCHAIN),cargo +$(MSRV_TOOLCHAIN) check -p vibeos-core,run ./setup.sh); \
	fi

all: $(ISO)

kernel: $(KERNEL_ELF)

$(LIMINE_BIN):
	@echo "limine binaries missing; run ./setup.sh" >&2
	@exit 1

$(INITRD): $(shell find crates/core/src/fs/fat -type f -name '*.rs') tests/hostlib/src/bin/mkinitrd.rs tests/hostlib/Cargo.toml \
		crates/core/Cargo.toml $(USER_HELLO) $(USER_INIT) $(USER_SH) $(USER_TESTS)
	mkdir -p $(dir $@)
	cargo run -p vibeos-hostlib-tests --bin mkinitrd --target $(HOST_TRIPLE) --quiet -- $(abspath $@) \
	    --add $(abspath $(USER_HELLO)):/hello \
	    --add $(abspath $(USER_INIT)):/sbin/init \
	    --add $(abspath $(USER_SH)):/bin/sh \
	    --add $(abspath $(USER_TESTS)):/bin/tests

user/%.bin: user/%.asm user/sys.inc
	nasm -f bin -I user/ -o $@ $<

user/hello: user/hello.bin scripts/mkuserelf.py
	python3 scripts/mkuserelf.py user/hello.bin $@

user/init: user/init.bin scripts/mkuserelf.py
	python3 scripts/mkuserelf.py user/init.bin $@

user/sh: user/sh.bin scripts/mkuserelf.py
	python3 scripts/mkuserelf.py user/sh.bin $@

user/tests: user/tests.bin scripts/mkuserelf.py
	python3 scripts/mkuserelf.py user/tests.bin $@

iso: $(ISO)

run: $(ISO)
	$(QEMU_BASE) -serial stdio

run-panic: $(ISO_PANIC)
	qemu-system-x86_64 -cdrom $(ISO_PANIC) -m $(VIBEOS_MEM) -smp $(VIBEOS_SMP) \
	    -cpu $(VIBEOS_QEMU_CPU) -accel $(VIBEOS_QEMU_ACCEL) -no-reboot -serial stdio -display none

layout: $(KERNEL_ELF)
	@echo "== sections =="
	@$(OBJDUMP) -h $(KERNEL_ELF)
	@echo "== exported symbols =="
	@$(NM) $(KERNEL_ELF) | grep __kernel_

test-unit:
	VIBEOS_TIER=$@ cargo test -p vibeos-core --lib --features std --target $(HOST_TRIPLE)
	VIBEOS_TIER=$@ cargo test -p vibeos-core --doc --features std --target $(HOST_TRIPLE)

test-harness:
	VIBEOS_TIER=$@ GITHUB_STEP_SUMMARY= python3 -m unittest discover -s tests/harness -t . -v

# Host mkfs/fsck share crates/core/src/fs/vibefs/. Artifacts land under
# $(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/ (A2). Defined above the tiers that
# name them: make expands a prerequisite list when it reads the rule.
MKFS_VIBEFS := $(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/debug/mkfs-vibefs
FSCK_VIBEFS := $(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/debug/fsck-vibefs
NBD_CACHE := $(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/debug/nbd-cache
VIBEFS_CAT := $(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/debug/vibefs-cat

ifneq ($(VIBEOS_PREBUILT),1)
$(MKFS_VIBEFS) $(FSCK_VIBEFS) $(NBD_CACHE) $(VIBEFS_CAT): $(shell find crates/core/src/fs/vibefs -type f -name '*.rs') \
		tests/hostlib/src/bin/mkfs_vibefs.rs tests/hostlib/src/bin/fsck_vibefs.rs \
		tests/hostlib/src/bin/nbd_cache.rs tests/hostlib/src/bin/vibefs_cat.rs \
		tests/hostlib/Cargo.toml crates/core/Cargo.toml
	cargo build -p vibeos-hostlib-tests --bins --target $(HOST_TRIPLE)
endif

# What a tier job downloads instead of building (DESIGN §8.6): every ISO and
# every host tool a `test-*` recipe lists. Recursive `=`, so it follows the
# variables' paths. The tar keeps the executable bit, which upload-artifact
# drops, and holds paths relative to $(CURDIR).
PREBUILT_FILES = $(ISO) $(ISO_PANIC) $(ISO_GP) $(ISO_KTEST) $(ISO_VIBEFS_CRASH) \
	$(MKFS_VIBEFS) $(FSCK_VIBEFS) $(NBD_CACHE) $(VIBEFS_CAT)

prebuilt: $(PREBUILT_FILES)
	mkdir -p build
	printf 'HOST_TRIPLE := %s\n' '$(HOST_TRIPLE)' > build/prebuilt.mk
	tar -cf build/prebuilt.tar build/prebuilt.mk $(patsubst $(CURDIR)/%,%,$(PREBUILT_FILES))

test-e2e: $(ISO) $(MKFS_VIBEFS)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO) VIBEOS_MKFS=$(MKFS_VIBEFS) python3 tests/harness/run_e2e.py

# Focused #66 check: COM1 echo then QEMU `sendkey` (same i8042 as the
# window). Already part of `test-e2e`; not a second boot in `make test`.
test-ps2: $(ISO)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO) python3 tests/harness/run_ps2.py

# UEFI path via OVMF. Skipped if OVMF is not installed.
OVMF ?= /usr/share/ovmf/OVMF.fd
test-e2e-uefi: $(ISO)
	@if [ ! -f "$(OVMF)" ]; then \
	    echo "test-e2e-uefi: OVMF not found at $(OVMF); skipping"; \
	    exit 0; \
	fi
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO) VIBEOS_BIOS=$(OVMF) python3 tests/harness/run_e2e.py

test-e2e-panic: $(ISO_PANIC)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_PANIC) VIBEOS_EXPECT_PANIC=1 python3 tests/harness/run_e2e.py

test-e2e-gp: $(ISO_GP)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_GP) VIBEOS_GP_TEST=1 python3 tests/harness/run_e2e.py

# Uncorrected machine check injected on CPU 0 through the QEMU monitor.
test-e2e-mce: $(ISO)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO) VIBEOS_MCE_TEST=1 python3 tests/harness/run_e2e.py

# PIT channel 2 calibration: HPET emulation off (`-machine pc,hpet=off`).
# Same ISO, same markers except the diagnostic names `pit` instead of `hpet`.
test-e2e-pit: $(ISO)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO) VIBEOS_EXPECT_PIT=1 python3 tests/harness/run_e2e.py

# RAM past the 8 GiB physmap cap (DESIGN §4.1) must stay out of the buddy.
test-e2e-highmem: $(ISO)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO) VIBEOS_MEM=9G python3 tests/harness/run_e2e.py

test-kernel: $(ISO_KTEST)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_KTEST) python3 tests/harness/run_ktest.py

test-kernel-smp4: $(ISO_KTEST)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_KTEST) VIBEOS_SMP=4 python3 tests/harness/run_ktest.py

test-lapic-fallback: $(ISO_KTEST)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_KTEST) VIBEOS_QEMU_CPU=qemu64,-tsc-deadline python3 tests/harness/run_ktest.py

# Over the volatile-cache device (DESIGN §8.3): nbd-cache serves the disk,
# vibefs-cat reads /w from each image rebuilt from its trace.
test-vibefs-crash: $(ISO_VIBEFS_CRASH) $(MKFS_VIBEFS) $(FSCK_VIBEFS) $(NBD_CACHE) $(VIBEFS_CAT)
	cargo test -p vibeos-hostlib-tests --target $(HOST_TRIPLE)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_VIBEFS_CRASH) VIBEOS_MKFS=$(MKFS_VIBEFS) VIBEOS_FSCK=$(FSCK_VIBEFS) \
	    VIBEOS_NBD_CACHE=$(NBD_CACHE) VIBEOS_VIBEFS_CAT=$(VIBEFS_CAT) python3 tests/harness/run_vibefs_crash.py

test: test-unit test-harness test-e2e test-e2e-uefi test-e2e-panic test-e2e-gp test-e2e-mce test-e2e-pit test-e2e-highmem test-kernel test-kernel-smp4 test-lapic-fallback test-vibefs-crash

# Longer high-CPU stress. Scheduled CI, not every push. ROADMAP §4.11.
test-smp-stress: $(ISO_KTEST)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_KTEST) VIBEOS_SMP=4 VIBEOS_TIMEOUT=180 python3 tests/harness/run_ktest.py

# Phase exit gate (ROADMAP §10.9): the gate map's entries and the box rules.
.PHONY: gate
gate:
	@test -n "$(PHASE)" || { echo "gate: set PHASE=N" >&2; exit 2; }
	python3 scripts/gate.py --phase "$(PHASE)" $(if $(filter 1,$(RECORD)),--record) $(if $(COMMIT),--commit "$(COMMIT)")

clean:
	rm -rf build/iso_root_* iso_root iso_root_panic iso_root_gp iso_root_ktest iso_root_vibefs_crash \
	    $(ISO) $(ISO_PANIC) $(ISO_GP) $(ISO_KTEST) $(ISO_VIBEFS_CRASH) \
	    target-panic target-gp $(KERNEL_TESTS_DIR) $(KERNEL_VIBEFS_CRASH_DIR) \
	    $(INITRD) initrd.fat \
	    user/hello user/hello.bin user/init user/init.bin user/sh user/sh.bin \
	    user/tests user/tests.bin
	$(CARGO) clean

distclean: clean
	rm -rf $(LIMINE_DIR)
