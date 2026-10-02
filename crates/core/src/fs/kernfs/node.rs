use super::*;

pub(super) fn set_target(buf: &mut [u8; MAX_NAME], len: &mut u8, t: &[u8]) -> Result<(), FsError> {
    if t.is_empty() || t.len() > MAX_NAME {
        return Err(FsError::Inval);
    }
    let mut i = 0usize;
    while i < t.len() {
        if t[i] == 0 {
            return Err(FsError::Inval);
        }
        i += 1;
    }
    buf[..t.len()].copy_from_slice(t);
    if t.len() < MAX_NAME {
        buf[t.len()..].fill(0);
    }
    *len = t.len() as u8;
    Ok(())
}

pub(super) fn target_bytes(n: &KernNode) -> &[u8] {
    &n.target[..n.target_len as usize]
}

/// Node `ino`'s index in the table, or `None` for 0 or past its end.
pub(super) fn kern_idx(k: &KernState, ino: u32) -> Option<usize> {
    let i = usize::try_from(ino.checked_sub(1)?).ok()?;
    if i >= k.nodes.len() { None } else { Some(i) }
}

pub(super) fn kern_get(k: &KernState, inst: u32, ino: u32) -> Option<&KernNode> {
    let i = kern_idx(k, ino)?;
    let n = &k.nodes[i];
    if n.used && n.inst == inst {
        Some(n)
    } else {
        None
    }
}

pub(super) fn kern_get_mut(k: &mut KernState, inst: u32, ino: u32) -> Option<&mut KernNode> {
    let i = kern_idx(k, ino)?;
    let n = &mut k.nodes[i];
    if n.used && n.inst == inst {
        Some(n)
    } else {
        None
    }
}

impl KernNode {
    pub(super) fn meta_into(&self, ino: &mut Inode) {
        ino.kind = self.kind.inode_kind();
        ino.mode = self.mode;
        ino.nlink = self.nlink;
        ino.size = self.size;
        ino.atime = self.atime;
        ino.mtime = self.mtime;
        ino.ctime = self.ctime;
    }
}

/// Node `ino`'s [`InodeInfo`], keyed `[ino, 0, 0]`.
pub(super) fn kern_info(k: &KernState, inst: u32, ino: u32) -> Result<InodeInfo, FsError> {
    let n = kern_get(k, inst, ino).ok_or(FsError::NotFound)?;
    Ok(InodeInfo {
        key: [ino, 0, 0],
        ino,
        kind: n.kind.inode_kind(),
        mode: n.mode,
        nlink: n.nlink,
        size: n.size,
        atime: n.atime,
        mtime: n.mtime,
        ctime: n.ctime,
        private: [0; 2],
    })
}

/// Take a node for instance `inst`: the free list's first, else a new one
/// in the table's spare capacity, which [`KernFs::with_room`] reserved.
/// Never allocates. `NoSpace` when the instance has all the nodes it may,
/// when neither has one, or when the ino would not fit the `u32` an inode
/// key holds.
pub(super) fn kern_alloc(k: &mut KernState, inst: u32) -> Result<u32, FsError> {
    if k.at_cap(inst) {
        return Err(FsError::NoSpace);
    }
    let (ino, i) = match kern_idx(k, k.free) {
        Some(i) => {
            let ino = k.free;
            k.free = k.nodes[i].next;
            k.free_len = k.free_len.saturating_sub(1);
            (ino, i)
        }
        None => {
            let i = k.nodes.len();
            if i >= k.nodes.capacity() {
                return Err(FsError::NoSpace);
            }
            let ino = i
                .checked_add(1)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or(FsError::NoSpace)?;
            // Within capacity, checked just above, so this push never
            // allocates.
            k.nodes
                .try_push(KernNode::EMPTY)
                .map_err(|_| FsError::NoSpace)?;
            (ino, i)
        }
    };
    k.nodes[i] = KernNode::EMPTY;
    k.nodes[i].used = true;
    k.nodes[i].inst = inst;
    if let Some(s) = k.skin_of(inst) {
        s.nodes = s.nodes.saturating_add(1);
    }
    Ok(ino)
}

