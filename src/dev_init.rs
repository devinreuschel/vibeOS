//! Device registry instance, bind, shell commands. ROADMAP §6.1.

use core::fmt::Write;

use vibeos::dev::{ClaimError, Device, Driver, MAX_DEVICES, Registry};
use vibeos::lock::RANK_DEVICE;
use vibeos::pci::Bdf;
use vibeos::shell::Command;

use crate::console_init::Console;
use crate::pci_init;
use crate::shell_init;
use crate::sync_init::SpinMutex;

static REG: SpinMutex<Registry> = SpinMutex::with_rank(Registry::new(), RANK_DEVICE);

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
                if let Some(slot) = g.get_mut(dev_i as usize) {
                    if slot.bound.is_none() {
                        *slot = dev;
                        slot.bound = Some(name);
                    }
                }
            }
            Err(_) => {}
        }
        i += 1;
    }
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn len() -> usize {
    REG.lock().len()
}

pub fn get(i: usize) -> Option<Device> {
    REG.lock().get(i).copied()
}

#[allow(dead_code)]
pub fn find_bdf(bdf: Bdf) -> Option<(usize, Device)> {
    let g = REG.lock();
    let mut i = 0usize;
    while i < g.len() {
        if let Some(d) = g.get(i) {
            if d.addr == bdf {
                return Some((i, *d));
            }
        }
        i += 1;
    }
    None
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn find_id(vendor: u16, device: u16) -> Option<(usize, Device)> {
    let g = REG.lock();
    let mut i = 0usize;
    while i < g.len() {
        if let Some(d) = g.get(i) {
            if d.vendor == vendor && d.device_id == device {
                return Some((i, *d));
            }
        }
        i += 1;
    }
    None
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn claim(dev_i: usize, bar: u8) -> Result<(), ClaimError> {
    REG.lock().claim(dev_i, bar)
}

/// Register lspci / devices. Bind any drivers already registered.
pub fn init() {
    bind_all();
    let _ = shell_init::register(Command {
        name: "lspci",
        help: "pci devices",
        run: cmd_lspci,
    });
    let _ = shell_init::register(Command {
        name: "devices",
        help: "device tree",
        run: cmd_devices,
    });
}

/// One Device at a time: a full [Device; MAX] (and a second Registry)
/// overflows the 16 KiB shell stack. Drop RANK_DEVICE before FB print.
fn cmd_lspci(_args: &[&str]) {
    let mut i = 0usize;
    while let Some(d) = get(i) {
        let _ = d.write_lspci(&mut Console);
        let _ = writeln!(Console);
        i += 1;
    }
}

fn cmd_devices(_args: &[&str]) {
    let mut i = 0usize;
    while let Some(d) = get(i) {
        let _ = d.write_tree(&mut Console);
        i += 1;
    }
}
