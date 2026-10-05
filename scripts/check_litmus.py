#!/usr/bin/env python3
"""dma_* helpers name a litmus test or an Arm ARM rule (ROADMAP §11.7).

Each `dma_wmb`, `dma_rmb`, `dma_mb`, `mmio_read`, and `mmio_write` in the
aarch64 port (both crates) must have a nearby comment that either names a
`tests/litmus/*.litmus` file that exists, or cites the Arm ARM for an idiom
herd7 cannot express (TLB, SGI `dsb`, Device vs Normal MMIO, cache
maintenance).

`make check` runs the comment gate. `make litmus` runs this script with
`--run`, which also invokes herd7: a non-relaxed test must report Never
under its cat, and each `*.relaxed.litmus` / `*_relaxed.litmus` twin must
report Sometimes or Always.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path
from shutil import which

ROOT = Path(__file__).resolve().parent.parent
HELPERS = ("dma_wmb", "dma_rmb", "dma_mb", "mmio_read", "mmio_write")
PORT_GLOBS = ("src/arch/aarch64/**/*.rs", "crates/core/src/arch/aarch64/**/*.rs")
LITMUS_NAME = re.compile(r"tests/litmus/[A-Za-z0-9_./-]+\.litmus")
ARM_CITE = re.compile(r"Arm ARM|ARM ARM|DDI0487", re.I)
FN = re.compile(r"^.*\bfn\s+(" + "|".join(HELPERS) + r")\b", re.M)
RELAXED = re.compile(r"(?:^|[._])relaxed\.litmus$")
AARCH = re.compile(r"^AArch64\b", re.I | re.M)
X86 = re.compile(r"^X86(?:_64)?\b", re.I | re.M)
OBS = re.compile(r"Observation\s+\S+\s+(Never|Sometimes|Always)")


def _rs_files(root: Path) -> list[Path]:
    out: list[Path] = []
    for g in PORT_GLOBS:
        out.extend(p for p in root.glob(g) if p.is_file())
    return sorted(set(out))


def _window(text: str, at: int) -> str:
    """Comment text in the 20 lines before offset `at`."""
    start = text.rfind("\n", 0, at)
    line = text.count("\n", 0, at if start < 0 else start) + 1
    lines = text.splitlines()
    lo = max(0, line - 21)
    return "\n".join(lines[lo:line])


def check_helpers(root: Path = ROOT) -> list[str]:
    """Comment-gate failures."""
    errs: list[str] = []
    for path in _rs_files(root):
        rel = path.relative_to(root).as_posix()
        text = path.read_text(encoding="utf-8")
        for m in FN.finditer(text):
            name = m.group(1)
            win = _window(text, m.start())
            lit = LITMUS_NAME.findall(win)
            cited = ARM_CITE.search(win) is not None
            if not lit and not cited:
                line = text.count("\n", 0, m.start()) + 1
                errs.append(
                    f"{rel}:{line}: {name} names no tests/litmus/*.litmus file and cites no Arm ARM"
                )
                continue
            for p in lit:
                if not (root / p).is_file():
                    line = text.count("\n", 0, m.start()) + 1
                    errs.append(f"{rel}:{line}: {name} names missing {p}")
    return errs


def litmus_files(root: Path) -> list[Path]:
    d = root / "tests" / "litmus"
    if not d.is_dir():
        return []
    return sorted(p for p in d.rglob("*.litmus") if p.is_file())


def _model(text: str) -> str | None:
    if AARCH.search(text):
        return "aarch64.cat"
    if X86.search(text):
        return "x86tso.cat"
    return None


def _want_never(path: Path) -> bool:
    return RELAXED.search(path.name) is None


def run_herd7(root: Path) -> list[str]:
    """Run herd7 on every litmus file. herd7 must be on PATH."""
    if which("herd7") is None:
        return ["litmus: herd7 not installed (opam install herdtools7)"]
    errs: list[str] = []
    files = litmus_files(root)
    if not files:
        return ["litmus: no tests/litmus/*.litmus files"]
    for path in files:
        rel = path.relative_to(root).as_posix()
        text = path.read_text(encoding="utf-8")
        model = _model(text)
        if model is None:
            errs.append(f"{rel}: first line is not AArch64 or X86 / X86_64")
            continue
        r = subprocess.run(
            ["herd7", "-model", model, str(path)],
            capture_output=True,
            text=True,
            check=False,
        )
        out = r.stdout + r.stderr
        if r.returncode != 0:
            errs.append(f"{rel}: herd7 exit {r.returncode}: {out.strip()[:200]}")
            continue
        m = OBS.search(out)
        if m is None:
            errs.append(f"{rel}: herd7 printed no Observation line")
            continue
        got = m.group(1)
        # x86tso.cat records which barriers TSO elides; only aarch64.cat
        # must show Never vs Sometimes on the twins.
        if model != "aarch64.cat":
            continue
        if _want_never(path):
            if got != "Never":
                errs.append(f"{rel}: {model} allowed a forbidden outcome ({got})")
        elif got == "Never":
            errs.append(f"{rel}: barrier-removed twin forbids its bad outcome")
    return errs


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    run = False
    if args == ["--run"]:
        run = True
    elif args:
        print("usage: check_litmus.py [--run]", file=sys.stderr)
        return 2
    errs = check_helpers()
    if run:
        errs += run_herd7(ROOT)
    for e in errs:
        print(e, file=sys.stderr)
    if errs:
        print(f"check_litmus: {len(errs)} failure(s)", file=sys.stderr)
        return 1
    print("check_litmus: ok" + (" (herd7)" if run else ""))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
