#!/usr/bin/env python3
"""The interactive QEMU launcher behind `make run`, `make run-panic` and
`make debug` (ROADMAP §10.1, §10.2, DESIGN §8.4).

The Makefile holds no QEMU command line. This builds the one the drivers
use: `env_config` reads every `VIBEOS_*` setting, `dataclasses.replace`
adds the mode's display, and `harness.qemu_argv` emits the argv, with
`-serial stdio` so COM1 is the terminal. It adds no monitor and asserts
nothing, so it writes no results file.

    run_interactive.py run      production ISO, a display window
    run_interactive.py panic    panic-test ISO, `-display none`
    run_interactive.py debug --kernel-elf ELF [--user-elf ELF]...
                                as `run`, halted with a gdb stub (`-s -S`);
                                writes build/debug/symbols.gdb, which
                                scripts/vibeos.gdb sources
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
    gdb: bool = False


MODES: dict[str, Mode] = {
    "run": Mode(variant="default", display=True),
    "panic": Mode(variant="panic", display=False),
    "debug": Mode(variant="default", display=True, gdb=True),
}

# What scripts/vibeos.gdb sources: the ELFs of the session `make debug` started.
GDB_SYMBOLS = "build/debug/symbols.gdb"
GDB_PORT = 1234


def interactive_config(mode: str) -> tuple[QemuConfig, float | None]:
    """The mode's QemuConfig and its session bound: `VIBEOS_TIMEOUT` when it
    is set, else None (an interactive session has no default bound)."""
    m = MODES[mode]
    env = env_config(default_iso=default_iso(m.variant), default_timeout=math.inf)
    cfg = dataclasses.replace(env.qemu(), display=m.display, gdb=m.gdb)
    return cfg, None if math.isinf(env.timeout) else env.timeout


def interactive_argv(mode: str) -> list[str]:
    return qemu_argv(interactive_config(mode)[0], None)


def write_gdb_symbols(path: str, kernel_elf: str, user_elfs: Sequence[str]) -> None:
    """The gdb commands that load the kernel ELF and each user ELF, by
    absolute path. The user ELFs load at their own link addresses (`-o 0`)."""
    lines = [f"file {os.path.abspath(kernel_elf)}"]
    lines += [f"add-symbol-file {os.path.abspath(u)} -o 0" for u in user_elfs]
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")


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
    dbg = sub.add_parser("debug", help="as run, halted with a gdb stub on :1234")
    dbg.add_argument("--kernel-elf", required=True)
    dbg.add_argument("--user-elf", action="append", default=[])
    args = ap.parse_args(argv)
    try:
        cfg, timeout = interactive_config(args.cmd)
        if args.cmd == "debug":
            write_gdb_symbols(GDB_SYMBOLS, args.kernel_elf, args.user_elf)
            print(
                f"debug: QEMU starts halted with a gdb stub on localhost:{GDB_PORT}; "
                "from the repository root, in another terminal, run\n"
                "    gdb -x scripts/vibeos.gdb        (x86_64-elf-gdb on macOS)\n"
                f"then `hbreak _start` and `continue`. Symbols: {GDB_SYMBOLS}",
                file=sys.stderr,
            )
        return launch(qemu_argv(cfg, None), timeout)
    except HarnessError as e:
        print(f"run_interactive: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
