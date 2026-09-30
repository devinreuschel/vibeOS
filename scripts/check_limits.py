#!/usr/bin/env python3
"""Table sizes and resource caps live in one `limits` module (ROADMAP §10.4, D1).

Over the Rust sources under `crates/core/src/` and `src/`:

1. A `const MAX_*` defined outside `crates/core/src/limits.rs`, at any
   visibility and inside a fn too, fails unless `ALLOW` names its path and
   name with a non-empty bound and a kind of `hardware` or `on-disk`: the
   bound a device, a hardware interface, or an on-disk format sets, which
   the kernel's resource policy does not.
2. An array type or repeat expression whose length is `limits::MAX_<T>`, or
   a bare `MAX_<T>`, fails for each of the eight table constants `T`: a cap
   is a constant, never an array type. A bare name is exempt in a file that
   defines `MAX_<T>` itself, or whose directory's `mod.rs` does (vibefs's
   own `MAX_INODES`, an on-disk count).
3. The eight `limits` values equal the numbers the Phase 10 exit-gate line
   of docs/ROADMAP.md states. A change to either one changes both.
4. An `ALLOW` entry whose constant no longer exists fails.

Prints `check_limits: ok (<n> files, <m> allowed)`, or one error per line on
stderr and exits 1.
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LIMITS = "crates/core/src/limits.rs"
SCOPE = ("crates/core/src", "src")
KINDS = frozenset({"hardware", "on-disk"})

# The eight table constants, and the words the exit-gate line gives each.
TABLES: dict[str, str] = {
    "MAX_PROCS": "processes",
    "MAX_FDS": "descriptors per process",
    "MAX_THREADS": "threads",
    "MAX_OPEN_FILES": "open files",
    "MAX_INODES": "inodes",
    "MAX_DENTRIES": "dentries",
    "MAX_MOUNTS": "mounts",
    "MAX_REGIONS": "regions per address space",
}

# The Phase 10 exit-gate line that states the limits (ROADMAP §10's exit gate).
GATE_KEY = "this gate states the limits"


@dataclass(frozen=True)
class Allow:
    path: str
    name: str
    kind: str
    bound: str


VIBEFS = "docs/VIBEFS.md §3 (v1 format constants); a larger count is a version bump"
FAT = "the FAT32 on-disk format (Microsoft FAT specification)"

# (path, name, kind, bound): each `MAX_*` a hardware or on-disk bound sizes.
ALLOW: list[Allow] = [
    Allow("crates/core/src/acpi/mod.rs", "MAX_CPUS", "hardware",
          "the u64 online and IPI masks: 64 CPUs (SMP.md §7.5)"),
    Allow("crates/core/src/acpi/mod.rs", "MAX_IOAPICS", "hardware",
          "I/O APICs one MADT describes on the supported machines (ACPI MADT type 1)"),
    Allow("crates/core/src/acpi/mod.rs", "MAX_ISOS", "hardware",
          "interrupt source overrides one MADT lists: the 16 ISA IRQs, twice over "
          "(ACPI MADT type 2)"),
    Allow("crates/core/src/irq/ipi.rs", "MAX_IPI_CPUS", "hardware",
          "the u64 CPU mask an IPI round targets: 64 CPUs"),
    Allow("crates/core/src/log/trace/mod.rs", "MAX_CPUS", "hardware",
          "the IPI mask's 64 CPUs (irq::ipi::MAX_IPI_CPUS), one trace ring each"),
    Allow("crates/core/src/mm/pmm.rs", "MAX_ORDER", "hardware",
          "a buddy block of 2^10 frames covers the 2 MiB large page (Intel SDM Vol. 3A §4.5)"),
    Allow("crates/core/src/dev/pci.rs", "MAX_BARS", "hardware",
          "a type 0 header's six BARs (PCI Local Bus Spec 3.0 §6.2.5)"),
    Allow("crates/core/src/dev/pci.rs", "MAX_SCAN", "hardware",
          "functions scanned on the QEMU machines' one bus: 32 devices, up to 8 functions "
          "each, to 64"),
    Allow("crates/core/src/dev/pci.rs", "MAX_CAP_WALK", "hardware",
          "capabilities 192 bytes of config space can chain, 4 bytes each "
          "(PCI Local Bus Spec 3.0 §6.7)"),
    Allow("crates/core/src/dev/pci.rs", "MAX_BAR_MAP", "hardware",
          "memory BAR bytes mapped at boot: QEMU VGA's 16 MiB BAR and NIC MMIO fit (DESIGN §4.1)"),
    Allow("crates/core/src/dev/mod.rs", "MAX_DEVICES", "hardware",
          "one device per PCI function the scan finds (dev::pci::MAX_SCAN)"),
    Allow("crates/core/src/dev/dma.rs", "MAX_SG", "hardware",
          "scatter-gather entries one device request carries, the virtqueue chain's data segments"),
    Allow("crates/core/src/dev/virtio.rs", "MAX_VENDOR_CAPS", "hardware",
          "virtio vendor capabilities: five types, each read once (virtio 1.2 §4.1.4)"),
    Allow("crates/core/src/dev/virtio.rs", "MAX_CHAIN", "hardware",
          "descriptors one virtio-blk request chains: header, data segments, status "
          "(virtio 1.2 §5.2.6)"),
    Allow("crates/core/src/block/mod.rs", "MAX_QUEUE", "hardware",
          "requests in flight per disk, the virtqueue's depth (virtio 1.2 §2.7)"),
    Allow("crates/core/src/block/mod.rs", "MAX_SEGS", "hardware",
          "data segments one request carries within a virtqueue chain (virtio 1.2 §5.2.6)"),
    Allow("src/drivers/virtio_blk_init/vq.rs", "MAX_VQ", "hardware",
          "request virtqueues a virtio-blk device offers with VIRTIO_BLK_F_MQ (virtio 1.2 §5.2.3)"),
    Allow("src/drivers/virtio_blk_init/vq.rs", "MAX_QSIZE", "hardware",
          "descriptors per virtqueue, at most the device's queue_size (virtio 1.2 §4.1.4.3)"),
    Allow("crates/core/src/console/fb.rs", "MAX_COLS", "hardware",
          "text columns on the largest supported framebuffer, 3840 pixels wide"),
    Allow("crates/core/src/console/fb.rs", "MAX_ROWS", "hardware",
          "text rows on the largest supported framebuffer, 2160 pixels high"),
    Allow("crates/core/src/console/fb.rs", "MAX_CELLS", "hardware",
          "character cells on the largest supported framebuffer, 3840 by 2160"),
    Allow("crates/core/src/block/part.rs", "MAX_EBR_DEPTH", "on-disk",
          "extended boot records one MBR chain links, bounded so a looping chain ends "
          "(MBR format)"),
    Allow("crates/core/src/fs/fat/mod.rs", "MAX_CLUS_BYTES", "on-disk",
          "the largest cluster read or written whole, 4096 bytes (" + FAT + ")"),
    Allow("crates/core/src/fs/fat/mod.rs", "MAX_DIR_BYTES", "on-disk",
          "a directory holds at most 65,536 entries of 32 bytes (" + FAT + ")"),
    Allow("crates/core/src/fs/fat/vol.rs", "MAX_NCLUS", "on-disk",
          "the highest FAT32 cluster count, 0x0FFFFFF5 (" + FAT + ")"),
    Allow("crates/core/src/fs/vibefs/mod.rs", "MAX_BLOCKS", "on-disk", VIBEFS),
    Allow("crates/core/src/fs/vibefs/mod.rs", "MAX_INODES", "on-disk", VIBEFS),
    Allow("crates/core/src/fs/vibefs/mod.rs", "MAX_DENTS", "on-disk", VIBEFS),
    Allow("crates/core/src/fs/vibefs/mod.rs", "MAX_NAME", "on-disk", VIBEFS),
    Allow("crates/core/src/fs/vibefs/mod.rs", "MAX_EXT", "on-disk", VIBEFS),
    Allow("crates/core/src/fs/vibefs/mod.rs", "MAX_SNAPS", "on-disk", VIBEFS),
    Allow("crates/core/src/fs/vibefs/mod.rs", "MAX_META", "on-disk",
          "metadata blocks one v1 generation writes, from §3's inode count "
          "(docs/VIBEFS.md §3, §6)"),
    Allow("crates/core/src/fs/vibefs/mod.rs", "MAX_DROP", "on-disk",
          "blocks one v1 commit frees, which §3's 96 dirents bound (docs/VIBEFS.md §3, §6)"),
    Allow("crates/core/src/fs/vibefs/mod.rs", "MAX_FILE_SIZE", "on-disk", VIBEFS),
]

CONST = re.compile(
    r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?const[ \t]+(MAX_[A-Z0-9_]+)[ \t]*:[ \t]*"
    r"(usize|u8|u16|u32|u64|i32|i64)[ \t]*=[ \t]*([^;]+);")
TABLE_ARRAY = re.compile(
    r";\s*((?:[A-Za-z_][A-Za-z0-9_]*::)*limits::)?(" + "|".join(TABLES) + r")\s*\]")
GATE_NUMBERS = re.compile(
    r"(\d+) processes, (\d+) descriptors per process, (\d+) threads, (\d+) open files, "
    r"(\d+) inodes, (\d+) dentries, (\d+) mounts, and (\d+) regions per address space")


def strip_comments(text: str) -> str:
    """`text` with each `//` comment blanked, so prose names no constant."""
    return re.sub(r"//[^\n]*", "", text)


def consts(text: str) -> dict[str, str]:
    """Each `const MAX_*` `text` defines, to its value expression."""
    return {m.group(1): m.group(3).strip() for m in CONST.finditer(strip_comments(text))}


def int_value(expr: str) -> int | None:
    """An integer literal's value, `_` separators allowed; None otherwise."""
    s = expr.replace("_", "")
    return int(s, 0) if re.fullmatch(r"0x[0-9a-fA-F]+|\d+", s) else None


