# vibefs on-disk format

**Version:** 1

This is the format document. Implementation code follows it, not the other way
around. A change to layout, checksums, or the commit protocol bumps `version`
in this file and in `src/vibefs.rs` (`VERSION`) in the same commit.

The byte layouts in §4 to §9 are the ones v1 code writes. v1 is not carried
forward: ROADMAP §14.8 retires it once root is v2 (§14).
Where v1 code does not yet meet a guarantee this file states (crash safety,
space accounting, checksum handling, snapshot delete, `fsck` coverage, the
file size limit, mount validation, truncate), the guarantee stands and the
section says what the code does. A note such as (F014; ROADMAP §10.11)
names a finding in the kernel review
([reviews/KERNEL_REVIEW.md](reviews/KERNEL_REVIEW.md)) and the ROADMAP
section whose line cites the same id and fixes the code.

Index: [DESIGN.md](DESIGN.md) §1.4. Block durability: DESIGN §10.2 (completion
waits, `Flush`, and `Fua`). Task list: [ROADMAP.md](ROADMAP.md) §8.5. Open v1
work: ROADMAP §10.2, §10.11, §12.5, §13.9, and §18.5. v2: the requirements in
§15, and ROADMAP §14.8.

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
v1 does not remove the directory wall yet: lookup scans one 96-entry table
per volume (§8, F067).

---

## 2. Consistency: CoW metadata + atomic superblock switch

**Pick:** copy-on-write metadata, dual superblocks, generation + checksum.
**Not:** a write-ahead journal.

Why CoW, not WAL:

- Recovery is "read both supers, take the highest generation whose checksum
  matches." No journal replay in the kernel, no torn-journal special cases.
- A commit is one ordered sequence, each step starting after the one before
  it completes: write new metadata blocks → `Flush` → write the inactive
  super slot → `Flush`. The live tree is never overwritten.
- Snapshots are a pinned generation (root block pointers). WAL would make
  snapshots a second mechanism; ROADMAP §8.5 lists snapshots as falling out of
  CoW.
- DESIGN §10.2: a commit is not durable until a `Flush` covers its superblock
  write, or that write carries `Fua`. The same rule a journal would need,
  without the journal. The block layer orders nothing, and a `Flush` covers
  only writes whose completion was reported before it was submitted, so a
  commit waits for its block writes before it sends one. v1 writes through the
  block cache, whose flush writes each dirty page and waits for it, but skips
  pages `blk-wb` is still writing (F015; ROADMAP §12.5, after §10.11 adds the
  wait under F043). The RAM-backed `/vibe` volume has no device or cache to
  wait for.

WAL was the alternative if we wanted in-place file data and a small log. We
do not: file data that replaces existing bytes is also CoW, so a crash during
`write` cannot mix old and new under the committed tree.

**Snapshots:** a snapshot stores `(name, generation, inode_root,
alloc_root, next_ino)` in one of the superblock's 4 snapshot entries (§4). Creating one commits
pending changes, increments the refcount of every block the live tree
reaches (share the tree), and commits again. Later CoW allocates new blocks; the snapshot
keeps the old ones. Snapshot delete walks that generation's trees and drops
one reference per reachable block. v1 code implements create only
(`Vol::snapshot`, called from the host test `snapshot_pins_generation` and
one in-guest test), and its create increments every block with a refcount
above 0, including blocks 0 and 1 and blocks that only an older snapshot
reaches. v1 has no snapshot delete, list, or mount, so a snapshot's blocks stay allocated until
the volume is reformatted, and `fsck` reports the ones only a snapshot
reaches as leak warnings (§11). Delete lands
in ROADMAP §12.5 (F067). WAL snapshots are N/A (we did not pick WAL).
This eager scheme is v1's. v2 tracks a snapshot's space by the generation
that wrote each block, with no per-block count (§15's Snapshots row).

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
| Max file size | (2^32 − 1) × 4096 bytes: file block indexes 0 to 2^32 − 2 (`u32`) |
| Snapshot slots | 4 |
| Root inode | 1 |

Larger volumes, more inodes, or a bigger block size are a version bump.

A volume smaller than 16 blocks is invalid (`mkfs` refuses).

v1 code does not enforce the file size limit yet: a `write` just below file
offset 2^44 makes the next access to that block overflow `map_block`'s `u32`
arithmetic, which panics the kernel in the default dev profile, and an
offset of 2^44 or more wraps onto the file's low blocks (F008; ROADMAP
§10.11).

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

