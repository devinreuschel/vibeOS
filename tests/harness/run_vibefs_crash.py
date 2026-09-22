#!/usr/bin/env python3
"""QEMU-kill crash consistency for vibefs. docs/VIBEFS.md §12 / ROADMAP §8.5.

Host CrashDisk (library tests) drops writes inside commit. This harness
mkfs's a virtio-blk image, boots the `vibefs_crash` kernel (write+fsync
loop), SIGKILLs QEMU at a randomized point after `vibeOS: vibefs: wr N`,
then runs host `fsck-vibefs`. fsck must run; a clean live tree (errors=0)
is required. Silent accept of a checksum-invalid tree is a fail.
"""

from __future__ import annotations

import os
import random
import subprocess
import sys
import time

from tests.harness.harness import (
    EnvConfig,
    HarnessError,
    env_config,
    env_int,
    env_str,
    kill_delay,
    make_disk,
    run_qemu_until_exit,
    virtio_blk_args,
)


IMAGE_BYTES = 256 * 1024
WR_PREFIX = "vibeOS: vibefs: wr "
READY = "vibeOS: vibefs: crash-ready"
MOUNT_FAIL = "vibeOS: vibefs: mount fail"


def _mkfs(mkfs: str, path: str) -> None:
    r = subprocess.run([mkfs, "-L", "crash", path], capture_output=True, text=True)
    if r.returncode != 0:
        raise HarnessError(f"mkfs-vibefs failed: {r.stderr or r.stdout}")


def _fsck(fsck: str, path: str) -> tuple[int, str]:
    r = subprocess.run([fsck, path], capture_output=True, text=True)
    out = (r.stdout or "") + (r.stderr or "")
    return r.returncode, out.strip()


def _wr_line(line: str, rng: random.Random) -> float | None:
    if line.startswith(WR_PREFIX):
        return kill_delay(rng)
    return None


def _one_round(
    env: EnvConfig,
    mkfs: str,
    fsck: str,
    rng: random.Random,
) -> str:
    disk = make_disk(IMAGE_BYTES, "vibeos-vibefs-")
    try:
        _mkfs(mkfs, disk)
        code, out = _fsck(fsck, disk)
        if code != 0:
            raise HarnessError(f"fresh image not clean: {out}")

        cfg = env.qemu(
            extra=virtio_blk_args(disk, env.smp, discard=False),
            boot_order="d",
            extra_panic=(MOUNT_FAIL,),
        )
        raw = run_qemu_until_exit(
            cfg,
            timeout_s=env.timeout,
            kill_after=lambda line: _wr_line(line, rng),
        )
        if not any(ln == READY for ln in raw.lines):
            tail = raw.lines[-8:] if raw.lines else []
            raise HarnessError(
                f"no crash-ready (qemu died={raw.exit_code} tail={tail})"
            )
        wr = -1
        for ln in raw.lines:
            if ln.startswith(WR_PREFIX):
                try:
                    wr = int(ln[len(WR_PREFIX) :].strip())
                except ValueError:
                    wr = 0
                break
        if wr < 0:
            raise HarnessError("saw crash-ready but no wr marker")

        code, out = _fsck(fsck, disk)
        if "fsck-vibefs:" not in out and code != 0:
            raise HarnessError(f"fsck did not run: code={code} out={out!r}")
        # CoW live tree must checksum. errors>0 means a super pointed at
        # torn metadata — that is silent-propagation's cousin, and a fail.
        if code != 0:
            raise HarnessError(f"fsck errors after kill wr={wr}: {out}")
        if "errors 0" not in out:
            raise HarnessError(f"fsck did not report errors 0: {out}")
        return f"wr={wr} {out}"
    finally:
        try:
            os.unlink(disk)
        except OSError:
            pass


def main() -> int:
    env = env_config(default_iso="vibeos-vibefs-crash.iso", default_timeout=90)
    mkfs = env_str("VIBEOS_MKFS", "mkfs-vibefs")
    fsck = env_str("VIBEOS_FSCK", "fsck-vibefs")
    rounds = env_int("VIBEOS_CRASH_ROUNDS", 8)
    seed = env_int("VIBEOS_CRASH_SEED", int(time.time()) & 0xFFFFFFFF)
    try:
        rng = random.Random(seed)
        print(f"[vibefs-crash] seed={seed} rounds={rounds}", file=sys.stderr)
        for i in range(rounds):
            msg = _one_round(env, mkfs, fsck, rng)
            print(f"[vibefs-crash] ok round {i + 1}/{rounds} {msg}", file=sys.stderr)
    except HarnessError as e:
        print(f"[vibefs-crash] FAIL: {e}", file=sys.stderr)
        return 1
    print("[vibefs-crash] ok", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
