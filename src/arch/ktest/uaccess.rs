//! In-guest SMAP and SMEP tests for arch (kernel_tests only): stray kernel
//! accesses to user pages (exit-gate line 1150). Rows: the list in
//! crate::ktest.

use vibeos::addr_space::UserPerms;
use vibeos::paging::PAGE_SIZE_4K;

use super::{PF_PRESENT, settled_frames};
use crate::addr_space_init;
use crate::ktest::Outcome;
use crate::ktest::user;
use crate::x86;

// Outside the
// user accessors' `stac`/`clac` window, SMAP faults a CPL-0 read or write of
// a user page, and SMEP faults a CPL-0 fetch from one.

/// A user page mapped RW for the SMAP test, and one mapped RX holding `ret`
/// for the SMEP test.
const STRAY_RW_VA: u64 = 0x5000_0000;
const STRAY_RX_VA: u64 = 0x5000_1000;

const PF_WRITE: u64 = 1 << 1;
const PF_USER: u64 = 1 << 2;
const PF_FETCH: u64 = 1 << 4;

/// A throwaway address space with `perms` at `va`, its page's first byte
/// `byte` (written through the physmap), loaded with IF=0 while `probe`
/// runs; then the kernel CR3 again and the space torn down. `Err` names
/// the step that could not build the space.
fn with_stray_page<R>(
    va: u64,
    perms: UserPerms,
    byte: u8,
    probe: impl FnOnce() -> R,
) -> Result<R, &'static str> {
    let Some(mut space) = addr_space_init::create() else {
        return Err("create");
    };
    // SAFETY: invariant: `space` is a fresh address space that no CPU has
    // loaded, and the range is page-aligned user space; established here.
    if unsafe { addr_space_init::map_anon(&mut space, va, PAGE_SIZE_4K, perms) }.is_err() {
        addr_space_init::teardown(space);
        return Err("map_anon");
    }
    if space.write_bytes(va, &[byte]).is_err() {
        addr_space_init::teardown(space);
        return Err("fill");
    }
    let r = {
        let _g = x86::InterruptGuard::enter();
        crate::proc::ktest::load_cr3(&space);
        x86::invlpg(va);
        let r = probe();
        addr_space_init::load_kernel_cr3();
        r
    };
    addr_space_init::teardown(space);
    Ok(r)
}

/// `None` when `f` is a present, supervisor `#PF` at `va` whose write and
/// fetch bits are `write` and `fetch`.
fn stray_verdict(
    what: &str,
    f: Option<crate::ktest::Fault>,
    va: u64,
    write: bool,
    fetch: bool,
) -> Option<Outcome> {
    let Some(f) = f else {
        return Some(crate::fail_fmt!("{what}: no fault"));
    };
    let ok = f.cr2 == va
        && f.error & PF_PRESENT != 0
        && f.error & PF_USER == 0
        && (f.error & PF_WRITE != 0) == write
        && (f.error & PF_FETCH != 0) == fetch;
    if ok {
        None
    } else {
        Some(crate::fail_fmt!(
            "{what}: cr2={:#x} err={:#x}, want {va:#x} P=1 U=0 W={} I/D={}",
            f.cr2,
            f.error,
            u8::from(write),
            u8::from(fetch)
        ))
    }
}

pub(crate) fn test_uaccess_smap_stray_fault() -> Outcome {
    if x86::read_cr4() & x86::CR4_SMAP == 0 {
        return Outcome::Skip("cpu: CR4.SMAP clear");
    }
    let before = settled_frames();
    let r = with_stray_page(STRAY_RW_VA, UserPerms::RW, 0x5A, || {
        // SAFETY: invariant: `STRAY_RW_VA` is a present user page in the
        // loaded space and AC is clear, so the read is a SMAP `#PF` that
        // `catch_fault` catches, or a plain read if SMAP let it through;
        // established by `arch::ktest::uaccess::with_stray_page`.
        let read = crate::ktest::catch_fault(|| unsafe {
            core::ptr::read_volatile(STRAY_RW_VA as *const u8);
        });
        // SAFETY: as for the read above; the page is writable and the
        // space is torn down after, so a write that got through touches
        // only it; established by `arch::ktest::uaccess::with_stray_page`.
        let write = crate::ktest::catch_fault(|| unsafe {
            core::ptr::write_volatile(STRAY_RW_VA as *mut u8, 0xA5);
        });
        (read, write)
    });
    let (read, write) = match r {
        Ok(rw) => rw,
        Err(e) => return Outcome::Fail(e),
    };
    if let Some(fail) = stray_verdict("read", read, STRAY_RW_VA, false, false) {
        return fail;
    }
    if let Some(fail) = stray_verdict("write", write, STRAY_RW_VA, true, false) {
        return fail;
    }
    if !user::frames_settle(before) {
        return crate::fail_fmt!("frame leak: {} -> {}", before, crate::ktest::free_frames());
    }
    Outcome::Ok
}

pub(crate) fn test_uaccess_smep_user_jump() -> Outcome {
    if x86::read_cr4() & x86::CR4_SMEP == 0 {
        return Outcome::Skip("cpu: CR4.SMEP clear");
    }
    let before = settled_frames();
    // 0xC3 is `ret`: a call that SMEP let through returns at once.
    let r = with_stray_page(STRAY_RX_VA, UserPerms::RX, 0xC3, || {
        // SAFETY: invariant: `STRAY_RX_VA` is a present, executable user
        // page in the loaded space holding `ret`, so the call is a SMEP
        // `#PF` that `catch_fault` catches, or a call that returns at once
        // if SMEP let it through; established by
        // `arch::ktest::uaccess::with_stray_page`.
        crate::ktest::catch_fault(|| unsafe {
            core::arch::asm!("call {0}", in(reg) STRAY_RX_VA, clobber_abi("C"));
        })
    });
    let fetch = match r {
        Ok(f) => f,
        Err(e) => return Outcome::Fail(e),
    };
    if let Some(fail) = stray_verdict("fetch", fetch, STRAY_RX_VA, false, true) {
        return fail;
    }
    if !user::frames_settle(before) {
        return crate::fail_fmt!("frame leak: {} -> {}", before, crate::ktest::free_frames());
    }
    Outcome::Ok
}
