"""Host tests for dev-host records: `make gate PHASE=N RECORD=1` and
`ci_history.py --record` (ROADMAP §10.9, the dev-host records box).

Every push goes to a bare repository in the test's own temporary directory,
never to `origin`: `ci-history` is public and a push cannot be taken back.
"""

from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from typing import Any

from scripts import ci_history, gate
from scripts.ci_history import HistoryRepo
from scripts.gate import Tools
from tests.harness.gitfixture import TempRepo

HOME = "/Users/plantuser"
SERIAL = "C02PLANTSERIAL"


class FakeHost:
    def __init__(self, system: str = "Darwin", machine: str = "arm64") -> None:
        self._system, self._machine = system, machine

    def system(self) -> str:
        return self._system

    def machine(self) -> str:
        return self._machine

    def hostnames(self) -> list[str]:
        return ["Plant-MBP.local", "Plant-MBP"]

    def user(self) -> str:
        return "plantuser"

    def home(self) -> str:
        return HOME

    def serial(self) -> str:
        return SERIAL

    def mac_model(self) -> str:
        return "Mac14,2"

    def macos(self) -> str:
        return "macOS 26.0 (25A100)"

    def qemu(self) -> dict[str, str]:
        return {"qemu-system-aarch64": "QEMU emulator version 10.2.1"}


class History:
    def __init__(self) -> None:
        self.records: list[dict[str, Any]] = []

    def runs(self, workflow: str) -> list[dict[str, Any]]:
        return []

    def dev_host_records(self) -> list[dict[str, Any]]:
        return []

    def write_record(self, path: Path, forbidden: list[tuple[str, str]]) -> str | None:
        rec = json.loads(path.read_text(encoding="utf-8"))
        self.records.append(rec)
        return "0" * 40


class Runner:
    """Checks where it runs, and plants what a run's results file may hold."""

    def __init__(self, main_tree: Path, argv: list[str], rc: int = 0) -> None:
        self.main_tree = main_tree
        self.argv = argv
        self.rc = rc
        self.seen: list[dict[str, Any]] = []

    def __call__(self, cmd: str, cwd: Path, log: Path) -> int:
        head = subprocess.run(["git", "-C", str(cwd), "rev-parse", "HEAD"], capture_output=True,
                              text=True, check=True).stdout.strip()
        self.seen.append({"cmd": cmd, "cwd": cwd, "head": head,
                          "dirty_absent": not (cwd / "dirty.txt").exists()})
        results = cwd / "build" / "results" / "aarch64-test-kernel.json"
        results.parent.mkdir(parents=True, exist_ok=True)
        data = {"schema": 1, "qemu": [{"argv": [*self.argv, str(cwd / "vibeos.iso")]}],
                "numbers": {"loom_models": 12}}
        results.write_text(json.dumps(data), encoding="utf-8")
        return self.rc


MAP = """
[[line]]
key = "models line"
[[line.entry]]
job = { workflow = "ci.yml", job = "check" }
[[line.entry]]
record = { cmd = "make models" }
"""


