//! Syscall numbers, errno, and the dispatch table's types. ROADMAP §9.3,
//! §10.5.
//!
//! The table itself is generated from `syscalls.toml` into
//! [`crate::proc::syscall_table`] (C-SYSTABLE), and its items are re-exported
//! here, with the x86_64 `SYS_*` numbers. Handlers live in the kernel half
//! (`proc_init`); `vibeos_syscall_stub` is the one entry path.

pub use crate::arch::x86_64::trap::UserFrame;

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
/// Linux `ENODEV`.
pub const ENODEV: i32 = 19;
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

pub const F_GETFD: u64 = 1;
pub const F_SETFD: u64 = 2;

pub const fn neg(errno: i32) -> i64 {
    -(errno as i64)
}

pub use crate::proc::syscall_table::x86_64::nr::*;
pub use crate::proc::syscall_table::{
    Arg, CType, Dir, Handlers, NrRule, NrTable, Ptr, PtrKind, ROWS, Row, Sys, SysResult, aarch64,
    x86_64,
};
#[cfg(test)]
pub use crate::proc::syscall_table::{Recorder, Val};

/// `r` as the value the syscall returns in `rax`: the result, or `-errno`.
pub const fn encode(r: SysResult) -> i64 {
    match r {
        Ok(v) => v as i64,
        Err(e) => neg(e.errno()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr_space::UserMemError;

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
        assert_eq!(ENODEV, 19);
        assert_eq!(EINVAL, 22);
        assert_eq!(EMFILE, 24);
        assert_eq!(EFBIG, 27);
        assert_eq!(ENAMETOOLONG, 36);
        assert_eq!(ENOSYS, 38);
        assert_eq!(neg(ENOSYS), -38);
        assert_eq!(UserMemError::EFAULT, EFAULT);
    }

    #[test]
    fn syscall_table_invariants() {
        for (tab, arch) in [(&x86_64::TABLE, 0), (&aarch64::TABLE, 1)] {
            let mut seen = [0usize; Sys::ALL.len()];
            for (nr, slot) in tab.slots().iter().enumerate() {
                let Some(sys) = slot else { continue };
                let row = sys.row();
                let want = if arch == 0 { row.x86_64 } else { row.aarch64 };
                assert_eq!(want, Some(nr as u32), "{} slot {nr}", row.name);
                seen[*sys as usize] += 1;
            }
            for row in &ROWS {
                let has = if arch == 0 { row.x86_64 } else { row.aarch64 };
                assert_eq!(
                    seen[row.sys as usize],
                    usize::from(has.is_some()),
                    "{}",
                    row.name
                );
            }
        }
        assert_eq!(ROWS.len(), Sys::ALL.len());
        for (i, sys) in Sys::ALL.iter().enumerate() {
            assert_eq!(*sys as usize, i);
            assert_eq!(sys.row().sys, *sys);
            assert!(core::ptr::eq(sys.row(), &ROWS[i]));
        }
        for row in &ROWS {
            assert!(row.arity() <= 6, "{}", row.name);
            for a in row.args {
                match a.ptr {
                    None => assert!(a.ty.is_int(), "{}.{} undeclared pointer", row.name, a.name),
                    Some(p) => {
                        assert_eq!(a.ty, CType::Ptr, "{}.{}", row.name, a.name);
                        if let PtrKind::Buf { len_from } = p.kind {
                            let len = row.args.get(usize::from(len_from)).map(|l| l.ty);
                            assert!(len.is_some_and(CType::is_int), "{}.{}", row.name, a.name);
                        }
                        assert!(!p.when.is_empty(), "{}.{}", row.name, a.name);
                    }
                }
            }
        }
        let declared = |sys: Sys, arg: &str| {
            let a = sys.row().args.iter().find(|a| a.name == arg).unwrap();
            a.ptr.is_some_and(|p| p.kind != PtrKind::Unread)
        };
        assert!(declared(Sys::Open, "pathname"));
        assert!(declared(Sys::Execve, "pathname"));
        assert!(declared(Sys::Execve, "argv"));
        assert!(declared(Sys::Execve, "envp"));
        assert!(declared(Sys::Wait4, "wstatus"));
        assert!(declared(Sys::Read, "buf"));
        assert!(declared(Sys::Write, "buf"));
        assert!(declared(Sys::Psinfo, "buf"));
        assert!(!declared(Sys::Wait4, "rusage"));
        assert_eq!(x86_64::TABLE.lookup(SYS_GETPID), Some(Sys::Getpid));
        assert_eq!(x86_64::TABLE.lookup(0xC0FFEE), None);
        assert_eq!(x86_64::TABLE.lookup(u64::MAX), None);
    }
}
