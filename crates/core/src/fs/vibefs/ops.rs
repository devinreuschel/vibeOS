use super::*;

impl Vol {
    pub fn lookup<D: Disk>(&mut self, _d: &mut D, dir: u32, name: &[u8]) -> Result<Node, Error> {
        name_ok(name)?;
        let ds = self.inode_slot(dir)?;
        if self.inodes[ds].kind != KIND_DIR {
            return Err(Error::NotDir);
        }
        let e = self.find_dent(dir, name)?;
        self.node_from(self.dents[e].ino, name)
    }

    pub fn walk<D: Disk>(&mut self, d: &mut D, path: &[u8]) -> Result<Node, Error> {
        let mut p = path;
        if p.is_empty() || p == b"/" {
            return self.node_from(self.root_ino, b"/");
        }
        if p[0] == b'/' {
            p = &p[1..];
        }
        let mut dir = self.root_ino;
        loop {
            let mut i = 0usize;
            while i < p.len() && p[i] != b'/' {
                i += 1;
            }
            let comp = &p[..i];
            if comp.is_empty() {
                return Err(Error::Inval);
            }
            let n = self.lookup(d, dir, comp)?;
            if i == p.len() {
                return Ok(n);
            }
            if n.kind != InodeKind::Dir {
                return Err(Error::NotDir);
            }
            dir = n.ino;
            p = &p[i + 1..];
            if p.is_empty() {
                return Ok(n);
            }
        }
    }

    pub fn readdir<D: Disk>(
        &mut self,
        _d: &mut D,
        dir: u32,
        cookie: u64,
        out: &mut Node,
    ) -> Result<Option<u64>, Error> {
        let ds = self.inode_slot(dir)?;
        if self.inodes[ds].kind != KIND_DIR {
            return Err(Error::NotDir);
        }
        let start = cookie as usize;
        let mut i = start;
        while i < MAX_DENTS {
            if self.dents[i].used && self.dents[i].parent == dir {
                *out = self.node_from(self.dents[i].ino, self.dents[i].name())?;
                return Ok(Some((i as u64) + 1));
            }
            i += 1;
        }
        Ok(None)
    }

    pub fn create<D: Disk>(
        &mut self,
        _d: &mut D,
        dir: u32,
        name: &[u8],
        kind: InodeKind,
        mode: u16,
        target: Option<&[u8]>,
    ) -> Result<Node, Error> {
        name_ok(name)?;
        let ds = self.inode_slot(dir)?;
        if self.inodes[ds].kind != KIND_DIR {
            return Err(Error::NotDir);
        }
        if self.find_dent(dir, name).is_ok() {
            return Err(Error::Exists);
        }
        let link_target = if kind == InodeKind::Lnk {
            let t = target.ok_or(Error::Inval)?;
            if t.is_empty() || t.len() > INLINE {
                return Err(Error::Inval);
            }
            Some(t)
        } else {
            None
        };
        if kind == InodeKind::Chr || kind == InodeKind::Blk {
            return Err(Error::Perm);
        }
        let slot = self.alloc_ino_slot()?;
        let de = self.alloc_dent()?;
        let ino = self.next_ino;
        if ino == 0 {
            return Err(Error::NoSpace);
        }
        self.next_ino = self.next_ino.saturating_add(1);
        let mut rec = Inode::EMPTY;
        rec.used = true;
        rec.ino = ino;
        rec.kind = kind_to(kind);
        rec.mode = mode;
        rec.nlink = 1;
        rec.atime = self.now;
        rec.mtime = self.now;
        rec.ctime = self.now;
        rec.flags = if kind != InodeKind::Dir { F_INLINE } else { 0 };
        if let Some(t) = link_target {
            rec.inline_len = t.len() as u8;
            rec.size = t.len() as u64;
            rec.inline_data[..t.len()].copy_from_slice(t);
        }
        self.inodes[slot] = rec;
        self.dents[de] = Dent {
            used: true,
            parent: dir,
            ino,
            kind: rec.kind,
            nlen: name.len() as u8,
            name: {
                let mut n = [0u8; MAX_NAME];
                n[..name.len()].copy_from_slice(name);
                n
            },
        };
        self.bump_mtime(dir);
        self.node_from(ino, name)
    }

