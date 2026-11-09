//! DMA buffers, identity map, SG lists, publish fences. ROADMAP §6.4.
//!
//! IOMMU later: keep every device-visible address behind [`DmaTranslate`].
//! Coherence is the device's (DESIGN §4.7): a buffer for a coherent device
//! syncs with [`dma_wmb`] and [`dma_rmb`]; one for a non-coherent device
//! runs the port's cache maintenance, [`Barriers::sync_for_device`] and
//! [`Barriers::sync_for_cpu`]. The fences are the port's: every barrier
//! here is generic over [`Barriers`] and calls its method (PORTABILITY
//! §11.1).

use crate::arch::Barriers;
use crate::atomic::{AtomicU16, Ordering};

use crate::pmm::{self, Buddy, Frames, PAGE_SIZE};

/// Address a device programs into a descriptor. Not a CPU VA.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct DeviceAddr(pub u64);

impl DeviceAddr {
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaError {
    Size,
    Align,
    Boundary,
    Exhausted,
    SgFull,
}

/// A bad DMA request is a bad argument; an exhausted pool or list, no memory.
impl From<DmaError> for crate::kerror::KError {
    fn from(e: DmaError) -> Self {
        match e {
            DmaError::Size | DmaError::Align | DmaError::Boundary => Self::Inval,
            DmaError::Exhausted | DmaError::SgFull => Self::NoMem,
        }
    }
}

impl DmaError {
    pub fn as_str(self) -> &'static str {
        match self {
            DmaError::Size => "size",
            DmaError::Align => "align",
            DmaError::Boundary => "boundary",
            DmaError::Exhausted => "exhausted",
            DmaError::SgFull => "sg full",
        }
    }
}

/// IOMMU insertion point. Identity until a domain exists.
pub trait DmaTranslate {
    fn dma_to_device(&self, pa: u64) -> DeviceAddr;
    fn dma_from_device(&self, da: DeviceAddr) -> u64;
}

pub struct IdentityDma;

impl DmaTranslate for IdentityDma {
    fn dma_to_device(&self, pa: u64) -> DeviceAddr {
        DeviceAddr(pa)
    }
    fn dma_from_device(&self, da: DeviceAddr) -> u64 {
        da.0
    }
}

/// Current map. Swap the body when IOMMU lands.
pub fn dma_to_device(pa: u64) -> DeviceAddr {
    IdentityDma.dma_to_device(pa)
}

pub fn dma_from_device(da: DeviceAddr) -> u64 {
    IdentityDma.dma_from_device(da)
}

pub const DMA32_BOUNDARY: u64 = 1u64 << 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaAlloc {
    pub size: u64,
    pub align: u64,
    pub boundary: u64,
    /// The device snoops the CPU's caches, as its registry entry records
    /// (`dev::Device::dma_coherent`). Every constructor takes it: there is
    /// no default, since a device with no `dma-coherent` is non-coherent
    /// (DESIGN §4.7).
    pub coherent: bool,
}

impl DmaAlloc {
    /// `size` bytes for a device whose coherence is `coherent`.
    pub const fn new(size: u64, coherent: bool) -> Self {
        Self {
            size,
            align: PAGE_SIZE,
            boundary: 0,
            coherent,
        }
    }

    /// As [`new`](Self::new), never crossing a 4 GiB line.
    pub const fn dma32(size: u64, coherent: bool) -> Self {
        Self {
            size,
            align: PAGE_SIZE,
            boundary: DMA32_BOUNDARY,
            coherent,
        }
    }

    /// Buddy order of the block that holds this request: `size` bytes at
    /// `align`, not crossing `boundary`. A naturally aligned block never
    /// lets `[base, base + size)` cross a power-of-two boundary that
    /// `size` does not exceed, so the only refusals the boundary adds are
    /// a boundary that is not a power of two and a `size` above it.
    pub fn order(&self) -> Option<u8> {
        let order = Buddy::order_for(self.size, self.align)?;
        if self.boundary != 0 && (!self.boundary.is_power_of_two() || self.size > self.boundary) {
            return None;
        }
        Some(order)
    }
}

