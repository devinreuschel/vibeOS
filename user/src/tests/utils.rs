//! The `/bin` utilities and `/bin/sh` run from a script (ROADMAP §10.5).
//!
//! `utilities_table` runs each row's `argv` through [`run_redirected`], with
//! the row's input, if any, on fd 0 and fd 1 on a scratch file, then checks
//! the exit status and what the file holds; a failure names its row.
//! `sleep` and `yes` run on their own, since they are judged while they
//! run. `sh_script_status` runs `/bin/sh` on a script, fd 1 and fd 2 on
//! the scratch file. Scratch files are `/tmp/u75-*`. Every fork goes through `utest::fork` (F069).

use core::ffi::{CStr, c_void};

use vibeos_user::cmd::{self, Reader, Status};
use vibeos_user::io::write_all;
use vibeos_user::rt;
use vibeos_user::sys::{self, Errno};
use vibeos_user::utest::{self, Outcome, Runner};

// From Linux `include/uapi/linux/wait.h` and signal(7).
const WNOHANG: i32 = 1;
const SIGKILL: i32 = 9;

/// The 1 ms sleeps `yes_row` takes, at most, while it waits for `yes`'s
/// first line: about 2 s in all, each sleep rounded up to the tick.
const YES_POLLS: u32 = 2_000;

const IN: &[u8] = b"/tmp/u75-in";
const OUT: &[u8] = b"/tmp/u75-out";
const GREP: &[u8] = b"/tmp/u75-grep";
const WC: &[u8] = b"/tmp/u75-wc";
const CAT: &[u8] = b"/tmp/u75-cat";
const CMP: [(&[u8], &[u8]); 4] = [
    (b"/tmp/u75-a", b"abc\n"),
    (b"/tmp/u75-b", b"abd\n"),
    (b"/tmp/u75-p", b"ab"),
    (b"/tmp/u75-q", b"abc"),
];

/// 1000 bytes of lines, so `cat` sees several short reads.
const K1: [u8; 1000] = {
    let mut a = [0u8; 1000];
    let mut i = 0;
    while i < a.len() {
        a[i] = if i % 40 == 39 {
            b'\n'
        } else {
            b'a' + (i % 26) as u8
        };
        i += 1;
    }
    a
};

/// What a row's fd 1 must hold.
enum Want {
    /// Exactly these bytes.
    Is(&'static [u8]),
    /// Bytes this check accepts.
    Check(fn(&[u8]) -> bool),
}

/// One `utilities_table` row.
struct Row {
    /// The row, as a failure names it.
    what: &'static str,
    argv: &'static [&'static CStr],
    /// Bytes written to [`IN`], which goes on fd 0.
    stdin: Option<&'static [u8]>,
    out: Want,
    code: u8,
}

const fn row(
    what: &'static str,
    argv: &'static [&'static CStr],
    stdin: Option<&'static [u8]>,
    out: Want,
    code: u8,
) -> Row {
    Row {
        what,
        argv,
        stdin,
        out,
        code,
    }
}

const ROWS: &[Row] = &[
    row("true", &[c"/bin/true"], None, Want::Is(b""), 0),
    row("false", &[c"/bin/false", c"x"], None, Want::Is(b""), 1),
    row(
        "echo a b",
        &[c"/bin/echo", c"a", c"b"],
        None,
        Want::Is(b"a b\n"),
        0,
    ),
    row(
        "echo -n x",
        &[c"/bin/echo", c"-n", c"x"],
        None,
        Want::Is(b"x"),
        0,
    ),
    row(
        "cat file",
        &[c"/bin/cat", c"/tmp/u75-cat"],
        None,
        Want::Is(&K1),
        0,
    ),
    row("cat stdin", &[c"/bin/cat"], Some(&K1), Want::Is(&K1), 0),
    row(
        "grep foo",
        &[c"/bin/grep", c"foo", c"/tmp/u75-grep"],
        None,
        Want::Is(b"foo\nfood\n"),
        0,
    ),
    row(
        "grep zzz",
        &[c"/bin/grep", c"zzz", c"/tmp/u75-grep"],
        None,
        Want::Is(b""),
        1,
    ),
    row(
        "grep missing file",
        &[c"/bin/grep", c"foo", c"/tmp/u75-none"],
        None,
        Want::Is(b""),
        2,
    ),
    row(
        "grep bar stdin",
        &[c"/bin/grep", c"bar"],
        Some(b"foo\nbar\nfood\n"),
        Want::Is(b"bar\n"),
        0,
    ),
    row(
        "wc file",
        &[c"/bin/wc", c"/tmp/u75-wc"],
        None,
        Want::Is(b"2 3 6 /tmp/u75-wc\n"),
        0,
    ),
    row(
        "wc -l stdin",
        &[c"/bin/wc", c"-l"],
        Some(b"a b\nc\n"),
        Want::Is(b"2\n"),
        0,
    ),
    row(
        "cmp equal",
        &[c"/bin/cmp", c"/tmp/u75-a", c"/tmp/u75-a"],
        None,
        Want::Is(b""),
        0,
    ),
    row(
        "cmp abc abd",
        &[c"/bin/cmp", c"/tmp/u75-a", c"/tmp/u75-b"],
        None,
        Want::Is(b"/tmp/u75-a /tmp/u75-b differ: char 3, line 1\n"),
        1,
    ),
    row(
        "cmp prefix",
        &[c"/bin/cmp", c"/tmp/u75-p", c"/tmp/u75-q"],
        None,
        Want::Is(b""),
        1,
    ),
    row(
        "cmp missing file",
        &[c"/bin/cmp", c"/tmp/u75-a", c"/tmp/u75-none"],
        None,
        Want::Is(b""),
        2,
    ),
    row(
        "ls /bin",
        &[c"/bin/ls", c"/bin"],
        None,
        Want::Check(ls_bin),
        0,
    ),
    row(
        "ls /hello",
        &[c"/bin/ls", c"/hello"],
        None,
        Want::Is(b"/hello\n"),
        0,
    ),
    row(
        "ls /nonexistent",
        &[c"/bin/ls", c"/nonexistent"],
        None,
        Want::Is(b""),
        2,
    ),
    row("sleep x", &[c"/bin/sleep", c"x"], None, Want::Is(b""), 1),
];

