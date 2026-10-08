# vibeOS Makefile. Hybrid BIOS+UEFI ISO, QEMU, and the test ladder.
#
# `make help` is the live target inventory (DESIGN §8.5).

# ARCH= selects the kernel target. Default keeps every existing recipe,
# `make check` included, on x86_64. aarch64 is ROADMAP §11.3 / §11.7.
ARCH ?= x86_64
ifeq ($(ARCH),aarch64)
TARGET := aarch64-unknown-none-softfloat
else ifeq ($(ARCH),x86_64)
TARGET := x86_64-unknown-none
else
$(error ARCH must be x86_64 or aarch64)
endif
export VIBEOS_ARCH := $(ARCH)
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
ISO_IRQOFF       := build/vibeos-irqoff.iso
ISO_KTEST_IRQOFF := build/vibeos-ktest-irqoff.iso
ISO_INIT_FAULT   := build/vibeos-init-fault.iso
ISO_NOSH         := build/vibeos-nosh.iso
ISO_HANG         := build/vibeos-hang.iso

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
INITRD := $(CURDIR)/build/initrd.fat
# The production initrd with `/sbin/init` from `user/src/bin/init_fault.rs`,
# for `make test-e2e-init-fault` only (AGENTS.md rule 9).
INITRD_INIT_FAULT := $(CURDIR)/build/initrd-init_fault.fat
# The production initrd without `/bin/sh` and with `/bin/false` as
# `/bin/tests`, for `make test-e2e-init-fault`'s `init_no_sh` case only.
INITRD_NOSH := $(CURDIR)/build/initrd-nosh.fat
KERNEL_DEPS := $(KERNEL_SRCS) Cargo.toml crates/core/Cargo.toml build.rs linker.ld linker-aarch64.ld Makefile rust-toolchain.toml \
	scripts/gen_ksyms.py scripts/mkiso.sh \
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
	$$(CARGO_SHIP) build --target $$(TARGET) $$(CARGO_FLAGS) $(2) --artifact-dir build/kernels/.vibeos-$(1)
	python3 scripts/gen_ksyms.py --nm "$$(NM)" build/kernels/.vibeos-$(1)/vibeos build/kernels/vibeos-$(1).ksyms.rs
	VIBEOS_KSYMS=$(CURDIR)/build/kernels/vibeos-$(1).ksyms.rs $$(CARGO_SHIP) build --target $$(TARGET) $$(CARGO_FLAGS) $(2) --artifact-dir build/kernels/.vibeos-$(1)
	python3 scripts/gen_ksyms.py --nm "$$(NM)" --check build/kernels/.vibeos-$(1)/vibeos build/kernels/vibeos-$(1).ksyms.rs
	python3 scripts/check_kernel_fp.py --objdump "$$(OBJDUMP)" build/kernels/.vibeos-$(1)/vibeos
	cp build/kernels/.vibeos-$(1)/vibeos $$@
$(3): build/kernels/vibeos-$(1).elf $(INITRD) limine.conf $(LIMINE_BIN) scripts/mkiso.sh scripts/iso_disk_id.py $(NOTICES_DEPS)
	LIMINE_DIR=$$(LIMINE_DIR) OBJCOPY=$$(OBJCOPY) scripts/mkiso.sh $$< $(INITRD) $$@ build/iso_root_$(1)
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
# x86-only ISO variants: #GP, nest/stop dumps. aarch64 `make prebuilt`
# must not compile them (gp_test_trip is x86-only; ROADMAP §11.7 aarch64
# tiers run default + ktest). Keep this order so x86 KERNEL_ELFS matches
# tests/harness/test_build_outputs.py VARIANTS.
ifeq ($(ARCH),x86_64)
$(eval $(call KERNEL_VARIANT,gp,--features gp_test,$(ISO_GP)))
$(eval $(call KERNEL_VARIANT,panic-nest,--features panic_nest_test,$(ISO_PANIC_NEST)))
$(eval $(call KERNEL_VARIANT,panic-stop,--features panic_stop_test,$(ISO_PANIC_STOP)))
endif
# ktest: in-guest registry, never packaged as production
$(eval $(call KERNEL_VARIANT,ktest,--features kernel_tests,$(ISO_KTEST)))
ifeq ($(ARCH),x86_64)
$(eval $(call KERNEL_VARIANT,vibefs-crash,--features vibefs_crash,$(ISO_VIBEFS_CRASH)))
$(eval $(call KERNEL_VARIANT,hang,--features hang_test,$(ISO_HANG)))
$(eval $(call KERNEL_VARIANT,irqoff,--features irqoff,$(ISO_IRQOFF)))
$(eval $(call KERNEL_VARIANT,ktest-irqoff,--features kernel_tests --features irqoff,$(ISO_KTEST_IRQOFF)))
endif

KERNEL_ELF := build/kernels/vibeos-default.elf

ifneq ($(VIBEOS_PREBUILT),1)
# The production ELF with the faulting init's initrd (ROADMAP §10.5): its
# panic ends the run through pvpanic, as every production panic does.
$(ISO_INIT_FAULT): $(KERNEL_ELF) $(INITRD_INIT_FAULT) limine.conf $(LIMINE_BIN) scripts/mkiso.sh scripts/iso_disk_id.py $(NOTICES_DEPS)
	LIMINE_DIR=$(LIMINE_DIR) OBJCOPY=$(OBJCOPY) scripts/mkiso.sh $< $(INITRD_INIT_FAULT) $@ build/iso_root_init-fault
