//! `errno_matrix` (ROADMAP §10.5, F077): every syscall with valid
//! arguments, and every `(call, errno)` pair SYSCALL.md §3 lists, from the
//! generated `sys::CALLS`.
//!
//! Each listed pair has one entry in [`PAIRS`]: a probe that provokes it
//! and returns what the call returned, the name of another `/bin/tests`
//! case that provokes it (`Delegate`), or, for a pair only a kernel fault
//! produces, the in-guest test the table names (`Ktest`, reported and not
//! run). A listed pair with no entry, an entry for a pair the table does
//! not list, and a probe that gets anything but `-errno` each fail the
//! case. The case runs every pair, names each failure on an info line, and
//! fails naming the first. Each call but `exit` (the child cases call it)
//! and `reboot` (whose valid call powers off) also has a success probe in
//! [`OKS`].
//!
//! The helpers here are the other suites' too (`efault`, `lifecycle`).

use core::ffi::CStr;
use core::fmt;

use vibeos_user::sys::{self, CALLS, Errno, Sys};
use vibeos_user::utest::{self, Got, Outcome, Runner};

// From Linux `include/uapi/asm-generic/fcntl.h`.
/// Fail if the file exists.
pub(super) const O_EXCL: i32 = 0o200;
// From Linux `include/uapi/linux/fs.h`.
/// `lseek` from the start.
pub(super) const SEEK_SET: u32 = 0;
/// `lseek` from the end.
pub(super) const SEEK_END: u32 = 2;
// From Linux `include/uapi/asm-generic/mman-common.h` and `mman.h`.
/// No access.
pub(super) const PROT_NONE: u64 = 0;
/// Readable.
pub(super) const PROT_READ: u64 = 1;
/// Writable.
pub(super) const PROT_WRITE: u64 = 2;
/// Private.
pub(super) const MAP_PRIVATE: u64 = 0x02;
/// At exactly `addr`.
pub(super) const MAP_FIXED: u64 = 0x10;
/// Anonymous.
pub(super) const MAP_ANONYMOUS: u64 = 0x20;
/// At exactly `addr`, never over a mapping.
pub(super) const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;
// From signal(7).
/// Kill.
pub(super) const SIGKILL: i32 = 9;
/// Stop.
pub(super) const SIGSTOP: i32 = 19;
// From wait(2).
/// Return at once when no child has exited.
pub(super) const WNOHANG: i32 = 1;
/// A kernel-half address: never a user pointer.
pub(super) const KERNEL_PTR: u64 = 0xFFFF_8000_0000_1000;
/// A page.
pub(super) const PAGE: u64 = 4096;

/// `/hello`, which exits 42.
pub(super) const HELLO: &CStr = c"/hello";
/// `/hello`'s exit code.
pub(super) const HELLO_EXIT: u8 = 42;

const NAME: &str = "errno_matrix";

/// What a probe got: the call's own result, or why its setup failed
/// ([`NOT_HERE`]: it does not run in this configuration).
type Probe = fn() -> Result<Result<usize, Errno>, &'static str>;

/// A probe's setup failure that is no failure: the probe does not run in
/// this configuration, and says why on an info line.
const NOT_HERE: &str = "not run in a kernel-parented run (no init)";