pub fn run(t: &mut Runner) {
    t.case("utilities_table", utilities_table);
    t.case("sh_script_status", sh_script_status);
}

/// At most 8 C strings as the NULL-terminated vector `execve` takes.
fn cvec(v: &[&CStr]) -> Result<[*const u8; 9], Errno> {
    let mut out = [core::ptr::null(); 9];
    if v.len() >= out.len() {
        return Err(Errno::E2BIG);
    }
    for (o, s) in out.iter_mut().zip(v) {
        *o = s.as_ptr().cast();
    }
    Ok(out)
}

/// In the child: `stdin` on fd 0, `out` (created or truncated) on fd 1, and
/// on fd 2 too when `err_too`.
fn redirect(stdin: Option<&[u8]>, out: &[u8], err_too: bool) -> Result<(), Errno> {
    if let Some(path) = stdin {
        let fd = cmd::open_flags(path, sys::O_RDONLY)?;
        sys::dup2(fd, 0)?;
        sys::close(fd)?;
    }
    let fd = cmd::open_flags(out, sys::O_WRONLY | sys::O_CREAT | sys::O_TRUNC)?;
    sys::dup2(fd, 1)?;
    if err_too {
        sys::dup2(fd, 2)?;
    }
    sys::close(fd)?;
    Ok(())
}

/// Fork a child that redirects as [`redirect`] says and execs `argv[0]`
/// with `argv` and `envp`; it exits 125 when a redirection fails and 127
/// when `execve` returns. The child's pid.
fn spawn(
    argv: &[&CStr],
    envp: &[&CStr],
    stdin: Option<&[u8]>,
    out: &[u8],
    err_too: bool,
) -> Result<i32, Errno> {
    let (a, e) = (cvec(argv)?, cvec(envp)?);
    match utest::fork()? {
        0 => {
            if redirect(stdin, out, err_too).is_err() {
                rt::exit(125);
            }
            #[expect(
                clippy::let_underscore_must_use,
                reason = "an execve that returns failed; status 127 reports it (DESIGN §2.5)"
            )]
            let _ = sys::execve(a[0], a.as_ptr(), e.as_ptr());
            rt::exit(127)
        }
        pid => i32::try_from(pid).map_err(|_| Errno::ESRCH),
    }
}

/// Run `argv` with `envp`, the file `stdin` (if any) on fd 0 and the file
/// `out` on fd 1, and return its `wait4` status word.
pub fn run_redirected(
    argv: &[&CStr],
    envp: &[&CStr],
    stdin: Option<&[u8]>,
    out: &[u8],
) -> Result<u32, Errno> {
    let pid = spawn(argv, envp, stdin, out, false)?;
    cmd::wait(pid, 0).map(|(_, status)| status)
}

/// Create or truncate `path` and write `bytes` to it.
fn put_file(path: &[u8], bytes: &[u8]) -> Result<(), Errno> {
    let fd = cmd::open_flags(path, sys::O_WRONLY | sys::O_CREAT | sys::O_TRUNC)?;
    let r = write_all(fd as i32, bytes);
    sys::close(fd)?;
    r
}

