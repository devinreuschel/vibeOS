//! `execve`'s arguments and environment (ROADMAP §10.5): the caller's
//! `envp` reaches the new image, `argv` and `envp` take Linux's limits
//! (131,072 bytes a string with its NUL, 2 MiB in all at the 8 MiB
//! `RLIMIT_STACK`), and an empty or NULL `argv` starts the image with
//! `argc` 1 and an empty `argv[0]`.
//!
//! Each case forks; the child calls `execve` on `/bin/envcheck` or
//! `/bin/argcheck` and, if the call returns, exits `100 + errno`. The
//! parent reads the child's exit status. Large vectors alias one short
//! string or one 131,073-byte buffer, in memory the child maps, so the
//! cases stay small.

use core::ffi::CStr;
use core::ptr;

use vibeos_user::rt;
use vibeos_user::sys::{self, Errno};
use vibeos_user::utest::{self, Outcome, Runner};

const ENVCHECK: &CStr = c"/bin/envcheck";
const ARGCHECK: &CStr = c"/bin/argcheck";

/// `MAX_ARG_STRLEN` (execve(2)): the longest string, its NUL included.
const MAX_ARG_STRLEN: usize = 131_072;

// From Linux `include/uapi/asm-generic/mman-common.h` (PROT_*,
// MAP_ANONYMOUS) and `include/uapi/linux/mman.h` (MAP_PRIVATE).
const PROT_READ: u64 = 0x1;
const PROT_WRITE: u64 = 0x2;
const MAP_PRIVATE: u64 = 0x02;
const MAP_ANONYMOUS: u64 = 0x20;

const PAGE: usize = 4096;

pub fn run(t: &mut Runner) {
    t.case("exec_env_k_v", || expect(env_case(c"K=v"), 0));
    t.case("exec_env_without_k_v", || expect(env_case(c"K=w"), 1));
    t.case("exec_10000_one_byte_args", || {
        expect(one_byte_args(10_000, c"ARGCHECK=10001:1"), 0)
    });
    t.case("exec_arg_131071_runs", || {
        expect(long_args(1, MAX_ARG_STRLEN - 1, c"ARGCHECK=2:131071"), 0)
    });
    t.case("exec_arg_131072_e2big", || {
        expect(
            long_args(1, MAX_ARG_STRLEN, c"ARGCHECK=2:131072"),
            100 + Errno::E2BIG.0,
        )
    });
    t.case("exec_args_near_limit_runs", || {
        expect(long_args(15, MAX_ARG_STRLEN - 1, c"ARGCHECK=16:131071"), 0)
    });
    t.case("exec_args_total_e2big", || {
        expect(
            long_args(17, MAX_ARG_STRLEN - 1, c"ARGCHECK=18:131071"),
            100 + Errno::E2BIG.0,
        )
    });
    t.case("exec_empty_argv", || expect(empty_argv(false), 0));
    t.case("exec_null_argv", || expect(empty_argv(true), 0));
    t.case("exec_arg_at_page_end", || expect(arg_at_page_end(), 0));
}

/// `s` as the byte pointer `execve` takes.
fn ptr_of(s: &CStr) -> *const u8 {
    s.as_ptr().cast()
}

/// Pass when the child exited with `code`.
fn expect(status: Result<u32, &'static str>, code: i32) -> Outcome {
    match status {
        Ok(s) if sys::exit_code(s).map(i32::from) == Some(code) => Outcome::Ok,
        Ok(s) => match sys::exit_code(s) {
            Some(0) => Outcome::Fail("exit 0, not the expected status"),
            Some(1) => Outcome::Fail("exit 1: check failed"),
            Some(2) => Outcome::Fail("exit 2: bad ARGCHECK mode"),
            Some(3) => Outcome::Fail("exit 3: wrong argc"),
            Some(4) => Outcome::Fail("exit 4: argv[0] not empty"),
            Some(5) => Outcome::Fail("exit 5: wrong argument length"),
            Some(107) => Outcome::Fail("execve returned E2BIG"),
            Some(112) => Outcome::Fail("execve returned ENOMEM"),
            Some(114) => Outcome::Fail("execve returned EFAULT"),
            Some(c) if c >= 100 => Outcome::Fail("execve returned an error"),
            Some(_) => Outcome::Fail("unexpected exit status"),
            None => Outcome::Fail("child killed by a signal"),
        },
        Err(why) => Outcome::Fail(why),
    }
}

/// Fork a child that runs `child`, which returns only if its `execve`
/// failed, and exits `100 + errno`; the child's status word.
fn fork_exec(child: impl FnOnce() -> Result<usize, Errno>) -> Result<u32, &'static str> {
    let pid = match utest::fork() {
        Ok(0) => {
            let e = match child() {
                Ok(_) => 0,
                Err(Errno(e)) => e,
            };
            rt::exit(100 + e)
        }
        Ok(pid) => pid,
        Err(_) => return Err("fork"),
    };
    let mut status = 0i32;
    // SAFETY: `wait4` writes 4 bytes through `&raw mut status`, a local no
    // reference covers, and nothing through the null rusage; established here.
    let r = unsafe { sys::wait4(pid as i32, &raw mut status, 0, ptr::null_mut()) };
    if r != Ok(pid) {
        return Err("wait4 did not reap the child");
    }
    Ok(status as u32)
}

