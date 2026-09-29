use super::*;

impl Vol {
    pub fn snapshot<D: Disk>(&mut self, d: &mut D, name: &[u8]) -> Result<(), Error> {
        if name.is_empty() || name.len() > 16 {
            return Err(Error::Inval);
        }
        if self.dirty {
            self.sync(d)?;
        }
        let mut i = 0usize;
        while i < MAX_SNAPS {
            if !self.snaps[i].used {
                break;
            }
            i += 1;
        }
        if i >= MAX_SNAPS {
            return Err(Error::NoSpace);
        }
        let mut nm = [0u8; 16];
        nm[..name.len()].copy_from_slice(name);
        self.snaps[i] = Snap {
            used: true,
            name: nm,
            generation: self.generation,
            inode_root: self.inode_root,
            alloc_root: self.alloc_root,
            next_ino: self.next_ino,
        };
        let mut b = 0u32;
        while b < self.nblocks {
            if self.refc[b as usize] > 0 && self.refc[b as usize] < 255 {
                self.refc[b as usize] += 1;
            }
            b += 1;
        }
        self.dirty = true;
        self.sync(d)
    }

    pub fn sync<D: Disk>(&mut self, d: &mut D) -> Result<(), Error> {
        if !self.dirty {
            return d.flush();
        }
        self.commit(d)
    }
}

fn count_used_inodes(v: &Vol) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while i < MAX_INODES {
        if v.inodes[i].used {
            n += 1;
        }
        i += 1;
    }
    n
}

fn inode_need(n: usize) -> (usize, usize) {
    if n == 0 {
        return (1, 0);
    }
    let leaves = n.div_ceil(INODE_PER_LEAF);
    let ints = if leaves > 1 { 1 } else { 0 };
    (leaves, ints)
}

fn dir_need(n: usize) -> (usize, usize) {
    if n == 0 {
        return (0, 0);
    }
    let leaves = n.div_ceil(DENT_PER_LEAF);
    let ints = if leaves > 1 { 1 } else { 0 };
    (leaves, ints)
}

impl Vol {
    fn collect_inodes(&self, out: &mut [usize; MAX_INODES]) -> usize {
        let mut n = 0usize;
        let mut i = 0usize;
        while i < MAX_INODES {
            if self.inodes[i].used {
                out[n] = i;
                n += 1;
            }
            i += 1;
        }
        let mut a = 1usize;
        while a < n {
            let mut b = a;
            while b > 0 && self.inodes[out[b - 1]].ino > self.inodes[out[b]].ino {
                out.swap(b - 1, b);
                b -= 1;
            }
            a += 1;
        }
        n
    }

    fn collect_dents(&self, parent: u32, out: &mut [usize; MAX_DENTS]) -> usize {
        let mut n = 0usize;
        let mut i = 0usize;
        while i < MAX_DENTS {
            if self.dents[i].used && self.dents[i].parent == parent {
                out[n] = i;
                n += 1;
            }
            i += 1;
        }
        let mut a = 1usize;
        while a < n {
            let mut b = a;
            while b > 0
                && name_cmp(self.dents[out[b - 1]].name(), self.dents[out[b]].name()).is_gt()
            {
                out.swap(b - 1, b);
                b -= 1;
            }
            a += 1;
        }
        n
    }

