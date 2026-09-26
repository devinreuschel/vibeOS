#!/usr/bin/env python3
"""QEMU-kill crash consistency for vibefs over the volatile-cache device.

docs/VIBEFS.md §12 item 2, ROADMAP §10.2 (F080), DESIGN §8.3.

Each round serves a fresh 256 KiB `mkfs-vibefs` image through `nbd-cache`
(a volatile write cache that replies to flushes late and traces every write,
flush and reply), boots the `vibefs_crash` kernel on it, and SIGKILLs QEMU
a jitter after `vibeOS: vibefs: wr K` for a random K in [1, 200]. A kill
loses no write the device received, so the round first checks that a replay
of the trace equals the served image, then rebuilds one image per superblock
write and one at the kill (`nbd_trace`). On each, `fsck-vibefs` must print
`errors 0 warnings 0`, and `vibefs-cat` must read `/w` as one iteration's
content no older than the last iteration whose final flush the device
replied before that image's crash point; the kill image must hold N or N-1
for the last `wr N`.
"""

from __future__ import annotations

import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass

from tests.harness import results
from tests.harness.harness import (
    EnvConfig,
    HarnessError,
    env_config,
    env_int,
    env_str,
    kill_delay,
    qemu_argv,
    run_qemu_until_exit,
    virtio_blk_args,
)
from tests.harness.nbd_trace import (
    CACHE_MODES,
    WR_PREFIX,
    NbdCache,
    check_harness_kill,
    check_iteration,
    decode_iteration,
    final_flushes,
    floor_at,
    kill_image,
    load_trace,
    pick_kill_commit,
    replay,
    superblock_images,
)

# The image `vibefs::tests::crash_workload_seeded_points` proves 200 commits fit.
IMAGE_BYTES = 256 * 1024
READY = "vibeOS: vibefs: crash-ready"
MOUNT_FAIL = "vibeOS: vibefs: mount fail"
SYNC_FAIL = "vibeOS: vibefs: sync fail"
CLEAN = "errors 0 warnings 0"
_FSCK_LINE = re.compile(r"fsck-vibefs: gen (\d+) errors (\d+) warnings (\d+)")


@dataclass
class Tools:
    mkfs: str
    fsck: str
    nbd: str
    cat: str


def _run(argv: list[str]) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(argv, capture_output=True)


def _mkfs(tools: Tools, path: str) -> None:
    with open(path, "wb") as f:
        f.truncate(IMAGE_BYTES)
    r = _run([tools.mkfs, "-L", "crash", path])
    if r.returncode != 0:
        raise HarnessError(f"mkfs-vibefs failed: {(r.stderr or r.stdout).decode(errors='replace')}")


def _fsck_clean(tools: Tools, path: str) -> int:
    """Runs fsck-vibefs; returns the generation, raising unless it is clean."""
    r = _run([tools.fsck, path])
    out = (r.stdout + r.stderr).decode(errors="replace").strip()
    m = _FSCK_LINE.search(out)
    if r.returncode != 0 or m is None or CLEAN not in out:
        raise HarnessError(f"fsck-vibefs exit {r.returncode}: {out!r}")
    return int(m.group(1))


def _cat_iteration(tools: Tools, path: str) -> int | None:
    r = _run([tools.cat, path, "/w"])
    if r.returncode != 0:
        raise HarnessError(f"vibefs-cat /w: {r.stderr.decode(errors='replace').strip()}")
    return decode_iteration(r.stdout)


