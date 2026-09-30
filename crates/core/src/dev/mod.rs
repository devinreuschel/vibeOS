//! Device model and driver registry. ROADMAP §6.1.
//!
//! Scan fills a device list; [`Registry::bind_all`] matches drivers by
//! id and probes in dependency order. Resource claims are exclusive.
//!
//! Each device is a counted entry ([`DevRef`], DESIGN §12.1 rule 1): its
//! record never changes after [`Registry::push`], and the registry slot
//! owns what changes, the claims, the bound driver's name and the
//! driver's per-device state ([`Instance`]).

pub mod dma;
pub mod entropy;
pub mod pci;
pub mod virtio;

use core::any::Any;
use core::fmt;
use core::ops::{Deref, Range};

use crate::kalloc::{AllocError, TryArc};
use crate::pci::{Bar, BarKind, Bdf, CapSet, FuncInfo, MAX_BARS, MAX_SCAN};

pub const MAX_DEVICES: usize = MAX_SCAN;
pub use crate::limits::MAX_CLAIMS;
pub use crate::limits::MAX_DRIVERS;

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
        if let Some(c) = self.class
            && c != class
        {
            return false;
        }
        if let Some(s) = self.subclass
            && s != subclass
        {
            return false;
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
}

impl Resource {
    pub const EMPTY: Self = Self {
        kind: ResourceKind::Empty,
        bar: 0,
        addr: 0,
        size: 0,
        prefetchable: false,
        mapped_va: 0,
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

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeError {
    Busy,
    NoResource,
    Failed,
    /// An allocation the probe needs failed (DESIGN §4.4).
    NoMemory,
}

impl ProbeError {
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeError::Busy => "busy",
            ProbeError::NoResource => "no resource",
            ProbeError::Failed => "failed",
            ProbeError::NoMemory => "no memory",
        }
    }
}

impl From<crate::kalloc::AllocError> for ProbeError {
    fn from(_: crate::kalloc::AllocError) -> Self {
        ProbeError::NoMemory
    }
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimError {
    Empty,
    Already,
    Overlap,
    BadIndex,
    /// A memory BAR overlaps a RAM-typed range of the boot memory map.
    Ram,
    /// The claims table is full.
    Full,
}

impl ClaimError {
    pub fn as_str(self) -> &'static str {
        match self {
            ClaimError::Empty => "empty",
            ClaimError::Already => "already claimed",
            ClaimError::Overlap => "overlap",
            ClaimError::BadIndex => "bad index",
            ClaimError::Ram => "ram",
            ClaimError::Full => "table full",
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
    /// Bind `dev`. `Ok(Some(inst))` hands the driver's per-device state
    /// to the device's registry slot, which owns it from then on; a driver
    /// keeps no list of its devices (DESIGN §12.1 rule 1).
    fn probe(&self, dev: &DevRef) -> Result<Option<Instance>, ProbeError>;
    fn remove(&self, dev: &DevRef);
}

/// A driver's per-device state, type-erased: the one owner type a
/// registry slot holds. Built with `TryArc::try_new_unsize(v, |p| p)`;
/// every holder clones this value and downcasts with `downcast_ref`.
pub type Instance = TryArc<dyn Any + Send + Sync>;

/// Wrap `v` as an [`Instance`].
pub fn instance<V: Any + Send + Sync>(v: V) -> Result<Instance, AllocError> {
    TryArc::<dyn Any + Send + Sync>::try_new_unsize(v, |p| p)
}

/// Whether `a` and `b` are the same instance.
pub fn same_instance(a: &Instance, b: &Instance) -> bool {
    core::ptr::addr_eq(&**a as *const (dyn Any + Send + Sync), &**b)
}

/// Whether `[addr, addr + len)` overlaps any of `ranges`. Ends saturate
/// at `u64::MAX`, and an empty range overlaps nothing. The one overlap
/// check (AGENTS.md rule 10): [`Registry::claim`] runs it against other
/// claims and RAM, and the kernel's `pci_init::map_mmio` against RAM.
pub fn overlaps_any(addr: u64, len: u64, ranges: impl IntoIterator<Item = Range<u64>>) -> bool {
    if len == 0 {
        return false;
    }
    let end = addr.saturating_add(len);
    ranges
        .into_iter()
        .any(|r| r.start < r.end && addr < r.end && r.start < end)
}

/// The index in `funcs` of the bridge whose secondary bus is the bus of
/// `funcs[i]`: that function's parent. `None` on the root bus, or when no
/// scanned bridge leads to it.
pub fn parent_bridge(funcs: &[FuncInfo], i: usize) -> Option<usize> {
    let bus = funcs.get(i)?.bdf.bus;
    funcs
        .iter()
        .position(|f| f.is_bridge() && f.secondary_bus != 0 && f.secondary_bus == bus)
}

/// A PCI function's record. Not `Copy` or `Clone`: the registry holds the
/// one copy behind a [`DevRef`] (DESIGN §12.1 rule 1).
///
/// ```compile_fail
/// let a = vibeos::dev::Device::empty();
/// let b = a;
/// let c = a; // `Device` is not `Copy`
/// ```
#[derive(Debug)]
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

