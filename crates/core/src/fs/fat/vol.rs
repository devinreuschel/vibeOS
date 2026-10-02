use super::*;

/// The [`FatInfo`] of a volume nothing is mounted in.
const NO_INFO: FatInfo = FatInfo {
    bps: 0,
    spc: 0,
    rsvd: 0,
    num_fats: 0,
    fatsz: 0,
    totsec: 0,
    root_clus: 0,
    fsinfo: 0,
    backup: 0,
    data_lba: 0,
    nclus: 0,
    media: 0,
};

impl Default for FatVol {
    fn default() -> Self {
        Self::new()
    }
}

impl FatVol {
    /// A volume with nothing mounted, for a slot to [`Self::mount_in`].
    pub const fn new() -> Self {
        Self {
            info: NO_INFO,
            cache: [FatSec::EMPTY; FAT_CACHE],
            hint: 2,
            free: 0xFFFFFFFF,
            fsinfo_dirty: false,
            now: 0,
            clbuf: [0; MAX_CLUS_BYTES],
        }
    }

    /// Reset to [`Self::new`]'s state in place. Never `*v = FatVol::new()`:
    /// that builds the whole volume, cluster buffer and FAT cache, as a
    /// temporary on the kernel stack (DESIGN §4.5).
    pub fn clear(&mut self) {
        self.info = NO_INFO;
        for c in self.cache.iter_mut() {
            c.used = false;
            c.dirty = false;
            c.idx = 0;
            c.data.fill(0);
        }
        self.hint = 2;
        self.free = 0xFFFFFFFF;
        self.fsinfo_dirty = false;
        self.now = 0;
        self.clbuf.fill(0);
    }

    /// Mount the volume on `d` by value: [`Self::new`] and
    /// [`Self::mount_in`], for host code and the boot-only initrd builder.
    /// The kernel mounts into its slot with `mount_in`.
    pub fn mount<D: Disk>(d: &mut D) -> Result<Self, FatError> {
        let mut vol = Self::new();
        vol.mount_in(d)?;
        Ok(vol)
    }

    /// Mount the volume on `d` into `self`, in place: [`Self::clear`], then
    /// the BPB and FSInfo. On an error `self` is left cleared or partly
    /// filled, and mounts nothing the caller may use.
    pub fn mount_in<D: Disk>(&mut self, d: &mut D) -> Result<(), FatError> {
        self.clear();
        let ss = d.sector_size();
        if ss != SEC as u32 {
            return Err(FatError::Inval);
        }
        let mut boot = [0u8; SEC];
        d.read(0, &mut boot)?;
        let info = parse_bpb(&boot, d.nsectors())?;
        if info.clus_bytes() == 0 || info.clus_bytes() > MAX_CLUS_BYTES {
            return Err(FatError::Inval);
        }
        self.info = info;
        let vol = self;
        // FSInfo, and its backup copy, are written back only to a sector
        // inside the reserved area that holds a valid FSInfo now: a BPB's
        // sector numbers are the image's, and could name a FAT or data
        // sector that the write-back would overwrite.
        let mut fs = [0u8; SEC];
        if info.fsinfo != 0 && info.fsinfo < info.rsvd {
            d.read(info.fsinfo, &mut fs)?;
        }
        if info.fsinfo != 0 && info.fsinfo < info.rsvd && fsinfo_valid(&fs)? {
            let free = le32(&fs, 488)?;
            let hint = le32(&fs, 492)?;
            if free != 0xFFFFFFFF {
                vol.free = free;
            }
            if hint >= 2 && !info.past_end(hint) {
                vol.hint = hint;
            }
        } else {
            vol.info.fsinfo = 0;
        }
        let b = info.backup.saturating_add(1);
        if vol.info.fsinfo == 0 || info.backup == 0 || b == info.fsinfo || b >= info.rsvd {
            vol.info.backup = 0;
        } else {
            d.read(b, &mut fs)?;
            if !fsinfo_valid(&fs)? {
                vol.info.backup = 0;
            }
        }
        if vol.free == 0xFFFFFFFF {
            vol.free = vol.count_free(d)?;
        }
        Ok(())
    }

