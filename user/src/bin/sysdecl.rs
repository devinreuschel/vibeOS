//! The in-guest tests' program for the syscall table's pointer
//! declarations (ROADMAP §10.5, `syscall_ptr_decl_efault`). Only
//! `kernel_tests` kernels embed it.
//!
//! `sysdecl <row> <arg> <bad>` sets up every argument of syscall `<row>`
//! valid, puts a bad pointer in argument `<arg>`, makes the call, and exits
//! with the errno it returns, 255 when it succeeds, 254 when the setup
//! fails, and 253 for a row with no pointer argument. `<bad>` is `unmapped`
//! (a page it maps and unmaps again) or `kernel` (a kernel-half address).
//! `sysdecl order-read` and `sysdecl order-wait4` pass two bad arguments
//! and exit with the errno, which must be the one Linux checks first
//! (SYSCALL.md §3). It prints nothing.

#![no_std]
#![no_main]

use vibeos_user::env::Env;
use vibeos_user::rt;
use vibeos_user::sys::{self, Errno, Sys};

vibeos_user::main!(main);

/// The call succeeded.
const EXIT_OK: i32 = 255;
/// The setup failed.
const EXIT_SETUP: i32 = 254;
/// The row declares no pointer argument.
const EXIT_NO_PTR: i32 = 253;

/// A kernel-half address, above the user half.
const KERNEL_PTR: usize = 0xFFFF_8000_0000_1000;

// From Linux `include/uapi/asm-generic/mman-common.h` (PROT_READ,
// MAP_ANONYMOUS) and `include/uapi/linux/mman.h` (MAP_PRIVATE).
const PROT_READ: u64 = 0x1;
const MAP_PRIVATE: u64 = 0x02;
const MAP_ANONYMOUS: u64 = 0x20;

const HELLO: &core::ffi::CStr = c"/hello";

fn main(env: &Env) -> i32 {
    let Some(unmapped) = unmapped_page() else {
        return EXIT_SETUP;
    };
    match env.arg(1) {
        Some(b"order-read") => {
            // read(-1, <unmapped>, 1): EBADF, the descriptor is checked first.
            let regs = [-1isize as usize, unmapped, 1, 0, 0, 0];
            call(Sys::Read, &regs)
        }
        Some(b"order-wait4") => {
            // wait4(-1, <unmapped>, 0, NULL) with no child: ECHILD.
            let regs = [-1isize as usize, unmapped, 0, 0, 0, 0];
            call(Sys::Wait4, &regs)
        }
        Some(row) => {
            let (Some(sys), Some(arg), Some(bad)) = (Sys::from_name(row), env.arg(2), env.arg(3))
            else {
                return EXIT_SETUP;
            };
            let bad = match bad {
                b"unmapped" => unmapped,
                b"kernel" => KERNEL_PTR,
                _ => return EXIT_SETUP,
            };
            let Some(i) = sys.args().iter().position(|a| a.as_bytes() == arg) else {
                return EXIT_SETUP;
            };
            let mut bufs = Bufs::new();
            let mut regs = match setup(sys, &mut bufs) {
                Setup::Regs(r) => r,
                Setup::NoPtr => return EXIT_NO_PTR,
                Setup::Failed => return EXIT_SETUP,
            };
            regs[i] = bad;
            call(sys, &regs)
        }
        None => EXIT_SETUP,
    }
}

/// Memory the valid arguments point at.
struct Bufs {
    byte: [u8; 64],
    status: i32,
    argv: [usize; 2],
    envp: [usize; 1],
}

impl Bufs {
    fn new() -> Bufs {
        Bufs {
            byte: [b'x'; 64],
            status: 0,
            argv: [HELLO.as_ptr() as usize, 0],
            envp: [0],
        }
    }
}

enum Setup {
    Regs([usize; 6]),
    NoPtr,
    Failed,
}

/// Every argument of `sys` valid, in register order. A new row with a
/// pointer argument fails to compile here until it gets a setup.
fn setup(sys: Sys, b: &mut Bufs) -> Setup {
    let byte = b.byte.as_mut_ptr() as usize;
    match sys {
        Sys::Read => match sys::open(HELLO.as_ptr().cast(), sys::O_RDONLY, 0) {
            Ok(fd) => Setup::Regs([fd, byte, 1, 0, 0, 0]),
            Err(_) => Setup::Failed,
        },
        Sys::Write => Setup::Regs([1, byte, 1, 0, 0, 0]),
        Sys::Open => Setup::Regs([HELLO.as_ptr() as usize, sys::O_RDONLY as usize, 0, 0, 0, 0]),
        Sys::Execve => Setup::Regs([
            HELLO.as_ptr() as usize,
            b.argv.as_ptr() as usize,
            b.envp.as_ptr() as usize,
            0,
            0,
            0,
        ]),
        Sys::Wait4 => match sys::fork() {
            Ok(0) => rt::exit(0),
            Ok(_) => Setup::Regs([-1isize as usize, &raw mut b.status as usize, 0, 0, 0, 0]),
            Err(_) => Setup::Failed,
        },
        Sys::Psinfo => Setup::Regs([byte, 64, 0, 0, 0, 0]),
        Sys::Fstat => Setup::Regs([1, byte, 0, 0, 0, 0]),
        Sys::Nanosleep => Setup::Regs([byte, 0, 0, 0, 0, 0]),
        Sys::Getdents64 => match sys::open(c"/".as_ptr().cast(), sys::O_RDONLY, 0) {
            Ok(fd) => Setup::Regs([fd, byte, 64, 0, 0, 0]),
            Err(_) => Setup::Failed,
        },
        Sys::Close
        | Sys::Lseek
        | Sys::Mmap
        | Sys::Munmap
        | Sys::Brk
        | Sys::SchedYield
        | Sys::Dup
        | Sys::Dup2
        | Sys::Getpid
        | Sys::Fork
        | Sys::Exit
        | Sys::Kill
        | Sys::Fcntl
        | Sys::Getppid => Setup::NoPtr,
    }
}

/// Make call `sys` with `regs`; its errno, or [`EXIT_OK`].
fn call(sys: Sys, regs: &[usize; 6]) -> i32 {
    // SAFETY: every pointer in `regs` is a bad one the kernel refuses with
    // `EFAULT` before writing, or points into `Bufs` or a 'static string,
    // which no reference covers while the kernel writes them; established
    // here and by `setup`.
    let r = unsafe {
        sys::syscall6(
            sys.nr(),
            regs[0],
            regs[1],
            regs[2],
            regs[3],
            regs[4],
            regs[5],
        )
    };
    match sys::result(r) {
        Ok(_) => EXIT_OK,
        Err(Errno(e)) => e,
    }
}

/// A page-aligned address with nothing mapped: a page this program maps
/// and unmaps again.
fn unmapped_page() -> Option<usize> {
    // SAFETY: a fresh anonymous mapping replaces nothing the program uses,
    // and the page is unmapped before anything points into it; established
    // here.
    let page =
        unsafe { sys::mmap(0, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, u64::MAX, 0) }.ok()?;
    // SAFETY: nothing refers to `page`, mapped just above; established here.
    unsafe { sys::munmap(page as u64, 4096) }.ok()?;
    Some(page)
}
