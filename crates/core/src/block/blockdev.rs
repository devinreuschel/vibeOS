//! The block-device registry: counted [`BlockRef`] handles (DEVICES.md
//! §12.1), keyed by an owned [`BlockName`] and by a 64-bit id that
//! [`DiskSeq`] never hands out twice within a boot (§12.1 rule 4, as
//! Linux's `diskseq`). A partition is a child entry whose I/O maps onto
//! its disk's.
//!
//! Removal follows INVARIANTS.md §2.11 rule 3 (DEVICES.md §12.4 rule 9):
//! [`Registry::unpublish`] takes a device and its subtree out of the
//! table, deepest first, so no new handle can be taken, and
//! [`BlockRef::kill`] closes the entry's [`OpGate`], after which I/O
//! through a handle still held returns [`BlockError::Gone`]. The memory
//! goes at the last put.
//!
//! Portable. The kernel keeps the one table under a ranked lock
//! (`blockdev_init`); host tests drive [`Registry`] directly.

use crate::atomic::statics;
use crate::atomic::{AtomicBool, Ordering};
use crate::block::part::{self, PartKind};
use crate::block::{BlockDevice, BlockError, DeviceState, MAX_BLOCKDEVS};
use crate::kalloc::{TryArc, TryBox};
use crate::sync::OpGate;
use core::fmt;

/// The longest device name, in bytes: Linux's `DISK_NAME_LEN`.
pub const DISK_NAME_LEN: usize = 32;

/// A device name owned by its registry entry: 1 to [`DISK_NAME_LEN`]
/// bytes of `[A-Za-z0-9._-]`, not starting with `.`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct BlockName {
    len: u8,
    bytes: [u8; DISK_NAME_LEN],
}

fn name_byte_ok(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
}

impl BlockName {
    /// Check `n` against the name rules and copy it. `Inval` otherwise.
    pub fn new(n: &[u8]) -> Result<Self, BlockError> {
        if n.is_empty() || n.len() > DISK_NAME_LEN || n.first() == Some(&b'.') {
            return Err(BlockError::Inval);
        }
        if !n.iter().all(|&b| name_byte_ok(b)) {
            return Err(BlockError::Inval);
        }
        let mut bytes = [0u8; DISK_NAME_LEN];
        let dst = bytes.get_mut(..n.len()).ok_or(BlockError::Inval)?;
        dst.copy_from_slice(n);
        let len = u8::try_from(n.len()).map_err(|_| BlockError::Inval)?;
        Ok(Self { len, bytes })
    }

    /// The name of child `n` of `p`: `<p>p<n>`. `n` comes from a partition
    /// table, so it is untrusted: `Inval` for 0 or when the name would pass
    /// [`DISK_NAME_LEN`].
    pub fn child(p: &BlockName, n: u32) -> Result<Self, BlockError> {
        if n == 0 {
            return Err(BlockError::Inval);
        }
        let mut digits = [0u8; 10];
        let mut nd = 0usize;
        let mut v = n;
        while v != 0 {
            let d = u8::try_from(v % 10).map_err(|_| BlockError::Inval)?;
            *digits.get_mut(nd).ok_or(BlockError::Inval)? = b'0' + d;
            nd = nd.checked_add(1).ok_or(BlockError::Inval)?;
            v /= 10;
        }
        let base = p.as_bytes();
        let total = base
            .len()
            .checked_add(1)
            .and_then(|t| t.checked_add(nd))
            .ok_or(BlockError::Inval)?;
        if total > DISK_NAME_LEN {
            return Err(BlockError::Inval);
        }
        let mut buf = [0u8; DISK_NAME_LEN];
        let mut i = 0usize;
        for &b in base.iter().chain(core::iter::once(&b'p')) {
            *buf.get_mut(i).ok_or(BlockError::Inval)? = b;
            i += 1;
        }
        for &b in digits.get(..nd).ok_or(BlockError::Inval)?.iter().rev() {
            *buf.get_mut(i).ok_or(BlockError::Inval)? = b;
            i += 1;
        }
        BlockName::new(buf.get(..total).ok_or(BlockError::Inval)?)
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or(&[])
    }

