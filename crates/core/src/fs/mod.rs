//! VFS: inodes, dentries, mounts, open files, path walk. ROADMAP §8.1 /
//! §8.4 / §10.4.
//!
//! Bounded caches with clock eviction. Path walk is iterative with a
//! symlink-depth cap (loop → [`FsError::Loop`], not stack smash).
//! [`RamFs`], whose store sits behind its own [`Guarded`] lock, is enough
//! to unit-test walks. Pseudo filesystems share [`kernfs`] (one dir tree,
//! four skins).
//!
//! Dispatch: a superblock's `ops` pointer ([`InodeOps`]) is the only way
//! to a backend, and no backend receives `&Vfs`: an op gets an [`OpCx`]
//! and copies of the [`Inode`]s it acts on. An inode carries its
//! backend's identity (`key`) and two private words; `Vfs` owns inodes,
//! dentries, mounts, superblocks and open files, and nothing
//! backend-specific. A dentry counts its holders, so a directory stays in
//! the cache while a child names it.
//!
//! Locking: `Vfs` itself only takes short locked steps. [`FileApi`] is
//! the one driver: under the lock ([`Guarded`]) a step resolves what it
//! can from the caches and returns counted references and the backend
//! call to make ([`Call`], [`SbCall`], a [`Walker`] lookup); the driver
//! drops the lock, makes the call, retakes the lock and commits. So no
//! backend runs under the VFS lock, the kernel's sleeping
//! `BlockingMutex` over the namespace tables (DESIGN §2.1), and a backend
//! may wait for its volume and its disk. The words data I/O changes (an
//! inode's size, link count and private words, [`InodeWords`]) sit
//! outside the lock: a backend reads and writes them through the inode it
//! is handed ([`Inode::words`]) under its own volume lock, and never
//! takes the VFS lock under a volume lock. The last put of an unlinked
//! inode and the last unmount of a superblock run the backend with the
//! lock dropped too; the slot stays reserved until the hook returns.
//!
//! Tables are static; do not allocate under the lock. No FS work from hard IRQ (DESIGN §2.2).

mod error;
pub mod fat;
mod file;
mod inode;
pub mod kernfs;
mod mount;
mod ramfs;
mod sizes;
pub mod vibefs;
mod walk;

pub use error::FsError;
pub use file::{FileId, FileRef, SeekFrom};
pub use ramfs::{RamFs, RamState};
pub use sizes::VfsSizes;
#[cfg(test)]
pub(crate) use sizes::{SMALL, host_vfs};
pub use walk::{DirRef, WalkBase, WalkCall, WalkReply, WalkStep, Walker, split_basename};

use crate::atomic::statics::{AtomicU64, Ordering};
use crate::dev::{Instance, same_instance};
use crate::kalloc::{AllocError, TryVec};
use crate::limits;

pub use crate::limits::MAX_DENTRIES;
pub use crate::limits::MAX_FDS;
pub use crate::limits::MAX_INODES;
pub use crate::limits::MAX_MOUNTS;
pub use crate::limits::MAX_NAME;
pub use crate::limits::MAX_OPEN_FILES as MAX_FILES;
pub use crate::limits::MAX_PATH;
pub use crate::limits::MAX_RAM_NODES;
pub use crate::limits::MAX_SYMLINK;
pub use crate::limits::MAX_TMPFS_DIR_ENTS as MAX_DIR_ENTS;
pub use crate::limits::MAX_TMPFS_FILE_BYTES as MAX_FILE_BYTES;
pub use crate::limits::MAX_WALK;

pub const S_IFMT: u16 = 0o170000;
pub const S_IFREG: u16 = 0o100000;
pub const S_IFDIR: u16 = 0o040000;
pub const S_IFCHR: u16 = 0o020000;
pub const S_IFBLK: u16 = 0o060000;
pub const S_IFLNK: u16 = 0o120000;
pub const S_IRWXU: u16 = 0o700;
pub const S_IRWXG: u16 = 0o070;
pub const S_IRWXO: u16 = 0o007;
pub const S_IFREG_MODE: u16 = S_IFREG | 0o644;
pub const S_IFDIR_MODE: u16 = S_IFDIR | 0o755;
pub const S_IFLNK_MODE: u16 = S_IFLNK | 0o777;

pub const O_RDONLY: u32 = 0;
pub const O_WRONLY: u32 = 1;
pub const O_RDWR: u32 = 2;
pub const O_ACCMODE: u32 = 3;
pub const O_CREAT: u32 = 0x40;
pub const O_EXCL: u32 = 0x80;
pub const O_TRUNC: u32 = 0x200;
pub const O_APPEND: u32 = 0x400;
pub const O_DIRECTORY: u32 = 0x10000;
pub const O_NOFOLLOW: u32 = 0x20000;
/// Linux `O_CLOEXEC`. Process fd table turns this into `FD_CLOEXEC`.
pub const O_CLOEXEC: u32 = 0x80000;

// `lseek`'s `whence`, from Linux `include/uapi/linux/fs.h`.
pub const SEEK_SET: u32 = 0;
pub const SEEK_CUR: u32 = 1;
pub const SEEK_END: u32 = 2;
/// The next data at or after the offset.
pub const SEEK_DATA: u32 = 3;
/// The next hole at or after the offset; Linux's last `whence`.
pub const SEEK_HOLE: u32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InodeKind {
    Reg,
    Dir,
    Lnk,
    Chr,
    Blk,
}

