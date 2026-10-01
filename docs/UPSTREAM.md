# Upstream reports

A failure traced to a bug in QEMU, or in another project vibeOS runs on, is never retried
([DESIGN §8.6](TESTING.md#86-ci-and-coverage)). The kernel or the harness's QEMU command line works
around it, and the report to that project is drafted here: what reproduces it, the versions it was
seen on, and the workaround in this tree. Filing a report needs an account on the project's
tracker, so the maintainer files each draft from their own account and replaces the draft's
**Upstream.** line with the report's link. A draft and its workaround are enough for a ROADMAP box
that asks for one: a gate never needs a new account (ROADMAP, How to read this). ROADMAP §17.7
extends this file.

Each entry is a level-3 heading `### <project>: <title>` followed by four fields, each a paragraph
that starts with its bold name:

- `**Reproducer.**` The smallest command line, guest, or input that shows the bug, and what it
  does against what it should do.
- `**Versions.**` The project's versions it was seen on, and the first fixed one when known.
- `**Workaround.**` The in-tree path and symbol that work around it, first, in a code span
  (`` `tests/harness/harness.py` `qemu_argv` ``), then what it does.
- `**Upstream.**` `draft; the maintainer files it` until the report's link replaces it.

`scripts/check_workflows.py` (`rule_upstream`) fails when DESIGN §8.6 does not link this file, when
an entry misses a field, or when its Workaround path does not exist.

No Phase 10 failure was traced to a QEMU bug, so there are no entries yet.
