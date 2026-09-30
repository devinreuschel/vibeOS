//! Device registry instance and bind. ROADMAP §6.1. The `lspci` and
//! `devices` commands are in `shell::cmds::dev`.
//!
//! The registry holds counted entries ([`DevRef`]) and the instances their
//! drivers returned (DESIGN §12.1 rule 1). Nothing is allocated under
//! [`REG`]: [`push`] builds its entry before it takes the lock, and every
//! reference a lookup returns is a clone the caller drops unlocked.
//!
//! [`bind_all`] is the kernel's binder. It probes one device at a time
//! under that device's sleeping lock ([`DEV_LOCKS`], DESIGN §12.1 rule 3),
//! taken with no other lock held and never under [`REG`], which it drops
//! across the driver's callback.

use vibeos::dev::{
    BarClaim, ClaimError, DevRef, DevState, Device, Driver, Instance, MAX_DEVICES, Registry,
};
use vibeos::kalloc::AllocError;
use vibeos::lock::RANK_DEVICE;
use vibeos::log::Level;
use vibeos::pci::Bdf;

use crate::boot;
use crate::sync::blocking_init::BlockingMutex;
use crate::sync_init::SpinMutex;

/// The device registry. `dev::ktest` reads it for its hooks.
pub(super) static REG: SpinMutex<Registry> = SpinMutex::with_rank(Registry::new(), RANK_DEVICE);

/// One sleeping lock per registry slot, by slot index: it serializes
/// `probe` and `remove` on that device (DESIGN §12.1 rule 3). Taken with
/// IF=1 and no spinlock held, before [`REG`], never under it.
static DEV_LOCKS: [BlockingMutex<()>; MAX_DEVICES] =
    [const { BlockingMutex::new(()) }; MAX_DEVICES];

/// `dev`'s lock; `None` for a device not in the table.
fn dev_lock(dev: &DevRef) -> Option<&'static BlockingMutex<()>> {
    let i = REG.lock().index_of(dev)?;
    DEV_LOCKS.get(i)
}

/// Register `d` under a fresh id, behind the device with id `parent` (its
/// bridge or root port). `AllocError` when the heap or the table is full.
pub fn push(d: Device, parent: Option<u64>) -> Result<DevRef, AllocError> {
    let id = REG.lock().take_id();
    let r = DevRef::try_new(id, d)?;
    // A clone goes in, so a refused insert drops only a count under the
    // lock, and `r`'s own count after it.
    REG.lock().insert(r.clone(), parent)?;
    Ok(r)
}

pub fn register_driver(drv: &'static dyn Driver) -> bool {
    REG.lock().register(drv)
}

/// Probe each `Present` device a registered driver matches, in
/// [`Driver::order`]. Each probe runs under the device's lock with [`REG`]
/// dropped: the device is `Probing` meanwhile, then `Bound` to the driver
/// and owning the instance it returned, or `Present` again on an error.
pub fn bind_all() {
    let mut jobs: [Option<(u8, DevRef)>; MAX_DEVICES] = [const { None }; MAX_DEVICES];
    let n = REG.lock().collect_bind_jobs(&mut jobs);
    for (drv_i, dev) in jobs.iter().take(n).flatten() {
        let Some(lock) = dev_lock(dev) else {
            continue;
        };
        let _serial = lock.lock();
        // A device bound since the jobs were collected is no longer
        // `Present`, and is left alone.
        let Some(drv) = REG.lock().begin_probe(dev, *drv_i) else {
            continue;
        };
        match drv.probe(dev) {
            Ok(inst) => {
                // `Probing` under this driver, which only this lock's
                // holder moves on: it binds. Were it refused, the instance
                // comes back and is dropped with the table unlocked.
                let refused = REG.lock().bind(dev, drv.name(), inst);
                drop(refused);
            }
            // The device is `Present` again, for a later driver.
            Err(e) => {
                REG.lock().abort_probe(dev);
                crate::klog!(
                    Level::Warn,
                    "vibeOS: dev: probe {} {} {:04x}:{:04x} failed: {}",
                    drv.name(),
                    dev.addr,
                    dev.vendor,
                    dev.device_id,
                    e.as_str()
                )
            }
        }
    }
}

/// A reference to device `i`, in registration order.
pub fn get(i: usize) -> Option<DevRef> {
    REG.lock().get(i)
}

/// The device at `bdf`.
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "C-INSTANCES lookup: ROADMAP §10.12's claims (P10-S96) look a device up by address"
    )
)]
pub fn find_bdf(bdf: Bdf) -> Option<DevRef> {
    let mut i = 0usize;
    while let Some(d) = get(i) {
        if d.addr == bdf {
            return Some(d);
        }
        i += 1;
    }
    None
}

/// The first device with `vendor:device`.
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "only the in-guest tests look a device up by id yet"
    )
)]
pub fn find_id(vendor: u16, device: u16) -> Option<DevRef> {
    let mut i = 0usize;
    while let Some(d) = get(i) {
        if d.vendor == vendor && d.device_id == device {
            return Some(d);
        }
        i += 1;
    }
    None
}

/// `dev`'s state (DESIGN §12.1 rule 3).
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "only the in-guest tests read a device's state until ROADMAP §20.9's removal"
    )
)]
pub fn state(dev: &DevRef) -> Option<DevState> {
    REG.lock().state(dev)
}

/// `dev`'s parent bridge or root port.
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "only the in-guest tests read a parent until DESIGN §12.2's parent-first order (ROADMAP §20.x)"
    )
)]
pub fn parent(dev: &DevRef) -> Option<DevRef> {
    REG.lock().parent(dev)
}

/// The name of the driver bound to `dev`.
pub fn bound(dev: &DevRef) -> Option<&'static str> {
    REG.lock().bound(dev)
}

/// A reference to `dev`'s driver instance.
pub fn instance(dev: &DevRef) -> Option<Instance> {
    REG.lock().instance(dev)
}

/// Claim BAR `bar` of `dev`, checked against every other claim and the
/// boot memory map's RAM-typed ranges with [`REG`] held (DESIGN §12.3
/// rule 8).
pub fn claim(dev: &DevRef, bar: u8) -> Result<BarClaim, ClaimError> {
    REG.lock().claim(dev, bar, boot::info().ram_ranges())
}

/// Give `claim` to its device's entry, mapped at `va`; the claim comes
/// back when the entry already holds that BAR.
pub fn hold(claim: BarClaim, va: Option<u64>) -> Result<(), BarClaim> {
    REG.lock().hold(claim, va)
}

/// End `claim`, after any mapping made through it is gone.
pub fn release(claim: BarClaim) {
    REG.lock().release(claim)
}

/// Whether BAR `bar` of `dev` is claimed.
pub fn is_claimed(dev: &DevRef, bar: u8) -> bool {
    REG.lock().is_claimed(dev, bar)
}

/// Bind any drivers already registered.
pub fn init() {
    bind_all();
}
