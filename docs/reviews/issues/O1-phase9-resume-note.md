# O1 · Note the Phase 9 pause and resume; research notes deferred

**Superseded 2026-09-22.** Phase 9 exit closed the same day (9C, CHANGELOG
`[Unreleased]`). Do not implement this issue as written — there is nothing
to "resume". Research notes stay deferred (maintainer: "not there yet").

| | |
|---|---|
| **Area** | 4.15 Other |
| **Impact / Effort / Phase** | Low / S / II |
| **Depends on** | — |
| **Blocks** | — |
| **Review** | [ARCHITECTURE_REVIEW.md §4.15](../ARCHITECTURE_REVIEW.md#415-other-project-specific) |

## Problem

The review snapshot had `CHANGELOG.md` saying "Phase 9 is paused" with no
reason. The maintainer clarified (2026-09-22) that the pause was a usage-limit
matter the agents recorded on their own, and that it is lifted. The README's
promise to record "where the models fall over" has no artifact yet; the
maintainer considers that premature ("not there yet").

## Recommended fix

One changelog line; no research document until asked.

## Implementation plan

1. **`CHANGELOG.md` `[Unreleased]`:** under "Changed": "Phase 9 (user mode) resumed." When the `[0.8.0]` section is cut (B3), edit the Phase 8D entry's last sentence from "Phase 9 is paused." to "Phase 8 exit." so the historical section does not claim a pause.
2. **`docs/ROADMAP.md`:** no change (checkboxes are the status).
3. **Research notes:** deferred. If later wanted, the cheapest form is a "Lessons" bullet list per phase in the ROADMAP, fed by the pitfalls that were agent-caused; not created now.

## Acceptance criteria

- `grep -n 'paused' CHANGELOG.md` empty after B3.

## Tests

None.

## Risks and rollback

None.
