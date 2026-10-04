//! virtio-mmio transport: DT `reg` claimed as a BAR (DESIGN §12.3).
//!
//! Each `MachineDesc` virtio-mmio window is published as a device whose
//! BAR 0 is that `reg` range. Probe reads Magic/Version/DeviceID after
//! `claim_mem_bars`, the same path a PCI BAR takes. Only Version=2
//! (virtio 1.2 §4.2) is a modern transport; QEMU's `virt` builds the
//! 32 windows with `force-legacy=on`, so the harness sets
//! `-global virtio-mmio.force-legacy=off`.

use vibeos::dev::{Device, Resource, ResourceKind};
use vibeos::irq::IrqSpecifier;
use vibeos::pci::Bdf;
use vibeos::virtio::{
    self, CLASS_MMIO, ID_BLOCK, MMIO_OFF_CONFIG, MMIO_OFF_DEV_FEATURES, MMIO_OFF_DEV_FEATURES_SEL,
    MMIO_OFF_DEVICE_ID, MMIO_OFF_DRV_FEATURES, MMIO_OFF_DRV_FEATURES_SEL, MMIO_OFF_ISR,
    MMIO_OFF_ISR_ACK, MMIO_OFF_MAGIC, MMIO_OFF_QDESC, MMIO_OFF_QDEVICE, MMIO_OFF_QDRIVER,
    MMIO_OFF_QNOTIFY, MMIO_OFF_QNUM, MMIO_OFF_QNUM_MAX, MMIO_OFF_QREADY, MMIO_OFF_QSEL,
    MMIO_OFF_STATUS, MMIO_OFF_VERSION, VENDOR_ID, modern_pci_id,
};

use crate::dev_init;
use crate::machine_init;
use crate::pci_init;
use crate::virtio_init;

/// Platform bus used for published virtio-mmio transports (not ECAM).
const MMIO_BUS: u8 = 0xFE;

pub(crate) fn is_mmio(dev: &Device) -> bool {
    dev.class == CLASS_MMIO
}

fn r32(va: u64, off: u16) -> u32 {
    // SAFETY: invariant I54: `va` is a virtio-mmio window `map_mmio`
    // mapped uncached, and `off` a register in that window; established
    // by `pci_init::map_mmio`.
    unsafe {
        u32::from_le(core::ptr::read_volatile(
            (va.wrapping_add(off as u64)) as *const u32,
        ))
    }
}

fn w32(va: u64, off: u16, v: u32) {
    // SAFETY: invariant I54: as `r32`; established by `pci_init::map_mmio`.
    unsafe {
        core::ptr::write_volatile((va.wrapping_add(off as u64)) as *mut u32, v.to_le());
    }
}

fn w64(va: u64, off: u16, v: u64) {
    w32(va, off, v as u32);
    w32(va, off + 4, (v >> 32) as u32);
}

pub(crate) fn reset(base: u64) -> bool {
    w32(base, MMIO_OFF_STATUS, 0);
    let mut n = 0u32;
    while n < virtio_init::RESET_POLLS {
        if r32(base, MMIO_OFF_STATUS) == 0 {
            return true;
        }
        n = n.saturating_add(1);
        core::hint::spin_loop();
    }
    false
}

pub(crate) fn read_features(base: u64) -> u64 {
    w32(base, MMIO_OFF_DEV_FEATURES_SEL, 0);
    let lo = r32(base, MMIO_OFF_DEV_FEATURES) as u64;
    w32(base, MMIO_OFF_DEV_FEATURES_SEL, 1);
    let hi = r32(base, MMIO_OFF_DEV_FEATURES) as u64;
    lo | (hi << 32)
}

pub(crate) fn write_features(base: u64, feat: u64) {
    w32(base, MMIO_OFF_DRV_FEATURES_SEL, 0);
    w32(base, MMIO_OFF_DRV_FEATURES, feat as u32);
    w32(base, MMIO_OFF_DRV_FEATURES_SEL, 1);
    w32(base, MMIO_OFF_DRV_FEATURES, (feat >> 32) as u32);
}

pub(crate) fn status(base: u64) -> u8 {
    r32(base, MMIO_OFF_STATUS) as u8
}

pub(crate) fn set_status(base: u64, v: u8) {
    w32(base, MMIO_OFF_STATUS, u32::from(v));
}

