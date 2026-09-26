//! VFS: inodes, dentries, mounts, path walk. ROADMAP §8.1 / §8.4.
//!
//! Bounded caches with clock eviction. Path walk is iterative with a
//! symlink-depth cap (loop → [`FsError::Loop`], not stack smash).
//! Dummy [`RamFs`] is enough to unit-test walks. [`FatFs`] is a VFS
//! stub (on-disk I/O lives in `fat` / `fat_init`). Pseudo filesystems
//! share [`kernfs`] (one dir tree, four skins).
//!
//! Locks (kernel): RANK_DEVICE. Tables are static; do not allocate
//! under the lock. No FS work from hard IRQ (DESIGN §2.2).

mod kernfs;

pub use kernfs::{DevFs, ProcFs, SysFs, TmpFs};

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

/// Mount / root-inode fill. Concrete filesystems implement this.
pub trait FileSystem {
    fn name(&self) -> &'static str;
    fn fstype(&self) -> FsType;
    fn fill_super(&self, vfs: &mut Vfs, sb: u8) -> Result<u32, FsError>;
}

/// Per-inode ops. May block on a real FS; never call from hard IRQ.
pub trait InodeOps {
    fn lookup(&self, vfs: &mut Vfs, dir_islot: u16, name: &[u8]) -> Result<u32, FsError>;
    fn create(
        &self,
        vfs: &mut Vfs,
        dir_islot: u16,
        name: &[u8],
        kind: InodeKind,
        mode: u16,
        target: Option<&[u8]>,
    ) -> Result<u32, FsError>;
    fn unlink(&self, vfs: &mut Vfs, dir_islot: u16, name: &[u8]) -> Result<(), FsError>;
    fn read(&self, vfs: &mut Vfs, islot: u16, off: u64, buf: &mut [u8]) -> Result<usize, FsError>;
    fn write(&self, vfs: &mut Vfs, islot: u16, off: u64, buf: &[u8]) -> Result<usize, FsError>;
    fn truncate(&self, vfs: &mut Vfs, islot: u16, size: u64) -> Result<(), FsError>;
    fn readdir(
        &self,
        vfs: &mut Vfs,
        islot: u16,
        cookie: u64,
        out: &mut Dirent,
    ) -> Result<Option<u64>, FsError>;
    fn stat(&self, vfs: &mut Vfs, islot: u16) -> Result<Stat, FsError>;
}

pub struct RamFs;

impl FileSystem for RamFs {
    fn name(&self) -> &'static str {
        "ramfs"
    }

    fn fstype(&self) -> FsType {
        FsType::Ram
    }

    fn fill_super(&self, vfs: &mut Vfs, sb: u8) -> Result<u32, FsError> {
        ram_fill_super(vfs, sb)
    }
}

impl InodeOps for RamFs {
    fn lookup(&self, vfs: &mut Vfs, dir_islot: u16, name: &[u8]) -> Result<u32, FsError> {
        ram_lookup(vfs, dir_islot, name)
    }

    fn create(
        &self,
        vfs: &mut Vfs,
        dir_islot: u16,
        name: &[u8],
        kind: InodeKind,
        mode: u16,
        target: Option<&[u8]>,
    ) -> Result<u32, FsError> {
        ram_create(vfs, dir_islot, name, kind, mode, target)
    }

    fn unlink(&self, vfs: &mut Vfs, dir_islot: u16, name: &[u8]) -> Result<(), FsError> {
        ram_unlink(vfs, dir_islot, name)
    }

    fn read(&self, vfs: &mut Vfs, islot: u16, off: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        ram_read(vfs, islot, off, buf)
    }

    fn write(&self, vfs: &mut Vfs, islot: u16, off: u64, buf: &[u8]) -> Result<usize, FsError> {
        ram_write(vfs, islot, off, buf)
    }

    fn truncate(&self, vfs: &mut Vfs, islot: u16, size: u64) -> Result<(), FsError> {
        ram_truncate(vfs, islot, size)
    }

    fn readdir(
        &self,
        vfs: &mut Vfs,
        islot: u16,
        cookie: u64,
        out: &mut Dirent,
    ) -> Result<Option<u64>, FsError> {
        ram_readdir(vfs, islot, cookie, out)
    }

    fn stat(&self, vfs: &mut Vfs, islot: u16) -> Result<Stat, FsError> {
        ram_stat(vfs, islot)
    }
}

/// FAT32 super fill. On-disk I/O lives in `fat` / `fat_init`; VFS
/// only stores the root cluster in the inode `data0`.
pub struct FatFs {
    pub root_clu: u32,
    pub vol: u8,
}

impl FileSystem for FatFs {
    fn name(&self) -> &'static str {
        "fat32"
    }

    fn fstype(&self) -> FsType {
        FsType::Fat
    }

    fn fill_super(&self, vfs: &mut Vfs, sb: u8) -> Result<u32, FsError> {
        vfs.supers[sb as usize].fat_clu = self.root_clu;
        vfs.supers[sb as usize].fat_vol = self.vol;
        Ok(1)
    }
}

/// vibefs super fill. On-disk I/O lives in `vibefs` / `vibefs_init`.
pub struct VibeFs {
    pub root_ino: u32,
    pub vol: u8,
}

