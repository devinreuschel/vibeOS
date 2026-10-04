//! Helpers the `/bin` utilities, `/sbin/init` and `/bin/sh` share (ROADMAP
//! §10.5): buffered output with decimal numbers, decimal parsing, opening an
//! operand, emptying a scratch file, the `<prog>: <what>: errno <n>` line,
//! buffered byte and line reads, and the `wait4` status word.
//!
//! Every read retries `EINTR` and every loop reads until `read` returns 0,
//! since a file `read` returns at most 256 bytes a call (SYSCALL.md §3.1).

extern crate alloc;

use alloc::vec::Vec;

use crate::env::Env;
use crate::io::write_all;
use crate::sys::{self, Errno, Stat};

/// Whether `e` is Linux's `EINTR`, 4 (`include/uapi/asm-generic/
/// errno-base.h`), which the helpers retry. No call returns it before
/// ROADMAP §13.8 gives signals handlers, so the `KError` table has no row
/// for it yet, and the generated `Errno` constants no name.
pub fn interrupted(e: Errno) -> bool {
    e.0 == 4
}

/// The longest path `open_flags` takes, as the `open` row allows.
const PATH_MAX: usize = 255;

/// How many bytes [`Out`] holds before it writes them.
pub const OUT_MAX: usize = 512;

/// Buffered output to one fd: [`Out::flush`] writes what [`Out::put`] and
/// [`Out::dec`] gathered in one `write`, so a short line stays whole on the
/// console. A full buffer is written before more is taken.
pub struct Out {
    fd: i32,
    len: usize,
    buf: [u8; OUT_MAX],
}

impl Out {
    /// An empty buffer for `fd`.
    pub const fn new(fd: i32) -> Out {
        Out {
            fd,
            len: 0,
            buf: [0; OUT_MAX],
        }
    }

    /// Append `bytes`.
    pub fn put(&mut self, mut bytes: &[u8]) -> Result<&mut Out, Errno> {
        while !bytes.is_empty() {
            if self.len == OUT_MAX {
                self.flush()?;
            }
            let n = bytes.len().min(OUT_MAX - self.len);
            let (head, rest) = bytes.split_at(n);
            if let Some(dst) = self.buf.get_mut(self.len..self.len + n) {
                dst.copy_from_slice(head);
            }
            self.len += n;
            bytes = rest;
        }
        Ok(self)
    }

    /// Append `v` in decimal.
    pub fn dec(&mut self, v: u64) -> Result<&mut Out, Errno> {
        let mut digits = [0u8; 20];
        let mut i = digits.len();
        let mut v = v;
        loop {
            i -= 1;
            if let Some(d) = digits.get_mut(i) {
                *d = b'0' + (v % 10) as u8;
            }
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.put(digits.get(i..).unwrap_or(&[]))
    }

    /// Write what the buffer holds and empty it.
    pub fn flush(&mut self) -> Result<(), Errno> {
        let r = write_all(self.fd, self.buf.get(..self.len).unwrap_or(&[]));
        self.len = 0;
        r
    }
}

/// `s` as a decimal number: one or more ASCII digits, nothing else, and no
/// more than `u64` holds.
pub fn parse_dec(s: &[u8]) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    s.iter().try_fold(0u64, |acc, &b| {
        let d = b.checked_sub(b'0').filter(|d| *d < 10)?;
        acc.checked_mul(10)?.checked_add(u64::from(d))
    })
}

/// The leading flag arguments, each `-` and letters from `set`: which
/// letters were given, and the index of the first operand. A lone `-` is an
/// operand; an unknown letter is `Err` with its argument.
pub fn flags<const N: usize>(
    env: &Env,
    set: &[u8; N],
) -> Result<([bool; N], usize), &'static [u8]> {
    let mut on = [false; N];
    let mut first = 1;
    while let Some(arg) = env
        .arg(first)
        .filter(|a| a.len() > 1 && a.starts_with(b"-"))
    {
        for f in arg.iter().skip(1) {
            let i = set.iter().position(|c| c == f).ok_or(arg)?;
            if let Some(o) = on.get_mut(i) {
                *o = true;
            }
        }
        first += 1;
    }
    Ok((on, first))
}

/// The operands from argument `first` on, or `default` alone when there are
/// none.
pub fn operands<'a>(
    env: &'a Env,
    first: usize,
    default: &'static [&'static [u8]; 1],
) -> impl Iterator<Item = &'static [u8]> + 'a {
    let none: &'static [&'static [u8]] = if env.argc() > first { &[] } else { default };
    env.args().skip(first).chain(none.iter().copied())
}