impl InodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            InodeKind::Reg => "reg",
            InodeKind::Dir => "dir",
            InodeKind::Lnk => "lnk",
            InodeKind::Chr => "chr",
            InodeKind::Blk => "blk",
        }
    }

    pub fn ifmt(self) -> u16 {
        match self {
            InodeKind::Reg => S_IFREG,
            InodeKind::Dir => S_IFDIR,
            InodeKind::Lnk => S_IFLNK,
            InodeKind::Chr => S_IFCHR,
            InodeKind::Blk => S_IFBLK,
        }
    }

    pub fn from_mode(mode: u16) -> Self {
        match mode & S_IFMT {
            S_IFDIR => InodeKind::Dir,
            S_IFLNK => InodeKind::Lnk,
            S_IFCHR => InodeKind::Chr,
            S_IFBLK => InodeKind::Blk,
            _ => InodeKind::Reg,
        }
    }
}

/// A filesystem type, for `df` and display. `Vfs` never dispatches on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FsType {
    #[default]
    Ram,
    Fat,
    Vibe,
    Dev,
    Tmp,
    Proc,
    Sys,
}

impl FsType {
    pub fn as_str(self) -> &'static str {
        match self {
            FsType::Ram => "ramfs",
            FsType::Fat => "fat32",
            FsType::Vibe => "vibefs",
            FsType::Dev => "devfs",
            FsType::Tmp => "tmpfs",
            FsType::Proc => "procfs",
            FsType::Sys => "sysfs",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Name {
    buf: [u8; MAX_NAME],
    len: u8,
}

impl Name {
    pub const EMPTY: Self = Self {
        buf: [0; MAX_NAME],
        len: 0,
    };

    pub fn from_bytes(s: &[u8]) -> Result<Self, FsError> {
        if s.is_empty() {
            return Err(FsError::Inval);
        }
        if s.len() > MAX_NAME {
            return Err(FsError::NameTooLong);
        }
        let mut i = 0usize;
        while i < s.len() {
            if s[i] == 0 || s[i] == b'/' {
                return Err(FsError::Inval);
            }
            i += 1;
        }
        let mut n = Self::EMPTY;
        n.buf[..s.len()].copy_from_slice(s);
        n.len = s.len() as u8;
        Ok(n)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }

    pub fn eq_bytes(&self, s: &[u8]) -> bool {
        self.as_bytes() == s
    }

    pub fn is_dot(&self) -> bool {
        self.len == 1 && self.buf[0] == b'.'
    }

    pub fn is_dotdot(&self) -> bool {
        self.len == 2 && self.buf[0] == b'.' && self.buf[1] == b'.'
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub ino: u32,
    pub kind: InodeKind,
    pub mode: u16,
    pub nlink: u32,
    pub size: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dirent {
    pub ino: u32,
    pub kind: InodeKind,
    pub name: Name,
}

impl Dirent {
    pub const EMPTY: Self = Self {
        ino: 0,
        kind: InodeKind::Reg,
        name: Name::EMPTY,
    };
}

/// One entry the File API's `readdir` reports.
pub type DirEntry = Dirent;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathRef {
    pub mount: u8,
    pub dslot: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct VfsStats {
    pub d_evicts: u32,
    pub i_evicts: u32,
    /// Files [`Vfs::open`] opened on a resolved dentry.
    pub opens: u32,
    /// Superblock `sync`s that failed at their last unmount; the unmount
    /// went on (DESIGN §2.5: a counter, and the caller's rate-limited
    /// line).
    pub sync_errs: u32,
}

/// A backend's identity for one of its inodes, unique within its
/// superblock: FAT's dirent location `[dir_clu, dir_off, 0]`, vibefs's
/// inode number `[ino, 0, 0]`, ramfs's and kernfs's node `[node, 0, 0]`.
pub type Key = [u32; 3];

/// What a backend reports about one of its inodes; [`Vfs`] fills an
/// [`Inode`] from it the first time the key is looked up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InodeInfo {
    pub key: Key,
    pub ino: u32,
    pub kind: InodeKind,
    pub mode: u16,
    pub nlink: u32,
    pub size: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub private: [u64; 2],
}

/// What an [`InodeOps`] call may touch besides the inodes it is handed:
/// a copy of its superblock's private words (only `fill_super`'s writes
/// are kept) and the clock. Never the [`Vfs`].
pub struct OpCx<'a> {
    pub sb: u8,
    pub fstype: FsType,
    pub private: &'a mut [u64; 2],
    pub now: u64,
    /// The superblock's volume instance, when its backend gave one.
    pub vol: Option<&'a Instance>,
}

/// A lock a backend's store, or the [`Vfs`], sits behind: a seam over the
/// kernel's `SpinMutex` (the stores) and `BlockingMutex` (the VFS) and, in
/// host tests, `std::sync::Mutex`, not a lock of its own.
pub trait Guarded<T> {
    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R;
}

#[cfg(test)]
impl<T> Guarded<T> for std::sync::Mutex<T> {
    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let mut g = self.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut g)
    }
}

/// What a rename's walks found: the key of the inode `oname` named, and
/// of the one `nname` named (`None` for none). The VFS holds those two
/// and commits the move on them, so the backend moves and replaces no
/// other: a racing change can get between the walks and the backend
/// call, which runs with the VFS lock dropped.
///
/// `src_words` and `tgt_words` are those two inodes' slot words, which
/// the VFS's holds keep for the call. The VFS commits the move only after
/// the backend call returns, so a write on either inode can run in
/// between; a backend that keys an inode by where its name is (FAT)
/// points the moved inode's words at its new name, and marks the replaced
/// one's as having none, under its own lock in the call.
#[derive(Clone, Copy, Debug)]
pub struct RenameSeen {
    pub src: Key,
    pub tgt: Option<Key>,
    pub src_words: Option<&'static InodeWords>,
    pub tgt_words: Option<&'static InodeWords>,
}

impl RenameSeen {
    /// `Stale` unless `src` and `tgt`, the keys of what `oname` and
    /// `nname` name in the backend's store now (`None` for nothing), are
    /// what the walks found. Nothing has changed then, and the VFS walks
    /// both names again.
    pub fn check(&self, src: Option<Key>, tgt: Option<Key>) -> Result<(), FsError> {
        if src == Some(self.src) && tgt == self.tgt {
            Ok(())
        } else {
            Err(FsError::Stale)
        }
    }
}

/// Per-inode ops, reached only through a superblock's `ops` pointer.
/// Each call gets copies of the inodes it acts on, whose slots the caller
/// holds a count on for the call, and runs with the VFS lock dropped
/// ([`FileApi`]); what it changes in a copy's public fields is written
/// back after it.
///
/// A default is the operation missing, and returns Linux's errno for that
/// operation (ROADMAP §10.4, A3, E2): making an object or a link, or
/// removing or renaming one, is `Perm`, except a regular file, whose
/// creation is `Acces`, as `open(O_CREAT)` in Linux's `/proc`; reading,
/// writing, truncating, or reading a link of an object that cannot is
/// `Inval`; looking up or listing in a non-directory is `NotDir`; `sync`
/// with nothing to write, `getattr`, `evict` and `check_seek` succeed, and
/// `can_rw` reports both operations.
pub trait InodeOps: Sync {
    fn lookup(&self, _cx: &mut OpCx<'_>, _dir: &Inode, _name: &[u8]) -> Result<InodeInfo, FsError> {
        Err(FsError::NotDir)
    }
    fn create(
        &self,
        _cx: &mut OpCx<'_>,
        _dir: &mut Inode,
        _name: &[u8],
        kind: InodeKind,
        _mode: u16,
        _target: Option<&[u8]>,
    ) -> Result<InodeInfo, FsError> {
        Err(match kind {
            InodeKind::Reg => FsError::Acces,
            InodeKind::Dir | InodeKind::Lnk | InodeKind::Chr | InodeKind::Blk => FsError::Perm,
        })
    }
    fn unlink(&self, _cx: &mut OpCx<'_>, _dir: &mut Inode, _name: &[u8]) -> Result<(), FsError> {
        Err(FsError::Perm)
    }
    /// Remove the empty directory `name` from `dir`.
    fn rmdir(&self, _cx: &mut OpCx<'_>, _dir: &mut Inode, _name: &[u8]) -> Result<(), FsError> {
        Err(FsError::Perm)
    }
    /// Give `target` the further name `name` in `dir`.
    fn link(
        &self,
        _cx: &mut OpCx<'_>,
        _dir: &mut Inode,
        _name: &[u8],
        _target: &mut Inode,
    ) -> Result<(), FsError> {
        Err(FsError::Perm)
    }
    /// Move `oname` in `odir` to `nname` in `ndir`, replacing what
    /// `nname` names, as rename(2) does. `seen` is what the VFS's walks
    /// found at the two names, the inodes it holds and accounts for:
    /// before it changes anything, under its own lock, the backend checks
    /// that the names still name them ([`RenameSeen::check`]), so an
    /// inode the move replaces is always the one `Vfs` held for `nname`,
    /// released at its last put. The moved inode's new key when the move
    /// changed it, as FAT's dirent-location key does.
    fn rename(
        &self,
        _cx: &mut OpCx<'_>,
        _odir: &mut Inode,
        _oname: &[u8],
        _ndir: &mut Inode,
        _nname: &[u8],
        _seen: RenameSeen,
    ) -> Result<Option<Key>, FsError> {
        Err(FsError::Perm)
    }
    fn read(
        &self,
        _cx: &mut OpCx<'_>,
        _ino: &mut Inode,
        _off: u64,
        _buf: &mut [u8],
    ) -> Result<usize, FsError> {
        Err(FsError::Inval)
    }
    fn write(
        &self,
        _cx: &mut OpCx<'_>,
        _ino: &mut Inode,
        _off: u64,
        _buf: &[u8],
    ) -> Result<usize, FsError> {
        Err(FsError::Inval)
    }
    /// Write `buf` at the end of the file, as `O_APPEND` does; the count
    /// written and the offset written at. A backend that serializes its
    /// writes reads the size in the same section as the write.
    fn write_append(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        buf: &[u8],
    ) -> Result<(usize, u64), FsError> {
        let off = ino.size;
        self.write(cx, ino, off, buf).map(|n| (n, off))
    }
    fn truncate(&self, _cx: &mut OpCx<'_>, _ino: &mut Inode, _size: u64) -> Result<(), FsError> {
        Err(FsError::Inval)
    }
    fn readdir(
        &self,
        _cx: &mut OpCx<'_>,
        _dir: &Inode,
        _cookie: u64,
        _out: &mut Dirent,
    ) -> Result<Option<u64>, FsError> {
        Err(FsError::NotDir)
    }
    fn getattr(&self, _cx: &mut OpCx<'_>, _ino: &mut Inode) -> Result<(), FsError> {
        Ok(())
    }
    fn readlink(
        &self,
        _cx: &mut OpCx<'_>,
        _ino: &Inode,
        _buf: &mut [u8],
    ) -> Result<usize, FsError> {
        Err(FsError::Inval)
    }
    /// Write the superblock's dirty state to its device.
    fn sync(&self, _cx: &mut OpCx<'_>) -> Result<(), FsError> {
        Ok(())
    }
    /// Release the storage of an unhashed inode: no links and no
    /// references. An `Err` leaves the inode unhashed for `umount` to
    /// retry.
    fn evict(&self, _cx: &mut OpCx<'_>, _ino: &Inode) -> Result<(), FsError> {
        Ok(())
    }
    /// Release the backend state of a superblock whose last mount is
    /// gone, after its `sync`: the volume it shows is retired here, and the
    /// superblock's own count on the volume instance goes when its slot is
    /// freed. Runs with the VFS lock dropped, never before the unmount
    /// succeeded.
    fn release(&self, _cx: &mut OpCx<'_>) {}
    /// Whether the dentry cache's name `cached` is the name `asked` a
    /// lookup gives: byte equality, or the backend's own rule, as FAT's
    /// case-insensitive one, so a lookup by another spelling finds the
    /// dentry a mount is on.
    fn name_eq(&self, cached: &[u8], asked: &[u8]) -> bool {
        cached == asked
    }
    /// Whether `ino` can seek: `SPipe` for an object that cannot, as a
    /// console, which `lseek` then refuses.
    fn check_seek(&self, _cx: &mut OpCx<'_>, _ino: &Inode) -> Result<(), FsError> {
        Ok(())
    }
    /// Whether `ino` has a read and a write operation, `(read, write)`:
    /// one it lacks makes `read` or `write` `Inval` before the buffer is
    /// looked at, as Linux's `FMODE_CAN_READ` and `FMODE_CAN_WRITE` do.
    fn can_rw(&self, _cx: &mut OpCx<'_>, _ino: &Inode) -> (bool, bool) {
        (true, true)
    }
}

/// The ops of a superblock whose filesystem has none: every operation is
/// the trait's default, the missing operation's errno.
pub struct NoOps;

impl InodeOps for NoOps {}

/// Mount-time half of a filesystem: its ops pointer, and the root inode
/// `fill_super` reports after setting up the superblock's private words.
/// Every hook runs with the VFS lock dropped.
pub trait FileSystem: Sync {
    fn name(&self) -> &'static str;
    fn fstype(&self) -> FsType;
    fn ops(&'static self) -> Option<&'static dyn InodeOps>;
    fn fill_super(&self, cx: &mut OpCx<'_>) -> Result<InodeInfo, FsError>;
    /// The largest offset a file may reach (Linux's `s_maxbytes`).
    fn max_bytes(&self) -> u64 {
        u64::MAX
    }
    /// After each mount of the superblock on `at`, a new one or a second
    /// mount of a shared one.
    fn on_mount(&self, _cx: &mut OpCx<'_>, _at: &[u8]) {}
    /// After the mount on `at` is gone; `last` when it was the
    /// superblock's last mount, before its `sync` and `release`.
    fn on_umount(&self, _cx: &mut OpCx<'_>, _at: &[u8], _last: bool) {}
}

/// A counted reference to a [`Vfs`] inode, from [`Vfs::iget_key`]. Hand
/// it back to [`FileApi::put`] or [`Vfs::put_ref`]: dropping it leaks the
/// count.
#[must_use]
#[derive(Debug)]
pub struct InodeRef {
    slot: u16,
    r#gen: u32,
}

impl InodeRef {
    /// A copyable name for this reference's inode, checked against the
    /// slot's generation on every use.
    pub fn handle(&self) -> InodeHandle {
        InodeHandle {
            slot: self.slot,
            r#gen: self.r#gen,
        }
    }
}

/// A copyable, generation-checked name for a referenced [`Vfs`] inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InodeHandle {
    slot: u16,
    r#gen: u32,
}

/// The words of one inode slot that change during data I/O: its size,
/// its link count, and its backend's two private words. They sit beside
/// [`Vfs`], outside its lock, one per inode slot: a backend reaches them
/// through the inode an op is handed ([`Inode::words`]), whose slot the
/// op's count keeps, and reads and writes them under its own volume lock
/// with the VFS lock dropped (DESIGN §2.1). `Vfs` fills them when it
/// fills the slot, writes back what an op changed in its copy, and reads
/// the size and link count from them.
#[derive(Debug)]
pub struct InodeWords {
    size: AtomicU64,
    nlink: AtomicU64,
    private: [AtomicU64; 2],
}

impl InodeWords {
    pub const fn new() -> Self {
        Self {
            size: AtomicU64::new(0),
            nlink: AtomicU64::new(0),
            private: [AtomicU64::new(0), AtomicU64::new(0)],
        }
    }

    pub fn size(&self) -> u64 {
        self.size.load(Ordering::Acquire)
    }

    pub fn set_size(&self, size: u64) {
        self.size.store(size, Ordering::Release);
    }

    /// The link count, which only `Vfs` changes.
    pub fn nlink(&self) -> u32 {
        self.nlink.load(Ordering::Acquire) as u32
    }

    fn set_nlink(&self, n: u32) {
        self.nlink.store(u64::from(n), Ordering::Release);
    }

    pub fn private(&self) -> [u64; 2] {
        [
            self.private[0].load(Ordering::Acquire),
            self.private[1].load(Ordering::Acquire),
        ]
    }

    pub fn set_private(&self, p: [u64; 2]) {
        self.private[0].store(p[0], Ordering::Release);
        self.private[1].store(p[1], Ordering::Release);
    }
}

impl Default for InodeWords {
    fn default() -> Self {
        Self::new()
    }
}

/// Two words are equal only as the same slot's words.
impl PartialEq for InodeWords {
    fn eq(&self, other: &Self) -> bool {
        core::ptr::eq(self, other)
    }
}

impl Eq for InodeWords {}

/// One [`InodeWords`] per inode slot, which a [`Vfs`] borrows for its
/// life as a `&'static [InodeWords]`: a heap table the kernel keeps in a
/// `BootCell`, a leaked one in host tests.
pub type WordsTable = TryVec<InodeWords>;

/// A table of `n` zeroed words.
pub fn words_table(n: usize) -> Result<WordsTable, AllocError> {
    limits::table(n, InodeWords::new)
}

/// Where an unhashed, unreferenced inode is in its release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rel {
    No,
    /// Its last put is done; a driver runs its backend's `evict` next.
    Queued,
    /// A driver is running its `evict`.
    Running,
    /// Its `evict` failed; `umount` retries it.
    Failed,
}

/// An in-core inode. `key` is the backend's identity for it and
/// `private` two words only its backend reads or writes; the rest is the
/// metadata `stat` reports. `gen` changes each time the slot is filled,
/// so an [`InodeHandle`] to an earlier occupant is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inode {
    used: bool,
    clock: bool,
    rel: Rel,
    refs: u16,
    sb: u8,
    slot: u16,
    r#gen: u32,
    pub key: Key,
    pub ino: u32,
    pub kind: InodeKind,
    pub mode: u16,
    pub nlink: u32,
    pub size: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub private: [u64; 2],
    /// The slot's words while it is used; a copy carries its slot's.
    words: Option<&'static InodeWords>,
}

impl Inode {
    const EMPTY: Self = Self {
        used: false,
        clock: false,
        rel: Rel::No,
        refs: 0,
        sb: 0,
        slot: 0,
        r#gen: 0,
        key: [0; 3],
        ino: 0,
        kind: InodeKind::Reg,
        mode: 0,
        nlink: 0,
        size: 0,
        atime: 0,
        mtime: 0,
        ctime: 0,
        private: [0; 2],
        words: None,
    };

