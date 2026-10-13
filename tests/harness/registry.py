"""The marker registry, `tests/contract/markers.toml` (ROADMAP §10.2, C-MARKERS).

The one loader of the file (AGENTS.md rule 10). `harness.py` builds every
configuration's contract list and every failure list from its rows, and
`scripts/check_markers.py` checks the rows against `src/` and
`crates/core/src/marker.rs`.

A row is one line the kernel, a user program or Limine prints and the
harness knows. Its `text` may hold placeholders, `<[a-z_]+>`, for the parts
that vary; the text never starts with one. The schema is in the file's
header comment, and `load_rows` enforces it.

Standard library only (`tomllib`), and nothing from `harness.py`, which
imports this.
"""

from __future__ import annotations

import functools
import re
import tomllib
from collections.abc import Iterable, Mapping
from dataclasses import dataclass
from pathlib import Path

REGISTRY_PATH = Path(__file__).resolve().parent.parent / "contract" / "markers.toml"

KINDS = ("contract", "diagnostic", "failure", "test")
ARCHES = ("both", "x86_64", "aarch64")
SOURCES = ("kernel", "user", "limine")
WHEN_TERMS = (
    "hpet",
    "pit",
    "smp>1",
    "gp_test",
    "panic_test",
    "panic_nest_test",
    "panic_stop_test",
    "hang_test",
)
REPEATS = ("per_ap",)
KEYS = frozenset(
    ("text", "kind", "arch", "source", "section", "order", "name", "when", "repeat")
)

PLACEHOLDER = re.compile(r"<[a-z_]+>")
SECTION = re.compile(r"§(\d+)\.(\d+)")
# aarch64-only rows sit between the existing x86 tens (ROADMAP §11.7).
ORDER_STEP = 5

# The regex metacharacters `_escape` escapes: every other character stands
# for itself, so a built pattern reads as its text does.
_META = re.compile(r"([.^$*+?{}\[\]\\|()])")


class RegistryError(Exception):
    """A malformed registry: the message is `<path>: row <i>: <why>`."""


@dataclass(frozen=True)
class Row:
    """One `[[marker]]` row. `order`, `name` and `repeat` are None, and `when`
    is empty, where the row leaves them out."""

    text: str
    kind: str
    arch: str
    source: str
    section: str
    order: int | None = None
    when: tuple[str, ...] = ()
    repeat: str | None = None
    name: str | None = None


def _escape(text: str) -> str:
    return _META.sub(r"\\\1", text)


def fragments(text: str) -> list[str]:
    """The literal pieces of `text` between its placeholders, empty ones
    included: `"a <n> b"` gives `["a ", " b"]`, `"a <n>"` gives `["a ", ""]`."""
    return PLACEHOLDER.split(text)


def placeholders(text: str) -> list[str]:
    """The names of `text`'s placeholders, in order, without the brackets."""
    return [p[1:-1] for p in PLACEHOLDER.findall(text)]


def bind(text: str, values: Mapping[str, str]) -> str:
    """`text` with each placeholder whose name `values` holds replaced by its value."""
    return PLACEHOLDER.sub(lambda m: values.get(m.group(0)[1:-1], m.group(0)), text)


def row_regex(text: str) -> str:
    """A regex for `text`: its literal parts as themselves, each placeholder `.+`."""
    return ".+".join(_escape(f) for f in fragments(text))


def sample(row: Row, values: Mapping[str, str] | None = None) -> str:
    """`row`'s text with every placeholder rendered: from `values` where it
    names one, otherwise a stand-in (`1` for a number, a word for the rest)."""
    values = dict(values or {})

    def one(m: re.Match[str]) -> str:
        name = m.group(0)[1:-1]
        if name in values:
            return values[name]
        return "1" if name in ("n", "ap", "status") else f"x{name}"

    return PLACEHOLDER.sub(one, row.text)


def is_head_signature(row: Row) -> bool:
    """A failure row whose placeholders all follow its last literal."""
    frags = fragments(row.text)
    return row.kind == "failure" and all(f == "" for f in frags[1:])


def head(text: str) -> str:
    """`text` up to its first placeholder."""
    return fragments(text)[0]


def _fail(path: Path, i: int, why: str) -> RegistryError:
    return RegistryError(f"{path}: row {i}: {why}")


