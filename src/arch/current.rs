//! The kernel's one name for its port (PORTABILITY §11.1). Kernel code names
//! the seam traits by their full path, `vibeos::arch::…`, since `crate::arch`
//! is the kernel's own module.

use vibeos::arch::{
    Barriers, ContextSwitch, CycleCounter, InterruptMask, PageTable, PerCpuBase, SyscallAbi,
};

/// This build's port, chosen by `cfg(target_arch)`.
#[cfg(target_arch = "x86_64")]
pub type Arch = super::x86_64::Arch;

/// The page-table port items that are not `PageTable` methods.
#[cfg(target_arch = "x86_64")]
pub use super::x86_64::mmu::{enable_nx, flush_local_global};

/// The IRQ-off exclusive cell over this build's port (DESIGN §2.3).
pub type IrqCell<T> = vibeos::cell::IrqCell<T, Arch>;

/// The split virtqueue over this build's port's barriers.
pub type SplitQueue = vibeos::virtio::SplitQueue<Arch>;

/// The page-table walker over this build's port's format.
pub type Mapper = vibeos::mm::paging::Mapper<Arch>;

/// A user address space over this build's port's page tables.
pub type AddressSpace = vibeos::proc::addr_space::AddressSpace<Arch>;

/// Compile-time conformance: the port implements the seam traits built so
/// far. The bound becomes `Port` once the port implements every seam trait
/// (ROADMAP §10.3).
const fn implements_seam_core<
    A: Barriers + ContextSwitch + CycleCounter + InterruptMask + PageTable + PerCpuBase + SyscallAbi,
>() {
}

const _: () = implements_seam_core::<Arch>();
