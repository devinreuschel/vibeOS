"""Host tests for scripts/doc_refs.py (DESIGN § and ROADMAP § citations, ROADMAP §10.3 DOC2)."""

from __future__ import annotations

import unittest

from scripts.doc_refs import (
    EXTERNAL,
    LAYOUT,
    REGISTER,
    ROADMAP,
    check_bare,
    check_citations,
    check_links,
    check_register,
    check_tree,
    design_index,
    headings,
    in_scope,
    register_rows,
    resolve_enforcer,
    roadmap_index,
    slug,
    tree_names,
    where,
)


def cite(doc: str, num: str) -> str:
    """A citation built at run time, so this file holds no literal dangling one."""
    return f"{doc} §{num}"


TITLES = {
    1: "Overview",
    2: "Invariants",
    3: "Boot",
    4: "Memory",
    5: "Interrupts",
    6: "Time",
    7: "SMP",
    8: "Testing",
    9: "Pitfalls",
    10: "Block I/O",
    11: "Portability",
    12: "Device model",
}

SUBS = {
    1: ["1.4 Documentation rules"],
    2: ["2.5 Panic policy"],
    3: ["3.3 `_start` order"],
    4: ["4.1 Virtual address map", "4.4 Kernel heap"],
    5: ["5.4 IRQ registration"],
    8: ["8.2 In-guest tests"],
    10: ["10.2 Ordering, flush, and FUA"],
}


def design_set() -> dict[str, str]:
    """The twelve LAYOUT files and ROADMAP.md, each minimal and correct."""
    files: dict[str, str] = {}
    for path, n in LAYOUT:
        body = [f"# {n}. {TITLES[n]}", ""]
        for sub in SUBS.get(n, []):
            body += [f"## {sub}", "", "Text.", ""]
        files[path] = "\n".join(body)
    contents = "".join(
        f"| {n} | [{TITLES[n]}]({path.removeprefix('docs/')}) |\n" for path, n in LAYOUT[1:]
    )
    head = "# Design\n\n## Contents\n\n" + contents + "\n"
    files["docs/DESIGN.md"] = head + files["docs/DESIGN.md"]
    files["docs/INVARIANTS.md"] += (
        "\n| # | Invariant | Established at | Relied on | Enforced by | Status | Holds today |\n"
        "|---|---|---|---|---|---|---|\n| I1 | Rule | `x` | §2.5 | none | documented | Yes |\n"
    )
    files[ROADMAP] = "# Roadmap\n\n## Phase 10: Hardening\n\n### 10.3 Documentation\n\n- [ ] box\n"
    return files


def index(files: dict[str, str]) -> dict[str, tuple[str, str]]:
    return design_index(files)[0]


class SlugTest(unittest.TestCase):
    def test_slug(self) -> None:
        self.assertEqual(slug("3.3 `_start` order"), "33-_start-order")
        self.assertEqual(
            slug("4.2 Physical memory: buddy allocator"), "42-physical-memory-buddy-allocator"
        )
        self.assertEqual(slug("10.2 Ordering, flush, and FUA"), "102-ordering-flush-and-fua")
        self.assertEqual(slug("10. Block I/O"), "10-block-io")
        self.assertEqual(slug("PTE flag policy"), "pte-flag-policy")

    def test_repeated_heading_gets_suffix(self) -> None:
        hs = headings("## Rules\n\n## Rules\n\n## Rules\n")
        self.assertEqual([h.slug for h in hs], ["rules", "rules-1", "rules-2"])


class HeadingsTest(unittest.TestCase):
    def test_fence_hides_heading(self) -> None:
        text = "# 3. Boot\n\n```sh\n# not a heading\n```\n\n~~~\n## 9.9 no\n~~~\n## 3.1 Yes\n"
        hs = headings(text)
        self.assertEqual([(h.line, h.level, h.number) for h in hs], [(1, 1, "3"), (10, 2, "3.1")])

    def test_numbers(self) -> None:
        text = "# 10. Block I/O\n## 10.2 Ordering, flush, and FUA\n### PTE flag policy\n"
        self.assertEqual([h.number for h in headings(text)], ["10", "10.2", None])


