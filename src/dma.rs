//! DMA buffers, identity map, SG lists, publish fences. ROADMAP §6.4.
//!
//! IOMMU later: keep every device-visible address behind [`DmaTranslate`].
//! x86 is coherent; [`sync_for_device`] / [`sync_for_cpu`] still run so
//! aarch64 can fill them in. Descriptor publish uses real fences, not
//! `compiler_fence`.

use core::sync::atomic::{Ordering, compiler_fence, fence};

use crate::pmm::{self, Buddy, PAGE_SIZE};

/// Address a device programs into a descriptor. Not a CPU VA.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct DeviceAddr(pub u64);

impl DeviceAddr {
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaError {
    Size,
    Align,
    Boundary,
    Exhausted,
    SgFull,
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
}

impl DmaAlloc {
    pub const fn new(size: u64) -> Self {
        Self {
            size,
            align: PAGE_SIZE,
            boundary: 0,
        }
    }

    pub const fn dma32(size: u64) -> Self {
        Self {
            size,
            align: PAGE_SIZE,
            boundary: DMA32_BOUNDARY,
        }
    }
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
    phys: u64,
    len: u64,
    order: u8,
}

impl DmaBuffer {
    /// Physical base of the block.
    pub fn phys(&self) -> u64 {
        self.phys
    }

    /// Kernel VA of the block.
    pub fn virt(&self) -> u64 {
        self.virt
    }

    /// Bytes requested; the block may be larger.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Never true: a buffer covers at least the bytes asked for.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Address a device programs into a descriptor.
    pub fn device(&self) -> DeviceAddr {
        dma_to_device(self.phys)
    }

    pub fn sync_for_device(&self) {
        dma_wmb();
    }

    pub fn sync_for_cpu(&self) {
        dma_rmb();
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

/// Store-side barrier before a device may observe a published index.
#[inline]
pub fn dma_wmb() {
    fence(Ordering::Release);
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("sfence", options(nostack, preserves_flags));
    }
    // Keep a compiler fence so a future port cannot "optimize" this
    // into a comment. The atomic fence above is the contract.
    compiler_fence(Ordering::Release);
}

/// Load-side barrier after the device writes a completion.
#[inline]
pub fn dma_rmb() {
    fence(Ordering::Acquire);
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("lfence", options(nostack, preserves_flags));
    }
    compiler_fence(Ordering::Acquire);
}

/// Publish `idx` after descriptor stores. Release + `sfence`.
#[inline]
pub fn publish_index(slot: &core::sync::atomic::AtomicU16, idx: u16) {
    dma_wmb();
    slot.store(idx, Ordering::Release);
}

/// The one constructor of [`DmaBuffer`]. `virt_of` maps the block's
/// physical base to the kernel VA the CPU uses.
pub fn alloc_from_buddy(
    buddy: &mut Buddy,
    spec: DmaAlloc,
    virt_of: impl Fn(u64) -> u64,
) -> Option<DmaBuffer> {
    let (phys, order) = buddy.allocate_constrained(spec.size, spec.align, spec.boundary)?;
    let buf = DmaBuffer {
        virt: virt_of(phys),
        phys,
        len: spec.size,
        order,
    };
    buf.sync_for_device();
    Some(buf)
}

pub fn free_to_buddy(buddy: &mut Buddy, buf: DmaBuffer) {
    buf.sync_for_cpu();
    unsafe { buddy.deallocate(buf.phys, buf.order) };
}

pub fn crosses_boundary(phys: u64, size: u64, boundary: u64) -> bool {
    pmm::crosses_boundary(phys, size, boundary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pmm::MAX_ORDER;
    use crate::pmm::testing::Pool;
    use core::sync::atomic::AtomicU16;

    #[test]
    fn identity_never_returns_a_high_va() {
        let pa = 0x1234_0000u64;
        let da = dma_to_device(pa);
        assert_eq!(da.0, pa);
        assert_eq!(dma_from_device(da), pa);
        assert_eq!(IdentityDma.dma_to_device(pa).0, pa);
        let mut p = Pool::new(16);
        let virt_of = |phys: u64| phys.wrapping_add(0xFFFF_8000_0000_0000);
        let buf = alloc_from_buddy(&mut p.buddy, DmaAlloc::new(4096), virt_of).unwrap();
        assert_eq!(buf.device().0, buf.phys());
        assert_ne!(buf.device().0, buf.virt());
        assert_eq!(buf.virt(), virt_of(buf.phys()));
        free_to_buddy(&mut p.buddy, buf);
    }

    #[test]
    fn alloc_honors_align_and_dma32() {
        let mut p = Pool::new(256);
        let spec = DmaAlloc {
            size: 0x1800,
            align: 0x2000,
            boundary: DMA32_BOUNDARY,
        };
        let buf = alloc_from_buddy(&mut p.buddy, spec, |phys| phys + 0x1000).unwrap();
        assert_eq!(buf.phys() & 0x1FFF, 0);
        assert_eq!(buf.len(), 0x1800);
        assert!(!buf.is_empty());
        assert!(!crosses_boundary(buf.phys(), spec.size, DMA32_BOUNDARY));
        assert_eq!(buf.device().0, buf.phys());
        assert_eq!(buf.virt(), buf.phys() + 0x1000);
        assert_eq!(buf.as_ptr() as u64, buf.virt());
        assert_eq!(p.buddy.stats().free_frames, 256 - 2);
        buf.sync_for_device();
        buf.sync_for_cpu();
        free_to_buddy(&mut p.buddy, buf);
        assert_eq!(p.buddy.stats().free_frames, 256);
        assert!(DmaAlloc::dma32(64).boundary == DMA32_BOUNDARY);
        let _ = MAX_ORDER;
    }

    #[test]
    fn sg_builds_from_buffer_and_caps() {
        let mut p = Pool::new(16);
        let buf = alloc_from_buddy(&mut p.buddy, DmaAlloc::new(0x1000), |phys| phys).unwrap();
        let sg = SgList::from_buffer(&buf).unwrap();
        assert_eq!(sg.n, 1);
        assert_eq!(sg.entries[0].addr, buf.device());
        assert_eq!(sg.entries[0].len, 0x1000);
        free_to_buddy(&mut p.buddy, buf);
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
    fn publish_uses_release_not_only_compiler_fence() {
        let idx = AtomicU16::new(0);
        // Descriptor payload would be stored first; then publish.
        publish_index(&idx, 3);
        assert_eq!(idx.load(Ordering::Acquire), 3);
        dma_wmb();
        dma_rmb();
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
}
