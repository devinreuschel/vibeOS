//! FP state across `fork` and `execve` (ROADMAP §10.6, F069): a `fork`
//! child sees the parent's control words and vector registers, and an image
//! `execve` starts sees the psABI's initial state. Each case sets the state
//! and makes the call through `arch::fp_syscall`, so no compiled code runs
//! between the two.

use core::ffi::CStr;
use core::ptr;

use vibeos_user::arch::{FpState, fp_syscall};
use vibeos_user::rt;
use vibeos_user::sys::{self, nr};
use vibeos_user::utest::{self, Outcome, Runner};

const FPCHECK: &CStr = c"/bin/fpcheck";

/// Not the initial state in any field: MXCSR rounds toward zero (`0x7F80`,
/// every exception still masked), FCW rounds toward zero, and the first
/// vector register holds a pattern.
const DIRTY: FpState = FpState {
    fcw: 0x0F7F,
    mxcsr: 0x7F80,
    xmm0: *b"vibeos fp state!",
};

pub fn run(t: &mut Runner) {
    t.case("fp_fork_inherits", fp_fork_inherits);
    t.case("fp_execve_initial", fp_execve_initial);
}

/// Wait for `pid` and return its exit code, or why there is none.
fn reap(pid: usize) -> Result<u8, &'static str> {
    let mut status = 0i32;
    // SAFETY: `wait4` writes 4 bytes through `&raw mut status`, a local no
    // reference covers, and nothing through the null rusage; established here.
    let r = unsafe { sys::wait4(pid as i32, &raw mut status, 0, ptr::null_mut()) };
    if r != Ok(pid) {
        return Err("wait4 did not reap the child");
    }
    sys::exit_code(status as u32).ok_or("child killed by a signal")
}

/// The parent sets [`DIRTY`] and forks in one block; the child exits 0 when
/// it reads [`DIRTY`] back, else 1, and the parent checks its own copy too.
fn fp_fork_inherits() -> Outcome {
    // SAFETY: `fork` writes through no argument; established here.
    let (ret, seen) = unsafe { fp_syscall(&DIRTY, nr::SYS_FORK, 0, 0, 0) };
    if ret == 0 {
        rt::exit(i32::from(seen != DIRTY));
    }
    let Ok(pid) = sys::result(ret) else {
        return Outcome::Fail("fork");
    };
    let parent_ok = seen == DIRTY;
    match reap(pid) {
        Ok(0) if parent_ok => Outcome::Ok,
        Ok(0) => Outcome::Fail("the parent's FP state changed across fork"),
        Ok(1) => Outcome::Fail("the child did not read the parent's FP state"),
        Ok(_) => Outcome::Fail("unexpected child status"),
        Err(why) => Outcome::Fail(why),
    }
}

/// A child sets [`DIRTY`] and execs `/bin/fpcheck` in one block;
/// `/bin/fpcheck` exits 0 only when it starts with the initial state.
fn fp_execve_initial() -> Outcome {
    let pid = match utest::fork() {
        Ok(0) => {
            let argv = [FPCHECK.as_ptr().cast::<u8>(), ptr::null()];
            // SAFETY: `execve` reads the path and the null-terminated
            // `argv`, both live locals, and writes through nothing;
            // established here.
            let (ret, _) = unsafe {
                fp_syscall(
                    &DIRTY,
                    nr::SYS_EXECVE,
                    FPCHECK.as_ptr() as usize,
                    argv.as_ptr() as usize,
                    0,
                )
            };
            rt::exit(100 + sys::result(ret).err().map_or(0, |e| e.0))
        }
        Ok(pid) => pid,
        Err(_) => return Outcome::Fail("fork"),
    };
    match reap(pid) {
        Ok(0) => Outcome::Ok,
        Ok(1) => Outcome::Fail("/bin/fpcheck started with a non-initial FP state"),
        Ok(_) => Outcome::Fail("execve of /bin/fpcheck failed"),
        Err(why) => Outcome::Fail(why),
    }
}