pub(crate) fn select_queue(base: u64, qi: u16) {
    w32(base, MMIO_OFF_QSEL, u32::from(qi));
}

pub(crate) fn queue_max(base: u64) -> u16 {
    r32(base, MMIO_OFF_QNUM_MAX) as u16
}

pub(crate) fn set_queue_num(base: u64, n: u16) {
    w32(base, MMIO_OFF_QNUM, u32::from(n));
}

pub(crate) fn set_queue_ready(base: u64, on: bool) {
    w32(base, MMIO_OFF_QREADY, u32::from(on));
}

pub(crate) fn set_queue_addrs(base: u64, desc: u64, driver: u64, device: u64) {
    w64(base, MMIO_OFF_QDESC, desc);
    w64(base, MMIO_OFF_QDRIVER, driver);
    w64(base, MMIO_OFF_QDEVICE, device);
}

pub(crate) fn notify_va(base: u64) -> u64 {
    base.wrapping_add(u64::from(MMIO_OFF_QNOTIFY))
}

pub(crate) fn isr_va(base: u64) -> u64 {
    base.wrapping_add(u64::from(MMIO_OFF_ISR))
}

pub(crate) fn config_va(base: u64) -> u64 {
    base.wrapping_add(u64::from(MMIO_OFF_CONFIG))
}

pub(crate) fn isr_bits(base: u64) -> u32 {
    r32(base, MMIO_OFF_ISR)
}

pub(crate) fn ack_isr(base: u64) {
    let v = isr_bits(base);
    w32(base, MMIO_OFF_ISR_ACK, v);
}

pub(crate) fn ident_ok(base: u64) -> bool {
    virtio::mmio_ident_ok(r32(base, MMIO_OFF_MAGIC), r32(base, MMIO_OFF_VERSION))
}

pub(crate) fn device_id(base: u64) -> u32 {
    r32(base, MMIO_OFF_DEVICE_ID)
}

pub(crate) fn irq_spec(dev: &Device) -> Option<IrqSpecifier> {
    (dev.intid != 0).then_some(IrqSpecifier::Gic { intid: dev.intid })
}

/// Peek Magic/Version/DeviceID through a temporary ioremap, then unmap.
fn peek(phys: u64, size: u64) -> Option<u32> {
    let va = pci_init::map_mmio(phys, size)?;
    let magic = r32(va, MMIO_OFF_MAGIC);
    let ver = r32(va, MMIO_OFF_VERSION);
    let id = r32(va, MMIO_OFF_DEVICE_ID);
    // SAFETY: `va` is the temporary ioremap this function created and
    // nothing else holds it; established by here.
    unsafe { pci_init::unmap_mmio(va, size) };
    if !virtio::mmio_ident_ok(magic, ver) {
        return None;
    }
    Some(id)
}

/// Publish each occupied virtio-mmio window as a registry device whose
/// BAR 0 is the DT `reg` range.
pub fn publish() {
    let Some(desc) = machine_init::info() else {
        return;
    };
    let n = desc.virtio_mmio_count.min(desc.virtio_mmio.len());
    let mut i = 0usize;
    let mut published = 0u32;
    while i < n {
        let m = desc.virtio_mmio[i];
        i = i.saturating_add(1);
        if m.size == 0 {
            continue;
        }
        let Some(vid) = peek(m.base, m.size) else {
            continue;
        };
        let Some(did) = modern_pci_id(vid) else {
            continue;
        };
        if vid != ID_BLOCK {
            continue;
        }
        let mut d = Device::empty();
        d.addr = Bdf::new(MMIO_BUS, (published as u8) & 0x1F, 0);
        d.vendor = VENDOR_ID;
        d.device_id = did;
        d.class = CLASS_MMIO;
        d.intid = m.irq;
        d.resources[0] = Resource {
            kind: ResourceKind::Memory,
            bar: 0,
            addr: m.base,
            size: m.size,
            prefetchable: false,
        };
        match dev_init::push(d, None) {
            Ok(_) => {
                published = published.saturating_add(1);
            }
            Err(_) => {
                crate::klog!(
                    vibeos::log::Level::Warn,
                    "vibeOS: virtio-mmio: registry full at {}",
                    i
                );
                break;
            }
        }
    }
    if published != 0 {
        crate::klog!(
            vibeos::log::Level::Info,
            "vibeOS: virtio-mmio: {published} devices"
        );
    }
}
