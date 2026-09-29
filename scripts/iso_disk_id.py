#!/usr/bin/env python3
"""Replace the ISO's MBR disk signature with one derived from the image.

`limine bios-install` seeds the 4-byte MBR disk signature at 0x1B8 from
`time(NULL)`, so two builds of one commit differ there (ROADMAP §10.2,
F152). `mkiso.sh` runs this after `bios-install`: the signature becomes the
first 4 bytes of the SHA-256 of the image with 0x1B8..0x1BC zeroed, or
`00 00 00 01` if those are zero. `limine.conf` names its files with
`boot():`, so nothing looks the disk up by this signature.

usage: iso_disk_id.py <image>
"""

from __future__ import annotations

import hashlib
import sys

DISK_ID_OFF = 0x1B8
DISK_ID_LEN = 4
FALLBACK = b"\x00\x00\x00\x01"


def derive(image: bytes) -> bytes:
    """The signature for `image`, which its current signature does not change."""
    if len(image) < DISK_ID_OFF + DISK_ID_LEN:
        raise ValueError(f"image is {len(image)} bytes, too short for an MBR")
    h = hashlib.sha256()
    h.update(image[:DISK_ID_OFF])
    h.update(bytes(DISK_ID_LEN))
    h.update(image[DISK_ID_OFF + DISK_ID_LEN :])
    return from_digest(h.digest())


def from_digest(digest: bytes) -> bytes:
    """The signature a digest gives: its first 4 bytes, never all zero."""
    out = digest[:DISK_ID_LEN]
    return FALLBACK if out == bytes(DISK_ID_LEN) else out


def stamp(path: str) -> bytes:
    """Write the derived signature into the image at `path`; return it."""
    with open(path, "r+b") as f:
        data = f.read()
        disk_id = derive(data)
        f.seek(DISK_ID_OFF)
        f.write(disk_id)
    return disk_id


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    if len(args) != 1:
        print("usage: iso_disk_id.py <image>", file=sys.stderr)
        return 2
    try:
        disk_id = stamp(args[0])
    except (OSError, ValueError) as e:
        print(f"iso_disk_id: {args[0]}: {e}", file=sys.stderr)
        return 1
    print(f"iso_disk_id: {args[0]}: {disk_id.hex()}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
