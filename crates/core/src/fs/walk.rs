use super::*;

/// Where a walk starts: an absolute path at `root`, a relative one at
/// `cwd`, and `..` stays put at `root` (path_resolution(7)). A process's
/// base names the directories its root and working-directory
/// [`DirRef`]s hold; `None` in a walk means the namespace root for both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalkBase {
    pub root: PathRef,
    pub cwd: PathRef,
}

/// A counted reference to a directory: one count on its mount, which
/// `umount`'s busy check sees, and one on its dentry, the count a child
/// dentry pins its parent with (DESIGN §2.11 rule 2), so neither is
/// evicted while it lives. A process's root and working directory are
/// each one. Hand it to [`Vfs::dir_put`]: dropping it leaks both counts,
/// and a debug build panics.
#[must_use]
#[derive(Debug)]
pub struct DirRef {
    at: PathRef,
}

impl DirRef {
    /// The directory this reference holds.
    pub fn at(&self) -> PathRef {
        self.at
    }
}

impl Drop for DirRef {
    #[allow(
        clippy::panic,
        reason = "a dropped DirRef leaks a mount and a dentry count; debug builds panic, as a dropped Frames does (DESIGN §4.2)"
    )]
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        {
            // A host test that fails while it holds a reference unwinds
            // through here; a second panic would abort the test binary.
            #[cfg(any(test, feature = "std"))]
            if std::thread::panicking() {
                return;
            }
            panic!(
                "fs: dropped DirRef on mount {} dentry {}",
                self.at.mount, self.at.dslot
            );
        }
    }
}

impl Vfs {
    /// A reference to the namespace root.
    pub fn dir_root(&mut self) -> Result<DirRef, FsError> {
        let r = self.root()?;
        self.path_get(r)?;
        Ok(DirRef { at: r })
    }

    /// Another reference to directory `at`, which a reference already
    /// holds, as `fork` copies a parent's.
    pub fn dir_dup(&mut self, at: PathRef) -> Result<DirRef, FsError> {
        debug_assert!(
            self.dentry_refs(at) != 0
                && self
                    .mounts
                    .get(at.mount as usize)
                    .is_some_and(|m| m.used && m.refs != 0),
            "dir_dup of a directory nothing holds"
        );
        self.path_get(at)?;
        Ok(DirRef { at })
    }

    /// Drop a reference. A dentry that lost its name while it was held
    /// is freed at its last put, and a release this queues runs at the
    /// driver's next step ([`FileApi::dir_put`]).
    pub fn dir_put(&mut self, r: DirRef) {
        self.path_put(r.at);
        core::mem::forget(r);
    }

    /// The holders of `at`'s dentry: its children, the mounts on it, the
    /// superblock for a root, and every held path and reference.
    pub fn dentry_refs(&self, at: PathRef) -> u32 {
        self.dentries
            .get(at.dslot as usize)
            .map_or(0, |d| u32::from(d.refs))
    }

    /// How many dentries the cache cannot evict: those in use with a
    /// holder (a child, a mount on it, the superblock for a root, or a held
    /// path or reference).
    pub fn dentries_held(&self) -> usize {
        self.dentries
            .iter()
            .filter(|d| d.used && d.refs != 0)
            .count()
    }

    /// The path from `base`'s root (the namespace root when none) to the
    /// directory `at`, into `out`, crossing mountpoints; its length.
    /// `NotFound` when a dentry on the way lost its name, `NameTooLong`
    /// when `out` is too short. A directory outside the root is named
    /// from the namespace root.
    pub fn dir_path(
        &self,
        base: Option<WalkBase>,
        at: PathRef,
        out: &mut [u8],
    ) -> Result<usize, FsError> {
        let mut root = match base {
            Some(b) => b.root,
            None => self.root()?,
        };
        self.follow_mount(&mut root.mount, &mut root.dslot);
        let (mut m, mut d) = (at.mount, at.dslot);
        let mut end = out.len();
        let mut n = 0usize;
        while (m, d) != (root.mount, root.dslot) {
            n += 1;
            if n > self.dentries.len().saturating_add(self.mounts.len()) {
                return Err(FsError::Loop);
            }
            let mt = self.mounts.get(m as usize).ok_or(FsError::Io)?;
            if !mt.used {
                return Err(FsError::NotFound);
            }
            if mt.root_dslot == d {
                match mt.parent {
                    None => break,
                    Some(p) => {
                        m = p;
                        d = mt.mp_dslot;
                        continue;
                    }
                }
            }
            let de = self.dentries.get(d as usize).ok_or(FsError::Io)?;
            if !de.used || de.dead || de.negative {
                return Err(FsError::NotFound);
            }
            let name = de.name.as_bytes();
            let start = end
                .checked_sub(name.len())
                .and_then(|e| e.checked_sub(1))
                .ok_or(FsError::NameTooLong)?;
            let dst = out.get_mut(start..end).ok_or(FsError::NameTooLong)?;
            if let Some((slash, rest)) = dst.split_first_mut() {
                *slash = b'/';
                rest.copy_from_slice(name);
            }
            end = start;
            d = de.parent;
        }
        if end == out.len() {
            let first = out.first_mut().ok_or(FsError::NameTooLong)?;
            *first = b'/';
            return Ok(1);
        }
        let len = out.len() - end;
        out.copy_within(end.., 0);
        Ok(len)
    }
}

