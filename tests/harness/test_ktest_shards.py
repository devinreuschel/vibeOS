"""The in-guest shards cover each variant once (ROADMAP §10.1, TESTING.md §8.6).

`ktest_shards.SHARDS` splits `make test-kernel`, `make test-kernel-smp4` and
`make test-lapic-fallback` into per-push tiers. These tests hold the table to
its promise, every registry row and every proof boot in exactly one shard,
and the Makefile to the table: each shard has its rule, in its variant's
configuration, and `make test` runs the shards, not the full targets.
"""

from __future__ import annotations

import contextlib
import io
import os
import re
import subprocess
import unittest
from collections import Counter
from pathlib import Path

from scripts.gen_syscalls import ktest_names
from tests.harness import ktest_shards
from tests.harness.harness import overlay_env
from tests.harness.ktest_shards import AARCH64_PROOF, AARCH64_ROWS, SHARDS, VARIANTS
from tests.harness.run_ktest import PROOF_BOOTS, main, proof_boot_names, shard_plan

ROOT = Path(__file__).resolve().parents[2]
ISO_KTEST = "build/vibeos-ktest.iso"
ROW_NAME = re.compile(r"[a-z0-9_]+")


def shards_of(variant: str) -> list[tuple[str, ktest_shards.Shard]]:
    return [(name, s) for name, s in SHARDS.items() if s.variant == variant]


def prereqs(text: str, target: str) -> list[str]:
    m = re.search(rf"^{re.escape(target)}:([^=\n]*)$", text, re.M)
    assert m is not None, target
    return m.group(1).split()


def gic_for_kernel_proofs() -> dict[str, str]:
    """`test-gic-fallback-<k>` -> the test-kernel proof shard it reruns."""
    return {
        "test-gic-fallback-" + name.removeprefix("test-kernel-"): name
        for name, s in AARCH64_PROOF.items()
        if s.variant == "test-kernel"
    }


def shard_recipes(text: str) -> dict[str, str]:
    """Makefile target -> `--shard` name, for recipes that pass one."""
    out: dict[str, str] = {}
    target: str | None = None
    for line in text.splitlines():
        if line.startswith("\t"):
            if target is not None:
                m = re.search(r"--shard (\S+)", line)
                if m:
                    out[target] = target if m.group(1) == "$@" else m.group(1)
            target = None
            continue
        m = re.match(r"^(test-[\w.-]+):", line)
        target = m.group(1) if m and "=" not in line else None
    return out


