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
ISO_PANIC_NEST   := build/vibeos-panic-nest.iso
ISO_PANIC_STOP   := build/vibeos-panic-stop.iso
ISO_KTEST        := build/vibeos-ktest.iso
ISO_VIBEFS_CRASH := build/vibeos-vibefs-crash.iso

LIMINE_DIR := ./limine
LIMINE_BIN := $(LIMINE_DIR)/limine

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
	user/hello.asm user/init.asm user/sh.asm user/tests.asm user/sys.inc \
	.cargo/config.toml Cargo.lock
# What mkiso.sh's /LICENSES/ notices are generated from (ROADMAP §10.9); the
# crate graph comes from Cargo.lock, which the ELF already depends on.
NOTICES_DEPS := LICENSE setup.sh scripts/gen_notices.py scripts/check_provenance.py \
	$(wildcard third_party/limine/* third_party/limine/*/* third_party/crates/*/*)

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
# CARGO_INCREMENTAL=0: a shipped ELF is built whole, as CI builds it, so a
# local ELF inlines and lays out frames as CI's does; check_stack_sizes.py
# screens that ELF, and an incremental build's codegen units give some
# functions different frames (ROADMAP §10.2).
CARGO_SHIP = CARGO_INCREMENTAL=0 $(CARGO) -Ztrim-paths -Zunstable-options --config 'profile.$(CARGO_PROFILE).trim-paths="all"'

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
	$$(CARGO_SHIP) build $$(CARGO_FLAGS) $(2) --artifact-dir build/kernels/.vibeos-$(1)
	python3 scripts/gen_ksyms.py --nm "$$(NM)" build/kernels/.vibeos-$(1)/vibeos build/kernels/vibeos-$(1).ksyms.rs
	VIBEOS_KSYMS=$(CURDIR)/build/kernels/vibeos-$(1).ksyms.rs $$(CARGO_SHIP) build $$(CARGO_FLAGS) $(2) --artifact-dir build/kernels/.vibeos-$(1)
	python3 scripts/gen_ksyms.py --nm "$$(NM)" --check build/kernels/.vibeos-$(1)/vibeos build/kernels/vibeos-$(1).ksyms.rs
	python3 scripts/check_kernel_fp.py --objdump "$$(OBJDUMP)" build/kernels/.vibeos-$(1)/vibeos
	cp build/kernels/.vibeos-$(1)/vibeos $$@
$(3): build/kernels/vibeos-$(1).elf $(INITRD) limine.conf $(LIMINE_BIN) scripts/mkiso.sh scripts/iso_disk_id.py $(NOTICES_DEPS)
	LIMINE_DIR=$$(LIMINE_DIR) scripts/mkiso.sh $$< $(INITRD) $$@ build/iso_root_$(1)
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
$(eval $(call KERNEL_VARIANT,panic,--features panic_test,$(ISO_PANIC)))
# gp: deliberate #GP after IDT
$(eval $(call KERNEL_VARIANT,gp,--features gp_test,$(ISO_GP)))
# panic-nest: an `irq_nest` underflow after boot, dumped without a guard
$(eval $(call KERNEL_VARIANT,panic-nest,--features panic_nest_test,$(ISO_PANIC_NEST)))
# panic-stop: two CPUs panic at -smp 5; the dump stops the other three
$(eval $(call KERNEL_VARIANT,panic-stop,--features panic_stop_test,$(ISO_PANIC_STOP)))
# ktest: in-guest registry, never packaged as production
$(eval $(call KERNEL_VARIANT,ktest,--features kernel_tests,$(ISO_KTEST)))
# vibefs-crash: write-loop kernel for QEMU-kill fsck
$(eval $(call KERNEL_VARIANT,vibefs-crash,--features vibefs_crash,$(ISO_VIBEFS_CRASH)))

KERNEL_ELF := build/kernels/vibeos-default.elf

# The Rust user programs (ROADMAP §10.5, C-USERBINS): each user/src/bin/<name>.rs
# links as a static non-PIE ET_EXEC at 1 GiB for $(USER_TRIPLE), through rust-lld
# and no C compiler. Only this invocation passes the flags, through --config, so
# .cargo/config.toml needs no table for the triple and its [build] -D warnings
# still applies (F147); the list repeats -D warnings anyway. Opt-level comes from
# the profile override, since member manifests' profiles are ignored.
# check_user_elf.py reads each ELF before the strip, which drops .symtab.
USER_TRIPLE := x86_64-unknown-linux-musl
USER_BIN_NAMES := $(sort $(basename $(notdir $(wildcard user/src/bin/*.rs))))
VIBEOS_USER_BINS ?= $(USER_BIN_NAMES)
USER_OUT := $(CURDIR)/build/user
USER_STAMP := $(USER_OUT)/.stamp
USER_SRCS := $(shell find user/src user/mem -type f 2>/dev/null)
USER_ELF_DIR := $(CARGO_TARGET_DIR)/$(USER_TRIPLE)/$(if $(filter release,$(CARGO_PROFILE)),release,debug)
OBJCOPY := $(if $(wildcard $(LLVM_TOOL_DIR)/llvm-objcopy),$(LLVM_TOOL_DIR)/llvm-objcopy,llvm-objcopy)
USER_CARGO_CONFIG := \
	--config 'build.rustflags=["-D","warnings","-C","linker=rust-lld","-C","relocation-model=static","-C","link-self-contained=no","-C","link-arg=-zseparate-loadable-segments","-C","link-arg=--image-base=0x40000000","-C","panic=abort"]' \
	--config 'profile.$(CARGO_PROFILE).opt-level="z"'

.PHONY: user
user: $(USER_STAMP)

all: user

ifneq ($(VIBEOS_PREBUILT),1)
# The build is $(CARGO_SHIP)'s, so trim-paths keeps host paths out of the
# programs as out of the kernel (ROADMAP §10.2).
$(USER_STAMP): $(USER_SRCS) user/Cargo.toml user/mem/Cargo.toml Cargo.toml Cargo.lock rust-toolchain.toml scripts/check_user_elf.py Makefile
	$(CARGO) clippy -p vibeos-user -p vibeos-user-mem --target $(USER_TRIPLE) $(CARGO_FLAGS) $(USER_CARGO_CONFIG) -- -D warnings
	$(CARGO_SHIP) build -p vibeos-user --target $(USER_TRIPLE) $(CARGO_FLAGS) $(USER_CARGO_CONFIG)
	python3 scripts/check_user_elf.py $(addprefix $(USER_ELF_DIR)/,$(USER_BIN_NAMES))
	mkdir -p $(USER_OUT)
	$(foreach b,$(USER_BIN_NAMES),$(OBJCOPY) --strip-all $(USER_ELF_DIR)/$(b) $(USER_OUT)/$(b) &&) true
	touch $@

# kernel_tests kernels embed the programs VIBEOS_USER_BINS names (build.rs), so
# only the ktest ELF's build sees the two variables.
build/kernels/vibeos-ktest.elf: $(USER_STAMP)
build/kernels/vibeos-ktest.elf: export VIBEOS_USER_BINS := $(VIBEOS_USER_BINS)
build/kernels/vibeos-ktest.elf: export VIBEOS_USER_DIR := $(USER_OUT)
endif

.PHONY: help check check-python check-msrv all kernel iso isos release-artifacts repro ci-budget run run-panic debug clean distclean setup layout prebuilt \
        test-unit test-harness test-e2e test-e2e-panic test-e2e-panic-nest test-e2e-panic-stop test-e2e-gp test-e2e-mce test \
        test-e2e-pit test-e2e-highmem test-e2e-strace test-ps2 test-kernel test-kernel-smp4 test-lapic-fallback \
        test-smp-stress test-vibefs-crash test-vibefs-crash-plants test-e2e-uefi test-qmp

help:
	@printf '%s\n' \
	  'vibeOS make targets:' \
	  '  check                 fast local gate (clippy/unit/harness/python)' \
	  '  check-python          ruff and mypy (VIBEOS_ALLOW_MISSING_TOOLS=1 skips a missing one)' \
	  '  check-msrv            vibeos-core with its MSRV toolchain (rust-version), host and kernel target' \
	  '  all / iso             kernel + build/vibeos.iso (hybrid BIOS/UEFI)' \
	  '  kernel                kernel ELF only (build/kernels/vibeos-default.elf)' \
	  '  user                  Rust user programs, as build/user/<name> (ROADMAP §10.5)' \
	  '  isos                  every ISO variant, as build/vibeos*.iso' \
	  '  release-artifacts     v* release images (release profile) into OUT=<dir>' \
	  '  repro                 build this commit twice; fail unless byte-identical (REPRO_ARGS=--share-rustup)' \
	  '  ci-budget             scheduled lanes under 60% busy and tier medians under 60 s (ci-history)' \
	  '  run                   boot production ISO in a QEMU window, COM1 on the terminal (VIBEOS_* apply)' \
	  '  run-panic             boot panic-test ISO, no window, COM1 on the terminal' \
	  '  debug                 as run, halted with a gdb stub on :1234; then gdb -x scripts/vibeos.gdb' \
	  '  layout                objdump sections + __kernel_ symbols' \
	  '  test-unit             vibeos-core host tests (any host triple)' \
	  '  models-quick          loom models (loom_*) at 3 preemptions, ROADMAP §10.8' \
	  '  test-harness          python unit tests for the harness' \
	  '  test-e2e              boot contract on the production ISO' \
	  '  test-e2e-uefi         same, UEFI firmware from the probe on pflash; none installed: skip (fail under CI)' \
	  '  test-e2e-panic        panic-test dump contract' \
	  '  test-e2e-panic-nest   irq_nest underflow: one dump, no reentered' \
	  '  test-e2e-panic-stop   -smp 5: the dump stops every other CPU' \
	  '  test-e2e-gp           #GP dump+halt contract' \
	  '  test-e2e-mce          injected #MC dump+halt contract' \
	  '  test-e2e-pit          PIT calibration fallback' \
	  '  test-e2e-highmem      boot contract with 9 GiB, past the physmap cap' \
	  '  test-e2e-strace       vibeos.strace=1 via fw_cfg: cmdline echo + syscall trace' \
	  '  test-ps2              QEMU sendkey echo (also part of test-e2e)' \
	  '  test-qmp              QMP event streams re-recorded and compared; one guest core checked' \
	  '  test-kernel           in-guest tests, -smp 2' \
	  '  test-kernel-smp4      in-guest tests, -smp 4' \
	  '  test-lapic-fallback   in-guest tests, TSC-deadline off' \
	  '  test-vibefs-crash     QEMU-kill + host fsck-vibefs' \
	  '  test-smp-stress       -smp 4 in-guest tier (weekly CI)' \
	  '  test                  all of the above except test-smp-stress and test-ps2' \
	  '  test-vibefs-crash-plants  each vibeos.crash_plant= defect caught, then a clean round' \
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

# cargo-deny's version, which the check job in .github/workflows/ci.yml pins
# (ROADMAP §10.9); it names the version in a missing-tool hint.
CARGO_DENY_PIN := $(shell sed -n 's/^ *CARGO_DENY_VERSION: *//p' .github/workflows/ci.yml | head -n1)

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
	# The portable core as the kernel links it (no `std`), for the host, where no
	# port exists, so only type parameters reach one (ROADMAP §10.3).
	cargo build -p vibeos-core --lib --target $(HOST_TRIPLE)
	cargo clippy -p vibeos-core --target $(TARGET) -- -D warnings
	$(MAKE) check-msrv
	cargo clippy --bin vibeos -- -D warnings
	$(MAKE) test-unit
	cargo test -p vibeos-core --lib --features std --target $(HOST_TRIPLE) --config 'profile.test.debug-assertions=false' -- release_assert_
	$(MAKE) models-quick
	$(MAKE) test-harness
	$(MAKE) check-python
	$(CARGO) build --bin vibeos --profile hookcheck --config 'profile.hookcheck.inherits="dev"'
	python3 scripts/gen_syscalls.py --check
	# The default kernel ELF, whose .stack_sizes check_stack_sizes.py reads.
	$(MAKE) kernel
	@set +e; \
	for s in scripts/check_*.py; do \
	    if [ -f "$$s" ]; then \
	        python3 "$$s" || exit 1; \
	    fi; \
	done
	python3 scripts/doc_refs.py
	@if command -v cargo-deny >/dev/null 2>&1; then \
	    set -x; \
	    cargo deny --workspace check licenses bans sources; \
	else \
	    $(call missing_tool,cargo-deny,cargo deny check licenses bans sources,cargo install cargo-deny --locked --version $(CARGO_DENY_PIN)); \
	fi
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

# v* release images (ROADMAP §10.1, §10.2; BOOT.md §3.5): the production ISO in
# the release profile, copied to OUT. The ISO and named-ELF paths do not name
# the profile, so a newer dev build would be reused: remove, rebuild, verify
# that the image's kernel is the release link's output.
RELEASE_ELF := $(CARGO_TARGET_DIR)/$(TARGET)/release/vibeos
release-artifacts:
	@if [ -z "$(OUT)" ]; then echo "release-artifacts: set OUT=<dir>" >&2; exit 2; fi
	@if [ -n "$$(ls -A "$(OUT)" 2>/dev/null)" ]; then echo "release-artifacts: $(OUT) is not empty" >&2; exit 2; fi
	rm -f $(ISO) $(KERNEL_ELF)
	$(MAKE) CARGO_PROFILE=release $(ISO)
	mkdir -p "$(OUT)" && cp $(ISO) "$(OUT)/vibeos.iso"
	rm -f build/release-kernel.elf
	xorriso -osirrox on -indev "$(OUT)/vibeos.iso" -extract /boot/vibeos build/release-kernel.elf
	cmp build/release-kernel.elf $(RELEASE_ELF)

# Two clean builds of one commit, compared byte for byte (ROADMAP §10.2,
# DESIGN §3.6). REPRO_ARGS: see scripts/repro_build.py.
repro:
	python3 scripts/repro_build.py $(REPRO_ARGS)

# The scheduled share and the per-push tiers, from ci-history (ROADMAP
# §10.1, DESIGN §8.6): both modes run, and the target fails if either does.
ci-budget:
	python3 scripts/ci_history.py --budget; b=$$?; python3 scripts/ci_history.py --tiers; t=$$?; [ $$b -eq 0 ] && [ $$t -eq 0 ]

# QEMU starts through the harness launcher, which builds the drivers' argv
# from the same VIBEOS_* settings and defaults (DESIGN §8.4); the ISO is
# harness.default_iso's unless VIBEOS_ISO names another.
run: $(ISO)
	python3 tests/harness/run_interactive.py run

run-panic: $(ISO_PANIC)
	python3 tests/harness/run_interactive.py panic

# The initrd's programs, whose symbols `make debug` loads beside the kernel's.
DEBUG_USER_ELFS := $(USER_HELLO) $(USER_INIT) $(USER_SH) $(USER_TESTS)

# QEMU halted with a gdb stub on :1234 (`-s -S`); attach with
# `gdb -x scripts/vibeos.gdb` from this directory (DESIGN §8.4).
debug: $(ISO) $(KERNEL_ELF) $(DEBUG_USER_ELFS)
	python3 tests/harness/run_interactive.py debug --kernel-elf $(KERNEL_ELF) \
	    $(foreach e,$(DEBUG_USER_ELFS),--user-elf $(e))

layout: $(KERNEL_ELF)
	@echo "== sections =="
	@$(OBJDUMP) -h $(KERNEL_ELF)
	@echo "== exported symbols =="
	@$(NM) $(KERNEL_ELF) | grep __kernel_

test-unit:
	VIBEOS_TIER=$@ cargo test -p vibeos-core --lib --features std --target $(HOST_TRIPLE)
	VIBEOS_TIER=$@ cargo test -p vibeos-core --doc --features std --target $(HOST_TRIPLE)
	VIBEOS_TIER=$@ cargo test -p vibeos-hostlib-tests --target $(HOST_TRIPLE)

# Loom models (C-LOOM): every `loom_*` test, each variant a `_fails` test that
# passes only when loom finds its race. Own target dir: `--cfg loom` rebuilds
# everything.
.PHONY: models-quick
models-quick:
	CARGO_TARGET_DIR=$(CARGO_TARGET_DIR)/loom RUSTFLAGS="--cfg loom -D warnings" LOOM_MAX_PREEMPTIONS=3 cargo test -p vibeos-core --lib --features std --release --target $(HOST_TRIPLE) -- loom_ --test-threads=1

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
# drops, and holds paths relative to $(CURDIR). The named ELFs go too: a failed
# run's guest core keeps the ELF behind its ISO (ROADMAP §10.7).
PREBUILT_FILES = $(ISO) $(ISO_PANIC) $(ISO_GP) $(ISO_PANIC_NEST) $(ISO_PANIC_STOP) $(ISO_KTEST) $(ISO_VIBEFS_CRASH) \
	$(KERNEL_ELFS) $(MKFS_VIBEFS) $(FSCK_VIBEFS) $(NBD_CACHE) $(VIBEFS_CAT)

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

# The boot contract under UEFI: the firmware probe (harness.FIRMWARE_TABLE,
# VIBEOS_FW_X86_64) finds a code image and its variable-store template, which
# the harness boots from pflash. One shell line, so the probe's answer decides
# the run: 0 runs the harness and passes its status on; 1 (none installed)
# prints a skip and exits 0, or fails when CI is set; 2 (a probe error) fails.
test-e2e-uefi: $(ISO)
	@python3 tests/harness/run_interactive.py firmware x86_64; rc=$$?; \
	case $$rc in \
	    0) VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO) VIBEOS_BIOS=uefi python3 tests/harness/run_e2e.py ;; \
	    1) if [ -n "$$CI" ]; then \
	           echo "test-e2e-uefi: FAIL: no x86_64 UEFI firmware installed, and CI is set" >&2; \
	           exit 1; \
	       fi; \
	       echo "test-e2e-uefi: SKIP: no x86_64 UEFI firmware installed (apt: ovmf; Homebrew: qemu; or set VIBEOS_FW_X86_64)"; \
	       exit 0 ;; \
	    *) exit 2 ;; \
	esac

