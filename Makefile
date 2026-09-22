# vibeOS Makefile. Phase 0: build a hybrid BIOS+UEFI ISO, run it in QEMU,
# and drive the Python e2e harness.
#
# See docs/DESIGN.md §8.5 for target inventory.

TARGET_JSON := x86_64-unknown-none-executable.json
TARGET      := x86_64-unknown-none-executable

# Nightly cargo needs -Zjson-target-spec to accept our custom target JSON.
# build-std lives here rather than in .cargo/config.toml so hostlib tests
# (a nested cargo workspace) do not inherit a second `core`.
CARGO         := cargo -Zjson-target-spec -Zbuild-std=core,compiler_builtins,alloc -Zbuild-std-features=compiler-builtins-mem
export CARGO_TARGET_DIR := $(CURDIR)/target
CARGO_PROFILE ?= dev
ifeq ($(CARGO_PROFILE),release)
CARGO_FLAGS   := --release
PROFILE_DIR   := release
else
CARGO_FLAGS   :=
PROFILE_DIR   := debug
endif

KERNEL_ELF := $(CARGO_TARGET_DIR)/$(TARGET)/$(PROFILE_DIR)/vibeos
ISO        := vibeos.iso
ISO_ROOT   := iso_root

LIMINE_DIR := ./limine
LIMINE_BIN := $(LIMINE_DIR)/limine

# QEMU config. `-smp 2` from day one, DESIGN §0.5. VIBEOS_SMP / VIBEOS_QEMU_CPU
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

# Prerequisites: everything under src/, the linker script, the target spec, the
# limine config, and this Makefile. A find(1) so newly added source dirs are
# not silently missed (DESIGN §9.1).
KERNEL_SRCS := $(shell find src -type f \( -name '*.rs' -o -name '*.asm' -o -name '*.S' \) 2>/dev/null)
USER_HELLO  := user/hello
USER_INIT   := user/init
USER_SH     := user/sh
USER_TESTS  := user/tests
KERNEL_DEPS := $(KERNEL_SRCS) Cargo.toml $(TARGET_JSON) linker.ld Makefile rust-toolchain.toml \
	scripts/gen_ksyms.py scripts/mkinitrd.py scripts/mkuserelf.py \
	user/hello.asm user/init.asm user/sh.asm user/tests.asm user/sys.inc initrd.fat

LLVM_TOOL_DIR := $(shell rustc --print sysroot)/lib/rustlib/$(shell rustc -vV | sed -n 's/^host: //p')/bin
OBJDUMP := $(if $(wildcard $(LLVM_TOOL_DIR)/llvm-objdump),$(LLVM_TOOL_DIR)/llvm-objdump,llvm-objdump)
NM      := $(if $(wildcard $(LLVM_TOOL_DIR)/llvm-nm),$(LLVM_TOOL_DIR)/llvm-nm,llvm-nm)

# Two-pass ksyms: first link has an empty table in .rodata, nm fills it,
# second link does not move .text (DESIGN §5.6). Do not wrap `$(CARGO)` in
# `$(call ...)`: `-Zbuild-std=core,compiler_builtins,alloc` contains commas.

.PHONY: all kernel iso run run-panic clean distclean setup layout \
        test-unit test-harness test-e2e test-e2e-panic test-e2e-gp test \
        test-e2e-pit test-ps2 test-kernel test-kernel-smp4 test-lapic-fallback \
        test-smp-stress test-vibefs-crash

all: $(ISO)

kernel: $(KERNEL_ELF)

$(KERNEL_ELF): $(KERNEL_DEPS)
	$(CARGO) build $(CARGO_FLAGS)
	python3 scripts/gen_ksyms.py --nm "$(NM)" $(KERNEL_ELF) $(CARGO_TARGET_DIR)/vibeos-ksyms.rs
	VIBEOS_KSYMS=$(CARGO_TARGET_DIR)/vibeos-ksyms.rs $(CARGO) build $(CARGO_FLAGS)

$(LIMINE_BIN):
	@echo "limine binaries missing; run ./setup.sh" >&2
	@exit 1

