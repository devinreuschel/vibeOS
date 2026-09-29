//! Test-only hooks over proc's kernel half (kernel_tests only, Q2): CR3
//! loads and SYSCALL MSR reads that no production path needs.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use vibeos::addr_space::AddressSpace;
use vibeos::desc::{KERNEL_CS, STAR_SYSRET};

use crate::addr_space_init;
use crate::pmm_init;
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

// Frame counts around `user_init::load_path`, recorded by its inline hook
// points.

/// Free-frame counts around one [`crate::user_init::load_path`] call.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ExecFrames {
    /// Buddy free frames at entry.
    pub before: usize,
    /// Buddy free frames at return, after a failed load's teardown.
    pub after: usize,
    pub ok: bool,
}

const SLOTS: usize = 4;
static NEXT: AtomicUsize = AtomicUsize::new(0);
static BEFORE: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static AFTER: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static OK: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

pub(crate) fn free_now() -> usize {
    pmm_init::with_buddy(|b| b.stats().free_frames)
}

pub(crate) fn record(before: usize, ok: bool) {
    let after = free_now();
    let i = NEXT.load(Ordering::Relaxed);
    BEFORE[i % SLOTS].store(before as u64, Ordering::Relaxed);
    AFTER[i % SLOTS].store(after as u64, Ordering::Relaxed);
    OK[i % SLOTS].store(u64::from(ok), Ordering::Relaxed);
    NEXT.store(i.wrapping_add(1), Ordering::Release);
}

/// Forget the recorded loads.
pub(crate) fn clear_exec_frames() {
    NEXT.store(0, Ordering::Release);
}

/// The last four `load_path` calls since [`clear_exec_frames`], oldest
/// first.
pub(crate) fn exec_frames() -> Vec<ExecFrames> {
    let n = NEXT.load(Ordering::Acquire);
    let first = n.saturating_sub(SLOTS);
    (first..n)
        .map(|i| ExecFrames {
            before: BEFORE[i % SLOTS].load(Ordering::Relaxed) as usize,
            after: AFTER[i % SLOTS].load(Ordering::Relaxed) as usize,
            ok: OK[i % SLOTS].load(Ordering::Relaxed) != 0,
        })
        .collect()
}
