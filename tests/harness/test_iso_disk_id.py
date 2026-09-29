"""Host tests for scripts/iso_disk_id.py (ROADMAP §10.2, F152)."""

from __future__ import annotations

import contextlib
import hashlib
import io
import tempfile
import unittest
from pathlib import Path

from scripts import iso_disk_id
from scripts.iso_disk_id import DISK_ID_OFF, FALLBACK, derive, from_digest, stamp


def image(disk_id: bytes = b"\x12\x34\x56\x78", size: int = 4096) -> bytes:
    data = bytearray((i * 7) & 0xFF for i in range(size))
    data[DISK_ID_OFF : DISK_ID_OFF + 4] = disk_id
    return bytes(data)


class DeriveTest(unittest.TestCase):
    def test_first_four_bytes_of_sha256_with_the_id_zeroed(self) -> None:
        img = image()
        zeroed = img[:DISK_ID_OFF] + bytes(4) + img[DISK_ID_OFF + 4 :]
        self.assertEqual(derive(img), hashlib.sha256(zeroed).digest()[:4])

    def test_the_current_id_does_not_matter(self) -> None:
        self.assertEqual(derive(image(b"\x00\x00\x00\x00")), derive(image(b"\xff\xee\xdd\xcc")))

    def test_the_rest_of_the_image_does(self) -> None:
        a = bytearray(image())
        a[0] ^= 1
        self.assertNotEqual(derive(bytes(a)), derive(image()))

    def test_too_short(self) -> None:
        with self.assertRaises(ValueError):
            derive(bytes(DISK_ID_OFF + 3))

    def test_zero_digest_uses_fallback(self) -> None:
        self.assertEqual(from_digest(bytes(32)), FALLBACK)
        self.assertEqual(from_digest(bytes(4) + b"\xff" * 28), FALLBACK)
        self.assertEqual(from_digest(b"\x00\x00\x00\x02" + bytes(28)), b"\x00\x00\x00\x02")


class StampTest(unittest.TestCase):
    def test_writes_only_the_signature(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "x.iso"
            before = image()
            path.write_bytes(before)
            got = stamp(str(path))
            after = path.read_bytes()
            self.assertEqual(got, derive(before))
            self.assertEqual(after[DISK_ID_OFF : DISK_ID_OFF + 4], got)
            self.assertEqual(after[:DISK_ID_OFF], before[:DISK_ID_OFF])
            self.assertEqual(after[DISK_ID_OFF + 4 :], before[DISK_ID_OFF + 4 :])
            # Idempotent: a second stamp writes the same bytes.
            self.assertEqual(stamp(str(path)), got)
            self.assertEqual(path.read_bytes(), after)


class MainTest(unittest.TestCase):
    def run_main(self, args: list[str]) -> tuple[int, str]:
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            rc = iso_disk_id.main(args)
        return rc, err.getvalue()

    def test_usage(self) -> None:
        self.assertEqual(self.run_main([])[0], 2)
        self.assertEqual(self.run_main(["a", "b"])[0], 2)

    def test_missing_and_short_files(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            rc, err = self.run_main([str(Path(d) / "none.iso")])
            self.assertEqual(rc, 1)
            self.assertIn("none.iso", err)
            short = Path(d) / "short.iso"
            short.write_bytes(b"x")
            self.assertEqual(self.run_main([str(short)])[0], 1)

    def test_stamps(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "x.iso"
            path.write_bytes(image())
            rc, err = self.run_main([str(path)])
            self.assertEqual(rc, 0)
            self.assertIn(derive(image()).hex(), err)


if __name__ == "__main__":
    unittest.main()