/// How a pair is covered.
#[derive(Clone, Copy)]
enum How {
    /// This probe provokes it.
    Run(Probe),
    /// The `/bin/tests` case named provokes it.
    Delegate(&'static str),
    /// Only a kernel fault produces it: the row's in-guest test does.
    Ktest,
}

struct Pair {
    sys: Sys,
    errno: Errno,
    how: How,
}

const fn pair(sys: Sys, errno: Errno, how: How) -> Pair {
    Pair { sys, errno, how }
}

/// The cases other suites run, which `errno_matrix` delegates to.
const DELEGATES: [&str; 2] = ["fork_bomb_eagain_at_limit", "exec_arg_131072_e2big"];

use How::{Delegate, Ktest, Run};

/// Every listed pair's coverage, in table order.
const PAIRS: &[Pair] = &[
    pair(Sys::Read, Errno::EBADF, Run(|| Ok(read_into(99, 16)))),
    pair(Sys::Read, Errno::EFAULT, Run(read_efault)),
    pair(Sys::Read, Errno::EISDIR, Run(read_eisdir)),
    pair(Sys::Read, Errno::EIO, Ktest),
    pair(
        Sys::Write,
        Errno::EBADF,
        Run(|| Ok(sys::write(99, b"x".as_ptr(), 1))),
    ),
    pair(
        Sys::Write,
        Errno::EFAULT,
        Run(|| Ok(sys::write(1, KERNEL_PTR as *const u8, 1))),
    ),
    pair(Sys::Write, Errno::EINVAL, Run(write_einval)),
    pair(Sys::Write, Errno::EFBIG, Run(write_efbig)),
    pair(Sys::Write, Errno::ENOSPC, Run(write_enospc)),
    pair(Sys::Write, Errno::EIO, Ktest),
    pair(
        Sys::Open,
        Errno::EFAULT,
        Run(|| Ok(sys::open(KERNEL_PTR as *const u8, 0, 0))),
    ),
    pair(Sys::Open, Errno::ENAMETOOLONG, Run(open_enametoolong)),
    pair(
        Sys::Open,
        Errno::EINVAL,
        Run(|| Ok(open_raw(c"", sys::O_RDONLY))),
    ),
    pair(
        Sys::Open,
        Errno::ENOENT,
        Run(|| Ok(open_raw(c"/utest_none", sys::O_RDONLY))),
    ),
    pair(
        Sys::Open,
        Errno::ENOTDIR,
        Run(|| Ok(open_raw(c"/hello/x", sys::O_RDONLY))),
    ),
    pair(
        Sys::Open,
        Errno::EISDIR,
        Run(|| Ok(open_raw(c"/", sys::O_WRONLY))),
    ),
    pair(
        Sys::Open,
        Errno::EEXIST,
        Run(|| Ok(open_raw(HELLO, sys::O_CREAT | O_EXCL))),
    ),
    pair(Sys::Open, Errno::EACCES, Run(open_eacces)),
    pair(
        Sys::Open,
        Errno::ELOOP,
        Run(|| Ok(open_raw(LOOP, sys::O_RDONLY))),
    ),
    pair(Sys::Open, Errno::EMFILE, Run(open_emfile)),
    pair(Sys::Open, Errno::ENFILE, Run(open_enfile)),
    pair(Sys::Open, Errno::ENOSPC, Run(open_enospc)),
    pair(Sys::Open, Errno::ENOMEM, Ktest),
    pair(Sys::Open, Errno::EIO, Ktest),
    pair(Sys::Close, Errno::EBADF, Run(|| Ok(sys::close(99)))),
    pair(Sys::Fstat, Errno::EBADF, Run(|| Ok(fstat_into(99)))),
    pair(Sys::Fstat, Errno::EFAULT, Run(fstat_efault)),
    pair(
        Sys::Lseek,
        Errno::EBADF,
        Run(|| Ok(sys::lseek(99, 0, SEEK_SET))),
    ),
    pair(
        Sys::Lseek,
        Errno::ESPIPE,
        Run(|| Ok(sys::lseek(1, 0, SEEK_SET))),
    ),
    pair(Sys::Lseek, Errno::EINVAL, Run(lseek_einval)),
    pair(
        Sys::Mmap,
        Errno::EINVAL,
        Run(|| Ok(mmap_raw(0, PAGE, PROT_READ, ANON, 1))),
    ),
    pair(Sys::Mmap, Errno::EBADF, Run(|| Ok(mmap_fd(200)))),
    pair(Sys::Mmap, Errno::ENODEV, Run(|| Ok(mmap_fd(1)))),
    pair(
        Sys::Mmap,
        Errno::ENOMEM,
        Run(|| Ok(mmap_raw(0, 1 << 47, PROT_READ, ANON, 0))),
    ),
    pair(
        Sys::Mmap,
        Errno::EPERM,
        Run(|| Ok(mmap_raw(0, PAGE, PROT_READ, ANON | MAP_FIXED, 0))),
    ),
    pair(Sys::Mmap, Errno::EEXIST, Run(mmap_eexist)),
    pair(Sys::Munmap, Errno::EINVAL, Run(munmap_einval)),
    pair(Sys::Munmap, Errno::ENOMEM, Run(munmap_enomem)),
    pair(Sys::Dup, Errno::EBADF, Run(|| Ok(sys::dup(99)))),
    pair(Sys::Dup, Errno::EMFILE, Run(dup_emfile)),
    pair(Sys::Dup2, Errno::EBADF, Run(|| Ok(sys::dup2(99, 50)))),
    pair(Sys::Nanosleep, Errno::EFAULT, Run(nanosleep_efault)),
    pair(
        Sys::Nanosleep,
        Errno::EINVAL,
        Run(|| Ok(sleep_ts(0, 1_000_000_000))),
    ),
    pair(
        Sys::Fork,
        Errno::EAGAIN,
        Delegate("fork_bomb_eagain_at_limit"),
    ),
    pair(Sys::Fork, Errno::ENOMEM, Ktest),
    pair(
        Sys::Execve,
        Errno::EFAULT,
        Run(|| exec_errno(KERNEL_PTR as *const u8)),
    ),
    pair(Sys::Execve, Errno::ENAMETOOLONG, Run(execve_enametoolong)),
    pair(
        Sys::Execve,
        Errno::EINVAL,
        Run(|| exec_errno(c"".as_ptr().cast())),
    ),
    pair(
        Sys::Execve,
        Errno::ENOENT,
        Run(|| exec_errno(c"/utest_none".as_ptr().cast())),
    ),
    pair(
        Sys::Execve,
        Errno::ENOTDIR,
        Run(|| exec_errno(c"/hello/x".as_ptr().cast())),
    ),
    pair(
        Sys::Execve,
        Errno::ELOOP,
        Run(|| exec_errno(LOOP.as_ptr().cast())),
    ),
    pair(Sys::Execve, Errno::ENFILE, Run(execve_enfile)),
    pair(Sys::Execve, Errno::E2BIG, Delegate("exec_arg_131072_e2big")),
    pair(Sys::Execve, Errno::ENOEXEC, Run(execve_enoexec)),
    pair(Sys::Execve, Errno::ENOMEM, Run(execve_enomem)),
    pair(Sys::Wait4, Errno::ECHILD, Run(wait4_echild)),
    pair(Sys::Wait4, Errno::EFAULT, Run(wait4_efault)),
    pair(Sys::Kill, Errno::EINVAL, Run(kill_einval)),
    pair(
        Sys::Kill,
        Errno::ESRCH,
        Run(|| Ok(sys::kill(0, utest::SIGCONT))),
    ),
    pair(
        Sys::Fcntl,
        Errno::EBADF,
        Run(|| Ok(sys::fcntl(99, F_GETFD, 0))),
    ),
    pair(
        Sys::Fcntl,
        Errno::EINVAL,
        Run(|| Ok(sys::fcntl(1, 9999, 0))),
    ),
    pair(
        Sys::Reboot,
        Errno::EINVAL,
        Run(|| Ok(sys::reboot(1, 2, 0, core::ptr::null_mut()))),
    ),
    pair(Sys::Reboot, Errno::EFAULT, Run(reboot_efault)),
    pair(
        Sys::Getdents64,
        Errno::EBADF,
        Run(|| Ok(dents_into(99, 512))),
    ),
    pair(
        Sys::Getdents64,
        Errno::ENOTDIR,
        Run(|| Ok(dents_into(1, 512))),
    ),
    pair(Sys::Getdents64, Errno::ESPIPE, Run(getdents64_espipe)),
    pair(Sys::Getdents64, Errno::EINVAL, Run(getdents64_einval)),
    pair(Sys::Getdents64, Errno::EFAULT, Run(getdents64_efault)),
    pair(Sys::Psinfo, Errno::EFAULT, Run(psinfo_efault)),
];

/// A success probe: `Err` says what went wrong.
type OkProbe = fn() -> Result<(), &'static str>;

/// Each call's valid use, but `exit` and `reboot`'s.
const OKS: &[(Sys, OkProbe)] = &[
    (Sys::Read, read_ok),
    (Sys::Write, write_ok),
    (Sys::Open, open_ok),
    (Sys::Close, open_ok),
    (Sys::Fstat, fstat_ok),
    (Sys::Lseek, lseek_ok),
    (Sys::Mmap, mmap_ok),
    (Sys::Munmap, mmap_ok),
    (Sys::Brk, brk_ok),
    (Sys::SchedYield, || ok_is(sys::sched_yield(), 0)),
    (Sys::Dup, dup_ok),
    (Sys::Dup2, dup2_ok),
    (Sys::Nanosleep, || ok_is(sleep_ts(0, 1_000_000), 0)),
    (Sys::Getpid, || ok_nonzero(sys::getpid())),
    (Sys::Fork, fork_ok),
    (Sys::Execve, execve_ok),
    (Sys::Wait4, fork_ok),
    (Sys::Kill, kill_ok),
    (Sys::Fcntl, || ok_is(sys::fcntl(1, F_GETFD, 0), 0)),
    (Sys::Getppid, || {
        sys::getppid().map(|_| ()).map_err(|_| "getppid failed")
    }),
    (Sys::Getdents64, getdents64_ok),
    (Sys::Psinfo, psinfo_ok),
];

/// The calls with no success probe, and why.
const NO_OK: &[Sys] = &[Sys::Exit, Sys::Reboot];

/// The case's deadline: about 3x its run under TCG at `-smp 2` (under a
/// second), rounded up to 5 s, and at least 10 s.
const DEADLINE_MS: u32 = 10_000;

pub fn run(t: &mut Runner) {
    let registered = DELEGATES.map(|d| t.is_registered(d));
    t.case_ms(NAME, DEADLINE_MS, move || matrix(&registered));
}

/// A matrix case's failures so far: each is named on an info line, and
/// the first fails the case.
pub(super) struct Failures {
    name: &'static str,
    n: u32,
    first: Option<Outcome>,
}

impl Failures {
    pub(super) const fn new(name: &'static str) -> Self {
        Self {
            name,
            n: 0,
            first: None,
        }
    }

