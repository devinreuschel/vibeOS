//! Whether to read `MSR_PLATFORM_INFO` for the CPUID-faulting probe.
//!
//! No CPUID bit says that MSR exists. Intel cores from Nehalem (the first
//! with SSE4.2) on have it. A hypervisor that reports that model may not,
//! and `rdmsr` of a missing MSR raises `#GP`. A VM starts with
//! `MSR_MISC_FEATURES_ENABLES` at 0, so there is nothing to clear.

/// CPUID.0 EBX, EDX, ECX: "GenuineIntel".
pub const VENDOR_INTEL: [u32; 3] = [0x756E_6547, 0x4965_6E69, 0x6C65_746E];

/// CPUID.01H:ECX[20]
pub const CPUID_ECX_SSE42: u32 = 1 << 20;
/// CPUID.01H:ECX[31], hypervisor present.
pub const CPUID_ECX_HYPERVISOR: u32 = 1 << 31;

/// Read `MSR_PLATFORM_INFO` only on a bare-metal Intel CPU with SSE4.2.
///
/// `ebx`, `edx` and `ecx` are CPUID.0's vendor registers in that order.
/// `leaf1_ecx` is CPUID.1:ECX. A set hypervisor bit skips the read: the
/// MSR may `#GP`, and the faulting enable starts clear.
#[must_use]
pub fn probe_platform_info(ebx: u32, edx: u32, ecx: u32, leaf1_ecx: u32) -> bool {
    [ebx, edx, ecx] == VENDOR_INTEL
        && leaf1_ecx & CPUID_ECX_SSE42 != 0
        && leaf1_ecx & CPUID_ECX_HYPERVISOR == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_metal_intel_with_sse42_is_probed() {
        let [ebx, edx, ecx] = VENDOR_INTEL;
        assert!(probe_platform_info(ebx, edx, ecx, CPUID_ECX_SSE42));
        assert!(probe_platform_info(ebx, edx, ecx, CPUID_ECX_SSE42 | 1));
    }

    #[test]
    fn hypervisor_intel_is_not_probed() {
        let [ebx, edx, ecx] = VENDOR_INTEL;
        assert!(!probe_platform_info(
            ebx,
            edx,
            ecx,
            CPUID_ECX_SSE42 | CPUID_ECX_HYPERVISOR
        ));
        assert!(!probe_platform_info(ebx, edx, ecx, CPUID_ECX_HYPERVISOR));
    }

    #[test]
    fn missing_sse42_or_other_vendor_is_not_probed() {
        let [ebx, edx, ecx] = VENDOR_INTEL;
        assert!(!probe_platform_info(ebx, edx, ecx, 0));
        assert!(!probe_platform_info(0, 0, 0, CPUID_ECX_SSE42));
        assert!(!probe_platform_info(
            ebx,
            edx,
            ecx.wrapping_add(1),
            CPUID_ECX_SSE42
        ));
    }
}