# The production ELF with the initrd that has no `/bin/sh` (ROADMAP §10.5):
# init's three failed shell starts end in its exit and the pid 1 panic.
$(ISO_NOSH): $(KERNEL_ELF) $(INITRD_NOSH) limine.conf $(LIMINE_BIN) scripts/mkiso.sh scripts/iso_disk_id.py $(NOTICES_DEPS)
	LIMINE_DIR=$(LIMINE_DIR) OBJCOPY=$(OBJCOPY) scripts/mkiso.sh $< $(INITRD_NOSH) $@ build/iso_root_nosh
endif

# The Rust user programs (ROADMAP §10.5, C-USERBINS): each user/src/bin/<name>.rs
# links as a static non-PIE ET_EXEC at 1 GiB for $(USER_TRIPLE), through rust-lld
# and no C compiler. Only this invocation passes the flags, through --config, so
# .cargo/config.toml needs no table for the triple and its [build] -D warnings
# still applies (F147); the list repeats -D warnings anyway. Opt-level comes from
# the profile override, since member manifests' profiles are ignored.
# check_user_elf.py reads each ELF before the strip, which drops .symtab.
ifeq ($(ARCH),aarch64)
USER_TRIPLE := aarch64-unknown-linux-musl
else
USER_TRIPLE := x86_64-unknown-linux-musl
endif
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
# programs as out of the kernel (ROADMAP §10.2). Its --config args go before
# `build`: cargo replaces the --config list given before the subcommand with
# one given after it, which would drop CARGO_SHIP's trim-paths.
$(USER_STAMP): $(USER_SRCS) user/Cargo.toml user/mem/Cargo.toml Cargo.toml Cargo.lock rust-toolchain.toml scripts/check_user_elf.py Makefile
	$(CARGO) clippy -p vibeos-user -p vibeos-user-mem --target $(USER_TRIPLE) $(CARGO_FLAGS) $(USER_CARGO_CONFIG) -- -D warnings
	$(CARGO_SHIP) $(USER_CARGO_CONFIG) build -p vibeos-user --target $(USER_TRIPLE) $(CARGO_FLAGS)
	python3 scripts/check_user_elf.py $(addprefix $(USER_ELF_DIR)/,$(USER_BIN_NAMES))
	mkdir -p $(USER_OUT)
	$(foreach b,$(USER_BIN_NAMES),$(OBJCOPY) --strip-all $(USER_ELF_DIR)/$(b) $(USER_OUT)/$(b) &&) true
	touch $@

# kernel_tests kernels embed the programs VIBEOS_USER_BINS names (build.rs), so
# only the ktest ELF's build sees the two variables.
build/kernels/vibeos-ktest.elf: $(USER_STAMP)
build/kernels/vibeos-ktest.elf: export VIBEOS_USER_BINS := $(VIBEOS_USER_BINS)
build/kernels/vibeos-ktest.elf: export VIBEOS_USER_DIR := $(USER_OUT)
build/kernels/vibeos-ktest-irqoff.elf: $(USER_STAMP)
build/kernels/vibeos-ktest-irqoff.elf: export VIBEOS_USER_BINS := $(VIBEOS_USER_BINS)
build/kernels/vibeos-ktest-irqoff.elf: export VIBEOS_USER_DIR := $(USER_OUT)
endif

.PHONY: help check check-python check-msrv all kernel iso isos release-artifacts repro ci-budget run run-panic debug clean distclean setup layout prebuilt \
        test-unit test-harness test-e2e test-e2e-panic test-e2e-panic-nest test-e2e-panic-stop test-e2e-gp test-e2e-mce test \
        test-e2e-pit test-e2e-highmem test-e2e-init-fault test-e2e-strace test-ps2 test-kernel test-kernel-smp4 test-lapic-fallback \
        test-smp-stress test-vibefs-crash test-vibefs-crash-1 test-vibefs-crash-2 test-vibefs-crash-plants test-e2e-uefi test-qmp test-forensics test-irqoff \
        test-aarch64 test-gic-fallback litmus

