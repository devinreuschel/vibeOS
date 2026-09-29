//! User-memory copies on x86_64: the `UserAccess` seam (INTERRUPTS §5.1,
//! ROADMAP §10.6).
//!
//! Each copy is one `rep movsb` inside `stac`/`clac` when SMAP is live (a
//! CPU without SMAP raises `#UD` on both, so the other form has neither),
//! with one `__ex_table` record whose fixup is the instruction after it. A
//! `#PF` on the `rep movsb` at CPL 0 with CR2 in the user half resumes there
//! ([`fixup`]) with RCX holding the bytes left, which the method returns.

use core::arch::asm;

use vibeos::arch::UserAccess;
use vibeos::paging::USER_MAP_END;
use vibeos::proc::uaccess::{ExEntry, ExKind, search, user_range_ok};

use super::Arch;
use super::cpu as x86;

/// Until ROADMAP §10.6's identity-teardown box removes the GLOBAL,
/// supervisor, writable identity map of VA 0 to 512 MiB, a user address
/// there can reach low physical memory the process never mapped, so the
/// accessors refuse any range that starts below it.
const LOW_IDENTITY_END: u64 = 512 << 20;

/// The range check the accessors repeat: the pure user-range rule, and the
/// temporary low-identity refusal.
fn accept(addr: u64, len: usize) -> bool {
    user_range_ok(addr, len as u64) && addr >= LOW_IDENTITY_END
}

/// `len` bytes from `src` to `dst` with `rep movsb`; the bytes not copied.
///
/// # Safety
///
/// One of `src..src + len` and `dst..dst + len` is kernel memory the caller
/// may read or write, and the other is a user range `accept` passed.
unsafe fn movsb(dst: u64, src: u64, len: usize) -> usize {
    let left: usize;
    if x86::smap_live() {
        // SAFETY: `stac` and `clac` are defined because `smap_live` is set
        // only once CR4.SMAP is on; the kernel side of the copy is memory
        // the caller may access and the user side is a checked user range
        // whose fault the `__ex_table` record resumes at `3:`, this fn's
        // `# Safety`; established at `arch::x86_64::cpu::init_control_regs`
        // and by the caller.
        unsafe {
            asm!(
                "stac",
                "2: rep movsb",
                "3: clac",
                ".pushsection __ex_table, \"a\"",
                ".balign 4",
                ".long 2b - .",
                ".long 3b - .",
                ".long 0",
                ".popsection",
                inout("rcx") len => left,
                inout("rdi") dst => _,
                inout("rsi") src => _,
                options(nostack),
            );
        }
    } else {
        // SAFETY: the kernel side of the copy is memory the caller may
        // access and the user side is a checked user range whose fault the
        // `__ex_table` record resumes at `3:`, this fn's `# Safety`;
        // established by `arch::x86_64::uaccess::accept` in its callers.
        unsafe {
            asm!(
                "2: rep movsb",
                "3:",
                ".pushsection __ex_table, \"a\"",
                ".balign 4",
                ".long 2b - .",
                ".long 3b - .",
                ".long 0",
                ".popsection",
                inout("rcx") len => left,
                inout("rdi") dst => _,
                inout("rsi") src => _,
                options(nostack),
            );
        }
    }
    left
}

impl UserAccess for Arch {
    /// Refuses, copying nothing, a range `user_range_ok` refuses or one
    /// that starts below 512 MiB.
    unsafe fn copy_in(dst: *mut u8, src: u64, len: usize) -> usize {
        if !accept(src, len) {
            return len;
        }
        // SAFETY: `dst` is `len` bytes the caller may write, the `# Safety`
        // contract of `vibeos::arch::UserAccess::copy_in`, and `accept`
        // passed `src..src + len`; established here.
        unsafe { movsb(dst as usize as u64, src, len) }
    }

    /// Refuses, copying nothing, a range `user_range_ok` refuses or one
    /// that starts below 512 MiB.
    unsafe fn copy_out(dst: u64, src: *const u8, len: usize) -> usize {
        if !accept(dst, len) {
            return len;
        }
        // SAFETY: `src` is `len` bytes the caller may read, the `# Safety`
        // contract of `vibeos::arch::UserAccess::copy_out`, and `accept`
        // passed `dst..dst + len`; established here.
        unsafe { movsb(dst, src as usize as u64, len) }
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
    // `linker.ld` places the section read-only between `__ex_table_start`
    // and `__ex_table_end`, so the range holds `n` initialized entries
    // that nothing writes; established here (`movsb`) and by `linker.ld`.
    unsafe { core::slice::from_raw_parts(start, n) }
}

/// Where a CPL-0 `#PF` at `rip` with fault address `cr2` resumes: the
/// fixup of the accessor instruction at `rip`, or `None` when `cr2` is
/// outside the user half or `rip` is no accessor's.
pub(crate) fn fixup(rip: u64, cr2: u64) -> Option<u64> {
    if cr2 >= USER_MAP_END {
        return None;
    }
    match search(table(), rip)? {
        (f, ExKind::Faulting) => Some(f),
        (_, ExKind::NonFaulting) => None,
    }
}
