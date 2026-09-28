//! In-guest tests of P10-S16, Per-CPU control registers and the ring-3 trap table (DESIGN §8.2).

use core::sync::atomic::{AtomicU64, Ordering};

use super::{Outcome, Test, test};
use crate::arch;
use crate::ipi_init;
use crate::per_cpu_init;
use crate::x86::{
    self, CR0_AM, CR0_CD, CR0_EM, CR0_MP, CR0_NE, CR0_NW, CR0_PE, CR0_PG, CR0_TS, CR0_WP, CR4_MCE,
    CR4_OSFXSR, CR4_OSXMMEXCPT, CR4_PAE, CR4_PGE,
};

pub(super) const TESTS: &[Test] = &[test("cpu_control_regs", cpu_control_regs)];

/// CPUID.01H:ECX[31] (a hypervisor is present) and leaf `0x4000_0000`
/// naming it `KVMKVMKVM\0\0\0`.
pub(super) fn on_kvm() -> bool {
    let (_, _, ecx1, _) = x86::cpuid(1, 0);
    if ecx1 & (1 << 31) == 0 {
        return false;
    }
    let (_, b, c, d) = x86::cpuid(0x4000_0000, 0);
    let mut id = [0u8; 12];
    id[..4].copy_from_slice(&b.to_le_bytes());
    id[4..8].copy_from_slice(&c.to_le_bytes());
    id[8..].copy_from_slice(&d.to_le_bytes());
    &id == b"KVMKVMKVM\0\0\0"
}

/// One CPU's CR0 and CR4. A zero CR0 (PE and PG clear) is an empty slot.
struct CrSnap {
    cr0: AtomicU64,
    cr4: AtomicU64,
}

fn snap_cr(arg: *mut ()) {
    let snaps = unsafe { &*(arg as *const [CrSnap; 64]) };
    let me = per_cpu_init::current().cpu_id as usize;
    if let Some(s) = snaps.get(me) {
        s.cr4.store(x86::read_cr4(), Ordering::Relaxed);
        s.cr0.store(x86::read_cr0(), Ordering::Release);
    }
}

const CR0_SET: [(u64, &str); 6] = [
    (CR0_PE, "PE"),
    (CR0_MP, "MP"),
    (CR0_NE, "NE"),
    (CR0_WP, "WP"),
    (CR0_AM, "AM"),
    (CR0_PG, "PG"),
];
const CR0_CLEAR: [(u64, &str); 4] = [
    (CR0_EM, "EM"),
    (CR0_TS, "TS"),
    (CR0_CD, "CD"),
    (CR0_NW, "NW"),
];
const CR4_SET: [(u64, &str); 5] = [
    (CR4_PAE, "PAE"),
    (CR4_MCE, "MCE"),
    (CR4_PGE, "PGE"),
    (CR4_OSFXSR, "OSFXSR"),
    (CR4_OSXMMEXCPT, "OSXMMEXCPT"),
];

/// Every online CPU runs with the bits ROADMAP §10.6's control-register box
/// names, and with exactly the CR0 and CR4 `arch::cpu::init_control_regs`
/// computed.
fn cpu_control_regs() -> Outcome {
    let Some(want) = arch::cpu::control_regs() else {
        return Outcome::Fail("control registers never computed");
    };
    let snaps: [CrSnap; 64] = core::array::from_fn(|_| CrSnap {
        cr0: AtomicU64::new(0),
        cr4: AtomicU64::new(0),
    });
    let mask = per_cpu_init::online_mask();
    {
        // `call_mask` skips the caller, so the local read and the calls
        // must see the same CPU: no migration in between.
        let _irq = x86::InterruptGuard::enter();
        snap_cr(&snaps as *const _ as *mut ());
        ipi_init::call_mask(mask, snap_cr, &snaps as *const _ as *mut (), true);
    }
    for (c, s) in snaps.iter().enumerate() {
        if mask & (1u64 << c) == 0 {
            continue;
        }
        let cr0 = s.cr0.load(Ordering::Acquire);
        let cr4 = s.cr4.load(Ordering::Relaxed);
        if cr0 == 0 {
            return crate::fail_fmt!("cpu {c}: no snapshot");
        }
        for (bit, name) in CR0_SET {
            if cr0 & bit == 0 {
                return crate::fail_fmt!("cpu {c}: CR0.{name} clear (cr0={cr0:#x})");
            }
        }
        for (bit, name) in CR0_CLEAR {
            if cr0 & bit != 0 {
                return crate::fail_fmt!("cpu {c}: CR0.{name} set (cr0={cr0:#x})");
            }
        }
        for (bit, name) in CR4_SET {
            if cr4 & bit == 0 {
                return crate::fail_fmt!("cpu {c}: CR4.{name} clear (cr4={cr4:#x})");
            }
        }
        if cr0 != want.cr0 {
            return crate::fail_fmt!("cpu {c}: cr0={cr0:#x}, routine computed {:#x}", want.cr0);
        }
        if cr4 != want.cr4 {
            return crate::fail_fmt!("cpu {c}: cr4={cr4:#x}, routine computed {:#x}", want.cr4);
        }
    }
    Outcome::Ok
}
