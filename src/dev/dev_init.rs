//! Device registry instance and bind. ROADMAP §6.1. The `lspci` and
//! `devices` commands are in `shell::cmds::dev`.
//!
//! The registry holds counted entries ([`DevRef`]) and the instances their
//! drivers returned (DESIGN §12.1 rule 1). Nothing is allocated under
//! [`REG`]: [`push`] builds its entry before it takes the lock, and every
//! reference a lookup returns is a clone the caller drops unlocked.

use vibeos::dev::{BarClaim, ClaimError, DevRef, Device, Driver, Instance, MAX_DEVICES, Registry};
use vibeos::kalloc::AllocError;
use vibeos::lock::RANK_DEVICE;
use vibeos::log::Level;
use vibeos::pci::Bdf;

use crate::boot;
use crate::sync_init::SpinMutex;

/// The device registry. `dev::ktest` reads it for its hooks.
pub(super) static REG: SpinMutex<Registry> = SpinMutex::with_rank(Registry::new(), RANK_DEVICE);

/// Register `d` under a fresh id. `AllocError` when the heap or the table
/// is full.
pub fn push(d: Device) -> Result<DevRef, AllocError> {
    let id = REG.lock().take_id();
    let r = DevRef::try_new(id, d)?;
    // A clone goes in, so a refused insert drops only a count under the
    // lock, and `r`'s own count after it.
    REG.lock().insert(r.clone())?;
    Ok(r)
}

pub fn register_driver(drv: &'static dyn Driver) -> bool {
    REG.lock().register(drv)
}

pub fn bind_all() {
    let mut jobs: [Option<(u8, DevRef)>; MAX_DEVICES] = [const { None }; MAX_DEVICES];
    let n = REG.lock().collect_bind_jobs(&mut jobs);
    for (drv_i, dev) in jobs.iter().take(n).flatten() {
        let drv = {
            let g = REG.lock();
            match g.driver_at(*drv_i as usize) {
                Some(drv) if g.bound(dev).is_none() => Some(drv),
                _ => None,
            }
        };
        let Some(drv) = drv else {
            continue;
        };
        match drv.probe(dev) {
            Ok(inst) => {
                let mut g = REG.lock();
                if g.bound(dev).is_none() {
                    // An unbound device and a registered driver: it binds.
                    g.bind(dev, drv.name(), inst);
                } else {
                    // Binding is serial, so this does not happen; were the
                    // device bound meanwhile, it keeps its driver, and
                    // `inst` is dropped with the lock dropped.
                    drop(g);
                    drop(inst);
                }
            }
            // The device stays unbound, its slot untouched.
            Err(e) => crate::klog!(
                Level::Warn,
                "vibeOS: dev: probe {} {} {:04x}:{:04x} failed: {}",
                drv.name(),
                dev.addr,
                dev.vendor,
                dev.device_id,
                e.as_str()
            ),
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
