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

mod kernfs;
mod ramfs;

pub use kernfs::{KernFs, KernSkin, KernState};
pub use ramfs::{RamFs, RamState};

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

/// An open-file table slot and the generation it had when opened: the
/// payload of a process's `FdKind::File` (C-FDGEN).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileId {
    pub fid: u16,
    pub r#gen: u16,
}

/// One counted reference to an open file. It is not `Copy`, and it has
/// no `Drop`: hand it to [`FileApi::close`], or move its count into an fd
/// table with [`FileRef::into_raw`]. A stale one fails with `Badf` at its
/// next use, so neither conversion is `unsafe`.
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub struct FileRef {
    id: FileId,
}

impl FileRef {
    pub fn id(&self) -> FileId {
        self.id
    }

    /// Move this reference's count into an fd table.
    pub fn into_raw(self) -> FileId {
        self.id
    }

    /// Take back a count an fd table holds, as `close` does.
    pub fn from_raw(id: FileId) -> FileRef {
        FileRef { id }
    }
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

/// Where `seek` moves a file's offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeekFrom {
    Start(u64),
    Current(i64),
    End(i64),
}

impl SeekFrom {
    /// Linux's `lseek(off, whence)`: a negative `SEEK_SET` or an unknown
    /// `whence` is `Inval`.
    pub fn from_whence(off: i64, whence: u32) -> Result<Self, FsError> {
        match whence {
            SEEK_SET => u64::try_from(off)
                .map(SeekFrom::Start)
                .map_err(|_| FsError::Inval),
            SEEK_CUR => Ok(SeekFrom::Current(off)),
            SEEK_END => Ok(SeekFrom::End(off)),
            _ => Err(FsError::Inval),
        }
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

    pub fn root(&self) -> Result<PathRef, FsError> {
        if !self.mounts[0].used {
            return Err(FsError::Io);
        }
        Ok(PathRef {
            mount: 0,
            dslot: self.mounts[0].root_dslot,
        })
    }

    pub fn islot(&self, p: PathRef) -> Result<u16, FsError> {
        self.d_islot(p.dslot)
    }

    /// The mounted superblock of block device `dev`.
    pub fn super_of_dev(&self, dev: u64) -> Option<u8> {
        self.supers
            .iter()
            .position(|s| s.live() && s.dev == Some(dev))
            .map(|i| i as u8)
    }

    pub fn sb_of_mount(&self, m: u8) -> Result<u8, FsError> {
        match self.mounts.get(m as usize) {
            Some(x) if x.used => Ok(x.sb),
            _ => Err(FsError::Io),
        }
    }

    /// The private words of mounted superblock `sb`.
    pub fn sb_private(&self, sb: u8) -> Result<[u64; 2], FsError> {
        self.sb_live(sb)?;
        Ok(self.supers[sb as usize].private)
    }

    /// A counted reference to the inode `info` describes: the cached one
    /// when `(sb, info.key)` is hashed, whose words `info` never
    /// overwrites, else a slot filled from `info`.
    pub fn iget_key(&mut self, sb: u8, info: &InodeInfo) -> Result<InodeRef, FsError> {
        self.sb_live(sb)?;
        let slot = self.iget_info(sb, info)?;
        Ok(InodeRef {
            slot,
            r#gen: self.inodes[slot as usize].r#gen,
        })
    }

    /// Drop a reference. The last one on an inode with no links queues
    /// its release, which [`FileApi`] runs with the lock dropped.
    pub fn put_ref(&mut self, r: InodeRef) {
        if let Ok(i) = self.slot_of(r.handle()) {
            self.iput(i as u16);
        }
    }

    /// The inode `h` names; `Badf` when its slot was refilled.
    pub fn inode(&self, h: InodeHandle) -> Result<&Inode, FsError> {
        let i = self.slot_of(h)?;
        Ok(&self.inodes[i])
    }

    /// The words of the inode `h` names, for a backend that keeps its
    /// inode state in `Vfs` to read inside its own volume lock.
    pub fn inode_words(&self, h: InodeHandle) -> Result<Words, FsError> {
        let n = &self.inodes[self.slot_of(h)?];
        Ok(Words {
            key: n.key,
            kind: n.kind,
            nlink: n.nlink,
            size: n.size,
            private: n.private,
        })
    }

    /// Store the private words and size of the inode `h` names, inside
    /// the volume lock [`Self::inode_words`] was read under.
    pub fn set_inode_words(
        &mut self,
        h: InodeHandle,
        private: [u64; 2],
        size: u64,
    ) -> Result<(), FsError> {
        let i = self.slot_of(h)?;
        self.inodes[i].private = private;
        self.inodes[i].size = size;
        Ok(())
    }

    /// Hashed inode slots of `sb` keyed `key`: one per file.
    pub fn inodes_with_key(&self, sb: u8, key: Key) -> usize {
        self.inodes
            .iter()
            .filter(|n| n.used && n.sb == sb && n.key == key && n.nlink != 0)
            .count()
    }

    /// The superblock and key of the inode open file `id` refers to.
    pub fn file_inode(&self, id: FileId) -> Result<(u8, Key), FsError> {
        let n = &self.inodes[self.files[self.file_slot(id)?].islot as usize];
        Ok((n.sb, n.key))
    }

    /// Each open-file slot's `(used, refs, gen)`.
    pub fn file_table(&self) -> [(bool, u16, u16); MAX_FILES] {
        let mut out = [(false, 0u16, 0u16); MAX_FILES];
        for (o, f) in out.iter_mut().zip(self.files.iter()) {
            *o = (f.used, f.refs, f.r#gen);
        }
        out
    }

    /// Drop every negative dentry of `sb`, as a create made outside the
    /// dentry cache requires.
    pub fn drop_negatives(&mut self, sb: u8) {
        let mut i = 0usize;
        while i < MAX_DENTRIES {
            let d = &self.dentries[i];
            if d.used && d.sb == sb && d.negative {
                self.dentry_evict(i as u16);
            }
            i += 1;
        }
    }

    /// Move the hashed inode keyed `from` to `to`, and drop the dentries
    /// that name it. A cached inode already at `to` leaves the hash.
    pub fn rekey(&mut self, sb: u8, from: Key, to: Key) -> Result<(), FsError> {
        self.sb_live(sb)?;
        if let Some(f) = self.hashed(sb, from) {
            self.rekey_slot(f, to);
        }
        Ok(())
    }

    /// Release a path [`FileApi::walk`] held: its dentry and its mount.
    pub fn path_put(&mut self, p: PathRef) {
        self.dput(p.dslot);
        let r = &mut self.mounts[p.mount as usize].refs;
        *r = r.saturating_sub(1);
    }

    /// Open a file on the resolved dentry `p` (C-FILEAPI's `open`
    /// locked step): the file counts the inode, the mount and the dentry.
    pub fn open(&mut self, p: PathRef, flags: OpenFlags) -> Result<FileId, FsError> {
        let islot = self.d_islot(p.dslot)?;
        open_check(self.inodes[islot as usize].kind, flags)?;
        let id = self.file_alloc(islot, p.mount, Some(p.dslot), flags)?;
        self.stats.opens = self.stats.opens.saturating_add(1);
        Ok(id)
    }

    /// Open a file on the inode `r` names, which a walk outside the
    /// dentry cache found, through a mount of its superblock. `r` is put.
    pub fn open_inode(&mut self, r: InodeRef, flags: OpenFlags) -> Result<FileId, FsError> {
        let i = self.slot_of(r.handle())?;
        let sb = self.inodes[i].sb;
        let res = match open_check(self.inodes[i].kind, flags) {
            Ok(()) => match self.mounts.iter().position(|m| m.used && m.sb == sb) {
                Some(m) => self.file_alloc(i as u16, m as u8, None, flags),
                None => Err(FsError::Io),
            },
            Err(e) => Err(e),
        };
        self.put_ref(r);
        res
    }
}

/// Whether a file of `kind` may be opened with `flags`.
fn open_check(kind: InodeKind, flags: OpenFlags) -> Result<(), FsError> {
    match kind {
        InodeKind::Dir => {
            if flags.writes() || flags.has(O_TRUNC) {
                return Err(FsError::IsDir);
            }
        }
        InodeKind::Reg | InodeKind::Chr | InodeKind::Blk => {
            if flags.has(O_DIRECTORY) {
                return Err(FsError::NotDir);
            }
        }
        InodeKind::Lnk => {
            if !flags.has(O_NOFOLLOW) {
                return Err(FsError::Loop);
            }
            if flags.has(O_DIRECTORY) {
                return Err(FsError::NotDir);
            }
        }
    }
    Ok(())
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
    fn sb_of(&self, mount: u8) -> u8 {
        self.mounts[mount as usize].sb
    }

    /// The type of superblock `sb`, for an [`OpCx`].
    fn fstype(&self, sb: u8) -> FsType {
        self.supers[sb as usize]
            .fs
            .map(|f| f.fstype())
            .unwrap_or_default()
    }

    fn d_islot(&self, dslot: u16) -> Result<u16, FsError> {
        let d = &self.dentries[dslot as usize];
        if !d.used || d.negative {
            return Err(FsError::NotFound);
        }
        Ok(d.islot)
    }

    fn kind_of(&self, p: PathRef) -> Result<InodeKind, FsError> {
        Ok(self.inodes[self.d_islot(p.dslot)? as usize].kind)
    }

    /// Hold path `p`: its dentry and its mount, across an unlocked call.
    fn path_get(&mut self, p: PathRef) -> Result<(), FsError> {
        let r = self.mounts[p.mount as usize]
            .refs
            .checked_add(1)
            .ok_or(FsError::NoSpace)?;
        self.dget(p.dslot)?;
        self.mounts[p.mount as usize].refs = r;
        Ok(())
    }

    /// Prepare a backend call on inode `islot`, which it counts.
    fn call(&mut self, islot: u16) -> Result<Call, FsError> {
        let sb = self.inodes[islot as usize].sb;
        let ops = self.supers[sb as usize].ops.ok_or(FsError::NotSupp)?;
        self.ihold(islot)?;
        Ok(self.raw_call(islot, ops))
    }

    /// A call on inode `islot` that takes no count: a release, whose slot
    /// its release state keeps.
    fn raw_call(&self, islot: u16, ops: &'static dyn InodeOps) -> Call {
        let n = self.inodes[islot as usize];
        let sb = n.sb;
        Call {
            ops,
            sb,
            fstype: self.fstype(sb),
            private: self.supers[sb as usize].private,
            now: self.now,
            before: n,
            ino: n,
        }
    }

    /// Commit call `c`: write back what the op changed when `merge`, and
    /// drop the call's count.
    fn finish(&mut self, c: Call, merge: bool) {
        self.finish_with(c, merge, |_| ());
    }

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

    /// A call on a superblock, which counts it `busy`.
    fn sb_call(&mut self, sb: u8) -> Result<SbCall, FsError> {
        let s = &mut self.supers[sb as usize];
        s.busy = s.busy.checked_add(1).ok_or(FsError::NoSpace)?;
        Ok(SbCall {
            fs: s.fs,
            ops: s.ops,
            sb,
            fstype: self.fstype(sb),
            private: self.supers[sb as usize].private,
            now: self.now,
        })
    }

    /// A superblock hook returned. After the last one of a superblock
    /// whose last mount is gone (`release`), its slot is free.
    fn sb_idle(&mut self, sb: u8, release: bool) {
        let s = &mut self.supers[sb as usize];
        s.busy = s.busy.saturating_sub(1);
        if release && s.busy == 0 {
            debug_assert!(s.refs == 0, "superblock released while held");
            *s = Super::EMPTY;
        }
    }

    /// The next unhashed inode whose release is queued, as a call to its
    /// backend's `evict`; one on a superblock with no ops is released
    /// here.
    fn take_release(&mut self) -> Option<Call> {
        let mut i = 0usize;
        while i < MAX_INODES {
            let n = &self.inodes[i];
            if n.used && n.rel == Rel::Queued {
                match self.supers[n.sb as usize].ops {
                    None => self.inode_clear(i),
                    Some(ops) => {
                        self.inodes[i].rel = Rel::Running;
                        return Some(self.raw_call(i as u16, ops));
                    }
                }
            }
            i += 1;
        }
        None
    }

    /// A release call returned: its slot is free, or, when `evict`
    /// failed, left unhashed for `umount` to retry.
    fn release_done(&mut self, c: Call, ok: bool) {
        let i = c.ino.slot as usize;
        let n = &mut self.inodes[i];
        if !n.used || n.r#gen != c.ino.r#gen || n.rel != Rel::Running {
            return;
        }
        if ok {
            self.inode_clear(i);
        } else {
            n.rel = Rel::Failed;
        }
    }

    // ---- mounts ----

    /// A mount's first locked step: `at` is the held mountpoint, or none
    /// for the root mount. A block device `dev` that is already mounted
    /// shares its superblock, as Linux's does, and one mounted with the
    /// other read-only flag, or as another filesystem type, is `Busy`;
    /// otherwise a superblock and a mount slot are reserved for
    /// `fill_super`.
    fn mount_begin(
        &mut self,
        at: Option<PathRef>,
        fs: &'static dyn FileSystem,
        dev: Option<u64>,
        ro: bool,
    ) -> Result<MountStep, FsError> {
        self.mountpoint_ok(at)?;
        if let Some(d) = dev
            && let Some(i) = self.supers.iter().position(|s| s.used && s.dev == Some(d))
        {
            let s = &self.supers[i];
            if !s.live() || s.ro != ro || s.fs.map(|f| f.fstype()) != Some(fs.fstype()) {
                return Err(FsError::Busy);
            }
            let at = at.ok_or(FsError::Busy)?;
            let sb = i as u8;
            let m = self.alloc_mount()?;
            let call = self.sb_call(sb)?;
            if let Err(e) = self.attach_mount(m, Some(at), sb) {
                self.sb_idle(sb, false);
                return Err(e);
            }
            let done = Mounted {
                mount: m,
                sb,
                shared: true,
            };
            return Ok(MountStep::Done(done, call));
        }
        let m = match at {
            None => 0,
            Some(_) => self.alloc_mount()?,
        };
        let sb = self.alloc_super()?;
        self.supers[sb as usize] = Super {
            used: true,
            filling: true,
            fs: Some(fs),
            ops: fs.ops(),
            dev,
            ro,
            maxbytes: fs.max_bytes(),
            ..Super::EMPTY
        };
        let call = match self.sb_call(sb) {
            Ok(c) => c,
            Err(e) => {
                self.supers[sb as usize] = Super::EMPTY;
                return Err(e);
            }
        };
        self.mounts[m as usize].reserved = true;
        Ok(MountStep::Fill(Fill { sb, mount: m, call }))
    }

    /// `at` is a directory no mount covers, or, with no `at`, the root
    /// mount is free.
    fn mountpoint_ok(&self, at: Option<PathRef>) -> Result<(), FsError> {
        match at {
            None => {
                if self.mounts[0].used || self.mounts[0].reserved {
                    return Err(FsError::Busy);
                }
            }
            Some(at) => {
                if self.kind_of(at)? != InodeKind::Dir {
                    return Err(FsError::NotDir);
                }
                if self.child_mount(at.mount, at.dslot).is_some() {
                    return Err(FsError::Busy);
                }
            }
        }
        Ok(())
    }

    /// A mount's commit after `fill_super` returned `res`, having set the
    /// superblock's words to `private`. On an error after a successful
    /// fill, the superblock's `kill_sb` is the caller's to run.
    fn mount_commit(
        &mut self,
        at: Option<PathRef>,
        f: Fill,
        res: Result<InodeInfo, FsError>,
        private: [u64; 2],
    ) -> Result<(Mounted, SbCall), (FsError, Option<SbCall>)> {
        self.mounts[f.mount as usize].reserved = false;
        let sb = f.sb;
        let info = match res {
            Ok(i) => i,
            Err(e) => {
                self.supers[sb as usize] = Super::EMPTY;
                return Err((e, None));
            }
        };
        self.supers[sb as usize].private = private;
        let mut call = f.call;
        call.private = private;
        match self.mount_fill(at, f.mount, sb, &info) {
            Ok(()) => Ok((
                Mounted {
                    mount: f.mount,
                    sb,
                    shared: false,
                },
                call,
            )),
            Err(e) => {
                self.sb_clear(sb);
                let s = &mut self.supers[sb as usize];
                s.filling = false;
                s.dying = true;
                Err((e, Some(call)))
            }
        }
    }

    /// Give filled superblock `sb` its root inode and dentry, which the
    /// superblock holds once, and mount it as `m` on `at`.
    fn mount_fill(
        &mut self,
        at: Option<PathRef>,
        m: u8,
        sb: u8,
        info: &InodeInfo,
    ) -> Result<(), FsError> {
        self.mountpoint_ok(at)?;
        if self.mounts[m as usize].used {
            return Err(FsError::Busy);
        }
        let islot = self.iget_info(sb, info)?;
        let dslot = match self.dentry_force_alloc() {
            Ok(d) => d,
            Err(e) => {
                self.iput(islot);
                return Err(e);
            }
        };
        self.dentries[dslot as usize] = Dentry {
            used: true,
            clock: true,
            negative: false,
            dead: false,
            refs: 1,
            parent: dslot,
            sb,
            name: Name::EMPTY,
            islot,
        };
        let s = &mut self.supers[sb as usize];
        s.root_islot = islot;
        s.root_dslot = dslot;
        s.filling = false;
        self.attach_mount(m, at, sb)
    }

    /// Make mount `m` of superblock `sb` on `at` (none: the root mount):
    /// it counts the superblock, the mountpoint and the parent mount.
    fn attach_mount(&mut self, m: u8, at: Option<PathRef>, sb: u8) -> Result<(), FsError> {
        let root = self.supers[sb as usize].root_dslot;
        let srefs = self.supers[sb as usize]
            .refs
            .checked_add(1)
            .ok_or(FsError::NoSpace)?;
        let (parent, mp) = match at {
            Some(at) => {
                let prefs = self.mounts[at.mount as usize]
                    .refs
                    .checked_add(1)
                    .ok_or(FsError::NoSpace)?;
                self.dget(at.dslot)?;
                self.mounts[at.mount as usize].refs = prefs;
                (Some(at.mount), at.dslot)
            }
            None => (None, root),
        };
        self.supers[sb as usize].refs = srefs;
        self.mounts[m as usize] = Mount {
            used: true,
            reserved: false,
            parent,
            mp_dslot: mp,
            root_dslot: root,
            sb,
            refs: 0,
        };
        Ok(())
    }

    /// An unmount's locked step on the mount whose root `p` names, a path
    /// the caller held and this step puts. A mount something still
    /// reaches the filesystem through (an open file, a child mount, a
    /// held path) is `Busy`; so is the superblock's last mount while any
    /// of its dentries or inodes is held or a hook of it runs. Every busy
    /// check runs before the first write, so a `Busy` leaves the tables as
    /// they were. After the last mount, the superblock's cached dentries
    /// and inodes go, and its slot stays until its hooks return.
    fn umount_step(&mut self, p: PathRef) -> Result<UmountStep, FsError> {
        self.path_put(p);
        let m = p.mount;
        if m == 0 {
            return Err(FsError::Busy);
        }
        if self.mounts[m as usize].root_dslot != p.dslot {
            return Err(FsError::Inval);
        }
        if self.mounts[m as usize].refs != 0 {
            return Err(FsError::Busy);
        }
        let sb = self.mounts[m as usize].sb;
        let last = self.mount_count(sb) == 1;
        if last {
            self.sb_busy(sb)?;
            if let Some(i) = self
                .inodes
                .iter()
                .position(|n| n.used && n.sb == sb && n.rel == Rel::Failed)
            {
                let ops = self.supers[sb as usize].ops.ok_or(FsError::Busy)?;
                self.inodes[i].rel = Rel::Running;
                return Ok(UmountStep::Release(self.raw_call(i as u16, ops)));
            }
        }
        let call = self.sb_call(sb)?;
        let mp = self.mounts[m as usize].mp_dslot;
        if let Some(pm) = self.mounts[m as usize].parent {
            let r = &mut self.mounts[pm as usize].refs;
            *r = r.saturating_sub(1);
        }
        self.dput(mp);
        self.mounts[m as usize] = Mount::EMPTY;
        let s = &mut self.supers[sb as usize];
        s.refs = s.refs.saturating_sub(1);
        if last {
            self.sb_clear(sb);
            self.supers[sb as usize].dying = true;
        }
        Ok(UmountStep::Done(Umounted { sb, last, call }))
    }

    /// Mounts of superblock `sb`.
    fn mount_count(&self, sb: u8) -> usize {
        self.mounts.iter().filter(|x| x.used && x.sb == sb).count()
    }

    /// `Busy` while a hook of `sb` runs, a dentry or inode of it is held
    /// beyond the cache's own holds, or an inode of it is being released.
    fn sb_busy(&self, sb: u8) -> Result<(), FsError> {
        if self.supers[sb as usize].busy != 0 {
            return Err(FsError::Busy);
        }
        let mut d = 0usize;
        while d < MAX_DENTRIES {
            let e = &self.dentries[d];
            if e.used && e.sb == sb && e.refs > self.expected_holds(d as u16) {
                return Err(FsError::Busy);
            }
            d += 1;
        }
        let mut n = 0usize;
        while n < MAX_INODES {
            let ino = &self.inodes[n];
            if ino.used
                && ino.sb == sb
                && (ino.refs > self.naming_dentries(n as u16)
                    || matches!(ino.rel, Rel::Queued | Rel::Running))
            {
                return Err(FsError::Busy);
            }
            n += 1;
        }
        Ok(())
    }

    /// Drop all that is cached for superblock `sb`: its dentries and
    /// their inode references, and its inodes. No backend is called.
    fn sb_clear(&mut self, sb: u8) {
        let mut d = 0usize;
        while d < MAX_DENTRIES {
            let e = self.dentries[d];
            if e.used && e.sb == sb {
                if !e.negative {
                    let r = &mut self.inodes[e.islot as usize].refs;
                    *r = r.saturating_sub(1);
                }
                self.dentries[d] = Dentry::EMPTY;
            }
            d += 1;
        }
        let mut n = 0usize;
        while n < MAX_INODES {
            if self.inodes[n].used && self.inodes[n].sb == sb {
                self.inode_clear(n);
            }
            n += 1;
        }
    }

    /// The next mounted superblock from `from` on with ops, as a `sync`
    /// call.
    fn sync_begin(&mut self, from: u8) -> Option<SbCall> {
        let mut i = from as usize;
        while i < MAX_MOUNTS {
            let s = &self.supers[i];
            if s.live() && s.ops.is_some() {
                return self.sb_call(i as u8).ok();
            }
            i += 1;
        }
        None
    }

    // ---- open files ----

    /// The live slot `id` names: `Badf` when it is out of range, unused,
    /// or of another generation (C-FDGEN).
    fn file_slot(&self, id: FileId) -> Result<usize, FsError> {
        let i = id.fid as usize;
        match self.files.get(i) {
            Some(f) if f.used && f.r#gen == id.r#gen => Ok(i),
            _ => Err(FsError::Badf),
        }
    }

    /// An open file on inode `islot` through `mount`, pinning dentry
    /// `dslot` when it has one: it counts one reference to each.
    fn file_alloc(
        &mut self,
        islot: u16,
        mount: u8,
        dslot: Option<u16>,
        flags: OpenFlags,
    ) -> Result<FileId, FsError> {
        let i = self
            .files
            .iter()
            .position(|f| !f.used)
            .ok_or(FsError::NoSpace)?;
        let mrefs = self.mounts[mount as usize]
            .refs
            .checked_add(1)
            .ok_or(FsError::NoSpace)?;
        if let Some(d) = dslot {
            self.dget(d)?;
        }
        if let Err(e) = self.ihold(islot) {
            if let Some(d) = dslot {
                self.dput(d);
            }
            return Err(e);
        }
        self.mounts[mount as usize].refs = mrefs;
        let g = self.files[i].r#gen;
        self.files[i] = File {
            used: true,
            refs: 1,
            r#gen: g,
            islot,
            dslot,
            mount,
            flags,
            offset: 0,
        };
        Ok(FileId {
            fid: i as u16,
            r#gen: g,
        })
    }

    fn file_islot(&self, id: FileId) -> Result<u16, FsError> {
        Ok(self.files[self.file_slot(id)?].islot)
    }

    fn file_kind(&self, id: FileId) -> Result<InodeKind, FsError> {
        Ok(self.inodes[self.file_islot(id)? as usize].kind)
    }

    /// Drop one reference to open file `id`; the last frees the slot,
    /// changes its generation, and puts the inode, dentry and mount.
    fn file_close(&mut self, id: FileId) -> Result<(), FsError> {
        let i = self.file_slot(id)?;
        if self.files[i].refs > 1 {
            self.files[i].refs -= 1;
            return Ok(());
        }
        let f = self.files[i];
        self.files[i] = File {
            r#gen: f.r#gen.wrapping_add(1),
            ..File::EMPTY
        };
        let r = &mut self.mounts[f.mount as usize].refs;
        *r = r.saturating_sub(1);
        if let Some(d) = f.dslot {
            self.dput(d);
        }
        self.iput(f.islot);
        Ok(())
    }

    /// One more reference to open file `id`, for `dup`, `fork`, or one
    /// syscall.
    fn file_addref(&mut self, id: FileId) -> Result<(), FsError> {
        let i = self.file_slot(id)?;
        self.files[i].refs = self.files[i].refs.checked_add(1).ok_or(FsError::NoSpace)?;
        Ok(())
    }

    fn read_begin(&mut self, id: FileId) -> Result<(Call, u64), FsError> {
        let f = self.files[self.file_slot(id)?];
        if !f.flags.reads() {
            return Err(FsError::Inval);
        }
        Ok((self.call(f.islot)?, f.offset))
    }

    /// Commit a read of `n` bytes at `off`: the offset moves past them
    /// unless the file was closed meanwhile (`Badf`).
    fn read_end(
        &mut self,
        id: FileId,
        mut c: Call,
        n: Option<usize>,
        off: u64,
    ) -> Result<(), FsError> {
        if n.is_some() {
            c.ino.atime = self.now;
        }
        let r = self.file_slot(id).and_then(|i| {
            if let Some(n) = n {
                let end = off.checked_add(n as u64).ok_or(FsError::FileTooBig)?;
                self.files[i].offset = end;
            }
            Ok(())
        });
        self.finish(c, true);
        r
    }

    /// A write's locked step: the call, the offset, and whether it
    /// appends.
    fn write_begin(&mut self, id: FileId) -> Result<(Call, u64, bool), FsError> {
        let f = self.files[self.file_slot(id)?];
        if !f.flags.writes() {
            return Err(FsError::Inval);
        }
        if self.inodes[f.islot as usize].kind == InodeKind::Dir {
            return Err(FsError::IsDir);
        }
        Ok((self.call(f.islot)?, f.offset, f.flags.has(O_APPEND)))
    }

    /// Commit a write of `n` bytes at `pos`: the offset moves past them
    /// unless the file was closed meanwhile (`Badf`), and the offset is
    /// never written back from a snapshot of `refs` or `used`.
    fn write_end(
        &mut self,
        id: FileId,
        mut c: Call,
        wrote: Option<(usize, u64)>,
    ) -> Result<(), FsError> {
        if wrote.is_some() {
            c.ino.mtime = self.now;
            c.ino.ctime = self.now;
        }
        let r = self.file_slot(id).and_then(|i| {
            if let Some((n, pos)) = wrote {
                let end = pos.checked_add(n as u64).ok_or(FsError::FileTooBig)?;
                self.files[i].offset = end;
            }
            Ok(())
        });
        self.finish(c, true);
        r
    }

    /// Move open file `id`'s offset: `SEEK_END` from the inode's size, and
    /// never past the superblock's `max_bytes` (`Inval`).
    fn file_seek(&mut self, id: FileId, pos: SeekFrom) -> Result<u64, FsError> {
        let i = self.file_slot(id)?;
        let f = self.files[i];
        let ino = &self.inodes[f.islot as usize];
        let rel = |base: u64, d: i64| -> Result<u64, FsError> {
            let base = i64::try_from(base).map_err(|_| FsError::Inval)?;
            let n = base.checked_add(d).ok_or(FsError::Inval)?;
            u64::try_from(n).map_err(|_| FsError::Inval)
        };
        let n = match pos {
            SeekFrom::Start(o) => o,
            SeekFrom::Current(d) => rel(f.offset, d)?,
            SeekFrom::End(d) => rel(ino.size, d)?,
        };
        if n > self.supers[ino.sb as usize].maxbytes {
            return Err(FsError::Inval);
        }
        self.files[i].offset = n;
        Ok(n)
    }

    /// A `readdir` step of open directory `id` at `cookie`: `.` and `..`
    /// are made here, the rest come from the backend.
    fn readdir_step(&mut self, id: FileId, cookie: u64) -> Result<Rd, FsError> {
        let f = self.files[self.file_slot(id)?];
        let ino = self.inodes[f.islot as usize];
        if ino.kind != InodeKind::Dir {
            return Err(FsError::NotDir);
        }
        let dot = |name: &[u8], ino: u32| -> Result<Dirent, FsError> {
            Ok(Dirent {
                ino,
                kind: InodeKind::Dir,
                name: Name::from_bytes(name)?,
            })
        };
        match cookie {
            0 => Ok(Rd::Entry(dot(b".", ino.ino)?, 1)),
            1 => {
                let up = match f.dslot {
                    Some(d) => {
                        let (mut m, mut ds) = (f.mount, d);
                        self.dotdot(&mut m, &mut ds);
                        self.inodes[self.d_islot(ds)? as usize].ino
                    }
                    None => ino.ino,
                };
                Ok(Rd::Entry(dot(b"..", up)?, 2))
            }
            n => Ok(Rd::Call(self.call(f.islot)?, n - 2)),
        }
    }

    /// A `getattr` call on inode `islot`, or none when its superblock has
    /// no ops.
    fn stat_call(&mut self, islot: u16) -> Result<Option<Call>, FsError> {
        let sb = self.inodes[islot as usize].sb;
        if self.supers[sb as usize].ops.is_none() {
            return Ok(None);
        }
        self.call(islot).map(Some)
    }

    fn stat_commit(&mut self, c: Call, res: Result<(), FsError>) -> Result<Stat, FsError> {
        let st = self.finish_with(c, res.is_ok(), |n| n.stat());
        res.map(|()| st)
    }

    /// A truncate's call on inode `islot`: a regular file.
    fn truncate_begin(&mut self, islot: u16) -> Result<Call, FsError> {
        match self.inodes[islot as usize].kind {
            InodeKind::Reg => {}
            InodeKind::Dir => return Err(FsError::IsDir),
            InodeKind::Lnk | InodeKind::Chr | InodeKind::Blk => return Err(FsError::Inval),
        }
        self.call(islot)
    }

    fn truncate_commit(&mut self, mut c: Call, res: Result<(), FsError>) -> Result<(), FsError> {
        if res.is_ok() {
            c.ino.mtime = self.now;
            c.ino.ctime = self.now;
        }
        self.finish(c, true);
        res
    }

    // ---- namespace ----

    /// A create's call on directory `dir`, having dropped the negative
    /// dentries a new name makes stale.
    fn create_begin(&mut self, dir: PathRef, name: &[u8]) -> Result<Call, FsError> {
        let di = self.d_islot(dir.dslot)?;
        if self.inodes[di as usize].kind != InodeKind::Dir {
            return Err(FsError::NotDir);
        }
        let sb = self.sb_of(dir.mount);
        self.dcache_drop_neg_in_dir(sb, dir.dslot);
        self.dcache_drop_name(sb, dir.dslot, name);
        self.call(di)
    }

    /// Cache `name` in `dir` as the inode a create made. The file exists
    /// once the backend made it, so a full cache is no error here.
    fn create_commit(
        &mut self,
        dir: PathRef,
        name: &[u8],
        c: Call,
        res: Result<InodeInfo, FsError>,
    ) -> Result<(), FsError> {
        self.finish(c, true);
        let info = res?;
        let sb = self.sb_of(dir.mount);
        self.dcache_drop_name(sb, dir.dslot, name);
        if let Ok(islot) = self.iget_info(sb, &info)
            && self
                .dcache_insert(sb, dir.dslot, name, Some(islot))
                .is_err()
        {
            self.iput(islot);
        }
        Ok(())
    }

    /// A routed create's commit: `name` was made in the directory `c` is
    /// on outside the dentry cache, so its negative dentries go.
    fn create_in_commit(
        &mut self,
        name: &[u8],
        c: Call,
        res: Result<InodeInfo, FsError>,
    ) -> Result<(), FsError> {
        let (sb, dir) = (c.sb, c.ino.slot);
        self.finish(c, true);
        res?;
        let mut i = 0usize;
        while i < MAX_DENTRIES {
            let d = self.dentries[i];
            if d.used
                && d.negative
                && d.sb == sb
                && !d.is_root(i as u16)
                && d.name.eq_bytes(name)
                && self.d_islot(d.parent) == Ok(dir)
            {
                self.dentry_evict(i as u16);
            }
            i += 1;
        }
        Ok(())
    }

    /// An unlink's or rmdir's call on directory `dir`, holding `victim`,
    /// the held dentry `name` resolved to, which this step puts.
    fn remove_begin(
        &mut self,
        dir: PathRef,
        name: &[u8],
        victim: PathRef,
        rmdir: bool,
    ) -> Result<(Call, u16), FsError> {
        let r = self.remove_check(dir, name, victim, rmdir);
        self.path_put(victim);
        let vi = r?;
        let di = self.d_islot(dir.dslot)?;
        self.ihold(vi)?;
        let sb = self.sb_of(dir.mount);
        self.dcache_drop_name(sb, dir.dslot, name);
        match self.call(di) {
            Ok(c) => Ok((c, vi)),
            Err(e) => {
                self.iput(vi);
                Err(e)
            }
        }
    }

    fn remove_check(
        &self,
        dir: PathRef,
        name: &[u8],
        victim: PathRef,
        rmdir: bool,
    ) -> Result<u16, FsError> {
        if self.kind_of(dir)? != InodeKind::Dir {
            return Err(FsError::NotDir);
        }
        if self.is_mountpoint(dir, name) || victim.mount != dir.mount {
            return Err(FsError::Busy);
        }
        let vi = self.d_islot(victim.dslot)?;
        if rmdir && self.inodes[vi as usize].kind != InodeKind::Dir {
            return Err(FsError::NotDir);
        }
        Ok(vi)
    }

    /// Commit an unlink or rmdir: the victim loses a link (a directory
    /// all of them) and, at its last put, its storage.
    fn remove_commit(
        &mut self,
        dir: PathRef,
        name: &[u8],
        c: Call,
        vi: u16,
        res: Result<(), FsError>,
    ) -> Result<(), FsError> {
        self.finish(c, true);
        if res.is_ok() {
            self.unlink_inode(vi);
            let sb = self.sb_of(dir.mount);
            self.dcache_drop_name(sb, dir.dslot, name);
        }
        self.iput(vi);
        res
    }

    /// Inode `i` lost a name: a directory all its links.
    fn unlink_inode(&mut self, i: u16) {
        let now = self.now;
        let v = &mut self.inodes[i as usize];
        v.nlink = if v.kind == InodeKind::Dir {
            0
        } else {
            v.nlink.saturating_sub(1)
        };
        v.ctime = now;
    }

    /// A rename's calls on its two directories, holding the inode it
    /// moves (`src`) and the one it may replace (`tgt`), held paths this
    /// step puts.
    fn rename_begin(
        &mut self,
        (od, oname): (PathRef, &[u8]),
        (nd, nname): (PathRef, &[u8]),
        src: PathRef,
        tgt: Option<PathRef>,
    ) -> Result<RenameCall, FsError> {
        let r = self.rename_check((od, oname), (nd, nname), src, tgt);
        self.path_put(src);
        if let Some(t) = tgt {
            self.path_put(t);
        }
        let (si, ti) = r?;
        self.ihold(si)?;
        if let Some(t) = ti
            && let Err(e) = self.ihold(t)
        {
            self.iput(si);
            return Err(e);
        }
        let sb = self.sb_of(od.mount);
        self.dcache_drop_name(sb, od.dslot, oname);
        self.dcache_drop_name(sb, nd.dslot, nname);
        let calls = self.d_islot(od.dslot).and_then(|o| {
            let a = self.call(o)?;
            match self.d_islot(nd.dslot).and_then(|n| self.call(n)) {
                Ok(b) => Ok((a, b)),
                Err(e) => {
                    self.finish(a, false);
                    Err(e)
                }
            }
        });
        match calls {
            Ok((a, b)) => Ok(RenameCall {
                a,
                b,
                src: si,
                tgt: ti,
            }),
            Err(e) => {
                self.iput(si);
                if let Some(t) = ti {
                    self.iput(t);
                }
                Err(e)
            }
        }
    }

    fn rename_check(
        &self,
        (od, oname): (PathRef, &[u8]),
        (nd, nname): (PathRef, &[u8]),
        src: PathRef,
        tgt: Option<PathRef>,
    ) -> Result<(u16, Option<u16>), FsError> {
        if self.sb_of(od.mount) != self.sb_of(nd.mount) {
            return Err(FsError::Inval);
        }
        if self.is_mountpoint(od, oname)
            || self.is_mountpoint(nd, nname)
            || src.mount != od.mount
            || tgt.is_some_and(|t| t.mount != nd.mount)
        {
            return Err(FsError::Busy);
        }
        let si = self.d_islot(src.dslot)?;
        let ti = match tgt {
            Some(t) => Some(self.d_islot(t.dslot)?),
            None => None,
        };
        Ok((si, ti))
    }

    /// Commit a rename: a replaced inode loses its link, a moved inode
    /// the backend re-keyed moves in the hash, and the stale names go.
    fn rename_commit(
        &mut self,
        (od, oname): (PathRef, &[u8]),
        (nd, nname): (PathRef, &[u8]),
        rc: RenameCall,
        res: Result<Option<Key>, FsError>,
    ) -> Result<(), FsError> {
        let RenameCall { a, b, src, tgt } = rc;
        self.finish(a, true);
        self.finish(b, true);
        let r = res.map(|moved| {
            if let Some(t) = tgt
                && t != src
            {
                self.unlink_inode(t);
            }
            if let Some(to) = moved {
                self.rekey_slot(src, to);
            }
            let sb = self.sb_of(od.mount);
            self.dcache_drop_name(sb, od.dslot, oname);
            self.dcache_drop_name(sb, nd.dslot, nname);
            self.dcache_drop_neg_in_dir(sb, nd.dslot);
        });
        self.iput(src);
        if let Some(t) = tgt {
            self.iput(t);
        }
        r
    }

    /// Move inode `i` to key `to`, and drop the dentries that name it. A
    /// cached inode already at `to` leaves the hash.
    fn rekey_slot(&mut self, i: u16, to: Key) {
        let sb = self.inodes[i as usize].sb;
        if self.inodes[i as usize].key == to {
            return;
        }
        if let Some(t) = self.hashed(sb, to)
            && t != i
        {
            self.unhash(t);
        }
        self.drop_dentries_of(i);
        self.inodes[i as usize].key = to;
    }

    /// A hard link's calls: on directory `nd` and on the regular file
    /// `src` names.
    fn link_begin(&mut self, src: PathRef, nd: PathRef) -> Result<(Call, Call), FsError> {
        let si = self.d_islot(src.dslot)?;
        if self.inodes[si as usize].kind != InodeKind::Reg {
            return Err(FsError::Inval);
        }
        if self.sb_of(src.mount) != self.sb_of(nd.mount) {
            return Err(FsError::Inval);
        }
        let d = self.call(self.d_islot(nd.dslot)?)?;
        match self.call(si) {
            Ok(t) => Ok((d, t)),
            Err(e) => {
                self.finish(d, false);
                Err(e)
            }
        }
    }

    fn link_commit(
        &mut self,
        nd: PathRef,
        name: &[u8],
        (d, t): (Call, Call),
        res: Result<(), FsError>,
    ) -> Result<(), FsError> {
        self.finish(d, true);
        self.finish(t, true);
        res?;
        let sb = self.sb_of(nd.mount);
        self.dcache_drop_neg_in_dir(sb, nd.dslot);
        self.dcache_drop_name(sb, nd.dslot, name);
        Ok(())
    }
}

impl Vfs {
    fn alloc_super(&mut self) -> Result<u8, FsError> {
        let i = self
            .supers
            .iter()
            .position(|s| !s.used)
            .ok_or(FsError::NoSpace)?;
        Ok(i as u8)
    }

