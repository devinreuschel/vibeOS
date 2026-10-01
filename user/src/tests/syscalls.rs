//! `write`, `getpid` and `dup` from ring 3, and `write`'s errors
//! (ROADMAP §9.8).

use core::sync::atomic::Ordering;

use vibeos_user::sys::{self, Errno};
use vibeos_user::utest::{Outcome, Runner};

use super::{BANNER, BANNER_WRITE};

const DUP_MSG: &[u8] = b"user: dup ok\n";

pub fn run(t: &mut Runner) {
    t.case("write_count", write_count);
    t.case("getpid_nonzero", getpid_nonzero);
    t.case("write_ebadf", write_ebadf);
    t.case("write_efault", write_efault);
    t.case("dup_write", dup_write);
}

/// The banner's `write` to fd 1 returned its length.
fn write_count() -> Outcome {
    if BANNER_WRITE.load(Ordering::Relaxed) == BANNER.len() {
        Outcome::Ok
    } else {
        Outcome::Fail("banner write count")
    }
}

fn getpid_nonzero() -> Outcome {
    match sys::getpid() {
        Ok(0) => Outcome::Fail("pid 0"),
        Ok(_) => Outcome::Ok,
        Err(_) => Outcome::Fail("getpid failed"),
    }
}

/// fd 3 is not open.
fn write_ebadf() -> Outcome {
    if sys::write(3, BANNER.as_ptr(), 1) == Err(Errno::EBADF) {
        Outcome::Ok
    } else {
        Outcome::Fail("write to fd 3 not EBADF")
    }
}

/// NULL, a kernel address and an unmapped user page, 8 bytes each, then a
/// length of `1 << 63`: each `EFAULT`.
fn write_efault() -> Outcome {
    let bad: [(u64, usize, &'static str); 4] = [
        (0, 8, "NULL not EFAULT"),
        (0xFFFF_8000_0000_1000, 8, "kernel pointer not EFAULT"),
        (0x7000_0000, 8, "unmapped page not EFAULT"),
        (BANNER.as_ptr() as u64, 1 << 63, "huge length not EFAULT"),
    ];
    for (addr, len, why) in bad {
        if sys::write(1, addr as *const u8, len) != Err(Errno::EFAULT) {
            return Outcome::Fail(why);
        }
    }
    Outcome::Ok
}

/// `dup(1)` is fd 3, a write through it lands, and it closes.
fn dup_write() -> Outcome {
    let fd = match sys::dup(1) {
        Ok(3) => 3,
        Ok(_) => return Outcome::Fail("dup(1) not 3"),
        Err(_) => return Outcome::Fail("dup failed"),
    };
    let n = sys::write(fd, DUP_MSG.as_ptr(), DUP_MSG.len());
    let closed = sys::close(fd);
    if n != Ok(DUP_MSG.len()) {
        return Outcome::Fail("write through dup");
    }
    if closed.is_err() {
        return Outcome::Fail("close");
    }
    Outcome::Ok
}
