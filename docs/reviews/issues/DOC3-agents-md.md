# DOC3 · Version the agent instructions in the repo (`AGENTS.md`)

**Landed** in the docs/meta PR (2026-09-22).

| | |
|---|---|
| **Area** | 4.12 Documentation |
| **Impact / Effort / Phase** | High / S / I |
| **Depends on** | — |
| **Blocks** | — |
| **Review** | [ARCHITECTURE_REVIEW.md §4.12](../ARCHITECTURE_REVIEW.md#412-documentation) |

## Problem

- The only operating instructions for agents, `.cursor/rules/general.mdc`, are gitignored (`.gitignore:160 .cursor/*`, with only `environment.json` un-ignored), so they exist on one machine.
- They reference `tests/e2e_boot.py` (does not exist; the driver is `tests/harness/run_e2e.py`), `docs/old_docs/` (does not exist), and say `.cargo/config.toml` sets the linker flags (they are in the target JSON).
- No `CLAUDE.md`, `AGENTS.md`, or `CONTRIBUTING.md`. The project's premise is agent authorship; the agent contract is unversioned and stale.

## Recommended fix

A tracked `AGENTS.md` that every tool reads (Cursor rule, `CLAUDE.md`, Codex all point at it), containing only what is not derivable from the code.

## Implementation plan

1. **`AGENTS.md`** at the root, about 60 lines:
   - *Read first:* `README.md` (status), `docs/INVARIANTS.md`/`DESIGN §2` and `§9`/`PITFALLS.md` before touching boot, paging, interrupts, AP bring-up; the ROADMAP phase you are implementing.
   - *Standing gates* (copy from ROADMAP "How to read this"): `make check` then `make test` green; new markers registered in the harness in the same commit; new portable logic gets host tests, new hardware behaviour gets a ktest; every fixed bug gets a regression test in the cheapest tier; CHANGELOG entry for user-visible change; design docs updated with any invariant or constant.
   - *Conventions:* lib/bin pairing (or the A1 directory rule once landed); `marker!` vs `klog!` (E3); the three cells (Q3); no `TODO` for correctness gaps (they go in the ROADMAP); no ephemeral "fixed X" comments (DESIGN §1.4).
   - *How to run things:* `./setup.sh`, `make`, `make run`, `make check`, `make test-e2e`, `make test-kernel`, `make test`; the `VIBEOS_*` table pointer (C2); macOS notes.
   - *Toolchain bump procedure* (C1).
   - *What not to do:* commit build products, edit `limine/`, add dependencies without a note in the PR, disable a test to make CI green.
2. **`CLAUDE.md`:** one line, `@AGENTS.md` (Claude Code supports file imports), or a copy if the tool in use does not.
3. **`.cursor/rules/general.mdc`:** replace the body with the machine facts (macOS host, QEMU) plus "Follow `AGENTS.md`"; un-ignore it: in `.gitignore` add `!.cursor/rules/` under the `.cursor/*` rule. Delete the stale references.
4. **`CONTRIBUTING.md`:** optional; for humans, two paragraphs: "code is written by agents; humans review, file bugs, and run the kernel" and the PR checklist.
5. **Keep it short:** anything longer than a screen belongs in `docs/`; `AGENTS.md` links there.

## Acceptance criteria

- `git ls-files AGENTS.md CLAUDE.md .cursor/rules/general.mdc` lists all three.
- `grep -n 'e2e_boot\|old_docs' .cursor/rules/general.mdc AGENTS.md` empty.

## Tests

None.

## Risks and rollback

None.