    /// The words of this inode's slot, which an op reads and writes under
    /// its own lock ([`InodeWords`]); `Io` for an empty slot.
    pub fn words(&self) -> Result<&'static InodeWords, FsError> {
        self.words.ok_or(FsError::Io)
    }

    /// The size: its slot's word, which a backend may change with the
    /// VFS lock dropped.
    fn cur_size(&self) -> u64 {
        self.words.map_or(self.size, InodeWords::size)
    }

    /// A copy with the size, link count and private words its slot's
    /// words hold now.
    fn fresh(&self) -> Inode {
        let mut n = *self;
        if let Some(w) = self.words {
            n.size = w.size();
            n.nlink = w.nlink();
            n.private = w.private();
        }
        n
    }

    /// Set the link count, in the slot's words too.
    fn set_nlink(&mut self, n: u32) {
        self.nlink = n;
        if let Some(w) = self.words {
            w.set_nlink(n);
        }
    }

    /// The generation-checked name of the slot this inode, or the inode
    /// this is a copy of, occupies.
    pub fn handle(&self) -> InodeHandle {
        InodeHandle {
            slot: self.slot,
            r#gen: self.r#gen,
        }
    }

    fn stat(&self) -> Stat {
        Stat {
            ino: self.ino,
            kind: self.kind,
            mode: self.mode,
            nlink: self.nlink,
            size: self.cur_size(),
            atime: self.atime,
            mtime: self.mtime,
            ctime: self.ctime,
        }
    }

