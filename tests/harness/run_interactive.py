#!/usr/bin/env python3
"""The interactive QEMU launcher behind `make run` and `make run-panic`
(ROADMAP §10.2, DESIGN §8.4).

The Makefile holds no QEMU command line. This builds the one the drivers
use: `env_config` reads every `VIBEOS_*` setting, `dataclasses.replace`
adds the mode's display, and `harness.qemu_argv` emits the argv, with
`-serial stdio` so COM1 is the terminal. It adds no monitor and asserts
nothing, so it writes no results file.

    run_interactive.py run      production ISO, a display window
    run_interactive.py panic    panic-test ISO, `-display none`
"""

from __future__ import annotations

import argparse
import dataclasses
import math
import os
import shutil
import signal
import subprocess
import sys
from collections.abc import Sequence
from dataclasses import dataclass

from tests.harness.harness import (
    HarnessError,
    QemuConfig,
    default_iso,
    env_config,
    qemu_argv,
)


@dataclass(frozen=True)
class Mode:
    variant: str
    display: bool


MODES: dict[str, Mode] = {
    "run": Mode(variant="default", display=True),
    "panic": Mode(variant="panic", display=False),
}


def interactive_config(mode: str) -> tuple[QemuConfig, float | None]:
    """The mode's QemuConfig and its session bound: `VIBEOS_TIMEOUT` when it
    is set, else None (an interactive session has no default bound)."""
    m = MODES[mode]
    env = env_config(default_iso=default_iso(m.variant), default_timeout=math.inf)
    cfg = dataclasses.replace(env.qemu(), display=m.display)
    return cfg, None if math.isinf(env.timeout) else env.timeout


def interactive_argv(mode: str) -> list[str]:
    return qemu_argv(interactive_config(mode)[0], None)


def launch(argv: Sequence[str], timeout: float | None) -> int:
    """Run QEMU on the inherited stdio and return its exit status. SIGINT is
    ignored here while it runs, so ^C reaches QEMU and the guest, not us."""
    if shutil.which(argv[0]) is None:
        raise HarnessError(f"{argv[0]} not found on PATH")
    if "-cdrom" in argv:
        iso = argv[list(argv).index("-cdrom") + 1]
        if not os.path.isfile(iso):
            raise HarnessError(f"ISO {iso} not found; build it with make")
    proc = subprocess.Popen(list(argv))
    old = signal.signal(signal.SIGINT, signal.SIG_IGN)
    try:
        try:
            return proc.wait(timeout)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
            print(f"run_interactive: VIBEOS_TIMEOUT={timeout:g} s reached; killed QEMU",
                  file=sys.stderr)
            return 124
    finally:
        signal.signal(signal.SIGINT, old)


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="run_interactive.py", description="interactive QEMU launcher")
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("run", help="production ISO in a QEMU window, COM1 on this terminal")
    sub.add_parser("panic", help="panic-test ISO, no display")
    args = ap.parse_args(argv)
    try:
        cfg, timeout = interactive_config(args.cmd)
        return launch(qemu_argv(cfg, None), timeout)
    except HarnessError as e:
        print(f"run_interactive: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
