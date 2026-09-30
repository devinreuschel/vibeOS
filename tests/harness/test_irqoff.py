"""Unit tests for tests/harness/irqoff.py (ROADMAP §10.3)."""

from __future__ import annotations

import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from tests.harness import irqoff, results
from tests.harness.harness import HarnessError, QemuConfig

FRAME = "[0.123456] "

BOOT_A = [
    FRAME + "vibeOS: irq: enabled",
    FRAME + "vibeOS: irqoff: on bound 100000 ns",
    FRAME + "vibeOS: irqoff: site vec0xf0 n 10 over 0 max 900 ns p99 800 ns",
    FRAME + "vibeOS: irqoff: over src/mm/heap_init.rs:120 n 1 max 150000 ns",
    FRAME + "vibeOS: irqoff: site src/mm/heap_init.rs:120 n 4 over 1 max 150000 ns p99 150000 ns",
    FRAME + "vibeOS: irqoff: site vec0xf0 n 25 over 0 max 1200 ns p99 1000 ns",
    FRAME + "vibeOS: irqoff: deliberate src/time/ktest.rs:738 n 1 max 50000000 ns "
    "clocksource 50 ms IF-off window",
    FRAME + "vibeOS: irqoff: unmatched src/arch/ktest.rs:283 n 2",
]

BOOT_B = [
    FRAME + "vibeOS: irqoff: on bound 100000 ns",
    FRAME + "vibeOS: irqoff: site vec0xf0 n 5 over 0 max 3000 ns p99 2000 ns",
    FRAME + "vibeOS: irqoff: dropped 3",
]


def cfg(accel: str = "tcg") -> QemuConfig:
    return QemuConfig(iso="build/vibeos-irqoff.iso", accel=accel)


class Parse(unittest.TestCase):
    def test_each_form(self) -> None:
        self.assertEqual(irqoff.parse(BOOT_A[1]), irqoff.Record("on", nums=(100000,)))
        self.assertEqual(
            irqoff.parse(FRAME + "vibeOS: irqoff: no reporter"), irqoff.Record("no_reporter")
        )
        self.assertEqual(
            irqoff.parse(BOOT_A[3]),
            irqoff.Record("over", "src/mm/heap_init.rs:120", (1, 150000)),
        )
        self.assertEqual(
            irqoff.parse(BOOT_A[2]), irqoff.Record("site", "vec0xf0", (10, 0, 900, 800))
        )
        self.assertEqual(
            irqoff.parse(BOOT_A[6]),
            irqoff.Record(
                "deliberate",
                "src/time/ktest.rs:738",
                (1, 50000000),
                "clocksource 50 ms IF-off window",
            ),
        )
        self.assertEqual(
            irqoff.parse(BOOT_A[7]), irqoff.Record("unmatched", "src/arch/ktest.rs:283", (2,))
        )
        self.assertEqual(irqoff.parse(BOOT_B[2]), irqoff.Record("dropped", nums=(3,)))

    def test_other_lines(self) -> None:
        self.assertIsNone(irqoff.parse(BOOT_A[0]))
        self.assertIsNone(irqoff.parse(FRAME + "vibeOS: irqoff: something else"))

    def test_glued_line(self) -> None:
        rec = irqoff.parse(BOOT_A[7] + "vibeOS: ktest: ok x")
        self.assertEqual(rec, irqoff.Record("unmatched", "src/arch/ktest.rs:283", (2,)))

    def test_deliberate_without_reason(self) -> None:
        rec = irqoff.parse(FRAME + "vibeOS: irqoff: deliberate vec0x20 n 1 max 5 ns ")
        self.assertEqual(rec, irqoff.Record("deliberate", "vec0x20", (1, 5), ""))


class Merge(unittest.TestCase):
    def test_last_line_wins_and_boots_merge(self) -> None:
        r = irqoff.Report()
        self.assertIsNotNone(r.add_boot(BOOT_A))
        self.assertIsNotNone(r.add_boot(BOOT_B))
        self.assertIsNone(r.add_boot(["no irqoff lines"]))
        self.assertEqual(len(r.boots), 2)
        a = r.boots[0].sites["vec0xf0"]
        self.assertEqual((a.n, a.max_ns, a.p99_ns), (25, 1200, 1000))
        m = r.merged()
        self.assertEqual(m["vec0xf0"].n, 30)
        self.assertEqual(m["vec0xf0"].max_ns, 3000)
        self.assertEqual(m["src/mm/heap_init.rs:120"].over, 1)
        self.assertEqual(m["src/arch/ktest.rs:283"].unmatched, 2)
        self.assertEqual(r.boots[1].dropped, 3)
        rows = r.rows()
        self.assertEqual(
            set(rows[0]),
            {"site", "n", "over", "deliberate", "max_ns", "p99_ns", "unmatched", "accel", "boot"},
        )
        self.assertEqual({row["boot"] for row in rows}, {0, 1})


