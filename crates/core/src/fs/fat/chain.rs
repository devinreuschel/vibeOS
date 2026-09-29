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
            i = i.checked_add(1).ok_or(FatError::Corrupt)?;
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
        let lba = self.info.clus_lba(clu)?;
        let dst = buf.get_mut(..n).ok_or(FatError::Inval)?;
        let (secs, _) = dst.as_chunks_mut::<SEC>();
        for (i, sec) in (0u32..).zip(secs) {
            d.read(lba.checked_add(i).ok_or(FatError::Corrupt)?, sec)?;
        }
        Ok(n)
    }

    pub(super) fn zero_cluster<D: Disk>(&mut self, d: &mut D, clu: u32) -> Result<(), FatError> {
        let z = [0u8; MAX_CLUS_BYTES];
        let n = self.info.clus_bytes();
        self.write_cluster(d, clu, z.get(..n).ok_or(FatError::Corrupt)?)
    }

    pub(super) fn fat_get<D: Disk>(&mut self, d: &mut D, clu: u32) -> Result<u32, FatError> {
        if self.info.past_end(clu) {
            return Err(FatError::Corrupt);
        }
        let (sec, ent_off) = fat_loc(clu)?;
        let c = self.fat_sec(d, sec)?;
        Ok(le32(&c.data, ent_off)? & 0x0FFF_FFFF)
    }

    /// The cached FAT sector `sec`, loaded when it is not cached.
    pub(super) fn fat_sec<D: Disk>(
        &mut self,
        d: &mut D,
        sec: u32,
    ) -> Result<&mut FatSec, FatError> {
        let s = self.fat_cache(d, sec)?;
        self.cache.get_mut(s).ok_or(FatError::Corrupt)
    }

    pub(super) fn fat_cache<D: Disk>(&mut self, d: &mut D, sec: u32) -> Result<usize, FatError> {
        if sec >= self.info.fatsz {
            return Err(FatError::Corrupt);
        }
        if let Some(i) = self.cache.iter().position(|c| c.used && c.idx == sec) {
            return Ok(i);
        }
        if let Some(i) = self.cache.iter().position(|c| !c.used) {
            return self.fat_load(d, i, sec);
        }
        if let Some(i) = self.cache.iter().position(|c| !c.dirty) {
            return self.fat_load(d, i, sec);
        }
        self.flush_one_fat(d, 0)?;
        self.fat_load(d, 0, sec)
    }

    fn fat_load<D: Disk>(&mut self, d: &mut D, slot: usize, sec: u32) -> Result<usize, FatError> {
        if self.cache.get(slot).ok_or(FatError::Corrupt)?.dirty {
            self.flush_one_fat(d, slot)?;
        }
        let lba = self.info.fat_lba(0, sec)?;
        let c = self.cache.get_mut(slot).ok_or(FatError::Corrupt)?;
        d.read(lba, &mut c.data)?;
        c.used = true;
        c.dirty = false;
        c.idx = sec;
        Ok(slot)
    }

    fn flush_one_fat<D: Disk>(&mut self, d: &mut D, slot: usize) -> Result<(), FatError> {
        let c = self.cache.get(slot).ok_or(FatError::Corrupt)?;
        if !c.used || !c.dirty {
            return Ok(());
        }
        for copy in 0..self.info.num_fats {
            let lba = self.info.fat_lba(copy, c.idx)?;
            d.write(lba, &c.data)?;
        }
        self.cache.get_mut(slot).ok_or(FatError::Corrupt)?.dirty = false;
        Ok(())
    }

    pub(super) fn commit_fat<D: Disk>(&mut self, d: &mut D) -> Result<(), FatError> {
        for i in 0..FAT_CACHE {
            self.flush_one_fat(d, i)?;
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
        put_le32(&mut fs, 0, 0x4161_5252)?;
        put_le32(&mut fs, 484, 0x6141_7272)?;
        put_le32(&mut fs, 488, self.free)?;
        put_le32(&mut fs, 492, self.hint)?;
        put_le32(&mut fs, 508, 0xAA55_0000)?;
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
            n = n.checked_add(1).ok_or(FatError::Corrupt)?;
            if n > self.info.nclus {
                return Err(FatError::Corrupt);
            }
        }
        Ok(())
    }

    pub(super) fn count_free<D: Disk>(&mut self, d: &mut D) -> Result<u32, FatError> {
        let mut n = 0u32;
        for i in 0..self.info.nclus {
            let c = i.checked_add(2).ok_or(FatError::Corrupt)?;
            if self.fat_get(d, c)? == 0 {
                n = n.checked_add(1).ok_or(FatError::Corrupt)?;
            }
        }
        Ok(n)
    }
}

/// The FAT sector holding cluster `clu`'s entry, and the entry's offset in
/// it; `Corrupt` for a cluster number whose byte offset overflows.
pub(super) fn fat_loc(clu: u32) -> Result<(u32, usize), FatError> {
    let off = clu.checked_mul(4).ok_or(FatError::Corrupt)? as usize;
    Ok(((off / SEC) as u32, off % SEC))
}

pub(super) fn is_eoc(v: u32) -> bool {
    v >= EOC_MIN
}