    /// The name as text. Every byte is ASCII ([`BlockName::new`]).
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).unwrap_or("")
    }
}

impl fmt::Debug for BlockName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Block-device ids: a 64-bit counter from 1 that never wraps, as Linux's
/// `diskseq`. `u64::MAX` is never handed out, so a table may use it as an
/// empty key.
pub struct DiskSeq {
    next: statics::AtomicU64,
}

impl DiskSeq {
    pub const fn new() -> Self {
        Self {
            next: statics::AtomicU64::new(1),
        }
    }

    /// The next id. `NoMem` once the counter is exhausted, never a reuse.
    pub fn next(&self) -> Result<u64, BlockError> {
        // Relaxed: the id orders nothing; only its uniqueness matters, which
        // the read-modify-write gives.
        let mut v = self.next.load(statics::Ordering::Relaxed);
        loop {
            let n = v.checked_add(1).ok_or(BlockError::NoMem)?;
            match self.next.compare_exchange_weak(
                v,
                n,
                statics::Ordering::Relaxed,
                statics::Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(v),
                Err(now) => v = now,
            }
        }
    }
}

impl Default for DiskSeq {
    fn default() -> Self {
        Self::new()
    }
}

/// A partition's window on its disk, in the disk's logical blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartInfo {
    pub start: u64,
    pub nsect: u64,
    pub kind: PartKind,
}

/// The cache a disk's I/O goes through. The kernel's page cache is the one
/// implementation (`cache_init::PAGE_CACHE`); its miss path calls
/// [`BlockRef::read_dev`] and friends.
pub trait BlockCache: Sync {
    fn read(&self, dev: &BlockRef, lba: u64, buf: &mut [u8]) -> Result<(), BlockError>;
    fn write(&self, dev: &BlockRef, lba: u64, buf: &[u8]) -> Result<(), BlockError>;
    fn flush(&self, dev: &BlockRef) -> Result<(), BlockError>;
}

/// What an entry's I/O reaches.
pub enum Backing {
    /// A disk: the driver it owns, and the cache in front of it, if any.
    Disk {
        ops: TryBox<dyn BlockDevice>,
        cache: Option<&'static dyn BlockCache>,
    },
    /// A window on `parent`, which this entry keeps alive.
    Part { parent: BlockRef, info: PartInfo },
}

struct Entry {
    id: u64,
    name: BlockName,
    /// Every operation through a handle enters it (INVARIANTS.md §2.11
    /// rule 3); [`BlockRef::kill`] closes it.
    gate: OpGate,
    /// In a [`Registry`]. Written only under the registry's `&mut`.
    published: AtomicBool,
    back: Backing,
}

/// A counted handle to one registered block device. Not `Copy`: each
/// clone holds a count, and the entry is freed at the last put.
#[derive(Clone)]
pub struct BlockRef(TryArc<Entry>);

impl fmt::Debug for BlockRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockRef")
            .field("name", &self.0.name)
            .field("id", &self.0.id)
            .finish()
    }
}

fn gone(_: crate::sync::Dead) -> BlockError {
    BlockError::Gone
}

impl BlockRef {
    /// Build an unpublished entry. A partition's window must lie inside
    /// its parent (`Inval`); an allocation failure is `NoMem`.
    pub fn try_new(id: u64, name: BlockName, dev: Backing) -> Result<Self, BlockError> {
        if let Backing::Part { parent, info } = &dev {
            let end = info
                .start
                .checked_add(info.nsect)
                .ok_or(BlockError::Inval)?;
            if info.nsect == 0 || end > parent.capacity_sectors()? {
                return Err(BlockError::Inval);
            }
        }
        let e = Entry {
            id,
            name,
            gate: OpGate::new(),
            published: AtomicBool::new(false),
            back: dev,
        };
        TryArc::try_new(e)
            .map(BlockRef)
            .map_err(|_| BlockError::NoMem)
    }

