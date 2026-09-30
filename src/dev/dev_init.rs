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
    BarClaim, ClaimError, DevRef, DevState, Device, Driver, Instance, MAX_DEVICES, ProbeError,
    Registry, ResourceKind,
};
use vibeos::kalloc::AllocError;
use vibeos::lock::RANK_DEVICE;
use vibeos::log::Level;
use vibeos::pci::Bdf;

use crate::boot;
use crate::pci_init;
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
/// and owning the instance it returned, or `Present` again on an error,
/// with any BAR it claimed unmapped and released.
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
                release_bars(dev);
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

/// The VA BAR `bar` of `dev` is mapped at, while its entry holds it.
pub fn bar_va(dev: &DevRef, bar: u8) -> Option<u64> {
    REG.lock().bar_va(dev, bar)
}

/// Claim every non-empty memory BAR of `dev`, map each one under the cap
/// through [`pci_init::map_bar`], and give each claim to `dev`'s entry; a
/// BAR above the cap is held with no VA. A driver's `probe` calls it
/// before it touches its device. On an error nothing this call claimed
/// stays claimed or mapped: `Busy` when another claim holds a range,
/// `NoResource` for RAM, a full table or a failed mapping.
pub fn claim_mem_bars(dev: &DevRef) -> Result<(), ProbeError> {
    let mut bar = 0u8;
    while let Some(r) = dev.resources.get(bar as usize) {
        let this = bar;
        bar = bar.saturating_add(1);
        if r.is_empty() || !matches!(r.kind, ResourceKind::Memory) {
            continue;
        }
        let claimed = match claim(dev, this) {
            Ok(c) => c,
            Err(e) => {
                release_bars(dev);
                return Err(match e {
                    ClaimError::Already | ClaimError::Overlap => ProbeError::Busy,
                    ClaimError::Ram
                    | ClaimError::Full
                    | ClaimError::Empty
                    | ClaimError::BadIndex => ProbeError::NoResource,
                });
            }
        };
        // Mapped with REG dropped: `map_bar` takes the page-table lock. A
        // BAR above the cap is held with no VA, and `map_bar` logs it.
        let va = pci_init::map_bar(&claimed);
        if va.is_none() && vibeos::pci::bar_map_allowed(claimed.len()) {
            release(claimed);
            release_bars(dev);
            return Err(ProbeError::NoResource);
        }
        if let Err(back) = hold(claimed, va) {
            if let Some(va) = va {
                // SAFETY: invariant I484: `va` is where `map_bar` just
                // mapped `back`, and nothing has used it; established
                // here.
                unsafe { pci_init::unmap_bar(&back, va) };
            }
            release(back);
            release_bars(dev);
            return Err(ProbeError::Busy);
        }
    }
    Ok(())
}

/// Unmap each BAR `dev`'s entry holds, then release its claim. A driver's
/// `remove` ends with it, after it stopped the device, and the binder
/// calls it after a failed probe.
pub fn release_bars(dev: &DevRef) {
    let mut bar = 0u8;
    while (bar as usize) < vibeos::pci::MAX_BARS {
        let taken = REG.lock().take_bar(dev, bar);
        bar = bar.saturating_add(1);
        let Some((claim, va)) = taken else {
            continue;
        };
        if let Some(va) = va {
            // SAFETY: invariant I484: `va` is where `map_bar` mapped
            // `claim` for `dev`'s driver, which has quiesced the device and
            // dropped its VAs (its `remove`, or a probe that failed after
            // its own stop), so nothing touches it; established by
            // `dev_init::claim_mem_bars`.
            unsafe { pci_init::unmap_bar(&claim, va) };
        }
        release(claim);
    }
}

/// Whether BAR `bar` of `dev` is claimed.
pub fn is_claimed(dev: &DevRef, bar: u8) -> bool {
    REG.lock().is_claimed(dev, bar)
}

/// Remove `dev`'s driver under the device's lock: `Bound` → `Removing`,
/// the driver's `remove`, its BARs unmapped and released, then `Present`.
/// `false` when `dev` was not bound.
#[cfg(feature = "kernel_tests")]
pub fn unbind(dev: &DevRef) -> bool {
    let Some(lock) = dev_lock(dev) else {
        return false;
    };
    let _serial = lock.lock();
    let Some(drv) = REG.lock().begin_remove(dev) else {
        return false;
    };
    drv.remove(dev);
    release_bars(dev);
    let inst = REG.lock().finish_remove(dev);
    drop(inst);
    true
}

/// Register `d`, a record no driver matches (vendor `0xFFFE`, class
/// `0xFF`), for a claim test; its id, or `None` when the table is full.
#[cfg(feature = "kernel_tests")]
pub fn push_test_device(mut d: Device) -> Option<u64> {
    d.vendor = 0xFFFE;
    d.class = 0xFF;
    push(d, None).ok().map(|r| r.id())
}

/// Bind any drivers already registered.
pub fn init() {
    bind_all();
}
