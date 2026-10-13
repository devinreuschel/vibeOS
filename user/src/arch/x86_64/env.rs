//! Ring-3 environment of DESIGN §11.4 (ROADMAP §11.6): `rdtsc` and `cpuid`
//! run, `rdpmc` is `SIGSEGV`.

use core::arch::asm;

use crate::rt;
use crate::sys;
use crate::utest::{self, Outcome};

const SIGSEGV: u32 = 11;

/// Run the ring-3 environment checks.
pub fn user_env() -> Outcome {
    if !rdtsc_ok() {
        return Outcome::Fail("rdtsc");
    }
    if !cpuid_ok() {
        return Outcome::Fail("cpuid");
    }
    match child_rdpmc() {
        Ok(st) if st & 0x7f == SIGSEGV => Outcome::Ok,
        Ok(_) => Outcome::Fail("rdpmc"),
        Err(_) => Outcome::Fail("fork"),
    }
}

fn rdtsc_ok() -> bool {
    let mut lo: u32;
    let mut hi: u32;
    // SAFETY: `rdtsc` is usable at ring 3 (DESIGN §11.4); established here.
    unsafe {
        asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags));
    }
    let _ = (lo, hi);
    true
}

fn cpuid_ok() -> bool {
    let mut a: u32;
    let mut b: u32;
    let mut c: u32;
    let mut d: u32;
    // SAFETY: `cpuid` of leaf 0 is usable at ring 3 (DESIGN §11.4);
    // established here.
    unsafe {
        asm!(
            "mov {b_save:r}, rbx",
            "cpuid",
            "xchg {b_save:r}, rbx",
            inout("eax") 0u32 => a,
            out("ecx") c,
            out("edx") d,
            b_save = out(reg) b,
            options(nomem, nostack, preserves_flags),
        );
    }
    let _ = (a, b, c, d);
    true
}

fn child_rdpmc() -> Result<u32, ()> {
    let pid = match utest::fork() {
        Ok(0) => {
            let mut lo: u32;
            let mut hi: u32;
            // SAFETY: `rdpmc` is meant to #GP; established here.
            unsafe {
                asm!(
                    "xor ecx, ecx",
                    "rdpmc",
                    out("eax") lo,
                    out("edx") hi,
                    out("ecx") _,
                    options(nomem, nostack),
                );
            }
            rt::exit((lo | hi) as i32)
        }
        Ok(pid) => pid,
        Err(_) => return Err(()),
    };
    let mut status = 0i32;
    // SAFETY: `wait4` writes 4 bytes through a local; established here.
    let r = unsafe { sys::wait4(pid as i32, &raw mut status, 0, core::ptr::null_mut()) };
    if r != Ok(pid) {
        return Err(());
    }
    Ok(status as u32)
}
