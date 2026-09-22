#!/usr/bin/env python3
"""Q3: generic Sync cells live in src/cell.rs; static mut only in catch."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def grep(pattern: str) -> list[str]:
    r = subprocess.run(
        ["grep", "-rn", "--include=*.rs", pattern, "src"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    return [ln for ln in r.stdout.splitlines() if ln]


def main() -> int:
    errors: list[str] = []
    for ln in grep(r"unsafe impl<T> Sync"):
        if not ln.startswith("src/cell.rs:"):
            errors.append(f"generic Sync outside cell.rs: {ln}")
    for ln in grep("static mut"):
        if "src/arch/catch.rs:" not in ln:
            errors.append(f"static mut outside catch.rs: {ln}")
    for ln in grep(r"-> &'static mut"):
        errors.append(f"function returns &'static mut: {ln}")
    for ln in grep("static_mut_refs"):
        errors.append(f"static_mut_refs allow/expect: {ln}")
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("check_cells: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
