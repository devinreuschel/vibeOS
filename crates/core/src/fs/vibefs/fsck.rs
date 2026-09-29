use super::*;

/// A class of defect `fsck` reports (docs/VIBEFS.md §11). Every class but
/// `Leak` is an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Defect {
    /// The volume does not mount, an out-of-range inode kind included.
    Mount,
    /// An extent is empty or runs outside the volume.
    Extent,
    /// An extent's data does not match its CRC.
    DataCrc,
    /// A mode's `S_IFMT` bits name another kind than the inode's.
    Mode,
    /// A dirent's kind differs from its inode's.
    Kind,
    /// A dirent names an inode that does not exist.
    Dangling,
    /// The inline flag on an inode larger than `INLINE` bytes.
    Inline,
    /// Two entries of one directory share a name.
    DupName,
    /// A directory's `nlink` differs from the dirents naming it.
    DirNlink,
    /// A non-directory's `nlink` differs from the dirents naming it.
    Nlink,
    /// An inode the root does not reach through dirents.
    Unreachable,
    /// A reachable block whose refcount is 0.
    RefFree,
    /// A reachable block whose bitmap bit is clear.
    BitFree,
    /// An unreachable block with a refcount or its bit set (a warning).
    Leak,
}

impl Defect {
    pub const ALL: [Defect; 14] = [
        Defect::Mount,
        Defect::Extent,
        Defect::DataCrc,
        Defect::Mode,
        Defect::Kind,
        Defect::Dangling,
        Defect::Inline,
        Defect::DupName,
        Defect::DirNlink,
        Defect::Nlink,
        Defect::Unreachable,
        Defect::RefFree,
        Defect::BitFree,
        Defect::Leak,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Defect::Mount => "mount",
            Defect::Extent => "extent",
            Defect::DataCrc => "data-crc",
            Defect::Mode => "mode",
            Defect::Kind => "kind",
            Defect::Dangling => "dangling",
            Defect::Inline => "inline",
            Defect::DupName => "dup-name",
            Defect::DirNlink => "dir-nlink",
            Defect::Nlink => "nlink",
            Defect::Unreachable => "unreachable",
            Defect::RefFree => "ref-free",
            Defect::BitFree => "bit-free",
            Defect::Leak => "leak",
        }
    }

    pub fn is_warning(self) -> bool {
        self == Defect::Leak
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsckReport {
    pub errors: u32,
    pub warnings: u32,
    pub generation: u64,
    /// Defects found, indexed by `Defect as usize`.
    pub counts: [u32; 14],
}

impl FsckReport {
    pub fn count(&self, d: Defect) -> u32 {
        self.counts.get(d as usize).copied().unwrap_or(0)
    }

    fn add(&mut self, d: Defect) {
        if let Some(c) = self.counts.get_mut(d as usize) {
            *c = c.saturating_add(1);
        }
        if d.is_warning() {
            self.warnings = self.warnings.saturating_add(1);
        } else {
            self.errors = self.errors.saturating_add(1);
        }
    }
}

pub fn fsck<D: Disk>(d: &mut D) -> Result<FsckReport, Error> {
    // Host/tests only. A Vol on this stack is ~30KiB; do not call from
    // the kernel (16 KiB stacks).
    let mut r = FsckReport {
        errors: 0,
        warnings: 0,
        generation: 0,
        counts: [0; 14],
    };
    let mut v = Vol::new();
    if mount(d, &mut v).is_err() {
        r.add(Defect::Mount);
        return Ok(r);
    }
    r.generation = v.generation;

    // Inodes: extents, data CRCs, mode and inline flag.
    let mut reached = [false; MAX_BLOCKS];
    reached[0] = true;
    reached[1] = true;
    for &b in &v.meta[..v.nmeta as usize] {
        if let Some(x) = reached.get_mut(b as usize) {
            *x = true;
        }
    }
    let mut i = 0usize;
    while i < MAX_INODES {
        if !v.inodes[i].used {
            i += 1;
            continue;
        }
        let ino = v.inodes[i];
        let mut e = 0usize;
        while e < ino.n_ext as usize {
            let ex = ino.extents[e];
            if ex.len == 0 || ex.phys < 2 || ex.phys as u64 + ex.len as u64 > v.nblocks as u64 {
                r.add(Defect::Extent);
            } else {
                for x in &mut reached[ex.phys as usize..(ex.phys + ex.len) as usize] {
                    *x = true;
                }
                if v.flags & FLAG_DATA_CRC != 0 && v.check_extent(d, ex).is_err() {
                    r.add(Defect::DataCrc);
                }
            }
            e += 1;
        }
        let fmt = ino.mode & crate::fs::S_IFMT;
        if let Ok(k) = kind_of(ino.kind)
            && fmt != 0
            && fmt != k.ifmt()
        {
            r.add(Defect::Mode);
        }
        if ino.flags & F_INLINE != 0 && ino.size > INLINE as u64 {
            r.add(Defect::Inline);
        }
        // Link count against the dirents naming it; the root has none.
        let mut links = 0u32;
        for de in &v.dents {
            if de.used && de.ino == ino.ino {
                links += 1;
            }
        }
        if ino.ino != v.root_ino && links != ino.nlink {
            if ino.kind == KIND_DIR {
                r.add(Defect::DirNlink);
            } else {
                r.add(Defect::Nlink);
            }
        }
        i += 1;
    }

    // Dirents: target, kind, and names unique within a directory.
    let mut j = 0usize;
    while j < MAX_DENTS {
        let de = v.dents[j];
        if !de.used {
            j += 1;
            continue;
        }
        match v.inode_slot(de.ino) {
            Ok(s) => {
                if v.inodes[s].kind != de.kind {
                    r.add(Defect::Kind);
                }
            }
            Err(_) => r.add(Defect::Dangling),
        }
        let mut k = 0usize;
        while k < j {
            let o = &v.dents[k];
            if o.used && o.parent == de.parent && o.name() == de.name() {
                r.add(Defect::DupName);
                break;
            }
            k += 1;
        }
        j += 1;
    }

    // Reachability from the root through dirents, one pass per level.
    let mut live = [false; MAX_INODES];
    if let Ok(s) = v.inode_slot(v.root_ino) {
        live[s] = true;
    }
    let mut pass = 0usize;
    while pass < MAX_INODES {
        let mut changed = false;
        for de in &v.dents {
            if !de.used {
                continue;
            }
            let (Ok(ps), Ok(cs)) = (v.inode_slot(de.parent), v.inode_slot(de.ino)) else {
                continue;
            };
            if live[ps] && v.inodes[ps].kind == KIND_DIR && !live[cs] {
                live[cs] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
        pass += 1;
    }
    i = 0;
    while i < MAX_INODES {
        if v.inodes[i].used && !live[i] {
            r.add(Defect::Unreachable);
        }
        i += 1;
    }

    // Blocks from 2 on: refcount and bitmap against reachability.
    let mut b = 2u32;
    while b < v.nblocks {
        let reach = reached[b as usize];
        let refc = v.refc[b as usize];
        let bit = bit_get(&v.bitmap, b);
        if reach && refc == 0 {
            r.add(Defect::RefFree);
        } else if reach && !bit {
            r.add(Defect::BitFree);
        } else if !reach && (refc > 0 || bit) {
            r.add(Defect::Leak);
        }
        b += 1;
    }
    Ok(r)
}