    pub fn id(&self) -> u64 {
        self.0.id
    }

    pub fn name(&self) -> &BlockName {
        &self.0.name
    }

    /// The disk a partition is a window on; `None` for a disk.
    pub fn parent(&self) -> Option<&BlockRef> {
        match &self.0.back {
            Backing::Part { parent, .. } => Some(parent),
            Backing::Disk { .. } => None,
        }
    }

    /// A partition's window; `None` for a disk.
    pub fn part(&self) -> Option<PartInfo> {
        match &self.0.back {
            Backing::Part { info, .. } => Some(*info),
            Backing::Disk { .. } => None,
        }
    }

    /// Whether this is the same entry as `other`.
    pub fn same(&self, other: &BlockRef) -> bool {
        self.0.id == other.0.id
    }

    pub fn logical_block_size(&self) -> Result<u32, BlockError> {
        let _g = self.0.gate.enter().map_err(gone)?;
        match &self.0.back {
            Backing::Disk { ops, .. } => Ok(ops.logical_block_size()),
            Backing::Part { parent, .. } => parent.logical_block_size(),
        }
    }

    /// Capacity in logical blocks.
    pub fn capacity_sectors(&self) -> Result<u64, BlockError> {
        let _g = self.0.gate.enter().map_err(gone)?;
        match &self.0.back {
            Backing::Disk { ops, .. } => Ok(ops.capacity_sectors()),
            Backing::Part { info, .. } => Ok(info.nsect),
        }
    }

    /// The driver's state; `Failed` once killed.
    pub fn state(&self) -> DeviceState {
        let Ok(_g) = self.0.gate.enter() else {
            return DeviceState::Failed;
        };
        match &self.0.back {
            Backing::Disk { ops, .. } => ops.state(),
            Backing::Part { parent, .. } => parent.state(),
        }
    }

    /// The parent LBA of a partition range of `bytes` bytes at `lba`.
    fn map(
        &self,
        parent: &BlockRef,
        info: &PartInfo,
        lba: u64,
        bytes: usize,
    ) -> Result<u64, BlockError> {
        let bs = parent.logical_block_size()?;
        if bs == 0 || !bytes.is_multiple_of(bs as usize) {
            return Err(BlockError::Inval);
        }
        let count = u64::try_from(bytes / (bs as usize)).map_err(|_| BlockError::Inval)?;
        part::map_child_lba(info.start, info.nsect, lba, count)
    }