test-e2e-panic: $(ISO_PANIC)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_PANIC) VIBEOS_EXPECT_PANIC=1 python3 tests/harness/run_e2e.py

# An `irq_nest` underflow after a full boot: the dump writes through
# `write_owner` with no guard, so it prints once (ROADMAP §10.7, F071).
test-e2e-panic-nest: $(ISO_PANIC_NEST)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_PANIC_NEST) VIBEOS_PANIC_VARIANT=nest python3 tests/harness/run_e2e.py

# Two CPUs panic at once at -smp 5 while three others print, wait on a
# lock, and spin with IF=0: the dump owner stops each (ROADMAP §10.7, F135).
test-e2e-panic-stop: $(ISO_PANIC_STOP)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_PANIC_STOP) VIBEOS_SMP=5 VIBEOS_PANIC_VARIANT=stop python3 tests/harness/run_e2e.py

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

# Kernel command line through fw_cfg (BOOT.md §3.2): vibeos.strace=1 turns
# the syscall trace on; the echo shows limine.conf's words, then the harness's.
test-e2e-strace: $(ISO)
	VIBEOS_TIER=test-e2e-strace VIBEOS_ISO=$(ISO) VIBEOS_CMDLINE=vibeos.strace=1 python3 -c 'from tests.harness.run_e2e import strace_main; raise SystemExit(strace_main())'

