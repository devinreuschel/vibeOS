//! `open` flag bits are this architecture's Linux values.

use core::ffi::CStr;

use vibeos_user::sys::{self, Errno};
use vibeos_user::utest::{Outcome, Runner};

pub fn run(t: &mut Runner) {
    t.case("open_uapi_flags", open_uapi_flags);
}

/// A directory opened with `O_DIRECTORY` succeeds, a file opened with it
/// returns `ENOTDIR`, and a symlink opened with `O_LARGEFILE` is followed.
/// `O_DIRECT` on a file is not `O_DIRECTORY`, and `O_NOFOLLOW` on a symlink
/// is still `ELOOP`.
fn open_uapi_flags() -> Outcome {
    let dir = match open(c"/tmp", sys::O_RDONLY | sys::O_DIRECTORY) {
        Ok(fd) => fd,
        Err(Errno::ENOTDIR) => return Outcome::Fail("O_DIRECTORY on a directory"),
        Err(_) => return Outcome::Fail("open directory"),
    };
    if let Err(why) = close(dir) {
        return Outcome::Fail(why);
    }
    match open(c"/hello", sys::O_RDONLY | sys::O_DIRECTORY) {
        Err(Errno::ENOTDIR) => {}
        Ok(fd) => {
            if let Err(why) = close(fd) {
                return Outcome::Fail(why);
            }
            return Outcome::Fail("O_DIRECTORY on a file");
        }
        Err(_) => return Outcome::Fail("O_DIRECTORY on a file"),
    }
    let link = match open(c"/proc/self", sys::O_RDONLY | sys::O_LARGEFILE) {
        Ok(fd) => fd,
        Err(Errno::ELOOP) => return Outcome::Fail("O_LARGEFILE on a symlink"),
        Err(_) => return Outcome::Fail("open symlink"),
    };
    if let Err(why) = close(link) {
        return Outcome::Fail(why);
    }
    match open(c"/hello", sys::O_RDONLY | sys::O_DIRECT) {
        Ok(fd) => {
            if let Err(why) = close(fd) {
                return Outcome::Fail(why);
            }
        }
        Err(Errno::ENOTDIR) => return Outcome::Fail("O_DIRECT on a file"),
        Err(_) => return Outcome::Fail("open O_DIRECT"),
    }
    match open(c"/proc/self", sys::O_RDONLY | sys::O_NOFOLLOW) {
        Err(Errno::ELOOP) => Outcome::Ok,
        Ok(fd) => {
            if let Err(why) = close(fd) {
                return Outcome::Fail(why);
            }
            Outcome::Fail("O_NOFOLLOW on a symlink")
        }
        Err(_) => Outcome::Fail("open O_NOFOLLOW"),
    }
}

fn open(path: &CStr, flags: i32) -> Result<u32, Errno> {
    sys::open(path.as_ptr().cast(), flags, 0).map(|fd| fd as u32)
}

fn close(fd: u32) -> Result<(), &'static str> {
    sys::close(fd).map(|_| ()).map_err(|_| "close")
}
