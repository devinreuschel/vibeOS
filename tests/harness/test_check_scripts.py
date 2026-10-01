"""Every `scripts/check_<name>.py` has a `tests/harness/test_<name>.py` that imports it
(ROADMAP §10.9).

A test counts only when its module imports the script: `import scripts.check_<name>`,
`from scripts.check_<name> import ...`, or `from scripts import check_<name>`. A test that
only runs the script as a subprocess does not count.
"""

from __future__ import annotations

import ast
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def imports_module(tree: ast.Module, module: str) -> bool:
    """`tree` imports `scripts.<module>` in one of the three forms."""
    full = f"scripts.{module}"
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            if any(a.name == full for a in node.names):
                return True
        elif isinstance(node, ast.ImportFrom) and node.level == 0:
            if node.module == full:
                return True
            if node.module == "scripts" and any(a.name == module for a in node.names):
                return True
    return False


def missing_tests(scripts_dir: Path, tests_dir: Path) -> list[str]:
    """Each `check_<name>.py` in `scripts_dir` whose `test_<name>.py` in `tests_dir` is
    missing or does not import it, with the reason."""
    out: list[str] = []
    for script in sorted(scripts_dir.glob("check_*.py")):
        module = script.stem
        test = tests_dir / f"test_{module[len('check_'):]}.py"
        if not test.is_file():
            out.append(f"{script.name}: no {test.name}")
            continue
        try:
            tree = ast.parse(test.read_text(encoding="utf-8"), filename=str(test))
        except (SyntaxError, UnicodeDecodeError) as e:
            out.append(f"{script.name}: {test.name} does not parse: {e}")
            continue
        if not imports_module(tree, module):
            out.append(f"{script.name}: {test.name} does not import scripts.{module}")
    return out


class TestMissingTests(unittest.TestCase):
    def plant(self, tests: dict[str, str]) -> list[str]:
        with tempfile.TemporaryDirectory() as d:
            scripts = Path(d) / "scripts"
            harness = Path(d) / "harness"
            scripts.mkdir()
            harness.mkdir()
            (scripts / "check_foo.py").write_text("", encoding="utf-8")
            (scripts / "gatelib.py").write_text("", encoding="utf-8")
            for name, body in tests.items():
                (harness / name).write_text(body, encoding="utf-8")
            return missing_tests(scripts, harness)

    def test_real_tree(self) -> None:
        self.assertEqual(missing_tests(ROOT / "scripts", ROOT / "tests" / "harness"), [])

    def test_missing_test_file_fails(self) -> None:
        self.assertEqual(self.plant({}), ["check_foo.py: no test_foo.py"])

    def test_subprocess_only_fails(self) -> None:
        body = (
            "import subprocess\n"
            "subprocess.run(['python3', 'scripts/check_foo.py'], check=False)\n"
        )
        got = self.plant({"test_foo.py": body})
        self.assertEqual(got, ["check_foo.py: test_foo.py does not import scripts.check_foo"])

    def test_other_script_import_fails(self) -> None:
        got = self.plant({"test_foo.py": "from scripts import check_foobar\n"})
        self.assertEqual(len(got), 1)

    def test_import_forms_pass(self) -> None:
        for body in (
            "import scripts.check_foo\n",
            "import scripts.check_foo as cf\n",
            "from scripts.check_foo import main\n",
            "from scripts import check_foo\n",
            "from scripts import gatelib, check_foo\n",
            "def f() -> None:\n    from scripts import check_foo\n",
        ):
            with self.subTest(body=body):
                self.assertEqual(self.plant({"test_foo.py": body}), [])

    def test_unparsable_test_fails(self) -> None:
        got = self.plant({"test_foo.py": "def (:\n"})
        self.assertEqual(len(got), 1)
        self.assertIn("does not parse", got[0])


if __name__ == "__main__":
    unittest.main()
