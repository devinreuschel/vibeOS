//! PCI config encoding, BAR decode, cap walk, bus scan. ROADMAP §6.2.
//!
//! No port I/O and no MMIO. Kernel `pci_init` supplies [`CfgIo`]. Host
//! tests drive a fake config space so R/W and size probes are portable.

use core::fmt;

/// Type-1 config address via `0xCF8`. Enable bit set. `offset` is aligned
/// down to a dword; the low two bits are zero (PCI).
pub const fn cf8_addr(bus: u8, dev: u8, func: u8, offset: u16) -> u32 {
    0x8000_0000
        | ((bus as u32) << 16)
        | (((dev as u32) & 0x1F) << 11)
        | (((func as u32) & 0x7) << 8)
        | ((offset as u32) & 0xFC)
}

/// Byte offset into an ECAM window for one allocation.
/// `addr = base + ecam_off(bus - start_bus, ...)`.
pub const fn ecam_off(bus_rel: u8, dev: u8, func: u8, offset: u16) -> u64 {
    ((bus_rel as u64) << 20)
        | (((dev as u64) & 0x1F) << 15)
        | (((func as u64) & 0x7) << 12)
        | (offset as u64 & 0xFFF)
}

/// Physical ECAM address, or `None` if `bus` is outside the window.
pub const fn ecam_phys(
    base: u64,
    start_bus: u8,
    end_bus: u8,
    bus: u8,
    dev: u8,
    func: u8,
    offset: u16,
) -> Option<u64> {
    if bus < start_bus || bus > end_bus {
        return None;
    }
    Some(base.wrapping_add(ecam_off(bus - start_bus, dev, func, offset)))
}

pub const CFG_VENDOR: u16 = 0x00;
pub const CFG_DEVICE: u16 = 0x02;
pub const CFG_COMMAND: u16 = 0x04;
pub const CFG_STATUS: u16 = 0x06;
pub const CFG_REVID_CLASS: u16 = 0x08;
pub const CFG_HEADER_TYPE: u16 = 0x0E;
pub const CFG_BAR0: u16 = 0x10;
pub const CFG_CAP_PTR: u16 = 0x34;
pub const CFG_IRQ_LINE: u16 = 0x3C;
pub const CFG_IRQ_PIN: u16 = 0x3D;
pub const CFG_SEC_BUS: u16 = 0x19;

pub const CMD_IO: u16 = 1 << 0;
pub const CMD_MEM: u16 = 1 << 1;
pub const CMD_MASTER: u16 = 1 << 2;

pub const STATUS_CAPS: u16 = 1 << 4;

pub const HEADER_MULTI: u8 = 1 << 7;
pub const HEADER_TYPE_MASK: u8 = 0x7F;
pub const HEADER_DEVICE: u8 = 0x00;
pub const HEADER_BRIDGE: u8 = 0x01;
pub const HEADER_CARDBUS: u8 = 0x02;

pub const CAP_PM: u8 = 0x01;
pub const CAP_MSI: u8 = 0x05;
pub const CAP_VENDOR: u8 = 0x09;
pub const CAP_PCIE: u8 = 0x10;
pub const CAP_MSIX: u8 = 0x11;

pub const INVALID_VENDOR: u16 = 0xFFFF;

/// Cap on mapping a memory BAR at boot. Bigger sizes are recorded but
/// not page-walked (DESIGN §4.1). 32 MiB covers QEMU VGA (16 MiB) and
/// the usual NIC MMIO without eating the 256 MiB ioremap window.
pub const MAX_BAR_MAP: u64 = 32 * 1024 * 1024;

pub const MAX_BARS: usize = 6;
pub const MAX_SCAN: usize = 64;
pub const MAX_CAP_WALK: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bdf {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl Bdf {
    pub const fn new(bus: u8, device: u8, function: u8) -> Self {
        Self {
            segment: 0,
            bus,
            device,
            function,
        }
    }
}

impl fmt::Display for Bdf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.device, self.function)
    }
}

/// Config dword R/W. Kernel: CF8 or ECAM. Tests: a byte buffer.
pub trait CfgIo {
    fn read32(&mut self, bdf: Bdf, offset: u16) -> u32;
    fn write32(&mut self, bdf: Bdf, offset: u16, value: u32);
}

pub fn read16<C: CfgIo>(cfg: &mut C, bdf: Bdf, offset: u16) -> u16 {
    let v = cfg.read32(bdf, offset & !3);
    ((v >> ((offset & 3) * 8)) & 0xFFFF) as u16
}