    /// The `devices` line of this record, bound to `bound`, with the BARs
    /// `claimed` marks.
    pub fn write_tree(
        &self,
        bound: Option<&str>,
        claimed: &[bool; MAX_BARS],
        f: &mut impl fmt::Write,
    ) -> fmt::Result {
        let drv = bound.unwrap_or("-");
        write!(
            f,
            "{} {:04x}:{:04x} {} drv={drv}",
            self.addr,
            self.vendor,
            self.device_id,
            pci::class_name(self.class, self.subclass)
        )?;
        if let Some(n) = pci::friendly_name(self.vendor, self.device_id) {
            write!(f, " [{n}]")?;
        }
        writeln!(f)?;
        let mut b = 0usize;
        while b < MAX_BARS {
            let r = self.resources[b];
            if !r.is_empty() {
                writeln!(
                    f,
                    "  bar{} {} {:#x}/{:#x}{}",
                    r.bar,
                    r.kind.name(),
                    r.addr,
                    r.size,
                    if claimed[b] { " claimed" } else { "" }
                )?;
            }
            b += 1;
        }
        Ok(())
    }
}

/// A device's record under the id it was registered with. Immutable after
/// [`Registry::push`]; shared through [`DevRef`].
pub struct DevEntry {
    id: u64,
    info: Device,
}

/// A counted reference to a registered device. Its id is never reused
/// within a boot. Derefs to the device's record.
#[derive(Clone)]
pub struct DevRef(TryArc<DevEntry>);

impl DevRef {
    /// Build an unregistered entry for `info` under `id`.
    pub fn try_new(id: u64, info: Device) -> Result<Self, AllocError> {
        TryArc::try_new(DevEntry { id, info }).map(DevRef)
    }

    pub fn id(&self) -> u64 {
        self.0.id
    }

    /// Whether `other` names the same entry.
    pub fn same(&self, other: &DevRef) -> bool {
        self.0.id == other.0.id
    }
}

impl Deref for DevRef {
    type Target = Device;
    fn deref(&self) -> &Device {
        &self.0.info
    }
}

impl fmt::Debug for DevRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DevRef")
            .field("id", &self.0.id)
            .field("addr", &self.0.info.addr)
            .finish()
    }
}

/// A claim on one BAR of one device, which [`Registry::claim`] grants and
/// only [`Registry::release`] ends (DESIGN §12.3 rule 8). Not `Copy` or
/// `Clone` (AGENTS.md rule 6), with no public constructor, so a mapping
/// made through it names a range the claims table holds. Dropping one
/// without `release` keeps its range reserved, which fails safe.
///
/// ```compile_fail
/// fn gone(c: vibeos::dev::BarClaim) -> u64 {
///     let moved = c;
///     drop(moved);
///     c.phys() // `c` was moved
/// }
/// ```
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub struct BarClaim {
    dev: u64,
    bar: u8,
    mem: bool,
    addr: u64,
    size: u64,
    prefetchable: bool,
}

impl BarClaim {
    /// The id of the claiming device's entry ([`DevRef::id`]).
    pub fn dev(&self) -> u64 {
        self.dev
    }

    pub fn bar(&self) -> u8 {
        self.bar
    }

    /// The BAR's bus address.
    pub fn phys(&self) -> u64 {
        self.addr
    }

    pub fn len(&self) -> u64 {
        self.size
    }

    /// Never true: [`Registry::claim`] refuses an empty BAR.
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    pub fn is_mem(&self) -> bool {
        self.mem
    }

    pub fn prefetchable(&self) -> bool {
        self.prefetchable
    }
}