    /// One failure.
    pub(super) fn add(&mut self, what: fmt::Arguments<'_>) {
        self.n += 1;
        utest::info(self.name, what);
        if self.first.is_none() {
            self.first = Some(utest::fail(what));
        }
    }

    /// The case's outcome: `Ok`, or the first failure.
    pub(super) fn outcome(self) -> Outcome {
        match self.first {
            None => Outcome::Ok,
            Some(first) => {
                if self.n > 1 {
                    let n = self.n;
                    utest::info(
                        self.name,
                        format_args!("{n} failures; the first fails the case"),
                    );
                }
                first
            }
        }
    }
}

fn matrix(registered: &[bool; 2]) -> Outcome {
    let mut f = Failures::new(NAME);
    for call in CALLS {
        let name = call.sys.name();
        for &e in call.errors {
            let Some(p) = PAIRS.iter().find(|p| p.sys == call.sys && p.errno == e) else {
                f.add(format_args!(
                    "{name} -{}: listed, but no case provokes it",
                    e.0
                ));
                continue;
            };
            let attributed = call.ktest.iter().find(|(k, _)| *k == e).map(|(_, t)| *t);
            match (p.how, attributed) {
                (Ktest, Some(test)) => {
                    utest::info(NAME, format_args!("{name} -{}: in-guest {test}", e.0));
                }
                (Ktest, None) => {
                    f.add(format_args!(
                        "{name} -{}: Ktest, but the row names no test",
                        e.0
                    ));
                }
                (_, Some(test)) => {
                    f.add(format_args!(
                        "{name} -{}: the row names {test}, but a case runs",
                        e.0
                    ));
                }
                (Delegate(case), None) => {
                    let i = DELEGATES.iter().position(|d| *d == case);
                    if i.and_then(|i| registered.get(i)) != Some(&true) {
                        f.add(format_args!("{name} -{}: {case} is not registered", e.0));
                    }
                }
                (Run(probe), None) => match probe() {
                    Ok(r) if r == Err(e) => {}
                    Ok(r) => f.add(format_args!("{name} -{}: got {}", e.0, Got(r))),
                    Err(NOT_HERE) => utest::info(NAME, format_args!("{name} -{}: {NOT_HERE}", e.0)),
                    Err(why) => f.add(format_args!("{name} -{}: setup: {why}", e.0)),
                },
            }
        }
        if NO_OK.contains(&call.sys) {
            continue;
        }
        match OKS.iter().find(|(s, _)| *s == call.sys) {
            None => f.add(format_args!("{name}: no success case")),
            Some((_, probe)) => {
                if let Err(why) = probe() {
                    f.add(format_args!("{name}: valid call: {why}"));
                }
            }
        }
    }
    for p in PAIRS {
        let listed = CALLS
            .iter()
            .any(|c| c.sys == p.sys && c.errors.contains(&p.errno));
        if !listed {
            let e = p.errno.0;
            f.add(format_args!(
                "{} -{e}: a case for a pair the table does not list",
                p.sys.name()
            ));
        }
    }
    f.outcome()
}

// ------------------------------------------------------------- helpers

/// Linux `F_GETFD`.
const F_GETFD: u32 = 1;
/// A private anonymous mapping.
const ANON: u64 = MAP_PRIVATE | MAP_ANONYMOUS;
/// Nine `/proc/self` links in one walk, one over `MAX_SYMLINK`.
const LOOP: &CStr = c"/proc/self/../self/../self/../self/../self/../self/../self/../self/../self";

/// `open(path, flags, 0644)`.
pub(super) fn open_raw(path: &CStr, flags: i32) -> Result<usize, Errno> {
    sys::open(path.as_ptr().cast(), flags, 0o644)
}

/// `open`, the descriptor as `u32`.
pub(super) fn open(path: &CStr, flags: i32) -> Result<u32, Errno> {
    open_raw(path, flags).map(|fd| fd as u32)
}

/// Close `fd`; a close that fails changes nothing a case checks.
pub(super) fn close(fd: u32) {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5: a failed close leaves a descriptor the process's exit closes"
    )]
    let _ = sys::close(fd);
}

/// The one file `/bin/tests`' matrices and lifecycle cases write, on the
/// root FAT volume, and truncated after each use ([`discard`]). Nothing
/// has `unlink` yet, so every name made stays for the boot: `/tmp` takes
/// none, since its nodes come from kernfs' 128, which the `kernel_tests`
/// boot's in-guest tests need nearly all of, and an `execve` from `/tmp`
/// runs deeper than a user thread's stack budget (ROADMAP §10.2's
/// stack-depth check).
pub(super) const SCRATCH: &CStr = c"/utest_scratch";