/// Open `path` with `flags` (mode 0644): `ENAMETOOLONG` past 255 bytes, and
/// `ENOENT` for an empty path or one with a NUL inside, as Linux's lookup
/// ends at the NUL.
pub fn open_flags(path: &[u8], flags: i32) -> Result<u32, Errno> {
    let mut buf = [0u8; PATH_MAX + 1];
    let dst = buf.get_mut(..path.len()).ok_or(Errno::ENAMETOOLONG)?;
    if path.is_empty() || path.contains(&0) {
        return Err(Errno::ENOENT);
    }
    dst.copy_from_slice(path);
    let fd = sys::open(buf.as_ptr(), flags, 0o644)?;
    u32::try_from(fd).map_err(|_| Errno::EBADF)
}

/// Truncate `path` to 0 bytes, which gives back the pages or clusters it
/// holds: how a program releases a scratch file while nothing has
/// `unlink`. A missing file holds nothing, so `ENOENT` is `Ok`.
pub fn discard(path: &[u8]) -> Result<(), Errno> {
    match open_flags(path, sys::O_WRONLY | sys::O_TRUNC) {
        Ok(fd) => sys::close(fd).map(drop),
        Err(Errno::ENOENT) => Ok(()),
        Err(e) => Err(e),
    }
}

/// [`discard`] each of `paths`, each one whatever the others did: the
/// first error, if any.
pub fn discard_all(paths: &[&[u8]]) -> Result<(), Errno> {
    let mut first = Ok(());
    for path in paths {
        let r = discard(path);
        if first.is_ok() {
            first = r;
        }
    }
    first
}

/// An input operand: `-` is fd 0; anything else is opened for reading.
pub fn open_arg(path: &[u8]) -> Result<u32, Errno> {
    if path == b"-" {
        Ok(0)
    } else {
        open_flags(path, sys::O_RDONLY)
    }
}

/// Close `fd` unless it is 0, 1 or 2. A failed close of a file only read
/// loses nothing, so the result is dropped.
pub fn close_arg(fd: u32) {
    if fd > 2 {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "DESIGN §2.5: closing a read-only fd loses no data, and the process exits next"
        )]
        let _ = sys::close(fd);
    }
}

/// Open the operand `path` ([`open_arg`]), run `f` on a [`Reader`] of it,
/// and close it.
pub fn with_reader<T>(
    path: &[u8],
    f: impl FnOnce(&mut Reader) -> Result<T, Errno>,
) -> Result<T, Errno> {
    let fd = open_arg(path)?;
    let r = f(&mut Reader::new(fd));
    close_arg(fd);
    r
}

/// Write `<prog>: <what>: errno <n>` to fd 2 in one `write`.
pub fn err(prog: &[u8], what: &[u8], e: Errno) {
    let mut o = Out::new(2);
    let line = (|| {
        o.put(prog)?.put(b": ")?.put(what)?.put(b": errno ")?;
        o.dec(u64::from(e.0.unsigned_abs()))?.put(b"\n")?;
        o.flush()
    })();
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5: fd 2 is where a failure is reported; the exit status still carries it"
    )]
    let _ = line;
}

/// [`err`], then `code`, the status the caller exits with.
pub fn fail(prog: &[u8], what: &[u8], e: Errno, code: i32) -> i32 {
    err(prog, what, e);
    code
}

/// One `read` of `fd` into `buf`, retrying `EINTR`; 0 is end of file.
pub fn read(fd: u32, buf: &mut [u8]) -> Result<usize, Errno> {
    loop {
        // SAFETY: `read` writes at most `buf.len()` bytes into `buf`, which
        // the `&mut` borrow holds alone; established here.
        match unsafe { sys::read(fd, buf.as_mut_ptr(), buf.len()) } {
            Err(e) if interrupted(e) => continue,
            r => return r.map(|n| n.min(buf.len())),
        }
    }
}

/// A buffered reader of one fd, a byte at a time.
pub struct Reader {
    fd: u32,
    pos: usize,
    len: usize,
    buf: [u8; 256],
}

impl Reader {
    /// A reader of `fd`, from its current offset.
    pub const fn new(fd: u32) -> Reader {
        Reader {
            fd,
            pos: 0,
            len: 0,
            buf: [0; 256],
        }
    }

    /// The next byte, or `None` at end of file.
    pub fn byte(&mut self) -> Result<Option<u8>, Errno> {
        if self.pos == self.len {
            self.len = read(self.fd, &mut self.buf)?;
            self.pos = 0;
        }
        let b = self.buf.get(self.pos).copied().filter(|_| self.len != 0);
        self.pos += usize::from(b.is_some());
        Ok(b)
    }

