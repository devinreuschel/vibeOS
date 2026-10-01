//! `/sbin/init` (ROADMAP §9.8, §10.5): pid 1. It first checks that it
//! started with the psABI's initial FP state (ROADMAP §10.6, F129), printing
//! `init: fp initial ok` or `init: fp initial wrong <state>`. It checks every `fork`,
//! `execve` and `wait4` result, and reports each failure on fd 2 in one
//! `write` (BOOT.md §3.3).
//!
//! First it runs `/bin/tests` and waits for it, printing
//! `init: /bin/tests exited <status>` when the status word is nonzero.
//! Then it starts `/bin/sh` and reaps until the shell ends, reaping any
//! orphan on the way, then yields and starts it again. A failed start, a
//! `fork` error or a shell child that exits 127 (what the child exits with
//! when its `execve` fails; a shell on the console never exits 127, since a
//! console read never ends the input), counts toward three in a row, after
//! which init exits 1: its exit panics the kernel (INVARIANTS.md §2.5).
//! Both children get init's environment.

#![no_std]
#![no_main]

use vibeos_user::arch;
use vibeos_user::cmd::{self, Out, Status};
use vibeos_user::env::Env;
use vibeos_user::sys::{self, Errno};
use vibeos_user::{rt, utest};

vibeos_user::main!(main);

const TESTS: &[u8] = b"/bin/tests\0";
const SH: &[u8] = b"/bin/sh\0";
const EXITED: &[u8] = b"init: /bin/tests exited ";

/// Failed `/bin/sh` starts in a row after which init exits.
const SH_STARTS: u32 = 3;

fn main(env: &Env) -> i32 {
    check_initial_fp();
    // A NULL `envp` is an empty environment, if the copy cannot be made.
    let vars = cmd::envp(env).unwrap_or_default();
    let envp = if vars.is_empty() {
        core::ptr::null()
    } else {
        vars.as_ptr()
    };
    tests(envp);
    let mut failed = 0;
    loop {
        match start(SH, envp, b"init: /bin/sh start failed: execve errno ") {
            Err(e) => {
                diag(b"init: /bin/sh start failed: fork errno ", e);
                failed += 1;
            }
            Ok(pid) => loop {
                match cmd::wait(-1, 0) {
                    Err(e) => {
                        diag(b"init: wait4: errno ", e);
                        break;
                    }
                    // An orphan init reaped (C-REAPER).
                    Ok((p, _)) if p != pid => {}
                    Ok((_, st)) if Status::of(st) == Status::Exited(127) => {
                        failed += 1;
                        break;
                    }
                    Ok((_, st)) => {
                        line(b"init: /bin/sh ended: ", u64::from(st));
                        failed = 0;
                        break;
                    }
                }
            },
        }
        if failed == SH_STARTS {
            return 1;
        }
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a yield has no failure init can act on"
        )]
        let _ = sys::sched_yield();
    }
}

/// Run `/bin/tests` and wait for it.
fn tests(envp: *const *const u8) {
    match start(TESTS, envp, b"init: /bin/tests start failed: execve errno ") {
        Err(e) => diag(b"init: /bin/tests start failed: fork errno ", e),
        Ok(pid) => match cmd::wait(pid as i32, 0) {
            Ok((_, 0)) => {}
            Ok((_, status)) => report(status),
            Err(e) => diag(b"init: wait4: errno ", e),
        },
    }
}

/// Fork a child that execs `path` (NUL-terminated) with argv `[path]` and
/// `envp`; a child whose `execve` returns writes `failed` and the errno,
/// and exits 127. The child's pid.
fn start(path: &[u8], envp: *const *const u8, failed: &[u8]) -> Result<usize, Errno> {
    match utest::fork()? {
        0 => {
            let argv = [path.as_ptr(), core::ptr::null()];
            if let Err(e) = sys::execve(path.as_ptr(), argv.as_ptr(), envp) {
                diag(failed, e);
            }
            rt::exit(127)
        }
        pid => Ok(pid),
    }
}

/// `head` and `e`'s number.
fn diag(head: &[u8], e: Errno) {
    line(head, u64::from(e.0.unsigned_abs()));
}

/// `head`, `n` in decimal and a newline, in one write to fd 2.
fn line(head: &[u8], n: u64) {
    let mut o = Out::new(2);
    let r = (|| o.put(head)?.dec(n)?.put(b"\n")?.flush())();
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5: init has nowhere else to report a failed write"
    )]
    let _ = r;
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
