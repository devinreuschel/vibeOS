# T4 · Fuzz the pure parsers now

| | |
|---|---|
| **Area** | 4.6 Testing / 4.8 Security |
| **Impact / Effort / Phase** | Medium / S / II |
| **Depends on** | A2 (hostlib builds natively) |
| **Blocks** | — |
| **Review** | [ARCHITECTURE_REVIEW.md §4.6](../ARCHITECTURE_REVIEW.md#46-testing-strategy--coverage) |

## Problem

The byte-slice-in, `Result`-out parsers already have test doubles: `acpi::walk<P: PhysMem>` and `parse_rsdp/parse_madt/parse_hpet/parse_fadt/parse_mcfg`, `part::parse_image(disk, sector_size)`, `fat::FatVol::mount(&mut MemDisk)`, `vibefs::{probe, mount, fsck}(&mut MemDisk)`, `pci::read_function<C: CfgIo>` and `walk_caps`, `virtio::read_modern_caps`, `shell::tokenize`, `kbd` scan-code decoding. ROADMAP §10.2 schedules the parser fuzzers now and §18.5 the syscall, ELF, and network fuzzers; nothing blocks doing it today with `cargo-fuzz` (nightly is already required).

## Recommended fix

A `tests/fuzz` crate with one target per parser, seeded from the generators the tests already have, run for a bounded time weekly; crashes become regression tests.

## Implementation plan

1. **Crate:** `cargo fuzz init` inside `tests/fuzz` with `vibeos-hostlib-tests = { path = "../hostlib" }` (or `vibeos-core` after A2 step 5). Targets in `tests/fuzz/fuzz_targets/`:
   - `acpi_walk.rs`: `PhysMem` over the input bytes at a fake base; `acpi::walk(&mem, base)`.
   - `part_parse.rs`: `part::parse_image(data, 512)`.
   - `fat_mount.rs`: `MemDisk::new(&mut data)` then `FatVol::mount`, then a fixed op script (lookup root entries, read first file, mkdir, unlink) ignoring `Err`.
   - `vibefs_mount_fsck.rs`: `vibefs::fsck` then `mount`, then the same op script; assert `fsck` never reports `errors == 0` for a tree that then fails to mount (consistency oracle).
   - `pci_function.rs`: `CfgIo` backed by the input; `read_function`, `walk_caps`, `read_msi_cap`, `read_msix_cap`.
   - `shell_tokenize.rs`, `kbd_decode.rs`.
2. **Seeds:** write `fat::mkinitrd` output, `vibefs::mkfs` output, the real MBR/GPT blobs from `part.rs` tests, and the ACPI fixtures from `acpi.rs` tests into `tests/fuzz/corpus/<target>/` via a small `seed.rs` example.
3. **Weekly:** in `smp-stress.yml`, `cargo fuzz run <target> -- -max_total_time=300` per target, `continue-on-error: false`; artifacts uploaded on crash.
4. **Regressions:** any crash input is minimized (`cargo fuzz tmin`) and committed under `tests/fuzz/regressions/<target>/`; a hostlib `#[test]` replays every file in that directory through the target function so the fix is guarded in the fast tier.
5. **Docs:** DESIGN §8.1 gains a "fuzz" paragraph; ROADMAP §18.5 references it as already started.

## Acceptance criteria

- Seven targets build; each runs 60 s locally without a crash (or with crashes turned into regression tests).
- Weekly job present; regression replay test present.

## Tests

The replay test.

## Risks and rollback

`cargo-fuzz` needs `-Z sanitizer` support on the host; on macOS use `--sanitizer none` (libFuzzer still works). None of this touches the kernel build.
