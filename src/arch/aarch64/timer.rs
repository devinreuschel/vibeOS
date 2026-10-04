//! Generic timer from the device-tree node (DESIGN §6.1, ROADMAP §11.3).

use core::arch::asm;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use vibeos::irq::gic::{self, GIC_PPI};
use vibeos::machine::TimerDesc;
use vibeos::time::{ClocksourceId, Counter};

use super::cpu;
use crate::machine_init;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    El1Virt,
    El2HypVirt,
    El2HypPhys,
}

static WHICH: AtomicU32 = AtomicU32::new(0);
static HZ: AtomicU64 = AtomicU64::new(0);
static INTID: AtomicU32 = AtomicU32::new(0);

pub fn cntfrq() -> u64 {
    let v: u64;
    // SAFETY: CNTFRQ_EL0; established here.
    unsafe { asm!("mrs {0}, cntfrq_el0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Bring up the timer for this exception level. Returns the counter.
pub fn init() -> Option<Counter> {
    let desc = machine_init::info().and_then(|d| d.arm_timer());
    let (which, intid, hz) = pick(desc);
    // Relaxed: published once on the BSP before irq: enabled; pairs with nothing.
    WHICH.store(which as u32, Ordering::Relaxed);
    // Relaxed: as above; pairs with nothing.
    INTID.store(intid, Ordering::Relaxed);
    // Relaxed: as above; pairs with nothing.
    HZ.store(hz, Ordering::Relaxed);
    crate::arch::aarch64::publish_cntfrq(hz);
    let name = match which {
        Which::El1Virt => "el1 virt",
        Which::El2HypVirt => "el2 hyp-virt",
        Which::El2HypPhys => "el2 hyp-phys",
    };
    crate::marker!("vibeOS: time: timer {name}");
    crate::marker!("vibeOS: time: cntfrq {hz}/s");
    Counter::new(ClocksourceId::Cntvct, hz, 64)
}

fn pick(desc: Option<TimerDesc>) -> (Which, u32, u64) {
    let frq = cntfrq();
    let (irqs, nirq, clock) = match desc {
        Some(TimerDesc::ArmGeneric {
            irqs,
            nirq,
            clock_frequency,
        }) => (irqs, nirq, clock_frequency),
        _ => ([0; 5], 0, None),
    };
    let hz = clock.map(u64::from).filter(|h| *h != 0).unwrap_or(frq);
    let vhe = cpu::el2_vhe();
    // irqs[] are INTID cells: type, number packed? FDT parser stores the
    // interrupt number cells in tree order as the PPI numbers (or INTIDs).
    // MachineDesc ArmGeneric.irqs is the ID cells. Use PPI virt=11 default.
    // Timer binding order: sec-phys, ns-phys, virt, hyp-phys, hyp-virt.
    let virt = irq_or(irqs, nirq, 2, 11);
    let hyp_phys = irq_or(irqs, nirq, 3, 10);
    let hyp_virt = irq_or(irqs, nirq, 4, 9);
    if !vhe {
        return (Which::El1Virt, virt, hz);
    }
    if nirq >= 5 {
        (Which::El2HypVirt, hyp_virt, hz)
    } else {
        (Which::El2HypPhys, hyp_phys, hz)
    }
}

fn irq_or(irqs: [u32; 5], nirq: u8, idx: usize, ppi: u32) -> u32 {
    let num = if (idx as u8) < nirq {
        irqs.get(idx).copied().unwrap_or(ppi)
    } else {
        ppi
    };
    gic::gic_intid(GIC_PPI, num).unwrap_or(16 + ppi)
}

pub fn intid() -> u32 {
    // Relaxed: as in `init`.
    // Relaxed: pairs with nothing.
    INTID.load(Ordering::Relaxed)
}

pub fn hz() -> u64 {
    // Relaxed: as in `init`.
    // Relaxed: pairs with nothing.
    HZ.load(Ordering::Relaxed)
}

/// Program TVAL for `ns` from now.
pub fn rearm_ns(ns: u64) {
    let hz = hz();
    if hz == 0 {
        return;
    }
    let ticks = ns.saturating_mul(hz) / 1_000_000_000;
    let tval = ticks.min(u32::MAX as u64) as u32;
    write_tval(tval);
}

fn write_tval(tval: u32) {
    // Relaxed: pairs with nothing.
    let which = WHICH.load(Ordering::Relaxed);
    // SAFETY: timer TVAL/CTL for the chosen timer; established here.
    unsafe {
        match which {
            x if x == Which::El2HypPhys as u32 => {
                asm!(
                    "msr cntp_tval_el0, {0}",
                    "mov {1}, #1",
                    "msr cntp_ctl_el0, {1}",
                    "isb",
                    in(reg) u64::from(tval),
                    out(reg) _,
                    options(nostack, preserves_flags),
                );
            }
            _ => {
                asm!(
                    "msr cntv_tval_el0, {0}",
                    "mov {1}, #1",
                    "msr cntv_ctl_el0, {1}",
                    "isb",
                    in(reg) u64::from(tval),
                    out(reg) _,
                    options(nostack, preserves_flags),
                );
            }
        }
    }
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "in-guest test hook")
)]
pub fn disable() {
    // Relaxed: pairs with nothing.
    let which = WHICH.load(Ordering::Relaxed);
    // SAFETY: clear ENABLE. established here.
    unsafe {
        if which == Which::El2HypPhys as u32 {
            asm!(
                "msr cntp_ctl_el0, xzr",
                "isb",
                options(nostack, preserves_flags)
            );
        } else {
            asm!(
                "msr cntv_ctl_el0, xzr",
                "isb",
                options(nostack, preserves_flags)
            );
        }
    }
}

pub fn enable() {
    rearm_ns(10_000_000);
}

pub fn on_tick() {
    rearm_ns(10_000_000);
}
