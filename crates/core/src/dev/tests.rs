//! Host tests for the device registry, its claims and its binder.

use super::*;
use core::iter::once;
use core::sync::atomic::{AtomicU32, Ordering};

#[test]
fn probe_error_no_memory_str() {
    assert_eq!(ProbeError::NoMemory.as_str(), "no memory");
    assert_eq!(
        ProbeError::from(crate::kalloc::AllocError),
        ProbeError::NoMemory
    );
    assert_eq!(crate::virtio::VirtioError::NoMemory.as_str(), "no memory");
}

struct D {
    name: &'static str,
    ids: &'static [IdMatch],
    order: u8,
    probes: AtomicU32,
    bar: u8,
}

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
    fn probe(&self, dev: &DevRef) -> Result<Option<Instance>, ProbeError> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        if self.bar != 0xFF {
            if dev.resources[self.bar as usize].is_empty() {
                return Err(ProbeError::NoResource);
            }
            return Ok(None);
        }
        Ok(None)
    }
    fn remove(&self, _dev: &DevRef) {}
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
    };
    d.resources[1] = Resource {
        kind: ResourceKind::Io,
        bar: 1,
        addr: 0xC000,
        size: 0x40,
        prefetchable: false,
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

/// No RAM: the claims that test overlap and slots, not RAM.
const NO_RAM: [Range<u64>; 0] = [];

#[test]
fn claim_rejects_second_and_overlap() {
    let mut r = Registry::new();
    let d0 = r.push(nic()).unwrap();
    let d1 = r.push(vga()).unwrap();
    let c0 = r.claim(&d0, 0, NO_RAM).unwrap();
    assert_eq!(
        (c0.dev(), c0.bar(), c0.phys(), c0.len()),
        (d0.id(), 0, 0xFEB8_0000, 0x20000)
    );
    assert!(c0.is_mem() && !c0.prefetchable() && !c0.is_empty());
    assert_eq!(r.claim(&d0, 0, NO_RAM), Err(ClaimError::Already));
    assert_eq!(r.claim(&d0, 9, NO_RAM), Err(ClaimError::BadIndex));
    assert_eq!(r.claim(&d0, 2, NO_RAM), Err(ClaimError::Empty));
    // Same range from a cloned resource on another slot: overlap.
    let mut clone = nic();
    clone.addr = Bdf::new(0, 4, 0);
    let d2 = r.push(clone).unwrap();
    assert_eq!(r.claim(&d2, 0, NO_RAM), Err(ClaimError::Overlap));
    // An I/O range never collides with a memory one.
    let c1 = r.claim(&d0, 1, NO_RAM).unwrap();
    assert!(!c1.is_mem());
    assert_eq!(r.claim(&d2, 1, NO_RAM), Err(ClaimError::Overlap));
    let c2 = r.claim(&d1, 0, NO_RAM).unwrap();
    assert!(c2.prefetchable());
    assert!(r.is_claimed(&d0, 0));
    assert!(!r.is_claimed(&d2, 0));
    let stray = DevRef::try_new(99, nic()).unwrap();
    assert_eq!(r.claim(&stray, 0, NO_RAM), Err(ClaimError::BadIndex));
    assert_eq!(ClaimError::Already.as_str(), "already claimed");
    assert_eq!(ClaimError::Ram.as_str(), "ram");
    assert_eq!(ClaimError::Full.as_str(), "table full");
    assert_eq!(ProbeError::Busy.as_str(), "busy");
    assert_eq!(ResourceKind::Memory.name(), "mem");
    r.release(c0);
    r.release(c1);
    r.release(c2);
}

#[test]
fn claim_refuses_ram_overlap() {
    let mut r = Registry::new();
    let d0 = r.push(nic()).unwrap();
    let d1 = r.push(vga()).unwrap();
    // Usable, bootloader-reclaimable and NVS-like ranges around the
    // nic's BAR0 at 0xFEB8_0000..0xFEBA_0000.
    let usable = 0..0x0800_0000u64;
    let reclaim = 0x0800_0000..0x0900_0000u64;
    let nvs = 0xFEB9_F000..0xFEBA_0000u64;
    assert_eq!(
        r.claim(&d0, 0, [usable.clone(), reclaim.clone(), nvs]),
        Err(ClaimError::Ram)
    );
    assert!(!r.is_claimed(&d0, 0));
    // RAM that ends one byte into the BAR overlaps; RAM that ends where
    // the BAR starts does not.
    assert_eq!(
        r.claim(&d0, 0, once(0xFEB0_0000..0xFEB8_0001)),
        Err(ClaimError::Ram)
    );
    let c = r.claim(&d0, 0, once(0xFEB0_0000..0xFEB8_0000)).unwrap();
    r.release(c);
    // An I/O BAR is not checked against RAM.
    let io = r.claim(&d0, 1, once(0..0x1_0000u64)).unwrap();
    assert!(!io.is_mem());
    r.release(io);
    // RAM is checked before other claims: a BAR that is both is `Ram`.
    let v = r.claim(&d1, 0, [usable.clone(), reclaim.clone()]).unwrap();
    let mut twin = vga();
    twin.addr = Bdf::new(0, 5, 0);
    let d2 = r.push(twin).unwrap();
    assert_eq!(
        r.claim(&d2, 0, once(0xFD00_0000..0xFD00_1000)),
        Err(ClaimError::Ram)
    );
    assert_eq!(r.claim(&d2, 0, NO_RAM), Err(ClaimError::Overlap));
    r.release(v);
}

#[test]
fn claim_release_frees_range() {
    let mut r = Registry::new();
    let d0 = r.push(nic()).unwrap();
    let mut clone = nic();
    clone.addr = Bdf::new(0, 4, 0);
    let d1 = r.push(clone).unwrap();
    let c = r.claim(&d0, 0, NO_RAM).unwrap();
    assert_eq!(r.claim(&d1, 0, NO_RAM), Err(ClaimError::Overlap));
    r.release(c);
    assert!(!r.is_claimed(&d0, 0));
    // The same range claims again, from either device.
    let c = r.claim(&d1, 0, NO_RAM).unwrap();
    r.release(c);
    // A hold, take and release cycle frees the row too.
    let c = r.claim(&d0, 0, NO_RAM).unwrap();
    assert!(r.hold(c, Some(0xFFFF_E000_0000_0000)).is_ok());
    assert_eq!(r.bar_va(&d0, 0), Some(0xFFFF_E000_0000_0000));
    assert_eq!(r.bar_va(&d1, 0), None);
    // A second hold of the same BAR hands its claim back.
    let io = r.claim(&d0, 1, NO_RAM).unwrap();
    assert!(r.hold(io, None).is_ok());
    let (c, va) = r.take_bar(&d0, 0).unwrap();
    assert_eq!(va, Some(0xFFFF_E000_0000_0000));
    assert!(r.take_bar(&d0, 0).is_none());
    assert_eq!(r.bar_va(&d0, 0), None);
    assert!(r.is_claimed(&d0, 0));
    r.release(c);
    assert!(!r.is_claimed(&d0, 0));
    let (io, va) = r.take_bar(&d0, 1).unwrap();
    assert_eq!(va, None);
    r.release(io);
    // Every row is free again: the table takes MAX_CLAIMS claims.
    let rows = r.claims.iter().filter(|c| c.is_none()).count();
    assert_eq!(rows, MAX_CLAIMS);
    // A claim for an entry that holds that BAR already comes back.
    let c = r.claim(&d1, 0, NO_RAM).unwrap();
    assert!(r.hold(c, None).is_ok());
    let (c, _) = r.take_bar(&d1, 0).unwrap();
    let dup = BarClaim {
        dev: c.dev,
        at: c.at,
        bar: c.bar,
        mem: c.mem,
        addr: c.addr,
        size: c.size,
        prefetchable: c.prefetchable,
    };
    assert!(r.hold(c, None).is_ok());
    let back = r.hold(dup, None).unwrap_err();
    assert_eq!(back.bar(), 0);
    let (c, _) = r.take_bar(&d1, 0).unwrap();
    r.release(c);
    r.release(back);
}

#[test]
fn claim_table_full_is_full() {
    let mut r = Registry::new();
    let mut held = Vec::new();
    let mut i = 0u64;
    // Distinct 4 KiB memory BARs, six per device, until the table fills.
    while held.len() < MAX_CLAIMS {
        let mut d = Device::empty();
        d.addr = Bdf::new(1, i as u8, 0);
        for (b, res) in d.resources.iter_mut().enumerate() {
            *res = Resource {
                kind: ResourceKind::Memory,
                bar: b as u8,
                addr: 0x1_0000_0000 + (i * MAX_BARS as u64 + b as u64) * 0x1000,
                size: 0x1000,
                prefetchable: false,
            };
        }
        let d = r.push(d).unwrap();
        for b in 0..MAX_BARS as u8 {
            if held.len() < MAX_CLAIMS {
                held.push(r.claim(&d, b, NO_RAM).unwrap());
            }
        }
        i += 1;
    }
    let d = r.push(nic()).unwrap();
    assert_eq!(r.claim(&d, 0, NO_RAM), Err(ClaimError::Full));
    // Order: `Already` and `Overlap` come before `Full`.
    let first = r.get(0).unwrap();
    assert_eq!(r.claim(&first, 0, NO_RAM), Err(ClaimError::Already));
    // A released row is reused.
    let c = held.pop().unwrap();
    r.release(c);
    let c = r.claim(&d, 0, NO_RAM).unwrap();
    r.release(c);
    for c in held {
        r.release(c);
    }
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
    let d0 = r.push(nic()).unwrap();
    let d1 = r.push(vga()).unwrap();
    // Register late first; early must still win the nic.
    assert!(r.register(&LATE));
    assert!(r.register(&EARLY));
    assert!(r.register(&VGA));
    let mut jobs: [Option<(u8, DevRef)>; MAX_DEVICES] = [const { None }; MAX_DEVICES];
    let nj = r.collect_bind_jobs(&mut jobs);
    assert!(nj >= 2);
    let (drv0, dev0) = jobs[0].as_ref().unwrap();
    assert_eq!(r.driver_at(*drv0 as usize).unwrap().name(), "early-nic");
    assert!(dev0.same(&d0));
    drop(jobs);
    let mut enables = 0u32;
    r.bind_all(|_| enables += 1);
    assert_eq!(enables, 2);
    assert_eq!(r.bound(&d0), Some("early-nic"));
    assert_eq!(r.bound(&d1), Some("vga"));
    assert_eq!(r.state(&d0), Some(DevState::Bound));
    assert_eq!(r.state(&d1), Some(DevState::Bound));
    assert_eq!(EARLY.probes.load(Ordering::SeqCst), 1);
    assert_eq!(LATE.probes.load(Ordering::SeqCst), 0);
    assert_eq!(VGA.probes.load(Ordering::SeqCst), 1);
    // A bound device does not bind again.
    assert!(r.bind(&d0, "late-nic", None).is_err());
    let c = r.claim(&d0, 0, NO_RAM).unwrap();
    let mut tree = String::new();
    r.write_tree(&mut tree).unwrap();
    assert!(!tree.contains(" claimed"));
    assert!(r.hold(c, None).is_ok());
    let mut tree = String::new();
    r.write_tree(&mut tree).unwrap();
    assert!(tree.contains("early-nic"));
    assert!(tree.contains("00:03.0"));
    assert!(tree.contains("bar0 mem"));
    assert!(tree.contains(" claimed"));
}

#[test]
fn registry_holds_references() {
    // The table holds counted entries, not records by value. Each
    // entry also holds its BARs' claims (six per device), which puts
    // the table past a shell stack's share: the kernel's is a static
    // (`dev_init::REG`) built in a const and used in place, and no
    // caller copies it. The bound keeps its growth in view.
    let reg = core::mem::size_of::<Registry>();
    assert!(reg < 32 * 1024, "Registry {reg} should be under 32 KiB");
    let claim = core::mem::size_of::<BarClaim>();
    assert!(claim <= 40, "BarClaim {claim} should be at most 40 bytes");
    let mut r = Registry::new();
    let d = r.push(nic()).unwrap();
    let g = r.get(0).unwrap();
    assert_eq!(g.id(), d.id());
    assert!(g.same(&d));
    assert_eq!(g.vendor, 0x8086);
    assert!(r.get(1).is_none());
}

#[test]
fn devref_ids_never_reused() {
    let mut r = Registry::new();
    let a = r.push(nic()).unwrap();
    let b = r.push(vga()).unwrap();
    let c = r.push(nic()).unwrap();
    assert_eq!((a.id(), b.id(), c.id()), (1, 2, 3));
    for (i, want) in [&a, &b, &c].into_iter().enumerate() {
        let got = r.get(i).unwrap();
        assert_eq!(got.id(), want.id());
        assert!(r.by_id(want.id()).unwrap().same(want));
    }
    assert!(r.by_id(4).is_none());
    // An id taken for an entry built outside the table is not reused.
    let id = r.take_id();
    assert_eq!(id, 4);
    assert_eq!(r.push(vga()).unwrap().id(), 5);
}

/// Drops of [`Counted`]. Only `probe_instance_owned_by_entry` makes a
/// `Counted`: host tests run on parallel threads, so a second test that
/// dropped one would race its counts.
static DROPS: AtomicU32 = AtomicU32::new(0);

/// A driver's per-device state, counting its drops.
struct Counted(u64);

impl Drop for Counted {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

/// A driver with no state of its own: each probe returns a new
/// instance for the registry to own.
struct Stateless;

impl Driver for Stateless {
    fn name(&self) -> &'static str {
        "stateless"
    }
    fn ids(&self) -> &'static [IdMatch] {
        E1000_IDS
    }
    fn probe(&self, dev: &DevRef) -> Result<Option<Instance>, ProbeError> {
        Ok(Some(instance(Counted(dev.id()))?))
    }
    fn remove(&self, _dev: &DevRef) {}
}

