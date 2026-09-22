# vibeOS

A toy x86_64 kernel in Rust, written by AI. The point is seeing how far coding agents get.

The rule is that no human writes code here. In practice someone will eventually fix a typo or babysit QEMU.

Two things matter: finding where the models fall over on low level work, and keeping the tree buildable and tested so that when one of them claims to have fixed something you can check.

If you learn something about hardware or Rust from reading this, good, but that's luck. There are better tutorials.

## Stack

- Dated Rust nightly, pinned in `rust-toolchain.toml`, with built-in `x86_64-unknown-none`
- Limine to boot, `linker.ld` for layout, `Makefile` to build the ISO, QEMU to run and test

## Status

Phases 0–9 landed: boot, memory, traps/ACPI/time, threads, SMP, console/log/shell, PCI/virtio, block
storage, filesystems including vibefs, and user mode (ring 3, syscalls, processes, userspace shell).
Phase 10 (demand paging / COW) is not started. See [The arc](docs/ROADMAP.md#the-arc).
Releases: [GitHub Releases](https://github.com/devinreuschel/vibeOS/releases) (`v0.<phase>.0` from Phase 8; first cut is `v0.8.0`).

Quickstart:

    ./setup.sh          # fetches Limine binaries, verifies host tools
    make check          # fast local gate (clippy, host unit, harness, ruff/mypy)
    make                # kernel + vibeos.iso (hybrid BIOS/UEFI)
    make run            # QEMU window = PS/2; the terminal is COM1 (`-serial stdio`)
    make test           # host units + harness units + e2e (BIOS, UEFI, panic) + in-guest

macOS: `brew install qemu xorriso nasm python`. `make test-unit` (hostlib) and QEMU e2e work. Host
tests of the kernel crate itself are not portable yet. UEFI e2e needs OVMF (`OVMF=/path/to/OVMF.fd`);
Homebrew qemu ships `share/qemu/edk2-x86_64-code.fd`.

A previous iteration got to SMP with a preemptive scheduler before being scrapped; what survived is
written down in `docs/`.

Agents: start at [AGENTS.md](AGENTS.md).

## Docs

Docs live in [`docs/`](docs/). Root stays at a readme and a changelog.

- [DESIGN.md](docs/DESIGN.md): invariants, boot order, address map, interrupts, time, SMP, testing, and
  a list of bugs already paid for once. Decisions, not narration.
- [ROADMAP.md](docs/ROADMAP.md): 21 phases from boot to self-hosting, each with a goal, an exit gate,
  and per-part task lists.
- [VIBEFS.md](docs/VIBEFS.md): vibefs on-disk format (version field in that file). Not DESIGN.
- [SYSCALL.md](docs/SYSCALL.md): syscall ABI. Not DESIGN.

Read [section 9](docs/DESIGN.md#9-pitfalls) before touching boot, paging, interrupts, or AP bring-up.

## License

MIT, see [LICENSE](LICENSE).
