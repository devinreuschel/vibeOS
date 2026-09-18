#!/usr/bin/env python3
"""Build a 64 KiB FAT32 initrd (hello.txt + etc/). ROADMAP §8.6."""

from __future__ import annotations

import struct
import sys

SEC = 512
SIZE = 64 * 1024
RSVD = 32
SPC = 1
NUM_FATS = 2
ROOT = 2


def put16(b: bytearray, o: int, v: int) -> None:
    struct.pack_into("<H", b, o, v)


def put32(b: bytearray, o: int, v: int) -> None:
    struct.pack_into("<I", b, o, v)


def main() -> int:
    path = sys.argv[1] if len(sys.argv) > 1 else "initrd.fat"
    totsec = SIZE // SEC
    fatsz = 1
    while True:
        data = totsec - RSVD - NUM_FATS * fatsz
        nclus = data // SPC
        need = ((nclus + 2) * 4 + SEC - 1) // SEC
        if need <= fatsz:
            break
        fatsz = need
        if RSVD + NUM_FATS * fatsz >= totsec:
            print("geometry failed", file=sys.stderr)
            return 1
    data_lba = RSVD + NUM_FATS * fatsz
    nclus = (totsec - data_lba) // SPC
    img = bytearray(SIZE)

    boot = bytearray(SEC)
    boot[0:3] = b"\xeb\x58\x90"
    boot[3:11] = b"VIBEOS  "
    put16(boot, 11, SEC)
    boot[13] = SPC
    put16(boot, 14, RSVD)
    boot[16] = NUM_FATS
    boot[21] = 0xF8
    put16(boot, 24, 32)
    put16(boot, 26, 2)
    put32(boot, 32, totsec)
    put32(boot, 36, fatsz)
    put32(boot, 44, ROOT)
    put16(boot, 48, 1)
    put16(boot, 50, 6)
    boot[64] = 0x80
    boot[66] = 0x29
    put32(boot, 67, 0x56424F53)
    boot[71:82] = b"VIBEOS     "
    boot[82:90] = b"FAT32   "
    boot[510] = 0x55
    boot[511] = 0xAA
    img[0:SEC] = boot
    img[6 * SEC : 7 * SEC] = boot

    fsinfo = bytearray(SEC)
    put32(fsinfo, 0, 0x41615252)
    put32(fsinfo, 484, 0x61417272)
    put32(fsinfo, 488, nclus - 1)
    put32(fsinfo, 492, 3)
    put32(fsinfo, 508, 0xAA550000)
    img[SEC : 2 * SEC] = fsinfo
    img[7 * SEC : 8 * SEC] = fsinfo

    fat = bytearray(SEC)
    put32(fat, 0, 0x0FFFFFF8)
    put32(fat, 4, 0x0FFFFFFF)
    put32(fat, 8, 0x0FFFFFFF)
    fat0 = RSVD * SEC
    fat1 = fat0 + fatsz * SEC
    img[fat0 : fat0 + SEC] = fat
    img[fat1 : fat1 + SEC] = fat

    # Root dir at cluster 2. HELLO.TXT (8.3) + ETC directory.
    msg = b"hello from initrd\n"
    hello_clu = 3
    etc_clu = 4
    # mark clusters 3,4 EOC in both FATs
    put32(img, fat0 + 3 * 4, 0x0FFFFFFF)
    put32(img, fat0 + 4 * 4, 0x0FFFFFFF)
    put32(img, fat1 + 3 * 4, 0x0FFFFFFF)
    put32(img, fat1 + 4 * 4, 0x0FFFFFFF)
    put32(fsinfo, 488, nclus - 3)
    put32(fsinfo, 492, 5)
    img[SEC : 2 * SEC] = fsinfo
    img[7 * SEC : 8 * SEC] = fsinfo

    def short_ent(name11: bytes, attr: int, clu: int, size: int) -> bytes:
        e = bytearray(32)
        e[0:11] = name11
        e[11] = attr
        put16(e, 20, (clu >> 16) & 0xFFFF)
        put16(e, 26, clu & 0xFFFF)
        put32(e, 28, size)
        return bytes(e)

    root = bytearray(SEC)
    root[0:32] = short_ent(b"VIBEOS     ", 0x08, 0, 0)
    root[32:64] = short_ent(b"HELLO   TXT", 0x20, hello_clu, len(msg))
    root[64:96] = short_ent(b"ETC        ", 0x10, etc_clu, 0)
    root_off = data_lba * SEC
    img[root_off : root_off + SEC] = root

    hello_off = (data_lba + (hello_clu - 2) * SPC) * SEC
    img[hello_off : hello_off + len(msg)] = msg

    etc = bytearray(SEC)
    etc[0:32] = short_ent(b".          ", 0x10, etc_clu, 0)
    etc[32:64] = short_ent(b"..         ", 0x10, 0, 0)
    etc_off = (data_lba + (etc_clu - 2) * SPC) * SEC
    img[etc_off : etc_off + SEC] = etc

    with open(path, "wb") as f:
        f.write(img)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
