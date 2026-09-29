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
//! backend runs under the VFS lock, which until ROADMAP §10.4's VFS-lock
//! box is an IRQ-off spinlock (DESIGN §2.1, §2.9 rule 2), and a backend
//! may wait for its volume and its disk. A backend that keeps inode words
//! in `Vfs` (FAT) reads and writes them in short locked sections of its
//! own inside its volume lock ([`Vfs::inode_words`]): volume first, VFS
//! second, never the reverse. The last put of an unlinked inode and the
//! last unmount of a superblock run the backend with the lock dropped
//! too; the slot stays reserved until the hook returns.
//!
//! Locks (kernel): RANK_DEVICE. Tables are static; do not allocate
//! under the lock. No FS work from hard IRQ (DESIGN §2.2).

pub mod fat;
mod file;
mod inode;
mod kernfs;
mod mount;
mod ramfs;
pub mod vibefs;
mod walk;

pub use file::{FileId, FileRef, SeekFrom};
pub use kernfs::{KernFs, KernSkin, KernState};
pub use ramfs::{RamFs, RamState};
pub use walk::{WalkCall, WalkReply, WalkStep, Walker, split_basename};

pub use crate::limits::MAX_DENTRIES;
pub use crate::limits::MAX_FDS;
pub use crate::limits::MAX_INODES;
pub use crate::limits::MAX_KERN_NODES;
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

pub const SEEK_SET: u32 = 0;
pub const SEEK_CUR: u32 = 1;
pub const SEEK_END: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    Exists,
    NotDir,
    IsDir,
    Inval,
    NoSpace,
    Loop,
    NameTooLong,
    NotEmpty,
    Busy,
    Badf,
    NotSupp,
    Io,
    /// Past a filesystem's maximum file size.
    FileTooBig,
    /// A kernel heap allocation failed (DESIGN §4.4).
    NoMem,
}

impl FsError {
    pub fn as_str(self) -> &'static str {
        match self {
            FsError::NotFound => "not found",
            FsError::Exists => "exists",
            FsError::NotDir => "not dir",
            FsError::IsDir => "is dir",
            FsError::Inval => "inval",
            FsError::NoSpace => "no space",
            FsError::Loop => "loop",
            FsError::NameTooLong => "name too long",
            FsError::NotEmpty => "not empty",
            FsError::Busy => "busy",
            FsError::Badf => "badf",
            FsError::NotSupp => "not supp",
            FsError::Io => "io",
            FsError::FileTooBig => "file too big",
            FsError::NoMem => "no memory",
        }
    }
}

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
}

/// A lock a backend's store, or the [`Vfs`], sits behind: a seam over the
/// kernel's `SpinMutex` and, in host tests, `std::sync::Mutex`, not a lock
/// of its own.
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