Mount: read both slots. Ignore a slot that fails magic, checksum, version,
slot id, block size, `16 ≤ nblocks ≤ 1024`, or snapshot count ≤ 4. Of the
valid slots, take the **highest generation**. Equal generation (both slots
after `mkfs`): prefer slot 0. Zero valid slots: unmountable (`Corrupt`). A
device smaller than `nblocks` is `Inval`. Live snapshot entries come first
in `snap[4]`: entry `i` is live when `i` is below the snapshot count and its
inode root is non-zero.

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
| 24 | `u32` | reserved, 0 (v1 writes 0 and never reads it) |
| 28 | `u32` | reserved, 0 (v1 writes 0 and never reads it) |

Kinds:

| `kind` | Name | Payload |
|--------|------|---------|
| 1 | `ALLOC` | bitmap + refcounts |
| 2 | `INODE_LEAF` | inode records |
| 3 | `INODE_INT` | `(ino_key, child_block)` |
| 4 | `DIR_LEAF` | directory entries |
| 5 | `DIR_INT` | `(child_block, name_key)` |

A checksum mismatch is `Corrupt`. Readers must not return the payload as file
data, directory entries, or allocator state. `fsck` reports it. It does not
rewrite a "repaired" copy of the bad bytes into the live tree.

A block whose checksum matches is still checked before use: an entry count
within the block's capacity (15 inode records, 56 directory entries, or 8
children of an internal node); every block pointer in `[2, nblocks)`;
extents with `len ≥ 1` and `phys + len ≤ nblocks`; the inline flag only
with `size ≤ 128`; unique inode numbers; `next_ino` above the highest inode
number; each metadata block referenced once; a refcount above 0 on every
reachable block. A failure is `Corrupt`, never a panic. v1 mount checks none
of these; of the record fields, it checks only the extent count,
`inline_len`, the inode kind, and the name length. A crafted image with
valid checksums can therefore panic the kernel or make copy-on-write
overwrite a live block (F061). v1 is not fixed for this: its mount is
reachable only by root, and v2 validates every block as it reads it (§15;
ROADMAP §14.8, §18.5).

A v1 header names neither its volume nor its own block number, and a v1
pointer holds only the child's block number, so v1 cannot tell a block
written to the wrong place, or one an older generation left at a reused
address, from the block its parent expects. v2 checks both (§15's
Self-describing blocks and Verified pointers rows).

---

## 6. Allocator

One `ALLOC` block (v1: `nblocks ≤ 1024` so bitmap + refcounts fit).

After the 32-byte header:

- bitmap: `(nblocks + 7) / 8` bytes, bit `i` set exactly when refcount `i`
  is above 0. Readers decide from the refcounts. v1 mount copies the bitmap
  into memory (`load_alloc`) and never tests a bit; v1 sets or clears bit
  `i` only when refcount `i` leaves or reaches 0, so a bit that disagrees
  with its refcount at mount is written back unchanged until that refcount
  next leaves or reaches 0.
- refcounts: `nblocks` bytes, `u8` each. 0 ⇒ free (bitmap clear). Super
  slots 0 and 1 are permanently allocated (ref ≥ 1).

The `ALLOC` header's entry count holds `nblocks`.

A block is reusable only when its refcount hits 0 **and** no live super
(including snapshots) still names a tree that contains it.

**Commit rule:** never overwrite a block the currently mounted super still
reaches. Allocate fresh blocks for new metadata and for replacement file
data; from ROADMAP §12.5 a file page's block is allocated when a commit
writes the page back (§10). Drop the in-memory refs on the replaced blocks only **after** the new super is
`Flush`ed.

**Space accounting:** the alloc map a commit writes already has that
commit's drops applied. It takes effect only with the new super, so the
commit rule still holds. After any sequence of commits and remounts, every
block from 2 on with a refcount above 0 is reachable from the live tree or a
snapshot. v1 code does not meet this yet:

- the alloc map it writes omits the commit's own drops, which happen only
  in memory, so the blocks the last commit of a mount session replaced stay
  allocated on disk (F049; ROADMAP §10.11)
- after the super flush, commit re-marks only the alloc and inode blocks as
  metadata, so the next commit never drops the directory blocks it wrote; a
  64-block volume stops committing after about 57 syncs (F014; ROADMAP
  §10.11)
- mount holds at most 48 metadata blocks (`MAX_META`) and fails past that,
  and commit does not check the cap, so it can write a volume that mount
  rejects (F014; ROADMAP §10.11)
- a `write` that needs a fifth extent fails and leaks a block (§13, F051;
  ROADMAP §10.11)

---

## 7. Inodes

B+tree keyed by inode number, at most two levels in v1: one `INODE_LEAF`,
or one `INODE_INT` (level 1) over at most 8 leaves. An internal entry is 8
bytes: at offset 0 the first `ino` in the child leaf (`u32`), at offset 4
the child block (`u32`); the entry count is the number of leaves. A leaf holds at most 15
records (`INODE_PER_LEAF`), sorted by `ino`. v1 code rewrites the whole
inode tree on every commit and never searches it: mount loads every record
into a 64-slot table. Leaf record is 256 bytes:

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
extents and clears the inline flag. Truncating an extent file to 128 bytes
or less moves its bytes back inline. Symlink targets use the inline area (an
empty target, or one longer than 128 bytes, is `Inval`).

v1 `truncate` does not meet the inline rule yet: growing an inline file past
128 bytes keeps the inline flag with `size` above 128. A read that reaches
past byte 128 of such a file, or a write that ends past byte 128, panics the
kernel, and mount and `fsck` accept such an inode (F062; ROADMAP §13.9).

Empty files may be inline with `inline_len = 0`.

---

## 8. Directories

B+tree keyed by name, at most two levels in v1: one `DIR_LEAF`, or one
`DIR_INT` (level 1) over at most 8 leaves. Keys are in `[u8]` lexicographic
order (`vibefs::name_cmp`): the first differing byte decides, and a name
that is a prefix of another sorts first. Not UTF-8 normalized. A leaf holds
at most 56 entries (`DENT_PER_LEAF`). `.` and `..` are **not** stored and
are invalid names (`Inval`); the VFS synthesizes them. A directory with
`dir_root = 0` is empty. Both block kinds carry the directory's inode number
in the header's owner field (§5).

v1 code rewrites the tree of every non-empty directory on every commit and
never searches it: mount loads every entry of the volume into one 96-entry
table, and lookup scans it (`find_dent`). Indexed lookup is v2 (F067;
ROADMAP §14.8).

Leaf entry (72 bytes):

| Off | Type | Field |
|-----|------|--------|
| 0 | `u32` | ino |
| 4 | `u8` | kind |
| 5 | `u8` | name length (1..64) |
| 6 | `[u8; 64]` | name, unused bytes zero |
| 70 | `u16` | reserved |

Internal entry (72 bytes; the header's entry count is the number of child
leaves):

| Off | Type | Field |
|-----|------|--------|
| 0 | `u32` | child `DIR_LEAF` block |
| 4 | `u8` | name length of the key (1..64) |
| 5 | `u8` | zero |
| 6 | `[u8; 64]` | key: the first name in that child leaf, unused bytes zero |
| 70 | `u16` | reserved, zero |

Names cannot contain `/` or NUL.

---

## 9. File data

Not metadata. No `VBLK` header. Contents are raw file bytes, block-aligned
on disk; a short last block is zero-padded on disk and truncated by `size`.
Bytes past `size`, in the last block and in the inline area, are zero. v1
`truncate` does not meet this yet: a shrink leaves the old bytes after the
new size in the last kept block and in the inline area, and a later
extension exposes them (F125; ROADMAP §13.9).