class RecordMode(unittest.TestCase):
    def setUp(self) -> None:
        self.repo = TempRepo()
        self.addCleanup(self.repo.cleanup)
        self.repo.write({"tests/gates/phase-10.toml": MAP, "dirty.txt": "committed\n"})
        self.repo.git("add", "-A")
        self.repo.git("commit", "-q", "-m", "map")
        self.commit = self.repo.git("rev-parse", "HEAD")
        (self.repo.path / "dirty.txt").unlink()  # a change in the main tree only

    def record(self, argv: list[str], host: FakeHost | None = None,
               rc: int = 0) -> tuple[int, list[str], Runner, History]:
        runner, history = Runner(self.repo.path, argv, rc), History()
        out: list[str] = []
        tools = Tools(runner, gate.GhCli(), history)
        code = gate.record_gate(10, self.commit, self.repo.path, tools, host or FakeHost(),
                                out.append)
        return code, out, runner, history

    def test_refuses_off_macos_arm64(self) -> None:
        for host in (FakeHost("Linux", "x86_64"), FakeHost("Darwin", "x86_64")):
            code, out, runner, history = self.record([], host)
            self.assertEqual(code, 2)
            self.assertEqual(runner.seen, [])
            self.assertEqual(history.records, [])

    def test_runs_in_a_worktree_of_the_commit(self) -> None:
        code, out, runner, history = self.record(["qemu-system-aarch64"])
        self.assertEqual(code, 0, out)
        (seen,) = runner.seen
        self.assertEqual(seen["cmd"], "make models")
        self.assertNotEqual(seen["cwd"], self.repo.path)
        self.assertEqual(seen["head"], self.commit)
        self.assertFalse(seen["dirty_absent"], "the worktree holds the committed file")
        self.assertFalse(seen["cwd"].exists(), "the worktree is removed")
        self.assertNotIn(str(seen["cwd"]), self.repo.git("worktree", "list"))
        self.assertIn("PASS  record make models  records/", out[0])

    def test_record_fields(self) -> None:
        code, _, runner, history = self.record(["qemu-system-aarch64"])
        (rec,) = history.records
        for f in ci_history.REQUIRED_RECORD_FIELDS:
            self.assertIn(f, rec)
        self.assertEqual(rec["event"], "dev-host")
        self.assertEqual(rec["host"], "dev-host")
        self.assertEqual(rec["commit"], self.commit)
        self.assertEqual(rec["head_sha"], self.commit)
        self.assertEqual((rec["phase"], rec["line"], rec["command"]),
                         (10, "models line", "make models"))
        self.assertEqual(rec["result"], "pass")
        self.assertEqual(rec["numbers"]["loom_models"], 12)
        self.assertIn("seconds", rec["numbers"])
        self.assertEqual(rec["results"][0]["qemu"][0]["argv"][-1], "<checkout>/vibeos.iso")
        self.assertEqual(rec["mac_model"], "Mac14,2")

    def test_failed_run_recorded_as_fail(self) -> None:
        code, _, _, history = self.record([], rc=3)
        self.assertEqual(code, 1)
        self.assertEqual(history.records[0]["result"], "fail")

    def test_home_path_scrubbed(self) -> None:
        code, out, _, history = self.record([f"{HOME}/qemu/bin/qemu-system-aarch64"])
        self.assertEqual(code, 0, out)
        self.assertEqual(history.records[0]["results"][0]["qemu"][0]["argv"][0],
                         "<home>/qemu/bin/qemu-system-aarch64")

    def test_planted_values_refused(self) -> None:
        for planted, kind in (("-name=Plant-MBP", "hostname"), ("/tmp/plantuser/x", "user name"),
                              (f"serial={SERIAL}", "serial number")):
            code, out, _, history = self.record(["qemu-system-aarch64", planted])
            self.assertEqual(code, 1, planted)
            self.assertEqual(history.records, [], planted)
            self.assertIn(kind, out[0])
            self.assertNotIn(planted.split("=")[-1], "\n".join(out), "values are never printed")

    def test_two_entries_sharing_a_path_refused(self) -> None:
        self.repo.write({"tests/gates/phase-10.toml": MAP + (
            '\n[[line]]\nkey = "other line"\n[[line.entry]]\nrecord = { cmd = "make miri" }\n')})
        self.repo.git("add", "-A")
        self.repo.git("commit", "-q", "-m", "two lines")
        self.commit = self.repo.git("rev-parse", "HEAD")
        code, _, runner, history = self.record([])
        self.assertEqual(code, 0)
        self.assertEqual(len(history.records), 2)
        paths = {ci_history.dev_host_record_path(r) for r in history.records}
        self.assertEqual(len(paths), 2, "one file per commit and entry")
        self.assertNotEqual(ci_history.record_entry_id(10, "models line", "make models"),
                            ci_history.record_entry_id(10, "models line", "make miri"))
        dup = MAP + '[[line.entry]]\nrecord = { cmd = "make models" }\n'
        self.repo.write({"tests/gates/phase-10.toml": dup})
        self.repo.git("add", "-A")
        self.repo.git("commit", "-q", "-m", "dup")
        self.commit = self.repo.git("rev-parse", "HEAD")
        code, out, runner, history = self.record([])
        self.assertEqual(code, 1)
        self.assertEqual(runner.seen, [], "refused before anything runs")
        self.assertIn("share one record path", out[0])


def good_record(**over: Any) -> dict[str, Any]:
    rec: dict[str, Any] = {
        "schema": 1, "event": "dev-host", "commit": "a" * 40, "head_sha": "a" * 40,
        "host": "dev-host", "mac_model": "Mac14,2", "macos": "macOS 26.0 (25A100)",
        "qemu": {"qemu-system-aarch64": "QEMU emulator version 10.2.1"}, "phase": 10,
        "line": "models line", "command": "make models", "numbers": {"seconds": 1.0},
        "result": "pass", "started": "2026-09-29T10:00:00Z",
        "finished": "2026-09-29T10:00:01Z", "results": [],
    }
    rec.update(over)
    return rec