    /// Write back into `self` the public fields an op changed in its copy:
    /// those where `after` differs from `before`. The key never changes
    /// this way.
    fn merge(&mut self, before: &Inode, after: &Inode) {
        if after.ino != before.ino {
            self.ino = after.ino;
        }
        if after.kind != before.kind {
            self.kind = after.kind;
        }
        if after.mode != before.mode {
            self.mode = after.mode;
        }
        if after.nlink != before.nlink {
            self.set_nlink(after.nlink);
        }
        if after.size != before.size {
            self.size = after.size;
            if let Some(w) = self.words {
                w.set_size(after.size);
            }
        }
        if after.atime != before.atime {
            self.atime = after.atime;
        }
        if after.mtime != before.mtime {
            self.mtime = after.mtime;
        }
        if after.ctime != before.ctime {
            self.ctime = after.ctime;
        }
        if after.private != before.private {
            self.private = after.private;
            if let Some(w) = self.words {
                w.set_private(after.private);
            }
        }
    }
}

/// A dentry cache slot. `refs` counts its holders (DESIGN §2.11 rule 2):
/// each child dentry, positive or negative, each mount whose `mp_dslot`
/// it is, the superblock for its root dentry, each open file and held
/// path on it, and explicit holds. Only a `refs == 0` dentry is evicted,
/// so a slot a child names as `parent` is never reused. A superblock's
/// root dentry is its own `parent`, and a dentry belongs to its
/// superblock, whichever mount shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Dentry {
    used: bool,
    clock: bool,
    negative: bool,
    /// Its name is gone (an unlink or rename while it was held): no
    /// lookup finds it, and its last put evicts it.
    dead: bool,
    refs: u16,
    parent: u16,
    sb: u8,
    name: Name,
    islot: u16,
}

impl Dentry {
    const EMPTY: Self = Self {
        used: false,
        clock: false,
        negative: true,
        dead: false,
        refs: 0,
        parent: 0,
        sb: 0,
        name: Name::EMPTY,
        islot: 0,
    };