help:
	@printf '%s\n' \
	  'vibeOS make targets:' \
	  '  check                 fast local gate (clippy/unit/harness/python)' \
	  '  check-python          ruff and mypy (VIBEOS_ALLOW_MISSING_TOOLS=1 skips a missing one)' \
	  '  check-msrv            vibeos-core with its MSRV toolchain (rust-version), host and kernel target' \
	  '  fuzz-check            tests/fuzz: fmt, clippy, build every target, replay every committed input' \
	  '  fuzz                  every cargo-fuzz target for FUZZ_TIME s (FUZZ_TARGETS=, FUZZ_SANITIZER=none|address)' \
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
	  '                        CORE=<core.zst> [ELF=]: no QEMU; gdb opens that core instead' \
	  '  layout                objdump sections + __kernel_ symbols' \
	  '  vmcore                the core tool: vmcore report --core <file|-> --elf <kernel.elf>' \
	  '  test-unit             vibeos-core host tests (any host triple)' \
	  '  models-quick          loom models (loom_*) at 3 preemptions, ROADMAP §10.8' \
	  '  models                loom models and Kani proofs (needs ./setup.sh --kani)' \
	  '  miri                  vibeos-core host tests under Miri' \
	  '  test-harness          python unit tests for the harness' \
	  '  test-e2e              boot contract on the production ISO' \
	  '  test-e2e-uefi         same, UEFI firmware from the probe on pflash; none installed: skip (fail under CI)' \
	  '  test-e2e-panic        panic-test dump contract' \
	  '  test-e2e-panic-nest   irq_nest underflow: one dump, no reentered' \
	  '  test-e2e-panic-stop   -smp 5: the dump stops every other CPU' \
	  '  test-e2e-gp           #GP dump+halt contract' \
	  '  test-e2e-mce          injected #MC dump+halt contract' \
	  '  test-e2e-pit          PIT calibration fallback' \
	  '  test-e2e-highmem      boot contract with 9 GiB; pmm counts RAM above 8 GiB' \
	  '  test-e2e-init-fault   /sbin/init faults, or finds no /bin/sh: pid 1 line, then the panic' \
	  '  test-e2e-strace       vibeos.strace=1 via fw_cfg: cmdline echo + syscall trace' \
	  '  test-ps2              QEMU sendkey echo (also part of test-e2e)' \
	  '  test-qmp              QMP event streams re-recorded and compared; one guest core checked' \
	  '  test-forensics        hang_test ISO at -smp 4: core after 5 s, core tool report/export/virt; #GP, 9 GiB' \
	  '  test-kernel           in-guest tests, -smp 2' \
	  '  test-kernel-smp4      in-guest tests, -smp 4' \
	  '  test-lapic-fallback   in-guest tests, TSC-deadline off' \
	  '  test-kernel-<k>       the per-push shards of those three, which test runs:' \
	  '                        test-kernel-smp4-<k> and test-lapic-fallback-<k> too' \
	  '  test-irqoff           test-kernel and test-e2e in the IF-off tracer build (nightly)' \
	  '  test-vibefs-crash     QEMU-kill + host fsck-vibefs' \
	  '  test-smp-stress       -smp 4 in-guest tier (weekly CI); aarch64 also boots weak_order_probe' \
	  '  test-aarch64          aarch64 e2e + kernel + smp4 + GICv2 fallback shards' \
	  '  test-gic-fallback     in-guest tests with VIBEOS_GIC=2 (aarch64)' \
	  '  litmus                herd7 on tests/litmus/ (needs herdtools7)' \
	  '  test                  all of the above except test-smp-stress and test-ps2' \
	  '  test-vibefs-crash-plants  each vibeos.crash_plant= defect caught, then a clean round' \
	  '  gate PHASE=N          phase exit gate: gate-map entries and box rules (RECORD=1: dev-host records)' \
	  '  prebuilt              the ISOs and host tools that architecture'\''s tiers use;' \
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
# features for $(TARGET) and vibeos-core's no_std build for the kernel target,
# so kernel-target code compiles before every commit; CI's ladder lints each
# other ISO feature set and kernel_shell (ROADMAP §10.1, F147).
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
	cargo clippy --bin vibeos --target $(TARGET) -- -D warnings
	$(MAKE) test-unit
	cargo test -p vibeos-core --lib --features std --target $(HOST_TRIPLE) --config 'profile.test.debug-assertions=false' -- release_assert_
	$(MAKE) models-quick
	$(MAKE) test-harness
	$(MAKE) fuzz-check
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
	    cargo deny --workspace check licenses bans sources && \
	    cargo deny --manifest-path $(FUZZ_DIR)/Cargo.toml check licenses bans sources; \
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

# The production initrd: the Rust programs `make user` built (C-USERBINS), each
# `<build file>:<initrd path>`, with the ten §10.5 utilities in /bin.
INITRD_UTILS := ls cat echo grep wc true false sleep yes cmp
INITRD_FILES := $(USER_OUT)/hello:/hello $(USER_OUT)/init:/sbin/init $(USER_OUT)/sh:/bin/sh \
	$(USER_OUT)/tests:/bin/tests $(USER_OUT)/envcheck:/bin/envcheck $(USER_OUT)/argcheck:/bin/argcheck \
	$(USER_OUT)/fpcheck:/bin/fpcheck \
	$(foreach u,$(INITRD_UTILS),$(USER_OUT)/$(u):/bin/$(u))
$(INITRD): $(HOSTLIB_DEPS) $(USER_STAMP)
	mkdir -p $(dir $@)
	cargo run -p vibeos-hostlib-tests --bin mkinitrd --target $(HOST_TRIPLE) --quiet -- $(abspath $@) \
	    $(foreach f,$(INITRD_FILES),--add $(f))

$(INITRD_INIT_FAULT): $(HOSTLIB_DEPS) $(USER_STAMP)
	mkdir -p $(dir $@)
	cargo run -p vibeos-hostlib-tests --bin mkinitrd --target $(HOST_TRIPLE) --quiet -- $(abspath $@) \
	    --add $(USER_OUT)/hello:/hello \
	    --add $(USER_OUT)/init_fault:/sbin/init \
	    --add $(USER_OUT)/sh:/bin/sh \
	    --add $(USER_OUT)/tests:/bin/tests

