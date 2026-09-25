# vibeOS Makefile. Hybrid BIOS+UEFI ISO, QEMU, and the test ladder.
#
# `make help` is the live target inventory (DESIGN §8.5).

TARGET := x86_64-unknown-none
CARGO  := cargo
export CARGO_TARGET_DIR := $(CURDIR)/target
# Host triple for vibeos-core tests and mkfs/fsck. Parent cargo config
# defaults to $(TARGET), so host recipes pass --target $(HOST_TRIPLE).
HOST_TRIPLE := $(shell rustc -vV | sed -n 's/^host: //p')
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

# Prerequisites: everything under src/, the linker script, the limine config,
# and this Makefile. A find(1) so newly added source dirs are not silently
# missed (DESIGN §9.1).
KERNEL_SRCS := $(shell find src -type f \( -name '*.rs' -o -name '*.asm' -o -name '*.S' \) 2>/dev/null)
USER_HELLO  := user/hello
USER_INIT   := user/init
USER_SH     := user/sh
USER_TESTS  := user/tests
INITRD := $(CURDIR)/build/initrd.fat
KERNEL_DEPS := $(KERNEL_SRCS) Cargo.toml crates/core/Cargo.toml build.rs linker.ld Makefile rust-toolchain.toml \
	scripts/gen_ksyms.py scripts/mkuserelf.py scripts/mkiso.sh \
	user/hello.asm user/init.asm user/sh.asm user/tests.asm user/sys.inc $(INITRD) \
	.cargo/config.toml Cargo.lock

LLVM_TOOL_DIR := $(shell rustc --print sysroot)/lib/rustlib/$(shell rustc -vV | sed -n 's/^host: //p')/bin
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
$(4): $(2)/$(TARGET)/$(PROFILE_DIR)/vibeos limine.conf $(LIMINE_BIN)
	LIMINE_DIR=$$(LIMINE_DIR) scripts/mkiso.sh $$< $$@ build/iso_root_$(1)
endef

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

KERNEL_ELF := $(CURDIR)/target/$(TARGET)/$(PROFILE_DIR)/vibeos
KERNEL_TESTS_DIR := $(CURDIR)/target-kernel-tests
KERNEL_VIBEFS_CRASH_DIR := $(CURDIR)/target-vibefs-crash

.PHONY: help check all kernel iso run run-panic clean distclean setup layout \
        test-unit test-harness test-e2e test-e2e-panic test-e2e-gp test \
        test-e2e-pit test-e2e-highmem test-ps2 test-kernel test-kernel-smp4 test-lapic-fallback \
        test-smp-stress test-vibefs-crash test-e2e-uefi

help:
	@printf '%s\n' \
	  'vibeOS make targets:' \
	  '  check                 fast local gate (clippy/unit/harness/python)' \
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
	  '  test-e2e-pit          PIT calibration fallback' \
	  '  test-e2e-highmem      boot contract with 9 GiB, past the physmap cap' \
	  '  test-ps2              QEMU sendkey echo (also part of test-e2e)' \
	  '  test-kernel           in-guest tests, -smp 2' \
	  '  test-kernel-smp4      in-guest tests, -smp 4' \
	  '  test-lapic-fallback   in-guest tests, TSC-deadline off' \
	  '  test-vibefs-crash     QEMU-kill + host fsck-vibefs' \
	  '  test-smp-stress       -smp 4, longer timeout (scheduled CI)' \
	  '  test                  all of the above except test-smp-stress and test-ps2' \
	  '  clean / distclean     build products; distclean also drops limine/'

# Fast local / CI `check` job gate (T3). It lints the kernel with its default
# features and vibeos-core's no_std build for the kernel target, so kernel-target
# code compiles before every commit; CI's ladder lints each other ISO feature
# set and kernel_shell (ROADMAP §10.1, F147).
# Guard scripts (scripts/check_*.py) run when present (A4, Q5, A1).
check:
	cargo fmt --check --all
	cargo clippy -p vibeos-core --all-targets --features std --target $(HOST_TRIPLE) -- -D warnings
	cargo clippy -p vibeos-hostlib-tests --all-targets --target $(HOST_TRIPLE) -- -D warnings
	cargo clippy -p vibeos-core --target $(TARGET) -- -D warnings
	cargo clippy --bin vibeos -- -D warnings
	$(MAKE) test-unit
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
	@set +e; \
	for s in scripts/check_*.py; do \
	    if [ -f "$$s" ]; then \
	        python3 "$$s" || exit 1; \
	    fi; \
	done
	@echo "check: ok"

