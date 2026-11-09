use super::*;

pub(super) const MAX_VQ: usize = 8;
pub(super) const MAX_QSIZE: usize = 64;
pub(super) const FREE: u8 = 0xFF;

pub(super) struct Vq {
    pub(super) vq: arch::current::SplitQueue,
    pub(super) qdma: DmaBuffer,
    pub(super) doorbell: u64,
    /// MMIO QueueNotify is 32-bit; PCI notify is 16-bit.
    pub(super) notify32: bool,
    pub(super) inflight: [u8; MAX_QSIZE],
}

pub(super) fn prefer_vq() -> usize {
    crate::arch::cpu_id_hint() as usize
}

pub(super) fn vq_has_room(v: &Vq, need: u16) -> bool {
    v.vq.num_free() >= need
}

pub(super) fn pick_vq(blk: &Blk, need: u16) -> Option<usize> {
    let nq = blk.nq as usize;
    if nq == 0 {
        return None;
    }
    let pref = prefer_vq() % nq;
    if let Some(v) = blk.vqs[pref].as_ref()
        && vq_has_room(v, need)
    {
        return Some(pref);
    }
    let mut i = 0usize;
    while i < nq {
        if let Some(v) = blk.vqs[i].as_ref()
            && vq_has_room(v, need)
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Notify queue `qi` through `virtio::notify`, an ordered `mmio_write`
/// after the descriptor and `avail.idx` stores (DESIGN §4.7).
pub(super) fn kick(doorbell: u64, qi: u16, notify32: bool) {
    // SAFETY: invariant I54: `doorbell` is a queue's notify register inside
    // a mapped virtio window (`map_mmio`); established by
    // `crate::dev::pci_init::map_mmio`. The store is that virtqueue's index
    // (virtio 1.2 sections 4.1.5.2, 4.2.2).
    unsafe { virtio::notify::<Arch>(doorbell, qi, notify32) }
}

/// How a device's DMA memory is laid out: from its registry entry's
/// coherence (DESIGN §4.7), which the `kernel_tests` command-line flag
/// `vibeos.virtio_blk_noncoherent` overrides to non-coherent, since QEMU's
/// `virt` marks every device coherent.
#[derive(Clone, Copy)]
pub(super) struct DmaPlan {
    pub(super) coherent: bool,
    /// What regions are aligned to and sized in: the port's
    /// `dma_granule` for a non-coherent device, 1 for a coherent one.
    pub(super) granule: usize,
    /// One request slot's stride (`virtio_blk::slot_stride`).
    pub(super) stride: usize,
}

impl DmaPlan {
    /// Out of line, so its log line's frame is not part of the probe's,
    /// which runs on the bootstrap stack (DESIGN §4.5).
    #[inline(never)]
    pub(super) fn for_device(dev: &Device) -> Self {
        let coherent = dev.dma_coherent && !force_non_coherent();
        let granule = if coherent {
            1
        } else {
            <Arch as vibeos::arch::Barriers>::dma_granule()
        };
        // The port's granule is a power of two no larger than a page
        // (`Barriers::dma_granule`); `SLOT_META + BOUNCE` rounds to at most
        // a page more.
        let stride = slot_stride(granule).unwrap_or(SLOT_META + BOUNCE);
        if !coherent {
            crate::klog!(
                vibeos::log::Level::Info,
                "vibeOS: virtio-blk: {}: non-coherent DMA, granule {}",
                dev.addr,
                granule
            );
        }
        Self {
            coherent,
            granule,
            stride,
        }
    }

    /// Set `vq` up for this plan, in place: non-coherent queues run their
    /// own cache maintenance (`SplitQueue::set_non_coherent`).
    pub(super) fn apply(self, vq: &mut arch::current::SplitQueue) -> Result<(), VirtioError> {
        if self.coherent {
            return Ok(());
        }
        vq.set_non_coherent()
    }
}

/// The `kernel_tests` knob: treat every virtio-blk device as non-coherent.
#[cfg(feature = "kernel_tests")]
fn force_non_coherent() -> bool {
    crate::boot::cmdline().flag("vibeos.virtio_blk_noncoherent")
}

#[cfg(not(feature = "kernel_tests"))]
fn force_non_coherent() -> bool {
    false
}
