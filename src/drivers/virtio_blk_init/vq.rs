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

pub(super) fn kick(doorbell: u64, qi: u16, notify32: bool) {
    dma::dma_wmb::<Arch>();
    // SAFETY: invariant I54: `doorbell` is a queue's notify register inside
    // a mapped virtio window (`map_mmio`); established by
    // `crate::dev::pci_init::map_mmio`. The store is that virtqueue's index
    // (virtio 1.2 §§4.1.5.2, 4.2.2).
    unsafe {
        if notify32 {
            core::ptr::write_volatile(doorbell as *mut u32, u32::from(virtio::queue_notify(qi)));
        } else {
            core::ptr::write_volatile(doorbell as *mut u16, virtio::queue_notify(qi));
        }
    }
}
