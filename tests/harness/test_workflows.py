"""Host tests for scripts/check_workflows.py (ROADMAP §10.1 workflow rules)."""

from __future__ import annotations

import textwrap
import unittest
from pathlib import Path

from scripts.check_workflows import (
    CI,
    ROOT,
    TEMPORARY,
    Node,
    Problem,
    Tree,
    Unsupported,
    check,
    load_tree,
    parse,
    rule_action_pins,
    rule_ci_triggers,
    rule_concurrency_group,
    rule_gate_dispatch,
    rule_no_expr_in_run,
    rule_permissions,
    rule_qemu_pin,
    rule_runs_on,
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


class TestRunsOn(unittest.TestCase):
    def run_label(self, label: str) -> list[tuple[int, str]]:
        return rules(rule_runs_on(tree(f"jobs:\n  a:\n    runs-on: {label}\n")))

    def test_pinned_images_and_macos_pass(self) -> None:
        for label in ("ubuntu-26.04", "ubuntu-26.04-arm", "macos-15", "[ubuntu-26.04]"):
            with self.subTest(label=label):
                self.assertEqual(self.run_label(label), [])

    def test_other_labels_fail(self) -> None:
        for label in ("ubuntu-latest", "ubuntu-24.04", "ubuntu-22.04-arm", "windows-2025"):
            with self.subTest(label=label):
                self.assertEqual(self.run_label(label), [(3, "runs_on")])

    def test_matrix_label_resolves_to_its_values(self) -> None:
        text = """\
            jobs:
              a:
                strategy:
                  matrix:
                    os: [ubuntu-26.04]
                    include:
                      - os: {os}
                runs-on: ${{{{ matrix.os }}}}
            """
        self.assertEqual(rules(rule_runs_on(tree(text.format(os="ubuntu-26.04-arm")))), [])
        self.assertEqual(
            rules(rule_runs_on(tree(text.format(os="ubuntu-latest")))), [(8, "runs_on")]
        )

    def test_matrix_label_without_values_fails(self) -> None:
        t = tree("jobs:\n  a:\n    runs-on: ${{ matrix.os }}\n")
        self.assertEqual(rules(rule_runs_on(t)), [(3, "runs_on")])

    def test_reusable_workflow_job_has_no_runner(self) -> None:
        self.assertEqual(rule_runs_on(tree("jobs:\n  a:\n    uses: ./x.yml\n")), [])


class TestQemuPin(unittest.TestCase):
    JOB = (
        "{top}jobs:\n"
        "  a:\n"
        "{env}"
        "    steps:\n"
        "      - run: sudo apt-get install -y qemu-system-x86\n"
        "  b:\n"
        "    steps:\n"
        "      - run: make check\n"
    )

    def run_pin(self, top: str = "", env: str = "") -> list[tuple[int, str]]:
        return rules(rule_qemu_pin(tree(self.JOB.format(top=top, env=env))))

    def test_job_pin_passes(self) -> None:
        self.assertEqual(self.run_pin(env='    env:\n      VIBEOS_QEMU_VERSION: "10.2.1"\n'), [])

    def test_workflow_pin_passes(self) -> None:
        self.assertEqual(self.run_pin(top='env:\n  VIBEOS_QEMU_VERSION: "10.2.1"\n'), [])

    def test_missing_pin_fails_only_the_qemu_job(self) -> None:
        self.assertEqual(self.run_pin(), [(2, "qemu_pin")])

    def test_malformed_pin_fails(self) -> None:
        for v in ('"10.2"', '"latest"', '""'):
            with self.subTest(v=v):
                env = f"    env:\n      VIBEOS_QEMU_VERSION: {v}\n"
                self.assertEqual(self.run_pin(env=env), [(2, "qemu_pin")])

    def test_qemu_in_env_counts(self) -> None:
        t = tree("jobs:\n  a:\n    env:\n      APT_PACKAGES: qemu-system-x86 ovmf\n")
        self.assertEqual(rules(rule_qemu_pin(t)), [(2, "qemu_pin")])


BRANCHES = ", ".join(f'"{b}"' for b in ("main", *TEMPORARY))
GROUP = (
    "${{ github.event_name == 'pull_request' && format('ci-pr-{0}', "
    "github.event.pull_request.number) || format('ci-run-{0}', github.run_id) }}"
)


def ci(
    push: str = f"  push:\n    branches: [{BRANCHES}]\n",
    pr: str = "  pull_request:\n",
    dispatch: str = "  workflow_dispatch:\n",
    group: str = GROUP,
) -> Tree:
    text = f"on:\n{push}{pr}{dispatch}concurrency:\n  group: {group}\n"
    return Tree(workflows={CI: parse(text, CI)})


class TestCiTriggers(unittest.TestCase):
    def test_built_shape_passes(self) -> None:
        self.assertEqual(rule_ci_triggers(ci()), [])

    def test_other_push_branches_fail(self) -> None:
        for branches in ('["**"]', "[main]", f"[{BRANCHES}, dev]"):
            with self.subTest(branches=branches):
                t = ci(push=f"  push:\n    branches: {branches}\n")
                self.assertEqual(rules(rule_ci_triggers(t)), [(2, "ci_triggers")])

    def test_push_without_branches_fails(self) -> None:
        self.assertEqual(rules(rule_ci_triggers(ci(push="  push:\n"))), [(2, "ci_triggers")])

    def test_filters_fail(self) -> None:
        for key in ("tags", "branches-ignore", "paths", "paths-ignore", "tags-ignore"):
            with self.subTest(key=key):
                push = f"  push:\n    branches: [{BRANCHES}]\n    {key}: [x]\n"
                self.assertEqual(rules(rule_ci_triggers(ci(push=push))), [(4, "ci_triggers")])
                pr = f"  pull_request:\n    {key}: [x]\n"
                self.assertEqual(rules(rule_ci_triggers(ci(pr=pr))), [(5, "ci_triggers")])

    def test_missing_pull_request_or_dispatch_fails(self) -> None:
        self.assertEqual(rules(rule_ci_triggers(ci(pr=""))), [(1, "ci_triggers")])
        self.assertEqual(rules(rule_ci_triggers(ci(dispatch=""))), [(1, "ci_triggers")])

    def test_group_without_pr_number_or_run_id_fails(self) -> None:
        for group in (
            "ci-${{ github.ref }}",
            "${{ format('ci-pr-{0}', github.event.pull_request.number) }}",
            "${{ format('ci-run-{0}', github.run_id) }}",
        ):
            with self.subTest(group=group):
                self.assertEqual(rules(rule_ci_triggers(ci(group=group))), [(7, "ci_triggers")])

    def test_missing_ci_yml_fails(self) -> None:
        self.assertEqual(rules(rule_ci_triggers(tree("on: push\n"))), [(1, "ci_triggers")])


class TestConcurrencyGroup(unittest.TestCase):
    def test_head_ref_and_ref_name_fail(self) -> None:
        for ref in ("github.head_ref", "github.ref_name"):
            with self.subTest(ref=ref):
                t = tree(f"concurrency:\n  group: ci-${{{{ {ref} }}}}\n")
                self.assertEqual(rules(rule_concurrency_group(t)), [(2, "concurrency_group")])

    def test_job_level_and_scalar_form_fail(self) -> None:
        t = tree("jobs:\n  a:\n    concurrency: lane-${{ github.ref_name }}\n")
        self.assertEqual(rules(rule_concurrency_group(t)), [(3, "concurrency_group")])

    def test_run_id_group_passes(self) -> None:
        t = tree(
            "concurrency:\n  group: ci-${{ github.run_id }}\njobs:\n  a:\n    concurrency: x\n"
        )
        self.assertEqual(rule_concurrency_group(t), [])


class TestGateDispatch(unittest.TestCase):
    NIGHTLY = ".github/workflows/nightly.yml"

    def gate(self, workflow: str, on: str) -> list[tuple[int, str]]:
        t = Tree(
            workflows={self.NIGHTLY: parse(f"name: nightly-run\non:\n{on}", self.NIGHTLY)},
            gate_workflows=[("tests/gates/phase-10.toml", workflow)],
        )
        return rules(rule_gate_dispatch(t))

    def test_named_workflow_without_dispatch_fails(self) -> None:
        on = "  schedule:\n    - cron: '0 3 * * *'\n"
        for name in ("nightly.yml", "nightly", "nightly-run"):
            with self.subTest(name=name):
                self.assertEqual(self.gate(name, on), [(1, "gate_dispatch")])

    def test_named_workflow_with_dispatch_passes(self) -> None:
        self.assertEqual(self.gate("nightly.yml", "  workflow_dispatch:\n"), [])
        self.assertEqual(self.gate("nightly-run", "  workflow_dispatch:\n"), [])

    def test_unknown_workflow_fails(self) -> None:
        self.assertEqual(self.gate("gone.yml", "  workflow_dispatch:\n"), [(1, "gate_dispatch")])

    def test_gate_map_is_read(self) -> None:
        import tempfile

        from scripts.check_workflows import _gate_workflows

        with tempfile.TemporaryDirectory() as d:
            gates = Path(d, "tests/gates")
            gates.mkdir(parents=True)
            gates.joinpath("phase-10.toml").write_text(
                '[[line]]\nkey = "k"\n[[line.entry]]\njob = {workflow = "ci.yml", job = "tier"}\n'
                '[[line.entry]]\ncmd = "make check"\n'
            )
            gates.joinpath("phase-10-needs.toml").write_text('[[box]]\nkey = "k"\nneeds = []\n')
            self.assertEqual(_gate_workflows(Path(d)), [("tests/gates/phase-10.toml", "ci.yml")])


class TestTree(unittest.TestCase):
    def test_real_tree_passes(self) -> None:
        self.assertEqual([str(p) for p in check(load_tree(ROOT))], [])

    def test_loads_every_workflow(self) -> None:
        t = load_tree(ROOT)
        names = {Path(p).name for p in t.workflows}
        self.assertTrue({"ci.yml", "release.yml", "smp-stress.yml"} <= names)


if __name__ == "__main__":
    unittest.main()
