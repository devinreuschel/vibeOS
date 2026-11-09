use super::*;
use crate::pci::{
    CFG_COMMAND, CFG_DEVICE, CFG_HEADER_TYPE, CFG_REVID_CLASS, CFG_VENDOR, HEADER_DEVICE, write16,
};
use std::vec;
use std::vec::Vec;

type Stub = crate::arch::stub::Arch;

#[test]
fn mmio_layout_and_ident() {
    assert_eq!(MMIO_MAGIC, u32::from_le_bytes(*b"virt"));
    assert_eq!(MMIO_OFF_QALIGN_LEGACY, 0x03c);
    assert_eq!(MMIO_OFF_QREADY, 0x044);
    assert_eq!(MMIO_OFF_QNOTIFY, 0x50);
    assert_eq!(MMIO_OFF_STATUS, 0x70);
    assert_eq!(MMIO_OFF_CONFIG, 0x100);
    assert!(mmio_ident_ok(MMIO_MAGIC, MMIO_VERSION));
    assert!(!mmio_ident_ok(0, MMIO_VERSION));
    assert!(!mmio_ident_ok(MMIO_MAGIC, 1));
    assert_eq!(modern_pci_id(ID_BLOCK), Some(DEV_BLK_MODERN));
    assert_eq!(modern_pci_id(ID_ENTROPY), Some(DEV_RNG_MODERN));
    assert_eq!(modern_pci_id(ID_INPUT), Some(DEV_INPUT_MODERN));
    assert_eq!(modern_pci_id(0), None);
    assert_eq!(queue_notify(3), 3);
}

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
    // SAFETY: invariant I53: every test passes a 16-byte-aligned `pool`
    // buffer of at least `layout.total` bytes, which it keeps alive
    // while it uses the queue; established here, by each caller.
    unsafe { SplitQueue::new(layout, base, event_idx) }.unwrap()
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
    assert!(!is_rng(VENDOR_ID, 0x1004));
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
    // A zero-length cap has no register, and the last two bytes are the
    // register: `delta == length - 1` runs past the cap.
    assert_eq!(notify_addr(0x1000, 0x200, 0, 0, 0), None);
    assert_eq!(notify_addr(0x1000, 0x200, 4, 1, 1), Some(0x1201));
    assert_eq!(notify_addr(0x1000, 0x200, 4, 3, 1), None);
    assert_eq!(notify_addr(0x1000, 0x200, 4, 0, 1), Some(0x1200));
    assert_eq!(notify_addr(0x1000, 0x200, 4, 2, 1), Some(0x1202));
    assert_eq!(notify_addr(0x1000, 0x200, 4, 1, 2), Some(0x1202));
    assert_eq!(notify_addr(0x1000, 0x200, 4, 2, 2), None);
}

#[test]
fn cap_must_fit_its_bar_and_hold_its_registers() {
    // Common config: 0x38 bytes at the end of a 0x1000-byte BAR fits.
    assert!(cap_fits(
        0x1000 - 0x38,
        0x38,
        min_cap_len(PCI_CAP_COMMON),
        0x1000
    ));
    assert!(!cap_fits(
        0x1000 - 0x37,
        0x38,
        min_cap_len(PCI_CAP_COMMON),
        0x1000
    ));
    assert!(!cap_fits(0, 0x37, min_cap_len(PCI_CAP_COMMON), 0x1000));
    assert!(!cap_fits(0, 0, min_cap_len(PCI_CAP_NOTIFY), 0x1000));
    assert!(cap_fits(0xFFE, 2, min_cap_len(PCI_CAP_NOTIFY), 0x1000));
    assert!(!cap_fits(0xFFF, 2, min_cap_len(PCI_CAP_NOTIFY), 0x1000));
    assert!(!cap_fits(0, 0, min_cap_len(PCI_CAP_ISR), 0x1000));
    assert!(cap_fits(u32::MAX, 0, 0, u64::MAX));
    assert!(!cap_fits(u32::MAX, u32::MAX, 0, u32::MAX as u64));
    assert!(!cap_fits(0, u32::MAX, 0, 0xFFFF_FFFE));
    assert_eq!(min_cap_len(PCI_CAP_DEVICE), 0);
}

