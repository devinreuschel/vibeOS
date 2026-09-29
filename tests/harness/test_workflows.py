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
    lane_map,
    ledger,
    load_tree,
    parse,
    rule_action_pins,
    rule_budget_doc,
    rule_ci_triggers,
    rule_concurrency_group,
    rule_gate_dispatch,
    rule_job_lane,
    rule_lane_capacity,
    rule_lane_map,
    rule_ledger_row,
    rule_no_expr_in_run,
    rule_permissions,
    rule_qemu_pin,
    rule_row_lane,
    rule_runs_on,
    rule_tiers,
    rule_upstream,
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


MAKEFILE = (
    "check:\n\tcargo fmt --check\n\t$(MAKE) test-unit\n\t$(MAKE) test-harness\n\n"
    "test-unit:\n\tcargo test\n"
    "test-a: x.iso\n\trun a\n"
    "test-b: x.iso\n\trun b\n"
    "test-c: x.iso\n\trun c\n"
    "test-smp-stress: x.iso\n\trun s\n"
    "test: test-unit test-harness test-a \\\n\ttest-b test-c\n"
)


def tier_ci(
    entries: str = (
        "          - {arch: x86_64, tier: one, targets: test-a test-b, jobs: 1}\n"
        "          - {arch: x86_64, tier: two, targets: test-c, jobs: 1}\n"
    ),
    needs: str = "[check, build]",
    fail_fast: str = "false",
    run: str = 'make -k -j "$JOBS" VIBEOS_PREBUILT=1 $TARGETS',
    jobs: str = "check",
) -> str:
    head = "jobs:\n"
    for j in jobs.split():
        head += f"  {j}:\n    runs-on: ubuntu-26.04\n"
    return (
        head
        + "  build:\n    runs-on: ubuntu-26.04\n"
        + f"  tier:\n    needs: {needs}\n    strategy:\n      fail-fast: {fail_fast}\n"
        + "      matrix:\n        include:\n"
        + entries
        + f"    steps:\n      - run: {run}\n"
    )


def tiers(text: str, makefile: str = MAKEFILE) -> list[str]:
    t = Tree(workflows={CI: parse(text, CI)}, makefile=makefile)
    return [p.message for p in rule_tiers(t)]


class TestTiers(unittest.TestCase):
    def test_built_shape_passes(self) -> None:
        self.assertEqual(tiers(tier_ci()), [])

    def test_missing_jobs_fail(self) -> None:
        self.assertEqual(tiers(tier_ci(jobs="")), ["no `check` job"])
        text = tier_ci().replace("  build:\n    runs-on: ubuntu-26.04\n", "")
        self.assertEqual(tiers(text), ["no `build` job"])
        self.assertEqual(tiers("jobs:\n  check:\n    x: 1\n"), ["no `build` job", "no `tier` job"])

    def test_needs_fail_fast_and_prebuilt_switch(self) -> None:
        self.assertEqual(len(tiers(tier_ci(needs="[build]"))), 1)
        self.assertEqual(len(tiers(tier_ci(needs="check"))), 1)
        self.assertEqual(len(tiers(tier_ci(fail_fast="true"))), 1)
        self.assertEqual(len(tiers(tier_ci(run="make $TARGETS"))), 1)

    def test_target_in_no_tier_fails(self) -> None:
        entries = "          - {arch: x86_64, tier: one, targets: test-a test-b, jobs: 1}\n"
        self.assertEqual(tiers(tier_ci(entries)), ["x86_64: `make test` runs test-c, no tier does"])

    def test_target_in_two_tiers_fails(self) -> None:
        entries = (
            "          - {arch: x86_64, tier: one, targets: test-a test-b, jobs: 1}\n"
            "          - {arch: x86_64, tier: two, targets: test-c test-a, jobs: 1}\n"
        )
        self.assertEqual(tiers(tier_ci(entries)), ["test-a in tiers one and two"])

    def test_same_target_on_two_arches_passes(self) -> None:
        entries = (
            "          - {arch: x86_64, tier: one, targets: test-a test-b test-c, jobs: 1}\n"
            "          - {arch: aarch64, tier: one, targets: test-a test-b test-c, jobs: 1}\n"
        )
        self.assertEqual(tiers(tier_ci(entries)), [])

    def test_check_submakes_and_unknown_targets(self) -> None:
        entries = (
            "          - {arch: x86_64, tier: one, targets: test-a test-b test-c, jobs: 1}\n"
            "          - {arch: x86_64, tier: two, targets: test-unit test-nope test-smp-stress,"
            " jobs: 1}\n"
        )
        self.assertEqual(
            tiers(tier_ci(entries)),
            [
                "tier two: target test-unit is not a `make test` tier",
                "tier two: target test-nope unknown",
                "tier two: target test-smp-stress is not a `make test` tier",
            ],
        )

    def test_entry_without_a_key_fails(self) -> None:
        entries = (
            "          - {arch: x86_64, tier: one, targets: test-a test-b test-c}\n"
        )
        self.assertEqual(tiers(tier_ci(entries)), ["tier entry without `jobs`"])