def make_n(*targets: str) -> dict[str, str]:
    """Each target's recipe as `make -n` prints it, the ISO taken as built."""
    env = {k: v for k, v in os.environ.items() if k not in ("MAKEFLAGS", "MFLAGS", "MAKELEVEL")}
    out = subprocess.run(
        ["make", "-n", "-o", ISO_KTEST, *targets],
        cwd=ROOT,
        env=env,
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    recipes = {}
    for line in out.splitlines():
        m = re.match(r"VIBEOS_TIER=(\S+) ", line)
        if m:
            recipes[m.group(1)] = line
    return recipes


class ShardTable(unittest.TestCase):
    def test_variants_named_in_order(self) -> None:
        self.assertEqual({s.variant for s in SHARDS.values()}, set(VARIANTS))
        for variant in VARIANTS:
            names = [name for name, _ in shards_of(variant)]
            want = [f"{variant}-{k}" for k in range(1, len(names) + 1)]
            self.assertEqual(names, want, variant)

    def test_ranges_hand_on_their_bounds(self) -> None:
        """The main boots' ranges run from the first row past the last, each
        starting where the one before ended: every row, a row added later
        included, falls in exactly one (vibeos-core's
        `ranges_sharing_bounds_partition_the_rows`)."""
        for variant in VARIANTS:
            rows = [s.rows for _, s in shards_of(variant) if s.rows is not None]
            self.assertTrue(rows, f"{variant}: no shard runs the registry")
            self.assertEqual(rows[0][0], "", variant)
            self.assertEqual(rows[-1][1], "", variant)
            for (_, hi), (lo, _) in zip(rows, rows[1:], strict=False):
                self.assertEqual(hi, lo, variant)
                self.assertRegex(hi, ROW_NAME)
            inner = [hi for _, hi in rows[:-1]]
            self.assertEqual(len(inner), len(set(inner)), variant)

    def test_bounds_are_registered_rows(self) -> None:
        """A bound that names no row fails the shard's boot (`bad option`);
        a renamed row fails here first."""
        registered = ktest_names(ROOT)
        for name, s in SHARDS.items():
            for bound in s.rows or ():
                if bound:
                    self.assertIn(bound, registered, name)
        for name, rows in AARCH64_ROWS.items():
            for bound in rows:
                if bound:
                    self.assertIn(bound, registered, name)

    def test_aarch64_ranges_hand_on_their_bounds(self) -> None:
        """Every aarch64 shard of a variant has a range, and the ranges
        partition the registry the way the x86 main boots do."""
        for variant in VARIANTS:
            names = [name for name, _ in shards_of(variant)]
            rows = [AARCH64_ROWS[name] for name in names]
            self.assertEqual(rows[0][0], "", variant)
            self.assertEqual(rows[-1][1], "", variant)
            for (_, hi), (lo, _) in zip(rows[:-1], rows[1:], strict=True):
                self.assertEqual(hi, lo, variant)
                self.assertRegex(hi, ROW_NAME)

    def test_proof_boots_once_each(self) -> None:
        for variant, v in VARIANTS.items():
            union = proof_boot_names(v.smp, v.hpet_off)
            listed = Counter(
                b for name, _ in shards_of(variant) for b in ktest_shards.boots_for(name, "x86_64")
            )
            self.assertEqual(sorted(listed.elements()), sorted(union), variant)
        # lapic-fallback is the x86 timer config. GICv2 reruns test-kernel.
        for variant in ("test-kernel", "test-kernel-smp4"):
            v = VARIANTS[variant]
            union = proof_boot_names(v.smp, v.hpet_off, "aarch64")
            names = [name for name, _ in shards_of(variant)]
            names += [name for name, s in AARCH64_PROOF.items() if s.variant == variant]
            listed = Counter(
                b for name in names for b in ktest_shards.boots_for(name, "aarch64")
            )
            self.assertEqual(sorted(listed.elements()), sorted(union), variant)

    def test_every_shard_runs_something(self) -> None:
        for name, s in {**SHARDS, **AARCH64_PROOF}.items():
            self.assertTrue(s.rows is not None or s.boots, name)

    def test_every_listed_shard_runs_a_boot(self) -> None:
        """A shard a per-push target runs on an arch runs at least one boot
        there. An empty plan exits 1."""
        text = (ROOT / "Makefile").read_text(encoding="utf-8")
        invoked = shard_recipes(text)
        for arch, union in (("x86_64", "test"), ("aarch64", "test-aarch64")):
            for target in prereqs(text, union):
                shard = invoked.get(target)
                if shard is None:
                    continue
                s = ktest_shards.shard_for(shard)
                v = VARIANTS[s.variant]
                word, boots = shard_plan(shard, arch, v.smp, v.hpet_off)
                self.assertTrue(word or boots, f"{arch} {target} -> {shard}")

    def test_shard_that_boots_nothing_fails(self) -> None:
        """An aarch64 proof shard has no x86 plan, and that is a failure."""
        err = io.StringIO()
        with overlay_env(clear=True), contextlib.redirect_stderr(err):
            rc = main(["--arch", "x86_64", "--shard", "test-kernel-7"])
        self.assertEqual(rc, 1)
        self.assertIn("runs no boot", err.getvalue())

    def test_range_word(self) -> None:
        self.assertEqual(
            ktest_shards.Shard("v", rows=("", "a_b")).range_word(), "vibeos.ktest_range=..a_b"
        )
        self.assertEqual(
            ktest_shards.Shard("v", rows=("x", "")).range_word(), "vibeos.ktest_range=x.."
        )
        self.assertIsNone(ktest_shards.Shard("v", boots=("select",)).range_word())

    def test_proof_boot_names(self) -> None:
        self.assertEqual(len({b.name for b in PROOF_BOOTS}), len(PROOF_BOOTS))
        self.assertEqual(proof_boot_names(2, True)[0], "hpet-off")
        self.assertNotIn("hpet-off", proof_boot_names(2, False))
        self.assertIn("repeat", proof_boot_names(2, False))
        self.assertNotIn("repeat", proof_boot_names(4, False))
        self.assertIn("deadline-trip", proof_boot_names(4, False))
        self.assertNotIn("deadline-trip", proof_boot_names(1, False))
        self.assertNotIn("hpet-off", proof_boot_names(2, True, "aarch64"))
        self.assertEqual(
            proof_boot_names(2, False, "aarch64"),
            [
                "select",
                "repeat",
                "deadline-trip",
                "planted",
                "vblk-readonly",
                "vblk-bad-sector",
            ],
        )
        self.assertEqual(
            proof_boot_names(4, False, "aarch64"),
            [
                "select",
                "deadline-trip",
                "planted",
                "vblk-readonly",
                "vblk-bad-sector",
                "stalled-ap",
                "aff-off",
            ],
        )
        self.assertNotIn("aff-off", proof_boot_names(4, False))
        self.assertNotIn("aff-off", proof_boot_names(2, False, "aarch64"))
        self.assertIn("select", proof_boot_names(1, False, "aarch64"))
        self.assertNotIn("deadline-trip", proof_boot_names(1, False, "aarch64"))
        self.assertIn("stalled-ap", proof_boot_names(4, False))
        self.assertNotIn("stalled-ap", proof_boot_names(2, False))

    def test_aarch64_proof_boots_once_each(self) -> None:
        for variant, v in VARIANTS.items():
            union = proof_boot_names(v.smp, v.hpet_off, "aarch64")
            listed = Counter(
                b
                for _, s in shards_of(variant)
                for b in (*s.boots, *s.aarch64_boots)
                if b in union
            )
            self.assertEqual(sorted(listed.elements()), sorted(union), variant)

    def test_unknown_shard_is_refused(self) -> None:
        with (
            self.assertRaises(SystemExit) as cm,
            open(os.devnull, "w") as null,
            contextlib.redirect_stderr(null),
        ):
            main(["--shard", "test-kernel-0"])
        self.assertEqual(cm.exception.code, 2)


class ShardMakefile(unittest.TestCase):
    def test_shards_share_their_variants_configuration(self) -> None:
        recipes = make_n(*VARIANTS, *SHARDS)
        for variant, v in VARIANTS.items():
            full = recipes[variant]
            flag = " --hpet-off" if v.hpet_off else ""
            self.assertTrue(full.endswith(f"run_ktest.py{flag}"), full)
            self.assertEqual(" VIBEOS_SMP=4 " in full, v.smp == 4, full)
            # aarch64 env_config defaults VIBEOS_SMP to 1; the -smp 2
            # shards must set it (ROADMAP §11.7).
            self.assertEqual(" VIBEOS_SMP=2 " in full, variant == "test-kernel", full)
            base = full.removesuffix(flag).replace(f"VIBEOS_TIER={variant} ", "", 1)
            for name, _ in shards_of(variant):
                want = f"VIBEOS_TIER={name} {base} --shard {name}"
                self.assertEqual(" ".join(recipes[name].split()), " ".join(want.split()))

    def test_gic_fallback_sets_gic_and_smp(self) -> None:
        recipes = make_n("test-gic-fallback-1", "test-gic-fallback-2")
        self.assertEqual(set(recipes), {"test-gic-fallback-1", "test-gic-fallback-2"})
        for name, line in recipes.items():
            self.assertIn(" VIBEOS_GIC=2 ", f" {line} ", name)
            self.assertIn(" VIBEOS_SMP=2 ", f" {line} ", name)

    def test_aarch64_proof_shards_are_aarch64_tiers(self) -> None:
        text = (ROOT / "Makefile").read_text(encoding="utf-8")
        x86 = prereqs(text, "test")
        arm = prereqs(text, "test-aarch64")
        invoked = shard_recipes(text)
        names = [*AARCH64_PROOF, *gic_for_kernel_proofs()]
        recipes = make_n(*names)
        for name, s in AARCH64_PROOF.items():
            self.assertNotIn(name, x86, name)
            self.assertIn(name, arm, name)
            line = recipes[name]
            self.assertIn(f" VIBEOS_SMP={VARIANTS[s.variant].smp} ", f" {line} ")
            self.assertIn(f"--shard {name}", line)
            self.assertNotIn("--hpet-off", line)
        for gic, shard in gic_for_kernel_proofs().items():
            self.assertIn(gic, arm, gic)
            self.assertEqual(invoked[gic], shard)
            line = recipes[gic]
            self.assertIn(" VIBEOS_GIC=2 ", f" {line} ")
            self.assertIn(" VIBEOS_SMP=2 ", f" {line} ")
            self.assertIn(f"--shard {shard}", line)

    def test_make_test_runs_the_shards(self) -> None:
        text = (ROOT / "Makefile").read_text(encoding="utf-8")
        m = re.search(r"^test:([^=\n]*)$", text, re.M)
        assert m is not None
        prereqs = m.group(1).split()
        for variant in VARIANTS:
            self.assertNotIn(variant, prereqs)
        for name in SHARDS:
            self.assertEqual(prereqs.count(name), 1, name)


if __name__ == "__main__":
    unittest.main()
