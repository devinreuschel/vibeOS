# R1 · Tidy the root, `tests/`, and remote branches

| | |
|---|---|
| **Area** | 4.14 Repository structure |
| **Impact / Effort / Phase** | Low / S / I |
| **Depends on** | B1 (paths in one place), T2 (tests layout) |
| **Blocks** | — |
| **Review** | [ARCHITECTURE_REVIEW.md §4.14](../ARCHITECTURE_REVIEW.md#414-repository-structure--organization) |

## Problem

- Build products land in the repo root: `vibeos*.iso`, `iso_root*/`, `initrd.fat`, `target-*/`, `limine/` (all gitignored, ten `.gitignore` entries wide).
- `tests/` mixes a Python package (`harness/`), two loose drivers (`kernel_boot.py`, `vibefs_crash.py`), an empty `e2e/`, stray `__pycache__` directories, and a Rust crate (`hostlib/`).
- The remote has 30+ merged `feature/*` branches and a local `phase_0` branch.

## Recommended fix

Build products under `build/`; Python under `tests/harness/`; the Rust host crate under `crates/` or `tools/` after the workspace split; merged branches pruned automatically.

## Implementation plan

1. **`build/`:** Makefile `ISO := build/vibeos.iso` (and the four variants), `ISO_ROOT := build/iso_root_<variant>`, `initrd.fat` → `build/initrd.fat` (B4), ksyms files → `build/`. `CARGO_TARGET_DIR` stays `target/`. `.gitignore` collapses the ten entries into `/build/`, `/target*/`, `/limine/`. `make clean` removes `build/` and `target*/`; `distclean` also `limine/`.
2. **`tests/`:** per T2, everything Python in `tests/harness/`; delete `tests/e2e/`; after A2 step 5 move `tests/hostlib` to `crates/hostlib` (it is a crate, not a test directory) and update the Makefile.
3. **Branches:** enable "Automatically delete head branches" in the GitHub repo settings; one-time cleanup `git branch -r --merged origin/main | grep 'origin/feature/' | sed 's#origin/##' | xargs -n1 git push origin --delete` after the maintainer confirms; delete local `phase_0` (`git branch -d phase_0`, it is merged).
4. **Root listing goal:** `Cargo.toml Cargo.lock Makefile README.md CHANGELOG.md LICENSE AGENTS.md CLAUDE.md rust-toolchain.toml rustfmt.toml pyproject.toml build.rs linker.ld limine.conf setup.sh crates/ docs/ scripts/ tests/ .cargo/ .github/ .cursor/`.

## Acceptance criteria

- `git status --ignored` at the root shows only `build/`, `target/`, `limine/`.
- `ls tests` shows `harness/` (and `hostlib/` until it moves).
- `git branch -r | wc -l` ≤ 3.

## Tests

None.

## Risks and rollback

Branch deletion is irreversible for unmerged work; the command above only touches branches merged into `main`.