/// `path`'s bytes into `buf`; `EFBIG` when they do not fit.
fn get_file<'b>(path: &[u8], buf: &'b mut [u8]) -> Result<&'b [u8], Errno> {
    let fd = cmd::open_flags(path, sys::O_RDONLY)?;
    let mut len = 0;
    let r = loop {
        let Some(rest) = buf.get_mut(len..) else {
            break Err(Errno::EFBIG);
        };
        if rest.is_empty() {
            let mut probe = [0u8; 1];
            break match cmd::read(fd, &mut probe) {
                Ok(0) => Ok(()),
                Ok(_) => Err(Errno::EFBIG),
                Err(e) => Err(e),
            };
        }
        match cmd::read(fd, rest) {
            Ok(0) => break Ok(()),
            Ok(n) => len += n,
            Err(e) => break Err(e),
        }
    };
    sys::close(fd)?;
    r.map(|()| buf.get(..len).unwrap_or(&[]))
}

/// `ls /bin`: the ten utilities, `sh` and `tests` among its lines, which
/// rise strictly.
fn ls_bin(out: &[u8]) -> bool {
    const NAMES: [&[u8]; 12] = [
        b"ls", b"cat", b"echo", b"grep", b"wc", b"true", b"false", b"sleep", b"yes", b"cmp", b"sh",
        b"tests",
    ];
    let mut lines = out.split(|&b| b == b'\n');
    let ends = out.last() == Some(&b'\n') && lines.next_back() == Some(&[][..]);
    let rising = lines.clone().zip(lines.clone().skip(1)).all(|(a, b)| a < b);
    ends && rising && NAMES.iter().all(|n| lines.clone().any(|l| l == *n))
}

/// Sleep `ms` milliseconds.
fn sleep_ms(ms: i64) -> Result<(), Errno> {
    let ts = [ms / 1000, ms % 1000 * 1_000_000];
    sys::nanosleep(ts.as_ptr().cast::<c_void>(), core::ptr::null_mut()).map(drop)
}

fn utilities_table() -> Outcome {
    let files: [(&[u8], &[u8]); 3] = [(GREP, b"foo\nbar\nfood\n"), (WC, b"a b\nc\n"), (CAT, &K1)];
    for (path, bytes) in files.into_iter().chain(CMP) {
        if put_file(path, bytes).is_err() {
            return Outcome::Fail("cannot write a /tmp/u75 input file");
        }
    }
    let mut buf = [0u8; 2048];
    for r in ROWS {
        if let Some(bytes) = r.stdin
            && put_file(IN, bytes).is_err()
        {
            return Outcome::Fail(r.what);
        }
        let Ok(status) = run_redirected(r.argv, &[], r.stdin.map(|_| IN), OUT) else {
            return Outcome::Fail(r.what);
        };
        let Ok(out) = get_file(OUT, &mut buf) else {
            return Outcome::Fail(r.what);
        };
        let out_ok = match r.out {
            Want::Is(want) => out == want,
            Want::Check(f) => f(out),
        };
        if Status::of(status) != Status::Exited(r.code) || !out_ok {
            return Outcome::Fail(r.what);
        }
    }
    match (sleep_row(), yes_row()) {
        (Ok(()), Ok(())) => Outcome::Ok,
        (Err(why), _) | (_, Err(why)) => Outcome::Fail(why),
    }
}

/// `sleep 1` still runs after 100 ms, then exits 0.
fn sleep_row() -> Result<(), &'static str> {
    let pid = spawn(&[c"/bin/sleep", c"1"], &[], None, OUT, false).map_err(|_| "sleep 1")?;
    sleep_ms(100).map_err(|_| "sleep 1: nanosleep")?;
    let early = cmd::wait(pid, WNOHANG);
    let done = cmd::wait(pid, 0);
    match (early, done) {
        (Ok((0, _)), Ok((p, st))) if p == pid as usize && st == 0 => Ok(()),
        (Ok((0, _)), _) => Err("sleep 1: not exit 0"),
        _ => Err("sleep 1: ended before 100 ms"),
    }
}