$(INITRD_NOSH): $(HOSTLIB_DEPS) $(USER_STAMP)
	mkdir -p $(dir $@)
	cargo run -p vibeos-hostlib-tests --bin mkinitrd --target $(HOST_TRIPLE) --quiet -- $(abspath $@) \
	    $(foreach f,$(filter-out %:/bin/sh %:/bin/tests,$(INITRD_FILES)) $(USER_OUT)/false:/bin/tests,--add $(f))

iso: $(ISO)

isos: $(ISOS)

# v* release images (ROADMAP §10.1, §10.2; BOOT.md §3.5): the production ISO in
# the release profile, copied to OUT. The ISO and named-ELF paths do not name
# the profile, so a newer dev build would be reused: remove, rebuild, verify
# that the image's kernel is the release link's output, less its DWARF
# sections (scripts/mkiso.sh strips them).
RELEASE_ELF := $(CARGO_TARGET_DIR)/$(TARGET)/release/vibeos
release-artifacts:
	@if [ -z "$(OUT)" ]; then echo "release-artifacts: set OUT=<dir>" >&2; exit 2; fi
	@if [ -n "$$(ls -A "$(OUT)" 2>/dev/null)" ]; then echo "release-artifacts: $(OUT) is not empty" >&2; exit 2; fi
	rm -f $(ISO) $(KERNEL_ELF)
	$(MAKE) CARGO_PROFILE=release $(ISO)
	mkdir -p "$(OUT)" && cp $(ISO) "$(OUT)/vibeos.iso"
	rm -f build/release-kernel.elf
	xorriso -osirrox on -indev "$(OUT)/vibeos.iso" -extract /boot/vibeos build/release-kernel.elf
	$(OBJCOPY) --strip-debug $(RELEASE_ELF) build/release-kernel.stripped.elf
	cmp build/release-kernel.elf build/release-kernel.stripped.elf

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

# The initrd's programs before the strip, whose symbols `make debug` loads
# beside the kernel's.
DEBUG_USER_ELFS := $(addprefix $(USER_ELF_DIR)/,hello init sh tests)

# QEMU halted with a gdb stub on :1234 (`-s -S`); attach with
# `gdb -x scripts/vibeos.gdb` from this directory (DESIGN §8.4).
# `make debug CORE=<core.zst> [ELF=<kernel.elf>]` starts no QEMU: the core
# tool writes that guest core's virtually addressed core, which the same gdb
# command opens (ROADMAP §10.7). ELF defaults to the production kernel.
debug: $(if $(CORE),$(VMCORE),$(ISO) $(KERNEL_ELF) $(USER_STAMP))
	$(if $(CORE),VIBEOS_VMCORE=$(VMCORE)) python3 tests/harness/run_interactive.py debug \
	    --kernel-elf $(or $(ELF),$(KERNEL_ELF)) \
	    $(if $(CORE),--core $(CORE),$(foreach e,$(DEBUG_USER_ELFS),--user-elf $(e)))

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

# ROADMAP §10.8: every loom model, models-quick's command without its
# preemption cap, then every Kani harness (`#[cfg(kani)] mod kani_proofs`)
# at setup.sh's KANI_VERSION. Not in `make check`: the nightly and macOS jobs
# run it. Kani builds with debug assertions off, because a debug `Frames`
# records its `#[track_caller]` site and Kani has no `caller_location`;
# `-Z stubbing` lets the buddy proof replace `node_ptr` (mm/pmm.rs).
KANI_VERSION := $(shell sed -n 's/^KANI_VERSION=//p' setup.sh)

.PHONY: models
models:
	@test -n "$(KANI_VERSION)" || { echo "models: KANI_VERSION reads empty in setup.sh" >&2; exit 1; }
	@found=$$(cargo kani --version 2>/dev/null | sed -n 's/^Kani Rust Verifier \([^ ]*\).*/\1/p'); \
	  [ "$$found" = "$(KANI_VERSION)" ] || { echo "models: need kani-verifier $(KANI_VERSION) (KANI_VERSION in setup.sh), found '$${found:-none}'; run ./setup.sh --kani" >&2; exit 1; }
	CARGO_TARGET_DIR=$(CARGO_TARGET_DIR)/loom RUSTFLAGS="--cfg loom -D warnings" cargo test -p vibeos-core --lib --features std --release --target $(HOST_TRIPLE) -- loom_ --test-threads=1
	CARGO_BUILD_TARGET=$(HOST_TRIPLE) CARGO_PROFILE_DEV_DEBUG_ASSERTIONS=false cargo kani -p vibeos-core --features std -Z stubbing

# ROADMAP §10.8: vibeos-core's host tests under Miri, the --lib and --doc
# sets test-unit runs. Tests leak their 'static backends on purpose
# (fs::testfs::ramfs), so a leak is not an error here; undefined behaviour
# is. A test Miri cannot run carries `cfg_attr(miri, ignore = "<reason>")`.
.PHONY: miri
miri:
	MIRIFLAGS=-Zmiri-ignore-leaks cargo miri test -p vibeos-core --lib --features std --target $(HOST_TRIPLE)
	MIRIFLAGS=-Zmiri-ignore-leaks cargo miri test -p vibeos-core --doc --features std --target $(HOST_TRIPLE)

test-harness:
	VIBEOS_TIER=$@ GITHUB_STEP_SUMMARY= python3 -m unittest discover -s tests/harness -t . -v