    fn alloc_mount(&mut self) -> Result<u8, FsError> {
        let i = self
            .mounts
            .iter()
            .position(|m| !m.used && !m.reserved)
            .ok_or(FsError::NoSpace)?;
        Ok(i as u8)
    }

    /// A counted reference to the inode `info` describes. A used inode
    /// of `sb` with the same key and links is a hit, and its cached state
    /// wins over `info`: it is the authoritative inode, whose words a
    /// lookup never overwrites. An unlinked one (`nlink == 0`) is out of
    /// the hash, so a new file that reuses its key gets a slot of its
    /// own. A new slot counts one reference to its superblock.
    fn iget_info(&mut self, sb: u8, info: &InodeInfo) -> Result<u16, FsError> {
        if let Some(i) = self.hashed(sb, info.key) {
            self.ihold(i)?;
            self.inodes[i as usize].clock = true;
            return Ok(i);
        }
        let slot = self.inode_alloc()?;
        let s = &mut self.supers[sb as usize];
        s.refs = s.refs.checked_add(1).ok_or(FsError::NoSpace)?;
        let g = self.inodes[slot as usize].r#gen.wrapping_add(1);
        self.inodes[slot as usize] = Inode {
            used: true,
            clock: true,
            rel: Rel::No,
            refs: 1,
            sb,
            slot,
            r#gen: g,
            key: info.key,
            ino: info.ino,
            kind: info.kind,
            mode: info.mode,
            nlink: info.nlink,
            size: info.size,
            atime: info.atime,
            mtime: info.mtime,
            ctime: info.ctime,
            private: info.private,
        };
        Ok(slot)
    }