    fn commit<D: Disk>(&mut self, d: &mut D) -> Result<(), Error> {
        let nino = count_used_inodes(self);
        let (ileaves, iints) = inode_need(nino.max(1));
        let mut dblocks = 0usize;
        let mut i = 0usize;
        while i < MAX_INODES {
            if self.inodes[i].used && self.inodes[i].kind == KIND_DIR {
                let c = self.dir_count(self.inodes[i].ino) as usize;
                let (l, t) = dir_need(c);
                dblocks += l + t;
            }
            i += 1;
        }
        let need = 1 + ileaves + iints + dblocks;
        // Every block counted here goes into `self.meta` after the super
        // flush, so the table check runs while the old super is live.
        if need > MAX_META {
            return Err(Error::NoSpace);
        }
        if self.free_count() < need as u32 {
            return Err(Error::NoSpace);
        }
        let old_meta_n = self.nmeta;
        let mut old_meta = [0u32; MAX_META];
        old_meta[..old_meta_n as usize].copy_from_slice(&self.meta[..old_meta_n as usize]);

        // Every metadata block this commit writes; `self.meta` after step 7.
        let mut new_meta = [0u32; MAX_META];
        let mut n_new = 0usize;
        let alloc_bno = self.alloc_block()?;
        *new_meta.get_mut(n_new).ok_or(Error::NoSpace)? = alloc_bno;
        n_new += 1;
        let mut ileaf = [0u32; 8];
        let mut li = 0usize;
        while li < ileaves {
            ileaf[li] = self.alloc_block()?;
            *new_meta.get_mut(n_new).ok_or(Error::NoSpace)? = ileaf[li];
            n_new += 1;
            li += 1;
        }
        let iroot = if iints > 0 {
            let b = self.alloc_block()?;
            *new_meta.get_mut(n_new).ok_or(Error::NoSpace)? = b;
            n_new += 1;
            b
        } else {
            ileaf[0]
        };

        let mut idx = [0usize; MAX_INODES];
        let ni = self.collect_inodes(&mut idx);

        // directory roots assigned while writing dir trees
        let mut dir_roots = [0u32; MAX_INODES];
        // The `new_meta` index of this commit's first directory block.
        let mut first_dir_meta: Option<usize> = None;

        li = 0;
        while li < MAX_INODES {
            if self.inodes[li].used && self.inodes[li].kind == KIND_DIR {
                let ino = self.inodes[li].ino;
                let mut dents = [0usize; MAX_DENTS];
                let nd = self.collect_dents(ino, &mut dents);
                if nd == 0 {
                    dir_roots[li] = 0;
                    li += 1;
                    continue;
                }
                let (leaves, ints) = dir_need(nd);
                if first_dir_meta.is_none() {
                    first_dir_meta = Some(n_new);
                }
                let mut dleaf = [0u32; 8];
                let mut k = 0usize;
                while k < leaves {
                    dleaf[k] = self.alloc_block()?;
                    *new_meta.get_mut(n_new).ok_or(Error::NoSpace)? = dleaf[k];
                    n_new += 1;
                    k += 1;
                }
                let droot = if ints > 0 {
                    let b = self.alloc_block()?;
                    *new_meta.get_mut(n_new).ok_or(Error::NoSpace)? = b;
                    n_new += 1;
                    b
                } else {
                    dleaf[0]
                };
                k = 0;
                while k < leaves {
                    let start = k * DENT_PER_LEAF;
                    let end = (start + DENT_PER_LEAF).min(nd);
                    meta_hdr(
                        &mut self.iobuf,
                        META_DIR_LEAF,
                        0,
                        (end - start) as u16,
                        self.generation + 1,
                        ino,
                    );
                    let mut e = 0usize;
                    while start + e < end {
                        let de = &self.dents[dents[start + e]];
                        let o = HDR + e * DENT_REC;
                        put32(&mut self.iobuf, o, de.ino);
                        self.iobuf[o + 4] = de.kind;
                        self.iobuf[o + 5] = de.nlen;
                        self.iobuf[o + 6..o + 6 + MAX_NAME].copy_from_slice(&de.name);
                        e += 1;
                    }
                    finish_meta(&mut self.iobuf);
                    d.write_block(dleaf[k], &self.iobuf)?;
                    k += 1;
                }
                if ints > 0 {
                    meta_hdr(
                        &mut self.iobuf,
                        META_DIR_INT,
                        1,
                        leaves as u16,
                        self.generation + 1,
                        ino,
                    );
                    k = 0;
                    while k < leaves {
                        let de = &self.dents[dents[k * DENT_PER_LEAF]];
                        let o = HDR + k * DENT_REC;
                        put32(&mut self.iobuf, o, dleaf[k]);
                        self.iobuf[o + 4] = de.nlen;
                        self.iobuf[o + 5] = 0;
                        self.iobuf[o + 6..o + 6 + MAX_NAME].copy_from_slice(&de.name);
                        k += 1;
                    }
                    finish_meta(&mut self.iobuf);
                    d.write_block(droot, &self.iobuf)?;
                }
                dir_roots[li] = droot;
            }
            li += 1;
        }

        let mut ii = 0usize;
        while ii < MAX_INODES {
            if self.inodes[ii].used && self.inodes[ii].kind == KIND_DIR {
                self.inodes[ii].dir_root = dir_roots[ii];
            }
            ii += 1;
        }

        // inode leaves
        li = 0;
        while li < ileaves {
            let start = li * INODE_PER_LEAF;
            let end = (start + INODE_PER_LEAF).min(ni.max(1));
            let count = if ni == 0 { 0 } else { end - start };
            meta_hdr(
                &mut self.iobuf,
                META_INODE_LEAF,
                0,
                count as u16,
                self.generation + 1,
                0,
            );
            let mut e = 0usize;
            while start + e < end && ni > 0 {
                let rec = &self.inodes[idx[start + e]];
                pack_inode(&mut self.iobuf[HDR + e * INODE_REC..], rec);
                e += 1;
            }
            finish_meta(&mut self.iobuf);
            d.write_block(ileaf[li], &self.iobuf)?;
            li += 1;
        }
        if iints > 0 {
            meta_hdr(
                &mut self.iobuf,
                META_INODE_INT,
                1,
                ileaves as u16,
                self.generation + 1,
                0,
            );
            li = 0;
            while li < ileaves {
                let key = if ni == 0 {
                    ROOT_INO
                } else {
                    self.inodes[idx[li * INODE_PER_LEAF]].ino
                };
                let o = HDR + li * 8;
                put32(&mut self.iobuf, o, key);
                put32(&mut self.iobuf, o + 4, ileaf[li]);
                li += 1;
            }
            finish_meta(&mut self.iobuf);
            d.write_block(iroot, &self.iobuf)?;
        }

        self.generation = self.generation.saturating_add(1);
        self.inode_root = iroot;
        self.alloc_root = alloc_bno;
        let mut sbuf = [0u8; BLOCK];
        write_alloc_into(self, &old_meta[..old_meta_n as usize], &mut sbuf);
        d.write_block(alloc_bno, &sbuf)?;
        let slot = (self.generation % 2) as u8;
        #[cfg(any(test, feature = "crash_plant"))]
        let early_super = self.plant == Plant::EarlySuper;
        #[cfg(not(any(test, feature = "crash_plant")))]
        let early_super = false;
        if early_super {
            pack_super(&mut sbuf, self, slot);
            d.write_block(slot as u32, &sbuf)?;
        }
        d.flush()?;

        if !early_super {
            pack_super(&mut sbuf, self, slot);
            d.write_block(slot as u32, &sbuf)?;
        }
        d.flush()?;

        // In memory, apply the drops the alloc map above already carries.
        let nblocks = self.nblocks;
        for &b in old_meta[..old_meta_n as usize]
            .iter()
            .chain(self.drop[..self.ndrop as usize].iter())
        {
            drop_ref(&mut self.bitmap, &mut self.refc, nblocks, b);
        }
        self.ndrop = 0;
        self.txn = [0; MAX_BLOCKS.div_ceil(8)];
        self.nmeta = 0;
        // `need <= MAX_META` was checked before the first allocation, so no
        // call below fails with the new super on disk.
        debug_assert_eq!(n_new, need);
        #[cfg(any(test, feature = "crash_plant"))]
        let leak = first_dir_meta.filter(|_| self.plant == Plant::Leak);
        #[cfg(not(any(test, feature = "crash_plant")))]
        let leak = first_dir_meta.filter(|_| false);
        for (i, &b) in new_meta[..n_new].iter().enumerate() {
            if leak == Some(i) {
                continue;
            }
            self.mark_meta(b)?;
        }
        self.dirty = false;
        Ok(())
    }

