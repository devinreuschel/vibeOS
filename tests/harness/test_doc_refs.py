"""Host tests for scripts/doc_refs.py (DESIGN § and ROADMAP § citations, ROADMAP §10.3 DOC2)."""

from __future__ import annotations

import unittest

from scripts.doc_refs import (
    LAYOUT,
    ROADMAP,
    check_citations,
    check_links,
    check_tree,
    design_index,
    headings,
    in_scope,
    roadmap_index,
    slug,
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
