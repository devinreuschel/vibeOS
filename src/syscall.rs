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
/// Linux `ESRCH`.
pub const ESRCH: i32 = 3;
/// Linux `ECHILD`.
pub const ECHILD: i32 = 10;
/// Linux `EAGAIN`.
pub const EAGAIN: i32 = 11;
/// Linux `ENOMEM`.
pub const ENOMEM: i32 = 12;
/// Linux `EACCES`.
pub const EACCES: i32 = 13;
/// Linux `EFAULT`.
pub const EFAULT: i32 = 14;
/// Linux `EBADF`.
pub const EBADF: i32 = 9;
/// Linux `EBUSY`.
pub const EBUSY: i32 = 16;
/// Linux `EEXIST`.
pub const EEXIST: i32 = 17;
/// Linux `ENOTDIR`.
pub const ENOTDIR: i32 = 20;
/// Linux `EISDIR`.
pub const EISDIR: i32 = 21;
/// Linux `EINVAL`.
pub const EINVAL: i32 = 22;
/// Linux `EMFILE`.
pub const EMFILE: i32 = 24;
/// Linux `EFBIG`.
pub const EFBIG: i32 = 27;
/// Linux `ENOSYS`.
pub const ENOSYS: i32 = 38;
/// Linux `ENAMETOOLONG`.
pub const ENAMETOOLONG: i32 = 36;
/// Linux `EIO`.
pub const EIO: i32 = 5;
/// Linux `E2BIG`.
pub const E2BIG: i32 = 7;
/// Linux `ENOEXEC`.
pub const ENOEXEC: i32 = 8;

pub const SYS_READ: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_OPEN: u64 = 2;
pub const SYS_CLOSE: u64 = 3;
pub const SYS_LSEEK: u64 = 8;
pub const SYS_DUP: u64 = 32;
pub const SYS_DUP2: u64 = 33;
pub const SYS_GETPID: u64 = 39;
pub const SYS_FORK: u64 = 57;
pub const SYS_EXECVE: u64 = 59;
pub const SYS_EXIT: u64 = 60;
pub const SYS_WAIT4: u64 = 61;
pub const SYS_KILL: u64 = 62;
pub const SYS_FCNTL: u64 = 72;
pub const SYS_GETPPID: u64 = 110;
pub const SYS_SCHED_YIELD: u64 = 24;
/// vibeOS-specific until ROADMAP §13.9 procfs replaces it. `rdi` buf, `rsi` len.
pub const SYS_PSINFO: u64 = 500;

pub const F_GETFD: u64 = 1;
pub const F_SETFD: u64 = 2;

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
        [
            self.arg0, self.arg1, self.arg2, self.arg3, self.arg4, self.arg5,
        ]
    }
}

/// A ring-3 register frame: the 21 words of Linux's x86_64
/// `user_regs_struct` (`<sys/user.h>`), in its order, low address first.
/// The last five are the hardware `iretq` frame. `arch::idt::TrapFrame`
/// ends with these words, so a CPL-3 entry leaves one at the top of the
/// thread's kernel stack (ROADMAP §10.6).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UserFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub orig_rax: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// Full user GPR set for fork child / exec / iret-into-user.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct UserRegs {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rsp: u64,
    pub rflags: u64,
    pub fs_base: u64,
}

impl UserRegs {
    pub const fn empty() -> Self {
        Self {
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rip: 0,
            rsp: 0,
            rflags: 0,
            fs_base: 0,
        }
    }

    pub fn from_syscall(f: &SyscallFrame, retval: u64) -> Self {
        Self {
            rax: retval,
            rbx: f.rbx,
            rcx: f.rip,
            rdx: f.arg2,
            rsi: f.arg1,
            rdi: f.arg0,
            rbp: f.rbp,
            r8: f.arg4,
            r9: f.arg5,
            r10: f.arg3,
            r11: f.r11,
            r12: f.r12,
            r13: f.r13,
            r14: f.r14,
            r15: f.r15,
            rip: f.rip,
            rsp: f.user_rsp,
            rflags: f.r11,
            fs_base: 0,
        }
    }

