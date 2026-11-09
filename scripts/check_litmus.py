#!/usr/bin/env python3
"""Each barrier helper names a litmus test or an Arm ARM rule (ROADMAP §11.7).

Helpers. Every barrier helper in the aarch64 port must have a comment that
either names a `tests/litmus/*.litmus` file that exists, or cites an Arm
ARM rule for an idiom herd7 cannot express (TLB maintenance, the `dsb`
before an SGI write, Device against Normal MMIO, cache maintenance). A
citation is `Arm ARM` or `DDI0487` followed by a section number (`B2.3.5`)
or a quoted section title (`"TLB maintenance requirements"`); naming the
manual alone cites no rule. A barrier helper is:

  - an item (`fn`, `const`, `static`, `global_asm!`) in the port's files
    (`src/arch/aarch64/`, `crates/core/src/arch/aarch64/`), outside its
    `ktest*.rs` files, its `#[cfg(test)]` module, and the `.if {ktest}`
    blocks of its assembly, which no published ISO builds (AGENTS.md rule
    9), with a string literal that starts with a barrier or maintenance
    instruction (`dmb`, `dsb`, `tlbi`, `dc`, `ic`, `stlr`, `ldar`, `ldapr`,
    `ldaxr`, `stlxr`). A bare `isb` is not one: it is a context
    synchronization event, which orders no memory access by itself, and
    each one in the port ends a System register write or a maintenance
    sequence that its helper already cites. Each barrier line is checked
    on its own. In a `fn`, `const` or `static`, one helper, its comment is
    the run of `//` and `#[..]` lines just above the item, or a `//` line
    in the item before the barrier. A `global_asm!` block holds several
    stubs, so each barrier there needs a `//` line in the 12 lines above
    it, inside the block;
  - any `dma_wmb`, `dma_rmb`, `dma_mb`, `mmio_read` or `mmio_write` there;
  - each item `SITES` lists: the TLB sequences and the kernel functions
    that run them, which hold no instruction text, and the portable
    idioms the port compiles to `stlr`/`ldar`, `dmb` and LSE atomics, one
    entry per idiom ROADMAP §11.7's litmus set covers (the seqlock, the
    log ring, `SpinMutex`, the wake inbox, the ASID compare-exchange
    against a rollover, publish-last in `IoWaiter::finish` and the
    `on_cpu` clear, the call-function slot, the secondary stub's status).
    `Type::name` finds `fn name` in the first `impl` block for `Type`. Its
    comment is the run just above it. An entry whose file or item is gone
    fails, so the list cannot go stale.

Twins. Every non-relaxed AArch64 test has at least one barrier-removed twin
beside it: `<test>_relaxed.litmus`, `<test>.relaxed.litmus`, or
`<test>_no_<barrier>.litmus`. A test with more than one barrier may show
each one's necessity in a twin of its own (`wake_inbox_no_release`,
`wake_inbox_no_acquire`); this script requires one twin, not one per
barrier. A twin names an existing test, and an AArch64 twin differs from
its test only in barriers: the same init block, threads, and condition, and
each instruction the same, or a weaker form of it (`LDAR` to `LDR`, `STLR`
to `STR`, an `A`/`L`/`AL` ordering suffix dropped from an LSE atomic), or a
`DMB`/`DSB`/`ISB` deleted, with at least one such change. An `ISB` counts
here because herd7's `aarch64.cat` orders a load after a control
dependency and an `ISB`. A twin's name says what it removes:
`_no_release` removes only release orderings and full or store fences,
`_no_acquire` only acquire orderings and full or load fences, and
`_no_fence` only fences.

`make check` runs both static checks. `make litmus` runs this script with
`--run`, which also invokes herd7. Under both `aarch64.cat` and
`x86tso.cat`, a non-relaxed test must report Never, and each twin must
report Sometimes or Always. An x86 message-passing idiom that TSO already
forbids has no relaxed twin: the unfenced test is Never, which is the
record that the barrier is elided. The store-then-load twins keep one,
because that shape needs `MFENCE`.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path
from shutil import which

ROOT = Path(__file__).resolve().parent.parent
HELPERS = ("dma_wmb", "dma_rmb", "dma_mb", "mmio_read", "mmio_write")
PORT_GLOBS = ("src/arch/aarch64/**/*.rs", "crates/core/src/arch/aarch64/**/*.rs")
# The barrier helpers that hold no instruction text: (file, item name).
SITES: tuple[tuple[str, str], ...] = (
    ("crates/core/src/arch/aarch64/tlb.rs", "LEAF_INVAL"),
    ("crates/core/src/arch/aarch64/tlb.rs", "ALL_INVAL"),
    ("crates/core/src/arch/aarch64/tlb.rs", "TABLE_INVAL"),
    ("crates/core/src/arch/aarch64/tlb.rs", "ASID_INVAL"),
    ("crates/core/src/arch/aarch64/tlb.rs", "ROLLOVER_LOCAL"),
    ("crates/core/src/arch/aarch64/tlb.rs", "TTBR0_SWITCH"),
    ("crates/core/src/arch/aarch64/tlb.rs", "BBM_AFTER_INVALID"),
    ("crates/core/src/arch/aarch64/tlb.rs", "BBM_AFTER_MAKE"),
    ("crates/core/src/arch/aarch64/tlb.rs", "PUBLISH_TABLE"),
    ("crates/core/src/arch/aarch64/tlb.rs", "sync_range"),
    ("src/arch/aarch64/cpu.rs", "tlbi_all"),
    ("src/arch/aarch64/cpu.rs", "tlbi_asid0_local"),
    ("src/arch/aarch64/cpu.rs", "tlbi_va"),
    ("src/arch/aarch64/cpu.rs", "bbm_break"),
    ("src/arch/aarch64/cpu.rs", "bbm_make"),
    ("src/arch/aarch64/cpu.rs", "publish_table"),
    ("crates/core/src/time/mod.rs", "TickClock::bump"),
    ("crates/core/src/time/mod.rs", "TickClock::write"),
    ("crates/core/src/time/mod.rs", "TickClock::read"),
    ("crates/core/src/log/trace/mod.rs", "Ring::push"),
    ("crates/core/src/log/trace/mod.rs", "Ring::get"),
    ("crates/core/src/sync/mod.rs", "SpinLock::try_acquire"),
    ("crates/core/src/sync/mod.rs", "SpinLock::release"),
    ("crates/core/src/irq/ipi.rs", "WakeInbox::push"),
    ("crates/core/src/irq/ipi.rs", "WakeInbox::drain"),
    ("crates/core/src/mm/asid.rs", "AsidAlloc::switch_fast"),
    ("crates/core/src/mm/asid.rs", "AsidAlloc::rollover"),
    ("src/block/block_init.rs", "IoWaiter::finish"),
    ("crates/core/src/block/mod.rs", "DoneWord::publish"),
    ("crates/core/src/block/mod.rs", "DoneWord::poll"),
    ("crates/core/src/sched/thread.rs", "OnCpu::clear"),
    ("crates/core/src/sched/thread.rs", "OnCpu::is_clear"),
    ("crates/core/src/irq/ipi.rs", "CallSlot::publish"),
    ("crates/core/src/irq/ipi.rs", "CallSlot::poll"),
    ("crates/core/src/irq/ipi.rs", "CallSlot::ack"),
    ("src/smp/smp_init.rs", "wait_secondary_status"),
)
LITMUS_NAME = re.compile(r"tests/litmus/[A-Za-z0-9_./-]+\.litmus")
# The manual, then within a few words a section number or a quoted title.
ARM_CITE = re.compile(
    r"(?:Arm ARM|ARM ARM|DDI0487)[^\"]{0,40}?(?:\b[A-K]\d+(?:\.\d+)+\b|\"[A-Z][^\"]{5,}\")",
    re.I,
)
ASM_WINDOW = 12
FN = re.compile(r"^.*\bfn\s+(" + "|".join(HELPERS) + r")\b", re.M)
BARRIER_ASM = re.compile(
    r"^\s*(?:dmb|dsb|tlbi|dc\s+[cizg]\w*|ic\s+i\w*|stlr[bh]?|ldar[bh]?|ldapr[bh]?"
    r"|ldaxr[bh]?|stlxr[bh]?)\b",
    re.I,
)
STRING = re.compile(r'"((?:[^"\\]|\\.)*)"')
_VIS = r"(?:pub(?:\([^)]*\))?\s+)?"
ITEM = re.compile(
    r"^\s*" + _VIS + r"(?:(?:const|unsafe|async|extern\s+\"[^\"]*\")\s+)*fn\s+(\w+)"
    r"|^\s*" + _VIS + r"(?:const|static(?:\s+mut)?)\s+([A-Z_][A-Z0-9_]*)\b"
    r"|^\s*(?:core::arch::)?(global_asm)!"
)
TEST_MOD = re.compile(r"^\s*#\[cfg\(test\)\]\s*$")
TWIN = re.compile(r"^(?P<base>.+?)(?:[._]relaxed|_no_[a-z0-9_]+)\.litmus$")
AARCH = re.compile(r"^AArch64\b", re.I | re.M)
X86 = re.compile(r"^X86(?:_64)?\b", re.I | re.M)
OBS = re.compile(r"Observation\s+\S+\s+(Never|Sometimes|Always)")


def _rs_files(root: Path) -> list[Path]:
    out: list[Path] = []
    for g in PORT_GLOBS:
        out.extend(p for p in root.glob(g) if p.is_file() and not p.name.startswith("ktest"))
    return sorted(set(out))


def _code_lines(text: str) -> list[str]:
    """The file's lines up to its `#[cfg(test)] mod`, which holds no helper."""
    lines = text.splitlines()
    for i, line in enumerate(lines):
        if TEST_MOD.match(line):
            rest = [ln for ln in lines[i + 1 : i + 3] if ln.strip()]
            if rest and re.match(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s", rest[0]):
                return lines[:i]
    return lines


def _doc_block(lines: list[str], item: int) -> list[str]:
    """The `//` and `#[..]` lines just above line `item`."""
    out: list[str] = []
    j = item - 1
    while j >= 0:
        s = lines[j].strip()
        if not (s.startswith("//") or s.startswith("#[")):
            break
        out.append(lines[j])
        j -= 1
    return out[::-1]


def _comments(lines: list[str]) -> str:
    """The `//` comment text of `lines`, joined into one line so a quoted
    title may wrap."""
    out = []
    for line in lines:
        at = line.find("//")
        if at >= 0:
            out.append(line[at:].lstrip("/!").strip())
    return " ".join(out)


def _cite_errors(root: Path, rel: str, line: int, name: str, text: str) -> list[str]:
    lit = LITMUS_NAME.findall(text)
    if not lit and ARM_CITE.search(text) is None:
        return [
            f"{rel}:{line}: {name} names no tests/litmus/*.litmus file and cites no Arm ARM "
            "rule (a section number or a quoted title)"
        ]
    return [f"{rel}:{line}: {name} names missing {p}" for p in lit if not (root / p).is_file()]


def _item_start(lines: list[str], at: int) -> tuple[int, str] | None:
    for j in range(at, -1, -1):
        m = ITEM.match(lines[j])
        if m is not None:
            return j, next(g for g in m.groups() if g)
    return None


def check_helpers(root: Path = ROOT) -> list[str]:
    """Comment-gate failures."""
    errs: list[str] = []
    for path in _rs_files(root):
        rel = path.relative_to(root).as_posix()
        text = path.read_text(encoding="utf-8")
        lines = _code_lines(text)
        for m in FN.finditer(text):
            line = text.count("\n", 0, m.start())
            if line >= len(lines):
                continue
            win = _comments(_doc_block(lines, line))
            errs += _cite_errors(root, rel, line + 1, m.group(1), win)
        conds: list[bool] = []
        for i, src in enumerate(lines):
            if src.strip().startswith("//"):
                continue
            strs = STRING.findall(src)
            for s in strs:
                d = s.strip()
                if d.startswith(".if"):
                    conds.append("{ktest}" in d)
                elif d.startswith(".endif") and conds:
                    conds.pop()
            if any(conds):
                continue
            if not any(BARRIER_ASM.match(s) for s in strs):
                continue
            start = _item_start(lines, i)
            if start is None:
                errs.append(f"{rel}:{i + 1}: barrier instruction outside any item")
                continue
            k, name = start
            if name == "global_asm":
                win = _comments(lines[max(k, i - ASM_WINDOW) : i + 1])
            else:
                win = _comments(_doc_block(lines, k) + lines[k : i + 1])
            ins = next(s.strip() for s in strs if BARRIER_ASM.match(s))
            errs += _cite_errors(root, rel, i + 1, f"{name} `{ins}`", win)
    return errs


def _find_site(lines: list[str], name: str) -> int | None:
    """The line of item `name`, or of `fn name` in `Type`'s first `impl`."""
    owner, _, item = name.rpartition("::")
    start = 0
    if owner:
        impl = re.compile(r"^\s*impl\b[^{]*?\b" + re.escape(owner) + r"\b")
        hit = next((i for i, src in enumerate(lines) if impl.match(src)), None)
        if hit is None:
            return None
        start = hit + 1
    for i in range(start, len(lines)):
        if owner and re.match(r"^impl\b", lines[i]):
            return None
        m = ITEM.match(lines[i])
        if m is not None and item in m.groups():
            return i
    return None


def check_sites(root: Path = ROOT, sites: tuple[tuple[str, str], ...] = SITES) -> list[str]:
    """Each `SITES` item exists and names a litmus test or the Arm ARM."""
    errs: list[str] = []
    for rel, name in sites:
        path = root / rel
        if not path.is_file():
            errs.append(f"{rel}: SITES names {name} in a file that is gone")
            continue
        lines = _code_lines(path.read_text(encoding="utf-8"))
        at = _find_site(lines, name)
        if at is None:
            errs.append(f"{rel}: SITES names {name}, which is gone")
            continue
        win = _comments(_doc_block(lines, at))
        errs += _cite_errors(root, rel, at + 1, name, win)
    return errs


def litmus_files(root: Path) -> list[Path]:
    d = root / "tests" / "litmus"
    if not d.is_dir():
        return []
    return sorted(p for p in d.rglob("*.litmus") if p.is_file())


def _model(text: str) -> str | None:
    if AARCH.search(text):
        return "aarch64.cat"
    if X86.search(text):
        return "x86tso.cat"
    return None


def twin_base(name: str) -> str | None:
    """`foo.litmus` for a twin named `foo_relaxed.litmus` or `foo_no_x.litmus`."""
    m = TWIN.match(name)
    return None if m is None else m.group("base") + ".litmus"


def _want_never(path: Path) -> bool:
    return twin_base(path.name) is None


# --- twin shape -----------------------------------------------------------

FENCE = re.compile(r"^(?:DMB|DSB|ISB)\b")
_ATOMIC = re.compile(
    r"^(LDADD|LDCLR|LDEOR|LDSET|LDSMAX|LDSMIN|LDUMAX|LDUMIN|SWP|CAS|CASP)(AL|A|L)?([BH]?)$"
)
_ACQ_REL = {
    "LDAR": "LDR",
    "LDAPR": "LDR",
    "STLR": "STR",
    "LDAXR": "LDXR",
    "STLXR": "STXR",
    "LDAXP": "LDXP",
    "STLXP": "STXP",
}
_SUBSETS = {"AL": ("A", "L", ""), "A": ("",), "L": ("",)}


def weaker_ops(op: str) -> set[str]:
    """Opcodes that are `op` with some or all of its ordering removed."""
    op = op.upper()
    m = _ATOMIC.match(op)
    if m is not None:
        stem, order, size = m.group(1), m.group(2) or "", m.group(3)
        return {stem + o + size for o in _SUBSETS.get(order, ())}
    for strong, weak in _ACQ_REL.items():
        for size in ("", "B", "H"):
            if op == strong + size:
                return {weak + size}
    return set()


def _canon(cell: str) -> str:
    s = " ".join(cell.split()).upper()
    return re.sub(r"\s*,\s*", ",", s)


def parse_litmus(text: str) -> tuple[str, str, list[list[str]], str] | str:
    """(arch, init, per-thread instructions, condition), or an error."""
    text = re.sub(r"\(\*.*?\*\)", "", text, flags=re.S)
    lines = text.splitlines()
    if not lines or not lines[0].split():
        return "empty file"
    arch = lines[0].split()[0].upper()
    rest = "\n".join(lines[1:])
    m = re.match(r"\s*\{(.*?)\}(.*)$", rest, re.S)
    if m is None:
        return "no init block"
    init = re.sub(r"\s+", "", m.group(1))
    body = m.group(2)
    c = re.search(r"^\s*(?:~?exists|forall|locations|filter)\b", body, re.M)
    if c is None:
        return "no condition"
    cond = re.sub(r"\s+", "", body[c.start() :])
    rows = [r.strip() for r in body[: c.start()].splitlines() if r.strip()]
    if not rows:
        return "no program"
    head = [_canon(x) for x in rows[0].rstrip(";").split("|")]
    threads: list[list[str]] = [[] for _ in head]
    for r in rows[1:]:
        cells = r.rstrip(";").split("|")
        if len(cells) != len(head):
            return f"row `{r}` has {len(cells)} columns, not {len(head)}"
        for t, cell in enumerate(cells):
            if cell.strip():
                threads[t].append(_canon(cell))
    return arch, init, threads, cond


def _split(ins: str) -> tuple[str, str]:
    op, _, args = ins.partition(" ")
    return op, args


def _weakening(strong: str, weak: str) -> set[str]:
    """What weakening `strong` to `weak` drops: "acquire", "release", or both."""
    strong, weak = strong.upper(), weak.upper()
    m = _ATOMIC.match(strong)
    if m is not None:
        w = _ATOMIC.match(weak)
        kept = (w.group(2) or "") if w is not None else ""
        gone = set(m.group(2) or "") - set(kept)
        return {"acquire" if c == "A" else "release" for c in gone}
    if strong.startswith(("LDA", "LDAP")):
        return {"acquire"}
    return {"release"}


def _fence_kinds(ins: str) -> set[str]:
    """What deleting fence `ins` drops: a load fence is an acquire, a store
    fence a release, a full fence either, an `ISB` neither."""
    op, _, arg = ins.partition(" ")
    if op == "ISB":
        return {"fence"}
    if arg.endswith("LD"):
        return {"fence", "acquire"}
    if arg.endswith("ST"):
        return {"fence", "release"}
    return {"fence", "acquire", "release"}


# A twin's name suffix, and the kind every one of its changes must allow.
NAMED_TWINS = {"_no_release": "release", "_no_acquire": "acquire", "_no_fence": "fence"}


def twin_changes(base: str, twin: str) -> list[set[str]] | str:
    """For each barrier `twin` removes from `base`, the kinds that change
    may count as ("release", "acquire", "fence"); or why `twin` is not
    `base` with barriers removed."""
    b = parse_litmus(base)
    t = parse_litmus(twin)
    if isinstance(b, str):
        return f"test: {b}"
    if isinstance(t, str):
        return f"twin: {t}"
    if b[0] != t[0]:
        return f"twin is {t[0]}, its test {b[0]}"
    if b[1] != t[1]:
        return "twin's init block differs"
    if b[3] != t[3]:
        return "twin's condition differs"
    if len(b[2]) != len(t[2]):
        return f"twin has {len(t[2])} threads, its test {len(b[2])}"
    changes: list[set[str]] = []
    for n, (bs, ts) in enumerate(zip(b[2], t[2], strict=True)):
        i = j = 0
        while i < len(bs) or j < len(ts):
            if i < len(bs) and j < len(ts):
                if bs[i] == ts[j]:
                    i += 1
                    j += 1
                    continue
                bop, bargs = _split(bs[i])
                top, targs = _split(ts[j])
                if bargs == targs and top in weaker_ops(bop):
                    changes.append(_weakening(bop, top))
                    i += 1
                    j += 1
                    continue
            if i < len(bs) and FENCE.match(bs[i]):
                changes.append(_fence_kinds(bs[i]))
                i += 1
                continue
            got = ts[j] if j < len(ts) else "(end)"
            want = bs[i] if i < len(bs) else "(end)"
            return f"P{n}: twin's `{got}` is not `{want}` with a barrier removed"
    if not changes:
        return "twin keeps every barrier of its test"
    return changes


def twin_diff_error(base: str, twin: str, name: str = "") -> str | None:
    """None when `twin` is `base` with one or more barriers removed, each of
    the kind the twin's file `name` says (`NAMED_TWINS`)."""
    got = twin_changes(base, twin)
    if isinstance(got, str):
        return got
    stem = name.removesuffix(".litmus")
    for suffix, kind in NAMED_TWINS.items():
        if stem.endswith(suffix) and any(kind not in c for c in got):
            return f"a {suffix} twin removes a barrier that is not of its kind ({kind})"
    return None


def check_twins(root: Path = ROOT) -> list[str]:
    """Each AArch64 test has a twin; each twin has a test and, on AArch64,
    differs from it only in barriers."""
    errs: list[str] = []
    files = litmus_files(root)
    by_path = {p: p.read_text(encoding="utf-8") for p in files}
    twinned: set[Path] = set()
    for path, text in by_path.items():
        rel = path.relative_to(root).as_posix()
        base = twin_base(path.name)
        if base is None:
            continue
        bpath = path.with_name(base)
        if bpath not in by_path:
            errs.append(f"{rel}: barrier-removed twin of missing {base}")
            continue
        twinned.add(bpath)
        if _model(by_path[bpath]) == "aarch64.cat" or _model(text) == "aarch64.cat":
            err = twin_diff_error(by_path[bpath], text, path.name)
            if err is not None:
                errs.append(f"{rel}: {err}")
    for path, text in by_path.items():
        if twin_base(path.name) is None and _model(text) == "aarch64.cat" and path not in twinned:
            rel = path.relative_to(root).as_posix()
            stem = path.name.removesuffix(".litmus")
            errs.append(
                f"{rel}: no barrier-removed twin "
                f"({stem}_relaxed.litmus or {stem}_no_<barrier>.litmus)"
            )
    return errs


def observation_error(rel: str, model: str, got: str) -> str | None:
    """None when `got` is the verdict `rel` must have under `model`."""
    if _want_never(Path(rel)):
        if got != "Never":
            return f"{rel}: {model} allowed a forbidden outcome ({got})"
        return None
    if got == "Never":
        return f"{rel}: barrier-removed twin forbids its bad outcome"
    return None


def run_herd7(root: Path) -> list[str]:
    """Run herd7 on every litmus file. herd7 must be on PATH."""
    if which("herd7") is None:
        return [
            "litmus: herd7 not installed (opam install herdtools7.<the version "
            ".github/workflows/nightly.yml pins>)"
        ]
    errs: list[str] = []
    files = litmus_files(root)
    if not files:
        return ["litmus: no tests/litmus/*.litmus files"]
    for path in files:
        rel = path.relative_to(root).as_posix()
        text = path.read_text(encoding="utf-8")
        model = _model(text)
        if model is None:
            errs.append(f"{rel}: first line is not AArch64 or X86 / X86_64")
            continue
        r = subprocess.run(
            ["herd7", "-model", model, str(path)],
            capture_output=True,
            text=True,
            check=False,
        )
        out = r.stdout + r.stderr
        if r.returncode != 0:
            errs.append(f"{rel}: herd7 exit {r.returncode}: {out.strip()[:200]}")
            continue
        m = OBS.search(out)
        if m is None:
            errs.append(f"{rel}: herd7 printed no Observation line")
            continue
        got = m.group(1)
        err = observation_error(rel, model, got)
        if err is not None:
            errs.append(err)
    return errs


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    run = False
    if args == ["--run"]:
        run = True
    elif args:
        print("usage: check_litmus.py [--run]", file=sys.stderr)
        return 2
    errs = check_helpers() + check_sites() + check_twins()
    if run:
        errs += run_herd7(ROOT)
    for e in errs:
        print(e, file=sys.stderr)
    if errs:
        print(f"check_litmus: {len(errs)} failure(s)", file=sys.stderr)
        return 1
    print("check_litmus: ok" + (" (herd7)" if run else ""))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