class LayoutTest(unittest.TestCase):
    def test_complete_set_indexes(self) -> None:
        idx, errors = design_index(design_set())
        self.assertEqual(errors, [])
        self.assertEqual(idx["5.4"], ("docs/INTERRUPTS.md", "54-irq-registration"))
        self.assertEqual(idx["1.4"], ("docs/DESIGN.md", "14-documentation-rules"))

    def test_heading_in_wrong_file(self) -> None:
        files = design_set()
        files["docs/BOOT.md"] += "\n## 4.1 Stray\n"
        errors = design_index(files)[1]
        self.assertTrue(
            any(
                e.startswith("docs/BOOT.md:")
                and "§4.1 is in the wrong file: DESIGN Contents puts §4 in docs/MEMORY.md" in e
                for e in errors
            ),
            errors,
        )

    def test_number_defined_twice(self) -> None:
        files = design_set()
        files["docs/INTERRUPTS.md"] += "\n## 5.4 Again\n"
        errors = design_index(files)[1]
        self.assertTrue(
            any("§5.4 also defined at docs/INTERRUPTS.md:" in e for e in errors), errors
        )

    def test_missing_file(self) -> None:
        files = design_set()
        del files["docs/TIME.md"]
        self.assertIn("docs/TIME.md: missing (DESIGN Contents names it)", design_index(files)[1])

    def test_missing_top_heading(self) -> None:
        files = design_set()
        files["docs/TIME.md"] = "Nothing here.\n"
        self.assertIn('docs/TIME.md: no top heading "# 6. <title>"', design_index(files)[1])

    def test_contents_omits_file(self) -> None:
        files = design_set()
        files["docs/DESIGN.md"] = files["docs/DESIGN.md"].replace("(DEVICES.md)", "(#x)")
        self.assertIn(
            "docs/DESIGN.md: Contents does not link docs/DEVICES.md", design_index(files)[1]
        )


class CitationTest(unittest.TestCase):
    def setUp(self) -> None:
        self.files = design_set()
        self.design = index(self.files)
        self.roadmap = roadmap_index(self.files[ROADMAP])

    def run_on(self, text: str, path: str = "src/x.rs") -> list[str]:
        return check_citations(path, text, self.design, self.roadmap)

    def test_design_resolves_across_files(self) -> None:
        text = f"{cite('DESIGN', '5.4')}, {cite('DESIGN.md', '2.5')}, {cite('DESIGN', '1.4')}"
        self.assertEqual(self.run_on(text), [])

    def test_design_dangling(self) -> None:
        self.assertEqual(
            self.run_on(f"x\n{cite('DESIGN', '5.99')}"),
            [f"src/x.rs:2: {cite('DESIGN', '5.99')}: no such section in the DESIGN files"],
        )

    def test_roadmap(self) -> None:
        text = f"{cite('ROADMAP', '10')} and {cite('ROADMAP.md', '10.3')}"
        self.assertEqual(self.run_on(text), [])
        self.assertEqual(
            self.run_on(cite("ROADMAP", "10.99")),
            [f"src/x.rs:1: {cite('ROADMAP', '10.99')}: no such section in docs/ROADMAP.md"],
        )

    def test_wrapped_in_rust_comment(self) -> None:
        self.assertEqual(self.run_on("// panics (DESIGN\n// §2.5) here"), [])
        self.assertEqual(
            self.run_on("x\n/// panics (DESIGN\n/// §2.99) here"),
            [f"src/x.rs:2: {cite('DESIGN', '2.99')}: no such section in the DESIGN files"],
        )

    def test_wrapped_in_markdown(self) -> None:
        self.assertEqual(self.run_on("as ROADMAP\n§10.3 says", "docs/X.md"), [])
        self.assertEqual(
            self.run_on("as\nDESIGN\n§7.99 says", "docs/X.md"),
            [f"docs/X.md:2: {cite('DESIGN', '7.99')}: no such section in the DESIGN files"],
        )

    def test_topic_file_citation(self) -> None:
        self.assertEqual(self.run_on(cite("MEMORY.md", "4.4")), [])
        self.assertEqual(
            self.run_on(cite("INVARIANTS.md", "4.4")),
            [
                f"src/x.rs:1: {cite('INVARIANTS.md', '4.4')}: not in docs/INVARIANTS.md"
                " (it is in docs/MEMORY.md)"
            ],
        )
        self.assertEqual(self.run_on(cite("MEMORY", "9.99")), [])

    def test_placeholder_is_not_a_citation(self) -> None:
        self.assertEqual(self.run_on(cite("DESIGN", "x.y") + " " + cite("ROADMAP", "x.y")), [])


