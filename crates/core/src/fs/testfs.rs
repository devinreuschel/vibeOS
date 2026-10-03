//! Host-test backends for `Vfs`'s tests: a keyed store outside `Vfs`,
//! one with no ops, and one that checks the VFS lock is dropped; and the
//! `v.mkdir(None, …)` wrappers that drive a `Vfs` through [`FileApi`].

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
        assert_eq!(ino.refs, 0, "evict of an inode the VFS holds");
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
        seen: RenameSeen,
    ) -> Result<Option<Key>, FsError> {
        self.unlocked();
        with_store(cx.private[0], |s| {
            let at = |d: &Inode, name: &[u8]| {
                s.names
                    .iter()
                    .find(|e| e.0 == d.key[0] && e.1 == name)
                    .map(|e| [e.2, 0, 0])
            };
            seen.check(at(odir, oname), at(ndir, nname))?;
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
    fn release(&self, _cx: &mut OpCx<'_>) {
        self.unlocked();
    }
}

/// The VFS with no lock around it, for host tests that drive one `Vfs`
/// through [`FileApi`] from one thread.
pub(crate) struct Direct<'a>(core::cell::RefCell<&'a mut Vfs>);

impl Guarded<Vfs> for Direct<'_> {
    fn with<R>(&self, f: impl FnOnce(&mut Vfs) -> R) -> R {
        f(&mut self.0.borrow_mut())
    }
}

/// Host-test wrappers over the driver, in the `v.mkdir(None, …)` style
/// the tests had before the File API: each runs [`FileApi`] calls on this
/// `Vfs`.
impl Vfs {
    pub(crate) fn api<R>(&mut self, f: impl FnOnce(&FileApi<'_, Direct<'_>>) -> R) -> R {
        let d = Direct(core::cell::RefCell::new(self));
        f(&FileApi::new(&d))
    }

    pub(crate) fn mount_root_fs(
        &mut self,
        fs: &'static dyn FileSystem,
    ) -> Result<PathRef, FsError> {
        self.api(|a| a.mount_root(fs, None, false, None))?;
        self.root()
    }

    pub(crate) fn mount(
        &mut self,
        base: Option<WalkBase>,
        at: &str,
        fs: &'static dyn FileSystem,
    ) -> Result<u8, FsError> {
        self.api(|a| a.mount_fs(base, at.as_bytes(), fs, None, false, None))
            .map(|m| m.mount)
    }

    pub(crate) fn mount_dev(
        &mut self,
        at: &str,
        fs: &'static dyn FileSystem,
        dev: u64,
        ro: bool,
    ) -> Result<Mounted, FsError> {
        self.api(|a| a.mount_fs(None, at.as_bytes(), fs, Some(dev), ro, None))
    }

    pub(crate) fn umount(&mut self, base: Option<WalkBase>, at: &str) -> Result<(), FsError> {
        self.api(|a| a.umount(base, at.as_bytes()))
    }

    /// [`FileApi::dir_get`] on this `Vfs`.
    pub(crate) fn dir_get(
        &mut self,
        base: Option<WalkBase>,
        path: &str,
    ) -> Result<DirRef, FsError> {
        self.api(|a| a.dir_get(base, path.as_bytes()))
    }

