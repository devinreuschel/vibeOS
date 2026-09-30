//! Syscall numbers and the dispatch table's types. ROADMAP §9.3,
//! §10.5.
//!
//! The table itself is generated from `syscalls.toml` into
//! [`crate::proc::syscall_table`] (C-SYSTABLE), and its items are re-exported
//! here, with the reference port's `SYS_*` numbers (`arch::syscall_nr`); each
//! port's dispatch is `SyscallAbi::dispatch`. Handlers live in the kernel half
//! (`proc_init`); `vibeos_syscall_stub` is the one entry path.

pub use crate::arch::UserFrame;

pub const F_GETFD: u32 = 1;
pub const F_SETFD: u32 = 2;

pub use crate::arch::syscall_nr::*;
pub use crate::proc::syscall_table::{
    Arg, CType, Dir, Handlers, NrRule, NrTable, Ptr, PtrKind, ROWS, Row, Sys, SysResult, aarch64,
};
#[cfg(test)]
pub use crate::proc::syscall_table::{Recorder, Val};

/// `r` as the value the syscall returns in `rax`: the result, or `-errno`.
pub const fn encode(r: SysResult) -> i64 {
    match r {
        Ok(v) => v as i64,
        Err(e) => -(e.errno() as i64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::SyscallAbi;
    use crate::arch::stub::Arch;
    use crate::kerror::KError;

    /// Every errno of the `KError` table reaches ring 3 as Linux's
    /// `-errno`, in the error range userspace tests for (SYSCALL.md §2).
    #[test]
    fn errno_linux_values() {
        for &e in KError::ALL {
            let r = encode(Err(e));
            assert_eq!(r, -i64::from(e.errno()), "{}", e.name());
            assert!((-4095..0).contains(&r), "{}", e.name());
        }
        assert_eq!(encode(Err(KError::BadF)), -9);
        assert_eq!(encode(Err(KError::NoSys)), -38);
    }

    #[test]
    fn syscall_table_invariants() {
        // The stub port's table is x86_64's.
        for tab in [Arch::table(), &aarch64::TABLE] {
            let mut seen = [0usize; Sys::ALL.len()];
            for (nr, slot) in tab.slots().iter().enumerate() {
                let Some(sys) = slot else { continue };
                let row = sys.row();
                assert_eq!(tab.number(*sys), Some(nr as u64), "{} slot {nr}", row.name);
                assert_eq!(tab.lookup(nr as u64), Some(*sys), "{} slot {nr}", row.name);
                seen[*sys as usize] += 1;
            }
            for row in &ROWS {
                assert_eq!(
                    seen[row.sys as usize],
                    usize::from(tab.number(row.sys).is_some()),
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
        assert_eq!(Arch::table().lookup(SYS_GETPID), Some(Sys::Getpid));
        assert_eq!(Arch::table().lookup(0xC0FFEE), None);
        assert_eq!(Arch::table().lookup(u64::MAX), None);
    }
    /// The value a handler of C type `ty` must see for register
    /// `0xFFFF_FFFF_8000_0001 + i` (`high`) or `0xDEAD_BEEF_0000_0003 + i`.
    fn want(ty: CType, high: bool, i: u64) -> Val {
        let (i32v, i64v, full) = if high {
            (-2_147_483_647, -2_147_483_647, 0xFFFF_FFFF_8000_0001u64)
        } else {
            (3, -0x2152_4110_FFFF_FFFD, 0xDEAD_BEEF_0000_0003u64)
        };
        let (lo32, lo16) = if high { (0x8000_0001u32, 1u16) } else { (3, 3) };
        let n = i as i32;
        match ty {
            CType::Int | CType::PidT => Val::I32(i32v + n),
            CType::UInt => Val::U32(lo32 + i as u32),
            CType::Long | CType::OffT => Val::I64(i64v + i64::from(n)),
            CType::ULong => Val::U64(full + i),
            CType::SizeT => Val::Usize((full + i) as usize),
            CType::UmodeT => Val::U16(lo16 + i as u16),
            CType::Ptr => Val::Ptr(full + i),
        }
    }

    #[test]
    fn dispatch_args_c_types() {
        for row in &ROWS {
            type Dispatch = fn(&mut Recorder, u64, &[u64; 6]) -> SysResult;
            let arches: [(Option<u64>, Dispatch); 2] = [
                (Arch::table().number(row.sys), Arch::dispatch::<Recorder>),
                (
                    aarch64::TABLE.number(row.sys),
                    aarch64::dispatch::<Recorder>,
                ),
            ];
            for (nr, dispatch) in arches {
                let Some(nr) = nr else { continue };
                for high in [true, false] {
                    let base = if high {
                        0xFFFF_FFFF_8000_0001u64
                    } else {
                        0xDEAD_BEEF_0000_0003u64
                    };
                    let regs: [u64; 6] = core::array::from_fn(|i| base + i as u64);
                    let mut rec = Recorder::default();
                    assert_eq!(dispatch(&mut rec, nr, &regs), Ok(0), "{}", row.name);
                    assert_eq!(rec.sys, Some(row.sys), "{}", row.name);
                    for (i, a) in row.args.iter().enumerate() {
                        let got = rec.args[i];
                        let w = want(a.ty, high, i as u64);
                        assert_eq!(got, Some(w), "{}.{} high={high}", row.name, a.name);
                    }
                    for slot in &rec.args[row.arity()..] {
                        assert_eq!(*slot, None, "{}", row.name);
                    }
                }
            }
        }
        // SYSCALL.md §1's examples.
        let mut rec = Recorder::default();
        let regs = [0xFFFF_FFFF_0000_0003, 0x1000, 1, 0, 0, 0];
        assert_eq!(Arch::dispatch(&mut rec, SYS_READ, &regs), Ok(0));
        assert_eq!(rec.args[0], Some(Val::U32(3)));
        let regs = [0x1_0000_0005, 9, 0, 0, 0, 0];
        assert_eq!(Arch::dispatch(&mut rec, SYS_KILL, &regs), Ok(0));
        assert_eq!(rec.args[0], Some(Val::I32(5)));
        let regs = [0xFFFF_FFFF, 0, 0, 0, 0, 0];
        assert_eq!(Arch::dispatch(&mut rec, SYS_WAIT4, &regs), Ok(0));
        assert_eq!(rec.args[0], Some(Val::I32(-1)));
    }

    #[test]
    fn dispatch_nr_eax_sign_extended() {
        let regs = [0; 6];
        let nosys = Err(KError::NoSys);
        let mut rec = Recorder::default();
        assert_eq!(
            Arch::dispatch(&mut rec, 0xFFFF_FFFF_0000_0001, &regs),
            Ok(0)
        );
        assert_eq!(rec.sys, Some(Sys::Write));
        assert_eq!(Arch::dispatch(&mut rec, 0x1_0000_0027, &regs), Ok(0));
        assert_eq!(rec.sys, Some(Sys::Getpid));
        // Negative as `eax`, and x32's bit 30 (LINUX.md `no-32bit`).
        for nr in [0x8000_0000, 0xFFFF_FFFF, 0x4000_0001, u64::MAX] {
            let mut rec = Recorder::default();
            assert_eq!(Arch::dispatch(&mut rec, nr, &regs), nosys, "{nr:#x}");
            assert_eq!(rec.sys, None, "{nr:#x}");
        }
        assert_eq!(
            aarch64::TABLE.lookup(0xFFFF_FFFF_0000_0040),
            Some(Sys::Write)
        );
        assert_eq!(aarch64::TABLE.lookup(0x8000_0040), None);
        let mut rec = Recorder::default();
        assert_eq!(aarch64::dispatch(&mut rec, 0x8000_0040, &regs), nosys);
        // A row with no number on an architecture is ENOSYS there.
        assert_eq!(aarch64::call(&mut rec, Sys::Open, &regs), nosys);
        assert_eq!(aarch64::call(&mut rec, Sys::Fork, &regs), nosys);
    }

    #[test]
    fn sysresult_encode() {
        assert_eq!(encode(Ok(0)), 0);
        assert_eq!(encode(Ok(42)), 42);
        assert_eq!(encode(Ok(0x7FFF_F7FF_F000)), 0x7FFF_F7FF_F000);
        assert_eq!(encode(Err(KError::BadF)), -9);
        assert_eq!(encode(Err(KError::NoSys)), -38);
        assert_eq!(encode(Err(KError::OpNotSupp)), -95);
    }
}
