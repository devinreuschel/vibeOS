//! PSCI function IDs, return codes, and the secondary-core parameter
//! block (ROADMAP §11.4). Host-tested so the stub and the BSP agree on
//! offsets.

use core::mem::offset_of;

/// 64-bit `CPU_ON` (PSCI 0.2 / 1.0).
pub const CPU_ON: u64 = 0xC400_0003;
/// 32-bit `CPU_OFF`.
pub const CPU_OFF: u64 = 0x8400_0002;
/// 64-bit `AFFINITY_INFO`.
pub const AFFINITY_INFO: u64 = 0xC400_0004;
/// 32-bit `SYSTEM_OFF`.
pub const SYSTEM_OFF: u64 = 0x8400_0008;
/// 32-bit `SYSTEM_RESET`.
pub const SYSTEM_RESET: u64 = 0x8400_0009;

/// `CPU_ON` returned this core already running.
pub const ALREADY_ON: i64 = -4;

/// `AFFINITY_INFO`: the core is on.
pub const AFF_ON: i64 = 0;
/// `AFFINITY_INFO`: the core is off.
pub const AFF_OFF: i64 = 1;
/// `AFFINITY_INFO`: `CPU_ON` has been accepted, the core has not arrived.
pub const AFF_ON_PENDING: i64 = 2;

/// `SecondaryParam.status`: the stub has not stored yet.
pub const STATUS_NONE: u64 = 0;
/// First shared write: MMU on, arrived.
pub const STATUS_ARRIVED: u64 = 1;
/// FEAT_LSE or FEAT_PAN missing; the core calls `CPU_OFF`.
pub const STATUS_FEATURE: u64 = 2;

/// One core's bring-up block. The boot CPU fills it, cleans it to the
/// Point of Coherency, and passes its physical address as PSCI
/// `context_id`. The stub reads it at that PA with the MMU off, then at
/// the identity map, then at `param_va` after it drops the identity map.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SecondaryParam {
    pub ttbr0_id: u64,
    pub ttbr1: u64,
    pub ttbr0_empty: u64,
    pub sctlr: u64,
    pub tcr: u64,
    pub mair: u64,
    pub cntkctl: u64,
    pub pmuserenr: u64,
    pub cpacr: u64,
    pub stack_top: u64,
    pub cpu_ptr: u64,
    pub entry_va: u64,
    pub continue_va: u64,
    pub param_va: u64,
    pub el2: u64,
    pub hcr_el2: u64,
    pub cptr_el2: u64,
    pub cnthctl_el2: u64,
    pub hstr_el2: u64,
    pub mdcr_el2: u64,
    pub icc_sre_el2: u64,
    pub sctlr_el2: u64,
    pub hcrx_el2: u64,
    pub hcrx_valid: u64,
    pub hfgrtr_el2: u64,
    pub hfgwtr_el2: u64,
    pub hfgitr_el2: u64,
    pub hdfgrtr_el2: u64,
    pub hdfgwtr_el2: u64,
    pub hafgrtr_el2: u64,
    pub fgt_valid: u64,
    pub conduit_hvc: u64,
    pub status: u64,
}