/// Harness `-device edu,dma_mask=` (ROADMAP §11.5 / §11.7).
pub const EDU_DMA_MASK: u64 = 0xFFFF_FFFF;

/// Whether a device address fits `mask` without truncation.
#[must_use]
pub const fn addr_fits_mask(addr: u64, mask: u64) -> bool {
    addr <= mask
}

/// Physically contiguous DMA memory: a move-only handle with private
/// fields. Only [`alloc_from_buddy`] builds one (the kernel reaches it
/// through `dma_init::alloc`), and [`free_to_buddy`] (`dma_init::free`)
/// takes it by value, so a buffer is freed at most once and only by its
/// owner. [`DmaBuffer::device`] is the translated phys, never a HHDM VA.
///
/// It cannot be copied:
///
/// ```compile_fail,E0382
/// fn twice(b: vibeos::dma::DmaBuffer) -> (vibeos::dma::DmaBuffer, vibeos::dma::DmaBuffer) {
///     (b, b)
/// }
/// ```
///
/// or cloned:
///
/// ```compile_fail,E0277
/// fn need<T: Clone>() {}
/// need::<vibeos::dma::DmaBuffer>();
/// ```
///
/// and safe code cannot build one from an address:
///
/// ```compile_fail,E0451
/// fn forge(b: vibeos::dma::DmaBuffer) -> vibeos::dma::DmaBuffer {
///     vibeos::dma::DmaBuffer { virt: 0x1000, len: 0x1000, ..b }
/// }
/// ```
pub struct DmaBuffer {
    virt: u64,
    len: u64,
    frames: Frames,
    coherent: bool,
}

impl DmaBuffer {
    /// Physical base of the block.
    pub fn phys(&self) -> u64 {
        self.frames.base()
    }

    /// Kernel VA of the block.
    pub fn virt(&self) -> u64 {
        self.virt
    }

    /// Bytes requested; the block may be larger.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Frames the block holds, which a buffer kept past a stuck device
    /// counts (DESIGN §12.3).
    pub fn frame_count(&self) -> usize {
        self.frames.count()
    }

    /// Never true: a buffer covers at least the bytes asked for.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Address a device programs into a descriptor.
    pub fn device(&self) -> DeviceAddr {
        dma_to_device(self.phys())
    }

    /// Whether the device this buffer serves is DMA-coherent.
    pub fn coherent(&self) -> bool {
        self.coherent
    }

    /// Hand the whole buffer to the device: [`sync_range_for_device`]
    /// over its `len` bytes.
    ///
    /// [`sync_range_for_device`]: Self::sync_range_for_device
    pub fn sync_for_device<A: Barriers>(&self) {
        self.sync_range_for_device::<A>(0, self.len);
    }

    /// Take the whole buffer back for the CPU: [`sync_range_for_cpu`] over
    /// its `len` bytes.
    ///
    /// [`sync_range_for_cpu`]: Self::sync_range_for_cpu
    pub fn sync_for_cpu<A: Barriers>(&self) {
        self.sync_range_for_cpu::<A>(0, self.len);
    }

    /// The part of `[off, off + len)` inside the block, as a VA and a length.
    fn clip(&self, off: u64, len: u64) -> (*const u8, usize) {
        let block = (self.frames.count() as u64).saturating_mul(PAGE_SIZE);
        let off = off.min(block);
        let len = len.min(block - off);
        (self.virt.wrapping_add(off) as *const u8, len as usize)
    }

    /// Make the CPU's writes to `[off, off + len)` visible to the device,
    /// before the device is told of them. A coherent device needs
    /// [`dma_wmb`] only; for a non-coherent one the port cleans every line
    /// of the range to the Point of Coherency, whatever the transfer's
    /// direction, so no dirty line is written back later over what the
    /// device writes (DESIGN §4.7). The range is clipped to the block.
    pub fn sync_range_for_device<A: Barriers>(&self, off: u64, len: u64) {
        if self.coherent {
            dma_wmb::<A>();
            return;
        }
        let (p, n) = self.clip(off, len);
        // SAFETY: `[p, p + n)` lies inside this buffer's block, which the
        // buffer owns and the kernel maps at `virt`; established by
        // `dma::alloc_from_buddy`.
        unsafe { A::sync_for_device(p, n) };
    }

