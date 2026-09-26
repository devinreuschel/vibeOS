"""Host side of the volatile-cache device (ROADMAP §10.2, F080; DESIGN §8.3).

`nbd-cache` (tests/hostlib/src/bin/nbd_cache.rs) serves the crash test's
virtio-blk disk over NBD and writes a JSONL trace: `{"t":"write","id","off",
"len"}` and `{"t":"flush","id"}` when a request arrives, `{"t":"reply","id"}`
after it sends a reply (reads included). `<trace>.data` holds the payloads of
the `write` records, concatenated in trace order.

A write is durable once the device has completed (replied to) a flush it
received after replying to that write. A kill of QEMU loses no write the
device received, so only images rebuilt here from the trace can show a lost
unflushed write: one per superblock write (the writes durable when it
arrived, plus that write) and one at the kill (the durable writes plus a
seeded subset of the later ones).

The oracle: `/w` in a rebuilt image holds the content of one crash-loop
iteration (`ITER_BYTES` bytes of `(N + k) as u8`), no older than the last
iteration whose commit's final flush the device replied before the image's
crash point.

Standard library only.
"""

from __future__ import annotations

import json
import os
import random
import select
import signal
import subprocess
import time
from collections.abc import Iterator
from dataclasses import dataclass, field
from types import TracebackType

from tests.harness.harness import NBD_CACHE_MODES, HarnessError

BLOCK = 4096
SUPER_SLOTS = (0, 4096)
ITER_BYTES = 300
KILL_COMMIT_MAX = 200
CACHE_MODES = NBD_CACHE_MODES
WR_PREFIX = "vibeOS: vibefs: wr "
# VIBEFS §4: the superblock's generation is a u64 LE at byte 16 of its slot.
SUPER_GEN_OFF = 16
LISTENING = "nbd-cache: listening"


@dataclass
class Write:
    id: int
    index: int
    off: int
    len: int
    data_off: int
    reply: int | None = None


@dataclass
class Flush:
    id: int
    index: int
    reply: int | None = None


@dataclass
class Trace:
    """A parsed trace. `index` is a record's position in the file."""

    nrecords: int = 0
    writes: list[Write] = field(default_factory=list)
    flushes: list[Flush] = field(default_factory=list)
    data: bytes = b""

    def payload(self, w: Write) -> bytes:
        return self.data[w.data_off : w.data_off + w.len]


def _int(rec: dict[str, object], key: str, where: str) -> int:
    v = rec.get(key)
    if not isinstance(v, int) or isinstance(v, bool) or v < 0:
        raise HarnessError(f"{where}: bad {key!r} in {rec!r}")
    return v


def load_trace(path: str) -> Trace:
    """Parse `path` and `path.data`; validate records, ids and the data size."""
    tr = Trace()
    by_id: dict[int, Write | Flush] = {}
    replied: set[int] = set()
    data_off = 0
    with open(path, encoding="utf-8") as f:
        lines = f.read().splitlines()
    for index, line in enumerate(lines):
        where = f"{path}:{index + 1}"
        try:
            rec = json.loads(line)
        except json.JSONDecodeError as e:
            raise HarnessError(f"{where}: not JSON: {line!r}") from e
        if not isinstance(rec, dict):
            raise HarnessError(f"{where}: not an object: {line!r}")
        t = rec.get("t")
        rid = _int(rec, "id", where)
        if t == "write" or t == "flush":
            if rid in by_id:
                raise HarnessError(f"{where}: duplicate id {rid}")
            if t == "write":
                n = _int(rec, "len", where)
                w = Write(rid, index, _int(rec, "off", where), n, data_off)
                data_off += n
                tr.writes.append(w)
                by_id[rid] = w
            else:
                fl = Flush(rid, index)
                tr.flushes.append(fl)
                by_id[rid] = fl
        elif t == "reply":
            if rid in replied:
                raise HarnessError(f"{where}: second reply for id {rid}")
            replied.add(rid)
            req = by_id.get(rid)
            if req is not None:
                req.reply = index
        else:
            raise HarnessError(f"{where}: unknown record type {t!r}")
    tr.nrecords = len(lines)
    with open(path + ".data", "rb") as f:
        tr.data = f.read()
    if len(tr.data) != data_off:
        raise HarnessError(
            f"{path}.data holds {len(tr.data)} bytes; the write records need {data_off}"
        )
    return tr


def durable_writes(trace: Trace, index: int) -> list[int]:
    """Ids of the writes durable at record `index`, in arrival order.

    A flush whose reply precedes `index` covers every write replied before
    that flush arrived; a write replied after it arrived is not covered.
    """
    cover = -1
    for fl in trace.flushes:
        if fl.reply is not None and fl.reply < index:
            cover = max(cover, fl.index)
    return [w.id for w in trace.writes if w.reply is not None and w.reply < cover]


def replay(base: bytes, trace: Trace, writes: list[int]) -> bytes:
    """`base` with the given writes applied in arrival order."""
    want = set(writes)
    img = bytearray(base)
    for w in trace.writes:
        if w.id in want:
            if w.off + w.len > len(img):
                raise HarnessError(f"write {w.id} ends past the image")
            img[w.off : w.off + w.len] = trace.payload(w)
    return bytes(img)


def _super_slots(w: Write) -> list[int]:
    """The superblock slots `w` covers. A partial overlap raises."""
    out = []
    for slot in SUPER_SLOTS:
        if w.off < slot + BLOCK and slot < w.off + w.len:
            if not (w.off <= slot and slot + BLOCK <= w.off + w.len):
                raise HarnessError(
                    f"write {w.id} [{w.off}, {w.off + w.len}) partly overlaps super slot {slot}"
                )
            out.append(slot)
    return out


