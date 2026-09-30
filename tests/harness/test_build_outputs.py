"""Build outputs: named kernel ELFs and ISOs under build/ (ROADMAP §10.2).

These tests read make's own database (`make -pRrq -f Makefile :`), so they
check the rules make runs, after every variable and `$(call)` is expanded.
"""

from __future__ import annotations

import functools
import re
import subprocess
import unittest
from dataclasses import dataclass
from pathlib import Path

from tests.harness.harness import default_iso

ROOT = Path(__file__).resolve().parent.parent.parent
VARIANTS = ("default", "panic", "gp", "panic-nest", "ktest", "vibefs-crash")


@dataclass(frozen=True)
class Rule:
    prereqs: tuple[str, ...]
    recipe: tuple[str, ...]


@dataclass(frozen=True)
class MakeDb:
    variables: dict[str, str]
    rules: dict[str, Rule]


def parse_db(text: str) -> MakeDb:
    """Variables and explicit rules from `make -p` output."""
    variables: dict[str, str] = {}
    rules: dict[str, Rule] = {}
    var_re = re.compile(r"^([A-Za-z_][A-Za-z0-9_]*) :?= ?(.*)$")
    rule_re = re.compile(r"^([^#\t\s][^:=]*?):(?!=)\s*(.*)$")
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        line = lines[i]
        i += 1
        m = var_re.match(line)
        if m:
            variables[m.group(1)] = m.group(2)
            continue
        m = rule_re.match(line)
        if not m or line.startswith("."):
            continue
        target, deps = m.group(1).strip(), m.group(2).split("|")[0].split()
        recipe: list[str] = []
        while i < len(lines) and (lines[i].startswith("#") or lines[i].startswith("\t")):
            if lines[i].startswith("\t"):
                recipe.append(lines[i][1:])
            i += 1
        rules[target] = Rule(tuple(deps), tuple(recipe))
    return MakeDb(variables, rules)


