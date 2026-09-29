"""Tests of `tests/harness/run_interactive.py`, the launcher behind `make run`,
`make run-panic` and `make debug` (ROADMAP §10.1, §10.2), and of
`scripts/vibeos.gdb`."""

from __future__ import annotations

import os
import re
import signal
import subprocess
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

from tests.harness import run_interactive
from tests.harness.harness import HarnessError, overlay_env

ROOT = Path(__file__).resolve().parents[2]
MAKEFILE = ROOT / "Makefile"

# Every variable env_config reads (tests/harness/harness.py).
ENV_CONFIG_VARS = (
    "VIBEOS_ISO",
    "VIBEOS_SMP",
    "VIBEOS_QEMU_CPU",
    "VIBEOS_MEM",
    "VIBEOS_BIOS",
    "VIBEOS_QEMU_ACCEL",
    "VIBEOS_TIMEOUT",
    "VIBEOS_QEMU_EXTRA",
    "VIBEOS_TIER",
    "VIBEOS_QEMU_VERSION",
)


def _recipe(target: str) -> str:
    """The recipe lines of `target:` in the Makefile, continuations joined."""
    text = MAKEFILE.read_text(encoding="utf-8").replace("\\\n", " ")
    m = re.search(rf"^{re.escape(target)}:[^=\n]*\n((?:\t.*\n)*)", text, re.M)
    assert m is not None, f"no {target} rule"
    return m.group(1)


def _opt(argv: list[str], flag: str) -> str:
    return argv[argv.index(flag) + 1]


class TestInteractiveLauncher(unittest.TestCase):
    """`make run`, `make run-panic` and `make debug` start QEMU through the
    launcher, with the drivers' argv and every `VIBEOS_*` setting."""

    def test_makefile_has_no_qemu_command_line(self) -> None:
        text = MAKEFILE.read_text(encoding="utf-8")
        self.assertNotIn("qemu-system", text)
        for var in ENV_CONFIG_VARS:
            self.assertIsNone(
                re.search(rf"^{var}\s*\?=", text, re.M), f"Makefile sets a default for {var}"
            )

    def test_make_targets_call_the_launcher(self) -> None:
        for target, mode in (("run", "run"), ("run-panic", "panic"), ("debug", "debug")):
            recipe = _recipe(target)
            self.assertIn(f"python3 tests/harness/run_interactive.py {mode}", recipe)
            self.assertNotIn("VIBEOS_ISO", recipe)

    def test_run_argv(self) -> None:
        with overlay_env({}, clear=True):
            argv = run_interactive.interactive_argv("run")
        self.assertEqual(argv[0], "qemu-system-x86_64")
        self.assertEqual(_opt(argv, "-cdrom"), "build/vibeos.iso")
        self.assertNotIn("-display", argv)
        self.assertEqual(_opt(argv, "-serial"), "stdio")
        self.assertNotIn("-monitor", argv)
        self.assertEqual(_opt(argv, "-accel"), "tcg")
        self.assertEqual(_opt(argv, "-smp"), "2")
        self.assertEqual(_opt(argv, "-cpu"), "max")
        self.assertEqual(_opt(argv, "-m"), "128M")
        self.assertIn("-no-reboot", argv)
        self.assertNotIn("-s", argv)
        self.assertNotIn("-S", argv)

    def test_panic_argv(self) -> None:
        with overlay_env({}, clear=True):
            argv = run_interactive.interactive_argv("panic")
        self.assertEqual(_opt(argv, "-cdrom"), "build/vibeos-panic.iso")
        self.assertEqual(_opt(argv, "-display"), "none")
        self.assertEqual(argv.count("-display"), 1)
        self.assertEqual(_opt(argv, "-serial"), "stdio")
        self.assertNotIn("-monitor", argv)

    def test_debug_argv(self) -> None:
        with overlay_env({}, clear=True):
            argv = run_interactive.interactive_argv("debug")
        self.assertEqual(_opt(argv, "-cdrom"), "build/vibeos.iso")
        self.assertNotIn("-display", argv)
        self.assertIn("-s", argv)
        self.assertIn("-S", argv)
        self.assertNotIn("-monitor", argv)

    def test_settings_reach_argv(self) -> None:
        env = {
            "VIBEOS_SMP": "4",
            "VIBEOS_QEMU_CPU": "qemu64,-tsc-deadline",
            "VIBEOS_MEM": "256M",
            "VIBEOS_ISO": "other.iso",
            "VIBEOS_QEMU_EXTRA": "-nic none",
            "VIBEOS_QEMU_ACCEL": "kvm",
        }
        for mode in run_interactive.MODES:
            with self.subTest(mode=mode), overlay_env(env, clear=True):
                argv = run_interactive.interactive_argv(mode)
                self.assertEqual(_opt(argv, "-smp"), "4")
                self.assertEqual(_opt(argv, "-cpu"), "qemu64,-tsc-deadline")
                self.assertEqual(_opt(argv, "-m"), "256M")
                self.assertEqual(_opt(argv, "-cdrom"), "other.iso")
                self.assertEqual(_opt(argv, "-accel"), "kvm")
                self.assertEqual(argv[-2:], ["-nic", "none"])

    def test_bios_reaches_argv(self) -> None:
        with overlay_env({"VIBEOS_BIOS": "/fw/OVMF.fd"}, clear=True):
            argv = run_interactive.interactive_argv("run")
        self.assertEqual(_opt(argv, "-bios"), "/fw/OVMF.fd")

    def test_empty_accel_omits_accel(self) -> None:
        for mode in run_interactive.MODES:
            with self.subTest(mode=mode), overlay_env({"VIBEOS_QEMU_ACCEL": ""}, clear=True):
                argv = run_interactive.interactive_argv(mode)
                self.assertNotIn("-accel", argv)
                self.assertIn("-no-reboot", argv)

    def test_timeout_only_when_set(self) -> None:
        with overlay_env({}, clear=True):
            self.assertIsNone(run_interactive.interactive_config("run")[1])
        with overlay_env({"VIBEOS_TIMEOUT": "7.5"}, clear=True):
            self.assertEqual(run_interactive.interactive_config("run")[1], 7.5)

    def _launch(self, timeout: float | None, wait: Any) -> tuple[int, mock.MagicMock]:
        with tempfile.NamedTemporaryFile(suffix=".iso") as iso:
            argv = ["qemu-system-x86_64", "-cdrom", iso.name, "-serial", "stdio"]
            proc = mock.MagicMock()
            proc.wait.side_effect = wait
            with (
                mock.patch("shutil.which", return_value="/q"),
                mock.patch("subprocess.Popen", return_value=proc) as p,
            ):
                rc = run_interactive.launch(argv, timeout)
            self.assertEqual(p.call_args.args, (argv,))
            for k in ("stdin", "stdout", "stderr"):
                self.assertNotIn(k, p.call_args.kwargs)
            return rc, proc

    def test_launch_inherits_stdio_and_ignores_sigint(self) -> None:
        seen: list[Any] = []

        def wait(timeout: float | None = None) -> int:
            seen.append((timeout, signal.getsignal(signal.SIGINT)))
            return 3

        before = signal.getsignal(signal.SIGINT)
        rc, _ = self._launch(None, wait)
        self.assertEqual(rc, 3)
        self.assertEqual(seen, [(None, signal.SIG_IGN)])
        self.assertIs(signal.getsignal(signal.SIGINT), before)

    def test_launch_timeout_kills(self) -> None:
        calls: list[float | None] = []

        def wait(timeout: float | None = None) -> int:
            calls.append(timeout)
            if len(calls) == 1:
                raise subprocess.TimeoutExpired("qemu", timeout or 0)
            return -9

        rc, proc = self._launch(2.0, wait)
        self.assertEqual(rc, 124)
        self.assertEqual(calls[0], 2.0)
        proc.kill.assert_called_once()

    def test_launch_refuses_missing_iso_or_qemu(self) -> None:
        argv = ["qemu-system-x86_64", "-cdrom", os.path.join(tempfile.gettempdir(), "no.iso")]
        with mock.patch("shutil.which", return_value="/q"):
            with self.assertRaisesRegex(HarnessError, "no.iso"):
                run_interactive.launch(argv, None)
        with mock.patch("shutil.which", return_value=None):
            with self.assertRaisesRegex(HarnessError, "qemu-system-x86_64"):
                run_interactive.launch(argv, None)