    /// Read whole logical blocks at `lba`. A partition maps onto its disk
    /// and fails with `Inval` past its end; a disk reads through its cache,
    /// or its driver when it has none.
    pub fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let _g = self.0.gate.enter().map_err(gone)?;
        match &self.0.back {
            Backing::Part { parent, info } => {
                let plba = self.map(parent, info, lba, buf.len())?;
                parent.read(plba, buf)
            }
            Backing::Disk { cache: Some(c), .. } => c.read(self, lba, buf),
            Backing::Disk { ops, cache: None } => ops.read(lba, buf),
        }
    }

    pub fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        let _g = self.0.gate.enter().map_err(gone)?;
        match &self.0.back {
            Backing::Part { parent, info } => {
                let plba = self.map(parent, info, lba, buf.len())?;
                parent.write(plba, buf)
            }
            Backing::Disk { cache: Some(c), .. } => c.write(self, lba, buf),
            Backing::Disk { ops, cache: None } => ops.write(lba, buf),
        }
    }

    /// Make every completed write durable. A partition flushes its disk.
    pub fn flush(&self) -> Result<(), BlockError> {
        let _g = self.0.gate.enter().map_err(gone)?;
        match &self.0.back {
            Backing::Part { parent, .. } => parent.flush(),
            Backing::Disk { cache: Some(c), .. } => c.flush(self),
            Backing::Disk { ops, cache: None } => ops.flush(),
        }
    }

    pub fn discard(&self, lba: u64, n: u64) -> Result<(), BlockError> {
        let _g = self.0.gate.enter().map_err(gone)?;
        match &self.0.back {
            Backing::Part { parent, info } => {
                let plba = part::map_child_lba(info.start, info.nsect, lba, n)?;
                parent.discard(plba, n)
            }
            Backing::Disk { ops, .. } => ops.discard(lba, n),
        }
    }

    fn disk_ops(&self) -> Result<&dyn BlockDevice, BlockError> {
        match &self.0.back {
            Backing::Disk { ops, .. } => Ok(&**ops),
            Backing::Part { .. } => Err(BlockError::Inval),
        }
    }

    /// Read below the cache: only the cache's miss path and the table
    /// parser call it. A partition gets `Inval`.
    pub fn read_dev(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let _g = self.0.gate.enter().map_err(gone)?;
        self.disk_ops()?.read(lba, buf)
    }

    /// Write below the cache. A partition gets `Inval`.
    pub fn write_dev(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        let _g = self.0.gate.enter().map_err(gone)?;
        self.disk_ops()?.write(lba, buf)
    }

    /// Flush below the cache. A partition gets `Inval`.
    pub fn flush_dev(&self) -> Result<(), BlockError> {
        let _g = self.0.gate.enter().map_err(gone)?;
        self.disk_ops()?.flush()
    }

    /// Close the gate and wait for the operations inside (INVARIANTS.md
    /// §2.11 rule 3). `Inval` while the entry is still published: unpublish
    /// first. The wake is a no-op, because a block device owns no wait
    /// queue and a request inside ends by its own deadline (BLOCK.md
    /// §10.3). It may sleep, so no spinlock may be held.
    pub fn kill(&self) -> Result<(), BlockError> {
        // Acquire: pairs with the Release store in `Registry::take`.
        if self.0.published.load(Ordering::Acquire) {
            return Err(BlockError::Inval);
        }
        self.0.gate.kill_and_wake(|| {});
        Ok(())
    }

    /// Whether [`kill`](Self::kill) has closed the gate.
    pub fn is_dead(&self) -> bool {
        self.0.gate.is_dead()
    }

    fn set_published(&self, on: bool) {
        // Release: a `kill` that reads `false` sees the removal before it.
        self.0.published.store(on, Ordering::Release);
    }
}

/// The table of registered devices. It holds counted handles (INVARIANTS.md
/// §2.11 rule 2) and never allocates or frees: a caller builds an entry
/// before [`insert`](Self::insert), and drops what
/// [`unpublish`](Self::unpublish) and [`snapshot`](Self::snapshot) hand
/// back after it leaves the table's lock.
pub struct Registry {
    slots: [Option<BlockRef>; MAX_BLOCKDEVS],
}

impl Registry {
    pub const fn new() -> Self {
        Self {
            slots: [const { None }; MAX_BLOCKDEVS],
        }
    }

    /// Publish `dev`. `Exists` if its name or id is taken, `Gone` if it is
    /// a partition whose parent is not published, `NoMem` if the table is
    /// full. On an error `dev` is dropped here, so a caller passes a clone
    /// and keeps its own count.
    pub fn insert(&mut self, dev: BlockRef) -> Result<(), BlockError> {
        let mut free = None;
        for (i, s) in self.slots.iter().enumerate() {
            match s {
                Some(r) if r.id() == dev.id() || r.name() == dev.name() => {
                    return Err(BlockError::Exists);
                }
                Some(_) => {}
                None => {
                    if free.is_none() {
                        free = Some(i);
                    }
                }
            }
        }
        if let Some(p) = dev.parent()
            && self.find_id(p.id()).is_none()
        {
            return Err(BlockError::Gone);
        }
        let slot = free
            .and_then(|i| self.slots.get_mut(i))
            .ok_or(BlockError::NoMem)?;
        dev.set_published(true);
        *slot = Some(dev);
        Ok(())
    }