    /// Empty inode slot `i`, keeping its generation, and drop its count
    /// on its superblock.
    fn inode_clear(&mut self, i: usize) {
        if self.inodes[i].used {
            let s = &mut self.supers[self.inodes[i].sb as usize];
            s.refs = s.refs.saturating_sub(1);
        }
        let g = self.inodes[i].r#gen;
        self.inodes[i] = Inode {
            r#gen: g,
            ..Inode::EMPTY
        };
    }

    /// The live slot `h` names: `Badf` when out of range, empty, or of
    /// another generation.
    fn slot_of(&self, h: InodeHandle) -> Result<usize, FsError> {
        let i = h.slot as usize;
        match self.inodes.get(i) {
            Some(n) if n.used && n.r#gen == h.r#gen => Ok(i),
            _ => Err(FsError::Badf),
        }
    }

    fn sb_live(&self, sb: u8) -> Result<(), FsError> {
        match self.supers.get(sb as usize) {
            Some(s) if s.live() => Ok(()),
            _ => Err(FsError::Io),
        }
    }

    /// Take inode `i` out of the hash: no links, and the dentries naming
    /// it dropped. An unreferenced one is cleared without calling its
    /// backend. True when it is still referenced.
    fn unhash(&mut self, i: u16) -> bool {
        self.drop_dentries_of(i);
        let n = &mut self.inodes[i as usize];
        n.nlink = 0;
        if n.refs == 0 && n.rel == Rel::No {
            self.inode_clear(i as usize);
            false
        } else {
            true
        }
    }

