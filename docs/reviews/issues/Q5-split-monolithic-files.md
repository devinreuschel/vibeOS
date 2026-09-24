# Q5 · Split monolithic files; add a size guard

| | |
|---|---|
| **Area** | 4.2 Code quality & consistency |
| **Impact / Effort / Phase** | Medium / M / III |
| **Depends on** | A1 (directories exist), T1 (for `ktest.rs`) |
| **Blocks** | — |
| **Review** | [ARCHITECTURE_REVIEW.md §4.2](../ARCHITECTURE_REVIEW.md#42-code-quality--consistency) |

## Problem

- `src/ktest.rs` 4,083 lines (T1); `src/fs/mod.rs` 2,873; `src/vibefs.rs` 2,549; `src/fat.rs` 2,387; `src/fs/kernfs.rs` 1,704; `src/file_init.rs` 1,516; `src/virtio_blk_init.rs` 1,283.
- `src/vibefs.rs` has three separate `impl Vol` blocks (lines 456, 830, 1668), and `src/fs/mod.rs` two `impl Vfs` blocks (612, 1248): accretion seams that are natural split points.
- An agent editing one corner of vibefs reads 2,500 lines of context; merge conflicts concentrate in three files.

## Recommended fix

Split each big file by responsibility into a directory, with no logic changes, one file per PR; add a soft size guard so the next split happens at the next touch rather than never.

## Implementation plan

1. **`src/fs/`** → `mod.rs` (constants, `FsError`, traits, `Vfs` struct + `new`), `inode.rs` (`Inode`, `Dentry`, caches, eviction), `mount.rs` (`Super`, `Mount`, `mount/umount/mount_root*`), `walk.rs` (`resolve`, symlink handling, `split_basename`), `file.rs` (`File`, `FdTable`, `open/close/read/write/seek/truncate/readdir`), `ramfs.rs` (`RamFs`, `RamNode`, `ram_*`), `tests.rs`.
2. **`src/vibefs/`** → `mod.rs` (constants, `Error`, `Disk` trait), `disk.rs` (`MemDisk`, `CrashDisk`), `layout.rs` (`Extent`, `Inode`, `Dent`, `Snap`, `Node`, record encoders), `vol.rs` (the first `impl Vol`: state and allocator), `ops.rs` (second `impl Vol`: lookup/create/read/write), `commit.rs` (third `impl Vol`: CoW commit protocol, snapshots), `mkfs.rs`, `fsck.rs`, `tests.rs`. Keep `VERSION` in `mod.rs` so `docs/VIBEFS.md §14` still points at one place.
3. **`src/fat/`** → `mod.rs` (`FatError`, `Disk`, `MemDisk`, `FatInfo`), `chain.rs` (FAT chain cache, alloc), `dirent.rs` (8.3 + LFN, checksum), `vol.rs` (`FatVol` mount/lookup/readdir), `rw.rs` (read/write/truncate/create/unlink/rename), `mkfs.rs` (`mkfs`, `mkinitrd`), `tests.rs`.
4. **`src/fs/kernfs.rs`** → `kernfs/{mod,node,devfs,tmpfs,procfs,sysfs}.rs`.
5. **`src/virtio_blk_init.rs`** → `drivers/virtio_blk/{mod,vq,issue,irq}.rs` inside D2's PR, which rewrites most of it: the split first, as a commit that only moves code, then D2's rewrite (ROADMAP Phase 10 Order, wave 3).
6. **Size guard.** `scripts/check_file_size.py` warns above 1,500 lines and fails above 3,000; run from `make check`. Exempt nothing; the point is visibility.

## Acceptance criteria

- No source file over 1,500 lines except with a documented exemption in the script.
- Host test count (367) and in-guest count (103) unchanged after each split; `cargo test` names unchanged.
- `git diff --stat` of each split PR shows moves only (verify with `git diff --color-moved=dimmed-zebra`).

## Tests

None new; counts must stay equal.

## Risks and rollback

Splits break `git log --follow` for the moved fragments; accept it (the PR description records the mapping). One file per PR; revert one at a time.

## Out of scope

Behavioural changes in any of these modules.