pub fn read8<C: CfgIo>(cfg: &mut C, bdf: Bdf, offset: u16) -> u8 {
    let v = cfg.read32(bdf, offset & !3);
    ((v >> ((offset & 3) * 8)) & 0xFF) as u8
}

pub fn write16<C: CfgIo>(cfg: &mut C, bdf: Bdf, offset: u16, val: u16) {
    let aligned = offset & !3;
    let shift = (offset & 3) * 8;
    let mut v = cfg.read32(bdf, aligned);
    v &= !(0xFFFFu32 << shift);
    v |= (val as u32) << shift;
    cfg.write32(bdf, aligned, v);
}

/// Write COMMAND without touching STATUS in the same dword. STATUS is
/// RW1C; an RMW that writes 1s back would clear posted errors.
pub fn write_command<C: CfgIo>(cfg: &mut C, bdf: Bdf, cmd: u16) {
    cfg.write32(bdf, CFG_COMMAND, cmd as u32);
}

pub fn vendor_id<C: CfgIo>(cfg: &mut C, bdf: Bdf) -> u16 {
    read16(cfg, bdf, CFG_VENDOR)
}

pub fn device_id<C: CfgIo>(cfg: &mut C, bdf: Bdf) -> u16 {
    read16(cfg, bdf, CFG_DEVICE)
}

