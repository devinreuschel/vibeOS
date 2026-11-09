//! The exit work on every return to user mode (DESIGN §5.10 rule 11): one
//! implementation that both ports' exits call, through
//! `syscall_init::exit_work`.

use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::arch::current::{UserFrame, body_mask, body_unmask, interrupts_enabled};

/// [`exit_work`]'s `kind` for the syscall exit (and a new thread's first
/// return); an x86_64 vector exit passes its vector, below this.
pub const EXIT_SYSCALL: u64 = 0x100;

/// The process layer's exit-work hooks (DESIGN §1.2): whether the current
/// process has a kill or stop to act on, read with IRQs masked, and the
/// work itself, run with them unmasked. `proc_init::init` sets both before
/// the first user entry; unset, no exit has work.
static EXIT_PENDING: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());
static EXIT_WORK: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Install the exit-work hooks.
pub fn set_exit_work_hooks(pending: fn() -> bool, work: fn(&mut UserFrame)) {
    // Release: pairs with the Acquire loads in `exit_work`.
    EXIT_WORK.store(work as *mut (), Ordering::Release);
    // Release: pairs with the Acquire load in `exit_work`.
    EXIT_PENDING.store(pending as *mut (), Ordering::Release);
}

/// Exit work on a return to user mode (DESIGN §5.10 rule 11): entered with
/// IRQs masked (IF=0 after the x86_64 exit's `cli`, all of DAIF on
/// aarch64); while the current process has a kill or a stop pending,
/// unmask as a syscall body runs, act on it (a kill does not return), mask
/// again, and check again. Returns masked once a check finds none. `kind`
/// is [`EXIT_SYSCALL`] or the x86_64 vector whose exit this is. aarch64's
/// `vibeos_el0_exit` passes [`EXIT_SYSCALL`] for every EL0 exception's
/// return, an IRQ's included, so there `kind` does not tell a syscall exit
/// from another: `exit_check_hook` still keys on the frame's syscall number,
/// which every non-`svc` entry saves as -1, and `exit_work_found` records
/// [`EXIT_SYSCALL`] for any aarch64 exit.
pub fn exit_work(kind: u64, frame: &mut UserFrame) {
    #[cfg(feature = "kernel_tests")]
    crate::proc::ktest::exit_seen(frame);
    // Acquire: pairs with the Release stores in `set_exit_work_hooks`.
    let pending = EXIT_PENDING.load(Ordering::Acquire);
    // Acquire: pairs with the Release store in `set_exit_work_hooks`.
    let work = EXIT_WORK.load(Ordering::Acquire);
    if !pending.is_null() && !work.is_null() {
        // SAFETY: invariant: non-null hooks hold a `fn() -> bool` and a
        // `fn(&mut UserFrame)`; established by
        // `proc::exit_work_init::set_exit_work_hooks`, their only stores.
        let (pending, work) = unsafe {
            (
                core::mem::transmute::<*mut (), fn() -> bool>(pending),
                core::mem::transmute::<*mut (), fn(&mut UserFrame)>(work),
            )
        };
        loop {
            debug_assert!(!interrupts_enabled(), "exit work check with IRQs on");
            if !pending() {
                break;
            }
            #[cfg(feature = "kernel_tests")]
            crate::proc::ktest::exit_work_found(kind);
            body_unmask();
            work(frame);
            body_mask();
        }
    }
    #[cfg(feature = "kernel_tests")]
    if kind == EXIT_SYSCALL {
        crate::proc::ktest::exit_check_hook(frame);
    }
    #[cfg(not(feature = "kernel_tests"))]
    let _ = kind;
}
