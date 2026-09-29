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
# Every time in the initrd and the ISOs (ROADMAP §10.2, F152): the caller's
# SOURCE_DATE_EPOCH, else the commit's time, else 2010-01-01.
SOURCE_DATE_EPOCH ?= $(or $(shell git log -1 --format=%ct 2>/dev/null),1262304000)
SOURCE_DATE_EPOCH := $(SOURCE_DATE_EPOCH)
export SOURCE_DATE_EPOCH
ifeq ($(CARGO_PROFILE),release)
CARGO_FLAGS   := --release
else
CARGO_FLAGS   :=
endif

# Every build product lives under build/ (ROADMAP §10.2): the named kernel
# ELFs in build/kernels/, the ISOs and their staging roots beside them.
ISO              := build/vibeos.iso
ISO_PANIC        := build/vibeos-panic.iso
ISO_GP           := build/vibeos-gp.iso
ISO_KTEST        := build/vibeos-ktest.iso
ISO_VIBEFS_CRASH := build/vibeos-vibefs-crash.iso

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
# Host tools and the initrd build from vibeos-core too, so they list every
# kernel source, the lockfile, the manifests and every hostlib binary
# (ROADMAP §10.2, F143).
HOSTLIB_DEPS := $(KERNEL_SRCS) Cargo.lock Cargo.toml crates/core/Cargo.toml tests/hostlib/Cargo.toml \
	$(wildcard tests/hostlib/src/bin/*.rs)
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

# The cargo that builds a shipped artifact: the kernel's two links. Cargo's
# trim-paths remaps the checkout, the sysroot and $CARGO_HOME out of panic
# Locations, DWARF and symbol names (ROADMAP §10.2, F151). It is unstable on
# the pinned nightly, so it goes on the command line: a manifest's
# `cargo-features` would stop every stable cargo reading the workspace.
# -Zunstable-options also enables --artifact-dir. Host tools do not ship.
CARGO_SHIP = $(CARGO) -Ztrim-paths -Zunstable-options --config 'profile.$(CARGO_PROFILE).trim-paths="all"'

# One kernel variant (DESIGN §8.2): every variant builds in the one target/,
# and --artifact-dir copies its ELF out under cargo's lock, so a parallel
# build of another variant cannot swap it. The recipe removes the named ELF
# first and writes it last, and the variant's ISO reads only that file, so a
# test build cannot be packaged as production.
# Two-pass ksyms: the first link has an empty table, nm fills it, and the
# second link must not move .text: --check regenerates the table from the
# final ELF and fails on any difference (DESIGN §2.5).
# $(1)=variant name  $(2)=feature flags  $(3)=iso file
# Feature flags use repeated --features, never commas (those split $(call)).
# $$ so $(CARGO_SHIP) is expanded when the recipe runs, not at $(eval) time.
# `cp`, not `mv`: cargo may hard-link the artifact to its own copy.
define KERNEL_VARIANT
KERNEL_ELFS += build/kernels/vibeos-$(1).elf
ISOS += $(3)
ifneq ($(VIBEOS_PREBUILT),1)
build/kernels/vibeos-$(1).elf: $(KERNEL_DEPS) $(PROFILE_STAMP)
	rm -rf $$@ build/kernels/vibeos-$(1).ksyms.rs build/kernels/.vibeos-$(1)
	VIBEOS_INITRD=$(INITRD) $$(CARGO_SHIP) build $$(CARGO_FLAGS) $(2) --artifact-dir build/kernels/.vibeos-$(1)
	python3 scripts/gen_ksyms.py --nm "$$(NM)" build/kernels/.vibeos-$(1)/vibeos build/kernels/vibeos-$(1).ksyms.rs
	VIBEOS_INITRD=$(INITRD) VIBEOS_KSYMS=$(CURDIR)/build/kernels/vibeos-$(1).ksyms.rs $$(CARGO_SHIP) build $$(CARGO_FLAGS) $(2) --artifact-dir build/kernels/.vibeos-$(1)
	python3 scripts/gen_ksyms.py --nm "$$(NM)" --check build/kernels/.vibeos-$(1)/vibeos build/kernels/vibeos-$(1).ksyms.rs
	python3 scripts/check_kernel_fp.py --objdump "$$(OBJDUMP)" build/kernels/.vibeos-$(1)/vibeos
	cp build/kernels/.vibeos-$(1)/vibeos $$@
$(3): build/kernels/vibeos-$(1).elf limine.conf $(LIMINE_BIN) scripts/mkiso.sh scripts/iso_disk_id.py
	LIMINE_DIR=$$(LIMINE_DIR) scripts/mkiso.sh $$< $$@ build/iso_root_$(1)
endif
endef

# The named ELFs do not name the profile, so each depends on a stamp that
# changes only when CARGO_PROFILE does: a release build after a dev build
# relinks instead of reusing the dev ELF.
PROFILE_STAMP := build/kernels/profile.stamp
$(PROFILE_STAMP): FORCE
	@mkdir -p $(dir $@)
	@[ "$$(cat $@ 2>/dev/null)" = "$(CARGO_PROFILE)" ] || echo "$(CARGO_PROFILE)" > $@

.PHONY: FORCE
FORCE:

KERNEL_ELFS :=
ISOS :=
# default: no extra features
$(eval $(call KERNEL_VARIANT,default,,$(ISO)))
# panic: deliberate panic-test dump
$(eval $(call KERNEL_VARIANT,panic,--features panic_test --features panic_exit,$(ISO_PANIC)))
# gp: deliberate #GP after IDT
$(eval $(call KERNEL_VARIANT,gp,--features gp_test --features panic_exit,$(ISO_GP)))
# ktest: in-guest registry, never packaged as production
$(eval $(call KERNEL_VARIANT,ktest,--features kernel_tests,$(ISO_KTEST)))
# vibefs-crash: write-loop kernel for QEMU-kill fsck
$(eval $(call KERNEL_VARIANT,vibefs-crash,--features vibefs_crash,$(ISO_VIBEFS_CRASH)))

KERNEL_ELF := build/kernels/vibeos-default.elf

.PHONY: help check all kernel iso isos run run-panic clean distclean setup layout prebuilt \
        test-unit test-harness test-e2e test-e2e-panic test-e2e-gp test-e2e-mce test \
        test-e2e-pit test-e2e-highmem test-ps2 test-kernel test-kernel-smp4 test-lapic-fallback \
        test-smp-stress test-vibefs-crash test-e2e-uefi

help:
	@printf '%s\n' \
	  'vibeOS make targets:' \
	  '  check                 fast local gate (clippy/unit/harness/python)' \
	  '  all / iso             kernel + build/vibeos.iso (hybrid BIOS/UEFI)' \
	  '  kernel                kernel ELF only (build/kernels/vibeos-default.elf)' \
	  '  isos                  every ISO variant, as build/vibeos*.iso' \
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
	cargo clippy --bin vibeos -- -D warnings
	$(MAKE) test-unit
	cargo test -p vibeos-core --lib --features std --target $(HOST_TRIPLE) --config 'profile.test.debug-assertions=false' -- release_assert_
	$(MAKE) test-harness
	@if command -v ruff >/dev/null 2>&1; then \
	    ruff check tests scripts; \
	elif python3 -m ruff --version >/dev/null 2>&1; then \
	    python3 -m ruff check tests scripts; \
	else \
	    echo "check: ruff not installed; pip install ruff"; \
	fi
	@if command -v mypy >/dev/null 2>&1; then \
	    mypy; \
	elif python3 -m mypy --version >/dev/null 2>&1; then \
	    python3 -m mypy; \
	else \
	    echo "check: mypy not installed; pip install mypy"; \
	fi
	$(CARGO) build --bin vibeos --profile hookcheck --config 'profile.hookcheck.inherits="dev"'
	@set +e; \
	for s in scripts/check_*.py; do \
	    if [ -f "$$s" ]; then \
	        python3 "$$s" || exit 1; \
	    fi; \
	done
	python3 scripts/doc_refs.py
	@echo "check: ok"

all: $(ISO)

kernel: $(KERNEL_ELF)

$(LIMINE_BIN):
	@echo "limine binaries missing; run ./setup.sh" >&2
	@exit 1

$(INITRD): $(HOSTLIB_DEPS) $(USER_HELLO) $(USER_INIT) $(USER_SH) $(USER_TESTS)
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

isos: $(ISOS)

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
	VIBEOS_TIER=$@ cargo test -p vibeos-hostlib-tests --target $(HOST_TRIPLE)

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
$(MKFS_VIBEFS) $(FSCK_VIBEFS) $(NBD_CACHE) $(VIBEFS_CAT): $(HOSTLIB_DEPS)
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

# Keeps build/results/ and a macOS build/OVMF.fd.
clean:
	rm -rf build/kernels build/iso_root_* $(ISOS) $(addsuffix .xorriso-version,$(ISOS)) \
	    $(INITRD) \
	    user/hello user/hello.bin user/init user/init.bin user/sh user/sh.bin \
	    user/tests user/tests.bin
	$(CARGO) clean

distclean: clean
	rm -rf $(LIMINE_DIR)