    pub fn root(&self) -> Node {
        let mut n = Node::EMPTY;
        n.ino = ROOT_INO;
        n.kind = InodeKind::Dir;
        n.clu = self.info.root_clus;
        n.attr = ATTR_DIR;
        n.name_len = 1;
        n.name[0] = b'/';
        n
    }

    pub fn lookup<D: Disk>(&mut self, d: &mut D, dir: u32, name: &[u8]) -> Result<Node, FatError> {
        if name_is_dot(name) {
            return self.node_from_clu(dir, InodeKind::Dir, 0, 0, 0);
        }
        if name_is_dotdot(name) {
            return Err(FatError::Inval);
        }
        let mut off = 0u32;
        loop {
            match self.read_dirent(d, dir, off)? {
                None => return Err(FatError::NotFound),
                Some((next, node)) => {
                    if eq_ci(node.name(), name) {
                        return Ok(node);
                    }
                    off = next;
                }
            }
        }
    }

    pub fn readdir<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        cookie: u64,
        out: &mut Node,
    ) -> Result<Option<u64>, FatError> {
        match self.read_dirent(d, dir, cookie as u32)? {
            None => Ok(None),
            Some((next, node)) => {
                *out = node;
                Ok(Some(next as u64))
            }
        }
    }

    pub fn sync<D: Disk>(&mut self, d: &mut D) -> Result<(), FatError> {
        self.commit_fat(d)?;
        d.flush()
    }

    /// Free space in bytes; saturates only for a `FatInfo` that
    /// `parse_bpb` never builds, whose cluster size is not a sector count.
    pub fn free_bytes(&self) -> u64 {
        u64::from(self.free).saturating_mul(self.info.clus_bytes() as u64)
    }

    pub fn fats_identical<D: Disk>(&mut self, d: &mut D) -> Result<bool, FatError> {
        if self.info.num_fats < 2 {
            return Ok(true);
        }
        let mut a = [0u8; SEC];
        let mut b = [0u8; SEC];
        for sec in 0..self.info.fatsz {
            d.read(self.info.fat_lba(0, sec)?, &mut a)?;
            d.read(self.info.fat_lba(1, sec)?, &mut b)?;
            if a != b {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn node_from_clu(
        &mut self,
        clu: u32,
        kind: InodeKind,
        size: u32,
        dir_clu: u32,
        dir_off: u32,
    ) -> Result<Node, FatError> {
        let mut n = Node::EMPTY;
        n.ino = stat_ino(dir_clu, dir_off);
        n.kind = kind;
        n.clu = clu;
        n.size = size;
        n.dir_clu = dir_clu;
        n.dir_off = dir_off;
        Ok(n)
    }

    pub(super) fn node_from_short(
        &mut self,
        dir_clu: u32,
        dir_off: u32,
        ent: &[u8; ENT],
        lfn: &[u8],
    ) -> Result<Node, FatError> {
        let clu = (le16(ent, 20)? as u32) << 16 | le16(ent, 26)? as u32;
        let size = le32(ent, 28)?;
        let attr = ent[11];
        let kind = if attr & ATTR_DIR != 0 {
            InodeKind::Dir
        } else {
            InodeKind::Reg
        };
        let mut n = self.node_from_clu(clu, kind, size, dir_clu, dir_off)?;
        n.attr = attr;
        n.mtime = fat_to_unix(le16(ent, 24)?, le16(ent, 22)?);
        if !lfn.is_empty() {
            let len = lfn.len().min(MAX_NAME);
            let src = lfn.get(..len).ok_or(FatError::Corrupt)?;
            n.name
                .get_mut(..len)
                .ok_or(FatError::Corrupt)?
                .copy_from_slice(src);
            n.name_len = len as u8;
        } else {
            let (nm, nl) = decode_short(ent);
            n.name = nm;
            n.name_len = nl;
        }
        Ok(n)
    }

    pub(super) fn check_name(&self, name: &[u8]) -> Result<(), FatError> {
        if name.is_empty() || name.len() > MAX_NAME {
            return Err(FatError::NameTooLong);
        }
        if name_is_dot(name) || name_is_dotdot(name) {
            return Err(FatError::Inval);
        }
        if name.iter().any(|&c| c < 0x20 || LFN_ILLEGAL.contains(&c)) {
            return Err(FatError::Inval);
        }
        core::str::from_utf8(name).map_err(|_| FatError::Inval)?;
        Ok(())
    }
}

/// The characters a long name may not hold, with `/` and controls.
const LFN_ILLEGAL: &[u8] = b"/\"*:<>?\\|";

/// The most clusters a FAT32 volume holds: numbers 2 to `0x0FFF_FFF6`.
const MAX_NCLUS: u32 = 0x0FFF_FFF5;

/// Whether `fs` holds FSInfo's three signatures (Microsoft's FAT
/// specification, "FAT32 FSInfo Sector Structure").
fn fsinfo_valid(fs: &[u8; SEC]) -> Result<bool, FatError> {
    Ok(le32(fs, 0)? == 0x4161_5252
        && le32(fs, 484)? == 0x6141_7272
        && le32(fs, 508)? == 0xAA55_0000)
}

fn parse_bpb(boot: &[u8; SEC], nsectors: u32) -> Result<FatInfo, FatError> {
    if boot[510] != 0x55 || boot[511] != 0xAA {
        return Err(FatError::Corrupt);
    }
    let bps = le16(boot, 11)? as u32;
    if bps != SEC as u32 {
        return Err(FatError::Inval);
    }
    let spc = boot[13];
    if !spc.is_power_of_two() {
        return Err(FatError::Inval);
    }
    let rsvd = le16(boot, 14)? as u32;
    let num_fats = boot[16];
    if rsvd == 0 || num_fats == 0 || num_fats > 2 {
        return Err(FatError::Inval);
    }
    if le16(boot, 17)? != 0 || le16(boot, 22)? != 0 {
        return Err(FatError::Inval);
    }
    let tot16 = le16(boot, 19)? as u32;
    let tot32 = le32(boot, 32)?;
    let totsec = if tot16 != 0 { tot16 } else { tot32 };
    if totsec == 0 || totsec > nsectors {
        return Err(FatError::Corrupt);
    }
    let fatsz = le32(boot, 36)?;
    if fatsz == 0 {
        return Err(FatError::Inval);
    }
    let root_clus = le32(boot, 44)?;
    if root_clus < 2 {
        return Err(FatError::Corrupt);
    }
    let fsinfo = le16(boot, 48)? as u32;
    let backup = le16(boot, 50)? as u32;
    let media = boot[21];
    let data_lba = u32::from(num_fats)
        .checked_mul(fatsz)
        .and_then(|f| f.checked_add(rsvd))
        .ok_or(FatError::Corrupt)?;
    let data_secs = match totsec.checked_sub(data_lba) {
        Some(n) if n > 0 => n,
        _ => return Err(FatError::Corrupt),
    };
    let nclus = data_secs
        .checked_div(u32::from(spc))
        .ok_or(FatError::Corrupt)?;
    // The cap also keeps `fat_loc`'s byte offset `clu * 4` inside a `u32`.
    if !(2..=MAX_NCLUS).contains(&nclus) {
        return Err(FatError::Corrupt);
    }
    Ok(FatInfo {
        bps,
        spc,
        rsvd,
        num_fats,
        fatsz,
        totsec,
        root_clus,
        fsinfo,
        backup,
        data_lba,
        nclus,
        media,
    })
}
