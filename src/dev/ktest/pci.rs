//! PCI in-guest tests.

use core::sync::atomic::Ordering;

use vibeos::dev::{ClaimError, DevRef, DevState, Driver, IdMatch, Instance, ProbeError};
use vibeos::paging::{PageFlags, VirtAddr};
use vibeos::pci::{self, Bdf, CFG_COMMAND, CFG_VENDOR, CMD_MASTER, CMD_MEM};

use crate::dev_init;
use crate::ktest::Outcome;
use crate::paging_init;
use crate::pci_init;

use super::{cfg_read32, cfg_write32, find_id, len, pci_live};

const PCI_QEMU_IDS: &[(u16, u16)] = &[
    (0x8086, 0x1237), // 440FX
    (0x8086, 0x7000), // PIIX3 ISA
    (0x8086, 0x7010), // PIIX3 IDE
    (0x8086, 0x7113), // PIIX4 ACPI
    (0x1234, 0x1111), // Bochs VGA
    (0x8086, 0x100e), // e1000
];

pub(crate) fn test_pci_qemu_set() -> Outcome {
    if !pci_live() {
        return Outcome::Fail("pci not live");
    }
    if len() < PCI_QEMU_IDS.len() {
        return Outcome::Fail("device count");
    }
    let mut i = 0usize;
    while i < PCI_QEMU_IDS.len() {
        let (v, d) = PCI_QEMU_IDS[i];
        if find_id(v, d).is_none() {
            return Outcome::Fail("missing qemu id");
        }
        i += 1;
    }
    Outcome::Ok
}

/// The boot scan sized every BAR while the BSP ran alone. A sizing write
/// moves a live BAR, and QEMU's TCG can send another CPU's MMIO through a
/// stale TLB entry meanwhile: a LAPIC EOI lost that way leaves the timer
/// vector in service, and that CPU takes no IPI again (ROADMAP §10.2).
#[cfg(target_arch = "x86_64")]
pub(crate) fn test_pci_scan_bsp_only() -> Outcome {
    // Acquire: pairs with the scan's Release store.
    match pci_init::SCAN_ONLINE.load(Ordering::Acquire) {
        0 => Outcome::Fail("scan has not run"),
        1 => Outcome::Ok,
        _ => Outcome::Fail("scan sized BARs with an AP online"),
    }
}

/// No driver binds the VGA function, so nothing claims or maps its BAR0:
/// the scan maps no BAR. The framebuffer that aliases it is on the
/// physmap write-back, never uncached (§9.2).
pub(crate) fn test_pci_bar_map() -> Outcome {
    let Some(d) = find_id(0x1234, 0x1111) else {
        return Outcome::Fail("no vga");
    };
    let r = d.resources[0];
    if r.is_empty() {
        return Outcome::Fail("vga bar0 empty");
    }
    if r.size == 0 || r.size > pci::MAX_BAR_MAP {
        return Outcome::Fail("vga bar0 size");
    }
    if dev_init::bound(&d).is_some() {
        return Outcome::Fail("vga bound");
    }
    if dev_init::is_claimed(&d, 0) {
        return Outcome::Fail("vga bar0 claimed");
    }
    if dev_init::bar_va(&d, 0).is_some() {
        return Outcome::Fail("vga bar0 mapped");
    }
    let va = paging_init::hhdm_offset().wrapping_add(r.addr);
    if let Some((_, _, flags)) = paging_init::translate(VirtAddr(va))
        && (flags.contains(PageFlags::PCD) || flags.contains(PageFlags::PWT))
    {
        return Outcome::Fail("vga bar0 physmap leaf not write-back");
    }
    Outcome::Ok
}

pub(crate) fn test_pci_cfg_rw() -> Outcome {
    let bdf = Bdf::new(0, 0, 0);
    let id = cfg_read32(bdf, CFG_VENDOR);
    if id as u16 != 0x8086 {
        return Outcome::Fail("host vendor");
    }
    if (id >> 16) as u16 != 0x1237 {
        return Outcome::Fail("host device");
    }
    let prev = cfg_read32(bdf, CFG_COMMAND) as u16;
    pci_init::update_command(bdf, CMD_MEM | CMD_MASTER, 0);
    let now = cfg_read32(bdf, CFG_COMMAND) as u16;
    cfg_write32(bdf, CFG_COMMAND, prev as u32);
    if now & (CMD_MEM | CMD_MASTER) != CMD_MEM | CMD_MASTER {
        return Outcome::Fail("cmd bits");
    }
    Outcome::Ok
}

pub(crate) fn test_pci_claim_exclusive() -> Outcome {
    let Some(d) = find_id(0x8086, 0x100e) else {
        return Outcome::Fail("no e1000");
    };
    let mut b = 0u8;
    let mut found = false;
    while (b as usize) < pci::MAX_BARS {
        if !d.resources[b as usize].is_empty() {
            found = true;
            break;
        }
        b += 1;
    }
    if !found {
        return Outcome::Fail("e1000 no bar");
    }
    let c = match dev_init::claim(&d, b) {
        Ok(c) => c,
        Err(e) => return Outcome::Fail(e.as_str()),
    };
    let why = match dev_init::claim(&d, b) {
        Err(ClaimError::Already) => None,
        Err(_) => Some("wrong claim err"),
        Ok(twice) => {
            dev_init::release(twice);
            Some("double claim")
        }
    };
    dev_init::release(c);
    if let Some(why) = why {
        return Outcome::Fail(why);
    }
    // A released claim frees its range: it claims again.
    match dev_init::claim(&d, b) {
        Ok(c) => {
            dev_init::release(c);
            Outcome::Ok
        }
        Err(e) => crate::fail_fmt!("claim after release: {}", e.as_str()),
    }
}

struct HostBridgeDrv;

static HOST_BRIDGE_IDS: &[IdMatch] = &[IdMatch::vid_did(0x8086, 0x1237)];

static HOST_BRIDGE_DRV: HostBridgeDrv = HostBridgeDrv;

impl Driver for HostBridgeDrv {
    fn name(&self) -> &'static str {
        "host-bridge"
    }
    fn ids(&self) -> &'static [IdMatch] {
        HOST_BRIDGE_IDS
    }
    fn order(&self) -> u8 {
        1
    }
    fn probe(&self, _dev: &DevRef) -> Result<Option<Instance>, ProbeError> {
        Ok(None)
    }
    fn remove(&self, _dev: &DevRef) {}
}

pub(crate) fn test_pci_bind_order() -> Outcome {
    if !dev_init::register_driver(&HOST_BRIDGE_DRV) {
        return Outcome::Fail("register");
    }
    dev_init::bind_all();
    let Some(d) = find_id(0x8086, 0x1237) else {
        return Outcome::Fail("no host");
    };
    match dev_init::bound(&d) {
        Some("host-bridge") => {}
        Some(_) => return Outcome::Fail("wrong driver"),
        None => return Outcome::Fail("unbound"),
    }
    if dev_init::state(&d) != Some(DevState::Bound) {
        return Outcome::Fail("host bridge not Bound");
    }
    // The host bridge sits on the root bus: no parent.
    if dev_init::parent(&d).is_some() {
        return Outcome::Fail("host bridge has a parent");
    }
    Outcome::Ok
}
