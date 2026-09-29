#!/usr/bin/env python3
"""Removed identifiers and paths stay removed (ROADMAP §10.9, C-GONE).

`GONE` lists what boxes removed, each row naming its box by a key of the
needs-file form (a substring of exactly one box line of docs/ROADMAP.md). The
check fails when a listed name appears in `src/`, `crates/`, `user/`,
`tests/`, `scripts/`, `.github/`, the `Makefile`, `build.rs`, or `setup.sh`,
outside this file and `tests/harness/test_gone.py`. `docs/` and
`CHANGELOG.md` are dated prose and are not read, and neither are the `key`
strings of a gate map (`tests/gates/phase-<N>.toml`), which quote exit-gate
lines of docs/ROADMAP.md in full (C-GATEMAP); its commands are read.

A row is one of:
- an identifier, which fails as a whole word;
- a path or glob (it holds `/` or a glob character), which fails when a file
  matches it, and a plain path also when its text appears in a file;
- a definition, `<path>: <kind> <name>` (`src/fs/fat_init.rs: fn route`),
  which fails only when that file still defines `<name>` as `<kind>`, so the
  bare word stays legal elsewhere.
A file whose basename equals a row fails too.

A deleting commit adds its rows here and names this script in its `Proves:`
line (ROADMAP, How to read this).
"""

from __future__ import annotations

import fnmatch
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts import gatelib  # noqa: E402

