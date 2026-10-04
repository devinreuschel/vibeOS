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
    "VIBEOS_ARCH",
    "VIBEOS_GIC",
    "VIBEOS_SMP",
    "VIBEOS_QEMU_CPU",
    "VIBEOS_MEM",
    "VIBEOS_BIOS",
    "VIBEOS_QEMU_ACCEL",
    "VIBEOS_TIMEOUT",
    "VIBEOS_QEMU_EXTRA",
    "VIBEOS_TIER",
    "VIBEOS_QEMU_VERSION",
    "VIBEOS_KTEST",
    "VIBEOS_KTEST_REPEAT",
    "VIBEOS_CMDLINE",
    "VIBEOS_FW_X86_64",
    "VIBEOS_FW_AARCH64",
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

    def test_uefi_reaches_argv(self) -> None:
        from tests.harness.harness import remove_vars_copies

        with tempfile.TemporaryDirectory() as d:
            code = Path(d, "OVMF_CODE_4M.fd")
            code.write_bytes(b"c")
            Path(d, "OVMF_VARS_4M.fd").write_bytes(b"v")
            env = {"VIBEOS_BIOS": "uefi", "VIBEOS_FW_X86_64": str(code)}
            for mode in run_interactive.MODES:
                with self.subTest(mode=mode), overlay_env(env, clear=True):
                    argv = run_interactive.interactive_argv(mode)
                    drive = f"if=pflash,format=raw,unit=0,readonly=on,file={code}"
                    self.assertEqual(_opt(argv, "-drive"), drive)
                    self.assertIn("-boot", argv)
            remove_vars_copies()

    def test_cmdline_reaches_argv(self) -> None:
        env = {"VIBEOS_CMDLINE": "vibeos.strace=1", "VIBEOS_KTEST": "t"}
        for mode in run_interactive.MODES:
            with self.subTest(mode=mode), overlay_env(env, clear=True):
                argv = run_interactive.interactive_argv(mode)
                self.assertIn(
                    "name=opt/vibeos/cmdline,string=vibeos.ktest=t vibeos.strace=1", argv
                )

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
                "set $vibeos_core = 0",
            ],
        )

    def test_write_gdb_symbols_for_a_core(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "symbols.gdb")
            run_interactive.write_gdb_symbols(path, "k.elf", [], core="build/debug/core.virt")
            text = Path(path).read_text(encoding="utf-8")
        self.assertEqual(
            text.splitlines(),
            [
                f"file {os.path.abspath('k.elf')}",
                f"core-file {os.path.abspath('build/debug/core.virt')}",
                "set $vibeos_core = 1",
            ],
        )

    def test_make_n_debug_core(self) -> None:
        env = {k: v for k, v in os.environ.items() if k not in ("MAKEFLAGS", "MFLAGS", "MAKELEVEL")}
        out = subprocess.run(
            ["make", "-n", "debug", "CORE=build/cores/x/001-boot/core.zst",
             "ELF=build/cores/x/001-boot/kernel.elf"],
            cwd=ROOT, env=env, capture_output=True, text=True, check=True,
        ).stdout.replace("\\\n", " ")
        line = next(ln for ln in out.splitlines() if "run_interactive.py debug" in ln)
        args = line.split()
        self.assertEqual(args[args.index("--core") + 1], "build/cores/x/001-boot/core.zst")
        self.assertEqual(args[args.index("--kernel-elf") + 1], "build/cores/x/001-boot/kernel.elf")
        self.assertNotIn("--user-elf", args)
        self.assertTrue(any(a.startswith("VIBEOS_VMCORE=") for a in args), line)
        self.assertNotIn("build/vibeos.iso", out)

    def test_make_n_debug(self) -> None:
        env = {k: v for k, v in os.environ.items() if k not in ("MAKEFLAGS", "MFLAGS", "MAKELEVEL")}
        out = subprocess.run(
            ["make", "-n", "-o", "build/vibeos.iso",
             "-o", "build/kernels/vibeos-default.elf",
             "-o", str(ROOT / "build/user/.stamp"), "debug"],
            cwd=ROOT, env=env, capture_output=True, text=True, check=True,
        ).stdout.replace("\\\n", " ")
        line = next(ln for ln in out.splitlines() if "run_interactive.py debug" in ln)
        args = line.split()
        self.assertEqual(args[args.index("--kernel-elf") + 1], "build/kernels/vibeos-default.elf")
        users = [args[i + 1] for i, a in enumerate(args) if a == "--user-elf"]
        # The initrd's programs before the strip (C-USERBINS).
        self.assertEqual([os.path.basename(u) for u in users], ["hello", "init", "sh", "tests"])
        for u in users:
            self.assertIn("/x86_64-unknown-linux-musl/", u)

    def test_gdb_script(self) -> None:
        lines = [
            ln.strip()
            for ln in (ROOT / "scripts/vibeos.gdb").read_text(encoding="utf-8").splitlines()
            if ln.strip() and not ln.lstrip().startswith("#")
        ]
        self.assertIn(f"source {run_interactive.GDB_SYMBOLS}", lines)
        # The stub only when symbols.gdb opened no core (`make debug CORE=`).
        self.assertEqual(
            lines[-3:],
            ["if $vibeos_core == 0", f"target remote localhost:{run_interactive.GDB_PORT}", "end"],
        )
        self.assertLess(lines.index(f"source {run_interactive.GDB_SYMBOLS}"), len(lines) - 3)
        self.assertIn("set architecture i386:x86-64", lines)