class TestDebugTarget(unittest.TestCase):
    """`make debug`: QEMU `-s -S` through the launcher, and a gdb script that
    loads the kernel ELF and the user ELFs."""

    def test_gdb_stub_window_and_settings(self) -> None:
        env = {"VIBEOS_SMP": "3", "VIBEOS_MEM": "64M", "VIBEOS_QEMU_ACCEL": ""}
        with overlay_env(env, clear=True):
            argv = run_interactive.interactive_argv("debug")
        i = argv.index("-s")
        self.assertEqual(argv[i : i + 2], ["-s", "-S"])
        self.assertNotIn("-display", argv)
        self.assertEqual(_opt(argv, "-serial"), "stdio")
        self.assertEqual(_opt(argv, "-smp"), "3")
        self.assertEqual(_opt(argv, "-m"), "64M")
        self.assertNotIn("-accel", argv)

    def test_write_gdb_symbols(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "sub", "symbols.gdb")
            run_interactive.write_gdb_symbols(path, "k.elf", ["user/a", "user/b"])
            text = Path(path).read_text(encoding="utf-8")
        self.assertEqual(
            text.splitlines(),
            [
                f"file {os.path.abspath('k.elf')}",
                f"add-symbol-file {os.path.abspath('user/a')} -o 0",
                f"add-symbol-file {os.path.abspath('user/b')} -o 0",
            ],
        )

    def test_make_n_debug(self) -> None:
        env = {k: v for k, v in os.environ.items() if k not in ("MAKEFLAGS", "MFLAGS", "MAKELEVEL")}
        out = subprocess.run(
            ["make", "-n", "-o", "build/vibeos.iso",
             "-o", "build/kernels/vibeos-default.elf",
             "-o", "user/hello", "-o", "user/init", "-o", "user/sh", "-o", "user/tests",
             "debug"],
            cwd=ROOT, env=env, capture_output=True, text=True, check=True,
        ).stdout.replace("\\\n", " ")
        line = next(ln for ln in out.splitlines() if "run_interactive.py debug" in ln)
        args = line.split()
        self.assertEqual(args[args.index("--kernel-elf") + 1], "build/kernels/vibeos-default.elf")
        users = [args[i + 1] for i, a in enumerate(args) if a == "--user-elf"]
        self.assertEqual(users, ["user/hello", "user/init", "user/sh", "user/tests"])

    def test_gdb_script(self) -> None:
        lines = [
            ln.strip()
            for ln in (ROOT / "scripts/vibeos.gdb").read_text(encoding="utf-8").splitlines()
            if ln.strip() and not ln.lstrip().startswith("#")
        ]
        self.assertIn(f"source {run_interactive.GDB_SYMBOLS}", lines)
        self.assertEqual(lines[-1], f"target remote localhost:{run_interactive.GDB_PORT}")
        self.assertLess(lines.index(f"source {run_interactive.GDB_SYMBOLS}"), len(lines) - 1)
        self.assertIn("set architecture i386:x86-64", lines)


if __name__ == "__main__":
    unittest.main()
