# Changelog

All notable changes to **vibeOS** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Seqlock `TickClock::write` odd-bumps with `fetch_add(AcqRel)` and
  stores (tick, tsc) as atomics, then Release-publishes the even
  sequence. Relaxed load/store on the odd bump let a torn pair stay
  visible while seq still looked even
  (`seqlock_threaded_writer_never_tears`).

### Added

- Phase 2 slice C: PIT bootstrap tick, TSC calibration, seqlock
  timekeeping, RTC wall-clock offset. After `acpi: xsdt`, boot prints
  `vibeOS: time: calibrated hpet|pit <n>/ms` then the exit-gate
  `vibeOS: time: tsc <n>/ms`. IRQ0 is unmasked and `sti` runs after
  calibration (keyboard stays masked; `irq: enabled` is still Phase 3).
  `uptime` reports tick milliseconds next to TSC microseconds.
  If FADT bit 0 skipped the boot PIC remap, the timer path still
  programs the 8259 so IRQ0 is vector `0x20` rather than `#DF`.
- PIT channel 0 mode 2, divisor 1193 (~1 kHz), `io_wait` between
  divisor bytes. IRQ0 handler increments the tick, snapshots TSC, EOI,
  returns — no alloc, no logging. Channel 2 one-shot via port `0x61` is
  the HPET-less calibration path (count 11932, ~10 ms).
- TSC: invariant-TSC CPUID check (loud log if absent), `lfence`/`rdtscp`,
  HPET main counter over ~10 ms when the ACPI table is usable, PIT
  channel 2 fallback. `tsc_per_ms` lives on the BSP `TimeState` (the
  per-CPU slot Phase 4 will own). Poison frequencies are refused.
  `busy_wait_ms` spins on TSC and `hlt`s when IF is set.
- `vibeos::time` in the library half: interpolation including near
  `u64::MAX`, seqlock retry under a simulated concurrent writer with an
  independent published timestamp, `next_deadline`. Host tests cover
  those plus PIT/HPET calib math and wall-clock offset.
- In-guest: PIT ~1 kHz, `now_us` monotonic over 10k reads (straight-line
  and under `hlt` yields), HPET vs PIT-ch2 agreement, uptime sides, RTC
  offset. `make test-e2e-pit` boots the production ISO with `-no-hpet`.

- Phase 2 slice A: GDT/TSS/IST, IDT/exceptions, 8259 PIC. Boot prints
  `vibeOS: gdt ok`, `vibeOS: pic: remapped`, `vibeOS: idt ok` after
  `kva: ready` (IST stacks come from KVA guarded stacks; relative order
  matches DESIGN §3.3), then the ACPI `xsdt` marker from slice B. PIC
  reads FADT `iapc_boot_arch` bit 0 and skips the ICW sequence when the
  legacy 8259 is absent; missing FADT still remaps+masks. `pic: remapped`
  is emitted after that step either way.
- Flat GDT with sysret selector order (null, kernel code/data, user
  data, user code, TSS). Per-CPU `CpuTables` (GDT+TSS) with RSP0 and
  IST1–4 (DF/NMI/MC/debug). 256-entry IDT, `x86-interrupt` handlers;
  `#BP` logs and returns; `#UD`/`#GP`/`#PF`/`#DF`/`#MC` dump RIP/CS/
  RFLAGS/RSP/SS/error/CR2 and halt. Scoped catcher for tests (longjmp
  or step RIP). Named vector constants with a host uniqueness test.
- 8259 remap to 0x20/0x28 with `io_wait`, mask-all, `mask`/`unmask`/
  `disable_all`, spurious IRQ7/15 without a bogus EOI.
- In-guest: `int3` roundtrip, scoped `#PF` skip, `#GP` catch, DF-on-IST
  via a poisoned RSP. `make test-e2e-gp` boots a `gp-test` kernel that
  dumps `#GP` and halts.
- Host tests for GDT/TSS/IDT packing, sysret selector arithmetic, PIC
  ICW plan, FADT skip policy, spurious EOI policy, and vector uniqueness.