    fn is_root(&self, slot: u16) -> bool {
        self.parent == slot
    }
}

/// A filesystem instance (DESIGN §2.11). `ops` is the only dispatch to
/// its backend; `private` holds two words only the backend reads or
/// writes. `refs` counts its mounts and its used inode slots; it lives
/// until its last mount goes. A block device's superblock records the
/// device and its read-only flag, and a second mount of the device
/// shares it. `busy` counts the superblock hooks in flight (`fill_super`,
/// `on_mount`, `sync`, the last unmount's), each with the VFS lock
/// dropped: the slot stays while any runs.
/// Not `Copy`: it holds a counted reference to its volume.
struct Super {
    used: bool,
    /// Its `fill_super` runs; not mounted yet.
    filling: bool,
    /// Its last mount is gone; its unmount hooks run.
    dying: bool,
    refs: u16,
    busy: u16,
    fs: Option<&'static dyn FileSystem>,
    ops: Option<&'static dyn InodeOps>,
    private: [u64; 2],
    dev: Option<u64>,
    ro: bool,
    maxbytes: u64,
    root_islot: u16,
    root_dslot: u16,
    /// The backend's volume instance, which the superblock holds a count
    /// on; the backend reads it from each op's [`OpCx::vol`].
    vol: Option<Instance>,
}

impl Super {
    const EMPTY: Self = Self {
        used: false,
        filling: false,
        dying: false,
        refs: 0,
        busy: 0,
        fs: None,
        ops: None,
        private: [0; 2],
        dev: None,
        ro: false,
        maxbytes: 0,
        root_islot: 0,
        root_dslot: 0,
        vol: None,
    };