def superblock_images(base: bytes, trace: Trace) -> Iterator[tuple[int, bytes]]:
    """One image per write that covers a superblock slot: the writes durable
    when it arrived, plus that write. Yields `(index of the write, image)`."""
    for w in trace.writes:
        if _super_slots(w):
            yield w.index, replay(base, trace, [*durable_writes(trace, w.index), w.id])


def kill_image(base: bytes, trace: Trace, rng: random.Random) -> bytes:
    """The image at the kill: every durable write plus a seeded subset (each
    kept with p = 1/2) of the later ones, in arrival order."""
    durable = durable_writes(trace, trace.nrecords)
    keep = set(durable)
    later = [w.id for w in trace.writes if w.id not in keep and rng.random() < 0.5]
    return replay(base, trace, durable + later)


def final_flushes(trace: Trace, fresh_gen: int) -> dict[int, int]:
    """Iteration -> index of the reply of its commit's final flush.

    A superblock write's iteration is its generation - `fresh_gen` - 1. Its
    final flush is the first flush that arrived after the write's reply.
    Iterations whose final flush was never replied are left out.
    """
    out: dict[int, int] = {}
    for w in trace.writes:
        slots = _super_slots(w)
        if not slots or w.reply is None:
            continue
        p = trace.payload(w)
        rel = slots[0] - w.off + SUPER_GEN_OFF
        gen = int.from_bytes(p[rel : rel + 8], "little")
        it = gen - fresh_gen - 1
        after = [fl for fl in trace.flushes if fl.index > w.reply]
        if not after or after[0].reply is None:
            continue
        out.setdefault(it, after[0].reply)
    return out


def floor_at(finals: dict[int, int], index: int) -> int:
    """The last iteration whose final flush was replied before `index`, or -1."""
    done = [it for it, rep in finals.items() if rep < index]
    return max(done, default=-1)


def iteration_bytes(n: int) -> bytes:
    return bytes((n + k) & 0xFF for k in range(ITER_BYTES))


def decode_iteration(content: bytes) -> int | None:
    """The iteration whose bytes `content` is, or None."""
    if len(content) != ITER_BYTES:
        return None
    n = content[0]
    return n if content == iteration_bytes(n) else None


def check_iteration(i: int | None, floor: int, allowed: tuple[int, ...] | None = None) -> None:
    if i is None:
        raise HarnessError("/w holds no iteration's content")
    if i < floor:
        raise HarnessError(f"/w holds iteration {i}, older than the durable floor {floor}")
    if allowed is not None and i not in allowed:
        raise HarnessError(f"/w holds iteration {i}, expected one of {allowed}")


def pick_kill_commit(rng: random.Random) -> int:
    return rng.randint(1, KILL_COMMIT_MAX)


def last_wr(lines: list[str]) -> int | None:
    n: int | None = None
    for ln in lines:
        if ln.startswith(WR_PREFIX):
            try:
                n = int(ln[len(WR_PREFIX) :].strip())
            except ValueError as e:
                raise HarnessError(f"bad wr line {ln!r}") from e
    return n


def check_harness_kill(exit_code: int | None, lines: list[str], k: int) -> int:
    """The last `wr N`. Raises unless QEMU died of the harness's SIGKILL with N >= K."""
    if exit_code != -signal.SIGKILL:
        raise HarnessError(f"QEMU exited {exit_code}, not by the harness's SIGKILL")
    n = last_wr(lines)
    if n is None or n < k:
        raise HarnessError(f"killed before wr {k} (last wr {n})")
    return n


class NbdCache:
    """Runs `nbd-cache`; entering waits up to 10 s for it to listen."""

    def __init__(self, tool: str, sock: str, image: str, trace: str, seed: int) -> None:
        self.argv = [
            tool,
            "--socket",
            sock,
            "--image",
            image,
            "--trace",
            trace,
            "--seed",
            str(seed),
        ]
        self.proc: subprocess.Popen[bytes] | None = None

    def __enter__(self) -> NbdCache:
        self.proc = subprocess.Popen(
            self.argv,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
        )
        assert self.proc.stdout is not None
        fd = self.proc.stdout.fileno()
        deadline = time.monotonic() + 10.0
        seen = b""
        while LISTENING.encode() not in seen:
            if time.monotonic() > deadline:
                self._kill()
                raise HarnessError(f"nbd-cache did not listen within 10 s: {seen!r}")
            r, _, _ = select.select([fd], [], [], 0.1)
            if r:
                chunk = os.read(fd, 4096)
                if not chunk:
                    code = self.proc.wait()
                    raise HarnessError(f"nbd-cache exited {code} before listening: {seen!r}")
                seen += chunk
        return self

    def _kill(self) -> None:
        if self.proc is not None and self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()

    def stop(self, timeout: float) -> int:
        """Waits for the server to exit (it does once QEMU has gone) and
        returns its exit code; kills it and raises after `timeout`."""
        assert self.proc is not None
        try:
            return self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired as e:
            self._kill()
            raise HarnessError(f"nbd-cache still running {timeout} s after QEMU") from e

    def __exit__(
        self,
        et: type[BaseException] | None,
        ev: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        self._kill()
        if self.proc is not None and self.proc.stdout is not None:
            self.proc.stdout.close()
