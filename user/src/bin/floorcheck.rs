//! The in-guest tests' program for the ROADMAP §10.5 floor calls
//! (`floor_syscalls_from_user`). Only `kernel_tests` kernels embed it.
//!
//! `floorcheck <case>` runs one case and exits 0 when every check passes.
//! On a failure it writes `floorcheck: <case>: <check>` to fd 2 and exits
//! 1. The cases:
//!
//! - `getdents`: `getdents64` over `/` and `/dev`, its errors, and the
//!   directory position.
//! - `fstat`: `fstat` on a file, a directory, the console, `/dev/null`, and
//!   its errors.
//! - `nanosleep`: a 50 ms sleep, which the kernel test times, and the
//!   call's errors.
//! - `sleepkill`: a child sleeping 10 s is killed with `SIGKILL` after
//!   100 ms; the kernel test checks the run takes under 5 s.
//! - `reboot-einval`: `reboot`'s argument checks, and the commands that
//!   change nothing.
//! - `poweroff` and `restart`: `reboot` powers the machine off or restarts
//!   it, so the case never ends; if the call returns, it fails.

#![no_std]
#![no_main]
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "a failed allocation ends this user process"
)]

use core::ffi::{CStr, c_void};

use vibeos_user::env::Env;
use vibeos_user::sys::{self, Errno, Stat};
use vibeos_user::{eprintln, rt};

vibeos_user::main!(main);

// Linux's errno values, from `include/uapi/asm-generic/errno-base.h`.
const EBADF: Errno = Errno(9);
const EFAULT: Errno = Errno(14);
const ENOTDIR: Errno = Errno(20);
const EINVAL: Errno = Errno(22);

// `d_type` values, from inode(7).
const DT_CHR: u8 = 2;
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;

// Mode bits, from inode(7).
const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFCHR: u32 = 0o020000;

// From signal(7).
const SIGKILL: i32 = 9;

// `lseek`'s whence, from lseek(2).
const SEEK_SET: u32 = 0;

// From Linux `include/uapi/asm-generic/mman-common.h` (PROT_READ,
// MAP_ANONYMOUS) and `include/uapi/linux/mman.h` (MAP_PRIVATE).
const PROT_READ: u64 = 0x1;
const MAP_PRIVATE: u64 = 0x02;
const MAP_ANONYMOUS: u64 = 0x20;

const ROOT: &CStr = c"/";
const DEV: &CStr = c"/dev";
const HELLO: &CStr = c"/hello.txt";
const NULL: &CStr = c"/dev/null";

/// A failed check: its words, for the `floorcheck: <case>: <check>` line.
type Check = Result<(), &'static str>;

fn main(env: &Env) -> i32 {
    let (name, r): (&str, Check) = match env.arg(1) {
        Some(b"getdents") => ("getdents", case_getdents()),
        Some(b"fstat") => ("fstat", case_fstat()),
        Some(b"nanosleep") => ("nanosleep", case_nanosleep()),
        Some(b"sleepkill") => ("sleepkill", case_sleepkill()),
        Some(b"reboot-einval") => ("reboot-einval", case_reboot_einval()),
        Some(b"poweroff") => ("poweroff", case_reboot_ends(CMD_POWER_OFF)),
        Some(b"restart") => ("restart", case_reboot_ends(CMD_RESTART)),
        _ => ("?", Err("unknown case")),
    };
    match r {
        Ok(()) => 0,
        Err(check) => {
            eprintln!("floorcheck: {name}: {check}");
            1
        }
    }
}

/// `ok` or the failed check `what`.
fn ensure(ok: bool, what: &'static str) -> Check {
    if ok { Ok(()) } else { Err(what) }
}

fn open(path: &CStr) -> Result<u32, &'static str> {
    match sys::open(path.as_ptr().cast(), sys::O_RDONLY, 0) {
        Ok(fd) => u32::try_from(fd).map_err(|_| "open: fd"),
        Err(_) => Err("open"),
    }
}

fn close(fd: u32) {
    // A failed close changes no check this program makes.
    if sys::close(fd).is_err() {
        eprintln!("floorcheck: close {fd} failed");
    }
}