# Host mkfs/fsck share crates/core/src/fs/vibefs/. Artifacts land under
# $(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/ (A2). Defined above the tiers that
# name them: make expands a prerequisite list when it reads the rule.
MKFS_VIBEFS := $(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/debug/mkfs-vibefs
FSCK_VIBEFS := $(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/debug/fsck-vibefs
NBD_CACHE := $(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/debug/nbd-cache
VIBEFS_CAT := $(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/debug/vibefs-cat

# The core tool (ROADMAP §10.7) decodes kernel types whose layout differs with
# debug assertions (docs/VMCOREINFO.md), so it builds in the kernel's profile.
VMCORE := $(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/$(if $(filter release,$(CARGO_PROFILE)),release,debug)/vmcore

ifneq ($(VIBEOS_PREBUILT),1)
$(MKFS_VIBEFS) $(FSCK_VIBEFS) $(NBD_CACHE) $(VIBEFS_CAT): $(HOSTLIB_DEPS)
	cargo build -p vibeos-hostlib-tests --bins --target $(HOST_TRIPLE)

$(VMCORE): $(HOSTLIB_DEPS)
	cargo build -p vibeos-hostlib-tests --bin vmcore --target $(HOST_TRIPLE) $(CARGO_FLAGS)
endif

.PHONY: vmcore
vmcore: $(VMCORE)

# What a tier job downloads instead of building (DESIGN §8.6): every ISO and
# every host tool a `test-*` recipe lists. Recursive `=`, so it follows the
# variables' paths. The tar keeps the executable bit, which upload-artifact
# drops, and holds paths relative to $(CURDIR). The named ELFs go too: a failed
# run's guest core keeps the ELF behind its ISO (ROADMAP §10.7).
ifeq ($(ARCH),aarch64)
# The ISOs aarch64 per-push tiers run (ROADMAP §11.7): e2e + ktest shards.
PREBUILT_FILES = $(ISO) $(ISO_KTEST) \
	build/kernels/vibeos-default.elf build/kernels/vibeos-ktest.elf \
	$(MKFS_VIBEFS) $(FSCK_VIBEFS) $(NBD_CACHE) $(VIBEFS_CAT) $(VMCORE)
else
PREBUILT_FILES = $(ISO) $(ISO_PANIC) $(ISO_GP) $(ISO_PANIC_NEST) $(ISO_PANIC_STOP) $(ISO_KTEST) $(ISO_VIBEFS_CRASH) $(ISO_INIT_FAULT) $(ISO_NOSH) \
	$(ISO_HANG) $(KERNEL_ELFS) $(MKFS_VIBEFS) $(FSCK_VIBEFS) $(NBD_CACHE) $(VIBEFS_CAT) $(VMCORE)
endif

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

# 9 GiB guest: the buddy includes RAM above 8 GiB (DESIGN §4.1).
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

# Pid 1's end panics the kernel with its registered line (ROADMAP §10.5,
# F068): the init-fault ISO's `/sbin/init` stores to 0x1000, and the nosh
# ISO's init exits after three failed `/bin/sh` starts (F128).
test-e2e-init-fault: $(ISO_INIT_FAULT) $(ISO_NOSH)
	VIBEOS_TIER=test-e2e-init-fault VIBEOS_ISO=$(ISO_INIT_FAULT) python3 tests/harness/run_pid1.py init_fault
	VIBEOS_TIER=test-e2e-init-fault VIBEOS_RESULTS_APPEND=1 VIBEOS_ISO=$(ISO_NOSH) python3 tests/harness/run_pid1.py init_no_sh

# The QMP event streams tests/harness/test_qmp.py replays, re-recorded on
# this QEMU and compared with tests/harness/fixtures/qmp/, then a guest core
# of the production ISO (DESIGN §8.3, ROADMAP §10.7).
test-qmp: $(ISO)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO) python3 tests/harness/run_qmp.py

# The forensics tier (ROADMAP §10.7, TESTING.md §8.5): the hang_test ISO at
# -smp 4, its core 5 s after the armed marker, and the core tool's report,
# trace export and virtual core; the BUILD-ID refusal, a #GP core, and a
# 9 GiB guest's core through the pipe. Its cores stay in build/forensics/.
test-forensics: $(ISO_HANG) $(ISO_GP) $(VMCORE)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_HANG) VIBEOS_VMCORE=$(VMCORE) python3 tests/harness/run_forensics.py

# The in-guest tiers (DESIGN §8.2). Each variant's target runs the whole
# registry in one boot, then the proof boots: the full run, for local use, the
# scheduled jobs and the gate. Its shards, `<variant>-<k>`, run the same in
# pieces under the 60 s a per-push tier may take, each one ci.yml tier, and
# `make test` runs them: tests/harness/ktest_shards.py splits the registry
# (`vibeos.ktest_range=`) and the proof boots between them, every row and boot
# in one shard (TESTING.md §8.6). KTEST_ENV is the variant's configuration,
# which a variant and its shards share.
KTEST_ENV =
KTEST_RUN = VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_KTEST) $(KTEST_ENV) python3 tests/harness/run_ktest.py
KERNEL_SHARDS := test-kernel-1 test-kernel-2 test-kernel-3 test-kernel-4 test-kernel-5 test-kernel-6
KERNEL_A64_PROOF := test-kernel-7 test-kernel-8 test-kernel-9
KERNEL_SMP4_SHARDS := test-kernel-smp4-1 test-kernel-smp4-2 test-kernel-smp4-3 test-kernel-smp4-4 test-kernel-smp4-5
KERNEL_SMP4_A64_PROOF := test-kernel-smp4-6 test-kernel-smp4-7 test-kernel-smp4-8
LAPIC_FALLBACK_SHARDS := test-lapic-fallback-1 test-lapic-fallback-2 test-lapic-fallback-3 test-lapic-fallback-4 test-lapic-fallback-5 test-lapic-fallback-6
.PHONY: $(KERNEL_SHARDS) $(KERNEL_A64_PROOF) $(KERNEL_SMP4_SHARDS) $(KERNEL_SMP4_A64_PROOF) $(LAPIC_FALLBACK_SHARDS)

# The nightly KVM leg adds +invtsc, so its invariant-TSC check still applies
# (DESIGN §8.4).
LAPIC_FALLBACK_CPU ?= qemu64,-tsc-deadline
# aarch64 env_config defaults VIBEOS_SMP to 1 (`make run`). The in-guest
# `-smp 2` tiers must set it: AP tests skip with `no AP` at 1 CPU, and
# those skips have no aarch64 skips.toml row (ROADMAP §11.7).
test-kernel $(KERNEL_SHARDS) $(KERNEL_A64_PROOF): KTEST_ENV = VIBEOS_SMP=2
test-kernel-smp4 $(KERNEL_SMP4_SHARDS) $(KERNEL_SMP4_A64_PROOF): KTEST_ENV = VIBEOS_SMP=4
test-lapic-fallback $(LAPIC_FALLBACK_SHARDS): KTEST_ENV = VIBEOS_QEMU_CPU=$(LAPIC_FALLBACK_CPU)

test-kernel: $(ISO_KTEST)
	$(KTEST_RUN) --hpet-off

test-kernel-1: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-kernel-2: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-kernel-3: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-kernel-4: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-kernel-5: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-kernel-6: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-kernel-7: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-kernel-8: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-kernel-9: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-kernel-smp4: $(ISO_KTEST)
	$(KTEST_RUN)

test-kernel-smp4-1: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-kernel-smp4-2: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-kernel-smp4-3: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-kernel-smp4-4: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-kernel-smp4-5: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-kernel-smp4-6: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-kernel-smp4-7: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-kernel-smp4-8: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-lapic-fallback: $(ISO_KTEST)
	$(KTEST_RUN)

test-lapic-fallback-1: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-lapic-fallback-2: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-lapic-fallback-3: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-lapic-fallback-4: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-lapic-fallback-5: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

test-lapic-fallback-6: $(ISO_KTEST)
	$(KTEST_RUN) --shard $@

# GICv2 fallback (ROADMAP §11.7): the test-kernel shards, registry and
# proof boots, with VIBEOS_GIC=2. Not in `make test` (x86); `make test-aarch64`
# and the aarch64 CI tiers run them.
GIC_FALLBACK_SHARDS := test-gic-fallback-1 test-gic-fallback-2 test-gic-fallback-3 test-gic-fallback-4 test-gic-fallback-5 test-gic-fallback-6 test-gic-fallback-7 test-gic-fallback-8 test-gic-fallback-9
.PHONY: test-gic-fallback $(GIC_FALLBACK_SHARDS)
test-gic-fallback $(GIC_FALLBACK_SHARDS): KTEST_ENV = VIBEOS_GIC=2 VIBEOS_SMP=2
test-gic-fallback: $(ISO_KTEST)
	$(KTEST_RUN)
test-gic-fallback-1: $(ISO_KTEST)
	$(KTEST_RUN) --shard test-kernel-1
test-gic-fallback-2: $(ISO_KTEST)
	$(KTEST_RUN) --shard test-kernel-2
test-gic-fallback-3: $(ISO_KTEST)
	$(KTEST_RUN) --shard test-kernel-3
test-gic-fallback-4: $(ISO_KTEST)
	$(KTEST_RUN) --shard test-kernel-4
test-gic-fallback-5: $(ISO_KTEST)
	$(KTEST_RUN) --shard test-kernel-5
test-gic-fallback-6: $(ISO_KTEST)
	$(KTEST_RUN) --shard test-kernel-6
test-gic-fallback-7: $(ISO_KTEST)
	$(KTEST_RUN) --shard test-kernel-7
test-gic-fallback-8: $(ISO_KTEST)
	$(KTEST_RUN) --shard test-kernel-8
test-gic-fallback-9: $(ISO_KTEST)
	$(KTEST_RUN) --shard test-kernel-9

# aarch64 per-push ladder (ROADMAP §11.7). x86-only e2e (PIT, #GP, #MC,
# 9 GiB, LAPIC fallback, vibefs-crash) stays on `make test`.
test-aarch64: test-e2e test-kernel-1 test-kernel-2 test-kernel-3 test-kernel-4 test-kernel-5 test-kernel-6 test-kernel-7 test-kernel-8 test-kernel-9 test-kernel-smp4-1 test-kernel-smp4-2 test-kernel-smp4-3 test-kernel-smp4-4 test-kernel-smp4-5 test-kernel-smp4-6 test-kernel-smp4-7 test-kernel-smp4-8 test-gic-fallback-1 test-gic-fallback-2 test-gic-fallback-3 test-gic-fallback-4 test-gic-fallback-5 test-gic-fallback-6 test-gic-fallback-7 test-gic-fallback-8 test-gic-fallback-9

.PHONY: litmus
litmus:
	python3 scripts/check_litmus.py --run

# Over the volatile-cache device (DESIGN §8.3): nbd-cache serves the disk,
# vibefs-cat reads /w from each image rebuilt from its trace. The 8 rounds
# run as two CI tiers of 4, each with its own seed (ROADMAP §10.1, --tiers);
# test-vibefs-crash runs both.
VIBEFS_CRASH_RUN = VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_VIBEFS_CRASH) VIBEOS_MKFS=$(MKFS_VIBEFS) \
	    VIBEOS_FSCK=$(FSCK_VIBEFS) VIBEOS_NBD_CACHE=$(NBD_CACHE) VIBEOS_VIBEFS_CAT=$(VIBEFS_CAT) \
	    python3 tests/harness/run_vibefs_crash.py --rounds 4
test-vibefs-crash: test-vibefs-crash-1 test-vibefs-crash-2
test-vibefs-crash-1: $(ISO_VIBEFS_CRASH) $(MKFS_VIBEFS) $(FSCK_VIBEFS) $(NBD_CACHE) $(VIBEFS_CAT)
	cargo test -p vibeos-hostlib-tests --target $(HOST_TRIPLE)
	$(VIBEFS_CRASH_RUN)
test-vibefs-crash-2: $(ISO_VIBEFS_CRASH) $(MKFS_VIBEFS) $(FSCK_VIBEFS) $(NBD_CACHE) $(VIBEFS_CAT)
	$(VIBEFS_CRASH_RUN)

# The crash test's planted defects (ROADMAP §10.2): each plant must fail a
# round, then an unplanted control round must pass. Its own tier keeps the
# planted failures out of test-vibefs-crash's results.
test-vibefs-crash-plants: $(ISO_VIBEFS_CRASH) $(MKFS_VIBEFS) $(FSCK_VIBEFS) $(NBD_CACHE) $(VIBEFS_CAT)
	cargo test -p vibeos-hostlib-tests --target $(HOST_TRIPLE)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_VIBEFS_CRASH) VIBEOS_MKFS=$(MKFS_VIBEFS) VIBEOS_FSCK=$(FSCK_VIBEFS) \
	    VIBEOS_NBD_CACHE=$(NBD_CACHE) VIBEOS_VIBEFS_CAT=$(VIBEFS_CAT) python3 tests/harness/run_vibefs_crash.py \
	    --plants leak,early_super