/// A row of the claims table: which device's BAR holds which range.
#[derive(Clone, Copy)]
struct Claim {
    dev: u64,
    bar: u8,
    mem: bool,
    addr: u64,
    size: u64,
}

/// A claim the device's entry holds, with the VA its driver mapped it at.
struct HeldBar {
    claim: BarClaim,
    va: Option<u64>,
}

/// A registry slot: the entry, and what changes about it after `push`.
struct Slot {
    dev: DevRef,
    /// Each BAR's claim, once its driver holds it.
    bars: [Option<HeldBar>; MAX_BARS],
    /// The bound driver's slot in `drivers`; set only while unbound. An
    /// index, not the name, keeps the table under 4 KiB.
    bound: Option<u8>,
    /// The bound driver's per-device state, owned here.
    inst: Option<Instance>,
}

/// The device table. It holds counted entries and the instances their
/// drivers returned, never a record by value.
pub struct Registry {
    slots: [Option<Slot>; MAX_DEVICES],
    n_dev: usize,
    /// The next entry's id; ids start at 1 and never repeat.
    next_id: u64,
    drivers: [Option<&'static dyn Driver>; MAX_DRIVERS],
    n_drv: usize,
    /// Every live claim; a released row is reused.
    claims: [Option<Claim>; MAX_CLAIMS],
}

impl Registry {
    pub const fn new() -> Self {
        Self {
            slots: [const { None }; MAX_DEVICES],
            n_dev: 0,
            next_id: 1,
            drivers: [None; MAX_DRIVERS],
            n_drv: 0,
            claims: [None; MAX_CLAIMS],
        }
    }

    pub fn len(&self) -> usize {
        self.n_dev
    }

    pub fn is_empty(&self) -> bool {
        self.n_dev == 0
    }

    pub fn driver_count(&self) -> usize {
        self.n_drv
    }

    /// Take the next entry id. Ids are handed out once each, so an entry
    /// built outside the table's lock with one is still unique.
    pub fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// Publish `dev`, built under an id [`take_id`](Self::take_id) gave.
    /// `AllocError` when the table is full; `dev` is dropped here then, so
    /// a caller passes a clone.
    pub fn insert(&mut self, dev: DevRef) -> Result<(), AllocError> {
        let slot = self.slots.get_mut(self.n_dev).ok_or(AllocError)?;
        *slot = Some(Slot {
            dev,
            bars: [const { None }; MAX_BARS],
            bound: None,
            inst: None,
        });
        self.n_dev += 1;
        Ok(())
    }

    /// Register `d` under a fresh id. Allocates: the kernel takes an id
    /// under its lock, builds the entry outside it and [`insert`]s it.
    ///
    /// [`insert`]: Self::insert
    pub fn push(&mut self, d: Device) -> Result<DevRef, AllocError> {
        if self.n_dev >= MAX_DEVICES {
            return Err(AllocError);
        }
        let r = DevRef::try_new(self.take_id(), d)?;
        self.insert(r.clone())?;
        Ok(r)
    }

    fn slot(&self, i: usize) -> Option<&Slot> {
        self.slots.get(i).and_then(Option::as_ref)
    }

    fn slot_of(&self, dev: &DevRef) -> Option<&Slot> {
        self.slots.iter().flatten().find(|s| s.dev.same(dev))
    }

    fn slot_of_mut(&mut self, dev: &DevRef) -> Option<&mut Slot> {
        self.slots.iter_mut().flatten().find(|s| s.dev.same(dev))
    }

    /// A reference to device `i`, in registration order.
    pub fn get(&self, i: usize) -> Option<DevRef> {
        self.slot(i).map(|s| s.dev.clone())
    }

    /// A reference to the device with id `id`.
    pub fn by_id(&self, id: u64) -> Option<DevRef> {
        self.slots
            .iter()
            .flatten()
            .find(|s| s.dev.id() == id)
            .map(|s| s.dev.clone())
    }

    fn driver_name(&self, i: u8) -> Option<&'static str> {
        self.driver_at(i as usize).map(|d| d.name())
    }