    fn load_inode_leaf(&mut self, buf: &[u8; BLOCK]) -> Result<(), Error> {
        let (_lvl, count, _) = parse_meta(buf, META_INODE_LEAF)?;
        let mut e = 0usize;
        while e < count as usize {
            let rec = unpack_inode(&buf[HDR + e * INODE_REC..])?;
            let slot = self.alloc_ino_slot()?;
            self.inodes[slot] = rec;
            e += 1;
        }
        Ok(())
    }

    fn load_dir_leaf(&mut self, buf: &[u8; BLOCK], parent: u32) -> Result<(), Error> {
        let (_lvl, count, _) = parse_meta(buf, META_DIR_LEAF)?;
        let mut e = 0usize;
        while e < count as usize {
            let o = HDR + e * DENT_REC;
            let de = self.alloc_dent()?;
            let nlen = buf[o + 5];
            if nlen as usize > MAX_NAME || nlen == 0 {
                return Err(Error::Corrupt);
            }
            let mut name = [0u8; MAX_NAME];
            name.copy_from_slice(&buf[o + 6..o + 6 + MAX_NAME]);
            self.dents[de] = Dent {
                used: true,
                parent,
                ino: le32(buf, o),
                kind: buf[o + 4],
                nlen,
                name,
            };
            e += 1;
        }
        Ok(())
    }
}