# (identifier, path, glob, or definition row; box key). One row per line,
# sorted. A key stops before any gone name, since tests/gates/ and this
# file's own keys would otherwise have to name it.
GONE: list[tuple[str, str]] = [
    ("EXIT_STATUS", "one ring-3 entry model:"),
    ("IN_USER", "one ring-3 entry model:"),
    ("STDOUT_LEN", "one ring-3 entry model:"),
    ("USER_JMP", "one ring-3 entry model:"),
    ("bind_current", "one ring-3 entry model:"),
    ("bind_probe", "one ring-3 entry model:"),
    ("boot_hello", "one ring-3 entry model:"),
    ("capture_stdout", "one ring-3 entry model:"),
    ("flags_if_on", "one ring-3 entry model:"),
    ("longjmp_user", "one ring-3 entry model:"),
    ("put_size", "one refcounted in-core FAT inode per file"),
    ("reset_stdout", "one ring-3 entry model:"),
    ("return_status_or_die", "one ring-3 entry model:"),
    ("run_path", "one ring-3 entry model:"),
    ("run_user", "one ring-3 entry model:"),
    ("set_exit_status", "one ring-3 entry model:"),
    ("src/proc/syscall_init.rs: static STDOUT", "one ring-3 entry model:"),
    ("stdout_bytes", "one ring-3 entry model:"),
    ("unbind_current", "one ring-3 entry model:"),
    ("unbind_probe", "one ring-3 entry model:"),
    ("vibeos_user_longjmp", "one ring-3 entry model:"),
    ("vibeos_user_setjmp", "one ring-3 entry model:"),
    ("with_user_as", "one ring-3 entry model:"),
    ("IrqsOffOnDrop", "the in-guest registry runs in production's interrupt context"),
    ("TRAMPOLINE_PHYS", "the AP trampoline page chosen from"),
    ("AP_TRAMPOLINE_PHYS", "the AP trampoline page chosen from"),
    ("SIPI_VECTOR", "the AP trampoline page chosen from"),
    ("MAX_EXCLUDES", "the AP trampoline page chosen from"),
    ("SMP2_TCG_PER_CPU_READY_HEAD_FLAKE",
     "the `-smp 2` `per_cpu_bsp: ready_head should be empty` assertion deleted"),
    ("SMP2_TCG_PER_CPU_READY_HEAD_FLAKE_ERROR",
     "the `-smp 2` `per_cpu_bsp: ready_head should be empty` assertion deleted"),
    ("with_timer", "the in-guest registry runs in production's interrupt context"),
    ("Barrier", "the block layer keeps only the orders DESIGN §10.2 names"),
    ("barrier_holds_later_requests", "the block layer keeps only the orders DESIGN §10.2 names"),
    ("barrier_writes_dirty_no_device_flush",
     "the block layer keeps only the orders DESIGN §10.2 names"),
    ("cached_barrier", "the block layer keeps only the orders DESIGN §10.2 names"),
    ("first_fence_seq", "the block layer keeps only the orders DESIGN §10.2 names"),
    ("flush_is_fence_then_device_op", "the block layer keeps only the orders DESIGN §10.2 names"),
    ("is_fence", "the block layer keeps only the orders DESIGN §10.2 names"),
    ("no_merge_across_barrier", "the block layer keeps only the orders DESIGN §10.2 names"),
    ("src/block/block_init.rs: fn barrier",
     "the block layer keeps only the orders DESIGN §10.2 names"),
    ("src/block/cache_init.rs: fn barrier",
     "the block layer keeps only the orders DESIGN §10.2 names"),
    ("src/drivers/virtio_blk_init.rs: fn barrier",
     "the block layer keeps only the orders DESIGN §10.2 names"),
    ("WritebackThenRead", "the block layer keeps only the orders DESIGN §10.2 names"),
    ("mark_clean", "the block layer keeps only the orders DESIGN §10.2 names"),
    ("mark_dirty", "the block layer keeps only the orders DESIGN §10.2 names"),
    ("restore_evict", "the block layer keeps only the orders DESIGN §10.2 names"),
    ("stash_evict", "the block layer keeps only the orders DESIGN §10.2 names"),
    ("writeback_all", "the block layer keeps only the orders DESIGN §10.2 names"),
    ("set_hhdm_offset", "a `Buddy::new` argument replaces"),
    ("from_phys", "`DmaBuffer` and `GuardedStack` are move-only"),
    ("DEFERRED", "every switch tail empties the"),
    ("MAX_DEFERRED", "every switch tail empties the"),
    ("MAX_DEFERRED_STACKS", "every switch tail empties the"),
    ("defer_free", "every switch tail empties the"),
    ("reap_zombies", "a dead thread's kernel stack is reused"),
    ("inos", "FAT and vibefs behind `InodeOps`"),
    ("ino_of", "FAT and vibefs behind `InodeOps`"),
    ("by_ino", "FAT and vibefs behind `InodeOps`"),
    ("Back", "FAT and vibefs behind `InodeOps`;"),
    ("Walked", "FAT and vibefs behind `InodeOps`;"),
    ("fat_vol_of", "FAT and vibefs behind `InodeOps`;"),
    ("fat_iget", "FAT and vibefs behind `InodeOps`;"),
    ("fat_dcache", "FAT and vibefs behind `InodeOps`;"),
    ("vfs_ls_snap", "FAT and vibefs behind `InodeOps`;"),
    ("vol_walk", "FAT and vibefs behind `InodeOps`;"),
    ("is_kernfs", "FAT and vibefs behind `InodeOps`;"),
    ("vfs_attach", "FAT and vibefs behind `InodeOps`;"),
    ("default_err", "every IDT vector enters through a stub"),
    ("default_noerr", "every IDT vector enters through a stub"),
    ("do_swapgs", "every IDT vector enters through a stub"),
    ("fn_addr_err", "every IDT vector enters through a stub"),
    ("fn_addr_noerr", "every IDT vector enters through a stub"),
    ("gs_enter", "every IDT vector enters through a stub"),
    ("gs_leave", "every IDT vector enters through a stub"),
    ("install_defaults", "every IDT vector enters through a stub"),
    ("install_err", "every IDT vector enters through a stub"),
    ("install_noerr", "every IDT vector enters through a stub"),
    ("overlay_named", "every IDT vector enters through a stub"),
    ("set_err", "every IDT vector enters through a stub"),
    ("set_noerr", "every IDT vector enters through a stub"),
    ("harden", "one per-CPU control-register routine, run"),
    ("enter_user", "`syscall_init::first_return` executes `cli`"),
    ("vibeos_iret_user", "`syscall_init::first_return` executes `cli`"),
    ("UserRegs", "one user frame (DESIGN §5.10):"),
    ("SyscallFrame", "one user frame (DESIGN §5.10):"),
    ("enter_user_full", "one user frame (DESIGN §5.10):"),
    ("vibeos_iret_user_full", "one user frame (DESIGN §5.10):"),
    ("wait_on", "stop and continue cannot lose a wakeup"),
    ("wake_queue", "stop and continue cannot lose a wakeup"),
    ("scroll_copy", "console `write` holds IF=0 only for bounded work"),
    ("_retry_hang", "the harness retries nothing"),
    ("retryable_ktest_failure", "the harness retries nothing"),
    ("silent_user_syscalls_hang", "the harness retries nothing"),
    ("check_markers_in_order", "the marker-order unit tests drive"),
    ("is_halting", "every two-way dependency between kernel modules"),
    ("DUMPING", "every two-way dependency between kernel modules"),
    ("write_bytes_raw", "every two-way dependency between kernel modules"),
    ("write_byte_raw", "every two-way dependency between kernel modules"),
    ("CONTROL_REGS", "every two-way dependency between kernel modules"),
    ("src/proc/syscall_init.rs: fn dispatch", "every two-way dependency between kernel modules"),
    ("crates/core/src/sync/lock.rs: fn acquire_mask", "the rank checker enforces DESIGN"),
    ("crates/core/src/sync/lock.rs: fn can_acquire", "the rank checker enforces DESIGN"),
    ("crates/core/src/sync/lock.rs: fn release_mask", "the rank checker enforces DESIGN"),
    ("heap_then_buddy_is_forbidden", "the heap ranks first"),
    ("KERNEL_TESTS_DIR", "one `target/` for every feature"),
    ("KERNEL_VIBEFS_CRASH_DIR", "one `target/` for every feature"),
    ("target-gp", "one `target/` for every feature"),
    ("target-kernel-tests", "one `target/` for every feature"),
    ("target-panic", "one `target/` for every feature"),
    ("target-vibefs-crash", "one `target/` for every feature"),
    ("find_features", "`vibeos-core` builds with its MSRV"),
    ("FEATURE_ATTR", "`vibeos-core` builds with its MSRV"),
    ("DEV_RAM0", "one registry of counted block-device"),
    ("DEV_VDA", "one registry of counted block-device"),
    ("NAMES_RAM", "one registry of counted block-device"),
    ("NAMES_VDA", "one registry of counted block-device"),
    ("parent_raw_read", "one registry of counted block-device"),
    ("parent_bs_cap", "one registry of counted block-device"),
    ("src/block/cache_init.rs: fn raw_read", "one registry of counted block-device"),
    ("src/block/cache_init.rs: fn raw_write", "one registry of counted block-device"),
    ("src/block/cache_init.rs: fn raw_flush", "one registry of counted block-device"),
    ("validate_buf", "user-VA accessors replace the physmap copy"),
    ("publish_uses_release_not_only_compiler_fence", "assertions that cannot fail replaced"),
    ("QEMU_BASE", "the Makefile holds no QEMU command line"),
    ("VIBEOS_INITRD", "the initrd is sized by"),
    ("INITRD_RO", "the initrd is sized by"),
    ("INITRD_BYTES", "the initrd is sized by"),
    ("MAX_ELF", "`MAX_ELF` removed: the loader maps"),
    ("TooBig", "`MAX_ELF` removed: the loader maps"),
    ("src/proc/user_init.rs: fn read_path", "`MAX_ELF` removed: the loader maps"),
    ("src/ktest/mod.rs: const SUITES", "the in-guest registry split per subsystem"),
    ("SyscallInfo", "syscall dispatch indexes the generated"),
    ("validate_args", "syscall dispatch indexes the generated"),
    ("ptr_mask", "syscall dispatch indexes the generated"),
    ("len_arg", "syscall dispatch indexes the generated"),
    ("check_user_ptr", "syscall dispatch indexes the generated"),
    ("crates/core/src/proc/syscall.rs: fn info", "syscall dispatch indexes the generated"),
    ("mix_rng", "`/dev/random` and `/dev/urandom` return only hardware bytes"),
    ("warn_xorshift", "`/dev/random` and `/dev/urandom` return only hardware bytes"),
    ("set_warn", "`/dev/random` and `/dev/urandom` return only hardware bytes"),
    ("XorShift", "`/dev/random` and `/dev/urandom` return only hardware bytes"),
    ("_PHASE0_BEFORE_TIME", "one marker registry, `tests/contract/markers.toml`, read with"),
    ("PHASE0_PANIC_PREFIX", "one marker registry, `tests/contract/markers.toml`, read with"),
    ("lapic_timer_marker", "one marker registry, `tests/contract/markers.toml`, read with"),
    ("BOOT_DONE", "one marker registry, `tests/contract/markers.toml`, read with"),
]

