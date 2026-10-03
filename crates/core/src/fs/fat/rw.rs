use super::*;

impl FatVol {
    pub(super) fn read<D: Disk>(
        &mut self,
        d: &mut D,
        clu: u32,
        size: u32,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FatError> {
        let max = match u64::from(size).checked_sub(off) {
            Some(m) if m > 0 => m as usize,
            _ => return Ok(0),
        };
        let n = buf.len().min(max);
        self.read_at(d, clu, off, buf.get_mut(..n).ok_or(FatError::Inval)?)?;
        Ok(n)
    }

    #[allow(clippy::too_many_arguments)] // FAT dirent + cluster + size update
    pub(super) fn write<D: Disk>(
        &mut self,
        d: &mut D,
        dir_clu: u32,
        dir_off: u32,
        first: &mut u32,
        size: &mut u32,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FatError> {
        self.write_chain(d, Some((dir_clu, dir_off)), first, size, off, buf)
    }

    /// Write `buf` at `off` into the chain at `*first`, growing it and
    /// `*size` as needed. `dirent` is the short entry to keep in step, or
    /// `None` for an unlinked file whose old slot may belong to another.
    fn write_chain<D: Disk>(
        &mut self,
        d: &mut D,
        dirent: Option<(u32, u32)>,
        first: &mut u32,
        size: &mut u32,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FatError> {
        if buf.is_empty() {
            return Ok(0);
        }
        // A FAT file ends below 4 GiB, its size being 32 bits: a write that
        // starts at the limit or past it is EFBIG, and one that crosses it
        // stops short there, as Linux's `s_maxbytes` check cuts it.
        let room = MAX_FILE_SIZE.checked_sub(off).filter(|&r| r > 0);
        let Some(room) = room else {
            return Err(FatError::FileTooBig);
        };
        let keep = usize::try_from(room).map_or(buf.len(), |r| r.min(buf.len()));
        let buf = buf.get(..keep).unwrap_or(buf);
        let end = off.saturating_add(buf.len() as u64);
        let need = u32::try_from(end).map_err(|_| FatError::FileTooBig)?;
        let old_first = *first;
        self.ensure_size(d, first, *size, need)?;
        self.write_at(d, *first, off, buf)?;
        if need > *size || *first != old_first {
            *size = (*size).max(need);
            if let Some((dir_clu, dir_off)) = dirent {
                self.update_short(d, dir_clu, dir_off, *first, *size)?;
            }
            d.flush()?;
        }
        Ok(buf.len())
    }

    #[cfg(test)]
    pub(super) fn truncate<D: Disk>(
        &mut self,
        d: &mut D,
        dir_clu: u32,
        dir_off: u32,
        first: &mut u32,
        size: &mut u32,
        new: u32,
    ) -> Result<(), FatError> {
        self.truncate_chain(d, Some((dir_clu, dir_off)), first, size, new)
    }

    /// Set the chain at `*first` to `new` bytes. `dirent` as for
    /// [`Self::write_chain`].
    fn truncate_chain<D: Disk>(
        &mut self,
        d: &mut D,
        dirent: Option<(u32, u32)>,
        first: &mut u32,
        size: &mut u32,
        new: u32,
    ) -> Result<(), FatError> {
        if new == *size {
            return Ok(());
        }
        if new > *size {
            self.ensure_size(d, first, *size, new)?;
            *size = new;
            if let Some((dir_clu, dir_off)) = dirent {
                self.update_short(d, dir_clu, dir_off, *first, *size)?;
            }
            return d.flush();
        }
        // A chain the image corrupted (a loop, or a link to a free, bad or
        // reserved cluster) fails here, before the dirent or the FAT
        // changes: the walks below would free clusters the dirent still
        // names.
        if *first >= 2 {
            self.chain_len(d, *first)?;
        }
        // Size first while clusters stay allocated. Then drop the cluster
        // pointer (still allocated) so the dirent never names a free cluster.
        if let Some((dir_clu, dir_off)) = dirent {
            self.update_short(d, dir_clu, dir_off, *first, new)?;
            d.flush()?;
        }
        let cb = self.info.clus_bytes() as u32;
        let keep = if new == 0 { 0 } else { new.div_ceil(cb) };
        if keep == 0 {
            let old = *first;
            *first = 0;
            *size = new;
            if let Some((dir_clu, dir_off)) = dirent {
                self.update_short(d, dir_clu, dir_off, 0, new)?;
                d.flush()?;
            }
            self.free_chain(d, old)?;
            return Ok(());
        }
        let mut clu = *first;
        let mut i = 0u32;
        while clu >= 2 && !is_eoc(clu) {
            let next = self.fat_get(d, clu)?;
            i = i.checked_add(1).ok_or(FatError::Corrupt)?;
            if i == keep {
                self.fat_set(d, clu, EOC_MIN)?;
            } else if i > keep {
                self.fat_set(d, clu, 0)?;
            }
            clu = next;
            if i > self.info.nclus {
                return Err(FatError::Corrupt);
            }
        }
        self.commit_fat(d)?;
        *size = new;
        d.flush()
    }

    pub fn create<D: Disk>(
        &mut self,
        d: &mut D,
        dir_clu: u32,
        name: &[u8],
        dir: bool,
    ) -> Result<Node, FatError> {
        self.check_name(name)?;
        match self.lookup(d, dir_clu, name) {
            Ok(_) => return Err(FatError::Exists),
            Err(FatError::NotFound) => {}
            Err(e) => return Err(e),
        }
        let mut short = [0u8; 11];
        let lfn = self.pick_short(d, dir_clu, name, &mut short)?;
        let n_lfn = if lfn {
            utf16_len(name)?.div_ceil(LFN_CHARS)
        } else {
            0
        };
        let slots = n_lfn.checked_add(1).ok_or(FatError::NameTooLong)?;
        let (ent_clu, ent_off) = self.dir_reserve(d, dir_clu, slots)?;
        let mut first = 0u32;
        if dir {
            first = self.alloc_clu(d, 0)?;
            self.init_dir_cluster(d, first, dir_clu)?;
            self.fat_set(d, first, EOC_MIN)?;
            self.commit_fat(d)?;
            d.flush()?;
        }
        let cs = lfn_checksum(&short);
        let (date, time) = fat_datetime(self.now);
        for (slot, ord) in (1..=n_lfn).rev().enumerate() {
            let last = slot == 0;
            let mut ent = [0u8; ENT];
            fill_lfn(&mut ent, ord as u8, last, cs, name)?;
            self.write_dir_raw(d, dir_clu, ent_at(ent_off, slot)?, &ent)?;
        }
        let mut ent = [0u8; ENT];
        ent[..11].copy_from_slice(&short);
        ent[11] = if dir { ATTR_DIR } else { ATTR_ARCH };
        put_le16(&mut ent, 14, time)?;
        put_le16(&mut ent, 16, date)?;
        put_le16(&mut ent, 18, date)?;
        put_le16(&mut ent, 20, (first >> 16) as u16)?;
        put_le16(&mut ent, 22, time)?;
        put_le16(&mut ent, 24, date)?;
        put_le16(&mut ent, 26, (first & 0xFFFF) as u16)?;
        put_le32(&mut ent, 28, 0)?;
        let short_off = ent_at(ent_off, n_lfn)?;
        self.write_dir_raw(d, dir_clu, short_off, &ent)?;
        d.flush()?;
        let _ = (ent_clu, date, time);
        self.node_from_short(dir_clu, short_off, &ent, name)
    }

    /// Remove `name` from `dir_clu` and return the removed entry's words.
    /// Its clusters stay allocated: the caller frees them with
    /// [`Self::free_chain`] once nothing holds the file.
    pub fn unlink<D: Disk>(
        &mut self,
        d: &mut D,
        dir_clu: u32,
        name: &[u8],
        rmdir: bool,
    ) -> Result<FatInode, FatError> {
        if name_is_dot(name) || name_is_dotdot(name) {
            return Err(FatError::Inval);
        }
        let node = self.lookup(d, dir_clu, name)?;
        if rmdir {
            if node.kind != InodeKind::Dir {
                return Err(FatError::NotDir);
            }
            if !self.dir_empty(d, node.clu)? {
                return Err(FatError::NotEmpty);
            }
        } else if node.kind == InodeKind::Dir {
            return Err(FatError::IsDir);
        }
        self.mark_deleted(d, dir_clu, node.dir_off)?;
        d.flush()?;
        Ok(FatInode::of_node(&node))
    }

    /// Move `src_name` in `src_dir` to `dst_name` in `dst_dir`, replacing
    /// a file there, or an empty directory when the source is one. The
    /// caller moves an open file's words to `to` and frees `replaced` once
    /// nothing holds it.
    ///
    /// Every check of the image, a moved directory's `..` included, runs
    /// before the first write. The new name is written and flushed before
    /// the source's entry goes, so a crash between leaves two names for
    /// the source's clusters, never none. A write that fails puts back
    /// what the rename wrote, so an error leaves both names as they were.
    pub fn rename<D: Disk>(
        &mut self,
        d: &mut D,
        src_dir: u32,
        src_name: &[u8],
        dst_dir: u32,
        dst_name: &[u8],
    ) -> Result<RenameMoved, FatError> {
        let src = self.lookup(d, src_dir, src_name)?;
        let from = (src.dir_clu, src.dir_off);
        if src_name == dst_name && src_dir == dst_dir {
            return Ok(RenameMoved {
                from,
                to: from,
                replaced: None,
            });
        }
        self.check_name(dst_name)?;
        if src.kind == InodeKind::Dir && self.in_subtree(d, src.clu, dst_dir)? {
            return Err(FatError::Inval);
        }
        match self.lookup(d, dst_dir, dst_name) {
            // The source's own dirent, as a case-only rename finds it: the
            // new name is written before the old one goes, so nothing is
            // unlinked.
            Ok(dst) if (dst.dir_clu, dst.dir_off) == from => {}
            Ok(dst) => return self.rename_over(d, (src_dir, &src), &dst),
            Err(FatError::NotFound) => {}
            Err(e) => return Err(e),
        }
        // Recreate dest name pointing at existing clusters, then drop src dirent.
        let mut short = [0u8; 11];
        let lfn = self.pick_short(d, dst_dir, dst_name, &mut short)?;
        let n_lfn = if lfn {
            utf16_len(dst_name)?.div_ceil(LFN_CHARS)
        } else {
            0
        };
        let slots = n_lfn.checked_add(1).ok_or(FatError::NameTooLong)?;
        let undo = self.rename_undo_of(d, src_dir, &src, dst_dir)?;
        let (_c, ent_off) = self.dir_reserve(d, dst_dir, slots)?;
        let short_off = ent_at(ent_off, n_lfn)?;
        let cs = lfn_checksum(&short);
        let mut ent = undo.src;
        ent[..11].copy_from_slice(&short);
        let r = (|| {
            for (slot, ord) in (1..=n_lfn).rev().enumerate() {
                let mut lfn = [0u8; ENT];
                fill_lfn(&mut lfn, ord as u8, slot == 0, cs, dst_name)?;
                self.write_dir_raw(d, dst_dir, ent_at(ent_off, slot)?, &lfn)?;
            }
            self.write_dir_raw(d, dst_dir, short_off, &ent)?;
            self.rename_finish(d, src_dir, &src, dst_dir, &undo)
        })();
        if let Err(e) = r {
            // The new name's slots were free; they become deleted entries.
            let mut gone = [0u8; ENT];
            gone[0] = ENT_DEL;
            let mut back = Ok(());
            for slot in 0..slots {
                back = back.and(
                    ent_at(ent_off, slot).and_then(|o| self.write_dir_raw(d, dst_dir, o, &gone)),
                );
            }
            return back
                .and(self.rename_undo(d, src_dir, &src, &undo))
                .and(Err(e));
        }
        Ok(RenameMoved {
            from,
            to: (dst_dir, short_off),
            replaced: None,
        })
    }

    /// What a rename of `src` from `src_dir` to `dst_dir` changes besides
    /// the new name, read and checked before its first write: the
    /// source's short entry and, when a directory changes parent, its
    /// `..` entry, which is `Corrupt` when it is not one.
    fn rename_undo_of<D: Disk>(
        &mut self,
        d: &mut D,
        src_dir: u32,
        src: &Node,
        dst_dir: u32,
    ) -> Result<RenameUndo, FatError> {
        let mut ent = [0u8; ENT];
        if !self.read_dir_raw(d, src_dir, src.dir_off, &mut ent)? {
            return Err(FatError::Corrupt);
        }
        let mut dotdot = None;
        if src.kind == InodeKind::Dir && src_dir != dst_dir {
            let mut dd = [0u8; ENT];
            if !self.read_dir_raw(d, src.clu, ENT as u32, &mut dd)? || &dd[..11] != b"..         " {
                return Err(FatError::Corrupt);
            }
            dotdot = Some(dd);
        }
        Ok(RenameUndo { src: ent, dotdot })
    }

    /// The steps of a rename after its new name is written: a moved
    /// directory's `..` names its new parent, the new name is flushed, and
    /// the source's entry goes.
    fn rename_finish<D: Disk>(
        &mut self,
        d: &mut D,
        src_dir: u32,
        src: &Node,
        dst_dir: u32,
        undo: &RenameUndo,
    ) -> Result<(), FatError> {
        if undo.dotdot.is_some() {
            let parent = if dst_dir == self.info.root_clus {
                0
            } else {
                dst_dir
            };
            self.set_dotdot(d, src.clu, parent)?;
        }
        d.flush()?;
        self.mark_deleted(d, src_dir, src.dir_off)?;
        d.flush()
    }

    /// Put back the source's side of a failed rename, after its caller put
    /// back the new name's slots: the moved directory's `..` and the
    /// source's short entry, then a flush. Every write is tried; the first
    /// error is returned. Long-name entries `mark_deleted` removed before
    /// it failed stay removed, so the source then keeps its short name.
    fn rename_undo<D: Disk>(
        &mut self,
        d: &mut D,
        src_dir: u32,
        src: &Node,
        undo: &RenameUndo,
    ) -> Result<(), FatError> {
        let mut r = Ok(());
        if let Some(dd) = &undo.dotdot {
            r = r.and(self.write_dir_raw(d, src.clu, ENT as u32, dd));
        }
        r.and(self.write_dir_raw(d, src_dir, src.dir_off, &undo.src))
            .and(d.flush())
    }

    /// [`Self::rename`] onto the existing `dst`, as rename(2) has it: a
    /// directory replaces only an empty directory and anything else only a
    /// non-directory. The source's entry is written over `dst`'s short
    /// entry under `dst`'s name, as Linux's vfat reuses the target's slot,
    /// so the name never goes missing and nothing is allocated; then the
    /// source's entry goes. A failed write puts `dst`'s entry back, so
    /// the target keeps its clusters. `dst`'s clusters are the caller's to
    /// free.
    fn rename_over<D: Disk>(
        &mut self,
        d: &mut D,
        (src_dir, src): (u32, &Node),
        dst: &Node,
    ) -> Result<RenameMoved, FatError> {
        match (src.kind == InodeKind::Dir, dst.kind == InodeKind::Dir) {
            (false, true) => return Err(FatError::IsDir),
            (true, false) => return Err(FatError::NotDir),
            (true, true) if !self.dir_empty(d, dst.clu)? => return Err(FatError::NotEmpty),
            _ => {}
        }
        let mut name = [0u8; ENT];
        if !self.read_dir_raw(d, dst.dir_clu, dst.dir_off, &mut name)? {
            return Err(FatError::Corrupt);
        }
        let undo = self.rename_undo_of(d, src_dir, src, dst.dir_clu)?;
        let mut ent = undo.src;
        ent[..11].copy_from_slice(&name[..11]);
        let r = self
            .write_dir_raw(d, dst.dir_clu, dst.dir_off, &ent)
            .and_then(|()| self.rename_finish(d, src_dir, src, dst.dir_clu, &undo));
        if let Err(e) = r {
            return self
                .write_dir_raw(d, dst.dir_clu, dst.dir_off, &name)
                .and(self.rename_undo(d, src_dir, src, &undo))
                .and(Err(e));
        }
        Ok(RenameMoved {
            from: (src.dir_clu, src.dir_off),
            to: (dst.dir_clu, dst.dir_off),
            replaced: Some(FatInode::of_node(dst)),
        })
    }

    /// The parent cluster `dir`'s `..` entry names, the root's for 0.
    pub(super) fn dotdot_of<D: Disk>(&mut self, d: &mut D, dir: u32) -> Result<u32, FatError> {
        let mut ent = [0u8; ENT];
        if !self.read_dir_raw(d, dir, ENT as u32, &mut ent)? || &ent[..11] != b"..         " {
            return Err(FatError::Corrupt);
        }
        let clu = (le16(&ent, 20)? as u32) << 16 | le16(&ent, 26)? as u32;
        Ok(if clu == 0 { self.info.root_clus } else { clu })
    }

    /// Point `dir`'s `..` entry at `parent` (0 for the root, as
    /// `init_dir_cluster` writes it).
    fn set_dotdot<D: Disk>(&mut self, d: &mut D, dir: u32, parent: u32) -> Result<(), FatError> {
        let mut ent = [0u8; ENT];
        if !self.read_dir_raw(d, dir, ENT as u32, &mut ent)? || &ent[..11] != b"..         " {
            return Err(FatError::Corrupt);
        }
        put_le16(&mut ent, 20, (parent >> 16) as u16)?;
        put_le16(&mut ent, 26, parent as u16)?;
        self.write_dir_raw(d, dir, ENT as u32, &ent)
    }

    /// Whether directory `dir` is `top` or lies below it, walking `..` up
    /// to the root.
    fn in_subtree<D: Disk>(&mut self, d: &mut D, top: u32, dir: u32) -> Result<bool, FatError> {
        let mut cur = dir;
        let mut steps = 0u32;
        loop {
            if cur == top {
                return Ok(true);
            }
            if cur == self.info.root_clus {
                return Ok(false);
            }
            if steps > self.info.nclus {
                return Err(FatError::Corrupt);
            }
            cur = self.dotdot_of(d, cur)?;
            steps = steps.checked_add(1).ok_or(FatError::Corrupt)?;
        }
    }

    pub fn read_ino<D: Disk>(
        &mut self,
        d: &mut D,
        n: &FatInode,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FatError> {
        if n.kind == InodeKind::Dir {
            return Err(FatError::IsDir);
        }
        let size = u32::try_from(n.size).map_err(|_| FatError::Corrupt)?;
        self.read(d, n.first_clu, size, off, buf)
    }

    /// Write `buf` at `off`, or at the end of the file when `append` is
    /// set; returns the count written and the position it wrote at. The
    /// dirent is kept in step while `linked`; an unlinked file's old slot
    /// may belong to another. `n` is updated even when a later step
    /// fails, since the chain may have grown.
    pub fn write_ino<D: Disk>(
        &mut self,
        d: &mut D,
        n: &mut FatInode,
        linked: bool,
        off: u64,
        append: bool,
        buf: &[u8],
    ) -> Result<(usize, u64), FatError> {
        if n.kind == InodeKind::Dir {
            return Err(FatError::IsDir);
        }
        let pos = if append { n.size } else { off };
        let mut size = u32::try_from(n.size).map_err(|_| FatError::Corrupt)?;
        let dirent = linked.then_some((n.dir_clu, n.dir_off));
        let r = self.write_chain(d, dirent, &mut n.first_clu, &mut size, pos, buf);
        n.size = u64::from(size);
        Ok((r?, pos))
    }

    /// Set the file to `new` bytes; `linked` and `n` as for
    /// [`Self::write_ino`].
    pub fn truncate_ino<D: Disk>(
        &mut self,
        d: &mut D,
        n: &mut FatInode,
        linked: bool,
        new: u64,
    ) -> Result<(), FatError> {
        // Past the 32-bit size is EFBIG, as Linux's `inode_newsize_ok`.
        let new = u32::try_from(new).map_err(|_| FatError::FileTooBig)?;
        if n.kind == InodeKind::Dir {
            return Err(FatError::IsDir);
        }
        let mut size = u32::try_from(n.size).map_err(|_| FatError::Corrupt)?;
        let dirent = linked.then_some((n.dir_clu, n.dir_off));
        let r = self.truncate_chain(d, dirent, &mut n.first_clu, &mut size, new);
        n.size = u64::from(size);
        r
    }

    /// Grow the chain at `*first` to hold `new` bytes. `NoSpace` before
    /// any allocation when the free count is short; an error part way
    /// through, such as a failed read or write, frees the clusters this
    /// call allocated and restores the old end of chain and `*first`.
    fn ensure_size<D: Disk>(
        &mut self,
        d: &mut D,
        first: &mut u32,
        old: u32,
        new: u32,
    ) -> Result<(), FatError> {
        if new <= old && *first >= 2 {
            return Ok(());
        }
        let cb = self.info.clus_bytes() as u32;
        let need = if new == 0 { 0 } else { new.div_ceil(cb) };
        if need == 0 {
            return Ok(());
        }
        let (have, tail) = if *first < 2 {
            (0, 0)
        } else {
            self.chain_len(d, *first)?
        };
        let short = match need.checked_sub(have) {
            Some(n) if n > 0 => n,
            _ => return Ok(()),
        };
        if short > self.free {
            return Err(FatError::NoSpace);
        }
        let old_first = *first;
        let mut grown = Grown {
            head: 0,
            unlinked: 0,
        };
        match self.extend_chain(d, first, tail, short, &mut grown) {
            Ok(()) => d.flush(),
            Err(e) => {
                *first = old_first;
                self.undo_extend(d, tail, &grown).and(Err(e))
            }
        }
    }

    /// Link `count` zeroed clusters after `tail`, or at `*first` when
    /// `tail` is 0, and commit the FAT; `grown` records what an error
    /// leaves for [`Self::undo_extend`].
    fn extend_chain<D: Disk>(
        &mut self,
        d: &mut D,
        first: &mut u32,
        mut tail: u32,
        count: u32,
        grown: &mut Grown,
    ) -> Result<(), FatError> {
        for _ in 0..count {
            // `alloc_clu` marks the cluster end of chain.
            let n = self.alloc_clu(d, tail)?;
            grown.unlinked = n;
            self.zero_cluster(d, n)?;
            if tail < 2 {
                *first = n;
            } else {
                self.fat_set(d, tail, n)?;
            }
            if grown.head == 0 {
                grown.head = n;
            }
            grown.unlinked = 0;
            tail = n;
        }
        self.commit_fat(d)
    }

    /// Free what a failed [`Self::extend_chain`] allocated and end the old
    /// chain at `tail` again.
    fn undo_extend<D: Disk>(
        &mut self,
        d: &mut D,
        tail: u32,
        grown: &Grown,
    ) -> Result<(), FatError> {
        if grown.unlinked >= 2 {
            self.fat_set(d, grown.unlinked, 0)?;
        }
        if grown.head >= 2 {
            if tail >= 2 {
                self.fat_set(d, tail, EOC_MIN)?;
            }
            self.release_chain(d, grown.head)?;
        }
        self.commit_fat(d)
    }

    fn read_at<D: Disk>(
        &mut self,
        d: &mut D,
        first: u32,
        off: u64,
        buf: &mut [u8],
    ) -> Result<(), FatError> {
        if first < 2 {
            if buf.iter().any(|_| true) && !buf.is_empty() {
                buf.fill(0);
            }
            return Ok(());
        }
        let cb = self.info.clus_bytes() as u64;
        let skip = off.checked_div(cb).ok_or(FatError::Corrupt)?;
        let mut pin = off.checked_rem(cb).ok_or(FatError::Corrupt)? as usize;
        let mut clu = first;
        let mut s = 0u32;
        while s < skip as u32 {
            clu = self.fat_get(d, clu)?;
            if clu < 2 || is_eoc(clu) {
                buf.fill(0);
                return Ok(());
            }
            s = s.checked_add(1).ok_or(FatError::Corrupt)?;
            if s > self.info.nclus {
                return Err(FatError::Corrupt);
            }
        }
        let mut rest: &mut [u8] = buf;
        while !rest.is_empty() {
            if clu < 2 || is_eoc(clu) {
                rest.fill(0);
                break;
            }
            let n = Self::read_cluster(&self.info, d, clu, &mut self.clbuf)?;
            let src = self.clbuf.get(pin..n).ok_or(FatError::Corrupt)?;
            let take = src.len().min(rest.len());
            let (head, tail) = core::mem::take(&mut rest)
                .split_at_mut_checked(take)
                .ok_or(FatError::Corrupt)?;
            head.copy_from_slice(src.get(..take).ok_or(FatError::Corrupt)?);
            rest = tail;
            pin = 0;
            if rest.is_empty() {
                break;
            }
            clu = self.fat_get(d, clu)?;
        }
        Ok(())
    }

    fn write_at<D: Disk>(
        &mut self,
        d: &mut D,
        first: u32,
        off: u64,
        buf: &[u8],
    ) -> Result<(), FatError> {
        let cb = self.info.clus_bytes() as u64;
        let skip = off.checked_div(cb).ok_or(FatError::Corrupt)?;
        let mut pin = off.checked_rem(cb).ok_or(FatError::Corrupt)? as usize;
        let mut clu = first;
        let mut s = 0u32;
        while s < skip as u32 {
            clu = self.fat_get(d, clu)?;
            if clu < 2 || is_eoc(clu) {
                return Err(FatError::Corrupt);
            }
            s = s.checked_add(1).ok_or(FatError::Corrupt)?;
        }
        let mut rest = buf;
        while !rest.is_empty() {
            if clu < 2 || is_eoc(clu) {
                return Err(FatError::Corrupt);
            }
            let n = Self::read_cluster(&self.info, d, clu, &mut self.clbuf)?;
            let dst = self.clbuf.get_mut(pin..n).ok_or(FatError::Corrupt)?;
            let take = dst.len().min(rest.len());
            let (head, tail) = rest.split_at_checked(take).ok_or(FatError::Corrupt)?;
            dst.get_mut(..take)
                .ok_or(FatError::Corrupt)?
                .copy_from_slice(head);
            Self::write_cluster(
                &self.info,
                d,
                clu,
                self.clbuf.get(..n).ok_or(FatError::Corrupt)?,
            )?;
            rest = tail;
            pin = 0;
            if rest.is_empty() {
                break;
            }
            clu = self.fat_get(d, clu)?;
        }
        Ok(())
    }
}

/// The offset of the `slot`th entry after `base` in a directory.
fn ent_at(base: u32, slot: usize) -> Result<u32, FatError> {
    let rel = slot
        .checked_mul(ENT)
        .and_then(|b| u32::try_from(b).ok())
        .ok_or(FatError::NoSpace)?;
    base.checked_add(rel).ok_or(FatError::NoSpace)
}

/// What a rename puts back when a write fails ([`FatVol::rename`]): the
/// source's short entry, and the moved directory's `..` entry when the
/// rename changes its parent.
struct RenameUndo {
    src: [u8; ENT],
    dotdot: Option<[u8; ENT]>,
}

/// The clusters a failed extend allocated: the first one it linked, and
/// one it allocated but has not linked yet (0 for none).
struct Grown {
    head: u32,
    unlinked: u32,
}
