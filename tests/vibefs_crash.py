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
import select
import subprocess
import sys
import tempfile
import time

IMAGE_BYTES = 256 * 1024
WR_PREFIX = "vibeOS: vibefs: wr "
READY = "vibeOS: vibefs: crash-ready"
PANIC = (
    "panicked at",
    "vibeOS: panic:",
    "#PF",
    "#GP",
    "#UD",
    "#DF",
    "double fault",
    "stack overflow",
    "vibeOS: vibefs: mount fail",
)


class Fail(Exception):
    pass


def _qemu_argv(iso: str, disk: str) -> list[str]:
    smp = os.environ.get("VIBEOS_SMP", "2")
    cpu = os.environ.get("VIBEOS_QEMU_CPU", "max")
    mem = os.environ.get("VIBEOS_MEM", "128M")
    accel = os.environ.get("VIBEOS_QEMU_ACCEL", "tcg")
    argv = [
        "qemu-system-x86_64",
        "-cdrom",
        iso,
        "-m",
        mem,
        "-smp",
        smp,
        "-cpu",
        cpu,
        "-no-reboot",
        "-display",
        "none",
        "-serial",
        "stdio",
        "-boot",
        "order=d",
        "-drive",
        f"file={disk},if=none,id=vibehd,format=raw,cache=writeback",
        "-device",
        f"virtio-blk-pci,drive=vibehd,disable-legacy=on,num-queues={smp}",
    ]
    if accel:
        argv.extend(["-accel", accel])
    bios = os.environ.get("VIBEOS_BIOS")
    if bios:
        argv += ["-bios", bios]
    return argv


def _mkfs(mkfs: str, path: str) -> None:
    r = subprocess.run([mkfs, "-L", "crash", path], capture_output=True, text=True)
    if r.returncode != 0:
        raise Fail(f"mkfs-vibefs failed: {r.stderr or r.stdout}")


def _fsck(fsck: str, path: str) -> tuple[int, str]:
    r = subprocess.run([fsck, path], capture_output=True, text=True)
    out = (r.stdout or "") + (r.stderr or "")
    return r.returncode, out.strip()


def _one_round(
    iso: str,
    mkfs: str,
    fsck: str,
    rng: random.Random,
    timeout_s: float,
) -> str:
    fd, disk = tempfile.mkstemp(prefix="vibeos-vibefs-", suffix=".img")
    try:
        os.ftruncate(fd, IMAGE_BYTES)
        os.close(fd)
        fd = -1
        _mkfs(mkfs, disk)
        code, out = _fsck(fsck, disk)
        if code != 0:
            raise Fail(f"fresh image not clean: {out}")

        argv = _qemu_argv(iso, disk)
        proc = subprocess.Popen(
            argv,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
        )
        assert proc.stdout is not None
        deadline = time.monotonic() + timeout_s
        buf = b""
        lines: list[str] = []
        saw_ready = False
        wr = -1
        kill_after: float | None = None
        try:
            while time.monotonic() < deadline:
                if kill_after is not None and time.monotonic() >= kill_after:
                    break
                remain = max(0.0, deadline - time.monotonic())
                if kill_after is not None:
                    remain = min(remain, max(0.0, kill_after - time.monotonic()))
                r, _, _ = select.select([proc.stdout], [], [], min(remain, 0.05))
                if proc.poll() is not None and not r:
                    break
                if not r:
                    continue
                chunk = os.read(proc.stdout.fileno(), 4096)
                if not chunk:
                    break
                buf += chunk
                while b"\n" in buf:
                    raw, buf = buf.split(b"\n", 1)
                    line = raw.decode("utf-8", errors="replace").rstrip("\r")
                    lines.append(line)
                    for sig in PANIC:
                        if sig in line:
                            raise Fail(f"panic/fail line: {line}")
                    if line == READY:
                        saw_ready = True
                    if line.startswith(WR_PREFIX):
                        try:
                            wr = int(line[len(WR_PREFIX) :].strip())
                        except ValueError:
                            wr = 0
                        if kill_after is None:
                            # 0–180ms after a wr marker: often mid-write/Flush.
                            delay = rng.uniform(0.0, 0.18)
                            kill_after = time.monotonic() + delay
            if not saw_ready:
                tail = lines[-8:] if lines else []
                raise Fail(f"no crash-ready (qemu died={proc.poll()} tail={tail})")
            if wr < 0:
                raise Fail("saw crash-ready but no wr marker")
        finally:
            if proc.poll() is None:
                proc.kill()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=2)

        code, out = _fsck(fsck, disk)
        if "fsck-vibefs:" not in out and code != 0:
            raise Fail(f"fsck did not run: code={code} out={out!r}")
        # CoW live tree must checksum. errors>0 means a super pointed at
        # torn metadata — that is silent-propagation's cousin, and a fail.
        if code != 0:
            raise Fail(f"fsck errors after kill wr={wr}: {out}")
        if "errors 0" not in out:
            raise Fail(f"fsck did not report errors 0: {out}")
        return f"wr={wr} {out}"
    finally:
        if fd >= 0:
            try:
                os.close(fd)
            except OSError:
                pass
        try:
            os.unlink(disk)
        except OSError:
            pass


def main() -> int:
    iso = os.environ.get("VIBEOS_ISO", "vibeos-vibefs-crash.iso")
    mkfs = os.environ.get("VIBEOS_MKFS", "mkfs-vibefs")
    fsck = os.environ.get("VIBEOS_FSCK", "fsck-vibefs")
    rounds = int(os.environ.get("VIBEOS_CRASH_ROUNDS", "8"))
    timeout = float(os.environ.get("VIBEOS_TIMEOUT", "90"))
    seed = int(os.environ.get("VIBEOS_CRASH_SEED", str(int(time.time()) & 0xFFFFFFFF)))
    if not os.path.exists(iso):
        print(f"[vibefs-crash] FAIL: ISO missing {iso}", file=sys.stderr)
        return 1
    rng = random.Random(seed)
    print(f"[vibefs-crash] seed={seed} rounds={rounds}", file=sys.stderr)
    try:
        for i in range(rounds):
            msg = _one_round(iso, mkfs, fsck, rng, timeout)
            print(f"[vibefs-crash] ok round {i + 1}/{rounds} {msg}", file=sys.stderr)
    except Fail as e:
        print(f"[vibefs-crash] FAIL: {e}", file=sys.stderr)
        return 1
    print("[vibefs-crash] ok", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
