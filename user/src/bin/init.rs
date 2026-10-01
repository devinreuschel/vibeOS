//! `/sbin/init` (ROADMAP §9.8, §10.5): pid 1. It first checks that it
//! started with the psABI's initial FP state (ROADMAP §10.6, F129), printing
//! `init: fp initial ok` or `init: fp initial wrong <state>`. It runs `/bin/tests` and
//! waits for it, printing `init: /bin/tests exited <status>` on fd 2 when
//! the status word is nonzero; then it starts `/bin/sh` and reaps orphans
//! forever. Its exit would panic the kernel (INVARIANTS.md §2.5).

#![no_std]
#![no_main]

use vibeos_user::arch;
use vibeos_user::env::Env;
use vibeos_user::rt;
use vibeos_user::sys::{self, Errno};

vibeos_user::main!(main);

const TESTS: &[u8] = b"/bin/tests\0";
const SH: &[u8] = b"/bin/sh\0";
const EXITED: &[u8] = b"init: /bin/tests exited ";

/// `fork`, out of line, so no vector register is live across it (F069).
#[inline(never)]
fn fork() -> Result<usize, Errno> {
    sys::fork()
}

/// Fork a child that execs `path` with argv `[path]`; a child whose
/// `execve` returns exits 127. The child's pid, or `None` if `fork` failed.
fn spawn(path: &[u8]) -> Option<usize> {
    match fork() {
        Ok(0) => {
            let argv = [path.as_ptr(), core::ptr::null()];
            #[expect(
                clippy::let_underscore_must_use,
                reason = "an execve that returns failed; status 127 reports it"
            )]
            let _ = sys::execve(path.as_ptr(), argv.as_ptr(), core::ptr::null());
            rt::exit(127)
        }
        Ok(pid) => Some(pid),
        Err(_) => None,
    }
}

fn main(_env: &Env) -> i32 {
    check_initial_fp();
    if let Some(pid) = spawn(TESTS) {
        let mut status = 0i32;
        // SAFETY: `wait4` writes 4 bytes through `&raw mut status`, a local
        // no reference covers, and nothing through the null rusage;
        // established here.
        let _waited = unsafe { sys::wait4(pid as i32, &raw mut status, 0, core::ptr::null_mut()) };
        if status != 0 {
            report(status as u32);
        }
    }
    let _sh = spawn(SH);
    loop {
        // SAFETY: a null status and rusage, so the kernel writes nothing;
        // established here.
        if unsafe { sys::wait4(-1, core::ptr::null_mut(), 0, core::ptr::null_mut()) }.is_err() {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a yield has no failure init can act on"
            )]
            let _ = sys::sched_yield();
        }
    }
}

/// `init: fp initial ok` when `_start` found FCW `0x037F`, MXCSR `0x1F80`
/// and the first vector register zero, else `init: fp initial wrong fcw
/// <hex> mxcsr <hex> xmm0 <hex>`, in one write to fd 2.
fn check_initial_fp() {
    let fp = arch::initial_fp();
    let mut buf = [0u8; 96];
    let mut w = Line {
        buf: &mut buf,
        len: 0,
    };
    if fp.is_initial() {
        w.push(b"init: fp initial ok\n");
    } else {
        w.push(b"init: fp initial wrong fcw ");
        w.hex(u64::from(fp.fcw), 4);
        w.push(b" mxcsr ");
        w.hex(u64::from(fp.mxcsr), 8);
        w.push(b" xmm0 ");
        for &b in fp.xmm0.iter().rev() {
            w.hex(u64::from(b), 2);
        }
        w.push(b"\n");
    }
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5: init has nowhere else to report a failed write"
    )]
    let _ = sys::write(2, w.buf.as_ptr(), w.len);
}

/// A line built in a fixed buffer; bytes past its end are dropped.
struct Line<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl Line<'_> {
    fn push(&mut self, s: &[u8]) {
        for &b in s {
            if let Some(slot) = self.buf.get_mut(self.len) {
                *slot = b;
                self.len += 1;
            }
        }
    }

    /// `v` as `digits` lowercase hex digits.
    fn hex(&mut self, v: u64, digits: u32) {
        for i in (0..digits).rev() {
            let d = ((v >> (i * 4)) & 0xF) as u8;
            self.push(&[if d < 10 { b'0' + d } else { b'a' + d - 10 }]);
        }
    }
}

/// `init: /bin/tests exited <status>`, the status word in decimal, in one
/// write to fd 2 so the line stays whole.
fn report(status: u32) {
    let mut buf = [0u8; 48];
    buf[..EXITED.len()].copy_from_slice(EXITED);
    let mut len = EXITED.len();
    let mut digits = [0u8; 10];
    let mut i = digits.len();
    let mut v = status;
    loop {
        i -= 1;
        digits[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let d = &digits[i..];
    buf[len..len + d.len()].copy_from_slice(d);
    len += d.len();
    buf[len] = b'\n';
    len += 1;
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5: init has nowhere else to report a failed write"
    )]
    let _ = sys::write(2, buf.as_ptr(), len);
}