/// Per-inode ops, reached only through a superblock's `ops` pointer.
/// Each call gets copies of the inodes it acts on, whose slots the caller
/// holds a count on for the call, and runs with the VFS lock dropped
/// ([`FileApi`]); what it changes in a copy's public fields is written
/// back after it.
pub trait InodeOps: Sync {
    fn lookup(&self, cx: &mut OpCx<'_>, dir: &Inode, name: &[u8]) -> Result<InodeInfo, FsError>;
    fn create(
        &self,
        cx: &mut OpCx<'_>,
        dir: &mut Inode,
        name: &[u8],
        kind: InodeKind,
        mode: u16,
        target: Option<&[u8]>,
    ) -> Result<InodeInfo, FsError>;
    fn unlink(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError>;
    /// Remove the empty directory `name` from `dir`.
    fn rmdir(&self, _cx: &mut OpCx<'_>, _dir: &mut Inode, _name: &[u8]) -> Result<(), FsError> {
        Err(FsError::NotSupp)
    }
    /// Give `target` the further name `name` in `dir`.
    fn link(
        &self,
        _cx: &mut OpCx<'_>,
        _dir: &mut Inode,
        _name: &[u8],
        _target: &mut Inode,
    ) -> Result<(), FsError> {
        Err(FsError::NotSupp)
    }
    /// Move `oname` in `odir` to `nname` in `ndir`. The moved inode's new
    /// key when the move changed it, as FAT's dirent-location key does.
    /// An inode the move replaced is the one `Vfs` held for `nname`,
    /// released at its last put.
    fn rename(
        &self,
        _cx: &mut OpCx<'_>,
        _odir: &mut Inode,
        _oname: &[u8],
        _ndir: &mut Inode,
        _nname: &[u8],
    ) -> Result<Option<Key>, FsError> {
        Err(FsError::NotSupp)
    }
    fn read(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError>;
    fn write(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError>;
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
    fn truncate(&self, cx: &mut OpCx<'_>, ino: &mut Inode, size: u64) -> Result<(), FsError>;
    fn readdir(
        &self,
        cx: &mut OpCx<'_>,
        dir: &Inode,
        cookie: u64,
        out: &mut Dirent,
    ) -> Result<Option<u64>, FsError>;
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
    /// Drop the backend state of an unmounted superblock.
    fn kill_sb(&self, _cx: &mut OpCx<'_>) {}
}

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
    /// superblock's last mount, before `kill_sb`.
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

/// The words a backend that keeps its inode state in `Vfs` reads and
/// writes back ([`Vfs::inode_words`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Words {
    pub key: Key,
    pub kind: InodeKind,
    pub nlink: u32,
    pub size: u64,
    pub private: [u64; 2],
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
    };

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
            size: self.size,
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
            self.nlink = after.nlink;
        }
        if after.size != before.size {
            self.size = after.size;
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
#[derive(Clone, Copy)]
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
    };

    fn live(&self) -> bool {
        self.used && !self.filling && !self.dying
    }
}

/// A mount of a superblock on a directory (DESIGN §2.11). It holds one
/// count on its superblock; `refs` counts the open files, child mounts
/// and held paths that reach the filesystem through this mount. A
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
/// here.
#[derive(Clone, Copy)]
struct File {
    used: bool,
    refs: u16,
    r#gen: u16,
    islot: u16,
    dslot: Option<u16>,
    mount: u8,
    flags: OpenFlags,
    offset: u64,
}

impl File {
    const EMPTY: Self = Self {
        used: false,
        refs: 0,
        r#gen: 0,
        islot: 0,
        dslot: None,
        mount: 0,
        flags: OpenFlags(0),
        offset: 0,
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

/// Phase 9 hangs an [`FdTable`] on a process. Slice A owns the shape.
#[derive(Clone, Copy)]
pub struct FdTable {
    fds: [u16; MAX_FDS],
}

impl FdTable {
    pub const fn new() -> Self {
        Self { fds: [0; MAX_FDS] }
    }

    pub fn install(&mut self, fid: u16) -> Result<u32, FsError> {
        if fid as usize >= MAX_FILES {
            return Err(FsError::Badf);
        }
        let mut i = 0usize;
        while i < MAX_FDS {
            if self.fds[i] == 0 {
                self.fds[i] = fid + 1;
                return Ok(i as u32);
            }
            i += 1;
        }
        Err(FsError::NoSpace)
    }

    pub fn get(&self, fd: u32) -> Result<u16, FsError> {
        let i = fd as usize;
        if i >= MAX_FDS || self.fds[i] == 0 {
            return Err(FsError::Badf);
        }
        Ok(self.fds[i] - 1)
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

pub struct Vfs {
    inodes: [Inode; MAX_INODES],
    dentries: [Dentry; MAX_DENTRIES],
    supers: [Super; MAX_MOUNTS],
    mounts: [Mount; MAX_MOUNTS],
    files: [File; MAX_FILES],
    ihand: u16,
    dhand: u16,
    pub now: u64,
    pub stats: VfsStats,
}

impl Default for Vfs {
    fn default() -> Self {
        Self::new()
    }
}

impl Vfs {
    pub const fn new() -> Self {
        Self {
            inodes: [Inode::EMPTY; MAX_INODES],
            dentries: [Dentry::EMPTY; MAX_DENTRIES],
            supers: [Super::EMPTY; MAX_MOUNTS],
            mounts: [Mount::EMPTY; MAX_MOUNTS],
            files: [File::EMPTY; MAX_FILES],
            ihand: 0,
            dhand: 0,
            now: 0,
            stats: VfsStats {
                d_evicts: 0,
                i_evicts: 0,
                opens: 0,
            },
        }
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
        };
        f(self.ops, &mut cx, &mut self.ino, &mut other.ino)
    }
}

/// A superblock hook prepared under the VFS lock and run with it dropped:
/// `fill_super`, `on_mount`, `sync`, and the last unmount's `on_umount`
/// and `kill_sb`. The superblock's `busy` count keeps its slot meanwhile.
pub struct SbCall {
    fs: Option<&'static dyn FileSystem>,
    ops: Option<&'static dyn InodeOps>,
    sb: u8,
    fstype: FsType,
    private: [u64; 2],
    now: u64,
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
/// moves and the one it may replace.
struct RenameCall {
    a: Call,
    b: Call,
    src: u16,
    tgt: Option<u16>,
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
}

fn no_window() {}

fn no_race() -> bool {
    false
}

impl Hooks {
    pub const NONE: Hooks = Hooks {
        write_window: no_window,
        open_race: no_race,
    };
}

/// The File API over a [`Vfs`] behind lock `L` (C-FILEAPI): every method
/// is a loop of short locked steps and backend calls made with the lock
/// dropped, and every release a step queues runs with it dropped too.
/// Paths are absolute or relative to `cwd`.
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

/// The VFS with no lock around it, for host tests that drive one `Vfs`
/// through [`FileApi`] from one thread.
#[cfg(test)]
pub(crate) struct Direct<'a>(core::cell::RefCell<&'a mut Vfs>);

#[cfg(test)]
impl Guarded<Vfs> for Direct<'_> {
    fn with<R>(&self, f: impl FnOnce(&mut Vfs) -> R) -> R {
        f(&mut self.0.borrow_mut())
    }
}

/// Host-test wrappers over the driver, in the `v.mkdir(None, …)` style
/// the tests had before the File API: each runs [`FileApi`] calls on this
/// `Vfs`.
#[cfg(test)]
impl Vfs {
    pub(crate) fn api<R>(&mut self, f: impl FnOnce(&FileApi<'_, Direct<'_>>) -> R) -> R {
        let d = Direct(core::cell::RefCell::new(self));
        f(&FileApi::new(&d))
    }

    pub(crate) fn mount_root_fs(
        &mut self,
        fs: &'static dyn FileSystem,
    ) -> Result<PathRef, FsError> {
        self.api(|a| a.mount_root(fs, None, false))?;
        self.root()
    }

    pub(crate) fn mount(
        &mut self,
        cwd: Option<PathRef>,
        at: &str,
        fs: &'static dyn FileSystem,
    ) -> Result<u8, FsError> {
        self.api(|a| a.mount_fs(cwd, at.as_bytes(), fs, None, false))
            .map(|m| m.mount)
    }

    pub(crate) fn mount_dev(
        &mut self,
        at: &str,
        fs: &'static dyn FileSystem,
        dev: u64,
        ro: bool,
    ) -> Result<Mounted, FsError> {
        self.api(|a| a.mount_fs(None, at.as_bytes(), fs, Some(dev), ro))
    }

    pub(crate) fn umount(&mut self, cwd: Option<PathRef>, at: &str) -> Result<(), FsError> {
        self.api(|a| a.umount(cwd, at.as_bytes()))
    }

    /// The path `path` resolves to, not held.
    pub(crate) fn resolve(
        &mut self,
        cwd: Option<PathRef>,
        path: &str,
        follow: bool,
    ) -> Result<PathRef, FsError> {
        self.api(|a| {
            let p = a.walk(cwd, path.as_bytes(), follow)?;
            a.put_path(p);
            Ok(p)
        })
    }

    /// A counted reference to the inode `p` names.
    pub(crate) fn iref(&mut self, p: PathRef) -> Result<InodeRef, FsError> {
        let slot = self.d_islot(p.dslot)?;
        self.ihold(slot)?;
        Ok(InodeRef {
            slot,
            r#gen: self.inodes[slot as usize].r#gen,
        })
    }

    pub(crate) fn stat(&mut self, cwd: Option<PathRef>, path: &str) -> Result<Stat, FsError> {
        self.api(|a| a.stat_path(cwd, path.as_bytes(), true))
    }

    pub(crate) fn lstat(&mut self, cwd: Option<PathRef>, path: &str) -> Result<Stat, FsError> {
        self.api(|a| a.stat_path(cwd, path.as_bytes(), false))
    }

    pub(crate) fn mkdir(
        &mut self,
        cwd: Option<PathRef>,
        path: &str,
        mode: u16,
    ) -> Result<(), FsError> {
        self.api(|a| a.mkdir(cwd, path.as_bytes(), u32::from(mode)))
    }

    pub(crate) fn creat(
        &mut self,
        cwd: Option<PathRef>,
        path: &str,
        mode: u16,
    ) -> Result<(), FsError> {
        self.api(|a| a.create(cwd, path.as_bytes(), InodeKind::Reg, mode | S_IFREG, None))
    }

    pub(crate) fn symlink(
        &mut self,
        cwd: Option<PathRef>,
        path: &str,
        target: &str,
    ) -> Result<(), FsError> {
        self.api(|a| a.symlink(cwd, path.as_bytes(), target.as_bytes()))
    }

    pub(crate) fn unlink(&mut self, cwd: Option<PathRef>, path: &str) -> Result<(), FsError> {
        self.api(|a| a.unlink(cwd, path.as_bytes()))
    }

    pub(crate) fn rmdir(&mut self, cwd: Option<PathRef>, path: &str) -> Result<(), FsError> {
        self.api(|a| a.rmdir(cwd, path.as_bytes()))
    }

    pub(crate) fn link(
        &mut self,
        cwd: Option<PathRef>,
        old: &str,
        new: &str,
    ) -> Result<(), FsError> {
        self.api(|a| a.link(cwd, old.as_bytes(), new.as_bytes()))
    }

    pub(crate) fn rename(
        &mut self,
        cwd: Option<PathRef>,
        old: &str,
        new: &str,
    ) -> Result<(), FsError> {
        self.api(|a| a.rename(cwd, old.as_bytes(), new.as_bytes()))
    }

    pub(crate) fn truncate(
        &mut self,
        cwd: Option<PathRef>,
        path: &str,
        size: u64,
    ) -> Result<(), FsError> {
        self.api(|a| a.truncate(cwd, path.as_bytes(), size))
    }

    pub(crate) fn open_path(
        &mut self,
        cwd: Option<PathRef>,
        path: &str,
        flags: u32,
        mode: u16,
    ) -> Result<FileRef, FsError> {
        self.api(|a| {
            a.open(
                cwd,
                path.as_bytes(),
                OpenFlags::from_bits(flags),
                u32::from(mode),
            )
        })
    }

    pub(crate) fn read(&mut self, f: &FileRef, buf: &mut [u8]) -> Result<usize, FsError> {
        self.api(|a| a.read(f, buf))
    }

    pub(crate) fn write(&mut self, f: &FileRef, buf: &[u8]) -> Result<usize, FsError> {
        self.api(|a| a.write(f, buf))
    }

    pub(crate) fn seek(&mut self, f: &FileRef, off: i64, whence: u32) -> Result<u64, FsError> {
        let pos = SeekFrom::from_whence(off, whence)?;
        self.api(|a| a.seek(f, pos))
    }

    pub(crate) fn close(&mut self, f: FileRef) -> Result<(), FsError> {
        self.api(|a| a.close(f))
    }

    /// Entry `cookie` of directory `dir`, `.` and `..` first, and the
    /// cookie of the next.
    pub(crate) fn readdir(
        &mut self,
        dir: PathRef,
        cookie: u64,
        out: &mut Dirent,
    ) -> Result<Option<u64>, FsError> {
        let f = FileRef::from_raw(self.open(dir, OpenFlags::from_bits(O_RDONLY))?);
        let mut i = 0u64;
        let mut hit = None;
        let r = self.api(|a| {
            a.readdir(&f, &mut |d| {
                if i == cookie {
                    hit = Some(*d);
                    return false;
                }
                i += 1;
                true
            })
        });
        self.close(f)?;
        r?;
        Ok(hit.map(|d| {
            *out = d;
            cookie + 1
        }))
    }
}

#[cfg(test)]
mod testfs;

#[cfg(test)]
mod tests;