/// Truncate `path` to 0 bytes, which frees its clusters.
pub(super) fn discard(path: &CStr) {
    if let Ok(fd) = open(path, sys::O_TRUNC | sys::O_WRONLY) {
        close(fd);
    }
}

/// `open(path, O_CREAT|O_TRUNC|O_RDWR)` and write `data` to it.
pub(super) fn put_file(path: &CStr, data: &[u8]) -> Result<(), &'static str> {
    let fd = open(path, sys::O_CREAT | sys::O_TRUNC | sys::O_RDWR).map_err(|_| "create")?;
    let n = sys::write(fd, data.as_ptr(), data.len());
    close(fd);
    if n == Ok(data.len()) {
        Ok(())
    } else {
        Err("write")
    }
}

/// `read(fd, <16 bytes on the stack>, n)`, `n` at most 16.
fn read_into(fd: u32, n: usize) -> Result<usize, Errno> {
    let mut buf = [0u8; 16];
    // SAFETY: the kernel writes at most `n <= 16` bytes into `buf`, a local
    // no reference covers during the call; established here.
    unsafe { sys::read(fd, buf.as_mut_ptr(), n.min(buf.len())) }
}

/// `fstat(fd, <a stack buffer>)`: the call's result and `st_mode`.
fn fstat_mode(fd: u32) -> (Result<usize, Errno>, u32) {
    let mut st = [0u32; 36];
    // SAFETY: the kernel writes 144 bytes into `st`, a local of 144 bytes
    // no reference covers during the call; established here.
    let r = unsafe { sys::fstat(fd, st.as_mut_ptr().cast()) };
    (r, st[6])
}

fn fstat_into(fd: u32) -> Result<usize, Errno> {
    fstat_mode(fd).0
}

/// `getdents64(fd, <512 bytes on the stack>, count)`, `count` at most 512.
fn dents_into(fd: u32, count: u32) -> Result<usize, Errno> {
    let mut buf = [0u64; 64];
    // SAFETY: the kernel writes at most `count <= 512` bytes into `buf`, a
    // local of 512 bytes no reference covers; established here.
    unsafe { sys::getdents64(fd, buf.as_mut_ptr().cast(), count.min(512)) }
}

/// `nanosleep` for `sec` s and `nsec` ns, with a NULL `rmtp`.
fn sleep_ts(sec: i64, nsec: i64) -> Result<usize, Errno> {
    let ts = [sec, nsec];
    sys::nanosleep(ts.as_ptr().cast(), core::ptr::null_mut())
}

/// Sleep about `ms` milliseconds.
pub(super) fn sleep_ms(ms: i64) {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a sleep cut short only makes the caller poll sooner (DESIGN §2.5)"
    )]
    let _ = sleep_ts(ms / 1000, (ms % 1000) * 1_000_000);
}

/// A raw `mmap` with no fd.
fn mmap_raw(addr: u64, len: u64, prot: u64, flags: u64, off: u64) -> Result<usize, Errno> {
    // SAFETY: a new mapping either fails or lands where nothing the
    // program uses is mapped (the kernel picks the address, or `MAP_FIXED`
    // names page 0, which it refuses); established here.
    unsafe { sys::mmap(addr, len, prot, flags, u64::MAX, off) }
}

/// A file mapping (no `MAP_ANONYMOUS`) of `fd`.
fn mmap_fd(fd: u64) -> Result<usize, Errno> {
    // SAFETY: a file mapping fails before anything is mapped (ROADMAP
    // §12.4 adds them), and would land where the kernel picks; established
    // here.
    unsafe { sys::mmap(0, PAGE, PROT_READ, MAP_PRIVATE, fd, 0) }
}

/// A fresh private anonymous mapping of `len` bytes with `prot`.
pub(super) fn map(len: u64, prot: u64) -> Result<u64, &'static str> {
    mmap_raw(0, len, prot, ANON, 0)
        .map(|a| a as u64)
        .map_err(|_| "mmap")
}

/// A private anonymous mapping of `len` bytes at exactly `addr`.
pub(super) fn map_at(addr: u64, len: u64, prot: u64) -> Result<u64, &'static str> {
    // SAFETY: `MAP_FIXED` never replaces a mapping here (SYSCALL.md §3.1:
    // it fails with `EEXIST` over one), so it maps only free memory;
    // established here.
    let r = unsafe { sys::mmap(addr, len, prot, ANON | MAP_FIXED, u64::MAX, 0) };
    r.map(|a| a as u64).map_err(|_| "mmap MAP_FIXED")
}

/// Unmap `[addr, addr + len)`, which the caller mapped and no longer uses.
pub(super) fn unmap(addr: u64, len: u64) {
    // SAFETY: the callers pass only a range they mapped themselves and no
    // longer reference; established here by each caller's own mapping.
    let r = unsafe { sys::munmap(addr, len as usize) };
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5: a range left mapped is freed at exit"
    )]
    let _ = r;
}

/// A page that was mapped and is not any more.
pub(super) fn unmapped_page() -> Result<u64, &'static str> {
    let a = map(PAGE, PROT_READ | PROT_WRITE)?;
    unmap(a, PAGE);
    Ok(a)
}

/// The raw call `nr` with `args` in its registers.
///
/// # Safety
///
/// The kernel writes through any pointer argument the call writes through:
/// no live Rust reference may cover that memory.
pub(super) unsafe fn raw(nr: usize, args: [u64; 6]) -> Result<usize, Errno> {
    let [a, b, c, d, e, f] = args.map(|v| v as usize);
    // SAFETY: the kernel's `syscall` convention, and this fn's `# Safety`
    // contract for what the call writes, established here by its caller.
    sys::result(unsafe { sys::syscall6(nr, a, b, c, d, e, f) })
}

/// Fork a child that runs `f` and exits `100 + errno` when it fails, `0`
/// when it returns `Ok`; what the child got, rebuilt from its status. A
/// child that `execve`s `/hello` exits 42: `Ok(42)`.
pub(super) fn child_result(
    f: impl FnOnce() -> Result<usize, Errno>,
) -> Result<Result<usize, Errno>, &'static str> {
    let pid = utest::fork_child(|| match f() {
        Ok(_) => 0,
        Err(Errno(e)) => 100 + e,
    })
    .map_err(|_| "fork")?;
    let st = utest::wait_status(pid).map_err(|_| "wait4")?;
    match utest::exited(st) {
        Some(c) if c >= 100 => Ok(Err(Errno(i32::from(c) - 100))),
        Some(c) => Ok(Ok(usize::from(c))),
        None => Err("the child was killed"),
    }
}

