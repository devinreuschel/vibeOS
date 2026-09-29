#!/usr/bin/env python3
"""No in-guest test hook in the production kernel ELF (Q2's `nm` check), and
no blanket `allow(dead_code)` in a production module (Q2's source rules).

The `catch` module, its asm setjmp and longjmp, and the ramdisk's
`FAIL_NEXT` fault injection compile only with `kernel_tests` (ROADMAP §10.2,
F146; AGENTS.md rule 9). This script lists the symbols of a kernel ELF
linked with the production features, through the toolchain's `llvm-nm
--demangle`, and fails on any symbol `PATTERNS` names.

The source rules (ROADMAP §10.2's Q2 box) read every `.rs` file under
`src/` and `crates/` except `ktest.rs` files, their `ktest/` children and
`src/ktest/`, and fail on:

- a module- or crate-level `allow(dead_code)`, bare or inside `cfg_attr`:
  an inner `#![..]` attribute, or an outer one on a `mod` declaration. The
  one exception is `src/main.rs`'s `cfg_attr(feature = "panic_test", ..)`
  on its `mod` lines, which the panic-test build needs, since it compiles
  out everything after `limine: ok`;
- a crate-level `cfg_attr(feature = "panic_test", allow(..))` in
  `src/main.rs`.

`PENDING` lists files whose blanket allow the sweep that owns them has not
removed; they are skipped, and an entry that now passes is an error.

    check_test_hooks.py [--elf PATH] [--nm PATH] [--root DIR]

`--elf` defaults to `${CARGO_TARGET_DIR:-target}/x86_64-unknown-none/
hookcheck/vibeos`, which `make check` links with the production features
before it runs the check scripts. `--root` checks the sources under DIR.
Exit 0 when clean, 1 on a match or a source-rule failure (each on stderr),
2 when the ELF or `nm` is missing.
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

ROOT = Path(__file__).resolve().parent.parent
SOURCE_DIRS = ("src", "crates")

# Files whose module-level `allow(dead_code)` the sweep that owns them has
# not removed (Q2); skipped until it does.
PENDING: tuple[str, ...] = (
    # P10-S33's log sweep: the parked printer thread and dmesg helpers.
    "src/log/log_init.rs",
)

ATTR_OPEN = re.compile(r"#!?\[")
DEAD_ALLOW = re.compile(r"\ballow\s*\([^()]*\bdead_code\b")
PANIC_TEST_ALLOW = re.compile(r'cfg_attr\s*\(\s*feature\s*=\s*"panic_test"\s*,\s*allow\s*\(')
MOD_ITEM = re.compile(r"(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*[;{]")

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


def is_test_source(rel: str) -> bool:
    """A `ktest.rs` file, a child of one (`ktest/`), or `src/ktest/`."""
    parts = rel.split("/")
    return parts[-1] == "ktest.rs" or "ktest" in parts[:-1]


def attributes(text: str) -> Iterable[tuple[bool, str, int, int]]:
    """`(inner, body, start, end)` for each `#[..]` / `#![..]` outside a
    full-line comment, brackets matched across lines."""
    code = "\n".join("" if ln.lstrip().startswith("//") else ln for ln in text.split("\n"))
    for m in ATTR_OPEN.finditer(code):
        depth, i = 0, m.end() - 1
        while i < len(code):
            if code[i] == "[":
                depth += 1
            elif code[i] == "]":
                depth -= 1
                if depth == 0:
                    break
            i += 1
        yield m.group(0) == "#![", code[m.end():i], m.start(), i + 1


def next_item(text: str, end: int) -> str:
    """The code after the attribute ending at `end`, past further attributes."""
    rest = text[end:]
    while True:
        rest = rest.lstrip()
        if rest.startswith("//"):
            rest = rest.split("\n", 1)[1] if "\n" in rest else ""
            continue
        if rest.startswith("#["):
            nxt = next(iter(attributes(rest)), None)
            if nxt is None:
                return rest
            rest = rest[nxt[3]:]
            continue
        return rest


def file_errors(rel: str, text: str) -> list[str]:
    """Source-rule failures in one file, as `path:line: reason`."""
    errs: list[str] = []
    for inner, body, start, end in attributes(text):
        line = text.count("\n", 0, start) + 1
        if inner and rel == "src/main.rs" and PANIC_TEST_ALLOW.search(body):
            errs.append(f"{rel}:{line}: crate-level panic_test allow; put it on the `mod` "
                        "lines and root aliases the panic_test build leaves unused (Q2)")
            continue
        if not DEAD_ALLOW.search(body):
            continue
        if inner:
            errs.append(f"{rel}:{line}: module-level allow(dead_code); cfg the item to the "
                        "builds that use it, or move a test hook into ktest.rs (Q2)")
        elif MOD_ITEM.match(next_item(text, end)):
            if rel == "src/main.rs" and PANIC_TEST_ALLOW.search(body):
                continue
            errs.append(f"{rel}:{line}: allow(dead_code) on a `mod` line; cfg the item to "
                        "the builds that use it, or move a test hook into ktest.rs (Q2)")
    return errs


def source_errors(root: Path = ROOT, pending: Iterable[str] = PENDING) -> list[str]:
    """Source-rule failures under `root`'s `src/` and `crates/`."""
    pend = set(pending)
    errors: list[str] = []
    seen: set[str] = set()
    for d in SOURCE_DIRS:
        for p in sorted((root / d).rglob("*.rs")):
            rel = p.relative_to(root).as_posix()
            if is_test_source(rel):
                continue
            errs = file_errors(rel, p.read_text(encoding="utf-8"))
            if rel in pend:
                seen.add(rel)
                if not errs:
                    errors.append(f"{rel}: listed in PENDING but passes; remove the entry")
                continue
            errors.extend(errs)
    errors.extend(f"{rel}: listed in PENDING but missing" for rel in sorted(pend - seen))
    return errors


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="check_test_hooks.py")
    ap.add_argument("--elf", default=None)
    ap.add_argument("--nm", default=None)
    ap.add_argument("--root", default=None)
    args = ap.parse_args(argv if argv is not None else [])
    src_errors = source_errors(Path(args.root) if args.root else ROOT)
    for err in src_errors:
        print(f"check_test_hooks: {err}", file=sys.stderr)
    if src_errors:
        print("check_test_hooks: blanket allow(dead_code) in a production module (Q2)",
              file=sys.stderr)
        return 1
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
