# Changelog

All notable changes to **vibeOS** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

One or two lines per entry. Say what changed for someone running vibeOS (new command, new
marker, new device, fixed hang). Link to the ROADMAP section instead of describing the design.

## [Unreleased]

### Added

- SMEP/SMAP/UMIP and `CR0.WP` on every CPU (`arch::cpu::harden`). `/dev/random` prefers
  virtio-rng, then RDRAND, then xorshift.
- [Phase 9](docs/ROADMAP.md#phase-9-user-mode-and-processes): ring 3, syscalls, ELF `/hello`,
  `fork`/`execve`/`wait4`, userspace `/sbin/init` and `/bin/sh`. Phase 9 exit; COW is Phase 12.
- MIT license (`LICENSE`). Both crate manifests declare `license = "MIT"`.
- Tracked `AGENTS.md` (DOC3). Cursor rules and `CLAUDE.md` point at it.
- `make check` as the fast local gate (host clippy, host units, harness, ruff/mypy).
  `make help` lists targets.
- `v*` tags publish `vibeos.iso` and `vibeos-ktest.iso` on GitHub Releases.
  Green `main` CI keeps a 7-day `vibeos.iso` artifact.

### Changed

- Roadmap restructured to 40 phases in eight eras, all on free infrastructure: [Phase 10 Consolidation](docs/ROADMAP.md#phase-10-consolidation)
  and [Phase 11 Portability](docs/ROADMAP.md#phase-11-portability) added; old phases 10–20 are now 12–22.
- CI: `check` job (`make check` + `vibeos-core` llvm-cov floor 87%) runs before the QEMU ladder.
  Host builds deny warnings; kernel builds do not yet.
- Crate version is `0.8.0` (Phase 8 backfill). Changelog cut; entries are ≤ 2 lines.
- Nightly date, Action SHAs, and Limine commit are pinned; `ruff`, `mypy`, and the `ubuntu-latest`
  runner image are not. Weekly smp-stress runs a non-blocking latest-nightly canary.
- Kernel builds with built-in `x86_64-unknown-none` (no custom target JSON, no `build-std`).
- Host tests (`make test-unit`) run `vibeos-core` on the host triple; `mkfs`/`fsck-vibefs` follow.
  Context-switch asm stays `x86_64`. I1 (macOS CI job) is still parked.
- Initrd is `build/initrd.fat` from hostlib `mkinitrd`. Bare `cargo build` embeds an empty image;
  run `make`. The AP trampoline is `global_asm!`, not nasm.
- ISO and ksyms recipes are one `KERNEL_VARIANT` template plus `scripts/mkiso.sh`.
- One QEMU launcher (`tests/harness/harness.py`) for e2e, ktest, PS/2, and vibefs-crash.
  `VIBEOS_*` is read there; drivers are `tests/harness/run_*.py`.
- README / DESIGN header / §1.3 match the tree. O1 superseded: Phase 9 closed 2026-09-22;
  no pause/resume note.
- Portable crate denies clippy `unwrap`/`expect`/`panic!` (E1). Parsers still return
  `Result` on bad data; `indexing_slicing` stays warn until those modules are clean.
- The harness retries a timed-out e2e or ktest boot, two known ktest FAIL lines, and at `-smp 4` a ktest
  panic in `ipi_init::wait_acks` or on the same serial line as `ktest: ok`; [ROADMAP §10.2](docs/ROADMAP.md#102-build-and-harness) removes them.

### Fixed

- Boot with more than 8 GiB of RAM no longer triple-faults after `heap ok`. RAM above the
  8 GiB physmap cap is left unused; `make test-e2e-highmem` boots with 9 GiB.
- UEFI e2e no longer hangs on OVMF PXE after a green marker boot; a timed-out boot prints
  its serial tail.

## [0.8.0]

Phase 8 exit: filesystems. Phases 0–8 in one cut. See [the arc](docs/ROADMAP.md#the-arc).
Not yet tagged: the 2026-09-23 kernel review reopened Phase 8's gate lines, so `v0.8.0` is cut, with this
section as its notes, when they close (ROADMAP, How to read this).

### Added

- [Phase 0](docs/ROADMAP.md#phase-0-ignition): hybrid BIOS+UEFI ISO, COM1 contract, panic halt,
  Python e2e harness, CI ladder.
- [Phase 1](docs/ROADMAP.md#phase-1-memory): buddy PMM, own page tables (`paging: cr3 ok`), heap,
  KVA, in-guest ktests (`vibeos-ktest.iso`).
- [Phase 2](docs/ROADMAP.md#phase-2-traps-acpi-and-time): GDT/IDT/PIC, ACPI, PIT/HPET/TSC
  (`time: tsc`). `int3` returns; `#GP` dumps and halts.
- [Phase 3](docs/ROADMAP.md#phase-3-threads-and-scheduling): kernel threads, preemptive RR,
  `sleep_ms`, idle, wait queues and blocking primitives.
- [Phase 4](docs/ROADMAP.md#phase-4-smp): LAPIC/IOAPIC, AP bring-up, per-CPU run queues, IPI
  shootdown. `smp: done`. TCG is the guest-test default.
- [Phase 5](docs/ROADMAP.md#phase-5-console-input-and-logging): FB console, PS/2, log ring,
  kernel shell. Markers `console ok` then `shell ready`.
- [Phase 6](docs/ROADMAP.md#phase-6-device-model-and-buses): PCI, MSI-X, DMA, modern virtio,
  workqueue/threaded IRQ. `pci: N devices`. Shell `lspci` / `devices`.
- [Phase 7](docs/ROADMAP.md#phase-7-block-storage): block layer, ramdisk, virtio-blk, GPT/MBR
  children, write-back cache. `block: <name> <n> sectors`. Persist round-trip.
- [Phase 8](docs/ROADMAP.md#phase-8-filesystems): VFS, FAT32 initrd root (`ls`/`cat`/`mkdir`/`rm`/`cp`),
  `/dev` `/proc` `/tmp` `/sys`, vibefs with host `mkfs`/`fsck` crash test. Phase 8 exit.
- `make test-ps2` types `echo ps2-ok` through QEMU `sendkey` on the same i8042 as the window.

### Changed

- `vibeOS: pic: remapped` means the PIC boot step finished (ICW ran, or FADT skipped the ports).
- `-Z build-std` lives on the Makefile `CARGO` line so hostlib does not compile a second `core`.
- ktest FAIL lines match skip: `vibeOS: ktest: FAIL <name>: <why>`.
- Boot-done marker is `vibeOS: boot: phase1 done` (was `phase0 done`); later `shell ready`.
- CI cancels superseded GitHub Actions runs for the same branch or PR.

### Fixed

- QEMU window / PS/2 keyboard reaches the shell: 8042 config clears keyboard-clock-off, and
  IRQ1 is IOAPIC-only once the LAPIC owns the tick.
- `now_us` / `now_ns` stay monotonic under TCG `hlt` (no backwards step after a late tick).
- `sleep_ms_50` and `tsc_calib_source` hold under TCG `-smp 4` (wider band when invariant TSC
  is absent; ~50 ms of `now_us` when ticks coalesce).
- Recycled IRQ vectors no longer keep a dead threaded handler after virtio probe-fail.
- virtio-rng tears down MSI-X, the vector, and device status if the virtqueue fails after arming.
- Freeing an I/O APIC vector masks the GSI first so a still-asserted level line cannot storm.
- `lspci` / `devices` copy one device at a time and no longer overflow the 16 KiB shell stack.
- ECAM reads that miss the 64-page cache still use the mapped config window (later buses stay visible).
- Framebuffer `\r` homes the column so the shell prompt does not reprint on every key.
- PS/2 typematic no longer retoggles Caps/Num or re-enqueues modifiers.
- Kernel log emit stays IRQ-off; `dmesg` uses a plain serial path and does not recapture.
- Shootdown and call-function wait IRQ-off so two concurrent unmaps cannot deadlock.
- Heap grow/retry no longer OOMs or walks page tables while holding HEAP.
- First-run threads keep IF on; `switch_context` no longer `popfq`s into a tick.
- Dead stacks reap from idle and voluntary no-switch return, never from the IRQ path.
- Seqlock timekeeping never publishes a torn (tick, tsc) pair.
- `make test-e2e-pit` uses `-machine pc,hpet=off` (QEMU 10 rejects `-no-hpet`).
- `thread_exit` into idle no longer leaves `irq_nest` raised.
- TSC-deadline arm is LVT timer write, then `MFENCE`, then `IA32_TSC_DEADLINE`.
- Condvar/RwLock timeouts wake the right waiters; spawn reuses Dead TCBs without growing the heap.

[Unreleased]: https://github.com/devinreuschel/vibeOS/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/devinreuschel/vibeOS/releases/tag/v0.8.0
