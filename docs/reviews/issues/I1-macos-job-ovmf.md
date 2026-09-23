# I1 · macOS CI job; find OVMF wherever it lives

| | |
|---|---|
| **Area** | 4.11 Infrastructure |
| **Impact / Effort / Phase** | Low / S / II |
| **Depends on** | A2 |
| **Blocks** | — |
| **Review** | [ARCHITECTURE_REVIEW.md §4.11](../ARCHITECTURE_REVIEW.md#411-infrastructure--deployment) |

## Problem

CI runs only on `ubuntu-latest`. `Makefile` `OVMF ?= /usr/share/ovmf/OVMF.fd` is a Linux path; on the maintainer's Mac the firmware is at `/opt/homebrew/share/qemu/edk2-x86_64-code.fd`, so `make test-e2e-uefi` silently prints "OVMF not found; skipping" and exits 0. macOS is a supported dev host (maintainer, 2026-09-22).

## Recommended fix

A scheduled macOS `check` job once A2 makes the host tier portable; an OVMF probe list; a visible "skipped" instead of a silent pass.

## Implementation plan

1. **CI:** add a `check-macos` job (`runs-on: macos-14`, pinned toolchain, `make check`; no QEMU needed) to the scheduled workflow, not the push workflow, per the ROADMAP §10.1 CI budget and §10.2. Optionally a weekly macOS `make test-e2e` with `brew install qemu xorriso nasm` in the same scheduled workflow to catch host-tool drift.
2. **Makefile:**
   ```make
   OVMF_CANDIDATES := /usr/share/ovmf/OVMF.fd /usr/share/OVMF/OVMF_CODE.fd \
                      /opt/homebrew/share/qemu/edk2-x86_64-code.fd /usr/local/share/qemu/edk2-x86_64-code.fd
   OVMF ?= $(firstword $(wildcard $(OVMF_CANDIDATES)))
   AAVMF_CANDIDATES := /usr/share/AAVMF/AAVMF_CODE.fd /usr/share/qemu-efi-aarch64/QEMU_EFI.fd \
                       /opt/homebrew/share/qemu/edk2-aarch64-code.fd /usr/local/share/qemu/edk2-aarch64-code.fd
   AAVMF ?= $(firstword $(wildcard $(AAVMF_CANDIDATES)))
   ```
   One variable per architecture, so the probe never picks the other architecture's image. `test-e2e-uefi` prints `[e2e] SKIP uefi: no OVMF (set OVMF=…)` and, under `VIBEOS_STRICT=1` (CI), exits 1 instead; the aarch64 firmware gets the same visible skip (ROADMAP §10.2).
3. **Harness:** `run_e2e.py` passes `-bios` as today; verify on the Mac that `edk2-x86_64-code.fd` boots via `-bios` (it is a code-only image; if it needs `-drive if=pflash`, add a `firmware_args(path)` helper that picks the right form by filename).
4. **`setup.sh`:** report which OVMF it found, or a warning with the brew/apt hint.
5. **`make test` summary:** print a one-line tally `passed N, skipped M (uefi)` at the end so skips are visible.

## Acceptance criteria

- The scheduled `check-macos` job green; a red run blocks the next phase tag (ROADMAP §10.1).
- On the maintainer's Mac, `make test-e2e-uefi` runs (not skips).
- CI fails if OVMF is missing on Linux.

## Tests

The e2e UEFI run itself.

## Risks and rollback

macOS runners are slower and rate-limited; keep the macOS job to `make check` only.
