//! In-guest tests of the syscall table's pointer declarations (kernel_tests
//! only, ROADMAP §10.5): `sysdecl`, embedded from `make user` (C-USERBINS),
//! run in ring 3. Rows: the list in crate::ktest.

use vibeos::arch::SyscallAbi;
use vibeos::proc::{wexitstatus, wifexited};
use vibeos::syscall::{EBADF, ECHILD, EFAULT, PtrKind, ROWS};

use crate::arch::current::Arch;
use crate::ktest::Outcome;
use crate::ktest::user::{self, Image};

/// The test program.
const SYSDECL: Image = Image::UserBin("sysdecl");

/// The bad pointers `sysdecl` puts in an argument: a page it mapped and
/// unmapped, and a kernel-half address.
const BAD: [&str; 2] = ["unmapped", "kernel"];

/// `sysdecl`'s exit status for `argv`, or a failure naming `what`.
fn run(argv: &[&str]) -> Result<u32, Outcome> {
    match user::run(&SYSDECL, argv) {
        Ok(st) if wifexited(st) => Ok(wexitstatus(st)),
        Ok(st) => Err(crate::fail_fmt!("{argv:?}: killed, status {st:#x}")),
        Err(e) => Err(crate::fail_fmt!("{argv:?}: spawn: {}", e.as_str())),
    }
}

/// Each declared pointer argument of each row, with every other argument
/// valid, returns `EFAULT` for an unmapped and a kernel-half pointer, so a
/// declaration cannot drift from its handler (F150).
pub(crate) fn syscall_ptr_decl_efault() -> Outcome {
    let mut cases = 0u32;
    for row in ROWS
        .iter()
        .filter(|r| <Arch as SyscallAbi>::table().number(r.sys).is_some())
    {
        for arg in row.args {
            if !arg.ptr.is_some_and(|p| p.kind != PtrKind::Unread) {
                continue;
            }
            for bad in BAD {
                let st = match run(&["sysdecl", row.name, arg.name, bad]) {
                    Ok(st) => st,
                    Err(o) => return o,
                };
                if st != EFAULT as u32 {
                    return crate::fail_fmt!(
                        "{}.{} {bad}: exit {st}, want EFAULT ({EFAULT})",
                        row.name,
                        arg.name
                    );
                }
                cases += 1;
            }
        }
    }
    if cases < 16 {
        return crate::fail_fmt!("{cases} cases, want at least 16");
    }
    Outcome::Ok
}

/// `read(-1, <unmapped>, 1)` returns `EBADF`: the descriptor is checked
/// before the buffer (SYSCALL.md §3).
pub(crate) fn read_ebadf_before_efault() -> Outcome {
    match run(&["sysdecl", "order-read"]) {
        Ok(st) if st == EBADF as u32 => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("exit {st}, want EBADF ({EBADF})"),
        Err(o) => o,
    }
}

/// `wait4(-1, <unmapped>, 0, NULL)` with no child returns `ECHILD`: the
/// child is looked for before the status is copied (SYSCALL.md §3).
pub(crate) fn wait4_echild_before_efault() -> Outcome {
    match run(&["sysdecl", "order-wait4"]) {
        Ok(st) if st == ECHILD as u32 => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("exit {st}, want ECHILD ({ECHILD})"),
        Err(o) => o,
    }
}