impl FileSystem for VibeFs {
    fn name(&self) -> &'static str {
        "vibefs"
    }

    fn fstype(&self) -> FsType {
        FsType::Vibe
    }

    fn fill_super(&self, vfs: &mut Vfs, sb: u8) -> Result<u32, FsError> {
        vfs.supers[sb as usize].fat_clu = self.root_ino;
        vfs.supers[sb as usize].fat_vol = self.vol;
        Ok(self.root_ino)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Inode {
    used: bool,
    clock: bool,
    refs: u16,
    sb: u8,
    ino: u32,
    kind: InodeKind,
    mode: u16,
    nlink: u32,
    size: u64,
    atime: u64,
    mtime: u64,
    ctime: u64,
    data0: u32,
}

impl Inode {
    const EMPTY: Self = Self {
        used: false,
        clock: false,
        refs: 0,
        sb: 0,
        ino: 0,
        kind: InodeKind::Reg,
        mode: 0,
        nlink: 0,
        size: 0,
        atime: 0,
        mtime: 0,
        ctime: 0,
        data0: 0,
    };
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Super {
    used: bool,
    fstype: FsType,
    root_ino: u32,
    root_islot: u16,
    root_dslot: u16,
    fat_clu: u32,
    fat_vol: u8,
}

impl Super {
    const EMPTY: Self = Self {
        used: false,
        fstype: FsType::Ram,
        root_ino: 0,
        root_islot: 0,
        root_dslot: 0,
        fat_clu: 0,
        fat_vol: 0,
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

#[derive(Clone, Copy)]
struct RamDent {
    name: Name,
    ino: u32,
}

impl RamDent {
    const EMPTY: Self = Self {
        name: Name::EMPTY,
        ino: 0,
    };
}

#[derive(Clone, Copy)]
struct RamNode {
    used: bool,
    sb: u8,
    kind: InodeKind,
    mode: u16,
    nlink: u32,
    size: u64,
    atime: u64,
    mtime: u64,
    ctime: u64,
    data: [u8; MAX_FILE_BYTES],
    dents: [RamDent; MAX_DIR_ENTS],
    ndent: u8,
}

impl RamNode {
    const EMPTY: Self = Self {
        used: false,
        sb: 0,
        kind: InodeKind::Reg,
        mode: 0,
        nlink: 0,
        size: 0,
        atime: 0,
        mtime: 0,
        ctime: 0,
        data: [0; MAX_FILE_BYTES],
        dents: [RamDent::EMPTY; MAX_DIR_ENTS],
        ndent: 0,
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
    ram: [RamNode; MAX_RAM_NODES],
    kern: kernfs::KernState,
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
            ram: [RamNode::EMPTY; MAX_RAM_NODES],
            kern: kernfs::KernState::new(),
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

    pub fn mount_root(&mut self) -> Result<PathRef, FsError> {
        self.mount_root_fs(&RamFs)
    }

    pub fn mount_root_fs(&mut self, fs: &dyn FileSystem) -> Result<PathRef, FsError> {
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
        fs: &dyn FileSystem,
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

    fn mount_on(&mut self, dir: PathRef, fs: &dyn FileSystem) -> Result<u8, FsError> {
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
        let islot = self.d_islot(p.dslot)?;
        self.ops_stat(self.sb_of(p.mount), islot)
    }

    pub fn lstat(&mut self, cwd: Option<PathRef>, path: &str) -> Result<Stat, FsError> {
        let p = self.resolve(cwd, path, false)?;
        let islot = self.d_islot(p.dslot)?;
        self.ops_stat(self.sb_of(p.mount), islot)
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
        let sb = self.sb_of(dir.mount);
        self.dcache_drop_name(sb, dir.dslot, name);
        self.ops_unlink(sb, islot, name)?;
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
        let sb = self.sb_of(dir.mount);
        self.dcache_drop_name(sb, dir.dslot, name);
        match self.fstype(sb) {
            FsType::Ram | FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                let islot = self.d_islot(dir.dslot)?;
                self.ops_unlink(sb, islot, name)?;
            }
            FsType::Fat | FsType::Vibe => return Err(FsError::NotSupp),
        }
        Ok(())
    }

    /// Hard link. FAT returns [`FsError::NotSupp`].
    pub fn link(&mut self, cwd: Option<PathRef>, old: &str, new: &str) -> Result<(), FsError> {
        let src = self.resolve(cwd, old, true)?;
        let sslot = self.d_islot(src.dslot)?;
        if self.inodes[sslot as usize].kind != InodeKind::Reg {
            return Err(FsError::Inval);
        }
        let sb = self.sb_of(src.mount);
        match self.fstype(sb) {
            FsType::Fat | FsType::Vibe | FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                return Err(FsError::NotSupp);
            }
            FsType::Ram => {}
        }
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
                ram_link(v, dislot, name, v.inodes[sslot as usize].ino)?;
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
        match self.fstype(osb) {
            FsType::Fat | FsType::Vibe | FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                return Err(FsError::NotSupp);
            }
            FsType::Ram => {
                let oslot = self.d_islot(od.dslot)?;
                let nslot = self.d_islot(nd.dslot)?;
                ram_rename(self, oslot, oname, nslot, nname)?;
            }
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
            self.sync_inode_from_ram(islot);
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
        self.sync_inode_from_ram(islot);
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
        self.sync_inode_from_ram(islot);
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

    pub fn ram_used(&self) -> usize {
        let mut n = 0usize;
        let mut i = 0usize;
        while i < MAX_RAM_NODES {
            if self.ram[i].used {
                n += 1;
            }
            i += 1;
        }
        n
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

    pub fn fat_vol_of(&self, p: PathRef) -> Result<u8, FsError> {
        if (p.mount as usize) >= MAX_MOUNTS || !self.mounts[p.mount as usize].used {
            return Err(FsError::Io);
        }
        Ok(self.supers[self.sb_of(p.mount) as usize].fat_vol)
    }

    pub fn fat_iget(
        &mut self,
        sb: u8,
        ino: u32,
        kind: InodeKind,
        size: u64,
        clu: u32,
    ) -> Result<u16, FsError> {
        let mut i = 0usize;
        while i < MAX_INODES {
            if self.inodes[i].used && self.inodes[i].sb == sb && self.inodes[i].ino == ino {
                self.inodes[i].size = size;
                self.inodes[i].data0 = clu;
                self.inodes[i].kind = kind;
                self.inodes[i].refs = self.inodes[i].refs.saturating_add(1);
                self.inodes[i].clock = true;
                return Ok(i as u16);
            }
            i += 1;
        }
        let slot = self.inode_alloc()?;
        self.inodes[slot as usize] = Inode {
            used: true,
            clock: true,
            refs: 1,
            sb,
            ino,
            kind,
            mode: if kind == InodeKind::Dir {
                S_IFDIR_MODE
            } else {
                S_IFREG_MODE
            },
            nlink: if kind == InodeKind::Dir { 2 } else { 1 },
            size,
            atime: self.now,
            mtime: self.now,
            ctime: self.now,
            data0: clu,
        };
        Ok(slot)
    }

    pub fn fat_dcache(
        &mut self,
        parent: PathRef,
        name: &[u8],
        islot: u16,
    ) -> Result<PathRef, FsError> {
        let sb = self.sb_of(parent.mount);
        self.dcache_drop_name(sb, parent.dslot, name);
        self.dcache_drop_neg_in_dir(sb, parent.dslot);
        let ds = self.dcache_insert(sb, parent.dslot, name, Some(islot))?;
        Ok(PathRef {
            mount: parent.mount,
            dslot: ds,
        })
    }

    pub fn drop_name(&mut self, parent: PathRef, name: &[u8]) {
        let sb = self.sb_of(parent.mount);
        self.dcache_drop_name(sb, parent.dslot, name);
    }

    pub fn release_inode(&mut self, islot: u16) {
        self.iput(islot);
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

    fn ops_lookup(&mut self, sb: u8, dir: u16, name: &[u8]) -> Result<u32, FsError> {
        match self.fstype(sb) {
            FsType::Ram => RamFs.lookup(self, dir, name),
            FsType::Fat | FsType::Vibe => Err(FsError::NotSupp),
            FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                kernfs::kern_lookup(self, dir, name)
            }
        }
    }

    fn ops_create(
        &mut self,
        sb: u8,
        dir: u16,
        name: &[u8],
        kind: InodeKind,
        mode: u16,
        target: Option<&[u8]>,
    ) -> Result<u32, FsError> {
        match self.fstype(sb) {
            FsType::Ram => RamFs.create(self, dir, name, kind, mode, target),
            FsType::Fat | FsType::Vibe => Err(FsError::NotSupp),
            FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                kernfs::kern_create(self, dir, name, kind, mode, target)
            }
        }
    }

    fn ops_unlink(&mut self, sb: u8, dir: u16, name: &[u8]) -> Result<(), FsError> {
        match self.fstype(sb) {
            FsType::Ram => RamFs.unlink(self, dir, name),
            FsType::Fat | FsType::Vibe => Err(FsError::NotSupp),
            FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                kernfs::kern_unlink(self, dir, name)
            }
        }
    }

    fn ops_read(&mut self, sb: u8, islot: u16, off: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        match self.fstype(sb) {
            FsType::Ram => RamFs.read(self, islot, off, buf),
            FsType::Fat | FsType::Vibe => Err(FsError::NotSupp),
            FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                kernfs::kern_read(self, islot, off, buf)
            }
        }
    }

    fn ops_write(&mut self, sb: u8, islot: u16, off: u64, buf: &[u8]) -> Result<usize, FsError> {
        match self.fstype(sb) {
            FsType::Ram => RamFs.write(self, islot, off, buf),
            FsType::Fat | FsType::Vibe => Err(FsError::NotSupp),
            FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                kernfs::kern_write(self, islot, off, buf)
            }
        }
    }

    fn ops_truncate(&mut self, sb: u8, islot: u16, size: u64) -> Result<(), FsError> {
        match self.fstype(sb) {
            FsType::Ram => RamFs.truncate(self, islot, size),
            FsType::Fat | FsType::Vibe => Err(FsError::NotSupp),
            FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                kernfs::kern_truncate(self, islot, size)
            }
        }
    }

    fn ops_readdir(
        &mut self,
        sb: u8,
        islot: u16,
        cookie: u64,
        out: &mut Dirent,
    ) -> Result<Option<u64>, FsError> {
        match self.fstype(sb) {
            FsType::Ram => RamFs.readdir(self, islot, cookie, out),
            FsType::Fat | FsType::Vibe => Err(FsError::NotSupp),
            FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                kernfs::kern_readdir(self, islot, cookie, out)
            }
        }
    }

    fn ops_stat(&mut self, sb: u8, islot: u16) -> Result<Stat, FsError> {
        match self.fstype(sb) {
            FsType::Ram => RamFs.stat(self, islot),
            FsType::Fat | FsType::Vibe => fat_stat_inode(self, islot),
            FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                kernfs::kern_stat(self, islot)
            }
        }
    }

    fn ops_readlink(&mut self, sb: u8, islot: u16, buf: &mut [u8]) -> Result<usize, FsError> {
        match self.fstype(sb) {
            FsType::Ram => ram_readlink(self, islot, buf),
            FsType::Fat | FsType::Vibe => Err(FsError::NotSupp),
            FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                kernfs::kern_readlink(self, islot, buf)
            }
        }
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

    /// A counted reference to inode `ino` of `sb`. An unlinked inode
    /// (`nlink == 0`) is out of the hash: a new file that reuses its
    /// number gets a slot of its own.
    fn iget(&mut self, sb: u8, ino: u32) -> Result<u16, FsError> {
        let mut i = 0usize;
        while i < MAX_INODES {
            if self.inodes[i].used
                && self.inodes[i].sb == sb
                && self.inodes[i].ino == ino
                && self.inodes[i].nlink != 0
            {
                self.inodes[i].refs = self.inodes[i].refs.saturating_add(1);
                self.inodes[i].clock = true;
                return Ok(i as u16);
            }
            i += 1;
        }
        let slot = self.inode_alloc()?;
        self.inode_load(slot, sb, ino)?;
        self.inodes[slot as usize].refs = 1;
        self.inodes[slot as usize].clock = true;
        Ok(slot)
    }

    fn iput(&mut self, islot: u16) {
        let i = islot as usize;
        if i >= MAX_INODES || !self.inodes[i].used {
            return;
        }
        if self.inodes[i].refs == 0 {
            return;
        }
        self.inodes[i].refs -= 1;
        if self.inodes[i].refs == 0 {
            let sb = self.inodes[i].sb;
            let ino = self.inodes[i].ino;
            match self.fstype(sb) {
                FsType::Ram => {
                    ram_try_free(self, sb, ino);
                    if ram_nlink(self, sb, ino) == 0 {
                        self.inodes[i] = Inode::EMPTY;
                    }
                }
                FsType::Fat | FsType::Vibe => {}
                FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                    kernfs::kern_try_free(self, sb, ino);
                    if kernfs::kern_nlink(self, sb, ino) == 0 {
                        self.inodes[i] = Inode::EMPTY;
                    }
                }
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
            self.inode_evict(s as u16);
            return Ok(s as u16);
        }
        let mut s = 0usize;
        while s < MAX_INODES {
            if self.inodes[s].used && self.inodes[s].refs == 0 {
                self.inode_evict(s as u16);
                return Ok(s as u16);
            }
            s += 1;
        }
        Err(FsError::NoSpace)
    }

    fn inode_evict(&mut self, slot: u16) {
        let i = slot as usize;
        if !self.inodes[i].used || self.inodes[i].refs != 0 {
            return;
        }
        self.stats.i_evicts = self.stats.i_evicts.saturating_add(1);
        let sb = self.inodes[i].sb;
        let ino = self.inodes[i].ino;
        match self.fstype(sb) {
            FsType::Ram => ram_try_free(self, sb, ino),
            FsType::Fat | FsType::Vibe => {}
            FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                kernfs::kern_try_free(self, sb, ino)
            }
        }
        self.inodes[i] = Inode::EMPTY;
    }

    fn inode_load(&mut self, slot: u16, sb: u8, ino: u32) -> Result<(), FsError> {
        match self.fstype(sb) {
            FsType::Ram => ram_fill_inode(self, slot, sb, ino),
            FsType::Fat | FsType::Vibe => fat_fill_inode(self, slot, sb, ino),
            FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                kernfs::kern_fill_inode(self, slot, sb, ino)
            }
        }
    }

    fn sync_inode_from_ram(&mut self, islot: u16) {
        let sb = self.inodes[islot as usize].sb;
        let ino = self.inodes[islot as usize].ino;
        match self.fstype(sb) {
            FsType::Ram => {
                if let Some(r) = ram_meta(self, sb, ino) {
                    self.inodes[islot as usize].size = r.size;
                    self.inodes[islot as usize].nlink = r.nlink;
                    self.inodes[islot as usize].mode = r.mode;
                    self.inodes[islot as usize].kind = r.kind;
                    self.inodes[islot as usize].mtime = r.mtime;
                    self.inodes[islot as usize].ctime = r.ctime;
                    self.inodes[islot as usize].atime = r.atime;
                }
            }
            FsType::Fat | FsType::Vibe => {}
            FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                if let Some(r) = kernfs::kern_meta(self, sb, ino) {
                    self.inodes[islot as usize].size = r.size;
                    self.inodes[islot as usize].nlink = r.nlink;
                    self.inodes[islot as usize].mode = r.mode;
                    self.inodes[islot as usize].kind = r.kind;
                    self.inodes[islot as usize].mtime = r.mtime;
                    self.inodes[islot as usize].ctime = r.ctime;
                    self.inodes[islot as usize].atime = r.atime;
                }
            }
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
    fn new_super(&mut self, fs: &dyn FileSystem) -> Result<(u8, u16), FsError> {
        let sb = self.alloc_super()?;
        self.supers[sb as usize].fstype = fs.fstype();
        let ino = match fs.fill_super(self, sb) {
            Ok(i) => i,
            Err(e) => {
                self.sb_teardown(sb);
                return Err(e);
            }
        };
        let islot = match self.iget(sb, ino) {
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
        s.root_ino = ino;
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
                self.inodes[n] = Inode::EMPTY;
            }
            n += 1;
        }
        match self.fstype(sb) {
            FsType::Ram => ram_drop_sb(self, sb),
            FsType::Fat | FsType::Vibe => {}
            FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => {
                kernfs::kern_drop_sb(self, sb)
            }
        }
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
            Ok(ino) => {
                let islot = self.iget(sb, ino)?;
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
        let ino = self.ops_create(sb, dir_islot, name, kind, mode, target)?;
        self.sync_inode_from_ram(dir_islot);
        let islot = self.iget(sb, ino)?;
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

fn ram_idx(ino: u32) -> Option<usize> {
    if ino == 0 {
        return None;
    }
    let i = (ino - 1) as usize;
    if i >= MAX_RAM_NODES { None } else { Some(i) }
}

fn ram_get(vfs: &Vfs, sb: u8, ino: u32) -> Option<&RamNode> {
    let i = ram_idx(ino)?;
    let r = &vfs.ram[i];
    if r.used && r.sb == sb { Some(r) } else { None }
}

fn ram_get_mut(vfs: &mut Vfs, sb: u8, ino: u32) -> Option<&mut RamNode> {
    let i = ram_idx(ino)?;
    let r = &mut vfs.ram[i];
    if r.used && r.sb == sb { Some(r) } else { None }
}

fn ram_nlink(vfs: &Vfs, sb: u8, ino: u32) -> u32 {
    ram_get(vfs, sb, ino).map(|r| r.nlink).unwrap_or(0)
}

#[derive(Clone, Copy)]
struct RamMeta {
    kind: InodeKind,
    mode: u16,
    nlink: u32,
    size: u64,
    atime: u64,
    mtime: u64,
    ctime: u64,
}

fn ram_meta(vfs: &Vfs, sb: u8, ino: u32) -> Option<RamMeta> {
    let r = ram_get(vfs, sb, ino)?;
    Some(RamMeta {
        kind: r.kind,
        mode: r.mode,
        nlink: r.nlink,
        size: r.size,
        atime: r.atime,
        mtime: r.mtime,
        ctime: r.ctime,
    })
}

fn ram_alloc(vfs: &mut Vfs, sb: u8) -> Result<u32, FsError> {
    let mut i = 0usize;
    while i < MAX_RAM_NODES {
        if !vfs.ram[i].used {
            vfs.ram[i] = RamNode::EMPTY;
            vfs.ram[i].used = true;
            vfs.ram[i].sb = sb;
            return Ok((i as u32) + 1);
        }
        i += 1;
    }
    Err(FsError::NoSpace)
}

fn ram_try_free(vfs: &mut Vfs, sb: u8, ino: u32) {
    let Some(i) = ram_idx(ino) else {
        return;
    };
    if !vfs.ram[i].used || vfs.ram[i].sb != sb {
        return;
    }
    if vfs.ram[i].nlink != 0 {
        return;
    }
    let mut c = 0usize;
    while c < MAX_INODES {
        if vfs.inodes[c].used
            && vfs.inodes[c].sb == sb
            && vfs.inodes[c].ino == ino
            && vfs.inodes[c].refs != 0
        {
            return;
        }
        c += 1;
    }
    vfs.ram[i] = RamNode::EMPTY;
}

fn ram_drop_sb(vfs: &mut Vfs, sb: u8) {
    let mut i = 0usize;
    while i < MAX_RAM_NODES {
        if vfs.ram[i].used && vfs.ram[i].sb == sb {
            vfs.ram[i] = RamNode::EMPTY;
        }
        i += 1;
    }
}

fn ram_fill_super(vfs: &mut Vfs, sb: u8) -> Result<u32, FsError> {
    let ino = ram_alloc(vfs, sb)?;
    let t = vfs.now;
    if let Some(r) = ram_get_mut(vfs, sb, ino) {
        r.kind = InodeKind::Dir;
        r.mode = S_IFDIR_MODE;
        r.nlink = 2;
        r.size = 0;
        r.atime = t;
        r.mtime = t;
        r.ctime = t;
    }
    Ok(ino)
}

fn ram_fill_inode(vfs: &mut Vfs, slot: u16, sb: u8, ino: u32) -> Result<(), FsError> {
    let r = ram_meta(vfs, sb, ino).ok_or(FsError::NotFound)?;
    vfs.inodes[slot as usize] = Inode {
        used: true,
        clock: true,
        refs: 0,
        sb,
        ino,
        kind: r.kind,
        mode: r.mode,
        nlink: r.nlink,
        size: r.size,
        atime: r.atime,
        mtime: r.mtime,
        ctime: r.ctime,
        data0: 0,
    };
    Ok(())
}

fn fat_fill_inode(vfs: &mut Vfs, slot: u16, sb: u8, ino: u32) -> Result<(), FsError> {
    let root = vfs.supers[sb as usize].root_ino;
    let dir = ino == root || ino == 1;
    vfs.inodes[slot as usize] = Inode {
        used: true,
        clock: true,
        refs: 0,
        sb,
        ino,
        kind: if dir { InodeKind::Dir } else { InodeKind::Reg },
        mode: if dir { S_IFDIR_MODE } else { S_IFREG_MODE },
        nlink: if dir { 2 } else { 1 },
        size: 0,
        atime: vfs.now,
        mtime: vfs.now,
        ctime: vfs.now,
        data0: if dir {
            vfs.supers[sb as usize].fat_clu
        } else {
            0
        },
    };
    Ok(())
}

fn fat_stat_inode(vfs: &Vfs, islot: u16) -> Result<Stat, FsError> {
    let i = &vfs.inodes[islot as usize];
    if !i.used {
        return Err(FsError::NotFound);
    }
    Ok(Stat {
        ino: i.ino,
        kind: i.kind,
        mode: i.mode,
        nlink: i.nlink,
        size: i.size,
        atime: i.atime,
        mtime: i.mtime,
        ctime: i.ctime,
    })
}

fn ram_lookup(vfs: &mut Vfs, dir_islot: u16, name: &[u8]) -> Result<u32, FsError> {
    let sb = vfs.inodes[dir_islot as usize].sb;
    let ino = vfs.inodes[dir_islot as usize].ino;
    let r = ram_get(vfs, sb, ino).ok_or(FsError::NotFound)?;
    if r.kind != InodeKind::Dir {
        return Err(FsError::NotDir);
    }
    let mut i = 0usize;
    while i < r.ndent as usize {
        if r.dents[i].name.eq_bytes(name) {
            return Ok(r.dents[i].ino);
        }
        i += 1;
    }
    Err(FsError::NotFound)
}

fn ram_create(
    vfs: &mut Vfs,
    dir_islot: u16,
    name: &[u8],
    kind: InodeKind,
    mode: u16,
    target: Option<&[u8]>,
) -> Result<u32, FsError> {
    let nm = Name::from_bytes(name)?;
    if nm.is_dot() || nm.is_dotdot() {
        return Err(FsError::Inval);
    }
    if kind == InodeKind::Lnk {
        let t = target.ok_or(FsError::Inval)?;
        if t.is_empty() || t.len() > MAX_FILE_BYTES {
            return Err(FsError::Inval);
        }
    }
    match kind {
        InodeKind::Reg | InodeKind::Dir | InodeKind::Lnk => {}
        InodeKind::Chr | InodeKind::Blk => return Err(FsError::NotSupp),
    }
    let sb = vfs.inodes[dir_islot as usize].sb;
    let dir_ino = vfs.inodes[dir_islot as usize].ino;
    {
        let r = ram_get(vfs, sb, dir_ino).ok_or(FsError::NotFound)?;
        if r.kind != InodeKind::Dir {
            return Err(FsError::NotDir);
        }
        let mut i = 0usize;
        while i < r.ndent as usize {
            if r.dents[i].name.eq_bytes(name) {
                return Err(FsError::Exists);
            }
            i += 1;
        }
        if r.ndent as usize >= MAX_DIR_ENTS {
            return Err(FsError::NoSpace);
        }
    }
    let ino = ram_alloc(vfs, sb)?;
    let t = vfs.now;
    if let Some(r) = ram_get_mut(vfs, sb, ino) {
        r.kind = kind;
        r.mode = (mode & !S_IFMT) | kind.ifmt();
        r.nlink = if kind == InodeKind::Dir { 2 } else { 1 };
        r.atime = t;
        r.mtime = t;
        r.ctime = t;
        match kind {
            InodeKind::Lnk => {
                if let Some(tgt) = target {
                    r.data[..tgt.len()].copy_from_slice(tgt);
                    r.size = tgt.len() as u64;
                }
            }
            InodeKind::Reg | InodeKind::Dir => {
                r.size = 0;
            }
            InodeKind::Chr | InodeKind::Blk => {
                r.size = 0;
            }
        }
    }
    {
        let r = ram_get_mut(vfs, sb, dir_ino).ok_or(FsError::NotFound)?;
        let n = r.ndent as usize;
        r.dents[n] = RamDent { name: nm, ino };
        r.ndent += 1;
        r.mtime = t;
        r.ctime = t;
        if kind == InodeKind::Dir {
            r.nlink = r.nlink.saturating_add(1);
        }
    }
    vfs.inodes[dir_islot as usize].nlink = ram_nlink(vfs, sb, dir_ino);
    Ok(ino)
}

fn ram_unlink(vfs: &mut Vfs, dir_islot: u16, name: &[u8]) -> Result<(), FsError> {
    let sb = vfs.inodes[dir_islot as usize].sb;
    let dir_ino = vfs.inodes[dir_islot as usize].ino;
    let child = {
        let r = ram_get(vfs, sb, dir_ino).ok_or(FsError::NotFound)?;
        if r.kind != InodeKind::Dir {
            return Err(FsError::NotDir);
        }
        let mut found = None;
        let mut i = 0usize;
        while i < r.ndent as usize {
            if r.dents[i].name.eq_bytes(name) {
                found = Some((i, r.dents[i].ino));
                break;
            }
            i += 1;
        }
        found.ok_or(FsError::NotFound)?
    };
    let (idx, ino) = child;
    let kind = ram_get(vfs, sb, ino).ok_or(FsError::NotFound)?.kind;
    if kind == InodeKind::Dir {
        let nd = ram_get(vfs, sb, ino).ok_or(FsError::NotFound)?.ndent;
        if nd != 0 {
            return Err(FsError::NotEmpty);
        }
    }
    let t = vfs.now;
    {
        let r = ram_get_mut(vfs, sb, dir_ino).ok_or(FsError::NotFound)?;
        let last = r.ndent as usize - 1;
        r.dents[idx] = r.dents[last];
        r.dents[last] = RamDent::EMPTY;
        r.ndent -= 1;
        r.mtime = t;
        r.ctime = t;
        if kind == InodeKind::Dir {
            r.nlink = r.nlink.saturating_sub(1);
        }
    }
    if let Some(c) = ram_get_mut(vfs, sb, ino) {
        c.nlink = c.nlink.saturating_sub(1);
        c.ctime = t;
    }
    let mut i = 0usize;
    while i < MAX_INODES {
        if vfs.inodes[i].used && vfs.inodes[i].sb == sb && vfs.inodes[i].ino == ino {
            vfs.inodes[i].nlink = ram_nlink(vfs, sb, ino);
            vfs.inodes[i].ctime = t;
        }
        i += 1;
    }
    ram_try_free(vfs, sb, ino);
    vfs.inodes[dir_islot as usize].nlink = ram_nlink(vfs, sb, dir_ino);
    Ok(())
}

fn ram_read(vfs: &mut Vfs, islot: u16, off: u64, buf: &mut [u8]) -> Result<usize, FsError> {
    let sb = vfs.inodes[islot as usize].sb;
    let ino = vfs.inodes[islot as usize].ino;
    let r = ram_get(vfs, sb, ino).ok_or(FsError::NotFound)?;
    match r.kind {
        InodeKind::Dir => Err(FsError::IsDir),
        InodeKind::Reg | InodeKind::Lnk => {
            if off > r.size {
                return Ok(0);
            }
            let start = off as usize;
            let avail = (r.size as usize).saturating_sub(start);
            let n = avail.min(buf.len());
            buf[..n].copy_from_slice(&r.data[start..start + n]);
            Ok(n)
        }
        InodeKind::Chr | InodeKind::Blk => Err(FsError::NotSupp),
    }
}

fn ram_write(vfs: &mut Vfs, islot: u16, off: u64, buf: &[u8]) -> Result<usize, FsError> {
    let sb = vfs.inodes[islot as usize].sb;
    let ino = vfs.inodes[islot as usize].ino;
    let t = vfs.now;
    let r = ram_get_mut(vfs, sb, ino).ok_or(FsError::NotFound)?;
    match r.kind {
        InodeKind::Dir => Err(FsError::IsDir),
        InodeKind::Lnk | InodeKind::Chr | InodeKind::Blk => Err(FsError::Inval),
        InodeKind::Reg => {
            if off as usize > MAX_FILE_BYTES {
                return Err(FsError::NoSpace);
            }
            let start = off as usize;
            if start + buf.len() > MAX_FILE_BYTES {
                return Err(FsError::NoSpace);
            }
            r.data[start..start + buf.len()].copy_from_slice(buf);
            let end = (start + buf.len()) as u64;
            if end > r.size {
                r.size = end;
            }
            r.mtime = t;
            r.ctime = t;
            Ok(buf.len())
        }
    }
}

fn ram_truncate(vfs: &mut Vfs, islot: u16, size: u64) -> Result<(), FsError> {
    if size as usize > MAX_FILE_BYTES {
        return Err(FsError::NoSpace);
    }
    let sb = vfs.inodes[islot as usize].sb;
    let ino = vfs.inodes[islot as usize].ino;
    let t = vfs.now;
    let r = ram_get_mut(vfs, sb, ino).ok_or(FsError::NotFound)?;
    match r.kind {
        InodeKind::Dir => Err(FsError::IsDir),
        InodeKind::Lnk | InodeKind::Chr | InodeKind::Blk => Err(FsError::Inval),
        InodeKind::Reg => {
            let old = r.size as usize;
            let n = size as usize;
            if n < old {
                let mut i = n;
                while i < old {
                    r.data[i] = 0;
                    i += 1;
                }
            } else {
                let mut i = old;
                while i < n {
                    r.data[i] = 0;
                    i += 1;
                }
            }
            r.size = size;
            r.mtime = t;
            r.ctime = t;
            Ok(())
        }
    }
}

fn ram_readdir(
    vfs: &mut Vfs,
    islot: u16,
    cookie: u64,
    out: &mut Dirent,
) -> Result<Option<u64>, FsError> {
    let sb = vfs.inodes[islot as usize].sb;
    let ino = vfs.inodes[islot as usize].ino;
    let r = ram_get(vfs, sb, ino).ok_or(FsError::NotFound)?;
    if r.kind != InodeKind::Dir {
        return Err(FsError::NotDir);
    }
    let i = cookie as usize;
    if i >= r.ndent as usize {
        return Ok(None);
    }
    let child = r.dents[i].ino;
    let kind = ram_get(vfs, sb, child)
        .map(|c| c.kind)
        .unwrap_or(InodeKind::Reg);
    out.ino = child;
    out.kind = kind;
    out.name = r.dents[i].name;
    Ok(Some(cookie + 1))
}

fn ram_link(vfs: &mut Vfs, dir_islot: u16, name: &[u8], target: u32) -> Result<(), FsError> {
    let nm = Name::from_bytes(name)?;
    let sb = vfs.inodes[dir_islot as usize].sb;
    let dir_ino = vfs.inodes[dir_islot as usize].ino;
    {
        let t = ram_get(vfs, sb, target).ok_or(FsError::NotFound)?;
        if t.kind != InodeKind::Reg {
            return Err(FsError::Inval);
        }
    }
    {
        let r = ram_get(vfs, sb, dir_ino).ok_or(FsError::NotFound)?;
        if r.kind != InodeKind::Dir {
            return Err(FsError::NotDir);
        }
        let mut i = 0usize;
        while i < r.ndent as usize {
            if r.dents[i].name.eq_bytes(name) {
                return Err(FsError::Exists);
            }
            i += 1;
        }
        if r.ndent as usize >= MAX_DIR_ENTS {
            return Err(FsError::NoSpace);
        }
    }
    let tnow = vfs.now;
    {
        let r = ram_get_mut(vfs, sb, dir_ino).ok_or(FsError::NotFound)?;
        let n = r.ndent as usize;
        r.dents[n] = RamDent {
            name: nm,
            ino: target,
        };
        r.ndent += 1;
        r.mtime = tnow;
        r.ctime = tnow;
    }
    if let Some(t) = ram_get_mut(vfs, sb, target) {
        t.nlink = t.nlink.saturating_add(1);
        t.ctime = tnow;
    }
    Ok(())
}

fn ram_rename(
    vfs: &mut Vfs,
    oslot: u16,
    oname: &[u8],
    nslot: u16,
    nname: &[u8],
) -> Result<(), FsError> {
    let nm = Name::from_bytes(nname)?;
    let sb = vfs.inodes[oslot as usize].sb;
    if vfs.inodes[nslot as usize].sb != sb {
        return Err(FsError::Inval);
    }
    let odir = vfs.inodes[oslot as usize].ino;
    let ndir = vfs.inodes[nslot as usize].ino;
    let child = {
        let r = ram_get(vfs, sb, odir).ok_or(FsError::NotFound)?;
        let mut found = None;
        let mut i = 0usize;
        while i < r.ndent as usize {
            if r.dents[i].name.eq_bytes(oname) {
                found = Some((i, r.dents[i].ino));
                break;
            }
            i += 1;
        }
        found.ok_or(FsError::NotFound)?
    };
    let (idx, ino) = child;
    let kind = ram_get(vfs, sb, ino).ok_or(FsError::NotFound)?.kind;
    if odir == ndir && oname == nname {
        return Ok(());
    }
    {
        let r = ram_get(vfs, sb, ndir).ok_or(FsError::NotFound)?;
        if r.kind != InodeKind::Dir {
            return Err(FsError::NotDir);
        }
        let mut i = 0usize;
        while i < r.ndent as usize {
            if r.dents[i].name.eq_bytes(nname) {
                return Err(FsError::Exists);
            }
            i += 1;
        }
        if odir != ndir && r.ndent as usize >= MAX_DIR_ENTS {
            return Err(FsError::NoSpace);
        }
    }
    let t = vfs.now;
    {
        let r = ram_get_mut(vfs, sb, odir).ok_or(FsError::NotFound)?;
        let last = r.ndent as usize - 1;
        r.dents[idx] = r.dents[last];
        r.dents[last] = RamDent::EMPTY;
        r.ndent -= 1;
        r.mtime = t;
        if kind == InodeKind::Dir {
            r.nlink = r.nlink.saturating_sub(1);
        }
    }
    {
        let r = ram_get_mut(vfs, sb, ndir).ok_or(FsError::NotFound)?;
        let n = r.ndent as usize;
        r.dents[n] = RamDent { name: nm, ino };
        r.ndent += 1;
        r.mtime = t;
        if kind == InodeKind::Dir {
            r.nlink = r.nlink.saturating_add(1);
        }
    }
    Ok(())
}

fn ram_stat(vfs: &mut Vfs, islot: u16) -> Result<Stat, FsError> {
    let sb = vfs.inodes[islot as usize].sb;
    let ino = vfs.inodes[islot as usize].ino;
    if let Some(r) = ram_get(vfs, sb, ino) {
        return Ok(Stat {
            ino,
            kind: r.kind,
            mode: r.mode,
            nlink: r.nlink,
            size: r.size,
            atime: r.atime,
            mtime: r.mtime,
            ctime: r.ctime,
        });
    }
    let n = &vfs.inodes[islot as usize];
    Ok(Stat {
        ino: n.ino,
        kind: n.kind,
        mode: n.mode,
        nlink: n.nlink,
        size: n.size,
        atime: n.atime,
        mtime: n.mtime,
        ctime: n.ctime,
    })
}

fn ram_readlink(vfs: &mut Vfs, islot: u16, buf: &mut [u8]) -> Result<usize, FsError> {
    let sb = vfs.inodes[islot as usize].sb;
    let ino = vfs.inodes[islot as usize].ino;
    let r = ram_get(vfs, sb, ino).ok_or(FsError::NotFound)?;
    match r.kind {
        InodeKind::Lnk => {
            let n = (r.size as usize).min(buf.len()).min(MAX_FILE_BYTES);
            buf[..n].copy_from_slice(&r.data[..n]);
            Ok(n)
        }
        InodeKind::Dir => Err(FsError::IsDir),
        InodeKind::Reg | InodeKind::Chr | InodeKind::Blk => Err(FsError::Inval),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ram() -> Vfs {
        let mut v = Vfs::new();
        v.mount_root().unwrap();
        v
    }

    fn ino_of(v: &Vfs, p: PathRef) -> u32 {
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
        assert_eq!(ino_of(&v, b), ino_of(&v, same));
        let a = v.resolve(None, "/a/b/..", true).unwrap();
        let a2 = v.resolve(None, "/a", true).unwrap();
        assert_eq!(ino_of(&v, a), ino_of(&v, a2));
        let root = v.resolve(None, "/a/b/../..", true).unwrap();
        assert_eq!(ino_of(&v, root), ino_of(&v, v.root().unwrap()));
        let stay = v.resolve(None, "/..", true).unwrap();
        assert_eq!(ino_of(&v, stay), ino_of(&v, v.root().unwrap()));
        let mixed = v.resolve(None, "/a/./b/../b", true).unwrap();
        assert_eq!(ino_of(&v, mixed), ino_of(&v, b));
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
        v.mount(None, "/mnt", &RamFs).unwrap();
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
        let mut v = ram();
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
        let used = v.ram_used();
        v.fd_close(&mut tab, fd).unwrap();
        assert!(v.ram_used() < used);
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
        assert_eq!(v.stat(Some(a), "f").unwrap().ino, ino_of(&v, f));
        let root = v.resolve(Some(a), "..", true).unwrap();
        assert_eq!(ino_of(&v, root), ino_of(&v, v.root().unwrap()));
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
        assert_eq!(v.ram.len(), limits::MAX_RAM_NODES);
        assert_eq!(FdTable::new().fds.len(), limits::MAX_FDS);
        assert_eq!(RamNode::EMPTY.data.len(), limits::MAX_TMPFS_FILE_BYTES);
        assert_eq!(RamNode::EMPTY.dents.len(), limits::MAX_TMPFS_DIR_ENTS);
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
        v.mount(None, "/a/m", &RamFs).unwrap();
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
        v.mount(None, "/p", &RamFs).unwrap();
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
        let m = v.mount(None, "/a/m", &RamFs).unwrap();
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
        v.mount(None, "/m", &RamFs).unwrap();
        v.creat(None, "/m/f", 0o644).unwrap();
        let f = v.resolve(None, "/m/f", true).unwrap();
        let fi = v.islot(f).unwrap();
        let held = v.iget(v.sb_of(f.mount), v.inodes[fi as usize].ino).unwrap();
        v.resolve(None, "/m", true).unwrap();
        let before = (v.dentries, v.inodes, v.mounts, v.supers);
        assert_eq!(v.umount(None, "/m").unwrap_err(), FsError::Busy);
        assert_eq!((v.dentries, v.inodes, v.mounts, v.supers), before);
        v.iput(held);
        assert_eq!(v.stat(None, "/m/f").unwrap().kind, InodeKind::Reg);
        v.creat(None, "/m/g", 0o644).unwrap();
        let g = v.resolve(None, "/m/g", true).unwrap();
        v.dget(g.dslot).unwrap();
        v.resolve(None, "/m", true).unwrap();
        let before = (v.dentries, v.inodes, v.mounts, v.supers);
        assert_eq!(v.umount(None, "/m").unwrap_err(), FsError::Busy);
        assert_eq!((v.dentries, v.inodes, v.mounts, v.supers), before);
        assert_eq!(v.stat(None, "/m/g").unwrap().kind, InodeKind::Reg);
        v.dput(g.dslot);
        assert_dcache_sound(&v);
        v.umount(None, "/m").unwrap();
        assert_dcache_sound(&v);
        assert_eq!(v.stat(None, "/m/f").unwrap_err(), FsError::NotFound);
        assert!(v.inodes.iter().all(|i| !i.used || i.sb == 0));
    }
}
