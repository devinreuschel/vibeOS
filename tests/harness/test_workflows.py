"""Host tests for scripts/check_workflows.py (ROADMAP §10.1 workflow rules)."""

from __future__ import annotations

import textwrap
import unittest
from pathlib import Path

from scripts.check_workflows import (
    ROOT,
    Node,
    Problem,
    Tree,
    Unsupported,
    check,
    load_tree,
    parse,
    rule_action_pins,
    rule_no_expr_in_run,
    rule_permissions,
)

WF = ".github/workflows/x.yml"
SHA = "11d5960a326750d5838078e36cf38b85af677262"


def wf(text: str) -> Node:
    return parse(textwrap.dedent(text), WF)


def tree(text: str, **kw: object) -> Tree:
    return Tree(workflows={WF: wf(text)}, **kw)  # type: ignore[arg-type]


def rules(problems: list[Problem]) -> list[tuple[int, str]]:
    return [(p.line, p.rule) for p in problems]


class TestParser(unittest.TestCase):
    def test_block_mapping_sequence_and_scalars(self) -> None:
        root = wf(
            """\
            name: ci  # the name
            on:
              push:
                branches: ["**", 'a''b', main]
              pull_request:
            jobs:
              a:
                steps:
                  - uses: x/y@v1 # v1
                    with:
                      k: "q # not a comment"
                  - name: two
                    run: |
                      echo "${{ x }}"
                      echo b
                  -
                    run: >-
                      folded
                      text
            """
        )
        self.assertEqual(root.get("name").value, "ci")  # type: ignore[union-attr]
        self.assertEqual(root.get("name").comment, "the name")  # type: ignore[union-attr]
        on = root.get("on")
        assert on is not None
        self.assertEqual(on.get("push").get("branches").scalars(), ["**", "a'b", "main"])  # type: ignore[union-attr]
        self.assertEqual(on.get("pull_request").kind, "null")  # type: ignore[union-attr]
        steps = root.get("jobs").get("a").get("steps")  # type: ignore[union-attr]
        assert steps is not None
        self.assertEqual(len(steps.items), 3)
        self.assertEqual(steps.items[0].get("uses").comment, "v1")  # type: ignore[union-attr]
        self.assertEqual(
            steps.items[0].get("with").get("k").value, "q # not a comment"  # type: ignore[union-attr]
        )
        run = steps.items[1].get("run")
        assert run is not None
        self.assertEqual(run.value, 'echo "${{ x }}"\necho b')
        self.assertEqual((run.line, run.value_line), (13, 14))
        self.assertEqual(steps.items[2].get("run").value, "folded text")  # type: ignore[union-attr]

    def test_flow_mapping_and_sequence_at_key_indent(self) -> None:
        root = wf(
            """\
            permissions: {}
            env: {A: "1", B: two}
            list:
            - a
            - b
            """
        )
        self.assertEqual(root.get("permissions").kind, "map")  # type: ignore[union-attr]
        self.assertEqual(root.get("env").get("B").value, "two")  # type: ignore[union-attr]
        self.assertEqual(root.get("list").scalars(), ["a", "b"])  # type: ignore[union-attr]

    def test_unsupported_constructs_raise(self) -> None:
        for text in (
            "a: &anchor 1\n",
            "a: *alias\n",
            "a: !tag 1\n",
            "---\na: 1\n",
            "a: [1,\n  2]\n",
            "a: plain\n  continued\n",
            "<<: {a: 1}\n",
            "a: 1\na: 2\n",
            "a:\n  - &x b\n",
        ):
            with self.subTest(text=text), self.assertRaises(Unsupported):
                parse(text, WF)

    def test_unsupported_names_path_and_line(self) -> None:
        with self.assertRaises(Unsupported) as cm:
            parse("a: 1\nb: &anchor 2\n", WF)
        self.assertEqual((cm.exception.path, cm.exception.line), (WF, 2))