    pub fn apply_to_syscall(self, f: &mut SyscallFrame) {
        f.user_rsp = self.rsp;
        f.rip = self.rip;
        f.arg2 = self.rdx;
        f.rbx = self.rbx;
        f.rbp = self.rbp;
        f.arg1 = self.rsi;
        f.arg0 = self.rdi;
        f.arg4 = self.r8;
        f.arg5 = self.r9;
        f.arg3 = self.r10;
        f.r11 = self.rflags;
        f.r12 = self.r12;
        f.r13 = self.r13;
        f.r14 = self.r14;
        f.r15 = self.r15;
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
const READ: SyscallInfo = SyscallInfo {
    name: "read",
    nr: SYS_READ,
    arity: 3,
    ptr_mask: 1 << 1,
    len_arg: 2,
};
const OPEN: SyscallInfo = SyscallInfo {
    name: "open",
    nr: SYS_OPEN,
    arity: 3,
    ptr_mask: 0,
    len_arg: 0xff,
};
const CLOSE: SyscallInfo = SyscallInfo {
    name: "close",
    nr: SYS_CLOSE,
    arity: 1,
    ptr_mask: 0,
    len_arg: 0xff,
};
const LSEEK: SyscallInfo = SyscallInfo {
    name: "lseek",
    nr: SYS_LSEEK,
    arity: 3,
    ptr_mask: 0,
    len_arg: 0xff,
};
const DUP: SyscallInfo = SyscallInfo {
    name: "dup",
    nr: SYS_DUP,
    arity: 1,
    ptr_mask: 0,
    len_arg: 0xff,
};
const DUP2: SyscallInfo = SyscallInfo {
    name: "dup2",
    nr: SYS_DUP2,
    arity: 2,
    ptr_mask: 0,
    len_arg: 0xff,
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
const GETPPID: SyscallInfo = SyscallInfo {
    name: "getppid",
    nr: SYS_GETPPID,
    arity: 0,
    ptr_mask: 0,
    len_arg: 0xff,
};
const FORK: SyscallInfo = SyscallInfo {
    name: "fork",
    nr: SYS_FORK,
    arity: 0,
    ptr_mask: 0,
    len_arg: 0xff,
};
const EXECVE: SyscallInfo = SyscallInfo {
    name: "execve",
    nr: SYS_EXECVE,
    arity: 3,
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
const WAIT4: SyscallInfo = SyscallInfo {
    name: "wait4",
    nr: SYS_WAIT4,
    arity: 4,
    ptr_mask: 0,
    len_arg: 0xff,
};
const KILL: SyscallInfo = SyscallInfo {
    name: "kill",
    nr: SYS_KILL,
    arity: 2,
    ptr_mask: 0,
    len_arg: 0xff,
};
const FCNTL: SyscallInfo = SyscallInfo {
    name: "fcntl",
    nr: SYS_FCNTL,
    arity: 3,
    ptr_mask: 0,
    len_arg: 0xff,
};
const PSINFO: SyscallInfo = SyscallInfo {
    name: "psinfo",
    nr: SYS_PSINFO,
    arity: 2,
    ptr_mask: 1 << 0,
    len_arg: 1,
};

const TABLE: &[SyscallInfo] = &[
    READ, WRITE, OPEN, CLOSE, LSEEK, DUP, DUP2, YIELD, GETPID, GETPPID, FORK, EXECVE, EXIT, WAIT4,
    KILL, FCNTL, PSINFO,
];

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
    fn user_frame_is_user_regs_struct() {
        assert_eq!(size_of::<UserFrame>(), 21 * 8);
        assert_eq!(offset_of!(UserFrame, r15), 0);
        assert_eq!(offset_of!(UserFrame, rbx), 5 * 8);
        assert_eq!(offset_of!(UserFrame, rax), 10 * 8);
        assert_eq!(offset_of!(UserFrame, rdi), 14 * 8);
        assert_eq!(offset_of!(UserFrame, orig_rax), 15 * 8);
        assert_eq!(offset_of!(UserFrame, rip), 16 * 8);
        assert_eq!(offset_of!(UserFrame, ss), 20 * 8);
    }

    #[test]
    fn errno_linux_values() {
        assert_eq!(EPERM, 1);
        assert_eq!(ENOENT, 2);
        assert_eq!(ESRCH, 3);
        assert_eq!(EIO, 5);
        assert_eq!(E2BIG, 7);
        assert_eq!(ENOEXEC, 8);
        assert_eq!(EBADF, 9);
        assert_eq!(ECHILD, 10);
        assert_eq!(EAGAIN, 11);
        assert_eq!(ENOMEM, 12);
        assert_eq!(EACCES, 13);
        assert_eq!(EFAULT, 14);
        assert_eq!(EINVAL, 22);
        assert_eq!(EMFILE, 24);
        assert_eq!(EFBIG, 27);
        assert_eq!(ENAMETOOLONG, 36);
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
        assert!(info(SYS_FORK).is_some());
        assert!(info(SYS_EXECVE).is_some());
        assert!(info(SYS_WAIT4).is_some());
        assert!(info(SYS_READ).is_some());
        assert!(info(SYS_OPEN).is_some());
        assert!(info(SYS_PSINFO).is_some());
        assert!(info(0xC0FFEE).is_none());
        assert!(info(u64::MAX).is_none());
        let mut n = 0;
        for e in TABLE {
            n += 1;
            match e.nr {
                SYS_READ | SYS_WRITE | SYS_OPEN | SYS_CLOSE | SYS_LSEEK | SYS_DUP | SYS_DUP2
                | SYS_SCHED_YIELD | SYS_GETPID | SYS_GETPPID | SYS_FORK | SYS_EXECVE | SYS_EXIT
                | SYS_WAIT4 | SYS_KILL | SYS_FCNTL | SYS_PSINFO => {}
                _ => panic!("unexpected nr"),
            }
        }
        assert_eq!(n, TABLE.len());
        assert_eq!(n, 17);
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
