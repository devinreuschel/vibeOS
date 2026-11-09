//! EL0 environment of DESIGN §11.4 (ROADMAP §11.6): readable ID registers,
//! cache ops, `wfe`/`wfi`, and the trapped system registers, `SCXTNUM_EL0`
//! included.

use core::arch::asm;

use crate::rt;
use crate::sys;
use crate::utest::{self, Outcome};

const SIGILL: u32 = 4;

/// Run the EL0 environment checks.
pub fn user_env() -> Outcome {
    if !id_regs_ok() {
        return Outcome::Fail("id regs");
    }
    if !cache_ops_ok() {
        return Outcome::Fail("cache ops");
    }
    let traps = [
        ("daifset", trap_daifset as fn()),
        ("cntpct", trap_cntpct as fn()),
        ("cntv_ctl", trap_cntv_ctl as fn()),
        ("cntp_ctl", trap_cntp_ctl as fn()),
        ("pmccntr", trap_pmccntr as fn()),
        // TSCXT traps this where FEAT_CSV2_2 is present. Without that
        // feature the encoding is unallocated, so it still raises SIGILL.
        ("scxtnum", trap_scxtnum as fn()),
    ];
    for (name, f) in traps {
        match child_trap(f) {
            Ok(st) if st & 0x7f == SIGILL => {}
            Ok(_) => return Outcome::Fail(name),
            Err(_) => return Outcome::Fail("fork"),
        }
    }
    Outcome::Ok
}

fn id_regs_ok() -> bool {
    let mut ctr: u64;
    let mut dczid: u64;
    let mut cntvct: u64;
    let mut cntfrq: u64;
    // SAFETY: these registers are readable at EL0 (DESIGN §11.4);
    // established here.
    unsafe {
        asm!("mrs {0}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, dczid_el0", out(reg) dczid, options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, cntvct_el0", out(reg) cntvct, options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, cntfrq_el0", out(reg) cntfrq, options(nomem, nostack, preserves_flags));
    }
    let _ = (ctr, cntvct);
    dczid & (1 << 4) == 0 && cntfrq != 0
}

/// Sentinel outside the block `dc zva` zeroes.
const CACHE_SENTINEL: u8 = 0xa5;

const CACHE_LEN: usize = 4096;

/// Page-sized and page-aligned, so every aligned `dc zva` block (at most
/// 2 KiB, DCZID_EL0.BS) that holds an address in it lies inside it.
#[repr(C, align(4096))]
struct CacheBuf([u8; CACHE_LEN]);

fn cache_ops_ok() -> bool {
    let dczid: u64;
    // SAFETY: DCZID_EL0 is readable at EL0 (DESIGN §11.4); established here.
    unsafe {
        asm!("mrs {0}, dczid_el0", out(reg) dczid, options(nomem, nostack, preserves_flags));
    }
    // DCZID_EL0.DZP (bit 4) set makes `dc zva` trap, so the op below would
    // kill this check with SIGILL instead of reporting it (Arm ARM DDI0487).
    if dczid & (1 << 4) != 0 {
        return false;
    }
    // DCZID_EL0.BS is log2 of the block size in words (Arm ARM DDI0487).
    let block = 4usize << (dczid & 0xf);
    if block > CACHE_LEN {
        return false;
    }
    let mut buf = CacheBuf([CACHE_SENTINEL; CACHE_LEN]);
    // An address inside the buffer, off any block boundary.
    let off = 2048 + 37;
    let start = off & !(block - 1);
    let p = buf.0.as_mut_ptr().wrapping_add(off);
    // SAFETY: `p` is inside `buf`, and each op touches only the line or
    // block containing `p`, which the aligned buffer holds whole; the ops
    // do not fault at EL0; established here.
    unsafe {
        asm!("dc zva, {0}", in(reg) p, options(nostack));
        asm!("dc cvau, {0}", in(reg) p, options(nostack));
        asm!("ic ivau, {0}", in(reg) p, options(nostack));
        asm!("wfe", options(nomem, nostack, preserves_flags));
        asm!("wfi", options(nomem, nostack, preserves_flags));
    }
    let bytes = &buf.0;
    let zeroed = bytes[start..start + block].iter().all(|&b| b == 0);
    let kept = bytes[..start].iter().all(|&b| b == CACHE_SENTINEL)
        && bytes[start + block..].iter().all(|&b| b == CACHE_SENTINEL);
    zeroed && kept
}

fn child_trap(f: fn()) -> Result<u32, ()> {
    let pid = match utest::fork() {
        Ok(0) => {
            f();
            rt::exit(0)
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

fn trap_daifset() {
    // SAFETY: the instruction is meant to trap; established here.
    unsafe { asm!("msr daifset, #2", options(nostack)) };
}

fn trap_cntpct() {
    let mut v: u64;
    // SAFETY: the instruction is meant to trap; established here.
    unsafe { asm!("mrs {0}, cntpct_el0", out(reg) v, options(nomem, nostack)) };
    let _ = v;
}

fn trap_cntv_ctl() {
    // SAFETY: the instruction is meant to trap; established here.
    unsafe { asm!("msr cntv_ctl_el0, xzr", options(nomem, nostack)) };
}

fn trap_cntp_ctl() {
    // SAFETY: the instruction is meant to trap; established here.
    unsafe { asm!("msr cntp_ctl_el0, xzr", options(nomem, nostack)) };
}

fn trap_pmccntr() {
    let mut v: u64;
    // SAFETY: the instruction is meant to trap; established here.
    unsafe { asm!("mrs {0}, pmccntr_el0", out(reg) v, options(nomem, nostack)) };
    let _ = v;
}

fn trap_scxtnum() {
    let mut v: u64;
    // `S3_3_C13_C0_7` is `SCXTNUM_EL0` (Arm ARM DDI0487). The baseline
    // assembler accepts that encoding.
    // SAFETY: the instruction is meant to trap (DESIGN §11.4); established here.
    unsafe { asm!("mrs {0}, S3_3_C13_C0_7", out(reg) v, options(nomem, nostack)) };
    let _ = v;
}
