//! Writing to file descriptors, and the print macros (ROADMAP §10.5).

use crate::sys::{self, Errno};

/// Write all of `buf` to `fd`, retrying short writes.
pub fn write_all(fd: i32, mut buf: &[u8]) -> Result<(), Errno> {
    while !buf.is_empty() {
        let n = sys::write(fd as u32, buf.as_ptr(), buf.len())?;
        if n == 0 {
            // A write that takes nothing would loop forever; report it as
            // the I/O error Linux's `write(2)` uses.
            return Err(Errno::EIO);
        }
        buf = buf.get(n..).unwrap_or(&[]);
    }
    Ok(())
}

/// A file descriptor as a `core::fmt::Write` sink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fd(pub i32);

impl core::fmt::Write for Fd {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        write_all(self.0, s.as_bytes()).map_err(|_| core::fmt::Error)
    }
}

/// Format to fd 1.
#[macro_export]
macro_rules! print {
    ($($a:tt)*) => {{
        use ::core::fmt::Write as _;
        #[expect(
            clippy::let_underscore_must_use,
            reason = "DESIGN §2.5: print! has no caller to report to, as in std"
        )]
        let _ = ::core::write!($crate::io::Fd(1), $($a)*);
    }};
}

/// Format to fd 1, then a newline.
#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($a:tt)*) => {{
        $crate::print!($($a)*);
        $crate::print!("\n");
    }};
}

/// Format to fd 2.
#[macro_export]
macro_rules! eprint {
    ($($a:tt)*) => {{
        use ::core::fmt::Write as _;
        #[expect(
            clippy::let_underscore_must_use,
            reason = "DESIGN §2.5: eprint! has no caller to report to, as in std"
        )]
        let _ = ::core::write!($crate::io::Fd(2), $($a)*);
    }};
}

/// Format to fd 2, then a newline.
#[macro_export]
macro_rules! eprintln {
    () => { $crate::eprint!("\n") };
    ($($a:tt)*) => {{
        $crate::eprint!($($a)*);
        $crate::eprint!("\n");
    }};
}
