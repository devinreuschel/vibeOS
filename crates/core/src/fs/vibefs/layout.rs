use super::*;

#[derive(Clone, Copy)]
pub struct Extent {
    pub log: u32,
    pub phys: u32,
    pub len: u32,
    pub crc: u32,
}

impl Extent {
    pub(super) const EMPTY: Self = Self {
        log: 0,
        phys: 0,
        len: 0,
        crc: 0,
    };
}

#[derive(Clone, Copy)]
pub struct Snap {
    pub used: bool,
    pub name: [u8; 16],
    pub generation: u64,
    pub inode_root: u32,
    pub alloc_root: u32,
    pub next_ino: u32,
}

impl Snap {
    pub(super) const EMPTY: Self = Self {
        used: false,
        name: [0; 16],
        generation: 0,
        inode_root: 0,
        alloc_root: 0,
        next_ino: 0,
    };
}

/// An inode as a lookup reports it, in memory only. Its times are the
/// record's (VIBEFS.md §7), with a 0 (unset) atime or ctime reported as
/// the mtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    pub ino: u32,
    pub kind: InodeKind,
    pub size: u64,
    pub mode: u16,
    pub nlink: u32,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub name_len: u8,
    pub name: [u8; MAX_NAME],
}

impl Node {
    pub const EMPTY: Self = Self {
        ino: 0,
        kind: InodeKind::Reg,
        size: 0,
        mode: 0,
        nlink: 0,
        atime: 0,
        mtime: 0,
        ctime: 0,
        name_len: 0,
        name: [0; MAX_NAME],
    };

    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }

    pub fn is_dir(self) -> bool {
        self.kind == InodeKind::Dir
    }
}

pub(super) fn pack_inode(buf: &mut [u8], rec: &Inode) {
    buf[..INODE_REC].fill(0);
    put32(buf, 0, rec.ino);
    buf[4] = rec.kind;
    buf[5] = rec.flags;
    put16(buf, 6, rec.mode);
    put32(buf, 8, rec.nlink);
    put32(buf, 12, rec.uid);
    put32(buf, 16, rec.gid);
    put64(buf, 20, rec.size);
    put64(buf, 28, rec.atime);
    put64(buf, 36, rec.mtime);
    put64(buf, 44, rec.ctime);
    put32(buf, 52, rec.dir_root);
    buf[56] = rec.n_ext;
    buf[57] = rec.inline_len;
    let mut i = 0usize;
    while i < MAX_EXT {
        let o = 60 + i * 16;
        put32(buf, o, rec.extents[i].log);
        put32(buf, o + 4, rec.extents[i].phys);
        put32(buf, o + 8, rec.extents[i].len);
        put32(buf, o + 12, rec.extents[i].crc);
        i += 1;
    }
    buf[128..128 + INLINE].copy_from_slice(&rec.inline_data);
}

pub(super) fn unpack_inode(buf: &[u8]) -> Result<Inode, Error> {
    let mut rec = Inode::EMPTY;
    rec.used = true;
    rec.ino = le32(buf, 0);
    rec.kind = buf[4];
    rec.flags = buf[5];
    rec.mode = le16(buf, 6);
    rec.nlink = le32(buf, 8);
    rec.uid = le32(buf, 12);
    rec.gid = le32(buf, 16);
    rec.size = le64(buf, 20);
    rec.atime = le64(buf, 28);
    rec.mtime = le64(buf, 36);
    rec.ctime = le64(buf, 44);
    rec.dir_root = le32(buf, 52);
    rec.n_ext = buf[56];
    rec.inline_len = buf[57];
    if rec.n_ext as usize > MAX_EXT || rec.inline_len as usize > INLINE {
        return Err(Error::Corrupt);
    }
    let _ = kind_of(rec.kind)?;
    let mut i = 0usize;
    while i < MAX_EXT {
        let o = 60 + i * 16;
        rec.extents[i] = Extent {
            log: le32(buf, o),
            phys: le32(buf, o + 4),
            len: le32(buf, o + 8),
            crc: le32(buf, o + 12),
        };
        i += 1;
    }
    rec.inline_data.copy_from_slice(&buf[128..128 + INLINE]);
    Ok(rec)
}

pub(super) fn meta_hdr(buf: &mut [u8; BLOCK], kind: u8, level: u8, count: u16, g: u64, owner: u32) {
    buf.fill(0);
    put32(buf, 0, MAGIC_META);
    buf[4] = kind;
    buf[5] = level;
    put16(buf, 6, count);
    put64(buf, 8, g);
    put32(buf, 16, 0);
    put32(buf, 20, owner);
}

pub(super) fn finish_meta(buf: &mut [u8; BLOCK]) {
    set_crc(buf, 16);
}

