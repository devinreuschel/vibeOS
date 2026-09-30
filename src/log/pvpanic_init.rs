//! QEMU's ISA `pvpanic` device, the kernel half (ROADMAP §10.7;
//! INVARIANTS §2.5 steps 6 and 7). [`probe`] runs once at boot, on the BSP
//! before any other CPU: it looks for fw_cfg's `etc/pvpanic-port`, which
//! QEMU lists only when the device is there, and reads the port that file
//! names once for the events the device supports. `vibeos::log::pvpanic`
//! chooses the event a panic step writes; [`signal`] writes it.
//!
//! fw_cfg is probed only when CPUID reports a hypervisor (invariant I244),
//! so bare metal sees no port access here, and the port written is only
//! the one fw_cfg named, never a guessed one.

use core::sync::atomic::{AtomicU32, Ordering};

use vibeos::log::pvpanic;

use crate::boot::fw_cfg_init;
use crate::x86;

/// The fw_cfg file whose two little-endian bytes name the device's port.
const PORT_FILE: &str = "etc/pvpanic-port";

/// `pvpanic::pack` of the port and the supported mask; 0 until [`probe`]
/// finds a device, and 0 for good when it finds none.
static STATE: AtomicU32 = AtomicU32::new(0);

/// Find the device and read its supported events, then print
/// `vibeOS: pvpanic: port 0x<port> events 0x<mask>` or
/// `vibeOS: pvpanic: absent`. Once, from `_start`, on the BSP before
/// anything else drives fw_cfg.
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
    // after CPUID reported a hypervisor (invariant I244); established here.
    let mask = unsafe { x86::inb(port) };
    // Release: pairs with the Acquire loads in `signal` and `found`, so a
    // CPU that sees the state also sees the probe's port read as done.
    STATE.store(pvpanic::pack(port, mask), Ordering::Release);
    crate::marker!("vibeOS: pvpanic: port 0x{port:x} events 0x{mask:x}");
}

/// Write the event `step` chooses (`pvpanic::write_for`) at the port the
/// probe found, when the device supports it; nothing without a device.
/// The panic path's: IF=0, no lock, no allocation, no interrupt guard, one
/// atomic load and at most one port write (DESIGN §2.5 steps 6 and 7).
pub fn signal(step: pvpanic::Step) {
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

/// The probe's result: the port and the supported mask, or `None` when no
/// device was found.
#[cfg(feature = "kernel_tests")]
pub fn found() -> Option<(u16, u8)> {
    // Acquire: pairs with `probe`'s Release store.
    let s = STATE.load(Ordering::Acquire);
    let port = (s & 0xFFFF) as u16;
    (port != 0).then_some((port, ((s >> 16) & 0xFF) as u8))
}
