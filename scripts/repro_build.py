#!/usr/bin/env python3
"""Build one commit twice and require byte-identical outputs (ROADMAP §10.2).

`make repro` runs this. Each build is a fresh clone of the commit, at
checkout paths of different lengths, with its own `CARGO_HOME` and
`RUSTUP_HOME` (DESIGN §3.6), a copy of this checkout's `limine/`, then
`./setup.sh` and `make isos`. The run fails unless every
`build/kernels/*.elf`, `build/*.iso` and `build/initrd.fat` of the two
builds match byte for byte, and none of them holds a host path: either
checkout, `$HOME`, or either build's `CARGO_HOME` or `RUSTUP_HOME`.

  --commit REV     the commit to build (default HEAD)
  --workdir DIR    where the two builds go (default: a new temporary dir); it
                   must not hold them already
  --keep           keep the two builds afterwards
  --share-rustup   both builds use the caller's RUSTUP_HOME, so no toolchain
                   is downloaded (local runs; the scheduled job leaves it off)
  --scan-only      build nothing: scan this checkout's build/ for host paths
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


@dataclass(frozen=True)
class BuildSpec:
    checkout: Path
    cargo_home: Path
    rustup_home: Path


def default_cargo_home() -> Path:
    raw = os.environ.get("CARGO_HOME")
    return Path(raw) if raw else Path.home() / ".cargo"


def default_rustup_home() -> Path:
    raw = os.environ.get("RUSTUP_HOME")
    return Path(raw) if raw else Path.home() / ".rustup"


def plan(workdir: Path, share_rustup: bool) -> tuple[BuildSpec, BuildSpec]:
    """Two builds whose checkout, CARGO_HOME and RUSTUP_HOME all differ, the
    checkouts at paths of different lengths (RUSTUP_HOME is shared under
    `share_rustup`)."""
    specs: list[BuildSpec] = []
    for tag in ("a", "build-bb"):
        rustup = default_rustup_home() if share_rustup else workdir / f"rustup-{tag}"
        specs.append(BuildSpec(workdir / tag / "vibeOS", workdir / f"cargo-{tag}", rustup))
    return specs[0], specs[1]


def outputs(root: Path) -> list[str]:
    """The compared outputs under `root`, as sorted paths relative to it."""
    build = root / "build"
    found = [*build.glob("kernels/*.elf"), *build.glob("*.iso")]
    if (build / "initrd.fat").is_file():
        found.append(build / "initrd.fat")
    rel = sorted(str(p.relative_to(root)) for p in found if p.is_file())
    if not rel:
        raise FileNotFoundError(f"{build}: no kernel ELF, ISO or initrd to compare")
    return rel


def first_difference(a: bytes, b: bytes) -> int | None:
    """The first offset at which `a` and `b` differ, None when equal."""
    for i, (x, y) in enumerate(zip(a, b, strict=False)):
        if x != y:
            return i
    if len(a) != len(b):
        return min(len(a), len(b))
    return None


def compare(a: Path, b: Path) -> list[str]:
    """Each difference between the outputs of the builds at `a` and `b`."""
    errors: list[str] = []
    left, right = set(outputs(a)), set(outputs(b))
    for rel in sorted(left - right):
        errors.append(f"{rel}: only in {a}")
    for rel in sorted(right - left):
        errors.append(f"{rel}: only in {b}")
    for rel in sorted(left & right):
        off = first_difference((a / rel).read_bytes(), (b / rel).read_bytes())
        if off is not None:
            errors.append(f"{rel}: differs at offset {off:#x}")
    return errors


def find_host_paths(data: bytes, needles: list[str]) -> list[str]:
    """The needles that occur in `data`."""
    return [n for n in needles if n and n.encode() in data]


def needles_for(specs: list[BuildSpec]) -> list[str]:
    out: set[str] = {str(Path.home())}
    for s in specs:
        out.update((str(s.checkout), str(s.cargo_home), str(s.rustup_home)))
    # A needle inside another is reported by the shorter one.
    return sorted(n for n in out if n not in ("", "/"))


def scan(root: Path, needles: list[str]) -> list[str]:
    errors: list[str] = []
    for rel in outputs(root):
        for n in find_host_paths((root / rel).read_bytes(), needles):
            errors.append(f"{rel}: holds host path {n}")
    return errors


def build_env(spec: BuildSpec) -> dict[str, str]:
    """The build's environment: its own homes, and no CARGO_TARGET_DIR, so
    each build uses its checkout's target/. rustup's proxies stay on PATH
    from the caller's CARGO_HOME; the build's own holds only its registry."""
    env = dict(os.environ)
    env.pop("CARGO_TARGET_DIR", None)
    env["CARGO_HOME"] = str(spec.cargo_home)
    env["RUSTUP_HOME"] = str(spec.rustup_home)
    return env


def run(cmd: list[str], cwd: Path, env: dict[str, str] | None = None) -> None:
    print(f"repro: {cwd}: {' '.join(cmd)}", file=sys.stderr, flush=True)
    subprocess.run(cmd, cwd=cwd, env=env, check=True)


def build(spec: BuildSpec, commit: str) -> None:
    spec.checkout.parent.mkdir(parents=True, exist_ok=True)
    run(["git", "clone", "--quiet", "--no-checkout", str(ROOT), str(spec.checkout)], ROOT)
    run(["git", "checkout", "--quiet", "--detach", commit], spec.checkout)
    if (ROOT / "limine").is_dir():
        shutil.copytree(ROOT / "limine", spec.checkout / "limine", symlinks=True)
    spec.cargo_home.mkdir(parents=True, exist_ok=True)
    registry = default_cargo_home() / "registry"
    for sub in ("index", "cache"):
        if (registry / sub).is_dir() and not (spec.cargo_home / "registry" / sub).exists():
            shutil.copytree(registry / sub, spec.cargo_home / "registry" / sub, symlinks=True)
    env = build_env(spec)
    if spec.rustup_home != default_rustup_home():
        # A fresh RUSTUP_HOME: install rust-toolchain.toml's toolchain.
        run(["rustup", "toolchain", "install", "--no-self-update"], spec.checkout, env)
    run(["./setup.sh"], spec.checkout, env)
    run(["make", "isos"], spec.checkout, env)


def made_paths(spec: BuildSpec) -> list[Path]:
    """What `build` creates for `spec`, to remove afterwards."""
    out = [spec.checkout.parent, spec.cargo_home]
    if spec.rustup_home != default_rustup_home():
        out.append(spec.rustup_home)
    return out


def resolve(commit: str) -> str:
    proc = subprocess.run(["git", "rev-parse", "--verify", f"{commit}^{{commit}}"], cwd=ROOT,
                          capture_output=True, text=True, check=True)
    return proc.stdout.strip()


def report(root: Path) -> int:
    rels = outputs(root)
    for rel in rels:
        digest = hashlib.sha256((root / rel).read_bytes()).hexdigest()
        print(f"{digest}  {rel}")
    return len(rels)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0] if __doc__ else None)
    ap.add_argument("--commit", default="HEAD")
    ap.add_argument("--workdir", type=Path)
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--share-rustup", action="store_true")
    ap.add_argument("--scan-only", action="store_true")
    args = ap.parse_args(argv)

    try:
        if args.scan_only:
            needles = needles_for([BuildSpec(ROOT, default_cargo_home(), default_rustup_home())])
            errors = scan(ROOT, needles)
            if errors:
                print("\n".join(f"repro: {e}" for e in errors), file=sys.stderr)
                return 1
            print(f"repro: ok {report(ROOT)} files (scan only)")
            return 0

        commit = resolve(args.commit)
        made_workdir = args.workdir is None
        workdir = (args.workdir or Path(tempfile.mkdtemp(prefix="vibeos-repro-"))).resolve()
        a, b = plan(workdir, args.share_rustup)
        taken = [p for s in (a, b) for p in made_paths(s) if p.exists()]
        if taken:
            print(f"repro: {taken[0]} already exists", file=sys.stderr)
            return 2
        try:
            for spec in (a, b):
                build(spec, commit)
            errors = compare(a.checkout, b.checkout)
            needles = needles_for([a, b])
            errors += scan(a.checkout, needles)
            if errors:
                print("\n".join(f"repro: {e}" for e in errors), file=sys.stderr)
                return 1
            n = report(a.checkout)
        finally:
            if not args.keep:
                for p in [p for s in (a, b) for p in made_paths(s)]:
                    shutil.rmtree(p, ignore_errors=True)
                if made_workdir:
                    shutil.rmtree(workdir, ignore_errors=True)
    except (OSError, subprocess.CalledProcessError) as e:
        print(f"repro: {e}", file=sys.stderr)
        return 1
    print(f"repro: ok {n} files")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
