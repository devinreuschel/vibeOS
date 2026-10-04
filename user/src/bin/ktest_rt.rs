//! The in-guest test `user_runtime`'s program (ROADMAP §10.5). Only
//! `kernel_tests` kernels embed it (the `ktest_` prefix).
//!
//! `argv[1]` names a mode, and the exit status is 0 on success or a code
//! naming the failed check:
//!
//! - `args` (10-19): the arguments arrive intact, a synthetic initial stack
//!   with an environment and an auxiliary vector parses, and the real one's
//!   `AT_PAGESZ` and `AT_ENTRY` are right.
//! - `mem` (20-29): `memcpy`, `memmove` both ways, `memset`, `memcmp`, `bcmp`.
//! - `fp` (30-39): `f64` and `f32` arithmetic.
//! - `panic-capture` (40-50): a forked child points fd 2 at a tmpfs file and
//!   panics; it must exit 101, and the file must hold the panic line. The
//!   file is emptied after, whatever the checks found (50 when that fails).
//! - `panic`: panics with fd 2 on the console, so exits 101.

#![no_std]
#![no_main]

use core::hint::black_box;

use vibeos_user::env::{self, Env};
use vibeos_user::{cmd, rt, sys};

vibeos_user::main!(main);

/// The arguments the kernel test passes after the mode: an empty one, one
/// with a space, and one of non-ASCII bytes.
const ARGS: [&[u8]; 3] = [b"", b"a b", "\u{e9}\u{2603}".as_bytes()];

/// The file the panic-capture child writes its fd 2 to.
const CAPTURE: &core::ffi::CStr = c"/tmp/ktest_rt_panic";

/// The message the panic modes panic with.
const MESSAGE: &str = "ktest_rt panic mode";

fn main(env: &Env) -> i32 {
    match env.arg(1) {
        Some(b"args") => args(env),
        Some(b"mem") => mem(),
        Some(b"fp") => fp(),
        Some(b"panic-capture") => panic_capture(),
        Some(b"panic") => panic!("{MESSAGE}"),
        _ => 2,
    }
}

fn args(env: &Env) -> i32 {
    if env.argc() != 2 + ARGS.len() {
        return 10;
    }
    if env.arg(0) != Some(b"ktest_rt".as_slice()) {
        return 11;
    }
    for (i, want) in ARGS.iter().enumerate() {
        if env.arg(2 + i) != Some(*want) {
            return 12;
        }
    }
    if env.args().count() != env.argc() || env.arg(env.argc()).is_some() {
        return 13;
    }
    if env.aux(env::AT_PAGESZ) != Some(4096) {
        return 14;
    }
    if env.aux(env::AT_ENTRY) != Some(vibeos_user::rt::entry_address()) {
        return 15;
    }
    synthetic()
}

/// Parse an initial stack built here, with an environment and an auxiliary
/// vector, since the kernel passes an empty environment.
fn synthetic() -> i32 {
    let a0 = b"prog\0";
    let e0 = b"K=v\0";
    let e1 = b"EMPTY=\0";
    let stack: [usize; 12] = black_box([
        1,
        a0.as_ptr() as usize,
        0,
        e0.as_ptr() as usize,
        e1.as_ptr() as usize,
        0,
        env::AT_PAGESZ,
        4096,
        env::AT_ENTRY,
        0x1234,
        env::AT_NULL,
        0,
    ]);
    // SAFETY: `stack` is an initial stack in the layout `Env::from_stack`
    // documents (argc, argv and NULL, envp and NULL, auxv ending in
    // AT_NULL), its strings are 'static, and `e` is not used after `stack`
    // goes; established here.
    let e = unsafe { Env::from_stack(stack.as_ptr()) };
    if e.argc() != 1 || e.arg(0) != Some(b"prog".as_slice()) || e.arg(1).is_some() {
        return 16;
    }
    let mut vars = e.vars();
    if vars.next() != Some(b"K=v".as_slice())
        || vars.next() != Some(b"EMPTY=".as_slice())
        || vars.next().is_some()
    {
        return 17;
    }
    if e.var(b"K") != Some(b"v".as_slice())
        || e.var(b"EMPTY") != Some(b"".as_slice())
        || e.var(b"KX").is_some()
        || e.var(b"").is_some()
    {
        return 18;
    }
    if e.aux(env::AT_PAGESZ) != Some(4096)
        || e.aux(env::AT_ENTRY) != Some(0x1234)
        || e.aux(env::AT_RANDOM).is_some()
    {
        return 19;
    }
    0
}

