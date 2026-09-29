"""Unit tests for scripts/check_markers.py (ROADMAP §10.2, C-MARKERS).

Runs under `python3 -m unittest discover`. Standard-library only.
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from scripts import check_markers
from scripts.check_markers import HOLE, render, scan_calls

ROADMAP = "# Roadmap\n\n### 1.1 One\n\n### 2.2 Two\n"
MARKER_RS = (
    'pub const READY: &str = "vibeOS: ready";\npub const CPU_PREFIX: &str = "vibeOS: cpu";\n'
)


def row(text: str, kind: str = "diagnostic", source: str = "kernel", extra: str = "") -> str:
    return (
        f'[[marker]]\ntext = "{text}"\nkind = "{kind}"\narch = "both"\n'
        f'source = "{source}"\nsection = "§1.1"\n{extra}\n'
    )


ROWS = (
    row("vibeOS: ready", "contract", extra='order = 10\nname = "ready"')
    + row("vibeOS: cpu<n> up", "contract", extra='order = 20\nname = "cpu<n>"\nrepeat = "per_ap"')
    + row("vibeOS: panic:", "failure")
    + row("vibeOS: halt: <why>", "failure")
)
MAIN = (
    "fn main() {\n"
    "    crate::marker!(marker::READY);\n"
    '    crate::marker!("{}{} up", marker::CPU_PREFIX, id);\n'
    "}\n"
)


class Tree:
    """A throwaway tree with the files `check_markers.check` reads."""

    def __init__(self, test: unittest.TestCase, rows: str = ROWS, **src: str) -> None:
        tmp = tempfile.TemporaryDirectory()
        test.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.write("docs/ROADMAP.md", ROADMAP)
        self.write("crates/core/src/marker.rs", MARKER_RS)
        self.write("tests/contract/markers.toml", rows)
        self.write("src/main.rs", MAIN)
        for name, text in src.items():
            self.write(f"src/{name}.rs", text)

    def write(self, path: str, text: str) -> None:
        p = self.root / path
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text, encoding="utf-8")

    def errors(self, printers: list[tuple[str, str, str, str]] | None = None) -> list[str]:
        return check_markers.check(self.root, printers=printers or [])[0]


class TestChecks(unittest.TestCase):
    def assertOneError(self, errors: list[str], needle: str) -> None:
        self.assertEqual(len(errors), 1, errors)
        self.assertIn(needle, errors[0])

    def test_fixture_is_clean(self) -> None:
        self.assertEqual(Tree(self).errors(), [])

    def test_unregistered_marker_fails(self) -> None:
        """Check 1."""
        t = Tree(self, extra='fn f() {\n    crate::marker!("vibeOS: zz: new");\n}\n')
        self.assertOneError(t.errors(), "src/extra.rs:2: 'vibeOS: zz: new' matches no row")

    def test_placeholder_line_matches_a_row(self) -> None:
        t = Tree(self, extra='fn f(e: E) {\n    marker!("vibeOS: halt: {}", e.as_str());\n}\n')
        self.assertEqual(t.errors(), [])

    def test_kernel_row_without_call_fails(self) -> None:
        """Check 2; a failure or user row needs no call."""
        rows = ROWS + row("vibeOS: gone") + row("user: hi", source="user")
        self.assertOneError(Tree(self, rows).errors(), "row 'vibeOS: gone' matches no marker!")

    def test_row_covered_by_a_wider_call(self) -> None:
        """Check 2 matches in either direction: a call's placeholder covers a
        row's literal text."""
        rows = ROWS + row("vibeOS: time: hpet <n>/ms", "contract", extra='order = 30\nname = "t"')
        rows += row("vibeOS: time: <source> <n>/ms")
        t = Tree(self, rows, time='fn t() {\n    marker!("vibeOS: time: {} {}/ms", s, n);\n}\n')
        self.assertEqual(t.errors(), [])

    def test_constant_differs_from_row_fails(self) -> None:
        """Check 3."""
        t = Tree(self)
        t.write("crates/core/src/marker.rs", MARKER_RS + 'pub const OLD: &str = "vibeOS: old";\n')
        self.assertOneError(t.errors(), "crates/core/src/marker.rs:3: OLD = 'vibeOS: old'")
        t.write("crates/core/src/marker.rs", MARKER_RS + 'pub const UP_SUFFIX: &str = " down";\n')
        self.assertOneError(t.errors(), "UP_SUFFIX = ' down' ends no row's text")

    def test_failure_head_in_contract_row_fails(self) -> None:
        """Check 4: a row a green boot prints may not hold a failure head."""
        rows = ROWS + row("vibeOS: panic: none", source="kernel")
        t = Tree(self, rows, extra='fn f() {\n    marker!("vibeOS: panic: none");\n}\n')
        self.assertOneError(t.errors(), "would fail a boot on failure 'vibeOS: panic:'")
        rows = ROWS + row("vibeOS: halt: ok")
        t = Tree(self, rows, extra='fn f() {\n    marker!("vibeOS: halt: ok");\n}\n')
        self.assertOneError(t.errors(), "would fail a boot on failure 'vibeOS: halt: '")

    def test_failure_head_of_another_source_passes(self) -> None:
        rows = ROWS + row("?vibeOS: panic: forged", source="user")
        self.assertEqual(Tree(self, rows).errors(), [])

    def test_schema_error_fails(self) -> None:
        t = Tree(self, ROWS + row("vibeOS: x").replace('kind = "diagnostic"', 'kind = "note"'))
        self.assertOneError(t.errors(), "row 5: kind 'note'")

    def test_section_must_be_a_heading(self) -> None:
        t = Tree(self, ROWS + row("vibeOS: panic: x", "failure").replace("§1.1", "§3.3"))
        self.assertOneError(t.errors(), "section §3.3 is no `### N.M` heading")

    def test_printer_line(self) -> None:
        rows = ROWS + row("vibeOS: block: <name> <n> sectors")
        t = Tree(self, rows)
        t.write("src/block.rs", "fn write_marker() {}\n")
        printer = ("src/block.rs", "fn write_marker(", "vibeOS: block: {} {} sectors", "why")
        self.assertEqual(t.errors([printer]), [])
        self.assertEqual(len(t.errors()), 1)

    def test_printer_needle_missing_fails(self) -> None:
        t = Tree(self)
        printer = ("src/main.rs", "fn write_marker(", "vibeOS: ready", "why")
        self.assertOneError(t.errors([printer]), "needle 'fn write_marker(' not found")

    def test_tree_is_clean(self) -> None:
        errors, rows, lines = check_markers.check()
        self.assertEqual(errors, [])
        self.assertGreater(rows, 100)
        self.assertGreater(lines, 100)


