"""Host tests for tests/harness/nesting.py (ROADMAP §11.7, the KVM leg's
nesting record): the probe against recorded QMP replies, and its module check."""

from __future__ import annotations

import tempfile
import unittest
from collections.abc import Collection, Sequence
from pathlib import Path
from unittest import mock

from tests.harness import nesting
from tests.harness.harness import HarnessError
from tests.harness.qmp import Event, QmpError, load_stream

FIXTURES = Path(__file__).resolve().parent / "fixtures" / "nesting"


class ReplayQmp:
    """Answers each command with the next recorded reply, which must be for
    the same command and arguments."""

    def __init__(self, rows: list[dict[str, object]]) -> None:
        self.rows = list(rows)

    def execute(
        self,
        cmd: str,
        args: dict[str, object] | None = None,
        *,
        fds: Sequence[int] = (),
        timeout_s: float = 10.0,
    ) -> object:
        if not self.rows:
            raise AssertionError(f"no recorded reply for {cmd}")
        row = self.rows.pop(0)
        if (row.get("execute"), row.get("arguments")) != (cmd, args):
            raise AssertionError(f"sent {cmd} {args}, recorded {row}")
        err = row.get("error")
        if isinstance(err, dict):
            raise QmpError(f"QMP {cmd}: {err.get('class')}: {err.get('desc')}")
        return row.get("return")

    def poll(self) -> list[Event]:
        return []

    def wait_event(self, names: Collection[str], timeout_s: float) -> Event | None:
        return None

    def close(self) -> None:
        pass


def replay(path: Path) -> tuple[str, ReplayQmp]:
    header, rows = load_stream(path)
    expect = header.get("expect")
    assert isinstance(expect, str)
    return expect, ReplayQmp(rows)


class RecordedReplies(unittest.TestCase):
    def test_every_fixture(self) -> None:
        paths = sorted(FIXTURES.glob("*.jsonl"))
        self.assertGreaterEqual(len(paths), 4)
        seen = set()
        for p in paths:
            with self.subTest(fixture=p.name):
                expect, qmp = replay(p)
                self.assertIn(expect, nesting.NESTINGS)
                self.assertEqual(nesting.read_vcpu(qmp), expect)
                self.assertEqual(qmp.rows, [], "every recorded reply was asked for")
                seen.add(expect)
        self.assertEqual(seen, set(nesting.NESTINGS), "a fixture for each answer")

    def test_other_error_raises(self) -> None:
        _, qmp = replay(FIXTURES / "tcg-skylake-none.jsonl")
        qmp.rows[1] = {**qmp.rows[1], "error": {"class": "CommandNotFound", "desc": "gone"}}
        with self.assertRaises(QmpError):
            nesting.read_vcpu(qmp)

    def test_bad_shapes_raise(self) -> None:
        shapes: list[object] = [[], {}, [{"cpu-index": 0}], [{"qom-path": ""}]]
        for ret in shapes:
            qmp = ReplayQmp([{"execute": "query-cpus-fast", "arguments": None, "return": ret}])
            with self.subTest(ret=ret), self.assertRaises(HarnessError):
                nesting.read_vcpu(qmp)
        _, qmp = replay(FIXTURES / "tcg-skylake-none.jsonl")
        qmp.rows[1] = {**qmp.rows[1], "return": "off"}
        with self.assertRaises(HarnessError):
            nesting.read_vcpu(qmp)


class Modules(unittest.TestCase):
    def sysfs(self, d: Path, values: dict[str, str]) -> Path:
        for mod, v in values.items():
            p = d / mod / "parameters" / "nested"
            p.parent.mkdir(parents=True)
            p.write_text(v + "\n", encoding="ascii")
        return d

    def test_nested_values(self) -> None:
        for values, want in (
            ({}, []),
            ({"kvm_intel": "N"}, []),
            ({"kvm_amd": "0"}, []),
            ({"kvm_intel": "Y"}, ["kvm_intel"]),
            ({"kvm_amd": "1"}, ["kvm_amd"]),
        ):
            with self.subTest(values=values), tempfile.TemporaryDirectory() as d:
                self.assertEqual(nesting.nested_modules(self.sysfs(Path(d), values)), want)

    def test_no_nested_module_starts_no_qemu(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            root = self.sysfs(Path(d), {"kvm_intel": "N"})
            with mock.patch.object(nesting, "probe_qemu") as pq:
                self.assertEqual(nesting.probe(root), "none")
            pq.assert_not_called()

    def test_nested_module_asks_the_vcpu(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            root = self.sysfs(Path(d), {"kvm_amd": "1"})
            with mock.patch.object(nesting, "probe_qemu", return_value="svm") as pq:
                self.assertEqual(nesting.probe(root, "qemu-x"), "svm")
            pq.assert_called_once_with("qemu-x")

    def test_argv_is_halted_host_cpu_under_kvm(self) -> None:
        argv = nesting.qemu_argv("qemu-system-x86_64", "/s/q.sock")
        self.assertIn("-S", argv)
        self.assertEqual(argv[argv.index("-accel") + 1], "kvm")
        self.assertEqual(argv[argv.index("-cpu") + 1], "host")
        self.assertEqual(argv[argv.index("-qmp") + 1], "unix:/s/q.sock,server=on,wait=off")


if __name__ == "__main__":
    unittest.main()