v1 always stores an extent CRC. A mismatch on read is `Corrupt`: the caller
gets an error, not the bad bytes (a syscall sees `EINVAL`, SYSCALL.md §2.1).
A write into a block whose extent fails its CRC returns `Corrupt` and
changes nothing. `fsck` lists the inode and extent. It does not copy the
corrupt payload into a new extent and call the volume clean.

v1 code does not meet the write rule yet: the overwrite path discards
`check_extent`'s result, merges the new bytes into the corrupt block, and
stores a fresh CRC, so one write hides the corruption. A split or partial
truncate of a multi-block extent also keeps the old whole-extent CRC (F063;
ROADMAP §10.11). v1 `fsck` counts a bad extent as one error and prints only
totals (§11).

Blocks written by a transaction that never committed are free under the
committed alloc map, so a crash before the super switch leaks nothing. The
leak warnings v1 `fsck` reports come from snapshots (§2), from the
accounting gaps in §6, and from blocks a failed commit allocated (§10,
F050).

---

## 10. Commit protocol

One transaction = one generation bump.

1. In memory: apply creates, writes, unlinks, snapshot records.
2. Allocate new blocks for the alloc map, the inode tree, and each dirty
   directory tree. Do not reuse blocks reachable from the current super or
   any snapshot. From ROADMAP §12.5, `write` only dirties page-cache pages,
   and the commit writes them back here: each dirty file page gets a fresh
   block, never one the current super or a snapshot reaches, the page's own
   frame is written there with step 3's blocks, and the file's in-memory
   block map records the move; the page keeps its place in the file's cache
   (DESIGN §10.6). The writeback threads write vibefs file data by starting
   a commit. Before §12.5, `write` has already allocated and written the
   replacement data blocks. v1 rewrites the whole inode tree and the tree
   of every non-empty directory, changed or not.
3. Write those blocks, and wait until every write has completed. The alloc
   map carries this commit's drops (§6).
4. `Flush`, which covers the writes step 3 waited for (DESIGN §10.2).
5. Write the inactive super slot (`generation + 1`, new roots, checksum):
   the slot opposite the super this mount last mounted or committed. The
   generation and roots change in memory only after step 6 succeeds.
6. `Flush`.
7. In memory, drop refcounts on the replaced metadata and data. Optionally
   mirror the super into the other slot (same generation) and `Flush`
   again; v1 does not mirror.

Each step starts only after the one before it has completed, since the
block layer orders nothing (DESIGN §10.2). Steps 5 and 6 together are a
`Fua` write of the superblock: on a device that offers FUA, the
superblock write carries the flag and step 6 sends nothing; on any other
device the block layer runs them as written.

Steps 1 to 4 make every allocation the commit needs, blocks and kernel
memory alike, including the memory step 7 uses; steps 5 to 7 allocate
nothing, so a commit whose super is durable always finishes its switch
in memory (DESIGN §4.4). v1's commit allocates no kernel memory,
and ROADMAP §10.11's F014 box moves its one table-slot check, `MAX_META`,
before step 5.

An error at step 5 or 6 leaves the new super's state unknown: the write
may have reached the media before the error was reported, and a later
mount picks it if it did. So the failed commit keeps the blocks it
allocated. The next commit writes the same slot, allocates none of those
blocks, and writes an alloc map that counts them free, since its own
super makes them unreachable (§6); memory frees them after its step 6.
An error before step 5 frees them at once, since no super on disk reaches
them. Freeing them at once after a step 5 or 6 error would let the retry
overwrite the tree of a super that a crash then mounts.

v1 code does not meet step 5 after a failed commit: it advances the
generation and roots in memory before the alloc write, and picks the slot
by generation parity, so the retry after a failed alloc write, first flush,
or super write overwrites the slot that holds the newest committed super (F050;
ROADMAP §12.5).

Crash:

| When | After reboot |
|------|----------------|
| During data / new-meta writes | old super still valid; its alloc map does not mark the new blocks, so they are free |
| During step 5 (torn super) | checksum fails; other slot wins |
| After step 6 | new generation is the volume |

`fsck` must not take a checksum-failing super or metadata block and treat
it as authoritative.

`fsync` / `sync` is this protocol.

