use super::*;

impl FatVol {
    pub(super) fn last_clu<D: Disk>(&mut self, d: &mut D, first: u32) -> Result<u32, FatError> {
        let mut clu = first;
        let mut i = 0u32;
        loop {
            let next = self.fat_get(d, clu)?;
            if is_eoc(next) {
                return Ok(clu);
            }
            if next < 2 || next == BAD_CLUS {
                return Err(FatError::Corrupt);
            }
            clu = next;
            i += 1;
            if i > self.info.nclus {
                return Err(FatError::Corrupt);
            }
        }
    }

    pub(super) fn read_cluster<D: Disk>(
        &mut self,
        d: &mut D,
        clu: u32,
        buf: &mut [u8],
    ) -> Result<usize, FatError> {
        let n = self.info.clus_bytes();
        if buf.len() < n {
            return Err(FatError::Inval);
        }
        let lba = self.info.clus_lba(clu)?;
        let mut i = 0u32;
        while i < self.info.spc as u32 {
            let off = i as usize * self.info.bps as usize;
            d.read(lba + i, &mut buf[off..off + self.info.bps as usize])?;
            i += 1;
        }
        Ok(n)
    }

    pub(super) fn zero_cluster<D: Disk>(&mut self, d: &mut D, clu: u32) -> Result<(), FatError> {
        let z = [0u8; MAX_CLUS_BYTES];
        let n = self.info.clus_bytes();
        self.write_cluster(d, clu, &z[..n])
    }

    pub(super) fn fat_get<D: Disk>(&mut self, d: &mut D, clu: u32) -> Result<u32, FatError> {
        if clu >= self.info.nclus + 2 {
            return Err(FatError::Corrupt);
        }
        let (sec, ent_off) = fat_loc(clu);
        let s = self.fat_cache(d, sec)?;
        Ok(le32(&self.cache[s].data, ent_off) & 0x0FFF_FFFF)
    }

    pub(super) fn fat_cache<D: Disk>(&mut self, d: &mut D, sec: u32) -> Result<usize, FatError> {
        if sec >= self.info.fatsz {
            return Err(FatError::Corrupt);
        }
        let mut i = 0usize;
        while i < FAT_CACHE {
            if self.cache[i].used && self.cache[i].idx == sec {
                return Ok(i);
            }
            i += 1;
        }
        i = 0;
        while i < FAT_CACHE {
            if !self.cache[i].used {
                return self.fat_load(d, i, sec);
            }
            i += 1;
        }
        let mut i = 0usize;
        while i < FAT_CACHE {
            if !self.cache[i].dirty {
                return self.fat_load(d, i, sec);
            }
            i += 1;
        }
        self.flush_one_fat(d, 0)?;
        self.fat_load(d, 0, sec)
    }

    fn fat_load<D: Disk>(&mut self, d: &mut D, slot: usize, sec: u32) -> Result<usize, FatError> {
        if self.cache[slot].dirty {
            self.flush_one_fat(d, slot)?;
        }
        let lba = self.info.fat_lba(0, sec)?;
        d.read(lba, &mut self.cache[slot].data)?;
        self.cache[slot].used = true;
        self.cache[slot].dirty = false;
        self.cache[slot].idx = sec;
        Ok(slot)
    }

    fn flush_one_fat<D: Disk>(&mut self, d: &mut D, slot: usize) -> Result<(), FatError> {
        if !self.cache[slot].used || !self.cache[slot].dirty {
            return Ok(());
        }
        let sec = self.cache[slot].idx;
        let mut copy = 0u8;
        while copy < self.info.num_fats {
            let lba = self.info.fat_lba(copy, sec)?;
            d.write(lba, &self.cache[slot].data)?;
            copy += 1;
        }
        self.cache[slot].dirty = false;
        Ok(())
    }

    pub(super) fn commit_fat<D: Disk>(&mut self, d: &mut D) -> Result<(), FatError> {
        let mut i = 0usize;
        while i < FAT_CACHE {
            self.flush_one_fat(d, i)?;
            i += 1;
        }
        if self.fsinfo_dirty {
            self.write_fsinfo(d)?;
            self.fsinfo_dirty = false;
        }
        Ok(())
    }

    fn write_fsinfo<D: Disk>(&mut self, d: &mut D) -> Result<(), FatError> {
        if self.info.fsinfo == 0 {
            return Ok(());
        }
        let mut fs = [0u8; SEC];
        put_le32(&mut fs, 0, 0x4161_5252);
        put_le32(&mut fs, 484, 0x6141_7272);
        put_le32(&mut fs, 488, self.free);
        put_le32(&mut fs, 492, self.hint);
        put_le32(&mut fs, 508, 0xAA55_0000);
        d.write(self.info.fsinfo, &fs)?;
        if self.info.backup != 0 {
            let b = self.info.backup.saturating_add(1);
            if b != self.info.fsinfo && b < self.info.rsvd {
                d.write(b, &fs)?;
            }
        }
        Ok(())
    }

    /// Free the chain at `first_clu` of a file nothing holds any more,
    /// as the last close of an unlinked file does, and commit the FAT.
    pub fn free_chain<D: Disk>(&mut self, d: &mut D, first_clu: u32) -> Result<(), FatError> {
        if first_clu < 2 {
            return Ok(());
        }
        self.release_chain(d, first_clu)?;
        self.commit_fat(d)?;
        d.flush()
    }

    fn release_chain<D: Disk>(&mut self, d: &mut D, mut clu: u32) -> Result<(), FatError> {
        let mut n = 0u32;
        while clu >= 2 && !is_eoc(clu) {
            let next = self.fat_get(d, clu)?;
            self.fat_set(d, clu, 0)?;
            clu = next;
            n += 1;
            if n > self.info.nclus {
                return Err(FatError::Corrupt);
            }
        }
        Ok(())
    }

    pub(super) fn count_free<D: Disk>(&mut self, d: &mut D) -> Result<u32, FatError> {
        let mut n = 0u32;
        let mut c = 2u32;
        while c < self.info.nclus + 2 {
            if self.fat_get(d, c)? == 0 {
                n += 1;
            }
            c += 1;
        }
        Ok(n)
    }
}

pub(super) fn fat_loc(clu: u32) -> (u32, usize) {
    let off = clu * 4;
    (off / SEC as u32, (off as usize) % SEC)
}

pub(super) fn is_eoc(v: u32) -> bool {
    v >= EOC_MIN
}