STUB_PYTHON = """#!/bin/sh
# python3 for TestE2eUefiRecipe: fakes the probe's exit code and records a
# run_e2e.py call; every other call goes to the real interpreter.
case "$1" in
tests/harness/run_interactive.py)
    if [ "$2" = firmware ]; then
        echo "probe $3" >> "$STUB_LOG"
        exit "$STUB_PROBE"
    fi ;;
tests/harness/run_e2e.py)
    echo "run_e2e tier=$VIBEOS_TIER iso=$VIBEOS_ISO bios=$VIBEOS_BIOS" >> "$STUB_LOG"
    exit "$STUB_E2E" ;;
esac
exec "$REAL_PYTHON" "$@"
"""


class TestE2eUefiRecipe(unittest.TestCase):
    """`make test-e2e-uefi` probes for the firmware and runs the harness in one
    shell line: 0 runs it, 1 skips (fails under CI), 2 fails. The real
    recipe, through `make`, with a stub python3 first on PATH and the ISO
    prerequisite marked old (`-o`), so nothing is built."""

    def setUp(self) -> None:
        import sys

        self._tmp = tempfile.TemporaryDirectory()
        d = self._tmp.name
        stub = Path(d, "python3")
        stub.write_text(STUB_PYTHON, encoding="utf-8")
        stub.chmod(0o755)
        self.log = Path(d, "log")
        self.env = {
            k: v
            for k, v in os.environ.items()
            if k not in ("MAKEFLAGS", "MFLAGS", "MAKELEVEL", "CI", "VIBEOS_PREBUILT")
            and not k.startswith("VIBEOS_")
        }
        self.env.update(
            PATH=f"{d}{os.pathsep}{os.environ.get('PATH', '')}",
            STUB_LOG=str(self.log),
            REAL_PYTHON=sys.executable,
        )

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def make(self, probe: int, e2e: int = 0, ci: str = "") -> tuple[int, str, list[str]]:
        env = dict(self.env, STUB_PROBE=str(probe), STUB_E2E=str(e2e))
        if ci:
            env["CI"] = ci
        self.log.write_text("", encoding="utf-8")
        r = subprocess.run(
            ["make", "--no-print-directory", "-o", "build/vibeos.iso", "test-e2e-uefi"],
            cwd=ROOT, env=env, capture_output=True, text=True, check=False, timeout=120,
        )
        calls = self.log.read_text(encoding="utf-8").splitlines()
        return r.returncode, r.stdout + r.stderr, calls

    def test_found_runs_the_harness(self) -> None:
        rc, out, calls = self.make(0)
        self.assertEqual(rc, 0, out)
        self.assertEqual(
            calls,
            ["probe x86_64", "run_e2e tier=test-e2e-uefi iso=build/vibeos.iso bios=uefi"],
        )

    def test_harness_status_passes_through(self) -> None:
        rc, out, calls = self.make(0, e2e=3)
        self.assertNotEqual(rc, 0)
        self.assertIn("Error 3", out)
        self.assertEqual(len(calls), 2)

    def test_none_installed_skips_outside_ci(self) -> None:
        rc, out, calls = self.make(1)
        self.assertEqual(rc, 0, out)
        self.assertIn("test-e2e-uefi: SKIP:", out)
        self.assertEqual(calls, ["probe x86_64"])

    def test_none_installed_fails_in_ci(self) -> None:
        rc, out, calls = self.make(1, ci="true")
        self.assertNotEqual(rc, 0)
        self.assertIn("test-e2e-uefi: FAIL:", out)
        self.assertIn("Error 1", out)
        self.assertEqual(calls, ["probe x86_64"])

    def test_probe_error_fails(self) -> None:
        for ci in ("", "true"):
            with self.subTest(ci=ci):
                rc, out, calls = self.make(2, ci=ci)
                self.assertNotEqual(rc, 0)
                self.assertIn("Error 2", out)
                self.assertEqual(calls, ["probe x86_64"])

    def test_help_line(self) -> None:
        env = {k: v for k, v in os.environ.items() if k not in ("MAKEFLAGS", "MFLAGS", "MAKELEVEL")}
        out = subprocess.run(
            ["make", "--no-print-directory", "help"],
            cwd=ROOT, env=env, capture_output=True, text=True, check=True,
        ).stdout
        line = next(ln for ln in out.splitlines() if ln.split()[:1] == ["test-e2e-uefi"])
        self.assertIn("probe", line)
        self.assertIn("pflash", line)
        self.assertIn("skip", line)
        self.assertIn("CI", line)


