//! ramfs: a small in-memory tree behind [`InodeOps`]. ROADMAP §8.1.
//!
//! The store ([`RamState`]) lives outside [`super::Vfs`], behind a
//! [`Guarded`] lock of its own, and every instance mounted from it tags
//! its nodes with the instance id `fill_super` takes from the store and
//! keeps in the superblock's private word 0. Node `n` is keyed `[n, 0, 0]`.

use super::{
    Dirent, FileSystem, FsError, FsType, Guarded, Inode, InodeInfo, InodeKind, InodeOps, Key,
    MAX_DIR_ENTS, MAX_FILE_BYTES, MAX_RAM_NODES, Name, OpCx, S_IFDIR_MODE, S_IFMT,
};

#[derive(Clone, Copy)]
struct RamDent {
    name: Name,
    ino: u32,
}

impl RamDent {
    const EMPTY: Self = Self {
        name: Name::EMPTY,
        ino: 0,
    };
}

#[derive(Clone, Copy)]
struct RamNode {
    used: bool,
    inst: u32,
    kind: InodeKind,
    mode: u16,
    nlink: u32,
    size: u64,
    atime: u64,
    mtime: u64,
    ctime: u64,
    data: [u8; MAX_FILE_BYTES],
    dents: [RamDent; MAX_DIR_ENTS],
    ndent: u8,
}

impl RamNode {
    const EMPTY: Self = Self {
        used: false,
        inst: 0,
        kind: InodeKind::Reg,
        mode: 0,
        nlink: 0,
        size: 0,
        atime: 0,
        mtime: 0,
        ctime: 0,
        data: [0; MAX_FILE_BYTES],
        dents: [RamDent::EMPTY; MAX_DIR_ENTS],
        ndent: 0,
    };

    fn meta_into(&self, ino: &mut Inode) {
        ino.kind = self.kind;
        ino.mode = self.mode;
        ino.nlink = self.nlink;
        ino.size = self.size;
        ino.atime = self.atime;
        ino.mtime = self.mtime;
        ino.ctime = self.ctime;
    }

    fn info(&self, node: u32) -> InodeInfo {
        InodeInfo {
            key: [node, 0, 0],
            ino: node,
            kind: self.kind,
            mode: self.mode,
            nlink: self.nlink,
            size: self.size,
            atime: self.atime,
            mtime: self.mtime,
            ctime: self.ctime,
            private: [0; 2],
        }
    }
}

/// Every ramfs node, of every instance, and the instance counter.
pub struct RamState {
    nodes: [RamNode; MAX_RAM_NODES],
    next_inst: u32,
}

impl RamState {
    pub const fn new() -> Self {
        Self {
            nodes: [RamNode::EMPTY; MAX_RAM_NODES],
            next_inst: 0,
        }
    }

    /// Nodes in use, across instances.
    pub fn used(&self) -> usize {
        self.nodes.iter().filter(|n| n.used).count()
    }
}

impl Default for RamState {
    fn default() -> Self {
        Self::new()
    }
}

/// ramfs over the store `S`: its [`FileSystem`] and its [`InodeOps`].
pub struct RamFs<S> {
    store: S,
}

impl<S: Guarded<RamState>> RamFs<S> {
    pub const fn new(store: S) -> Self {
        Self { store }
    }

    /// Run `f` on the store.
    pub fn with<R>(&self, f: impl FnOnce(&mut RamState) -> R) -> R {
        self.store.with(f)
    }
}

/// The instance `cx`'s superblock was filled as.
fn inst_of(cx: &OpCx<'_>) -> u32 {
    cx.private[0] as u32
}

impl<S: Guarded<RamState> + Sync + 'static> FileSystem for RamFs<S> {
    fn name(&self) -> &'static str {
        "ramfs"
    }

    fn fstype(&self) -> FsType {
        FsType::Ram
    }

    fn ops(&'static self) -> Option<&'static dyn InodeOps> {
        Some(self)
    }

    fn fill_super(&self, cx: &mut OpCx<'_>) -> Result<InodeInfo, FsError> {
        let now = cx.now;
        let (inst, info) = self.store.with(|st| {
            let inst = st.next_inst.wrapping_add(1).max(1);
            let node = ram_alloc(st, inst)?;
            st.next_inst = inst;
            let r = ram_get_mut(st, inst, node).ok_or(FsError::NotFound)?;
            r.kind = InodeKind::Dir;
            r.mode = S_IFDIR_MODE;
            r.nlink = 2;
            r.size = 0;
            r.atime = now;
            r.mtime = now;
            r.ctime = now;
            Ok::<_, FsError>((inst, r.info(node)))
        })?;
        *cx.private = [u64::from(inst), 0];
        Ok(info)
    }
}