FORBIDDEN = gate.forbidden_values(FakeHost())


class Validate(unittest.TestCase):
    def test_good(self) -> None:
        self.assertEqual(ci_history.validate_record(good_record(), FORBIDDEN), [])

    def test_refuses_each_value(self) -> None:
        for value, kind in (("Plant-MBP.local", "hostname"), ("plant-mbp", "hostname"),
                            ("plantuser", "user name"), (f"{HOME}/x", "home directory"),
                            (SERIAL, "serial number")):
            got = ci_history.validate_record(good_record(command=f"make {value}"), FORBIDDEN)
            self.assertIn(f"it holds the machine's {kind}", got, value)
            self.assertNotIn(value, "; ".join(got))

    def test_user_name_is_a_word(self) -> None:
        short = [("user name", "dev")]
        self.assertEqual(ci_history.validate_record(good_record(), short), [])
        self.assertNotEqual(ci_history.validate_record(good_record(command="cat /dev/x"),
                                                       short), [])

    def test_fields(self) -> None:
        rec = good_record()
        del rec["macos"]
        self.assertIn("no `macos`", ci_history.validate_record(rec, []))
        self.assertIn("`event` is not 'dev-host'",
                      ci_history.validate_record(good_record(event="push"), []))
        self.assertIn("`host` is not 'dev-host'",
                      ci_history.validate_record(good_record(host="Plant-MBP"), []))

    def test_path(self) -> None:
        rec = good_record()
        entry = ci_history.record_entry_id(10, "models line", "make models")
        self.assertEqual(len(entry), 12)
        self.assertEqual(ci_history.dev_host_record_path(rec),
                         f"records/2026-09-29-dev-host-{'a' * 40}-{entry}.json")
        self.assertEqual(entry, ci_history.record_entry_id(10, "models\n  line", "make models"))
        with self.assertRaises(ci_history.HistoryError):
            ci_history.dev_host_record_path(good_record(commit="../../x"))


class Writer(unittest.TestCase):
    def setUp(self) -> None:
        self.env = TempRepo()  # the fixed identity and no global git config
        self.addCleanup(self.env.cleanup)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
        self.bare = self.tmp / "remote.git"
        subprocess.run(["git", "init", "-q", "--bare", str(self.bare)], check=True)

    def history(self, name: str) -> HistoryRepo:
        h = HistoryRepo(self.tmp / name, str(self.bare), sleep=lambda s: None)
        h.open(depth=1)
        return h

    def remote_files(self) -> list[str]:
        out = subprocess.run(["git", "-C", str(self.bare), "ls-tree", "-r", "--name-only",
                              f"refs/heads/{ci_history.BRANCH}"], capture_output=True,
                             text=True, check=True).stdout
        return out.split()

    def test_writes_and_retries_after_a_concurrent_push(self) -> None:
        other = self.history("other")
        other.commit_files({"runs/ci/1.json": b"{}\n"}, "seed")
        mine = self.history("mine")
        other.commit_files({"runs/ci/2.json": b"{}\n"}, "concurrent")  # mine is now behind
        path = self.tmp / "rec.json"
        path.write_bytes(ci_history.encode(good_record()))
        pushed = ci_history.record_main(path, mine, FORBIDDEN)
        self.assertIsNotNone(pushed)
        rel = ci_history.dev_host_record_path(good_record())
        self.assertTrue(rel.startswith("records/2026-09-29-dev-host-"))
        self.assertEqual(sorted(self.remote_files()),
                         sorted(["runs/ci/1.json", "runs/ci/2.json", rel]))
        reader = self.history("reader")
        self.assertEqual(ci_history.dev_host_records(reader), [good_record()])

    def test_refuses_before_pushing(self) -> None:
        mine = self.history("mine")
        path = self.tmp / "rec.json"
        path.write_bytes(ci_history.encode(good_record(macos="Plant-MBP")))
        with self.assertRaisesRegex(ci_history.HistoryError, "hostname"):
            ci_history.record_main(path, mine, FORBIDDEN)
        self.assertIsNone(mine.remote_tip())


if __name__ == "__main__":
    unittest.main()