    /// The name of the driver bound to `dev`.
    pub fn bound(&self, dev: &DevRef) -> Option<&'static str> {
        self.slot_of(dev)
            .and_then(|s| s.bound)
            .and_then(|i| self.driver_name(i))
    }

    /// Record the registered driver `name` as `dev`'s, owning `inst`. Only
    /// an unbound device binds, to a registered driver; `false` otherwise,
    /// and `inst` is dropped.
    pub fn bind(&mut self, dev: &DevRef, name: &'static str, inst: Option<Instance>) -> bool {
        let Some(di) = (0..self.n_drv).find(|&i| self.driver_name(i as u8) == Some(name)) else {
            return false;
        };
        match self.slot_of_mut(dev) {
            Some(s) if s.bound.is_none() => {
                s.bound = Some(di as u8);
                s.inst = inst;
                true
            }
            _ => false,
        }
    }

    /// A reference to `dev`'s driver instance.
    pub fn instance(&self, dev: &DevRef) -> Option<Instance> {
        self.slot_of(dev).and_then(|s| s.inst.clone())
    }

    /// Whether the claims table holds BAR `bar` of `dev`.
    pub fn is_claimed(&self, dev: &DevRef, bar: u8) -> bool {
        self.claims
            .iter()
            .flatten()
            .any(|c| c.dev == dev.id() && c.bar == bar)
    }

    pub fn register(&mut self, drv: &'static dyn Driver) -> bool {
        if self.n_drv >= MAX_DRIVERS {
            return false;
        }
        let mut i = 0usize;
        while i < self.n_drv {
            if let Some(d) = self.drivers[i]
                && d.name() == drv.name()
            {
                return false;
            }
            i += 1;
        }
        self.drivers[self.n_drv] = Some(drv);
        self.n_drv += 1;
        true
    }

    /// Claim BAR `bar` of `dev` for its driver. Refused, in this order:
    /// `BadIndex` (no such BAR or entry), `Empty`, `Already` (the table
    /// holds this BAR), `Ram` (a memory BAR over any of `ram`, the boot
    /// memory map's RAM-typed ranges), `Overlap` (a claim of the same
    /// kind), `Full`. The kernel calls it with its table locked, so the
    /// check and the record are one step (DESIGN §12.3 rule 8).
    pub fn claim(
        &mut self,
        dev: &DevRef,
        bar: u8,
        ram: impl IntoIterator<Item = Range<u64>>,
    ) -> Result<BarClaim, ClaimError> {
        let Some(s) = self.slot_of(dev) else {
            return Err(ClaimError::BadIndex);
        };
        let Some(&res) = s.dev.resources.get(bar as usize) else {
            return Err(ClaimError::BadIndex);
        };
        if res.is_empty() {
            return Err(ClaimError::Empty);
        }
        let id = dev.id();
        if self.is_claimed(dev, bar) {
            return Err(ClaimError::Already);
        }
        let mem = matches!(res.kind, ResourceKind::Memory);
        if mem && overlaps_any(res.addr, res.size, ram) {
            return Err(ClaimError::Ram);
        }
        let taken = self
            .claims
            .iter()
            .flatten()
            .filter(|c| c.mem == mem)
            .map(|c| c.addr..c.addr.saturating_add(c.size));
        if overlaps_any(res.addr, res.size, taken) {
            return Err(ClaimError::Overlap);
        }
        let row = self
            .claims
            .iter_mut()
            .find(|c| c.is_none())
            .ok_or(ClaimError::Full)?;
        *row = Some(Claim {
            dev: id,
            bar,
            mem,
            addr: res.addr,
            size: res.size,
        });
        Ok(BarClaim {
            dev: id,
            bar,
            mem,
            addr: res.addr,
            size: res.size,
            prefetchable: res.prefetchable,
        })
    }

    /// End `claim`: its row is free for the next claim.
    pub fn release(&mut self, claim: BarClaim) {
        for row in self.claims.iter_mut() {
            if row.is_some_and(|c| c.dev == claim.dev && c.bar == claim.bar) {
                *row = None;
            }
        }
    }

    /// Give `claim` to its device's entry, mapped at `va`. Refused, with
    /// the claim handed back, when the entry is gone or already holds that
    /// BAR.
    pub fn hold(&mut self, claim: BarClaim, va: Option<u64>) -> Result<(), BarClaim> {
        let id = claim.dev;
        let Some(held) = self
            .slots
            .iter_mut()
            .flatten()
            .find(|s| s.dev.id() == id)
            .and_then(|s| s.bars.get_mut(claim.bar as usize))
        else {
            return Err(claim);
        };
        if held.is_some() {
            return Err(claim);
        }
        *held = Some(HeldBar { claim, va });
        Ok(())
    }

    /// The VA BAR `bar` of `dev` is mapped at, while its entry holds it.
    pub fn bar_va(&self, dev: &DevRef, bar: u8) -> Option<u64> {
        self.slot_of(dev)
            .and_then(|s| s.bars.get(bar as usize))
            .and_then(Option::as_ref)
            .and_then(|h| h.va)
    }

    /// Take BAR `bar`'s claim and VA back from `dev`'s entry, to unmap and
    /// then [`release`](Self::release) it.
    pub fn take_bar(&mut self, dev: &DevRef, bar: u8) -> Option<(BarClaim, Option<u64>)> {
        self.slot_of_mut(dev)
            .and_then(|s| s.bars.get_mut(bar as usize))
            .and_then(Option::take)
            .map(|h| (h.claim, h.va))
    }

    pub fn driver_at(&self, i: usize) -> Option<&'static dyn Driver> {
        self.drivers.get(i).and_then(|d| *d)
    }

    fn sorted_driver_idx(
        &self,
        idx: &mut [u8; MAX_DRIVERS],
        order: &mut [u8; MAX_DRIVERS],
    ) -> usize {
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
        n
    }

    /// `(driver_slot, device)` in bind order. Probe outside the registry
    /// lock: drivers may alloc and take RANK_DEVICE. The caller drops the
    /// references with the lock dropped.
    pub fn collect_bind_jobs(&self, out: &mut [Option<(u8, DevRef)>]) -> usize {
        let mut order = [0u8; MAX_DRIVERS];
        let mut idx = [0u8; MAX_DRIVERS];
        let n = self.sorted_driver_idx(&mut idx, &mut order);
        let mut w = 0usize;
        let mut di = 0usize;
        while di < n && w < out.len() {
            let drv = match self.drivers[idx[di] as usize] {
                Some(d) => d,
                None => {
                    di += 1;
                    continue;
                }
            };
            for s in self.slots.iter().flatten() {
                if w >= out.len() {
                    break;
                }
                if s.bound.is_none() && s.dev.matches_driver(drv) {
                    out[w] = Some((idx[di], s.dev.clone()));
                    w += 1;
                }
            }
            di += 1;
        }
        w
    }

    /// Match unbound devices to drivers (the host binder). `enable` runs
    /// before `probe` (command bits, etc). Drivers are sorted by
    /// [`Driver::order`].
    pub fn bind_all(&mut self, mut enable: impl FnMut(&Device)) {
        let mut order = [0u8; MAX_DRIVERS];
        let mut idx = [0u8; MAX_DRIVERS];
        let n = self.sorted_driver_idx(&mut idx, &mut order);
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
                let job = self
                    .slot(dv)
                    .filter(|s| s.bound.is_none() && s.dev.matches_driver(drv))
                    .map(|s| s.dev.clone());
                if let Some(dev) = job {
                    enable(&dev);
                    // A failed probe leaves the device unbound for a later
                    // driver; the kernel's `dev_init::bind_all` also logs it.
                    if let Ok(inst) = drv.probe(&dev) {
                        self.bind(&dev, drv.name(), inst);
                    }
                }
                dv += 1;
            }
            di += 1;
        }
    }

    /// Each device's `devices` lines, with its binding and claims.
    pub fn write_tree(&self, f: &mut impl fmt::Write) -> fmt::Result {
        for s in self.slots.iter().flatten() {
            let bound = s.bound.and_then(|i| self.driver_name(i));
            let claimed = core::array::from_fn(|b| s.bars.get(b).is_some_and(Option::is_some));
            s.dev.write_tree(bound, &claimed, f)?;
        }
        Ok(())
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
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
            mapped_va: 0,
        };
        d.resources[1] = Resource {
            kind: ResourceKind::Io,
            bar: 1,
            addr: 0xC000,
            size: 0x40,
            prefetchable: false,
            mapped_va: 0,
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
                    mapped_va: 0,
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
        assert_eq!(EARLY.probes.load(Ordering::SeqCst), 1);
        assert_eq!(LATE.probes.load(Ordering::SeqCst), 0);
        assert_eq!(VGA.probes.load(Ordering::SeqCst), 1);
        // A bound device does not bind again.
        assert!(!r.bind(&d0, "late-nic", None));
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
        assert!(reg < 24 * 1024, "Registry {reg} should be under 24 KiB");
        let claim = core::mem::size_of::<BarClaim>();
        assert!(claim <= 32, "BarClaim {claim} should be at most 32 bytes");
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
}