impl<S: Guarded<RamState> + Sync + 'static> InodeOps for RamFs<S> {
    fn lookup(&self, cx: &mut OpCx<'_>, dir: &Inode, name: &[u8]) -> Result<InodeInfo, FsError> {
        let inst = inst_of(cx);
        self.store.with(|st| ram_lookup(st, inst, dir.key[0], name))
    }

    fn create(
        &self,
        cx: &mut OpCx<'_>,
        dir: &mut Inode,
        name: &[u8],
        kind: InodeKind,
        mode: u16,
        target: Option<&[u8]>,
    ) -> Result<InodeInfo, FsError> {
        let (inst, now) = (inst_of(cx), cx.now);
        self.store
            .with(|st| ram_create(st, inst, now, dir, name, kind, mode, target))
    }

    fn unlink(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        let (inst, now) = (inst_of(cx), cx.now);
        self.store.with(|st| ram_unlink(st, inst, now, dir, name))
    }

    fn rmdir(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        self.unlink(cx, dir, name)
    }

    fn read(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        let inst = inst_of(cx);
        self.store
            .with(|st| ram_read(st, inst, ino.key[0], off, buf))
    }

    fn write(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError> {
        let (inst, now) = (inst_of(cx), cx.now);
        self.store
            .with(|st| ram_write(st, inst, now, ino, off, buf))
    }

    fn truncate(&self, cx: &mut OpCx<'_>, ino: &mut Inode, size: u64) -> Result<(), FsError> {
        let (inst, now) = (inst_of(cx), cx.now);
        self.store.with(|st| ram_truncate(st, inst, now, ino, size))
    }

    fn readdir(
        &self,
        cx: &mut OpCx<'_>,
        dir: &Inode,
        cookie: u64,
        out: &mut Dirent,
    ) -> Result<Option<u64>, FsError> {
        let inst = inst_of(cx);
        self.store
            .with(|st| ram_readdir(st, inst, dir.key[0], cookie, out))
    }

    fn getattr(&self, cx: &mut OpCx<'_>, ino: &mut Inode) -> Result<(), FsError> {
        let inst = inst_of(cx);
        self.store.with(|st| {
            if let Some(r) = ram_get(st, inst, ino.key[0]) {
                r.meta_into(ino);
            }
        });
        Ok(())
    }

    fn readlink(&self, cx: &mut OpCx<'_>, ino: &Inode, buf: &mut [u8]) -> Result<usize, FsError> {
        let inst = inst_of(cx);
        self.store
            .with(|st| ram_readlink(st, inst, ino.key[0], buf))
    }

    fn link(
        &self,
        cx: &mut OpCx<'_>,
        dir: &mut Inode,
        name: &[u8],
        target: &mut Inode,
    ) -> Result<(), FsError> {
        let (inst, now) = (inst_of(cx), cx.now);
        self.store.with(|st| {
            ram_link(st, inst, now, dir.key[0], name, target.key[0])?;
            refresh(st, inst, dir);
            refresh(st, inst, target);
            Ok(())
        })
    }

    fn rename(
        &self,
        cx: &mut OpCx<'_>,
        odir: &mut Inode,
        oname: &[u8],
        ndir: &mut Inode,
        nname: &[u8],
    ) -> Result<Option<Key>, FsError> {
        let (inst, now) = (inst_of(cx), cx.now);
        self.store.with(|st| {
            ram_rename(st, inst, now, odir.key[0], oname, ndir.key[0], nname)?;
            refresh(st, inst, odir);
            refresh(st, inst, ndir);
            Ok(None)
        })
    }

    fn evict(&self, cx: &mut OpCx<'_>, ino: &Inode) -> Result<(), FsError> {
        let inst = inst_of(cx);
        self.store.with(|st| {
            if let Some(r) = ram_get_mut(st, inst, ino.key[0])
                && r.nlink == 0
            {
                *r = RamNode::EMPTY;
            }
        });
        Ok(())
    }

    fn kill_sb(&self, cx: &mut OpCx<'_>) {
        let inst = inst_of(cx);
        self.store.with(|st| {
            for n in st.nodes.iter_mut() {
                if n.used && n.inst == inst {
                    *n = RamNode::EMPTY;
                }
            }
        });
    }
}

/// Copy node `ino.key[0]`'s metadata into `ino`.
fn refresh(st: &RamState, inst: u32, ino: &mut Inode) {
    if let Some(r) = ram_get(st, inst, ino.key[0]) {
        r.meta_into(ino);
    }
}

