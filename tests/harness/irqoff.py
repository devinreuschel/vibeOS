"""The IF-off tracer's lines (ROADMAP §10.3, TESTING.md §8.2).

An `irqoff` kernel build prints `vibeOS: irqoff: ...` lines: the bound it
checks, and per site the IF=0 stretches it measured (INVARIANTS.md §2.9
rule 2). Every harness launch function hands its boot's lines to
`observe`, which does nothing unless the boot printed such a line. For one
that did, it appends a table to `$GITHUB_STEP_SUMMARY` when that is set and
stores the sites in the tier's results file as its `irqoff` section
(C-RESULTS), which the §10.9 history keeps.

Line forms (tests/contract/markers.toml, §10.3); `<site>` has no space and
a `<reason>` comes last:

    vibeOS: irqoff: on bound <n> ns
    vibeOS: irqoff: no reporter
    vibeOS: irqoff: over <site> n <n> max <n> ns
    vibeOS: irqoff: site <site> n <n> over <n> max <n> ns p99 <n> ns
    vibeOS: irqoff: deliberate <site> n <n> max <n> ns <reason>
    vibeOS: irqoff: unmatched <site> n <n>
    vibeOS: irqoff: dropped <n>

Every count is cumulative since boot, so within one boot the last line of
each kind for a site wins. Standard library only.
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass, field
from typing import Any

from tests.harness.harness import HarnessError, QemuConfig, effective_accel_name

PREFIX = "vibeOS: irqoff: "
TIER = "test-irqoff"

_FORMS: dict[str, re.Pattern[str]] = {
    "on": re.compile(r"on bound (\d+) ns"),
    "no_reporter": re.compile(r"no reporter"),
    "over": re.compile(r"over (\S+) n (\d+) max (\d+) ns"),
    "site": re.compile(r"site (\S+) n (\d+) over (\d+) max (\d+) ns p99 (\d+) ns"),
    "deliberate": re.compile(r"deliberate (\S+) n (\d+) max (\d+) ns ?(.*)"),
    "unmatched": re.compile(r"unmatched (\S+) n (\d+)"),
    "dropped": re.compile(r"dropped (\d+)"),
}


@dataclass(frozen=True)
class Record:
    """One parsed line: its kind, its site (or ""), and its numbers."""

    kind: str
    site: str = ""
    nums: tuple[int, ...] = ()
    reason: str = ""


def parse(line: str) -> Record | None:
    """The record of one serial line, or None when it is not an irqoff line."""
    start = line.find(PREFIX)
    if start < 0:
        return None
    rest = line[start + len(PREFIX) :].rstrip("\r\n")
    # A line glued to the next kernel line ends at that line's prefix.
    glued = rest.find("vibeOS:")
    if glued >= 0:
        rest = rest[:glued]
    rest = rest.strip()
    for kind, pat in _FORMS.items():
        m = pat.fullmatch(rest)
        if m is None:
            continue
        groups = m.groups()
        if kind in ("on", "dropped"):
            return Record(kind, nums=(int(groups[0]),))
        if kind == "no_reporter":
            return Record(kind)
        if kind == "deliberate":
            return Record(kind, groups[0], tuple(int(g) for g in groups[1:3]), groups[3].strip())
        return Record(kind, groups[0], tuple(int(g) for g in groups[1:]))
    return None


@dataclass
class SiteRow:
    """One site in one boot."""

    site: str
    n: int = 0
    over: int = 0
    deliberate: int = 0
    max_ns: int = 0
    p99_ns: int = 0
    unmatched: int = 0
    deliberate_max_ns: int = 0
    reason: str = ""


@dataclass
class Boot:
    """What one boot printed."""

    bound_ns: int | None = None
    reporter: bool = True
    dropped: int = 0
    sites: dict[str, SiteRow] = field(default_factory=dict)

    def add(self, rec: Record) -> None:
        if rec.kind == "on":
            self.bound_ns = rec.nums[0]
            return
        if rec.kind == "no_reporter":
            self.reporter = False
            return
        if rec.kind == "dropped":
            self.dropped = max(self.dropped, rec.nums[0])
            return
        row = self.sites.setdefault(rec.site, SiteRow(rec.site))
        if rec.kind == "site":
            row.n, row.over, row.max_ns, row.p99_ns = rec.nums
        elif rec.kind == "over":
            row.over = max(row.over, rec.nums[0])
            row.max_ns = max(row.max_ns, rec.nums[1])
        elif rec.kind == "deliberate":
            row.deliberate, row.deliberate_max_ns = rec.nums
            row.reason = rec.reason
        elif rec.kind == "unmatched":
            row.unmatched = rec.nums[0]


@dataclass
class Report:
    """Every boot of one tier, in order."""

    accel: str = "tcg"
    boots: list[Boot] = field(default_factory=list)

    def add_boot(self, lines: list[str]) -> Boot | None:
        """Parse one boot's lines; None (and nothing added) without irqoff lines."""
        boot = Boot()
        seen = False
        for line in lines:
            rec = parse(line)
            if rec is None:
                continue
            seen = True
            boot.add(rec)
        if not seen:
            return None
        self.boots.append(boot)
        return boot

    def merged(self) -> dict[str, SiteRow]:
        """Per site over all boots: counts summed, maxima and p99 the largest."""
        out: dict[str, SiteRow] = {}
        for boot in self.boots:
            for s in boot.sites.values():
                m = out.setdefault(s.site, SiteRow(s.site))
                m.n += s.n
                m.over += s.over
                m.deliberate += s.deliberate
                m.unmatched += s.unmatched
                m.max_ns = max(m.max_ns, s.max_ns)
                m.p99_ns = max(m.p99_ns, s.p99_ns)
                m.deliberate_max_ns = max(m.deliberate_max_ns, s.deliberate_max_ns)
                m.reason = m.reason or s.reason
        return out

    def rows(self) -> list[dict[str, Any]]:
        """The results file's `irqoff` rows, one per site and boot."""
        rows: list[dict[str, Any]] = []
        for i, boot in enumerate(self.boots):
            for s in sorted(boot.sites.values(), key=lambda r: r.site):
                rows.append(
                    {
                        "site": s.site,
                        "n": s.n,
                        "over": s.over,
                        "deliberate": s.deliberate,
                        "max_ns": s.max_ns,
                        "p99_ns": s.p99_ns,
                        "unmatched": s.unmatched,
                        "accel": self.accel,
                        "boot": i,
                    }
                )
        return rows