    /// Drop a reference. The last one on an inode with no links queues
    /// its release, which a driver runs with the lock dropped; the slot
    /// stays reserved until the backend's `evict` returns.
    fn iput(&mut self, islot: u16) {
        let i = islot as usize;
        if i >= MAX_INODES || !self.inodes[i].used || self.inodes[i].refs == 0 {
            return;
        }
        let n = &mut self.inodes[i];
        n.refs -= 1;
        if n.refs == 0 && n.nlink == 0 && n.rel == Rel::No {
            n.rel = Rel::Queued;
        }
    }

    fn inode_alloc(&mut self) -> Result<u16, FsError> {
        let mut i = 0usize;
        while i < MAX_INODES {
            if !self.inodes[i].used {
                return Ok(i as u16);
            }
            i += 1;
        }
        let mut n = 0usize;
        while n < MAX_INODES * 2 {
            let s = self.ihand as usize % MAX_INODES;
            self.ihand = self.ihand.wrapping_add(1);
            if !self.inodes[s].used {
                return Ok(s as u16);
            }
            if self.inodes[s].refs != 0 || self.inodes[s].rel != Rel::No {
                n += 1;
                continue;
            }
            if self.inodes[s].clock {
                self.inodes[s].clock = false;
                n += 1;
                continue;
            }
            if self.inode_evict(s as u16) {
                return Ok(s as u16);
            }
            n += 1;
        }
        let mut s = 0usize;
        while s < MAX_INODES {
            if self.inode_evict(s as u16) {
                return Ok(s as u16);
            }
            s += 1;
        }
        Err(FsError::NoSpace)
    }

