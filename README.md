# vibeOS

A toy x86_64 kernel in Rust, written by AI. The point is seeing how far coding agents get.

The rule is that no human writes code here. In practice someone will eventually fix a typo or babysit QEMU.

Two things matter: finding where the models fall over on low level work, and keeping the tree buildable and tested so that when one of them claims to have fixed something you can check.

If you learn something about hardware or Rust from reading this, good, but that's luck. There are better tutorials.

## Stack

- Dated Rust nightly, pinned in `rust-toolchain.toml`, with built-in `x86_64-unknown-none`
- Limine to boot, `linker.ld` for layout, `Makefile` to build the ISO, QEMU to run and test

## Status

Phases 0–9 are built: boot, memory, traps/ACPI/time, threads, SMP, console/log/shell, PCI/virtio, block
storage, filesystems including vibefs, and user mode (ring 3, syscalls, processes, userspace shell).
The 2026-09-23 [kernel review](docs/reviews/KERNEL_REVIEW.md) lists 152 findings in that code and its tests,
docs, and CI, 3 of them critical and 17 high. Among them are three ways a user process can halt the kernel
(F005, F008, F010). ROADMAP boxes the review showed false are open again or reworded to match the code,
and the ROADMAP line that fixes each finding cites its id.
Phase 10 (consolidation) is in progress; Phase 11 (portability: the aarch64 port) and Phase 12 (demand
paging / COW) are not started. See [The arc](docs/ROADMAP.md#the-arc).
Do not attach a virtio-blk disk you want to keep: every boot writes a GPT over the first one (`vda`) when
it is 512 KiB or more and its partition table is missing, empty, or unreadable (F003), and
`vibeos-ktest.iso` writes fixed sectors of any attached one (F145).
Releases: [GitHub Releases](https://github.com/devinreuschel/vibeOS/releases). From Phase 8 on, the commit that closes a phase
gets a `phase-<N>` tag and the next `v0.<m>.0` release, numbered in closing order: `v0.8.0` to `v0.14.0` are Phases 8 to 14,
later release notes name their phase, and Phase 39 is `v1.0.0` ([How to read this](docs/ROADMAP.md#how-to-read-this)).

Quickstart:

    ./setup.sh          # fetches Limine binaries, verifies host tools
    make check          # fast local gate (fmt, host clippy, host units, harness, ruff/mypy, check scripts)
    make                # kernel + vibeos.iso (hybrid BIOS/UEFI)
    make run            # QEMU window = PS/2; the terminal is COM1 (`-serial stdio`)
    make test           # host + harness units, e2e (BIOS, UEFI, panic, #GP, PIT, 9 GiB), in-guest, vibefs crash

macOS setup, including the firmware image `make test` needs there:
[AGENTS.md, How to run](AGENTS.md#how-to-run).

A previous iteration got to SMP with a preemptive scheduler before being scrapped; what survived is
written down in `docs/`.

Agents: start at [AGENTS.md](AGENTS.md).

## Docs

Docs live in [`docs/`](docs/). The other docs in the root are the changelog, [AGENTS.md](AGENTS.md) (with its
`CLAUDE.md` pointer), [CONTRIBUTING.md](CONTRIBUTING.md), and [LICENSE](LICENSE).

- [DESIGN.md](docs/DESIGN.md): invariants, boot order, address map, interrupts, time, SMP, testing, and
  a list of bugs already paid for once. Decisions, not narration.
- [ROADMAP.md](docs/ROADMAP.md): 40 phases in eight eras, from boot through self-hosting to a stable 1.0,
  all on free infrastructure, each with a goal, an exit gate, and per-part task lists; what money would
  add is a separate list of funded goals.
- [VIBEFS.md](docs/VIBEFS.md): vibefs on-disk format (version field in that file). Not DESIGN.
- [SYSCALL.md](docs/SYSCALL.md): syscall ABI. Not DESIGN.
- [reviews/](docs/reviews/): the architecture, roadmap, and kernel reviews Phase 10 comes from, and
  per-item plans in `reviews/issues/`.

Read [section 9](docs/DESIGN.md#9-pitfalls) before touching boot, paging, interrupts, syscall entry and exit, or AP bring-up.

## License

MIT, see [LICENSE](LICENSE).
