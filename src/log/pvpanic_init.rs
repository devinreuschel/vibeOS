//! QEMU's `pvpanic` device, the kernel half (ROADMAP §10.7, §11.7;
//! INVARIANTS §2.5 steps 6 and 7). On x86_64 [`probe`] finds the ISA
//! device through fw_cfg's `etc/pvpanic-port`. On aarch64 [`probe_pci`]
//! finds `pvpanic-pci` by vendor and device id after the PCI scan.
//! `vibeos::log::pvpanic` chooses the event a panic step writes;
//! [`signal`] writes it.

#[cfg(target_arch = "x86_64")]
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering;
#[cfg(target_arch = "aarch64")]
use core::sync::atomic::{AtomicU8, AtomicUsize};

use vibeos::log::pvpanic;

#[cfg(target_arch = "x86_64")]
use crate::boot::fw_cfg_init;
#[cfg(target_arch = "x86_64")]
use crate::x86;

/// Red Hat / QEMU `pvpanic-pci` (QEMU `docs/specs/pvpanic.rst`).
#[cfg(target_arch = "aarch64")]
const PCI_VENDOR: u16 = 0x1B36;
#[cfg(target_arch = "aarch64")]
const PCI_DEVICE: u16 = 0x0011;

/// The fw_cfg file whose two little-endian bytes name the ISA port.
#[cfg(target_arch = "x86_64")]
const PORT_FILE: &str = "etc/pvpanic-port";

/// `pvpanic::pack` of the ISA port and the supported mask; 0 until
/// [`probe`] finds a device, and 0 for good when it finds none.
#[cfg(target_arch = "x86_64")]
static STATE: AtomicU32 = AtomicU32::new(0);

/// BAR0 VA of `pvpanic-pci`; 0 until [`probe_pci`] maps it.
#[cfg(target_arch = "aarch64")]
static PCI_VA: AtomicUsize = AtomicUsize::new(0);
/// Events the PCI device supports; published with [`PCI_VA`].
#[cfg(target_arch = "aarch64")]
static PCI_MASK: AtomicU8 = AtomicU8::new(0);

/// Find the ISA device and read its supported events, then print
/// `vibeOS: pvpanic: port 0x<port> events 0x<mask>` or
/// `vibeOS: pvpanic: absent`. Once, from `_start`, on the BSP before
/// anything else drives fw_cfg.
#[cfg(target_arch = "x86_64")]
pub fn probe() {
    let port = if fw_cfg_init::present() {
        fw_cfg_init::file(PORT_FILE).and_then(|f| {
            let mut raw = [0u8; 2];
            (fw_cfg_init::read(&f, &mut raw) == raw.len()).then(|| u16::from_le_bytes(raw))
        })
    } else {
        None
    };
    let Some(port) = port.filter(|&p| p != 0) else {
        crate::marker!("vibeOS: pvpanic: absent");
        return;
    };
    // SAFETY: `port` is the one QEMU's fw_cfg names in `etc/pvpanic-port`,
    // the ISA pvpanic device's byte register, where a read returns the
    // supported events (QEMU `docs/specs/pvpanic.rst`); fw_cfg answered only
    // after CPUID reported a hypervisor (invariant I57); established here.
    let mask = unsafe { x86::inb(port) };
    // Release: pairs with the Acquire loads in `signal` and `found`, so a
    // CPU that sees the state also sees the probe's port read as done.
    STATE.store(pvpanic::pack(port, mask), Ordering::Release);
    crate::marker!("vibeOS: pvpanic: port 0x{port:x} events 0x{mask:x}");
}

/// Find `pvpanic-pci` after the PCI scan, map BAR0, and read the events
/// it supports. A missing device is `vibeOS: pvpanic: absent`.
#[cfg(target_arch = "aarch64")]
pub fn probe_pci() {
    let Some(dev) = crate::dev_init::find_id(PCI_VENDOR, PCI_DEVICE) else {
        crate::marker!("vibeOS: pvpanic: absent");
        return;
    };
    let Ok(claim) = crate::dev_init::claim(&dev, 0) else {
        crate::marker!("vibeOS: pvpanic: absent");
        return;
    };
    let Some(va) = crate::pci_init::map_bar(&claim) else {
        crate::dev_init::release(claim);
        crate::marker!("vibeOS: pvpanic: absent");
        return;
    };
    if let Err(back) = crate::dev_init::hold(claim, Some(va)) {
        crate::dev_init::release(back);
        crate::marker!("vibeOS: pvpanic: absent");
        return;
    }
    // SAFETY: `va` is BAR0 of the `pvpanic-pci` this probe just mapped
    // uncached (`pci_init::map_bar`); a read returns the supported events
    // (QEMU `docs/specs/pvpanic.rst`); established here.
    let mask = unsafe { core::ptr::read_volatile(va as *const u8) };
    // Relaxed: the Release store of `PCI_VA` below publishes it; pairs with nothing.
    PCI_MASK.store(mask, Ordering::Relaxed);
    // Release: pairs with the Acquire load in `signal`, so a CPU that
    // sees the VA also sees the mask.
    PCI_VA.store(va as usize, Ordering::Release);
    crate::marker!("vibeOS: pvpanic: pci events 0x{mask:x}");
}

/// Write the event `step` chooses at the ISA port or the PCI BAR the
/// probe found, when the device supports it; nothing without a device.
/// The panic path's: IF=0, no lock, no allocation, no interrupt guard
/// (DESIGN §2.5 steps 6 and 7).
pub fn signal(step: pvpanic::Step) {
    #[cfg(target_arch = "x86_64")]
    {
        // Acquire: pairs with `probe`'s Release store.
        let state = STATE.load(Ordering::Acquire);
        if let Some((port, event)) = pvpanic::write_for(state, step) {
            // SAFETY: `port` is the pvpanic port fw_cfg named, which `probe`
            // stored only after reading the device's supported events there,
            // and `event` is one of those events (`pvpanic::write_for`), as
            // QEMU `docs/specs/pvpanic.rst` defines a write; established at
            // `log::pvpanic_init::probe`.
            unsafe { x86::outb(port, event) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        // Acquire: pairs with `probe_pci`'s Release store.
        let va = PCI_VA.load(Ordering::Acquire);
        if va != 0 {
            // Relaxed: the Acquire of `PCI_VA` above orders it; pairs with nothing.
            let mask = PCI_MASK.load(Ordering::Relaxed);
            if let Some(event) = pvpanic::event_for(step, mask) {
                // SAFETY: `va` is BAR0 of `pvpanic-pci`, mapped uncached by
                // `probe_pci` and never unmapped; `event` is one bit
                // `event_for` chose from the mask that probe read there
                // (QEMU `docs/specs/pvpanic.rst`); established at
                // `log::pvpanic_init::probe_pci`.
                unsafe { core::ptr::write_volatile(va as *mut u8, event) };
            }
        }
    }
}

/// The ISA probe's result: the port and the supported mask, or `None`
/// when no device was found.
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub fn found() -> Option<(u16, u8)> {
    // Acquire: pairs with `probe`'s Release store.
    let s = STATE.load(Ordering::Acquire);
    let port = (s & 0xFFFF) as u16;
    (port != 0).then_some((port, ((s >> 16) & 0xFF) as u8))
}

/// `pvpanic-pci` was found and its BAR mapped. `kernel_tests` only.
#[cfg(all(feature = "kernel_tests", target_arch = "aarch64"))]
pub fn pci_found() -> bool {
    // Acquire: pairs with `probe_pci`'s Release store.
    PCI_VA.load(Ordering::Acquire) != 0
}