    /// Empty unreferenced, linked inode `slot`. An unlinked one waits for
    /// its release instead.
    fn inode_evict(&mut self, slot: u16) -> bool {
        let n = &self.inodes[slot as usize];
        if !n.used || n.refs != 0 || n.rel != Rel::No || n.nlink == 0 {
            return false;
        }
        self.stats.i_evicts = self.stats.i_evicts.saturating_add(1);
        self.inode_clear(slot as usize);
        true
    }

    /// Whether `name` in `dir` is covered by a mount.
    fn is_mountpoint(&self, dir: PathRef, name: &[u8]) -> bool {
        let sb = self.sb_of(dir.mount);
        match self.dcache_peek(sb, dir.dslot, name) {
            Some(ds) => self.mount_pins(ds) != 0,
            None => false,
        }
    }

    fn child_mount(&self, mount: u8, dslot: u16) -> Option<u8> {
        let mut i = 0u8;
        while i < MAX_MOUNTS as u8 {
            let m = &self.mounts[i as usize];
            if m.used && m.parent == Some(mount) && m.mp_dslot == dslot {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    fn follow_mount(&self, mount: &mut u8, dslot: &mut u16) {
        loop {
            match self.child_mount(*mount, *dslot) {
                Some(c) => {
                    *mount = c;
                    *dslot = self.mounts[c as usize].root_dslot;
                }
                None => return,
            }
        }
    }

    fn dotdot(&self, mount: &mut u8, dslot: &mut u16) {
        let m = *mount;
        if self.mounts[m as usize].root_dslot == *dslot {
            match self.mounts[m as usize].parent {
                None => {}
                Some(p) => {
                    let mp = self.mounts[m as usize].mp_dslot;
                    *mount = p;
                    *dslot = self.dentries[mp as usize].parent;
                }
            }
            return;
        }
        *dslot = self.dentries[*dslot as usize].parent;
    }

    /// The hashed inode of `sb` keyed `key`.
    fn hashed(&self, sb: u8, key: Key) -> Option<u16> {
        let mut i = 0usize;
        while i < MAX_INODES {
            let n = &self.inodes[i];
            if n.used && n.sb == sb && n.key == key && n.nlink != 0 {
                return Some(i as u16);
            }
            i += 1;
        }
        None
    }

    /// Count one more reference to used inode `islot`.
    fn ihold(&mut self, islot: u16) -> Result<(), FsError> {
        let n = &mut self.inodes[islot as usize];
        n.refs = n.refs.checked_add(1).ok_or(FsError::NoSpace)?;
        Ok(())
    }

    /// Drop the dentries naming inode `i` that nothing but their own
    /// descendants holds, releasing their counts on it without a put.
    fn drop_dentries_of(&mut self, i: u16) {
        let mut d = 0usize;
        while d < MAX_DENTRIES {
            let e = self.dentries[d];
            if e.used && !e.negative && e.islot == i && !e.is_root(d as u16) {
                if e.refs != 0 && e.refs == self.child_count(d as u16) {
                    self.dentry_prune(d as u16);
                }
                if self.dentries[d].refs == 0 {
                    self.stats.d_evicts = self.stats.d_evicts.saturating_add(1);
                    self.dentries[d] = Dentry::EMPTY;
                    self.dput(e.parent);
                    let r = &mut self.inodes[i as usize].refs;
                    *r = r.saturating_sub(1);
                } else {
                    self.dentries[d].dead = true;
                }
            }
            d += 1;
        }
    }

    fn dentry_force_alloc(&mut self) -> Result<u16, FsError> {
        let mut i = 0usize;
        while i < MAX_DENTRIES {
            if !self.dentries[i].used {
                return Ok(i as u16);
            }
            i += 1;
        }
        let mut n = 0usize;
        while n < MAX_DENTRIES * 2 {
            let s = self.dhand as usize % MAX_DENTRIES;
            self.dhand = self.dhand.wrapping_add(1);
            if !self.dentries[s].used {
                return Ok(s as u16);
            }
            if self.dentries[s].refs != 0 {
                n += 1;
                continue;
            }
            if self.dentries[s].clock {
                self.dentries[s].clock = false;
                n += 1;
                continue;
            }
            self.dentry_evict(s as u16);
            return Ok(s as u16);
        }
        let mut s = 0usize;
        while s < MAX_DENTRIES {
            if self.dentries[s].used && self.dentries[s].refs == 0 {
                self.dentry_evict(s as u16);
                return Ok(s as u16);
            }
            s += 1;
        }
        Err(FsError::NoSpace)
    }

    /// Free an unheld dentry: drop its inode reference and, for a
    /// non-root dentry, its hold on its parent.
    fn dentry_evict(&mut self, slot: u16) {
        let d = self.dentries[slot as usize];
        if !d.used || d.refs != 0 {
            return;
        }
        self.stats.d_evicts = self.stats.d_evicts.saturating_add(1);
        self.dentries[slot as usize] = Dentry::EMPTY;
        if !d.is_root(slot) {
            self.dput(d.parent);
        }
        if !d.negative {
            self.iput(d.islot);
        }
    }

    /// Evict the unheld dentries below `top`, leaves first. What a mount
    /// or an explicit hold keeps stays, and so do its ancestors.
    fn dentry_prune(&mut self, top: u16) {
        loop {
            let mut hit = false;
            let mut i = 0usize;
            while i < MAX_DENTRIES {
                let d = &self.dentries[i];
                if d.used && d.refs == 0 && i as u16 != top && self.below(i as u16, top) {
                    self.dentry_evict(i as u16);
                    hit = true;
                }
                i += 1;
            }
            if !hit {
                return;
            }
        }
    }

    /// Whether dentry `slot` lies strictly below `top`.
    fn below(&self, slot: u16, top: u16) -> bool {
        let mut cur = slot;
        let mut n = 0usize;
        while n < MAX_DENTRIES {
            let d = &self.dentries[cur as usize];
            if !d.used || d.is_root(cur) {
                return false;
            }
            cur = d.parent;
            if cur == top {
                return true;
            }
            n += 1;
        }
        false
    }

    /// Count one holder of dentry `slot`.
    fn dget(&mut self, slot: u16) -> Result<(), FsError> {
        let d = &mut self.dentries[slot as usize];
        d.refs = d.refs.checked_add(1).ok_or(FsError::NoSpace)?;
        Ok(())
    }

    /// Drop one holder of dentry `slot`. A live dentry stays for clock
    /// eviction to reclaim; a dead one is evicted at its last put.
    fn dput(&mut self, slot: u16) {
        let d = &mut self.dentries[slot as usize];
        debug_assert!(d.refs != 0, "dput of an unheld dentry");
        d.refs = d.refs.saturating_sub(1);
        if d.refs == 0 && d.dead {
            self.dentry_evict(slot);
        }
    }

    /// Used dentries whose parent is `slot`.
    fn child_count(&self, slot: u16) -> u16 {
        let mut n = 0u16;
        let mut i = 0usize;
        while i < MAX_DENTRIES {
            let d = &self.dentries[i];
            if d.used && d.parent == slot && !d.is_root(i as u16) {
                n = n.saturating_add(1);
            }
            i += 1;
        }
        n
    }

    /// Mounts whose mountpoint is dentry `slot`.
    fn mount_pins(&self, slot: u16) -> u16 {
        let mut n = 0u16;
        for m in self.mounts.iter() {
            if m.used && m.parent.is_some() && m.mp_dslot == slot {
                n = n.saturating_add(1);
            }
        }
        n
    }

    /// The holds dentry `slot` has with no explicit hold: its children,
    /// the mounts on it, and the superblock's hold on a root dentry.
    fn expected_holds(&self, slot: u16) -> u16 {
        let root = u16::from(self.dentries[slot as usize].is_root(slot));
        self.child_count(slot)
            .saturating_add(self.mount_pins(slot))
            .saturating_add(root)
    }

    /// Positive dentries that name inode `islot`.
    fn naming_dentries(&self, islot: u16) -> u16 {
        let mut n = 0u16;
        for d in self.dentries.iter() {
            if d.used && !d.negative && d.islot == islot {
                n = n.saturating_add(1);
            }
        }
        n
    }

    fn dcache_peek(&self, sb: u8, parent: u16, name: &[u8]) -> Option<u16> {
        let mut i = 0usize;
        while i < MAX_DENTRIES {
            let d = &self.dentries[i];
            if d.used
                && !d.dead
                && d.sb == sb
                && d.parent == parent
                && !d.is_root(i as u16)
                && d.name.eq_bytes(name)
            {
                return Some(i as u16);
            }
            i += 1;
        }
        None
    }

    fn dcache_find(&mut self, sb: u8, parent: u16, name: &[u8]) -> Option<u16> {
        let ds = self.dcache_peek(sb, parent, name)?;
        self.dentries[ds as usize].clock = true;
        Some(ds)
    }

    /// Cache `name` in `parent`, positive when `islot` is given. The
    /// parent is held before the allocation, so it cannot be the slot the
    /// allocation evicts.
    fn dcache_insert(
        &mut self,
        sb: u8,
        parent: u16,
        name: &[u8],
        islot: Option<u16>,
    ) -> Result<u16, FsError> {
        let nm = Name::from_bytes(name)?;
        self.dget(parent)?;
        let slot = match self.dentry_force_alloc() {
            Ok(s) => s,
            Err(e) => {
                self.dput(parent);
                return Err(e);
            }
        };
        self.dentries[slot as usize] = Dentry {
            used: true,
            clock: true,
            negative: islot.is_none(),
            dead: false,
            refs: 0,
            parent,
            sb,
            name: nm,
            islot: islot.unwrap_or(0),
        };
        Ok(slot)
    }

    /// Forget `name` in `parent`. An unheld dentry is evicted; one held
    /// only by its descendants is pruned with its subtree; one a mount or
    /// an explicit hold keeps stays.
    fn dcache_drop_name(&mut self, sb: u8, parent: u16, name: &[u8]) {
        let Some(ds) = self.dcache_peek(sb, parent, name) else {
            return;
        };
        let d = &self.dentries[ds as usize];
        if d.refs != 0 && d.refs == self.child_count(ds) {
            self.dentry_prune(ds);
        }
        self.dentry_evict(ds);
        if self.dentries[ds as usize].used {
            self.dentries[ds as usize].dead = true;
        }
    }

    fn dcache_drop_neg_in_dir(&mut self, sb: u8, parent: u16) {
        let mut i = 0usize;
        while i < MAX_DENTRIES {
            let d = &self.dentries[i];
            if d.used && d.sb == sb && d.parent == parent && d.negative && !d.is_root(i as u16) {
                self.dentry_evict(i as u16);
            }
            i += 1;
        }
    }
}

/// The one path walker, resumable: [`Walker::step`] walks under the VFS
/// lock as far as the dentry cache reaches and returns the backend call a
/// miss needs (a lookup, or a symlink's readlink); the driver makes it
/// with the lock dropped, and [`Walker::resume`] caches its result and
/// the walk goes on. A pending call pins the walk's directory.
pub struct Walker {
    rem: [u8; MAX_PATH],
    rem_len: usize,
    cwd: Option<PathRef>,
    mount: u8,
    dslot: u16,
    depth: u32,
    steps: u32,
    jump_root: bool,
    follow_last: bool,
    started: bool,
    comp: [u8; MAX_NAME],
    clen: usize,
    /// The end, in `rem`, of the component a pending call is for.
    at: usize,
}

/// Where a walk step stopped: at the held path it resolved, or at a
/// backend call.
#[expect(
    clippy::large_enum_variant,
    reason = "a walk step lives on the stack: boxing it would allocate on every open (AGENTS.md rule 4)"
)]
pub enum WalkStep {
    Done(PathRef),
    Call(WalkCall),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Need {
    Lookup,
    Readlink,
}

/// A backend call a walk needs, on the directory it pins (`dir`): a
/// lookup of the walk's current component in it, or the readlink of the
/// link found there.
pub struct WalkCall {
    need: Need,
    call: Call,
    dir: PathRef,
}

/// A walk call's result.
pub enum WalkReply {
    Found(Result<InodeInfo, FsError>),
    Link(Result<usize, FsError>, [u8; MAX_FILE_BYTES]),
}

impl WalkCall {
    /// Make the call, with the VFS lock dropped.
    pub fn run(&mut self, w: &Walker) -> WalkReply {
        match self.need {
            Need::Lookup => {
                let name = w.comp();
                WalkReply::Found(self.call.run(|o, cx, d| o.lookup(cx, d, name)))
            }
            Need::Readlink => {
                let mut buf = [0u8; MAX_FILE_BYTES];
                let r = self.call.run(|o, cx, n| o.readlink(cx, n, &mut buf));
                WalkReply::Link(r, buf)
            }
        }
    }
}

impl Walker {
    /// A walk of `path` from `cwd` (the root when none, or when `path` is
    /// absolute); `follow_last` follows a symlink in the last component.
    pub fn new(cwd: Option<PathRef>, path: &[u8], follow_last: bool) -> Result<Self, FsError> {
        if path.is_empty() {
            return Err(FsError::Inval);
        }
        if path.len() > MAX_PATH {
            return Err(FsError::NameTooLong);
        }
        let mut rem = [0u8; MAX_PATH];
        rem[..path.len()].copy_from_slice(path);
        Ok(Self {
            rem,
            rem_len: path.len(),
            cwd,
            mount: 0,
            dslot: 0,
            depth: 0,
            steps: 0,
            jump_root: path[0] == b'/',
            follow_last,
            started: false,
            comp: [0; MAX_NAME],
            clen: 0,
            at: 0,
        })
    }