    fn find_id(&self, id: u64) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.as_ref().is_some_and(|r| r.id() == id))
    }

    /// A handle to the device named `name`.
    pub fn lookup(&self, name: &[u8]) -> Option<BlockRef> {
        self.slots
            .iter()
            .flatten()
            .find(|r| r.name().as_bytes() == name)
            .cloned()
    }

    /// A handle to the device with id `id`.
    pub fn lookup_id(&self, id: u64) -> Option<BlockRef> {
        self.slots.iter().flatten().find(|r| r.id() == id).cloned()
    }

    /// How many ancestors `r` has.
    fn depth(r: &BlockRef) -> usize {
        let mut d = 0usize;
        let mut p = r.parent();
        while let Some(q) = p {
            d = d.saturating_add(1);
            p = q.parent();
        }
        d
    }

    fn under(r: &BlockRef, root: &BlockRef) -> bool {
        let mut p = Some(r);
        while let Some(q) = p {
            if q.same(root) {
                return true;
            }
            p = q.parent();
        }
        false
    }

    /// Take `dev` and its subtree out of the table, deepest first (DEVICES.md
    /// §12.2 rule 5), moving their handles into `out` in that order. `Gone`
    /// if `dev` is not published; `Inval`, with nothing removed, if `out`
    /// cannot hold the subtree. The caller kills each after it drops the
    /// table's lock.
    pub fn unpublish(
        &mut self,
        dev: &BlockRef,
        out: &mut [Option<BlockRef>],
    ) -> Result<usize, BlockError> {
        if self.find_id(dev.id()).is_none() {
            return Err(BlockError::Gone);
        }
        let n = self
            .slots
            .iter()
            .flatten()
            .filter(|r| Self::under(r, dev))
            .count();
        if n > out.len() {
            return Err(BlockError::Inval);
        }
        let mut k = 0usize;
        while k < n {
            // The deepest remaining member of the subtree.
            let mut pick: Option<(usize, usize)> = None;
            for (i, s) in self.slots.iter().enumerate() {
                if let Some(r) = s
                    && Self::under(r, dev)
                {
                    let d = Self::depth(r);
                    if pick.is_none_or(|(_, pd)| d > pd) {
                        pick = Some((i, d));
                    }
                }
            }
            let Some((i, _)) = pick else { break };
            let Some(r) = self.slots.get_mut(i).and_then(Option::take) else {
                break;
            };
            r.set_published(false);
            let Some(o) = out.get_mut(k) else { break };
            *o = Some(r);
            k += 1;
        }
        Ok(k)
    }

    /// Clone every published handle into `out`, in table order, and return
    /// how many. Stops when `out` is full.
    pub fn snapshot(&self, out: &mut [Option<BlockRef>]) -> usize {
        let mut n = 0usize;
        for (o, r) in out.iter_mut().zip(self.slots.iter().flatten()) {
            *o = Some(r.clone());
            n += 1;
        }
        n
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// Host-test devices.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::sync::Mutex;
    use std::vec::Vec;

    /// A disk over a byte vector.
    pub(crate) struct MemDisk {
        pub(crate) bs: u32,
        pub(crate) data: Mutex<Vec<u8>>,
    }

    impl MemDisk {
        pub(crate) fn new(bs: u32, nsect: u64) -> Self {
            let len = usize::try_from(nsect).unwrap() * bs as usize;
            Self {
                bs,
                data: Mutex::new((0..len).map(|i| (i % 251) as u8).collect()),
            }
        }

        fn range(&self, lba: u64, len: usize) -> Result<core::ops::Range<usize>, BlockError> {
            let bs = self.bs as usize;
            if len == 0 || !len.is_multiple_of(bs) {
                return Err(BlockError::Inval);
            }
            let start = usize::try_from(lba).map_err(|_| BlockError::Inval)? * bs;
            let end = start + len;
            if end > self.data.lock().unwrap().len() {
                return Err(BlockError::Inval);
            }
            Ok(start..end)
        }
    }

    impl BlockDevice for MemDisk {
        fn name(&self) -> &'static str {
            "mem"
        }
        fn logical_block_size(&self) -> u32 {
            self.bs
        }
        fn capacity_sectors(&self) -> u64 {
            (self.data.lock().unwrap().len() / self.bs as usize) as u64
        }
        fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            let r = self.range(lba, buf.len())?;
            buf.copy_from_slice(&self.data.lock().unwrap()[r]);
            Ok(())
        }
        fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
            let r = self.range(lba, buf.len())?;
            self.data.lock().unwrap()[r].copy_from_slice(buf);
            Ok(())
        }
        fn flush(&self) -> Result<(), BlockError> {
            Ok(())
        }
        fn discard(&self, lba: u64, n: u64) -> Result<(), BlockError> {
            let len = usize::try_from(n).map_err(|_| BlockError::Inval)? * self.bs as usize;
            self.range(lba, len).map(|_| ())
        }
    }

    /// A registered 512-byte-sector [`MemDisk`] of `nsect` sectors.
    pub(crate) fn disk(reg: &mut Registry, seq: &DiskSeq, name: &[u8], nsect: u64) -> BlockRef {
        let ops =
            TryBox::<dyn BlockDevice>::try_new_unsize(MemDisk::new(512, nsect), |b| b).unwrap();
        let r = BlockRef::try_new(
            seq.next().unwrap(),
            BlockName::new(name).unwrap(),
            Backing::Disk { ops, cache: None },
        )
        .unwrap();
        reg.insert(r.clone()).unwrap();
        r
    }

    /// A registered partition `name` of `parent` at `start`, `nsect` long.
    pub(crate) fn part(
        reg: &mut Registry,
        seq: &DiskSeq,
        parent: &BlockRef,
        name: &[u8],
        start: u64,
        nsect: u64,
    ) -> BlockRef {
        let info = PartInfo {
            start,
            nsect,
            kind: PartKind::Mbr {
                sys: part::MBR_LINUX,
            },
        };
        let r = BlockRef::try_new(
            seq.next().unwrap(),
            BlockName::new(name).unwrap(),
            Backing::Part {
                parent: parent.clone(),
                info,
            },
        )
        .unwrap();
        reg.insert(r.clone()).unwrap();
        r
    }

    /// Unpublish `dev`'s subtree and kill each member, as a removal does.
    pub(crate) fn remove(reg: &mut Registry, dev: &BlockRef) {
        let mut out: [Option<BlockRef>; MAX_BLOCKDEVS] = [const { None }; MAX_BLOCKDEVS];
        let n = reg.unpublish(dev, &mut out).unwrap();
        for r in out.iter().take(n).flatten() {
            r.kill().unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{disk, part, remove};
    use super::*;

    #[test]
    fn blockdev_unregister_partition_gone() {
        let seq = DiskSeq::new();
        let mut reg = Registry::new();
        let d = disk(&mut reg, &seq, b"fake", 64);
        let p = part(&mut reg, &seq, &d, b"fakep1", 8, 16);
        let h = reg.lookup(b"fakep1").unwrap();
        let old = h.id();
        let mut buf = [0u8; 512];
        h.read(0, &mut buf).unwrap();
        drop(p);
        remove(&mut reg, &h);
        assert_eq!(h.read(0, &mut buf), Err(BlockError::Gone));
        assert_eq!(h.write(0, &buf), Err(BlockError::Gone));
        assert_eq!(h.flush(), Err(BlockError::Gone));
        assert!(reg.lookup(b"fakep1").is_none());
        let again = part(&mut reg, &seq, &d, b"fakep1", 8, 16);
        assert!(again.id() > old);
        again.read(0, &mut buf).unwrap();
    }

    #[test]
    fn blockdev_ids_never_reused() {
        let seq = DiskSeq::new();
        let mut reg = Registry::new();
        let a = disk(&mut reg, &seq, b"a", 8);
        assert_eq!(a.id(), 1);
        remove(&mut reg, &a);
        let b = disk(&mut reg, &seq, b"a", 8);
        assert_eq!(b.id(), 2);
        assert!(reg.lookup_id(1).is_none());
        assert!(reg.lookup_id(2).unwrap().same(&b));
        let end = DiskSeq {
            next: statics::AtomicU64::new(u64::MAX - 1),
        };
        assert_eq!(end.next(), Ok(u64::MAX - 1));
        assert_eq!(end.next(), Err(BlockError::NoMem));
        assert_eq!(end.next(), Err(BlockError::NoMem));
    }

    #[test]
    fn blockdev_name_rules() {
        assert!(BlockName::new(b"").is_err());
        assert!(BlockName::new(b".x").is_err());
        assert!(BlockName::new(b"a/b").is_err());
        assert!(BlockName::new(b"a b").is_err());
        assert!(BlockName::new(&[b'a'; 33]).is_err());
        let n = BlockName::new(&[b'a'; 32]).unwrap();
        assert_eq!(n.as_bytes().len(), 32);
        let v = BlockName::new(b"vda").unwrap();
        assert_eq!(v.as_str(), "vda");
        assert_eq!(BlockName::child(&v, 1).unwrap().as_str(), "vdap1");
        assert_eq!(BlockName::child(&v, 128).unwrap().as_str(), "vdap128");
        assert_eq!(
            BlockName::child(&v, u32::MAX).unwrap().as_str(),
            "vdap4294967295"
        );
        assert_eq!(BlockName::child(&v, 0), Err(BlockError::Inval));
        let long = BlockName::new(&[b'x'; 30]).unwrap();
        assert!(BlockName::child(&long, 1).is_ok());
        assert_eq!(BlockName::child(&long, 10), Err(BlockError::Inval));
    }

    #[test]
    fn blockdev_child_is_offset_limited() {
        let seq = DiskSeq::new();
        let mut reg = Registry::new();
        let d = disk(&mut reg, &seq, b"fake", 64);
        let p = part(&mut reg, &seq, &d, b"fakep1", 8, 16);
        assert_eq!(p.capacity_sectors(), Ok(16));
        assert_eq!(p.logical_block_size(), Ok(512));
        assert!(p.parent().unwrap().same(&d));
        assert_eq!(p.part().unwrap().start, 8);
        let mut a = [0u8; 512];
        let mut b = [0u8; 512];
        p.read(3, &mut a).unwrap();
        d.read(11, &mut b).unwrap();
        assert_eq!(a, b);
        p.write(15, &[7u8; 512]).unwrap();
        d.read(23, &mut b).unwrap();
        assert_eq!(b, [7u8; 512]);
        assert_eq!(p.read(16, &mut a), Err(BlockError::Inval));
        let mut two = [0u8; 1024];
        assert_eq!(p.read(15, &mut two), Err(BlockError::Inval));
        assert_eq!(p.read(0, &mut a[..100]), Err(BlockError::Inval));
        assert_eq!(p.read_dev(0, &mut a), Err(BlockError::Inval));
        assert_eq!(p.discard(16, 1), Err(BlockError::Inval));
        // A window past the disk's end is refused at build time.
        let bad = BlockRef::try_new(
            seq.next().unwrap(),
            BlockName::new(b"fakep2").unwrap(),
            Backing::Part {
                parent: d.clone(),
                info: PartInfo {
                    start: 60,
                    nsect: 8,
                    kind: PartKind::Mbr { sys: 0x83 },
                },
            },
        );
        assert_eq!(bad.err(), Some(BlockError::Inval));
    }

    #[test]
    fn blockdev_unpublish_disk_takes_children() {
        let seq = DiskSeq::new();
        let mut reg = Registry::new();
        let d = disk(&mut reg, &seq, b"fake", 64);
        let p1 = part(&mut reg, &seq, &d, b"fakep1", 0, 8);
        let p2 = part(&mut reg, &seq, &d, b"fakep2", 8, 8);
        let other = disk(&mut reg, &seq, b"other", 8);
        // Still published: kill refuses.
        assert_eq!(p1.kill(), Err(BlockError::Inval));
        let mut small: [Option<BlockRef>; 2] = [None, None];
        assert_eq!(reg.unpublish(&d, &mut small), Err(BlockError::Inval));
        assert!(reg.lookup(b"fakep1").is_some());
        let mut out: [Option<BlockRef>; MAX_BLOCKDEVS] = [const { None }; MAX_BLOCKDEVS];
        assert_eq!(reg.unpublish(&d, &mut out), Ok(3));
        assert!(out[0].as_ref().unwrap().parent().is_some());
        assert!(out[1].as_ref().unwrap().parent().is_some());
        assert!(out[2].as_ref().unwrap().same(&d));
        for r in out.iter().flatten() {
            r.kill().unwrap();
        }
        let mut buf = [0u8; 512];
        assert_eq!(p2.read(0, &mut buf), Err(BlockError::Gone));
        assert_eq!(d.read_dev(0, &mut buf), Err(BlockError::Gone));
        assert_eq!(d.state(), DeviceState::Failed);
        assert!(reg.lookup(b"fake").is_none());
        assert!(reg.lookup(b"fakep2").is_none());
        assert_eq!(reg.unpublish(&d, &mut out), Err(BlockError::Gone));
        other.read(0, &mut buf).unwrap();
        // A partition of a gone disk cannot be published.
        let orphan = BlockRef::try_new(
            seq.next().unwrap(),
            BlockName::new(b"fakep3").unwrap(),
            Backing::Part {
                parent: d.clone(),
                info: PartInfo {
                    start: 0,
                    nsect: 1,
                    kind: PartKind::Mbr { sys: 0x83 },
                },
            },
        );
        // Geometry of a killed parent is `Gone`.
        assert_eq!(orphan.err(), Some(BlockError::Gone));
    }

    #[test]
    fn blockdev_registry_full() {
        let seq = DiskSeq::new();
        let mut reg = Registry::new();
        let mut held = std::vec::Vec::new();
        for i in 0..MAX_BLOCKDEVS {
            let name = [b'd', b'0' + i as u8];
            held.push(disk(&mut reg, &seq, &name, 8));
        }
        let extra = BlockRef::try_new(
            seq.next().unwrap(),
            BlockName::new(b"z").unwrap(),
            Backing::Disk {
                ops: TryBox::<dyn BlockDevice>::try_new_unsize(
                    testing::MemDisk::new(512, 8),
                    |b| b,
                )
                .unwrap(),
                cache: None,
            },
        )
        .unwrap();
        assert_eq!(reg.insert(extra.clone()), Err(BlockError::NoMem));
        let dup = BlockRef::try_new(
            seq.next().unwrap(),
            BlockName::new(b"d0").unwrap(),
            Backing::Disk {
                ops: TryBox::<dyn BlockDevice>::try_new_unsize(
                    testing::MemDisk::new(512, 8),
                    |b| b,
                )
                .unwrap(),
                cache: None,
            },
        )
        .unwrap();
        assert_eq!(reg.insert(dup), Err(BlockError::Exists));
        let mut out: [Option<BlockRef>; MAX_BLOCKDEVS] = [const { None }; MAX_BLOCKDEVS];
        assert_eq!(reg.snapshot(&mut out), MAX_BLOCKDEVS);
        let mut two: [Option<BlockRef>; 2] = [None, None];
        assert_eq!(reg.snapshot(&mut two), 2);
        remove(&mut reg, &held[3]);
        reg.insert(extra.clone()).unwrap();
        assert!(reg.lookup(b"z").unwrap().same(&extra));
        assert_eq!(reg.insert(extra), Err(BlockError::Exists));
    }
}
