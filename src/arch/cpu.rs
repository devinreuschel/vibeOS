//! Per-CPU CR0/CR4 hardening. S1; DESIGN §5.1.

use vibeos::log::Level;

use crate::x86::{
    self, CPUID_EBX_SMAP, CPUID_EBX_SMEP, CPUID_ECX_UMIP, CR0_WP, CR4_SMAP, CR4_SMEP, CR4_UMIP,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Features {
    pub smep: bool,
    pub smap: bool,
    pub umip: bool,
}

pub fn cpuid_features() -> Features {
    let (ebx, ecx) = x86::cpuid_leaf7();
    Features {
        smep: ebx & CPUID_EBX_SMEP != 0,
        smap: ebx & CPUID_EBX_SMAP != 0,
        umip: ecx & CPUID_ECX_UMIP != 0,
    }
}

/// Enable SMEP/SMAP/UMIP when CPUID allows; assert `CR0.WP`.
pub fn harden() {
    let f = cpuid_features();
    let mut cr4 = x86::read_cr4();
    if f.smep {
        cr4 |= CR4_SMEP;
    }
    if f.smap {
        cr4 |= CR4_SMAP;
    }
    if f.umip {
        cr4 |= CR4_UMIP;
    }
    unsafe { x86::write_cr4(cr4) };
    x86::set_smap_live(f.smap);

    let mut cr0 = x86::read_cr0();
    if cr0 & CR0_WP == 0 {
        cr0 |= CR0_WP;
        unsafe { x86::write_cr0(cr0) };
    }
    assert!(x86::read_cr0() & CR0_WP != 0, "CR0.WP");

    let cr4 = x86::read_cr4();
    let smep = u8::from(cr4 & CR4_SMEP != 0);
    let smap = u8::from(cr4 & CR4_SMAP != 0);
    let umip = u8::from(cr4 & CR4_UMIP != 0);
    let wp = u8::from(x86::read_cr0() & CR0_WP != 0);
    crate::klog!(
        Level::Info,
        "vibeOS: cpu: smep={smep} smap={smap} umip={umip} wp={wp}"
    );
}
