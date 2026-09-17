# Changelog

All notable changes to **vibeOS** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Phase 0 kernel: `_start` verifies the Limine base revision, brings up COM1,
  and prints the phase 0 marker contract before halting.
- `x86_64-unknown-none-executable.json` custom target: no PIE, static reloc,
  `code-model: kernel`, `disable-redzone`, `+soft-float`, no RELRO.
- `linker.ld` at `0xFFFF_FFFF_8000_0000` with `.got` inside the mapped image,
  page-aligned sections, and exported `__kernel_vma_{start,end}`.
- Library / binary split from the first kernel commit: `src/lib.rs` holds
  portable modules (`fmt_util`, `marker`, `uart`) with unit tests, `src/main.rs`
  and the binary-only modules (`serial`, `panic`, `x86`) hold hardware pokes.
- `panic-test` build feature + `make run-panic` / `make test-e2e-panic` for
  exercising the panic path end to end (file, line, message, halt without reboot).
- Hybrid BIOS + UEFI ISO built via `xorriso` + `limine bios-install`. `make run`
  boots BIOS by default at `-smp 2`; `make test-e2e-uefi` covers the OVMF path.
- Python e2e harness (stdlib only) with ordered marker assertion, panic-signature
  fast-fail, monitor-quit early exit, and its own unittest suite under
  `make test-harness`.
- `setup.sh` fetches the pinned Limine binary tag and builds the `limine`
  host tool; verifies `qemu-system-x86_64`, `xorriso`, `nasm`, `python3`.
- `make layout` for section table + exported symbols via the toolchain's
  bundled `llvm-objdump` / `llvm-nm`.
- GitHub Actions workflow on push and pull request running the phase 0 ladder.
- `docs/DESIGN.md` and `docs/ROADMAP.md`. Root keeps only the readme and this file.
- Design doc section 9 lists bugs with the rule that prevents each one. Worth
  reading before boot, paging, interrupt, or SMP work.
