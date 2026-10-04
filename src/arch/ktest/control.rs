//! In-guest tests of the control registers every CPU runs with and the bits
//! `syscall_init::init_cpu` clears (kernel_tests only). Rows: the parent
//! `ktest.rs`'s `TESTS`.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch;
use crate::ipi_init;
use crate::ktest::Outcome;
use crate::per_cpu_init;
use crate::x86::{
    self, CR0_AM, CR0_MP, CR0_NE, CR0_PE, CR0_PG, CR0_TS, CR0_WP, CR4_MCE, CR4_OSFXSR,
    CR4_OSXMMEXCPT, CR4_OSXSAVE, CR4_PAE, CR4_PCE, CR4_PGE, CR4_PKE, CR4_SMAP, CR4_SMEP, CR4_TSD,
    CR4_UMIP, EFER_FFXSR, IA32_EFER, MISC_FEATURES_CPUID_FAULTING, MSR_MISC_FEATURES_ENABLES,
};

const CR0_EM: u64 = 1 << 2;
const CR0_NW: u64 = 1 << 29;
const CR0_CD: u64 = 1 << 30;
/// CPUID.01H:ECX[26]
const CPUID_ECX_XSAVE: u32 = 1 << 26;
/// CPUID.(EAX=7,ECX=0):ECX[3]
const CPUID_ECX_PKU: u32 = 1 << 3;
/// CPUID.80000001H:EDX[25]
const CPUID_EDX_FFXSR: u32 = 1 << 25;

/// One CPU's CR0, CR4, EFER, and `MSR_MISC_FEATURES_ENABLES` (0 where the
/// CPU has no CPUID faulting). A zero CR0 (PE and PG clear) is an empty slot.
struct CrSnapS16 {
    cr0: AtomicU64,
    cr4: AtomicU64,
    efer: AtomicU64,
    misc: AtomicU64,
}

