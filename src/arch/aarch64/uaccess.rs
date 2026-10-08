//! User-memory copies on aarch64: the `UserAccess` seam (INTERRUPTS §5.1,
//! ROADMAP §11.6).
//!
//! Each copy is a byte loop with PAN cleared only for the load/store, and
//! one `__ex_table` record per accessor instruction. A data abort at EL1
//! with FAR in the user half resumes at the fixup with the bytes left.

use core::arch::asm;

use vibeos::arch::UserAccess;
use vibeos::paging::USER_MAP_END;
use vibeos::proc::uaccess::{ExEntry, ExKind, search, user_range_ok};

use super::Arch;
#[cfg(all(feature = "kernel_tests", target_arch = "aarch64"))]
use super::cpu;

/// The range check the accessors repeat: the pure user-range rule.
fn accept(addr: u64, len: usize) -> bool {
    user_range_ok(addr, len as u64)
}

/// Run `f` with PAN cleared, then set it again: the window an in-guest
/// test needs to touch a user page from EL1 (`kernel_tests` only).
#[cfg(all(feature = "kernel_tests", target_arch = "aarch64"))]
pub(crate) fn with_window<R>(f: impl FnOnce() -> R) -> R {
    cpu::clear_pan();
    let r = f();
    cpu::set_pan();
    r
}

/// `len` bytes from `src` to `dst`; the bytes not copied.
///
/// # Safety
///
/// One of `src..src + len` and `dst..dst + len` is kernel memory the caller
/// may read or write, and the other is a user range `accept` passed.
unsafe fn copy_bytes(dst: u64, src: u64, len: usize) -> usize {
    let left: usize;
    // SAFETY: PAN is the ISA floor (ROADMAP §11.1); the kernel side of the
    // copy is memory the caller may access and the user side is a checked
    // user range whose fault the `__ex_table` records resume at `3:`, this
    // fn's `# Safety`; established by `arch::aarch64::cpu::clear_pan` and
    // by the caller.
    unsafe {
        asm!(
            "msr pan, #0",
            "cbz {len}, 3f",
            "2:",
            "ldrb {tmp:w}, [{src}]",
            "strb {tmp:w}, [{dst}]",
            "add {src}, {src}, #1",
            "add {dst}, {dst}, #1",
            "sub {len}, {len}, #1",
            "cbnz {len}, 2b",
            "3:",
            "msr pan, #1",
            ".pushsection __ex_table, \"a\"",
            ".balign 4",
            ".long 2b - .",
            ".long 3b - .",
            ".long 0",
            ".long 2b + 4 - .",
            ".long 3b - .",
            ".long 0",
            ".popsection",
            dst = inout(reg) dst => _,
            src = inout(reg) src => _,
            len = inout(reg) len => left,
            tmp = out(reg) _,
            options(nostack),
        );
    }
    left
}

impl UserAccess for Arch {
    /// Refuses, copying nothing, a range `user_range_ok` refuses.
    unsafe fn copy_in(dst: *mut u8, src: u64, len: usize) -> usize {
        if !accept(src, len) {
            return len;
        }
        // SAFETY: `dst` is `len` bytes the caller may write, the `# Safety`
        // contract of `vibeos::arch::UserAccess::copy_in`, and `accept`
        // passed `src..src + len`; established here.
        unsafe { copy_bytes(dst as usize as u64, src, len) }
    }

    /// Refuses, copying nothing, a range `user_range_ok` refuses.
    unsafe fn copy_out(dst: u64, src: *const u8, len: usize) -> usize {
        if !accept(dst, len) {
            return len;
        }
        // SAFETY: `src` is `len` bytes the caller may read, the `# Safety`
        // contract of `vibeos::arch::UserAccess::copy_out`, and `accept`
        // passed `dst..dst + len`; established here.
        unsafe { copy_bytes(dst, src as usize as u64, len) }
    }
}

unsafe extern "C" {
    static __ex_table_start: [ExEntry; 0];
    static __ex_table_end: [ExEntry; 0];
}

/// The exception table the linker gathers into `__ex_table`.
fn table() -> &'static [ExEntry] {
    let start = (&raw const __ex_table_start).cast::<ExEntry>();
    let end = (&raw const __ex_table_end).cast::<ExEntry>();
    let bytes = (end as usize).saturating_sub(start as usize);
    let n = bytes / core::mem::size_of::<ExEntry>();
    // SAFETY: invariant: only this module emits into `__ex_table`, each
    // record three 4-aligned `.long`s laid out as `ExEntry`, and
    // `linker-aarch64.ld` places the section read-only between
    // `__ex_table_start` and `__ex_table_end`, so the range holds `n`
    // initialized entries that nothing writes; established here
    // (`copy_bytes`) and by `linker-aarch64.ld`.
    unsafe { core::slice::from_raw_parts(start, n) }
}

/// Where an EL1 data abort at `elr` with fault address `far` resumes: the
/// fixup of the accessor instruction at `elr`, or `None` when `far` is
/// outside the user half or `elr` is no accessor's.
pub(crate) fn fixup(elr: u64, far: u64) -> Option<u64> {
    if far >= USER_MAP_END {
        return None;
    }
    match search(table(), elr)? {
        (f, ExKind::Faulting) => Some(f),
        (_, ExKind::NonFaulting) => None,
    }
}
