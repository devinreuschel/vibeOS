//! Per-CPU control registers: one routine writes CR0 and CR4 whole on every
//! CPU from values the BSP computes once from CPUID (DESIGN §5.1, §11.4).

use vibeos::log::Level;

use crate::cell::BootCell;
use crate::x86::{
    self, CPUID_EBX_SMAP, CPUID_EBX_SMEP, CPUID_ECX_UMIP, CPUID_EDX_MCE, CPUID_EDX_PGE, CR0_AM,
    CR0_ET, CR0_MP, CR0_NE, CR0_PE, CR0_PG, CR0_WP, CR4_LA57, CR4_MCE, CR4_OSFXSR, CR4_OSXMMEXCPT,
    CR4_PAE, CR4_PGE, CR4_SMAP, CR4_SMEP, CR4_UMIP,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Features {
    pub smep: bool,
    pub smap: bool,
    pub umip: bool,
    pub mce: bool,
    pub pge: bool,
}

pub fn cpuid_features() -> Features {
    let (ebx, ecx) = x86::cpuid_leaf7();
    let (_, _, _, edx1) = x86::cpuid(1, 0);
    Features {
        smep: ebx & CPUID_EBX_SMEP != 0,
        smap: ebx & CPUID_EBX_SMAP != 0,
        umip: ecx & CPUID_ECX_UMIP != 0,
        mce: edx1 & CPUID_EDX_MCE != 0,
        pge: edx1 & CPUID_EDX_PGE != 0,
    }
}

/// The CR0 and CR4 every CPU runs with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlRegs {
    pub cr0: u64,
    pub cr4: u64,
}

static CONTROL_REGS: BootCell<ControlRegs> = BootCell::new();

/// CR0: PE, MP, ET, NE, WP, AM, PG, so EM, TS, CD and NW are clear. CR4:
/// PAE, OSFXSR, OSXMMEXCPT, and MCE, PGE, SMEP, SMAP and UMIP where CPUID
/// reports them.
fn compute() -> ControlRegs {
    let f = cpuid_features();
    let cr0 = CR0_PE | CR0_MP | CR0_ET | CR0_NE | CR0_WP | CR0_AM | CR0_PG;
    let mut cr4 = CR4_PAE | CR4_OSFXSR | CR4_OSXMMEXCPT;
    for (on, bit) in [
        (f.mce, CR4_MCE),
        (f.pge, CR4_PGE),
        (f.smep, CR4_SMEP),
        (f.smap, CR4_SMAP),
        (f.umip, CR4_UMIP),
    ] {
        if on {
            cr4 |= bit;
        }
    }
    ControlRegs { cr0, cr4 }
}

/// Write this CPU's CR0 and CR4 whole. Every CPU calls it from
/// `syscall_init::init_cpu`: the BSP through `init_bsp`, each AP through
/// `init_ap`. The BSP's call, the first, computes the values before
/// `smp: done`.
pub fn init_control_regs() {
    // A whole CR4 write that clears LA57 under 5-level paging raises #GP.
    // The kernel asks Limine for no 5-level paging, and the AP trampoline
    // builds 4-level mode, so no CPU has it set.
    assert!(x86::read_cr4() & CR4_LA57 == 0, "CR4.LA57 set");
    let first = CONTROL_REGS.try_get().is_none();
    if first {
        // SAFETY: `BootCell::set` needs one writer before `smp: done`;
        // established at `normal_boot_tail`, where the BSP's
        // `syscall_init::init_bsp` makes the first call before
        // `smp_init::init` starts any AP.
        unsafe { CONTROL_REGS.set(compute()) };
    }
    let regs = *CONTROL_REGS.get();
    // SAFETY: CR0 keeps PE and PG and CR4 keeps PAE, which long mode
    // requires, and every other bit is one CPUID reports or every x86-64
    // CPU has; established by `arch::cpu::compute`.
    unsafe {
        x86::write_cr0(regs.cr0);
        x86::write_cr4(regs.cr4);
    }
    x86::set_smap_live(regs.cr4 & CR4_SMAP != 0);
    if first {
        let smep = u8::from(regs.cr4 & CR4_SMEP != 0);
        let smap = u8::from(regs.cr4 & CR4_SMAP != 0);
        let umip = u8::from(regs.cr4 & CR4_UMIP != 0);
        let wp = u8::from(regs.cr0 & CR0_WP != 0);
        crate::klog!(
            Level::Info,
            "vibeOS: cpu: cr0={:#x} cr4={:#x} smep={smep} smap={smap} umip={umip} wp={wp}",
            regs.cr0,
            regs.cr4
        );
    }
}

/// The values `init_control_regs` writes, once the BSP has computed them.
#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn control_regs() -> Option<ControlRegs> {
    CONTROL_REGS.try_get().copied()
}