/// A page-aligned address with nothing mapped: a page this program maps
/// and unmaps again.
fn unmapped_page() -> Result<usize, &'static str> {
    // SAFETY: a fresh anonymous mapping replaces nothing the program uses,
    // and the page is unmapped before anything points into it; established
    // here.
    let page = unsafe { sys::mmap(0, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, u64::MAX, 0) }
        .map_err(|_| "mmap")?;
    // SAFETY: nothing refers to `page`, mapped just above; established here.
    unsafe { sys::munmap(page as u64, 4096) }.map_err(|_| "munmap")?;
    Ok(page)
}

// ---- getdents ----

/// One `linux_dirent64` record read back.
#[derive(Clone, Copy)]
struct Rec<'a> {
    ino: u64,
    off: u64,
    kind: u8,
    name: &'a [u8],
}

/// The records in `buf[..n]`, a `getdents64` result.
struct Recs<'a> {
    buf: &'a [u8],
}

impl<'a> Iterator for Recs<'a> {
    type Item = Result<Rec<'a>, &'static str>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.buf.is_empty() {
            return None;
        }
        let r = parse(self.buf);
        match r {
            Ok((rec, len)) => {
                self.buf = self.buf.get(len..).unwrap_or(&[]);
                Some(Ok(rec))
            }
            Err(e) => {
                self.buf = &[];
                Some(Err(e))
            }
        }
    }
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The record at the start of `b`, and its length.
fn parse(b: &[u8]) -> Result<(Rec<'_>, usize), &'static str> {
    let ino = u64_at(b, 0).ok_or("record header")?;
    let off = u64_at(b, 8).ok_or("record header")?;
    let len = match b.get(16..18) {
        Some(&[lo, hi]) => usize::from(u16::from_le_bytes([lo, hi])),
        _ => return Err("record header"),
    };
    let kind = *b.get(18).ok_or("record header")?;
    let rec = b.get(..len).ok_or("d_reclen past the result")?;
    if len % 8 != 0 {
        return Err("d_reclen not a multiple of 8");
    }
    let tail = rec.get(19..).ok_or("d_reclen too short")?;
    let nul = tail
        .iter()
        .position(|&c| c == 0)
        .ok_or("d_name has no NUL")?;
    let name = tail.get(..nul).ok_or("d_name")?;
    if tail.get(nul..).is_some_and(|p| p.iter().any(|&c| c != 0)) {
        return Err("padding not zero");
    }
    Ok((
        Rec {
            ino,
            off,
            kind,
            name,
        },
        len,
    ))
}

fn getdents(fd: u32, buf: &mut [u8]) -> Result<usize, Errno> {
    let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    // SAFETY: the kernel writes at most `len` bytes into `buf`, which no
    // other reference covers during the call; established here.
    unsafe { sys::getdents64(fd, buf.as_mut_ptr().cast::<c_void>(), len) }
}

/// What reading a directory to its end saw.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Listing {
    count: u32,
    /// An order-free sum over the names and types.
    sum: u64,
    dot: u32,
    dotdot: u32,
    hello: u32,
    hello_ino: u64,
    etc: u32,
    chr: u32,
}

fn fnv(name: &[u8], kind: u8) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in name.iter().chain(core::iter::once(&kind)) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Read `fd` to its end in calls of `size` bytes, then check the end reads
/// 0 twice.
fn list(fd: u32, size: usize) -> Result<Listing, &'static str> {
    let mut buf = [0u8; 4096];
    let buf = buf.get_mut(..size).ok_or("size")?;
    let mut l = Listing::default();
    loop {
        let n = getdents(fd, buf).map_err(|_| "getdents64 failed mid-directory")?;
        if n == 0 {
            break;
        }
        for r in (Recs {
            buf: buf.get(..n).ok_or("byte count past the buffer")?,
        }) {
            let r = r?;
            l.count += 1;
            l.sum = l.sum.wrapping_add(fnv(r.name, r.kind));
            match r.name {
                b"." => l.dot += u32::from(r.kind == DT_DIR),
                b".." => l.dotdot += u32::from(r.kind == DT_DIR),
                b"hello.txt" => {
                    l.hello += u32::from(r.kind == DT_REG);
                    l.hello_ino = r.ino;
                }
                b"etc" => l.etc += u32::from(r.kind == DT_DIR),
                b"null" | b"zero" | b"console" => l.chr += u32::from(r.kind == DT_CHR),
                _ => {}
            }
        }
    }
    ensure(getdents(fd, buf) == Ok(0), "end did not read 0 again")?;
    Ok(l)
}