/// Return node `i` to the free list, its tmpfs extent freed first. The
/// table keeps its length: a later [`kern_alloc`] reuses the node.
pub(super) fn kern_release(k: &mut KernState, i: usize) {
    let Some(ino) = i.checked_add(1).and_then(|n| u32::try_from(n).ok()) else {
        return;
    };
    let Some(n) = k.nodes.get(i) else {
        return;
    };
    if !n.used {
        return;
    }
    let inst = n.inst;
    if n.kind == KernKind::File {
        tmp_free_extent(k, i);
    }
    if let Some(s) = k.skin_of(inst) {
        s.nodes = s.nodes.saturating_sub(1);
    }
    k.nodes[i] = KernNode::EMPTY;
    k.nodes[i].next = k.free;
    k.free = ino;
    k.free_len = k.free_len.saturating_add(1);
}

pub(super) fn kern_link(k: &mut KernState, parent: u32, child: u32) {
    let Some(pi) = kern_idx(k, parent) else {
        return;
    };
    let Some(ci) = kern_idx(k, child) else {
        return;
    };
    k.nodes[ci].parent = parent;
    k.nodes[ci].next = k.nodes[pi].child;
    k.nodes[pi].child = child;
}

fn kern_unlink_child(k: &mut KernState, parent: u32, child: u32) {
    let Some(pi) = kern_idx(k, parent) else {
        return;
    };
    let mut prev: Option<usize> = None;
    let mut cur = k.nodes[pi].child;
    while cur != 0 {
        let Some(ci) = kern_idx(k, cur) else {
            break;
        };
        let next = k.nodes[ci].next;
        if cur == child {
            match prev {
                None => k.nodes[pi].child = next,
                Some(p) => k.nodes[p].next = next,
            }
            k.nodes[ci].next = 0;
            k.nodes[ci].parent = 0;
            return;
        }
        prev = Some(ci);
        cur = next;
    }
}

pub(super) fn kern_find_child(k: &KernState, inst: u32, parent: u32, name: &[u8]) -> Option<u32> {
    let p = kern_get(k, inst, parent)?;
    let mut cur = p.child;
    while cur != 0 {
        let Some(n) = kern_get(k, inst, cur) else {
            break;
        };
        if n.name.eq_bytes(name) {
            return Some(cur);
        }
        cur = n.next;
    }
    None
}

pub(super) fn kern_lookup_ino(
    k: &KernState,
    inst: u32,
    parent: u32,
    name: &[u8],
) -> Result<u32, FsError> {
    kern_find_child(k, inst, parent, name).ok_or(FsError::NotFound)
}

pub(super) fn kern_mk_root(k: &mut KernState, now: u64, inst: u32) -> Result<u32, FsError> {
    let ino = kern_alloc(k, inst)?;
    let t = now;
    if let Some(n) = kern_get_mut(k, inst, ino) {
        n.kind = KernKind::Dir;
        n.mode = S_IFDIR_MODE;
        n.nlink = 2;
        n.atime = t;
        n.mtime = t;
        n.ctime = t;
        n.parent = ino;
    }
    Ok(ino)
}

pub(super) fn kern_mk_dir(
    k: &mut KernState,
    now: u64,
    inst: u32,
    parent: u32,
    name: &[u8],
) -> Result<u32, FsError> {
    if kern_find_child(k, inst, parent, name).is_some() {
        return Err(FsError::Exists);
    }
    let nm = Name::from_bytes(name)?;
    let ino = kern_alloc(k, inst)?;
    let t = now;
    if let Some(n) = kern_get_mut(k, inst, ino) {
        n.kind = KernKind::Dir;
        n.mode = S_IFDIR_MODE;
        n.nlink = 2;
        n.atime = t;
        n.mtime = t;
        n.ctime = t;
        n.name = nm;
    }
    kern_link(k, parent, ino);
    if let Some(p) = kern_get_mut(k, inst, parent) {
        p.nlink = p.nlink.saturating_add(1);
        p.mtime = t;
        p.ctime = t;
    }
    Ok(ino)
}

pub(super) fn kern_mk_lnk(
    k: &mut KernState,
    now: u64,
    inst: u32,
    parent: u32,
    name: &[u8],
    target: &[u8],
) -> Result<u32, FsError> {
    if let Some(ino) = kern_find_child(k, inst, parent, name) {
        return Ok(ino);
    }
    let nm = Name::from_bytes(name)?;
    let ino = kern_alloc(k, inst)?;
    let t = now;
    let mut bad = false;
    if let Some(n) = kern_get_mut(k, inst, ino) {
        n.kind = KernKind::Lnk;
        n.mode = S_IFLNK_MODE;
        n.nlink = 1;
        n.size = target.len() as u64;
        n.atime = t;
        n.mtime = t;
        n.ctime = t;
        n.name = nm;
        bad = set_target(&mut n.target, &mut n.target_len, target).is_err();
    }
    if bad {
        if let Some(i) = kern_idx(k, ino) {
            kern_release(k, i);
        }
        return Err(FsError::Inval);
    }
    kern_link(k, parent, ino);
    touch_dir(k, inst, parent, t);
    Ok(ino)
}