    fn live(&self) -> bool {
        self.used && !self.filling && !self.dying
    }
}

/// A mount of a superblock on a directory (DESIGN §2.11). It holds one
/// count on its superblock; `refs` counts the open files, child mounts,
/// held paths and directory references ([`DirRef`]: working directories
/// and roots) that reach the filesystem through this mount, and
/// `umount` is `Busy` while it is not zero. A
/// mountpoint is found by its parent mount and dentry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mount {
    used: bool,
    /// Held for a mount whose `fill_super` runs.
    reserved: bool,
    parent: Option<u8>,
    mp_dslot: u16,
    root_dslot: u16,
    sb: u8,
    refs: u16,
}

impl Mount {
    const EMPTY: Self = Self {
        used: false,
        reserved: false,
        parent: None,
        mp_dslot: 0,
        root_dslot: 0,
        sb: 0,
        refs: 0,
    };
}

/// What a mount made: the mount, its superblock, and whether the
/// superblock was already mounted from the same device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mounted {
    pub mount: u8,
    pub sb: u8,
    pub shared: bool,
}

/// An open-file table slot. It counts one reference to its inode and one
/// to its mount, and pins its dentry when it was opened by path. `gen`
/// changes when the slot is freed, so a [`FileId`] to an earlier file is
/// refused with `Badf` (C-FDGEN). The size lives in the inode, never
/// here. A slot is free, `reserved` by an `open` that has not created or
/// truncated anything yet, or `used`. `rw` is what the backend can do to
/// the inode, `(read, write)`, from [`InodeOps::can_rw`] at the open.
#[derive(Clone, Copy)]
struct File {
    used: bool,
    reserved: bool,
    refs: u16,
    r#gen: u16,
    islot: u16,
    dslot: Option<u16>,
    mount: u8,
    flags: OpenFlags,
    offset: u64,
    rw: (bool, bool),
}

impl File {
    const EMPTY: Self = Self {
        used: false,
        reserved: false,
        refs: 0,
        r#gen: 0,
        islot: 0,
        dslot: None,
        mount: 0,
        flags: OpenFlags(0),
        offset: 0,
        rw: (true, true),
    };
}

/// Open flags: Linux's `O_*` bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenFlags(u32);

impl OpenFlags {
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    fn has(self, f: u32) -> bool {
        self.0 & f != 0
    }

    fn reads(self) -> bool {
        self.0 & O_ACCMODE != O_WRONLY
    }

