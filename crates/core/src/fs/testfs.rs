//! Host-test backends for `Vfs`'s tests: a keyed store outside `Vfs`,
//! one with no ops, and one that checks the VFS lock is dropped.

use super::*;

/// A ramfs over a store of its own, which the test leaks.
pub(crate) fn ramfs() -> &'static RamFs<std::sync::Mutex<RamState>> {
    std::boxed::Box::leak(std::boxed::Box::new(RamFs::new(std::sync::Mutex::new(
        RamState::new(),
    ))))
}

/// A test backend whose storage lives outside `Vfs`, in `KEYFS`, found
/// by the store id in its superblock's private word 0. Node `n` has
/// key `[n, 0, 0]`, `st_ino` `n + 100` and private words `[7 * n, w]`,
/// where `w` counts the writes made through the inode.
pub(crate) struct KeyFs {
    pub(crate) id: u64,
}

pub(crate) struct KeyOps;

#[derive(Clone)]
pub(crate) struct KNode {
    pub(crate) kind: InodeKind,
    pub(crate) nlink: u32,
    pub(crate) data: Vec<u8>,
    pub(crate) alive: bool,
}

#[derive(Default)]
pub(crate) struct Store {
    pub(crate) nodes: Vec<KNode>,
    pub(crate) names: Vec<(u32, Vec<u8>, u32)>,
    pub(crate) evicts: u32,
    pub(crate) fills: u32,
    pub(crate) mounts: Vec<Vec<u8>>,
    pub(crate) umounts: Vec<(Vec<u8>, bool)>,
}

pub(crate) static KEYFS: std::sync::Mutex<Vec<Store>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn keyfs_new() -> &'static KeyFs {
    let mut g = KEYFS.lock().unwrap();
    g.push(Store::default());
    std::boxed::Box::leak(std::boxed::Box::new(KeyFs {
        id: (g.len() - 1) as u64,
    }))
}

pub(crate) fn with_store<R>(id: u64, f: impl FnOnce(&mut Store) -> R) -> R {
    f(&mut KEYFS.lock().unwrap()[id as usize])
}

pub(crate) fn knode_info(s: &Store, n: u32) -> InodeInfo {
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
            s.fills += 1;
            if s.nodes.is_empty() {
                s.nodes.push(KNode {
                    kind: InodeKind::Dir,
                    nlink: 2,
                    data: Vec::new(),
                    alive: true,
                });
            }
            Ok(knode_info(s, 0))
        })
    }
    fn on_mount(&self, _cx: &mut OpCx<'_>, at: &[u8]) {
        with_store(self.id, |s| s.mounts.push(at.to_vec()));
    }
    fn on_umount(&self, _cx: &mut OpCx<'_>, at: &[u8], last: bool) {
        with_store(self.id, |s| s.umounts.push((at.to_vec(), last)));
    }
}

impl InodeOps for KeyOps {
    fn lookup(&self, cx: &mut OpCx<'_>, dir: &Inode, name: &[u8]) -> Result<InodeInfo, FsError> {
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
            if let Some(n) = s.nodes.get_mut(ino.key[0] as usize) {
                n.alive = false;
            }
            s.evicts += 1;
            Ok(())
        })
    }
}

/// A filesystem with no ops: its root and nothing else.
pub(crate) struct NoOpsFs;

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

/// A block-device filesystem over `KeyFs`'s store whose every hook and op
/// checks that the VFS lock, `vfs`, is not held: `try_lock` succeeds.
pub(crate) struct LockedFs {
    pub(crate) key: &'static KeyFs,
    pub(crate) vfs: &'static std::sync::Mutex<Vfs>,
    pub(crate) calls: std::sync::atomic::AtomicU32,
}