fn snap_cr(arg: *mut ()) {
    // SAFETY: `arg` is the `[CrSnapS16; 64]` that `cpu_control_regs` passes
    // to itself and to a waiting `ipi_init::call_mask`, live until every
    // target acks; established at `arch::ktest::control::cpu_control_regs`.
    let snaps = unsafe { &*(arg as *const [CrSnapS16; 64]) };
    let me = per_cpu_init::current().cpu_id as usize;
    if let Some(s) = snaps.get(me) {
        let misc = if x86::cpuid_faulting() {
            x86::rdmsr(MSR_MISC_FEATURES_ENABLES)
        } else {
            0
        };
        s.cr4.store(x86::read_cr4(), Ordering::Relaxed);
        s.efer.store(x86::rdmsr(IA32_EFER), Ordering::Relaxed);
        s.misc.store(misc, Ordering::Relaxed);
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

const CR4_CLEAR: [(u64, &str); 4] = [
    (CR4_TSD, "TSD"),
    (CR4_PCE, "PCE"),
    (CR4_OSXSAVE, "OSXSAVE"),
    (CR4_PKE, "PKE"),
];

/// Every online CPU runs with the bits ROADMAP §10.6's control-register box
/// names, SMEP, SMAP and UMIP exactly where CPUID enumerates them, §11.1's
/// clears (`CR4.OSXSAVE`, `CR4.PKE`, `EFER.FFXSR`) and DESIGN §11.4's
/// (`CR4.TSD`, `CR4.PCE`, CPUID faulting), and exactly the CR0 and CR4
/// `arch::cpu::init_control_regs` computed.
pub(crate) fn cpu_control_regs() -> Outcome {
    let Some(want) = arch::cpu::stored_control_regs() else {
        return Outcome::Fail("control registers never computed");
    };
    let f = arch::cpu::cpuid_features();
    let by_cpuid = [
        (f.smep, CR4_SMEP, "SMEP"),
        (f.smap, CR4_SMAP, "SMAP"),
        (f.umip, CR4_UMIP, "UMIP"),
    ];
    let snaps: [CrSnapS16; 64] = core::array::from_fn(|_| CrSnapS16 {
        cr0: AtomicU64::new(0),
        cr4: AtomicU64::new(0),
        efer: AtomicU64::new(0),
        misc: AtomicU64::new(0),
    });
    let mask = per_cpu_init::online_mask();
    {
        // `call_mask` skips the caller, so the local read and the calls
        // must see the same CPU: no migration in between.
        let _irq = x86::InterruptGuard::enter();
        // `snap_cr` stores only to `snaps`' atomics, through `&`.
        // PROVENANCE: nothing writes through the pointer.
        snap_cr(&snaps as *const _ as *mut ());
        // PROVENANCE: nothing writes through the pointer, as above.
        ipi_init::call_mask(mask, snap_cr, &snaps as *const _ as *mut (), true);
    }
    for (c, s) in snaps.iter().enumerate() {
        if mask & (1u64 << c) == 0 {
            continue;
        }
        let cr0 = s.cr0.load(Ordering::Acquire);
        let cr4 = s.cr4.load(Ordering::Relaxed);
        let efer = s.efer.load(Ordering::Relaxed);
        let misc = s.misc.load(Ordering::Relaxed);
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
        for (bit, name) in CR4_CLEAR {
            if cr4 & bit != 0 {
                return crate::fail_fmt!("cpu {c}: CR4.{name} set (cr4={cr4:#x})");
            }
        }
        for (on, bit, name) in by_cpuid {
            if on != (cr4 & bit != 0) {
                return crate::fail_fmt!("cpu {c}: CR4.{name} differs from CPUID (cr4={cr4:#x})");
            }
        }
        if efer & EFER_FFXSR != 0 {
            return crate::fail_fmt!("cpu {c}: EFER.FFXSR set (efer={efer:#x})");
        }
        if misc & MISC_FEATURES_CPUID_FAULTING != 0 {
            return crate::fail_fmt!("cpu {c}: CPUID faulting on (misc={misc:#x})");
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

/// `syscall_init::init_cpu` clears what firmware or a loader might leave
/// set (ROADMAP §11.1, DESIGN §11.4): CR4's TSD, PCE, OSXSAVE and PKE,
/// `EFER.FFXSR`, and CPUID faulting, each planted where the CPU has it, are
/// clear after it runs, so CR4 is written whole and never read back.
pub(crate) fn cpu_control_clears() -> Outcome {
    let (_, _, ecx1, _) = x86::cpuid(1, 0);
    let (_, ecx7) = x86::cpuid_leaf7();
    let (ext, ..) = x86::cpuid(0x8000_0000, 0);
    let ffxsr = ext >= 0x8000_0001 && x86::cpuid(0x8000_0001, 0).3 & CPUID_EDX_FFXSR != 0;
    let mut cr4 = CR4_TSD | CR4_PCE;
    if ecx1 & CPUID_ECX_XSAVE != 0 {
        cr4 |= CR4_OSXSAVE;
    }
    if ecx7 & CPUID_ECX_PKU != 0 {
        cr4 |= CR4_PKE;
    }
    // IF off, so the plant and the rewrite happen on one CPU and no ring-3
    // code or FP switch runs in between.
    let _g = x86::InterruptGuard::enter();
    let fault = x86::cpuid_faulting();
    // SAFETY: each planted bit is one CPUID enumerates (OSXSAVE, PKE,
    // FFXSR, CPUID faulting) or every x86_64 CPU has (TSD, PCE); with IF
    // off nothing on this CPU sees them before `init_cpu` below clears
    // them; established here.
    unsafe {
        x86::write_cr4(x86::read_cr4() | cr4);
        if ffxsr {
            x86::wrmsr(IA32_EFER, x86::rdmsr(IA32_EFER) | EFER_FFXSR);
        }
        if fault {
            let misc = x86::rdmsr(MSR_MISC_FEATURES_ENABLES);
            x86::wrmsr(
                MSR_MISC_FEATURES_ENABLES,
                misc | MISC_FEATURES_CPUID_FAULTING,
            );
        }
    }
    // SAFETY: `init_cpu`'s contract: the GDT is loaded and `GS_BASE` is
    // this CPU's `PerCpu`, as bring-up left them before `smp: done`;
    // established at `proc::syscall_init::init_bsp` and
    // `proc::syscall_init::init_ap`.
    unsafe { crate::syscall_init::init_cpu() };
    let left = x86::read_cr4() & cr4;
    if left != 0 {
        return crate::fail_fmt!("CR4 {left:#x} of planted {cr4:#x} left set by init_cpu");
    }
    if x86::rdmsr(IA32_EFER) & EFER_FFXSR != 0 {
        return Outcome::Fail("EFER.FFXSR left set by init_cpu");
    }
    if fault && x86::rdmsr(MSR_MISC_FEATURES_ENABLES) & MISC_FEATURES_CPUID_FAULTING != 0 {
        return Outcome::Fail("CPUID faulting left on by init_cpu");
    }
    Outcome::Ok
}
