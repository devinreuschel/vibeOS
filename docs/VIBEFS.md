# vibefs on-disk format

**Version:** 1

This is the format document. Implementation code follows it, not the other way
around. A change to layout, checksums, or the commit protocol bumps `version`
in this file and in `src/vibefs.rs` (`VERSION`) in the same commit.

Index: [DESIGN.md](DESIGN.md) §1.4. Block durability: DESIGN §10.2 (`Flush`, not
`Barrier`). Task list: [ROADMAP.md](ROADMAP.md) §8.5.

---

## 1. Why not FAT32

FAT32 is the compatibility volume (initrd, host `fsck.fat`). It is not the
filesystem we want to keep:

| Gap | FAT32 | Why it becomes a wall |
|-----|-------|------------------------|
| Permissions | no POSIX mode/uid | everything is world-writable; cannot express `0755` vs `0644` |
| Symlinks | none (`EOPNOTSUPP`) | cannot have `/bin` → target, or a loop we can *detect* on disk |
| Crash consistency | flush-order only | power loss during write can orphan clusters or point a dirent at free space |
| Checksums | none | bit rot is returned as file data |
| Directories | linear | large dirs are a scan |
| Allocation | cluster chains | fragmentation is structural |

vibefs exists so those walls are not load-bearing. FAT32 stays for interchange.

---

## 2. Consistency: CoW metadata + atomic superblock switch

**Pick:** copy-on-write metadata, dual superblocks, generation + checksum.
**Not:** a write-ahead journal.

Why CoW, not WAL:

- Recovery is "read both supers, take the highest generation whose checksum
  matches." No journal replay in the kernel, no torn-journal special cases.
- A commit is one ordered sequence: write new metadata blocks → `Flush` →
  write the inactive super slot → `Flush`. The live tree is never overwritten.
- Snapshots are a pinned generation (root block pointers). WAL would make
  snapshots a second mechanism; ROADMAP §8.5 lists snapshots as falling out of
  CoW.
- DESIGN §10.2: a commit is not durable until `Flush`. Barrier is not enough.
  The same rule a journal would need, without the journal.

WAL was the alternative if we wanted in-place file data and a small log. We
do not: file data that replaces existing bytes is also CoW, so a crash during
`write` cannot mix old and new under the committed tree.

**Snapshots:** implemented. A snapshot stores `(name, generation, inode_root,
alloc_root, next_ino)` in the superblock. Creating one increments the
refcount of every currently allocated block (share the tree). Later CoW
allocates new blocks; the snapshot keeps the old ones. Snapshot delete walks
that generation's trees and drops refs. WAL snapshots are N/A (we did not
pick WAL).

---

## 3. Constants

Little-endian. Block size is fixed for v1.

| Name | Value |
|------|-------|
| `VERSION` | `1` (`u16`) |
| Block size | 4096 bytes |
| Super slots | block 0 and block 1 |
| Super magic | `VIBE` (`u32` LE `0x4542_4956`) |
| Meta magic | `VBLK` (`u32` LE `0x4B4C_4256`) |
| Checksum | CRC-32/ISO-HDLC (poly `0xEDB88320`, same as GPT in `src/part.rs`) |
| Max blocks (v1 volume) | 1024 (4 MiB at 4 KiB) |
| Max inodes | 64 |
| Max dirents (volume) | 96 |
| Max name | 64 bytes (matches VFS `MAX_NAME`) |
| Inline file cap | 128 bytes |
| Extents per inode | 4 |
| Snapshot slots | 4 |
| Root inode | 1 |

Larger volumes, more inodes, or a bigger block size are a version bump.

A volume smaller than 16 blocks is invalid (`mkfs` refuses).

---

## 4. Superblock (blocks 0 and 1)

One superblock occupies a full 4096-byte block. Unused bytes are zero.

| Off | Type | Field |
|-----|------|--------|
| 0 | `u32` | magic `VIBE` |
| 4 | `u16` | `VERSION` (must be 1) |
| 6 | `u8` | slot (0 or 1) |
| 7 | `u8` | flags (bit 0: data checksums present; v1 always sets this) |
| 8 | `u32` | block size (4096) |
| 12 | `u32` | `nblocks` |
| 16 | `u64` | generation (strictly increasing) |
| 24 | `u32` | inode B-tree root block |
| 28 | `u32` | alloc-map block |
| 32 | `u32` | `next_ino` |
| 36 | `u32` | root inode number (1) |
| 40 | `[u8; 16]` | uuid |
| 56 | `[u8; 32]` | label, NUL-padded UTF-8 |
| 88 | `u8` | snapshot count (0..4) |
| 89 | `[u8; 7]` | reserved zero |
| 96 | snap[4] | see below, 40 bytes each |
| 256 | … | reserved zero |
| 4092 | `u32` | CRC-32 of bytes `[0, 4092)` with this field treated as zero |

