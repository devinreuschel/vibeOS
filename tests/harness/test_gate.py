"""Host tests for scripts/gate.py, `make gate PHASE=N` (ROADMAP §10.9)."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from scripts.check_gates import Entry
from scripts.gate import EntryResult, Tools, line_verdict, run_gate

C = "c" * 40


ROADMAP = """# Roadmap
## Phase 4: SMP
**Exit gate**
- [x] four
- [ ] tag `phase-4` and release `v0.4.0`

### 4.1 Cores
- [ ] an old box. Reopened by F001, lands in §10.4.
- [x] a ticked box. lands in §10.4.
- [ ] a box whose example `lands in §10.2` sits in a code span. lands in §11.1.
## Phase 9: Users
**Exit gate**
- [x] nine ticked
- [ ] nine open
- [ ] tag `phase-9` and release `v0.9.0`

### 9.1 Loader
- [ ] a deferred box, lands in §10.3.
## Phase 10: Consolidation
**Exit gate**
- [x] line one
- [ ] line two
- [ ] tag `phase-10` and release `v0.10.0`

### 10.1 Gates
- [ ] no note
- [ ] deferred, lands in §11.2.
- [ ] not deferred, lands in §10.2.
- [x] ticked with no note
## Phase 11: Portability
**Exit gate**
- [ ] eleven
- [ ] tag `phase-11` and release `v0.11.0`