static STATELESS: Stateless = Stateless;

/// A driver like [`Stateless`] whose instance is a plain id, so a test can
/// probe with it without touching [`DROPS`].
struct Plain;

impl Driver for Plain {
    fn name(&self) -> &'static str {
        "plain"
    }
    fn ids(&self) -> &'static [IdMatch] {
        E1000_IDS
    }
    fn probe(&self, dev: &DevRef) -> Result<Option<Instance>, ProbeError> {
        Ok(Some(instance(dev.id())?))
    }
    fn remove(&self, _dev: &DevRef) {}
}

static PLAIN: Plain = Plain;

#[test]
fn probe_instance_owned_by_entry() {
    assert_eq!(core::mem::size_of::<Stateless>(), 0);
    DROPS.store(0, Ordering::SeqCst);
    let mut r = Registry::new();
    let d0 = r.push(nic()).unwrap();
    let mut n2 = nic();
    n2.addr = Bdf::new(0, 4, 0);
    let d1 = r.push(n2).unwrap();
    assert!(r.register(&STATELESS));
    r.bind_all(|_| {});
    let i0 = r.instance(&d0).unwrap();
    let i1 = r.instance(&d1).unwrap();
    let c0 = i0.downcast_ref::<Counted>().unwrap();
    let c1 = i1.downcast_ref::<Counted>().unwrap();
    assert_eq!((c0.0, c1.0), (d0.id(), d1.id()));
    assert!(!core::ptr::eq(c0, c1));
    assert!(!same_instance(&i0, &i1));
    assert!(same_instance(&i0, &r.instance(&d0).unwrap()));
    drop((i0, i1));
    assert_eq!(DROPS.load(Ordering::SeqCst), 0);
    drop(r);
    assert_eq!(DROPS.load(Ordering::SeqCst), 2);
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
    let mut r = Registry::new();
    let d = r.push(d).unwrap();
    assert!(r.bound(&d).is_none());
    assert!(r.instance(&d).is_none());
    assert_eq!(r.state(&d), Some(DevState::Present));
    assert!(r.parent(&d).is_none());
}