impl Vfs {
    /// A create's call on directory `dir`, having dropped the negative
    /// dentries a new name makes stale.
    fn create_begin(&mut self, dir: PathRef, name: &[u8]) -> Result<Call, FsError> {
        self.hashed_dir(dir)?;
        let di = self.d_islot(dir.dslot)?;
        if self.inodes[di as usize].kind != InodeKind::Dir {
            return Err(FsError::NotDir);
        }
        let sb = self.sb_of(dir.mount);
        self.dcache_drop_neg_in_dir(sb, dir.dslot);
        self.dcache_evict_name(sb, dir.dslot, name);
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
        let ops = self.sb_ops(sb);
        let mut i = 0usize;
        while i < self.dentries.len() {
            let d = self.dentries[i];
            if d.used
                && d.negative
                && d.sb == sb
                && !d.is_root(i as u16)
                && ops.name_eq(d.name.as_bytes(), name)
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
        self.dcache_evict_name(sb, dir.dslot, name);
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
        self.dcache_evict_name(sb, nd.dslot, nname);
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
            return Err(FsError::XDev);
        }
        self.hashed_dir(od)?;
        self.hashed_dir(nd)?;
        if self.is_mountpoint(od, oname)
            || self.is_mountpoint(nd, nname)
            || src.mount != od.mount
            || tgt.is_some_and(|t| t.mount != nd.mount)
        {
            return Err(FsError::Busy);
        }
        let si = self.d_islot(src.dslot)?;
        // A directory never moves below itself: its dentry would become
        // its own ancestor.
        if self.inodes[si as usize].kind == InodeKind::Dir
            && (nd.dslot == src.dslot || self.below(nd.dslot, src.dslot))
        {
            return Err(FsError::Inval);
        }
        let ti = match tgt {
            Some(t) => Some(self.d_islot(t.dslot)?),
            None => None,
        };
        Ok((si, ti))
    }