pub(super) fn kern_mk_special(
    k: &mut KernState,
    now: u64,
    inst: u32,
    parent: u32,
    name: &[u8],
    kind: KernKind,
    tag: u64,
) -> Result<u32, FsError> {
    if let Some(ino) = kern_find_child(k, inst, parent, name) {
        return Ok(ino);
    }
    let nm = Name::from_bytes(name)?;
    let ino = kern_alloc(k, inst)?;
    let t = now;
    let size = match kind {
        KernKind::ProcCmdline => PROC_CMDLINE.len() as u64,
        KernKind::ProcStatus => PROC_STATUS.len() as u64,
        KernKind::ProcMaps => PROC_MAPS.len() as u64,
        _ => 0,
    };
    let mode = match kind.inode_kind() {
        InodeKind::Dir => S_IFDIR_MODE,
        InodeKind::Lnk => S_IFLNK_MODE,
        InodeKind::Blk => S_IFBLK | 0o666,
        InodeKind::Chr => S_IFCHR | 0o666,
        InodeKind::Reg => S_IFREG_MODE,
    };
    let nlink = if kind.inode_kind() == InodeKind::Dir {
        2
    } else {
        1
    };
    if let Some(n) = kern_get_mut(k, inst, ino) {
        n.kind = kind;
        n.mode = mode;
        n.nlink = nlink;
        n.size = size;
        n.atime = t;
        n.mtime = t;
        n.ctime = t;
        n.name = nm;
        n.tag = tag;
    }
    kern_link(k, parent, ino);
    if kind.inode_kind() == InodeKind::Dir
        && let Some(p) = kern_get_mut(k, inst, parent)
    {
        p.nlink = p.nlink.saturating_add(1);
    }
    touch_dir(k, inst, parent, t);
    Ok(ino)
}

pub(super) fn kern_drop_sb(k: &mut KernState, inst: u32, is_tmp: bool) {
    let mut i = 0usize;
    while i < k.nodes.len() {
        if k.nodes[i].used && k.nodes[i].inst == inst {
            kern_release(k, i);
        }
        i += 1;
    }
    if is_tmp {
        k.tmp_bits = 0;
        k.tmp_cache.drop_dev(TMPFS_DEV);
        k.tmp_back = [0u8; TMPFS_BACK_BYTES];
    }
}

/// kernfs `evict`: free a node with no links.
pub(super) fn kern_try_free(k: &mut KernState, inst: u32, ino: u32) {
    let Some(i) = kern_idx(k, ino) else {
        return;
    };
    if !k.nodes[i].used || k.nodes[i].inst != inst || k.nodes[i].nlink != 0 {
        return;
    }
    kern_release(k, i);
}

pub(super) fn kern_lookup(
    k: &mut KernState,
    x: Kx,
    dir: &Inode,
    name: &[u8],
) -> Result<InodeInfo, FsError> {
    let (k, inst) = (&*k, x.inst);
    let n = kern_get(k, inst, dir.key[0]).ok_or(FsError::NotFound)?;
    if n.kind.inode_kind() != InodeKind::Dir {
        return Err(FsError::NotDir);
    }
    let child = kern_find_child(k, inst, dir.key[0], name).ok_or(FsError::NotFound)?;
    kern_info(k, inst, child)
}

