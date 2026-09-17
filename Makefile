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

QEMU_BASE = qemu-system-x86_64 \
    -cdrom $(ISO) \
    -m $(VIBEOS_MEM) \
    -smp $(VIBEOS_SMP) \
    -cpu $(VIBEOS_QEMU_CPU) \
    -no-reboot

# Prerequisites: everything under src/, the linker script, the target spec, the
# limine config, and this Makefile. A find(1) so newly added source dirs are
# not silently missed (DESIGN §9.1).
KERNEL_SRCS := $(shell find src -type f \( -name '*.rs' -o -name '*.asm' -o -name '*.S' \) 2>/dev/null)
KERNEL_DEPS := $(KERNEL_SRCS) Cargo.toml $(TARGET_JSON) linker.ld Makefile rust-toolchain.toml

.PHONY: all kernel iso run run-panic clean distclean setup layout \
        test-unit test-harness test-e2e test-e2e-panic test test-kernel \
        test-kernel-smp4 test-lapic-fallback

all: $(ISO)

kernel: $(KERNEL_ELF)

$(KERNEL_ELF): $(KERNEL_DEPS)
	$(CARGO) build $(CARGO_FLAGS)

$(LIMINE_BIN):
	@echo "limine binaries missing; run ./setup.sh" >&2
	@exit 1

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
	CARGO_TARGET_DIR=$(CURDIR)/target-panic $(CARGO) build $(CARGO_FLAGS) --features panic-test
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
	    -cpu $(VIBEOS_QEMU_CPU) -no-reboot -serial stdio -display none

LLVM_TOOL_DIR := $(shell rustc --print sysroot)/lib/rustlib/$(shell rustc -vV | sed -n 's/^host: //p')/bin
OBJDUMP := $(if $(wildcard $(LLVM_TOOL_DIR)/llvm-objdump),$(LLVM_TOOL_DIR)/llvm-objdump,llvm-objdump)
NM      := $(if $(wildcard $(LLVM_TOOL_DIR)/llvm-nm),$(LLVM_TOOL_DIR)/llvm-nm,llvm-nm)

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

# In-guest tests: separate target dir + ISO so a test build can never be
# packaged as production (DESIGN §8.2 / §9.7).
KERNEL_TESTS_DIR := $(CURDIR)/target-kernel-tests
ISO_KTEST        := vibeos-ktest.iso
ISO_ROOT_KTEST   := iso_root_ktest

$(ISO_KTEST): $(KERNEL_DEPS) limine.conf $(LIMINE_BIN)
	CARGO_TARGET_DIR=$(KERNEL_TESTS_DIR) $(CARGO) build $(CARGO_FLAGS) --features kernel_tests
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

test: test-unit test-harness test-e2e test-e2e-uefi test-e2e-panic test-kernel

clean:
	rm -rf $(ISO_ROOT) $(ISO_ROOT_PANIC) $(ISO_ROOT_KTEST) $(ISO) $(ISO_PANIC) $(ISO_KTEST) target-panic $(KERNEL_TESTS_DIR)
	$(CARGO) clean

distclean: clean
	rm -rf $(LIMINE_DIR)