    fn writes(self) -> bool {
        self.0 & O_ACCMODE != O_RDONLY
    }
}

/// Phase 9 hangs an [`FdTable`] on a process. Slice A owns the shape. A
/// heap row of a fixed length, built by [`FdTable::try_new`];
/// [`FdTable::new`] has no room.
pub struct FdTable {
    fds: TryVec<u16>,
}

impl FdTable {
    /// A table with no room, for a `const` initializer.
    pub const fn new() -> Self {
        Self { fds: TryVec::new() }
    }

    /// A table of `len` closed descriptors.
    pub fn try_new(len: usize) -> Result<Self, AllocError> {
        Ok(Self {
            fds: limits::table(len, || 0)?,
        })
    }

    pub fn install(&mut self, fid: u16) -> Result<u32, FsError> {
        if fid as usize >= MAX_FILES {
            return Err(FsError::Badf);
        }
        let i = self
            .fds
            .iter()
            .position(|&f| f == 0)
            .ok_or(FsError::NoSpace)?;
        self.fds[i] = fid + 1;
        Ok(i as u32)
    }

    pub fn get(&self, fd: u32) -> Result<u16, FsError> {
        match self.fds.get(fd as usize) {
            Some(&f) if f != 0 => Ok(f - 1),
            _ => Err(FsError::Badf),
        }
    }

    pub fn take(&mut self, fd: u32) -> Result<u16, FsError> {
        let fid = self.get(fd)?;
        self.fds[fd as usize] = 0;
        Ok(fid)
    }

    pub fn dup(&mut self, fd: u32) -> Result<u32, FsError> {
        let fid = self.get(fd)?;
        self.install(fid)
    }
}

impl Default for FdTable {
    fn default() -> Self {
        Self::new()
    }
}

/// The VFS's namespace tables: heap tables of the lengths [`Vfs::new`]
/// was given, built once and never grown (ROADMAP §10.4, D1).
pub struct Vfs {
    inodes: TryVec<Inode>,
    words: &'static [InodeWords],
    dentries: TryVec<Dentry>,
    supers: TryVec<Super>,
    mounts: TryVec<Mount>,
    files: TryVec<File>,
    ihand: u16,
    dhand: u16,
    pub now: u64,
    pub stats: VfsStats,
}

impl Vfs {
    /// An empty VFS with tables of `sizes`' lengths, whose inode slots'
    /// words are `words`'s: one per inode slot, so the inode table is
    /// `sizes.inodes` long or `words.len()`, whichever is shorter.
    pub fn new(sizes: &VfsSizes, words: &'static [InodeWords]) -> Result<Self, AllocError> {
        Ok(Self {
            inodes: limits::table(sizes.inodes.min(words.len()), || Inode::EMPTY)?,
            words,
            dentries: limits::table(sizes.dentries, || Dentry::EMPTY)?,
            supers: limits::table(sizes.mounts, || Super::EMPTY)?,
            mounts: limits::table(sizes.mounts, || Mount::EMPTY)?,
            files: limits::table(sizes.files, || File::EMPTY)?,
            ihand: 0,
            dhand: 0,
            now: 0,
            stats: VfsStats {
                d_evicts: 0,
                i_evicts: 0,
                opens: 0,
                sync_errs: 0,
            },
        })
    }
}

/// A backend call prepared under the VFS lock and made with it dropped
/// ([`FileApi`]): the superblock's ops and words, and a copy of the inode
/// the call acts on, whose slot the call holds a count on. Its commit
/// writes back what the op changed in the copy and drops the count.
pub struct Call {
    ops: &'static dyn InodeOps,
    sb: u8,
    fstype: FsType,
    private: [u64; 2],
    now: u64,
    vol: Option<Instance>,
    before: Inode,
    ino: Inode,
}

impl Call {
    pub fn inode(&self) -> &Inode {
        &self.ino
    }