pub(super) fn kern_create(
    k: &mut KernState,
    x: Kx,
    dir: &mut Inode,
    name: &[u8],
    kind: InodeKind,
    mode: u16,
    target: Option<&[u8]>,
) -> Result<InodeInfo, FsError> {
    if x.ty != FsType::Tmp {
        // devfs, procfs and sysfs make nothing: a regular file is
        // `Acces`, as `open(O_CREAT)` in Linux's `/proc`, the rest `Perm`.
        return Err(match kind {
            InodeKind::Reg => FsError::Acces,
            InodeKind::Dir | InodeKind::Lnk | InodeKind::Chr | InodeKind::Blk => FsError::Perm,
        });
    }
    let (k, now, inst) = (&mut *k, x.now, x.inst);
    let dir_ino = dir.key[0];
    let nm = Name::from_bytes(name)?;
    if nm.is_dot() || nm.is_dotdot() {
        return Err(FsError::Inval);
    }
    match kind {
        InodeKind::Reg | InodeKind::Dir | InodeKind::Lnk => {}
        InodeKind::Chr | InodeKind::Blk => return Err(FsError::Perm),
    }
    if kind == InodeKind::Lnk {
        let t = target.ok_or(FsError::Inval)?;
        if t.is_empty() {
            return Err(FsError::Inval);
        }
    }
    let d = kern_get(k, inst, dir_ino).ok_or(FsError::NotFound)?;
    if d.kind.inode_kind() != InodeKind::Dir {
        return Err(FsError::NotDir);
    }
    if kern_find_child(k, inst, dir_ino, name).is_some() {
        return Err(FsError::Exists);
    }
    let ino = match kind {
        InodeKind::Dir => kern_mk_dir(k, now, inst, dir_ino, name)?,
        InodeKind::Lnk => kern_mk_lnk(k, now, inst, dir_ino, name, target.unwrap_or(b""))?,
        InodeKind::Reg => {
            let ino = kern_alloc(k, inst)?;
            if let Some(n) = kern_get_mut(k, inst, ino) {
                n.kind = KernKind::File;
                n.mode = (mode & !S_IFMT) | InodeKind::Reg.ifmt();
                n.nlink = 1;
                n.atime = now;
                n.mtime = now;
                n.ctime = now;
                n.name = nm;
            }
            kern_link(k, dir_ino, ino);
            touch_dir(k, inst, dir_ino, now);
            ino
        }
        InodeKind::Chr | InodeKind::Blk => return Err(FsError::Perm),
    };
    if let Some(n) = kern_get(k, inst, dir_ino) {
        n.meta_into(dir);
    }
    kern_info(k, inst, ino)
}

/// Remove `name` from `dir`. The child keeps its node until [`Vfs`]
/// evicts it at its last put; a removed directory has no links left.
pub(super) fn kern_unlink(
    k: &mut KernState,
    x: Kx,
    dir: &mut Inode,
    name: &[u8],
) -> Result<(), FsError> {
    if x.ty != FsType::Tmp {
        return Err(FsError::Perm);
    }
    let (k, t, inst) = (&mut *k, x.now, x.inst);
    let dir_ino = dir.key[0];
    let child = kern_find_child(k, inst, dir_ino, name).ok_or(FsError::NotFound)?;
    let ch = kern_get(k, inst, child).ok_or(FsError::NotFound)?;
    let is_dir = ch.kind.inode_kind() == InodeKind::Dir;
    if is_dir && ch.child != 0 {
        return Err(FsError::NotEmpty);
    }
    kern_unlink_child(k, dir_ino, child);
    if is_dir && let Some(p) = kern_get_mut(k, inst, dir_ino) {
        p.nlink = p.nlink.saturating_sub(1);
    }
    if let Some(c) = kern_get_mut(k, inst, child) {
        c.nlink = if is_dir { 0 } else { c.nlink.saturating_sub(1) };
        c.ctime = t;
    }
    touch_dir(k, inst, dir_ino, t);
    if let Some(n) = kern_get(k, inst, dir_ino) {
        n.meta_into(dir);
    }
    Ok(())
}

pub(super) fn kern_read(
    k: &mut KernState,
    x: Kx,
    ino: &mut Inode,
    off: u64,
    buf: &mut [u8],
) -> Result<usize, FsError> {
    let (k, inst) = (&mut *k, x.inst);
    let ino = ino.key[0];
    let kind = kern_get(k, inst, ino).ok_or(FsError::NotFound)?.kind;
    match kind {
        KernKind::Dir | KernKind::ProcFdDir => Err(FsError::IsDir),
        KernKind::Null => Ok(0),
        KernKind::Zero => {
            buf.fill(0);
            Ok(buf.len())
        }
        // Hardware bytes only, until ROADMAP §13.10's CSPRNG: a short count
        // when virtio-rng and RDRAND supply less, `Again` when they supply
        // none (ROADMAP §10.12, F134).
        KernKind::Random | KernKind::Urandom => {
            if buf.is_empty() {
                return Ok(0);
            }
            match crate::entropy::hw_fill(buf) {
                0 => Err(FsError::Again),
                n => Ok(n),
            }
        }
        KernKind::Console | KernKind::Tty => Ok(0),
        // `KernSkin::read` sends a block node with a device to `blk_read`
        // before it gets here; one whose slot is empty has no device.
        KernKind::Block => Err(FsError::Io),
        KernKind::Lnk => {
            let n = kern_get(k, inst, ino).ok_or(FsError::NotFound)?;
            copy_off(target_bytes(n), off, buf)
        }
        KernKind::ProcCmdline => copy_off(PROC_CMDLINE, off, buf),
        KernKind::ProcStatus => copy_off(PROC_STATUS, off, buf),
        KernKind::ProcMaps => copy_off(PROC_MAPS, off, buf),
        KernKind::SysAttr => sys_attr_read(k, inst, ino, off, buf),
        KernKind::File => tmp_read(k, inst, ino, off, buf),
    }
}