class TestFirmwareCli(unittest.TestCase):
    """`run_interactive.py firmware <arch>`: 0 found, 1 none, 2 probe error,
    the codes the Makefile's `test-e2e-uefi` and setup.sh act on."""

    def run_cli(self, arch: str, env: dict[str, str], root: str) -> tuple[int, str, str]:
        import contextlib
        import io

        from tests.harness import harness

        out, err = io.StringIO(), io.StringIO()
        real = harness.probe_firmware

        def rooted(a: str, environ: Any = None, *, root_: str = root) -> Any:
            return real(a, environ, root=root_)

        with (
            overlay_env(env, clear=True),
            mock.patch.object(run_interactive, "probe_firmware", rooted),
            contextlib.redirect_stdout(out),
            contextlib.redirect_stderr(err),
        ):
            rc = run_interactive.main(["firmware", arch])
        return rc, out.getvalue(), err.getvalue()

    def test_codes(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            rc, out, err = self.run_cli("x86_64", {}, root)
            self.assertEqual(rc, 1)
            self.assertIn("/usr/share/OVMF", err)
            self.assertIn("VIBEOS_FW_X86_64", err)
            d = os.path.join(root, "usr/share/OVMF")
            os.makedirs(d)
            Path(d, "OVMF_CODE_4M.fd").write_bytes(b"c")
            rc, out, err = self.run_cli("x86_64", {}, root)
            self.assertEqual(rc, 2)
            self.assertIn("OVMF_VARS_4M.fd", err)
            Path(d, "OVMF_VARS_4M.fd").write_bytes(b"v")
            rc, out, err = self.run_cli("x86_64", {}, root)
            self.assertEqual(rc, 0)
            self.assertEqual(
                out.strip(),
                f"firmware: x86_64 code={d}/OVMF_CODE_4M.fd vars={d}/OVMF_VARS_4M.fd",
            )
            rc, _, err = self.run_cli("x86_64", {"VIBEOS_FW_X86_64": f"{root}/none.fd"}, root)
            self.assertEqual(rc, 2)

    def test_bad_arch_is_a_usage_error(self) -> None:
        import contextlib
        import io

        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as cm:
            run_interactive.main(["firmware", "riscv64"])
        self.assertEqual(cm.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
