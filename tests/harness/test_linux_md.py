"""Host tests for scripts/check_linux_md.py (the docs/LINUX.md register)."""

from __future__ import annotations

import unittest

from scripts.check_linux_md import LINUX_MD, SYSCALL_MD, check_linux, check_syscall, split_row

LINUX = """# Linux contract

## Baseline

| Field | Value |
|-------|-------|
| Series | not named yet |

## Deliberate differences

| Id | Interface | Linux | vibeOS | Reason | Decided in |
|----|-----------|-------|--------|--------|------------|
| `no-modules` | modules | loads code | `ENOSYS` | one image | ROADMAP Non-goals |
| `no-vsyscall` | vsyscall | mapped | not mapped | old glibc only | here |

## Native interfaces

| Id | Interface | Format | Reason | Until |
|----|-----------|--------|--------|-------|
| `psinfo` | syscall 500 | lines | `ps` | ROADMAP §13.9 |
"""

SYSCALL = """# Syscall ABI

## 2. Return and errno

| Name | Value | Used |
|------|------:|------|
| `EINVAL` | 22 | the non-Linux cases in §2.1 |
| `EPERM` | 1 | defined |

### 2.1 Differences from Linux

- `lseek` on the console returns `EINVAL` (F083;
  ROADMAP §10.4)
- `init_module` returns `ENOSYS` (`no-modules`)

### 3.1 Behavior and differences from Linux

- `read`: a short read (ROADMAP §12.5)
"""


class TestTree(unittest.TestCase):
    def test_tree_passes(self) -> None:
        errors, ids = check_linux(LINUX_MD.read_text(encoding="utf-8"))
        self.assertEqual(errors, [])
        self.assertIn("psinfo", ids)
        self.assertEqual(check_syscall(SYSCALL_MD.read_text(encoding="utf-8"), ids), [])


class TestLinux(unittest.TestCase):
    def test_sample_passes(self) -> None:
        errors, ids = check_linux(LINUX)
        self.assertEqual(errors, [])
        self.assertEqual(ids, {"no-modules", "no-vsyscall", "psinfo"})

    def test_pipe_in_backticks_is_text(self) -> None:
        self.assertEqual(split_row("| `a|b` | c |"), ["`a|b`", "c"])

    def test_missing_section(self) -> None:
        errors, _ = check_linux(LINUX.replace("## Native interfaces", "## Native"))
        self.assertEqual(errors, ["docs/LINUX.md:1: missing section '## Native interfaces'"])

    def test_empty_cell(self) -> None:
        text = LINUX.replace("| one image |", "|  |")
        errors, _ = check_linux(text)
        self.assertEqual(len(errors), 1)
        self.assertIn("empty cell in column 'Reason'", errors[0])

    def test_dash_is_a_value(self) -> None:
        errors, _ = check_linux(LINUX.replace("| one image |", "| — |"))
        self.assertEqual(errors, [])

    def test_short_row(self) -> None:
        errors, _ = check_linux(LINUX.replace(" | old glibc only | here |", " | here |"))
        self.assertEqual(len(errors), 1)
        self.assertIn("row has 5 cells, its header 6", errors[0])

    def test_repeated_id(self) -> None:
        errors, _ = check_linux(LINUX.replace("`no-vsyscall`", "`no-modules`"))
        self.assertEqual(len(errors), 1)
        self.assertIn("id `no-modules` repeats line", errors[0])

    def test_malformed_id(self) -> None:
        errors, _ = check_linux(LINUX.replace("`no-vsyscall`", "No_Vsyscall"))
        self.assertEqual(len(errors), 1)
        self.assertIn("is not one backticked lowercase name", errors[0])

    def test_decided_in_names_no_section(self) -> None:
        errors, _ = check_linux(LINUX.replace("| here |", "| the review |"))
        self.assertEqual(len(errors), 1)
        self.assertIn("Decided in names no document section", errors[0])

    def test_native_interface_under_a_vibeos_name_passes(self) -> None:
        row = "| `faults` | `/proc/<pid>/vibeos/faults` | counts | fault kinds | — |\n"
        errors, _ = check_linux(LINUX + row)
        self.assertEqual(errors, [])

    def test_native_interface_outside_the_vibeos_names_fails(self) -> None:
        row = "| `syscalls` | `/proc/<pid>/syscalls` | a count | tracing | — |\n"
        errors, _ = check_linux(LINUX + row)
        self.assertEqual(
            errors,
            ["docs/LINUX.md:21: native interface outside the vibeOS names (SYSCALL.md §8)"],
        )


class TestSyscall(unittest.TestCase):
    ids = {"no-modules", "no-vsyscall", "psinfo"}

    def test_sample_passes(self) -> None:
        self.assertEqual(check_syscall(SYSCALL, self.ids), [])

    def test_uncited_difference_fails(self) -> None:
        text = SYSCALL.replace("(`no-modules`)", "(unlisted)")
        errors = check_syscall(text, self.ids)
        self.assertEqual(len(errors), 1)
        self.assertTrue(errors[0].startswith("docs/SYSCALL.md:14: difference from Linux cites no"))

    def test_cite_split_across_lines_counts(self) -> None:
        text = SYSCALL.replace("(F083;\n  ROADMAP §10.4)", "(returns\n  ROADMAP §10.4)")
        self.assertEqual(check_syscall(text, self.ids), [])

    def test_errno_row_naming_linux_needs_a_cite(self) -> None:
        text = SYSCALL.replace("| defined |", "| defined; Linux returns it |")
        errors = check_syscall(text, self.ids)
        self.assertEqual(len(errors), 1)
        self.assertIn("errno row `EPERM` names Linux", errors[0])

    def test_bullet_outside_differences_is_not_checked(self) -> None:
        text = SYSCALL + "\n## 4. File descriptors\n\n- `dup` copies the slot\n"
        self.assertEqual(check_syscall(text, self.ids), [])


if __name__ == "__main__":
    unittest.main()
