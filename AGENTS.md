# Agent instructions

Code in this tree is written by agents. Humans review, file bugs, and run the kernel.
This file is the contract. Cursor rules and `CLAUDE.md` point here. Longer material lives in `docs/`.

## Read first

1. [README.md](README.md) — status and how to build.
2. [docs/DESIGN.md](docs/DESIGN.md) [§2](docs/DESIGN.md#2-invariants) and [§9](docs/DESIGN.md#9-pitfalls) before touching boot, paging, interrupts, or AP bring-up.
3. The [ROADMAP.md](docs/ROADMAP.md) phase you are implementing. Checkboxes are the status.

`docs/INVARIANTS.md` / `docs/PITFALLS.md` are a later split (DOC2). Until then, DESIGN §2 / §9.

## Standing gates

From ROADMAP "How to read this". Not optional.

- `make check` green (fast local gate: host clippy, host units, harness, ruff/mypy when installed). Run it before every commit. `make test` green, all tiers, before every PR.
- new serial markers registered in the harness in the same commit ([DESIGN §8.3](docs/DESIGN.md#83-end-to-end))
- new portable logic gets host tests; new hardware behaviour gets a ktest
- every fixed bug gets a regression test in the cheapest tier that catches it
- `CHANGELOG.md` entry for anything visible to someone running the kernel (≤ 2 lines, user-facing)
- design docs updated in the same commit as any change to an invariant or a constant
- no `TODO` describing a correctness gap — those become lines in the ROADMAP

## Conventions

- **lib/bin pairing:** `src/foo.rs` is portable (`src/lib.rs`, host-tested). `src/foo_init.rs` is the kernel half (`src/main.rs`). Nested today: `src/arch/`, `src/fs/`. Do not invent `src/mm/` until A1. Map: [DESIGN §1.3](docs/DESIGN.md#13-module-map).
- **Emit:** `serial::line(marker::…)` / `writeln!(Serial, "vibeOS: …")` for contract lines (never filtered, captured into the log). `klog!` for everything else. `PlainSerial` only for `dmesg` and panic dumps. (`marker!` is E3, not landed.)
- **Cells:** modules roll their own `UnsafeCell` wrappers. Q3 will replace them with `BootCell` / `IrqCell` plus `SpinMutex`. Do not add another copy.
- No ephemeral "fixed X" comments ([DESIGN §1.4](docs/DESIGN.md#14-documentation-rules)).

## How to run

    ./setup.sh          # Limine clone + host-tool check (verifies pinned Limine commit)
    make check          # fast local gate (clippy, host unit, harness, ruff/mypy)
    make                # kernel + vibeos.iso
    make run            # QEMU window = PS/2; the terminal is COM1
    make test-unit      # tests/hostlib
    make test-harness   # Python harness units
    make test-e2e       # BIOS boot contract (tests/harness/run_e2e.py)
    make test-kernel    # in-guest registry (tests/harness/run_ktest.py)
    make test           # full ladder

`make help` lists targets. Optional: `pre-commit install` (ruff + `scripts/check_*.py` when those exist). rustfmt `--check` and clippy `-D warnings` landed with Q1 (#80).

`VIBEOS_*` overrides: `SMP`, `QEMU_CPU`, `MEM`, `QEMU_ACCEL` (default `tcg`), `ISO`, `TIMEOUT`, `BIOS`, `QEMU_EXTRA`. Makefile `?=` defaults are the source for `make run`. One reader: `tests/harness/harness.py` (`env_config`).

macOS: `brew install qemu xorriso nasm python`. Hostlib and QEMU e2e work. Kernel-crate `cargo test --lib` is not portable yet (A2). UEFI e2e needs OVMF (`OVMF=`); Homebrew qemu ships `share/qemu/edk2-x86_64-code.fd`.

## Toolchain bump (C1)

Bump the date in `rust-toolchain.toml` and the matching `toolchain:` inputs in
`.github/workflows/ci.yml`, `.github/workflows/smp-stress.yml`, and
`.github/workflows/release.yml` in one PR.
`make test` must be green. Do not re-introduce an undated nightly except the
weekly canary job in `smp-stress.yml`. Not a drive-by.

## Do not

- commit build products (`vibeos*.iso`, `iso_root*`, `initrd.fat`, `target*/`, `limine/`)
- edit `limine/` (cloned by `setup.sh`)
- add dependencies without a note in the PR
- disable a test to make CI green
