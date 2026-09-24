# A3 · Make the VFS the only file-operation dispatch point

| | |
|---|---|
| **Area** | 4.1 Architecture & module boundaries |
| **Impact / Effort / Phase** | High / L / III |
| **Depends on** | Q3 (`BootCell` for late VFS init); A1 helpful, not required |
| **Blocks** | D1 (fd tables), E2, A4 (fs cluster) |
| **Review** | [ARCHITECTURE_REVIEW.md §4.1](../ARCHITECTURE_REVIEW.md#41-architecture--module-boundaries) |

## Problem

- `src/fs/mod.rs` defines `FileSystem` and `InodeOps`, but only `RamFs` (line 290) and the kernfs skins implement `InodeOps`. `FatFs` (line 345) and `VibeFs` (line 367) implement only `fill_super`, storing a root cluster or inode in `Super.fat_clu`.
- `Vfs::ops_lookup` (line 1274) and its siblings match on `FsType` and return `FsError::NotSupp` for `FsType::Fat | FsType::Vibe`. `Vfs` carries FAT-specific helpers `fat_vol_of`, `fat_iget`, `fat_dcache` (lines 1173–1238) and owns backend storage (`ram: [RamNode; 64]`, `kern: KernState`).
- `src/file_init.rs` re-dispatches by hand: `enum Back { Fat, Vibe }`, a `Walked` struct whose FAT fields (`clu`, `dir_clu`, `dir_off`) are overloaded for vibefs (`clu: n.ino`), 39 `match … back` arms and 44 direct calls into `fat_init::*` / `vibefs_init::*`. `fat_init::route` and `vibefs_init::route` do their own path routing outside the VFS mount table.
- The VFS sits behind an IRQ-off `SpinMutex` (`src/fs_init.rs:21`), which is why the File API must drop it before block I/O (DESIGN §2.1) and why backends could not be called from inside `Vfs`.

## Recommended fix

(The maintainer deferred the `InodeOps`-vs-separate-trait question; this plan uses `InodeOps`.) Every backend implements `InodeOps`; `Vfs` dispatches through a per-superblock ops pointer; the VFS lock becomes a blocking mutex that guards the namespace tables only, and file data I/O runs on counted references with it dropped (DESIGN §2.1); `file_init` shrinks to a thin File API over `Vfs`; ramfs/kernfs storage moves out of `Vfs`.

## Implementation plan

1. **Ops pointer.** Add `ops: Option<&'static dyn InodeOps>` to `Super`; `FileSystem::fill_super` sets it. Replace every `match … fstype` in `ops_lookup / ops_create / ops_unlink / ops_read / ops_write / ops_truncate / ops_readdir / ops_stat` with `self.supers[sb].ops.ok_or(FsError::NotSupp)?.method(...)`. Keep `FsType` for `df` and display.
2. **Generic backend words on `Inode`.** Replace `data0: u32` with `key: [u32; 3]` (FAT: cluster, dir cluster, dir offset; vibefs: ino; ramfs/kernfs: node index). Delete `fat_vol_of`, `fat_iget`, `fat_dcache` once FAT uses `key`.
3. **Trait completeness.** Add `readlink`, `rename`, and `sync` to `InodeOps` (vibefs already has `readlink`/`rename`; FAT has `rename`); default `readlink` returns `NotSupp`.
4. **Kernel-side impls.** In `src/fat_init.rs`: `struct FatOps; impl InodeOps for FatOps` wrapping the existing `lookup / readdir / read / write / create / unlink / truncate / rename / sync`; each op receives its superblock and inode through the counted references the VFS lookup returned, never `&Vfs`, and finds its volume from the superblock. Same in `src/vibefs_init.rs` (`VibeOps`). Register from `fs_init::init` via `mount_root_fs(&FatFs { …, ops: &FatOps })`. The per-volume busy flag becomes the volume's `BlockingMutex` at DESIGN §2.1 level 4, taken with a plain `lock()`, which nothing force-clears and whose contention never returns `EIO` (F060, ROADMAP §10.4).
5. **Lock kind.** Change `static VFS: SpinMutex<Vfs>` to a `BlockingMutex<Vfs>` (exists in `src/sync_init.rs`) at DESIGN §2.1 level 1's mount-table position, with no spin rank. It guards the namespace tables only: it is held for a lookup, an insert, or a removal, and for a namespace change's or a dentry-cache miss's directory I/O, never across data I/O, a pipe, TTY, socket, or page wait, or a user copy. IRQ context never takes it. Because `Vfs::new` is `const` and a blocking mutex needs the scheduler, build the VFS in `fs_init::init` (after `sched: cpu0 ready`) inside a `BootCell` (Q3).
6. **Shrink `file_init`.** Rewrite `open / close / read / write / seek / stat_path / mkdir_one / mkdir_p / unlink_path / rename_path / symlink_path / link_path / truncate_path / sync_fs` as a lookup under the VFS lock that returns a counted inode or file reference, then the operation on that reference with the lock dropped (`let f = fs_init::with(|v| v.file(fid))?; f.read(buf)`); `mkdir_one`, `mkdir_p`, `unlink_path`, `rename_path`, `symlink_path`, and `link_path` run their namespace change under the lock. Delete `Back`, `Walked`, `vol_walk`, `vol_parent`, `walk_abs`, `is_kernfs`, `vfs_ls_snap`, and `route` / `routed_rest` in both backends. `OpenFile` / `FILES` become `Vfs::files` + `FdTable`.
7. **Move storage out of `Vfs`** (second PR): `ram: [RamNode]` into `ramfs.rs`, `kern: KernState` into `kernfs.rs`, each behind the same clock eviction. `Vfs` then holds inodes, dentries, supers, mounts, files only.
8. **Docs:** DESIGN §2.1 drops its not-yet-enforced note, ROADMAP §8.1 note, module headers.

## Acceptance criteria

- `grep -c 'Back::' src/file_init.rs` = 0; `grep -n 'fn route' src/fat_init.rs src/vibefs_init.rs` empty; `ops_lookup` contains no `FsType` match.
- `cat /hello.txt`, `ls /vibe`, `cp /hello.txt /vibe/h`, `cat /vibe/h` succeed through `Vfs::open` only (new ktest `vfs_backends_via_ops`).
- In-guest `fat_initrd`, `vibefs`, `vfs_walk`, `pseudo_fs`, `shell_dispatch` green; host `fs` tests green plus a new `FakeBlockFs` host test exercising the ops pointer.
- `make test-vibefs-crash` green (the crash loop now goes through the File API and `Vfs`).
- In-guest `vfs_io_off_lock`: a read of a `vda` file held at a `kernel_tests` hook inside the backend delays neither an `open` of a tmpfs path nor a tmpfs `read` on another CPU.

## Tests

Host: `FakeBlockFs` implementing `InodeOps` mounted at `/blk`; walk, open, read, readdir through `Vfs`. In-guest: the acceptance test; a mount/umount cycle of a vibefs on `vda` on a thread started with `spawn`'s 16 KiB stack; and two kernel threads reading and writing FAT files on `vda` through `Vfs` while a `kernel_tests` hook delays each block request by 2 s, neither getting `EIO`.

## Risks and rollback

The lock-kind change is the risky step. Land steps 1–4 first (backends still called with the lock dropped, through the ops pointer), then step 5 as its own PR with `vfs_io_off_lock`.

## Out of scope

Per-process fd tables (D1, ROADMAP §10.4); page-cache unification (ROADMAP §12.5).
