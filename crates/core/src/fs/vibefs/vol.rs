use super::*;

impl Vol {
    pub const fn new() -> Self {
        Self {
            nblocks: 0,
            generation: 0,
            inode_root: 0,
            alloc_root: 0,
            next_ino: 2,
            root_ino: ROOT_INO,
            uuid: [0; 16],
            label: [0; 32],
            flags: FLAG_DATA_CRC,
            dirty: false,
            now: 0,
            bitmap: [0; MAX_BLOCKS.div_ceil(8)],
            refc: [0; MAX_BLOCKS],
            txn: [0; MAX_BLOCKS.div_ceil(8)],
            inodes: [Inode::EMPTY; MAX_INODES],
            dents: [Dent::EMPTY; MAX_DENTS],
            snaps: [Snap::EMPTY; MAX_SNAPS],
            meta: [0; MAX_META],
            nmeta: 0,
            drop: [0; MAX_DROP],
            ndrop: 0,
            iobuf: [0; BLOCK],
            #[cfg(any(test, feature = "crash_plant"))]
            plant: Plant::None,
        }
    }

    /// Zero in place. Do not `*v = Vol::new()`: that builds a ~30KiB
    /// temporary on the kernel stack.
    pub fn clear(&mut self) {
        self.nblocks = 0;
        self.generation = 0;
        self.inode_root = 0;
        self.alloc_root = 0;
        self.next_ino = 2;
        self.root_ino = ROOT_INO;
        self.now = 0;
        self.uuid = [0; 16];
        self.label = [0; 32];
        self.flags = FLAG_DATA_CRC;
        self.dirty = false;
        self.bitmap.fill(0);
        self.refc.fill(0);
        self.txn.fill(0);
        let mut i = 0usize;
        while i < MAX_INODES {
            self.inodes[i] = Inode::EMPTY;
            i += 1;
        }
        i = 0;
        while i < MAX_DENTS {
            self.dents[i] = Dent::EMPTY;
            i += 1;
        }
        i = 0;
        while i < MAX_SNAPS {
            self.snaps[i] = Snap::EMPTY;
            i += 1;
        }
        i = 0;
        while i < MAX_META {
            self.meta[i] = 0;
            i += 1;
        }
        self.nmeta = 0;
        i = 0;
        while i < MAX_DROP {
            self.drop[i] = 0;
            i += 1;
        }
        self.ndrop = 0;
        self.iobuf.fill(0);
        #[cfg(any(test, feature = "crash_plant"))]
        {
            self.plant = Plant::None;
        }
    }

    /// Plant `p` in every later commit of this volume (test-only).
    #[cfg(any(test, feature = "crash_plant"))]
    pub fn set_plant(&mut self, p: Plant) {
        self.plant = p;
    }
}

impl Default for Vol {
    fn default() -> Self {
        Self::new()
    }
}

impl Vol {
    pub(super) fn mark_meta(&mut self, bno: u32) -> Result<(), Error> {
        if self.nmeta as usize >= MAX_META {
            return Err(Error::NoSpace);
        }
        self.meta[self.nmeta as usize] = bno;
        self.nmeta += 1;
        Ok(())
    }

    pub(super) fn pending_drop(&mut self, bno: u32) -> Result<(), Error> {
        if bno < 2 {
            return Ok(());
        }
        if bit_get(&self.txn, bno) {
            if self.refc[bno as usize] > 0 {
                self.refc[bno as usize] -= 1;
            }
            if self.refc[bno as usize] == 0 {
                bit_set(&mut self.bitmap, bno, false);
            }
            bit_set(&mut self.txn, bno, false);
            return Ok(());
        }
        if self.ndrop as usize >= MAX_DROP {
            return Err(Error::NoSpace);
        }
        self.drop[self.ndrop as usize] = bno;
        self.ndrop += 1;
        Ok(())
    }

    /// How many of the `len` blocks from `phys` [`Self::pending_drop`]
    /// would put on the drop list: those not new in this transaction.
    pub(super) fn drops_needed(&self, phys: u32, len: u32) -> usize {
        (0..len)
            .filter_map(|b| phys.checked_add(b))
            .filter(|&bno| bno >= 2 && !bit_get(&self.txn, bno))
            .count()
    }

