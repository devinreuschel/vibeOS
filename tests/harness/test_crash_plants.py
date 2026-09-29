"""Host tests for run_vibefs_crash.run_plants (ROADMAP §10.2, the crash
test's planted defects): a plant is caught by the first CrashCheckError, an
uncaught plant, any other failure or a failing control round fails the run,
and the option goes after the words VIBEOS_CMDLINE set."""

from __future__ import annotations

import unittest

from tests.harness.harness import EnvConfig, HarnessError
from tests.harness.run_vibefs_crash import CrashCheckError, run_plants, with_plant


def env(cmdline: str = "") -> EnvConfig:
    return EnvConfig(
        iso="x.iso",
        smp=1,
        cpu="max",
        mem="256M",
        firmware=None,
        accel=None,
        timeout=1.0,
        extra=(),
        cmdline=cmdline,
    )


class Rounds:
    """A fake round function: `script[cmdline]` lists what each round does,
    `None` to pass and an exception to raise; calls are recorded."""

    def __init__(self, script: dict[str, list[BaseException | None]]) -> None:
        self.script = script
        self.calls: list[tuple[str, int]] = []

    def __call__(self, e: EnvConfig, i: int) -> str:
        self.calls.append((e.cmdline, i))
        steps = self.script.get(e.cmdline, [])
        step = steps[i] if i < len(steps) else None
        if step is not None:
            raise step
        return "clean"


LEAK = "vibeos.crash_plant=leak"
EARLY = "vibeos.crash_plant=early_super"


class RunPlants(unittest.TestCase):
    def test_stops_at_first_crash_check_error(self) -> None:
        fn = Rounds(
            {
                LEAK: [None, CrashCheckError("super@3: fsck"), None],
                EARLY: [CrashCheckError("sync fail")],
            }
        )
        out = run_plants(env(), ["leak", "early_super"], 8, fn)
        self.assertEqual(fn.calls, [(LEAK, 0), (LEAK, 1), (EARLY, 0), ("", 0)])
        self.assertEqual(len(out), 3)
        self.assertIn("round 2/8", out[0])

    def test_uncaught_plant_fails(self) -> None:
        fn = Rounds({})
        with self.assertRaisesRegex(HarnessError, "plant leak not caught in 3 rounds"):
            run_plants(env(), ["leak"], 3, fn)
        self.assertEqual(fn.calls, [(LEAK, 0), (LEAK, 1), (LEAK, 2)])

    def test_other_failure_fails(self) -> None:
        fn = Rounds({LEAK: [HarnessError("no crash-ready")]})
        with self.assertRaises(HarnessError) as cm:
            run_plants(env(), ["leak"], 8, fn)
        self.assertNotIsInstance(cm.exception, CrashCheckError)
        self.assertEqual(fn.calls, [(LEAK, 0)])

    def test_failing_control_round_fails(self) -> None:
        for err in (CrashCheckError("kill: fsck"), HarnessError("timeout")):
            fn = Rounds({LEAK: [CrashCheckError("caught")], "": [err]})
            with self.assertRaisesRegex(HarnessError, "unplanted control round"):
                run_plants(env(), ["leak"], 8, fn)

    def test_option_appended_to_cmdline(self) -> None:
        self.assertEqual(with_plant(env(), "leak").cmdline, LEAK)
        base = env("vibeos.strace=1 loglevel=7")
        self.assertEqual(
            with_plant(base, "early_super").cmdline, f"vibeos.strace=1 loglevel=7 {EARLY}"
        )
        self.assertEqual(base.cmdline, "vibeos.strace=1 loglevel=7")
        fn = Rounds({f"vibeos.strace=1 loglevel=7 {LEAK}": [CrashCheckError("caught")]})
        run_plants(base, ["leak"], 2, fn)
        self.assertEqual(
            fn.calls, [(f"vibeos.strace=1 loglevel=7 {LEAK}", 0), ("vibeos.strace=1 loglevel=7", 0)]
        )


if __name__ == "__main__":
    unittest.main()
