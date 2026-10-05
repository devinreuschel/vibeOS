//! aarch64 §10.6 user-memory tests (ROADMAP §11.6): PAN stray, PXN jump,
//! and the portable syscall-copy / readonly-EFAULT cases.

use vibeos::addr_space::UserPerms;
use vibeos::paging::PAGE_SIZE_4K;
use vibeos::proc::{wexitstatus, wifexited};

use crate::addr_space_init;
use crate::arch;
use crate::arch::current::InterruptGuard;
use crate::fill_init;
use crate::ktest::user::{self, DEFAULT, Image, Layout, user_code};
use crate::ktest::{Outcome, fid};

const STRAY_RW_VA: u64 = 0x5000_0000;
const STRAY_RX_VA: u64 = 0x5000_1000;
const COPIES: &str = "/tmp/uaccess_syscall_copies";
const READONLY: &str = "/tmp/uaccess_readonly_efault";

const ESR_EC_SHIFT: u32 = 26;
const ESR_EC_MASK: u64 = 0x3F;
const ESR_FSC_MASK: u64 = 0x3F;
const ESR_WNR: u64 = 1 << 6;
const EC_IABT_CUR: u64 = 0x20;
const EC_DABT_CUR: u64 = 0x24;

fn esr_ec(esr: u64) -> u64 {
    (esr >> ESR_EC_SHIFT) & ESR_EC_MASK
}

fn esr_perm(esr: u64) -> bool {
    (esr & ESR_FSC_MASK) & 0b11_1100 == 0b001100
}

fn with_stray_page<R>(
    va: u64,
    perms: UserPerms,
    bytes: &[u8],
    probe: impl FnOnce() -> R,
) -> Result<R, &'static str> {
    let Ok(mut space) = addr_space_init::create() else {
        return Err("create");
    };
    // SAFETY: `space` is a fresh address space that no CPU has loaded,
    // and the range is page-aligned user space; established here.
    if unsafe { addr_space_init::map_anon(&space, va, PAGE_SIZE_4K, perms) }.is_err() {
        return Err("map_anon");
    }
    if fill_init::write(&mut space, va, bytes).is_err() {
        return Err("fill");
    }
    let r = {
        let _g = InterruptGuard::enter();
        // SAFETY: `space.root` is a user TTBR0 `create` built; established here.
        unsafe { addr_space_init::load_cr3_u64(space.root().as_u64()) };
        let r = probe();
        addr_space_init::load_kernel_cr3();
        r
    };
    Ok(r)
}

fn stray_verdict(
    what: &str,
    c: Option<arch::aarch64::catch::Caught>,
    va: u64,
    write: bool,
    fetch: bool,
) -> Option<Outcome> {
    let Some(c) = c else {
        return Some(crate::fail_fmt!("{what}: no fault"));
    };
    let want_ec = if fetch { EC_IABT_CUR } else { EC_DABT_CUR };
    let wnr = c.esr & ESR_WNR != 0;
    let ok = c.far == va
        && esr_ec(c.esr) == want_ec
        && esr_perm(c.esr)
        && wnr == write
        && (!fetch || !wnr);
    if ok {
        None
    } else {
        Some(crate::fail_fmt!(
            "{what}: far={:#x} esr={:#x}, want {va:#x} ec={want_ec:#x} perm W={}",
            c.far,
            c.esr,
            u8::from(write)
        ))
    }
}