    pub fn unlink<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        name: &[u8],
        rmdir: bool,
    ) -> Result<(), Error> {
        name_ok(name)?;
        let ds = self.inode_slot(dir)?;
        if self.inodes[ds].kind != KIND_DIR {
            return Err(Error::NotDir);
        }
        let e = self.find_dent(dir, name)?;
        let ino = self.dents[e].ino;
        let is = self.inode_slot(ino)?;
        let k = self.inodes[is].kind;
        if rmdir {
            if k != KIND_DIR {
                return Err(Error::NotDir);
            }
            if self.dir_count(ino) != 0 {
                return Err(Error::NotEmpty);
            }
        } else if k == KIND_DIR {
            return Err(Error::IsDir);
        }
        // The one step that can fail goes first, so a refused unlink
        // leaves the name and the link count as they were.
        let nlink = self.inodes[is].nlink.saturating_sub(1);
        if nlink == 0 {
            self.free_inode_data(d, ino)?;
        }
        self.dents[e] = Dent::EMPTY;
        self.inodes[is].nlink = nlink;
        if nlink == 0 {
            self.inodes[is] = Inode::EMPTY;
        } else {
            self.bump_ctime(ino);
        }
        self.bump_mtime(dir);
        Ok(())
    }

    /// Release every data block of inode `ino`. The drop list's room is
    /// checked first, so a failure queues nothing.
    fn free_inode_data<D: Disk>(&mut self, _d: &mut D, ino: u32) -> Result<(), Error> {
        let is = self.inode_slot(ino)?;
        let n_ext = (self.inodes[is].n_ext as usize).min(MAX_EXT);
        let mut ex = [Extent::EMPTY; MAX_EXT];
        ex.copy_from_slice(&self.inodes[is].extents);
        let mut drops = 0usize;
        for e in &ex[..n_ext] {
            drops += self.drops_needed(e.phys, e.len);
        }
        self.drop_room(drops)?;
        let mut i = 0usize;
        while i < n_ext {
            let mut b = 0u32;
            while b < ex[i].len {
                self.pending_drop(ex[i].phys + b)?;
                b += 1;
            }
            i += 1;
        }
        Ok(())
    }

    pub fn rename<D: Disk>(
        &mut self,
        d: &mut D,
        src_dir: u32,
        src_name: &[u8],
        dst_dir: u32,
        dst_name: &[u8],
    ) -> Result<(), Error> {
        name_ok(src_name)?;
        name_ok(dst_name)?;
        let e = self.find_dent(src_dir, src_name)?;
        if src_dir == dst_dir && src_name == dst_name {
            return Ok(());
        }
        // Validate before anything changes, so a refused rename loses no
        // destination.
        let ds = self.inode_slot(dst_dir)?;
        if self.inodes[ds].kind != KIND_DIR {
            return Err(Error::NotDir);
        }
        let src_ino = self.dents[e].ino;
        let ss = self.inode_slot(src_ino)?;
        if self.inodes[ss].kind == KIND_DIR && self.in_subtree(src_ino, dst_dir)? {
            return Err(Error::Inval);
        }
        // The destination goes as rename(2) has it: a directory only for
        // a directory and only when empty (an rmdir's checks), anything
        // else only for a non-directory (an unlink's).
        if self.find_dent(dst_dir, dst_name).is_ok() {
            self.unlink(d, dst_dir, dst_name, self.inodes[ss].kind == KIND_DIR)?;
        }
        let mut nm = [0u8; MAX_NAME];
        nm[..dst_name.len()].copy_from_slice(dst_name);
        self.dents[e].parent = dst_dir;
        self.dents[e].nlen = dst_name.len() as u8;
        self.dents[e].name = nm;
        self.bump_mtime(src_dir);
        self.bump_mtime(dst_dir);
        self.bump_ctime(src_ino);
        Ok(())
    }

    /// Whether directory `dir` is `top` or lies below it, walking up through
    /// the one dirent that names each directory to the root.
    fn in_subtree(&self, top: u32, dir: u32) -> Result<bool, Error> {
        let mut cur = dir;
        let mut steps = 0usize;
        loop {
            if cur == top {
                return Ok(true);
            }
            if cur == self.root_ino {
                return Ok(false);
            }
            if steps >= MAX_INODES {
                return Err(Error::Corrupt);
            }
            let up = self
                .dents
                .iter()
                .find(|de| de.used && de.ino == cur)
                .ok_or(Error::Corrupt)?;
            cur = up.parent;
            steps += 1;
        }
    }

    pub(super) fn extent_crc<D: Disk>(
        &mut self,
        d: &mut D,
        phys: u32,
        len: u32,
    ) -> Result<u32, Error> {
        let mut crc = 0xFFFF_FFFFu32;
        let mut b = 0u32;
        while b < len {
            d.read_block(phys + b, &mut self.iobuf)?;
            let mut i = 0usize;
            while i < BLOCK {
                crc ^= self.iobuf[i] as u32;
                let mut k = 0;
                while k < 8 {
                    if crc & 1 != 0 {
                        crc = (crc >> 1) ^ 0xEDB8_8320;
                    } else {
                        crc >>= 1;
                    }
                    k += 1;
                }
                i += 1;
            }
            b += 1;
        }
        Ok(!crc)
    }

    pub(super) fn check_extent<D: Disk>(&mut self, d: &mut D, e: Extent) -> Result<(), Error> {
        if e.len == 0 {
            return Err(Error::Corrupt);
        }
        let got = self.extent_crc(d, e.phys, e.len)?;
        if got != e.crc {
            return Err(Error::Corrupt);
        }
        Ok(())
    }

    fn write_extent_bytes<D: Disk>(
        &mut self,
        d: &mut D,
        phys: u32,
        len: u32,
        data: &[u8],
    ) -> Result<u32, Error> {
        let mut off = 0usize;
        let mut b = 0u32;
        while b < len {
            self.iobuf.fill(0);
            let n = (data.len() - off).min(BLOCK);
            if n > 0 {
                self.iobuf[..n].copy_from_slice(&data[off..off + n]);
                off += n;
            }
            d.write_block(phys + b, &self.iobuf)?;
            b += 1;
        }
        self.extent_crc(d, phys, len)
    }

    fn spill_inline<D: Disk>(&mut self, d: &mut D, ino: u32) -> Result<(), Error> {
        let is = self.inode_slot(ino)?;
        if self.inodes[is].flags & F_INLINE == 0 {
            return Ok(());
        }
        let size = self.inodes[is].size as usize;
        // As in `read`: a size past the inline bytes is corruption.
        if size > INLINE {
            return Err(Error::Corrupt);
        }
        let mut tmp = [0u8; INLINE];
        tmp.copy_from_slice(&self.inodes[is].inline_data);
        if size == 0 {
            self.inodes[is].flags &= !F_INLINE;
            self.inodes[is].inline_len = 0;
            self.inodes[is].n_ext = 0;
            return Ok(());
        }
        let nb = size.div_ceil(BLOCK) as u32;
        if nb as usize > MAX_EXT {
            return Err(Error::NoSpace);
        }
        let mut ex = [Extent::EMPTY; MAX_EXT];
        let mut p = 0u32;
        while p < nb {
            let r = self.spill_block(d, &tmp[..size], p);
            match r {
                Ok(e) => ex[p as usize] = e,
                Err(e) => {
                    // Each block is new in this transaction, so
                    // `pending_drop` frees it and cannot fail.
                    let mut q = 0usize;
                    let mut rel = Ok(());
                    while q < p as usize {
                        rel = rel.and(self.pending_drop(ex[q].phys));
                        q += 1;
                    }
                    return rel.and(Err(e));
                }
            }
            p += 1;
        }
        let is = self.inode_slot(ino)?;
        self.inodes[is].extents = ex;
        self.inodes[is].flags &= !F_INLINE;
        self.inodes[is].inline_len = 0;
        self.inodes[is].n_ext = nb as u8;
        Ok(())
    }

    /// Write block `p` of the inline bytes `data` to a new block; its
    /// extent. A write error releases the block.
    fn spill_block<D: Disk>(&mut self, d: &mut D, data: &[u8], p: u32) -> Result<Extent, Error> {
        let start = (p as usize) * BLOCK;
        let chunk = &data[start..data.len().min(start + BLOCK)];
        let phys = self.alloc_block()?;
        match self.write_extent_bytes(d, phys, 1, chunk) {
            Ok(crc) => Ok(Extent {
                log: p,
                phys,
                len: 1,
                crc,
            }),
            // `phys` is new in this transaction: `pending_drop` frees it
            // and cannot fail.
            Err(e) => self.pending_drop(phys).and(Err(e)),
        }
    }

    /// The size of inode `ino`, which `SEEK_END` and `O_APPEND` read.
    pub fn file_size(&self, ino: u32) -> Result<u64, Error> {
        let s = self.inode_slot(ino)?;
        Ok(self.inodes[s].size)
    }

    /// The extent index and physical block of file block `file_blk`, or
    /// `None` for a hole. Extent ends are compared in `u64`; an extent
    /// whose physical block overflows is `Corrupt`.
    fn map_block(&self, ino_slot: usize, file_blk: u32) -> Result<Option<(usize, u32)>, Error> {
        let r = self.inodes.get(ino_slot).ok_or(Error::Corrupt)?;
        let fb = u64::from(file_blk);
        for (i, e) in r.extents.iter().take(r.n_ext as usize).enumerate() {
            let start = u64::from(e.log);
            let end = start.checked_add(u64::from(e.len)).ok_or(Error::Corrupt)?;
            if fb >= start && fb < end {
                let phys = e.phys.checked_add(file_blk - e.log).ok_or(Error::Corrupt)?;
                return Ok(Some((i, phys)));
            }
        }
        Ok(None)
    }

    /// File block index and offset within it of byte `pos`.
    fn block_of(pos: u64) -> Result<(u32, usize), Error> {
        let fblk = u32::try_from(pos / BLOCK as u64).map_err(|_| Error::FileTooBig)?;
        Ok((fblk, (pos % BLOCK as u64) as usize))
    }

    pub fn read<D: Disk>(
        &mut self,
        d: &mut D,
        ino: u32,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        let is = self.inode_slot(ino)?;
        if self.inodes[is].kind == KIND_DIR {
            return Err(Error::IsDir);
        }
        let size = self.inodes[is].size;
        if off >= size || buf.is_empty() {
            return Ok(0);
        }
        let want = core::cmp::min(buf.len() as u64, size - off) as usize;
        if self.inodes[is].flags & F_INLINE != 0 {
            // An inline file's bytes are `inline_data`'s: a size past them
            // is the image's corruption (fsck's `inline`).
            let s = off as usize;
            let end = s.checked_add(want).ok_or(Error::Corrupt)?;
            let src = self.inodes[is]
                .inline_data
                .get(s..end)
                .ok_or(Error::Corrupt)?;
            buf[..want].copy_from_slice(src);
            return Ok(want);
        }
        // Every extent the range touches verifies before anything is
        // copied, so a `Corrupt` leaves `buf` untouched.
        let (first, _) = Self::block_of(off)?;
        let (last, _) = Self::block_of(off + (want as u64 - 1))?;
        let n_ext = (self.inodes[is].n_ext as usize).min(MAX_EXT);
        let mut i = 0usize;
        while i < n_ext {
            let e = self.inodes[is].extents[i];
            let end = u64::from(e.log)
                .checked_add(u64::from(e.len))
                .ok_or(Error::Corrupt)?;
            if e.log <= last && end > u64::from(first) {
                self.check_extent(d, e)?;
            }
            i += 1;
        }
        let mut done = 0usize;
        while done < want {
            let pos = off.checked_add(done as u64).ok_or(Error::FileTooBig)?;
            let (fblk, pin) = Self::block_of(pos)?;
            let n = (BLOCK - pin).min(want - done);
            let (_, phys) = match self.map_block(is, fblk)? {
                Some(mapping) => mapping,
                None => {
                    buf[done..done + n].fill(0);
                    done += n;
                    continue;
                }
            };
            d.read_block(phys, &mut self.iobuf)?;
            buf[done..done + n].copy_from_slice(&self.iobuf[pin..pin + n]);
            done += n;
        }
        Ok(want)
    }

    fn add_extent(
        &mut self,
        is: usize,
        log: u32,
        phys: u32,
        len: u32,
        crc: u32,
    ) -> Result<(), Error> {
        let n = self.inodes[is].n_ext as usize;
        if n >= MAX_EXT {
            return Err(Error::NoSpace);
        }
        self.inodes[is].extents[n] = Extent {
            log,
            phys,
            len,
            crc,
        };
        self.inodes[is].n_ext = (n as u8) + 1;
        Ok(())
    }

    pub fn write<D: Disk>(
        &mut self,
        d: &mut D,
        ino: u32,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, Error> {
        let is = self.inode_slot(ino)?;
        if self.inodes[is].kind == KIND_DIR {
            return Err(Error::IsDir);
        }
        if self.inodes[is].kind == KIND_LNK {
            return Err(Error::Inval);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        // Refuse a write that starts at or past the limit, and cut one
        // that would cross it short at the limit, as Linux does.
        if off >= MAX_FILE_SIZE {
            return Err(Error::FileTooBig);
        }
        let room = MAX_FILE_SIZE - off;
        let keep = usize::try_from(room).map_or(buf.len(), |r| r.min(buf.len()));
        let buf = &buf[..keep];
        let end = off.saturating_add(buf.len() as u64);
        if end <= INLINE as u64
            && (self.inodes[is].flags & F_INLINE != 0)
            && self.inodes[is].n_ext == 0
        {
            let s = off as usize;
            self.inodes[is].inline_data[s..s + buf.len()].copy_from_slice(buf);
            if (s + buf.len()) as u8 > self.inodes[is].inline_len {
                self.inodes[is].inline_len = (s + buf.len()) as u8;
            }
            if end > self.inodes[is].size {
                self.inodes[is].size = end;
            }
            self.inodes[is].flags |= F_INLINE;
            self.bump_mtime(ino);
            return Ok(buf.len());
        }
        if self.inodes[is].flags & F_INLINE != 0 {
            self.spill_inline(d, ino)?;
        }
        let mut done = 0usize;
        while done < buf.len() {
            let pos = off.checked_add(done as u64).ok_or(Error::FileTooBig)?;
            let (fblk, pin) = Self::block_of(pos)?;
            let n = (BLOCK - pin).min(buf.len() - done);
            match self.write_chunk(d, ino, fblk, pin, &buf[done..done + n]) {
                Ok(()) => done += n,
                Err(e) if done == 0 => return Err(e),
                // A short count: the chunks before this one are written.
                Err(_) => break,
            }
        }
        let end = off + done as u64;
        let is = self.inode_slot(ino)?;
        if end > self.inodes[is].size {
            self.inodes[is].size = end;
        }
        self.bump_mtime(ino);
        Ok(done)
    }

    /// The extent slots a write to file block `fblk` adds: 1 for a hole,
    /// 0 to replace a 1-block extent, 1 at either end of a longer extent,
    /// and 2 in its middle.
    fn slots_needed(&self, is: usize, fblk: u32) -> Result<usize, Error> {
        let Some((ei, _)) = self.map_block(is, fblk)? else {
            return Ok(1);
        };
        let e = self.inodes[is].extents[ei];
        let last = e.log + (e.len - 1);
        Ok(match (e.len, fblk == e.log, fblk == last) {
            (1, _, _) => 0,
            (_, true, _) | (_, _, true) => 1,
            _ => 2,
        })
    }

    /// Write `data` into file block `fblk` at byte `pin` through a new
    /// block. Everything that can refuse the write is checked before the
    /// block is allocated, and an error after that releases it.
    fn write_chunk<D: Disk>(
        &mut self,
        d: &mut D,
        ino: u32,
        fblk: u32,
        pin: usize,
        data: &[u8],
    ) -> Result<(), Error> {
        let is = self.inode_slot(ino)?;
        let n_ext = self.inodes[is].n_ext as usize;
        if n_ext + self.slots_needed(is, fblk)? > MAX_EXT {
            return Err(Error::NoSpace);
        }
        let existing = self.map_block(is, fblk)?;
        self.iobuf.fill(0);
        if let Some((ei, phys)) = existing {
            self.drop_room(self.drops_needed(phys, 1))?;
            let old_e = self.inodes[is].extents[ei];
            // The check fills `iobuf` with the whole extent, so the target
            // block is read after it.
            self.check_extent(d, old_e)?;
            d.read_block(phys, &mut self.iobuf)?;
            self.iobuf[pin..pin + data.len()].copy_from_slice(data);
            let newp = self.alloc_block()?;
            let r = self.place_block(d, newp, |v, d, crc| {
                // replace this one physical block in the extent list
                v.split_replace_extent(d, is, fblk, newp, crc)
            });
            if r.is_ok() {
                // `drop_room` above left room for `phys`.
                self.pending_drop(phys)?;
            }
            r
        } else {
            self.iobuf[pin..pin + data.len()].copy_from_slice(data);
            let newp = self.alloc_block()?;
            self.place_block(d, newp, |v, _, crc| v.add_extent(is, fblk, newp, 1, crc))
        }
    }

    /// Write `iobuf` to the new block `newp` and hand its CRC to `link`;
    /// on any error, release `newp`.
    fn place_block<D: Disk>(
        &mut self,
        d: &mut D,
        newp: u32,
        link: impl FnOnce(&mut Self, &mut D, u32) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let r = d
            .write_block(newp, &self.iobuf)
            .and_then(|()| self.extent_crc(d, newp, 1))
            .and_then(|crc| link(self, d, crc));
        match r {
            Ok(()) => Ok(()),
            // `newp` is new in this transaction: `pending_drop` frees it
            // and cannot fail.
            Err(e) => self.pending_drop(newp).and(Err(e)),
        }
    }

    /// Put `newp`, whose CRC is `crc`, in place of file block `fblk`. A
    /// longer extent splits, and the prefix and suffix left of it get
    /// CRCs of their own, over bytes the caller verified just before.
    fn split_replace_extent<D: Disk>(
        &mut self,
        d: &mut D,
        is: usize,
        fblk: u32,
        newp: u32,
        crc: u32,
    ) -> Result<(), Error> {
        let (ei, _) = self.map_block(is, fblk)?.ok_or(Error::Inval)?;
        let e = self.inodes[is].extents[ei];
        if e.len == 1 {
            self.inodes[is].extents[ei].phys = newp;
            self.inodes[is].extents[ei].crc = crc;
            return Ok(());
        }
        // split into prefix + new + suffix; may need extra extent slots.
        // `map_block` found `fblk` inside `e`.
        let fb = u64::from(fblk);
        let left_len = fb.checked_sub(u64::from(e.log)).ok_or(Error::Corrupt)?;
        let right_log = fb.checked_add(1).ok_or(Error::Corrupt)?;
        let right_phys = u64::from(e.phys)
            .checked_add(left_len)
            .and_then(|p| p.checked_add(1))
            .ok_or(Error::Corrupt)?;
        let right_len = u64::from(e.log)
            .checked_add(u64::from(e.len))
            .and_then(|end| end.checked_sub(right_log))
            .ok_or(Error::Corrupt)?;
        let to32 = |v: u64| u32::try_from(v).map_err(|_| Error::Corrupt);
        let (left_len, right_log, right_phys, right_len) = (
            to32(left_len)?,
            to32(right_log)?,
            to32(right_phys)?,
            to32(right_len)?,
        );
        // Both CRCs before any change, so a read error leaves the list whole.
        let left_crc = if left_len > 0 {
            self.extent_crc(d, e.phys, left_len)?
        } else {
            0
        };
        let right_crc = if right_len > 0 {
            self.extent_crc(d, right_phys, right_len)?
        } else {
            0
        };
        // shrink original to left, or replace with the new block if left_len==0
        if left_len == 0 {
            self.inodes[is].extents[ei] = Extent {
                log: fblk,
                phys: newp,
                len: 1,
                crc,
            };
        } else {
            self.inodes[is].extents[ei] = Extent {
                log: e.log,
                phys: e.phys,
                len: left_len,
                crc: left_crc,
            };
            self.add_extent(is, fblk, newp, 1, crc)?;
        }
        if right_len > 0 {
            self.add_extent(is, right_log, right_phys, right_len, right_crc)?;
        }
        Ok(())
    }

    pub fn truncate<D: Disk>(&mut self, d: &mut D, ino: u32, new: u64) -> Result<(), Error> {
        let is = self.inode_slot(ino)?;
        if self.inodes[is].kind == KIND_DIR {
            return Err(Error::IsDir);
        }
        if new > MAX_FILE_SIZE {
            return Err(Error::FileTooBig);
        }
        let old = self.inodes[is].size;
        let inline = self.inodes[is].flags & F_INLINE != 0;
        if new >= old {
            if inline && new <= INLINE as u64 {
                self.inodes[is].size = new;
                self.inodes[is].inline_len = new as u8;
                self.bump_mtime(ino);
                return Ok(());
            }
            // Past the inline bytes the file needs extents (§7): its bytes
            // move to a block while `size` still says how many there are,
            // as a write past byte 128 moves them. `Corrupt` for an inline
            // size an image set past them.
            if inline {
                self.spill_inline(d, ino)?;
            }
            let is = self.inode_slot(ino)?;
            self.inodes[is].size = new;
            self.bump_mtime(ino);
            return Ok(());
        }
        if inline {
            // `new < old`, so `new` fits the inline bytes unless an image
            // set `old` past them; `inline_len` above 128 fails the mount.
            let keep = usize::try_from(new)
                .ok()
                .filter(|&n| old <= INLINE as u64 && n <= INLINE)
                .ok_or(Error::Corrupt)?;
            self.inodes[is].size = new;
            self.inodes[is].inline_len = keep as u8;
            // A later grow reads zeros past `new`, not the old bytes.
            self.inodes[is].inline_data[keep..].fill(0);
            self.bump_mtime(ino);
            return Ok(());
        }
        if new <= INLINE as u64 {
            // The kept bytes are read, which verifies them, before any
            // extent goes.
            let mut tmp = [0u8; INLINE];
            if new > 0 {
                self.read(d, ino, 0, &mut tmp[..new as usize])?;
            }
            self.free_inode_data(d, ino)?;
            let is = self.inode_slot(ino)?;
            self.inodes[is].flags |= F_INLINE;
            self.inodes[is].n_ext = 0;
            self.inodes[is].extents = [Extent::EMPTY; MAX_EXT];
            self.inodes[is].inline_len = new as u8;
            self.inodes[is].inline_data = tmp;
            self.inodes[is].size = new;
            self.bump_mtime(ino);
            return Ok(());
        }
        let keep_blks = u32::try_from(new.div_ceil(BLOCK as u64)).map_err(|_| Error::FileTooBig)?;
        let n_ext = (self.inodes[is].n_ext as usize).min(MAX_EXT);
        // Before any change: every block this truncate drops fits the drop
        // list, and the one extent that straddles the new end verifies and
        // gets the CRC of the part it keeps.
        let mut drops = 0usize;
        let mut straddle = None;
        let mut i = 0usize;
        while i < n_ext {
            let e = self.inodes[is].extents[i];
            let end = u64::from(e.log)
                .checked_add(u64::from(e.len))
                .ok_or(Error::Corrupt)?;
            let gone = if e.log >= keep_blks {
                e.len
            } else if end > u64::from(keep_blks) {
                let keep = keep_blks - e.log;
                self.check_extent(d, e)?;
                straddle = Some((i, self.extent_crc(d, e.phys, keep)?));
                e.len - keep
            } else {
                0
            };
            let from = e.phys.checked_add(e.len - gone).ok_or(Error::Corrupt)?;
            drops += self.drops_needed(from, gone);
            i += 1;
        }
        self.drop_room(drops)?;
        let mut i = 0usize;
        while i < n_ext {
            let e = self.inodes[is].extents[i];
            let end = u64::from(e.log)
                .checked_add(u64::from(e.len))
                .ok_or(Error::Corrupt)?;
            if e.log >= keep_blks {
                let mut b = 0u32;
                while b < e.len {
                    self.pending_drop(e.phys.checked_add(b).ok_or(Error::Corrupt)?)?;
                    b += 1;
                }
                self.inodes[is].extents[i] = Extent::EMPTY;
            } else if end > u64::from(keep_blks) {
                let keep = keep_blks - e.log;
                let mut b = keep;
                while b < e.len {
                    self.pending_drop(e.phys.checked_add(b).ok_or(Error::Corrupt)?)?;
                    b += 1;
                }
                self.inodes[is].extents[i].len = keep;
                if let Some((si, crc)) = straddle
                    && si == i
                {
                    self.inodes[is].extents[i].crc = crc;
                }
            }
            i += 1;
        }
        // compact extent array
        let is = self.inode_slot(ino)?;
        let mut w = 0usize;
        let mut r = 0usize;
        while r < MAX_EXT {
            if self.inodes[is].extents[r].len != 0 {
                self.inodes[is].extents[w] = self.inodes[is].extents[r];
                w += 1;
            }
            r += 1;
        }
        while w < MAX_EXT {
            self.inodes[is].extents[w] = Extent::EMPTY;
            w += 1;
        }
        self.inodes[is].n_ext = self.inodes[is]
            .extents
            .iter()
            .filter(|e| e.len != 0)
            .count() as u8;
        self.inodes[is].size = new;
        self.bump_mtime(ino);
        Ok(())
    }

    pub fn readlink<D: Disk>(
        &mut self,
        _d: &mut D,
        ino: u32,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        let is = self.inode_slot(ino)?;
        if self.inodes[is].kind != KIND_LNK {
            return Err(Error::Inval);
        }
        let n = self.inodes[is].inline_len as usize;
        let n = n.min(buf.len());
        buf[..n].copy_from_slice(&self.inodes[is].inline_data[..n]);
        Ok(n)
    }
}
