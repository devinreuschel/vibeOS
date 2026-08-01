# vibeOS

A toy x86_64 kernel in Rust, written by AI. The point is seeing how far coding agents get.

The rule is that no human writes code here. In practice someone will eventually fix a typo or babysit QEMU.

Two things matter: finding where the models fall over on low level work, and keeping the tree buildable and tested so that when one of them claims to have fixed something you can check.

If you learn something about hardware or Rust from reading this, good, but that's luck. There are better tutorials.

## Stack

- Rust nightly with `rust-src`, for `-Z build-std`
- Custom target spec: `x86_64-unknown-none-executable.json`
- Limine to boot, `linker.ld` for layout, `Makefile` to build the ISO, QEMU to run and test

## Status

Nothing yet. The tree is fresh and phase 0 is not done. A previous iteration got to SMP with a
preemptive scheduler before being scrapped; what survived is written down in `docs/`.

## Docs

Two files in [`docs/`](docs/). Root stays at a readme and a changelog.

- [DESIGN.md](docs/DESIGN.md): invariants, boot order, address map, interrupts, time, SMP, testing, and
  a list of bugs already paid for once. Decisions, not narration.
- [ROADMAP.md](docs/ROADMAP.md): 21 phases from boot to self-hosting, each with a goal, an exit gate,
  and per-part task lists.

Read [section 9](docs/DESIGN.md#9-pitfalls) before touching boot, paging, interrupts, or AP bring-up.

## License

all rights reserved for now.