/// `execve(path, [path], NULL)` in a child.
fn exec_errno(path: *const u8) -> Result<Result<usize, Errno>, &'static str> {
    child_result(|| {
        let argv = [path, core::ptr::null()];
        sys::execve(path, argv.as_ptr(), core::ptr::null())
    })
}

/// Reap every child that has exited, without waiting: how many.
pub(super) fn reap_exited() -> u32 {
    let mut n = 0;
    loop {
        // SAFETY: a null status and rusage, so the kernel writes nothing;
        // established here.
        let r = unsafe { sys::wait4(-1, core::ptr::null_mut(), WNOHANG, core::ptr::null_mut()) };
        if !matches!(r, Ok(p) if p != 0) {
            break;
        }
        n += 1;
    }
    n
}

/// `psinfo`'s text: the length written into `buf`.
pub(super) fn psinfo(buf: &mut [u8; 512]) -> Result<usize, Errno> {
    // SAFETY: the kernel writes at most 512 bytes into `buf`, which the
    // `&mut` borrow lends to this call alone; established here.
    unsafe { sys::psinfo(buf.as_mut_ptr(), buf.len()) }
}

/// The state field (`run`, `stop`, `zombie`) of `pid`'s `psinfo` line, if
/// it has one; `line` gets its `<ppid>` too.
pub(super) fn ps_state(pid: usize) -> Option<(usize, [u8; 8])> {
    let mut buf = [0u8; 512];
    let n = psinfo(&mut buf).ok()?;
    let text = buf.get(..n)?;
    for line in text.split(|&b| b == b'\n') {
        let mut f = line.split(|&b| b == b' ');
        let (Some(p), Some(pp), Some(st)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if parse_dec(p) != Some(pid) {
            continue;
        }
        let mut state = [0u8; 8];
        let k = st.len().min(state.len());
        state[..k].copy_from_slice(&st[..k]);
        return Some((parse_dec(pp)?, state));
    }
    None
}

/// `s` as a decimal number.
pub(super) fn parse_dec(s: &[u8]) -> Option<usize> {
    vibeos_user::cmd::parse_dec(s).map(|v| v as usize)
}

/// Whether `state` (from [`ps_state`]) is `word`.
pub(super) fn state_is(state: &[u8; 8], word: &[u8]) -> bool {
    state.get(..word.len()) == Some(word) && state.get(word.len()).is_none_or(|&b| b == 0)
}

/// Yield until `pred` holds, at most `tries` times.
pub(super) fn poll(tries: u32, mut pred: impl FnMut() -> bool) -> bool {
    for _ in 0..tries {
        if pred() {
            return true;
        }
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a yield has no failure a poll can act on (DESIGN §2.5)"
        )]
        let _ = sys::sched_yield();
    }
    pred()
}

fn ok_is(r: Result<usize, Errno>, want: usize) -> Result<(), &'static str> {
    if r == Ok(want) {
        Ok(())
    } else {
        Err("unexpected result")
    }
}

fn ok_nonzero(r: Result<usize, Errno>) -> Result<(), &'static str> {
    match r {
        Ok(0) => Err("returned 0"),
        Ok(_) => Ok(()),
        Err(_) => Err("failed"),
    }
}

// ---------------------------------------------------------- the probes

fn read_efault() -> Result<Result<usize, Errno>, &'static str> {
    let fd = open(HELLO, sys::O_RDONLY).map_err(|_| "open /hello")?;
    // SAFETY: the kernel-half destination is no memory of this process; established here.
    let r = unsafe { sys::read(fd, KERNEL_PTR as *mut u8, 16) };
    close(fd);
    Ok(r)
}

fn read_eisdir() -> Result<Result<usize, Errno>, &'static str> {
    let fd = open(c"/", sys::O_RDONLY).map_err(|_| "open /")?;
    let r = read_into(fd, 16);
    close(fd);
    Ok(r)
}

/// A `/proc` attribute opened for writing.
fn write_einval() -> Result<Result<usize, Errno>, &'static str> {
    let fd = open(c"/proc/1/status", sys::O_WRONLY).map_err(|_| "open /proc/1/status")?;
    let r = sys::write(fd, b"x".as_ptr(), 1);
    close(fd);
    Ok(r)
}

/// One byte written to the scratch file at `off`.
fn write_at(off: i64) -> Result<Result<usize, Errno>, &'static str> {
    let fd = open(SCRATCH, sys::O_CREAT | sys::O_TRUNC | sys::O_RDWR).map_err(|_| "create")?;
    let r = match sys::lseek(fd, off, SEEK_SET) {
        Ok(_) => Ok(sys::write(fd, b"x".as_ptr(), 1)),
        Err(_) => Err("lseek"),
    };
    close(fd);
    discard(SCRATCH);
    r
}

/// A FAT write at 4 GiB, FAT's file-size limit.
fn write_efbig() -> Result<Result<usize, Errno>, &'static str> {
    write_at(1 << 32)
}

/// A FAT write at 4 GiB less 64 KiB: more clusters than the volume has
/// free, refused before any is allocated.
fn write_enospc() -> Result<Result<usize, Errno>, &'static str> {
    write_at(0xFFFF_0000)
}

/// A path of 300 bytes.
fn open_enametoolong() -> Result<Result<usize, Errno>, &'static str> {
    let mut p = [b'a'; 301];
    p[0] = b'/';
    p[300] = 0;
    Ok(sys::open(p.as_ptr(), sys::O_RDONLY, 0))
}

fn execve_enametoolong() -> Result<Result<usize, Errno>, &'static str> {
    let mut p = [b'a'; 301];
    p[0] = b'/';
    p[300] = 0;
    exec_errno(p.as_ptr())
}

/// A new name in `/dev`.
fn open_eacces() -> Result<Result<usize, Errno>, &'static str> {
    Ok(open_raw(c"/dev/utest_new", sys::O_CREAT | sys::O_WRONLY))
}

