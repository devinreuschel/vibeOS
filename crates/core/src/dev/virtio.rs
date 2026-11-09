//! Modern virtio PCI transport and split virtqueue. ROADMAP §6.5.
//!
//! Ring math and cap decode live here so a simulated device can host-test
//! index wrap, indirect descriptors, and `EVENT_IDX`. Packed VQ is later.
//! Kernel MMIO / DMA / MSI-X live in `virtio_init`.

use core::marker::PhantomData;

use crate::arch::{Barriers, MmioWidth};
use crate::atomic::AtomicU16;

use crate::dma::{self, publish_index};
use crate::kalloc::TryBox;
use crate::limits::MAX_VIRTQ_SIZE;
use crate::pci::{
    self, Bdf, CAP_VENDOR, CFG_CAP_PTR, CFG_STATUS, CfgIo, MAX_CAP_WALK, STATUS_CAPS,
};

pub const VENDOR_ID: u16 = 0x1AF4;

/// Modern PCI device IDs are `0x1040 + virtio device id` (virtio 1.2 §4.1.2).
pub const DEV_RNG_MODERN: u16 = 0x1044;
/// Transitional entropy source. Probe still requires [`F_VERSION_1`].
/// Transitional `0x1004` is SCSI host, not rng.
pub const DEV_RNG_LEGACY: u16 = 0x1005;
/// Modern virtio-blk (`0x1040 + 2`).
pub const DEV_BLK_MODERN: u16 = 0x1042;
/// Transitional virtio-blk. Probe still requires [`F_VERSION_1`].
pub const DEV_BLK_LEGACY: u16 = 0x1001;
/// Modern virtio-input (`0x1040 + 18`).
pub const DEV_INPUT_MODERN: u16 = 0x1052;

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

/// virtio 1.2 §4.2.2 MMIO register layout (modern, Version = 2).
///
/// `QueueReady` is 0x044 in Linux uapi `virtio_mmio.h` (BSD) and QEMU 8.2's
/// copy of it. 0x03c is legacy `QueueAlign`; a Version=2 window ignores
/// writes there and never installs the rings.
pub const MMIO_MAGIC: u32 = 0x7472_6976;
pub const MMIO_VERSION: u32 = 2;
pub const MMIO_OFF_MAGIC: u16 = 0x000;
pub const MMIO_OFF_VERSION: u16 = 0x004;
pub const MMIO_OFF_DEVICE_ID: u16 = 0x008;
pub const MMIO_OFF_VENDOR_ID: u16 = 0x00c;
pub const MMIO_OFF_DEV_FEATURES: u16 = 0x010;
pub const MMIO_OFF_DEV_FEATURES_SEL: u16 = 0x014;
pub const MMIO_OFF_DRV_FEATURES: u16 = 0x020;
pub const MMIO_OFF_DRV_FEATURES_SEL: u16 = 0x024;
pub const MMIO_OFF_QSEL: u16 = 0x030;
pub const MMIO_OFF_QNUM_MAX: u16 = 0x034;
pub const MMIO_OFF_QNUM: u16 = 0x038;
/// Legacy `QueueAlign`. Version=2 does not implement this offset.
pub const MMIO_OFF_QALIGN_LEGACY: u16 = 0x03c;
pub const MMIO_OFF_QREADY: u16 = 0x044;
pub const MMIO_OFF_QNOTIFY: u16 = 0x050;
pub const MMIO_OFF_ISR: u16 = 0x060;
pub const MMIO_OFF_ISR_ACK: u16 = 0x064;
pub const MMIO_OFF_STATUS: u16 = 0x070;
pub const MMIO_OFF_QDESC: u16 = 0x080;
pub const MMIO_OFF_QDRIVER: u16 = 0x090;
pub const MMIO_OFF_QDEVICE: u16 = 0x0a0;
pub const MMIO_OFF_CONFIG: u16 = 0x100;

/// Virtio device id: block (virtio 1.2 §5).
pub const ID_BLOCK: u32 = 2;
/// Virtio device id: entropy source.
pub const ID_ENTROPY: u32 = 4;
/// Virtio device id: input.
pub const ID_INPUT: u32 = 18;

/// PCI class used for a virtio-mmio transport published from the DT `reg`.
pub const CLASS_MMIO: u8 = 0xFF;

/// Modern PCI device id `0x1040 + virtio device id`, or `None` for an
/// empty transport (device id 0) or a value that would wrap.
#[must_use]
pub const fn modern_pci_id(virtio_id: u32) -> Option<u16> {
    if virtio_id == 0 || virtio_id > 0xFBF {
        return None;
    }
    Some(0x1040u16.saturating_add(virtio_id as u16))
}