    fn comp(&self) -> &[u8] {
        &self.comp[..self.clen]
    }

    /// Walk under the lock until the path resolves, which returns it
    /// held ([`Vfs::path_put`] releases it), or a miss needs a backend
    /// call.
    pub fn step(&mut self, v: &mut Vfs) -> Result<WalkStep, FsError> {
        if !self.started {
            let (m, d) = match self.cwd {
                Some(p) if !self.jump_root => (p.mount, p.dslot),
                _ => {
                    let r = v.root()?;
                    (r.mount, r.dslot)
                }
            };
            self.mount = m;
            self.dslot = d;
            v.follow_mount(&mut self.mount, &mut self.dslot);
            self.started = true;
        }
        loop {
            self.steps += 1;
            if self.steps > MAX_WALK {
                return Err(FsError::Loop);
            }
            let mut i = 0usize;
            while i < self.rem_len && self.rem[i] == b'/' {
                i += 1;
            }
            if self.jump_root {
                let r = v.root()?;
                self.mount = r.mount;
                self.dslot = r.dslot;
                self.jump_root = false;
            }
            if i == self.rem_len {
                v.follow_mount(&mut self.mount, &mut self.dslot);
                let p = PathRef {
                    mount: self.mount,
                    dslot: self.dslot,
                };
                v.path_get(p)?;
                return Ok(WalkStep::Done(p));
            }
            let mut j = i;
            while j < self.rem_len && self.rem[j] != b'/' {
                j += 1;
            }
            let clen = j - i;
            if clen > MAX_NAME {
                return Err(FsError::NameTooLong);
            }
            self.comp[..clen].copy_from_slice(&self.rem[i..j]);
            self.clen = clen;
            let mut k = j;
            while k < self.rem_len && self.rem[k] == b'/' {
                k += 1;
            }
            let last = k == self.rem_len;
            if name_is_dot(self.comp()) {
                shift_down(&mut self.rem, &mut self.rem_len, j);
                continue;
            }
            if name_is_dotdot(self.comp()) {
                v.dotdot(&mut self.mount, &mut self.dslot);
                shift_down(&mut self.rem, &mut self.rem_len, j);
                continue;
            }
            let dir = PathRef {
                mount: self.mount,
                dslot: self.dslot,
            };
            let sb = v.sb_of(dir.mount);
            let child = match v.dcache_find(sb, dir.dslot, &self.comp[..clen]) {
                Some(ds) if v.dentries[ds as usize].negative => return Err(FsError::NotFound),
                Some(ds) => ds,
                None => {
                    let call = v.call(v.d_islot(dir.dslot)?)?;
                    return self.pend(v, Need::Lookup, call, dir, j);
                }
            };
            let islot = v.d_islot(child)?;
            if v.inodes[islot as usize].kind == InodeKind::Lnk && (!last || self.follow_last) {
                if self.depth >= MAX_SYMLINK {
                    return Err(FsError::Loop);
                }
                self.depth += 1;
                let call = v.call(islot)?;
                return self.pend(v, Need::Readlink, call, dir, j);
            }
            self.dslot = child;
            v.follow_mount(&mut self.mount, &mut self.dslot);
            shift_down(&mut self.rem, &mut self.rem_len, j);
        }
    }

    /// Stop at `call`, pinning `dir`, for the component ending at `j`.
    fn pend(
        &mut self,
        v: &mut Vfs,
        need: Need,
        call: Call,
        dir: PathRef,
        j: usize,
    ) -> Result<WalkStep, FsError> {
        if let Err(e) = v.path_get(dir) {
            v.finish(call, false);
            return Err(e);
        }
        self.at = j;
        Ok(WalkStep::Call(WalkCall { need, call, dir }))
    }