#[test]
fn state_moves_through_probe_and_remove() {
    let mut r = Registry::new();
    let bridge = r.push(vga()).unwrap();
    let id = r.take_id();
    let d = DevRef::try_new(id, nic()).unwrap();
    r.insert(d.clone(), Some(bridge.id())).unwrap();
    assert!(r.parent(&d).unwrap().same(&bridge));
    assert_eq!(r.index_of(&d), Some(1));
    assert!(r.register(&PLAIN));
    assert!(r.register(&VGA));
    let di = (0..r.driver_count())
        .find(|&i| r.driver_at(i).unwrap().name() == "plain")
        .unwrap() as u8;
    // Only a `Present` device starts a probe, and only once.
    let drv = r.begin_probe(&d, di).unwrap();
    assert_eq!(r.state(&d), Some(DevState::Probing));
    assert!(r.begin_probe(&d, di).is_none());
    assert!(r.bound(&d).is_none());
    // A failed probe returns it to `Present`.
    r.abort_probe(&d);
    assert_eq!(r.state(&d), Some(DevState::Present));
    // Both devices are `Present`, so both are jobs again.
    let mut jobs: [Option<(u8, DevRef)>; MAX_DEVICES] = [const { None }; MAX_DEVICES];
    assert_eq!(r.collect_bind_jobs(&mut jobs), 2);
    drop(jobs);
    // A probe binds only under the driver that began it.
    assert!(r.begin_probe(&d, di).is_some());
    assert!(r.bind(&d, "vga", None).is_err());
    let inst = drv.probe(&d).unwrap();
    assert!(r.bind(&d, "plain", inst).is_ok());
    assert_eq!(r.state(&d), Some(DevState::Bound));
    assert_eq!(r.bound(&d), Some("plain"));
    let mut jobs: [Option<(u8, DevRef)>; MAX_DEVICES] = [const { None }; MAX_DEVICES];
    assert_eq!(r.collect_bind_jobs(&mut jobs), 1);
    drop(jobs);
    // Removal hands back the instance and leaves the device `Present`.
    assert!(r.finish_remove(&d).is_none());
    assert_eq!(r.begin_remove(&d).unwrap().name(), "plain");
    assert_eq!(r.state(&d), Some(DevState::Removing));
    assert!(r.bound(&d).is_none());
    assert!(r.begin_remove(&d).is_none());
    assert!(r.finish_remove(&d).is_some());
    assert_eq!(r.state(&d), Some(DevState::Present));
    assert!(r.instance(&d).is_none());
    assert_eq!(DevState::Removing.name(), "removing");
}

