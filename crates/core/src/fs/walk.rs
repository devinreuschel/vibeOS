use super::*;

impl Vfs {
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
        let n = if v.kind == InodeKind::Dir {
            0
        } else {
            v.nlink.saturating_sub(1)
        };
        v.set_nlink(n);
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
    pub(super) fn rekey_slot(&mut self, i: u16, to: Key) {
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
                debug_assert!(
                    !name_is_dot(name) && !name_is_dotdot(name),
                    "a backend lookup never sees `.` or `..`"
                );
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
                            #[expect(
                                clippy::let_underscore_must_use,
                                reason = "an uncached negative dentry costs only a later lookup; nothing to act on (DESIGN §2.5)"
                            )]
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

impl<'l, L: Guarded<Vfs>> FileApi<'l, L> {
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