#[test]
fn queue_notify_and_parse_multiplier() {
    assert_eq!(queue_notify(0), 0);
    assert_eq!(queue_notify(3), 3);
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
    assert!(SplitLayout::new(MAX_VIRTQ_SIZE).is_some());
    assert!(SplitLayout::new(MAX_VIRTQ_SIZE * 2).is_none());
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
        let old = q.last_avail();
        let new = q.publish();
        // The device asks for a kick at `old` (the first buffer since the
        // last kick) and not at `new` (already notified past it).
        q.wr16(layout.avail_event(), old);
        assert!(q.should_kick(old), "iteration {n}: avail_event = old");
        q.wr16(layout.avail_event(), new);
        assert!(!q.should_kick(old), "iteration {n}: avail_event = new");
        // SAFETY: descriptors address `base` from offset 0, the `pool`
        // buffer; established here.
        let done = unsafe { sim_complete(&mut q, (0xA0 + n) as u8, base, 0) };
        assert_eq!(done, 1);
        let u = q.get_used().unwrap().unwrap();
        assert_eq!(u.len, 8);
        assert_eq!(q.get_used(), Ok(None));
        n += 1;
    }
    assert_eq!(q.num_free(), 4);
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
        let old = q.last_avail();
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
            assert!(q.get_used().unwrap().is_some());
            assert_eq!(q.get_used(), Ok(None));
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
    let u = q.get_used().unwrap().unwrap();
    assert_eq!(u.len, 4);
    // SAFETY: `data_off` lies in the `pool` buffer; established here.
    unsafe {
        assert_eq!(base.add(data_off as usize).read_volatile(), 0x5A);
    }
    q.wr16(q.layout.used_flags(), USED_F_NO_NOTIFY);
    q.add(data_off, 4, DESC_F_WRITE).unwrap();
    let old = q.last_avail();
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
    assert_eq!(q.num_free(), 5);
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
    let u = q.get_used().unwrap().unwrap();
    assert_eq!(u.id, head);
    assert_eq!(u.len, 513);
    assert_eq!(q.num_free(), 8);
    assert_eq!(q.get_used(), Ok(None));
}

/// A used-ring entry for `id`, published as the device would.
fn forge_used(q: &mut SplitQueue<Stub>, id: u32) {
    let used = q.used_idx();
    let off = q.layout.used_elem(used);
    q.wr32(off, id);
    q.wr32(off + 4, 0);
    dma::dma_wmb::<Stub>();
    q.wr16(q.layout.used_idx(), used.wrapping_add(1));
}

/// Every free descriptor once: no double free left `free_head` on a
/// listed descriptor or `num_free` past the size.
fn assert_free_list_sane(q: &SplitQueue<Stub>, want_free: u16) {
    assert_eq!(q.num_free(), want_free);
    let mut seen = [false; 8];
    let mut i = q.free_head;
    for _ in 0..want_free {
        assert!(!seen[i as usize], "descriptor {i} listed twice");
        seen[i as usize] = true;
        i = q.link_of(i);
    }
}

/// `a` is completed with its descriptor rewritten to chain to `forged`.
fn complete_forged(forged_is_live: bool) {
    let layout = SplitLayout::new(8).unwrap();
    let (_keep, base) = pool(layout.total);
    let mut q = queue(layout, base, false);
    q.init();
    let a = q.add(0x1000, 4, DESC_F_WRITE).unwrap();
    let b = q.add(0x2000, 4, DESC_F_WRITE).unwrap();
    q.publish();
    let forged = if forged_is_live { b } else { a };
    let off = q.layout.desc(a);
    q.wr16(off + 12, DESC_F_NEXT | DESC_F_WRITE);
    q.wr16(off + 14, forged);
    forge_used(&mut q, a as u32);
    assert_eq!(q.get_used().unwrap().unwrap().id, a);
    assert_free_list_sane(&q, 7);
    // `b` is still its own: the next chain does not reuse it.
    let c = q.add(0x3000, 4, DESC_F_WRITE).unwrap();
    assert_ne!(c, b);
    assert_eq!(q.desc_addr(b), 0x2000);
    forge_used(&mut q, b as u32);
    assert_eq!(q.get_used().unwrap().unwrap().id, b);
    assert_free_list_sane(&q, 7);
}

#[test]
fn forged_next_to_a_live_head_does_not_free_it() {
    complete_forged(true);
}

#[test]
fn forged_next_to_itself_frees_it_once() {
    complete_forged(false);
}

#[test]
fn used_id_out_of_range_breaks_the_queue() {
    let layout = SplitLayout::new(8).unwrap();
    // 0x1_0000 truncates to the live head 0 as a u16.
    for bad in [8u32, 9, 0xFFFF, 0x1_0000, u32::MAX] {
        let (_keep, base) = pool(layout.total);
        let mut q = queue(layout, base, false);
        q.init();
        let head = q.add(0x1000, 4, DESC_F_WRITE).unwrap();
        assert_eq!(head, 0);
        q.publish();
        forge_used(&mut q, bad);
        assert_eq!(q.get_used(), Err(VirtioError::BadUsed), "id {bad:#x}");
        assert_free_list_sane(&q, 7);
        // Sticky, even for an honest completion after it.
        forge_used(&mut q, 0);
        assert_eq!(q.get_used(), Err(VirtioError::BadUsed));
        assert_free_list_sane(&q, 7);
        q.init();
        assert_eq!(q.get_used(), Ok(None));
    }
}