    /// Take a walk call's result under the lock: cache a lookup's dentry,
    /// positive or negative (one another walk cached meanwhile is kept),
    /// or splice a link's target into the path. The next
    /// [`Walker::step`] goes on from there.
    pub fn resume(&mut self, v: &mut Vfs, wc: WalkCall, reply: WalkReply) -> Result<(), FsError> {
        let WalkCall { need, call, dir } = wc;
        let sb = v.sb_of(dir.mount);
        let r = match (need, reply) {
            (Need::Lookup, WalkReply::Found(res)) => {
                // The component is walked again, from the cache.
                self.steps = self.steps.saturating_sub(1);
                let cached = v.dcache_peek(sb, dir.dslot, self.comp()).is_some();
                match res {
                    Ok(_) if cached => Ok(()),
                    Ok(info) => v.iget_info(sb, &info).and_then(|islot| {
                        v.dcache_insert(sb, dir.dslot, self.comp(), Some(islot))
                            .map(|_| ())
                            .inspect_err(|_| v.iput(islot))
                    }),
                    Err(FsError::NotFound) => {
                        if !cached {
                            let _ = v.dcache_insert(sb, dir.dslot, self.comp(), None);
                        }
                        Err(FsError::NotFound)
                    }
                    Err(e) => Err(e),
                }
            }
            (Need::Readlink, WalkReply::Link(Ok(n), tgt)) => self.splice(&tgt[..n.min(tgt.len())]),
            (Need::Readlink, WalkReply::Link(Err(e), _)) => Err(e),
            _ => Err(FsError::Io),
        };
        v.finish(call, false);
        v.path_put(dir);
        r
    }

    /// Replace the path walked so far, through the link's component, with
    /// the link's target `tgt` followed by the rest.
    fn splice(&mut self, tgt: &[u8]) -> Result<(), FsError> {
        let mut rest = [0u8; MAX_PATH];
        let rlen = self.rem_len - self.at;
        rest[..rlen].copy_from_slice(&self.rem[self.at..self.rem_len]);
        let mut joined = [0u8; MAX_PATH];
        let jl = join_path(tgt, &rest[..rlen], &mut joined)?;
        self.rem[..jl].copy_from_slice(&joined[..jl]);
        self.rem_len = jl;
        self.jump_root = tgt.first() == Some(&b'/');
        Ok(())
    }
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

    /// Resolve `path` to a held path; [`Self::put_path`] releases it.
    pub fn walk(
        &self,
        cwd: Option<PathRef>,
        path: &[u8],
        follow: bool,
    ) -> Result<PathRef, FsError> {
        let mut w = Walker::new(cwd, path, follow)?;
        let mut reply: Option<(WalkCall, WalkReply)> = None;
        loop {
            let st = self.step(|v| {
                if let Some((wc, r)) = reply.take() {
                    w.resume(v, wc, r)?;
                }
                w.step(v)
            })?;
            match st {
                WalkStep::Done(p) => return Ok(p),
                WalkStep::Call(mut wc) => {
                    let r = wc.run(&w);
                    reply = Some((wc, r));
                }
            }
        }
    }

    pub fn put_path(&self, p: PathRef) {
        self.step(|v| v.path_put(p));
    }

    /// Resolve `path`'s parent to a held directory and name its last
    /// component, which is neither `.` nor `..`.
    fn walk_parent<'p>(
        &self,
        cwd: Option<PathRef>,
        path: &'p [u8],
    ) -> Result<(PathRef, &'p [u8]), FsError> {
        let (parent, name) = split_basename(path)?;
        if name_is_dot(name) || name_is_dotdot(name) {
            return Err(FsError::Inval);
        }
        let dir = self.walk(cwd, parent, true)?;
        match self.with(|v| v.kind_of(dir)) {
            Ok(InodeKind::Dir) => Ok((dir, name)),
            r => {
                self.put_path(dir);
                Err(r.err().unwrap_or(FsError::NotDir))
            }
        }
    }

    /// Open `path` (C-FILEAPI `open`): `O_CREAT` creates a regular file
    /// that is not there, `O_TRUNC` empties a regular file.
    pub fn open(
        &self,
        cwd: Option<PathRef>,
        path: &[u8],
        flags: OpenFlags,
        mode: u32,
    ) -> Result<FileRef, FsError> {
        let follow = !flags.has(O_NOFOLLOW);
        if flags.has(O_CREAT) {
            match self.walk(cwd, path, follow) {
                Ok(p) => {
                    self.put_path(p);
                    if flags.has(O_EXCL) {
                        return Err(FsError::Exists);
                    }
                }
                Err(FsError::NotFound) => {
                    let mode = file_mode(mode) | S_IFREG;
                    if (self.hooks.open_race)() {
                        self.create(cwd, path, InodeKind::Reg, mode, None)?;
                    }
                    match self.create(cwd, path, InodeKind::Reg, mode, None) {
                        Ok(()) => {}
                        // Created since the walk: without O_EXCL, open it.
                        Err(FsError::Exists) if !flags.has(O_EXCL) => {}
                        Err(e) => return Err(e),
                    }
                }
                Err(e) => return Err(e),
            }
        }
        let p = self.walk(cwd, path, follow)?;
        let id = self.step(|v| {
            let r = v.open(p, flags);
            v.path_put(p);
            r
        })?;
        self.opened(FileRef::from_raw(id), flags)
    }

    /// Finish an open: truncate a regular file for `O_TRUNC`.
    fn opened(&self, f: FileRef, flags: OpenFlags) -> Result<FileRef, FsError> {
        if flags.has(O_TRUNC)
            && self.with(|v| v.file_kind(f.id)) == Ok(InodeKind::Reg)
            && let Err(e) = self.ftruncate(&f, 0)
        {
            let _ = self.close(f);
            return Err(e);
        }
        Ok(f)
    }

    /// Open the inode `r` names, found by a walk outside the dentry
    /// cache; `r` is put.
    pub fn open_inode(&self, r: InodeRef, flags: OpenFlags) -> Result<FileRef, FsError> {
        let id = self.step(|v| v.open_inode(r, flags))?;
        self.opened(FileRef::from_raw(id), flags)
    }

    /// Drop a reference an [`InodeRef`] held.
    pub fn put(&self, r: InodeRef) {
        self.step(|v| v.put_ref(r));
    }

    /// A new counted reference to open file `id`, for one syscall.
    pub fn fget(&self, id: FileId) -> Result<FileRef, FsError> {
        self.with(|v| v.file_addref(id))?;
        Ok(FileRef::from_raw(id))
    }

    /// One more count on open file `id`, which an fd table holds.
    pub fn addref(&self, id: FileId) -> Result<(), FsError> {
        self.with(|v| v.file_addref(id))
    }

    pub fn close(&self, f: FileRef) -> Result<(), FsError> {
        self.step(|v| v.file_close(f.into_raw()))
    }

    pub fn read(&self, f: &FileRef, buf: &mut [u8]) -> Result<usize, FsError> {
        let (mut c, off) = self.with(|v| v.read_begin(f.id))?;
        let r = c.run(|o, cx, n| o.read(cx, n, off, buf));
        let end = self.step(|v| v.read_end(f.id, c, r.as_ref().ok().copied(), off));
        let n = r?;
        end?;
        Ok(n)
    }

    pub fn write(&self, f: &FileRef, buf: &[u8]) -> Result<usize, FsError> {
        let (mut c, off, append) = self.with(|v| v.write_begin(f.id))?;
        let r = if append {
            c.run(|o, cx, n| o.write_append(cx, n, buf))
        } else {
            c.run(|o, cx, n| o.write(cx, n, off, buf).map(|k| (k, off)))
        };
        if r.is_ok() {
            (self.hooks.write_window)();
        }
        let end = self.step(|v| v.write_end(f.id, c, r.as_ref().ok().copied()));
        let (n, _) = r?;
        end?;
        Ok(n)
    }

    pub fn seek(&self, f: &FileRef, pos: SeekFrom) -> Result<u64, FsError> {
        self.with(|v| v.file_seek(f.id, pos))
    }

    pub fn stat(&self, f: &FileRef) -> Result<Stat, FsError> {
        let islot = self.with(|v| v.file_islot(f.id))?;
        self.stat_islot(islot)
    }

    /// Stat inode `islot`, which the caller keeps referenced, through a
    /// `getattr` call.
    fn stat_islot(&self, islot: u16) -> Result<Stat, FsError> {
        let c = self.with(|v| match v.stat_call(islot) {
            Ok(Some(c)) => Ok(Ok(c)),
            Ok(None) => Ok(Err(v.inodes[islot as usize].stat())),
            Err(e) => Err(e),
        })?;
        match c {
            Err(st) => Ok(st),
            Ok(mut c) => {
                let r = c.run(|o, cx, n| o.getattr(cx, n));
                self.step(|v| v.stat_commit(c, r))
            }
        }
    }

    /// Set open file `f`'s size, whatever its access mode.
    pub fn ftruncate(&self, f: &FileRef, size: u64) -> Result<(), FsError> {
        let mut c = self.with(|v| v.file_islot(f.id).and_then(|i| v.truncate_begin(i)))?;
        let r = c.run(|o, cx, n| o.truncate(cx, n, size));
        self.step(|v| v.truncate_commit(c, r))
    }

    /// Report each entry of open directory `f` to `cb`, `.` and `..`
    /// first, until `cb` returns false. `cb` runs with the lock dropped.
    pub fn readdir(
        &self,
        f: &FileRef,
        cb: &mut dyn FnMut(&DirEntry) -> bool,
    ) -> Result<(), FsError> {
        let mut cookie = 0u64;
        loop {
            let (ent, next) = match self.with(|v| v.readdir_step(f.id, cookie))? {
                Rd::Entry(d, next) => (d, next),
                Rd::Call(mut c, bc) => {
                    let mut out = Dirent::EMPTY;
                    let r = c.run(|o, cx, d| o.readdir(cx, d, bc, &mut out));
                    self.step(|v| v.finish(c, false));
                    match r? {
                        None => return Ok(()),
                        Some(n) => (out, n.checked_add(2).ok_or(FsError::Io)?),
                    }
                }
            };
            cookie = next;
            if !cb(&ent) {
                return Ok(());
            }
        }
    }

    /// `stat` (`follow`) or `lstat` of `path`.
    pub fn stat_path(
        &self,
        cwd: Option<PathRef>,
        path: &[u8],
        follow: bool,
    ) -> Result<Stat, FsError> {
        let p = self.walk(cwd, path, follow)?;
        let r = self.with(|v| v.islot(p)).and_then(|i| self.stat_islot(i));
        self.put_path(p);
        r
    }

    /// Create `path` as a `kind` node; `Exists` when it is there.
    pub fn create(
        &self,
        cwd: Option<PathRef>,
        path: &[u8],
        kind: InodeKind,
        mode: u16,
        target: Option<&[u8]>,
    ) -> Result<(), FsError> {
        let (dir, name) = self.walk_parent(cwd, path)?;
        let r = self.with(|v| v.create_begin(dir, name)).and_then(|mut c| {
            let res = c.run(|o, cx, d| o.create(cx, d, name, kind, mode, target));
            self.step(|v| v.create_commit(dir, name, c, res))
        });
        self.put_path(dir);
        r
    }