# IF-off tracer (ROADMAP §10.3). TCG: -icount shift=0, one CPU, so guest ns =
# instructions; KVM takes neither (it rejects -icount) and records max and p99
# with no threshold. Unset VIBEOS_QEMU_ACCEL is the harness's default, TCG.
# Not in `test`: the nightly job runs it. The e2e driver appends to the ktest
# driver's results file (VIBEOS_RESULTS_APPEND), which starts empty.
IRQOFF_ENV = $(if $(filter tcg,$(or $(VIBEOS_QEMU_ACCEL),tcg)),VIBEOS_SMP=1 VIBEOS_QEMU_EXTRA="-icount shift=0 $(VIBEOS_QEMU_EXTRA)")
test-irqoff: $(ISO_KTEST_IRQOFF) $(ISO_IRQOFF) $(MKFS_VIBEFS)
	rm -f build/results/x86_64-$@.json
	VIBEOS_TIER=$@ $(IRQOFF_ENV) VIBEOS_ISO=$(ISO_KTEST_IRQOFF) python3 tests/harness/run_ktest.py
	VIBEOS_TIER=$@ $(IRQOFF_ENV) VIBEOS_RESULTS_APPEND=1 VIBEOS_ISO=$(ISO_IRQOFF) VIBEOS_MKFS=$(MKFS_VIBEFS) python3 tests/harness/run_e2e.py

