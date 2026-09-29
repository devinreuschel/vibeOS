//! Test-only hooks over proc's kernel half (kernel_tests only, Q2): CR3
//! loads and SYSCALL MSR reads that no production path needs.

use core::sync::atomic::Ordering;

use vibeos::addr_space::AddressSpace;
use vibeos::desc::{KERNEL_CS, STAR_SYSRET};

use crate::addr_space_init;
use crate::x86::{self, EFER_SCE, IA32_EFER, IA32_STAR};

/// Load `space`'s root into CR3 unless this CPU already has it, and record
/// it in this CPU's `PerCpuRemote.as_cr3`. No TCB names it, so the next
/// switch back to the calling thread loads that thread's own root again.
pub(crate) fn load_cr3(space: &AddressSpace) {
    // SAFETY: invariant I128: the root is a PML4 `addr_space_init::create`
    // or `addr_space_init::clone_full` built, whose kernel half is the
    // kernel's, and only `addr_space_init::teardown` frees it, which refuses
    // a root this CPU has loaded or recorded; established by
    // `addr_space_init::teardown`.
    unsafe { addr_space_init::load_cr3_u64(space.root().as_u64()) };
}

/// This CPU's recorded root is `space`'s: a [`load_cr3`] of it skipped the
/// write.
pub(crate) fn cr3_was_skipped(space: &AddressSpace) -> bool {
    crate::per_cpu_init::current()
        .remote
        .as_cr3
        .load(Ordering::Relaxed)
        == space.root().as_u64()
}

/// STAR holds the kernel and SYSRET selectors, and EFER.SCE is set.
pub(crate) fn star_configured() -> bool {
    let star = x86::rdmsr(IA32_STAR);
    let efer = x86::rdmsr(IA32_EFER);
    let syscall_cs = ((star >> 32) & 0xFFFF) as u16;
    let sysret_cs = ((star >> 48) & 0xFFFF) as u16;
    syscall_cs == KERNEL_CS && sysret_cs == STAR_SYSRET && (efer & EFER_SCE) != 0
}
