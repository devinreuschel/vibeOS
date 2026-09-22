# DX1 · A fast local gate (`make check`) and formatter/linter configuration

**Status:** implemented (with B1). rustfmt `--check` as a hard fail is Q1.

| | |
|---|---|
| **Area** | 4.13 Developer experience |
| **Impact / Effort / Phase** | Medium / S / I |
| **Depends on** | Q1 (gates), B1 (`make check` target), A2 (runs on macOS) |
| **Blocks** | — |
| **Review** | [ARCHITECTURE_REVIEW.md §4.13](../ARCHITECTURE_REVIEW.md#413-developer-experience--tooling) |

## Problem

The only documented local gates are `make test-e2e` (one QEMU boot) and `make test` (twelve boots, ten kernel links). There is no `make check`, no `rustfmt.toml`/`clippy.toml`, and no Python lint or type configuration for ~1,400 lines of harness code that is itself unit-tested. `setup.sh` verifies host tools but not the Rust components (`rust-src`, `llvm-tools`) or the pinned toolchain.

## Recommended fix

`make check` in under a minute on any host; `ruff` + `mypy --strict` for `tests/`; `setup.sh` installs the toolchain and components; an optional `pre-commit` config.

## Implementation plan

1. **`make check`** (B1 step 3): fmt, clippy, `test-unit`, `test-harness`, guard scripts; plus `ruff check tests scripts` and `mypy tests scripts` when the tools are installed (`command -v ruff` guard; print a one-line hint otherwise).
2. **`pyproject.toml`:** `[tool.ruff] line-length = 100, target-version = "py311"`, `[tool.ruff.lint] select = ["E","F","I","UP","B"]`; `[tool.mypy] strict = true, files = ["tests", "scripts"]`. Fix the findings (the harness already uses type hints throughout).
3. **`setup.sh`:** `rustup toolchain install $(pinned) --component rust-src llvm-tools`; `rustup target add x86_64-unknown-none` (after B2); check `python3 --version` ≥ 3.11; report ruff/mypy presence.
4. **`.pre-commit-config.yaml`** (optional): `cargo fmt --check`, `ruff`, and `scripts/check_*.py`; documented in `AGENTS.md` as optional.
5. **`make help`** (B1) lists `check` first.
6. **Docs:** README quickstart adds `make check`; `AGENTS.md` says "run `make check` before every commit, `make test` before every PR".

## Acceptance criteria

- `time make check` < 60 s on a warm cache on the maintainer's Mac.
- `ruff check` and `mypy` clean on `tests/` and `scripts/`.

## Tests

None beyond the gates themselves.

## Risks and rollback

None.