class Summary(unittest.TestCase):
    def report(self) -> irqoff.Report:
        r = irqoff.Report()
        r.add_boot(BOOT_A)
        r.add_boot(BOOT_B)
        return r

    def test_tcg_lists_logged_sites_then_totals(self) -> None:
        md = irqoff.summary_markdown(self.report(), "tcg")
        self.assertIn("Logged sites (over 100,000 ns)", md)
        self.assertIn("`src/mm/heap_init.rs:120`", md)
        self.assertNotIn("`vec0xf0`", md)
        self.assertIn("Totals: 2 boots", md)
        self.assertIn("3 dropped", md)
        self.assertIn("Unmatched: `src/arch/ktest.rs:283`", md)

    def test_tcg_none_over(self) -> None:
        r = irqoff.Report()
        r.add_boot(BOOT_B)
        self.assertIn("None.", irqoff.summary_markdown(r, "tcg"))

    def test_kvm_lists_every_site_without_threshold(self) -> None:
        md = irqoff.summary_markdown(self.report(), "kvm")
        self.assertIn("no threshold", md)
        self.assertIn("| `vec0xf0` | 30 | 3000 | 2000 |", md)
        self.assertIn("`src/mm/heap_init.rs:120`", md)
        self.assertNotIn("over 100,000", md)


class Observe(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        irqoff._report = None
        self.addCleanup(setattr, irqoff, "_report", None)
        env = {k: v for k, v in os.environ.items() if k != "GITHUB_STEP_SUMMARY"}
        patcher = mock.patch.dict(os.environ, env, clear=True)
        patcher.start()
        self.addCleanup(patcher.stop)

    def results(self, tier: str) -> results.Results:
        return results.Results(tier, out_dir=Path(self.tmp.name))

    def test_noop_without_irqoff_lines(self) -> None:
        r = self.results("test-kernel")
        irqoff.observe(cfg(), ["vibeOS: ktest: begin 3"])
        self.assertNotIn("irqoff", r.as_dict())

    def test_raises_on_irqoff_tier_without_on_line(self) -> None:
        self.results(irqoff.TIER)
        with self.assertRaisesRegex(HarnessError, "not an irqoff build"):
            irqoff.observe(cfg(), ["vibeOS: ktest: begin 3"])

    def test_rows_through_add_section_and_summary(self) -> None:
        r = self.results(irqoff.TIER)
        summary = Path(self.tmp.name) / "summary.md"
        os.environ["GITHUB_STEP_SUMMARY"] = str(summary)
        irqoff.observe(cfg(), BOOT_A)
        irqoff.observe(cfg(), BOOT_B)
        rows = r.as_dict()["irqoff"]
        self.assertEqual({row["boot"] for row in rows}, {0, 1})
        self.assertTrue(all(row["accel"] == "tcg" for row in rows))
        path = r.write()
        self.assertEqual(json.loads(path.read_text())["irqoff"], rows)
        text = summary.read_text()
        self.assertIn("Logged sites (over 100,000 ns)", text)
        self.assertIn("`src/mm/heap_init.rs:120`", text)


class ResultsSection(unittest.TestCase):
    def test_add_section_appends_and_append_merges_drivers(self) -> None:
        with tempfile.TemporaryDirectory() as d, mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("VIBEOS_RESULTS_APPEND", None)
            first = results.Results("test-irqoff", out_dir=Path(d))
            first.record("ktest", "a", "passed")
            first.add_section("irqoff", [{"site": "vec0xf0", "boot": 0}])
            first.add_section("irqoff", [{"site": "vec0x20", "boot": 1}])
            first.write()
            os.environ["VIBEOS_RESULTS_APPEND"] = "1"
            second = results.Results("test-irqoff", out_dir=Path(d))
            second.record("marker", "irq_enabled", "passed")
            second.add_section("irqoff", [{"site": "syscall:entry", "boot": 0}])
            data = json.loads(second.write().read_text())
            self.assertEqual(data["schema"], 1)
            self.assertEqual(
                [r["site"] for r in data["irqoff"]], ["vec0xf0", "vec0x20", "syscall:entry"]
            )
            self.assertEqual([r["boot"] for r in data["irqoff"]], [0, 1, 2])
            self.assertEqual(data["ktest"]["passed"], ["a"])
            self.assertEqual(data["marker"]["passed"], ["irq_enabled"])

    def test_schema_key_refused(self) -> None:
        r = results.Results("adhoc", out_dir=Path(tempfile.gettempdir()))
        with self.assertRaises(ValueError):
            r.add_section("ktest", [])


if __name__ == "__main__":
    unittest.main()
