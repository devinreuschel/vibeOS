# DOC2 · Split `DESIGN.md` per its own rule; as-built goes in tables, not prose

**ROADMAP:** the §10.3 box that cites DOC2. Where this plan and that box differ, the box decides. Superseded here: the acceptance's 600-line bar, which leaves out `docs/reviews/`, whose reviews are dated records, as well as `ROADMAP.md`; the Problem's line counts are the review's.

| | |
|---|---|
| **Area** | 4.12 Documentation |
| **Impact / Effort / Phase** | Medium / M / III |
| **Depends on** | A1 (stable module map) helpful; otherwise any time |
| **Blocks** | — |
| **Review** | [ARCHITECTURE_REVIEW.md §4.12](../ARCHITECTURE_REVIEW.md#412-documentation) |

## Problem

`docs/DESIGN.md` is 1,838 lines: Overview 94, Invariants 129, Boot 189, Memory 170, Interrupts 209, Time 135, SMP 254, Testing 243, Pitfalls 297, Block I/O to EOF. §1.4 says "When this file outgrows one page per subsystem, split it into `docs/<topic>.md` and leave an index behind." §3.3 carries a boot-order table followed by ~60 lines of narrative corrections ("Live boot through Phase 3 slice B runs steps 6–10 before steps 3–5…", "the old table that listed console as step 15 before SMP was drift and is gone"). The pitfalls (§9, 53 entries) are the most valuable part and the hardest to find. Source headers cite sections by number (`DESIGN §5.4`), so anchors must survive.

## Recommended fix

`docs/design/<topic>.md` per subsystem, `docs/INVARIANTS.md` and `docs/PITFALLS.md` as first-class files, `DESIGN.md` as the index. Fold every narrative correction into its table.

## Implementation plan

1. **Files:** `docs/design/{boot,memory,interrupts,time,smp,testing,block}.md` = §3, §4, §5, §6, §7, §8, §10; `docs/INVARIANTS.md` = §2; `docs/PITFALLS.md` = §9; `docs/DESIGN.md` keeps §1 plus a table of contents linking the files. Keep the section numbers in the headings (`# 5. Interrupts` in `interrupts.md`) so `DESIGN §5.4` still means something; add `<a id="54-irq-registration">` anchors where GitHub's slug would change.
2. **Reference map:** `scripts/doc_refs.py` lists every `DESIGN §x.y` citation in `src/`, `tests/`, `docs/` and checks the target heading exists in the new file set. Run it in `make check`.
3. **Boot order:** rewrite §3.3 as a single table in live order (6–10, 3–5, 11, 12, 13, 13b, 14–18 with the sub-steps for 17b/17c/17d), delete the correction paragraphs, and move the "learned the hard way" bullets to `PITFALLS.md`.
4. **Marker contract:** the ordered list in §8.3 becomes a table generated from `tests/harness/harness.py::boot_contract_markers()` (a script prints it; the doc embeds the output with a "generated" comment), so the contract has one source.
5. **Index hygiene:** `README.md` Docs section lists the new files; `AGENTS.md` says "read INVARIANTS.md and PITFALLS.md before touching boot, paging, interrupts, or AP bring-up" (the README already says this for §9).
6. **Do it in one PR** with `git mv`-free content moves (history for a split is in the PR description).

## Acceptance criteria

- No file under `docs/` over 600 lines except `ROADMAP.md`.
- `scripts/doc_refs.py` reports zero dangling citations.
- §3.3 has no prose below the table other than "Ordering rules" bullets.

## Tests

The reference checker.

## Risks and rollback

Broken anchors in external links (none known). Rollback is the previous single file.