    pub fn mkdir(&self, cwd: Option<PathRef>, path: &[u8], mode: u32) -> Result<(), FsError> {
        self.create(cwd, path, InodeKind::Dir, file_mode(mode) | S_IFDIR, None)
    }

    pub fn symlink(&self, cwd: Option<PathRef>, path: &[u8], target: &[u8]) -> Result<(), FsError> {
        if target.is_empty() {
            return Err(FsError::Inval);
        }
        self.create(cwd, path, InodeKind::Lnk, S_IFLNK_MODE, Some(target))
    }

    /// Create `name` in the directory `dir` names, found by a walk
    /// outside the dentry cache, as a `kind` node.
    pub fn create_in(
        &self,
        dir: &InodeRef,
        name: &[u8],
        kind: InodeKind,
        mode: u16,
    ) -> Result<(), FsError> {
        if name_is_dot(name) || name_is_dotdot(name) {
            return Err(FsError::Inval);
        }
        let mut c = self.with(|v| {
            let i = v.slot_of(dir.handle())?;
            if v.inodes[i].kind != InodeKind::Dir {
                return Err(FsError::NotDir);
            }
            v.call(i as u16)
        })?;
        let res = c.run(|o, cx, d| o.create(cx, d, name, kind, mode, None));
        self.step(|v| v.create_in_commit(name, c, res))
    }

    pub fn unlink(&self, cwd: Option<PathRef>, path: &[u8]) -> Result<(), FsError> {
        self.remove(cwd, path, false)
    }

    pub fn rmdir(&self, cwd: Option<PathRef>, path: &[u8]) -> Result<(), FsError> {
        self.remove(cwd, path, true)
    }

    fn remove(&self, cwd: Option<PathRef>, path: &[u8], rmdir: bool) -> Result<(), FsError> {
        let (dir, name) = self.walk_parent(cwd, path)?;
        let r = self.walk(Some(dir), name, false).and_then(|victim| {
            let (mut c, vi) = self.step(|v| v.remove_begin(dir, name, victim, rmdir))?;
            let res = c.run(|o, cx, d| {
                if rmdir {
                    o.rmdir(cx, d, name)
                } else {
                    o.unlink(cx, d, name)
                }
            });
            self.step(|v| v.remove_commit(dir, name, c, vi, res))
        });
        self.put_path(dir);
        r
    }

    pub fn rename(&self, cwd: Option<PathRef>, old: &[u8], new: &[u8]) -> Result<(), FsError> {
        let (od, oname) = self.walk_parent(cwd, old)?;
        let r = self.walk_parent(cwd, new).and_then(|(nd, nname)| {
            let r = self.rename_in((od, oname), (nd, nname));
            self.put_path(nd);
            r
        });
        self.put_path(od);
        r
    }

    fn rename_in(&self, o: (PathRef, &[u8]), n: (PathRef, &[u8])) -> Result<(), FsError> {
        let src = self.walk(Some(o.0), o.1, false)?;
        let tgt = match self.walk(Some(n.0), n.1, false) {
            Ok(t) => Some(t),
            Err(FsError::NotFound) => None,
            Err(e) => {
                self.put_path(src);
                return Err(e);
            }
        };
        let mut rc = self.step(|v| v.rename_begin(o, n, src, tgt))?;
        let res =
            rc.a.run2(&mut rc.b, |ops, cx, x, y| ops.rename(cx, x, o.1, y, n.1));
        self.step(|v| v.rename_commit(o, n, rc, res))
    }

    /// Hard link `new` to the regular file `old` names.
    pub fn link(&self, cwd: Option<PathRef>, old: &[u8], new: &[u8]) -> Result<(), FsError> {
        let src = self.walk(cwd, old, true)?;
        let r = match self.with(|v| v.kind_of(src)) {
            Ok(InodeKind::Reg) => self.walk_parent(cwd, new).and_then(|(nd, name)| {
                let r = self
                    .with(|v| v.link_begin(src, nd))
                    .and_then(|(mut d, mut t)| {
                        let res = d.run2(&mut t, |o, cx, dir, tg| o.link(cx, dir, name, tg));
                        self.step(|v| v.link_commit(nd, name, (d, t), res))
                    });
                self.put_path(nd);
                r
            }),
            Ok(_) => Err(FsError::Inval),
            Err(e) => Err(e),
        };
        self.put_path(src);
        r
    }

    /// Set the size of the regular file `path` names.
    pub fn truncate(&self, cwd: Option<PathRef>, path: &[u8], size: u64) -> Result<(), FsError> {
        let p = self.walk(cwd, path, true)?;
        let r = self
            .with(|v| v.islot(p).and_then(|i| v.truncate_begin(i)))
            .and_then(|mut c| {
                let res = c.run(|o, cx, n| o.truncate(cx, n, size));
                self.step(|v| v.truncate_commit(c, res))
            });
        self.put_path(p);
        r
    }

    /// Mount `fs` on directory `at` (C-FILEAPI `mount`'s core); see
    /// [`Vfs::super_of_dev`] for a device already mounted.
    pub fn mount_fs(
        &self,
        cwd: Option<PathRef>,
        at: &[u8],
        fs: &'static dyn FileSystem,
        dev: Option<u64>,
        ro: bool,
    ) -> Result<Mounted, FsError> {
        let p = self.walk(cwd, at, true)?;
        let r = self.mount_at(Some(p), at, fs, dev, ro);
        self.put_path(p);
        r
    }

    /// Mount `fs` as the root.
    pub fn mount_root(
        &self,
        fs: &'static dyn FileSystem,
        dev: Option<u64>,
        ro: bool,
    ) -> Result<Mounted, FsError> {
        self.mount_at(None, b"/", fs, dev, ro)
    }

    fn mount_at(
        &self,
        at: Option<PathRef>,
        path: &[u8],
        fs: &'static dyn FileSystem,
        dev: Option<u64>,
        ro: bool,
    ) -> Result<Mounted, FsError> {
        let (m, mut call) = match self.with(|v| v.mount_begin(at, fs, dev, ro))? {
            MountStep::Done(m, call) => (m, call),
            MountStep::Fill(mut f) => {
                let res = f.call.run(|_, _, cx| fs.fill_super(cx));
                let private = f.call.private;
                match self.step(|v| v.mount_commit(at, f, res, private)) {
                    Ok(done) => done,
                    Err((e, kill)) => {
                        if let Some(mut k) = kill {
                            k.run(|_, ops, cx| {
                                if let Some(o) = ops {
                                    o.kill_sb(cx);
                                }
                            });
                            self.with(|v| v.sb_idle(k.sb, true));
                        }
                        return Err(e);
                    }
                }
            }
        };
        call.run(|fs, _, cx| {
            if let Some(fs) = fs {
                fs.on_mount(cx, path);
            }
        });
        self.with(|v| v.sb_idle(call.sb, false));
        Ok(m)
    }

    /// Unmount the mount whose root `at` names; the superblock's last
    /// mount releases it, its hooks run with the lock dropped.
    pub fn umount(&self, cwd: Option<PathRef>, at: &[u8]) -> Result<(), FsError> {
        let mut tries = 0usize;
        loop {
            let p = self.walk(cwd, at, true)?;
            match self.step(|v| v.umount_step(p))? {
                UmountStep::Release(mut c) => {
                    let ok = c.run(|o, cx, n| o.evict(cx, n)).is_ok();
                    self.with(|v| v.release_done(c, ok));
                    tries += 1;
                    if !ok || tries > MAX_INODES {
                        return Err(FsError::Busy);
                    }
                }
                UmountStep::Done(mut u) => {
                    let last = u.last;
                    u.call.run(|fs, ops, cx| {
                        if let Some(fs) = fs {
                            fs.on_umount(cx, at, last);
                        }
                        if last && let Some(o) = ops {
                            o.kill_sb(cx);
                        }
                    });
                    self.with(|v| v.sb_idle(u.sb, last));
                    return Ok(());
                }
            }
        }
    }

    /// Write every mounted superblock's dirty state to its device; the
    /// first error.
    pub fn sync(&self) -> Result<(), FsError> {
        let mut out = Ok(());
        let mut next = 0u8;
        while let Some(mut c) = self.with(|v| v.sync_begin(next)) {
            let r = c.run(|_, ops, cx| ops.map_or(Ok(()), |o| o.sync(cx)));
            self.with(|v| v.sb_idle(c.sb, false));
            if out.is_ok() {
                out = r;
            }
            next = c.sb.saturating_add(1);
        }
        out
    }
}

/// A mode argument's permission bits.
fn file_mode(mode: u32) -> u16 {
    (mode & 0o7777) as u16
}

fn shift_down(rem: &mut [u8; MAX_PATH], rem_len: &mut usize, from: usize) {
    if from >= *rem_len {
        *rem_len = 0;
        return;
    }
    let n = *rem_len - from;
    let mut tmp = [0u8; MAX_PATH];
    tmp[..n].copy_from_slice(&rem[from..*rem_len]);
    rem[..n].copy_from_slice(&tmp[..n]);
    *rem_len = n;
}

fn join_path(target: &[u8], rest: &[u8], out: &mut [u8; MAX_PATH]) -> Result<usize, FsError> {
    let mut rest = rest;
    while rest.first() == Some(&b'/') {
        rest = &rest[1..];
    }
    if target.len() > MAX_PATH {
        return Err(FsError::NameTooLong);
    }
    out[..target.len()].copy_from_slice(target);
    let mut n = target.len();
    if !rest.is_empty() {
        if n == 0 || out[n - 1] != b'/' {
            if n >= MAX_PATH {
                return Err(FsError::NameTooLong);
            }
            out[n] = b'/';
            n += 1;
        }
        if n + rest.len() > MAX_PATH {
            return Err(FsError::NameTooLong);
        }
        out[n..n + rest.len()].copy_from_slice(rest);
        n += rest.len();
    }
    Ok(n)
}

pub fn split_basename(path: &[u8]) -> Result<(&[u8], &[u8]), FsError> {
    if path.is_empty() {
        return Err(FsError::Inval);
    }
    let mut end = path.len();
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return Err(FsError::Inval);
    }
    let p = &path[..end];
    let mut slash: Option<usize> = None;
    let mut i = 0usize;
    while i < p.len() {
        if p[i] == b'/' {
            slash = Some(i);
        }
        i += 1;
    }
    match slash {
        None => Ok((b".", p)),
        Some(0) => Ok((b"/", &p[1..])),
        Some(s) => Ok((&p[..s], &p[s + 1..])),
    }
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
mod tests;
