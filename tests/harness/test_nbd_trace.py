"""Unit tests for tests/harness/nbd_trace.py (ROADMAP §10.2, F080).

Runs under `python3 -m unittest discover`. Standard-library only.
"""

from __future__ import annotations

import json
import os
import random
import signal
import tempfile
import unittest

from tests.harness.harness import HarnessError
from tests.harness.nbd_trace import (
    BLOCK,
    ITER_BYTES,
    KILL_COMMIT_MAX,
    SUPER_GEN_OFF,
    Trace,
    check_harness_kill,
    check_iteration,
    decode_iteration,
    durable_writes,
    final_flushes,
    floor_at,
    iteration_bytes,
    kill_image,
    load_trace,
    pick_kill_commit,
    replay,
    superblock_images,
)

IMG = 4 * BLOCK


class Builder:
    """Writes a synthetic trace and its `.data` sidecar."""

    def __init__(self) -> None:
        self.recs: list[dict[str, object]] = []
        self.data = b""
        self.next_id = 0

    def write(self, off: int, payload: bytes) -> int:
        i = self.next_id
        self.next_id += 1
        self.recs.append({"t": "write", "id": i, "off": off, "len": len(payload)})
        self.data += payload
        return i

    def flush(self) -> int:
        i = self.next_id
        self.next_id += 1
        self.recs.append({"t": "flush", "id": i})
        return i

    def reply(self, i: int) -> int:
        self.recs.append({"t": "reply", "id": i})
        return len(self.recs) - 1

    def wr(self, off: int, payload: bytes) -> int:
        i = self.write(off, payload)
        self.reply(i)
        return i

    def fl(self) -> int:
        """A flush replied at once; returns the reply's index."""
        return self.reply(self.flush())

    def save(self, d: str, data: bytes | None = None) -> str:
        p = os.path.join(d, "trace")
        with open(p, "w", encoding="utf-8") as f:
            for r in self.recs:
                f.write(json.dumps(r) + "\n")
        with open(p + ".data", "wb") as f:
            f.write(self.data if data is None else data)
        return p


def superblock(gen: int) -> bytes:
    b = bytearray(BLOCK)
    b[SUPER_GEN_OFF : SUPER_GEN_OFF + 8] = gen.to_bytes(8, "little")
    return bytes(b)


class NbdTraceTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.dir = self._td.name

    def tearDown(self) -> None:
        self._td.cleanup()

    def load(self, b: Builder) -> Trace:
        return load_trace(b.save(self.dir))

    def test_parse_rejects_unknown_type(self) -> None:
        b = Builder()
        b.recs.append({"t": "trim", "id": 0})
        with self.assertRaises(HarnessError):
            self.load(b)

    def test_parse_rejects_duplicate_id(self) -> None:
        b = Builder()
        b.wr(0, b"a")
        b.recs.append({"t": "flush", "id": 0})
        with self.assertRaises(HarnessError):
            self.load(b)

    def test_parse_rejects_short_data(self) -> None:
        b = Builder()
        b.wr(0, b"abcd")
        with self.assertRaises(HarnessError):
            load_trace(b.save(self.dir, data=b"abc"))

    def test_parse_accepts_read_replies(self) -> None:
        b = Builder()
        b.reply(99)  # a read: reply only
        b.wr(0, b"x")
        t = self.load(b)
        self.assertEqual(len(t.writes), 1)
        self.assertEqual(t.nrecords, 3)

    def test_write_replied_before_flush_is_durable_after_its_reply(self) -> None:
        b = Builder()
        w = b.wr(0, b"a")
        f = b.flush()
        r = b.reply(f)
        t = self.load(b)
        self.assertEqual(durable_writes(t, r), [])
        self.assertEqual(durable_writes(t, r + 1), [w])

    def test_write_arriving_during_pending_flush_is_not_covered(self) -> None:
        b = Builder()
        w1 = b.wr(0, b"a")
        f = b.flush()
        w2 = b.wr(BLOCK, b"b")
        b.reply(f)
        t = self.load(b)
        self.assertEqual(durable_writes(t, t.nrecords), [w1])
        self.assertNotIn(w2, durable_writes(t, t.nrecords))

    def test_unreplied_flush_covers_nothing(self) -> None:
        b = Builder()
        b.wr(0, b"a")
        b.flush()
        t = self.load(b)
        self.assertEqual(durable_writes(t, t.nrecords), [])

    def test_superblock_images(self) -> None:
        b = Builder()
        meta = b.wr(2 * BLOCK, b"M" * BLOCK)
        b.fl()
        s1 = b.write(0, superblock(2))
        b.reply(s1)
        b.wr(3 * BLOCK, b"L" * BLOCK)  # later, never flushed
        s2 = b.write(BLOCK, superblock(3))
        b.reply(s2)
        t = self.load(b)
        base = bytes(IMG)
        imgs = list(superblock_images(base, t))
        self.assertEqual([i for i, _ in imgs], [t.writes[1].index, t.writes[3].index])
        first = imgs[0][1]
        self.assertEqual(first[2 * BLOCK : 3 * BLOCK], b"M" * BLOCK)
        self.assertEqual(first[:BLOCK], superblock(2))
        second = imgs[1][1]
        self.assertEqual(second[BLOCK : 2 * BLOCK], superblock(3))
        self.assertEqual(second[:BLOCK], bytes(BLOCK))  # s1 is not durable
        self.assertEqual(second[3 * BLOCK :], bytes(BLOCK))  # later write excluded
        self.assertIn(meta, durable_writes(t, t.writes[3].index))

    def test_superblock_partial_overlap_raises(self) -> None:
        b = Builder()
        b.wr(512, b"x" * BLOCK)
        t = self.load(b)
        with self.assertRaises(HarnessError):
            list(superblock_images(bytes(IMG), t))

    def test_superblock_merged_with_neighbour(self) -> None:
        b = Builder()
        b.wr(BLOCK, superblock(5) + b"N" * BLOCK)
        t = self.load(b)
        (_, img), = list(superblock_images(bytes(IMG), t))
        self.assertEqual(img[BLOCK : 2 * BLOCK], superblock(5))
        self.assertEqual(final_flushes(t, 1), {})

    def test_kill_image_deterministic_and_reorders(self) -> None:
        b = Builder()
        b.wr(0, b"D" * BLOCK)
        b.fl()
        b.wr(BLOCK, b"1" * BLOCK)
        b.wr(2 * BLOCK, b"2" * BLOCK)
        t = self.load(b)
        base = bytes(IMG)
        a = kill_image(base, t, random.Random(5))
        self.assertEqual(a, kill_image(base, t, random.Random(5)))
        self.assertEqual(a[:BLOCK], b"D" * BLOCK)
        seen = False
        for seed in range(64):
            img = kill_image(base, t, random.Random(seed))
            dropped_first = img[BLOCK : 2 * BLOCK] == bytes(BLOCK)
            if dropped_first and img[2 * BLOCK : 3 * BLOCK] == b"2" * BLOCK:
                seen = True
        self.assertTrue(seen, "no seed kept a later write while dropping an earlier one")

    def test_replay_matches_server_image(self) -> None:
        b = Builder()
        b.wr(0, b"a" * 10)
        b.wr(5, b"b" * 10)
        b.fl()
        t = self.load(b)
        base = bytes(IMG)
        server = bytearray(base)
        server[0:10] = b"a" * 10
        server[5:15] = b"b" * 10
        allw = [w.id for w in t.writes]
        self.assertEqual(replay(base, t, allw), bytes(server))
        server[100] = 1
        self.assertNotEqual(replay(base, t, allw), bytes(server))

    def test_final_flushes_and_floor(self) -> None:
        # Three commits: meta, flush, super, flush. fresh gen 1 -> gens 2, 3, 4.
        b = Builder()
        finals = []
        for it in range(3):
            b.wr(2 * BLOCK, bytes([it]) * BLOCK)
            b.fl()
            b.wr(super_slot(it), superblock(2 + it))
            finals.append(b.fl())
        t = self.load(b)
        got = final_flushes(t, 1)
        self.assertEqual(got, {0: finals[0], 1: finals[1], 2: finals[2]})
        self.assertEqual(floor_at(got, 0), -1)
        self.assertEqual(floor_at(got, finals[0]), -1)
        self.assertEqual(floor_at(got, finals[0] + 1), 0)
        self.assertEqual(floor_at(got, finals[2] + 1), 2)

    def test_decode_iteration(self) -> None:
        for n in range(256):
            self.assertEqual(decode_iteration(iteration_bytes(n)), n)
        self.assertIsNone(decode_iteration(iteration_bytes(3)[:-1]))
        bad = bytearray(iteration_bytes(3))
        bad[ITER_BYTES - 1] ^= 1
        self.assertIsNone(decode_iteration(bytes(bad)))

    def test_oracle(self) -> None:
        check_iteration(7, 6, (6, 7))
        check_iteration(6, 6, (6, 7))
        with self.assertRaises(HarnessError):
            check_iteration(5, 5, (6, 7))
        with self.assertRaises(HarnessError):
            check_iteration(4, 5)
        with self.assertRaises(HarnessError):
            check_iteration(None, 0)
        check_iteration(9, 5)

    def test_pick_kill_commit_range(self) -> None:
        rng = random.Random(1)
        ks = [pick_kill_commit(rng) for _ in range(2000)]
        self.assertEqual(min(ks), 1)
        self.assertEqual(max(ks), KILL_COMMIT_MAX)

    def test_check_harness_kill(self) -> None:
        lines = ["vibeOS: vibefs: crash-ready", "vibeOS: vibefs: wr 1", "vibeOS: vibefs: wr 5"]
        self.assertEqual(check_harness_kill(-signal.SIGKILL, lines, 5), 5)
        self.assertEqual(check_harness_kill(-signal.SIGKILL, lines, 3), 5)
        with self.assertRaises(HarnessError):
            check_harness_kill(0, lines, 5)
        with self.assertRaises(HarnessError):
            check_harness_kill(None, lines, 5)
        with self.assertRaises(HarnessError):
            check_harness_kill(-signal.SIGKILL, lines, 6)


def super_slot(it: int) -> int:
    return (it % 2) * BLOCK


if __name__ == "__main__":
    unittest.main()