class TestNoExprInRun(unittest.TestCase):
    def test_inline_run_fails(self) -> None:
        t = tree(
            """\
            jobs:
              a:
                steps:
                  - run: echo "${{ github.ref_name }}"
            """
        )
        self.assertEqual(rules(rule_no_expr_in_run(t)), [(4, "no_expr_in_run")])

    def test_block_run_fails_on_its_line(self) -> None:
        t = tree(
            """\
            jobs:
              a:
                steps:
                  - run: |
                      echo ok
                      echo ${{ inputs.tag }}
            """
        )
        self.assertEqual(rules(rule_no_expr_in_run(t)), [(6, "no_expr_in_run")])

    def test_env_and_if_pass(self) -> None:
        t = tree(
            """\
            jobs:
              a:
                if: ${{ github.event_name == 'push' }}
                steps:
                  - env:
                      TAG: ${{ github.ref_name }}
                    run: echo "$TAG"
            """
        )
        self.assertEqual(rule_no_expr_in_run(t), [])


class TestPermissions(unittest.TestCase):
    def test_missing_top_level_fails(self) -> None:
        t = tree("jobs:\n  a:\n    runs-on: x\n")
        self.assertEqual(rules(rule_permissions(t)), [(1, "permissions")])

    def test_read_all_and_write_all_fail(self) -> None:
        for v in ("read-all", "write-all"):
            with self.subTest(v=v):
                t = tree(f"permissions: {v}\n")
                self.assertEqual(rules(rule_permissions(t)), [(1, "permissions")])

    def test_contents_read_and_none_pass(self) -> None:
        t = tree(
            """\
            permissions:
              contents: read
              issues: none
            jobs:
              a:
                permissions: {}
            """
        )
        self.assertEqual(rule_permissions(t), [])

    def test_grant_without_comment_fails_top_and_job(self) -> None:
        t = tree(
            """\
            permissions:
              contents: write
            jobs:
              a:
                permissions:
                  actions: read
              b:
                permissions: write-all
            """
        )
        self.assertEqual(
            rules(rule_permissions(t)), [(2, "permissions"), (6, "permissions"), (8, "permissions")]
        )

    def test_grant_with_comment_passes(self) -> None:
        t = tree(
            """\
            permissions:
              contents: read
            jobs:
              a:
                permissions:
                  contents: write # publishes the release
            """
        )
        self.assertEqual(rule_permissions(t), [])


class TestActionPins(unittest.TestCase):
    def run_uses(self, uses: str) -> list[tuple[int, str]]:
        return rules(rule_action_pins(tree(f"jobs:\n  a:\n    steps:\n      - uses: {uses}\n")))

    def test_sha_and_version_passes(self) -> None:
        self.assertEqual(self.run_uses(f"actions/checkout@{SHA} # v4.4.0"), [])

    def test_tag_fails(self) -> None:
        self.assertEqual(self.run_uses("actions/checkout@v4 # v4"), [(4, "action_pins")])

    def test_sha_without_version_comment_fails(self) -> None:
        self.assertEqual(self.run_uses(f"actions/checkout@{SHA}"), [(4, "action_pins")])

    def test_short_sha_fails(self) -> None:
        self.assertEqual(self.run_uses("actions/checkout@11d5960 # v4"), [(4, "action_pins")])

    def test_local_and_docker_digest_pass(self) -> None:
        self.assertEqual(self.run_uses("./.github/actions/x"), [])
        self.assertEqual(self.run_uses("docker://alpine@sha256:" + "a" * 64), [])

    def test_docker_tag_fails(self) -> None:
        self.assertEqual(self.run_uses("docker://alpine:3"), [(4, "action_pins")])

    def test_job_level_reusable_workflow(self) -> None:
        t = tree("jobs:\n  a:\n    uses: o/r/.github/workflows/w.yml@main\n")
        self.assertEqual(rules(rule_action_pins(t)), [(3, "action_pins")])


class TestTree(unittest.TestCase):
    def test_real_tree_passes(self) -> None:
        self.assertEqual([str(p) for p in check(load_tree(ROOT))], [])

    def test_loads_every_workflow(self) -> None:
        t = load_tree(ROOT)
        names = {Path(p).name for p in t.workflows}
        self.assertTrue({"ci.yml", "release.yml", "smp-stress.yml"} <= names)


if __name__ == "__main__":
    unittest.main()