def gate_values(roadmap: str) -> dict[str, int] | None:
    """The eight numbers the exit-gate line states, by constant; None when no
    line holds `GATE_KEY` with them."""
    for line in roadmap.splitlines():
        if GATE_KEY not in line:
            continue
        m = GATE_NUMBERS.search(line)
        if m:
            return {name: int(v) for name, v in zip(TABLES, m.groups(), strict=True)}
    return None


def check(files: dict[str, str], allow: list[Allow], roadmap: str) -> list[str]:
    """One message per failure. `files` maps each repo-relative `.rs` path in
    scope to its text."""
    errors: list[str] = []
    defined = {path: consts(text) for path, text in files.items()}
    allowed: dict[tuple[str, str], Allow] = {}
    for a in allow:
        key = (a.path, a.name)
        if key in allowed:
            errors.append(f"ALLOW: {a.path} {a.name} is listed twice")
        allowed[key] = a
        if a.kind not in KINDS:
            errors.append(f"ALLOW: {a.path} {a.name}: kind {a.kind!r}, not hardware or on-disk")
        if not a.bound.strip():
            errors.append(f"ALLOW: {a.path} {a.name}: no bound names what sizes it")
        if a.name not in defined.get(a.path, {}):
            errors.append(f"ALLOW: {a.path} {a.name}: no such constant; delete the entry")
    # Rule 1: a MAX_* outside the limits module needs its bound.
    for path in sorted(defined):
        if path == LIMITS:
            continue
        for name in sorted(defined[path]):
            if (path, name) not in allowed:
                errors.append(
                    f"{path}: const {name} outside the limits module; move it to {LIMITS}, "
                    "or name it in ALLOW with the hardware or on-disk bound that sizes it")
    # Rule 2: no array is sized by a table constant.
    for path in sorted(files):
        text = strip_comments(files[path])
        home = defined.get(path, {})
        parent = defined.get(str(Path(path).parent / "mod.rs"), {})
        for m in TABLE_ARRAY.finditer(text):
            qualified, name = m.group(1), m.group(2)
            if not qualified and (name in home or name in parent):
                continue
            line = text.count("\n", 0, m.start()) + 1
            errors.append(f"{path}:{line}: an array sized by {name}; a cap is a constant, "
                          "not a type: build a heap table with limits::table")
    # Rule 3: the limits equal the exit gate's numbers.
    limits = defined.get(LIMITS, {})
    gate = gate_values(roadmap)
    if gate is None:
        errors.append(
            f"docs/ROADMAP.md: no exit-gate line holds `{GATE_KEY}` with the eight limits")
    else:
        for name, want in gate.items():
            got = int_value(limits[name]) if name in limits else None
            if got != want:
                errors.append(f"{LIMITS}: {name} is {limits.get(name, 'missing')}, the exit gate "
                              f"states {want} {TABLES[name]}; a change to either changes both")
    return errors


def source_files(root: Path) -> dict[str, str]:
    """Each `.rs` file under `SCOPE`, repo-relative, to its text."""
    out: dict[str, str] = {}
    for top in SCOPE:
        for p in sorted((root / top).rglob("*.rs")):
            out[p.relative_to(root).as_posix()] = p.read_text(encoding="utf-8", errors="replace")
    return out


def main(argv: list[str] | None = None) -> int:
    del argv
    files = source_files(ROOT)
    roadmap = (ROOT / "docs" / "ROADMAP.md").read_text(encoding="utf-8")
    errors = check(files, ALLOW, roadmap)
    if errors:
        for e in errors:
            print(f"check_limits: {e}", file=sys.stderr)
        return 1
    print(f"check_limits: ok ({len(files)} files, {len(ALLOW)} allowed)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