class TestParser(unittest.TestCase):
    CONSTS = {"READY": "vibeOS: ready", "P": "vibeOS: p "}

    def lines(self, text: str) -> list[str]:
        calls, errors, _ = scan_calls("src/x.rs", text, self.CONSTS)
        self.assertEqual(errors, [])
        return [c.rendered for c in calls]

    def test_multi_line(self) -> None:
        text = (
            'fn f() {\n    crate::marker!(\n        "vibeOS: a {} b",\n'
            "        (x, y).0\n    );\n}\n"
        )
        self.assertEqual(self.lines(text), [f"vibeOS: a {HOLE} b"])
        self.assertEqual(scan_calls("src/x.rs", text)[0][0].line, 2)

    def test_named_and_spec(self) -> None:
        self.assertEqual(
            self.lines('marker!("vibeOS: {name} at {:#x} {0:>4}", a);'),
            [f"vibeOS: {HOLE} at {HOLE} {HOLE}"],
        )

    def test_escaped_braces(self) -> None:
        self.assertEqual(
            self.lines('marker!("vibeOS: {{set}} {}", n);'), [f"vibeOS: {{set}} {HOLE}"]
        )

    def test_marker_const_args(self) -> None:
        self.assertEqual(self.lines("marker!(marker::READY);"), ["vibeOS: ready"])
        self.assertEqual(self.lines("marker!(crate::marker::READY);"), ["vibeOS: ready"])
        self.assertEqual(
            self.lines('marker!("{}{} up", marker::P, n);'), [f"vibeOS: p {HOLE} up"]
        )
        self.assertEqual(self.lines('marker!("{} {x}", n, x = marker::P);'),
                         [f"{HOLE} vibeOS: p "])

    def test_escapes(self) -> None:
        self.assertEqual(self.lines(r'marker!("vibeOS: a\nb\x1ec");'), ["vibeOS: a\nb\x1ec"])

    def test_comments(self) -> None:
        text = (
            '// marker!("vibeOS: line comment");\n'
            '/* marker!("vibeOS: block comment"); */\n'
            '/// `marker!("vibeOS: doc")`\n'
            'let s = "marker!(\\"vibeOS: in a string\\")";\n'
            'marker!("vibeOS: real"); // marker!("vibeOS: after")\n'
        )
        self.assertEqual(self.lines(text), ["vibeOS: real"])

    def test_macro_rules_body_skipped(self) -> None:
        text = (
            "macro_rules! marker {\n    ($($a:tt)*) => { $crate::marker!(x) };\n}\n"
            'fn f() { crate::marker!("vibeOS: real"); }\n'
        )
        self.assertEqual(self.lines(text), ["vibeOS: real"])

    def test_non_literal_argument_is_an_error(self) -> None:
        _, errors, _ = scan_calls("src/x.rs", "fn f() {\n    marker!(make_line());\n}\n", {})
        self.assertEqual(len(errors), 1)
        self.assertIn("src/x.rs:2: marker! argument 'make_line()'", errors[0])

    def test_forwarder(self) -> None:
        files = {
            "src/boot.rs": 'pub fn halt_with(msg: &str) -> ! {\n    crate::marker!(msg);\n}\n',
            "src/a.rs": (
                'fn a() {\n    boot::halt_with("vibeOS: a: {} literal");\n'
                '    let stack = |pages, why| alloc(pages).unwrap_or_else(|_| halt_with(why));\n'
                '    stack(4, "vibeOS: a: stack");\n}\n'
            ),
            "src/b.rs": 'fn b() {\n    let s = pick();\n    crate::boot::halt_with(s);\n}\n',
        }
        calls, errors = check_markers._forwarders(files)
        self.assertEqual(
            sorted((c.path, c.line, c.rendered) for c in calls),
            [("src/a.rs", 2, "vibeOS: a: {} literal"), ("src/a.rs", 4, "vibeOS: a: stack")],
        )
        self.assertEqual(len(errors), 1)
        self.assertIn("src/b.rs:3: halt_with() is a marker! forwarder", errors[0])

    def test_render(self) -> None:
        self.assertEqual(render("a {} {{b}} {:04x}"), f"a {HOLE} {{b}} {HOLE}")


if __name__ == "__main__":
    unittest.main()