/// `yes abc` until its first whole line, checked every 1 ms for at most
/// `YES_POLLS` sleeps, then `SIGKILL`: signal 9, or exit status 1 when
/// `/tmp`'s store filled first, which a one-byte `O_APPEND` write to the
/// file confirms by failing with `ENOSPC`; and every whole line of its
/// output, one or more, is `abc`. Only fd 1 is on the file, so `yes`'s
/// complaint about the failed write is not in it.
fn yes_row() -> Result<(), &'static str> {
    // Empty, so its size counts only what `yes` writes.
    put_file(OUT, b"").map_err(|_| "yes abc: empty the file")?;
    let pid = spawn(&[c"/bin/yes", c"abc"], &[], None, OUT, false).map_err(|_| "yes abc")?;
    // The child runs while the parent sleeps (F128: both are pinned to the
    // BSP). A fork copies the whole address space under TCG, so the first
    // line can take a while there; under KVM, `yes` can fill `/tmp` before
    // a long sleep ends, so the wait is in 1 ms steps.
    let mut slept = Ok(());
    for _ in 0..YES_POLLS {
        if slept.is_err() || file_len(OUT).is_ok_and(|n| n >= 4) {
            break;
        }
        slept = sleep_ms(1);
    }
    // A `yes` that already exited is a zombie until the wait below, and
    // `kill` returns 0 for it too.
    let killed = sys::kill(pid, SIGKILL);
    let status = cmd::wait(pid, 0);
    slept.map_err(|_| "yes abc: nanosleep")?;
    killed.map_err(|_| "yes abc: kill")?;
    match status.map(|(_, st)| Status::of(st)) {
        Ok(Status::Signaled(sig)) if sig == SIGKILL as u8 => {}
        Ok(Status::Exited(1)) => {
            if append_byte(OUT) != Err(Errno::ENOSPC) {
                return Err("yes abc: exit 1, but /tmp has room");
            }
        }
        _ => return Err("yes abc: not signal 9, nor exit 1 on a full /tmp"),
    }
    let fd = cmd::open_flags(OUT, sys::O_RDONLY).map_err(|_| "yes abc: open")?;
    let mut r = Reader::new(fd);
    let (mut lines, mut at) = (0u64, 0usize);
    let ok = loop {
        match r.byte() {
            Ok(Some(b)) if b"abc\n".get(at) == Some(&b) => {
                (lines, at) = if b == b'\n' {
                    (lines + 1, 0)
                } else {
                    (lines, at + 1)
                };
            }
            Ok(Some(_)) | Err(_) => break false,
            Ok(None) => break lines > 0,
        }
    };
    sys::close(fd).map_err(|_| "yes abc: close")?;
    if ok {
        Ok(())
    } else {
        Err("yes abc: a line is not abc")
    }
}

/// Write one byte to `path` opened with `O_APPEND`: the `write`'s result.
fn append_byte(path: &[u8]) -> Result<usize, Errno> {
    let fd = cmd::open_flags(path, sys::O_WRONLY | sys::O_APPEND)?;
    let r = sys::write(fd, b"\n".as_ptr(), 1);
    sys::close(fd)?;
    r
}

/// `path`'s size, from `lseek(SEEK_END)`.
fn file_len(path: &[u8]) -> Result<usize, Errno> {
    // From Linux `include/uapi/linux/fs.h`.
    const SEEK_END: u32 = 2;
    let fd = cmd::open_flags(path, sys::O_RDONLY)?;
    let n = sys::lseek(fd, 0, SEEK_END);
    sys::close(fd)?;
    n
}

/// Whether `out` has a line equal to `line`.
fn has_line(out: &[u8], line: &[u8]) -> bool {
    out.split(|&b| b == b'\n').any(|l| l == line)
}

/// `/bin/sh` on a script: the lines it must print and its status.
fn sh_case(envp: &[&CStr], script: &[u8], lines: &[&[u8]], code: u8) -> Result<(), ()> {
    put_file(IN, script).map_err(drop)?;
    let pid = spawn(&[c"/bin/sh"], envp, Some(IN), OUT, true).map_err(drop)?;
    let (_, status) = cmd::wait(pid, 0).map_err(drop)?;
    let mut buf = [0u8; 2048];
    let out = get_file(OUT, &mut buf).map_err(drop)?;
    let ok = Status::of(status) == Status::Exited(code) && lines.iter().all(|l| has_line(out, l));
    if ok { Ok(()) } else { Err(()) }
}

fn sh_script_status() -> Outcome {
    let script: &[u8] = b"echo a b\nfalse\nnosuch\ntrue\n";
    let lines: &[&[u8]] = &[b"a b", b"sh: false: exit 1", b"sh: nosuch: not found"];
    if sh_case(&[c"PATH=/bin"], script, lines, 0).is_err() {
        return Outcome::Fail("PATH=/bin: echo, false, nosuch, true");
    }
    if sh_case(
        &[c"PATH=/nowhere"],
        b"true\n",
        &[b"sh: true: not found"],
        127,
    )
    .is_err()
    {
        return Outcome::Fail("PATH=/nowhere: true");
    }
    if sh_case(&[], b"true\n", &[], 0).is_err() {
        return Outcome::Fail("no PATH: true");
    }
    Outcome::Ok
}
