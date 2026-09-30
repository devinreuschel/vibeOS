//! The kernel's one name for its port (PORTABILITY §11.1). Kernel code names
//! the seam traits by their full path, `vibeos::arch::…`, since `crate::arch`
//! is the kernel's own module.

/// This build's port, chosen by `cfg(target_arch)`.
#[cfg(target_arch = "x86_64")]
pub type Arch = super::x86_64::Arch;

/// The page-table port items that are not `PageTable` methods.
#[cfg(target_arch = "x86_64")]
pub use super::x86_64::mmu::{enable_nx, flush_local_global};

/// The port's CPU primitives that shared kernel code calls by these
/// port-neutral names (`docs/ARCH.md`): the interrupt guard and flag, the
/// halt and the one-interrupt wait, the test exit, the hardware RNG, the
/// user TLS register, the stack pointer, and the per-CPU hooks.
#[cfg(target_arch = "x86_64")]
pub use super::x86_64::cpu::{
    InterruptGuard, cli as irq_disable, halt, hlt_once as wait_for_interrupt, hw_rng64,
    interrupts_enabled, set_per_cpu_hooks, set_user_tls, stack_pointer, sti as irq_enable,
    user_tls,
};

// The registers the panic path saves for a CPU that stops (DESIGN §2.5
// step 1): where it is, its frame pointer, and its flags.
#[cfg(target_arch = "x86_64")]
pub use super::x86_64::cpu::{
    read_rbp as frame_pointer, read_rip as instruction_pointer, rflags as irq_flags,
};

/// The test and `panic_exit` builds' end of a QEMU run.
#[cfg(all(
    target_arch = "x86_64",
    any(feature = "kernel_tests", feature = "panic_exit")
))]
pub use super::x86_64::cpu::qemu_exit;

/// The per-CPU base register's port module (`PerCpuBase`'s fast path, the
/// base install and the hardware CPU id).
#[cfg(target_arch = "x86_64")]
pub use super::x86_64::percpu;

/// The IRQ-off exclusive cell over this build's port (DESIGN §2.3).
pub type IrqCell<T> = vibeos::cell::IrqCell<T, Arch>;

/// The split virtqueue over this build's port's barriers.
pub type SplitQueue = vibeos::virtio::SplitQueue<Arch>;

/// The page-table walker over this build's port's format.
pub type Mapper = vibeos::mm::paging::Mapper<Arch>;

/// A user address space over this build's port's page tables.
pub type AddressSpace = vibeos::proc::addr_space::AddressSpace<Arch>;

/// Compile-time conformance: the port implements every seam trait.
const _: () = vibeos::arch::assert_port::<Arch>();