A volume whose device is removed or `Failed` (DESIGN §10.3, §12) never
commits again: its last committed generation stays the on-disk state, and
the transaction in memory is dropped, with its error reported to `fsync`.

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
3. Walk inode B-tree; verify every metadata checksum. From v2, also check
   each block's header and the pointer that reached it (§15), and report a
   mismatch with the block, the generation the pointer expects, and the one
   the block holds.
4. For each inode: kind/mode sanity, extent ranges in-volume and allocated,
   inline vs size, directory B-tree walk, no name duplicates, nlink matches
   dirent count (root nlink is 1 in v1; subdirs do not store `..`).
5. For each extent: optional data CRC verify.
6. Compare reachable blocks to the refcounts and the bitmap. Allocated but
   unreachable (a leak) = warning. Free but reachable = error.
7. Exit 0 only if no error. Warnings (leaks) are printed; default is still
   0 if the live tree is checksum-clean. Corruption (bad CRC, bad magic,
   cycle, out-of-range) is nonzero **and** must not be "fixed" by using the
   bad payload.

From v2, steps 3 and 6 cover every snapshot's tree as well as the live one,
and step 6 compares the blocks they reach with the allocation map. `fsck`
also checks each snapshot's deadlist (§15): every entry is reachable from
that snapshot and not from its successor, and sits in the sub-list for its
birth range.

v1 `fsck-vibefs` does not run every step yet (F067; ROADMAP §10.11). It
mounts the volume as the kernel does, which covers steps 1 to 3 and counts a
mount failure as one error. Then it checks each extent (`len ≥ 1`,
`phys ≥ 2`, `phys + len ≤ nblocks`, data CRC), the `nlink` of each directory
except root against the entries naming it, and, for blocks 2 to `nblocks` − 1,
reachability against the refcounts. Beyond the inode kind that mount checks
(§5), it does not check mode, a dirent's kind against its inode's,
inline against size, duplicate names, regular-file `nlink`, or the bitmap.
It does not walk snapshot trees, so a block only a snapshot reaches is a leak
warning. It prints `fsck-vibefs: gen G errors E warnings W` and exits 1 on
any error.

v1 has no repair mode: `fsck-vibefs` takes no options and never writes, so
leaked blocks stay allocated. The rule for a repair mode, when one is
added: it drops only unreachable blocks, only after a successful tree walk
and with an explicit flag, and never writes over a live file's bytes.

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

v1's tests do not check this pass criterion yet:

- the host test (`crash_at_each_write_is_consistent` in `src/vibefs.rs`)
  replays one workload once per device operation N (a write or a flush),
  applies ops 1 to N in order, drops every later op, and checks only that
  `fsck` reports 0 errors
  and `mount` succeeds; the committed-prefix oracle and reordering since
  the last flush arrive with ROADMAP §12.5
- the QEMU test (`tests/harness/run_vibefs_crash.py`) passes when
  `fsck-vibefs` prints `errors 0`. It does not read `/crash/w` from the
  image or compare it with a committed prefix. The guest ignores `sync_fs`
  errors, and the 64-block image fills near generation 57 (§6, F014),
  after which rounds kill a volume that no longer changes (F080; ROADMAP
  §10.2)

---

## 13. What v1 will not do

- Data journaling or WAL
- Compression, encryption, RAID
- Block size other than 4096
- Inode extents overflow to an extent tree (4 extents is the cap; files
  that would need a fifth return `NoSpace`). v1 code writes only 1-block
  extents and never merges them, so a file it writes holds at most 4 data
  blocks (16 KiB) until v2 (ROADMAP §14.8). The `write` that needs a fifth
  extent fails and leaks the block it allocated first (F051; ROADMAP §10.11)
- POSIX ACLs, xattrs

---

## 14. Versioning

`VERSION` is 1. Readers reject any other value. Additive on-disk changes
that old readers can ignore use flag bits. Layout changes bump `VERSION`.

v1 is experimental and carries no compatibility promise. The kernel stops
mounting it when v2 becomes the root filesystem (ROADMAP §14.8), v2 readers
do not read it, and no tool converts it: no v1 volume holds data anyone
kept, since every v1 image is a CI artifact or the RAM-backed `/vibe`. From
v2 on, §15's feature-flag sets carry additive changes, and the v2 format is
the one ROADMAP §39.1 freezes.

