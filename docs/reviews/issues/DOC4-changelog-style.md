# DOC4 · Keep `CHANGELOG.md` short and user-facing

| | |
|---|---|
| **Area** | 4.12 Documentation |
| **Impact / Effort / Phase** | Low / S / I |
| **Depends on** | B3 (cut a section) |
| **Blocks** | — |
| **Review** | [ARCHITECTURE_REVIEW.md §4.12](../ARCHITECTURE_REVIEW.md#412-documentation) |

## Problem

Entries are 15–20-line paragraphs of implementation detail (the Phase 8B entry explains lock dropping, busy flags, FAT copies and FSInfo sync), contrary to the rules file's own guidance ("short and user-oriented, not implementation trivia"). The same material already lives in the module `//!` headers and DESIGN. The file is 37 KB with one `[Unreleased]` section.

## Recommended fix

Two lines per change, phrased for someone booting the kernel; implementation notes stay in module headers and DESIGN; sections are cut per phase (B3).

## Implementation plan

1. **Style rule** at the top of `CHANGELOG.md` (below the Keep a Changelog line): "One or two lines per entry. Say what changed for someone running vibeOS (new command, new marker, new device, fixed hang). Link to the ROADMAP section instead of describing the design."
2. **Rewrite the current `[Unreleased]`** into the `[0.8.0]` section (B3) using the rule; move any sentence that is not user-visible to the relevant module header if it is not already there (spot-check three entries; most are already duplicated).
3. **PR template** (`.github/pull_request_template.md`): a checkbox "CHANGELOG entry (≤ 2 lines) if user-visible".
4. **`AGENTS.md`** repeats the rule in one line (DOC3).

## Acceptance criteria

- No changelog entry over three lines after the rewrite.
- `[0.8.0]` section exists.

## Tests

None.

## Risks and rollback

None.