    /// Commit a rename: a replaced inode loses its link, and the moved
    /// inode's dentry moves to its new parent and name, held or not, as
    /// Linux's `d_move` does, so a reference to it follows the move. The
    /// replaced name's dentry is unhashed when held and evicted otherwise.
    /// A moved inode the backend re-keyed moves in the hash, keeping that
    /// dentry.
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
            let sb = self.sb_of(od.mount);
            if tgt == Some(src) {
                // Two names of one file: the rename changed nothing.
                self.dcache_drop_name(sb, od.dslot, oname);
                self.dcache_drop_name(sb, nd.dslot, nname);
                return;
            }
            if let Some(t) = tgt {
                self.unlink_inode(t);
            }
            let sd = self
                .dcache_peek(sb, od.dslot, oname)
                .filter(|&d| self.d_islot(d) == Ok(src));
            if self.dcache_peek(sb, nd.dslot, nname) != sd {
                self.dcache_drop_name(sb, nd.dslot, nname);
            }
            if let Some(to) = moved {
                self.rekey_keep(src, to, sd);
            }
            if let Some(d) = sd
                && self.d_move(d, nd.dslot, nname).is_err()
            {
                self.dcache_drop_name(sb, od.dslot, oname);
            }
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
        self.rekey_keep(i, to, None);
    }

    /// [`Self::rekey_slot`], keeping dentry `keep`, which a rename moves
    /// to the inode's new name.
    fn rekey_keep(&mut self, i: u16, to: Key, keep: Option<u16>) {
        let sb = self.inodes[i as usize].sb;
        if self.inodes[i as usize].key == to {
            return;
        }
        if let Some(t) = self.hashed(sb, to)
            && t != i
        {
            self.unhash(t);
        }
        self.drop_dentries_except(i, keep);
        self.inodes[i as usize].key = to;
    }

    /// Move dentry `d` to parent `np` under `name`: the parent pin moves
    /// with it, from its old parent to the new one.
    fn d_move(&mut self, d: u16, np: u16, name: &[u8]) -> Result<(), FsError> {
        let nm = Name::from_bytes(name)?;
        self.dget(np)?;
        let e = &mut self.dentries[d as usize];
        let old = e.parent;
        e.parent = np;
        e.name = nm;
        e.clock = true;
        self.dput(old);
        Ok(())
    }

    /// `NotFound` for a directory whose dentry lost its name (an rmdir,
    /// or a rename over it, while it was held): nothing is made in it,
    /// and nothing is found in it.
    fn hashed_dir(&self, dir: PathRef) -> Result<(), FsError> {
        match self.dentries.get(dir.dslot as usize) {
            Some(d) if d.used && !d.dead => Ok(()),
            _ => Err(FsError::NotFound),
        }
    }

    /// A hard link's calls: on directory `nd` and on the regular file
    /// `src` names.
    fn link_begin(&mut self, src: PathRef, nd: PathRef) -> Result<(Call, Call), FsError> {
        self.hashed_dir(nd)?;
        let si = self.d_islot(src.dslot)?;
        if self.inodes[si as usize].kind != InodeKind::Reg {
            return Err(FsError::Perm);
        }
        if self.sb_of(src.mount) != self.sb_of(nd.mount) {
            return Err(FsError::XDev);
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
    base: Option<WalkBase>,
    /// The base's root, with the mounts on it followed: where an
    /// absolute walk or link starts, and where `..` stays put.
    root: PathRef,
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
    /// A walk of `path` from `base` (the namespace root for both its root
    /// and its working directory when none): an absolute path from its
    /// root, a relative one from its working directory; `follow_last`
    /// follows a symlink in the last component. The empty path is
    /// `NotFound`, as path_resolution(7) gives it.
    pub fn new(base: Option<WalkBase>, path: &[u8], follow_last: bool) -> Result<Self, FsError> {
        if path.is_empty() {
            return Err(FsError::NotFound);
        }
        if path.len() > MAX_PATH {
            return Err(FsError::NameTooLong);
        }
        let mut rem = [0u8; MAX_PATH];
        rem[..path.len()].copy_from_slice(path);
        Ok(Self {
            rem,
            rem_len: path.len(),
            base,
            root: PathRef { mount: 0, dslot: 0 },
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
            let (mut root, start) = match self.base {
                Some(b) => (b.root, b.cwd),
                None => {
                    let r = v.root()?;
                    (r, r)
                }
            };
            v.follow_mount(&mut root.mount, &mut root.dslot);
            self.root = root;
            self.mount = start.mount;
            self.dslot = start.dslot;
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
                self.mount = self.root.mount;
                self.dslot = self.root.dslot;
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
            // A component followed by `/`, the last one too, names a
            // directory: a link there is followed even where the last
            // component's would not be.
            let dir_only = j < self.rem_len;
            if name_is_dot(self.comp()) {
                shift_down(&mut self.rem, &mut self.rem_len, j);
                continue;
            }
            if name_is_dotdot(self.comp()) {
                v.dotdot(Some(self.root), &mut self.mount, &mut self.dslot);
                shift_down(&mut self.rem, &mut self.rem_len, j);
                continue;
            }
            let dir = PathRef {
                mount: self.mount,
                dslot: self.dslot,
            };
            v.hashed_dir(dir)?;
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
            let kind = v.inodes[islot as usize].kind;
            if kind == InodeKind::Lnk && (dir_only || self.follow_last) {
                if self.depth >= MAX_SYMLINK {
                    return Err(FsError::Loop);
                }
                self.depth += 1;
                let call = v.call(islot)?;
                return self.pend(v, Need::Readlink, call, dir, j);
            }
            if dir_only && kind != InodeKind::Dir {
                return Err(FsError::NotDir);
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
        base: Option<WalkBase>,
        path: &[u8],
        follow: bool,
    ) -> Result<PathRef, FsError> {
        let mut w = Walker::new(base, path, follow)?;
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

    /// Resolve `name`, one component, in the held directory `dir`.
    fn walk_in(&self, dir: PathRef, name: &[u8], follow: bool) -> Result<PathRef, FsError> {
        let base = WalkBase {
            root: dir,
            cwd: dir,
        };
        self.walk(Some(base), name, follow)
    }

    pub fn put_path(&self, p: PathRef) {
        self.step(|v| v.path_put(p));
    }

    /// A reference to the directory `path` names from `base`: `NotDir`
    /// when it names something else.
    pub fn dir_get(&self, base: Option<WalkBase>, path: &[u8]) -> Result<DirRef, FsError> {
        let p = self.walk(base, path, true)?;
        match self.with(|v| v.kind_of(p)) {
            Ok(InodeKind::Dir) => Ok(DirRef { at: p }),
            r => {
                self.put_path(p);
                Err(r.err().unwrap_or(FsError::NotDir))
            }
        }
    }

    /// A reference to the namespace root.
    pub fn dir_root(&self) -> Result<DirRef, FsError> {
        self.with(|v| v.dir_root())
    }

    /// Another reference to directory `at`, which a reference holds.
    pub fn dir_dup(&self, at: PathRef) -> Result<DirRef, FsError> {
        self.with(|v| v.dir_dup(at))
    }

    /// Drop a reference, running any release its put queued.
    pub fn dir_put(&self, r: DirRef) {
        self.step(|v| v.dir_put(r));
    }

    /// [`Vfs::dir_path`] under the lock.
    pub fn dir_path(
        &self,
        base: Option<WalkBase>,
        at: PathRef,
        out: &mut [u8],
    ) -> Result<usize, FsError> {
        self.with(|v| v.dir_path(base, at, out))
    }

    /// [`Vfs::dentry_refs`] under the lock.
    pub fn dentry_refs(&self, at: PathRef) -> u32 {
        self.with(|v| v.dentry_refs(at))
    }

    /// Resolve `path`'s parent to a held directory and name its last
    /// component, which is neither `.` nor `..`, and whether a `/`
    /// follows it: a name that must be a directory.
    fn walk_parent<'p>(
        &self,
        base: Option<WalkBase>,
        path: &'p [u8],
    ) -> Result<(PathRef, &'p [u8], bool), FsError> {
        let (parent, name, dir_only) = split_basename(path)?;
        if name_is_dot(name) || name_is_dotdot(name) {
            return Err(FsError::Inval);
        }
        let dir = self.walk(base, parent, true)?;
        match self.with(|v| v.kind_of(dir)) {
            Ok(InodeKind::Dir) => Ok((dir, name, dir_only)),
            r => {
                self.put_path(dir);
                Err(r.err().unwrap_or(FsError::NotDir))
            }
        }
    }

    /// `stat` (`follow`) or `lstat` of `path`.
    pub fn stat_path(
        &self,
        base: Option<WalkBase>,
        path: &[u8],
        follow: bool,
    ) -> Result<Stat, FsError> {
        let p = self.walk(base, path, follow)?;
        let r = self.with(|v| v.islot(p)).and_then(|i| self.stat_islot(i));
        self.put_path(p);
        r
    }

    /// Create `path` as a `kind` node; `Exists` when it is there.
    pub fn create(
        &self,
        base: Option<WalkBase>,
        path: &[u8],
        kind: InodeKind,
        mode: u16,
        target: Option<&[u8]>,
    ) -> Result<(), FsError> {
        let (dir, name, dir_only) = self.walk_parent(base, path)?;
        // A `/` after the new name is for a directory only.
        if dir_only && kind != InodeKind::Dir {
            self.put_path(dir);
            return Err(FsError::NotDir);
        }
        let r = self.with(|v| v.create_begin(dir, name)).and_then(|mut c| {
            let res = c.run(|o, cx, d| o.create(cx, d, name, kind, mode, target));
            self.step(|v| v.create_commit(dir, name, c, res))
        });
        self.put_path(dir);
        r
    }

    pub fn mkdir(&self, base: Option<WalkBase>, path: &[u8], mode: u32) -> Result<(), FsError> {
        self.create(base, path, InodeKind::Dir, file_mode(mode) | S_IFDIR, None)
    }

    pub fn symlink(
        &self,
        base: Option<WalkBase>,
        path: &[u8],
        target: &[u8],
    ) -> Result<(), FsError> {
        if target.is_empty() {
            return Err(FsError::Inval);
        }
        self.create(base, path, InodeKind::Lnk, S_IFLNK_MODE, Some(target))
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

    pub fn unlink(&self, base: Option<WalkBase>, path: &[u8]) -> Result<(), FsError> {
        self.remove(base, path, false)
    }

    pub fn rmdir(&self, base: Option<WalkBase>, path: &[u8]) -> Result<(), FsError> {
        self.remove(base, path, true)
    }

    fn remove(&self, base: Option<WalkBase>, path: &[u8], rmdir: bool) -> Result<(), FsError> {
        let (dir, name, dir_only) = self.walk_parent(base, path)?;
        let r = self.walk_in(dir, name, false).and_then(|victim| {
            if dir_only && self.with(|v| v.kind_of(victim)) != Ok(InodeKind::Dir) {
                self.put_path(victim);
                return Err(FsError::NotDir);
            }
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

    pub fn rename(&self, base: Option<WalkBase>, old: &[u8], new: &[u8]) -> Result<(), FsError> {
        let (od, oname, o_dir) = self.walk_parent(base, old)?;
        let r = self.walk_parent(base, new).and_then(|(nd, nname, n_dir)| {
            let r = self.rename_in((od, oname), (nd, nname), o_dir || n_dir);
            self.put_path(nd);
            r
        });
        self.put_path(od);
        r
    }

    fn rename_in(
        &self,
        o: (PathRef, &[u8]),
        n: (PathRef, &[u8]),
        dir_only: bool,
    ) -> Result<(), FsError> {
        let src = self.walk_in(o.0, o.1, false)?;
        // A `/` after either name moves a directory only.
        if dir_only && self.with(|v| v.kind_of(src)) != Ok(InodeKind::Dir) {
            self.put_path(src);
            return Err(FsError::NotDir);
        }
        let tgt = match self.walk_in(n.0, n.1, false) {
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
    pub fn link(&self, base: Option<WalkBase>, old: &[u8], new: &[u8]) -> Result<(), FsError> {
        let src = self.walk(base, old, true)?;
        let r = match self.with(|v| v.kind_of(src)) {
            Ok(InodeKind::Reg) => self
                .walk_parent(base, new)
                .and_then(|(nd, name, dir_only)| {
                    if dir_only {
                        self.put_path(nd);
                        return Err(FsError::NotDir);
                    }
                    let r = self
                        .with(|v| v.link_begin(src, nd))
                        .and_then(|(mut d, mut t)| {
                            let res = d.run2(&mut t, |o, cx, dir, tg| o.link(cx, dir, name, tg));
                            self.step(|v| v.link_commit(nd, name, (d, t), res))
                        });
                    self.put_path(nd);
                    r
                }),
            Ok(_) => Err(FsError::Perm),
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

/// A link's target `target` followed by the rest of the path, `rest`,
/// into `out`. A `/` that ended the path still ends it, so the name the
/// link leads to must be a directory.
fn join_path(target: &[u8], rest: &[u8], out: &mut [u8; MAX_PATH]) -> Result<usize, FsError> {
    let mut rest = rest;
    let slash = !rest.is_empty();
    while rest.first() == Some(&b'/') {
        rest = &rest[1..];
    }
    if rest.is_empty() && slash {
        rest = b"/";
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

/// `path`'s parent, its last component, and whether a `/` follows that
/// component (it names a directory). The empty path is `NotFound`, as
/// path_resolution(7) gives it.
pub fn split_basename(path: &[u8]) -> Result<(&[u8], &[u8], bool), FsError> {
    if path.is_empty() {
        return Err(FsError::NotFound);
    }
    let mut end = path.len();
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return Err(FsError::Inval);
    }
    let dir_only = end < path.len();
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
        None => Ok((b".", p, dir_only)),
        Some(0) => Ok((b"/", &p[1..], dir_only)),
        Some(s) => Ok((&p[..s], &p[s + 1..], dir_only)),
    }
}
