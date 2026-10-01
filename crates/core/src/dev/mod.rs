//! Device model and driver registry. ROADMAP §6.1.
//!
//! Scan fills a device list; the kernel's binder, `dev_init::bind_all`,
//! matches drivers by id and probes in dependency order, one device at a
//! time under that device's lock. [`Registry::bind_all`] is the host
//! tests' binder.
//!
//! Each device is a counted entry ([`DevRef`], DESIGN §12.1 rule 1): its
//! record never changes after [`Registry::push`], and the registry slot
//! owns what changes: its parent, its [`DevState`], the bound driver, the
//! driver's per-device state ([`Instance`]) and its BARs' claims.
//!
//! A BAR is claimed through [`Registry::claim`], which refuses a range
//! that overlaps another claim or RAM, and mapped only by the driver that
//! holds its [`BarClaim`], in `probe` (DESIGN §12.3 rule 8): nothing maps
//! a BAR at enumeration.

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
}

impl Resource {
    pub const EMPTY: Self = Self {
        kind: ResourceKind::Empty,
        bar: 0,
        addr: 0,
        size: 0,
        prefetchable: false,
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

/// A probe's errno: Linux's for each condition.
impl From<ProbeError> for crate::kerror::KError {
    fn from(e: ProbeError) -> Self {
        match e {
            ProbeError::Busy => Self::Busy,
            ProbeError::NoResource => Self::NoDev,
            ProbeError::Failed => Self::Io,
            ProbeError::NoMemory => Self::NoMem,
        }
    }
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

/// A claimed resource, or a BAR over RAM, is busy; an empty slot is no
/// device; a bad index, a bad argument; a full claims table, no memory.
impl From<ClaimError> for crate::kerror::KError {
    fn from(e: ClaimError) -> Self {
        match e {
            ClaimError::Empty => Self::NoDev,
            ClaimError::Already | ClaimError::Overlap | ClaimError::Ram => Self::Busy,
            ClaimError::BadIndex => Self::Inval,
            ClaimError::Full => Self::NoMem,
        }
    }
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
    /// The claiming device's address, for the lines its mapping logs.
    at: Bdf,
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

    /// The claiming device's bus address.
    pub fn bdf(&self) -> Bdf {
        self.at
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

/// Where a device is in its life (DESIGN §12.1 rule 3). The binder moves
/// it `Present` → `Probing` → `Bound`, or back to `Present` on a failed
/// probe; a removal moves it `Bound` → `Removing` → `Present`. `Resetting`,
/// `Failed`, `Suspended` and `Dead` belong to later phases' error handling,
/// suspend and removal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevState {
    Present,
    Probing,
    Bound,
    Resetting,
    Failed,
    Suspended,
    Removing,
    Dead,
}

impl DevState {
    pub fn name(self) -> &'static str {
        match self {
            DevState::Present => "present",
            DevState::Probing => "probing",
            DevState::Bound => "bound",
            DevState::Resetting => "resetting",
            DevState::Failed => "failed",
            DevState::Suspended => "suspended",
            DevState::Removing => "removing",
            DevState::Dead => "dead",
        }
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
    /// The id of the bridge or root port the device sits behind; `None`
    /// on the root bus.
    parent: Option<u64>,
    state: DevState,
    /// The driver's slot in `drivers` while the device is `Probing`,
    /// `Bound` or `Removing`. An index, not the name, keeps the slot small.
    drv: Option<u8>,
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

    /// Publish `dev`, built under an id [`take_id`](Self::take_id) gave,
    /// behind the device with id `parent`. `AllocError` when the table is
    /// full; `dev` is dropped here then, so a caller passes a clone.
    pub fn insert(&mut self, dev: DevRef, parent: Option<u64>) -> Result<(), AllocError> {
        let slot = self.slots.get_mut(self.n_dev).ok_or(AllocError)?;
        *slot = Some(Slot {
            dev,
            bars: [const { None }; MAX_BARS],
            parent,
            state: DevState::Present,
            drv: None,
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
        self.insert(r.clone(), None)?;
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

    /// The index of `dev`'s slot, in registration order; it never moves.
    pub fn index_of(&self, dev: &DevRef) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.as_ref().is_some_and(|s| s.dev.same(dev)))
    }

    /// `dev`'s parent bridge or root port.
    pub fn parent(&self, dev: &DevRef) -> Option<DevRef> {
        self.slot_of(dev)
            .and_then(|s| s.parent)
            .and_then(|id| self.by_id(id))
    }

    /// `dev`'s state; `None` for a device not in the table.
    pub fn state(&self, dev: &DevRef) -> Option<DevState> {
        self.slot_of(dev).map(|s| s.state)
    }

    /// The name of the driver bound to `dev`.
    pub fn bound(&self, dev: &DevRef) -> Option<&'static str> {
        self.slot_of(dev)
            .filter(|s| s.state == DevState::Bound)
            .and_then(|s| s.drv)
            .and_then(|i| self.driver_name(i))
    }

    /// Record the registered driver `name` as `dev`'s, owning `inst`. Only
    /// a `Present` device, or one `Probing` with that driver, binds;
    /// otherwise `inst` comes back, for the caller to drop unlocked.
    pub fn bind(
        &mut self,
        dev: &DevRef,
        name: &'static str,
        inst: Option<Instance>,
    ) -> Result<(), Option<Instance>> {
        let Some(di) = (0..self.n_drv).find(|&i| self.driver_name(i as u8) == Some(name)) else {
            return Err(inst);
        };
        let di = di as u8;
        match self.slot_of_mut(dev) {
            Some(s)
                if s.state == DevState::Present
                    || (s.state == DevState::Probing && s.drv == Some(di)) =>
            {
                s.state = DevState::Bound;
                s.drv = Some(di);
                s.inst = inst;
                Ok(())
            }
            _ => Err(inst),
        }
    }

    /// Move a `Present` `dev` to `Probing` by the driver in slot `drv`,
    /// returning that driver; `None`, and no change, otherwise.
    pub fn begin_probe(&mut self, dev: &DevRef, drv: u8) -> Option<&'static dyn Driver> {
        let d = self.driver_at(drv as usize)?;
        let s = self.slot_of_mut(dev)?;
        if s.state != DevState::Present {
            return None;
        }
        s.state = DevState::Probing;
        s.drv = Some(drv);
        Some(d)
    }

    /// Return a `Probing` `dev` to `Present` after its probe failed.
    pub fn abort_probe(&mut self, dev: &DevRef) {
        if let Some(s) = self.slot_of_mut(dev)
            && s.state == DevState::Probing
        {
            s.state = DevState::Present;
            s.drv = None;
        }
    }

    /// Move a `Bound` `dev` to `Removing`, returning its driver.
    pub fn begin_remove(&mut self, dev: &DevRef) -> Option<&'static dyn Driver> {
        let s = self.slot_of(dev)?;
        if s.state != DevState::Bound {
            return None;
        }
        let d = s.drv.and_then(|i| self.driver_at(i as usize))?;
        let s = self.slot_of_mut(dev)?;
        s.state = DevState::Removing;
        Some(d)
    }

    /// Return a `Removing` `dev` to `Present`, handing back its driver's
    /// instance for the caller to drop with the table unlocked.
    pub fn finish_remove(&mut self, dev: &DevRef) -> Option<Instance> {
        let s = self.slot_of_mut(dev)?;
        if s.state != DevState::Removing {
            return None;
        }
        s.state = DevState::Present;
        s.drv = None;
        s.inst.take()
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
            at: dev.addr,
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
                if s.state == DevState::Present && s.dev.matches_driver(drv) {
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
                    .filter(|s| s.state == DevState::Present && s.dev.matches_driver(drv))
                    .map(|s| s.dev.clone());
                if let Some(dev) = job {
                    enable(&dev);
                    // A failed probe leaves the device unbound for a later
                    // driver; the kernel's `dev_init::bind_all` also logs it.
                    if let Ok(inst) = drv.probe(&dev) {
                        // A `Present` device and a registered driver: it
                        // binds, so nothing comes back.
                        let refused = self.bind(&dev, drv.name(), inst);
                        drop(refused);
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
            let bound = s
                .drv
                .filter(|_| s.state == DevState::Bound)
                .and_then(|i| self.driver_name(i));
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
mod tests;