    /// Make the call on this call's inode copy.
    pub fn run<R>(&mut self, f: impl FnOnce(&dyn InodeOps, &mut OpCx<'_>, &mut Inode) -> R) -> R {
        let mut private = self.private;
        let mut cx = OpCx {
            sb: self.sb,
            fstype: self.fstype,
            private: &mut private,
            now: self.now,
            vol: self.vol.as_ref(),
        };
        f(self.ops, &mut cx, &mut self.ino)
    }

    /// Make the call on this call's inode copy and `other`'s, which is on
    /// the same superblock.
    pub fn run2<R>(
        &mut self,
        other: &mut Call,
        f: impl FnOnce(&dyn InodeOps, &mut OpCx<'_>, &mut Inode, &mut Inode) -> R,
    ) -> R {
        let mut private = self.private;
        let mut cx = OpCx {
            sb: self.sb,
            fstype: self.fstype,
            private: &mut private,
            now: self.now,
            vol: self.vol.as_ref(),
        };
        f(self.ops, &mut cx, &mut self.ino, &mut other.ino)
    }
}

/// A superblock hook prepared under the VFS lock and run with it dropped:
/// `fill_super`, `on_mount`, `sync`, and the last unmount's `on_umount`
/// and `release`. The superblock's `busy` count keeps its slot meanwhile.
pub struct SbCall {
    fs: Option<&'static dyn FileSystem>,
    ops: Option<&'static dyn InodeOps>,
    sb: u8,
    fstype: FsType,
    private: [u64; 2],
    now: u64,
    vol: Option<Instance>,
}

impl SbCall {
    fn run<R>(
        &mut self,
        f: impl FnOnce(Option<&dyn FileSystem>, Option<&dyn InodeOps>, &mut OpCx<'_>) -> R,
    ) -> R {
        let mut cx = OpCx {
            sb: self.sb,
            fstype: self.fstype,
            private: &mut self.private,
            now: self.now,
            vol: self.vol.as_ref(),
        };
        f(self.fs, self.ops, &mut cx)
    }
}

/// A mount's first step: done (a shared superblock), or `fill_super` to
/// run for a new one.
enum MountStep {
    Done(Mounted, SbCall),
    Fill(Fill),
}

/// A new superblock and a mount slot reserved while `fill_super` runs.
struct Fill {
    sb: u8,
    mount: u8,
    call: SbCall,
}

/// An unmount's step: an unlinked inode's release to retry first, or the
/// mount gone and its hooks to run.
enum UmountStep {
    Release(Call),
    Done(Umounted),
}

struct Umounted {
    sb: u8,
    last: bool,
    call: SbCall,
}

/// A `readdir` step: an entry `Vfs` makes itself (`.` and `..`) and the
/// next cookie, or the backend call for the entry at a backend cookie.
enum Rd {
    Entry(Dirent, u64),
    Call(Call, u64),
}

/// A rename's two directory calls and the inodes it holds: the one it
/// moves and the one it may replace, whose keys `seen` gives the backend.
struct RenameCall {
    a: Call,
    b: Call,
    src: u16,
    tgt: Option<u16>,
    seen: RenameSeen,
}

impl Vfs {
    /// [`Self::finish`], reading the inode through `f` before the count
    /// is dropped.
    fn finish_with<R>(&mut self, c: Call, merge: bool, f: impl FnOnce(&Inode) -> R) -> R {
        let i = c.ino.slot as usize;
        let live = self.inodes[i].used && self.inodes[i].r#gen == c.ino.r#gen;
        if !live {
            return f(&c.ino);
        }
        if merge {
            self.inodes[i].merge(&c.before, &c.ino);
        }
        let r = f(&self.inodes[i]);
        self.iput(i as u16);
        r
    }

    // ---- mounts ----

    // ---- open files ----

    // ---- namespace ----
}

/// Test hooks a [`FileApi`] calls (AGENTS.md rule 9: the kernel sets them
/// only in its `kernel_tests` build).
#[derive(Clone, Copy)]
pub struct Hooks {
    /// Between a write's backend call and its commit.
    pub write_window: fn(),
    /// Whether an `O_CREAT` open creates the file itself between its walk
    /// and its create, as another opener would.
    pub open_race: fn() -> bool,
    /// Between a namespace change's walks and its begin step (an unlink,
    /// rmdir, rename or link), where a racing change can take a name it
    /// walked.
    pub change_window: fn(),
    /// Between a rename's begin step and its backend call, with the VFS
    /// lock dropped, where a racing change can make, take or move a name
    /// the rename walked, or remove its new directory.
    pub rename_window: fn(),
}

fn no_window() {}

fn no_race() -> bool {
    false
}

impl Hooks {
    pub const NONE: Hooks = Hooks {
        write_window: no_window,
        open_race: no_race,
        change_window: no_window,
        rename_window: no_window,
    };
}

/// The File API over a [`Vfs`] behind lock `L` (C-FILEAPI): every method
/// is a loop of short locked steps and backend calls made with the lock
/// dropped, and every release a step queues runs with it dropped too.
/// An absolute path starts at its base's root, a relative one at its
/// working directory ([`WalkBase`]).
pub struct FileApi<'l, L: Guarded<Vfs>> {
    lock: &'l L,
    hooks: Hooks,
}

impl<'l, L: Guarded<Vfs>> FileApi<'l, L> {
    pub const fn new(lock: &'l L) -> Self {
        Self {
            lock,
            hooks: Hooks::NONE,
        }
    }

    pub const fn with_hooks(lock: &'l L, hooks: Hooks) -> Self {
        Self { lock, hooks }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Vfs) -> R) -> R {
        self.lock.with(f)
    }

    /// Run a locked step, then the releases it queued.
    fn step<R>(&self, f: impl FnOnce(&mut Vfs) -> R) -> R {
        let r = self.with(f);
        self.drain();
        r
    }

    /// Run each queued release's backend `evict` with the lock dropped.
    fn drain(&self) {
        let mut n = 0usize;
        while n < MAX_INODES {
            let Some(mut c) = self.with(|v| v.take_release()) else {
                return;
            };
            let ok = c.run(|o, cx, ino| o.evict(cx, ino)).is_ok();
            self.with(|v| v.release_done(c, ok));
            n += 1;
        }
    }
}

/// A mode argument's permission bits.
fn file_mode(mode: u32) -> u16 {
    (mode & 0o7777) as u16
}

fn name_is_dot(n: &[u8]) -> bool {
    n.len() == 1 && n[0] == b'.'
}

fn name_is_dotdot(n: &[u8]) -> bool {
    n.len() == 2 && n[0] == b'.' && n[1] == b'.'
}

#[cfg(test)]
mod testfs;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod walk_tests;