def _row(path: Path, i: int, raw: object) -> Row:
    if not isinstance(raw, dict):
        raise _fail(path, i, "not a table")
    unknown = sorted(set(raw) - KEYS)
    if unknown:
        raise _fail(path, i, f"unknown key {unknown[0]!r}")
    for key in ("text", "kind", "arch", "source", "section"):
        if not isinstance(raw.get(key), str):
            raise _fail(path, i, f"{key!r} missing or not a string")
    text, kind = raw["text"], raw["kind"]
    if not text or PLACEHOLDER.match(text):
        raise _fail(path, i, f"text {text!r} is empty or starts with a placeholder")
    if "<" in PLACEHOLDER.sub("", text) or ">" in PLACEHOLDER.sub("", text):
        raise _fail(path, i, f"text {text!r} has a malformed placeholder")
    for key, allowed in (("kind", KINDS), ("arch", ARCHES), ("source", SOURCES)):
        if raw[key] not in allowed:
            raise _fail(path, i, f"{key} {raw[key]!r} is not one of {', '.join(allowed)}")
    if SECTION.fullmatch(raw["section"]) is None:
        raise _fail(path, i, f"section {raw['section']!r} is not §N.M")
    order, name = raw.get("order"), raw.get("name")
    if kind == "contract":
        if not isinstance(order, int) or isinstance(order, bool):
            raise _fail(path, i, "a contract row needs an int 'order'")
        if not isinstance(name, str) or not name:
            raise _fail(path, i, "a contract row needs a 'name'")
    else:
        for key in ("order", "name", "when", "repeat"):
            if key in raw:
                raise _fail(path, i, f"{key!r} is only for contract rows")
    when = raw.get("when", [])
    if not isinstance(when, list) or not all(isinstance(t, str) for t in when):
        raise _fail(path, i, "'when' is not a list of strings")
    for term in when:
        if term.removeprefix("!") not in WHEN_TERMS:
            raise _fail(path, i, f"unknown 'when' term {term!r}")
    repeat = raw.get("repeat")
    if repeat is not None and repeat not in REPEATS:
        raise _fail(path, i, f"repeat {repeat!r} is not one of {', '.join(REPEATS)}")
    return Row(
        text=text,
        kind=kind,
        arch=raw["arch"],
        source=raw["source"],
        section=raw["section"],
        order=order,
        when=tuple(when),
        repeat=repeat,
        name=name,
    )


def parse_rows(text: str, path: Path = REGISTRY_PATH) -> tuple[Row, ...]:
    """The rows of a registry's TOML `text`; `path` names it in errors."""
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise RegistryError(f"{path}: {e}") from e
    unknown = sorted(set(data) - {"marker"})
    if unknown:
        raise RegistryError(f"{path}: unknown table {unknown[0]!r}")
    raws = data.get("marker", [])
    if not isinstance(raws, list):
        raise RegistryError(f"{path}: 'marker' is not an array of tables")
    rows = tuple(_row(path, i, raw) for i, raw in enumerate(raws, 1))
    orders: dict[int, int] = {}
    names: dict[str, int] = {}
    for i, row in enumerate(rows, 1):
        if row.order is not None:
            if row.order in orders:
                raise _fail(path, i, f"order {row.order} repeats row {orders[row.order]}'s")
            if row.order % ORDER_STEP:
                raise _fail(path, i, f"order {row.order} is not a multiple of {ORDER_STEP}")
            orders[row.order] = i
        if row.name is not None:
            if row.name in names:
                raise _fail(path, i, f"name {row.name!r} repeats row {names[row.name]}'s")
            names[row.name] = i
    return rows


@functools.cache
def _load(path: str) -> tuple[Row, ...]:
    p = Path(path)
    try:
        text = p.read_text(encoding="utf-8")
    except OSError as e:
        raise RegistryError(f"{p}: {e.strerror}") from e
    return parse_rows(text, p)


def load_rows(path: Path | str = REGISTRY_PATH) -> tuple[Row, ...]:
    """The rows of the registry at `path`, validated (cached per path)."""
    return _load(str(Path(path).resolve()))


@dataclass(frozen=True)
class BootConfig:
    """What a boot's contract depends on: `smp` CPUs, HPET or the PIT, the
    LAPIC timer mode the kernel prints (`<mode>`), the clocksource it names
    (`<clocksource>`), and the test builds. On aarch64, `el` and `arm_timer`
    bind `<el>` and `<timer>` when the machine asks for EL2; empty leaves
    those placeholders open."""

    hpet: bool
    smp: int
    lapic_mode: str
    clocksource: str = ""
    gp_test: bool = False
    panic_test: bool = False
    panic_nest_test: bool = False
    panic_stop_test: bool = False
    hang_test: bool = False
    arch: str = "x86_64"
    el: str = ""
    arm_timer: str = ""


