//! The kernel's error type at the syscall boundary (ROADMAP §10.4, E2).
//!
//! A syscall handler returns `Result<usize, KError>`, and dispatch encodes
//! an `Err` as `-errno` (SYSCALL.md §2). Today `KError` only carries a Linux
//! errno; ROADMAP §10.4's `KError` box gives it its variants and a `From`
//! for every module error.

/// An error a syscall returns: a Linux errno, 1 to 4095.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KError {
    errno: i32,
}

impl KError {
    /// The error for Linux errno `errno`.
    ///
    /// Callers pass the errno constants of `vibeos::syscall`, never a value
    /// from input, so `errno` in 1 to 4095 is a kernel invariant (DESIGN
    /// §9.4) and the assertion stays.
    pub const fn from_errno(errno: i32) -> KError {
        assert!(matches!(errno, 1..=4095), "errno out of range");
        KError { errno }
    }

    /// The Linux errno, 1 to 4095.
    pub const fn errno(self) -> i32 {
        self.errno
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kerror_errno_range() {
        assert_eq!(KError::from_errno(1).errno(), 1);
        assert_eq!(KError::from_errno(38).errno(), 38);
        assert_eq!(KError::from_errno(4095).errno(), 4095);
        assert_eq!(KError::from_errno(14), KError::from_errno(14));
        assert_ne!(KError::from_errno(9), KError::from_errno(10));
        for bad in [0, -1, 4096, i32::MIN, i32::MAX] {
            assert!(std::panic::catch_unwind(|| KError::from_errno(bad)).is_err());
        }
    }
}