def sec(num: str) -> str:
    """A bare section sign built at run time, as `cite` does for a citation."""
    return f"§{num}"


class TestBareRule(unittest.TestCase):
    def run_on(self, path: str, text: str) -> list[str]:
        files = design_set()
        files[path] = files.get(path, "") + "\n\n" + text + "\n"
        design = index(files)
        roadmap = roadmap_index(files[ROADMAP])
        return [e for e in check_bare(files, design, roadmap) if e.startswith(path + ":")]

    def assert_flags(self, path: str, text: str, nums: list[str]) -> None:
        errors = self.run_on(path, text)
        self.assertEqual([e.split(": bare ")[1] for e in errors],
                         [f"{sec(n)} resolves in no file" for n in nums], errors)

    def test_own_file_roadmap(self) -> None:
        self.assert_flags(ROADMAP, f"- [ ] as {sec('10.3')} says", [])
        self.assert_flags(ROADMAP, f"- [ ] as {sec('5.4')} says", ["5.4"])

    def test_own_file_across_design_topic_files(self) -> None:
        self.assert_flags("docs/BOOT.md", f"The IRQ path ({sec('5.4')}) and {sec('4.4')}.", [])
        self.assert_flags("docs/DESIGN.md", f"See {sec('8.2')}.", [])
        self.assert_flags("docs/BOOT.md", f"See {sec('10.3')}.", ["10.3"])

    def test_earlier_design_citation(self) -> None:
        self.assert_flags(ROADMAP, f"- [ ] {cite('DESIGN', '5.4')} and {sec('4.4')} hold", [])

    def test_earlier_roadmap_citation(self) -> None:
        self.assert_flags("docs/MEMORY.md", f"{cite('ROADMAP', '10')}, {sec('10.3')} land it.", [])

    def test_citation_in_previous_sentence_does_not_count(self) -> None:
        self.assert_flags("docs/MEMORY.md", f"See {cite('ROADMAP', '10')}. Then {sec('10.3')}.",
                          ["10.3"])
        self.assert_flags(ROADMAP, f"Per {cite('DESIGN', '5.4')}. `x` and {sec('4.4')}.", ["4.4"])

    def test_wrapped_lines_join(self) -> None:
        text = f"as {cite('ROADMAP', '10')} and\n{sec('10.3')} say"
        self.assert_flags("docs/MEMORY.md", text, [])
        errors = self.run_on("docs/MEMORY.md", f"one\ntwo {sec('10.3')}")
        self.assertEqual(len(errors), 1)
        whole = design_set()["docs/MEMORY.md"] + f"\n\none\ntwo {sec('10.3')}\n"
        line = whole.splitlines().index(f"two {sec('10.3')}") + 1
        want = f"docs/MEMORY.md:{line}: bare {sec('10.3')} resolves in no file"
        self.assertEqual(errors[0], want)

    def test_column_header(self) -> None:
        table = f"| Box | ROADMAP |\n|---|---|\n| a | {sec('10.3')} |\n"
        self.assert_flags("docs/PORTABILITY.md", table, [])
        table = f"| Box | Phase |\n|---|---|\n| a | {sec('10.3')} |\n"
        self.assert_flags("docs/PORTABILITY.md", table, ["10.3"])

    def test_external_specifications(self) -> None:
        self.assertIn("virtio 1.2", EXTERNAL)
        text = " ".join(f"{name} {sec('2.7.13')}" for name in EXTERNAL)
        self.assert_flags("docs/BLOCK.md", text, [])

    def test_other_documents(self) -> None:
        text = (f"VIBEFS.md {sec('15')}, [the format](VIBEFS.md) {sec('16')}, "
                f"`docs/SYSCALL.md` {sec('2')}, and KERNEL_REVIEW {sec('8.3')}.")
        self.assert_flags("docs/BLOCK.md", text, [])

    def test_code_span_and_fence(self) -> None:
        self.assert_flags("docs/BLOCK.md", f"`{sec('99.9')}` and\n\n```\n{sec('99.8')}\n```\n", [])

    def test_blockquote_wrap(self) -> None:
        text = f"> as the box says (ROADMAP\n> {sec('10.3')}), here"
        self.assert_flags("docs/PITFALLS.md", text, [])
        self.assert_flags("docs/PITFALLS.md", f"> as the box says\n> {sec('10.3')}", ["10.3"])

    def test_check_tree_reports_bare(self) -> None:
        files = design_set()
        files["docs/TIME.md"] += f"\nSee {sec('19.5')}.\n"
        self.assertEqual(len([e for e in check_tree(files) if "bare" in e]), 1)


