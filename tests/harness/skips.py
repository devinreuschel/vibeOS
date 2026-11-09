"""Expected skips as data (DESIGN §8.2, ROADMAP §10.2, C-SKIPS).

`tests/harness/skips.toml` lists each test allowed to skip, with its reason
and the configurations it skips in. A run fails when its skipped set
differs, in either direction, from the rows that match the configuration
it launched (`check_skips`), so a QEMU line that loses a device, or a
detection that regresses, fails the tier instead of turning tests into
skips. Only tests with a `run` line in the boot are compared: a test the
run does not select needs no row, and one that `VIBEOS_KTEST` names
without a glob character must run whatever the file says.

Standard library only.
"""

from __future__ import annotations

import os
import platform
import tomllib
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, field

from tests.harness.harness import (
    HarnessError,
    QemuConfig,
    effective_accel_name,
    guest_cpu,
    qemu_argv,
)

SKIPS_TOML = os.path.join(os.path.dirname(os.path.abspath(__file__)), "skips.toml")

# The configuration fields a row may constrain; `smp` is an integer.
FIELDS = ("arch", "accel", "cpu", "smp", "mem", "machine", "host")
_INT_FIELDS = frozenset({"smp"})
# `counterpart` / `no_counterpart` are check_arch.py's aarch64 fields
# (ROADMAP §11.1); they do not constrain a launch.
_KEYS = frozenset({"name", "reason", "counterpart", "no_counterpart", *FIELDS})
# A `VIBEOS_KTEST` item holding one of these is a glob, not a name.
GLOB_CHARS = "*?["

Value = str | int


@dataclass(frozen=True)
class SkipRow:
    """One `[[skip]]` row: `name` may skip with `reason` wherever every
    field in `match` holds one of its values. A field left out matches
    every value."""

    name: str
    reason: str
    match: Mapping[str, tuple[Value, ...]] = field(default_factory=dict)


def _values(where: str, key: str, raw: object) -> tuple[Value, ...]:
    items = raw if isinstance(raw, list) else [raw]
    if not items:
        raise HarnessError(f"{where}: {key} is an empty array")
    out: list[Value] = []
    for v in items:
        if key in _INT_FIELDS:
            if not isinstance(v, int) or isinstance(v, bool):
                raise HarnessError(f"{where}: {key} = {v!r} is not an integer")
        elif not isinstance(v, str) or not v:
            raise HarnessError(f"{where}: {key} = {v!r} is not a non-empty string")
        out.append(v)
    return tuple(out)


def parse_skips(data: Mapping[str, object], where: str = "skips.toml") -> list[SkipRow]:
    """The rows of a parsed skips file. Raises `HarnessError` on an unknown
    key, a missing `name` or `reason`, or a value of the wrong type."""
    unknown_top = set(data) - {"skip"}
    if unknown_top:
        raise HarnessError(f"{where}: unknown top-level key(s) {sorted(unknown_top)}")
    raw_rows = data.get("skip", [])
    if not isinstance(raw_rows, list):
        raise HarnessError(f"{where}: `skip` is not an array of tables")
    rows: list[SkipRow] = []
    for i, raw in enumerate(raw_rows):
        at = f"{where}: skip {i + 1}"
        if not isinstance(raw, dict):
            raise HarnessError(f"{at}: not a table")
        unknown = set(raw) - _KEYS
        if unknown:
            raise HarnessError(f"{at}: unknown key(s) {sorted(unknown)}")
        name = raw.get("name")
        reason = raw.get("reason")
        if not isinstance(name, str) or not name:
            raise HarnessError(f"{at}: `name` missing or not a string")
        if not isinstance(reason, str) or not reason:
            raise HarnessError(f"{at} ({name}): `reason` missing or not a string")
        match = {k: _values(f"{at} ({name})", k, raw[k]) for k in FIELDS if k in raw}
        rows.append(SkipRow(name, reason, match))
    return rows


def load_skips(path: str = SKIPS_TOML) -> list[SkipRow]:
    """Read and validate `path` (`tests/harness/skips.toml` by default)."""
    try:
        with open(path, "rb") as f:
            data = tomllib.load(f)
    except (OSError, tomllib.TOMLDecodeError) as e:
        raise HarnessError(f"{path}: {e}") from e
    return parse_skips(data, os.path.basename(path))


def _machine(argv: Sequence[str]) -> str:
    """The last `-machine` value in `argv`, or `pc`, QEMU's x86 default."""
    machine = "pc"
    for i, arg in enumerate(argv[:-1]):
        if arg == "-machine":
            machine = argv[i + 1]
    return machine


def launch_config(cfg: QemuConfig) -> dict[str, Value]:
    """The configuration `cfg` launches, as the fields rows match on."""
    argv = qemu_argv(cfg, None)
    prog = os.path.basename(argv[0])
    arch = prog.removeprefix("qemu-system-")
    return {
        "arch": arch,
        "accel": effective_accel_name(cfg),
        "cpu": guest_cpu(cfg),
        "smp": cfg.smp,
        "mem": cfg.mem,
        "machine": _machine(argv),
        "host": platform.system().lower(),
    }


def row_matches(row: SkipRow, config: Mapping[str, Value]) -> bool:
    """Whether every field `row` sets holds `config`'s value."""
    return all(config.get(k) in vals for k, vals in row.match.items())


def must_run_names(ktest: str) -> frozenset[str]:
    """The items of a `VIBEOS_KTEST` value that name one test: no glob
    character. Such a test must run, whatever the rows say."""
    return frozenset(
        item for item in ktest.split(",") if item and not any(c in item for c in GLOB_CHARS)
    )


def check_skips(
    skipped: Mapping[str, str],
    selected: Iterable[str],
    config: Mapping[str, Value],
    rows: Iterable[SkipRow],
    *,
    must_run: Iterable[str] = (),
) -> None:
    """Compare one boot's skips with the rows that match `config`.

    `skipped` maps each test that printed a `skip` line to its reason;
    `selected` names every test with a `run` line. Raises one
    `HarnessError` listing every unlisted skip, every test of a matching
    row that ran instead of skipping, every reason that differs from its
    row's, and every skip of a name in `must_run`. A row whose test has no
    `run` line needs nothing.
    """
    selected = set(selected)
    must = set(must_run)
    expected: dict[str, set[str]] = {}
    for row in rows:
        if row_matches(row, config):
            expected.setdefault(row.name, set()).add(row.reason)
    problems: list[str] = []
    for name in sorted(skipped):
        why = skipped[name]
        if name in must:
            problems.append(f"{name} skipped ({why!r}), but VIBEOS_KTEST names it: it must run")
        elif name not in expected:
            problems.append(f"{name} skipped ({why!r}), and no skips.toml row matches")
        elif why not in expected[name]:
            want = " or ".join(repr(r) for r in sorted(expected[name]))
            problems.append(f"{name} skipped with reason {why!r}; skips.toml says {want}")
    for name in sorted(expected):
        if name in selected and name not in skipped and name not in must:
            problems.append(f"{name} ran, but skips.toml lists it as a skip here")
    if problems:
        cfg = " ".join(f"{k}={config[k]}" for k in FIELDS if k in config)
        body = "\n  ".join(problems)
        raise HarnessError(f"ktest skips differ from skips.toml ({cfg}):\n  {body}")
