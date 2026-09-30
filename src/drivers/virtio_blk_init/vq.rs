use super::*;

pub(super) const MAX_VQ: usize = 8;
pub(super) const MAX_QSIZE: usize = 64;
pub(super) const FREE: u8 = 0xFF;

pub(super) struct Vq {
    pub(super) vq: arch::current::SplitQueue,
    pub(super) qdma: DmaBuffer,
    pub(super) doorbell: u64,
    pub(super) inflight: [u8; MAX_QSIZE],
}

pub(super) fn clamp_qsize(hw: u16) -> u16 {
    let n = hw.min(MAX_QSIZE as u16);
    if n == 0 {
        return 0;
    }
    1u16 << (15u32 - n.leading_zeros())
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

pub(super) fn kick(doorbell: u64) {
    dma::dma_wmb::<Arch>();
    // SAFETY: invariant I234: `doorbell` is a queue's notify register inside
    // the notify capability's BAR, which `map_mmio` mapped uncached, checked
    // against the capability length by `vibeos::dev::virtio::notify_addr`;
    // established by `crate::dev::pci_init::map_mmio`.
    unsafe {
        core::ptr::write_volatile(doorbell as *mut u16, 0u16);
    }
}