pub fn present<C: CfgIo>(cfg: &mut C, bdf: Bdf) -> bool {
    let v = vendor_id(cfg, bdf);
    v != INVALID_VENDOR && v != 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarKind {
    None,
    Io,
    Mem32,
    Mem64,
}

impl BarKind {
    pub fn name(self) -> &'static str {
        match self {
            BarKind::None => "none",
            BarKind::Io => "io",
            BarKind::Mem32 => "mem32",
            BarKind::Mem64 => "mem64",
        }
    }

    pub fn is_mem(self) -> bool {
        match self {
            BarKind::Mem32 | BarKind::Mem64 => true,
            BarKind::None | BarKind::Io => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    pub kind: BarKind,
    pub prefetchable: bool,
    pub addr: u64,
    pub size: u64,
}

impl Bar {
    pub const NONE: Self = Self {
        kind: BarKind::None,
        prefetchable: false,
        addr: 0,
        size: 0,
    };

    pub fn is_none(self) -> bool {
        matches!(self.kind, BarKind::None) || self.size == 0
    }
}

/// True if this BAR may be mapped at boot (size probe succeeded and is
/// under the physmap/ioremap cap). Callers still refuse a map that would
/// exhaust the ioremap window.
pub const fn bar_map_allowed(size: u64) -> bool {
    size > 0 && size <= MAX_BAR_MAP
}

fn size_from_mask(mask: u64, flag_bits: u64, wide: bool) -> u64 {
    let m = mask & !flag_bits;
    if m == 0 {
        0
    } else if wide {
        (!m).wrapping_add(1)
    } else {
        // 32-bit (and I/O) masks must invert in 32-bit space.
        (!(m as u32)).wrapping_add(1) as u64
    }
}

/// Decode one BAR from the original programmed address and the all-1s
/// size-probe readback. `raw1`/`mask1` are the next dword for 64-bit mem.
pub fn decode_bar(raw0: u32, raw1: u32, mask0: u32, mask1: u32) -> (Bar, bool) {
    if raw0 == 0 && mask0 == 0 && raw1 == 0 && mask1 == 0 {
        return (Bar::NONE, false);
    }
    if raw0 & 1 != 0 {
        let addr = (raw0 & 0xFFFF_FFFC) as u64;
        let size = size_from_mask(mask0 as u64, 0x3, false);
        let bar = if size == 0 {
            Bar::NONE
        } else {
            Bar {
                kind: BarKind::Io,
                prefetchable: false,
                addr,
                size,
            }
        };
        return (bar, false);
    }
    let typ = (raw0 >> 1) & 0x3;
    let prefetchable = raw0 & (1 << 3) != 0;
    if typ == 0x2 {
        let addr = ((raw1 as u64) << 32) | (raw0 as u64 & !0xF);
        let mask = ((mask1 as u64) << 32) | (mask0 as u64);
        let size = size_from_mask(mask, 0xF, true);
        let bar = if size == 0 && addr == 0 {
            Bar::NONE
        } else {
            Bar {
                kind: BarKind::Mem64,
                prefetchable,
                addr,
                size,
            }
        };
        (bar, true)
    } else {
        let addr = (raw0 & !0xF) as u64;
        let size = size_from_mask(mask0 as u64, 0xF, false);
        let bar = if size == 0 && addr == 0 {
            Bar::NONE
        } else {
            Bar {
                kind: BarKind::Mem32,
                prefetchable,
                addr,
                size,
            }
        };
        (bar, false)
    }
}

/// Size-probe BAR `index` (0..5). Restores the original value(s).
pub fn probe_bar<C: CfgIo>(cfg: &mut C, bdf: Bdf, index: u8) -> (Bar, bool) {
    if index >= 6 {
        return (Bar::NONE, false);
    }
    let off = CFG_BAR0 + (index as u16) * 4;
    let raw0 = cfg.read32(bdf, off);
    cfg.write32(bdf, off, 0xFFFF_FFFF);
    let mask0 = cfg.read32(bdf, off);
    cfg.write32(bdf, off, raw0);
    let is64 = raw0 & 1 == 0 && ((raw0 >> 1) & 0x3) == 0x2;
    let (raw1, mask1) = if is64 && index + 1 < 6 {
        let off1 = off + 4;
        let r1 = cfg.read32(bdf, off1);
        cfg.write32(bdf, off1, 0xFFFF_FFFF);
        let m1 = cfg.read32(bdf, off1);
        cfg.write32(bdf, off1, r1);
        (r1, m1)
    } else {
        (0, 0)
    };
    decode_bar(raw0, raw1, mask0, mask1)
}

/// Capability offsets in config space. None = not present. Slice A
/// records them; Slice B enables MSI/MSI-X.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapSet {
    pub pm: Option<u8>,
    pub msi: Option<u8>,
    pub msix: Option<u8>,
    pub pcie: Option<u8>,
    pub vendor: Option<u8>,
}

impl CapSet {
    pub const fn empty() -> Self {
        Self {
            pm: None,
            msi: None,
            msix: None,
            pcie: None,
            vendor: None,
        }
    }
}

impl Default for CapSet {
    fn default() -> Self {
        Self::empty()
    }
}

/// Walk the cap list. Records offsets; does not enable MSI/MSI-X.
pub fn walk_caps<C: CfgIo>(cfg: &mut C, bdf: Bdf) -> CapSet {
    let mut out = CapSet::default();
    let status = read16(cfg, bdf, CFG_STATUS);
    if status & STATUS_CAPS == 0 {
        return out;
    }
    let mut ptr = read8(cfg, bdf, CFG_CAP_PTR);
    let mut n = 0usize;
    while ptr >= 0x40 && n < MAX_CAP_WALK {
        n += 1;
        let id = read8(cfg, bdf, ptr as u16);
        match id {
            CAP_PM => out.pm = Some(ptr),
            CAP_MSI => out.msi = Some(ptr),
            CAP_MSIX => out.msix = Some(ptr),
            CAP_PCIE => out.pcie = Some(ptr),
            CAP_VENDOR => {
                if out.vendor.is_none() {
                    out.vendor = Some(ptr);
                }
            }
            _ => {}
        }
        let next = read8(cfg, bdf, ptr as u16 + 1);
        if next == 0 || next == ptr {
            break;
        }
        ptr = next;
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuncInfo {
    pub bdf: Bdf,
    pub vendor: u16,
    pub device: u16,
    pub revision: u8,
    pub prog_if: u8,
    pub subclass: u8,
    pub class: u8,
    pub header: u8,
    pub irq_line: u8,
    pub irq_pin: u8,
    pub secondary_bus: u8,
    pub bars: [Bar; MAX_BARS],
    pub caps: CapSet,
}

impl FuncInfo {
    pub fn empty() -> Self {
        Self {
            bdf: Bdf::new(0, 0, 0),
            vendor: 0,
            device: 0,
            revision: 0,
            prog_if: 0,
            subclass: 0,
            class: 0,
            header: 0,
            irq_line: 0,
            irq_pin: 0,
            secondary_bus: 0,
            bars: [Bar::NONE; MAX_BARS],
            caps: CapSet::empty(),
        }
    }

    pub fn is_bridge(self) -> bool {
        self.header & HEADER_TYPE_MASK == HEADER_BRIDGE
    }

    pub fn multifunction(self) -> bool {
        self.header & HEADER_MULTI != 0
    }
}

pub fn read_function<C: CfgIo>(cfg: &mut C, bdf: Bdf) -> Option<FuncInfo> {
    if !present(cfg, bdf) {
        return None;
    }
    let vendor = vendor_id(cfg, bdf);
    let device = device_id(cfg, bdf);
    let class_dw = cfg.read32(bdf, CFG_REVID_CLASS);
    let header = read8(cfg, bdf, CFG_HEADER_TYPE);
    let mut info = FuncInfo {
        bdf,
        vendor,
        device,
        revision: (class_dw & 0xFF) as u8,
        prog_if: ((class_dw >> 8) & 0xFF) as u8,
        subclass: ((class_dw >> 16) & 0xFF) as u8,
        class: ((class_dw >> 24) & 0xFF) as u8,
        header,
        irq_line: read8(cfg, bdf, CFG_IRQ_LINE),
        irq_pin: read8(cfg, bdf, CFG_IRQ_PIN),
        secondary_bus: 0,
        bars: [Bar::NONE; MAX_BARS],
        caps: walk_caps(cfg, bdf),
    };
    if info.is_bridge() {
        info.secondary_bus = read8(cfg, bdf, CFG_SEC_BUS);
    }
    // Type-1/2 headers reuse BAR slots as bus-number / window regs.
    let n_bars = bar_slots(info.header);
    let mut i = 0u8;
    while i < n_bars {
        let (bar, wide) = probe_bar(cfg, bdf, i);
        info.bars[i as usize] = bar;
        if wide {
            if (i as usize) + 1 < MAX_BARS {
                info.bars[(i as usize) + 1] = Bar::NONE;
            }
            i = i.saturating_add(2);
        } else {
            i = i.saturating_add(1);
        }
    }
    Some(info)
}

fn bar_slots(header: u8) -> u8 {
    match header & HEADER_TYPE_MASK {
        HEADER_DEVICE => 6,
        HEADER_BRIDGE => 2,
        HEADER_CARDBUS => 1,
        _ => 0,
    }
}

fn bus_bit(seen: &mut [u64; 4], bus: u8) -> bool {
    let i = (bus / 64) as usize;
    let bit = 1u64 << (bus % 64);
    if seen[i] & bit != 0 {
        true
    } else {
        seen[i] |= bit;
        false
    }
}

/// Recursive scan from `start_bus`. Bridges with a non-zero secondary
/// bus are followed. Does not assign bus numbers.
pub fn enumerate<C: CfgIo>(cfg: &mut C, start_bus: u8, out: &mut [FuncInfo]) -> usize {
    let mut n = 0usize;
    let mut seen = [0u64; 4];
    scan_bus(cfg, start_bus, out, &mut n, &mut seen);
    n
}

fn scan_bus<C: CfgIo>(
    cfg: &mut C,
    bus: u8,
    out: &mut [FuncInfo],
    n: &mut usize,
    seen: &mut [u64; 4],
) {
    if bus_bit(seen, bus) {
        return;
    }
    for dev in 0u8..32 {
        let bdf0 = Bdf::new(bus, dev, 0);
        if !present(cfg, bdf0) {
            continue;
        }
        let hdr = read8(cfg, bdf0, CFG_HEADER_TYPE);
        let max_fn = if hdr & HEADER_MULTI != 0 { 8u8 } else { 1u8 };
        for func in 0..max_fn {
            if *n >= out.len() {
                return;
            }
            let bdf = Bdf::new(bus, dev, func);
            let Some(info) = read_function(cfg, bdf) else {
                continue;
            };
            let bridge = info.is_bridge();
            let sec = info.secondary_bus;
            out[*n] = info;
            *n += 1;
            if bridge && sec != 0 && sec != bus {
                scan_bus(cfg, sec, out, n, seen);
            }
        }
    }
}

pub fn enable_mem_master(cmd: u16) -> u16 {
    cmd | CMD_MEM | CMD_MASTER
}

/// Class/subclass one-liner for `lspci`. Unknown stays `device`.
pub fn class_name(class: u8, subclass: u8) -> &'static str {
    match (class, subclass) {
        (0x01, 0x01) => "ide",
        (0x01, 0x06) => "sata",
        (0x01, _) => "storage",
        (0x02, _) => "ethernet",
        (0x03, 0x00) => "vga",
        (0x03, _) => "display",
        (0x06, 0x00) => "host bridge",
        (0x06, 0x01) => "isa bridge",
        (0x06, 0x04) => "pci bridge",
        (0x06, _) => "bridge",
        (0x0c, 0x03) => "usb",
        (0x0c, 0x05) => "smbus",
        (0x0c, _) => "serial bus",
        (_, _) => "device",
    }
}

/// Handful of names worth printing. Unknown is `None`.
pub fn friendly_name(vendor: u16, device: u16) -> Option<&'static str> {
    match (vendor, device) {
        (0x8086, 0x1237) => Some("440FX"),
        (0x8086, 0x7000) => Some("PIIX3"),
        (0x8086, 0x7010) => Some("PIIX3 IDE"),
        (0x8086, 0x7020) => Some("PIIX3 USB"),
        (0x8086, 0x7113) => Some("PIIX4 ACPI"),
        (0x8086, 0x100e) => Some("e1000"),
        (0x8086, 0x10d3) => Some("e1000e"),
        (0x8086, 0x29c0) => Some("Q35"),
        (0x8086, 0x2918) => Some("ICH9 LPC"),
        (0x8086, 0x2922) => Some("ICH9 SATA"),
        (0x8086, 0x2930) => Some("ICH9 SMBus"),
        (0x1234, 0x1111) => Some("bochs"),
        (0x1af4, 0x1000) => Some("virtio-net"),
        (0x1af4, 0x1001) => Some("virtio-blk"),
        (0x1af4, 0x1041) => Some("virtio-net"),
        (0x1af4, 0x1042) => Some("virtio-blk"),
        (0x1af4, 0x1050) => Some("virtio-gpu"),
        _ => None,
    }
}

pub fn write_lspci_line(f: &mut impl fmt::Write, info: &FuncInfo) -> fmt::Result {
    write!(
        f,
        "{} {:04x}:{:04x} {}",
        info.bdf,
        info.vendor,
        info.device,
        class_name(info.class, info.subclass)
    )?;
    if let Some(n) = friendly_name(info.vendor, info.device) {
        write!(f, " [{n}]")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        slots: [Option<Slot>; 16],
    }

    struct Slot {
        bdf: Bdf,
        data: [u8; 256],
        /// Writable mask per BAR dword. 0 = unimplemented.
        bar_rw: [u32; 6],
    }

    impl Fake {
        fn new() -> Self {
            Self {
                slots: core::array::from_fn(|_| None),
            }
        }

        fn slot_mut(&mut self, bdf: Bdf) -> &mut Slot {
            for s in &mut self.slots {
                if let Some(sl) = s.as_mut() {
                    if sl.bdf == bdf {
                        return sl;
                    }
                }
            }
            for s in &mut self.slots {
                if s.is_none() {
                    *s = Some(Slot {
                        bdf,
                        data: [0xFF; 256],
                        bar_rw: [0; 6],
                    });
                    return s.as_mut().unwrap();
                }
            }
            panic!("fake bus full");
        }

        fn put16(&mut self, bdf: Bdf, off: u16, v: u16) {
            let s = self.slot_mut(bdf);
            let o = off as usize;
            s.data[o] = v as u8;
            s.data[o + 1] = (v >> 8) as u8;
        }

        fn put8(&mut self, bdf: Bdf, off: u16, v: u8) {
            self.slot_mut(bdf).data[off as usize] = v;
        }

        fn put32(&mut self, bdf: Bdf, off: u16, v: u32) {
            let s = self.slot_mut(bdf);
            let o = (off as usize) & !3;
            s.data[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }

        fn device(&mut self, bdf: Bdf, vendor: u16, device: u16, class: u8, subclass: u8) {
            self.put16(bdf, CFG_VENDOR, vendor);
            self.put16(bdf, CFG_DEVICE, device);
            self.put8(bdf, CFG_HEADER_TYPE, HEADER_DEVICE);
            self.put32(
                bdf,
                CFG_REVID_CLASS,
                (class as u32) << 24 | (subclass as u32) << 16,
            );
            self.put16(bdf, CFG_STATUS, 0);
            self.put16(bdf, CFG_COMMAND, 0);
            self.put8(bdf, CFG_IRQ_PIN, 0);
            self.put8(bdf, CFG_IRQ_LINE, 0);
            for i in 0..6 {
                self.put32(bdf, CFG_BAR0 + i * 4, 0);
            }
        }

        fn set_bar32(&mut self, bdf: Bdf, index: u8, addr: u32, size: u32, io: bool) {
            let s = self.slot_mut(bdf);
            let i = index as usize;
            let flags = if io { 1u32 } else { 0 };
            s.bar_rw[i] = if io {
                !(size.wrapping_sub(1)) & !0x3
            } else {
                !(size.wrapping_sub(1)) & !0xF
            };
            let o = (CFG_BAR0 as usize) + i * 4;
            let raw = (addr & s.bar_rw[i]) | flags;
            s.data[o..o + 4].copy_from_slice(&raw.to_le_bytes());
        }

        fn set_bar64(&mut self, bdf: Bdf, index: u8, addr: u64, size: u64) {
            let s = self.slot_mut(bdf);
            let i = index as usize;
            let mask = !(size.wrapping_sub(1));
            s.bar_rw[i] = (mask as u32) & !0xF;
            s.bar_rw[i + 1] = (mask >> 32) as u32;
            let lo = (addr as u32 & !0xF) | 0x4; // 64-bit mem
            let hi = (addr >> 32) as u32;
            let o = (CFG_BAR0 as usize) + i * 4;
            s.data[o..o + 4].copy_from_slice(&lo.to_le_bytes());
            s.data[o + 4..o + 8].copy_from_slice(&hi.to_le_bytes());
        }
    }

    impl CfgIo for Fake {
        fn read32(&mut self, bdf: Bdf, offset: u16) -> u32 {
            let off = (offset as usize) & !3;
            for s in &self.slots {
                if let Some(sl) = s {
                    if sl.bdf == bdf && off + 4 <= 256 {
                        return u32::from_le_bytes(sl.data[off..off + 4].try_into().unwrap());
                    }
                }
            }
            0xFFFF_FFFF
        }

        fn write32(&mut self, bdf: Bdf, offset: u16, value: u32) {
            let off = (offset as usize) & !3;
            for s in &mut self.slots {
                let Some(sl) = s else { continue };
                if sl.bdf != bdf || off + 4 > 256 {
                    continue;
                }
                if (CFG_BAR0 as usize..CFG_BAR0 as usize + 24).contains(&off) {
                    let i = (off - CFG_BAR0 as usize) / 4;
                    let rw = sl.bar_rw[i];
                    let cur = u32::from_le_bytes(sl.data[off..off + 4].try_into().unwrap());
                    let flags = if i + 1 < 6 && sl.bar_rw[i + 1] != 0 && i % 2 == 0 {
                        cur & 0xF
                    } else if cur & 1 != 0 {
                        cur & 0x3
                    } else {
                        cur & 0xF
                    };
                    let stored = (value & rw) | flags;
                    sl.data[off..off + 4].copy_from_slice(&stored.to_le_bytes());
                    return;
                }
                sl.data[off..off + 4].copy_from_slice(&value.to_le_bytes());
                return;
            }
        }
    }

    #[test]
    fn cf8_sets_enable_and_fields() {
        let a = cf8_addr(1, 2, 3, 0x12);
        assert_eq!(a & 0x8000_0000, 0x8000_0000);
        assert_eq!((a >> 16) & 0xFF, 1);
        assert_eq!((a >> 11) & 0x1F, 2);
        assert_eq!((a >> 8) & 0x7, 3);
        assert_eq!(a & 0xFC, 0x10);
        assert_eq!(cf8_addr(0, 0, 0, 0), 0x8000_0000);
    }

    #[test]
    fn ecam_offset_and_window() {
        assert_eq!(ecam_off(0, 0, 0, 0), 0);
        assert_eq!(
            ecam_off(1, 2, 3, 0x10),
            (1 << 20) | (2 << 15) | (3 << 12) | 0x10
        );
        assert_eq!(
            ecam_phys(0xE000_0000, 0, 0xFF, 1, 0, 0, 0),
            Some(0xE000_0000 + (1 << 20))
        );
        assert_eq!(ecam_phys(0xE000_0000, 1, 3, 0, 0, 0, 0), None);
        assert_eq!(
            ecam_phys(0xB000_0000, 1, 3, 2, 0, 1, 4),
            Some(0xB000_0000 + ecam_off(1, 0, 1, 4))
        );
    }

    #[test]
    fn config_rw_dword_and_16() {
        let mut f = Fake::new();
        let b = Bdf::new(0, 1, 0);
        f.device(b, 0x8086, 0x1237, 0x06, 0x00);
        assert_eq!(vendor_id(&mut f, b), 0x8086);
        assert_eq!(device_id(&mut f, b), 0x1237);
        f.write32(b, CFG_COMMAND, 0x0000_0007);
        assert_eq!(read16(&mut f, b, CFG_COMMAND), 7);
        write16(&mut f, b, CFG_COMMAND, 0x0006);
        assert_eq!(read16(&mut f, b, CFG_COMMAND), 6);
        assert_eq!(enable_mem_master(0), CMD_MEM | CMD_MASTER);
        f.put16(b, CFG_STATUS, 0xFFFF);
        write_command(&mut f, b, 0x0006);
        assert_eq!(read16(&mut f, b, CFG_COMMAND), 6);
        // High half written as 0 so RW1C STATUS bits are not cleared
        // by writing 1. Fake stores the dword as written.
        assert_eq!(read16(&mut f, b, CFG_STATUS), 0);
    }

    #[test]
    fn bar_32_mem_size() {
        let (bar, wide) = decode_bar(0xF000_0000, 0, 0xFFFF_F000, 0);
        assert!(!wide);
        assert_eq!(bar.kind, BarKind::Mem32);
        assert_eq!(bar.addr, 0xF000_0000);
        assert_eq!(bar.size, 0x1000);
        assert!(bar_map_allowed(bar.size));
    }

    #[test]
    fn bar_64_mem_size() {
        let (bar, wide) = decode_bar(0x4, 0x0000_0001, 0xFFFF_F004, 0xFFFF_FFFF);
        assert!(wide);
        assert_eq!(bar.kind, BarKind::Mem64);
        assert_eq!(bar.addr, 0x1_0000_0000);
        assert_eq!(bar.size, 0x1000);
    }

    #[test]
    fn bar_io_size() {
        let (bar, wide) = decode_bar(0xC001, 0, 0xFFFF_FFFD, 0);
        assert!(!wide);
        assert_eq!(bar.kind, BarKind::Io);
        assert_eq!(bar.addr, 0xC000);
        assert_eq!(bar.size, 4);
    }

    #[test]
    fn huge_bar_refused_for_map() {
        // 1 TiB 64-bit window.
        let size = 1u64 << 40;
        let mask = !(size - 1);
        let (bar, wide) = decode_bar(0x4, 0, mask as u32, (mask >> 32) as u32);
        assert!(wide);
        assert_eq!(bar.size, size);
        assert!(!bar_map_allowed(bar.size));
        assert!(bar_map_allowed(16 * 1024 * 1024));
        assert!(!bar_map_allowed(MAX_BAR_MAP + 1));
        assert!(!bar_map_allowed(0));
    }

    #[test]
    fn probe_bar_restores_and_sizes() {
        let mut f = Fake::new();
        let b = Bdf::new(0, 2, 0);
        f.device(b, 0x1234, 0x1111, 0x03, 0x00);
        f.set_bar32(b, 0, 0xFD00_0000, 0x0100_0000, false);
        let orig = f.read32(b, CFG_BAR0);
        let (bar, wide) = probe_bar(&mut f, b, 0);
        assert!(!wide);
        assert_eq!(bar.kind, BarKind::Mem32);
        assert_eq!(bar.size, 0x0100_0000);
        assert_eq!(bar.addr, 0xFD00_0000);
        assert_eq!(f.read32(b, CFG_BAR0), orig);
        f.set_bar32(b, 1, 0xC000, 0x20, true);
        let (io, _) = probe_bar(&mut f, b, 1);
        assert_eq!(io.kind, BarKind::Io);
        assert_eq!(io.size, 0x20);
        assert_eq!(io.addr, 0xC000);
        f.set_bar64(b, 2, 0x12_0000_0000, 0x2000);
        let (b64, wide) = probe_bar(&mut f, b, 2);
        assert!(wide);
        assert_eq!(b64.kind, BarKind::Mem64);
        assert_eq!(b64.size, 0x2000);
        assert_eq!(b64.addr, 0x12_0000_0000);
    }

    #[test]
    fn cap_walk_records_ids() {
        let mut f = Fake::new();
        let b = Bdf::new(0, 3, 0);
        f.device(b, 0x8086, 0x100e, 0x02, 0x00);
        f.put16(b, CFG_STATUS, STATUS_CAPS);
        f.put8(b, CFG_CAP_PTR, 0x40);
        // 0x40: PM, next 0x50
        f.put8(b, 0x40, CAP_PM);
        f.put8(b, 0x41, 0x50);
        // 0x50: MSI, next 0x60
        f.put8(b, 0x50, CAP_MSI);
        f.put8(b, 0x51, 0x60);
        // 0x60: MSI-X, next 0x70
        f.put8(b, 0x60, CAP_MSIX);
        f.put8(b, 0x61, 0x70);
        // 0x70: PCIe, next 0
        f.put8(b, 0x70, CAP_PCIE);
        f.put8(b, 0x71, 0);
        let c = walk_caps(&mut f, b);
        assert_eq!(c.pm, Some(0x40));
        assert_eq!(c.msi, Some(0x50));
        assert_eq!(c.msix, Some(0x60));
        assert_eq!(c.pcie, Some(0x70));
        let mut empty = Fake::new();
        empty.device(b, 0x8086, 0x1237, 0x06, 0x00);
        assert_eq!(walk_caps(&mut empty, b), CapSet::default());
    }

    #[test]
    fn enumerate_follows_bridge() {
        let mut f = Fake::new();
        let host = Bdf::new(0, 0, 0);
        let br = Bdf::new(0, 1, 0);
        let leaf = Bdf::new(1, 0, 0);
        f.device(host, 0x8086, 0x1237, 0x06, 0x00);
        f.device(br, 0x8086, 0x2448, 0x06, 0x04);
        f.put8(br, CFG_HEADER_TYPE, HEADER_BRIDGE);
        f.put8(br, CFG_SEC_BUS, 1);
        f.device(leaf, 0x8086, 0x100e, 0x02, 0x00);
        let mut out = [FuncInfo::empty(); 8];
        let n = enumerate(&mut f, 0, &mut out);
        assert_eq!(n, 3);
        assert_eq!(out[0].device, 0x1237);
        assert_eq!(out[1].device, 0x2448);
        assert!(out[1].is_bridge());
        assert_eq!(out[2].bdf.bus, 1);
        assert_eq!(out[2].device, 0x100e);
        // BAR probe must not smash type-1 bus numbers.
        assert_eq!(read8(&mut f, br, CFG_SEC_BUS), 1);
    }

    #[test]
    fn enumerate_skips_empty_and_unnumbered_bridge() {
        let mut f = Fake::new();
        let host = Bdf::new(0, 0, 0);
        let br = Bdf::new(0, 1, 0);
        f.device(host, 0x8086, 0x1237, 0x06, 0x00);
        f.device(br, 0x8086, 0x2448, 0x06, 0x04);
        f.put8(br, CFG_HEADER_TYPE, HEADER_BRIDGE);
        f.put8(br, CFG_SEC_BUS, 0);
        let mut out = [FuncInfo::empty(); 8];
        let n = enumerate(&mut f, 0, &mut out);
        assert_eq!(n, 2);
    }

    #[test]
    fn multifunction_scans_other_fns() {
        let mut f = Fake::new();
        let a = Bdf::new(0, 1, 0);
        let b = Bdf::new(0, 1, 1);
        f.device(a, 0x8086, 0x7000, 0x06, 0x01);
        f.put8(a, CFG_HEADER_TYPE, HEADER_DEVICE | HEADER_MULTI);
        f.device(b, 0x8086, 0x7010, 0x01, 0x01);
        f.put8(b, CFG_HEADER_TYPE, HEADER_DEVICE | HEADER_MULTI);
        let mut out = [FuncInfo::empty(); 8];
        let n = enumerate(&mut f, 0, &mut out);
        assert_eq!(n, 2);
        assert_eq!(out[0].device, 0x7000);
        assert_eq!(out[1].device, 0x7010);
        assert_eq!(out[1].bdf.function, 1);
    }

    #[test]
    fn missing_vendor_is_absent() {
        let mut f = Fake::new();
        assert!(!present(&mut f, Bdf::new(0, 7, 0)));
        assert!(read_function(&mut f, Bdf::new(0, 7, 0)).is_none());
    }

    #[test]
    fn class_and_friendly_names() {
        assert_eq!(class_name(0x06, 0x00), "host bridge");
        assert_eq!(class_name(0x06, 0x01), "isa bridge");
        assert_eq!(class_name(0x01, 0x01), "ide");
        assert_eq!(class_name(0x03, 0x00), "vga");
        assert_eq!(class_name(0x02, 0x00), "ethernet");
        assert_eq!(class_name(0x99, 0x00), "device");
        assert_eq!(friendly_name(0x8086, 0x1237), Some("440FX"));
        assert_eq!(friendly_name(0x1234, 0x1111), Some("bochs"));
        assert_eq!(friendly_name(0x8086, 0x100e), Some("e1000"));
        assert_eq!(friendly_name(0x0000, 0x0000), None);
        let mut info = FuncInfo::empty();
        info.bdf = Bdf::new(0, 2, 0);
        info.vendor = 0x1234;
        info.device = 0x1111;
        info.class = 0x03;
        info.subclass = 0x00;
        let mut s = String::new();
        write_lspci_line(&mut s, &info).unwrap();
        assert!(s.contains("00:02.0"));
        assert!(s.contains("1234:1111"));
        assert!(s.contains("vga"));
        assert!(s.contains("bochs"));
        assert_eq!(BarKind::Io.name(), "io");
        assert!(BarKind::Mem64.is_mem());
        assert!(!BarKind::Io.is_mem());
    }

    #[test]
    fn cap_walk_stops_on_loop() {
        let mut f = Fake::new();
        let b = Bdf::new(0, 4, 0);
        f.device(b, 0x8086, 0x1237, 0x06, 0x00);
        f.put16(b, CFG_STATUS, STATUS_CAPS);
        f.put8(b, CFG_CAP_PTR, 0x40);
        f.put8(b, 0x40, CAP_VENDOR);
        f.put8(b, 0x41, 0x40);
        let c = walk_caps(&mut f, b);
        assert_eq!(c.vendor, Some(0x40));
    }
}