    /// Make the device's writes to `[off, off + len)` visible to the CPU,
    /// before the CPU reads them. A coherent device needs [`dma_rmb`] only;
    /// for a non-coherent one the port invalidates every line of the range.
    /// The CPU stores nothing to the range while the device owns it, so the
    /// invalidate discards no CPU store. The range is clipped to the block.
    pub fn sync_range_for_cpu<A: Barriers>(&self, off: u64, len: u64) {
        if self.coherent {
            dma_rmb::<A>();
            return;
        }
        let (p, n) = self.clip(off, len);
        // SAFETY: as in `sync_range_for_device`: the range lies inside the
        // buffer's own block; established by `dma::alloc_from_buddy`.
        unsafe { A::sync_for_cpu(p, n) };
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.virt as *mut u8
    }
}

pub const MAX_SG: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SgEntry {
    pub addr: DeviceAddr,
    pub len: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct SgList {
    pub entries: [SgEntry; MAX_SG],
    pub n: usize,
}

impl SgList {
    pub const fn new() -> Self {
        Self {
            entries: [SgEntry {
                addr: DeviceAddr(0),
                len: 0,
            }; MAX_SG],
            n: 0,
        }
    }

    pub fn push(&mut self, addr: DeviceAddr, len: u32) -> Result<(), DmaError> {
        if self.n >= MAX_SG {
            return Err(DmaError::SgFull);
        }
        if len == 0 {
            return Err(DmaError::Size);
        }
        self.entries[self.n] = SgEntry { addr, len };
        self.n += 1;
        Ok(())
    }

    pub fn from_buffer(buf: &DmaBuffer) -> Result<Self, DmaError> {
        let mut s = Self::new();
        let len = buf.len().min(u32::MAX as u64) as u32;
        s.push(buf.device(), len)?;
        Ok(s)
    }
}

impl Default for SgList {
    fn default() -> Self {
        Self::new()
    }
}

/// Store-side barrier before a device may observe a published index: the
/// port's [`Barriers::dma_wmb`].
#[inline]
pub fn dma_wmb<A: Barriers>() {
    #[cfg(test)]
    trace::record(trace::Ev::Wmb);
    A::dma_wmb();
}

/// Load-side barrier after the device writes a completion: the port's
/// [`Barriers::dma_rmb`].
#[inline]
pub fn dma_rmb<A: Barriers>() {
    #[cfg(test)]
    trace::record(trace::Ev::Rmb);
    A::dma_rmb();
}

/// Full barrier: orders earlier stores before later loads, as well as what
/// [`dma_wmb`] and [`dma_rmb`] order. The port's [`Barriers::dma_mb`]
/// (`mfence` on x86_64). A virtqueue runs it between its index store and the
/// load that decides a kick or re-reads `used.idx` (virtio 1.2
/// §2.7.13.4.1, F016).
#[inline]
pub fn dma_mb<A: Barriers>() {
    #[cfg(test)]
    trace::record(trace::Ev::Mb);
    A::dma_mb();
}

/// Publish `idx` after descriptor stores: [`dma_wmb`], then a Release store.
#[inline]
pub fn publish_index<A: Barriers>(slot: &AtomicU16, idx: u16) {
    dma_wmb::<A>();
    // Release: pairs with the device's read of the index; `dma_wmb` orders it for DMA.
    slot.store(idx, Ordering::Release);
}

/// The one constructor of [`DmaBuffer`]. `virt_of` maps the block's
/// physical base to the kernel VA the CPU uses.
///
/// No address limit applies: `max_phys` is `u64::MAX` until ROADMAP §20.6
/// (F030) keeps a 32-bit device's buffer below 4 GiB.
pub fn alloc_from_buddy<A: Barriers>(
    buddy: &mut Buddy,
    spec: DmaAlloc,
    virt_of: impl Fn(u64) -> u64,
) -> Option<DmaBuffer> {
    let frames = buddy.alloc_constrained(spec.order()?, u64::MAX)?;
    let buf = DmaBuffer {
        virt: virt_of(frames.base()),
        len: spec.size,
        frames,
        coherent: spec.coherent,
    };
    buf.sync_for_device::<A>();
    Some(buf)
}

/// Takes the buffer by value and frees its block once.
pub fn free_to_buddy<A: Barriers>(buddy: &mut Buddy, buf: DmaBuffer) {
    buf.sync_for_cpu::<A>();
    buddy.free(buf.frames);
}

/// `size` rounded up to a whole number of `granule`-byte granules (a power
/// of two), the size of a non-coherent device's sub-page region (DESIGN
/// §4.7); `None` on overflow or a granule that is not a power of two.
pub const fn granule_round(size: usize, granule: usize) -> Option<usize> {
    if !granule.is_power_of_two() {
        return None;
    }
    match size.checked_add(granule - 1) {
        Some(v) => Some(v & !(granule - 1)),
        None => None,
    }
}

pub fn crosses_boundary(phys: u64, size: u64, boundary: u64) -> bool {
    pmm::crosses_boundary(phys, size, boundary)
}

/// Host-test trace of ring accesses and barriers, per thread: between
/// [`trace::start`] and [`trace::take`], the `dma_*` barriers here and the
/// virtqueue's ring accessors record each event, so a test can check where a
/// barrier falls between a store and a later load.
#[cfg(test)]
pub(crate) mod trace {
    use core::cell::RefCell;

    /// Events a [`Log`] keeps; a later one sets [`Log::overflow`].
    pub(crate) const CAP: usize = 256;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Ev {
        /// A ring load at this address.
        Load(usize),
        /// A ring store at this address.
        Store(usize),
        Wmb,
        Rmb,
        Mb,
        /// The port's `sync_for_device` over `(addr, len)`.
        Clean(usize, usize),
        /// The port's `sync_for_cpu` over `(addr, len)`.
        Inval(usize, usize),
        /// An ordered MMIO read at this address.
        MmioRead(usize),
        /// An ordered MMIO write at this address.
        MmioWrite(usize),
    }

    /// The events recorded since [`start`], oldest first.
    #[derive(Clone, Copy)]
    pub(crate) struct Log {
        evs: [Ev; CAP],
        n: usize,
        /// Set when an event did not fit.
        pub(crate) overflow: bool,
    }

    impl Log {
        const EMPTY: Log = Log {
            evs: [Ev::Mb; CAP],
            n: 0,
            overflow: false,
        };

        pub(crate) fn as_slice(&self) -> &[Ev] {
            &self.evs[..self.n]
        }
    }

    struct State {
        on: bool,
        log: Log,
    }

    std::thread_local! {
        static STATE: RefCell<State> = const {
            RefCell::new(State {
                on: false,
                log: Log::EMPTY,
            })
        };
    }

    /// Clear the log and record from now on.
    pub(crate) fn start() {
        STATE.with(|c| {
            let mut s = c.borrow_mut();
            s.on = true;
            s.log = Log::EMPTY;
        });
    }

    /// Record `e` if recording is on.
    pub(crate) fn record(e: Ev) {
        STATE.with(|c| {
            let mut s = c.borrow_mut();
            if !s.on {
                return;
            }
            let n = s.log.n;
            if n < CAP {
                s.log.evs[n] = e;
                s.log.n = n + 1;
            } else {
                s.log.overflow = true;
            }
        });
    }

    /// Stop recording and return what was recorded.
    pub(crate) fn take() -> Log {
        STATE.with(|c| {
            let mut s = c.borrow_mut();
            s.on = false;
            core::mem::replace(&mut s.log, Log::EMPTY)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pmm::MAX_ORDER;
    use crate::pmm::testing::Pool;

    type Stub = crate::arch::stub::Arch;

    #[test]
    fn identity_never_returns_a_high_va() {
        let pa = 0x1234_0000u64;
        let da = dma_to_device(pa);
        assert_eq!(da.0, pa);
        assert_eq!(dma_from_device(da), pa);
        assert_eq!(IdentityDma.dma_to_device(pa).0, pa);
        let mut p = Pool::new(16);
        let virt_of = |phys: u64| phys.wrapping_add(0xFFFF_8000_0000_0000);
        let buf =
            alloc_from_buddy::<Stub>(&mut p.buddy, DmaAlloc::new(4096, true), virt_of).unwrap();
        assert_eq!(buf.device().0, buf.phys());
        assert_ne!(buf.device().0, buf.virt());
        assert_eq!(buf.virt(), virt_of(buf.phys()));
        assert_eq!(buf.frame_count(), 1);
        free_to_buddy::<Stub>(&mut p.buddy, buf);
    }

    #[test]
    fn alloc_honors_align_and_dma32() {
        let mut p = Pool::new(256);
        let spec = DmaAlloc {
            size: 0x1800,
            align: 0x2000,
            boundary: DMA32_BOUNDARY,
            coherent: true,
        };
        let buf = alloc_from_buddy::<Stub>(&mut p.buddy, spec, |phys| phys + 0x1000).unwrap();
        assert_eq!(buf.phys() & 0x1FFF, 0);
        assert_eq!(buf.len(), 0x1800);
        assert!(!buf.is_empty());
        assert!(!crosses_boundary(buf.phys(), spec.size, DMA32_BOUNDARY));
        assert_eq!(buf.device().0, buf.phys());
        assert_eq!(buf.virt(), buf.phys() + 0x1000);
        assert_eq!(buf.as_ptr() as u64, buf.virt());
        assert_eq!(p.buddy.stats().free_frames, 256 - 2);
        buf.sync_for_device::<Stub>();
        buf.sync_for_cpu::<Stub>();
        free_to_buddy::<Stub>(&mut p.buddy, buf);
        assert_eq!(p.buddy.stats().free_frames, 256);
        assert!(DmaAlloc::dma32(64, true).boundary == DMA32_BOUNDARY);
        let _ = MAX_ORDER;
    }

    #[test]
    fn sg_builds_from_buffer_and_caps() {
        let mut p = Pool::new(16);
        let buf = alloc_from_buddy::<Stub>(&mut p.buddy, DmaAlloc::new(0x1000, true), |phys| phys)
            .unwrap();
        let sg = SgList::from_buffer(&buf).unwrap();
        assert_eq!(sg.n, 1);
        assert_eq!(sg.entries[0].addr, buf.device());
        assert_eq!(sg.entries[0].len, 0x1000);
        free_to_buddy::<Stub>(&mut p.buddy, buf);
        let mut s = SgList::new();
        let mut i = 0u32;
        while i < MAX_SG as u32 {
            s.push(DeviceAddr(i as u64 * 0x1000), 0x1000).unwrap();
            i += 1;
        }
        assert_eq!(s.push(DeviceAddr(1), 8), Err(DmaError::SgFull));
        assert_eq!(DmaError::Boundary.as_str(), "boundary");
    }

    #[test]
    fn dma_order_refuses_oversize_boundary() {
        let fits = DmaAlloc {
            size: 0x800,
            align: PAGE_SIZE,
            boundary: 0x800,
            coherent: true,
        };
        assert_eq!(fits.order(), Some(0));
        let over = DmaAlloc {
            size: 0x1000,
            ..fits
        };
        assert_eq!(over.order(), None);
        let odd = DmaAlloc {
            size: 0x800,
            align: PAGE_SIZE,
            boundary: 0x3000,
            coherent: true,
        };
        assert_eq!(odd.order(), None);
        assert_eq!(DmaAlloc::new(0x3000, true).order(), Some(2));
        assert_eq!(DmaAlloc::dma32(0x1000, true).order(), Some(0));
        assert_eq!(DmaAlloc::new(0, true).order(), None);
        let mut p = Pool::new(16);
        assert!(alloc_from_buddy::<Stub>(&mut p.buddy, over, |phys| phys).is_none());
        assert_eq!(p.buddy.stats().free_frames, 16);
    }

    #[test]
    fn four_gib_boundary_helper() {
        assert!(!crosses_boundary(0x1000, 0x1000, DMA32_BOUNDARY));
        assert!(crosses_boundary(
            DMA32_BOUNDARY - 0x800,
            0x1000,
            DMA32_BOUNDARY
        ));
        assert!(!crosses_boundary(0x1000, 0x1000, 0));
    }

    #[test]
    fn edu_addr_fits_harness_mask() {
        assert!(addr_fits_mask(0, EDU_DMA_MASK));
        assert!(addr_fits_mask(0x4000_0000, EDU_DMA_MASK));
        assert!(addr_fits_mask(EDU_DMA_MASK, EDU_DMA_MASK));
        assert!(!addr_fits_mask(EDU_DMA_MASK.wrapping_add(1), EDU_DMA_MASK));
        assert!(!addr_fits_mask(0x1_0000_0000, EDU_DMA_MASK));
    }

    #[test]
    fn coherent_buffer_syncs_with_barriers_only() {
        let mut p = Pool::new(16);
        let buf = alloc_from_buddy::<Stub>(&mut p.buddy, DmaAlloc::new(0x1000, true), |phys| phys)
            .unwrap();
        assert!(buf.coherent());
        trace::start();
        buf.sync_for_device::<Stub>();
        buf.sync_for_cpu::<Stub>();
        buf.sync_range_for_device::<Stub>(0x40, 0x40);
        let log = trace::take();
        assert_eq!(
            log.as_slice(),
            [trace::Ev::Wmb, trace::Ev::Rmb, trace::Ev::Wmb]
        );
        free_to_buddy::<Stub>(&mut p.buddy, buf);
    }

    #[test]
    fn non_coherent_buffer_runs_the_port_maintenance() {
        let mut p = Pool::new(16);
        // Host memory stands in for the block: `virt_of` maps it there.
        let mut mem = std::vec![0u8; 0x3000];
        let host = mem.as_mut_ptr() as u64;
        let spec = DmaAlloc::new(0x1800, false);
        let buf = alloc_from_buddy::<Stub>(&mut p.buddy, spec, |_| host).unwrap();
        assert!(!buf.coherent());
        trace::start();
        buf.sync_for_device::<Stub>();
        buf.sync_for_cpu::<Stub>();
        buf.sync_range_for_device::<Stub>(0x40, 0x40);
        buf.sync_range_for_cpu::<Stub>(0x80, 0x10);
        // Clipped to the two-page block.
        buf.sync_range_for_cpu::<Stub>(0x1F00, 0x1000);
        buf.sync_range_for_cpu::<Stub>(0x9000, 0x10);
        let log = trace::take();
        let h = host as usize;
        assert_eq!(
            log.as_slice(),
            [
                trace::Ev::Clean(h, 0x1800),
                trace::Ev::Inval(h, 0x1800),
                trace::Ev::Clean(h + 0x40, 0x40),
                trace::Ev::Inval(h + 0x80, 0x10),
                trace::Ev::Inval(h + 0x1F00, 0x100),
                trace::Ev::Inval(h + 0x2000, 0),
            ]
        );
        free_to_buddy::<Stub>(&mut p.buddy, buf);
        drop(mem);
    }

    #[test]
    fn granule_round_sizes_in_whole_granules() {
        assert_eq!(granule_round(0, 64), Some(0));
        assert_eq!(granule_round(1, 64), Some(64));
        assert_eq!(granule_round(64, 64), Some(64));
        assert_eq!(granule_round(8256, 2048), Some(10240));
        assert_eq!(granule_round(8256, 64), Some(8256));
        assert_eq!(granule_round(17, 1), Some(17));
        assert_eq!(granule_round(17, 48), None);
        assert_eq!(granule_round(17, 0), None);
        assert_eq!(granule_round(usize::MAX, 64), None);
    }
}
