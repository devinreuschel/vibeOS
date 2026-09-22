//! Syscall numbers, errno, dispatch metadata. ROADMAP §9.3.
//!
//! Slice A: entry/exit + this symbol returning `ENOSYS`.
//! Slice B: same `vibeos_syscall_stub` looks up the table. Handlers live
//! in the kernel half (`syscall_init`). No second entry path.

use crate::addr_space::UserMemError;

/// Linux `EPERM`.
pub const EPERM: i32 = 1;
/// Linux `ENOENT`.
pub const ENOENT: i32 = 2;
/// Linux `EBADF`.
pub const EBADF: i32 = 9;
/// Linux `ENOMEM`.
pub const ENOMEM: i32 = 12;
/// Linux `EFAULT`.
pub const EFAULT: i32 = 14;
/// Linux `EINVAL`.
pub const EINVAL: i32 = 22;
/// Linux `ENOSYS`.
pub const ENOSYS: i32 = 38;

pub const SYS_READ: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_SCHED_YIELD: u64 = 24;
pub const SYS_GETPID: u64 = 39;
pub const SYS_EXIT: u64 = 60;

/// Bootstrap pid until Slice C’s `Process`. Documented in `docs/SYSCALL.md`.
pub const BOOTSTRAP_PID: i64 = 1;

/// Early stdout/stderr until C’s fd table. Not a Process.
pub const EARLY_STDOUT_FD: u64 = 1;
pub const EARLY_STDERR_FD: u64 = 2;

pub const fn neg(errno: i32) -> i64 {
    -(errno as i64)
}

/// Saved frame on the kernel stack. Layout matches `vibeos_syscall_entry`
/// pushes, low address first.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SyscallFrame {
    pub user_rsp: u64,
    pub nr: u64,
    pub rip: u64,
    pub arg2: u64,
    pub rbx: u64,
    pub rbp: u64,
    pub arg1: u64,
    pub arg0: u64,
    pub arg4: u64,
    pub arg5: u64,
    pub arg3: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

impl SyscallFrame {
    pub const fn args(self) -> [u64; 6] {
        [self.arg0, self.arg1, self.arg2, self.arg3, self.arg4, self.arg5]
    }
}

/// Per-entry arity + which args are user pointers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyscallInfo {
    pub name: &'static str,
    pub nr: u64,
    pub arity: u8,
    /// Bit i set → arg i is a user pointer.
    pub ptr_mask: u8,
    /// Index of the length arg for that pointer, or `0xff` if none.
    pub len_arg: u8,
}

const WRITE: SyscallInfo = SyscallInfo {
    name: "write",
    nr: SYS_WRITE,
    arity: 3,
    ptr_mask: 1 << 1,
    len_arg: 2,
};
const YIELD: SyscallInfo = SyscallInfo {
    name: "sched_yield",
    nr: SYS_SCHED_YIELD,
    arity: 0,
    ptr_mask: 0,
    len_arg: 0xff,
};
const GETPID: SyscallInfo = SyscallInfo {
    name: "getpid",
    nr: SYS_GETPID,
    arity: 0,
    ptr_mask: 0,
    len_arg: 0xff,
};
const EXIT: SyscallInfo = SyscallInfo {
    name: "exit",
    nr: SYS_EXIT,
    arity: 1,
    ptr_mask: 0,
    len_arg: 0xff,
};

const TABLE: &[SyscallInfo] = &[WRITE, YIELD, GETPID, EXIT];

pub fn info(nr: u64) -> Option<SyscallInfo> {
    let mut i = 0;
    while i < TABLE.len() {
        if TABLE[i].nr == nr {
            return Some(TABLE[i]);
        }
        i += 1;
    }
    None
}

/// All-or-nothing: any failure → `EFAULT`, copy nothing. `len == 0` is ok.
pub fn check_user_ptr(
    check: impl Fn(u64, u64) -> Result<(), UserMemError>,
    ptr: u64,
    len: u64,
) -> Result<(), i32> {
    if len == 0 {
        return Ok(());
    }
    match check(ptr, len) {
        Ok(()) => Ok(()),
        Err(e) => match e {
            UserMemError::NonCanonical
            | UserMemError::Kernel
            | UserMemError::Overflow
            | UserMemError::Unmapped
            | UserMemError::NullGuard => Err(e.errno()),
        },
    }
}

