use super::*;

pub fn mkfs<D: Disk>(d: &mut D, label: &[u8], v: &mut Vol) -> Result<(), Error> {
    let nblocks = d.nblocks();
    if nblocks < MIN_BLOCKS || nblocks as usize > MAX_BLOCKS {
        return Err(Error::Inval);
    }
    v.clear();
    v.nblocks = nblocks;
    v.generation = 1;
    v.flags = FLAG_DATA_CRC;
    v.root_ino = ROOT_INO;
    v.next_ino = 2;
    let n = label.len().min(32);
    v.label[..n].copy_from_slice(&label[..n]);
    v.uuid[0] = 0x76;
    v.uuid[1] = 0x31;
    put32(&mut v.uuid, 4, nblocks);

    v.refc[0] = 1;
    v.refc[1] = 1;
    bit_set(&mut v.bitmap, 0, true);
    bit_set(&mut v.bitmap, 1, true);

    let alloc_bno = 2u32;
    let leaf = 3u32;
    v.refc[2] = 1;
    v.refc[3] = 1;
    bit_set(&mut v.bitmap, 2, true);
    bit_set(&mut v.bitmap, 3, true);
    v.alloc_root = alloc_bno;
    v.inode_root = leaf;

    v.inodes[0] = Inode {
        used: true,
        ino: ROOT_INO,
        kind: KIND_DIR,
        flags: 0,
        mode: 0o755,
        nlink: 1,
        uid: 0,
        gid: 0,
        size: 0,
        atime: 0,
        mtime: 0,
        ctime: 0,
        dir_root: 0,
        n_ext: 0,
        inline_len: 0,
        extents: [Extent::EMPTY; MAX_EXT],
        inline_data: [0; INLINE],
    };

    meta_hdr(&mut v.iobuf, META_INODE_LEAF, 0, 1, 1, 0);
    pack_inode(&mut v.iobuf[HDR..], &v.inodes[0]);
    finish_meta(&mut v.iobuf);
    d.write_block(leaf, &v.iobuf)?;
    let mut sbuf = [0u8; BLOCK];
    write_alloc_into(v, &[], &mut sbuf);
    d.write_block(alloc_bno, &sbuf)?;
    pack_super(&mut sbuf, v, 0);
    d.write_block(0, &sbuf)?;
    pack_super(&mut sbuf, v, 1);
    d.write_block(1, &sbuf)?;
    d.flush()
}
