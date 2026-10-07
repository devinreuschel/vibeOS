"""The per-push shards of the in-guest tiers (ROADMAP §10.1, TESTING.md §8.6).

`make test-kernel`, `make test-kernel-smp4` and `make test-lapic-fallback`
each run the whole registry in one boot, then the proof boots
(`run_ktest.PROOF_BOOTS`): about 200 s of TCG each, over the 60 s a per-push
tier may take (`ci_history.py --tiers`). Each is split into shards, the
Makefile targets `<variant>-<k>`, each its own `ci.yml` tier with its own
check name. A shard runs one stretch of the registry in its main boot
(`vibeos.ktest_range=<from>..<to>`, BOOT.md §3.2), with the persist reboot
when that stretch holds `block_persist`, or a subset of the proof boots, in
the variant's configuration, which the Makefile gives the variant and its
shards alike (`KTEST_ENV`). A variant's shards hand on their bounds, the
first starting at the first row and the last running past the last, and
split its proof boots between them, so every registry row, a row added later
included, and every proof boot runs in exactly one per-push tier;
`test_ktest_shards.py` holds the table to that and to the Makefile.

The bounds and the grouping are chosen from a local TCG run of each variant
at the commit that split them (TESTING.md §8.6): each shard about 40 s, the
per-boot cost (QEMU start, boot, and `ktest::quiesce_frames`' warm-up, 11 to
13 s) included.
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class Variant:
    """A union target's configuration, as its Makefile recipe sets it."""

    smp: int
    # `run_ktest.py --hpet-off`: the hpet=off boot after the main boot.
    hpet_off: bool


@dataclass(frozen=True)
class Shard:
    variant: str
    # `(from, to)` of `vibeos.ktest_range=`, "" for the open end; None when
    # the shard has no main boot.
    rows: tuple[str, str] | None = None
    # Names from `run_ktest.PROOF_BOOTS`, run after the main boot.
    boots: tuple[str, ...] = ()
    # Proof boots only the aarch64 union runs. Kept off `boots` so the
    # x86 union, which shares this table, does not gain them.
    aarch64_boots: tuple[str, ...] = ()

    def range_word(self) -> str | None:
        """The main boot's `vibeos.ktest_range=` word, or None."""
        if self.rows is None:
            return None
        return f"vibeos.ktest_range={self.rows[0]}..{self.rows[1]}"


VARIANTS: dict[str, Variant] = {
    "test-kernel": Variant(smp=2, hpet_off=True),
    "test-kernel-smp4": Variant(smp=4, hpet_off=False),
    "test-lapic-fallback": Variant(smp=2, hpet_off=False),
}

# The bounds. `user_single_step` follows `preempt_gpr_canaries`, the
# registry's longest row (9 s at `-smp 2`); at `-smp 4` the sched and irq
# groups run longer and that row shorter, so the first stretch runs on to
# `spawn_sentinel`, the sched group's first row. `block_vblk_rw` starts the
# drivers group, so the last stretch holds every row on `vda` (drivers, fs)
# and the persist reboot, which reruns that stretch on the disk the first
# boot wrote.
def _rows(variant: str, cut: str) -> dict[str, Shard]:
    bounds = (("", cut), (cut, "block_vblk_rw"), ("block_vblk_rw", ""))
    return {f"{variant}-{k}": Shard(variant, rows=r) for k, r in enumerate(bounds, start=1)}


# aarch64 runs the portable groups plus its arch group (src/ktest/mod.rs).
# The cuts are rows of that registry. `block_vblk_rw` starts the drivers
# group, so the last stretch holds every row on `vda` and the persist reboot.
# x86's `user_single_step` cut is not used: that row skips, and the portable
# proc group is large enough to need its own stretches.
def _aarch64_spans(variant: str, cuts: tuple[str, ...]) -> dict[str, tuple[str, str]]:
    bounds = ("", *cuts, "")
    return {f"{variant}-{k}": (bounds[k - 1], bounds[k]) for k in range(1, len(bounds))}


_AARCH64_KERNEL = (
    "el0_uaccess",
    "tls_survive",
    "user_runtime",
    "spawn_sentinel",
    "block_vblk_rw",
)
_AARCH64_SMP4 = (
    "el0_uaccess",
    "user_runtime",
    "spawn_sentinel",
    "block_vblk_rw",
)

AARCH64_ROWS: dict[str, tuple[str, str]] = {
    **_aarch64_spans("test-kernel", _AARCH64_KERNEL),
    **_aarch64_spans("test-kernel-smp4", _AARCH64_SMP4),
    **_aarch64_spans("test-lapic-fallback", _AARCH64_KERNEL),
}


def rows_for(name: str, arch: str) -> tuple[str, str] | None:
    """The main-boot range of shard `name` on `arch`."""
    if arch == "aarch64" and name in AARCH64_ROWS:
        return AARCH64_ROWS[name]
    return SHARDS[name].rows


def range_word_for(name: str, arch: str) -> str | None:
    rows = rows_for(name, arch)
    if rows is None:
        return None
    return f"vibeos.ktest_range={rows[0]}..{rows[1]}"


SHARDS: dict[str, Shard] = {
    **_rows("test-kernel", "user_single_step"),
    "test-kernel-4": Shard("test-kernel", boots=("hpet-off", "select", "repeat")),
    "test-kernel-5": Shard("test-kernel", boots=("deadline-trip", "planted", "fat")),
    "test-kernel-6": Shard("test-kernel", boots=("vblk-readonly", "vblk-bad-sector")),
    **_rows("test-kernel-smp4", "spawn_sentinel"),
    "test-kernel-smp4-4": Shard("test-kernel-smp4", boots=("select", "deadline-trip", "planted")),
    "test-kernel-smp4-5": Shard(
        "test-kernel-smp4",
        boots=("fat", "vblk-readonly", "vblk-bad-sector", "stalled-ap"),
        aarch64_boots=("aff-off",),
    ),
    **_rows("test-lapic-fallback", "user_single_step"),
    "test-lapic-fallback-4": Shard(
        "test-lapic-fallback", boots=("select", "repeat", "deadline-trip")
    ),
    "test-lapic-fallback-5": Shard("test-lapic-fallback", boots=("planted", "fat")),
    "test-lapic-fallback-6": Shard(
        "test-lapic-fallback", boots=("vblk-readonly", "vblk-bad-sector")
    ),
}