initrd.fat: scripts/mkinitrd.py $(USER_HELLO) $(USER_INIT) $(USER_SH) $(USER_TESTS)
	python3 scripts/mkinitrd.py $@ \
	    --add $(USER_HELLO):/hello \
	    --add $(USER_INIT):/sbin/init \
	    --add $(USER_SH):/bin/sh \
	    --add $(USER_TESTS):/bin/tests

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

$(ISO): $(KERNEL_ELF) limine.conf $(LIMINE_BIN)
	@echo "  ISO $(ISO)"
	@rm -rf $(ISO_ROOT)
	@mkdir -p $(ISO_ROOT)/boot $(ISO_ROOT)/EFI/BOOT
	@cp $(KERNEL_ELF) $(ISO_ROOT)/boot/vibeos
	@cp limine.conf $(ISO_ROOT)/boot/
	@cp $(LIMINE_DIR)/limine-bios.sys $(ISO_ROOT)/boot/
	@cp $(LIMINE_DIR)/limine-bios-cd.bin $(ISO_ROOT)/boot/
	@cp $(LIMINE_DIR)/limine-uefi-cd.bin $(ISO_ROOT)/boot/
	@cp $(LIMINE_DIR)/BOOTX64.EFI $(ISO_ROOT)/EFI/BOOT/
	@xorriso -as mkisofs -quiet \
	    -b boot/limine-bios-cd.bin \
	    -no-emul-boot -boot-load-size 4 -boot-info-table \
	    --efi-boot boot/limine-uefi-cd.bin \
	    -efi-boot-part --efi-boot-image --protective-msdos-label \
	    $(ISO_ROOT) -o $(ISO)
	@$(LIMINE_BIN) bios-install $(ISO) >/dev/null

iso: $(ISO)

run: $(ISO)
	$(QEMU_BASE) -serial stdio

# `panic-test` build: same ISO layout, kernel built with the feature so it
# deliberately panics after `serial online`.
KERNEL_ELF_PANIC := $(CARGO_TARGET_DIR)/$(TARGET)/$(PROFILE_DIR)/vibeos
ISO_PANIC        := vibeos-panic.iso
ISO_ROOT_PANIC   := iso_root_panic

$(ISO_PANIC): $(KERNEL_DEPS) limine.conf $(LIMINE_BIN)
	CARGO_TARGET_DIR=$(CURDIR)/target-panic $(CARGO) build $(CARGO_FLAGS) --features panic-test --features panic_exit
	python3 scripts/gen_ksyms.py --nm "$(NM)" $(CURDIR)/target-panic/$(TARGET)/$(PROFILE_DIR)/vibeos $(CURDIR)/target-panic/vibeos-ksyms.rs
	VIBEOS_KSYMS=$(CURDIR)/target-panic/vibeos-ksyms.rs CARGO_TARGET_DIR=$(CURDIR)/target-panic $(CARGO) build $(CARGO_FLAGS) --features panic-test --features panic_exit
	@rm -rf $(ISO_ROOT_PANIC)
	@mkdir -p $(ISO_ROOT_PANIC)/boot $(ISO_ROOT_PANIC)/EFI/BOOT
	@cp $(CURDIR)/target-panic/$(TARGET)/$(PROFILE_DIR)/vibeos $(ISO_ROOT_PANIC)/boot/vibeos
	@cp limine.conf $(ISO_ROOT_PANIC)/boot/
	@cp $(LIMINE_DIR)/limine-bios.sys $(ISO_ROOT_PANIC)/boot/
	@cp $(LIMINE_DIR)/limine-bios-cd.bin $(ISO_ROOT_PANIC)/boot/
	@cp $(LIMINE_DIR)/limine-uefi-cd.bin $(ISO_ROOT_PANIC)/boot/
	@cp $(LIMINE_DIR)/BOOTX64.EFI $(ISO_ROOT_PANIC)/EFI/BOOT/
	@xorriso -as mkisofs -quiet \
	    -b boot/limine-bios-cd.bin \
	    -no-emul-boot -boot-load-size 4 -boot-info-table \
	    --efi-boot boot/limine-uefi-cd.bin \
	    -efi-boot-part --efi-boot-image --protective-msdos-label \
	    $(ISO_ROOT_PANIC) -o $(ISO_PANIC)
	@$(LIMINE_BIN) bios-install $(ISO_PANIC) >/dev/null