# Power-off and restart through the `reboot` syscall (ROADMAP §10.5): one
# kernel_tests boot per opt-in row, each passing when the row's line prints
# and QEMU exits by itself with status 0.
test-e2e-power: $(ISO_KTEST)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_KTEST) python3 tests/harness/run_power.py

# The QMP event streams tests/harness/test_qmp.py replays, re-recorded on
# this QEMU and compared with tests/harness/fixtures/qmp/, then a guest core
# of the production ISO (DESIGN §8.3, ROADMAP §10.7).
test-qmp: $(ISO)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO) python3 tests/harness/run_qmp.py

test-kernel: $(ISO_KTEST)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_KTEST) python3 tests/harness/run_ktest.py --hpet-off

test-kernel-smp4: $(ISO_KTEST)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_KTEST) VIBEOS_SMP=4 python3 tests/harness/run_ktest.py

# The nightly KVM leg adds +invtsc, so its invariant-TSC check still applies
# (DESIGN §8.4).
LAPIC_FALLBACK_CPU ?= qemu64,-tsc-deadline
test-lapic-fallback: $(ISO_KTEST)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_KTEST) VIBEOS_QEMU_CPU=$(LAPIC_FALLBACK_CPU) python3 tests/harness/run_ktest.py

# Over the volatile-cache device (DESIGN §8.3): nbd-cache serves the disk,
# vibefs-cat reads /w from each image rebuilt from its trace.
test-vibefs-crash: $(ISO_VIBEFS_CRASH) $(MKFS_VIBEFS) $(FSCK_VIBEFS) $(NBD_CACHE) $(VIBEFS_CAT)
	cargo test -p vibeos-hostlib-tests --target $(HOST_TRIPLE)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_VIBEFS_CRASH) VIBEOS_MKFS=$(MKFS_VIBEFS) VIBEOS_FSCK=$(FSCK_VIBEFS) \
	    VIBEOS_NBD_CACHE=$(NBD_CACHE) VIBEOS_VIBEFS_CAT=$(VIBEFS_CAT) python3 tests/harness/run_vibefs_crash.py