pub(super) fn parse_meta(buf: &[u8; BLOCK], want: u8) -> Result<(u8, u16, u32), Error> {
    if le32(buf, 0) != MAGIC_META {
        return Err(Error::Corrupt);
    }
    check_crc(buf, 16)?;
    if buf[4] != want {
        return Err(Error::Corrupt);
    }
    Ok((buf[5], le16(buf, 6), le32(buf, 20)))
}

/// What a superblock records, borrowed apart from the buffer it is packed
/// into, so a commit can pack it into the volume's own `iobuf`.
struct SuperParts<'a> {
    flags: u8,
    nblocks: u32,
    generation: u64,
    inode_root: u32,
    alloc_root: u32,
    next_ino: u32,
    root_ino: u32,
    uuid: &'a [u8; 16],
    label: &'a [u8; 32],
    snaps: &'a [Snap; MAX_SNAPS],
}

pub(super) fn pack_super(buf: &mut [u8; BLOCK], v: &Vol, slot: u8) {
    let p = SuperParts {
        flags: v.flags,
        nblocks: v.nblocks,
        generation: v.generation,
        inode_root: v.inode_root,
        alloc_root: v.alloc_root,
        next_ino: v.next_ino,
        root_ino: v.root_ino,
        uuid: &v.uuid,
        label: &v.label,
        snaps: &v.snaps,
    };
    pack_super_parts(buf, &p, slot);
}

fn pack_super_parts(buf: &mut [u8; BLOCK], v: &SuperParts<'_>, slot: u8) {
    buf.fill(0);
    put32(buf, 0, MAGIC_SUPER);
    put16(buf, 4, VERSION);
    buf[6] = slot;
    buf[7] = v.flags;
    put32(buf, 8, BLOCK as u32);
    put32(buf, 12, v.nblocks);
    put64(buf, 16, v.generation);
    put32(buf, 24, v.inode_root);
    put32(buf, 28, v.alloc_root);
    put32(buf, 32, v.next_ino);
    put32(buf, 36, v.root_ino);
    buf[40..56].copy_from_slice(v.uuid);
    buf[56..88].copy_from_slice(v.label);
    let mut ns = 0u8;
    let mut i = 0usize;
    while i < MAX_SNAPS {
        if v.snaps[i].used {
            ns += 1;
        }
        i += 1;
    }
    buf[88] = ns;
    i = 0;
    while i < MAX_SNAPS {
        let o = 96 + i * 40;
        buf[o..o + 16].copy_from_slice(&v.snaps[i].name);
        put64(buf, o + 16, v.snaps[i].generation);
        put32(buf, o + 24, v.snaps[i].inode_root);
        put32(buf, o + 28, v.snaps[i].alloc_root);
        put32(buf, o + 32, v.snaps[i].next_ino);
        i += 1;
    }
    set_crc(buf, SB_CRC_OFF);
}

fn parse_super(buf: &[u8; BLOCK], slot: u8) -> Result<SuperInfo, Error> {
    if le32(buf, 0) != MAGIC_SUPER {
        return Err(Error::Corrupt);
    }
    check_crc(buf, SB_CRC_OFF)?;
    if le16(buf, 4) != VERSION {
        return Err(Error::Inval);
    }
    if buf[6] != slot {
        return Err(Error::Corrupt);
    }
    if le32(buf, 8) != BLOCK as u32 {
        return Err(Error::Inval);
    }
    let nblocks = le32(buf, 12);
    if nblocks < MIN_BLOCKS || nblocks as usize > MAX_BLOCKS {
        return Err(Error::Inval);
    }
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&buf[40..56]);
    let mut label = [0u8; 32];
    label.copy_from_slice(&buf[56..88]);
    let mut snaps = [Snap::EMPTY; MAX_SNAPS];
    let ns = buf[88] as usize;
    if ns > MAX_SNAPS {
        return Err(Error::Corrupt);
    }
    let mut i = 0usize;
    while i < MAX_SNAPS {
        let o = 96 + i * 40;
        snaps[i].name.copy_from_slice(&buf[o..o + 16]);
        snaps[i].generation = le64(buf, o + 16);
        snaps[i].inode_root = le32(buf, o + 24);
        snaps[i].alloc_root = le32(buf, o + 28);
        snaps[i].next_ino = le32(buf, o + 32);
        snaps[i].used = i < ns && snaps[i].inode_root != 0;
        i += 1;
    }
    Ok(SuperInfo {
        generation: le64(buf, 16),
        nblocks,
        inode_root: le32(buf, 24),
        alloc_root: le32(buf, 28),
        next_ino: le32(buf, 32),
        root_ino: le32(buf, 36),
        flags: buf[7],
        uuid,
        label,
        snaps,
    })
}