#[test]
fn duplicate_used_id_is_not_freed_twice() {
    let layout = SplitLayout::new(8).unwrap();
    let (_keep, base) = pool(layout.total);
    let mut q = queue(layout, base, false);
    q.init();
    let head = q.add(0x1000, 4, DESC_F_WRITE).unwrap();
    q.publish();
    forge_used(&mut q, head as u32);
    forge_used(&mut q, head as u32);
    assert_eq!(q.get_used().unwrap().unwrap().id, head);
    assert_free_list_sane(&q, 8);
    assert_eq!(q.get_used(), Err(VirtioError::BadUsed));
    assert_free_list_sane(&q, 8);
}

#[test]
fn stale_used_id_is_not_freed() {
    let layout = SplitLayout::new(8).unwrap();
    let (_keep, base) = pool(layout.total);
    let mut q = queue(layout, base, false);
    q.init();
    // Never added.
    forge_used(&mut q, 3);
    assert_eq!(q.get_used(), Err(VirtioError::BadUsed));
    assert_free_list_sane(&q, 8);

    q.init();
    let chain = [DescBuf {
        addr: 0x1000,
        len: 4,
        flags: 0,
    }; 2];
    let h = q.add_chain(&chain).unwrap();
    let tail = q.desc_next(h);
    q.publish();
    // The chain's second descriptor is not a head.
    forge_used(&mut q, tail as u32);
    assert_eq!(q.get_used(), Err(VirtioError::BadUsed));
    assert_free_list_sane(&q, 6);
}

#[test]
fn used_idx_ahead_of_the_queue_breaks_it() {
    let layout = SplitLayout::new(8).unwrap();
    let (_keep, base) = pool(layout.total);
    let mut q = queue(layout, base, false);
    q.init();
    let head = q.add(0x1000, 4, DESC_F_WRITE).unwrap();
    q.publish();
    forge_used(&mut q, head as u32);
    // used.idx claims 9 completions of an 8-entry ring.
    q.wr16(layout.used_idx(), 9);
    assert_eq!(q.pending(), 9);
    assert_eq!(q.get_used(), Err(VirtioError::BadUsed));
    assert_free_list_sane(&q, 7);
    q.init();
    q.wr16(layout.used_idx(), 0xFFFF);
    assert_eq!(q.get_used(), Err(VirtioError::BadUsed));
    assert_eq!(q.num_free(), 8);
}

