# Changelog

All notable changes to **vibeOS** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Phase 1 slice B: page tables + MMIO attributes. `vibeos::paging` in the
  library half carries typed `PhysAddr` / `VirtAddr` newtypes, a
  `PageFlags` bitset, a 4-level walk over 4 KiB and 2 MiB leaves, and
  `map_page` / `map_range` / `unmap_page` / `translate` plus
  `patch_physmap_uc` and an `IoremapWindow` bump reservation. The
  mapper refuses to silently overwrite a present leaf (`MapMode::Fresh`
  vs `Remap`) and refuses to split a 2 MiB leaf on a 4 KiB request. Host
  tests cover index math, canonicalization, PTE round-trip, 4 KiB and
  2 MiB round-trips, `map_range` picking 2 MiB when aligned, overlap
  rejection, size-mismatch rejection, `patch_physmap_uc` preserving the
  2 MiB page size, and the ioremap window's bump/exhaustion behavior.
- Kernel-side `paging_init::install` builds a fresh PML4 from buddy
  frames per DESIGN §4.3: kernel `.text` / `.rodata` / `.limine_requests`
  / `.data+bss` mapped with per-section permissions, physmap at HHDM
  with 2 MiB pages over `[0, map_end)` (capped at 8 GiB), 512 MiB low
  identity with the first 2 MiB left executable for the AP trampoline,
  and — when `rsp` falls outside our own map — a duplicate of Limine's
  covering PML4 entry so the stack survives `mov cr3`. `EFER.NXE`
  is set before install, `mov cr3` loads the fresh root, and the exit
  marker `vibeOS: paging: cr3 ok` (DESIGN §3.3 step 7) fires.
- `paging_init::patch_physmap_uc` and `paging_init::ioremap` wire the
  §1.3 API to the kernel; both `invlpg` locally after every edit and
  call `paging::tlb_shootdown_others`, a single-CPU no-op today that
  phase 4 replaces with the IPI 0xFC path (DESIGN §7.9).
- Harness contract picks up `vibeOS: paging: cr3 ok` between the PMM
  free-frames line and the boot-done marker (DESIGN §8.3), still using
  `Marker.and_contains` for the PMM line's shape assertion.
- `x86::read_cr3`, `write_cr3`, `invlpg`, `rdmsr`, `wrmsr`, `read_rsp`,
  plus `IA32_EFER` / `EFER_NXE` constants. All in the binary crate.
- Phase 1 slice A: buddy physical allocator. `vibeos::pmm::Buddy` lives in
  the library half with intrusive doubly-linked free lists inside the free
  pages, splitting on allocate and merging on free up to 4 MiB blocks
  (`MAX_ORDER = 10`). O(1) running free-frame counter; `stats()` reports
  total, free, and the largest available order. Double-free is caught even
  after coalescing by scanning every covering order. Host tests cover the
  full phase-1 checklist: exhaustion, per-order alignment, coalescing after
  freeing alternate blocks, a random alloc/free stream that restores the
  initial free count, and double-free panics.
- Kernel-side `pmm_init` walks Limine's memmap and hands `USABLE` regions
  to the buddy after subtracting frame 0, the loaded kernel image (via
  Limine's executable-address response), the AP trampoline page at
  `0x8000`, and every framebuffer. Prints `vibeOS: pmm: <n> free 4KiB
  frames` as the phase-1 exit-gate marker, followed by a diagnostic
  totals/largest-order line. Paging, heap, and KVA are deliberately
  deferred to slices B–D.
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