pub(super) fn kern_write(
    k: &mut KernState,
    x: Kx,
    ino: &mut Inode,
    off: u64,
    buf: &[u8],
) -> Result<usize, FsError> {
    let (k, now, inst) = (&mut *k, x.now, x.inst);
    let key = ino.key[0];
    let kind = kern_get(k, inst, key).ok_or(FsError::NotFound)?.kind;
    match kind {
        KernKind::Dir | KernKind::ProcFdDir => Err(FsError::IsDir),
        KernKind::Null | KernKind::Zero => Ok(buf.len()),
        KernKind::Random | KernKind::Urandom => Ok(buf.len()),
        KernKind::Console | KernKind::Tty => {
            // Accept the write. Do not fan out to serial here: VFS
            // holds RANK_DEVICE and DESIGN §2.1 forbids holding that
            // across serial.
            let n = buf.len().min(64);
            k.cons_out[..n].copy_from_slice(&buf[..n]);
            k.cons_len = n as u8;
            Ok(buf.len())
        }
        // As in `kern_read`: `KernSkin::write` sends it to `blk_write`.
        KernKind::Block => Err(FsError::Io),
        KernKind::Lnk
        | KernKind::ProcCmdline
        | KernKind::ProcStatus
        | KernKind::ProcMaps
        | KernKind::SysAttr => Err(FsError::Inval),
        KernKind::File => {
            let n = tmp_write(k, now, inst, key, off, buf)?;
            if let Some(node) = kern_get(k, inst, key) {
                node.meta_into(ino);
            }
            Ok(n)
        }
    }
}

pub(super) fn kern_truncate(
    k: &mut KernState,
    x: Kx,
    ino: &mut Inode,
    size: u64,
) -> Result<(), FsError> {
    let (k, now, inst) = (&mut *k, x.now, x.inst);
    let key = ino.key[0];
    let kind = kern_get(k, inst, key).ok_or(FsError::NotFound)?.kind;
    match kind {
        KernKind::File => {
            tmp_truncate(k, now, inst, key, size)?;
            if let Some(node) = kern_get(k, inst, key) {
                node.meta_into(ino);
            }
            Ok(())
        }
        KernKind::Dir | KernKind::ProcFdDir => Err(FsError::IsDir),
        _ => Err(FsError::Inval),
    }
}

pub(super) fn kern_readdir(
    k: &mut KernState,
    x: Kx,
    dir: &Inode,
    cookie: u64,
    out: &mut Dirent,
) -> Result<Option<u64>, FsError> {
    let (k, inst) = (&*k, x.inst);
    let n = kern_get(k, inst, dir.key[0]).ok_or(FsError::NotFound)?;
    if n.kind.inode_kind() != InodeKind::Dir {
        return Err(FsError::NotDir);
    }
    let mut cur = n.child;
    let mut i = 0u64;
    while cur != 0 {
        if i == cookie {
            let c = kern_get(k, inst, cur).ok_or(FsError::NotFound)?;
            out.ino = cur;
            out.kind = c.kind.inode_kind();
            out.name = c.name;
            return Ok(Some(cookie + 1));
        }
        let next = kern_get(k, inst, cur).ok_or(FsError::NotFound)?.next;
        cur = next;
        i += 1;
    }
    Ok(None)
}

pub(super) fn kern_readlink(
    k: &mut KernState,
    x: Kx,
    ino: &Inode,
    buf: &mut [u8],
) -> Result<usize, FsError> {
    let n = kern_get(k, x.inst, ino.key[0]).ok_or(FsError::NotFound)?;
    match n.kind {
        KernKind::Lnk => {
            let t = target_bytes(n);
            let n = t.len().min(buf.len());
            buf[..n].copy_from_slice(&t[..n]);
            Ok(n)
        }
        // `readlink` of anything but a symlink is `EINVAL`, as Linux's.
        _ => Err(FsError::Inval),
    }
}