fn ram_idx(node: u32) -> Option<usize> {
    let i = (node.checked_sub(1)?) as usize;
    if i >= MAX_RAM_NODES { None } else { Some(i) }
}

fn ram_get(st: &RamState, inst: u32, node: u32) -> Option<&RamNode> {
    let r = &st.nodes[ram_idx(node)?];
    if r.used && r.inst == inst {
        Some(r)
    } else {
        None
    }
}

fn ram_get_mut(st: &mut RamState, inst: u32, node: u32) -> Option<&mut RamNode> {
    let r = &mut st.nodes[ram_idx(node)?];
    if r.used && r.inst == inst {
        Some(r)
    } else {
        None
    }
}

fn ram_alloc(st: &mut RamState, inst: u32) -> Result<u32, FsError> {
    let i = st
        .nodes
        .iter()
        .position(|n| !n.used)
        .ok_or(FsError::NoSpace)?;
    st.nodes[i] = RamNode::EMPTY;
    st.nodes[i].used = true;
    st.nodes[i].inst = inst;
    Ok((i as u32) + 1)
}

fn ram_lookup(st: &RamState, inst: u32, dir: u32, name: &[u8]) -> Result<InodeInfo, FsError> {
    let r = ram_get(st, inst, dir).ok_or(FsError::NotFound)?;
    if r.kind != InodeKind::Dir {
        return Err(FsError::NotDir);
    }
    let d = r.dents[..r.ndent as usize]
        .iter()
        .find(|d| d.name.eq_bytes(name))
        .ok_or(FsError::NotFound)?;
    let c = ram_get(st, inst, d.ino).ok_or(FsError::NotFound)?;
    Ok(c.info(d.ino))
}