/// Open `/hello` until a call fails: that call's error, with every
/// descriptor it opened closed.
fn open_until_error(max: usize) -> Result<usize, Errno> {
    let mut fds = [0u32; 300];
    let mut n = 0;
    let mut last = Ok(0);
    while n < max.min(fds.len()) {
        match open(HELLO, sys::O_RDONLY) {
            Ok(fd) => {
                fds[n] = fd;
                n += 1;
            }
            Err(e) => {
                last = Err(e);
                break;
            }
        }
    }
    for &fd in &fds[..n] {
        close(fd);
    }
    last
}

fn open_emfile() -> Result<Result<usize, Errno>, &'static str> {
    Ok(open_until_error(300))
}

/// Run `f` while four stopped children hold as many open files as the
/// system table lets them, each up to its own descriptor limit; then kill
/// and reap them.
fn with_files_full<T>(f: impl FnOnce() -> T) -> Result<T, &'static str> {
    let mut kids = [0usize; 4];
    let mut made = 0;
    let mut why = None;
    for k in kids.iter_mut() {
        let child = utest::fork_child(|| {
            while open(HELLO, sys::O_RDONLY).is_ok() {}
            #[expect(
                clippy::let_underscore_must_use,
                reason = "the parent sees the stop in psinfo, or times out (DESIGN §2.5)"
            )]
            let _ = sys::kill(sys::getpid().unwrap_or(0) as i32, SIGSTOP);
            loop {
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "the parent kills it (DESIGN §2.5)"
                )]
                let _ = sys::sched_yield();
            }
        });
        match child {
            Ok(pid) => {
                *k = pid;
                made += 1;
            }
            Err(_) => {
                why = Some("fork");
                break;
            }
        }
        if !poll(20_000, || {
            ps_state(*k).is_some_and(|(_, s)| state_is(&s, b"stop"))
        }) {
            why = Some("a child holding files did not stop");
            break;
        }
    }
    let r = if why.is_none() { Some(f()) } else { None };
    for &k in &kids[..made] {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a child already gone is reaped below all the same (DESIGN §2.5)"
        )]
        let _ = sys::kill(k as i32, SIGKILL);
        if utest::wait_status(k).is_err() {
            why = why.or(Some("reap a child holding files"));
        }
    }
    match (r, why) {
        (Some(r), None) => Ok(r),
        (_, Some(why)) => Err(why),
        (None, None) => Err("no result"),
    }
}

/// `open` with the system's open-file table full.
fn open_enfile() -> Result<Result<usize, Errno>, &'static str> {
    with_files_full(|| open_until_error(250))
}

/// `execve` with the system's open-file table full: this process fills
/// the rest of it first, and a child (sharing the files) runs `execve`.
fn execve_enfile() -> Result<Result<usize, Errno>, &'static str> {
    with_files_full(|| {
        let mut fds = [0u32; 250];
        let mut n = 0;
        while n < fds.len() {
            match open(HELLO, sys::O_RDONLY) {
                Ok(fd) => {
                    fds[n] = fd;
                    n += 1;
                }
                Err(_) => break,
            }
        }
        let r = exec_errno(HELLO.as_ptr().cast());
        for &fd in &fds[..n] {
            close(fd);
        }
        r
    })?
}

/// Fill the root FAT volume through one file, then create names in its
/// root directory until one needs a cluster; then free the clusters. Only
/// under init: a kernel-parented run (`kernel_tests`' `user_syscalls`)
/// shares the initrd volume with the in-guest tests that run after it,
/// whose own files need the root directory's room.
fn open_enospc() -> Result<Result<usize, Errno>, &'static str> {
    if sys::getppid() != Ok(1) {
        return Err(NOT_HERE);
    }
    static FILL: [u8; 4096] = [0x55; 4096];
    let fill = open(c"/utest_fill", sys::O_CREAT | sys::O_TRUNC | sys::O_RDWR)
        .map_err(|_| "create /utest_fill")?;
    let mut full = false;
    for _ in 0..16_384 {
        match sys::write(fill, FILL.as_ptr(), FILL.len()) {
            Ok(_) => {}
            Err(e) => {
                full = e == Errno::ENOSPC;
                break;
            }
        }
    }
    close(fill);
    let mut r = Err("the volume did not fill");
    if full {
        r = Err("200 names fit in the root directory");
        let mut name = *b"/utest_ns000\0";
        for i in 0..200u32 {
            name[9] = b'0' + (i / 100) as u8;
            name[10] = b'0' + (i / 10 % 10) as u8;
            name[11] = b'0' + (i % 10) as u8;
            match sys::open(name.as_ptr(), sys::O_CREAT | sys::O_RDWR, 0o644) {
                Ok(fd) => close(fd as u32),
                Err(e) => {
                    r = Ok(Err(e));
                    break;
                }
            }
        }
    }
    match open(c"/utest_fill", sys::O_TRUNC | sys::O_RDWR) {
        Ok(fd) => close(fd),
        Err(_) => return Err("truncate /utest_fill"),
    }
    r
}

fn fstat_efault() -> Result<Result<usize, Errno>, &'static str> {
    // SAFETY: the kernel-half destination is no memory of this process; established here.
    Ok(unsafe { sys::fstat(1, KERNEL_PTR as *mut core::ffi::c_void) })
}

/// A `whence` of 9.
fn lseek_einval() -> Result<Result<usize, Errno>, &'static str> {
    let fd = open(HELLO, sys::O_RDONLY).map_err(|_| "open /hello")?;
    let r = sys::lseek(fd, 0, 9);
    close(fd);
    Ok(r)
}

/// `MAP_FIXED_NOREPLACE` over a mapping.
fn mmap_eexist() -> Result<Result<usize, Errno>, &'static str> {
    let a = map(PAGE, PROT_READ)?;
    let r = mmap_raw(a, PAGE, PROT_READ, ANON | MAP_FIXED_NOREPLACE, 0);
    unmap(a, PAGE);
    Ok(r)
}

fn munmap_einval() -> Result<Result<usize, Errno>, &'static str> {
    // SAFETY: an unaligned address fails before anything is unmapped; established here.
    Ok(unsafe { sys::munmap(0x1001, PAGE as usize) })
}

