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


SHARDS: dict[str, Shard] = {
    **_rows("test-kernel", "user_single_step"),
    "test-kernel-4": Shard("test-kernel", boots=("hpet-off", "select", "repeat")),
    "test-kernel-5": Shard("test-kernel", boots=("deadline-trip", "planted", "fat")),
    "test-kernel-6": Shard("test-kernel", boots=("vblk-readonly", "vblk-bad-sector")),
    **_rows("test-kernel-smp4", "spawn_sentinel"),
    "test-kernel-smp4-4": Shard("test-kernel-smp4", boots=("select", "deadline-trip", "planted")),
    "test-kernel-smp4-5": Shard(
        "test-kernel-smp4", boots=("fat", "vblk-readonly", "vblk-bad-sector", "stalled-ap")
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