### 11.1 Ports
- [ ] eleven open, lands in §12.1.
### 11.8 Stretch: riscv64
- [ ] a stretch box
# Beyond
- [ ] a proposal, lands in §10.9.
"""


def map_text(one: list[str], two: list[str]) -> str:
    def entries(rows: list[str]) -> str:
        return "".join(f"[[line.entry]]\n{r}\n" for r in rows)

    return (f'[[line]]\nkey = "line one"\n{entries(one)}\n'
            f'[[line]]\nkey = "line two"\n{entries(two)}')


class FakeRunner:
    """Exit status by command; writes a results file when told to."""

    def __init__(self, rc: dict[str, int] | None = None,
                 results: dict[str, tuple[str, list[str]]] | None = None) -> None:
        self.rc = rc or {}
        self.results = results or {}
        self.calls: list[tuple[str, Path]] = []

    def __call__(self, cmd: str, cwd: Path, log: Path) -> int:
        self.calls.append((cmd, cwd))
        if cmd in self.results:
            tier, passed = self.results[cmd]
            p = cwd / "build" / "results" / f"x86_64-{tier}.json"
            p.parent.mkdir(parents=True, exist_ok=True)
            data = {"schema": 1, "ktest": {"passed": passed, "skipped": [], "failed": []}}
            p.write_text(json.dumps(data), encoding="utf-8")
        return self.rc.get(cmd, 0)



class Tree:
    def __init__(self, case: unittest.TestCase, files: dict[str, str]) -> None:
        tmp = tempfile.TemporaryDirectory()
        case.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        for rel, text in files.items():
            p = self.root / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text, encoding="utf-8")

    def gate(self, phase: int = 10, runner: FakeRunner | None = None) -> tuple[bool, list[str]]:
        out: list[str] = []
        ok = run_gate(phase, C, self.root, Tools(runner or FakeRunner()), out.append)
        return ok, out


def tree(case: unittest.TestCase, one: list[str], two: list[str],
         roadmap: str = ROADMAP) -> Tree:
    """A tree whose roadmap has only phase 10's section boxes ticked, so the
    box rules pass and the lines decide."""
    quiet = "\n".join(
        line.replace("- [ ]", "- [x]") if "lands in §10" in line or line.endswith(("no note",
                                                                                   "§10.2."))
        else line
        for line in roadmap.splitlines()
    )
    return Tree(case, {"docs/ROADMAP.md": quiet + "\n",
                       "tests/gates/phase-10.toml": map_text(one, two)})


def row(out: list[str], line: int) -> str:
    return next(r for r in out if f"  L{line}  " in r)


class Verdicts(unittest.TestCase):
    def test_line_verdict(self) -> None:
        e = Entry("cmd", "true", "", "", False)
        self.assertTrue(line_verdict([EntryResult(e, True), EntryResult(e, True)]))
        self.assertFalse(line_verdict([EntryResult(e, True), EntryResult(e, False)]))
        self.assertFalse(line_verdict([]))

    def test_every_entry_required(self) -> None:
        t = tree(self, ['cmd = "a"', 'cmd = "b"'], ['cmd = "c"'])
        ok, out = t.gate(runner=FakeRunner({"b": 1}))
        self.assertFalse(ok)
        self.assertTrue(row(out, 21).startswith("FAIL"))
        self.assertTrue(row(out, 22).startswith("PASS"))
        ok, out = t.gate()
        self.assertTrue(ok, out)
        self.assertEqual(out[-1], f"gate: phase 10 at {C}: pass")

    def test_expect_fail_needs_a_plain_pass(self) -> None:
        t = tree(self, ['cmd = "plain"', 'cmd = "bad"\nexpect = "fail"'], ['cmd = "c"'])
        ok, _ = t.gate(runner=FakeRunner({"bad": 1}))
        self.assertTrue(ok)
        ok, _ = t.gate(runner=FakeRunner({"bad": 0}))
        self.assertFalse(ok)
        runner = FakeRunner({"plain": 1, "bad": 1})
        ok, out = t.gate(runner=runner)
        self.assertFalse(ok)
        self.assertNotIn("bad", [c for c, _ in runner.calls])
        self.assertTrue(any("no plain entry of this line passed" in r for r in out))
        only = tree(self, ['cmd = "bad"\nexpect = "fail"'], ['cmd = "c"'])
        self.assertFalse(only.gate(runner=FakeRunner({"bad": 1}))[0])

    def test_identical_commands_run_once(self) -> None:
        runner = FakeRunner()
        t = tree(self, ['cmd = "make check"'], ['cmd = "make check"', 'cmd = "x"'])
        self.assertTrue(t.gate(runner=runner)[0])
        self.assertEqual([c for c, _ in runner.calls], ["make check", "x"])
        self.assertTrue(all(cwd == t.root for _, cwd in runner.calls))

    def test_ktest_selection(self) -> None:
        cmd = "VIBEOS_KTEST='lifetime_*,exit_burst' make test-kernel"
        t = tree(self, [f'cmd = "{cmd}"'], ['cmd = "c"'])
        good = FakeRunner(results={cmd: ("test-kernel", ["lifetime_a", "exit_burst"])})
        self.assertTrue(t.gate(runner=good)[0])
        missing = FakeRunner(results={cmd: ("test-kernel", ["lifetime_a"])})
        ok, out = t.gate(runner=missing)
        self.assertFalse(ok)
        self.assertTrue(any("exit_burst did not pass" in r for r in out))
        no_glob = FakeRunner(results={cmd: ("test-kernel", ["exit_burst"])})
        ok, out = t.gate(runner=no_glob)
        self.assertFalse(ok)
        self.assertTrue(any("lifetime_* matched no passed test" in r for r in out))

    def test_stale_results_file_deleted(self) -> None:
        cmd = "VIBEOS_KTEST=a make test-kernel"
        t = tree(self, [f'cmd = "{cmd}"'], ['cmd = "c"'])
        stale = t.root / "build" / "results" / "x86_64-test-kernel.json"
        stale.parent.mkdir(parents=True)
        stale.write_text(json.dumps({"schema": 1, "ktest": {"passed": ["a"]}}), encoding="utf-8")
        ok, out = t.gate(runner=FakeRunner())
        self.assertFalse(ok)
        self.assertTrue(any("no results file" in r for r in out))

    def test_one_row_per_gate_line(self) -> None:
        t = tree(self, ['cmd = "a"'], ['cmd = "b"'])
        _, out = t.gate()
        heads = [r for r in out if r[:4] in ("PASS", "FAIL", "TAG ")]
        self.assertEqual([r.split()[1] for r in heads], ["L21", "L22", "L23"])
        self.assertTrue(row(out, 23).startswith("TAG   L23  tag `phase-10`"))



if __name__ == "__main__":
    unittest.main()