SCOPE = ["src", "crates", "user", "tests", "scripts", ".github", "Makefile", "build.rs",
         "setup.sh"]
EXEMPT = frozenset({"scripts/check_gone.py", "tests/harness/test_gone.py"})
GATE_MAP = re.compile(r"^tests/gates/phase-\d+\.toml$")
GATE_KEY = re.compile(r'^key = (?:"""[\s\S]*?"""|"[^"\n]*")', re.M)
DEFINITION = re.compile(r"^(\S+): (fn|struct|enum|union|trait|type|const|static|mod|macro"
                        r"|def|class) ([A-Za-z_][A-Za-z0-9_]*)$")
GLOB_CHARS = frozenset("*?[")


def _word(name: str) -> re.Pattern[str]:
    return re.compile(r"(?<![A-Za-z0-9_])" + re.escape(name) + r"(?![A-Za-z0-9_])")


def _definition(kind: str, name: str) -> re.Pattern[str]:
    if kind == "macro":
        return re.compile(r"macro_rules!\s*" + re.escape(name) + r"(?![A-Za-z0-9_])")
    return re.compile(r"(?<![A-Za-z0-9_])" + kind + r"\s+" + re.escape(name)
                      + r"(?![A-Za-z0-9_])")


def _hits(pattern: re.Pattern[str], files: dict[str, str]) -> list[tuple[str, int]]:
    out: list[tuple[str, int]] = []
    for path in sorted(files):
        for n, line in enumerate(files[path].splitlines(), start=1):
            if pattern.search(line):
                out.append((path, n))
    return out