test: test-unit test-harness test-e2e test-e2e-uefi test-e2e-panic test-e2e-panic-nest test-e2e-panic-stop test-e2e-gp test-e2e-mce test-e2e-pit test-e2e-highmem test-e2e-init-fault test-e2e-strace test-e2e-power test-qmp test-forensics test-kernel-1 test-kernel-2 test-kernel-3 test-kernel-4 test-kernel-5 test-kernel-6 test-kernel-smp4-1 test-kernel-smp4-2 test-kernel-smp4-3 test-kernel-smp4-4 test-kernel-smp4-5 test-lapic-fallback-1 test-lapic-fallback-2 test-lapic-fallback-3 test-lapic-fallback-4 test-lapic-fallback-5 test-lapic-fallback-6 test-vibefs-crash-1 test-vibefs-crash-2 test-vibefs-crash-plants

# The -smp 4 in-guest tier, weekly in CI, not every push. ROADMAP §4.11.
test-smp-stress: $(ISO_KTEST)
	VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_KTEST) VIBEOS_SMP=4 python3 tests/harness/run_ktest.py
	@if [ "$(ARCH)" = aarch64 ]; then \
	    VIBEOS_TIER=$@ VIBEOS_ISO=$(ISO_KTEST) VIBEOS_SMP=4 VIBEOS_KTEST=weak_order_probe \
	        VIBEOS_RESULTS_APPEND=1 python3 tests/harness/run_ktest.py; \
	fi

