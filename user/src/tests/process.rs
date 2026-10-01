//! `fork`, `execve`, `wait4` and a fault's signal (ROADMAP §9.8); the
//! process table's limit is `lifecycle`'s. Every fork goes through
//! `utest::fork` (F069).

use vibeos_user::rt;
use vibeos_user::sys;
use vibeos_user::utest::{self, Outcome, Runner};

// From signal(7).
const SIGSEGV: u32 = 11;

const HELLO: &[u8] = b"/hello\0";

pub fn run(t: &mut Runner) {
    t.case("fork_exit_status", fork_exit_status);
    t.case("exec_hello", exec_hello);
    t.case("fork_segv", fork_segv);
}

/// Wait for `pid` and return its status word.
fn wait(pid: usize) -> Result<u32, &'static str> {
    let mut status = 0i32;
    // SAFETY: `wait4` writes 4 bytes through `&raw mut status`, a local no
    // reference covers, and nothing through the null rusage; established here.
    let r = unsafe { sys::wait4(pid as i32, &raw mut status, 0, core::ptr::null_mut()) };
    if r != Ok(pid) {
        return Err("wait4 did not reap the child");
    }
    Ok(status as u32)
}

/// A child that exits 7 reports status 0x0700.
fn fork_exit_status() -> Outcome {
    let pid = match utest::fork() {
        Ok(0) => rt::exit(7),
        Ok(pid) => pid,
        Err(_) => return Outcome::Fail("fork"),
    };
    match wait(pid) {
        Ok(0x0700) => Outcome::Ok,
        Ok(_) => Outcome::Fail("status not 0x0700"),
        Err(why) => Outcome::Fail(why),
    }
}

/// A child that execs `/hello` reports its exit 42: status 0x2A00.
fn exec_hello() -> Outcome {
    let pid = match utest::fork() {
        Ok(0) => {
            let argv = [HELLO.as_ptr(), core::ptr::null()];
            #[expect(
                clippy::let_underscore_must_use,
                reason = "an execve that returns failed; status 127 reports it"
            )]
            let _ = sys::execve(HELLO.as_ptr(), argv.as_ptr(), core::ptr::null());
            rt::exit(127)
        }
        Ok(pid) => pid,
        Err(_) => return Outcome::Fail("fork"),
    };
    match wait(pid) {
        Ok(0x2A00) => Outcome::Ok,
        Ok(_) => Outcome::Fail("status not 0x2A00"),
        Err(why) => Outcome::Fail(why),
    }
}

/// A child that reads unmapped low memory dies of `SIGSEGV`. It reads
/// `0x8`, not NULL: a volatile read of NULL is a precondition panic in
/// Rust, not a fault.
fn fork_segv() -> Outcome {
    let pid = match utest::fork() {
        Ok(0) => {
            // SAFETY: nothing is at 0x8 (the image loads at 1 GiB), so the
            // read faults and the kernel kills the child with SIGSEGV before
            // it returns, which is what this case checks; established here.
            let v = unsafe { core::ptr::read_volatile(core::ptr::without_provenance::<u64>(0x8)) };
            rt::exit(v as i32)
        }
        Ok(pid) => pid,
        Err(_) => return Outcome::Fail("fork"),
    };
    match wait(pid) {
        Ok(st) if st & 0x7f == SIGSEGV => Outcome::Ok,
        Ok(_) => Outcome::Fail("status not SIGSEGV"),
        Err(why) => Outcome::Fail(why),
    }
}