    /// `NoSpace` unless the drop list has room for `n` more blocks.
    pub(super) fn drop_room(&self, n: usize) -> Result<(), Error> {
        if self.ndrop as usize + n > MAX_DROP {
            return Err(Error::NoSpace);
        }
        Ok(())
    }

    pub(super) fn alloc_block(&mut self) -> Result<u32, Error> {
        let mut i = 2u32;
        while i < self.nblocks {
            if self.refc[i as usize] == 0 {
                self.refc[i as usize] = 1;
                bit_set(&mut self.bitmap, i, true);
                bit_set(&mut self.txn, i, true);
                return Ok(i);
            }
            i += 1;
        }
        Err(Error::NoSpace)
    }

    pub(super) fn free_count(&self) -> u32 {
        let mut n = 0u32;
        let mut i = 2u32;
        while i < self.nblocks {
            if self.refc[i as usize] == 0 {
                n += 1;
            }
            i += 1;
        }
        n
    }

    pub(super) fn inode_slot(&self, ino: u32) -> Result<usize, Error> {
        let mut i = 0usize;
        while i < MAX_INODES {
            if self.inodes[i].used && self.inodes[i].ino == ino {
                return Ok(i);
            }
            i += 1;
        }
        Err(Error::NotFound)
    }

    pub(super) fn alloc_ino_slot(&mut self) -> Result<usize, Error> {
        let mut i = 0usize;
        while i < MAX_INODES {
            if !self.inodes[i].used {
                return Ok(i);
            }
            i += 1;
        }
        Err(Error::NoSpace)
    }

    pub(super) fn alloc_dent(&mut self) -> Result<usize, Error> {
        let mut i = 0usize;
        while i < MAX_DENTS {
            if !self.dents[i].used {
                return Ok(i);
            }
            i += 1;
        }
        Err(Error::NoSpace)
    }

    pub(super) fn find_dent(&self, parent: u32, name: &[u8]) -> Result<usize, Error> {
        let mut i = 0usize;
        while i < MAX_DENTS {
            if self.dents[i].used && self.dents[i].parent == parent && self.dents[i].name() == name
            {
                return Ok(i);
            }
            i += 1;
        }
        Err(Error::NotFound)
    }

    pub(super) fn dir_count(&self, parent: u32) -> u32 {
        let mut n = 0u32;
        let mut i = 0usize;
        while i < MAX_DENTS {
            if self.dents[i].used && self.dents[i].parent == parent {
                n += 1;
            }
            i += 1;
        }
        n
    }

    pub(super) fn node_from(&self, ino: u32, name: &[u8]) -> Result<Node, Error> {
        let s = self.inode_slot(ino)?;
        let r = &self.inodes[s];
        let mut n = Node::EMPTY;
        n.ino = r.ino;
        n.kind = kind_of(r.kind)?;
        n.size = r.size;
        n.mode = r.mode;
        n.nlink = r.nlink;
        let set = |t: u64| if t != 0 { t } else { r.mtime };
        n.atime = set(r.atime);
        n.mtime = r.mtime;
        n.ctime = set(r.ctime);
        let l = name.len().min(MAX_NAME);
        n.name[..l].copy_from_slice(&name[..l]);
        n.name_len = l as u8;
        Ok(n)
    }

    /// Stamp `ino`'s mtime and ctime with [`Vol::now`], the Unix seconds
    /// VIBEFS.md's inode record holds; with no clock, one past its old
    /// mtime, so a change still moves it.
    pub(super) fn bump_mtime(&mut self, ino: u32) {
        let now = self.now;
        if let Ok(s) = self.inode_slot(ino) {
            let t = if now != 0 {
                now
            } else {
                self.inodes[s].mtime.saturating_add(1)
            };
            self.inodes[s].mtime = t;
            self.inodes[s].ctime = t;
        }
        self.dirty = true;
    }

    /// Stamp `ino`'s ctime alone: a change to the inode, not its data.
    pub(super) fn bump_ctime(&mut self, ino: u32) {
        if self.now != 0
            && let Ok(s) = self.inode_slot(ino)
        {
            self.inodes[s].ctime = self.now;
        }
        self.dirty = true;
    }

    pub fn df(&self) -> (u64, u64, u32) {
        let tot = self.nblocks as u64 * BLOCK as u64;
        let free = self.free_count() as u64 * BLOCK as u64;
        (tot, free, self.nblocks)
    }
}