TABLE = (
    "## 8.6 CI\n\ntext\n\n| Arch | Tier | Targets | QEMU s |\n|---|---|---|---|\n"
    "{rows}\nafter\n\n## 8.7 Next\n"
)


def budget(rows: str, text: str | None = None) -> list[tuple[int, str]]:
    t = Tree(
        workflows={CI: parse(text or tier_ci(), CI)},
        testing_md=TABLE.format(rows=rows),
    )
    return [(p.line, p.message) for p in rule_budget_doc(t)]


class TestBudgetDoc(unittest.TestCase):
    ROWS = (
        "| x86_64 | one | `test-a`, `test-b` | 30 |\n"
        "| x86_64 | two | `test-c` | 40 |"
    )

    def test_matching_table_passes(self) -> None:
        self.assertEqual(budget(self.ROWS), [])

    def test_differing_targets_fail(self) -> None:
        rows = self.ROWS.replace("`test-a`, `test-b`", "`test-a`")
        want = "tier (x86_64, one) targets ['test-a'], ci.yml ['test-a', 'test-b']"
        self.assertEqual(budget(rows), [(7, want)])

    def test_missing_and_extra_rows_fail(self) -> None:
        rows = "| x86_64 | one | `test-a`, `test-b` | 30 |\n| x86_64 | three | `test-c` | 40 |"
        self.assertEqual(
            budget(rows),
            [(5, "no row for tier (x86_64, two)"), (8, "row (x86_64, three) is no ci.yml tier")],
        )

    def test_malformed_row_fails(self) -> None:
        rows = self.ROWS.replace("| 40 |", "| n/a |")
        self.assertEqual([m for _, m in budget(rows)][0][:18], "tier row is not `|")

    def test_missing_table_fails(self) -> None:
        t = Tree(workflows={CI: parse(tier_ci(), CI)}, testing_md="## 8.6 CI\n\nno table\n")
        self.assertEqual(rules(rule_budget_doc(t)), [(1, "budget_doc")])


ENTRY = (
    "### QEMU: a title\n\n**Reproducer.** run it\n\n**Versions.** 10.2.1\n\n"
    "**Workaround.** `{path}` `qemu_argv`: an extra flag\n\n"
    "**Upstream.** draft; the maintainer files it\n"
)
LINKED = "## 8.6 CI\n\nSee [UPSTREAM.md](UPSTREAM.md).\n"


def upstream(md: str | None, testing: str = LINKED) -> list[tuple[int, str]]:
    t = Tree(workflows={}, testing_md=testing, upstream_md=md, root=ROOT)
    return [(p.line, p.message) for p in rule_upstream(t)]