fn mem() -> i32 {
    use core::ffi::c_void;
    unsafe extern "C" {
        fn memcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void;
        fn memmove(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void;
        fn memset(d: *mut c_void, c: i32, n: usize) -> *mut c_void;
        fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32;
        fn bcmp(a: *const c_void, b: *const c_void, n: usize) -> i32;
    }
    let mut buf = [0u8; 32];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = i as u8;
    }
    let buf = black_box(buf);
    let n = black_box(16usize);
    let mut dst = [0u8; 32];
    // SAFETY: both arrays hold 32 bytes and `n` is 16; established here.
    unsafe { memcpy(dst.as_mut_ptr().cast(), buf.as_ptr().cast(), n) };
    if dst[..16] != buf[..16] || dst[16..] != [0; 16] {
        return 20;
    }
    // Forward overlap: move [0, 16) to [4, 20).
    let mut m = buf;
    // SAFETY: both ranges lie inside `m`'s 32 bytes; established here.
    unsafe { memmove(m.as_mut_ptr().add(4).cast(), m.as_ptr().cast(), n) };
    if m[..4] != buf[..4] || m[4..20] != buf[..16] || m[20..] != buf[20..] {
        return 21;
    }
    // Backward overlap: move [4, 20) to [0, 16).
    let mut m = buf;
    // SAFETY: both ranges lie inside `m`'s 32 bytes; established here.
    unsafe { memmove(m.as_mut_ptr().cast(), m.as_ptr().add(4).cast(), n) };
    if m[..16] != buf[4..20] || m[16..] != buf[16..] {
        return 22;
    }
    let mut s = buf;
    // SAFETY: the range lies inside `s`'s 32 bytes; established here.
    unsafe { memset(s.as_mut_ptr().add(8).cast(), black_box(0x1a5), n) };
    if s[..8] != buf[..8] || s[8..24] != [0xa5; 16] || s[24..] != buf[24..] {
        return 23;
    }
    let mut c = buf;
    c[10] = 200;
    // SAFETY: every range lies inside a 32-byte array; established here.
    let (eq, lt, gt, b_eq, b_ne) = unsafe {
        (
            memcmp(buf.as_ptr().cast(), buf.as_ptr().cast(), 32),
            memcmp(buf.as_ptr().cast(), c.as_ptr().cast(), 32),
            memcmp(c.as_ptr().cast(), buf.as_ptr().cast(), 32),
            bcmp(buf.as_ptr().cast(), buf.as_ptr().cast(), 32),
            bcmp(buf.as_ptr().cast(), c.as_ptr().cast(), 32),
        )
    };
    if eq != 0 || lt >= 0 || gt <= 0 || b_eq != 0 || b_ne == 0 {
        return 24;
    }
    0
}

fn fp() -> i32 {
    let a = black_box(1.5f64);
    let b = black_box(2.25f64);
    if a * b != 3.375 || a + b != 3.75 || b - a != 0.75 || b / a != 1.5 {
        return 30;
    }
    if black_box(3i32) as f64 * a != 4.5 || (b * 4.0) as i64 != 9 {
        return 31;
    }
    let x = black_box(0.1f32);
    let y = black_box(0.2f32);
    let z = x + y;
    if !(0.29999 < z && z < 0.30001) || !(0.29999 < z as f64 && (z as f64) < 0.30001) {
        return 32;
    }
    if (a as f32) * black_box(0.25f32) != 0.375 || black_box(7.9f32) as i32 != 7 {
        return 33;
    }
    0
}

fn panic_capture() -> i32 {
    let pid = match sys::fork() {
        Ok(0) => {
            let flags = sys::O_WRONLY | sys::O_CREAT | sys::O_TRUNC;
            let Ok(fd) = sys::open(CAPTURE.as_ptr().cast(), flags, 0o644) else {
                rt::exit(41);
            };
            if sys::dup2(fd as u32, 2).is_err() {
                rt::exit(42);
            }
            panic!("{MESSAGE}");
        }
        Ok(pid) => pid,
        Err(_) => return 40,
    };
    let code = captured(pid);
    // The file's `/tmp` page goes back whatever the checks found: `/tmp`
    // is one store for the whole boot.
    match cmd::discard(CAPTURE.to_bytes()) {
        Ok(()) => code,
        Err(_) if code == 0 => 50,
        Err(_) => code,
    }
}

/// Reap the panic-capture child `pid` and check what it left in
/// [`CAPTURE`]: 0, or the failed check's code.
fn captured(pid: usize) -> i32 {
    let mut status = 0i32;
    // SAFETY: `wait4` writes 4 bytes through `&raw mut status`, a local no
    // reference covers, and nothing through the null rusage; established here.
    let r = unsafe { sys::wait4(pid as i32, &raw mut status, 0, core::ptr::null_mut()) };
    if r != Ok(pid) {
        return 43;
    }
    match sys::exit_code(status as u32) {
        Some(101) => {}
        Some(41) => return 44,
        Some(42) => return 45,
        _ => return 46,
    }
    let Ok(fd) = sys::open(CAPTURE.as_ptr().cast(), sys::O_RDONLY, 0) else {
        return 47;
    };
    let mut buf = [0u8; 256];
    // SAFETY: `read` writes at most `buf.len()` bytes into `buf`, a local no
    // other reference covers; established here.
    let n = unsafe { sys::read(fd as u32, buf.as_mut_ptr(), buf.len()) }.unwrap_or(0);
    let got = &buf[..n];
    if !got.starts_with(b"panicked at ") {
        return 48;
    }
    if !got.windows(MESSAGE.len()).any(|w| w == MESSAGE.as_bytes()) {
        return 49;
    }
    0
}
