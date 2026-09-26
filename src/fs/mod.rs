//! VFS: inodes, dentries, mounts, path walk. ROADMAP §8.1 / §8.4.
//!
//! Bounded caches with clock eviction. Path walk is iterative with a
//! symlink-depth cap (loop → [`FsError::Loop`], not stack smash).
//! [`RamFs`], whose store sits behind its own [`Guarded`] lock, is enough
//! to unit-test walks. Pseudo filesystems share [`kernfs`] (one dir tree,
//! four skins).
//!
//! Dispatch: a superblock's `ops` pointer ([`InodeOps`]) is the only way
//! to a backend, and no backend receives `&Vfs`: an op gets an [`OpCx`]
//! and the [`Inode`]s it acts on. An inode carries its backend's
//! identity (`key`) and two private words; `Vfs` owns inodes, dentries,
//! mounts and files. A dentry counts its holders, so a directory stays
//! in the cache while a child names it.
//!
//! Interim lock rule, until ROADMAP §10.4's box makes the VFS lock a
//! `BlockingMutex`: `Vfs` calls [`InodeOps`] with its IRQ-off spinlock
//! held, so an op never waits (DESIGN §2.1, §2.9 rule 4). A disk backend
//! takes its volume with one compare-and-swap and returns `Busy` when the
//! volume is busy or on a block device; its File API path takes the
//! volume first and this lock second, never the reverse.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsType {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathRef {
    pub mount: u8,
    pub dslot: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct VfsStats {
    pub d_evicts: u32,
    pub i_evicts: u32,
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
/// its superblock's private words and the clock. Never the [`Vfs`].
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
/// `ino` or `dir` is a [`Vfs`] table slot the caller holds a count on for
/// the call. Until ROADMAP §10.4's VFS-lock box, [`Vfs`] calls these with
/// its IRQ-off spinlock held, so an op never waits (DESIGN §2.1, §2.9
/// rule 4).
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
    /// Release the storage of an inode with no links and no references.
    /// An `Err` leaves the inode unhashed for a later retry.
    fn evict(&self, _cx: &mut OpCx<'_>, _ino: &Inode) -> Result<(), FsError> {
        Ok(())
    }
    /// Drop the backend state of an unmounted superblock.
    fn kill_sb(&self, _cx: &mut OpCx<'_>) {}
}

/// Mount-time half of a filesystem: its ops pointer, and the root inode
/// `fill_super` reports after setting up the superblock's private words.
pub trait FileSystem: Sync {
    fn name(&self) -> &'static str;
    fn fstype(&self) -> FsType;
    fn ops(&'static self) -> Option<&'static dyn InodeOps>;
    fn fill_super(&self, cx: &mut OpCx<'_>) -> Result<InodeInfo, FsError>;
}

/// A counted reference to a [`Vfs`] inode, from [`Vfs::iget_key`] or
/// [`Vfs::iref`]. Hand it back to [`Vfs::put_ref`]: dropping it leaks the
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

/// What [`Vfs::put_ref`] hands back when it drops the last reference to
/// an inode with no links: the backend's words, for the caller to release
/// its storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Evicted {
    pub sb: u8,
    pub key: Key,
    pub private: [u64; 2],
    pub size: u64,
}

/// An in-core inode. `key` is the backend's identity for it and
/// `private` two words only its backend reads or writes; the rest is the
/// metadata `stat` reports. `gen` changes each time the slot is filled,
/// so an [`InodeHandle`] to an earlier occupant is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inode {
    used: bool,
    clock: bool,
    refs: u16,
    sb: u8,
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
        refs: 0,
        sb: 0,
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
}

/// A dentry cache slot. `refs` counts its holders (DESIGN §2.11 rule 2):
/// each child dentry, positive or negative, each mount whose `mp_dslot`
/// it is, the superblock for its root dentry, and explicit holds. Only a
/// `refs == 0` dentry is evicted, so a slot a child names as `parent` is
/// never reused. A superblock's root dentry is its own `parent`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Dentry {
    used: bool,
    clock: bool,
    negative: bool,
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

/// A mounted filesystem instance. `ops` is the only dispatch to its
/// backend; `private` holds two words only the backend reads or writes.
#[derive(Clone, Copy)]
struct Super {
    used: bool,
    fstype: FsType,
    ops: Option<&'static dyn InodeOps>,
    private: [u64; 2],
    root_islot: u16,
    root_dslot: u16,
}

impl Super {
    const EMPTY: Self = Self {
        used: false,
        fstype: FsType::Ram,
        ops: None,
        private: [0; 2],
        root_islot: 0,
        root_dslot: 0,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mount {
    used: bool,
    parent: Option<u8>,
    mp_dslot: u16,
    root_dslot: u16,
    sb: u8,
}

impl Mount {
    const EMPTY: Self = Self {
        used: false,
        parent: None,
        mp_dslot: 0,
        root_dslot: 0,
        sb: 0,
    };
}

#[derive(Clone, Copy)]
struct File {
    used: bool,
    refs: u16,
    islot: u16,
    mount: u8,
    flags: u32,
    offset: u64,
}

impl File {
    const EMPTY: Self = Self {
        used: false,
        refs: 0,
        islot: 0,
        mount: 0,
        flags: 0,
        offset: 0,
    };
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

    pub fn mount_root_fs(&mut self, fs: &'static dyn FileSystem) -> Result<PathRef, FsError> {
        if self.mounts[0].used {
            return Err(FsError::Busy);
        }
        let m = self.alloc_mount()?;
        debug_assert_eq!(m, 0);
        let (sb, dslot) = self.new_super(fs)?;
        self.mounts[0] = Mount {
            used: true,
            parent: None,
            mp_dslot: dslot,
            root_dslot: dslot,
            sb,
        };
        Ok(PathRef { mount: 0, dslot })
    }

    /// Mount `fs` on an existing directory. `..` from the new root
    /// walks to the parent of the covered dentry. The mountpoint is held
    /// before anything is allocated, so no allocation can evict it.
    pub fn mount(
        &mut self,
        cwd: Option<PathRef>,
        at: &str,
        fs: &'static dyn FileSystem,
    ) -> Result<u8, FsError> {
        let dir = self.resolve(cwd, at, true)?;
        let islot = self.d_islot(dir.dslot)?;
        if self.inodes[islot as usize].kind != InodeKind::Dir {
            return Err(FsError::NotDir);
        }
        if self.child_mount(dir.mount, dir.dslot).is_some() {
            return Err(FsError::Busy);
        }
        self.dget(dir.dslot)?;
        let r = self.mount_on(dir, fs);
        if r.is_err() {
            self.dput(dir.dslot);
        }
        r
    }

    fn mount_on(&mut self, dir: PathRef, fs: &'static dyn FileSystem) -> Result<u8, FsError> {
        let m = self.alloc_mount()?;
        let (sb, r_dslot) = self.new_super(fs)?;
        self.mounts[m as usize] = Mount {
            used: true,
            parent: Some(dir.mount),
            mp_dslot: dir.dslot,
            root_dslot: r_dslot,
            sb,
        };
        Ok(m)
    }

    /// Unmount the filesystem whose root `at` names. Every busy check
    /// runs before the first write, so a `Busy` leaves the dentries,
    /// inodes and mounts as they were.
    pub fn umount(&mut self, cwd: Option<PathRef>, at: &str) -> Result<(), FsError> {
        let p = self.resolve(cwd, at, true)?;
        let m = p.mount;
        if m == 0 {
            return Err(FsError::Busy);
        }
        if self.mounts[m as usize].root_dslot != p.dslot {
            return Err(FsError::Inval);
        }
        if self.mounts.iter().any(|x| x.used && x.parent == Some(m)) {
            return Err(FsError::Busy);
        }
        let sb = self.mounts[m as usize].sb;
        if self
            .files
            .iter()
            .any(|f| f.used && self.mounts[f.mount as usize].sb == sb)
        {
            return Err(FsError::Busy);
        }
        let mut k = 0usize;
        while k < MAX_MOUNTS {
            if k != m as usize && self.mounts[k].used && self.mounts[k].sb == sb {
                return Err(FsError::Busy);
            }
            k += 1;
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
            if ino.used && ino.sb == sb && ino.refs > self.naming_dentries(n as u16) {
                return Err(FsError::Busy);
            }
            n += 1;
        }
        // Unlinked inodes whose release failed at their last put: the
        // backend must take them back before the superblock goes.
        let mut n = 0usize;
        while n < MAX_INODES {
            let ino = &self.inodes[n];
            if ino.used && ino.sb == sb && ino.refs == 0 && ino.nlink == 0 {
                if self.ops_evict(sb, n as u16).is_err() {
                    return Err(FsError::Busy);
                }
                self.inode_clear(n);
            }
            n += 1;
        }
        let mp = self.mounts[m as usize].mp_dslot;
        self.dput(mp);
        self.sb_teardown(sb);
        self.mounts[m as usize] = Mount::EMPTY;
        Ok(())
    }

    pub fn resolve(
        &mut self,
        cwd: Option<PathRef>,
        path: &str,
        follow_last: bool,
    ) -> Result<PathRef, FsError> {
        self.walk(cwd, path.as_bytes(), follow_last)
    }

    pub fn islot(&self, p: PathRef) -> Result<u16, FsError> {
        self.d_islot(p.dslot)
    }

    pub fn stat(&mut self, cwd: Option<PathRef>, path: &str) -> Result<Stat, FsError> {
        let p = self.resolve(cwd, path, true)?;
        self.stat_at(p)
    }

    pub fn lstat(&mut self, cwd: Option<PathRef>, path: &str) -> Result<Stat, FsError> {
        let p = self.resolve(cwd, path, false)?;
        self.stat_at(p)
    }

    fn stat_at(&mut self, p: PathRef) -> Result<Stat, FsError> {
        let islot = self.d_islot(p.dslot)?;
        self.ops_getattr(self.sb_of(p.mount), islot)?;
        Ok(self.inodes[islot as usize].stat())
    }

    pub fn mkdir(
        &mut self,
        cwd: Option<PathRef>,
        path: &str,
        mode: u16,
    ) -> Result<PathRef, FsError> {
        self.create_node(cwd, path, InodeKind::Dir, mode | S_IFDIR, None)
    }

    pub fn creat(
        &mut self,
        cwd: Option<PathRef>,
        path: &str,
        mode: u16,
    ) -> Result<PathRef, FsError> {
        self.create_node(cwd, path, InodeKind::Reg, mode | S_IFREG, None)
    }

    pub fn symlink(
        &mut self,
        cwd: Option<PathRef>,
        path: &str,
        target: &str,
    ) -> Result<PathRef, FsError> {
        if target.is_empty() || target.len() > MAX_FILE_BYTES {
            return Err(FsError::Inval);
        }
        self.create_node(
            cwd,
            path,
            InodeKind::Lnk,
            S_IFLNK_MODE,
            Some(target.as_bytes()),
        )
    }

    pub fn unlink(&mut self, cwd: Option<PathRef>, path: &str) -> Result<(), FsError> {
        let (parent, name) = split_basename(path.as_bytes())?;
        if name_is_dot(name) || name_is_dotdot(name) {
            return Err(FsError::Inval);
        }
        let dir = self.walk(cwd, parent, true)?;
        self.held(dir.dslot, |v| v.unlink_in(dir, name))
    }

    fn unlink_in(&mut self, dir: PathRef, name: &[u8]) -> Result<(), FsError> {
        let islot = self.d_islot(dir.dslot)?;
        if self.inodes[islot as usize].kind != InodeKind::Dir {
            return Err(FsError::NotDir);
        }
        if self.is_mountpoint(dir, name) {
            return Err(FsError::Busy);
        }
        let ds = self.lookup_step(dir.mount, dir.dslot, name)?;
        let victim = self.d_islot(ds)?;
        self.remove_name(dir, islot, name, victim, false)
    }

    /// Unlink `name`, whose inode is `victim`, from `dir` (inode `islot`)
    /// through the backend. The victim is held across the call, so its
    /// last put, here or at a later close, is what releases its storage;
    /// no backend scans the inode table.
    fn remove_name(
        &mut self,
        dir: PathRef,
        islot: u16,
        name: &[u8],
        victim: u16,
        rmdir: bool,
    ) -> Result<(), FsError> {
        let sb = self.sb_of(dir.mount);
        self.ihold(victim)?;
        self.dcache_drop_name(sb, dir.dslot, name);
        let r = if rmdir {
            self.with_op(sb, islot, |o, cx, d| o.rmdir(cx, d, name))
        } else {
            self.ops_unlink(sb, islot, name)
        };
        if let Err(e) = r {
            self.iput(victim);
            return Err(e);
        }
        let v = &mut self.inodes[victim as usize];
        v.nlink = if v.kind == InodeKind::Dir {
            0
        } else {
            v.nlink.saturating_sub(1)
        };
        v.ctime = self.now;
        let got = self.ops_getattr(sb, victim);
        self.iput(victim);
        got?;
        self.inodes[islot as usize].mtime = self.now;
        self.inodes[islot as usize].ctime = self.now;
        Ok(())
    }

    pub fn rmdir(&mut self, cwd: Option<PathRef>, path: &str) -> Result<(), FsError> {
        let (parent, name) = split_basename(path.as_bytes())?;
        if name_is_dot(name) || name_is_dotdot(name) {
            return Err(FsError::Inval);
        }
        let dir = self.walk(cwd, parent, true)?;
        self.held(dir.dslot, |v| v.rmdir_in(cwd, path, dir, name))
    }

    fn rmdir_in(
        &mut self,
        cwd: Option<PathRef>,
        path: &str,
        dir: PathRef,
        name: &[u8],
    ) -> Result<(), FsError> {
        if self.is_mountpoint(dir, name) {
            return Err(FsError::Busy);
        }
        let child = self.walk(cwd, path.as_bytes(), false)?;
        let cslot = self.d_islot(child.dslot)?;
        if self.inodes[cslot as usize].kind != InodeKind::Dir {
            return Err(FsError::NotDir);
        }
        let islot = self.d_islot(dir.dslot)?;
        self.remove_name(dir, islot, name, cslot, true)
    }

    /// Hard link, through the superblock's ops; FAT's returns
    /// [`FsError::NotSupp`].
    pub fn link(&mut self, cwd: Option<PathRef>, old: &str, new: &str) -> Result<(), FsError> {
        let src = self.resolve(cwd, old, true)?;
        let sslot = self.d_islot(src.dslot)?;
        if self.inodes[sslot as usize].kind != InodeKind::Reg {
            return Err(FsError::Inval);
        }
        let sb = self.sb_of(src.mount);
        let (parent, name) = split_basename(new.as_bytes())?;
        if name_is_dot(name) || name_is_dotdot(name) {
            return Err(FsError::Inval);
        }
        self.held(src.dslot, |v| {
            let dir = v.walk(cwd, parent, true)?;
            v.held(dir.dslot, |v| {
                if v.sb_of(dir.mount) != sb {
                    return Err(FsError::Inval);
                }
                let dislot = v.d_islot(dir.dslot)?;
                v.with_op2(sb, dislot, sslot, |o, cx, d, t| o.link(cx, d, name, t))?;
                v.dcache_drop_neg_in_dir(sb, dir.dslot);
                v.dcache_drop_name(sb, dir.dslot, name);
                Ok(())
            })
        })
    }

    pub fn rename(&mut self, cwd: Option<PathRef>, old: &str, new: &str) -> Result<(), FsError> {
        let (op, oname) = split_basename(old.as_bytes())?;
        let (np, nname) = split_basename(new.as_bytes())?;
        if name_is_dot(oname)
            || name_is_dotdot(oname)
            || name_is_dot(nname)
            || name_is_dotdot(nname)
        {
            return Err(FsError::Inval);
        }
        let od = self.walk(cwd, op, true)?;
        self.held(od.dslot, |v| {
            let nd = v.walk(cwd, np, true)?;
            v.held(nd.dslot, |v| v.rename_in(od, oname, nd, nname))
        })
    }

    fn rename_in(
        &mut self,
        od: PathRef,
        oname: &[u8],
        nd: PathRef,
        nname: &[u8],
    ) -> Result<(), FsError> {
        let osb = self.sb_of(od.mount);
        let nsb = self.sb_of(nd.mount);
        if osb != nsb {
            return Err(FsError::Inval);
        }
        if self.is_mountpoint(od, oname) || self.is_mountpoint(nd, nname) {
            return Err(FsError::Busy);
        }
        let src = self.lookup_step(od.mount, od.dslot, oname)?;
        let from = self.inodes[self.d_islot(src)? as usize].key;
        let oslot = self.d_islot(od.dslot)?;
        let nslot = self.d_islot(nd.dslot)?;
        let moved = self.with_op2(osb, oslot, nslot, |o, cx, a, b| {
            o.rename(cx, a, oname, b, nname)
        })?;
        if let Some(to) = moved {
            self.rekey(osb, from, to)?;
        }
        self.dcache_drop_name(osb, od.dslot, oname);
        self.dcache_drop_name(nsb, nd.dslot, nname);
        self.dcache_drop_neg_in_dir(nsb, nd.dslot);
        Ok(())
    }

    pub fn open(
        &mut self,
        cwd: Option<PathRef>,
        path: &str,
        flags: u32,
        mode: u16,
    ) -> Result<u16, FsError> {
        let follow = flags & O_NOFOLLOW == 0;
        if flags & O_CREAT != 0 {
            match self.resolve(cwd, path, follow) {
                Ok(_) => {
                    if flags & O_EXCL != 0 {
                        return Err(FsError::Exists);
                    }
                }
                Err(FsError::NotFound) => {
                    self.create_node(cwd, path, InodeKind::Reg, mode | S_IFREG, None)?;
                }
                Err(e) => return Err(e),
            }
        }
        let p = self.resolve(cwd, path, follow)?;
        let islot = self.d_islot(p.dslot)?;
        let kind = self.inodes[islot as usize].kind;
        match kind {
            InodeKind::Dir => {
                let acc = flags & O_ACCMODE;
                if acc == O_WRONLY || acc == O_RDWR || flags & O_TRUNC != 0 {
                    return Err(FsError::IsDir);
                }
            }
            InodeKind::Reg | InodeKind::Chr | InodeKind::Blk => {
                if flags & O_DIRECTORY != 0 {
                    return Err(FsError::NotDir);
                }
            }
            InodeKind::Lnk => {
                if follow {
                    return Err(FsError::Loop);
                }
                if flags & O_DIRECTORY != 0 {
                    return Err(FsError::NotDir);
                }
            }
        }
        if flags & O_TRUNC != 0 && kind == InodeKind::Reg {
            let sb = self.sb_of(p.mount);
            self.ops_truncate(sb, islot, 0)?;
            self.ops_getattr(sb, islot)?;
        }
        self.file_alloc(islot, p.mount, flags)
    }

    pub fn close(&mut self, fid: u16) -> Result<(), FsError> {
        let i = fid as usize;
        if i >= MAX_FILES || !self.files[i].used {
            return Err(FsError::Badf);
        }
        if self.files[i].refs == 0 {
            return Err(FsError::Badf);
        }
        self.files[i].refs -= 1;
        if self.files[i].refs == 0 {
            let islot = self.files[i].islot;
            self.files[i] = File::EMPTY;
            self.iput(islot);
        }
        Ok(())
    }

    pub fn fd_open(
        &mut self,
        tab: &mut FdTable,
        cwd: Option<PathRef>,
        path: &str,
        flags: u32,
        mode: u16,
    ) -> Result<u32, FsError> {
        let fid = self.open(cwd, path, flags, mode)?;
        match tab.install(fid) {
            Ok(fd) => Ok(fd),
            Err(e) => {
                let _ = self.close(fid);
                Err(e)
            }
        }
    }

    pub fn fd_close(&mut self, tab: &mut FdTable, fd: u32) -> Result<(), FsError> {
        let fid = tab.take(fd)?;
        self.close(fid)
    }

    pub fn fd_dup(&mut self, tab: &mut FdTable, fd: u32) -> Result<u32, FsError> {
        let fid = tab.get(fd)?;
        let i = fid as usize;
        if i >= MAX_FILES || !self.files[i].used {
            return Err(FsError::Badf);
        }
        self.files[i].refs += 1;
        match tab.install(fid) {
            Ok(n) => Ok(n),
            Err(e) => {
                self.files[i].refs -= 1;
                Err(e)
            }
        }
    }

    pub fn read(&mut self, fid: u16, buf: &mut [u8]) -> Result<usize, FsError> {
        let (islot, mount, flags, off) = self.file_meta(fid)?;
        if flags & O_ACCMODE == O_WRONLY {
            return Err(FsError::Inval);
        }
        let sb = self.sb_of(mount);
        let n = self.ops_read(sb, islot, off, buf)?;
        self.files[fid as usize].offset = off.saturating_add(n as u64);
        self.inodes[islot as usize].atime = self.now;
        Ok(n)
    }

    pub fn write(&mut self, fid: u16, buf: &[u8]) -> Result<usize, FsError> {
        let (islot, mount, flags, mut off) = self.file_meta(fid)?;
        if flags & O_ACCMODE == O_RDONLY {
            return Err(FsError::Inval);
        }
        if self.inodes[islot as usize].kind == InodeKind::Dir {
            return Err(FsError::IsDir);
        }
        if flags & O_APPEND != 0 {
            off = self.inodes[islot as usize].size;
        }
        let sb = self.sb_of(mount);
        let n = self.ops_write(sb, islot, off, buf)?;
        self.ops_getattr(sb, islot)?;
        self.files[fid as usize].offset = off.saturating_add(n as u64);
        self.inodes[islot as usize].mtime = self.now;
        self.inodes[islot as usize].ctime = self.now;
        Ok(n)
    }

    pub fn seek(&mut self, fid: u16, off: i64, whence: u32) -> Result<u64, FsError> {
        let (islot, _, _, cur) = self.file_meta(fid)?;
        let size = self.inodes[islot as usize].size as i64;
        let base = match whence {
            SEEK_SET => 0i64,
            SEEK_CUR => cur as i64,
            SEEK_END => size,
            _ => return Err(FsError::Inval),
        };
        let n = base.saturating_add(off);
        if n < 0 {
            return Err(FsError::Inval);
        }
        self.files[fid as usize].offset = n as u64;
        Ok(n as u64)
    }

    pub fn truncate(&mut self, cwd: Option<PathRef>, path: &str, size: u64) -> Result<(), FsError> {
        let p = self.resolve(cwd, path, true)?;
        let islot = self.d_islot(p.dslot)?;
        match self.inodes[islot as usize].kind {
            InodeKind::Reg => {}
            InodeKind::Dir => return Err(FsError::IsDir),
            InodeKind::Lnk | InodeKind::Chr | InodeKind::Blk => return Err(FsError::Inval),
        }
        let sb = self.sb_of(p.mount);
        self.ops_truncate(sb, islot, size)?;
        self.ops_getattr(sb, islot)?;
        self.inodes[islot as usize].mtime = self.now;
        self.inodes[islot as usize].ctime = self.now;
        Ok(())
    }

    pub fn readdir(
        &mut self,
        dir: PathRef,
        cookie: u64,
        out: &mut Dirent,
    ) -> Result<Option<u64>, FsError> {
        let islot = self.d_islot(dir.dslot)?;
        if self.inodes[islot as usize].kind != InodeKind::Dir {
            return Err(FsError::NotDir);
        }
        match cookie {
            0 => {
                out.ino = self.inodes[islot as usize].ino;
                out.kind = InodeKind::Dir;
                out.name = Name::from_bytes(b".")?;
                Ok(Some(1))
            }
            1 => {
                let mut m = dir.mount;
                let mut d = dir.dslot;
                self.dotdot(&mut m, &mut d);
                let pslot = self.d_islot(d)?;
                out.ino = self.inodes[pslot as usize].ino;
                out.kind = InodeKind::Dir;
                out.name = Name::from_bytes(b"..")?;
                Ok(Some(2))
            }
            n => {
                let sb = self.sb_of(dir.mount);
                match self.ops_readdir(sb, islot, n - 2, out)? {
                    Some(next) => Ok(Some(next + 2)),
                    None => Ok(None),
                }
            }
        }
    }

    pub fn file_offset(&self, fid: u16) -> Result<u64, FsError> {
        let i = fid as usize;
        if i >= MAX_FILES || !self.files[i].used {
            return Err(FsError::Badf);
        }
        Ok(self.files[i].offset)
    }

    pub fn fstype_at(&self, p: PathRef) -> Result<FsType, FsError> {
        if (p.mount as usize) >= MAX_MOUNTS || !self.mounts[p.mount as usize].used {
            return Err(FsError::Io);
        }
        Ok(self.fstype(self.sb_of(p.mount)))
    }

    pub fn sb_of_path(&self, p: PathRef) -> Result<u8, FsError> {
        if (p.mount as usize) >= MAX_MOUNTS || !self.mounts[p.mount as usize].used {
            return Err(FsError::Io);
        }
        Ok(self.sb_of(p.mount))
    }

    pub fn drop_name(&mut self, parent: PathRef, name: &[u8]) {
        let sb = self.sb_of(parent.mount);
        self.dcache_drop_name(sb, parent.dslot, name);
    }

    /// A counted reference to the inode `info` describes: the cached one
    /// when `(sb, info.key)` is hashed, else a slot filled from `info`.
    pub fn iget_key(&mut self, sb: u8, info: &InodeInfo) -> Result<InodeRef, FsError> {
        self.sb_live(sb)?;
        let slot = self.iget_info(sb, info)?;
        Ok(InodeRef {
            slot,
            r#gen: self.inodes[slot as usize].r#gen,
        })
    }

    /// A counted reference to the inode `p` names.
    pub fn iref(&mut self, p: PathRef) -> Result<InodeRef, FsError> {
        let slot = self.d_islot(p.dslot)?;
        self.ihold(slot)?;
        Ok(InodeRef {
            slot,
            r#gen: self.inodes[slot as usize].r#gen,
        })
    }

    /// Drop a reference. The last one on an inode with no links clears
    /// its slot and returns the backend's words; the backend is not
    /// called, so the caller releases its storage.
    pub fn put_ref(&mut self, r: InodeRef) -> Option<Evicted> {
        let i = self.slot_of(r.handle()).ok()?;
        let n = &mut self.inodes[i];
        debug_assert!(n.refs != 0, "put_ref of an unreferenced inode");
        n.refs = n.refs.saturating_sub(1);
        if n.refs != 0 || n.nlink != 0 {
            return None;
        }
        let out = Evicted {
            sb: n.sb,
            key: n.key,
            private: n.private,
            size: n.size,
        };
        self.inode_clear(i);
        Some(out)
    }

    /// The inode `h` names; `Badf` when its slot was refilled.
    pub fn inode(&self, h: InodeHandle) -> Result<&Inode, FsError> {
        let i = self.slot_of(h)?;
        Ok(&self.inodes[i])
    }

    pub fn inode_mut(&mut self, h: InodeHandle) -> Result<&mut Inode, FsError> {
        let i = self.slot_of(h)?;
        Ok(&mut self.inodes[i])
    }

    /// Move the hashed inode keyed `from` to `to`, as a rename made
    /// outside `Vfs` does, and drop the dentries that name it. A cached
    /// inode already at `to` leaves the hash.
    pub fn rekey(&mut self, sb: u8, from: Key, to: Key) -> Result<(), FsError> {
        self.sb_live(sb)?;
        if from == to {
            return Ok(());
        }
        if let Some(t) = self.hashed(sb, to) {
            self.unhash(t);
        }
        if let Some(f) = self.hashed(sb, from) {
            self.drop_dentries_of(f);
            self.inodes[f as usize].key = to;
        }
        Ok(())
    }

    /// An unlink made outside `Vfs`: the inode keyed `key` gets no links
    /// and leaves the hash, and the dentries naming it are dropped. True
    /// when it is still referenced; its last put then reports it.
    pub fn forget(&mut self, sb: u8, key: Key) -> bool {
        match self.hashed(sb, key) {
            Some(i) => self.unhash(i),
            None => false,
        }
    }

    /// Cache `name` in `parent` as the inode `info` describes, and return
    /// its dentry. A name already cached positive returns that dentry.
    pub fn attach(
        &mut self,
        parent: PathRef,
        name: &[u8],
        info: &InodeInfo,
    ) -> Result<PathRef, FsError> {
        let sb = self.sb_of_path(parent)?;
        if let Some(ds) = self.dcache_find(sb, parent.dslot, name) {
            if !self.dentries[ds as usize].negative {
                return Ok(PathRef {
                    mount: parent.mount,
                    dslot: ds,
                });
            }
            self.dentry_evict(ds);
        }
        let islot = self.iget_info(sb, info)?;
        match self.dcache_insert(sb, parent.dslot, name, Some(islot)) {
            Ok(ds) => Ok(PathRef {
                mount: parent.mount,
                dslot: ds,
            }),
            Err(e) => {
                self.iput(islot);
                Err(e)
            }
        }
    }

    /// Drop every negative dentry of `sb`, as a create made outside
    /// `Vfs` requires.
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

    pub fn sb_of_mount(&self, m: u8) -> Result<u8, FsError> {
        match self.mounts.get(m as usize) {
            Some(x) if x.used => Ok(x.sb),
            _ => Err(FsError::Io),
        }
    }

    /// The private words of the superblock `p` is on.
    pub fn sb_private(&self, p: PathRef) -> Result<[u64; 2], FsError> {
        let sb = self.sb_of_path(p)?;
        Ok(self.supers[sb as usize].private)
    }
}

impl Default for Vfs {
    fn default() -> Self {
        Self::new()
    }
}

impl Vfs {
    fn sb_of(&self, mount: u8) -> u8 {
        self.mounts[mount as usize].sb
    }

    fn fstype(&self, sb: u8) -> FsType {
        self.supers[sb as usize].fstype
    }

    fn d_islot(&self, dslot: u16) -> Result<u16, FsError> {
        let d = &self.dentries[dslot as usize];
        if !d.used || d.negative {
            return Err(FsError::NotFound);
        }
        Ok(d.islot)
    }

    fn file_meta(&self, fid: u16) -> Result<(u16, u8, u32, u64), FsError> {
        let i = fid as usize;
        if i >= MAX_FILES || !self.files[i].used {
            return Err(FsError::Badf);
        }
        let f = &self.files[i];
        Ok((f.islot, f.mount, f.flags, f.offset))
    }

    /// Run `f` on superblock `sb`'s ops with an [`OpCx`] built from
    /// fields disjoint from the inode table, and `inode(islot)`.
    fn with_op<R>(
        &mut self,
        sb: u8,
        islot: u16,
        f: impl FnOnce(&dyn InodeOps, &mut OpCx<'_>, &mut Inode) -> Result<R, FsError>,
    ) -> Result<R, FsError> {
        let ops = self.supers[sb as usize].ops.ok_or(FsError::NotSupp)?;
        let now = self.now;
        let Vfs { supers, inodes, .. } = self;
        let s = &mut supers[sb as usize];
        let mut cx = OpCx {
            sb,
            fstype: s.fstype,
            private: &mut s.private,
            now,
        };
        f(ops, &mut cx, &mut inodes[islot as usize])
    }

    /// [`Self::with_op`] on two inodes, which may be one: each is handed
    /// to `f` as a copy and written back after it, `a` first.
    fn with_op2<R>(
        &mut self,
        sb: u8,
        a: u16,
        b: u16,
        f: impl FnOnce(&dyn InodeOps, &mut OpCx<'_>, &mut Inode, &mut Inode) -> Result<R, FsError>,
    ) -> Result<R, FsError> {
        let mut ib = self.inodes[b as usize];
        let r = self.with_op(sb, a, |o, cx, ia| f(o, cx, ia, &mut ib));
        self.inodes[b as usize] = ib;
        r
    }

    fn ops_lookup(&mut self, sb: u8, dir: u16, name: &[u8]) -> Result<InodeInfo, FsError> {
        self.with_op(sb, dir, |o, cx, d| o.lookup(cx, d, name))
    }

    fn ops_create(
        &mut self,
        sb: u8,
        dir: u16,
        name: &[u8],
        kind: InodeKind,
        mode: u16,
        target: Option<&[u8]>,
    ) -> Result<InodeInfo, FsError> {
        self.with_op(sb, dir, |o, cx, d| {
            o.create(cx, d, name, kind, mode, target)
        })
    }

    fn ops_unlink(&mut self, sb: u8, dir: u16, name: &[u8]) -> Result<(), FsError> {
        self.with_op(sb, dir, |o, cx, d| o.unlink(cx, d, name))
    }

    fn ops_read(&mut self, sb: u8, islot: u16, off: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        self.with_op(sb, islot, |o, cx, n| o.read(cx, n, off, buf))
    }

    fn ops_write(&mut self, sb: u8, islot: u16, off: u64, buf: &[u8]) -> Result<usize, FsError> {
        self.with_op(sb, islot, |o, cx, n| o.write(cx, n, off, buf))
    }

    fn ops_truncate(&mut self, sb: u8, islot: u16, size: u64) -> Result<(), FsError> {
        self.with_op(sb, islot, |o, cx, n| o.truncate(cx, n, size))
    }

    fn ops_readdir(
        &mut self,
        sb: u8,
        islot: u16,
        cookie: u64,
        out: &mut Dirent,
    ) -> Result<Option<u64>, FsError> {
        self.with_op(sb, islot, |o, cx, d| o.readdir(cx, d, cookie, out))
    }

    fn ops_readlink(&mut self, sb: u8, islot: u16, buf: &mut [u8]) -> Result<usize, FsError> {
        self.with_op(sb, islot, |o, cx, n| o.readlink(cx, n, buf))
    }

    /// Refresh inode `islot` from its backend. A superblock with no ops
    /// has nothing to refresh.
    fn ops_getattr(&mut self, sb: u8, islot: u16) -> Result<(), FsError> {
        if self.supers[sb as usize].ops.is_none() {
            return Ok(());
        }
        self.with_op(sb, islot, |o, cx, n| o.getattr(cx, n))
    }

    fn ops_evict(&mut self, sb: u8, islot: u16) -> Result<(), FsError> {
        if self.supers[sb as usize].ops.is_none() {
            return Ok(());
        }
        self.with_op(sb, islot, |o, cx, n| o.evict(cx, n))
    }

    fn ops_kill_sb(&mut self, sb: u8) {
        let Some(ops) = self.supers[sb as usize].ops else {
            return;
        };
        let now = self.now;
        let Vfs { supers, .. } = self;
        let s = &mut supers[sb as usize];
        let mut cx = OpCx {
            sb,
            fstype: s.fstype,
            private: &mut s.private,
            now,
        };
        ops.kill_sb(&mut cx);
    }

    /// Fill a new superblock through `fs`, which sets its private words.
    fn fill_super(&mut self, sb: u8, fs: &dyn FileSystem) -> Result<InodeInfo, FsError> {
        let now = self.now;
        let Vfs { supers, .. } = self;
        let s = &mut supers[sb as usize];
        let mut cx = OpCx {
            sb,
            fstype: s.fstype,
            private: &mut s.private,
            now,
        };
        fs.fill_super(&mut cx)
    }

    fn alloc_super(&mut self) -> Result<u8, FsError> {
        let mut i = 0usize;
        while i < MAX_MOUNTS {
            if !self.supers[i].used {
                self.supers[i] = Super::EMPTY;
                return Ok(i as u8);
            }
            i += 1;
        }
        Err(FsError::NoSpace)
    }

    fn alloc_mount(&mut self) -> Result<u8, FsError> {
        let mut i = 0usize;
        while i < MAX_MOUNTS {
            if !self.mounts[i].used {
                return Ok(i as u8);
            }
            i += 1;
        }
        Err(FsError::NoSpace)
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

    /// A counted reference to the inode `info` describes. A used inode
    /// of `sb` with the same key and links is a hit, and its cached state
    /// wins over `info`: it is the authoritative inode. An unlinked one
    /// (`nlink == 0`) is out of the hash, so a new file that reuses its
    /// key gets a slot of its own.
    fn iget_info(&mut self, sb: u8, info: &InodeInfo) -> Result<u16, FsError> {
        if let Some(i) = self.hashed(sb, info.key) {
            self.ihold(i)?;
            self.inodes[i as usize].clock = true;
            return Ok(i);
        }
        let slot = self.inode_alloc()?;
        let g = self.inodes[slot as usize].r#gen.wrapping_add(1);
        self.inodes[slot as usize] = Inode {
            used: true,
            clock: true,
            refs: 1,
            sb,
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

    /// Empty inode slot `i`, keeping its generation.
    fn inode_clear(&mut self, i: usize) {
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
            Some(s) if s.used => Ok(()),
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
        if n.refs == 0 {
            self.inode_clear(i as usize);
            false
        } else {
            true
        }
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
                }
            }
            d += 1;
        }
    }

    /// Drop a reference. The last one on an inode with no links releases
    /// its storage through the backend; an `Err` leaves it unhashed for
    /// the clock sweep or `umount` to retry.
    fn iput(&mut self, islot: u16) {
        let i = islot as usize;
        if i >= MAX_INODES || !self.inodes[i].used || self.inodes[i].refs == 0 {
            return;
        }
        self.inodes[i].refs -= 1;
        if self.inodes[i].refs == 0 && self.inodes[i].nlink == 0 {
            let sb = self.inodes[i].sb;
            if self.ops_evict(sb, islot).is_ok() {
                self.inode_clear(i);
            }
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
            if self.inodes[s].refs != 0 {
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
            if self.inodes[s].used && self.inodes[s].refs == 0 && self.inode_evict(s as u16) {
                return Ok(s as u16);
            }
            s += 1;
        }
        Err(FsError::NoSpace)
    }

    /// Empty unreferenced inode `slot`. One with no links is released
    /// through its backend first; false when that fails.
    fn inode_evict(&mut self, slot: u16) -> bool {
        let i = slot as usize;
        if !self.inodes[i].used || self.inodes[i].refs != 0 {
            return false;
        }
        if self.inodes[i].nlink == 0 && self.ops_evict(self.inodes[i].sb, slot).is_err() {
            return false;
        }
        self.stats.i_evicts = self.stats.i_evicts.saturating_add(1);
        self.inode_clear(i);
        true
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

    /// Drop one holder of dentry `slot`. It frees nothing: clock eviction
    /// reclaims an unheld dentry later.
    fn dput(&mut self, slot: u16) {
        let d = &mut self.dentries[slot as usize];
        debug_assert!(d.refs != 0, "dput of an unheld dentry");
        d.refs = d.refs.saturating_sub(1);
    }

    /// Run `f` with dentry `slot` held, so nothing `f` allocates evicts it.
    fn held<R>(
        &mut self,
        slot: u16,
        f: impl FnOnce(&mut Self) -> Result<R, FsError>,
    ) -> Result<R, FsError> {
        self.dget(slot)?;
        let r = f(self);
        self.dput(slot);
        r
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

    /// Whether `name` in `dir` is covered by a mount.
    fn is_mountpoint(&self, dir: PathRef, name: &[u8]) -> bool {
        let sb = self.sb_of(dir.mount);
        match self.dcache_peek(sb, dir.dslot, name) {
            Some(ds) => self.mount_pins(ds) != 0,
            None => false,
        }
    }

    /// Allocate and fill a superblock with its root inode and its root
    /// dentry, which the superblock holds once. An error undoes every
    /// step taken, last first.
    fn new_super(&mut self, fs: &'static dyn FileSystem) -> Result<(u8, u16), FsError> {
        let sb = self.alloc_super()?;
        self.supers[sb as usize].fstype = fs.fstype();
        self.supers[sb as usize].ops = fs.ops();
        let info = match self.fill_super(sb, fs) {
            Ok(i) => i,
            Err(e) => {
                self.sb_teardown(sb);
                return Err(e);
            }
        };
        let islot = match self.iget_info(sb, &info) {
            Ok(i) => i,
            Err(e) => {
                self.sb_teardown(sb);
                return Err(e);
            }
        };
        let dslot = match self.dentry_force_alloc() {
            Ok(d) => d,
            Err(e) => {
                self.iput(islot);
                self.sb_teardown(sb);
                return Err(e);
            }
        };
        self.dentries[dslot as usize] = Dentry {
            used: true,
            clock: true,
            negative: false,
            refs: 1,
            parent: dslot,
            sb,
            name: Name::EMPTY,
            islot,
        };
        let s = &mut self.supers[sb as usize];
        s.used = true;
        s.root_islot = islot;
        s.root_dslot = dslot;
        Ok((sb, dslot))
    }

    /// Drop superblock `sb` and all that is cached for it: its dentries
    /// and their inode references, its inodes, and the backend's state.
    fn sb_teardown(&mut self, sb: u8) {
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
        self.ops_kill_sb(sb);
        self.supers[sb as usize] = Super::EMPTY;
    }

    fn dcache_peek(&self, sb: u8, parent: u16, name: &[u8]) -> Option<u16> {
        let mut i = 0usize;
        while i < MAX_DENTRIES {
            let d = &self.dentries[i];
            if d.used
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

    fn lookup_step(&mut self, mount: u8, dir: u16, name: &[u8]) -> Result<u16, FsError> {
        let sb = self.sb_of(mount);
        if let Some(ds) = self.dcache_find(sb, dir, name) {
            if self.dentries[ds as usize].negative {
                return Err(FsError::NotFound);
            }
            return Ok(ds);
        }
        let dir_islot = self.d_islot(dir)?;
        match self.ops_lookup(sb, dir_islot, name) {
            Ok(info) => {
                let islot = self.iget_info(sb, &info)?;
                match self.dcache_insert(sb, dir, name, Some(islot)) {
                    Ok(ds) => Ok(ds),
                    Err(e) => {
                        self.iput(islot);
                        Err(e)
                    }
                }
            }
            Err(FsError::NotFound) => {
                let _ = self.dcache_insert(sb, dir, name, None);
                Err(FsError::NotFound)
            }
            Err(e) => Err(e),
        }
    }

    fn walk(
        &mut self,
        cwd: Option<PathRef>,
        path: &[u8],
        follow_last: bool,
    ) -> Result<PathRef, FsError> {
        if path.is_empty() {
            return Err(FsError::Inval);
        }
        if path.len() > MAX_PATH {
            return Err(FsError::NameTooLong);
        }
        let mut rem = [0u8; MAX_PATH];
        rem[..path.len()].copy_from_slice(path);
        let mut rem_len = path.len();
        let mut jump_root = path[0] == b'/';
        let (mut mount, mut dslot) = match cwd {
            Some(p) if !jump_root => (p.mount, p.dslot),
            _ => {
                let r = self.root()?;
                (r.mount, r.dslot)
            }
        };
        self.follow_mount(&mut mount, &mut dslot);
        let mut depth = 0u32;
        let mut steps = 0u32;
        loop {
            steps += 1;
            if steps > MAX_WALK {
                return Err(FsError::Loop);
            }
            let mut i = 0usize;
            while i < rem_len && rem[i] == b'/' {
                i += 1;
            }
            if jump_root {
                let r = self.root()?;
                mount = r.mount;
                dslot = r.dslot;
                jump_root = false;
            }
            if i == rem_len {
                self.follow_mount(&mut mount, &mut dslot);
                return Ok(PathRef { mount, dslot });
            }
            let mut j = i;
            while j < rem_len && rem[j] != b'/' {
                j += 1;
            }
            let clen = j - i;
            if clen > MAX_NAME {
                return Err(FsError::NameTooLong);
            }
            let mut comp = [0u8; MAX_NAME];
            comp[..clen].copy_from_slice(&rem[i..j]);
            let mut k = j;
            while k < rem_len && rem[k] == b'/' {
                k += 1;
            }
            let last = k == rem_len;
            if clen == 1 && comp[0] == b'.' {
                shift_down(&mut rem, &mut rem_len, j);
                continue;
            }
            if clen == 2 && comp[0] == b'.' && comp[1] == b'.' {
                self.dotdot(&mut mount, &mut dslot);
                shift_down(&mut rem, &mut rem_len, j);
                continue;
            }
            let child = self.lookup_step(mount, dslot, &comp[..clen])?;
            let islot = self.d_islot(child)?;
            let is_lnk = self.inodes[islot as usize].kind == InodeKind::Lnk;
            if is_lnk && (!last || follow_last) {
                if depth >= MAX_SYMLINK {
                    return Err(FsError::Loop);
                }
                depth += 1;
                let sb = self.dentries[child as usize].sb;
                let mut tgt = [0u8; MAX_FILE_BYTES];
                let n = self.ops_readlink(sb, islot, &mut tgt)?;
                let mut restb = [0u8; MAX_PATH];
                let rlen = rem_len - j;
                restb[..rlen].copy_from_slice(&rem[j..rem_len]);
                let mut joined = [0u8; MAX_PATH];
                let jl = join_path(&tgt[..n], &restb[..rlen], &mut joined)?;
                rem[..jl].copy_from_slice(&joined[..jl]);
                rem_len = jl;
                jump_root = n > 0 && tgt[0] == b'/';
                continue;
            }
            dslot = child;
            self.follow_mount(&mut mount, &mut dslot);
            shift_down(&mut rem, &mut rem_len, j);
        }
    }

    fn create_node(
        &mut self,
        cwd: Option<PathRef>,
        path: &str,
        kind: InodeKind,
        mode: u16,
        target: Option<&[u8]>,
    ) -> Result<PathRef, FsError> {
        let (parent, name) = split_basename(path.as_bytes())?;
        if name_is_dot(name) || name_is_dotdot(name) {
            return Err(FsError::Inval);
        }
        let dir = self.walk(cwd, parent, true)?;
        self.held(dir.dslot, |v| v.create_in(dir, name, kind, mode, target))
    }

    fn create_in(
        &mut self,
        dir: PathRef,
        name: &[u8],
        kind: InodeKind,
        mode: u16,
        target: Option<&[u8]>,
    ) -> Result<PathRef, FsError> {
        let dir_islot = self.d_islot(dir.dslot)?;
        if self.inodes[dir_islot as usize].kind != InodeKind::Dir {
            return Err(FsError::NotDir);
        }
        let sb = self.sb_of(dir.mount);
        self.dcache_drop_neg_in_dir(sb, dir.dslot);
        self.dcache_drop_name(sb, dir.dslot, name);
        let info = self.ops_create(sb, dir_islot, name, kind, mode, target)?;
        self.ops_getattr(sb, dir_islot)?;
        let islot = self.iget_info(sb, &info)?;
        let ds = match self.dcache_insert(sb, dir.dslot, name, Some(islot)) {
            Ok(d) => d,
            Err(e) => {
                self.iput(islot);
                return Err(e);
            }
        };
        Ok(PathRef {
            mount: dir.mount,
            dslot: ds,
        })
    }

    fn file_alloc(&mut self, islot: u16, mount: u8, flags: u32) -> Result<u16, FsError> {
        let mut i = 0usize;
        while i < MAX_FILES {
            if !self.files[i].used {
                self.inodes[islot as usize].refs =
                    self.inodes[islot as usize].refs.saturating_add(1);
                self.files[i] = File {
                    used: true,
                    refs: 1,
                    islot,
                    mount,
                    flags,
                    offset: 0,
                };
                return Ok(i as u16);
            }
            i += 1;
        }
        Err(FsError::NoSpace)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A ramfs over a store of its own, which the test leaks.
    pub(super) fn ramfs() -> &'static RamFs<std::sync::Mutex<RamState>> {
        std::boxed::Box::leak(std::boxed::Box::new(RamFs::new(std::sync::Mutex::new(
            RamState::new(),
        ))))
    }

    fn ram() -> Vfs {
        let mut v = Vfs::new();
        v.mount_root_fs(ramfs()).unwrap();
        v
    }

    fn st_ino_of(v: &Vfs, p: PathRef) -> u32 {
        let s = v.islot(p).unwrap();
        v.inodes[s as usize].ino
    }

    #[test]
    fn root_stat_is_dir() {
        let mut v = ram();
        let s = v.stat(None, "/").unwrap();
        assert_eq!(s.kind, InodeKind::Dir);
        assert_eq!(s.nlink, 2);
        assert_eq!(s.ino, 1);
    }

    #[test]
    fn walk_dot_and_dotdot() {
        let mut v = ram();
        v.mkdir(None, "/a", 0o755).unwrap();
        v.mkdir(None, "/a/b", 0o755).unwrap();
        let b = v.resolve(None, "/a/b", true).unwrap();
        let same = v.resolve(None, "/a/b/.", true).unwrap();
        assert_eq!(st_ino_of(&v, b), st_ino_of(&v, same));
        let a = v.resolve(None, "/a/b/..", true).unwrap();
        let a2 = v.resolve(None, "/a", true).unwrap();
        assert_eq!(st_ino_of(&v, a), st_ino_of(&v, a2));
        let root = v.resolve(None, "/a/b/../..", true).unwrap();
        assert_eq!(st_ino_of(&v, root), st_ino_of(&v, v.root().unwrap()));
        let stay = v.resolve(None, "/..", true).unwrap();
        assert_eq!(st_ino_of(&v, stay), st_ino_of(&v, v.root().unwrap()));
        let mixed = v.resolve(None, "/a/./b/../b", true).unwrap();
        assert_eq!(st_ino_of(&v, mixed), st_ino_of(&v, b));
    }

    #[test]
    fn walk_nested_file() {
        let mut v = ram();
        v.mkdir(None, "/a", 0o755).unwrap();
        v.creat(None, "/a/f", 0o644).unwrap();
        let s = v.stat(None, "/a/f").unwrap();
        assert_eq!(s.kind, InodeKind::Reg);
        assert_eq!(s.nlink, 1);
    }

    #[test]
    fn missing_is_not_found() {
        let mut v = ram();
        assert_eq!(v.stat(None, "/nope").unwrap_err(), FsError::NotFound);
        assert_eq!(v.stat(None, "/nope/x").unwrap_err(), FsError::NotFound);
    }

    #[test]
    fn negative_dentry_invalidates_on_create() {
        let mut v = ram();
        assert_eq!(v.stat(None, "/foo").unwrap_err(), FsError::NotFound);
        v.creat(None, "/foo", 0o644).unwrap();
        let s = v.stat(None, "/foo").unwrap();
        assert_eq!(s.kind, InodeKind::Reg);
        assert_eq!(v.stat(None, "/bar").unwrap_err(), FsError::NotFound);
        v.creat(None, "/baz", 0o644).unwrap();
        assert_eq!(v.stat(None, "/bar").unwrap_err(), FsError::NotFound);
        let s = v.stat(None, "/baz").unwrap();
        assert_eq!(s.kind, InodeKind::Reg);
    }

    #[test]
    fn symlink_follow_and_lstat() {
        let mut v = ram();
        v.mkdir(None, "/d", 0o755).unwrap();
        v.creat(None, "/d/f", 0o644).unwrap();
        v.symlink(None, "/l", "/d/f").unwrap();
        let followed = v.stat(None, "/l").unwrap();
        let file = v.stat(None, "/d/f").unwrap();
        assert_eq!(followed.ino, file.ino);
        assert_eq!(followed.kind, InodeKind::Reg);
        let link = v.lstat(None, "/l").unwrap();
        assert_eq!(link.kind, InodeKind::Lnk);
        assert_ne!(link.ino, file.ino);
    }

    #[test]
    fn relative_symlink() {
        let mut v = ram();
        v.mkdir(None, "/d", 0o755).unwrap();
        v.creat(None, "/d/f", 0o644).unwrap();
        v.symlink(None, "/d/l", "f").unwrap();
        let s = v.stat(None, "/d/l").unwrap();
        let f = v.stat(None, "/d/f").unwrap();
        assert_eq!(s.ino, f.ino);
        v.symlink(None, "/d/up", "../d/f").unwrap();
        let s = v.stat(None, "/d/up").unwrap();
        assert_eq!(s.ino, f.ino);
    }

    #[test]
    fn symlink_loop_is_error() {
        let mut v = ram();
        v.symlink(None, "/a", "/b").unwrap();
        v.symlink(None, "/b", "/a").unwrap();
        assert_eq!(v.stat(None, "/a").unwrap_err(), FsError::Loop);
        v.symlink(None, "/self", "/self").unwrap();
        assert_eq!(v.stat(None, "/self").unwrap_err(), FsError::Loop);
    }

    #[test]
    fn symlink_depth_cap() {
        let mut v = ram();
        v.creat(None, "/end", 0o644).unwrap();
        v.symlink(None, "/s8", "/end").unwrap();
        v.symlink(None, "/s7", "/s8").unwrap();
        v.symlink(None, "/s6", "/s7").unwrap();
        v.symlink(None, "/s5", "/s6").unwrap();
        v.symlink(None, "/s4", "/s5").unwrap();
        v.symlink(None, "/s3", "/s4").unwrap();
        v.symlink(None, "/s2", "/s3").unwrap();
        v.symlink(None, "/s1", "/s2").unwrap();
        v.symlink(None, "/s0", "/s1").unwrap();
        // s0..s8 is 9 follows to /end; cap is 8.
        assert_eq!(v.stat(None, "/s0").unwrap_err(), FsError::Loop);
        assert_eq!(v.stat(None, "/s1").unwrap().kind, InodeKind::Reg);
    }

    #[test]
    fn mount_crossing_dotdot() {
        let mut v = ram();
        v.mkdir(None, "/mnt", 0o755).unwrap();
        let mnt_before = v.stat(None, "/mnt").unwrap().ino;
        v.mount(None, "/mnt", ramfs()).unwrap();
        let mnt_after = v.stat(None, "/mnt").unwrap();
        assert_eq!(mnt_after.kind, InodeKind::Dir);
        assert_ne!(mnt_after.ino, mnt_before);
        v.creat(None, "/mnt/x", 0o644).unwrap();
        let x = v.stat(None, "/mnt/x").unwrap();
        assert_eq!(x.kind, InodeKind::Reg);
        let up = v.stat(None, "/mnt/x/..").unwrap();
        assert_eq!(up.ino, mnt_after.ino);
        let root = v.stat(None, "/mnt/x/../..").unwrap();
        assert_eq!(root.ino, v.stat(None, "/").unwrap().ino);
        let root2 = v.stat(None, "/mnt/..").unwrap();
        assert_eq!(root2.ino, root.ino);
        v.umount(None, "/mnt").unwrap();
        assert_eq!(v.stat(None, "/mnt/x").unwrap_err(), FsError::NotFound);
        assert_eq!(v.stat(None, "/mnt").unwrap().ino, mnt_before);
    }

    #[test]
    fn unlinked_open_keeps_data_until_close() {
        let fs = ramfs();
        let mut v = Vfs::new();
        v.mount_root_fs(fs).unwrap();
        let mut tab = FdTable::new();
        let fd = v
            .fd_open(&mut tab, None, "/f", O_RDWR | O_CREAT, 0o644)
            .unwrap();
        let fid = tab.get(fd).unwrap();
        assert_eq!(v.write(fid, b"hello").unwrap(), 5);
        v.unlink(None, "/f").unwrap();
        assert_eq!(v.stat(None, "/f").unwrap_err(), FsError::NotFound);
        v.seek(fid, 0, SEEK_SET).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(v.read(fid, &mut buf).unwrap(), 5);
        assert_eq!(&buf[..5], b"hello");
        let used = fs.with(|st| st.used());
        v.fd_close(&mut tab, fd).unwrap();
        assert!(fs.with(|st| st.used()) < used);
        v.creat(None, "/f", 0o644).unwrap();
        let s = v.stat(None, "/f").unwrap();
        assert_eq!(s.size, 0);
    }

    #[test]
    fn fd_dup_shares_offset() {
        let mut v = ram();
        let mut tab = FdTable::new();
        let fd = v
            .fd_open(&mut tab, None, "/f", O_RDWR | O_CREAT, 0o644)
            .unwrap();
        let fid = tab.get(fd).unwrap();
        v.write(fid, b"abcd").unwrap();
        let fd2 = v.fd_dup(&mut tab, fd).unwrap();
        let fid2 = tab.get(fd2).unwrap();
        assert_eq!(v.file_offset(fid).unwrap(), v.file_offset(fid2).unwrap());
        v.seek(fid2, 0, SEEK_SET).unwrap();
        assert_eq!(v.file_offset(fid).unwrap(), 0);
        v.fd_close(&mut tab, fd).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(v.read(fid2, &mut buf).unwrap(), 4);
        assert_eq!(&buf, b"abcd");
        v.fd_close(&mut tab, fd2).unwrap();
    }

    #[test]
    fn read_write_truncate() {
        let mut v = ram();
        let fid = v.open(None, "/t", O_RDWR | O_CREAT, 0o644).unwrap();
        assert_eq!(v.write(fid, b"xyz").unwrap(), 3);
        v.truncate(None, "/t", 1).unwrap();
        v.seek(fid, 0, SEEK_SET).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(v.read(fid, &mut buf).unwrap(), 1);
        assert_eq!(buf[0], b'x');
        assert_eq!(v.stat(None, "/t").unwrap().size, 1);
        v.close(fid).unwrap();
    }

    #[test]
    fn readdir_dots_and_kids() {
        let mut v = ram();
        v.mkdir(None, "/a", 0o755).unwrap();
        v.creat(None, "/a/f", 0o644).unwrap();
        let dir = v.resolve(None, "/a", true).unwrap();
        let mut d = Dirent {
            ino: 0,
            kind: InodeKind::Reg,
            name: Name::EMPTY,
        };
        let c1 = v.readdir(dir, 0, &mut d).unwrap().unwrap();
        assert!(d.name.is_dot());
        let c2 = v.readdir(dir, c1, &mut d).unwrap().unwrap();
        assert!(d.name.is_dotdot());
        let c3 = v.readdir(dir, c2, &mut d).unwrap().unwrap();
        assert!(d.name.eq_bytes(b"f"));
        assert!(v.readdir(dir, c3, &mut d).unwrap().is_none());
    }

    #[test]
    fn dentry_clock_eviction() {
        let mut v = ram();
        let mut i = 0u32;
        while v.stats.d_evicts == 0 && i < 200 {
            let mut path = [0u8; 5];
            path[0] = b'/';
            path[1] = b'n';
            path[2] = b'0' + ((i / 10) as u8 % 10);
            path[3] = b'0' + ((i % 10) as u8);
            let s = core::str::from_utf8(&path[..4]).unwrap();
            let _ = v.stat(None, s);
            i += 1;
        }
        assert!(v.stats.d_evicts >= 1);
        v.creat(None, "/real", 0o644).unwrap();
        assert_eq!(v.stat(None, "/real").unwrap().kind, InodeKind::Reg);
    }

    #[test]
    fn inode_cache_evicts_idle() {
        let mut v = ram();
        v.mkdir(None, "/d", 0o755).unwrap();
        let mut i = 0u32;
        while i < 20 {
            let mut path = [0u8; 8];
            path[..3].copy_from_slice(b"/d/");
            path[3] = b'f';
            path[4] = b'0' + ((i / 10) as u8);
            path[5] = b'0' + ((i % 10) as u8);
            let s = core::str::from_utf8(&path[..6]).unwrap();
            v.creat(None, s, 0o644).unwrap();
            i += 1;
        }
        i = 0;
        while v.stats.d_evicts == 0 && i < 200 {
            let mut path = [0u8; 8];
            path[..3].copy_from_slice(b"/d/");
            path[3] = b'n';
            path[4] = b'0' + ((i / 10) as u8 % 10);
            path[5] = b'0' + ((i % 10) as u8);
            let s = core::str::from_utf8(&path[..6]).unwrap();
            let _ = v.stat(None, s);
            i += 1;
        }
        v.mkdir(None, "/z", 0o755).unwrap();
        let mut j = 0u32;
        while v.stats.i_evicts == 0 && j < 80 {
            let mut path = [0u8; 8];
            path[..3].copy_from_slice(b"/z/");
            path[3] = b'g';
            path[4] = b'0' + ((j % 10) as u8);
            path[5] = b'0' + (((j / 10) % 10) as u8);
            let s = core::str::from_utf8(&path[..6]).unwrap();
            let _ = v.creat(None, s, 0o644);
            j += 1;
        }
        assert!(v.stats.i_evicts >= 1);
        assert_eq!(v.stat(None, "/d/f00").unwrap().kind, InodeKind::Reg);
    }

    #[test]
    fn cwd_relative_walk() {
        let mut v = ram();
        v.mkdir(None, "/a", 0o755).unwrap();
        v.creat(None, "/a/f", 0o644).unwrap();
        let a = v.resolve(None, "/a", true).unwrap();
        let f = v.resolve(Some(a), "f", true).unwrap();
        assert_eq!(v.stat(Some(a), "f").unwrap().ino, st_ino_of(&v, f));
        let root = v.resolve(Some(a), "..", true).unwrap();
        assert_eq!(st_ino_of(&v, root), st_ino_of(&v, v.root().unwrap()));
    }

    #[test]
    fn ram_rename_and_link() {
        let mut v = ram();
        v.creat(None, "/a", 0o644).unwrap();
        v.mkdir(None, "/d", 0o755).unwrap();
        v.rename(None, "/a", "/d/b").unwrap();
        assert_eq!(v.stat(None, "/a").unwrap_err(), FsError::NotFound);
        assert_eq!(v.stat(None, "/d/b").unwrap().kind, InodeKind::Reg);
        v.link(None, "/d/b", "/c").unwrap();
        assert_eq!(v.stat(None, "/c").unwrap().nlink, 2);
        assert_eq!(v.stat(None, "/d/b").unwrap().nlink, 2);
    }

    #[test]
    fn fixed_tables_match_limits() {
        use crate::limits;
        let v = Vfs::new();
        assert_eq!(v.inodes.len(), limits::MAX_INODES);
        assert_eq!(v.dentries.len(), limits::MAX_DENTRIES);
        assert_eq!(v.supers.len(), limits::MAX_MOUNTS);
        assert_eq!(v.mounts.len(), limits::MAX_MOUNTS);
        assert_eq!(v.files.len(), limits::MAX_OPEN_FILES);
        assert_eq!(FdTable::new().fds.len(), limits::MAX_FDS);
    }

    /// Negative lookups of fresh names under `dir` until the dentry cache
    /// has evicted `n` more dentries.
    fn press(v: &mut Vfs, dir: &str, n: u32, seq: &mut u32) {
        let goal = v.stats.d_evicts.saturating_add(n);
        while v.stats.d_evicts < goal {
            let p = format!("{dir}/n{}", *seq);
            *seq += 1;
            assert_eq!(v.stat(None, &p).unwrap_err(), FsError::NotFound);
        }
    }

    #[test]
    fn dcache_f065_evicted_parent_keeps_mount() {
        let mut v = ram();
        v.mkdir(None, "/a", 0o755).unwrap();
        v.mkdir(None, "/a/m", 0o755).unwrap();
        v.mount(None, "/a/m", ramfs()).unwrap();
        v.creat(None, "/a/m/marker", 0o644).unwrap();
        let marker = v.stat(None, "/a/m/marker").unwrap().ino;
        assert_dcache_sound(&v);
        let mut seq = 0u32;
        while v.stats.d_evicts < 2 * MAX_DENTRIES as u32 {
            press(&mut v, "", 8, &mut seq);
            assert_eq!(v.stat(None, "/a/m/marker").unwrap().ino, marker);
            assert_dcache_sound(&v);
        }
        v.umount(None, "/a/m").unwrap();
        assert_dcache_sound(&v);
        assert_eq!(v.stat(None, "/a/m/marker").unwrap_err(), FsError::NotFound);
    }

    #[test]
    fn dcache_f065_reused_slot_never_aliases() {
        let mut v = ram();
        let mut seq = 0u32;
        let mut round = 0u32;
        while round < 24 {
            let a = format!("/a{round}");
            let ax = format!("/a{round}/x");
            let c = format!("/c{round}");
            let cx = format!("/c{round}/x");
            v.mkdir(None, &a, 0o755).unwrap();
            v.creat(None, &ax, 0o644).unwrap();
            let ino = v.stat(None, &ax).unwrap().ino;
            press(&mut v, "", round % 7 + 1, &mut seq);
            v.mkdir(None, &c, 0o755).unwrap();
            assert_eq!(v.stat(None, &cx).unwrap_err(), FsError::NotFound);
            assert_eq!(v.stat(None, &ax).unwrap().ino, ino);
            assert_dcache_sound(&v);
            v.unlink(None, &ax).unwrap();
            v.rmdir(None, &a).unwrap();
            v.rmdir(None, &c).unwrap();
            assert_dcache_sound(&v);
            round += 1;
        }
    }

    /// Every used dentry is held exactly by its children, the mounts on
    /// it and, for a root, its superblock; a non-root dentry's parent is
    /// used, positive and in the same superblock.
    fn assert_dcache_sound(v: &Vfs) {
        let mut i = 0usize;
        while i < MAX_DENTRIES {
            let d = &v.dentries[i];
            if d.used {
                assert_eq!(d.refs, v.expected_holds(i as u16), "dentry {i}'s holders");
                if !d.is_root(i as u16) {
                    let p = &v.dentries[d.parent as usize];
                    assert!(p.used && !p.negative && p.sb == d.sb, "dentry {i}'s parent");
                }
            }
            i += 1;
        }
    }

    /// Show the superblock the mount on `from` shows again on `at`, by
    /// hand: the shape P10-S12's shared `Super` gives two mounts.
    fn mount_again(v: &mut Vfs, from: &str, at: &str) -> u8 {
        let src = v.resolve(None, from, true).unwrap();
        let dir = v.resolve(None, at, true).unwrap();
        v.dget(dir.dslot).unwrap();
        let m = v.alloc_mount().unwrap();
        v.mounts[m as usize] = Mount {
            used: true,
            parent: Some(dir.mount),
            mp_dslot: dir.dslot,
            root_dslot: src.dslot,
            sb: v.mounts[src.mount as usize].sb,
        };
        m
    }

    #[test]
    fn dcache_f065_two_mounts_one_dentry() {
        let mut v = ram();
        v.mkdir(None, "/p", 0o755).unwrap();
        v.mkdir(None, "/q", 0o755).unwrap();
        v.mount(None, "/p", ramfs()).unwrap();
        mount_again(&mut v, "/p", "/q");
        assert_dcache_sound(&v);
        v.creat(None, "/p/f", 0o644).unwrap();
        let pf = v.resolve(None, "/p/f", true).unwrap();
        let qf = v.resolve(None, "/q/f", true).unwrap();
        assert_ne!(pf.mount, qf.mount);
        assert_eq!(pf.dslot, qf.dslot, "one dentry for the name");
        v.creat(None, "/q/g", 0o644).unwrap();
        let pg = v.resolve(None, "/p/g", true).unwrap();
        let qg = v.resolve(None, "/q/g", true).unwrap();
        assert_eq!(pg.dslot, qg.dslot);
        let named = |v: &Vfs, n: &[u8]| {
            v.dentries
                .iter()
                .filter(|d| d.used && d.name.eq_bytes(n))
                .count()
        };
        assert_eq!(named(&v, b"f"), 1);
        assert_eq!(named(&v, b"g"), 1);
        assert_eq!(
            v.stat(None, "/p/g").unwrap().ino,
            v.stat(None, "/q/g").unwrap().ino
        );
        assert_dcache_sound(&v);
    }

    #[test]
    fn dcache_f065_mount_pins_mountpoint_first() {
        let mut v = ram();
        v.mkdir(None, "/a", 0o755).unwrap();
        v.mkdir(None, "/a/m", 0o755).unwrap();
        let a_ino = v.stat(None, "/a").unwrap().ino;
        let mp = v.resolve(None, "/a/m", true).unwrap().dslot;
        // Fill every slot, clear every clock bit and aim the hand at the
        // mountpoint, so the mount's root dentry must evict and the
        // mountpoint is the first candidate.
        let mut seq = 0u32;
        while v.dentries.iter().any(|d| !d.used) {
            let _ = v.stat(None, &format!("/n{seq}"));
            seq += 1;
        }
        for d in v.dentries.iter_mut() {
            d.clock = false;
        }
        v.dhand = mp;
        let m = v.mount(None, "/a/m", ramfs()).unwrap();
        let mt = v.mounts[m as usize];
        assert_eq!(mt.mp_dslot, mp);
        assert_ne!(mt.mp_dslot, mt.root_dslot);
        assert_eq!(v.stat(None, "/a/m/..").unwrap().ino, a_ino);
        assert_dcache_sound(&v);
        v.umount(None, "/a/m").unwrap();
        assert_dcache_sound(&v);
    }

    #[test]
    fn dcache_f065_umount_checks_before_state() {
        let mut v = ram();
        v.mkdir(None, "/m", 0o755).unwrap();
        v.mount(None, "/m", ramfs()).unwrap();
        v.creat(None, "/m/f", 0o644).unwrap();
        let f = v.resolve(None, "/m/f", true).unwrap();
        let held = v.iref(f).unwrap();
        v.resolve(None, "/m", true).unwrap();
        let before = (v.dentries, v.inodes, v.mounts);
        assert_eq!(v.umount(None, "/m").unwrap_err(), FsError::Busy);
        assert_eq!((v.dentries, v.inodes, v.mounts), before);
        assert_eq!(v.put_ref(held), None);
        assert_eq!(v.stat(None, "/m/f").unwrap().kind, InodeKind::Reg);
        v.creat(None, "/m/g", 0o644).unwrap();
        let g = v.resolve(None, "/m/g", true).unwrap();
        v.dget(g.dslot).unwrap();
        v.resolve(None, "/m", true).unwrap();
        let before = (v.dentries, v.inodes, v.mounts);
        assert_eq!(v.umount(None, "/m").unwrap_err(), FsError::Busy);
        assert_eq!((v.dentries, v.inodes, v.mounts), before);
        assert_eq!(v.stat(None, "/m/g").unwrap().kind, InodeKind::Reg);
        v.dput(g.dslot);
        assert_dcache_sound(&v);
        v.umount(None, "/m").unwrap();
        assert_dcache_sound(&v);
        assert_eq!(v.stat(None, "/m/f").unwrap_err(), FsError::NotFound);
        assert!(v.inodes.iter().all(|i| !i.used || i.sb == 0));
    }

    /// A test backend whose storage lives outside `Vfs`, in `KEYFS`, found
    /// by the store id in its superblock's private word 0. Node `n` has
    /// key `[n, 0, 0]`, `st_ino` `n + 100` and private words `[7 * n, w]`,
    /// where `w` counts the writes made through the inode.
    pub(super) struct KeyFs {
        id: u64,
    }

    struct KeyOps;

    #[derive(Clone)]
    struct KNode {
        kind: InodeKind,
        nlink: u32,
        data: Vec<u8>,
        alive: bool,
    }

    #[derive(Default)]
    struct Store {
        nodes: Vec<KNode>,
        names: Vec<(u32, Vec<u8>, u32)>,
        evicts: u32,
    }

    static KEYFS: std::sync::Mutex<Vec<Store>> = std::sync::Mutex::new(Vec::new());

    pub(super) fn keyfs_new() -> &'static KeyFs {
        let mut g = KEYFS.lock().unwrap();
        g.push(Store::default());
        std::boxed::Box::leak(std::boxed::Box::new(KeyFs {
            id: (g.len() - 1) as u64,
        }))
    }

    fn with_store<R>(id: u64, f: impl FnOnce(&mut Store) -> R) -> R {
        f(&mut KEYFS.lock().unwrap()[id as usize])
    }

    fn knode_info(s: &Store, n: u32) -> InodeInfo {
        let k = &s.nodes[n as usize];
        InodeInfo {
            key: [n, 0, 0],
            ino: n + 100,
            kind: k.kind,
            mode: k.kind.ifmt() | 0o644,
            nlink: k.nlink,
            size: k.data.len() as u64,
            atime: 0,
            mtime: 0,
            ctime: 0,
            private: [7 * u64::from(n), 0],
        }
    }

    impl FileSystem for KeyFs {
        fn name(&self) -> &'static str {
            "keyfs"
        }
        fn fstype(&self) -> FsType {
            FsType::Ram
        }
        fn ops(&'static self) -> Option<&'static dyn InodeOps> {
            Some(&KeyOps)
        }
        fn fill_super(&self, cx: &mut OpCx<'_>) -> Result<InodeInfo, FsError> {
            *cx.private = [self.id, 0];
            with_store(self.id, |s| {
                s.nodes.push(KNode {
                    kind: InodeKind::Dir,
                    nlink: 2,
                    data: Vec::new(),
                    alive: true,
                });
                Ok(knode_info(s, 0))
            })
        }
    }

    impl InodeOps for KeyOps {
        fn lookup(
            &self,
            cx: &mut OpCx<'_>,
            dir: &Inode,
            name: &[u8],
        ) -> Result<InodeInfo, FsError> {
            with_store(cx.private[0], |s| {
                let n = s
                    .names
                    .iter()
                    .find(|e| e.0 == dir.key[0] && e.1 == name)
                    .ok_or(FsError::NotFound)?
                    .2;
                Ok(knode_info(s, n))
            })
        }
        fn create(
            &self,
            cx: &mut OpCx<'_>,
            dir: &mut Inode,
            name: &[u8],
            kind: InodeKind,
            _mode: u16,
            _target: Option<&[u8]>,
        ) -> Result<InodeInfo, FsError> {
            with_store(cx.private[0], |s| {
                if s.names.iter().any(|e| e.0 == dir.key[0] && e.1 == name) {
                    return Err(FsError::Exists);
                }
                s.nodes.push(KNode {
                    kind,
                    nlink: 1,
                    data: Vec::new(),
                    alive: true,
                });
                let n = (s.nodes.len() - 1) as u32;
                s.names.push((dir.key[0], name.to_vec(), n));
                Ok(knode_info(s, n))
            })
        }
        fn unlink(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
            with_store(cx.private[0], |s| {
                let i = s
                    .names
                    .iter()
                    .position(|e| e.0 == dir.key[0] && e.1 == name)
                    .ok_or(FsError::NotFound)?;
                let n = s.names.remove(i).2 as usize;
                s.nodes[n].nlink -= 1;
                Ok(())
            })
        }
        fn read(
            &self,
            cx: &mut OpCx<'_>,
            ino: &mut Inode,
            off: u64,
            buf: &mut [u8],
        ) -> Result<usize, FsError> {
            with_store(cx.private[0], |s| {
                let d = &s.nodes[ino.key[0] as usize].data;
                let off = (off as usize).min(d.len());
                let n = buf.len().min(d.len() - off);
                buf[..n].copy_from_slice(&d[off..off + n]);
                Ok(n)
            })
        }
        fn write(
            &self,
            cx: &mut OpCx<'_>,
            ino: &mut Inode,
            off: u64,
            buf: &[u8],
        ) -> Result<usize, FsError> {
            with_store(cx.private[0], |s| {
                let d = &mut s.nodes[ino.key[0] as usize].data;
                let end = off as usize + buf.len();
                if d.len() < end {
                    d.resize(end, 0);
                }
                d[off as usize..end].copy_from_slice(buf);
                ino.size = ino.size.max(end as u64);
                ino.private[1] += 1;
                Ok(buf.len())
            })
        }
        fn truncate(&self, cx: &mut OpCx<'_>, ino: &mut Inode, size: u64) -> Result<(), FsError> {
            with_store(cx.private[0], |s| {
                s.nodes[ino.key[0] as usize].data.resize(size as usize, 0);
                ino.size = size;
                Ok(())
            })
        }
        fn readdir(
            &self,
            cx: &mut OpCx<'_>,
            dir: &Inode,
            cookie: u64,
            out: &mut Dirent,
        ) -> Result<Option<u64>, FsError> {
            with_store(cx.private[0], |s| {
                let mut kids = s.names.iter().filter(|e| e.0 == dir.key[0]);
                let Some(e) = kids.nth(cookie as usize) else {
                    return Ok(None);
                };
                out.ino = e.2 + 100;
                out.kind = s.nodes[e.2 as usize].kind;
                out.name = Name::from_bytes(&e.1)?;
                Ok(Some(cookie + 1))
            })
        }
        fn getattr(&self, cx: &mut OpCx<'_>, ino: &mut Inode) -> Result<(), FsError> {
            with_store(cx.private[0], |s| {
                if let Some(k) = s.nodes.get(ino.key[0] as usize) {
                    ino.nlink = k.nlink;
                }
                Ok(())
            })
        }
        fn evict(&self, cx: &mut OpCx<'_>, ino: &Inode) -> Result<(), FsError> {
            with_store(cx.private[0], |s| {
                s.nodes[ino.key[0] as usize].alive = false;
                s.evicts += 1;
                Ok(())
            })
        }
    }

    /// A filesystem with no ops: its root and nothing else.
    struct NoOpsFs;

    impl FileSystem for NoOpsFs {
        fn name(&self) -> &'static str {
            "noops"
        }
        fn fstype(&self) -> FsType {
            FsType::Fat
        }
        fn ops(&'static self) -> Option<&'static dyn InodeOps> {
            None
        }
        fn fill_super(&self, cx: &mut OpCx<'_>) -> Result<InodeInfo, FsError> {
            Ok(InodeInfo {
                key: [0, 0, 0],
                ino: 1,
                kind: InodeKind::Dir,
                mode: S_IFDIR_MODE,
                nlink: 2,
                size: 0,
                atime: cx.now,
                mtime: cx.now,
                ctime: cx.now,
                private: [2, 0],
            })
        }
    }

    /// `ram()` with a fresh `KeyFs` on `/k`; its store id.
    fn keyed() -> (Vfs, u64) {
        let mut v = ram();
        v.mkdir(None, "/k", 0o755).unwrap();
        let fs = keyfs_new();
        v.mount(None, "/k", fs).unwrap();
        (v, fs.id)
    }

    #[test]
    fn ops_keyed_backend_via_super_ops() {
        let (mut v, id) = keyed();
        v.creat(None, "/k/a", 0o644).unwrap();
        let na = with_store(id, |s| {
            let n = s.names[0].2;
            s.names.push((0, b"b".to_vec(), n));
            s.nodes[n as usize].nlink += 1;
            n
        });
        let a = v.resolve(None, "/k/a", true).unwrap();
        let b = v.resolve(None, "/k/b", true).unwrap();
        assert_ne!(a.dslot, b.dslot);
        assert_eq!(
            v.islot(a).unwrap(),
            v.islot(b).unwrap(),
            "one inode per key"
        );
        assert_eq!(v.stat(None, "/k/b").unwrap().ino, na + 100);
        let fid = v.open(None, "/k/a", O_RDWR, 0).unwrap();
        assert_eq!(v.write(fid, b"hello").unwrap(), 5);
        assert_eq!(v.write(fid, b"!").unwrap(), 1);
        assert_eq!(v.stat(None, "/k/b").unwrap().size, 6);
        let r = v.iref(b).unwrap();
        let n = *v.inode(r.handle()).unwrap();
        assert_eq!(n.key, [na, 0, 0]);
        assert_eq!(
            n.private,
            [7 * u64::from(na), 2],
            "private words round-trip"
        );
        assert_eq!(v.put_ref(r), None);
        v.seek(fid, 0, SEEK_SET).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(v.read(fid, &mut buf).unwrap(), 6);
        assert_eq!(&buf[..6], b"hello!");
        v.unlink(None, "/k/a").unwrap();
        v.unlink(None, "/k/b").unwrap();
        assert_eq!(v.stat(None, "/k/b").unwrap_err(), FsError::NotFound);
        assert_eq!(
            with_store(id, |s| s.evicts),
            0,
            "an open file keeps its storage"
        );
        v.close(fid).unwrap();
        assert_eq!(
            with_store(id, |s| s.evicts),
            1,
            "evicted once at the last put"
        );
        assert!(!with_store(id, |s| s.nodes[na as usize].alive));
        v.umount(None, "/k").unwrap();
        assert_eq!(with_store(id, |s| s.evicts), 1);
        v.mkdir(None, "/n", 0o755).unwrap();
        v.mount(None, "/n", &NoOpsFs).unwrap();
        assert_eq!(v.stat(None, "/n/x").unwrap_err(), FsError::NotSupp);
        assert_eq!(v.creat(None, "/n/y", 0o644).unwrap_err(), FsError::NotSupp);
        assert_eq!(v.stat(None, "/n").unwrap().kind, InodeKind::Dir);
        assert_dcache_sound(&v);
    }

    #[test]
    fn ops_inode_ref_api() {
        let (mut v, id) = keyed();
        let k = v.resolve(None, "/k", true).unwrap();
        let sb = v.sb_of_mount(k.mount).unwrap();
        assert_eq!(v.sb_private(k).unwrap(), [id, 0]);
        let info = InodeInfo {
            key: [42, 0, 0],
            ino: 4242,
            kind: InodeKind::Reg,
            mode: S_IFREG_MODE,
            nlink: 1,
            size: 9,
            atime: 0,
            mtime: 0,
            ctime: 0,
            private: [5, 6],
        };
        let r1 = v.iget_key(sb, &info).unwrap();
        let r2 = v.iget_key(sb, &InodeInfo { size: 1, ..info }).unwrap();
        let h = r1.handle();
        assert_eq!(h, r2.handle(), "one inode per key");
        assert_eq!(v.inode(h).unwrap().size, 9, "the cached inode wins");
        v.inode_mut(h).unwrap().size = 11;
        let x = v.attach(k, b"x", &info).unwrap();
        assert_eq!(
            v.attach(k, b"x", &info).unwrap(),
            x,
            "a cached name returns its dentry"
        );
        assert_eq!(v.stat(None, "/k/x").unwrap().ino, 4242);
        assert_eq!(v.stat(None, "/k/x").unwrap().size, 11);
        v.rekey(sb, [42, 0, 0], [43, 0, 0]).unwrap();
        assert_eq!(v.inode(h).unwrap().key, [43, 0, 0]);
        assert!(
            v.dcache_peek(sb, k.dslot, b"x").is_none(),
            "rekey drops its dentries"
        );
        assert!(!v.forget(sb, [44, 0, 0]));
        assert!(v.forget(sb, [43, 0, 0]), "still referenced");
        assert_eq!(v.inode(h).unwrap().nlink, 0);
        let other = v
            .iget_key(
                sb,
                &InodeInfo {
                    key: [43, 0, 0],
                    ..info
                },
            )
            .unwrap();
        assert_ne!(other.handle(), h, "a forgotten inode is out of the hash");
        assert_eq!(v.put_ref(other), None);
        assert_eq!(v.put_ref(r2), None);
        assert_eq!(
            v.put_ref(r1),
            Some(Evicted {
                sb,
                key: [43, 0, 0],
                private: [5, 6],
                size: 11,
            })
        );
        assert_eq!(v.inode(h).unwrap_err(), FsError::Badf);
        let again = v.iget_key(sb, &info).unwrap();
        if again.handle() != h {
            assert_eq!(v.inode(h).unwrap_err(), FsError::Badf, "a stale generation");
        }
        assert_eq!(v.put_ref(again), None);
        assert_eq!(
            with_store(id, |s| s.evicts),
            0,
            "put_ref never calls the backend"
        );
        assert_eq!(v.stat(None, "/k/none").unwrap_err(), FsError::NotFound);
        assert!(
            v.dentries
                .iter()
                .any(|d| d.used && d.sb == sb && d.negative)
        );
        v.drop_negatives(sb);
        assert!(
            !v.dentries
                .iter()
                .any(|d| d.used && d.sb == sb && d.negative)
        );
        assert_dcache_sound(&v);
    }
}