class TestUpstream(unittest.TestCase):
    PRE = "# Upstream reports\n\nPreamble.\n\n```\n### not: an entry\n```\n\n"

    def test_empty_file_and_complete_entry_pass(self) -> None:
        self.assertEqual(upstream(self.PRE), [])
        self.assertEqual(upstream(self.PRE + ENTRY.format(path="tests/harness/harness.py")), [])

    def test_symbol_after_path_passes(self) -> None:
        md = self.PRE + ENTRY.format(path="tests/harness/harness.py::qemu_argv")
        self.assertEqual(upstream(md), [])

    def test_missing_link_fails(self) -> None:
        self.assertEqual(
            upstream(self.PRE, testing="## 8.6 CI\n\nno link\n"),
            [(1, "§8.6 does not link UPSTREAM.md")],
        )

    def test_missing_file_fails(self) -> None:
        self.assertEqual(upstream(None), [(1, "docs/UPSTREAM.md is missing")])

    def test_missing_fields_fail(self) -> None:
        full = ENTRY.format(path="tests/harness/harness.py")
        for field in ("Reproducer", "Versions", "Upstream"):
            with self.subTest(field=field):
                md = self.PRE + full.replace(f"**{field}.**", "")
                self.assertEqual(upstream(md), [(9, f"entry has no **{field}.** field")])
        md = self.PRE + full.replace("**Workaround.**", "")
        self.assertEqual(upstream(md), [(9, "entry has no **Workaround.** field")])

    def test_workaround_path_must_exist(self) -> None:
        md = self.PRE + ENTRY.format(path="tests/harness/nope.py")
        self.assertEqual(
            upstream(md), [(9, "Workaround names no in-tree path first (tests/harness/nope.py)")]
        )

    def test_bad_heading_fails(self) -> None:
        md = self.PRE + ENTRY.format(path="Makefile").replace("### QEMU: a title", "### a title")
        self.assertEqual(upstream(md), [(9, "entry heading is not `### <project>: <title>`")])


SCHED = ".github/workflows/sched.yml"
LANE_DOC = """\
## 8.6 CI

| Lane | Reserved for | Jobs |
|---|---|---|
| `sched-lane-0` | nightly | `sched.yml` `a` |
| `sched-lane-1` | nightly | `sched.yml` `b` |
| `sched-lane-7` | none | multi-day chains |

Release windows: none

| Workflow | Cadence | Jobs per run | Job-hours per run | Peak concurrent jobs | Lanes |
|---|---|---|---|---|---|
| `sched.yml` | daily and dispatch | 2 | 1 (estimated) | 2 | `sched-lane-0`, `sched-lane-1` |

## 8.7 next
"""


def sched_wf(jobs: str, on: str = "schedule:\n    - cron: \"1 2 * * *\"") -> str:
    return f"on:\n  {on}\npermissions:\n  contents: read\njobs:\n{jobs}"


def lane_job(name: str, lane: str, extra: str = "", timeout: str = "30") -> str:
    return (
        f"  {name}:\n    runs-on: ubuntu-26.04\n    timeout-minutes: {timeout}\n"
        f"    concurrency:\n      group: {lane}\n      queue: max\n{extra}"
        "    steps:\n      - run: make x\n"
    )


GOOD_JOBS = lane_job("a", "sched-lane-0") + lane_job("b", "sched-lane-1")


def sched_tree(jobs: str = GOOD_JOBS, doc: str = LANE_DOC, **kw: str) -> Tree:
    return Tree(workflows={SCHED: parse(sched_wf(jobs, **kw), SCHED)}, testing_md=doc)


def lane_rules(t: Tree) -> list[tuple[str, str]]:
    out = []
    for rule in (rule_ledger_row, rule_lane_capacity, rule_job_lane, rule_row_lane, rule_lane_map):
        out += [(p.rule, p.message) for p in rule(t)]
    return out


class TestLaneTables(unittest.TestCase):
    def test_parses_both_tables(self) -> None:
        lanes = lane_map(LANE_DOC)
        self.assertEqual(sorted(lanes), ["sched-lane-0", "sched-lane-1", "sched-lane-7"])
        self.assertEqual(lanes["sched-lane-0"].reserved, "nightly")
        self.assertEqual(lanes["sched-lane-0"].workflows, frozenset({"sched.yml"}))
        self.assertEqual(lanes["sched-lane-7"].workflows, frozenset())
        rows = ledger(LANE_DOC)
        self.assertEqual(list(rows), ["sched.yml"])
        self.assertEqual(rows["sched.yml"].jobs_per_run, 2)
        self.assertEqual(rows["sched.yml"].lanes, ("sched-lane-0", "sched-lane-1"))

    def test_other_section_and_header_ignored(self) -> None:
        self.assertEqual(ledger(LANE_DOC.replace("## 8.6", "## 8.5")), {})
        self.assertEqual(lane_map(LANE_DOC.replace("| Reserved for |", "| Reserved |")), {})