#[test]
fn overlaps_any_edges() {
    // Adjacent ranges do not overlap; one shared byte does.
    assert!(!overlaps_any(0x1000, 0x1000, once(0x2000..0x3000)));
    assert!(!overlaps_any(0x2000, 0x1000, once(0x1000..0x2000)));
    assert!(overlaps_any(0x1FFF, 2, once(0x2000..0x3000)));
    assert!(overlaps_any(0x2FFF, 1, once(0x2000..0x3000)));
    // Zero length on either side overlaps nothing.
    assert!(!overlaps_any(0x2000, 0, once(0x1000..0x3000)));
    assert!(!overlaps_any(0x2000, 0x10, once(0x2000..0x2000)));
    // A range ending at u64::MAX saturates rather than wrapping.
    assert!(overlaps_any(
        u64::MAX - 0xF,
        0x100,
        once(u64::MAX - 1..u64::MAX)
    ));
    assert!(!overlaps_any(u64::MAX - 0xF, 0x100, once(0..0x1000)));
    assert!(overlaps_any(0, u64::MAX, once(u64::MAX - 1..u64::MAX)));
    // Any of several.
    assert!(overlaps_any(0x5000, 0x10, [0..0x1000, 0x4000..0x6000]));
    assert!(!overlaps_any(0x3000, 0x10, [0..0x1000, 0x4000..0x6000]));
    assert!(!overlaps_any(0x3000, 0x10, core::iter::empty()));
}