fn rewind(fd: u32) -> Check {
    ensure(sys::lseek(fd, 0, SEEK_SET) == Ok(0), "lseek to 0")
}

/// The first record `getdents64` returns from `fd`'s position.
fn first(fd: u32, buf: &mut [u8; 512]) -> Result<(u64, u64, u8, [u8; 64], usize), &'static str> {
    let n = getdents(fd, buf).map_err(|_| "getdents64")?;
    let (r, _) = parse(buf.get(..n).ok_or("byte count")?)?;
    let mut name = [0u8; 64];
    let k = r.name.len().min(64);
    name[..k].copy_from_slice(&r.name[..k]);
    Ok((r.ino, r.off, r.kind, name, k))
}

fn case_getdents() -> Check {
    let fd = open(ROOT)?;
    let r = getdents_root(fd);
    close(fd);
    r?;
    let file = open(HELLO)?;
    let mut buf = [0u8; 512];
    let r = getdents(file, &mut buf);
    close(file);
    ensure(r == Err(ENOTDIR), "a regular file did not give ENOTDIR")?;
    ensure(
        getdents(1, &mut buf) == Err(ENOTDIR),
        "fd 1 did not give ENOTDIR",
    )?;
    ensure(
        getdents(99, &mut buf) == Err(EBADF),
        "fd 99 did not give EBADF",
    )?;
    let dev = open(DEV)?;
    let l = list(dev, 4096);
    close(dev);
    let l = l?;
    ensure(l.chr == 3, "/dev lacks null, zero or console as DT_CHR")
}

fn getdents_root(fd: u32) -> Check {
    let big = list(fd, 4096)?;
    ensure(big.dot == 1, "/ lists . not once as DT_DIR")?;
    ensure(big.dotdot == 1, "/ lists .. not once as DT_DIR")?;
    ensure(big.hello == 1, "/ lists hello.txt not once as DT_REG")?;
    ensure(big.etc == 1, "/ lists etc not once as DT_DIR")?;
    rewind(fd)?;
    let small = list(fd, 88)?;
    ensure(small == big, "an 88-byte buffer lists other names")?;
    rewind(fd)?;
    let mut one = [0u8; 1];
    ensure(
        getdents(fd, &mut one) == Err(EINVAL),
        "count 1 did not give EINVAL",
    )?;
    let bad = unmapped_page()?;
    let len = 4096u32;
    // SAFETY: `bad` is unmapped (`unmapped_page` above), so the kernel
    // writes nothing; established here.
    let r = unsafe { sys::getdents64(fd, bad as *mut c_void, len) };
    ensure(r == Err(EFAULT), "an unmapped buffer did not give EFAULT")?;
    let mut buf = [0u8; 512];
    let (_, off, _, name, k) = first(fd, &mut buf)?;
    ensure(
        &name[..k] == b".",
        "after EFAULT the next call did not start at .",
    )?;
    ensure(
        sys::lseek(fd, off as i64, SEEK_SET) == Ok(off as usize),
        "lseek to d_off",
    )?;
    let (_, _, _, name, k) = first(fd, &mut buf)?;
    ensure(
        &name[..k] == b"..",
        "lseek to .'s d_off did not resume at ..",
    )?;
    rewind(fd)?;
    let (_, _, _, name, k) = first(fd, &mut buf)?;
    ensure(&name[..k] == b".", "lseek to 0 did not rewind")
}

// ---- fstat ----

fn stat(fd: u32) -> Result<Stat, Errno> {
    let mut st = Stat::default();
    // SAFETY: the kernel writes 144 bytes into `st`, which no other
    // reference covers during the call; established here.
    unsafe { sys::fstat(fd, (&raw mut st).cast::<c_void>()) }?;
    Ok(st)
}

fn fmt(st: &Stat) -> u32 {
    st.st_mode & S_IFMT
}