    /// The path `path` resolves to, not held.
    pub(crate) fn resolve(
        &mut self,
        base: Option<WalkBase>,
        path: &str,
        follow: bool,
    ) -> Result<PathRef, FsError> {
        self.api(|a| {
            let p = a.walk(base, path.as_bytes(), follow)?;
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

    pub(crate) fn stat(&mut self, base: Option<WalkBase>, path: &str) -> Result<Stat, FsError> {
        self.api(|a| a.stat_path(base, path.as_bytes(), true))
    }

    pub(crate) fn lstat(&mut self, base: Option<WalkBase>, path: &str) -> Result<Stat, FsError> {
        self.api(|a| a.stat_path(base, path.as_bytes(), false))
    }

    pub(crate) fn mkdir(
        &mut self,
        base: Option<WalkBase>,
        path: &str,
        mode: u16,
    ) -> Result<(), FsError> {
        self.api(|a| a.mkdir(base, path.as_bytes(), u32::from(mode)))
    }

    pub(crate) fn creat(
        &mut self,
        base: Option<WalkBase>,
        path: &str,
        mode: u16,
    ) -> Result<(), FsError> {
        self.api(|a| a.create(base, path.as_bytes(), InodeKind::Reg, mode | S_IFREG, None))
    }

    pub(crate) fn symlink(
        &mut self,
        base: Option<WalkBase>,
        path: &str,
        target: &str,
    ) -> Result<(), FsError> {
        self.api(|a| a.symlink(base, path.as_bytes(), target.as_bytes()))
    }

    pub(crate) fn unlink(&mut self, base: Option<WalkBase>, path: &str) -> Result<(), FsError> {
        self.api(|a| a.unlink(base, path.as_bytes()))
    }

    pub(crate) fn rmdir(&mut self, base: Option<WalkBase>, path: &str) -> Result<(), FsError> {
        self.api(|a| a.rmdir(base, path.as_bytes()))
    }

    pub(crate) fn link(
        &mut self,
        base: Option<WalkBase>,
        old: &str,
        new: &str,
    ) -> Result<(), FsError> {
        self.api(|a| a.link(base, old.as_bytes(), new.as_bytes()))
    }

    pub(crate) fn rename(
        &mut self,
        base: Option<WalkBase>,
        old: &str,
        new: &str,
    ) -> Result<(), FsError> {
        self.api(|a| a.rename(base, old.as_bytes(), new.as_bytes()))
    }

    pub(crate) fn truncate(
        &mut self,
        base: Option<WalkBase>,
        path: &str,
        size: u64,
    ) -> Result<(), FsError> {
        self.api(|a| a.truncate(base, path.as_bytes(), size))
    }

    pub(crate) fn open_path(
        &mut self,
        base: Option<WalkBase>,
        path: &str,
        flags: u32,
        mode: u16,
    ) -> Result<FileRef, FsError> {
        self.api(|a| {
            a.open(
                base,
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

/// A filesystem over `KeyFs`'s store that keys files as FAT does, by
/// where their entry sits: names match without regard to case, a rename
/// that changes only a name's case moves the file to a new key and frees
/// the old one, and a create takes the lowest free key, so the next file
/// made can take the key a rename left.
pub(crate) struct FoldFs {
    pub(crate) key: &'static KeyFs,
}

pub(crate) fn foldfs_new() -> &'static FoldFs {
    std::boxed::Box::leak(std::boxed::Box::new(FoldFs { key: keyfs_new() }))
}

fn fold_eq(a: &[u8], b: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b)
}

impl FileSystem for FoldFs {
    fn name(&self) -> &'static str {
        "fold"
    }
    fn fstype(&self) -> FsType {
        self.key.fstype()
    }
    fn ops(&'static self) -> Option<&'static dyn InodeOps> {
        Some(self)
    }
    fn fill_super(&self, cx: &mut OpCx<'_>) -> Result<InodeInfo, FsError> {
        self.key.fill_super(cx)
    }
}

impl InodeOps for FoldFs {
    fn name_eq(&self, cached: &[u8], asked: &[u8]) -> bool {
        fold_eq(cached, asked)
    }
    fn lookup(&self, cx: &mut OpCx<'_>, dir: &Inode, name: &[u8]) -> Result<InodeInfo, FsError> {
        with_store(cx.private[0], |s| {
            let n = s
                .names
                .iter()
                .find(|e| e.0 == dir.key[0] && fold_eq(&e.1, name))
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
            if s.names
                .iter()
                .any(|e| e.0 == dir.key[0] && fold_eq(&e.1, name))
            {
                return Err(FsError::Exists);
            }
            let node = KNode {
                kind,
                nlink: 1,
                data: Vec::new(),
                alive: true,
            };
            let n = match s.nodes.iter().position(|k| !k.alive) {
                Some(i) => {
                    s.nodes[i] = node;
                    i
                }
                None => {
                    s.nodes.push(node);
                    s.nodes.len() - 1
                }
            } as u32;
            s.names.push((dir.key[0], name.to_vec(), n));
            Ok(knode_info(s, n))
        })
    }
    fn rename(
        &self,
        cx: &mut OpCx<'_>,
        odir: &mut Inode,
        oname: &[u8],
        ndir: &mut Inode,
        nname: &[u8],
        seen: RenameSeen,
    ) -> Result<Option<Key>, FsError> {
        with_store(cx.private[0], |s| {
            let at = |d: &Inode, name: &[u8]| {
                s.names
                    .iter()
                    .find(|e| e.0 == d.key[0] && fold_eq(&e.1, name))
                    .map(|e| [e.2, 0, 0])
            };
            seen.check(at(odir, oname), at(ndir, nname))?;
            let i = s
                .names
                .iter()
                .position(|e| e.0 == odir.key[0] && fold_eq(&e.1, oname))
                .ok_or(FsError::NotFound)?;
            if odir.key[0] != ndir.key[0] || !fold_eq(oname, nname) {
                return Err(FsError::Inval);
            }
            // A new entry for the new spelling, the old one freed: the file
            // moves to the next key, and its old key is free to reuse.
            let old = s.names[i].2 as usize;
            let moved = s.nodes[old].clone();
            s.nodes.push(moved);
            let n = (s.nodes.len() - 1) as u32;
            s.nodes[old].alive = false;
            s.nodes[old].data.clear();
            s.names[i] = (ndir.key[0], nname.to_vec(), n);
            Ok(Some([n, 0, 0]))
        })
    }
    fn read(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        KeyOps.read(cx, ino, off, buf)
    }
    fn write(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError> {
        KeyOps.write(cx, ino, off, buf)
    }
    fn truncate(&self, cx: &mut OpCx<'_>, ino: &mut Inode, size: u64) -> Result<(), FsError> {
        KeyOps.truncate(cx, ino, size)
    }
    fn getattr(&self, cx: &mut OpCx<'_>, ino: &mut Inode) -> Result<(), FsError> {
        KeyOps.getattr(cx, ino)
    }
    fn evict(&self, cx: &mut OpCx<'_>, ino: &Inode) -> Result<(), FsError> {
        KeyOps.evict(cx, ino)
    }
}