#[test]
fn parent_bridge_behind_bridge() {
    let mut host = FuncInfo::empty();
    host.bdf = Bdf::new(0, 0, 0);
    let mut bridge = FuncInfo::empty();
    bridge.bdf = Bdf::new(0, 0x1e, 0);
    bridge.header = 0x01;
    bridge.secondary_bus = 1;
    let mut child = FuncInfo::empty();
    child.bdf = Bdf::new(1, 2, 0);
    let mut bridge2 = FuncInfo::empty();
    bridge2.bdf = Bdf::new(1, 3, 0);
    bridge2.header = 0x01;
    bridge2.secondary_bus = 2;
    let mut grandchild = FuncInfo::empty();
    grandchild.bdf = Bdf::new(2, 0, 0);
    let mut orphan = FuncInfo::empty();
    orphan.bdf = Bdf::new(5, 0, 0);
    let funcs = [host, bridge, child, bridge2, grandchild, orphan];
    assert_eq!(parent_bridge(&funcs, 0), None);
    assert_eq!(parent_bridge(&funcs, 1), None);
    assert_eq!(parent_bridge(&funcs, 2), Some(1));
    assert_eq!(parent_bridge(&funcs, 3), Some(1));
    assert_eq!(parent_bridge(&funcs, 4), Some(3));
    assert_eq!(parent_bridge(&funcs, 5), None);
    assert_eq!(parent_bridge(&funcs, 6), None);
}

#[test]
fn fixed_tables_match_limits() {
    let r = Registry::new();
    assert_eq!(r.drivers.len(), crate::limits::MAX_DRIVERS);
    assert_eq!(r.claims.len(), crate::limits::MAX_CLAIMS);
}