/// The bytes read from `fd` to end of file.
fn size_by_reading(fd: u32) -> Result<u64, &'static str> {
    let mut buf = [0u8; 256];
    let mut total = 0u64;
    loop {
        // SAFETY: the kernel writes at most 256 bytes into `buf`, which no
        // other reference covers during the call; established here.
        match unsafe { sys::read(fd, buf.as_mut_ptr(), buf.len()) } {
            Ok(0) => return Ok(total),
            Ok(n) => total += n as u64,
            Err(_) => return Err("read"),
        }
    }
}

/// `hello.txt`'s `d_ino` in `/`.
fn hello_ino() -> Result<u64, &'static str> {
    let fd = open(ROOT)?;
    let l = list(fd, 4096);
    close(fd);
    let l = l?;
    ensure(l.hello == 1, "no hello.txt in /")?;
    Ok(l.hello_ino)
}

fn case_fstat() -> Check {
    let fd = open(HELLO)?;
    let st = stat(fd);
    let size = size_by_reading(fd);
    close(fd);
    let st = st.map_err(|_| "fstat /hello.txt")?;
    let size = size?;
    ensure(fmt(&st) == S_IFREG, "/hello.txt is not S_IFREG")?;
    ensure(st.st_size as u64 == size, "st_size is not the bytes read")?;
    ensure(
        st.st_blocks as u64 == size.div_ceil(512),
        "st_blocks is not ceil(size/512)",
    )?;
    ensure(st.st_blksize == 4096, "st_blksize is not 4096")?;
    ensure(st.st_ino == hello_ino()?, "st_ino is not hello.txt's d_ino")?;
    let root = open(ROOT)?;
    let st = stat(root);
    close(root);
    ensure(st.is_ok_and(|s| fmt(&s) == S_IFDIR), "/ is not S_IFDIR")?;
    ensure(
        stat(1).is_ok_and(|s| fmt(&s) == S_IFCHR),
        "fd 1 is not S_IFCHR",
    )?;
    let null = open(NULL)?;
    let st = stat(null);
    close(null);
    ensure(
        st.is_ok_and(|s| fmt(&s) == S_IFCHR),
        "/dev/null is not S_IFCHR",
    )?;
    ensure(stat(99) == Err(EBADF), "fd 99 did not give EBADF")?;
    let bad = unmapped_page()?;
    // SAFETY: `bad` is unmapped (`unmapped_page` above), so the kernel
    // writes nothing; established here.
    let r = unsafe { sys::fstat(1, bad as *mut c_void) };
    ensure(r == Err(EFAULT), "an unmapped statbuf did not give EFAULT")?;
    let text = rt::entry_address();
    // SAFETY: the program's text is mapped read-only (the loader follows
    // its segment flags), so the kernel's write faults and writes nothing;
    // established here.
    let r = unsafe { sys::fstat(1, text as *mut c_void) };
    ensure(r == Err(EFAULT), "a read-only statbuf did not give EFAULT")
}

// ---- nanosleep ----

/// A `struct __kernel_timespec`: seconds, then nanoseconds.
type Timespec = [i64; 2];

fn nanosleep(ts: &Timespec) -> Result<usize, Errno> {
    sys::nanosleep(ts.as_ptr().cast::<c_void>(), core::ptr::null_mut())
}

fn case_nanosleep() -> Check {
    ensure(
        nanosleep(&[0, 50_000_000]) == Ok(0),
        "50 ms did not return 0",
    )?;
    for (ts, what) in [
        ([0, 1_000_000_000], "nsec 10^9 did not give EINVAL"),
        ([0, -1], "nsec -1 did not give EINVAL"),
        ([-1, 0], "sec -1 did not give EINVAL"),
    ] {
        ensure(nanosleep(&ts) == Err(EINVAL), what)?;
    }
    let null = sys::nanosleep(core::ptr::null(), core::ptr::null_mut());
    ensure(null == Err(EFAULT), "a NULL rqtp did not give EFAULT")?;
    let bad = unmapped_page()?;
    let r = sys::nanosleep(bad as *const c_void, core::ptr::null_mut());
    ensure(r == Err(EFAULT), "an unmapped rqtp did not give EFAULT")?;
    ensure(nanosleep(&[0, 0]) == Ok(0), "{0,0} did not return 0")?;
    let ts: Timespec = [0, 1_000_000];
    let r = sys::nanosleep(ts.as_ptr().cast::<c_void>(), bad as *mut c_void);
    ensure(r == Ok(0), "1 ms with an unmapped rmtp did not return 0")
}