REG_HEAD = (
    "| # | Invariant | Established at | Relied on | Enforced by | Status | Holds today |\n"
    "|---|---|---|---|---|---|---|\n"
)

TREE = {
    "Makefile": "check: build\n\tcargo test\ntest-e2e test-kernel: iso\n\trun\nX := y\n",
    "Cargo.toml": "[workspace.lints.clippy]\nlet_underscore_must_use = \"deny\"\n"
    "needless_return = \"warn\"\n[workspace.lints.rust]\nunsafe_op_in_unsafe_fn = "
    "{ level = \"forbid\", priority = 1 }\n",
    "src/main.rs": "#![deny(\n    clippy::unwrap_used,\n    clippy::panic\n)]\nstruct Kernel;\n",
    "crates/core/src/mm.rs": "#[cfg(test)]\nmod tests {\n    #[test]\n    fn buddy_merges() {}\n"
    "    fn helper() {}\n}\n#[kani::proof]\nfn proof_range() {}\npub enum MapError {}\n",
    "src/mm/ktest.rs": "pub(crate) const TESTS: &[Test] = &[\n    test(\n"
    "        \"kernel_va0_faults\", f),\n];\n",
    "user/src/tests/console.rs": "const X: &[u8] = b\"a\\\nb\";\n"
    "t.case(\"console_forged_lines\", f);\n",
    "tests/harness/test_frame.py": "class T:\n    def test_framed(self) -> None:\n        pass\n",
    "scripts/check_entry.py": "",
}


def register(*rows: str) -> str:
    body = "".join(r + "\n" for r in rows)
    return "## 2.7 Invariant register\n\n" + REG_HEAD + body + "\n## 2.8\n"


def row(rid: str, enforced: str, status: str = "documented", relied: str = "§2.1") -> str:
    return f"| {rid} | Rule | `x` | {relied} | {enforced} | {status} | Yes |"