run-panic: $(ISO_PANIC)
	qemu-system-x86_64 -cdrom $(ISO_PANIC) -m $(VIBEOS_MEM) -smp $(VIBEOS_SMP) \
	    -cpu $(VIBEOS_QEMU_CPU) -accel $(VIBEOS_QEMU_ACCEL) -no-reboot -serial stdio -display none

layout: $(KERNEL_ELF)
	@echo "== sections =="
	@$(OBJDUMP) -h $(KERNEL_ELF)
	@echo "== exported symbols =="
	@$(NM) $(KERNEL_ELF) | grep __kernel_

test-unit:
	cd tests/hostlib && cargo test --lib

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

# Deliberate #GP after IDT install: dump + halt. Separate target dir so
# it cannot leak into the production ISO.
ISO_GP        := vibeos-gp.iso
ISO_ROOT_GP   := iso_root_gp

$(ISO_GP): $(KERNEL_DEPS) limine.conf $(LIMINE_BIN)
	CARGO_TARGET_DIR=$(CURDIR)/target-gp $(CARGO) build $(CARGO_FLAGS) --features gp-test --features panic_exit
	python3 scripts/gen_ksyms.py --nm "$(NM)" $(CURDIR)/target-gp/$(TARGET)/$(PROFILE_DIR)/vibeos $(CURDIR)/target-gp/vibeos-ksyms.rs
	VIBEOS_KSYMS=$(CURDIR)/target-gp/vibeos-ksyms.rs CARGO_TARGET_DIR=$(CURDIR)/target-gp $(CARGO) build $(CARGO_FLAGS) --features gp-test --features panic_exit
	@rm -rf $(ISO_ROOT_GP)
	@mkdir -p $(ISO_ROOT_GP)/boot $(ISO_ROOT_GP)/EFI/BOOT
	@cp $(CURDIR)/target-gp/$(TARGET)/$(PROFILE_DIR)/vibeos $(ISO_ROOT_GP)/boot/vibeos
	@cp limine.conf $(ISO_ROOT_GP)/boot/
	@cp $(LIMINE_DIR)/limine-bios.sys $(ISO_ROOT_GP)/boot/
	@cp $(LIMINE_DIR)/limine-bios-cd.bin $(ISO_ROOT_GP)/boot/
	@cp $(LIMINE_DIR)/limine-uefi-cd.bin $(ISO_ROOT_GP)/boot/
	@cp $(LIMINE_DIR)/BOOTX64.EFI $(ISO_ROOT_GP)/EFI/BOOT/
	@xorriso -as mkisofs -quiet \
	    -b boot/limine-bios-cd.bin \
	    -no-emul-boot -boot-load-size 4 -boot-info-table \
	    --efi-boot boot/limine-uefi-cd.bin \
	    -efi-boot-part --efi-boot-image --protective-msdos-label \
	    $(ISO_ROOT_GP) -o $(ISO_GP)
	@$(LIMINE_BIN) bios-install $(ISO_GP) >/dev/null

test-e2e-gp: $(ISO_GP)
	VIBEOS_ISO=$(ISO_GP) VIBEOS_GP_TEST=1 python3 tests/harness/run_e2e.py

# PIT channel 2 calibration: HPET emulation off (`-machine pc,hpet=off`).
# Same ISO, same markers except the diagnostic names `pit` instead of `hpet`.
test-e2e-pit: $(ISO)
	VIBEOS_ISO=$(ISO) VIBEOS_EXPECT_PIT=1 python3 tests/harness/run_e2e.py

# In-guest tests: separate target dir + ISO so a test build can never be
# packaged as production (DESIGN §8.2 / §9.7).
KERNEL_TESTS_DIR := $(CURDIR)/target-kernel-tests
ISO_KTEST        := vibeos-ktest.iso
ISO_ROOT_KTEST   := iso_root_ktest

