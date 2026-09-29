use super::*;

impl FatVol {
    pub fn mount<D: Disk>(d: &mut D) -> Result<Self, FatError> {
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
        let mut vol = Self {
            info,
            cache: [FatSec::EMPTY; FAT_CACHE],
            hint: 2,
            free: 0xFFFFFFFF,
            fsinfo_dirty: false,
            now: 0,
        };
        if info.fsinfo != 0 && info.fsinfo < info.rsvd {
            let mut fs = [0u8; SEC];
            d.read(info.fsinfo, &mut fs)?;
            if le32(&fs, 0) == 0x4161_5252
                && le32(&fs, 484) == 0x6141_7272
                && le32(&fs, 508) == 0xAA55_0000
            {
                let free = le32(&fs, 488);
                let hint = le32(&fs, 492);
                if free != 0xFFFFFFFF {
                    vol.free = free;
                }
                if hint >= 2 && hint < info.nclus + 2 {
                    vol.hint = hint;
                }
            }
        }
        if vol.free == 0xFFFFFFFF {
            vol.free = vol.count_free(d)?;
        }
        Ok(vol)
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

    pub fn walk<D: Disk>(&mut self, d: &mut D, path: &[u8]) -> Result<Node, FatError> {
        if path.is_empty() {
            return Err(FatError::Inval);
        }
        let mut node = self.root();
        let mut stack = [0u32; 16];
        let mut sp = 0usize;
        let mut i = 0usize;
        while i < path.len() && path[i] == b'/' {
            i += 1;
        }
        while i < path.len() {
            let mut j = i;
            while j < path.len() && path[j] != b'/' {
                j += 1;
            }
            let comp = &path[i..j];
            if !comp.is_empty() && !name_is_dot(comp) {
                if name_is_dotdot(comp) {
                    if sp == 0 {
                        node = self.root();
                    } else {
                        sp -= 1;
                        node = self.node_from_clu(stack[sp], InodeKind::Dir, 0, 0, 0)?;
                    }
                } else {
                    if node.kind != InodeKind::Dir {
                        return Err(FatError::NotDir);
                    }
                    if sp < stack.len() {
                        stack[sp] = node.clu;
                        sp += 1;
                    }
                    node = self.lookup(d, node.clu, comp)?;
                }
            }
            while j < path.len() && path[j] == b'/' {
                j += 1;
            }
            i = j;
        }
        Ok(node)
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

    pub fn free_bytes(&self) -> u64 {
        self.free as u64 * self.info.clus_bytes() as u64
    }

    pub fn fats_identical<D: Disk>(&mut self, d: &mut D) -> Result<bool, FatError> {
        if self.info.num_fats < 2 {
            return Ok(true);
        }
        let mut a = [0u8; SEC];
        let mut b = [0u8; SEC];
        let mut sec = 0u32;
        while sec < self.info.fatsz {
            d.read(self.info.fat_lba(0, sec)?, &mut a)?;
            d.read(self.info.fat_lba(1, sec)?, &mut b)?;
            if a != b {
                return Ok(false);
            }
            sec += 1;
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
        let clu = (le16(ent, 20) as u32) << 16 | le16(ent, 26) as u32;
        let size = le32(ent, 28);
        let attr = ent[11];
        let kind = if attr & ATTR_DIR != 0 {
            InodeKind::Dir
        } else {
            InodeKind::Reg
        };
        let mut n = self.node_from_clu(clu, kind, size, dir_clu, dir_off)?;
        n.attr = attr;
        n.mtime = fat_to_unix(le16(ent, 24), le16(ent, 22));
        if !lfn.is_empty() {
            let len = lfn.len().min(MAX_NAME);
            n.name[..len].copy_from_slice(&lfn[..len]);
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
        let mut i = 0usize;
        while i < name.len() {
            let c = name[i];
            if c == 0 || c == b'/' || c < 0x20 {
                return Err(FatError::Inval);
            }
            i += 1;
        }
        Ok(())
    }
}

fn parse_bpb(boot: &[u8; SEC], nsectors: u32) -> Result<FatInfo, FatError> {
    if boot[510] != 0x55 || boot[511] != 0xAA {
        return Err(FatError::Corrupt);
    }
    let bps = le16(boot, 11) as u32;
    if bps != SEC as u32 {
        return Err(FatError::Inval);
    }
    let spc = boot[13];
    if spc == 0 || (spc & (spc - 1)) != 0 {
        return Err(FatError::Inval);
    }
    let rsvd = le16(boot, 14) as u32;
    let num_fats = boot[16];
    if rsvd == 0 || num_fats == 0 || num_fats > 2 {
        return Err(FatError::Inval);
    }
    if le16(boot, 17) != 0 || le16(boot, 22) != 0 {
        return Err(FatError::Inval);
    }
    let tot16 = le16(boot, 19) as u32;
    let tot32 = le32(boot, 32);
    let totsec = if tot16 != 0 { tot16 } else { tot32 };
    if totsec == 0 || totsec > nsectors {
        return Err(FatError::Corrupt);
    }
    let fatsz = le32(boot, 36);
    if fatsz == 0 {
        return Err(FatError::Inval);
    }
    let root_clus = le32(boot, 44);
    if root_clus < 2 {
        return Err(FatError::Corrupt);
    }
    let fsinfo = le16(boot, 48) as u32;
    let backup = le16(boot, 50) as u32;
    let media = boot[21];
    let data_lba = rsvd + num_fats as u32 * fatsz;
    if data_lba >= totsec {
        return Err(FatError::Corrupt);
    }
    let data_secs = totsec - data_lba;
    let nclus = data_secs / spc as u32;
    if nclus < 2 {
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