/// Whether a virtio-mmio identity pair is a modern transport.
#[must_use]
pub const fn mmio_ident_ok(magic: u32, version: u32) -> bool {
    magic == MMIO_MAGIC && version == MMIO_VERSION
}

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
    /// The device completed something it was not given, or more than it was
    /// given. The queue is dead until `init`.
    BadUsed,
    /// The device is DMA-non-coherent and this driver runs no cache
    /// maintenance (DESIGN §4.7), so it does not bind.
    NonCoherent,
}

/// A virtio setup error's errno: Linux's for each condition.
impl From<VirtioError> for crate::kerror::KError {
    fn from(e: VirtioError) -> Self {
        match e {
            VirtioError::NoVersion1
            | VirtioError::NoCaps
            | VirtioError::Features
            | VirtioError::NonCoherent => Self::NoDev,
            VirtioError::BadQueue
            | VirtioError::Notify
            | VirtioError::Failed
            | VirtioError::BadUsed => Self::Io,
            VirtioError::NoDesc => Self::Again,
            VirtioError::NoMemory => Self::NoMem,
        }
    }
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
            VirtioError::BadUsed => "bad used ring",
            VirtioError::NonCoherent => "non-coherent DMA",
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

/// Value written to a queue notify register: the virtqueue index
/// (virtio 1.2 §§4.1.5.2 PCI Queue Notify, 4.2.2 MMIO QueueNotify).
#[must_use]
pub const fn queue_notify(qi: u16) -> u16 {
    qi
}

/// Notify the device of queue `qi` at its doorbell, through the port's
/// ordered [`Barriers::mmio_write`]: the write follows every earlier store,
/// the descriptors and `avail.idx` included, so the caller adds no barrier
/// (DESIGN §4.7). Every driver's kick is this call. `notify32` picks
/// virtio-mmio's 32-bit QueueNotify over PCI's 16-bit register.
///
/// # Safety
/// `doorbell` is the queue's mapped, aligned notify register, which the
/// caller's driver owns.
pub unsafe fn notify<A: Barriers>(doorbell: u64, qi: u16, notify32: bool) {
    // SAFETY: the doorbell is the caller's register, this fn's `# Safety`
    // section; established by `virtio::notify`'s caller.
    unsafe {
        if notify32 {
            A::mmio_write(doorbell as *mut u32, u32::from(queue_notify(qi)));
        } else {
            A::mmio_write(doorbell as *mut u16, queue_notify(qi));
        }
    }
}

/// Read an interrupt-status register (PCI's ISR byte, virtio-mmio's
/// InterruptStatus) through the port's ordered [`Barriers::mmio_read`]:
/// the read completes before any later load, so the ring reads that follow
/// see what the status announced (DESIGN §4.7).
///
/// # Safety
/// `reg` is the device's mapped, aligned status register, and the read
/// (which acknowledges on PCI) is one the caller's driver owns.
pub unsafe fn read_isr<A: Barriers, W: MmioWidth>(reg: *const W) -> W {
    // SAFETY: the register is the caller's, this fn's `# Safety` section;
    // established by `virtio::read_isr`'s caller.
    unsafe { A::mmio_read(reg) }
}

/// Smallest `length` a capability of `cfg_type` may declare and still hold
/// the registers the driver reads: the common config is 0x38 bytes
/// (virtio 1.2 §4.1.4.3), a notify register 2 (§4.1.4.4), the ISR status 1
/// (§4.1.4.5). A device config's length depends on its device, so its driver
/// checks it, and this returns 0 for it.
pub const fn min_cap_len(cfg_type: u8) -> u32 {
    match cfg_type {
        PCI_CAP_COMMON => 0x38,
        PCI_CAP_NOTIFY => 2,
        PCI_CAP_ISR => 1,
        _ => 0,
    }
}

/// Whether `[offset, offset + length)` lies inside a BAR of `bar_size` bytes,
/// and `length` is at least `min_len`.
pub fn cap_fits(offset: u32, length: u32, min_len: u32, bar_size: u64) -> bool {
    length >= min_len
        && (offset as u64)
            .checked_add(length as u64)
            .is_some_and(|end| end <= bar_size)
}

/// `notify = bar_va + cap.offset + queue_notify_off * multiplier`. The
/// two-byte register must end inside `cap_length`, which must be nonzero.
/// `None` on wrap or past `cap.length`.
pub fn notify_addr(
    bar_va: u64,
    cap_offset: u32,
    cap_length: u32,
    queue_notify_off: u16,
    multiplier: u32,
) -> Option<u64> {
    let delta = (queue_notify_off as u64).checked_mul(multiplier as u64)?;
    if cap_length == 0 || delta.checked_add(2)? > cap_length as u64 {
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
        Self::with_granule(size, 4)
    }

    /// [`new`](Self::new) with the used ring, which the device writes, at a
    /// `granule` boundary and the total sized in granules, so for a
    /// non-coherent device no granule holds both what the CPU writes (the
    /// descriptors and the avail ring) and what the device writes (DESIGN
    /// §4.7). `granule` is a power of two; below 4 it is 4, the used ring's
    /// alignment (virtio 1.2 §2.7).
    pub fn with_granule(size: u16, granule: usize) -> Option<Self> {
        if !is_pow2_u16(size) || size > MAX_VIRTQ_SIZE {
            return None;
        }
        let g = granule.max(4);
        let n = size as usize;
        let desc = n * 16;
        let avail = 4 + 2 * n + 2;
        let used_off = dma::granule_round(desc + avail, g)?;
        let used = 4 + 8 * n + 2;
        let total = if g == 4 {
            used_off + used
        } else {
            dma::granule_round(used_off + used, g)?
        };
        Some(Self {
            size,
            desc_off: 0,
            avail_off: desc,
            used_off,
            total,
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

// SAFETY: invariant I53: the memory at the pointer is owned alongside the
// queue that holds it (the kernel keeps its `DmaBuffer` with it) and reached
// only through that queue, so moving the queue to another thread moves that
// access with it; established by `virtio::SplitQueue::new`.
unsafe impl Send for RingPtr {}

// `Shadow::link` holds an index in a byte.
const _: () = assert!(MAX_VIRTQ_SIZE <= 256);

/// A [`SplitQueue`]'s guest-only copy of each descriptor's next. The
/// table's own `next` and `flags` are device-visible, so freeing and
/// allocating walk this instead.
struct Shadow {
    /// Indices fit a byte, since a queue holds at most `MAX_VIRTQ_SIZE`.
    link: [u8; MAX_VIRTQ_SIZE as usize],
    /// Whether a chain continues past each descriptor, set by `add_chain`.
    has_next: [u64; MAX_VIRTQ_SIZE as usize / 64],
}

/// Split VQ over a caller-owned DMA / sim buffer. `A` is the port whose
/// [`Barriers`] order the ring against the device.
pub struct SplitQueue<A: Barriers> {
    layout: SplitLayout,
    base: RingPtr,
    last_used: u16,
    free_head: u16,
    num_free: u16,
    /// Chain heads handed to the device and not yet completed. Guest memory
    /// only, so a device cannot forge it.
    inflight: [u64; MAX_VIRTQ_SIZE as usize / 64],
    /// Each descriptor's next, for the free list and for chains. The
    /// table's own `next` and `flags` are device-visible, so freeing and
    /// allocating walk this guest-only copy (as Linux's `desc_extra`).
    /// Guest-only descriptor links, on the heap so a queue array in a
    /// driver's probe frame stays small (as Linux's `desc_extra` is).
    shadow: TryBox<Shadow>,
    /// A bad completion was seen; `get_used` fails until `init`.
    broken: bool,
    pub event_idx: bool,
    /// Avail idx last published. Kick uses this as `old`.
    last_avail: u16,
    /// The device snoops the CPU's caches. When clear, each ring store is
    /// cleaned to the Point of Coherency after it and each load from the
    /// used ring invalidated before it (DESIGN §4.7).
    coherent: bool,
    _port: PhantomData<fn() -> A>,
}

impl<A: Barriers> SplitQueue<A> {
    /// A queue over the ring memory at `base`.
    ///
    /// # Safety
    /// Invariant I53: `base` is 16-byte aligned and valid for volatile
    /// reads and writes of `layout.total` bytes for the queue's whole life,
    /// and nothing else reaches that memory but the device the queue serves.
    /// `NoMemory` when the guest-only links cannot be allocated.
    pub unsafe fn new(
        layout: SplitLayout,
        base: *mut u8,
        event_idx: bool,
    ) -> Result<Self, VirtioError> {
        let shadow = TryBox::try_new(Shadow {
            link: [0; MAX_VIRTQ_SIZE as usize],
            has_next: [0; MAX_VIRTQ_SIZE as usize / 64],
        })
        .map_err(|_| VirtioError::NoMemory)?;
        Ok(Self {
            layout,
            base: RingPtr(base),
            last_used: 0,
            free_head: 0,
            num_free: layout.size,
            inflight: [0; MAX_VIRTQ_SIZE as usize / 64],
            shadow,
            broken: false,
            event_idx,
            last_avail: 0,
            coherent: true,
            _port: PhantomData,
        })
    }

    /// Serve a non-coherent device: from now on the queue runs the port's
    /// cache maintenance around each ring access. `BadQueue` unless the used
    /// ring starts on a [`Barriers::dma_granule`] boundary, as
    /// [`SplitLayout::with_granule`] places it.
    pub fn set_non_coherent(&mut self) -> Result<(), VirtioError> {
        let g = A::dma_granule();
        let used = (self.base.0 as usize).wrapping_add(self.layout.used_off);
        if !g.is_power_of_two() || used & (g - 1) != 0 {
            return Err(VirtioError::BadQueue);
        }
        self.coherent = false;
        Ok(())
    }

    /// Whether the queue serves a coherent device.
    pub fn coherent(&self) -> bool {
        self.coherent
    }

    /// Before a load of `len` bytes at ring offset `off`: for a
    /// non-coherent device, invalidate it when the device writes it (the
    /// used ring), so the load sees memory, not a stale line.
    fn before_load(&self, p: *const u8, off: usize, len: usize) {
        if !self.coherent && off >= self.layout.used_off {
            // SAFETY: invariant I53: `[p, p + len)` lies inside the queue's
            // ring (checked by `field`), and the CPU stores nothing to the
            // used ring once `init` has cleaned it; established by
            // `virtio::SplitQueue::new`.
            unsafe { A::sync_for_cpu(p, len) };
        }
    }

    /// After a store of `len` bytes at `p`: for a non-coherent device,
    /// clean it to the Point of Coherency before the device can be told.
    fn after_store(&self, p: *const u8, len: usize) {
        if !self.coherent {
            // SAFETY: invariant I53: `[p, p + len)` lies inside the queue's
            // ring (checked by `field`); established by
            // `virtio::SplitQueue::new`.
            unsafe { A::sync_for_device(p, len) };
        }
    }

    // The layout and the cursors are private, with these read accessors,
    // so the safe methods reach only the ring memory `new`'s contract
    // covers (ROADMAP §10.3, F042).
    pub fn layout(&self) -> SplitLayout {
        self.layout
    }

    /// The used index this queue has consumed up to.
    pub fn last_used(&self) -> u16 {
        self.last_used
    }

    /// Descriptors free for `add`.
    pub fn num_free(&self) -> u16 {
        self.num_free
    }

    /// The avail index last published, which a kick passes as `old`.
    pub fn last_avail(&self) -> u16 {
        self.last_avail
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
        self.before_load(p, off, 2);
        // SAFETY: invariant I53: `p` lies inside the queue's ring (checked
        // by `field`), which `base` keeps valid and aligned; established by
        // `virtio::SplitQueue::new`.
        unsafe { load_u16(p, 0) }
    }

    fn wr16(&self, off: usize, v: u16) {
        let p = self.field(off, 2);
        // SAFETY: invariant I53, as in `rd16`; established by
        // `virtio::SplitQueue::new`.
        unsafe { store_u16(p, 0, v) };
        self.after_store(p, 2)
    }

    fn rd32(&self, off: usize) -> u32 {
        let p = self.field(off, 4);
        self.before_load(p, off, 4);
        // SAFETY: invariant I53, as in `rd16`; established by
        // `virtio::SplitQueue::new`.
        unsafe { load_u32(p, 0) }
    }

    fn wr32(&self, off: usize, v: u32) {
        let p = self.field(off, 4);
        // SAFETY: invariant I53, as in `rd16`; established by
        // `virtio::SplitQueue::new`.
        unsafe { store_u32(p, 0, v) };
        self.after_store(p, 4)
    }

    fn rd64(&self, off: usize) -> u64 {
        let p = self.field(off, 8);
        self.before_load(p, off, 8);
        // SAFETY: invariant I53, as in `rd16`; established by
        // `virtio::SplitQueue::new`.
        unsafe { load_u64(p, 0) }
    }

    fn wr64(&self, off: usize, v: u64) {
        let p = self.field(off, 8);
        // SAFETY: invariant I53, as in `rd16`; established by
        // `virtio::SplitQueue::new`.
        unsafe { store_u64(p, 0, v) };
        self.after_store(p, 8)
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
            let next = if i + 1 < n { i + 1 } else { 0 };
            self.wr16(off + 14, next);
            self.set_link(i, next);
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
        self.inflight = [0; MAX_VIRTQ_SIZE as usize / 64];
        self.shadow.has_next = [0; MAX_VIRTQ_SIZE as usize / 64];
        self.broken = false;
        self.last_used = 0;
        self.last_avail = 0;
    }

    fn pop_free(&mut self) -> Option<u16> {
        if self.num_free == 0 {
            return None;
        }
        let i = self.free_head;
        self.free_head = self.link_of(i);
        self.num_free -= 1;
        Some(i)
    }

    fn push_free(&mut self, i: u16) {
        self.wr16(self.layout.desc(i) + 14, self.free_head);
        self.wr16(self.layout.desc(i) + 12, 0);
        self.set_link(i, self.free_head);
        self.free_head = i;
        self.num_free = self.num_free.saturating_add(1);
    }

    /// `i`'s guest-only next; 0 for an index past the table.
    fn link_of(&self, i: u16) -> u16 {
        u16::from(self.shadow.link.get(i as usize).copied().unwrap_or(0))
    }

    fn set_link(&mut self, i: u16, next: u16) {
        // `next < size <= MAX_VIRTQ_SIZE` (256), so it fits a byte.
        if let (Some(l), Ok(n)) = (self.shadow.link.get_mut(i as usize), u8::try_from(next)) {
            *l = n;
        }
    }

    fn continues(&self, i: u16) -> bool {
        self.shadow
            .has_next
            .get(i as usize / 64)
            .is_some_and(|w| w & (1 << (i as u32 % 64)) != 0)
    }

    fn set_continues(&mut self, i: u16, on: bool) {
        if let Some(w) = self.shadow.has_next.get_mut(i as usize / 64) {
            if on {
                *w |= 1 << (i as u32 % 64);
            } else {
                *w &= !(1 << (i as u32 % 64));
            }
        }
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
            self.set_link(ids[i], next);
            self.set_continues(ids[i], !last);
            i += 1;
        }
        let head = ids[0];
        self.set_inflight(head, true);
        let aidx = self.rd16(self.layout.avail_idx());
        self.wr16(self.layout.avail_ring(aidx), head);
        Ok(head)
    }

    fn set_inflight(&mut self, head: u16, on: bool) {
        let (w, b) = (head as usize / 64, head as u32 % 64);
        if let Some(word) = self.inflight.get_mut(w) {
            if on {
                *word |= 1 << b;
            } else {
                *word &= !(1 << b);
            }
        }
    }

    /// Clears `head`'s in-flight bit; whether it was set.
    fn take_inflight(&mut self, head: u16) -> bool {
        let was = self
            .inflight
            .get(head as usize / 64)
            .is_some_and(|w| w & (1 << (head as u32 % 64)) != 0);
        self.set_inflight(head, false);
        was
    }

    /// Free the chain at `head`, an in-flight head below the queue size.
    /// Walks the guest-only links `add_chain` recorded, never the
    /// device-visible table, so a device that rewrites a descriptor's
    /// `next` or `flags` cannot free a descriptor that is still in flight.
    fn free_chain(&mut self, head: u16) {
        let mut i = head;
        // `add_chain` links at most `MAX_CHAIN` descriptors.
        for _ in 0..MAX_CHAIN {
            let more = self.continues(i);
            let next = self.link_of(i);
            self.set_continues(i, false);
            self.push_free(i);
            if !more {
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

    /// The next completion, with its chain freed. A device that completes
    /// more than it was given, an id past the queue, or a chain that is not
    /// in flight breaks the queue: that and every later call is `BadUsed`.
    pub fn get_used(&mut self) -> Result<Option<UsedElem>, VirtioError> {
        if self.broken {
            return Err(VirtioError::BadUsed);
        }
        let pending = self.pending();
        if pending == 0 {
            return Ok(None);
        }
        dma::dma_rmb::<A>();
        let off = self.layout.used_elem(self.last_used);
        let raw = self.rd32(off);
        let len = self.rd32(off + 4);
        let id = u16::try_from(raw).ok().filter(|&id| id < self.layout.size);
        let Some(id) = id.filter(|&id| pending <= self.layout.size && self.take_inflight(id))
        else {
            self.broken = true;
            return Err(VirtioError::BadUsed);
        };
        self.last_used = self.last_used.wrapping_add(1);
        self.free_chain(id);
        if self.event_idx {
            self.wr16(self.layout.used_event(), self.last_used);
            // The next `used.idx` load, here or in the caller's loop, comes
            // after the `used_event` store (virtio 1.2 §2.7.13.4.1, F016).
            dma::dma_mb::<A>();
        }
        Ok(Some(UsedElem { id, len }))
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
#[path = "virtio_tests.rs"]
mod tests;
