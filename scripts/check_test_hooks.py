#!/usr/bin/env python3
"""No in-guest test hook in the production kernel ELF (Q2's `nm` check).

The `catch` module, its asm setjmp and longjmp, and the ramdisk's
`FAIL_NEXT` fault injection compile only with `kernel_tests` (ROADMAP §10.2,
F146; AGENTS.md rule 9). This script lists the symbols of a kernel ELF
linked with the production features, through the toolchain's `llvm-nm
--demangle`, and fails on any symbol `PATTERNS` names.

    check_test_hooks.py [--elf PATH] [--nm PATH]

`--elf` defaults to `${CARGO_TARGET_DIR:-target}/x86_64-unknown-none/
hookcheck/vibeos`, which `make check` links with the production features
before it runs the check scripts. Exit 0 when clean, 1 on a match (each
symbol and its reason on stderr), 2 when the ELF or `nm` is missing.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
from collections.abc import Iterable
from pathlib import Path

# (regex on the demangled, hash-stripped symbol, reason). A pattern that
# matches a symbol the production ELF needs is a bug in this table.
PATTERNS: tuple[tuple[re.Pattern[str], str], ...] = (
    (re.compile(r"::arch::(?:\w+::)?catch::"),
     "the `arch::catch` module compiles only with `kernel_tests` (ROADMAP §10.2, F146)"),
    (re.compile(r"^(?:vibeos_jmpbuf|vibeos_setjmp|vibeos_longjmp|vibeos_catch|vibeos_catch_thunk)$"),
     "`arch::catch`'s setjmp and longjmp compile only with `kernel_tests` (ROADMAP §10.2, F146)"),
    (re.compile(r"(?:^|::)FAIL_NEXT$"),
     "the ramdisk's fault injection compiles only with `kernel_tests` (ROADMAP §10.2, F146)"),
    (re.compile(r"(?:^|::)(?:inject_\w*|push_for_test|exercise_fail\w*)$"),
     "a mutation hook compiles only with `kernel_tests` (Q2)"),
)

HASH = re.compile(r"::h[0-9a-f]{16}$")
LLVM_SUFFIX = re.compile(r"\.llvm\.\d+$")


def default_elf() -> str:
    """The production-feature ELF `make check` links."""
    base = os.environ.get("CARGO_TARGET_DIR") or "target"
    return str(Path(base) / "x86_64-unknown-none" / "hookcheck" / "vibeos")


def default_nm() -> str:
    """`llvm-nm` of the active toolchain's host `bin` directory, else `PATH`'s."""
    try:
        sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True,
                                 text=True, check=True).stdout.strip()
        vv = subprocess.run(["rustc", "-vV"], capture_output=True, text=True,
                            check=True).stdout
    except (OSError, subprocess.CalledProcessError):
        sysroot, vv = "", ""
    host = next((ln.split(": ", 1)[1] for ln in vv.splitlines() if ln.startswith("host: ")), "")
    if sysroot and host:
        cand = Path(sysroot) / "lib" / "rustlib" / host / "bin" / "llvm-nm"
        if cand.is_file():
            return str(cand)
    return shutil.which("llvm-nm") or shutil.which("nm") or "llvm-nm"


def symbol_name(line: str) -> str | None:
    """The hash-stripped name on one `nm` line: `addr type name` or `type name`."""
    parts = line.strip().split(None, 2)
    if len(parts) == 3 and len(parts[1]) == 1:
        name = parts[2]
    elif len(parts) == 2 and len(parts[0]) == 1:
        name = parts[1]
    else:
        return None
    return HASH.sub("", LLVM_SUFFIX.sub("", name.strip()))


def scan(listing: Iterable[str]) -> list[str]:
    """`<symbol>: <reason>` for each symbol in `nm` output that `PATTERNS` matches."""
    errors: list[str] = []
    for line in listing:
        name = symbol_name(line)
        if name is None:
            continue
        for pat, why in PATTERNS:
            if pat.search(name):
                errors.append(f"{name}: {why}")
                break
    return errors


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="check_test_hooks.py")
    ap.add_argument("--elf", default=None)
    ap.add_argument("--nm", default=None)
    args = ap.parse_args(argv if argv is not None else [])
    elf = args.elf or default_elf()
    if not Path(elf).is_file():
        print(f"check_test_hooks: {elf}: no such ELF; `make check` links it", file=sys.stderr)
        return 2
    nm = args.nm or default_nm()
    try:
        out = subprocess.run([nm, "--demangle", elf], capture_output=True, text=True,
                             check=True).stdout
    except (OSError, subprocess.CalledProcessError) as e:
        print(f"check_test_hooks: {nm}: {e}", file=sys.stderr)
        return 2
    errors = scan(out.splitlines())
    for err in errors:
        print(f"check_test_hooks: {elf}: {err}", file=sys.stderr)
    if errors:
        print("check_test_hooks: test-only symbol in a production-feature ELF "
              "(AGENTS.md rule 9, Q2)", file=sys.stderr)
        return 1
    print("check_test_hooks: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
