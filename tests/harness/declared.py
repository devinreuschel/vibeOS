"""Failure lines an in-guest test declares (ROADMAP §10.7, TESTING.md §8.3).

A `failure` row of the marker registry (`tests/contract/markers.toml`)
fails every run that prints it. A ktest boot is the one exception: a line
that falls inside the `run`..`ok`/`FAIL` window of a test that `DECLARED`
lists it for is that test's expected output, and that test fails when it
reaches `ok` without it.

`DECLARED` maps a test name to the registry texts it declares, placeholders
included, so a declaration names a registered line (`check_markers.py`
keeps the texts registered through `test_declared.py`). A line matches a
text through the registry's placeholder matcher (`registry.row_regex`),
the one `harness.FAILURE_PATTERNS` uses (AGENTS.md rule 10).

Standard library and `registry` only: `harness.py` imports this.
"""

from __future__ import annotations

import re
from collections.abc import Iterable

from tests.harness import registry

# Test name -> the registry texts of the failure lines it provokes.
DECLARED: dict[str, tuple[str, ...]] = {}


def _pattern(text: str) -> re.Pattern[str]:
    return re.compile(registry.row_regex(text))


def is_declared(test: str | None, line: str) -> bool:
    """Whether kernel text `line` is a failure line `test` declares."""
    if test is None:
        return False
    return any(_pattern(t).search(line) for t in DECLARED.get(test, ()))


def missing(test: str, seen: Iterable[str]) -> list[str]:
    """The texts `test` declares that no line of `seen` (kernel text from
    its run window) matches, in declaration order."""
    lines = list(seen)
    return [t for t in DECLARED.get(test, ()) if not any(_pattern(t).search(ln) for ln in lines)]
