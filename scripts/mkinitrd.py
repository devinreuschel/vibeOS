#!/usr/bin/env python3
"""Build a 64 KiB FAT32 initrd (hello.txt, etc/, optional ELF). ROADMAP §8.6 / §9.8."""

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


def short_ent(name11: bytes, attr: int, clu: int, size: int) -> bytes:
    e = bytearray(32)
    e[0:11] = name11
    e[11] = attr
    put16(e, 20, (clu >> 16) & 0xFFFF)
    put16(e, 26, clu & 0xFFFF)
    put32(e, 28, size)
    return bytes(e)


def name11(filename: str) -> bytes:
    base = filename.rsplit("/", 1)[-1]
    if "." in base:
        stem, ext = base.rsplit(".", 1)
    else:
        stem, ext = base, ""
    stem = stem.upper()[:8].ljust(8)
    ext = ext.upper()[:3].ljust(3)
    return (stem + ext).encode("ascii")


def main() -> int:
    args = sys.argv[1:]
    path = "initrd.fat"
    extras: list[tuple[str, bytes]] = []
    i = 0
    while i < len(args):
        a = args[i]
        if a == "--add" and i + 1 < len(args):
            spec = args[i + 1]
            src, _, dest = spec.partition(":")
            dest = dest or src
            extras.append((dest, open(src, "rb").read()))
            i += 2
            continue
        if not a.startswith("-"):
            path = a
        i += 1

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

    def write_fats(fat: bytearray) -> None:
        fat0 = RSVD * SEC
        fat1 = fat0 + fatsz * SEC
        img[fat0 : fat0 + SEC] = fat
        img[fat1 : fat1 + SEC] = fat

    fat = bytearray(SEC)
    put32(fat, 0, 0x0FFFFFF8)
    put32(fat, 4, 0x0FFFFFFF)
    put32(fat, 8, 0x0FFFFFFF)  # root cluster 2
    write_fats(fat)

    msg = b"hello from initrd\n"
    hello_clu = 3
    etc_clu = 4
    next_clu = 5
    used_data = 3  # hello.txt, etc, plus extras

    def fat_eoc(clu: int) -> None:
        fat0 = RSVD * SEC
        fat1 = fat0 + fatsz * SEC
        put32(img, fat0 + clu * 4, 0x0FFFFFFF)
        put32(img, fat1 + clu * 4, 0x0FFFFFFF)

    def fat_link(clu: int, nxt: int) -> None:
        fat0 = RSVD * SEC
        fat1 = fat0 + fatsz * SEC
        put32(img, fat0 + clu * 4, nxt)
        put32(img, fat1 + clu * 4, nxt)

    def clu_off(clu: int) -> int:
        return (data_lba + (clu - 2) * SPC) * SEC

    fat_eoc(hello_clu)
    fat_eoc(etc_clu)

    extra_ents: list[bytes] = []
    for dest, blob in extras:
        n = max(1, (len(blob) + SEC - 1) // SEC)
        first = next_clu
        rest = blob
        clu = first
        for k in range(n):
            if clu >= 2 + nclus:
                print("out of clusters", file=sys.stderr)
                return 1
            chunk = rest[:SEC]
            rest = rest[SEC:]
            off = clu_off(clu)
            img[off : off + len(chunk)] = chunk
            if k + 1 == n:
                fat_eoc(clu)
            else:
                fat_link(clu, clu + 1)
            clu += 1
        next_clu = clu
        used_data += n
        extra_ents.append(short_ent(name11(dest), 0x20, first, len(blob)))

    fsinfo = bytearray(SEC)
    put32(fsinfo, 0, 0x41615252)
    put32(fsinfo, 484, 0x61417272)
    put32(fsinfo, 488, nclus - used_data)
    put32(fsinfo, 492, next_clu)
    put32(fsinfo, 508, 0xAA550000)
    img[SEC : 2 * SEC] = fsinfo
    img[7 * SEC : 8 * SEC] = fsinfo

    root = bytearray(SEC)
    root[0:32] = short_ent(b"VIBEOS     ", 0x08, 0, 0)
    root[32:64] = short_ent(b"HELLO   TXT", 0x20, hello_clu, len(msg))
    root[64:96] = short_ent(b"ETC        ", 0x10, etc_clu, 0)
    for i, ent in enumerate(extra_ents):
        o = 96 + i * 32
        if o + 32 > SEC:
            print("root dir full", file=sys.stderr)
            return 1
        root[o : o + 32] = ent
    root_off = data_lba * SEC
    img[root_off : root_off + SEC] = root

    hello_off = clu_off(hello_clu)
    img[hello_off : hello_off + len(msg)] = msg

    etc = bytearray(SEC)
    etc[0:32] = short_ent(b".          ", 0x10, etc_clu, 0)
    etc[32:64] = short_ent(b"..         ", 0x10, 0, 0)
    etc_off = clu_off(etc_clu)
    img[etc_off : etc_off + SEC] = etc

    with open(path, "wb") as f:
        f.write(img)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
