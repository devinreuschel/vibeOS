# Changelog

All notable changes to **vibeOS** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Phase 1 slice B: 4-level page tables + MMIO attributes. `vibeos::paging`
  in the library half owns typed `PhysAddr` / `VirtAddr`, PTE flag
  constants (including `PTE_UC = PCD|PWT`), and a `PageTables` walker
  with `map_page`, `map_2m`, `map_range_4k`, `map_range_2m`,
  `unmap_page`, `translate`, and `patch_physmap_uc`. Address map
  constants (`LOW_IDENT_LIMIT`, `PHYSMAP_CAP`, `MMIO_WINDOW_*`) match
  DESIGN §4.1 and are host-tested. Host tests cover 4 KiB / 2 MiB
  map+translate+unmap, range mapping, overlap and misalignment
  rejection, frame exhaustion, and PCD+PWT patching without splitting a
  2 MiB entry.
- `paging_init` (binary half) builds a fresh PML4 from buddy frames and
  installs it: kernel image per section (`.text` X+RO, `.rodata` RO+NX,
  `.data`/`.bss` RW+NX), physmap at Limine's HHDM offset in 2 MiB pages
  up to `map_end` (usable RAM / kernel image end / framebuffer extent,
  capped at 8 GiB), low identity window [0, 512 MiB) with the first
  2 MiB executable so the AP trampoline can run there, and the PML4
  slot Limine used for the current RSP borrowed verbatim so the
  bootloader stack keeps working across `mov cr3`. `EFER.NXE` set
  before install. `paging::flush_all_cpus` wraps `invlpg` as a
  single-CPU shootdown no-op so phase 4 changes one function, not fifty
  call sites (DESIGN §4.3). `pmm_init::buddy_mut()` exposed so the
  paging bringup and later `ioremap` can pull frames from the same
  allocator.
- `paging_init::patch_physmap_uc` and `paging_init::ioremap` wired
  through to the walker; both are hooks pending phase 2 (LAPIC / I/O
  APIC / HPET) callers and gated with `#[allow(dead_code)]` until then.
- Boot markers: `vibeOS: paging: cr3 ok` and `vibeOS: paging: mmio uc`,
  emitted in order after the PMM line per DESIGN §3.3 steps 7 and 8.
  `PHASE0_MARKERS` grown to assert both new lines are present in the
  correct order; the hostlib smoke test pins both to their exact
  strings so the harness and the code cannot drift.
- `src/x86.rs` gains `rdmsr`, `wrmsr`, `read_cr3`, `write_cr3`,
  `invlpg`, and `enable_nxe`.
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