pub fn validate_args(
    inf: SyscallInfo,
    args: [u64; 6],
    check: impl Fn(u64, u64) -> Result<(), UserMemError>,
) -> Result<(), i32> {
    let mut i = 0u8;
    while i < inf.arity {
        if inf.ptr_mask & (1 << i) != 0 {
            let ptr = args[i as usize];
            let len = if inf.len_arg == 0xff {
                0
            } else {
                args[inf.len_arg as usize]
            };
            check_user_ptr(&check, ptr, len)?;
        }
        i += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr_space::UserMemError;
    use core::mem::{offset_of, size_of};

    #[test]
    fn errno_linux_values() {
        assert_eq!(EPERM, 1);
        assert_eq!(ENOENT, 2);
        assert_eq!(EBADF, 9);
        assert_eq!(ENOMEM, 12);
        assert_eq!(EFAULT, 14);
        assert_eq!(EINVAL, 22);
        assert_eq!(ENOSYS, 38);
        assert_eq!(neg(ENOSYS), -38);
        assert_eq!(UserMemError::EFAULT, EFAULT);
    }

    #[test]
    fn table_arity_and_unknown_is_none() {
        let w = info(SYS_WRITE).unwrap();
        assert_eq!(w.name, "write");
        assert_eq!(w.arity, 3);
        assert_eq!(w.ptr_mask, 1 << 1);
        assert_eq!(w.len_arg, 2);
        assert!(info(SYS_GETPID).is_some());
        assert!(info(SYS_EXIT).is_some());
        assert!(info(SYS_SCHED_YIELD).is_some());
        assert!(info(SYS_READ).is_none());
        assert!(info(0xC0FFEE).is_none());
        assert!(info(u64::MAX).is_none());
        let mut n = 0;
        for e in TABLE {
            n += 1;
            match e.nr {
                SYS_WRITE | SYS_SCHED_YIELD | SYS_GETPID | SYS_EXIT => {}
                _ => panic!("unexpected nr"),
            }
        }
        assert_eq!(n, 4);
    }

    #[test]
    fn frame_layout_matches_entry_pushes() {
        assert_eq!(size_of::<SyscallFrame>(), 128);
        assert_eq!(offset_of!(SyscallFrame, user_rsp), 0);
        assert_eq!(offset_of!(SyscallFrame, nr), 8);
        assert_eq!(offset_of!(SyscallFrame, rip), 16);
        assert_eq!(offset_of!(SyscallFrame, arg2), 24);
        assert_eq!(offset_of!(SyscallFrame, arg1), 48);
        assert_eq!(offset_of!(SyscallFrame, arg0), 56);
        assert_eq!(offset_of!(SyscallFrame, arg4), 64);
        assert_eq!(offset_of!(SyscallFrame, arg5), 72);
        assert_eq!(offset_of!(SyscallFrame, arg3), 80);
    }

    #[test]
    fn ptr_policy_all_or_nothing() {
        let check_ok = |_p, _l| Ok(());
        assert!(check_user_ptr(check_ok, 0x1000, 0).is_ok());
        assert!(check_user_ptr(check_ok, 0x1000, 8).is_ok());
        let check_fault = |_p, _l| Err(UserMemError::Unmapped);
        assert_eq!(check_user_ptr(check_fault, 0x1000, 8), Err(EFAULT));
        assert!(check_user_ptr(check_fault, 0x1000, 0).is_ok());

        let inf = info(SYS_WRITE).unwrap();
        let args = [1, 0x1000, 4, 0, 0, 0];
        assert!(validate_args(inf, args, check_ok).is_ok());
        assert_eq!(validate_args(inf, args, check_fault), Err(EFAULT));
        let zero = [1, 0xFFFF_8000_0000_0000, 0, 0, 0, 0];
        assert!(validate_args(inf, zero, check_fault).is_ok());
    }
}
