//! Modern virtio PCI transport and split virtqueue. ROADMAP §6.5.
//!
//! Ring math and cap decode live here so a simulated device can host-test
//! index wrap, indirect descriptors, and `EVENT_IDX`. Packed VQ is later.
//! Kernel MMIO / DMA / MSI-X live in `virtio_init`.

use core::marker::PhantomData;

use crate::arch::Barriers;
use crate::atomic::AtomicU16;

use crate::dma::{self, publish_index};
use crate::pci::{
    self, Bdf, CAP_VENDOR, CFG_CAP_PTR, CFG_STATUS, CfgIo, MAX_CAP_WALK, STATUS_CAPS,
};

pub const VENDOR_ID: u16 = 0x1AF4;

/// Modern PCI device IDs are `0x1040 + virtio device id`.
pub const DEV_RNG_MODERN: u16 = 0x1044;
/// Transitional rng. Probe still requires [`F_VERSION_1`].
pub const DEV_RNG_LEGACY: u16 = 0x1004;
/// Modern virtio-blk (`0x1040 + 2`).
pub const DEV_BLK_MODERN: u16 = 0x1042;
/// Transitional virtio-blk. Probe still requires [`F_VERSION_1`].
pub const DEV_BLK_LEGACY: u16 = 0x1001;

pub const F_INDIRECT_DESC: u64 = 1 << 28;
pub const F_EVENT_IDX: u64 = 1 << 29;
pub const F_VERSION_1: u64 = 1 << 32;

pub const STATUS_ACKNOWLEDGE: u8 = 1;
pub const STATUS_DRIVER: u8 = 2;
pub const STATUS_DRIVER_OK: u8 = 4;
pub const STATUS_FEATURES_OK: u8 = 8;
pub const STATUS_NEEDS_RESET: u8 = 64;
pub const STATUS_FAILED: u8 = 128;

pub const DESC_F_NEXT: u16 = 1;
pub const DESC_F_WRITE: u16 = 2;
pub const DESC_F_INDIRECT: u16 = 4;

pub const AVAIL_F_NO_INTERRUPT: u16 = 1;
pub const USED_F_NO_NOTIFY: u16 = 1;

pub const PCI_CAP_COMMON: u8 = 1;
pub const PCI_CAP_NOTIFY: u8 = 2;
pub const PCI_CAP_ISR: u8 = 3;
pub const PCI_CAP_DEVICE: u8 = 4;
pub const PCI_CAP_PCI_CFG: u8 = 5;

pub const MSI_NO_VECTOR: u16 = 0xFFFF;

pub const COMMON_OFF_DFSEL: u16 = 0;
pub const COMMON_OFF_DF: u16 = 4;
pub const COMMON_OFF_DRSEL: u16 = 8;
pub const COMMON_OFF_DR: u16 = 12;
pub const COMMON_OFF_MSIX_CFG: u16 = 16;
pub const COMMON_OFF_NUM_QUEUES: u16 = 18;
pub const COMMON_OFF_STATUS: u16 = 20;
pub const COMMON_OFF_GEN: u16 = 21;
pub const COMMON_OFF_QSEL: u16 = 22;
pub const COMMON_OFF_QSIZE: u16 = 24;
pub const COMMON_OFF_QMSIX: u16 = 26;
pub const COMMON_OFF_QENABLE: u16 = 28;
pub const COMMON_OFF_QNOTIFY: u16 = 30;
pub const COMMON_OFF_QDESC: u16 = 32;
pub const COMMON_OFF_QDRIVER: u16 = 40;
pub const COMMON_OFF_QDEVICE: u16 = 48;

pub const CAP_HDR: u16 = 16;
pub const MAX_VENDOR_CAPS: usize = 8;

/// Features the transport will accept if the device offers them.
pub const OFFER: u64 = F_VERSION_1 | F_INDIRECT_DESC | F_EVENT_IDX;

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VirtioError {
    NoVersion1,
    BadQueue,
    NoCaps,
    Notify,
    NoDesc,
    Features,
    Failed,
    /// An allocation the driver needs failed (DESIGN §4.4).
    NoMemory,
}