impl LockedFs {
    fn unlocked(&self) {
        assert!(
            self.vfs.try_lock().is_ok(),
            "backend called under the VFS lock"
        );
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl FileSystem for LockedFs {
    fn name(&self) -> &'static str {
        "locked"
    }
    fn fstype(&self) -> FsType {
        self.key.fstype()
    }
    fn ops(&'static self) -> Option<&'static dyn InodeOps> {
        Some(self)
    }
    fn fill_super(&self, cx: &mut OpCx<'_>) -> Result<InodeInfo, FsError> {
        self.unlocked();
        self.key.fill_super(cx)
    }
    fn on_mount(&self, cx: &mut OpCx<'_>, at: &[u8]) {
        self.unlocked();
        self.key.on_mount(cx, at);
    }
    fn on_umount(&self, cx: &mut OpCx<'_>, at: &[u8], last: bool) {
        self.unlocked();
        self.key.on_umount(cx, at, last);
    }
}

impl InodeOps for LockedFs {
    fn lookup(&self, cx: &mut OpCx<'_>, dir: &Inode, name: &[u8]) -> Result<InodeInfo, FsError> {
        self.unlocked();
        KeyOps.lookup(cx, dir, name)
    }
    fn create(
        &self,
        cx: &mut OpCx<'_>,
        dir: &mut Inode,
        name: &[u8],
        kind: InodeKind,
        mode: u16,
        target: Option<&[u8]>,
    ) -> Result<InodeInfo, FsError> {
        self.unlocked();
        KeyOps.create(cx, dir, name, kind, mode, target)
    }
    fn unlink(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        self.unlocked();
        KeyOps.unlink(cx, dir, name)
    }
    fn rmdir(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        self.unlocked();
        with_store(cx.private[0], |s| {
            let n = s
                .names
                .iter()
                .find(|e| e.0 == dir.key[0] && e.1 == name)
                .ok_or(FsError::NotFound)?
                .2;
            if s.names.iter().any(|e| e.0 == n) {
                return Err(FsError::NotEmpty);
            }
            Ok(())
        })?;
        KeyOps.unlink(cx, dir, name)
    }
    fn rename(
        &self,
        cx: &mut OpCx<'_>,
        odir: &mut Inode,
        oname: &[u8],
        ndir: &mut Inode,
        nname: &[u8],
    ) -> Result<Option<Key>, FsError> {
        self.unlocked();
        with_store(cx.private[0], |s| {
            if s.names.iter().any(|e| e.0 == ndir.key[0] && e.1 == nname) {
                return Err(FsError::Exists);
            }
            let e = s
                .names
                .iter_mut()
                .find(|e| e.0 == odir.key[0] && e.1 == oname)
                .ok_or(FsError::NotFound)?;
            *e = (ndir.key[0], nname.to_vec(), e.2);
            Ok(None)
        })
    }
    fn read(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        self.unlocked();
        KeyOps.read(cx, ino, off, buf)
    }
    fn write(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError> {
        self.unlocked();
        KeyOps.write(cx, ino, off, buf)
    }
    fn truncate(&self, cx: &mut OpCx<'_>, ino: &mut Inode, size: u64) -> Result<(), FsError> {
        self.unlocked();
        KeyOps.truncate(cx, ino, size)
    }
    fn readdir(
        &self,
        cx: &mut OpCx<'_>,
        dir: &Inode,
        cookie: u64,
        out: &mut Dirent,
    ) -> Result<Option<u64>, FsError> {
        self.unlocked();
        KeyOps.readdir(cx, dir, cookie, out)
    }
    fn getattr(&self, cx: &mut OpCx<'_>, ino: &mut Inode) -> Result<(), FsError> {
        self.unlocked();
        KeyOps.getattr(cx, ino)
    }
    fn sync(&self, _cx: &mut OpCx<'_>) -> Result<(), FsError> {
        self.unlocked();
        Ok(())
    }
    fn evict(&self, cx: &mut OpCx<'_>, ino: &Inode) -> Result<(), FsError> {
        self.unlocked();
        KeyOps.evict(cx, ino)
    }
    fn kill_sb(&self, _cx: &mut OpCx<'_>) {
        self.unlocked();
    }
}