- Phase 2 slice B: ACPI discovery. `vibeos::acpi` in the library half
  validates RSDP (signature, v1 20-byte checksum, v2 extended checksum),
  walks XSDT with per-table checksums (RSDT fallback), and parses MADT
  (LAPIC base, type 5 override, I/O APIC+GSI, type 2 ISOs, enabled APIC
  IDs), HPET (rejecting zero addresses and I/O-space GAS), FADT
  (`iapc_boot_arch` bit 0, reset/sleep GAS), and MCFG (ECAM base stored
  for phase 6; ECAM is not walked). Packed fields go through
  `read_unaligned_*`. Host tests cover RSDP checksum rejection, HPET
  validation, and MADT iteration over truncated / zero-length input.
- Kernel `acpi_init` reads Limine's RSDP (physical at base revision 3),
  maps any ACPI-table pages that sit outside `map_end`, UC-patches
  discovered LAPIC / I/O APIC / HPET physmap leaves via
  `patch_physmap_uc` (mapping missing 4 KiB leaves first — those bases
  sit above a 128 MiB physmap), then reads the HPET GEN_CAP period.
  Emits `vibeOS: paging: mmio uc` only when a real leaf was patched, and
  `vibeOS: acpi: xsdt <n> tables` after GDT/PIC/IDT, plus a summary of
  CPU count, I/O APIC count, and HPET presence. In-guest `acpi_discovery`
  checks table counts against QEMU and that LAPIC/IOAPIC/HPET physmap
  leaves are UC.

- Phase 1 slice C: kernel heap, KVA allocator, in-guest tests, meminfo.
  Free-list heap at `HEAP_START` (1 MiB initial, grows in 4 KiB steps to
  the 64 MiB cap), `GlobalAlloc` with interrupts off, and
  `#[alloc_error_handler]` panicking with the failed `Layout`. KVA is a
  first-fit range allocator over the 64 GiB window; `alloc_guarded_stack`
  maps order-0 frames above an unmapped guard, `vmap` presents
  non-contiguous frames contiguously, and a deferred-free list has an
  explicit drain. Boot prints `vibeOS: heap ok` then `vibeOS: kva: ready`,
  then a shell-less meminfo dump (PMM totals, heap used/capacity, KVA
  used) plus a coalesced page-table range walk.
- `kernel_tests` feature builds a second kernel into `target-kernel-tests`
  / `vibeos-ktest.iso`. After init it runs a registry over serial
  (`ktest: begin` / `ok` / `FAIL` / `skip` / `end`) and exits QEMU via
  `isa-debug-exit` at `0xf4` (`0x10` pass, `0x11` fail).
  `make test-kernel` drives `tests/kernel_boot.py`. In-guest coverage:
  map/unmap, NX instruction-fetch, heap Box/growth/align/reuse/OOM,
  stack guard-page fault, KVA frame-count roundtrip, deferred drain,
  vmap, and physmap UC PTE flag read-back (§1.3).
- Host tests for the heap (alignment, reuse, OOM-without-corruption,
  extend, coalesce, double-free), the KVA first-fit/tail-free rules, and
  page-table range walking.
- Harness contract picks up `vibeOS: heap ok` and `vibeOS: kva: ready`
  between `paging: cr3 ok` and boot-done.

### Changed

- `vibeOS: pic: remapped` means the PIC boot step finished: ICW
  remap+mask ran, or FADT skipped the ports. Unlike `paging: mmio uc`,
  it is not a claim that hardware was programmed.

- `-Z build-std` moved off `.cargo/config.toml` onto the Makefile `CARGO`
  line. Cargo merges parent config into `tests/hostlib`, and an inherited
  `build-std` compiles a second `core` that collides with std.
- `ktest` FAIL lines match skip: `vibeOS: ktest: FAIL <name>: <why>`.
- Boot-done marker is `vibeOS: boot: phase1 done` (was `phase0 done`).

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
  frames` as the phase-1 slice-A marker, followed by a diagnostic
  totals/largest-order line.
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