# The crash test's planted defects (ROADMAP §10.2): each plant must fail a
# round, then an unplanted control round must pass. Its own tier keeps the
# planted failures out of test-vibefs-crash's results.
test-vibefs-crash-plants: $(ISO_VIBEFS_CRASH) $(MKFS_VIBEFS) $(FSCK_VIBEFS) $(NBD_CACHE) $(VIBEFS_CAT)
	cargo test -p vibeos-hostlib-tests --target $(HOST_TRIPLE)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_VIBEFS_CRASH) VIBEOS_MKFS=$(MKFS_VIBEFS) VIBEOS_FSCK=$(FSCK_VIBEFS) \
	    VIBEOS_NBD_CACHE=$(NBD_CACHE) VIBEOS_VIBEFS_CAT=$(VIBEFS_CAT) python3 tests/harness/run_vibefs_crash.py \
	    --plants leak,early_super

test: test-unit test-harness test-e2e test-e2e-uefi test-e2e-panic test-e2e-panic-nest test-e2e-panic-stop test-e2e-gp test-e2e-mce test-e2e-pit test-e2e-highmem test-e2e-strace test-e2e-power test-qmp test-kernel test-kernel-smp4 test-lapic-fallback test-vibefs-crash

# The -smp 4 in-guest tier, weekly in CI, not every push. ROADMAP §4.11.
test-smp-stress: $(ISO_KTEST)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_KTEST) VIBEOS_SMP=4 python3 tests/harness/run_ktest.py

# Phase exit gate (ROADMAP §10.9): the gate map's entries and the box rules.
.PHONY: gate
gate:
	@test -n "$(PHASE)" || { echo "gate: set PHASE=N" >&2; exit 2; }
	python3 scripts/gate.py --phase "$(PHASE)" $(if $(filter 1,$(RECORD)),--record) $(if $(COMMIT),--commit "$(COMMIT)")

# Keeps build/results/.
clean:
	rm -rf build/kernels build/iso_root_* $(ISOS) $(addsuffix .xorriso-version,$(ISOS)) \
	    $(INITRD) \
	    user/hello user/hello.bin user/init user/init.bin user/sh user/sh.bin \
	    user/tests user/tests.bin
	$(CARGO) clean

distclean: clean
	rm -rf $(LIMINE_DIR)