/// A split of a three-page mapping with the region table full.
fn munmap_enomem() -> Result<Result<usize, Errno>, &'static str> {
    let base = map(3 * PAGE, PROT_READ | PROT_WRITE)?;
    let mut fills = [0u64; 300];
    let mut n = 0;
    let mut full = false;
    while n < fills.len() {
        match mmap_raw(0, PAGE, PROT_NONE, ANON, 0) {
            Ok(a) => {
                fills[n] = a as u64;
                n += 1;
            }
            Err(e) => {
                full = e == Errno::ENOMEM;
                break;
            }
        }
    }
    // SAFETY: the middle page of `base`, which this probe mapped and
    // nothing references; established here.
    let r = unsafe { sys::munmap(base + PAGE, PAGE as usize) };
    for &a in &fills[..n] {
        unmap(a, PAGE);
    }
    unmap(base, 3 * PAGE);
    if full {
        Ok(r)
    } else {
        Err("the region table did not fill")
    }
}

fn dup_emfile() -> Result<Result<usize, Errno>, &'static str> {
    let mut fds = [0u32; 300];
    let mut n = 0;
    let mut last = Ok(0);
    while n < fds.len() {
        match sys::dup(1) {
            Ok(fd) => {
                fds[n] = fd as u32;
                n += 1;
            }
            Err(e) => {
                last = Err(e);
                break;
            }
        }
    }
    for &fd in &fds[..n] {
        close(fd);
    }
    Ok(last)
}

fn nanosleep_efault() -> Result<Result<usize, Errno>, &'static str> {
    Ok(sys::nanosleep(
        KERNEL_PTR as *const core::ffi::c_void,
        core::ptr::null_mut(),
    ))
}

/// A script: no ELF magic.
fn execve_enoexec() -> Result<Result<usize, Errno>, &'static str> {
    put_file(SCRATCH, b"#!/bin/sh\nexit 0\n")?;
    let r = exec_errno(SCRATCH.as_ptr().cast());
    discard(SCRATCH);
    r
}

/// An ELF whose one `PT_LOAD` asks for 1 GiB and a page, over
/// `limits::EXEC_IMAGE_MAX`.
fn execve_enomem() -> Result<Result<usize, Errno>, &'static str> {
    put_file(SCRATCH, &big_elf())?;
    let r = exec_errno(SCRATCH.as_ptr().cast());
    discard(SCRATCH);
    r
}

/// A 64-bit little-endian ELF header and one `PT_LOAD` (the System V
/// gABI), for this architecture: `filesz` 0, `memsz` 1 GiB + 4 KiB at
/// 4 MiB.
fn big_elf() -> [u8; 120] {
    let mut e = [0u8; 120];
    let mut put = |at: usize, bytes: &[u8]| e[at..at + bytes.len()].copy_from_slice(bytes);
    put(0, b"\x7fELF\x02\x01\x01");
    put(16, &2u16.to_le_bytes()); // ET_EXEC
    put(18, &sys::ELF_MACHINE.to_le_bytes()); // e_machine
    put(20, &1u32.to_le_bytes()); // EV_CURRENT
    put(24, &0x40_0000u64.to_le_bytes()); // e_entry
    put(32, &64u64.to_le_bytes()); // e_phoff
    put(52, &64u16.to_le_bytes()); // e_ehsize
    put(54, &56u16.to_le_bytes()); // e_phentsize
    put(56, &1u16.to_le_bytes()); // e_phnum
    put(64, &1u32.to_le_bytes()); // PT_LOAD
    put(68, &5u32.to_le_bytes()); // PF_R | PF_X
    put(80, &0x40_0000u64.to_le_bytes()); // p_vaddr
    put(88, &0x40_0000u64.to_le_bytes()); // p_paddr
    put(104, &((1u64 << 30) + PAGE).to_le_bytes()); // p_memsz
    put(112, &PAGE.to_le_bytes()); // p_align
    e
}

/// No child: `wait4(-1, NULL, WNOHANG)`.
fn wait4_echild() -> Result<Result<usize, Errno>, &'static str> {
    if reap_exited() != 0 {
        return Err("an exited child was left over");
    }
    // SAFETY: a null status and rusage, so the kernel writes nothing; established here.
    Ok(unsafe { sys::wait4(-1, core::ptr::null_mut(), WNOHANG, core::ptr::null_mut()) })
}

/// A zombie child reaped into a kernel-half status: `EFAULT`, and the
/// child is gone.
fn wait4_efault() -> Result<Result<usize, Errno>, &'static str> {
    let pid = utest::fork_child(|| 0).map_err(|_| "fork")?;
    if !poll(20_000, || utest::zombie(pid)) {
        return Err("the child did not exit");
    }
    // SAFETY: the kernel-half status is no memory of this process; established here.
    let r = unsafe { sys::wait4(pid as i32, KERNEL_PTR as *mut i32, 0, core::ptr::null_mut()) };
    // SAFETY: a null status and rusage, so the kernel writes nothing; established here.
    let again = unsafe { sys::wait4(pid as i32, core::ptr::null_mut(), 0, core::ptr::null_mut()) };
    if again != Err(Errno::ECHILD) {
        return Err("the child was not reaped before the status copy");
    }
    Ok(r)
}

fn kill_einval() -> Result<Result<usize, Errno>, &'static str> {
    let me = sys::getpid().map_err(|_| "getpid")?;
    Ok(sys::kill(me as i32, 0))
}

/// `RESTART2` with a kernel-half string: refused before any restart.
fn reboot_efault() -> Result<Result<usize, Errno>, &'static str> {
    Ok(sys::reboot(
        REBOOT_MAGIC1,
        REBOOT_MAGIC2,
        REBOOT_CMD_RESTART2,
        KERNEL_PTR as *mut core::ffi::c_void,
    ))
}

// From reboot(2).
/// `LINUX_REBOOT_MAGIC1`.
pub(super) const REBOOT_MAGIC1: i32 = 0xfee1_dead_u32 as i32;
/// `LINUX_REBOOT_MAGIC2`.
pub(super) const REBOOT_MAGIC2: i32 = 0x2812_1969;
/// `LINUX_REBOOT_CMD_RESTART2`.
pub(super) const REBOOT_CMD_RESTART2: u32 = 0xa1b2_c3d4;

/// `/dev/console` opened by path.
fn getdents64_espipe() -> Result<Result<usize, Errno>, &'static str> {
    let fd = open(c"/dev/console", sys::O_RDONLY).map_err(|_| "open /dev/console")?;
    let r = dents_into(fd, 512);
    close(fd);
    Ok(r)
}

