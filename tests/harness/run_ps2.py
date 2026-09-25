#!/usr/bin/env python3
"""PS/2 window-keyboard regression. DESIGN §8.3 / #66.

Boots the production ISO and requires a QEMU `sendkey` echo. That is the
same i8042 path as a focused QEMU window (`-display none` still has the
controller). Serial-only is not enough.

`make test-e2e` already runs this after the boot contract. `make test-ps2`
is the focused rerun without the marker walk.
"""

from __future__ import annotations

import sys

from tests.harness import results
from tests.harness.harness import HarnessError, env_config, qemu_argv, run_qemu_console_input


def main() -> int:
    env = env_config(default_iso="vibeos.iso", default_timeout=60)
    res = results.Results(env.tier)
    cfg = env.qemu()
    try:
        inp = run_qemu_console_input(cfg, timeout_s=env.timeout)
    except HarnessError as e:
        res.add_boot(qemu_argv(cfg, None), cfg, None)
        print(f"[ps2] FAIL: {e}", file=sys.stderr)
        return 1
    for name in inp.matched:
        res.record("marker", name, "passed")
    res.add_boot(qemu_argv(cfg, None), cfg, inp.exit_code)
    print("[ps2] ok: serial+ps2 console input", file=sys.stderr)
    for name in inp.matched:
        print(f"[ps2]   . {name}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(results.run_main(main))