/// The newer valid superblock of the two slots. A slot that does not
/// parse is a torn or never-written one and is passed over; a slot that
/// cannot be read fails the pick, since it may hold the newer generation,
/// and mounting the older one would roll the volume back.
pub(super) fn pick_super<D: Disk>(d: &mut D, buf: &mut [u8; BLOCK]) -> Result<SuperInfo, Error> {
    let mut best: Option<SuperInfo> = None;
    let mut slot = 0u8;
    while slot < 2 {
        d.read_block(slot as u32, buf)?;
        if let Ok(s) = parse_super(buf, slot) {
            let take = match &best {
                None => true,
                Some(b) => {
                    s.generation > b.generation || (s.generation == b.generation && slot == 0)
                }
            };
            if take {
                best = Some(s);
            }
        }
        slot += 1;
    }
    best.ok_or(Error::Corrupt)
}

pub fn probe<D: Disk>(d: &mut D) -> bool {
    let mut buf = [0u8; BLOCK];
    pick_super(d, &mut buf).is_ok()
}

/// Serialize the alloc map with `old_meta` and the drop list already
/// dropped, so the map a commit writes matches memory after step 7.
pub(super) fn write_alloc_into(v: &Vol, old_meta: &[u32], buf: &mut [u8; BLOCK]) {
    let drop = &v.drop[..v.ndrop as usize];
    alloc_into(
        buf,
        v.nblocks,
        v.generation,
        &v.bitmap,
        &v.refc,
        drop,
        old_meta,
    );
}

/// The alloc map of a volume whose fields are borrowed apart from `buf`.
fn alloc_into(
    buf: &mut [u8; BLOCK],
    nblocks: u32,
    generation: u64,
    bitmap_src: &[u8],
    refc_src: &[u8],
    dropped: &[u32],
    old_meta: &[u32],
) {
    let nbytes = (nblocks as usize).div_ceil(8);
    meta_hdr(buf, META_ALLOC, 0, nblocks as u16, generation, 0);
    buf[HDR..HDR + nbytes].copy_from_slice(&bitmap_src[..nbytes]);
    buf[HDR + nbytes..HDR + nbytes + nblocks as usize]
        .copy_from_slice(&refc_src[..nblocks as usize]);
    {
        let (head, rest) = buf.split_at_mut(HDR + nbytes);
        let bitmap = &mut head[HDR..];
        let refc = &mut rest[..nblocks as usize];
        for &b in old_meta.iter().chain(dropped.iter()) {
            drop_ref(bitmap, refc, nblocks, b);
        }
    }
    finish_meta(buf);
}

impl Vol {
    /// [`write_alloc_into`] into the volume's own `iobuf`, with no block
    /// buffer on the stack (a commit runs on a 16 KiB kernel stack).
    pub(super) fn alloc_into_iobuf(&mut self, old_meta: &[u32]) {
        let Vol {
            iobuf,
            nblocks,
            generation,
            bitmap,
            refc,
            drop,
            ndrop,
            ..
        } = self;
        let dropped = &drop[..*ndrop as usize];
        alloc_into(
            iobuf,
            *nblocks,
            *generation,
            bitmap,
            refc,
            dropped,
            old_meta,
        );
    }

    /// [`pack_super`] into the volume's own `iobuf`.
    pub(super) fn super_into_iobuf(&mut self, slot: u8) {
        let Vol {
            iobuf,
            flags,
            nblocks,
            generation,
            inode_root,
            alloc_root,
            next_ino,
            root_ino,
            uuid,
            label,
            snaps,
            ..
        } = self;
        let p = SuperParts {
            flags: *flags,
            nblocks: *nblocks,
            generation: *generation,
            inode_root: *inode_root,
            alloc_root: *alloc_root,
            next_ino: *next_ino,
            root_ino: *root_ino,
            uuid,
            label,
            snaps,
        };
        pack_super_parts(iobuf, &p, slot);
    }
}

/// Load the alloc map in `v.iobuf`.
pub(super) fn load_alloc(v: &mut Vol) -> Result<(), Error> {
    let Vol {
        iobuf: buf,
        nblocks,
        bitmap,
        refc,
        ..
    } = v;
    parse_meta(buf, META_ALLOC)?;
    let nbytes = (*nblocks as usize).div_ceil(8);
    if HDR + nbytes + *nblocks as usize > BLOCK {
        return Err(Error::Corrupt);
    }
    bitmap[..nbytes].copy_from_slice(&buf[HDR..HDR + nbytes]);
    refc[..*nblocks as usize].copy_from_slice(&buf[HDR + nbytes..HDR + nbytes + *nblocks as usize]);
    Ok(())
}
