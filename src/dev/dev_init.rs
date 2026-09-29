//! Device registry instance and bind. ROADMAP §6.1. The `lspci` and
//! `devices` commands are in `shell::cmds::dev`.

use vibeos::dev::{Device, Driver, MAX_DEVICES, Registry};
use vibeos::lock::RANK_DEVICE;
use vibeos::log::Level;

use crate::pci_init;
use crate::sync_init::SpinMutex;

/// The device registry. `dev::ktest` reads it for its hooks.
pub(super) static REG: SpinMutex<Registry> = SpinMutex::with_rank(Registry::new(), RANK_DEVICE);

pub fn push(d: Device) -> bool {
    REG.lock().push(d)
}

pub fn register_driver(drv: &'static dyn Driver) -> bool {
    REG.lock().register(drv)
}

pub fn bind_all() {
    let mut jobs = [(0u8, 0u8); MAX_DEVICES];
    let n = REG.lock().collect_bind_jobs(&mut jobs);
    let mut i = 0usize;
    while i < n {
        let (drv_i, dev_i) = jobs[i];
        let Some((drv, mut dev)) = ({
            let g = REG.lock();
            match (g.driver_at(drv_i as usize), g.get(dev_i as usize).copied()) {
                (Some(drv), Some(dev)) if dev.bound.is_none() => Some((drv, dev)),
                _ => None,
            }
        }) else {
            i += 1;
            continue;
        };
        pci_init::enable_mem_master(dev.addr);
        match drv.probe(&mut dev) {
            Ok(()) => {
                let name = drv.name();
                let mut g = REG.lock();
                if let Some(slot) = g.get_mut(dev_i as usize)
                    && slot.bound.is_none()
                {
                    *slot = dev;
                    slot.bound = Some(name);
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
        i += 1;
    }
}

pub fn get(i: usize) -> Option<Device> {
    REG.lock().get(i).copied()
}

/// Bind any drivers already registered.
pub fn init() {
    bind_all();
}