impl VirtioError {
    pub fn as_str(self) -> &'static str {
        match self {
            VirtioError::NoVersion1 => "no VERSION_1",
            VirtioError::BadQueue => "bad queue",
            VirtioError::NoCaps => "no modern caps",
            VirtioError::Notify => "notify off",
            VirtioError::NoDesc => "no desc",
            VirtioError::Features => "features_ok",
            VirtioError::Failed => "failed",
            VirtioError::NoMemory => "no memory",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciCap {
    pub cfg_type: u8,
    pub bar: u8,
    pub offset: u32,
    pub length: u32,
    /// Notify cap only. 0 means every queue shares one doorbell.
    pub notify_off_multiplier: u32,
}

impl PciCap {
    pub const EMPTY: Self = Self {
        cfg_type: 0,
        bar: 0,
        offset: 0,
        length: 0,
        notify_off_multiplier: 0,
    };

    pub fn parse(cfg_type: u8, bar: u8, offset: u32, length: u32, mult: u32) -> Self {
        Self {
            cfg_type,
            bar,
            offset,
            length,
            notify_off_multiplier: if cfg_type == PCI_CAP_NOTIFY { mult } else { 0 },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModernCaps {
    pub common: Option<PciCap>,
    pub notify: Option<PciCap>,
    pub isr: Option<PciCap>,
    pub device: Option<PciCap>,
}

impl ModernCaps {
    pub const fn empty() -> Self {
        Self {
            common: None,
            notify: None,
            isr: None,
            device: None,
        }
    }

    pub fn is_complete(self) -> bool {
        self.common.is_some() && self.notify.is_some() && self.isr.is_some()
    }
}

/// First of each modern cap type. Extra ids of the same type are ignored.
///
/// The capability list is device input (ROADMAP §10.1): a capability whose
/// body runs past the 256-byte header, whose BAR index is not 0 to 5, or
/// whose `offset + length` overflows is skipped, so a crafted list leaves
/// the result incomplete instead of panicking.
#[deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
pub fn read_modern_caps<C: CfgIo>(cfg: &mut C, bdf: Bdf) -> ModernCaps {
    let mut out = ModernCaps::empty();
    let status = pci::read16(cfg, bdf, CFG_STATUS);
    if status & STATUS_CAPS == 0 {
        return out;
    }
    let mut ptr = pci::read8(cfg, bdf, CFG_CAP_PTR);
    let mut n = 0usize;
    while ptr >= 0x40 && n < MAX_CAP_WALK {
        n = n.saturating_add(1);
        let id = pci::read8(cfg, bdf, u16::from(ptr));
        if id == CAP_VENDOR
            && let Some(cap) = read_vendor_cap(cfg, bdf, u16::from(ptr))
        {
            let slot = match cap.cfg_type {
                PCI_CAP_COMMON => Some(&mut out.common),
                PCI_CAP_NOTIFY => Some(&mut out.notify),
                PCI_CAP_ISR => Some(&mut out.isr),
                PCI_CAP_DEVICE => Some(&mut out.device),
                _ => None,
            };
            if let Some(slot) = slot
                && slot.is_none()
            {
                *slot = Some(cap);
            }
        }
        // `ptr` is a u8, so `ptr + 1` fits u16.
        let next = pci::read8(cfg, bdf, u16::from(ptr).saturating_add(1));
        if next == 0 || next == ptr {
            break;
        }
        ptr = next;
    }
    out
}

/// One virtio vendor capability at `ptr`, or `None` when it is short, runs
/// past the 256-byte header, names a BAR index above 5, or its
/// `offset + length` overflows.
#[deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
fn read_vendor_cap<C: CfgIo>(cfg: &mut C, bdf: Bdf, ptr: u16) -> Option<PciCap> {
    let cap_len = u16::from(pci::read8(cfg, bdf, ptr.checked_add(2)?));
    if cap_len < CAP_HDR || ptr.checked_add(cap_len)? > CFG_SPACE_LEN {
        return None;
    }
    let cfg_type = pci::read8(cfg, bdf, ptr.checked_add(3)?);
    let bar = pci::read8(cfg, bdf, ptr.checked_add(4)?);
    if usize::from(bar) >= pci::MAX_BARS {
        return None;
    }
    let offset = cfg.read32(bdf, ptr.checked_add(8)?);
    let length = cfg.read32(bdf, ptr.checked_add(12)?);
    offset.checked_add(length)?;
    let mut mult = 0u32;
    if cfg_type == PCI_CAP_NOTIFY && cap_len >= CAP_HDR.checked_add(4)? {
        mult = cfg.read32(bdf, ptr.checked_add(16)?);
    }
    Some(PciCap::parse(cfg_type, bar, offset, length, mult))
}

/// Bytes of the PCI header's config space, where capabilities live.
const CFG_SPACE_LEN: u16 = 0x100;

pub fn is_rng(vendor: u16, device: u16) -> bool {
    vendor == VENDOR_ID && (device == DEV_RNG_MODERN || device == DEV_RNG_LEGACY)
}

pub fn is_blk(vendor: u16, device: u16) -> bool {
    vendor == VENDOR_ID && (device == DEV_BLK_MODERN || device == DEV_BLK_LEGACY)
}

pub fn is_pow2_u16(n: u16) -> bool {
    n != 0 && n & n.wrapping_sub(1) == 0
}

pub fn ring_index(idx: u16, size: u16) -> u16 {
    idx & size.wrapping_sub(1)
}

/// Linux `vring_need_event`. `EVENT_IDX` kick / interrupt decision.
pub fn need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}

pub fn used_pending(used_idx: u16, last_used: u16) -> u16 {
    used_idx.wrapping_sub(last_used)
}

/// `notify = bar_va + cap.offset + queue_notify_off * multiplier`.
/// `None` on wrap or past `cap.length`.
pub fn notify_addr(
    bar_va: u64,
    cap_offset: u32,
    cap_length: u32,
    queue_notify_off: u16,
    multiplier: u32,
) -> Option<u64> {
    let delta = (queue_notify_off as u64).checked_mul(multiplier as u64)?;
    if cap_length != 0 && delta >= cap_length as u64 {
        return None;
    }
    bar_va.checked_add(cap_offset as u64)?.checked_add(delta)
}

pub fn pick_features(device: u64, offer: u64) -> Result<u64, VirtioError> {
    if device & F_VERSION_1 == 0 {
        return Err(VirtioError::NoVersion1);
    }
    Ok((device & offer) | F_VERSION_1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SplitLayout {
    pub size: u16,
    pub desc_off: usize,
    pub avail_off: usize,
    pub used_off: usize,
    pub total: usize,
}

impl SplitLayout {
    pub fn new(size: u16) -> Option<Self> {
        if !is_pow2_u16(size) {
            return None;
        }
        let n = size as usize;
        let desc = n * 16;
        let avail = 4 + 2 * n + 2;
        let used_off = (desc + avail + 3) & !3;
        let used = 4 + 8 * n + 2;
        Some(Self {
            size,
            desc_off: 0,
            avail_off: desc,
            used_off,
            total: used_off + used,
        })
    }

    pub fn desc(&self, i: u16) -> usize {
        self.desc_off + (ring_index(i, self.size) as usize) * 16
    }

    pub fn avail_idx(&self) -> usize {
        self.avail_off + 2
    }

    pub fn avail_ring(&self, i: u16) -> usize {
        self.avail_off + 4 + (ring_index(i, self.size) as usize) * 2
    }

    pub fn used_event(&self) -> usize {
        self.avail_off + 4 + (self.size as usize) * 2
    }

    pub fn used_idx(&self) -> usize {
        self.used_off + 2
    }

    pub fn used_elem(&self, i: u16) -> usize {
        self.used_off + 4 + (ring_index(i, self.size) as usize) * 8
    }

    pub fn avail_event(&self) -> usize {
        self.used_off + 4 + (self.size as usize) * 8
    }

    pub fn used_flags(&self) -> usize {
        self.used_off
    }
}

/// Little-endian volatile field access to device-shared memory.
///
/// # Safety
/// `base + off` is valid for a volatile read (`load_*`) or write
/// (`store_*`) of the field's size, and aligned to it.
unsafe fn load_u16(base: *const u8, off: usize) -> u16 {
    #[cfg(test)]
    dma::trace::record(dma::trace::Ev::Load(base as usize + off));
    // SAFETY: the caller meets this fn's contract; established by
    // `virtio::load_u16`'s `# Safety` section.
    unsafe { u16::from_le(core::ptr::read_volatile(base.add(off) as *const u16)) }
}

/// # Safety
/// As [`load_u16`], for a write.
unsafe fn store_u16(base: *mut u8, off: usize, v: u16) {
    #[cfg(test)]
    dma::trace::record(dma::trace::Ev::Store(base as usize + off));
    // SAFETY: the caller meets this fn's contract; established by
    // `virtio::load_u16`'s `# Safety` section.
    unsafe { core::ptr::write_volatile(base.add(off) as *mut u16, v.to_le()) }
}

/// # Safety
/// As [`load_u16`], for four bytes.
unsafe fn load_u32(base: *const u8, off: usize) -> u32 {
    #[cfg(test)]
    dma::trace::record(dma::trace::Ev::Load(base as usize + off));
    // SAFETY: the caller meets this fn's contract; established by
    // `virtio::load_u16`'s `# Safety` section.
    unsafe { u32::from_le(core::ptr::read_volatile(base.add(off) as *const u32)) }
}

/// # Safety
/// As [`load_u16`], for a four-byte write.
unsafe fn store_u32(base: *mut u8, off: usize, v: u32) {
    #[cfg(test)]
    dma::trace::record(dma::trace::Ev::Store(base as usize + off));
    // SAFETY: the caller meets this fn's contract; established by
    // `virtio::load_u16`'s `# Safety` section.
    unsafe { core::ptr::write_volatile(base.add(off) as *mut u32, v.to_le()) }
}

/// Two dword reads, low first. `off` is 4-byte aligned.
///
/// # Safety
/// As [`load_u16`], for eight bytes.
unsafe fn load_u64(base: *const u8, off: usize) -> u64 {
    // SAFETY: both dwords lie in the caller's eight valid bytes; established
    // by `virtio::load_u64`'s `# Safety` section.
    let (lo, hi) = unsafe { (load_u32(base, off), load_u32(base, off + 4)) };
    lo as u64 | ((hi as u64) << 32)
}

/// # Safety
/// As [`load_u16`], for an eight-byte write.
unsafe fn store_u64(base: *mut u8, off: usize, v: u64) {
    // SAFETY: both dwords lie in the caller's eight valid bytes; established
    // by `virtio::store_u64`'s `# Safety` section.
    unsafe {
        store_u32(base, off, v as u32);
        store_u32(base, off + 4, (v >> 32) as u32);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsedElem {
    pub id: u16,
    pub len: u32,
}

/// One buffer in a descriptor chain. [`DESC_F_NEXT`] is applied by [`SplitQueue::add_chain`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DescBuf {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
}

pub const MAX_CHAIN: usize = 8;

/// The ring memory a [`SplitQueue`] reaches, at its 16-byte aligned base.
struct RingPtr(*mut u8);

// SAFETY: invariant I233: the memory at the pointer is owned alongside the
// queue that holds it (the kernel keeps its `DmaBuffer` with it) and reached
// only through that queue, so moving the queue to another thread moves that
// access with it; established by `virtio::SplitQueue::new`.
unsafe impl Send for RingPtr {}

/// Split VQ over a caller-owned DMA / sim buffer. `A` is the port whose
/// [`Barriers`] order the ring against the device.
pub struct SplitQueue<A: Barriers> {
    layout: SplitLayout,
    base: RingPtr,
    pub last_used: u16,
    free_head: u16,
    pub num_free: u16,
    pub event_idx: bool,
    /// Avail idx last published. Kick uses this as `old`.
    pub last_avail: u16,
    _port: PhantomData<fn() -> A>,
}

impl<A: Barriers> SplitQueue<A> {
    /// A queue over the ring memory at `base`.
    ///
    /// # Safety
    /// Invariant I233: `base` is 16-byte aligned and valid for volatile
    /// reads and writes of `layout.total` bytes for the queue's whole life,
    /// and nothing else reaches that memory but the device the queue serves.
    pub unsafe fn new(layout: SplitLayout, base: *mut u8, event_idx: bool) -> Self {
        Self {
            layout,
            base: RingPtr(base),
            last_used: 0,
            free_head: 0,
            num_free: layout.size,
            event_idx,
            last_avail: 0,
            _port: PhantomData,
        }
    }

    pub fn layout(&self) -> SplitLayout {
        self.layout
    }

    /// Byte offset `off` of a `len`-byte ring field, checked against the
    /// ring. Every offset comes from `SplitLayout`, which masks each index
    /// by the ring size, so the check holds by construction.
    fn field(&self, off: usize, len: usize) -> *mut u8 {
        assert!(
            off.checked_add(len)
                .is_some_and(|end| end <= self.layout.total),
            "virtio: ring offset past the queue"
        );
        self.base.0.wrapping_add(off)
    }

    fn rd16(&self, off: usize) -> u16 {
        let p = self.field(off, 2);
        // SAFETY: invariant I233: `p` lies inside the queue's ring (checked
        // by `field`), which `base` keeps valid and aligned; established by
        // `virtio::SplitQueue::new`.
        unsafe { load_u16(p, 0) }
    }

    fn wr16(&self, off: usize, v: u16) {
        let p = self.field(off, 2);
        // SAFETY: invariant I233, as in `rd16`; established by
        // `virtio::SplitQueue::new`.
        unsafe { store_u16(p, 0, v) }
    }

    fn rd32(&self, off: usize) -> u32 {
        let p = self.field(off, 4);
        // SAFETY: invariant I233, as in `rd16`; established by
        // `virtio::SplitQueue::new`.
        unsafe { load_u32(p, 0) }
    }

    fn wr32(&self, off: usize, v: u32) {
        let p = self.field(off, 4);
        // SAFETY: invariant I233, as in `rd16`; established by
        // `virtio::SplitQueue::new`.
        unsafe { store_u32(p, 0, v) }
    }

    fn rd64(&self, off: usize) -> u64 {
        let p = self.field(off, 8);
        // SAFETY: invariant I233, as in `rd16`; established by
        // `virtio::SplitQueue::new`.
        unsafe { load_u64(p, 0) }
    }

    fn wr64(&self, off: usize, v: u64) {
        let p = self.field(off, 8);
        // SAFETY: invariant I233, as in `rd16`; established by
        // `virtio::SplitQueue::new`.
        unsafe { store_u64(p, 0, v) }
    }

    /// Chain free descriptors. Zero rings.
    pub fn init(&mut self) {
        let n = self.layout.size;
        let mut i = 0u16;
        while i < n {
            let off = self.layout.desc(i);
            self.wr64(off, 0);
            self.wr32(off + 8, 0);
            self.wr16(off + 12, 0);
            self.wr16(off + 14, if i + 1 < n { i + 1 } else { 0 });
            i += 1;
        }
        self.wr16(self.layout.avail_off, 0);
        self.wr16(self.layout.avail_idx(), 0);
        self.wr16(self.layout.used_flags(), 0);
        self.wr16(self.layout.used_idx(), 0);
        self.wr16(self.layout.used_event(), 0);
        self.wr16(self.layout.avail_event(), 0);
        self.free_head = 0;
        self.num_free = n;
        self.last_used = 0;
        self.last_avail = 0;
    }

    fn pop_free(&mut self) -> Option<u16> {
        if self.num_free == 0 {
            return None;
        }
        let i = self.free_head;
        let next = self.rd16(self.layout.desc(i) + 14);
        self.free_head = next;
        self.num_free -= 1;
        Some(i)
    }

    fn push_free(&mut self, i: u16) {
        self.wr16(self.layout.desc(i) + 14, self.free_head);
        self.wr16(self.layout.desc(i) + 12, 0);
        self.free_head = i;
        self.num_free = self.num_free.saturating_add(1);
    }

    fn write_desc(&self, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let off = self.layout.desc(i);
        self.wr64(off, addr);
        self.wr32(off + 8, len);
        self.wr16(off + 12, flags);
        self.wr16(off + 14, next);
    }

    pub fn add(&mut self, addr: u64, len: u32, flags: u16) -> Result<u16, VirtioError> {
        self.add_chain(&[DescBuf { addr, len, flags }])
    }

    /// Chain `bufs` with [`DESC_F_NEXT`]. Head goes in the avail ring.
    /// `flags` is WRITE/INDIRECT; NEXT is added on every desc but the last.
    pub fn add_chain(&mut self, bufs: &[DescBuf]) -> Result<u16, VirtioError> {
        let n = bufs.len();
        if n == 0 || n > MAX_CHAIN {
            return Err(VirtioError::NoDesc);
        }
        if (self.num_free as usize) < n {
            return Err(VirtioError::NoDesc);
        }
        let mut ids = [0u16; MAX_CHAIN];
        let mut i = 0usize;
        while i < n {
            ids[i] = self.pop_free().ok_or(VirtioError::NoDesc)?;
            i += 1;
        }
        i = 0;
        while i < n {
            let last = i + 1 == n;
            let flags = if last {
                bufs[i].flags
            } else {
                bufs[i].flags | DESC_F_NEXT
            };
            let next = if last { 0 } else { ids[i + 1] };
            self.write_desc(ids[i], bufs[i].addr, bufs[i].len, flags, next);
            i += 1;
        }
        let head = ids[0];
        let aidx = self.rd16(self.layout.avail_idx());
        self.wr16(self.layout.avail_ring(aidx), head);
        Ok(head)
    }

    fn free_chain(&mut self, mut i: u16) {
        let cap = self.layout.size;
        let mut n = 0u16;
        loop {
            if n >= cap {
                break;
            }
            n += 1;
            let flags = self.desc_flags(i);
            let next = self.desc_next(i);
            self.push_free(i);
            if flags & DESC_F_NEXT == 0 {
                break;
            }
            i = next;
        }
    }

    /// One main desc with [`DESC_F_INDIRECT`] pointing at a table.
    pub fn add_indirect(&mut self, table_addr: u64, table_len: u32) -> Result<u16, VirtioError> {
        self.add(table_addr, table_len, DESC_F_INDIRECT)
    }

    /// Descriptor stores first. Then avail.idx with a real store-side barrier.
    pub fn publish(&mut self) -> u16 {
        let old = self.rd16(self.layout.avail_idx());
        let new = old.wrapping_add(1);
        dma::dma_wmb::<A>();
        self.wr16(self.layout.avail_idx(), new);
        self.last_avail = new;
        new
    }

    /// Same publish via [`publish_index`] so the DMA helper stays on the path.
    pub fn publish_atomic(&mut self, slot: &AtomicU16) -> u16 {
        let old = self.rd16(self.layout.avail_idx());
        let new = old.wrapping_add(1);
        self.wr16(self.layout.avail_idx(), new);
        publish_index::<A>(slot, new);
        self.last_avail = new;
        new
    }

    pub fn avail_idx(&self) -> u16 {
        self.rd16(self.layout.avail_idx())
    }

    pub fn used_idx(&self) -> u16 {
        self.rd16(self.layout.used_idx())
    }

    pub fn pending(&self) -> u16 {
        used_pending(self.used_idx(), self.last_used)
    }

    pub fn get_used(&mut self) -> Option<UsedElem> {
        if self.pending() == 0 {
            return None;
        }
        dma::dma_rmb::<A>();
        let off = self.layout.used_elem(self.last_used);
        let id = self.rd32(off) as u16;
        let len = self.rd32(off + 4);
        self.last_used = self.last_used.wrapping_add(1);
        self.free_chain(id);
        if self.event_idx {
            self.wr16(self.layout.used_event(), self.last_used);
            // The next `used.idx` load, here or in the caller's loop, comes
            // after the `used_event` store (virtio 1.2 §2.7.13.4.1, F016).
            dma::dma_mb::<A>();
        }
        Some(UsedElem { id, len })
    }

    /// Whether the device wants a kick for the buffers published since
    /// `old_avail`. Runs [`dma::dma_mb`] first, so its `avail_event` or
    /// `used.flags` load follows `publish`'s `avail.idx` store (virtio 1.2
    /// §2.7.13.4.1, F016).
    pub fn should_kick(&self, old_avail: u16) -> bool {
        dma::dma_mb::<A>();
        let new = self.avail_idx();
        if self.event_idx {
            let event = self.rd16(self.layout.avail_event());
            need_event(event, new, old_avail)
        } else {
            self.rd16(self.layout.used_flags()) & USED_F_NO_NOTIFY == 0
        }
    }

    pub fn desc_addr(&self, i: u16) -> u64 {
        self.rd64(self.layout.desc(i))
    }

    pub fn desc_len(&self, i: u16) -> u32 {
        self.rd32(self.layout.desc(i) + 8)
    }

    pub fn desc_flags(&self, i: u16) -> u16 {
        self.rd16(self.layout.desc(i) + 12)
    }

    pub fn desc_next(&self, i: u16) -> u16 {
        self.rd16(self.layout.desc(i) + 14)
    }
}

/// One WRITE desc in an indirect table.
///
/// # Safety
/// `table` is 16-byte aligned and valid for volatile writes of 16 bytes.
pub unsafe fn write_indirect_write(table: *mut u8, addr: u64, len: u32) {
    // SAFETY: the four fields lie in the caller's 16 valid bytes; established
    // by `virtio::write_indirect_write`'s `# Safety` section.
    unsafe {
        store_u64(table, 0, addr);
        store_u32(table, 8, len);
        store_u16(table, 12, DESC_F_WRITE);
        store_u16(table, 14, 0);
    }
}

/// Simulated device: consume avail, complete WRITE / INDIRECT WRITE descs.
///
/// # Safety
/// `guest_mem` covers the queue's guest physical addresses minus `guest_off`.
pub unsafe fn sim_complete<A: Barriers>(
    q: &mut SplitQueue<A>,
    fill: u8,
    guest_mem: *mut u8,
    guest_off: u64,
) -> u16 {
    let avail = q.avail_idx();
    let used = q.used_idx();
    let n = used_pending(avail, used);
    let mut i = 0u16;
    while i < n {
        let head = q.rd16(q.layout.avail_ring(used.wrapping_add(i)));
        let flags = q.desc_flags(head);
        let mut written = 0u32;
        if flags & DESC_F_INDIRECT != 0 {
            let taddr = q.desc_addr(head);
            let tlen = q.desc_len(head);
            if tlen >= 16 && taddr >= guest_off {
                // SAFETY: the indirect table's 16 bytes lie in `guest_mem`
                // at `taddr - guest_off`; established by
                // `virtio::sim_complete`'s `# Safety` section.
                let (daddr, dlen, dflags) = unsafe {
                    let p = guest_mem.add((taddr - guest_off) as usize);
                    (load_u64(p, 0), load_u32(p, 8), load_u16(p, 12))
                };
                if dflags & DESC_F_WRITE != 0 && daddr >= guest_off {
                    // SAFETY: the WRITE buffer's `dlen` bytes lie in
                    // `guest_mem`; established by `virtio::sim_complete`'s
                    // `# Safety` section.
                    unsafe {
                        fill_bytes(
                            guest_mem.add((daddr - guest_off) as usize),
                            dlen as usize,
                            fill,
                        )
                    };
                    written = dlen;
                }
            }
        } else if flags & DESC_F_WRITE != 0 {
            let daddr = q.desc_addr(head);
            let dlen = q.desc_len(head);
            if daddr >= guest_off {
                // SAFETY: the WRITE buffer's `dlen` bytes lie in `guest_mem`;
                // established by `virtio::sim_complete`'s `# Safety` section.
                unsafe {
                    fill_bytes(
                        guest_mem.add((daddr - guest_off) as usize),
                        dlen as usize,
                        fill,
                    )
                };
            }
            written = dlen;
        }
        let uoff = q.layout.used_elem(used.wrapping_add(i));
        q.wr32(uoff, head as u32);
        q.wr32(uoff + 4, written);
        i += 1;
    }
    if n != 0 {
        dma::dma_wmb::<A>();
        q.wr16(q.layout.used_idx(), used.wrapping_add(n));
    }
    n
}

/// # Safety
/// `p` is valid for volatile writes of `len` bytes.
unsafe fn fill_bytes(p: *mut u8, len: usize, fill: u8) {
    let mut i = 0usize;
    while i < len {
        // SAFETY: `i < len`; established by `virtio::fill_bytes`'s
        // `# Safety` section.
        unsafe { p.add(i).write_volatile(fill) };
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pci::{
        CFG_COMMAND, CFG_DEVICE, CFG_HEADER_TYPE, CFG_REVID_CLASS, CFG_VENDOR, HEADER_DEVICE,
        write16,
    };
    use std::vec;
    use std::vec::Vec;

    type Stub = crate::arch::stub::Arch;

    struct Fake {
        data: [u8; 256],
    }

    impl Fake {
        fn new() -> Self {
            Self { data: [0xFF; 256] }
        }
        fn put8(&mut self, off: u16, v: u8) {
            self.data[off as usize] = v;
        }
        fn put16(&mut self, off: u16, v: u16) {
            self.data[off as usize] = v as u8;
            self.data[off as usize + 1] = (v >> 8) as u8;
        }
        fn put32(&mut self, off: u16, v: u32) {
            self.data[off as usize..off as usize + 4].copy_from_slice(&v.to_le_bytes());
        }
        fn device(&mut self) {
            self.put16(CFG_VENDOR, VENDOR_ID);
            self.put16(CFG_DEVICE, DEV_RNG_MODERN);
            self.put8(CFG_HEADER_TYPE, HEADER_DEVICE);
            self.put32(CFG_REVID_CLASS, 0xFF00_0000);
            self.put16(CFG_STATUS, STATUS_CAPS);
            self.put16(CFG_COMMAND, 0);
        }
        #[allow(clippy::too_many_arguments)] // PCI vendor-cap fields
        fn vend(&mut self, off: u8, next: u8, typ: u8, bar: u8, cap_off: u32, len: u32, mult: u32) {
            self.put8(off as u16, CAP_VENDOR);
            self.put8(off as u16 + 1, next);
            self.put8(off as u16 + 2, if typ == PCI_CAP_NOTIFY { 20 } else { 16 });
            self.put8(off as u16 + 3, typ);
            self.put8(off as u16 + 4, bar);
            self.put32(off as u16 + 8, cap_off);
            self.put32(off as u16 + 12, len);
            if typ == PCI_CAP_NOTIFY {
                self.put32(off as u16 + 16, mult);
            }
        }
    }

    impl CfgIo for Fake {
        fn read32(&mut self, _bdf: Bdf, offset: u16) -> u32 {
            let o = (offset as usize) & !3;
            u32::from_le_bytes(self.data[o..o + 4].try_into().unwrap())
        }
        fn write32(&mut self, _bdf: Bdf, offset: u16, value: u32) {
            let o = (offset as usize) & !3;
            self.data[o..o + 4].copy_from_slice(&value.to_le_bytes());
        }
    }

    fn pool(n: usize) -> (Vec<u8>, *mut u8) {
        let mut v = vec![0u8; n + 16];
        let p = v.as_mut_ptr();
        let adj = (16 - (p as usize % 16)) % 16;
        // SAFETY: `adj < 16` and the vector has 16 spare bytes; established
        // here.
        (v, unsafe { p.add(adj) })
    }

    /// A queue over a [`pool`] buffer.
    fn queue(layout: SplitLayout, base: *mut u8, event_idx: bool) -> SplitQueue<Stub> {
        // SAFETY: invariant I233: every test passes a 16-byte-aligned `pool`
        // buffer of at least `layout.total` bytes, which it keeps alive
        // while it uses the queue; established here, by each caller.
        unsafe { SplitQueue::new(layout, base, event_idx) }
    }

    #[test]
    fn version1_required() {
        assert_eq!(pick_features(0, OFFER), Err(VirtioError::NoVersion1));
        assert_eq!(
            pick_features(F_EVENT_IDX, OFFER),
            Err(VirtioError::NoVersion1)
        );
        let f = pick_features(F_VERSION_1 | F_EVENT_IDX | (1 << 5), OFFER).unwrap();
        assert_eq!(f & F_VERSION_1, F_VERSION_1);
        assert_eq!(f & F_EVENT_IDX, F_EVENT_IDX);
        assert_eq!(f & (1 << 5), 0);
        assert_eq!(VirtioError::NoVersion1.as_str(), "no VERSION_1");
        assert!(is_rng(VENDOR_ID, DEV_RNG_MODERN));
        assert!(is_rng(VENDOR_ID, DEV_RNG_LEGACY));
        assert!(!is_rng(0x8086, DEV_RNG_MODERN));
        assert!(is_blk(VENDOR_ID, DEV_BLK_MODERN));
        assert!(is_blk(VENDOR_ID, DEV_BLK_LEGACY));
        assert!(!is_blk(VENDOR_ID, DEV_RNG_MODERN));
    }

    #[test]
    fn notify_uses_multiplier() {
        assert_eq!(
            notify_addr(0x1000, 0x200, 0x1000, 3, 4),
            Some(0x1000 + 0x200 + 12)
        );
        assert_eq!(notify_addr(0x1000, 0x200, 0x1000, 1, 0), Some(0x1200));
        assert_eq!(notify_addr(0x1000, 0x10, 8, 3, 4), None);
        assert!(notify_addr(u64::MAX - 8, 16, 4, 1, 1).is_none());
        assert_eq!(
            PciCap::parse(PCI_CAP_NOTIFY, 1, 0x10, 0x100, 4).notify_off_multiplier,
            4
        );
        assert_eq!(
            PciCap::parse(PCI_CAP_COMMON, 0, 0, 0x38, 99).notify_off_multiplier,
            0
        );
    }

    #[test]
    fn modern_caps_from_vendor_list() {
        let mut f = Fake::new();
        let b = Bdf::new(0, 4, 0);
        f.device();
        f.put8(CFG_CAP_PTR, 0x40);
        f.vend(0x40, 0x54, PCI_CAP_COMMON, 0, 0, 0x38, 0);
        f.vend(0x54, 0x6C, PCI_CAP_NOTIFY, 1, 0x100, 0x20, 4);
        f.vend(0x6C, 0x80, PCI_CAP_ISR, 2, 0, 4, 0);
        f.vend(0x80, 0, PCI_CAP_DEVICE, 0, 0x40, 4, 0);
        let c = read_modern_caps(&mut f, b);
        assert!(c.is_complete());
        assert_eq!(c.common.unwrap().bar, 0);
        assert_eq!(c.notify.unwrap().notify_off_multiplier, 4);
        assert_eq!(c.notify.unwrap().offset, 0x100);
        assert_eq!(c.isr.unwrap().bar, 2);
        assert_eq!(c.device.unwrap().offset, 0x40);
        write16(&mut f, b, CFG_COMMAND, 6);
        assert_eq!(pci::read16(&mut f, b, CFG_COMMAND), 6);
    }

    /// Crafted capability lists (ROADMAP §10.1): each breaks one cap of an
    /// otherwise complete list, and the result is incomplete, with no panic.
    #[test]
    fn read_modern_caps_crafted_chain() {
        let b = Bdf::new(0, 4, 0);
        let good = |f: &mut Fake| {
            f.device();
            f.put8(CFG_CAP_PTR, 0x40);
            f.vend(0x40, 0x54, PCI_CAP_COMMON, 0, 0, 0x38, 0);
            f.vend(0x54, 0x6C, PCI_CAP_NOTIFY, 1, 0x100, 0x20, 4);
            f.vend(0x6C, 0, PCI_CAP_ISR, 2, 0, 4, 0);
        };
        let mut f = Fake::new();
        good(&mut f);
        assert!(read_modern_caps(&mut f, b).is_complete());

        // A `cap_len` shorter than the virtio header.
        let mut f = Fake::new();
        good(&mut f);
        f.put8(0x42, 8);
        assert!(read_modern_caps(&mut f, b).common.is_none());

        // `offset + length` overflows u32.
        let mut f = Fake::new();
        good(&mut f);
        f.put32(0x54 + 8, 0xFFFF_FFF0);
        f.put32(0x54 + 12, 0x20);
        let c = read_modern_caps(&mut f, b);
        assert!(c.notify.is_none() && !c.is_complete());

        // A BAR index of 6 or more.
        for bar in [6u8, 0xFF] {
            let mut f = Fake::new();
            good(&mut f);
            f.put8(0x6C + 4, bar);
            assert!(!read_modern_caps(&mut f, b).is_complete());
        }

        // A cap at 0xFC whose body would run past the 256-byte header.
        let mut f = Fake::new();
        f.device();
        f.put8(CFG_CAP_PTR, 0xFC);
        f.put8(0xFC, CAP_VENDOR);
        f.put8(0xFD, 0);
        f.put8(0xFE, 16);
        f.put8(0xFF, PCI_CAP_COMMON);
        assert_eq!(read_modern_caps(&mut f, b), ModernCaps::empty());
    }

    #[test]
    fn missing_caps_and_no_status() {
        let mut f = Fake::new();
        let b = Bdf::new(0, 4, 0);
        f.device();
        f.put16(CFG_STATUS, 0);
        assert!(!read_modern_caps(&mut f, b).is_complete());
        f.put16(CFG_STATUS, STATUS_CAPS);
        f.put8(CFG_CAP_PTR, 0x40);
        f.vend(0x40, 0, PCI_CAP_COMMON, 0, 0, 0x38, 0);
        assert!(!read_modern_caps(&mut f, b).is_complete());
    }

    #[test]
    fn event_idx_arithmetic() {
        assert!(need_event(0, 1, 0));
        assert!(!need_event(2, 2, 0));
        assert!(need_event(0xFFFF, 0, 0xFFFF));
        assert_eq!(used_pending(3, 1), 2);
        assert_eq!(used_pending(1, 0xFFFF), 2);
        assert_eq!(ring_index(5, 4), 1);
        assert!(is_pow2_u16(8));
        assert!(!is_pow2_u16(0));
        assert!(!is_pow2_u16(3));
        assert!(SplitLayout::new(3).is_none());
        let l = SplitLayout::new(4).unwrap();
        assert_eq!(l.desc(1), 16);
        assert_eq!(l.avail_off, 64);
        assert_eq!(l.avail_idx(), 66);
    }

    #[test]
    fn split_wrap_and_sim_device() {
        let layout = SplitLayout::new(4).unwrap();
        let data_off = layout.total as u64;
        let (_keep, base) = pool(layout.total + 64);
        let mut q = queue(layout, base, true);
        q.init();
        let mut n = 0u16;
        while n < 20 {
            let da = data_off;
            q.add(da, 8, DESC_F_WRITE).unwrap();
            let old = q.last_avail;
            q.publish();
            // SAFETY: descriptors address `base` from offset 0, the `pool`
            // buffer; established here.
            let done = unsafe { sim_complete(&mut q, (0xA0 + n) as u8, base, 0) };
            assert_eq!(done, 1);
            let u = q.get_used().unwrap();
            assert_eq!(u.len, 8);
            assert_eq!(q.get_used(), None);
            assert!(q.should_kick(old) || n > 0);
            n += 1;
        }
        assert_eq!(q.num_free, 4);
        assert_eq!(q.avail_idx(), 20);
        assert_eq!(q.used_idx(), 20);
    }

    /// Whether a `Mb` lies between the last `Store(store)` and the first
    /// `Load(load)` after it in `log`, which must hold both.
    fn mb_between(log: &[dma::trace::Ev], store: usize, load: usize) -> bool {
        use dma::trace::Ev;
        let s = log
            .iter()
            .rposition(|e| *e == Ev::Store(store))
            .expect("store recorded");
        let l = s + log[s..]
            .iter()
            .position(|e| *e == Ev::Load(load))
            .expect("load recorded after the store");
        log[s..l].contains(&Ev::Mb)
    }

    /// F016: `dma_mb` separates `publish`'s `avail.idx` store from
    /// `should_kick`'s `avail_event` (EVENT_IDX) or `used.flags` load, and
    /// `get_used`'s `used_event` store from the next `used.idx` load.
    #[test]
    fn split_queue_dma_mb_order() {
        let layout = SplitLayout::new(4).unwrap();
        let data_off = layout.total as u64;
        for event_idx in [true, false] {
            let (_keep, base) = pool(layout.total + 64);
            let at = |off: usize| base as usize + off;
            let mut q = queue(layout, base, event_idx);
            q.init();
            q.add(data_off, 8, DESC_F_WRITE).unwrap();
            let old = q.last_avail;
            dma::trace::start();
            q.publish();
            let _ = q.should_kick(old);
            let log = dma::trace::take();
            assert!(!log.overflow);
            let load = if event_idx {
                layout.avail_event()
            } else {
                layout.used_flags()
            };
            assert!(
                mb_between(log.as_slice(), at(layout.avail_idx()), at(load)),
                "event_idx {event_idx}: no dma_mb between avail.idx and the kick load"
            );

            // SAFETY: descriptors address `base` from offset 0, the `pool`
            // buffer; established here.
            assert_eq!(unsafe { sim_complete(&mut q, 0x11, base, 0) }, 1);
            if event_idx {
                dma::trace::start();
                assert!(q.get_used().is_some());
                assert_eq!(q.get_used(), None);
                let log = dma::trace::take();
                assert!(!log.overflow);
                assert!(
                    mb_between(
                        log.as_slice(),
                        at(layout.used_event()),
                        at(layout.used_idx())
                    ),
                    "no dma_mb between used_event and the next used.idx load"
                );
            }
        }
    }

    #[test]
    fn indirect_and_publish_index() {
        let layout = SplitLayout::new(8).unwrap();
        let table_off = ((layout.total + 15) & !15) as u64;
        let data_off = table_off + 16;
        let (_keep, base) = pool((data_off as usize) + 16);
        let mut q = queue(layout, base, false);
        q.init();
        // SAFETY: the table's 16 aligned bytes lie in the `pool` buffer;
        // established here.
        unsafe { write_indirect_write(base.add(table_off as usize), data_off, 4) };
        q.add_indirect(table_off, 16).unwrap();
        let slot = AtomicU16::new(0);
        q.publish_atomic(&slot);
        assert_eq!(slot.load(core::sync::atomic::Ordering::Acquire), 1);
        // SAFETY: descriptors address `base` from offset 0; established here.
        assert_eq!(unsafe { sim_complete(&mut q, 0x5A, base, 0) }, 1);
        let u = q.get_used().unwrap();
        assert_eq!(u.len, 4);
        // SAFETY: `data_off` lies in the `pool` buffer; established here.
        unsafe {
            assert_eq!(base.add(data_off as usize).read_volatile(), 0x5A);
        }
        q.wr16(q.layout.used_flags(), USED_F_NO_NOTIFY);
        q.add(data_off, 4, DESC_F_WRITE).unwrap();
        let old = q.last_avail;
        q.publish();
        assert!(!q.should_kick(old));
    }

    #[test]
    fn no_desc_when_full() {
        let layout = SplitLayout::new(2).unwrap();
        let (_keep, base) = pool(layout.total);
        let mut q = queue(layout, base, false);
        q.init();
        q.add(0x1000, 4, DESC_F_WRITE).unwrap();
        q.add(0x2000, 4, DESC_F_WRITE).unwrap();
        assert_eq!(q.add(0x3000, 4, DESC_F_WRITE), Err(VirtioError::NoDesc));
    }

    #[test]
    fn chain_three_and_free() {
        let layout = SplitLayout::new(8).unwrap();
        let (_keep, base) = pool(layout.total);
        let mut q = queue(layout, base, false);
        q.init();
        let head = q
            .add_chain(&[
                DescBuf {
                    addr: 0x1000,
                    len: 16,
                    flags: 0,
                },
                DescBuf {
                    addr: 0x2000,
                    len: 512,
                    flags: 0,
                },
                DescBuf {
                    addr: 0x3000,
                    len: 1,
                    flags: DESC_F_WRITE,
                },
            ])
            .unwrap();
        assert_eq!(q.num_free, 5);
        assert_eq!(q.desc_flags(head) & DESC_F_NEXT, DESC_F_NEXT);
        let d1 = q.desc_next(head);
        assert_eq!(q.desc_flags(d1) & DESC_F_NEXT, DESC_F_NEXT);
        let d2 = q.desc_next(d1);
        assert_eq!(q.desc_flags(d2) & DESC_F_NEXT, 0);
        assert_eq!(q.desc_flags(d2) & DESC_F_WRITE, DESC_F_WRITE);
        assert_eq!(q.desc_addr(head), 0x1000);
        assert_eq!(q.desc_len(d1), 512);
        q.publish();
        let used = q.used_idx();
        let uoff = q.layout.used_elem(used);
        q.wr32(uoff, head as u32);
        q.wr32(uoff + 4, 513);
        dma::dma_wmb::<Stub>();
        q.wr16(q.layout.used_idx(), used.wrapping_add(1));
        let u = q.get_used().unwrap();
        assert_eq!(u.id, head);
        assert_eq!(u.len, 513);
        assert_eq!(q.num_free, 8);
        assert_eq!(q.get_used(), None);
    }
}