class TestLedgerRow(unittest.TestCase):
    def test_passing_workflow(self) -> None:
        self.assertEqual(lane_rules(sched_tree()), [])

    def test_missing_row_fails(self) -> None:
        doc = LANE_DOC.replace("| `sched.yml` | daily", "| `other.yml` | daily")
        got = [p.message for p in rule_ledger_row(sched_tree(doc=doc))]
        self.assertEqual(got, ["scheduled workflow sched.yml has no §8.6 ledger row"])

    def test_dispatch_only_counts_and_push_only_does_not(self) -> None:
        doc = LANE_DOC.replace("`sched.yml` | daily", "`x.yml` | daily")
        t = sched_tree(doc=doc, on="workflow_dispatch:")
        self.assertEqual([p.rule for p in rule_ledger_row(t)], ["ledger_row"])
        t = sched_tree("  a:\n    runs-on: ubuntu-26.04\n", doc=doc, on="push:")
        self.assertEqual(rule_ledger_row(t) + rule_job_lane(t), [])

    def test_ci_and_release_exempt(self) -> None:
        wf = parse(sched_wf("  a:\n    runs-on: ubuntu-26.04\n"), CI)
        rel = parse(sched_wf("  a:\n    runs-on: ubuntu-26.04\n"), ".github/workflows/release.yml")
        t = Tree(workflows={CI: wf, ".github/workflows/release.yml": rel}, testing_md="")
        self.assertEqual(lane_rules(t), [])


class TestLaneCapacity(unittest.TestCase):
    def matrix(self, body: str) -> str:
        return lane_job("a", "sched-lane-0", extra=f"    strategy:\n      matrix:\n{body}")

    def test_under_capacity_passes(self) -> None:
        axes = "        x: [" + ", ".join(str(i) for i in range(10)) + "]\n"
        axes += "        y: [" + ", ".join(str(i) for i in range(10)) + "]\n"
        jobs = self.matrix(axes) + lane_job("b", "sched-lane-1")
        self.assertEqual(rule_lane_capacity(sched_tree(jobs)), [])

    def test_over_capacity_fails(self) -> None:
        axes = "        x: [" + ", ".join(str(i) for i in range(10)) + "]\n"
        axes += "        y: [" + ", ".join(str(i) for i in range(10)) + "]\n"
        axes += "        include:\n          - x: 99\n"
        got = [p.message for p in rule_lane_capacity(sched_tree(self.matrix(axes)))]
        self.assertEqual(got, ["101 jobs of one run in sched-lane-0; a lane holds 100"])

    def test_jobs_in_one_lane_add_up(self) -> None:
        axes = "        x: [" + ", ".join(str(i) for i in range(100)) + "]\n"
        jobs = self.matrix(axes) + lane_job("b", "sched-lane-0")
        got = [p.message for p in rule_lane_capacity(sched_tree(jobs))]
        self.assertEqual(got, ["101 jobs of one run in sched-lane-0; a lane holds 100"])

    def test_expression_matrix_fails_as_uncountable(self) -> None:
        bodies = ("        x: ${{ fromJSON(inputs.x) }}\n", "        x: [a, \"${{ inputs.y }}\"]\n")
        for body in bodies:
            with self.subTest(body=body):
                got = [p.message for p in rule_lane_capacity(sched_tree(self.matrix(body)))]
                self.assertEqual(
                    got, ["job `a`: matrix uses an expression; its job count is uncountable"]
                )

    def test_row_over_lane_capacity_fails(self) -> None:
        doc = LANE_DOC.replace("| 2 | 1 (estimated)", "| 201 | 1 (estimated)")
        got = [p.message for p in rule_lane_capacity(sched_tree(doc=doc))]
        self.assertEqual(got, ["ledger row sched.yml: 201 jobs per run in 2 lane(s) of 100"])


