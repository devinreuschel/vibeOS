use super::*;

impl Vfs {
    pub fn root(&self) -> Result<PathRef, FsError> {
        if !self.mounts[0].used {
            return Err(FsError::Io);
        }
        Ok(PathRef {
            mount: 0,
            dslot: self.mounts[0].root_dslot,
        })
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

    /// A reference to the volume instance of the superblock `p` is on;
    /// `Inval` for one whose backend gave none.
    pub fn volume_of(&self, p: PathRef) -> Result<Instance, FsError> {
        let sb = self.sb_of_mount(p.mount)?;
        self.sb_live(sb)?;
        self.supers[sb as usize].vol.clone().ok_or(FsError::Inval)
    }

    /// Whether a mounted superblock shows volume `vol`.
    pub fn shows_volume(&self, vol: &Instance) -> bool {
        self.supers
            .iter()
            .any(|s| s.live() && s.vol.as_ref().is_some_and(|v| same_instance(v, vol)))
    }
}

impl Vfs {
    pub(super) fn sb_of(&self, mount: u8) -> u8 {
        self.mounts[mount as usize].sb
    }

    /// The type of superblock `sb`, for an [`OpCx`].
    pub(super) fn fstype(&self, sb: u8) -> FsType {
        self.supers[sb as usize]
            .fs
            .map(|f| f.fstype())
            .unwrap_or_default()
    }

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
        vol: Option<Instance>,
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
            vol,
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
        while d < self.dentries.len() {
            let e = &self.dentries[d];
            if e.used && e.sb == sb && e.refs > self.expected_holds(d as u16) {
                return Err(FsError::Busy);
            }
            d += 1;
        }
        let mut n = 0usize;
        while n < self.inodes.len() {
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
        while d < self.dentries.len() {
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
        while n < self.inodes.len() {
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
        while i < self.supers.len() {
            let s = &self.supers[i];
            if s.live() && s.ops.is_some() {
                return self.sb_call(i as u8).ok();
            }
            i += 1;
        }
        None
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

    pub(super) fn sb_live(&self, sb: u8) -> Result<(), FsError> {
        match self.supers.get(sb as usize) {
            Some(s) if s.live() => Ok(()),
            _ => Err(FsError::Io),
        }
    }

    /// Whether `name` in `dir` is covered by a mount.
    pub(super) fn is_mountpoint(&self, dir: PathRef, name: &[u8]) -> bool {
        let sb = self.sb_of(dir.mount);
        match self.dcache_peek(sb, dir.dslot, name) {
            Some(ds) => self.mount_pins(ds) != 0,
            None => false,
        }
    }

    fn child_mount(&self, mount: u8, dslot: u16) -> Option<u8> {
        let mut i = 0u8;
        while i < self.mounts.len() as u8 {
            let m = &self.mounts[i as usize];
            if m.used && m.parent == Some(mount) && m.mp_dslot == dslot {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    pub(super) fn follow_mount(&self, mount: &mut u8, dslot: &mut u16) {
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

    /// Step from `(mount, dslot)` to its parent: nowhere at `root` (the
    /// walk's base root, with its mounts followed) or at the namespace
    /// root, and from a mount's root to the parent of its mountpoint.
    pub(super) fn dotdot(&self, root: Option<PathRef>, mount: &mut u8, dslot: &mut u16) {
        if root.is_some_and(|r| r.mount == *mount && r.dslot == *dslot) {
            return;
        }
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

    /// Mounts whose mountpoint is dentry `slot`.
    pub(super) fn mount_pins(&self, slot: u16) -> u16 {
        let mut n = 0u16;
        for m in self.mounts.iter() {
            if m.used && m.parent.is_some() && m.mp_dslot == slot {
                n = n.saturating_add(1);
            }
        }
        n
    }
}

impl<'l, L: Guarded<Vfs>> FileApi<'l, L> {
    /// Mount `fs` on directory `at` (C-FILEAPI `mount`'s core); see
    /// [`Vfs::super_of_dev`] for a device already mounted. A new
    /// superblock holds `vol`, the backend's volume instance; a shared
    /// one keeps its own.
    pub fn mount_fs(
        &self,
        base: Option<WalkBase>,
        at: &[u8],
        fs: &'static dyn FileSystem,
        dev: Option<u64>,
        ro: bool,
        vol: Option<Instance>,
    ) -> Result<Mounted, FsError> {
        let p = self.walk(base, at, true)?;
        let r = self.mount_at(Some(p), at, fs, dev, ro, vol);
        self.put_path(p);
        r
    }

    /// Mount `fs` as the root, holding `vol`.
    pub fn mount_root(
        &self,
        fs: &'static dyn FileSystem,
        dev: Option<u64>,
        ro: bool,
        vol: Option<Instance>,
    ) -> Result<Mounted, FsError> {
        self.mount_at(None, b"/", fs, dev, ro, vol)
    }

    fn mount_at(
        &self,
        at: Option<PathRef>,
        path: &[u8],
        fs: &'static dyn FileSystem,
        dev: Option<u64>,
        ro: bool,
        vol: Option<Instance>,
    ) -> Result<Mounted, FsError> {
        let (m, mut call) = match self.with(|v| v.mount_begin(at, fs, dev, ro, vol))? {
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
    pub fn umount(&self, base: Option<WalkBase>, at: &[u8]) -> Result<(), FsError> {
        let mut tries = 0usize;
        loop {
            let p = self.walk(base, at, true)?;
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
