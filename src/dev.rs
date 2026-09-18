//! Device model and driver registry. ROADMAP §6.1.
//!
//! Scan fills a device list; [`Registry::bind_all`] matches drivers by
//! id and probes in dependency order. Resource claims are exclusive.

use core::fmt;

use crate::pci::{self, Bar, BarKind, Bdf, CapSet, FuncInfo, MAX_BARS, MAX_SCAN};

pub const MAX_DEVICES: usize = MAX_SCAN;
pub const MAX_DRIVERS: usize = 16;
pub const MAX_CLAIMS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdMatch {
    pub vendor: u16,
    pub device: u16,
    pub class: Option<u8>,
    pub subclass: Option<u8>,
}

impl IdMatch {
    pub const fn vid_did(vendor: u16, device: u16) -> Self {
        Self {
            vendor,
            device,
            class: None,
            subclass: None,
        }
    }

    pub const fn any_vendor(vendor: u16) -> Self {
        Self {
            vendor,
            device: 0xFFFF,
            class: None,
            subclass: None,
        }
    }

    pub const fn class(class: u8, subclass: Option<u8>) -> Self {
        Self {
            vendor: 0xFFFF,
            device: 0xFFFF,
            class: Some(class),
            subclass,
        }
    }

    pub fn matches(self, vendor: u16, device: u16, class: u8, subclass: u8) -> bool {
        if self.vendor != 0xFFFF && self.vendor != vendor {
            return false;
        }
        if self.device != 0xFFFF && self.device != device {
            return false;
        }
        if let Some(c) = self.class {
            if c != class {
                return false;
            }
        }
        if let Some(s) = self.subclass {
            if s != subclass {
                return false;
            }
        }
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    Empty,
    Memory,
    Io,
}

impl ResourceKind {
    pub fn name(self) -> &'static str {
        match self {
            ResourceKind::Empty => "empty",
            ResourceKind::Memory => "mem",
            ResourceKind::Io => "io",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resource {
    pub kind: ResourceKind,
    pub bar: u8,
    pub addr: u64,
    pub size: u64,
    pub prefetchable: bool,
    /// Kernel VA after ioremap / physmap. 0 = not mapped.
    pub mapped_va: u64,
    pub claimed: bool,
}

impl Resource {
    pub const EMPTY: Self = Self {
        kind: ResourceKind::Empty,
        bar: 0,
        addr: 0,
        size: 0,
        prefetchable: false,
        mapped_va: 0,
        claimed: false,
    };

    pub fn from_bar(bar_index: u8, bar: Bar) -> Self {
        if bar.is_none() {
            return Self::EMPTY;
        }
        let kind = match bar.kind {
            BarKind::None => return Self::EMPTY,
            BarKind::Io => ResourceKind::Io,
            BarKind::Mem32 | BarKind::Mem64 => ResourceKind::Memory,
        };
        Self {
            kind,
            bar: bar_index,
            addr: bar.addr,
            size: bar.size,
            prefetchable: bar.prefetchable,
            mapped_va: 0,
            claimed: false,
        }
    }

    pub fn is_empty(self) -> bool {
        matches!(self.kind, ResourceKind::Empty) || self.size == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrqBind {
    pub pin: u8,
    pub line: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeError {
    Busy,
    NoResource,
    Failed,
}

impl ProbeError {
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeError::Busy => "busy",
            ProbeError::NoResource => "no resource",
            ProbeError::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimError {
    Empty,
    Already,
    Overlap,
    BadIndex,
}

impl ClaimError {
    pub fn as_str(self) -> &'static str {
        match self {
            ClaimError::Empty => "empty",
            ClaimError::Already => "already claimed",
            ClaimError::Overlap => "overlap",
            ClaimError::BadIndex => "bad index",
        }
    }
}

pub trait Driver: Send + Sync {
    fn name(&self) -> &'static str;
    fn ids(&self) -> &'static [IdMatch];
    /// Lower binds first. Use this for dependency, not discovery order.
    fn order(&self) -> u8 {
        100
    }
    fn probe(&self, dev: &mut Device) -> Result<(), ProbeError>;
    fn remove(&self, dev: &mut Device);
}

#[derive(Clone, Copy, Debug)]
pub struct Device {
    pub addr: Bdf,
    pub vendor: u16,
    pub device_id: u16,
    pub revision: u8,
    pub prog_if: u8,
    pub subclass: u8,
    pub class: u8,
    pub resources: [Resource; MAX_BARS],
    pub irq: IrqBind,
    pub caps: CapSet,
    pub bound: Option<&'static str>,
}

impl Device {
    pub const fn empty() -> Self {
        Self {
            addr: Bdf::new(0, 0, 0),
            vendor: 0,
            device_id: 0,
            revision: 0,
            prog_if: 0,
            subclass: 0,
            class: 0,
            resources: [Resource::EMPTY; MAX_BARS],
            irq: IrqBind { pin: 0, line: 0 },
            caps: CapSet::empty(),
            bound: None,
        }
    }

    pub fn from_func(info: FuncInfo) -> Self {
        let mut resources = [Resource::EMPTY; MAX_BARS];
        let mut i = 0usize;
        while i < MAX_BARS {
            resources[i] = Resource::from_bar(i as u8, info.bars[i]);
            i += 1;
        }
        Self {
            addr: info.bdf,
            vendor: info.vendor,
            device_id: info.device,
            revision: info.revision,
            prog_if: info.prog_if,
            subclass: info.subclass,
            class: info.class,
            resources,
            irq: IrqBind {
                pin: info.irq_pin,
                line: info.irq_line,
            },
            caps: info.caps,
            bound: None,
        }
    }

    pub fn matches_id(&self, id: IdMatch) -> bool {
        id.matches(self.vendor, self.device_id, self.class, self.subclass)
    }

    pub fn matches_driver(&self, drv: &dyn Driver) -> bool {
        for id in drv.ids() {
            if self.matches_id(*id) {
                return true;
            }
        }
        false
    }

    pub fn write_lspci(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(
            f,
            "{} {:04x}:{:04x} {}",
            self.addr,
            self.vendor,
            self.device_id,
            pci::class_name(self.class, self.subclass)
        )?;
        if let Some(n) = pci::friendly_name(self.vendor, self.device_id) {
            write!(f, " [{n}]")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Claim {
    mem: bool,
    addr: u64,
    size: u64,
}

pub struct Registry {
    devices: [Device; MAX_DEVICES],
    n_dev: usize,
    drivers: [Option<&'static dyn Driver>; MAX_DRIVERS],
    n_drv: usize,
    claims: [Option<Claim>; MAX_CLAIMS],
    n_claim: usize,
}

impl Registry {
    pub const fn new() -> Self {
        Self {
            devices: [Device::empty(); MAX_DEVICES],
            n_dev: 0,
            drivers: [None; MAX_DRIVERS],
            n_drv: 0,
            claims: [None; MAX_CLAIMS],
            n_claim: 0,
        }
    }

    pub fn snapshot(&self, out: &mut [Device]) -> usize {
        let n = self.n_dev.min(out.len());
        let mut i = 0usize;
        while i < n {
            out[i] = self.devices[i];
            i += 1;
        }
        n
    }

    pub fn len(&self) -> usize {
        self.n_dev
    }

    pub fn driver_count(&self) -> usize {
        self.n_drv
    }

    pub fn get(&self, i: usize) -> Option<&Device> {
        if i < self.n_dev {
            Some(&self.devices[i])
        } else {
            None
        }
    }

    pub fn get_mut(&mut self, i: usize) -> Option<&mut Device> {
        if i < self.n_dev {
            Some(&mut self.devices[i])
        } else {
            None
        }
    }

    pub fn push(&mut self, d: Device) -> bool {
        if self.n_dev >= MAX_DEVICES {
            return false;
        }
        self.devices[self.n_dev] = d;
        self.n_dev += 1;
        true
    }

    pub fn register(&mut self, drv: &'static dyn Driver) -> bool {
        if self.n_drv >= MAX_DRIVERS {
            return false;
        }
        let mut i = 0usize;
        while i < self.n_drv {
            if let Some(d) = self.drivers[i] {
                if d.name() == drv.name() {
                    return false;
                }
            }
            i += 1;
        }
        self.drivers[self.n_drv] = Some(drv);
        self.n_drv += 1;
        true
    }

    fn range_overlap(a: u64, alen: u64, b: u64, blen: u64) -> bool {
        alen != 0 && blen != 0 && a < b.saturating_add(blen) && b < a.saturating_add(alen)
    }

    pub fn claim(&mut self, dev_i: usize, bar: u8) -> Result<(), ClaimError> {
        if bar as usize >= MAX_BARS {
            return Err(ClaimError::BadIndex);
        }
        if dev_i >= self.n_dev {
            return Err(ClaimError::BadIndex);
        }
        let res = self.devices[dev_i].resources[bar as usize];
        if res.is_empty() {
            return Err(ClaimError::Empty);
        }
        if res.claimed {
            return Err(ClaimError::Already);
        }
        let mem = matches!(res.kind, ResourceKind::Memory);
        let mut i = 0usize;
        while i < self.n_claim {
            if let Some(c) = self.claims[i] {
                if c.mem == mem && Self::range_overlap(c.addr, c.size, res.addr, res.size) {
                    return Err(ClaimError::Overlap);
                }
            }
            i += 1;
        }
        if self.n_claim >= MAX_CLAIMS {
            return Err(ClaimError::Overlap);
        }
        self.claims[self.n_claim] = Some(Claim {
            mem,
            addr: res.addr,
            size: res.size,
        });
        self.n_claim += 1;
        self.devices[dev_i].resources[bar as usize].claimed = true;
        Ok(())
    }

    /// Match unbound devices to drivers. `enable` runs before `probe`
    /// (command bits, etc). Drivers are sorted by [`Driver::order`].
    pub fn bind_all(&mut self, mut enable: impl FnMut(&Device)) {
        let mut order = [0u8; MAX_DRIVERS];
        let mut idx = [0u8; MAX_DRIVERS];
        let mut n = 0usize;
        let mut i = 0usize;
        while i < self.n_drv {
            if let Some(d) = self.drivers[i] {
                order[n] = d.order();
                idx[n] = i as u8;
                n += 1;
            }
            i += 1;
        }
        // Insertion sort: lower order first, registration as tie-break.
        let mut a = 1usize;
        while a < n {
            let mut b = a;
            while b > 0
                && (order[b] < order[b - 1] || (order[b] == order[b - 1] && idx[b] < idx[b - 1]))
            {
                order.swap(b, b - 1);
                idx.swap(b, b - 1);
                b -= 1;
            }
            a += 1;
        }
        let mut di = 0usize;
        while di < n {
            let drv = match self.drivers[idx[di] as usize] {
                Some(d) => d,
                None => {
                    di += 1;
                    continue;
                }
            };
            let mut dv = 0usize;
            while dv < self.n_dev {
                if self.devices[dv].bound.is_none() && self.devices[dv].matches_driver(drv) {
                    enable(&self.devices[dv]);
                    match drv.probe(&mut self.devices[dv]) {
                        Ok(()) => self.devices[dv].bound = Some(drv.name()),
                        Err(_) => {}
                    }
                }
                dv += 1;
            }
            di += 1;
        }
    }

    pub fn write_tree(&self, f: &mut impl fmt::Write) -> fmt::Result {
        let mut i = 0usize;
        while i < self.n_dev {
            let d = &self.devices[i];
            let drv = d.bound.unwrap_or("-");
            write!(
                f,
                "{} {:04x}:{:04x} {} drv={drv}",
                d.addr,
                d.vendor,
                d.device_id,
                pci::class_name(d.class, d.subclass)
            )?;
            if let Some(n) = pci::friendly_name(d.vendor, d.device_id) {
                write!(f, " [{n}]")?;
            }
            writeln!(f)?;
            let mut b = 0usize;
            while b < MAX_BARS {
                let r = d.resources[b];
                if !r.is_empty() {
                    writeln!(
                        f,
                        "  bar{} {} {:#x}/{:#x}{}",
                        r.bar,
                        r.kind.name(),
                        r.addr,
                        r.size,
                        if r.claimed { " claimed" } else { "" }
                    )?;
                }
                b += 1;
            }
            i += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU32, Ordering};

    struct D {
        name: &'static str,
        ids: &'static [IdMatch],
        order: u8,
        probes: AtomicU32,
        bar: u8,
    }

    // Safety: tests are single-threaded.
    unsafe impl Sync for D {}

    impl Driver for D {
        fn name(&self) -> &'static str {
            self.name
        }
        fn ids(&self) -> &'static [IdMatch] {
            self.ids
        }
        fn order(&self) -> u8 {
            self.order
        }
        fn probe(&self, dev: &mut Device) -> Result<(), ProbeError> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            if self.bar != 0xFF {
                let mut i = 0usize;
                while i < MAX_BARS {
                    if dev.resources[i].bar == self.bar && !dev.resources[i].is_empty() {
                        if dev.resources[i].claimed {
                            return Err(ProbeError::Busy);
                        }
                        dev.resources[i].claimed = true;
                        return Ok(());
                    }
                    i += 1;
                }
                return Err(ProbeError::NoResource);
            }
            Ok(())
        }
        fn remove(&self, _dev: &mut Device) {}
    }

    fn nic() -> Device {
        let mut d = Device::empty();
        d.addr = Bdf::new(0, 3, 0);
        d.vendor = 0x8086;
        d.device_id = 0x100e;
        d.class = 0x02;
        d.subclass = 0;
        d.resources[0] = Resource {
            kind: ResourceKind::Memory,
            bar: 0,
            addr: 0xFEB8_0000,
            size: 0x20000,
            prefetchable: false,
            mapped_va: 0,
            claimed: false,
        };
        d.resources[1] = Resource {
            kind: ResourceKind::Io,
            bar: 1,
            addr: 0xC000,
            size: 0x40,
            prefetchable: false,
            mapped_va: 0,
            claimed: false,
        };
        d
    }

    fn vga() -> Device {
        let mut d = Device::empty();
        d.addr = Bdf::new(0, 2, 0);
        d.vendor = 0x1234;
        d.device_id = 0x1111;
        d.class = 0x03;
        d.resources[0] = Resource {
            kind: ResourceKind::Memory,
            bar: 0,
            addr: 0xFD00_0000,
            size: 0x100_0000,
            prefetchable: true,
            mapped_va: 0,
            claimed: false,
        };
        d
    }

    #[test]
    fn id_match_vendor_device_and_class() {
        let id = IdMatch::vid_did(0x8086, 0x100e);
        assert!(id.matches(0x8086, 0x100e, 0x02, 0));
        assert!(!id.matches(0x8086, 0x1237, 0x06, 0));
        let any = IdMatch::any_vendor(0x8086);
        assert!(any.matches(0x8086, 0x1237, 0x06, 0));
        assert!(!any.matches(0x1234, 0x1111, 0x03, 0));
        let cls = IdMatch::class(0x03, Some(0x00));
        assert!(cls.matches(0x1234, 0x1111, 0x03, 0x00));
        assert!(!cls.matches(0x1234, 0x1111, 0x02, 0x00));
    }

    #[test]
    fn claim_rejects_second_and_overlap() {
        let mut r = Registry::new();
        assert!(r.push(nic()));
        assert!(r.push(vga()));
        r.claim(0, 0).unwrap();
        assert_eq!(r.claim(0, 0), Err(ClaimError::Already));
        assert_eq!(r.claim(0, 9), Err(ClaimError::BadIndex));
        // Same range from a cloned resource on another slot: overlap.
        let mut clone = nic();
        clone.addr = Bdf::new(0, 4, 0);
        assert!(r.push(clone));
        assert_eq!(r.claim(2, 0), Err(ClaimError::Overlap));
        r.claim(0, 1).unwrap();
        r.claim(1, 0).unwrap();
        assert!(r.get(0).unwrap().resources[0].claimed);
        assert_eq!(ClaimError::Already.as_str(), "already claimed");
        assert_eq!(ProbeError::Busy.as_str(), "busy");
        assert_eq!(ResourceKind::Memory.name(), "mem");
    }

    static E1000_IDS: &[IdMatch] = &[IdMatch::vid_did(0x8086, 0x100e)];
    static VGA_IDS: &[IdMatch] = &[IdMatch::vid_did(0x1234, 0x1111)];
    static CLASS_NET: &[IdMatch] = &[IdMatch::class(0x02, None)];

    static LATE: D = D {
        name: "late-nic",
        ids: CLASS_NET,
        order: 50,
        probes: AtomicU32::new(0),
        bar: 0xFF,
    };
    static EARLY: D = D {
        name: "early-nic",
        ids: E1000_IDS,
        order: 10,
        probes: AtomicU32::new(0),
        bar: 0,
    };
    static VGA: D = D {
        name: "vga",
        ids: VGA_IDS,
        order: 10,
        probes: AtomicU32::new(0),
        bar: 0,
    };

    #[test]
    fn bind_order_by_dependency_not_register_order() {
        LATE.probes.store(0, Ordering::SeqCst);
        EARLY.probes.store(0, Ordering::SeqCst);
        VGA.probes.store(0, Ordering::SeqCst);
        let mut r = Registry::new();
        r.push(nic());
        r.push(vga());
        // Register late first; early must still win the nic.
        assert!(r.register(&LATE));
        assert!(r.register(&EARLY));
        assert!(r.register(&VGA));
        let mut enables = 0u32;
        r.bind_all(|_| enables += 1);
        assert_eq!(enables, 2);
        assert_eq!(r.get(0).unwrap().bound, Some("early-nic"));
        assert_eq!(r.get(1).unwrap().bound, Some("vga"));
        assert_eq!(EARLY.probes.load(Ordering::SeqCst), 1);
        assert_eq!(LATE.probes.load(Ordering::SeqCst), 0);
        assert_eq!(VGA.probes.load(Ordering::SeqCst), 1);
        let mut tree = String::new();
        r.write_tree(&mut tree).unwrap();
        assert!(tree.contains("early-nic"));
        assert!(tree.contains("00:03.0"));
        assert!(tree.contains("bar0 mem"));
    }

    #[test]
    fn from_func_copies_bars_and_caps() {
        let mut info = FuncInfo::empty();
        info.bdf = Bdf::new(0, 2, 0);
        info.vendor = 0x1234;
        info.device = 0x1111;
        info.class = 0x03;
        info.bars[0] = Bar {
            kind: BarKind::Mem32,
            prefetchable: true,
            addr: 0xFD00_0000,
            size: 0x1000,
        };
        info.caps.msi = Some(0x50);
        let d = Device::from_func(info);
        assert_eq!(d.vendor, 0x1234);
        assert_eq!(d.resources[0].kind, ResourceKind::Memory);
        assert_eq!(d.resources[0].size, 0x1000);
        assert_eq!(d.caps.msi, Some(0x50));
        assert!(d.bound.is_none());
    }
}