class TestJobLane(unittest.TestCase):
    def test_missing_group_fails(self) -> None:
        jobs = "  a:\n    runs-on: ubuntu-26.04\n    steps:\n      - run: x\n"
        got = [p.message for p in rule_job_lane(sched_tree(jobs + lane_job("b", "sched-lane-1")))]
        self.assertEqual(
            got, ["job `a` declares no `concurrency: {group: sched-lane-<n>, queue: max}`"]
        )

    def test_missing_queue_fails(self) -> None:
        jobs = GOOD_JOBS.replace("      queue: max\n", "", 1)
        self.assertEqual([p.rule for p in rule_job_lane(sched_tree(jobs))], ["job_lane"])

    def test_expression_group_fails(self) -> None:
        jobs = GOOD_JOBS.replace("group: sched-lane-0", "group: sched-lane-${{ inputs.n }}")
        self.assertEqual([p.rule for p in rule_job_lane(sched_tree(jobs))], ["job_lane"])

    def test_non_lane_group_fails(self) -> None:
        jobs = GOOD_JOBS.replace("group: sched-lane-0", "group: sched-lane-10")
        self.assertEqual([p.rule for p in rule_job_lane(sched_tree(jobs))], ["job_lane"])

    def test_cancel_in_progress_fails(self) -> None:
        jobs = GOOD_JOBS.replace("queue: max\n", "queue: max\n      cancel-in-progress: true\n", 1)
        got = [p.message for p in rule_job_lane(sched_tree(jobs))]
        self.assertEqual(got, ["job `a`: `cancel-in-progress` with `queue: max`"])

    def test_reserved_lane_needs_short_timeout(self) -> None:
        jobs = lane_job("a", "sched-lane-0", timeout="331") + lane_job("b", "sched-lane-1")
        got = [p.message for p in rule_job_lane(sched_tree(jobs))]
        self.assertEqual(got, ["job `a` in reserved sched-lane-0 needs `timeout-minutes` <= 330"])
        jobs = lane_job("a", "sched-lane-0", timeout="330") + lane_job("b", "sched-lane-1")
        self.assertEqual(rule_job_lane(sched_tree(jobs)), [])

    def test_unreserved_lane_takes_a_long_job(self) -> None:
        doc = LANE_DOC.replace(
            "`sched-lane-0`, `sched-lane-1` |", "`sched-lane-1`, `sched-lane-7` |"
        )
        doc = doc.replace("| multi-day chains |", "| multi-day chains; `sched.yml` `a` |")
        jobs = lane_job("a", "sched-lane-7", timeout="3000") + lane_job("b", "sched-lane-1")
        self.assertEqual(lane_rules(sched_tree(jobs, doc=doc)), [])

    def test_self_hosted_job_is_not_laned(self) -> None:
        jobs = GOOD_JOBS + "  c:\n    runs-on: [self-hosted, linux]\n    steps:\n      - run: x\n"
        self.assertEqual(rule_job_lane(sched_tree(jobs)), [])


class TestRowLane(unittest.TestCase):
    def test_lane_outside_row_fails(self) -> None:
        jobs = lane_job("a", "sched-lane-0") + lane_job("b", "sched-lane-7")
        got = [p.message for p in rule_row_lane(sched_tree(jobs))]
        self.assertEqual(
            got,
            ["job `b` runs in sched-lane-7, not in its ledger row's lanes "
             "['sched-lane-0', 'sched-lane-1']"],
        )

    def test_lane_in_row_passes(self) -> None:
        self.assertEqual(rule_row_lane(sched_tree()), [])


class TestLaneMap(unittest.TestCase):
    def test_row_lane_the_map_does_not_give_fails(self) -> None:
        doc = LANE_DOC.replace("| nightly | `sched.yml` `b` |", "| nightly | none |")
        got = [p.message for p in rule_lane_map(sched_tree(doc=doc))]
        want = "ledger row sched.yml names sched-lane-1, which the lane map does not give it"
        self.assertEqual(got, [want])

    def test_row_lane_missing_from_map_fails(self) -> None:
        doc = LANE_DOC.replace("| `sched-lane-1` | nightly | `sched.yml` `b` |\n", "")
        self.assertEqual([p.rule for p in rule_lane_map(sched_tree(doc=doc))], ["lane_map"])

    def test_map_that_lists_the_workflow_passes(self) -> None:
        self.assertEqual(rule_lane_map(sched_tree()), [])


class TestTree(unittest.TestCase):
    def test_real_tree_passes(self) -> None:
        self.assertEqual([str(p) for p in check(load_tree(ROOT))], [])

    def test_loads_every_workflow(self) -> None:
        t = load_tree(ROOT)
        names = {Path(p).name for p in t.workflows}
        self.assertTrue({"ci.yml", "release.yml", "smp-stress.yml"} <= names)


if __name__ == "__main__":
    unittest.main()