/// A `count` too small for `.`.
fn getdents64_einval() -> Result<Result<usize, Errno>, &'static str> {
    let fd = open(c"/", sys::O_RDONLY).map_err(|_| "open /")?;
    let r = dents_into(fd, 10);
    close(fd);
    Ok(r)
}

fn getdents64_efault() -> Result<Result<usize, Errno>, &'static str> {
    let fd = open(c"/", sys::O_RDONLY).map_err(|_| "open /")?;
    // SAFETY: the kernel-half destination is no memory of this process; established here.
    let r = unsafe { sys::getdents64(fd, KERNEL_PTR as *mut core::ffi::c_void, 512) };
    close(fd);
    Ok(r)
}

fn psinfo_efault() -> Result<Result<usize, Errno>, &'static str> {
    // SAFETY: the kernel-half destination is no memory of this process; established here.
    Ok(unsafe { sys::psinfo(KERNEL_PTR as *mut u8, 64) })
}

// --------------------------------------------------- the success probes

/// `/hello`'s first 16 bytes: the ELF magic.
fn read_ok() -> Result<(), &'static str> {
    let fd = open(HELLO, sys::O_RDONLY).map_err(|_| "open /hello")?;
    let mut buf = [0u8; 16];
    // SAFETY: the kernel writes at most 16 bytes into `buf`, a local no
    // reference covers during the call; established here.
    let r = unsafe { sys::read(fd, buf.as_mut_ptr(), buf.len()) };
    close(fd);
    match r {
        Ok(16) if buf.starts_with(b"\x7fELF") => Ok(()),
        _ => Err("read of /hello"),
    }
}

/// Five bytes to the scratch file, and back.
fn write_ok() -> Result<(), &'static str> {
    put_file(SCRATCH, b"hello")?;
    let fd = open(SCRATCH, sys::O_RDONLY).map_err(|_| "reopen")?;
    let r = read_into(fd, 16);
    close(fd);
    discard(SCRATCH);
    if r == Ok(5) { Ok(()) } else { Err("read back") }
}

/// Open and close `/hello`.
fn open_ok() -> Result<(), &'static str> {
    let fd = open(HELLO, sys::O_RDONLY).map_err(|_| "open /hello")?;
    if fd < 3 {
        return Err("a descriptor below 3");
    }
    sys::close(fd).map(|_| ()).map_err(|_| "close")
}

/// The console is a character device.
fn fstat_ok() -> Result<(), &'static str> {
    const S_IFMT: u32 = 0o170_000;
    const S_IFCHR: u32 = 0o020_000;
    match fstat_mode(1) {
        (Ok(0), mode) if mode & S_IFMT == S_IFCHR => Ok(()),
        _ => Err("fstat of fd 1"),
    }
}

/// `SEEK_END` of `/hello` is its size.
fn lseek_ok() -> Result<(), &'static str> {
    let fd = open(HELLO, sys::O_RDONLY).map_err(|_| "open /hello")?;
    let r = sys::lseek(fd, 0, SEEK_END);
    close(fd);
    match r {
        Ok(n) if n > 64 => Ok(()),
        _ => Err("lseek SEEK_END"),
    }
}

/// Map a page, write it, unmap it.
fn mmap_ok() -> Result<(), &'static str> {
    let a = map(PAGE, PROT_READ | PROT_WRITE)?;
    let p = core::ptr::with_exposed_provenance_mut::<u8>(a as usize);
    // SAFETY: `p` is the first byte of the page this fn just mapped
    // writable, which nothing else references; established here.
    unsafe { p.write_volatile(7) };
    // SAFETY: the same page, as above; established here.
    let v = unsafe { p.read_volatile() };
    // SAFETY: the page this fn mapped, with no reference to it left; established here.
    let r = unsafe { sys::munmap(a, PAGE as usize) };
    if v == 7 && r == Ok(0) {
        Ok(())
    } else {
        Err("map, write, unmap")
    }
}

/// `brk(0)` is the break, above the image.
fn brk_ok() -> Result<(), &'static str> {
    // SAFETY: `brk(0)` only reads the break; established here.
    match unsafe { sys::brk(0) } {
        Ok(b) if b > 0x4000_0000 => Ok(()),
        _ => Err("brk(0)"),
    }
}

fn dup_ok() -> Result<(), &'static str> {
    let fd = sys::dup(1).map_err(|_| "dup(1)")?;
    sys::close(fd as u32).map(|_| ()).map_err(|_| "close")
}

fn dup2_ok() -> Result<(), &'static str> {
    if sys::dup2(1, 50) != Ok(50) {
        return Err("dup2(1, 50)");
    }
    sys::close(50).map(|_| ()).map_err(|_| "close")
}

/// A child that exits 7 reports it.
fn fork_ok() -> Result<(), &'static str> {
    let pid = utest::fork_child(|| 7).map_err(|_| "fork")?;
    match utest::wait_status(pid).map(utest::exited) {
        Ok(Some(7)) => Ok(()),
        _ => Err("child status"),
    }
}

/// A child runs `/hello`.
fn execve_ok() -> Result<(), &'static str> {
    match exec_errno(HELLO.as_ptr().cast())? {
        Ok(c) if c == usize::from(HELLO_EXIT) => Ok(()),
        _ => Err("execve /hello"),
    }
}

/// `SIGCONT` to itself, which changes nothing.
fn kill_ok() -> Result<(), &'static str> {
    let me = sys::getpid().map_err(|_| "getpid")?;
    ok_is(sys::kill(me as i32, utest::SIGCONT), 0)
}

fn getdents64_ok() -> Result<(), &'static str> {
    let fd = open(c"/", sys::O_RDONLY).map_err(|_| "open /")?;
    let r = dents_into(fd, 512);
    close(fd);
    match r {
        Ok(n) if n >= 48 => Ok(()),
        _ => Err("getdents64 of /"),
    }
}

/// This process's own line is there.
fn psinfo_ok() -> Result<(), &'static str> {
    let me = sys::getpid().map_err(|_| "getpid")?;
    match ps_state(me) {
        Some((_, s)) if state_is(&s, b"run") => Ok(()),
        _ => Err("no run line for this process"),
    }
}