@functools.cache
def make_db() -> MakeDb:
    proc = subprocess.run(
        ["make", "-pRrq", "-f", "Makefile", ":"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    return parse_db(proc.stdout)


def workflow_texts() -> dict[str, str]:
    workflows = (ROOT / ".github/workflows").glob("*.yml")
    return {p.name: p.read_text(encoding="utf-8") for p in workflows}


class ParseDbTest(unittest.TestCase):
    def test_rule_and_variable(self) -> None:
        db = parse_db(
            "ISO := build/vibeos.iso\n"
            "build/vibeos.iso: build/kernels/vibeos-default.elf limine.conf\n"
            "#  recipe to execute (from 'Makefile', line 1):\n"
            "\tscripts/mkiso.sh $< $@ x\n"
            "\n"
        )
        self.assertEqual(db.variables["ISO"], "build/vibeos.iso")
        rule = db.rules["build/vibeos.iso"]
        self.assertEqual(rule.prereqs, ("build/kernels/vibeos-default.elf", "limine.conf"))
        self.assertEqual(rule.recipe, ("scripts/mkiso.sh $< $@ x",))


class NamedOutputsTest(unittest.TestCase):
    """ROADMAP §10.2: one `target/` for every feature build."""

    def test_every_variant_has_its_elf_and_iso(self) -> None:
        db = make_db()
        elfs = db.variables["KERNEL_ELFS"].split()
        isos = db.variables["ISOS"].split()
        self.assertEqual(elfs, [f"build/kernels/vibeos-{v}.elf" for v in VARIANTS])
        self.assertEqual(isos, [default_iso(v) for v in VARIANTS])
        self.assertEqual(db.variables["KERNEL_ELF"], "build/kernels/vibeos-default.elf")

    def test_iso_reads_only_its_own_elf(self) -> None:
        db = make_db()
        for v in VARIANTS:
            rule = db.rules[default_iso(v)]
            self.assertEqual(rule.prereqs[0], f"build/kernels/vibeos-{v}.elf", v)
            others = [p for p in rule.prereqs if "vibeos-" in p and p.endswith(".elf")]
            self.assertEqual(others, [f"build/kernels/vibeos-{v}.elf"], v)
            self.assertEqual(len(rule.recipe), 1, v)
            self.assertIn("scripts/mkiso.sh $<", rule.recipe[0])
            self.assertIn(f"build/iso_root_{v}", rule.recipe[0])

    def test_elf_recipe_removes_first_and_copies_last(self) -> None:
        db = make_db()
        for v in VARIANTS:
            recipe = db.rules[f"build/kernels/vibeos-{v}.elf"].recipe
            self.assertTrue(recipe[0].startswith("rm -rf $@ "), v)
            self.assertEqual(recipe[-1], f"cp build/kernels/.vibeos-{v}/vibeos $@", v)
            builds = [line for line in recipe if " build " in line and "CARGO_SHIP" in line]
            self.assertEqual(len(builds), 2, v)
            for line in builds:
                self.assertIn(f"--artifact-dir build/kernels/.vibeos-{v}", line)
            # Nothing else writes $@.
            self.assertFalse(any("$@" in line for line in recipe[1:-1]), v)

    def test_elf_relinks_when_the_profile_changes(self) -> None:
        db = make_db()
        stamp = db.variables["PROFILE_STAMP"]
        self.assertTrue(stamp.startswith("build/"), stamp)
        self.assertIn("CARGO_PROFILE", "\n".join(db.rules[stamp].recipe))
        for v in VARIANTS:
            self.assertIn(stamp, db.rules[f"build/kernels/vibeos-{v}.elf"].prereqs, v)

    def test_no_second_target_dir(self) -> None:
        texts = {"Makefile": (ROOT / "Makefile").read_text(encoding="utf-8"), **workflow_texts()}
        for name, text in texts.items():
            self.assertIsNone(re.search(r"\btarget-[a-z]", text), name)
            # Only a directory inside target/ (check-msrv's other toolchain).
            for m in re.finditer(r"CARGO_TARGET_DIR=(\S*)", text):
                self.assertTrue(m.group(1).startswith("$(CARGO_TARGET_DIR)/"), f"{name}: {m[0]}")
        for line in make_db().rules["build/kernels/vibeos-default.elf"].recipe:
            self.assertNotIn("CARGO_TARGET_DIR", line)

    def test_outputs_under_build(self) -> None:
        db = make_db()
        for var in ("KERNEL_ELFS", "ISOS", "KERNEL_ELF", "INITRD"):
            for path in db.variables[var].split():
                rel = path.removeprefix(str(ROOT) + "/")
                self.assertTrue(rel.startswith("build/"), f"{var}: {path}")
        for v in VARIANTS:
            self.assertIn(f"build/iso_root_{v}", db.rules[default_iso(v)].recipe[0])

    def test_ci_caches_only_target(self) -> None:
        for name, text in workflow_texts().items():
            for m in re.finditer(r"uses: actions/cache@[^\n]*\n(?:[ \t]+[^\n]*\n)*", text):
                block = m.group(0)
                for path in re.findall(r"^\s+(target[^\s]*)\s*$", block, re.M):
                    self.assertEqual(path, "target", name)

    def test_default_iso(self) -> None:
        self.assertEqual(default_iso(), "build/vibeos.iso")
        self.assertEqual(default_iso("ktest"), "build/vibeos-ktest.iso")
        self.assertEqual(default_iso("vibefs-crash"), "build/vibeos-vibefs-crash.iso")
        for bad in ("", "../x", "Ktest", "a b", "vibefs_crash"):
            with self.assertRaises(ValueError):
                default_iso(bad)
        self.assertEqual(make_db().variables["ISO"], default_iso())


class HostToolDepsTest(unittest.TestCase):
    """ROADMAP §10.2: the `mkfs-vibefs`/`fsck-vibefs` and `$(INITRD)` rules depend."""

    def test_host_tools_and_initrd_list_every_source(self) -> None:
        db = make_db()
        srcs = db.variables["KERNEL_SRCS"].split()
        for path in ("crates/core/src/lib.rs", "crates/core/src/block/part.rs"):
            self.assertIn(path, srcs)
        self.assertTrue(any(p.startswith("crates/core/src/fs/") for p in srcs))
        targets = [db.variables[v] for v in ("MKFS_VIBEFS", "FSCK_VIBEFS", "INITRD")]
        for target in targets:
            prereqs = set(db.rules[target].prereqs)
            self.assertIn("Cargo.lock", prereqs, target)
            self.assertIn("tests/hostlib/Cargo.toml", prereqs, target)
            missing = [p for p in srcs if p not in prereqs]
            self.assertEqual(missing, [], target)
            bins = sorted(str(p.relative_to(ROOT)) for p in
                          (ROOT / "tests/hostlib/src/bin").glob("*.rs"))
            self.assertEqual([b for b in bins if b not in prereqs], [], target)


class TrimPathsTest(unittest.TestCase):
    """ROADMAP §10.2: no host path in any artifact (per-push half)."""

    def test_cargo_ship_trims_paths(self) -> None:
        ship = make_db().variables["CARGO_SHIP"]
        # Built whole, as CI builds it (check_stack_sizes.py screens this ELF).
        self.assertTrue(ship.startswith("CARGO_INCREMENTAL=0 $(CARGO) "), ship)
        self.assertIn("-Ztrim-paths", ship.split())
        self.assertIn("--config 'profile.$(CARGO_PROFILE).trim-paths=\"all\"'", ship)
        # The flags go before the subcommand.
        self.assertNotIn(" build", ship)

    def test_every_kernel_build_uses_cargo_ship(self) -> None:
        db = make_db()
        for v in VARIANTS:
            for line in db.rules[f"build/kernels/vibeos-{v}.elf"].recipe:
                if " build " in line:
                    self.assertIn("$(CARGO_SHIP) build ", line, v)
                    self.assertNotIn("$(CARGO) build", line, v)

    def test_no_manifest_opts_into_cargo_features(self) -> None:
        manifests = [ROOT / "Cargo.toml", *ROOT.glob("crates/*/Cargo.toml"),
                     *ROOT.glob("tests/*/Cargo.toml")]
        for m in manifests:
            text = m.read_text(encoding="utf-8")
            self.assertIsNone(re.search(r"^\s*cargo-features\s*=", text, re.M), m)


if __name__ == "__main__":
    unittest.main()