/// Room for one more entry named `name` in directory `dir`.
fn ram_dir_room(st: &RamState, inst: u32, dir: u32, name: &[u8]) -> Result<(), FsError> {
    let r = ram_get(st, inst, dir).ok_or(FsError::NotFound)?;
    if r.kind != InodeKind::Dir {
        return Err(FsError::NotDir);
    }
    if r.dents[..r.ndent as usize]
        .iter()
        .any(|d| d.name.eq_bytes(name))
    {
        return Err(FsError::Exists);
    }
    if r.ndent as usize >= MAX_DIR_ENTS {
        return Err(FsError::NoSpace);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // InodeOps::create's arguments and the store's
fn ram_create(
    st: &mut RamState,
    inst: u32,
    now: u64,
    dir: &mut Inode,
    name: &[u8],
    kind: InodeKind,
    mode: u16,
    target: Option<&[u8]>,
) -> Result<InodeInfo, FsError> {
    let nm = Name::from_bytes(name)?;
    if nm.is_dot() || nm.is_dotdot() {
        return Err(FsError::Inval);
    }
    if kind == InodeKind::Lnk {
        let t = target.ok_or(FsError::Inval)?;
        if t.is_empty() || t.len() > MAX_FILE_BYTES {
            return Err(FsError::Inval);
        }
    }
    match kind {
        InodeKind::Reg | InodeKind::Dir | InodeKind::Lnk => {}
        InodeKind::Chr | InodeKind::Blk => return Err(FsError::NotSupp),
    }
    let dir_node = dir.key[0];
    ram_dir_room(st, inst, dir_node, name)?;
    let node = ram_alloc(st, inst)?;
    let info = {
        let r = ram_get_mut(st, inst, node).ok_or(FsError::NotFound)?;
        r.kind = kind;
        r.mode = (mode & !S_IFMT) | kind.ifmt();
        r.nlink = if kind == InodeKind::Dir { 2 } else { 1 };
        r.atime = now;
        r.mtime = now;
        r.ctime = now;
        r.size = 0;
        if let (InodeKind::Lnk, Some(tgt)) = (kind, target) {
            r.data[..tgt.len()].copy_from_slice(tgt);
            r.size = tgt.len() as u64;
        }
        r.info(node)
    };
    let r = ram_get_mut(st, inst, dir_node).ok_or(FsError::NotFound)?;
    let n = r.ndent as usize;
    r.dents[n] = RamDent {
        name: nm,
        ino: node,
    };
    r.ndent += 1;
    r.mtime = now;
    r.ctime = now;
    if kind == InodeKind::Dir {
        r.nlink = r.nlink.saturating_add(1);
    }
    r.meta_into(dir);
    Ok(info)
}

/// Remove `name` from `dir`. The child keeps its node until `Vfs`
/// evicts it at its last put; a removed directory has no links left.
fn ram_unlink(
    st: &mut RamState,
    inst: u32,
    now: u64,
    dir: &mut Inode,
    name: &[u8],
) -> Result<(), FsError> {
    let dir_node = dir.key[0];
    let (idx, child) = {
        let r = ram_get(st, inst, dir_node).ok_or(FsError::NotFound)?;
        if r.kind != InodeKind::Dir {
            return Err(FsError::NotDir);
        }
        let i = r.dents[..r.ndent as usize]
            .iter()
            .position(|d| d.name.eq_bytes(name))
            .ok_or(FsError::NotFound)?;
        (i, r.dents[i].ino)
    };
    let c = ram_get(st, inst, child).ok_or(FsError::NotFound)?;
    let kind = c.kind;
    if kind == InodeKind::Dir && c.ndent != 0 {
        return Err(FsError::NotEmpty);
    }
    if let Some(c) = ram_get_mut(st, inst, child) {
        c.nlink = if kind == InodeKind::Dir {
            0
        } else {
            c.nlink.saturating_sub(1)
        };
        c.ctime = now;
    }
    let r = ram_get_mut(st, inst, dir_node).ok_or(FsError::NotFound)?;
    remove_dent(r, idx);
    r.mtime = now;
    r.ctime = now;
    if kind == InodeKind::Dir {
        r.nlink = r.nlink.saturating_sub(1);
    }
    r.meta_into(dir);
    Ok(())
}

/// Drop entry `idx` of directory `r`, moving its last entry into the gap.
fn remove_dent(r: &mut RamNode, idx: usize) {
    let Some(last) = (r.ndent as usize).checked_sub(1) else {
        return;
    };
    r.dents[idx] = r.dents[last];
    r.dents[last] = RamDent::EMPTY;
    r.ndent -= 1;
}

fn ram_read(
    st: &RamState,
    inst: u32,
    node: u32,
    off: u64,
    buf: &mut [u8],
) -> Result<usize, FsError> {
    let r = ram_get(st, inst, node).ok_or(FsError::NotFound)?;
    match r.kind {
        InodeKind::Dir => Err(FsError::IsDir),
        InodeKind::Reg | InodeKind::Lnk => {
            if off >= r.size {
                return Ok(0);
            }
            let start = off as usize;
            let avail = (r.size as usize).saturating_sub(start);
            let n = avail.min(buf.len());
            buf[..n].copy_from_slice(&r.data[start..start + n]);
            Ok(n)
        }
        InodeKind::Chr | InodeKind::Blk => Err(FsError::NotSupp),
    }
}

fn ram_write(
    st: &mut RamState,
    inst: u32,
    now: u64,
    ino: &mut Inode,
    off: u64,
    buf: &[u8],
) -> Result<usize, FsError> {
    let r = ram_get_mut(st, inst, ino.key[0]).ok_or(FsError::NotFound)?;
    match r.kind {
        InodeKind::Dir => Err(FsError::IsDir),
        InodeKind::Lnk | InodeKind::Chr | InodeKind::Blk => Err(FsError::Inval),
        InodeKind::Reg => {
            let start = usize::try_from(off).map_err(|_| FsError::NoSpace)?;
            let end = start.checked_add(buf.len()).ok_or(FsError::NoSpace)?;
            if end > MAX_FILE_BYTES {
                return Err(FsError::NoSpace);
            }
            r.data[start..end].copy_from_slice(buf);
            if end as u64 > r.size {
                r.size = end as u64;
            }
            r.mtime = now;
            r.ctime = now;
            r.meta_into(ino);
            Ok(buf.len())
        }
    }
}

fn ram_truncate(
    st: &mut RamState,
    inst: u32,
    now: u64,
    ino: &mut Inode,
    size: u64,
) -> Result<(), FsError> {
    if size > MAX_FILE_BYTES as u64 {
        return Err(FsError::NoSpace);
    }
    let r = ram_get_mut(st, inst, ino.key[0]).ok_or(FsError::NotFound)?;
    match r.kind {
        InodeKind::Dir => Err(FsError::IsDir),
        InodeKind::Lnk | InodeKind::Chr | InodeKind::Blk => Err(FsError::Inval),
        InodeKind::Reg => {
            let old = r.size as usize;
            let n = size as usize;
            if n < old {
                r.data[n..old].fill(0);
            } else {
                r.data[old..n].fill(0);
            }
            r.size = size;
            r.mtime = now;
            r.ctime = now;
            r.meta_into(ino);
            Ok(())
        }
    }
}

fn ram_readdir(
    st: &RamState,
    inst: u32,
    dir: u32,
    cookie: u64,
    out: &mut Dirent,
) -> Result<Option<u64>, FsError> {
    let r = ram_get(st, inst, dir).ok_or(FsError::NotFound)?;
    if r.kind != InodeKind::Dir {
        return Err(FsError::NotDir);
    }
    let Some(d) = usize::try_from(cookie)
        .ok()
        .and_then(|i| r.dents[..r.ndent as usize].get(i))
    else {
        return Ok(None);
    };
    out.ino = d.ino;
    out.kind = ram_get(st, inst, d.ino)
        .map(|c| c.kind)
        .unwrap_or(InodeKind::Reg);
    out.name = d.name;
    Ok(Some(cookie + 1))
}

/// Link `name` in directory node `dir` to node `target`.
fn ram_link(
    st: &mut RamState,
    inst: u32,
    now: u64,
    dir: u32,
    name: &[u8],
    target: u32,
) -> Result<(), FsError> {
    let nm = Name::from_bytes(name)?;
    let t = ram_get(st, inst, target).ok_or(FsError::NotFound)?;
    if t.kind != InodeKind::Reg {
        return Err(FsError::Inval);
    }
    ram_dir_room(st, inst, dir, name)?;
    let r = ram_get_mut(st, inst, dir).ok_or(FsError::NotFound)?;
    let n = r.ndent as usize;
    r.dents[n] = RamDent {
        name: nm,
        ino: target,
    };
    r.ndent += 1;
    r.mtime = now;
    r.ctime = now;
    if let Some(t) = ram_get_mut(st, inst, target) {
        t.nlink = t.nlink.saturating_add(1);
        t.ctime = now;
    }
    Ok(())
}

/// Move `oname` in directory node `odir` to `nname` in `ndir`.
fn ram_rename(
    st: &mut RamState,
    inst: u32,
    now: u64,
    odir: u32,
    oname: &[u8],
    ndir: u32,
    nname: &[u8],
) -> Result<(), FsError> {
    let nm = Name::from_bytes(nname)?;
    let (idx, node) = {
        let r = ram_get(st, inst, odir).ok_or(FsError::NotFound)?;
        let i = r.dents[..r.ndent as usize]
            .iter()
            .position(|d| d.name.eq_bytes(oname))
            .ok_or(FsError::NotFound)?;
        (i, r.dents[i].ino)
    };
    let kind = ram_get(st, inst, node).ok_or(FsError::NotFound)?.kind;
    if odir == ndir && oname == nname {
        return Ok(());
    }
    {
        let r = ram_get(st, inst, ndir).ok_or(FsError::NotFound)?;
        if r.kind != InodeKind::Dir {
            return Err(FsError::NotDir);
        }
        if r.dents[..r.ndent as usize]
            .iter()
            .any(|d| d.name.eq_bytes(nname))
        {
            return Err(FsError::Exists);
        }
        if odir != ndir && r.ndent as usize >= MAX_DIR_ENTS {
            return Err(FsError::NoSpace);
        }
    }
    {
        let r = ram_get_mut(st, inst, odir).ok_or(FsError::NotFound)?;
        remove_dent(r, idx);
        r.mtime = now;
        if kind == InodeKind::Dir {
            r.nlink = r.nlink.saturating_sub(1);
        }
    }
    let r = ram_get_mut(st, inst, ndir).ok_or(FsError::NotFound)?;
    let n = r.ndent as usize;
    r.dents[n] = RamDent {
        name: nm,
        ino: node,
    };
    r.ndent += 1;
    r.mtime = now;
    if kind == InodeKind::Dir {
        r.nlink = r.nlink.saturating_add(1);
    }
    Ok(())
}

fn ram_readlink(st: &RamState, inst: u32, node: u32, buf: &mut [u8]) -> Result<usize, FsError> {
    let r = ram_get(st, inst, node).ok_or(FsError::NotFound)?;
    match r.kind {
        InodeKind::Lnk => {
            let n = (r.size as usize).min(buf.len()).min(MAX_FILE_BYTES);
            buf[..n].copy_from_slice(&r.data[..n]);
            Ok(n)
        }
        InodeKind::Dir => Err(FsError::IsDir),
        InodeKind::Reg | InodeKind::Chr | InodeKind::Blk => Err(FsError::Inval),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits;

    #[test]
    fn fixed_tables_match_limits() {
        let st = std::boxed::Box::new(RamState::new());
        assert_eq!(st.nodes.len(), limits::MAX_RAM_NODES);
        assert_eq!(RamNode::EMPTY.data.len(), limits::MAX_TMPFS_FILE_BYTES);
        assert_eq!(RamNode::EMPTY.dents.len(), limits::MAX_TMPFS_DIR_ENTS);
    }
}
