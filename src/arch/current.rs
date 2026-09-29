//! The kernel's one name for its port (PORTABILITY §11.1). Kernel code names
//! the seam traits by their full path, `vibeos::arch::…`, since `crate::arch`
//! is the kernel's own module.

use vibeos::arch::{Barriers, CycleCounter, InterruptMask, PerCpuBase, SyscallAbi};

/// This build's port, chosen by `cfg(target_arch)`.
#[cfg(target_arch = "x86_64")]
pub type Arch = super::x86_64::Arch;

/// The split virtqueue over this build's port's barriers.
pub type SplitQueue = vibeos::virtio::SplitQueue<Arch>;

/// Compile-time conformance: the port implements the seam traits built so
/// far. The bound becomes `Port` once the port implements every seam trait
/// (ROADMAP §10.3).
const fn implements_seam_core<
    A: Barriers + CycleCounter + InterruptMask + PerCpuBase + SyscallAbi,
>() {
}

const _: () = implements_seam_core::<Arch>();