def summary_markdown(report: Report, accel: str) -> str:
    """The job-summary table. TCG: the sites over the bound, then the totals.
    KVM: every site's max and p99, with no threshold (its host can
    deschedule a vCPU mid-stretch)."""
    merged = report.merged()
    bound = next((b.bound_ns for b in report.boots if b.bound_ns is not None), None)
    out: list[str] = []
    if accel == "kvm":
        out.append(f"### IF-off stretches under KVM ({len(merged)} sites, no threshold)")
        out.append("")
        out.append("| site | n | max ns | p99 ns |")
        out.append("|---|---:|---:|---:|")
        for s in sorted(merged.values(), key=lambda r: (-r.max_ns, r.site)):
            out.append(f"| `{s.site}` | {s.n} | {s.max_ns} | {s.p99_ns} |")
        return "\n".join(out) + "\n"
    over = sorted((s for s in merged.values() if s.over), key=lambda r: (-r.max_ns, r.site))
    shown = f"{bound:,}" if bound is not None else "100,000"
    out.append(f"### Logged sites (over {shown} ns)")
    out.append("")
    if over:
        out.append("| site | over | n | max ns | p99 ns |")
        out.append("|---|---:|---:|---:|---:|")
        for s in over:
            out.append(f"| `{s.site}` | {s.over} | {s.n} | {s.max_ns} | {s.p99_ns} |")
    else:
        out.append("None.")
    out.append("")
    stretches = sum(s.n for s in merged.values())
    deliberate = sum(s.deliberate for s in merged.values())
    unmatched = sorted(s.site for s in merged.values() if s.unmatched)
    dropped = sum(b.dropped for b in report.boots)
    out.append(
        f"Totals: {len(report.boots)} boots, {len(merged)} sites, {stretches} stretches, "
        f"{len(over)} sites over, {deliberate} deliberate, {dropped} dropped."
    )
    if unmatched:
        out.append("")
        out.append("Unmatched: " + ", ".join(f"`{s}`" for s in unmatched))
    return "\n".join(out) + "\n"


_report: Report | None = None


def observe(cfg: QemuConfig, lines: list[str]) -> None:
    """Take one boot's lines (every harness launch function calls this).

    A no-op unless a line starts an irqoff line. On the `test-irqoff` tier a
    boot without the `on` line is the wrong ISO, and raises."""
    from tests.harness import results

    tier = results.current().tier
    has = any(PREFIX in line for line in lines)
    if tier == TIER and not any(
        (r := parse(line)) is not None and r.kind == "on" for line in lines
    ):
        raise HarnessError(f"{TIER}: no '{PREFIX}on' line: this boot is not an irqoff build")
    if not has:
        return
    global _report
    accel = effective_accel_name(cfg)
    if _report is None:
        _report = Report(accel=accel)
    before = len(_report.boots)
    _report.add_boot(lines)
    if len(_report.boots) == before:
        return
    single = Report(accel=accel, boots=[_report.boots[-1]])
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write(f"\n#### `{tier}` boot {before}\n\n")
            f.write(summary_markdown(single, accel))
    rows = single.rows()
    for row in rows:
        row["boot"] = before
    results.current().add_section("irqoff", rows)
