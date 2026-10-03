use super::*;

impl Vfs {
    pub fn islot(&self, p: PathRef) -> Result<u16, FsError> {
        self.d_islot(p.dslot)
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

    /// Hashed inode slots of `sb` keyed `key`: one per file.
    pub fn inodes_with_key(&self, sb: u8, key: Key) -> usize {
        self.inodes
            .iter()
            .filter(|n| n.used && n.sb == sb && n.key == key && n.nlink != 0)
            .count()
    }

    /// Drop every negative dentry of `sb`, as a create made outside the
    /// dentry cache requires.
    pub fn drop_negatives(&mut self, sb: u8) {
        let mut i = 0usize;
        while i < self.dentries.len() {
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
}

impl Vfs {
    pub(super) fn d_islot(&self, dslot: u16) -> Result<u16, FsError> {
        let d = &self.dentries[dslot as usize];
        if !d.used || d.negative {
            return Err(FsError::NotFound);
        }
        Ok(d.islot)
    }

    pub(super) fn kind_of(&self, p: PathRef) -> Result<InodeKind, FsError> {
        Ok(self.inodes[self.d_islot(p.dslot)? as usize].kind)
    }

    /// Hold path `p`: its dentry and its mount, across an unlocked call.
    pub(super) fn path_get(&mut self, p: PathRef) -> Result<(), FsError> {
        let r = self.mounts[p.mount as usize]
            .refs
            .checked_add(1)
            .ok_or(FsError::NoSpace)?;
        self.dget(p.dslot)?;
        self.mounts[p.mount as usize].refs = r;
        Ok(())
    }

    /// Prepare a backend call on inode `islot`, which it counts.
    pub(super) fn call(&mut self, islot: u16) -> Result<Call, FsError> {
        let sb = self.inodes[islot as usize].sb;
        let ops = self.supers[sb as usize].ops.unwrap_or(&NoOps);
        self.ihold(islot)?;
        Ok(self.raw_call(islot, ops))
    }

    /// A call on inode `islot` that takes no count: a release, whose slot
    /// its release state keeps.
    pub(super) fn raw_call(&self, islot: u16, ops: &'static dyn InodeOps) -> Call {
        let n = self.inodes[islot as usize].fresh();
        let sb = n.sb;
        Call {
            ops,
            sb,
            fstype: self.fstype(sb),
            private: self.supers[sb as usize].private,
            now: self.now,
            vol: self.supers[sb as usize].vol.clone(),
            before: n,
            ino: n,
        }
    }

    /// Commit call `c`: write back what the op changed when `merge`, and
    /// drop the call's count.
    pub(super) fn finish(&mut self, c: Call, merge: bool) {
        self.finish_with(c, merge, |_| ());
    }

    /// A call on a superblock, which counts it `busy`.
    pub(super) fn sb_call(&mut self, sb: u8) -> Result<SbCall, FsError> {
        let s = &mut self.supers[sb as usize];
        s.busy = s.busy.checked_add(1).ok_or(FsError::NoSpace)?;
        let (fs, ops, vol) = (s.fs, s.ops, s.vol.clone());
        Ok(SbCall {
            fs,
            ops,
            sb,
            fstype: self.fstype(sb),
            private: self.supers[sb as usize].private,
            now: self.now,
            vol,
        })
    }

    /// A superblock hook returned. After the last one of a superblock
    /// whose last mount is gone (`release`), its slot is free, and its
    /// count on its volume instance comes back for the caller to drop
    /// with the lock released (DESIGN §2.11 rule 6).
    #[must_use = "the volume instance is dropped after the VFS lock"]
    pub(super) fn sb_idle(&mut self, sb: u8, release: bool) -> Option<Instance> {
        let s = &mut self.supers[sb as usize];
        s.busy = s.busy.saturating_sub(1);
        if release && s.busy == 0 {
            debug_assert!(s.refs == 0, "superblock released while held");
            let vol = s.vol.take();
            *s = Super::EMPTY;
            return vol;
        }
        None
    }

    /// The next unhashed inode whose release is queued and that nothing
    /// holds, as a call to its backend's `evict`; one on a superblock with
    /// no ops is released here. One held again after its release was
    /// queued waits for its last put.
    pub(super) fn take_release(&mut self) -> Option<Call> {
        let mut i = 0usize;
        while i < self.inodes.len() {
            let n = &self.inodes[i];
            if n.used && n.rel == Rel::Queued && n.refs == 0 {
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
    pub(super) fn release_done(&mut self, c: Call, ok: bool) {
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
}

impl Vfs {
    /// A counted reference to the inode `info` describes. A used inode
    /// of `sb` with the same key and links is a hit, and its cached state
    /// wins over `info`: it is the authoritative inode, whose words a
    /// lookup never overwrites. An unlinked one (`nlink == 0`) is out of
    /// the hash, so a new file that reuses its key gets a slot of its
    /// own. A new slot counts one reference to its superblock.
    pub(super) fn iget_info(&mut self, sb: u8, info: &InodeInfo) -> Result<u16, FsError> {
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
            words: Some(&self.words[slot as usize]),
        };
        let w = &self.words[slot as usize];
        w.set_size(info.size);
        w.set_nlink(info.nlink);
        w.set_private(info.private);
        Ok(slot)
    }

    /// Empty inode slot `i`, keeping its generation, and drop its count
    /// on its superblock.
    pub(super) fn inode_clear(&mut self, i: usize) {
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
    pub(super) fn slot_of(&self, h: InodeHandle) -> Result<usize, FsError> {
        let i = h.slot as usize;
        match self.inodes.get(i) {
            Some(n) if n.used && n.r#gen == h.r#gen => Ok(i),
            _ => Err(FsError::Badf),
        }
    }

    /// Take inode `i` out of the hash: no links, and the dentries naming
    /// it dropped. An unreferenced one is cleared without calling its
    /// backend. True when it is still referenced.
    pub(super) fn unhash(&mut self, i: u16) -> bool {
        self.drop_dentries_of(i);
        let n = &mut self.inodes[i as usize];
        n.set_nlink(0);
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
    pub(super) fn iput(&mut self, islot: u16) {
        let i = islot as usize;
        if i >= self.inodes.len() || !self.inodes[i].used || self.inodes[i].refs == 0 {
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
        while i < self.inodes.len() {
            if !self.inodes[i].used {
                return Ok(i as u16);
            }
            i += 1;
        }
        let mut n = 0usize;
        while n < self.inodes.len() * 2 {
            let s = self.ihand as usize % self.inodes.len();
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
        while s < self.inodes.len() {
            if self.inode_evict(s as u16) {
                return Ok(s as u16);
            }
            s += 1;
        }
        self.dentry_shrink()
    }

    /// Every inode is held, most by unheld dentries: shrink the dentry
    /// cache with its clock hand, as Linux's shrinker does under inode
    /// pressure, until a dentry it evicts releases a free inode, and
    /// return that inode's slot.
    fn dentry_shrink(&mut self) -> Result<u16, FsError> {
        let len = self.dentries.len();
        let mut n = 0usize;
        while n < len * 2 {
            let s = self.dhand as usize % len;
            self.dhand = self.dhand.wrapping_add(1);
            n += 1;
            let d = &mut self.dentries[s];
            if !d.used || d.refs != 0 {
                continue;
            }
            if d.clock {
                d.clock = false;
                continue;
            }
            if let Some(i) = self.dentry_kill(s as u16) {
                return Ok(i);
            }
        }
        let mut s = 0usize;
        while s < len {
            if let Some(i) = self.dentry_kill(s as u16) {
                return Ok(i);
            }
            s += 1;
        }
        // A dead dentry that a put evicted may have released one.
        let mut s = 0usize;
        while s < self.inodes.len() {
            if self.inode_evict(s as u16) {
                return Ok(s as u16);
            }
            s += 1;
        }
        Err(FsError::NoSpace)
    }

    /// Evict unheld dentry `slot`, then each ancestor that leaves
    /// unheld, as Linux's shrinker kills a parent whose last child it
    /// freed, until one of them releases its inode: that inode is
    /// emptied and its slot returned. An unlinked inode waits for its
    /// release instead.
    fn dentry_kill(&mut self, slot: u16) -> Option<u16> {
        let mut cur = slot;
        let mut n = 0usize;
        while n < self.dentries.len() {
            let d = self.dentries[cur as usize];
            if !d.used || d.refs != 0 {
                return None;
            }
            self.dentry_evict(cur);
            if !d.negative && self.inode_evict(d.islot) {
                return Some(d.islot);
            }
            if d.is_root(cur) {
                return None;
            }
            cur = d.parent;
            n += 1;
        }
        None
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

    /// The hashed inode of `sb` keyed `key`.
    pub(super) fn hashed(&self, sb: u8, key: Key) -> Option<u16> {
        let mut i = 0usize;
        while i < self.inodes.len() {
            let n = &self.inodes[i];
            if n.used && n.sb == sb && n.key == key && n.nlink != 0 {
                return Some(i as u16);
            }
            i += 1;
        }
        None
    }

    /// Count one more reference to used inode `islot`.
    pub(super) fn ihold(&mut self, islot: u16) -> Result<(), FsError> {
        let n = &mut self.inodes[islot as usize];
        n.refs = n.refs.checked_add(1).ok_or(FsError::NoSpace)?;
        Ok(())
    }

    /// Drop the dentries naming inode `i` that nothing but their own
    /// descendants holds, releasing their counts on it without a put;
    /// the held ones lose their names.
    pub(super) fn drop_dentries_of(&mut self, i: u16) {
        self.drop_dentries_except(i, None);
    }

    /// [`Self::drop_dentries_of`], but for dentry `keep`.
    pub(super) fn drop_dentries_except(&mut self, i: u16, keep: Option<u16>) {
        let mut d = 0usize;
        while d < self.dentries.len() {
            let e = self.dentries[d];
            if e.used
                && !e.negative
                && e.islot == i
                && !e.is_root(d as u16)
                && keep != Some(d as u16)
            {
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

    pub(super) fn dentry_force_alloc(&mut self) -> Result<u16, FsError> {
        let mut i = 0usize;
        while i < self.dentries.len() {
            if !self.dentries[i].used {
                return Ok(i as u16);
            }
            i += 1;
        }
        let mut n = 0usize;
        while n < self.dentries.len() * 2 {
            let s = self.dhand as usize % self.dentries.len();
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
        while s < self.dentries.len() {
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
    pub(super) fn dentry_evict(&mut self, slot: u16) {
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
            while i < self.dentries.len() {
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
    pub(super) fn below(&self, slot: u16, top: u16) -> bool {
        let mut cur = slot;
        let mut n = 0usize;
        while n < self.dentries.len() {
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
    pub(super) fn dget(&mut self, slot: u16) -> Result<(), FsError> {
        let d = &mut self.dentries[slot as usize];
        d.refs = d.refs.checked_add(1).ok_or(FsError::NoSpace)?;
        Ok(())
    }

    /// Drop one holder of dentry `slot`. A live dentry stays for clock
    /// eviction to reclaim; a dead one is evicted at its last put.
    pub(super) fn dput(&mut self, slot: u16) {
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
        while i < self.dentries.len() {
            let d = &self.dentries[i];
            if d.used && d.parent == slot && !d.is_root(i as u16) {
                n = n.saturating_add(1);
            }
            i += 1;
        }
        n
    }

    /// The holds dentry `slot` has with no explicit hold: its children,
    /// the mounts on it, and the superblock's hold on a root dentry.
    pub(super) fn expected_holds(&self, slot: u16) -> u16 {
        let root = u16::from(self.dentries[slot as usize].is_root(slot));
        self.child_count(slot)
            .saturating_add(self.mount_pins(slot))
            .saturating_add(root)
    }

    /// Positive dentries that name inode `islot`.
    pub(super) fn naming_dentries(&self, islot: u16) -> u16 {
        let mut n = 0u16;
        for d in self.dentries.iter() {
            if d.used && !d.negative && d.islot == islot {
                n = n.saturating_add(1);
            }
        }
        n
    }

    /// The hashed dentry `name` names in `parent`, its names compared
    /// through the superblock's [`InodeOps::name_eq`].
    pub(super) fn dcache_peek(&self, sb: u8, parent: u16, name: &[u8]) -> Option<u16> {
        let ops = self.sb_ops(sb);
        let mut i = 0usize;
        while i < self.dentries.len() {
            let d = &self.dentries[i];
            if d.used
                && !d.dead
                && d.sb == sb
                && d.parent == parent
                && !d.is_root(i as u16)
                && ops.name_eq(d.name.as_bytes(), name)
            {
                return Some(i as u16);
            }
            i += 1;
        }
        None
    }

    /// The ops of superblock `sb`: [`NoOps`] when it has none.
    pub(super) fn sb_ops(&self, sb: u8) -> &'static dyn InodeOps {
        self.supers
            .get(sb as usize)
            .and_then(|s| s.ops)
            .unwrap_or(&NoOps)
    }

    pub(super) fn dcache_find(&mut self, sb: u8, parent: u16, name: &[u8]) -> Option<u16> {
        let ds = self.dcache_peek(sb, parent, name)?;
        self.dentries[ds as usize].clock = true;
        Some(ds)
    }

    /// Cache `name` in `parent`, positive when `islot` is given. The
    /// parent is held before the allocation, so it cannot be the slot the
    /// allocation evicts.
    pub(super) fn dcache_insert(
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
    pub(super) fn dcache_drop_name(&mut self, sb: u8, parent: u16, name: &[u8]) {
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

    /// Forget `name` in `parent` when nothing but its descendants holds
    /// it, as a namespace change does before its backend call: a held
    /// dentry keeps its name until the change succeeds.
    pub(super) fn dcache_evict_name(&mut self, sb: u8, parent: u16, name: &[u8]) {
        let Some(ds) = self.dcache_peek(sb, parent, name) else {
            return;
        };
        let d = &self.dentries[ds as usize];
        if d.refs != 0 && d.refs == self.child_count(ds) {
            self.dentry_prune(ds);
        }
        self.dentry_evict(ds);
    }

    pub(super) fn dcache_drop_neg_in_dir(&mut self, sb: u8, parent: u16) {
        let mut i = 0usize;
        while i < self.dentries.len() {
            let d = &self.dentries[i];
            if d.used && d.sb == sb && d.parent == parent && d.negative && !d.is_root(i as u16) {
                self.dentry_evict(i as u16);
            }
            i += 1;
        }
    }
}