def find(gone: list[tuple[str, str]], files: dict[str, str], paths: list[str]) -> list[str]:
    """Each place a row of `gone` still appears. `files` maps a scoped path to
    its text; `paths` lists every scoped path, unreadable ones included."""
    errors: list[str] = []
    for row, key in gone:
        why = f"{row!r} is gone ({key})"
        d = DEFINITION.match(row)
        if d is not None:
            path, kind, name = d.groups()
            text = files.get(path)
            if text is not None:
                for n, line in enumerate(text.splitlines(), start=1):
                    if _definition(kind, name).search(line):
                        errors.append(f"{path}:{n}: defines {kind} {name}: {why}")
            continue
        for p in paths:
            if p.rsplit("/", 1)[-1] == row:
                errors.append(f"{p}: file named {why}")
        if "/" in row or GLOB_CHARS & set(row):
            for p in paths:
                if fnmatch.fnmatchcase(p, row) or fnmatch.fnmatchcase(p, row.rstrip("/") + "/*"):
                    errors.append(f"{p}: path {why}")
            if GLOB_CHARS & set(row):
                continue
            pattern = re.compile(re.escape(row))
        else:
            pattern = _word(row)
        for path, n in _hits(pattern, files):
            errors.append(f"{path}:{n}: {why}")
    return errors


def check_table(gone: list[tuple[str, str]], roadmap_text: str) -> list[str]:
    """Each row names its box by a key of one box line, and no row repeats."""
    errors: list[str] = []
    boxes = gatelib.parse_boxes(roadmap_text)
    lines = roadmap_text.splitlines()
    seen: set[str] = set()
    for row, key in gone:
        if row in seen:
            errors.append(f"GONE: {row!r} listed twice")
        seen.add(row)
        try:
            gatelib.match_key(key, boxes, lines)
        except gatelib.GateError as e:
            errors.append(f"GONE: {row!r}: {e}")
    return errors


def drop_gate_keys(path: str, text: str) -> str:
    """A gate map's text with each `key` string emptied, its lines kept."""
    if not GATE_MAP.match(path):
        return text
    return GATE_KEY.sub(lambda m: 'key = ""' + "\n" * m.group(0).count("\n"), text)


def scoped_files(repo: Path = ROOT) -> tuple[dict[str, str], list[str]]:
    """Tracked and untracked, not ignored, files in scope, less the exempt."""
    out = gatelib.git(repo, "ls-files", "-z", "--cached", "--others", "--exclude-standard",
                      "--", *SCOPE)
    paths = sorted({p for p in out.split("\0") if p and p not in EXEMPT})
    files: dict[str, str] = {}
    for p in paths:
        try:
            files[p] = drop_gate_keys(p, (repo / p).read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError):
            continue
    return files, paths


def main(argv: list[str] | None = None) -> int:
    if argv:
        print("usage: check_gone.py", file=sys.stderr)
        return 2
    errors = check_table(GONE, gatelib.ROADMAP.read_text(encoding="utf-8"))
    files, paths = scoped_files()
    errors += find(GONE, files, paths)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"check_gone: ok ({len(GONE)} rows, {len(paths)} files)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