/// PAN faults a stray EL1 load or store of a user page (SMAP's counterpart).
pub(crate) fn test_uaccess_smap_stray_fault() -> Outcome {
    if !arch::aarch64::cpu::pan_is_set() {
        return Outcome::Fail("PAN clear");
    }
    let before = crate::ktest::free_frames();
    let r = with_stray_page(STRAY_RW_VA, UserPerms::RW, &[0x5A], || {
        // SAFETY: `STRAY_RW_VA` is a present user page in the loaded space
        // and PAN is set, so the read is a DABT that `catch_dabt` catches,
        // or a plain read if PAN let it through; established by
        // `arch::aarch64::ktest_uaccess::with_stray_page`.
        let read = arch::catch::catch_dabt(|| unsafe {
            core::ptr::read_volatile(STRAY_RW_VA as *const u8);
        });
        // SAFETY: as for the read; a write that got through touches only
        // this throwaway page; established by `with_stray_page`.
        let write = arch::catch::catch_dabt(|| unsafe {
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

/// PXN faults an EL1 fetch from a user page (SMEP's counterpart).
pub(crate) fn test_uaccess_smep_user_jump() -> Outcome {
    if !arch::aarch64::cpu::pan_is_set() {
        return Outcome::Fail("PAN clear");
    }
    let before = crate::ktest::free_frames();
    // `ret` (`0xd65f03c0`): a `blr` that PXN let through returns at once.
    let ret = [0xC0, 0x03, 0x5F, 0xD6];
    let r = with_stray_page(STRAY_RX_VA, UserPerms::RX, &ret, || {
        // SAFETY: `STRAY_RX_VA` is a present, executable user page holding
        // `ret`, so the `blr` is a PXN IABT that `catch_dabt` catches, or
        // a call that returns if PXN let it through; established by
        // `arch::aarch64::ktest_uaccess::with_stray_page`.
        arch::catch::catch_dabt(|| unsafe {
            core::arch::asm!("blr {0}", in(reg) STRAY_RX_VA, clobber_abi("C"));
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

fn run_then_unlink(img: &Image, argv: &[&str], path: &str) -> Result<u32, Outcome> {
    let st = user::run(img, argv);
    let unlinked = fid::unlink_path(path, false);
    let st = st.map_err(|e| crate::fail_fmt!("spawn: {}", e.as_str()))?;
    unlinked.map_err(|e| crate::fail_fmt!("unlink {path}: {}", e.as_str()))?;
    Ok(st)
}

fn uaccess_status(st: u32, names: &[&'static str]) -> Outcome {
    if !wifexited(st) {
        return crate::fail_fmt!("status {st:#x}, want exited");
    }
    match wexitstatus(st) {
        0 => Outcome::Ok,
        code => match names.get(code as usize - 1) {
            Some(n) => Outcome::Fail(n),
            None => crate::fail_fmt!("exit {code}"),
        },
    }
}

user_code!(
    UACCESS_SHORT,
    "
    adr x20, 1f
1:
    bfc x20, #0, #12
    mov x0, #-100
    adr x1, 90f
    mov x2, #0x241
    mov x3, #0x1a4
    mov x8, #56
    svc #0
    mov w19, #1
    tbnz x0, #63, 8f
    mov x21, x0
    mov x0, x21
    mov x1, x20
    mov x2, #300
    mov x8, #64
    svc #0
    mov w19, #2
    cmp x0, #300
    b.ne 8f
    mov x0, x21
    mov x8, #57
    svc #0
    mov x0, #-100
    adr x1, 90f
    mov x2, xzr
    mov x3, xzr
    mov x8, #56
    svc #0
    mov w19, #3
    tbnz x0, #63, 8f
    mov x21, x0
    mov x0, x21
    add x1, x20, #0x1f9c
    mov x2, #256
    mov x8, #63
    svc #0
    mov w19, #4
    cmp x0, #100
    b.ne 8f
    mov x0, x21
    mov x1, xzr
    mov x2, #1
    mov x8, #62
    svc #0
    mov w19, #5
    cmp x0, #100
    b.ne 8f
    mov x0, x20
    add x1, x20, #0x1f9c
    mov x2, #100
    mov w19, #6
10:
    cbz x2, 11f
    ldrb w3, [x0], #1
    ldrb w4, [x1], #1
    cmp w3, w4
    b.ne 8f
    sub x2, x2, #1
    b 10b
11:
    mov x0, #17
    mov x1, xzr
    mov x2, xzr
    mov x3, xzr
    mov x4, xzr
    mov x8, #220
    svc #0
    mov w19, #7
    tbnz x0, #63, 8f
    cbnz x0, 3f
    mov x0, xzr
    mov x8, #93
    svc #0
    brk #0
3:
    mov x0, #-1
    add x1, x20, #0x2000
    mov x2, xzr
    mov x3, xzr
    mov x8, #260
    svc #0
    mov w19, #8
    cmn x0, #14
    b.ne 8f
    mov x0, #-1
    mov x1, xzr
    mov x2, xzr
    mov x3, xzr
    mov x8, #260
    svc #0
    mov w19, #9
    cmn x0, #10
    b.ne 8f
    mov w19, wzr
8:
    mov w0, w19
    mov x8, #93
    svc #0
    brk #0
90:
    .asciz \"/tmp/uaccess_syscall_copies\"
    "
);

pub(crate) fn test_uaccess_syscall_copies() -> Outcome {
    let layout = Layout {
        vaddr: 0x4000_0000,
        memsz: Some(0x2000),
        writable: true,
    };
    let img = Image::Code(UACCESS_SHORT, layout);
    let st = match run_then_unlink(&img, &["uaccess_short"], COPIES) {
        Ok(st) => st,
        Err(fail) => return fail,
    };
    uaccess_status(
        st,
        &[
            "open for write",
            "write 300",
            "open read-only",
            "short read did not return 100",
            "offset after short read is not 100",
            "short read copied the wrong bytes",
            "fork",
            "wait4 to an unmapped status did not return EFAULT",
            "second wait4 did not return ECHILD",
        ],
    )
}

user_code!(
    UACCESS_RO,
    "
    adr x20, 1f
1:
    bfc x20, #0, #12
    sub sp, sp, #512
    mov x0, #-100
    adr x1, 90f
    mov x2, #0x241
    mov x3, #0x1a4
    mov x8, #56
    svc #0
    mov w19, #1
    tbnz x0, #63, 8f
    mov x21, x0
    mov x0, x21
    mov x1, x20
    mov x2, #300
    mov x8, #64
    svc #0
    mov w19, #2
    cmp x0, #300
    b.ne 8f
    mov x0, x21
    mov x8, #57
    svc #0
    mov x0, #-100
    adr x1, 90f
    mov x2, xzr
    mov x3, xzr
    mov x8, #56
    svc #0
    mov w19, #3
    tbnz x0, #63, 8f
    mov x21, x0
    mov x0, x21
    mov x1, x20
    mov x2, #16
    mov x8, #63
    svc #0
    mov w19, #4
    cmn x0, #14
    b.ne 8f
    mov x0, x21
    mov x1, xzr
    mov x2, #1
    mov x8, #62
    svc #0
    mov w19, #5
    cbnz x0, 8f
    mov x0, x20
    mov x1, #64
    mov x8, #500
    svc #0
    mov w19, #6
    cmn x0, #14
    b.ne 8f
    mov x0, #17
    mov x1, xzr
    mov x2, xzr
    mov x3, xzr
    mov x4, xzr
    mov x8, #220
    svc #0
    mov w19, #7
    tbnz x0, #63, 8f
    cbnz x0, 3f
    mov x0, xzr
    mov x8, #93
    svc #0
    brk #0
3:
    mov x0, #-1
    mov x1, x20
    mov x2, xzr
    mov x3, xzr
    mov x8, #260
    svc #0
    mov w19, #8
    cmn x0, #14
    b.ne 8f
    mov x0, x21
    movz x1, #0
    movk x1, #0x8000, lsl #32
    movk x1, #0xffff, lsl #48
    mov x2, #16
    mov x8, #63
    svc #0
    mov w19, #9
    cmn x0, #14
    b.ne 8f
    mov x0, x21
    mov x1, sp
    mov x2, #256
    mov x8, #63
    svc #0
    mov w19, #10
    cmp x0, #256
    b.ne 8f
    mov x0, x20
    mov x1, sp
    mov x2, #256
    mov w19, #11
20:
    cbz x2, 21f
    ldrb w3, [x0], #1
    ldrb w4, [x1], #1
    cmp w3, w4
    b.ne 8f
    sub x2, x2, #1
    b 20b
21:
    mov w19, wzr
8:
    mov w0, w19
    mov x8, #93
    svc #0
    brk #0
90:
    .asciz \"/tmp/uaccess_readonly_efault\"
    "
);

pub(crate) fn test_uaccess_readonly_efault() -> Outcome {
    let img = Image::Code(UACCESS_RO, DEFAULT);
    let st = match run_then_unlink(&img, &["uaccess_ro"], READONLY) {
        Ok(st) => st,
        Err(fail) => return fail,
    };
    uaccess_status(
        st,
        &[
            "open for write",
            "write 300",
            "open read-only",
            "read into own text did not return EFAULT",
            "failed read moved the offset",
            "psinfo into own text did not return EFAULT",
            "fork",
            "wait4 status into own text did not return EFAULT",
            "read into the kernel half did not return EFAULT",
            "read back to the stack",
            "own text changed",
        ],
    )
}
