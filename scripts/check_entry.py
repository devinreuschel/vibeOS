#!/usr/bin/env python3
"""One entry path: no `x86-interrupt` handler outside `src/arch/` (AGENTS.md rule 1).

Every IDT vector enters through a stub that `src/arch/idt.rs` generates, and
`idt::set_handler` takes a plain body fn (ROADMAP §10.6, DESIGN §5.10 rule 1).
Two rules:
- `x86_interrupt_outside_arch`: an `extern "x86-interrupt"` in a `.rs` file
  under `src/`, `crates/`, `user/`, or `tests/`, outside `src/arch/`, fails.
  Text after `//` is ignored.
- `stale_abi_feature`: `src/main.rs` enabling `abi_x86_interrupt` fails when
  no `extern "x86-interrupt"` is left anywhere.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts import gatelib  # noqa: E402

SCOPE = ["src", "crates", "user", "tests"]
ARCH = "src/arch/"
MAIN = "src/main.rs"
ABI = re.compile(r'extern\s+"x86-interrupt"')
FEATURE = re.compile(r"#!\[feature\([^)]*\babi_x86_interrupt\b")


def _code(line: str) -> str:
    """`line` less a `//` comment."""
    return line.split("//", 1)[0]


def _abi_lines(files: dict[str, str]) -> list[tuple[str, int]]:
    out: list[tuple[str, int]] = []
    for path in sorted(files):
        if not path.endswith(".rs"):
            continue
        for n, line in enumerate(files[path].splitlines(), start=1):
            if ABI.search(_code(line)):
                out.append((path, n))
    return out


def x86_interrupt_outside_arch(files: dict[str, str]) -> list[str]:
    """Each `extern "x86-interrupt"` in `files` outside `src/arch/`."""
    return [f'{path}:{n}: extern "x86-interrupt" outside {ARCH} (AGENTS.md rule 1)'
            for path, n in _abi_lines(files) if not path.startswith(ARCH)]


def stale_abi_feature(main_text: str, files: dict[str, str]) -> list[str]:
    """The feature line in `main_text` when `files` has no handler left."""
    if _abi_lines(files):
        return []
    return [f"{MAIN}:{n}: abi_x86_interrupt enabled with no x86-interrupt fn left"
            for n, line in enumerate(main_text.splitlines(), start=1)
            if FEATURE.search(_code(line))]


def scoped_files(repo: Path = ROOT) -> dict[str, str]:
    """Tracked and untracked, not ignored, `.rs` files in scope."""
    out = gatelib.git(repo, "ls-files", "-z", "--cached", "--others", "--exclude-standard",
                      "--", *SCOPE)
    files: dict[str, str] = {}
    for p in sorted({p for p in out.split("\0") if p.endswith(".rs")}):
        try:
            files[p] = (repo / p).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
    return files


def main(argv: list[str] | None = None) -> int:
    if argv:
        print("usage: check_entry.py", file=sys.stderr)
        return 2
    files = scoped_files()
    errors = x86_interrupt_outside_arch(files)
    errors += stale_abi_feature(files.get(MAIN, ""), files)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("check_entry: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