class TestRegister(unittest.TestCase):
    def setUp(self) -> None:
        self.tree = tree_names(TREE)

    def check(self, *rows: str) -> list[str]:
        return check_register(register(*rows), self.tree)

    def test_rows_found_by_header(self) -> None:
        rows, problems = register_rows(register(row("I1", "none"), row("I2", "`buddy_merges`")))
        self.assertEqual(problems, [])
        self.assertEqual([(r.line, r.rid) for r in rows], [(5, "I1"), (6, "I2")])
        self.assertEqual(rows[1].cells["Enforced by"], "`buddy_merges`")

    def test_each_name_kind_found(self) -> None:
        for name in ("scripts/check_entry.py", "tests/harness/test_frame.py", "src/mm",
                     "make check", "make test-kernel", "clippy::let_underscore_must_use",
                     "clippy::unwrap_used", "rust::unsafe_op_in_unsafe_fn", "buddy_merges",
                     "vibeos::mm::tests::buddy_merges", "proof_range", "kernel_va0_faults",
                     "console_forged_lines", "test_framed", "Kernel", "MapError"):
            self.assertTrue(resolve_enforcer(name, self.tree), name)

    def test_each_name_kind_missing(self) -> None:
        for name in ("scripts/check_gone.py", "make X", "make iso", "clippy::needless_return",
                     "clippy::todo", "rust::dead_code", "helper", "kernel_va1_faults",
                     "console_other", "test_unframed", "Nothing"):
            self.assertFalse(resolve_enforcer(name, self.tree), name)

    def test_missing_name_message(self) -> None:
        self.assertEqual(
            self.check(row("I7", "`buddy_merges` and `no_such_test` (commentary)")),
            [f"{REGISTER}:5: I7: Enforced by names no_such_test, not in the tree"],
        )

    def test_none_under_each_status(self) -> None:
        want = [f"{REGISTER}:5: I3: Status is enforced and Enforced by is none"]
        self.assertEqual(self.check(row("I3", "none", "enforced")), want)
        self.assertEqual(self.check(row("I3", "none", "enforced in part (the bound)")), want)
        self.assertEqual(self.check(row("I3", "none", "documented")), [])
        self.assertEqual(self.check(row("I3", "`buddy_merges`", "enforced")), [])

    def test_empty_cells(self) -> None:
        self.assertEqual(self.check(row("I4", "")),
                         [f"{REGISTER}:5: I4: Enforced by is empty"])
        self.assertEqual(self.check(row("I4", "none", relied="")),
                         [f"{REGISTER}:5: I4: Relied on is empty"])
        self.assertEqual(self.check(row("I4", "the lints")),
                         [f"{REGISTER}:5: I4: Enforced by is neither `none` nor backticked names"])

    def test_check_tree_runs_it(self) -> None:
        files = design_set()
        files.update(TREE)
        files[REGISTER] = "# 2. Invariants\n\n" + register(row("I9", "`gone`"))
        self.assertEqual(len([e for e in check_tree(files) if "I9" in e]), 1)


class LinkTest(unittest.TestCase):
    def setUp(self) -> None:
        files = design_set()
        self.anchors = {p: [h.slug for h in headings(files[p])] for p, _ in LAYOUT}

    def test_moved_slug_in_readme(self) -> None:
        self.assertEqual(
            check_links("README.md", "[x](docs/DESIGN.md#33-_start-order)", self.anchors),
            [
                "README.md:1: link docs/DESIGN.md#33-_start-order: docs/DESIGN.md has no "
                "heading #33-_start-order (it is in docs/BOOT.md)"
            ],
        )

    def test_same_file_links(self) -> None:
        self.assertEqual(check_links("docs/BOOT.md", "[x](#33-_start-order)", self.anchors), [])
        errors = check_links("docs/BOOT.md", "a\n[x](#41-virtual-address-map)", self.anchors)
        self.assertEqual(
            errors,
            [
                "docs/BOOT.md:2: link #41-virtual-address-map: docs/BOOT.md has no heading "
                "#41-virtual-address-map (it is in docs/MEMORY.md)"
            ],
        )

    def test_roadmap_into_topic_file(self) -> None:
        text = "[x](TESTING.md#82-in-guest-tests)"
        self.assertEqual(check_links(ROADMAP, text, self.anchors), [])

    def test_link_without_fragment(self) -> None:
        self.assertEqual(check_links("README.md", "[x](docs/BOOT.md)", self.anchors), [])


class TreeTest(unittest.TestCase):
    def test_in_scope(self) -> None:
        self.assertFalse(in_scope("docs/reviews/KERNEL_REVIEW.md"))
        self.assertFalse(in_scope("CHANGELOG.md"))
        self.assertTrue(in_scope("src/main.rs"))

    def test_where(self) -> None:
        self.assertEqual(where(design_set(), "5.4"), "docs/INTERRUPTS.md#54-irq-registration")
        self.assertIsNone(where(design_set(), "5.99"))

    def test_check_tree_clean(self) -> None:
        self.assertEqual(check_tree(design_set()), [])

    def test_check_tree_skips_excluded(self) -> None:
        files = design_set()
        files["docs/reviews/old.md"] = cite("ROADMAP", "17.8")
        files["src/main.rs"] = cite("ROADMAP", "17.8")
        self.assertEqual(
            check_tree(files),
            [f"src/main.rs:1: {cite('ROADMAP', '17.8')}: no such section in docs/ROADMAP.md"],
        )


if __name__ == "__main__":
    unittest.main()
