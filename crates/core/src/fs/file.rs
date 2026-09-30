use super::*;

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

impl Vfs {
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

impl Vfs {
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
            .ok_or(FsError::NFile)?;
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
            return Err(FsError::Badf);
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
            return Err(FsError::Badf);
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
            SeekFrom::End(d) => rel(ino.cur_size(), d)?,
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
}

impl<'l, L: Guarded<Vfs>> FileApi<'l, L> {
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
            #[expect(
                clippy::let_underscore_must_use,
                reason = "cleanup after an error already returned (DESIGN §2.5)"
            )]
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

    /// Move open file `f`'s offset, when its inode can seek (`check_seek`).
    pub fn seek(&self, f: &FileRef, pos: SeekFrom) -> Result<u64, FsError> {
        let mut c = self.with(|v| v.file_islot(f.id).and_then(|i| v.call(i)))?;
        let r = c.run(|o, cx, n| o.check_seek(cx, n));
        self.step(|v| {
            v.finish(c, false);
            r.and_then(|()| v.file_seek(f.id, pos))
        })
    }

    pub fn stat(&self, f: &FileRef) -> Result<Stat, FsError> {
        let islot = self.with(|v| v.file_islot(f.id))?;
        self.stat_islot(islot)
    }

    /// Stat inode `islot`, which the caller keeps referenced, through a
    /// `getattr` call.
    pub(super) fn stat_islot(&self, islot: u16) -> Result<Stat, FsError> {
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
        self.readdir_from(f, 0, &mut |d, _| cb(d)).map(|_| ())
    }

    /// Report the entries of open directory `f` from cookie `start` (`.`
    /// at 0, `..` at 1, the backend's cookies after them) to `emit`, each
    /// with the cookie of the entry after it, until `emit` returns false or
    /// the entries run out. Returns the cookie of the first entry not
    /// consumed: the one `emit` refused, or the end. The file position does
    /// not move. `emit` runs with the lock dropped.
    pub fn readdir_from(
        &self,
        f: &FileRef,
        start: u64,
        emit: &mut dyn FnMut(&DirEntry, u64) -> bool,
    ) -> Result<u64, FsError> {
        let mut cookie = start;
        loop {
            let (ent, next) = match self.with(|v| v.readdir_step(f.id, cookie))? {
                Rd::Entry(d, next) => (d, next),
                Rd::Call(mut c, bc) => {
                    let mut out = Dirent::EMPTY;
                    let r = c.run(|o, cx, d| o.readdir(cx, d, bc, &mut out));
                    self.step(|v| v.finish(c, false));
                    match r? {
                        None => return Ok(cookie),
                        Some(n) => (out, n.checked_add(2).ok_or(FsError::Io)?),
                    }
                }
            };
            if !emit(&ent, next) {
                return Ok(cookie);
            }
            cookie = next;
        }
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
}