# Phase exit gate (ROADMAP §10.9): the gate map's entries and the box rules.
.PHONY: gate
gate:
	@test -n "$(PHASE)" || { echo "gate: set PHASE=N" >&2; exit 2; }
	python3 scripts/gate.py --phase "$(PHASE)" $(if $(filter 1,$(RECORD)),--record) $(if $(COMMIT),--commit "$(COMMIT)")

# Keeps build/results/.
clean:
	rm -rf build/kernels build/iso_root_* $(ISOS) $(addsuffix .xorriso-version,$(ISOS)) \
	    $(INITRD) $(USER_OUT) $(ISO_INIT_FAULT) $(INITRD_INIT_FAULT) $(ISO_NOSH) $(INITRD_NOSH)
	$(CARGO) clean

distclean: clean
	rm -rf $(LIMINE_DIR)

# Fuzz targets for vibeos-core's byte parsers (C-FUZZ, TESTING.md §8.1). The
# crate is its own workspace with its own target dir; each cargo command
# passes the host triple, since .cargo/config.toml defaults to the kernel's.
FUZZ_DIR := tests/fuzz
FUZZ_TARGET_DIR := $(CURDIR)/target/fuzz
# `make fuzz`'s output: its growing corpus and crash artifacts.
FUZZ_OUT := $(CURDIR)/build/fuzz
# Seconds per target, and per input before libFuzzer calls it a hang.
FUZZ_TIME ?= 60
FUZZ_UNIT_TIMEOUT ?= 10
FUZZ_TARGETS ?= $(sort $(basename $(notdir $(wildcard $(FUZZ_DIR)/fuzz_targets/*.rs))))
# AddressSanitizer is not usable with Rust on macOS hosts.
FUZZ_SANITIZER ?= $(if $(filter Darwin,$(shell uname -s)),none,address)
CARGO_FUZZ_VERSION := 0.13.2

.PHONY: fuzz fuzz-check
# Its own target dir, exported to every cargo and cargo-fuzz command below.
fuzz fuzz-check: export CARGO_TARGET_DIR := $(FUZZ_TARGET_DIR)

# `make check`'s fuzz step: fmt, clippy, a build of every target, and
# `cargo test`, which replays every committed corpus and regression input
# (no fuzzing).
fuzz-check:
	cargo fmt --manifest-path $(FUZZ_DIR)/Cargo.toml --check
	cargo clippy --manifest-path $(FUZZ_DIR)/Cargo.toml --locked --all-targets --target $(HOST_TRIPLE) -- -D warnings
	cargo build --manifest-path $(FUZZ_DIR)/Cargo.toml --locked --bins --target $(HOST_TRIPLE)
	cargo test --manifest-path $(FUZZ_DIR)/Cargo.toml --locked --target $(HOST_TRIPLE)

# Run every target for FUZZ_TIME seconds. New inputs go to $(FUZZ_OUT)/corpus
# only (libFuzzer writes to its first corpus directory), so the committed
# corpus and regressions never grow here. A failing target does not stop the
# rest; the recipe fails at the end, naming each with its artifacts.
fuzz:
	@v=$$(cargo fuzz --version 2>/dev/null | sed -n 's/^cargo-fuzz //p'); \
	if [ "$$v" != "$(CARGO_FUZZ_VERSION)" ]; then \
	    echo "fuzz: cargo-fuzz $(CARGO_FUZZ_VERSION) not installed (found: $${v:-none}); cargo install cargo-fuzz --locked --version $(CARGO_FUZZ_VERSION)" >&2; \
	    exit 1; \
	fi
	cargo fuzz build --fuzz-dir $(FUZZ_DIR) --sanitizer $(FUZZ_SANITIZER)
	@failed=""; \
	for t in $(FUZZ_TARGETS); do \
	    mkdir -p $(FUZZ_OUT)/corpus/$$t $(FUZZ_OUT)/artifacts/$$t $(FUZZ_DIR)/regressions/$$t; \
	    echo "fuzz: $$t for $(FUZZ_TIME) s"; \
	    cargo fuzz run --fuzz-dir $(FUZZ_DIR) --sanitizer $(FUZZ_SANITIZER) $$t \
	        $(FUZZ_OUT)/corpus/$$t $(FUZZ_DIR)/corpus/$$t $(FUZZ_DIR)/regressions/$$t -- \
	        -max_total_time=$(FUZZ_TIME) -timeout=$(FUZZ_UNIT_TIMEOUT) -rss_limit_mb=2048 \
	        -artifact_prefix=$(FUZZ_OUT)/artifacts/$$t/ || failed="$$failed $$t"; \
	done; \
	if [ -n "$$failed" ]; then \
	    for t in $$failed; do echo "fuzz: FAIL $$t: artifacts in $(FUZZ_OUT)/artifacts/$$t" >&2; done; \
	    exit 1; \
	fi; \
	echo "fuzz: ok ($(words $(FUZZ_TARGETS)) targets, $(FUZZ_TIME) s each)"