---

## 15. Version 2 requirements

ROADMAP §14.8 writes the v2 format into this file before any v2 code, as
§8.5 did for v1. v2 is the format ROADMAP §39.1 freezes for 1.0, and every
volume written after it carries these choices for as long as it exists, so
they are decided here, before a line of v2 is written:

| Property | v2 requires | Why |
|---|---|---|
| Byte order | little-endian, as v1 | both architectures are little-endian; one byte order has no swap path to test |
| Block addresses and counts | 64-bit | v1's `u32` block numbers end at 16 TiB of 4 KiB blocks, and F008 is the same truncation in file offsets |
| Inode numbers | 64-bit, allocated in increasing order and never reused in a volume's life | Linux's `ino_t` is 64-bit; backup tools and NFS key on inode numbers, and no reuse needs no generation field |
| File size | up to 2^63 − 1 bytes | Linux's `loff_t` |
| Names | 1 to 255 bytes of any value but `/` and NUL, not normalized | Linux's `NAME_MAX`; v1's 64-byte cap follows the VFS limit that ROADMAP §13.9 raises |
| Timestamps | signed 64-bit seconds and 32-bit nanoseconds for atime, mtime, ctime, and a birth time | Linux's `statx`; v1 stores whole seconds |
| Checksums | CRC-32C (Castagnoli) over every metadata block, and over every data block on its own: one checksum per block of file data, never one per extent. Data checksums live in a checksum tree keyed by physical block number. A read loads the checksums of the blocks it reads before it submits them, so verification at completion reads nothing (DESIGN §10.1) | a hardware instruction on both architectures (SSE4.2 `crc32`, the Armv8 CRC32C instructions), and what ext4, XFS, and btrfs use; v1's CRC-32/ISO-HDLC has no instruction on x86_64. Per block, a read verifies only the blocks it reads, a write or truncate that splits an extent keeps its neighbours' checksums, and §29.4's scrub repairs one block at a time. A checksum over a whole extent would make a 4 KiB read of a 1 GiB extent read the whole extent, and every split recompute checksums over data it did not touch (F063 is v1's instance). Keyed by physical block, as btrfs keys its checksum tree, a block that snapshots and clones share has one checksum, and extent records stay fixed-size |
| Identity | the superblock holds two 128-bit ids: `uuid`, the filesystem's user-visible id (what `blkid` reports, `UUID=` names, and statfs's `f_fsid` is derived from), and `meta_uuid`, which every metadata block header carries (next row). `mkfs` sets both to one value, and nothing changes `meta_uuid` after that. `tune-vibefs --uuid`, run offline on the host or in the guest, gives a volume a new `uuid` through two commits that change nothing else, so both superblock slots carry it and no other block is written. A compat superblock flag, `template`, marks a volume built to be copied; the first boot of a copy gives it a fresh `uuid` and clears the flag (ROADMAP §26.2) | a byte copy of a volume, from a cloud image, a disk snapshot, or `dd`, carries its source's id, so two attached devices can carry one filesystem UUID, and a copy must be able to take a new id cheaply (DESIGN §10.5). A header that carried the user-visible id would make that a rewrite of every metadata block, as it was on btrfs before its `metadata_uuid` feature and on XFS before `meta_uuid`, both added after their formats shipped. To reject another volume's blocks, the header needs only an id that never changes |
| Self-describing blocks | every metadata block header carries the volume's `meta_uuid` (Identity row), its own block number, and the generation that wrote it | a checksum accepts a correct block written to the wrong place, and a block an earlier filesystem left in a reused partition; the block number and the `meta_uuid` reject them, and the generation tells `fsck` when a stray block was written. A header cannot reject a block that an older generation of this volume left at the right address, since that block describes itself correctly; its parent's pointer does (next row) |
| Verified pointers | every pointer to a metadata block of a tree (the superblock's tree roots, each internal node's children, and each snapshot root) holds the child's block number, the generation that wrote it, and its CRC-32C. A read checks all three, and the child's header, before it uses the block; a mismatch is `Corrupt`, which §29.4's mirror read retries from another copy. `fsck` reports the generation the pointer expects and the one the block holds. A data block needs no pointer check: its checksum lives in the checksum tree, which verified pointers reach, so a lost data write fails its checksum | a device can acknowledge a write and the flush after it and keep the old bytes (a firmware or virtual-disk bug, a stacked target that drops a write, a mirror member that returns a stale copy). Under copy-on-write that address held an older node of this volume, whose header and checksum are valid. The generation gives the diagnosis and the block's birth time. The checksum is needed as well because generations repeat: a commit whose superblock never became durable is followed, after a remount, by another commit of the same number, which may reuse the same addresses, and a generation-only check (btrfs's) accepts that commit's lost write. ZFS keeps a checksum in every block pointer. The two fields cost 12 bytes per pointer, about 43% of an internal node's fanout with 64-bit keys, and no write, since a copy-on-write parent is rewritten whenever its child changes |
| Reference counts | none for snapshots (next row). Sharing file extents (`FICLONE`, `FICLONERANGE`, and a sharing `copy_file_range`) is a later ro_compat feature, `clone`, which adds a reference-count tree keyed by physical block, with an entry only for a block that more than one extent references and counts of at least 32 bits. Until it lands, `FICLONE` and `FICLONERANGE` return `EOPNOTSUPP`, as Linux's do on a filesystem without shared extents, and `copy_file_range` copies bytes | v1's `u8` counts overflow once 255 snapshots share a block, and a count per block makes each snapshot write a count for every block it shares. Birth generations serve snapshots with no count, and a sparse tree costs nothing on a volume without clones; OpenZFS pairs its deadlists with a block-cloning table the same way. ro_compat, because an older reader can read a volume with shared extents but must not free one of their blocks |
| Snapshots | a snapshot is a named (1 to 255 bytes), read-only, whole-volume generation, and the snapshots' roots live in a snapshot tree with no fixed count. Space is tracked by birth generation, with no per-block count: a metadata block's birth is the generation in the pointer that reaches it (Verified pointers row), and every extent record carries the generation that wrote its blocks. A commit that frees a block born after the newest snapshot frees it; one that frees an older block appends it to the newest snapshot's deadlist, the list of blocks that snapshot reaches and its successor (the next snapshot, or the live tree) does not. Deleting a snapshot walks its own deadlist: it frees each block born after the previous snapshot and moves the rest to the previous snapshot's deadlist, so it costs time in proportion to that list. A deadlist keeps its entries in sub-lists by birth range, bounded by the older snapshots' generations, so the entries that move go over as whole sub-lists. Creating a snapshot writes a number of blocks that does not grow with the volume. A snapshot mounts read-only with the mount option `snapshot=<name>`, beside the live tree's mount of the same device; the mount has its own `st_dev` and keeps the live tree's inode numbers. Snapshots are created, listed, and deleted through ioctls on the misc device node `/dev/vibeos/vibefs-control`, which name the volume by a file descriptor open on it and which `docs/LINUX.md` lists as a native interface | v1's scheme (§2) writes a count for every block the live tree reaches at each create and walks the whole snapshot at each delete: about 2.7 × 10^8 counts per snapshot of a 1 TiB volume. A block the live tree drops is never reached again, so a block born before the newest snapshot that the live tree still reaches is reachable from that snapshot, and its birth decides whether a free is real, as in ZFS. A snapshot's inode numbers repeat the live tree's, so its own `st_dev` keeps `find`, `du`, and `tar` from taking its files for hard links of the live ones |
| Block size | a superblock field: `mkfs` writes 4096, and readers accept every power of two from 4096 to 65536 | a later default needs no format change |
| Extension | three feature-flag sets: compat (an older reader ignores the feature), ro_compat (an older reader mounts read-only), and incompat (an older reader refuses to mount); `VERSION` changes only for a change the flags cannot express | additive changes land without a version bump, as in ext4 and XFS; v1 has one flag byte |
| Validation | every check §5 lists (entry counts, pointers in range, extent bounds, inline sizes, unique inode numbers, each metadata block referenced once within a tree, since snapshots share subtrees), with the allocation map in place of v1's refcounts, so that a reachable block the map marks free is `Corrupt`, runs when a block is read, before its contents are used | F061: v1's mount trusts on-disk counts, and a crafted image must return `Corrupt`, never panic (DESIGN §2.10) |
| Directories | an on-disk B-tree per directory with no volume-wide entry cap; a lookup reads one block per level; each entry has a 64-bit position cookie that stays valid while other entries are added and removed | ROADMAP §14.8 (F067); `getdents64`'s `d_off`, `seekdir`, and NFS readdir cookies need stable positions |
| Extended attributes | stored per inode for any name, up to Linux's `XATTR_NAME_MAX` (255 bytes) and `XATTR_SIZE_MAX` (64 KiB) per value; the VFS, not the format, decides which namespaces a mount accepts | ROADMAP §14.8 adds `user.` and `trusted.`, §18.6 `security.capability`, and §21.5's overlayfs keeps its whiteouts in `trusted.`; a format that stored only some namespaces would need a version for each one added |
| Commit | §2's copy-on-write with an atomic superblock switch, where a commit writes only the blocks it changed and their ancestors, and makes every allocation before the superblock write (§10). An `fsync` is a commit. If ROADMAP §14.8's recorded verdict calls for one (ROADMAP §25.7), a per-volume intent log arrives as the compat feature `intent_log`, as btrfs's tree log and ZFS's intent log did. The format reserves its place from the start: the log lives only in the extents of one reserved inode, which no directory names and whose extents every v2 reader and `fsck` counts as allocated. Each log block carries the log's identity, a sequence number that the block before it or the superblock names, the generation its records extend, and its own CRC-32C; replay applies only records that extend the mounted generation, and stops at the first block whose sequence or checksum fails. An incompat bit, `log_replay`, is set while the log may hold records: the first commit after a read-write mount that uses the log sets it, before any `fsync` is acknowledged through the log, and the last commit of a clean unmount, a freeze, or a remount read-only clears it | v1 rewrites every inode and directory tree on every commit (§7, §8). Databases and package managers call `fsync` once per transaction, and Phase 30's gate measures PostgreSQL against Linux. A compat log with an incompat replay bit, like ext4's journal with its `needs_recovery` bit, lets a release without the log, an older rescue image, or an older `fsck-vibefs` mount and check a volume that was shut down cleanly, where an incompat feature would lock them out for the volume's life. The reserved inode keeps such a release from reusing the log's blocks, and the generation in each log block makes replay ignore records a later commit has overtaken. A log appended between commits cannot carry its blocks' checksums in a pointer written before them (Verified pointers row), so its blocks chain their own |

Rejected: carrying v1's field widths into v2 and raising limits later with
new versions that `fsck` upgrades in place. Each raise after 1.0 would be a
migration of every volume in use, where a wider field now costs a few bytes
per block.

Rejected for data checksums: one checksum per extent, as v1 stores it, for
the read and split costs above; and a checksum array inside each extent
record, which makes records variable-sized and stores a shared block's
checksum once per file that references it. Cost of the checksum tree: one
more tree lookup per data read, cached like the other metadata.

Rejected for pointers: a generation alone, which misses a lost write under
a repeated generation; a checksum alone, which detects the loss but gives
neither a diagnosis nor the birth time snapshots use; a rule that a child's
generation is at most its parent's, which a stale block passes; and a
cryptographic hash, which costs 32 bytes per pointer and a hash on every
metadata read to resist tampering, which is ROADMAP §18.7's encryption's
job, not the checksum's.

Rejected for snapshots: v1's eager count per reachable block; lazy
reference counts as btrfs keeps them, where the first write under a
snapshot updates the count of every child of each shared node it copies, on
the `fsync` path ROADMAP §14.8 measures, and a free needs back-reference
walks; a snapshot id in every key, as bcachefs keeps it, which every lookup
must filter; writable snapshots in v2.0, which no ROADMAP line needs; and
snapshot ioctls on a directory descriptor, as btrfs issues them, which are
not a native interface (ROADMAP, How to read this). Cost of the birth
scheme: one generation per extent record, and deadlist blocks sized by what
dies under snapshots.