Snapshot entry (40 bytes):

| Off | Type | Field |
|-----|------|--------|
| 0 | `[u8; 16]` | name, NUL-padded |
| 16 | `u64` | generation |
| 24 | `u32` | inode root |
| 28 | `u32` | alloc-map block |
| 32 | `u32` | `next_ino` |
| 36 | `u32` | reserved |

Mount: read both slots. Ignore a slot that fails magic, version, block size,
slot id, or checksum. Of the valid slots, take the **highest generation**.
Equal generation (should not happen): prefer slot 0. Zero valid slots:
unmountable (`Corrupt`).

---

## 5. Metadata block header

Every non-super, non-data block starts with 32 bytes:

| Off | Type | Field |
|-----|------|--------|
| 0 | `u32` | magic `VBLK` |
| 4 | `u8` | kind |
| 5 | `u8` | level (0 = leaf) |
| 6 | `u16` | entry count |
| 8 | `u64` | generation (commit that wrote this block) |
| 16 | `u32` | CRC-32 of the block with this field zeroed |
| 20 | `u32` | owner inode (dir trees; 0 otherwise) |
| 24 | `u32` | left sibling (0 = none) |
| 28 | `u32` | right sibling (0 = none) |

Kinds:

| `kind` | Name | Payload |
|--------|------|---------|
| 1 | `ALLOC` | bitmap + refcounts |
| 2 | `INODE_LEAF` | inode records |
| 3 | `INODE_INT` | `(ino_key, child_block)` |
| 4 | `DIR_LEAF` | directory entries |
| 5 | `DIR_INT` | `(name_key, child_block)` |

A checksum mismatch is `Corrupt`. Readers must not return the payload as file
data, directory entries, or allocator state. `fsck` reports it. It does not
rewrite a "repaired" copy of the bad bytes into the live tree.

---

## 6. Allocator

One `ALLOC` block (v1: `nblocks ≤ 1024` so bitmap + refcounts fit).

After the 32-byte header:

- bitmap: `(nblocks + 7) / 8` bytes, bit `i` set ⇒ block `i` is not free
- refcounts: `nblocks` bytes, `u8` each. 0 ⇒ free (bitmap clear). Super
  slots 0 and 1 are permanently allocated (ref ≥ 1).

A block is reusable only when its refcount hits 0 **and** no live super
(including snapshots) still names a tree that contains it.

**Commit rule:** never overwrite a block the currently mounted super still
reaches. Allocate fresh blocks for new metadata and for replacement file
data. Drop refs on the replaced blocks only **after** the new super is
`Flush`ed.

---

## 7. Inodes

B+tree keyed by inode number. Internal node entry: `ino` (`u32`) + child
block (`u32`). Leaf record is 256 bytes:

| Off | Type | Field |
|-----|------|--------|
| 0 | `u32` | ino |
| 4 | `u8` | kind: 1 file, 2 dir, 3 symlink |
| 5 | `u8` | flags: bit 0 = inline data |
| 6 | `u16` | mode (POSIX; vibefs stores and returns it) |
| 8 | `u32` | nlink |
| 12 | `u32` | uid |
| 16 | `u32` | gid |
| 20 | `u64` | size |
| 28 | `u64` | atime (unix seconds, 0 = unset) |
| 36 | `u64` | mtime |
| 44 | `u64` | ctime |
| 52 | `u32` | dir B-tree root (dirs; 0 = empty) |
| 56 | `u8` | `n_ext` (0..4) |
| 57 | `u8` | `inline_len` (0..128) |
| 58 | `u16` | reserved |
| 60 | extent[4] | 16 bytes each |
| 124 | reserved to 128 | zero |
| 128 | `[u8; 128]` | inline bytes |

Extent (16 bytes):

| Off | Type | Field |
|-----|------|--------|
| 0 | `u32` | logical start (file offset in blocks) |
| 4 | `u32` | physical start |
| 8 | `u32` | length in blocks (≥ 1) |
| 12 | `u32` | CRC-32 of the extent's data bytes |

A regular file with `size ≤ 128` and flag inline set stores bytes in the
inode. Crossing 128 bytes (or any write that no longer fits) allocates
extents and clears the inline flag. Symlink targets use the inline area
(target longer than 128 is `NameTooLong` / `Inval` in v1).

Empty files may be inline with `inline_len = 0`.

---

## 8. Directories

B+tree keyed by name (byte-wise, length then bytes; not UTF-8 normalized).
`.` and `..` are **not** stored; the VFS synthesizes them. A directory with
`dir_root = 0` is empty.

Leaf entry (72 bytes):