/// `len` bytes of fresh anonymous memory, zeroed, or `None`.
fn map(len: usize) -> Option<*mut u8> {
    // SAFETY: a new private anonymous mapping at an address the kernel
    // picks covers no memory the program uses; established here.
    let va = unsafe {
        sys::mmap(
            0,
            len as u64,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            u64::MAX,
            0,
        )
    }
    .ok()?;
    Some(ptr::with_exposed_provenance_mut(va))
}

/// A vector of `n` pointers plus its NULL, in mapped memory, each from
/// `at(i)`.
fn vector(n: usize, at: impl Fn(usize) -> *const u8) -> Option<*const *const u8> {
    let v = map((n + 1) * 8)?.cast::<*const u8>();
    for i in 0..n {
        // SAFETY: `v` maps `n + 1` pointers, so slot `i < n` is in it, and
        // nothing else refers to the fresh mapping; established here.
        unsafe { v.add(i).write(at(i)) };
    }
    // The fresh mapping is zeroed, so slot `n` is already NULL.
    Some(v.cast_const())
}

/// `envcheck` with the environment `[entry]`.
fn env_case(entry: &'static CStr) -> Result<u32, &'static str> {
    fork_exec(|| {
        let argv = [ptr_of(ENVCHECK), ptr::null()];
        let envp = [ptr_of(entry), ptr::null()];
        sys::execve(ptr_of(ENVCHECK), argv.as_ptr(), envp.as_ptr())
    })
}

/// `argcheck` with `n` arguments of `"a"` after `argv[0]`.
fn one_byte_args(n: usize, mode: &'static CStr) -> Result<u32, &'static str> {
    fork_exec(|| {
        let argv = vector(n + 1, |i| {
            if i == 0 {
                ptr_of(ARGCHECK)
            } else {
                ptr_of(c"a")
            }
        })
        .ok_or(Errno::ENOMEM)?;
        let envp = [ptr_of(mode), ptr::null()];
        sys::execve(ptr_of(ARGCHECK), argv, envp.as_ptr())
    })
}

/// `argcheck` with `n` arguments of `len` bytes after `argv[0]`, every
/// one the same string.
fn long_args(n: usize, len: usize, mode: &'static CStr) -> Result<u32, &'static str> {
    fork_exec(|| {
        let buf = map(len + 1).ok_or(Errno::ENOMEM)?;
        // SAFETY: `buf` maps `len + 1` bytes that nothing else refers to;
        // the last stays the zeroed mapping's NUL; established here.
        unsafe { buf.write_bytes(b'x', len) };
        let s = buf.cast_const();
        let argv =
            vector(n + 1, |i| if i == 0 { ptr_of(ARGCHECK) } else { s }).ok_or(Errno::ENOMEM)?;
        let envp = [ptr_of(mode), ptr::null()];
        sys::execve(ptr_of(ARGCHECK), argv, envp.as_ptr())
    })
}

/// `argcheck` with `argv` `[NULL]`, or a NULL `argv` when `null`.
fn empty_argv(null: bool) -> Result<u32, &'static str> {
    fork_exec(|| {
        let empty = [ptr::null::<u8>()];
        let argv = if null { ptr::null() } else { empty.as_ptr() };
        let envp = [ptr_of(c"ARGCHECK=empty"), ptr::null()];
        sys::execve(ptr_of(ARGCHECK), argv, envp.as_ptr())
    })
}

/// `argcheck` with a 100-byte argument whose NUL is the last byte of a
/// page with nothing mapped after it: the kernel must stop at the NUL.
fn arg_at_page_end() -> Result<u32, &'static str> {
    fork_exec(|| {
        let two = map(2 * PAGE).ok_or(Errno::ENOMEM)?;
        // SAFETY: the second page of the mapping `map` just made, which
        // nothing refers to; established here.
        unsafe { sys::munmap(two.add(PAGE).addr() as u64, PAGE) }?;
        // SAFETY: bytes `PAGE - 101 .. PAGE - 1` of the first page, which
        // nothing else refers to; the page's last byte stays its NUL;
        // established here.
        let s = unsafe {
            let s = two.add(PAGE - 101);
            s.write_bytes(b'y', 100);
            s.cast_const()
        };
        let argv = [ptr_of(ARGCHECK), s, ptr::null()];
        let envp = [ptr_of(c"ARGCHECK=2:100"), ptr::null()];
        sys::execve(ptr_of(ARGCHECK), argv.as_ptr(), envp.as_ptr())
    })
}
