#!/usr/bin/env python3
"""PS/2 window-keyboard regression. DESIGN §8.3 / #66.

Boots the production ISO and requires a QEMU `sendkey` echo. That is the
same i8042 path as a focused QEMU window (`-display none` still has the
controller). Serial-only is not enough.

`make test-e2e` already runs this after the boot contract. `make test-ps2`
is the focused rerun without the marker walk.
"""

from __future__ import annotations

import os
import sys

from tests.harness.harness import HarnessError, QemuConfig, run_qemu_console_input


def main() -> int:
    iso = os.environ.get("VIBEOS_ISO", "vibeos.iso")
    smp = int(os.environ.get("VIBEOS_SMP", "2"))
    cpu = os.environ.get("VIBEOS_QEMU_CPU", "max")
    mem = os.environ.get("VIBEOS_MEM", "128M")
    bios = os.environ.get("VIBEOS_BIOS")
    extra = tuple(x for x in os.environ.get("VIBEOS_QEMU_EXTRA", "").split() if x)
    cfg = QemuConfig(
        iso=iso,
        smp=smp,
        cpu=cpu,
        mem=mem,
        bios=bios,
        extra=extra,
    )
    try:
        inp = run_qemu_console_input(
            cfg,
            timeout_s=float(os.environ.get("VIBEOS_TIMEOUT", "60")),
        )
    except HarnessError as e:
        print(f"[ps2] FAIL: {e}", file=sys.stderr)
        return 1
    print("[ps2] ok: serial+ps2 console input", file=sys.stderr)
    for name in inp.matched:
        print(f"[ps2]   . {name}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