/// `WIFSIGNALED` and `WTERMSIG`, from wait(2).
fn term_signal(status: i32) -> Option<i32> {
    let sig = status & 0x7f;
    (sig != 0 && sig != 0x7f).then_some(sig)
}

fn case_sleepkill() -> Check {
    let child = match sys::fork() {
        Ok(0) => {
            if nanosleep(&[10, 0]).is_err() {
                rt::exit(2);
            }
            rt::exit(3);
        }
        Ok(pid) => i32::try_from(pid).map_err(|_| "fork: pid")?,
        Err(_) => return Err("fork"),
    };
    let slept = nanosleep(&[0, 100_000_000]);
    let killed = sys::kill(child, SIGKILL);
    let mut status = 0i32;
    // SAFETY: the kernel writes 4 bytes into `status`, which no other
    // reference covers during the call; established here.
    let waited = unsafe { sys::wait4(child, &raw mut status, 0, core::ptr::null_mut()) };
    ensure(slept == Ok(0), "the parent's 100 ms did not return 0")?;
    ensure(killed == Ok(0), "kill did not return 0")?;
    ensure(waited == Ok(child as usize), "wait4 did not reap the child")?;
    ensure(
        term_signal(status) == Some(SIGKILL),
        "wait4 did not report SIGKILL",
    )
}

// ---- reboot ----

// reboot(2)'s magic numbers and commands.
const MAGIC1: i32 = 0xfee1_dead_u32 as i32;
const MAGIC2: [i32; 4] = [672_274_793, 85_072_278, 369_367_448, 537_993_216];
const CMD_RESTART: u32 = 0x0123_4567;
const CMD_HALT: u32 = 0xcdef_0123;
const CMD_CAD_ON: u32 = 0x89ab_cdef;
const CMD_CAD_OFF: u32 = 0;
const CMD_POWER_OFF: u32 = 0x4321_fedc;
const CMD_RESTART2: u32 = 0xa1b2_c3d4;

fn reboot(magic1: i32, magic2: i32, cmd: u32, arg: *mut c_void) -> Result<usize, Errno> {
    sys::reboot(magic1, magic2, cmd, arg)
}

fn case_reboot_einval() -> Check {
    let none = core::ptr::null_mut();
    let m2 = MAGIC2[0];
    ensure(
        reboot(0x0fee_1dea, m2, CMD_CAD_OFF, none) == Err(EINVAL),
        "a bad magic1 did not give EINVAL",
    )?;
    ensure(
        reboot(MAGIC1, 0x2812_1968, CMD_CAD_OFF, none) == Err(EINVAL),
        "a bad magic2 did not give EINVAL",
    )?;
    ensure(
        reboot(MAGIC1, m2, 0x1234_5678, none) == Err(EINVAL),
        "cmd 0x12345678 did not give EINVAL",
    )?;
    ensure(
        reboot(MAGIC1, m2, CMD_HALT, none) == Err(EINVAL),
        "HALT did not give EINVAL",
    )?;
    let bad = unmapped_page()?;
    ensure(
        reboot(MAGIC1, m2, CMD_RESTART2, bad as *mut c_void) == Err(EFAULT),
        "RESTART2 with an unmapped arg did not give EFAULT",
    )?;
    for m2 in MAGIC2 {
        ensure(
            reboot(MAGIC1, m2, CMD_CAD_ON, none) == Ok(0),
            "CAD_ON did not return 0",
        )?;
        ensure(
            reboot(MAGIC1, m2, CMD_CAD_OFF, none) == Ok(0),
            "CAD_OFF did not return 0",
        )?;
    }
    Ok(())
}

/// `reboot(cmd)`, which must not return.
fn case_reboot_ends(cmd: u32) -> Check {
    match reboot(MAGIC1, MAGIC2[0], cmd, core::ptr::null_mut()) {
        Ok(_) => Err("reboot returned 0"),
        Err(_) => Err("reboot returned an error"),
    }
}