    /// Up to `buf.len()` bytes: what the buffer holds, else one `read`; 0
    /// at end of file.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        let held = self.buf.get(self.pos..self.len).unwrap_or(&[]);
        if held.is_empty() {
            return read(self.fd, buf);
        }
        let n = held.len().min(buf.len());
        if let (Some(dst), Some(src)) = (buf.get_mut(..n), held.get(..n)) {
            dst.copy_from_slice(src);
        }
        self.pos += n;
        Ok(n)
    }

    /// The next line, without its `\n`, into `line` (cleared first), which
    /// grows on the heap; `false` at end of file with nothing read. A failed
    /// allocation is `ENOMEM`.
    pub fn line(&mut self, line: &mut Vec<u8>) -> Result<bool, Errno> {
        line.clear();
        loop {
            match self.byte()? {
                None => return Ok(!line.is_empty()),
                Some(b'\n') => return Ok(true),
                Some(b) => {
                    line.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
                    line.push(b);
                }
            }
        }
    }
}

/// Whether `fd` is a directory (`fstat`'s `S_IFDIR`).
pub fn is_dir(fd: u32) -> Result<bool, Errno> {
    // `S_IFMT` and `S_IFDIR`, from Linux `include/uapi/linux/stat.h`.
    const S_IFMT: u32 = 0o170000;
    const S_IFDIR: u32 = 0o040000;
    let mut st = Stat::default();
    // SAFETY: `fstat` writes `size_of::<Stat>()` bytes, the size
    // `arch::stat` checks against the kernel's, into `st`, a local no
    // reference covers; established here.
    unsafe { sys::fstat(fd, (&raw mut st).cast()) }?;
    Ok(st.st_mode & S_IFMT == S_IFDIR)
}

/// Every name `getdents64` returns for the directory `fd`, `.` and `..`
/// included, in the order it returns them, each on the heap. A record
/// shorter than its header is `EIO`; a failed allocation is `ENOMEM`.
pub fn dir_names(fd: u32) -> Result<Vec<Vec<u8>>, Errno> {
    // `struct linux_dirent64`: `d_reclen` at 16, `d_name` at 19.
    const RECLEN: usize = 16;
    const NAME: usize = 19;
    let mut names = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        // SAFETY: `getdents64` writes at most `buf.len()` bytes into `buf`,
        // a local no reference covers; established here.
        let n = unsafe { sys::getdents64(fd, buf.as_mut_ptr().cast(), buf.len() as u32) }?;
        if n == 0 {
            return Ok(names);
        }
        let mut recs = buf.get(..n).unwrap_or(&[]);
        while let Some(len) = recs.get(RECLEN..RECLEN + 2) {
            let len = <[u8; 2]>::try_from(len).map_or(0, u16::from_ne_bytes);
            let (rec, rest) = recs
                .split_at_checked(usize::from(len))
                .filter(|(r, _)| r.len() > NAME)
                .ok_or(Errno::EIO)?;
            let name = rec.get(NAME..).unwrap_or(&[]);
            let name = name.split(|&b| b == 0).next().unwrap_or(&[]);
            let mut v = Vec::new();
            v.try_reserve_exact(name.len()).map_err(|_| Errno::ENOMEM)?;
            v.extend_from_slice(name);
            names.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
            names.push(v);
            recs = rest;
        }
    }
}

/// How a child ended, from its `wait4` status word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// It exited with this code (`WEXITSTATUS`).
    Exited(u8),
    /// A signal ended it (`WTERMSIG`).
    Signaled(u8),
}

impl Status {
    /// Decode a `wait4` status word.
    pub fn of(word: u32) -> Status {
        match sys::exit_code(word) {
            Some(code) => Status::Exited(code),
            None => Status::Signaled((word & 0x7f) as u8),
        }
    }

    /// A shell's `$?`: the exit code, or 128 plus the signal.
    pub fn code(self) -> i32 {
        match self {
            Status::Exited(c) => i32::from(c),
            Status::Signaled(s) => 128 + i32::from(s),
        }
    }
}

/// `wait4(pid, &status, options)`, retrying `EINTR`: the pid it returned
/// (0 under `WNOHANG` while the child runs) and the status word.
pub fn wait(pid: i32, options: i32) -> Result<(usize, u32), Errno> {
    loop {
        let mut status = 0i32;
        // SAFETY: `wait4` writes 4 bytes through `&raw mut status`, a local
        // no reference covers, and nothing through the null rusage;
        // established here.
        match unsafe { sys::wait4(pid, &raw mut status, options, core::ptr::null_mut()) } {
            Err(e) if interrupted(e) => continue,
            r => return r.map(|p| (p, status as u32)),
        }
    }
}

/// The process's environment as the NULL-terminated vector `execve` takes.
/// Each string of [`Env::vars`] is NUL-terminated in the initial stack, so
/// its start is a C string.
pub fn envp(env: &Env) -> Result<Vec<*const u8>, Errno> {
    let mut v: Vec<*const u8> = Vec::new();
    for s in env.vars().map(|s| s.as_ptr()).chain([core::ptr::null()]) {
        v.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
        v.push(s);
    }
    Ok(v)
}
