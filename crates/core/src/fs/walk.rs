use super::*;

/// How many times an unlink, rmdir or rename walks its names: once, and
/// again when a racing change left a name naming another file than the
/// walk found before the begin step (DESIGN §2.5's bounded retry).
const WALKS: usize = 2;

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

    /// Whether held path `p`, which a walk of `name` in `dir` resolved,
    /// still names that file: its dentry is live and still `name` in
    /// `dir`. A racing unlink kills the dentry, and a racing rename kills
    /// the one it replaces and moves the one it moves, to another parent
    /// or name; either way `name` may now be another file, or none. A
    /// mount's root, where the walk followed a mount on `name`, still
    /// names it: a mount point is neither unlinked nor renamed.
    fn still_named(&self, dir: PathRef, name: &[u8], p: PathRef) -> bool {
        let d = &self.dentries[p.dslot as usize];
        if !d.used || d.dead || d.negative {
            return false;
        }
        p.mount != dir.mount
            || (d.parent == dir.dslot
                && self
                    .sb_ops(self.sb_of(dir.mount))
                    .name_eq(d.name.as_bytes(), name))
    }

    /// An unlink's or rmdir's call on directory `dir`, holding `victim`,
    /// the held dentry `name` resolved to, which this step puts. `None`
    /// when that dentry no longer names `name` in `dir`
    /// ([`Self::still_named`]), after a racing change between the walk
    /// and this step: the caller walks again. This step closes only that
    /// window: the backend call after it runs with the VFS lock dropped,
    /// and acts on whatever `name` names then, until the directory locks
    /// ROADMAP §13.9's `renameat` lines build serialize the two.
    fn remove_begin(
        &mut self,
        dir: PathRef,
        name: &[u8],
        victim: PathRef,
        rmdir: bool,
    ) -> Result<Option<(Call, u16)>, FsError> {
        if !self.still_named(dir, name, victim) {
            self.path_put(victim);
            return Ok(None);
        }
        let r = self.remove_check(dir, name, victim, rmdir);
        // The victim's hold comes before the path's put, which can be its
        // last reference and queue its release.
        let held = r.and_then(|vi| self.ihold(vi).map(|()| vi));
        self.path_put(victim);
        let vi = held?;
        let di = match self.d_islot(dir.dslot) {
            Ok(di) => di,
            Err(e) => {
                self.iput(vi);
                return Err(e);
            }
        };
        let sb = self.sb_of(dir.mount);
        self.dcache_evict_name(sb, dir.dslot, name);
        match self.call(di) {
            Ok(c) => Ok(Some((c, vi))),
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
        let vi = self.d_islot(victim.dslot)?;
        // unlink(2) of a directory is EISDIR, and rmdir(2) of anything
        // else ENOTDIR, ahead of a mount point's EBUSY, as Linux's
        // `may_delete` runs before its mount point check.
        match (rmdir, self.inodes[vi as usize].kind == InodeKind::Dir) {
            (false, true) => return Err(FsError::IsDir),
            (true, false) => return Err(FsError::NotDir),
            _ => {}
        }
        if self.is_mountpoint(dir, name) || victim.mount != dir.mount {
            return Err(FsError::Busy);
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
    /// step puts. `None` when a racing change after the walks left either
    /// name naming another file than the walk found, as in
    /// `remove_begin`: `src` or `tgt` no longer named by `oname` or
    /// `nname`, or a file now at an `nname` the walk found free. The
    /// caller walks both again.
    fn rename_begin(
        &mut self,
        (od, oname): (PathRef, &[u8]),
        (nd, nname): (PathRef, &[u8]),
        src: PathRef,
        tgt: Option<PathRef>,
    ) -> Result<Option<RenameCall>, FsError> {
        let made = || {
            let sb = self.sb_of(nd.mount);
            self.dcache_peek(sb, nd.dslot, nname)
                .is_some_and(|d| !self.dentries[d as usize].negative)
        };
        let stale = !self.still_named(od, oname, src)
            || match tgt {
                Some(t) => !self.still_named(nd, nname, t),
                None => made(),
            };
        if stale {
            self.path_put(src);
            if let Some(t) = tgt {
                self.path_put(t);
            }
            return Ok(None);
        }
        // The inodes' holds come before the paths' puts, which can be
        // their last references, as in `remove_begin`.
        let held = self
            .rename_check((od, oname), (nd, nname), src, tgt)
            .and_then(|(si, ti)| {
                self.ihold(si)?;
                if let Some(t) = ti
                    && let Err(e) = self.ihold(t)
                {
                    self.iput(si);
                    return Err(e);
                }
                Ok((si, ti))
            });
        self.path_put(src);
        if let Some(t) = tgt {
            self.path_put(t);
        }
        let (si, ti) = held?;
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
            Ok((a, b)) => Ok(Some(RenameCall {
                a,
                b,
                src: si,
                tgt: ti,
                seen: RenameSeen {
                    src: self.inodes[si as usize].key,
                    tgt: ti.map(|t| self.inodes[t as usize].key),
                },
            })),
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
        // rename(2): a directory replaces only a directory, and anything
        // else only a non-directory; the backend refuses a directory
        // target that is not empty.
        if let Some(t) = ti
            && t != si
        {
            let dir = |i: u16| self.inodes[i as usize].kind == InodeKind::Dir;
            match (dir(si), dir(t)) {
                (false, true) => return Err(FsError::IsDir),
                (true, false) => return Err(FsError::NotDir),
                _ => {}
            }
        }
        // A mount point neither moves nor goes, checked after the kinds as
        // Linux's `vfs_rename` checks it.
        if self.is_mountpoint(od, oname)
            || self.is_mountpoint(nd, nname)
            || src.mount != od.mount
            || tgt.is_some_and(|t| t.mount != nd.mount)
        {
            return Err(FsError::Busy);
        }
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
        let RenameCall { a, b, src, tgt, .. } = rc;
        self.finish(a, true);
        self.finish(b, true);
        let r = res.map(|moved| {
            let sb = self.sb_of(od.mount);
            let same = tgt == Some(src);
            if same && moved.is_none() {
                // Two names of one file: the rename changed nothing.
                self.dcache_drop_name(sb, od.dslot, oname);
                self.dcache_drop_name(sb, nd.dslot, nname);
                return;
            }
            // The target is another inode, which loses its link; or, when
            // the target walk found the source itself through a name that
            // differs only in case and the backend moved the file to a new
            // key (FAT), it is the move below, with no link lost.
            if let Some(t) = tgt.filter(|_| !same) {
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
        // A file whose last name a racing unlink took after the walk gets
        // no new one, as Linux's `link` refuses it.
        if self.inodes[si as usize].nlink == 0 {
            return Err(FsError::NotFound);
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

    /// Resolve `path`'s parent to a held directory, and name its last
    /// component, what kind of component that is, and whether a `/`
    /// follows it: a name that must be a directory.
    fn walk_parent_last<'p>(
        &self,
        base: Option<WalkBase>,
        path: &'p [u8],
    ) -> Result<(PathRef, &'p [u8], bool, Last), FsError> {
        let (parent, name, dir_only) = split_basename(path)?;
        let dir = self.walk(base, parent, true)?;
        match self.with(|v| v.kind_of(dir)) {
            Ok(InodeKind::Dir) => Ok((dir, name, dir_only, Last::of(name))),
            r => {
                self.put_path(dir);
                Err(r.err().unwrap_or(FsError::NotDir))
            }
        }
    }

    /// [`Self::walk_parent_last`] for a change that makes or removes a
    /// name: a last component that is none (`.`, `..`, or the root) is
    /// `odd`'s error for it, after the parent's walk, whose errors come
    /// first, as Linux checks the type `filename_parentat` returns.
    fn walk_parent<'p>(
        &self,
        base: Option<WalkBase>,
        path: &'p [u8],
        odd: impl FnOnce(Last) -> FsError,
    ) -> Result<(PathRef, &'p [u8], bool), FsError> {
        let (dir, name, dir_only, last) = self.walk_parent_last(base, path)?;
        if last != Last::Name {
            self.put_path(dir);
            return Err(odd(last));
        }
        Ok((dir, name, dir_only))
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
        // `.`, `..` or the root names no new entry: EEXIST, as Linux's
        // `filename_create` gives it.
        let (dir, name, dir_only) = self.walk_parent(base, path, |_| FsError::Exists)?;
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

    /// Linux's errnos for a last component that is no name: unlink(2)
    /// is EISDIR for each; rmdir(2) is EINVAL for `.`, ENOTEMPTY for `..`,
    /// and EBUSY for the root.
    fn remove(&self, base: Option<WalkBase>, path: &[u8], rmdir: bool) -> Result<(), FsError> {
        let odd = |last| match (rmdir, last) {
            (false, _) => FsError::IsDir,
            (true, Last::Dot) => FsError::Inval,
            (true, Last::DotDot) => FsError::NotEmpty,
            (true, Last::Name | Last::Root) => FsError::Busy,
        };
        let (dir, name, dir_only) = self.walk_parent(base, path, odd)?;
        let r = self.remove_in(dir, name, dir_only, rmdir);
        self.put_path(dir);
        r
    }

    /// Remove `name` from `dir`. The walk and the begin step are two
    /// holds of the VFS lock, so a racing change can leave `name` naming
    /// another file between them; `name` is then walked again, up to
    /// [`WALKS`] times in all (DESIGN §2.5's bounded retry), and is
    /// `NotFound` after.
    fn remove_in(
        &self,
        dir: PathRef,
        name: &[u8],
        dir_only: bool,
        rmdir: bool,
    ) -> Result<(), FsError> {
        for _ in 0..WALKS {
            let victim = self.walk_in(dir, name, false)?;
            if dir_only && self.with(|v| v.kind_of(victim)) != Ok(InodeKind::Dir) {
                self.put_path(victim);
                return Err(FsError::NotDir);
            }
            (self.hooks.change_window)();
            let Some((mut c, vi)) = self.step(|v| v.remove_begin(dir, name, victim, rmdir))? else {
                continue;
            };
            let res = c.run(|o, cx, d| {
                if rmdir {
                    o.rmdir(cx, d, name)
                } else {
                    o.unlink(cx, d, name)
                }
            });
            return self.step(|v| v.remove_commit(dir, name, c, vi, res));
        }
        Err(FsError::NotFound)
    }

    /// rename(2). A last component that is no name, on either side, is
    /// EBUSY, after both parents' walks and the cross-filesystem EXDEV, as
    /// Linux's `do_renameat2` orders them.
    pub fn rename(&self, base: Option<WalkBase>, old: &[u8], new: &[u8]) -> Result<(), FsError> {
        let (od, oname, o_dir, o_last) = self.walk_parent_last(base, old)?;
        let r = self
            .walk_parent_last(base, new)
            .and_then(|(nd, nname, n_dir, n_last)| {
                let r = if o_last != Last::Name || n_last != Last::Name {
                    Err(if self.with(|v| v.sb_of(od.mount) != v.sb_of(nd.mount)) {
                        FsError::XDev
                    } else {
                        FsError::Busy
                    })
                } else {
                    self.rename_in((od, oname), (nd, nname), o_dir || n_dir)
                };
                self.put_path(nd);
                r
            });
        self.put_path(od);
        r
    }

    /// Move `o`'s name to `n`'s, walking both again when a racing change
    /// left either naming another file than the walks found, as
    /// [`Self::remove_in`] does: before the begin step, which sees it in
    /// the dentry cache, or before the backend call, whose backend sees it
    /// in its store ([`RenameSeen`]) and returns `Stale`.
    fn rename_in(
        &self,
        o: (PathRef, &[u8]),
        n: (PathRef, &[u8]),
        dir_only: bool,
    ) -> Result<(), FsError> {
        for _ in 0..WALKS {
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
            (self.hooks.change_window)();
            let Some(mut rc) = self.step(|v| v.rename_begin(o, n, src, tgt))? else {
                continue;
            };
            (self.hooks.rename_window)();
            let seen = rc.seen;
            let res = rc.a.run2(&mut rc.b, |ops, cx, x, y| {
                ops.rename(cx, x, o.1, y, n.1, seen)
            });
            match self.step(|v| v.rename_commit(o, n, rc, res)) {
                // A racing change after the begin step left a name naming
                // another file than the walks found, and the backend
                // changed nothing: walk both again.
                Err(FsError::Stale) => continue,
                r => return r,
            }
        }
        Err(FsError::NotFound)
    }

    /// Hard link `new` to the regular file `old` names.
    pub fn link(&self, base: Option<WalkBase>, old: &[u8], new: &[u8]) -> Result<(), FsError> {
        // `.`, `..` or the root as the new name: EEXIST, as for a create.
        let exists = |_| FsError::Exists;
        let src = self.walk(base, old, true)?;
        let r = match self.with(|v| v.kind_of(src)) {
            Ok(InodeKind::Reg) => {
                self.walk_parent(base, new, exists)
                    .and_then(|(nd, name, dir_only)| {
                        if dir_only {
                            self.put_path(nd);
                            return Err(FsError::NotDir);
                        }
                        (self.hooks.change_window)();
                        let r = self
                            .with(|v| v.link_begin(src, nd))
                            .and_then(|(mut d, mut t)| {
                                let res =
                                    d.run2(&mut t, |o, cx, dir, tg| o.link(cx, dir, name, tg));
                                self.step(|v| v.link_commit(nd, name, (d, t), res))
                            });
                        self.put_path(nd);
                        r
                    })
            }
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

/// What a path's last component is, as Linux's `LAST_*` types sort it:
/// a name, `.`, `..`, or none at all, in a path of slashes (the root).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Last {
    Name,
    Dot,
    DotDot,
    Root,
}

impl Last {
    /// The kind of last component `name` is, as [`split_basename`] gives
    /// it: empty for the root.
    fn of(name: &[u8]) -> Self {
        if name.is_empty() {
            Last::Root
        } else if name_is_dot(name) {
            Last::Dot
        } else if name_is_dotdot(name) {
            Last::DotDot
        } else {
            Last::Name
        }
    }
}

/// `path`'s parent, its last component, and whether a `/` follows that
/// component (it names a directory). A path of slashes has no last
/// component: its parent is `/` and its name empty. The empty path is
/// `NotFound`, as path_resolution(7) gives it.
pub fn split_basename(path: &[u8]) -> Result<(&[u8], &[u8], bool), FsError> {
    if path.is_empty() {
        return Err(FsError::NotFound);
    }
    let mut end = path.len();
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return Ok((b"/", b"", false));
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