def _term(term: str, cfg: BootConfig) -> bool:
    value = {
        "hpet": cfg.hpet,
        "pit": not cfg.hpet,
        "smp>1": cfg.smp > 1,
        "gp_test": cfg.gp_test,
        "panic_test": cfg.panic_test,
        "panic_nest_test": cfg.panic_nest_test,
        "panic_stop_test": cfg.panic_stop_test,
        "hang_test": cfg.hang_test,
    }[term.removeprefix("!")]
    return not value if term.startswith("!") else value


def holds(row: Row, cfg: BootConfig) -> bool:
    """Whether `row` applies to `cfg`: its arch and every `when` term."""
    if row.arch not in ("both", cfg.arch):
        return False
    return all(_term(t, cfg) for t in row.when)


def contract(rows: Iterable[Row], cfg: BootConfig) -> list[tuple[Row, dict[str, str]]]:
    """`cfg`'s contract, in order: each contract row that holds, with its
    bindings. A run of consecutive per-AP rows is emitted once for each AP,
    `i` in `1..smp`, binding `<n>` to `i` and `<ap>` to `i - 1`; `<mode>` is
    `cfg.lapic_mode` and `<clocksource>` is `cfg.clocksource` everywhere.
    `<el>` and `<timer>` bind only when `cfg` sets them."""
    base = {"mode": cfg.lapic_mode, "clocksource": cfg.clocksource}
    if cfg.el:
        base["el"] = cfg.el
    if cfg.arm_timer:
        base["timer"] = cfg.arm_timer
    live = sorted(
        (r for r in rows if r.kind == "contract" and holds(r, cfg)),
        key=lambda r: r.order or 0,
    )
    out: list[tuple[Row, dict[str, str]]] = []
    i = 0
    while i < len(live):
        if live[i].repeat != "per_ap":
            out.append((live[i], dict(base)))
            i += 1
            continue
        j = i
        while j < len(live) and live[j].repeat == "per_ap":
            j += 1
        for n in range(1, max(cfg.smp, 1)):
            for row in live[i:j]:
                out.append((row, {**base, "n": str(n), "ap": str(n - 1)}))
        i = j
    return out


def per_ap_close(rows: Iterable[Row], cfg: BootConfig) -> tuple[Row, Row] | None:
    """(the last row of `cfg`'s per-AP group, the contract row after it), or
    None without such a pair: the second row's line comes after exactly one
    first-row line per AP, and before any other (TESTING.md §8.3, F141)."""
    live = sorted(
        (r for r in rows if r.kind == "contract" and holds(r, cfg)),
        key=lambda r: r.order or 0,
    )
    for a, b in zip(live, live[1:], strict=False):
        if a.repeat == "per_ap" and b.repeat != "per_ap":
            return a, b
    return None


def _failures(rows: Iterable[Row], sources: Iterable[str], arch: str) -> list[Row]:
    want = set(sources)
    return [
        r
        for r in rows
        if r.kind == "failure" and r.source in want and r.arch in ("both", arch)
    ]


def signatures(
    rows: Iterable[Row], sources: Iterable[str], arch: str = "x86_64"
) -> tuple[str, ...]:
    """The head signature of each head-signature failure row from `sources`,
    in file order. A signature that contains another one is dropped, so a
    line holding it already fails on the shorter one."""
    heads: list[str] = []
    for r in _failures(rows, sources, arch):
        if is_head_signature(r) and head(r.text) not in heads:
            heads.append(head(r.text))
    return tuple(s for s in heads if not any(t != s and t in s for t in heads))


def failure_patterns(
    rows: Iterable[Row], sources: Iterable[str], arch: str = "x86_64"
) -> tuple[re.Pattern[str], ...]:
    """A compiled `row_regex` for each failure row from `sources` with a
    placeholder before its last literal, in file order; a line fails when
    one matches anywhere in it."""
    return tuple(
        re.compile(row_regex(r.text))
        for r in _failures(rows, sources, arch)
        if not is_head_signature(r)
    )


def ansi_tolerant(signature: str) -> re.Pattern[str]:
    """`signature` as a pattern that also matches it with ANSI colour codes
    before its first `:`, as Limine colours the word it prints there."""
    word, colon, rest = signature.partition(":")
    ansi = r"(?:\x1b\[[0-9;]*m)*" if colon else ""
    return re.compile(_escape(word) + ansi + _escape(colon + rest))