def _one_round(env: EnvConfig, tools: Tools, rng: random.Random, round: int) -> str:
    """One kill and its checks. Raises HarnessError naming the failing image
    (`super@<index>` or `kill`); on failure the round's directory is kept."""
    # Short: a unix socket path is at most 104 bytes on macOS.
    d = tempfile.mkdtemp(prefix="vnbd-")
    ok = False
    try:
        base_path = os.path.join(d, "base.img")
        work = os.path.join(d, "work.img")
        sock = os.path.join(d, "s")
        trace_path = os.path.join(d, "trace")
        _mkfs(tools, base_path)
        fresh_gen = _fsck_clean(tools, base_path)
        shutil.copyfile(base_path, work)

        seed = rng.getrandbits(32)
        k = pick_kill_commit(rng)
        jitter = kill_delay(rng)
        cache = CACHE_MODES[round % len(CACHE_MODES)]
        want = f"{WR_PREFIX}{k}"
        with NbdCache(tools.nbd, sock, work, trace_path, seed) as srv:
            cfg = env.qemu(
                extra=virtio_blk_args(sock, env.smp, nbd=True, cache=cache),
                boot_order="d",
                extra_panic=(MOUNT_FAIL, SYNC_FAIL),
            )
            try:
                raw = run_qemu_until_exit(
                    cfg,
                    timeout_s=env.timeout,
                    kill_after=lambda line: jitter if line.strip() == want else None,
                )
            except HarnessError:
                results.current().add_boot(qemu_argv(cfg, None), cfg, None)
                raise
            results.current().add_boot(qemu_argv(cfg, None), cfg, raw.exit_code)
            if READY not in raw.lines:
                raise HarnessError(f"no crash-ready (exit {raw.exit_code}, tail {raw.lines[-8:]})")
            n = check_harness_kill(raw.exit_code, raw.lines, k)
            code = srv.stop(10.0)
            if code != 0:
                raise HarnessError(f"nbd-cache exited {code}")

        tr = load_trace(trace_path)
        with open(base_path, "rb") as f:
            base = f.read()
        with open(work, "rb") as f:
            served = f.read()
        if replay(base, tr, [w.id for w in tr.writes]) != served:
            raise HarnessError("a replay of the trace differs from the served image")

        finals = final_flushes(tr, fresh_gen)
        images: list[tuple[str, bytes, int, tuple[int, ...] | None]] = [
            (f"super@{i}", img, floor_at(finals, i), None) for i, img in superblock_images(base, tr)
        ]
        kill_floor = floor_at(finals, tr.nrecords)
        # `wr N` follows the return of iteration N-1's sync_fs, so the device
        # replied that commit's final flush before the kill.
        if kill_floor < n - 1:
            raise HarnessError(
                f"the trace shows no replied final flush for iteration {n - 1} "
                f"(floor {kill_floor}, K={k} N={n} cache={cache})"
            )
        images.append(("kill", kill_image(base, tr, rng), kill_floor, (n - 1, n)))
        check = os.path.join(d, "check.img")
        for label, img, floor, allowed in images:
            with open(check, "wb") as f:
                f.write(img)
            try:
                _fsck_clean(tools, check)
                check_iteration(_cat_iteration(tools, check), floor, allowed)
            except HarnessError as e:
                with open(os.path.join(d, f"{label}.img"), "wb") as f:
                    f.write(img)
                raise HarnessError(f"{label}: {e} (K={k} N={n} cache={cache})") from e
        ok = True
        return (
            f"K={k} N={n} cache={cache} images={len(images)} {CLEAN} "
            f"(writes {len(tr.writes)}, flushes {len(tr.flushes)})"
        )
    finally:
        if ok:
            shutil.rmtree(d, ignore_errors=True)
        else:
            print(f"[vibefs-crash] kept {d}", file=sys.stderr)


def main() -> int:
    env = env_config(default_iso="vibeos-vibefs-crash.iso", default_timeout=90)
    res = results.Results(env.tier)
    tools = Tools(
        mkfs=env_str("VIBEOS_MKFS", "mkfs-vibefs"),
        fsck=env_str("VIBEOS_FSCK", "fsck-vibefs"),
        nbd=env_str("VIBEOS_NBD_CACHE", "nbd-cache"),
        cat=env_str("VIBEOS_VIBEFS_CAT", "vibefs-cat"),
    )
    rounds = env_int("VIBEOS_CRASH_ROUNDS", 8)
    seed = env_int("VIBEOS_CRASH_SEED", int(time.time()) & 0xFFFFFFFF)
    rng = random.Random(seed)
    print(f"[vibefs-crash] seed={seed} rounds={rounds}", file=sys.stderr)
    for i in range(rounds):
        name = f"crash_round_{i + 1}"
        try:
            msg = _one_round(env, tools, rng, i)
        except HarnessError as e:
            res.record("marker", name, "failed")
            print(f"[vibefs-crash] FAIL seed={seed} round {i + 1}/{rounds}: {e}", file=sys.stderr)
            return 1
        res.record("marker", name, "passed")
        print(f"[vibefs-crash] ok round {i + 1}/{rounds} {msg}", file=sys.stderr)
    print("[vibefs-crash] ok", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(results.run_main(main))