impl SecondaryParam {
    pub const TTBR0_ID: usize = offset_of!(Self, ttbr0_id);
    pub const TTBR1: usize = offset_of!(Self, ttbr1);
    pub const TTBR0_EMPTY: usize = offset_of!(Self, ttbr0_empty);
    pub const SCTLR: usize = offset_of!(Self, sctlr);
    pub const TCR: usize = offset_of!(Self, tcr);
    pub const MAIR: usize = offset_of!(Self, mair);
    pub const CNTKCTL: usize = offset_of!(Self, cntkctl);
    pub const PMUSERENR: usize = offset_of!(Self, pmuserenr);
    pub const CPACR: usize = offset_of!(Self, cpacr);
    pub const STACK_TOP: usize = offset_of!(Self, stack_top);
    pub const CPU_PTR: usize = offset_of!(Self, cpu_ptr);
    pub const ENTRY_VA: usize = offset_of!(Self, entry_va);
    pub const CONTINUE_VA: usize = offset_of!(Self, continue_va);
    pub const PARAM_VA: usize = offset_of!(Self, param_va);
    pub const EL2: usize = offset_of!(Self, el2);
    pub const HCR_EL2: usize = offset_of!(Self, hcr_el2);
    pub const CPTR_EL2: usize = offset_of!(Self, cptr_el2);
    pub const CNTHCTL_EL2: usize = offset_of!(Self, cnthctl_el2);
    pub const HSTR_EL2: usize = offset_of!(Self, hstr_el2);
    pub const MDCR_EL2: usize = offset_of!(Self, mdcr_el2);
    pub const ICC_SRE_EL2: usize = offset_of!(Self, icc_sre_el2);
    pub const SCTLR_EL2: usize = offset_of!(Self, sctlr_el2);
    pub const HCRX_EL2: usize = offset_of!(Self, hcrx_el2);
    pub const HCRX_VALID: usize = offset_of!(Self, hcrx_valid);
    pub const HFGRTR_EL2: usize = offset_of!(Self, hfgrtr_el2);
    pub const HFGWTR_EL2: usize = offset_of!(Self, hfgwtr_el2);
    pub const HFGITR_EL2: usize = offset_of!(Self, hfgitr_el2);
    pub const HDFGRTR_EL2: usize = offset_of!(Self, hdfgrtr_el2);
    pub const HDFGWTR_EL2: usize = offset_of!(Self, hdfgwtr_el2);
    pub const HAFGRTR_EL2: usize = offset_of!(Self, hafgrtr_el2);
    pub const FGT_VALID: usize = offset_of!(Self, fgt_valid);
    pub const CONDUIT_HVC: usize = offset_of!(Self, conduit_hvc);
    pub const STATUS: usize = offset_of!(Self, status);

    pub const fn empty() -> Self {
        Self {
            ttbr0_id: 0,
            ttbr1: 0,
            ttbr0_empty: 0,
            sctlr: 0,
            tcr: 0,
            mair: 0,
            cntkctl: 0,
            pmuserenr: 0,
            cpacr: 0,
            stack_top: 0,
            cpu_ptr: 0,
            entry_va: 0,
            continue_va: 0,
            param_va: 0,
            el2: 0,
            hcr_el2: 0,
            cptr_el2: 0,
            cnthctl_el2: 0,
            hstr_el2: 0,
            mdcr_el2: 0,
            icc_sre_el2: 0,
            sctlr_el2: 0,
            hcrx_el2: 0,
            hcrx_valid: 0,
            hfgrtr_el2: 0,
            hfgwtr_el2: 0,
            hfgitr_el2: 0,
            hdfgrtr_el2: 0,
            hdfgwtr_el2: 0,
            hafgrtr_el2: 0,
            fgt_valid: 0,
            conduit_hvc: 0,
            status: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::aarch64::sysreg;

    #[test]
    fn psci_ids_match_the_spec() {
        assert_eq!(CPU_ON, 0xC400_0003);
        assert_eq!(CPU_OFF, 0x8400_0002);
        assert_eq!(AFFINITY_INFO, 0xC400_0004);
        assert_eq!(SYSTEM_OFF, 0x8400_0008);
        assert_eq!(SYSTEM_RESET, 0x8400_0009);
        assert_eq!(ALREADY_ON, -4);
        assert_eq!(AFF_ON, 0);
        assert_eq!(AFF_OFF, 1);
        assert_eq!(AFF_ON_PENDING, 2);
    }

    #[test]
    fn param_offsets_are_packed() {
        assert_eq!(SecondaryParam::TTBR0_ID, 0);
        assert_eq!(SecondaryParam::TTBR1, 8);
        assert_eq!(SecondaryParam::SCTLR, 24);
        assert_eq!(SecondaryParam::CNTKCTL, 48);
        assert_eq!(SecondaryParam::STATUS, 32 * 8);
        assert_eq!(core::mem::size_of::<SecondaryParam>(), 33 * 8);
        assert!(core::mem::size_of::<SecondaryParam>() <= 4096);
    }

    #[test]
    fn computed_sysregs_are_the_202_values() {
        assert_eq!(sysreg::sctlr_el1() & 1, 1);
        assert_eq!(sysreg::cntkctl_el1(), 1 << 1);
        // E2H=0 encoding. VHE writes `cntkctl_el1` into CNTHCTL_EL2.
        assert_eq!(sysreg::cnthctl_el2(), 0b11);
        assert_eq!(sysreg::pmuserenr_el0(), 0);
        assert_eq!(sysreg::cpacr_el1() >> 20 & 0b11, 0b11);
    }
}