$(ISO_KTEST): $(KERNEL_DEPS) limine.conf $(LIMINE_BIN)
	CARGO_TARGET_DIR=$(KERNEL_TESTS_DIR) $(CARGO) build $(CARGO_FLAGS) --features kernel_tests
	python3 scripts/gen_ksyms.py --nm "$(NM)" $(KERNEL_TESTS_DIR)/$(TARGET)/$(PROFILE_DIR)/vibeos $(KERNEL_TESTS_DIR)/vibeos-ksyms.rs
	VIBEOS_KSYMS=$(KERNEL_TESTS_DIR)/vibeos-ksyms.rs CARGO_TARGET_DIR=$(KERNEL_TESTS_DIR) $(CARGO) build $(CARGO_FLAGS) --features kernel_tests
	@echo "  ISO $(ISO_KTEST)"
	@rm -rf $(ISO_ROOT_KTEST)
	@mkdir -p $(ISO_ROOT_KTEST)/boot $(ISO_ROOT_KTEST)/EFI/BOOT
	@cp $(KERNEL_TESTS_DIR)/$(TARGET)/$(PROFILE_DIR)/vibeos $(ISO_ROOT_KTEST)/boot/vibeos
	@cp limine.conf $(ISO_ROOT_KTEST)/boot/
	@cp $(LIMINE_DIR)/limine-bios.sys $(ISO_ROOT_KTEST)/boot/
	@cp $(LIMINE_DIR)/limine-bios-cd.bin $(ISO_ROOT_KTEST)/boot/
	@cp $(LIMINE_DIR)/limine-uefi-cd.bin $(ISO_ROOT_KTEST)/boot/
	@cp $(LIMINE_DIR)/BOOTX64.EFI $(ISO_ROOT_KTEST)/EFI/BOOT/
	@xorriso -as mkisofs -quiet \
	    -b boot/limine-bios-cd.bin \
	    -no-emul-boot -boot-load-size 4 -boot-info-table \
	    --efi-boot boot/limine-uefi-cd.bin \
	    -efi-boot-part --efi-boot-image --protective-msdos-label \
	    $(ISO_ROOT_KTEST) -o $(ISO_KTEST)
	@$(LIMINE_BIN) bios-install $(ISO_KTEST) >/dev/null

test-kernel: $(ISO_KTEST)
	VIBEOS_ISO=$(ISO_KTEST) python3 tests/kernel_boot.py

test-kernel-smp4: $(ISO_KTEST)
	VIBEOS_ISO=$(ISO_KTEST) VIBEOS_SMP=4 python3 tests/kernel_boot.py

test-lapic-fallback: $(ISO_KTEST)
	VIBEOS_ISO=$(ISO_KTEST) VIBEOS_QEMU_CPU=qemu64,-tsc-deadline python3 tests/kernel_boot.py