pub fn mount<D: Disk>(d: &mut D, v: &mut Vol) -> Result<(), Error> {
    v.clear();
    let mut blk = [0u8; BLOCK];
    let sb = pick_super(d, &mut blk)?;
    if sb.nblocks != d.nblocks() && d.nblocks() < sb.nblocks {
        return Err(Error::Inval);
    }
    v.nblocks = sb.nblocks;
    v.generation = sb.generation;
    v.inode_root = sb.inode_root;
    v.alloc_root = sb.alloc_root;
    v.next_ino = sb.next_ino;
    v.root_ino = sb.root_ino;
    v.flags = sb.flags;
    v.uuid = sb.uuid;
    v.label = sb.label;
    v.snaps = sb.snaps;
    v.dirty = false;

    d.read_block(v.alloc_root, &mut blk)?;
    load_alloc(v, &blk)?;
    v.mark_meta(v.alloc_root)?;

    d.read_block(v.inode_root, &mut blk)?;
    let kind = blk[4];
    match kind {
        META_INODE_LEAF => {
            check_crc(&blk, 16)?;
            v.mark_meta(v.inode_root)?;
            v.load_inode_leaf(&blk)?;
        }
        META_INODE_INT => {
            parse_meta(&blk, META_INODE_INT)?;
            v.mark_meta(v.inode_root)?;
            let count = le16(&blk, 6) as usize;
            let mut kids = [0u32; 8];
            let mut i = 0usize;
            while i < count && i < 8 {
                kids[i] = le32(&blk, HDR + i * 8 + 4);
                i += 1;
            }
            i = 0;
            while i < count && i < 8 {
                d.read_block(kids[i], &mut blk)?;
                v.mark_meta(kids[i])?;
                v.load_inode_leaf(&blk)?;
                i += 1;
            }
        }
        _ => return Err(Error::Corrupt),
    }

    let mut i = 0usize;
    while i < MAX_INODES {
        if v.inodes[i].used && v.inodes[i].kind == KIND_DIR {
            let root = v.inodes[i].dir_root;
            let ino = v.inodes[i].ino;
            if root != 0 {
                d.read_block(root, &mut blk)?;
                let k = blk[4];
                match k {
                    META_DIR_LEAF => {
                        v.mark_meta(root)?;
                        v.load_dir_leaf(&blk, ino)?;
                    }
                    META_DIR_INT => {
                        parse_meta(&blk, META_DIR_INT)?;
                        v.mark_meta(root)?;
                        let count = le16(&blk, 6) as usize;
                        let mut kids = [0u32; 8];
                        let mut j = 0usize;
                        while j < count && j < 8 {
                            kids[j] = le32(&blk, HDR + j * DENT_REC);
                            j += 1;
                        }
                        j = 0;
                        while j < count && j < 8 {
                            d.read_block(kids[j], &mut blk)?;
                            v.mark_meta(kids[j])?;
                            v.load_dir_leaf(&blk, ino)?;
                            j += 1;
                        }
                    }
                    _ => return Err(Error::Corrupt),
                }
            }
        }
        i += 1;
    }
    if v.inode_slot(v.root_ino).is_err() {
        return Err(Error::Corrupt);
    }
    Ok(())
}