all: $(ISO)

kernel: $(KERNEL_ELF)

$(LIMINE_BIN):
	@echo "limine binaries missing; run ./setup.sh" >&2
	@exit 1

$(INITRD): src/fat.rs tests/hostlib/src/bin/mkinitrd.rs tests/hostlib/Cargo.toml \
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
	cargo test -p vibeos-core --lib --features std --target $(HOST_TRIPLE)

test-harness:
	python3 -m unittest discover -s tests/harness -t . -v

test-e2e: $(ISO)
	VIBEOS_ISO=$(ISO) python3 tests/harness/run_e2e.py

# Focused #66 check: COM1 echo then QEMU `sendkey` (same i8042 as the
# window). Already part of `test-e2e`; not a second boot in `make test`.
test-ps2: $(ISO)
	VIBEOS_ISO=$(ISO) python3 tests/harness/run_ps2.py

# UEFI path via OVMF. Skipped if OVMF is not installed.
OVMF ?= /usr/share/ovmf/OVMF.fd
test-e2e-uefi: $(ISO)
	@if [ ! -f "$(OVMF)" ]; then \
	    echo "test-e2e-uefi: OVMF not found at $(OVMF); skipping"; \
	    exit 0; \
	fi
	VIBEOS_ISO=$(ISO) VIBEOS_BIOS=$(OVMF) python3 tests/harness/run_e2e.py

test-e2e-panic: $(ISO_PANIC)
	VIBEOS_ISO=$(ISO_PANIC) VIBEOS_EXPECT_PANIC=1 python3 tests/harness/run_e2e.py

test-e2e-gp: $(ISO_GP)
	VIBEOS_ISO=$(ISO_GP) VIBEOS_GP_TEST=1 python3 tests/harness/run_e2e.py

# PIT channel 2 calibration: HPET emulation off (`-machine pc,hpet=off`).
# Same ISO, same markers except the diagnostic names `pit` instead of `hpet`.
test-e2e-pit: $(ISO)
	VIBEOS_ISO=$(ISO) VIBEOS_EXPECT_PIT=1 python3 tests/harness/run_e2e.py

# RAM past the 8 GiB physmap cap (DESIGN §4.1) must stay out of the buddy.
test-e2e-highmem: $(ISO)
	VIBEOS_ISO=$(ISO) VIBEOS_MEM=9G python3 tests/harness/run_e2e.py

test-kernel: $(ISO_KTEST)
	VIBEOS_ISO=$(ISO_KTEST) python3 tests/harness/run_ktest.py

test-kernel-smp4: $(ISO_KTEST)
	VIBEOS_ISO=$(ISO_KTEST) VIBEOS_SMP=4 python3 tests/harness/run_ktest.py

test-lapic-fallback: $(ISO_KTEST)
	VIBEOS_ISO=$(ISO_KTEST) VIBEOS_QEMU_CPU=qemu64,-tsc-deadline python3 tests/harness/run_ktest.py

# Host mkfs/fsck share src/vibefs.rs. Artifacts land under
# $(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/ (A2).
MKFS_VIBEFS := $(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/debug/mkfs-vibefs
FSCK_VIBEFS := $(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/debug/fsck-vibefs

$(MKFS_VIBEFS) $(FSCK_VIBEFS): src/vibefs.rs tests/hostlib/src/bin/mkfs_vibefs.rs \
		tests/hostlib/src/bin/fsck_vibefs.rs tests/hostlib/Cargo.toml crates/core/Cargo.toml
	cargo build -p vibeos-hostlib-tests --bins --target $(HOST_TRIPLE)

test-vibefs-crash: $(ISO_VIBEFS_CRASH) $(MKFS_VIBEFS) $(FSCK_VIBEFS)
	VIBEOS_ISO=$(ISO_VIBEFS_CRASH) VIBEOS_MKFS=$(MKFS_VIBEFS) VIBEOS_FSCK=$(FSCK_VIBEFS) python3 tests/harness/run_vibefs_crash.py

test: test-unit test-harness test-e2e test-e2e-uefi test-e2e-panic test-e2e-gp test-e2e-pit test-e2e-highmem test-kernel test-kernel-smp4 test-lapic-fallback test-vibefs-crash

# Longer high-CPU stress. Scheduled CI, not every push. ROADMAP §4.11.
test-smp-stress: $(ISO_KTEST)
	VIBEOS_ISO=$(ISO_KTEST) VIBEOS_SMP=4 VIBEOS_TIMEOUT=180 python3 tests/harness/run_ktest.py

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