# Host mkfs/fsck share src/vibefs.rs. CARGO_TARGET_DIR is the kernel's
# `./target`; hostlib's .cargo/config selects the GNU triple.
HOST_TRIPLE := x86_64-unknown-linux-gnu
MKFS_VIBEFS := $(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/debug/mkfs-vibefs
FSCK_VIBEFS := $(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/debug/fsck-vibefs

$(MKFS_VIBEFS) $(FSCK_VIBEFS): src/vibefs.rs tests/hostlib/src/bin/mkfs_vibefs.rs \
		tests/hostlib/src/bin/fsck_vibefs.rs tests/hostlib/Cargo.toml tests/hostlib/src/lib.rs
	cd tests/hostlib && cargo build --bins

# QEMU-kill crash consistency. Separate target dir so the write-loop
# kernel cannot land in the production ISO (DESIGN §8.2 / §9.7).
KERNEL_VIBEFS_CRASH_DIR := $(CURDIR)/target-vibefs-crash
ISO_VIBEFS_CRASH        := vibeos-vibefs-crash.iso
ISO_ROOT_VIBEFS_CRASH   := iso_root_vibefs_crash

$(ISO_VIBEFS_CRASH): $(KERNEL_DEPS) limine.conf $(LIMINE_BIN)
	CARGO_TARGET_DIR=$(KERNEL_VIBEFS_CRASH_DIR) $(CARGO) build $(CARGO_FLAGS) --features vibefs_crash
	python3 scripts/gen_ksyms.py --nm "$(NM)" $(KERNEL_VIBEFS_CRASH_DIR)/$(TARGET)/$(PROFILE_DIR)/vibeos $(KERNEL_VIBEFS_CRASH_DIR)/vibeos-ksyms.rs
	VIBEOS_KSYMS=$(KERNEL_VIBEFS_CRASH_DIR)/vibeos-ksyms.rs CARGO_TARGET_DIR=$(KERNEL_VIBEFS_CRASH_DIR) $(CARGO) build $(CARGO_FLAGS) --features vibefs_crash
	@echo "  ISO $(ISO_VIBEFS_CRASH)"
	@rm -rf $(ISO_ROOT_VIBEFS_CRASH)
	@mkdir -p $(ISO_ROOT_VIBEFS_CRASH)/boot $(ISO_ROOT_VIBEFS_CRASH)/EFI/BOOT
	@cp $(KERNEL_VIBEFS_CRASH_DIR)/$(TARGET)/$(PROFILE_DIR)/vibeos $(ISO_ROOT_VIBEFS_CRASH)/boot/vibeos
	@cp limine.conf $(ISO_ROOT_VIBEFS_CRASH)/boot/
	@cp $(LIMINE_DIR)/limine-bios.sys $(ISO_ROOT_VIBEFS_CRASH)/boot/
	@cp $(LIMINE_DIR)/limine-bios-cd.bin $(ISO_ROOT_VIBEFS_CRASH)/boot/
	@cp $(LIMINE_DIR)/limine-uefi-cd.bin $(ISO_ROOT_VIBEFS_CRASH)/boot/
	@cp $(LIMINE_DIR)/BOOTX64.EFI $(ISO_ROOT_VIBEFS_CRASH)/EFI/BOOT/
	@xorriso -as mkisofs -quiet \
	    -b boot/limine-bios-cd.bin \
	    -no-emul-boot -boot-load-size 4 -boot-info-table \
	    --efi-boot boot/limine-uefi-cd.bin \
	    -efi-boot-part --efi-boot-image --protective-msdos-label \
	    $(ISO_ROOT_VIBEFS_CRASH) -o $(ISO_VIBEFS_CRASH)
	@$(LIMINE_BIN) bios-install $(ISO_VIBEFS_CRASH) >/dev/null

test-vibefs-crash: $(ISO_VIBEFS_CRASH) $(MKFS_VIBEFS) $(FSCK_VIBEFS)
	VIBEOS_ISO=$(ISO_VIBEFS_CRASH) VIBEOS_MKFS=$(MKFS_VIBEFS) VIBEOS_FSCK=$(FSCK_VIBEFS) python3 tests/vibefs_crash.py

test: test-unit test-harness test-e2e test-e2e-uefi test-e2e-panic test-e2e-gp test-e2e-pit test-kernel test-kernel-smp4 test-lapic-fallback test-vibefs-crash

# Longer high-CPU stress. Scheduled CI, not every push. ROADMAP §4.11.
test-smp-stress: $(ISO_KTEST)
	VIBEOS_ISO=$(ISO_KTEST) VIBEOS_SMP=4 VIBEOS_TIMEOUT=180 python3 tests/kernel_boot.py

clean:
	rm -rf $(ISO_ROOT) $(ISO_ROOT_PANIC) $(ISO_ROOT_GP) $(ISO_ROOT_KTEST) $(ISO_ROOT_VIBEFS_CRASH) \
	    $(ISO) $(ISO_PANIC) $(ISO_GP) $(ISO_KTEST) $(ISO_VIBEFS_CRASH) \
	    target-panic target-gp $(KERNEL_TESTS_DIR) $(KERNEL_VIBEFS_CRASH_DIR) initrd.fat \
	    user/hello user/hello.bin user/init user/init.bin user/sh user/sh.bin \
	    user/tests user/tests.bin
	$(CARGO) clean

distclean: clean
	rm -rf $(LIMINE_DIR)