| Off | Type | Field |
|-----|------|--------|
| 0 | `u32` | ino |
| 4 | `u8` | kind |
| 5 | `u8` | name length (1..64) |
| 6 | `[u8; 64]` | name, unused bytes zero |
| 70 | `u16` | reserved |

Internal entry: same name key + child block `u32` at offset 68 (name field
still 64 bytes; reserved overwritten by the pointer). Packed as 72 bytes:
`ino` unused/zero, `kind` unused, `nlen` + `name`, `child` `u32`.

Names cannot contain `/` or NUL.

---

## 9. File data

Not metadata. No `VBLK` header. Contents are raw file bytes, block-aligned
on disk; a short last block is zero-padded on disk and truncated by `size`.

v1 always stores an extent CRC. A mismatch on read is `Corrupt`: the caller
gets an error, not the bad bytes. `fsck` lists the inode and extent. It
does not copy the corrupt payload into a new extent and call the volume
clean.

Unreferenced data blocks (written then crashed before super switch) are
leaks. `fsck` reports them. Check-only mode does not reuse them as file
contents.

---

## 10. Commit protocol

One transaction = one generation bump.

1. In memory: apply creates, writes, unlinks, snapshot records.
2. Allocate new blocks for: alloc-map, inode tree, each dirty directory
   tree, replacement data extents. Do not reuse blocks reachable from the
   current super or any snapshot.
3. Write those blocks (data extents may already have been written during
   `write`).
4. `Flush`.
5. Write the inactive super slot (`generation + 1`, new roots, checksum).
6. `Flush`.
7. Drop refcounts on replaced metadata and data. Optionally mirror the
   super into the other slot (same generation) and `Flush` again.

Crash:

| When | After reboot |
|------|----------------|
| During data / new-meta writes | old super still valid; new blocks leaked |
| During step 5 (torn super) | checksum fails; other slot wins |
| After step 6 | new generation is the volume |

`fsck` must not take a checksum-failing super or metadata block and treat
it as authoritative.

`fsync` / `sync` is this protocol. `Barrier` is not a commit.

---

## 11. `mkfs` and `fsck`

Same module as the kernel (`src/vibefs.rs`). Host binaries
`mkfs-vibefs` and `fsck-vibefs` are thin CLIs over that module.

`mkfs`: zero image, write alloc-map (supers + alloc + one inode leaf
allocated), root directory inode 1, generation 1 in **both** super slots,
`Flush`.

`fsck` (check-only default):

1. Pick the valid super as mount would.
2. Walk alloc-map; verify its header checksum.
3. Walk inode B-tree; verify every metadata checksum.
4. For each inode: kind/mode sanity, extent ranges in-volume and allocated,
   inline vs size, directory B-tree walk, no name duplicates, nlink matches
   dirent count (root nlink is 1 in v1; subdirs do not store `..`).
5. For each extent: optional data CRC verify.
6. Compare reachable blocks to the bitmap. Leaks = warning. Bitmap-clear
   but reachable = error.
7. Exit 0 only if no error. Warnings (leaks) are printed; default is still
   0 if the live tree is checksum-clean. Corruption (bad CRC, bad magic,
   cycle, out-of-range) is nonzero **and** must not be "fixed" by using the
   bad payload.

Repair in v1 is limited to dropping leaked (unreachable) blocks from the
bitmap after a successful tree walk, and only with an explicit flag. Repair
never writes over a live file's bytes.

---

## 12. Crash-consistency test

Not a clean unmount.

1. **Host:** a disk wrapper that drops writes after a randomized budget,
   including in the middle of a commit (`write` of a metadata block or the
   super, and `Flush`). Then `fsck`. Repeat. A clean `fsck` plus `mount`
   must yield a tree that is some committed prefix of the workload, never
   a mix that fails a checksum while `fsck` said ok.
2. **QEMU:** `mkfs` a file-backed virtio-blk image, boot a write loop that
   `fsync`s, `kill -9` QEMU at a randomized point after the loop has
   started (including mid-`write`/`Flush`), then host `fsck-vibefs`. Same
   pass criterion.

Killing only between syscalls is not enough; the host wrapper injects the
drop on device write calls, which is inside `write`/`fsync`.

---

## 13. What v1 will not do

- Data journaling or WAL
- Compression, encryption, RAID
- Block size other than 4096
- Inode extents overflow to an extent tree (4 extents is the cap; files
  that would need a fifth return `NoSpace`)
- POSIX ACLs, xattrs
- NVMe/AHCI (parked; this format is device-agnostic)

---

## 14. Versioning

`VERSION` is 1. Readers reject any other value. Additive on-disk changes
that old readers can ignore use flag bits. Layout changes bump `VERSION`.