/// DESIGN §4.7, for a non-coherent device, over one virtio-blk request:
/// every ring store and the request's slot are cleaned before the doorbell,
/// which is an ordered `mmio_write` after the `avail.idx` store; the
/// interrupt-status read is an ordered `mmio_read` before the used-ring
/// reads; every used-ring load is invalidated first, and the slot is
/// invalidated before the CPU reads it. The per-line expansion of each
/// clean and invalidate is `arch::aarch64::tlb`'s `dma_clean` and
/// `dma_inval`, which its own tests check line by line.
#[test]
fn non_coherent_request_path_cleans_and_invalidates() {
    use crate::dma::trace::Ev;
    use crate::dma::{DmaAlloc, alloc_from_buddy, free_to_buddy};
    use crate::virtio_blk::{
        SLOT_META, SLOT_STATUS, hand_slot, slot_region, slot_stride, take_slot,
    };

    let g = <Stub as Barriers>::dma_granule();
    let layout = SplitLayout::with_granule(8, g).unwrap();
    assert_eq!(layout.used_off % g, 0);
    assert!(layout.used_event() + 2 <= layout.used_off);
    assert_eq!(layout.total % g, 0);
    // Ring memory at a page boundary, so granule offsets are addresses.
    let mut ring_mem = vec![0u8; layout.total + 4096];
    let ring = {
        let p = ring_mem.as_mut_ptr();
        // SAFETY: the adjustment is under 4096, inside the spare page;
        // established here.
        unsafe { p.add((4096 - p as usize % 4096) % 4096) }
    };
    let mut q = queue(layout, ring, true);
    q.set_non_coherent().unwrap();
    q.init();
    let in_ring = |a: usize| a >= ring as usize && a < ring as usize + layout.total;
    let used_lo = ring as usize + layout.used_off;

    let mut pool = crate::pmm::testing::Pool::new(64);
    let hhdm = pool.hhdm();
    let stride = slot_stride(g).unwrap();
    let spec = DmaAlloc::new(4 * stride as u64).coherent(false);
    let slots = alloc_from_buddy::<Stub>(&mut pool.buddy, spec, |p| p.wrapping_add(hhdm)).unwrap();
    let si = 1usize;
    let (soff, slen) = slot_region(si, stride);
    let slot_va = slots.virt() as usize + soff as usize;
    assert_eq!(soff % g as u64, 0);
    assert_eq!(slen % g as u64, 0);

    let doorbell = 0u16;
    let isr = 1u8;
    // The device reaches the slot buffer at device offsets from its base.
    let chain = [
        DescBuf {
            addr: soff,
            len: 16,
            flags: 0,
        },
        DescBuf {
            addr: soff + SLOT_META as u64,
            len: 512,
            flags: DESC_F_WRITE,
        },
        DescBuf {
            addr: soff + SLOT_STATUS as u64,
            len: 1,
            flags: DESC_F_WRITE,
        },
    ];

    dma::trace::start();
    hand_slot::<Stub>(&slots, si, stride);
    q.add_chain(&chain).unwrap();
    let old = q.last_avail();
    q.publish();
    let _ = q.should_kick(old);
    // SAFETY: `doorbell` is a live, aligned u16 standing in for the notify
    // register; established here.
    unsafe { notify::<Stub>(&doorbell as *const u16 as u64, 0, false) };
    let sub = dma::trace::take();
    assert!(!sub.overflow);
    let sub = sub.as_slice();
    let bell = sub
        .iter()
        .position(|e| *e == Ev::MmioWrite(&doorbell as *const u16 as usize))
        .expect("doorbell written");
    assert!(
        sub[..bell].contains(&Ev::Clean(slot_va, slen as usize)),
        "slot not cleaned before the doorbell"
    );
    let idx = sub
        .iter()
        .rposition(|e| *e == Ev::Store(ring as usize + layout.avail_idx()))
        .expect("avail.idx stored");
    assert!(idx < bell, "the doorbell is not after the avail.idx store");
    let mut stores = 0;
    for (i, e) in sub[..bell].iter().enumerate() {
        if let Ev::Store(a) = *e {
            assert!(in_ring(a));
            stores += 1;
            // The next maintenance after the store cleans its bytes.
            let clean = sub[i + 1..bell]
                .iter()
                .find(|e| matches!(e, Ev::Clean(..) | Ev::Inval(..)));
            assert!(
                matches!(clean, Some(Ev::Clean(s, n)) if *s <= a && a < s + n),
                "ring store at {a:#x} not cleaned before the doorbell"
            );
        }
    }
    assert!(stores >= 4, "descriptors, avail entry, avail.idx");

    // SAFETY: the chain's buffers are device offsets into the slot buffer
    // at `slots.virt()`, which holds them; established here.
    let done = unsafe { sim_complete(&mut q, 0, slots.virt() as *mut u8, 0) };
    assert_eq!(done, 1);

    dma::trace::start();
    // SAFETY: `isr` is a live byte standing in for the ISR register;
    // established here.
    let _ = unsafe { read_isr::<Stub, u8>(&isr) };
    assert!(q.get_used().unwrap().is_some());
    take_slot::<Stub>(&slots, si, stride);
    let comp = dma::trace::take();
    assert!(!comp.overflow);
    let comp = comp.as_slice();
    assert_eq!(comp[0], Ev::MmioRead(&isr as *const u8 as usize));
    let mut loads = 0;
    for (i, e) in comp.iter().enumerate() {
        if let Ev::Load(a) = *e
            && a >= used_lo
            && in_ring(a)
        {
            loads += 1;
            assert!(i > 0, "a used-ring load before the ISR read");
            assert!(
                matches!(comp[i - 1], Ev::Inval(s, n) if s <= a && a < s + n),
                "used-ring load at {a:#x} not invalidated first"
            );
        }
    }
    assert!(loads >= 3, "used.idx, the element's id and len");
    let last_load = comp.iter().rposition(|e| matches!(e, Ev::Load(_))).unwrap();
    assert!(
        comp[last_load..].contains(&Ev::Inval(slot_va, slen as usize)),
        "slot not invalidated before the CPU reads it"
    );
    free_to_buddy::<Stub>(&mut pool.buddy, slots);
    drop(ring_mem);
}

/// A non-coherent queue needs its used ring on a granule boundary.
#[test]
fn non_coherent_queue_refuses_a_packed_layout() {
    let g = <Stub as Barriers>::dma_granule();
    let packed = SplitLayout::new(8).unwrap();
    assert_ne!(packed.used_off % g, 0);
    let (_keep, base) = pool(packed.total + 4096);
    let mut q = queue(packed, base, false);
    assert_eq!(q.set_non_coherent(), Err(VirtioError::BadQueue));
    assert!(q.coherent());
    assert_eq!(SplitLayout::with_granule(8, 4), SplitLayout::new(8));
    assert_eq!(SplitLayout::with_granule(8, 48), None);
}
