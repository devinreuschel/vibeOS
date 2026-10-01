//! `/bin/sh` (ROADMAP §9.8, §10.5): the shell init starts. It prints
//! `vibeOS: shell ready` once, then the prompt `vibeos> `, and reads fd 0
//! one byte at a time, echoing what it keeps. CR or LF submits the line;
//! BS or DEL erases a byte. A line holds at most 4096 bytes; its words, at
//! most 64, split on spaces and tabs, with no quoting, pipes or redirection
//! until ROADMAP §13.7.
//!
//! The built-ins are `poweroff` and `reboot`, through the `reboot` call,
//! and `ps`, which writes what `psinfo` returns. Any other first word is a
//! program: a name with a `/` runs as given, any other is looked up in each
//! `PATH` directory (`/bin:/sbin` when `PATH` is unset), and the child gets
//! the shell's environment. On fd 2 a program that exits non-zero prints
//! `sh: <name>: exit <n>`, and one a signal ends `sh: <name>: signal <n>`.
//! A read error exits 1, and end of input (a file on fd 0) exits with the
//! last status, the exit code or 128 plus the signal.

#![no_std]
#![no_main]

use vibeos_user::cmd::{self, Out, Status};
use vibeos_user::env::Env;
use vibeos_user::sys::{self, Errno};
use vibeos_user::{rt, utest};

vibeos_user::main!(main);

const READY: &[u8] = b"vibeOS: shell ready\n";
const PROMPT: &[u8] = b"vibeos> ";
const ERASE: &[u8] = b"\x08 \x08";
const NL: &[u8] = b"\n";

/// The bytes a line keeps; the rest are dropped.
const LINE_MAX: usize = 4096;
/// The words a line may have.
const WORDS_MAX: usize = 64;
/// `PATH` when the environment has none.
const DEFAULT_PATH: &[u8] = b"/bin:/sbin";
/// The longest `<dir>/<name>` the `PATH` walk tries, as `open` allows.
const PATH_MAX: usize = 255;

// reboot(2)'s magic numbers and commands, from Linux
// `include/uapi/linux/reboot.h`.
const MAGIC1: i32 = 0xfee1_dead_u32 as i32;
const MAGIC2: i32 = 0x2812_1969;
const CMD_RESTART: u32 = 0x0123_4567;
const CMD_POWER_OFF: u32 = 0x4321_fedc;

/// Write `buf` to fd 1 in one call.
fn out(buf: &[u8]) {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5: the shell has nowhere to report a failed console write"
    )]
    let _ = sys::write(1, buf.as_ptr(), buf.len());
}

/// Write the pieces of one line to fd 2 in one `write`, each `Some` number
/// in decimal after the bytes before it.
fn say(parts: &[&[u8]], n: Option<u64>) {
    let mut o = Out::new(2);
    let line = (|| {
        for p in parts {
            o.put(p)?;
        }
        if let Some(n) = n {
            o.dec(n)?;
        }
        o.put(NL)?.flush()
    })();
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5: fd 2 is where the shell reports; nothing is left to tell"
    )]
    let _ = line;
}

/// `e` as the number a status line prints.
fn errno(e: Errno) -> Option<u64> {
    Some(u64::from(e.0.unsigned_abs()))
}

fn main(env: &Env) -> i32 {
    let envp = match cmd::envp(env) {
        Ok(v) => v,
        Err(e) => {
            say(&[b"sh: environment: errno "], errno(e));
            return 1;
        }
    };
    let path = env.var(b"PATH").unwrap_or(DEFAULT_PATH);
    out(READY);
    let mut line = [0u8; LINE_MAX + 1];
    let mut last = 0;
    loop {
        out(PROMPT);
        let mut len = 0usize;
        loop {
            let mut key = [0u8; 1];
            match cmd::read(0, &mut key) {
                Ok(0) if len == 0 => return last,
                Ok(0) => {
                    // End of input inside a line: run what came, then end.
                    out(NL);
                    return run(&mut line, len, path, &envp).unwrap_or(last);
                }
                Ok(_) => {}
                Err(_) => return 1,
            }
            match key[0] {
                b'\r' | b'\n' => {
                    out(NL);
                    last = run(&mut line, len, path, &envp).unwrap_or(last);
                    break;
                }
                0x08 | 0x7f => {
                    if len > 0 {
                        len -= 1;
                        out(ERASE);
                    }
                }
                k if len < LINE_MAX => {
                    line[len] = k;
                    len += 1;
                    out(&[k]);
                }
                _ => {}
            }
        }
    }
}

/// Run the first `len` bytes of `line`, splitting its words in place: the
/// new last status, or `None` for a line with no words.
fn run(line: &mut [u8; LINE_MAX + 1], len: usize, path: &[u8], envp: &[*const u8]) -> Option<i32> {
    line[len] = 0;
    for b in line.iter_mut().take(len) {
        if *b == b' ' || *b == b'\t' {
            *b = 0;
        }
    }
    let mut argv = [core::ptr::null::<u8>(); WORDS_MAX + 1];
    let mut name: &[u8] = &[];
    for (i, w) in line[..len]
        .split(|&b| b == 0)
        .filter(|w| !w.is_empty())
        .enumerate()
    {
        if i == WORDS_MAX {
            say(
                &[b"sh: ", name, b": more than 64 words: errno "],
                errno(Errno::E2BIG),
            );
            return Some(1);
        }
        if i == 0 {
            name = w;
        }
        argv[i] = w.as_ptr();
    }
    Some(match name {
        b"" => return None,
        b"poweroff" => power(name, CMD_POWER_OFF),
        b"reboot" => power(name, CMD_RESTART),
        b"ps" => ps(),
        _ => spawn(name, &argv, path, envp),
    })
}

/// `poweroff` and `reboot`: the call returns only on a failure.
fn power(name: &[u8], cmd: u32) -> i32 {
    match sys::reboot(MAGIC1, MAGIC2, cmd, core::ptr::null_mut()) {
        Ok(_) => 0,
        Err(e) => {
            say(&[b"sh: ", name, b": errno "], errno(e));
            1
        }
    }
}

/// `ps`: the process list `psinfo` writes.
fn ps() -> i32 {
    let mut buf = [0u8; 4096];
    // SAFETY: `psinfo` writes at most `buf.len()` bytes into `buf`, a local
    // no reference covers; established here.
    match unsafe { sys::psinfo(buf.as_mut_ptr(), buf.len()) } {
        Ok(n) => {
            out(buf.get(..n).unwrap_or(&buf));
            0
        }
        Err(e) => {
            say(&[b"sh: ps: errno "], errno(e));
            1
        }
    }
}

/// Fork, exec `name` in the child, wait, and report how it ended.
fn spawn(name: &[u8], argv: &[*const u8], path: &[u8], envp: &[*const u8]) -> i32 {
    let pid = match utest::fork() {
        Ok(0) => rt::exit(exec(name, argv, path, envp)),
        Ok(pid) => pid as i32,
        Err(e) => {
            say(&[b"sh: fork: errno "], errno(e));
            return 1;
        }
    };
    let status = match cmd::wait(pid, 0) {
        Ok((_, st)) => Status::of(st),
        Err(e) => {
            say(&[b"sh: wait4: errno "], errno(e));
            return 1;
        }
    };
    match status {
        Status::Exited(0) => {}
        Status::Exited(c) => say(&[b"sh: ", name, b": exit "], Some(u64::from(c))),
        Status::Signaled(s) => say(&[b"sh: ", name, b": signal "], Some(u64::from(s))),
    }
    status.code()
}

/// In the child: exec `name`, as given when it has a `/`, else from each
/// `PATH` directory, past `ENOENT` and `ENOTDIR`. Returns the exit status
/// when nothing ran: 127 when nothing was found, 126 on another error.
fn exec(name: &[u8], argv: &[*const u8], path: &[u8], envp: &[*const u8]) -> i32 {
    let mut found = None;
    if name.contains(&b'/') {
        // `name` is NUL-terminated in the line, so `argv[0]` is its path.
        found = sys::execve(argv[0], argv.as_ptr(), envp.as_ptr()).err();
    } else {
        let mut full = [0u8; PATH_MAX + 1];
        for dir in path.split(|&b| b == b':') {
            let dir: &[u8] = if dir.is_empty() { b"." } else { dir };
            let n = dir.len() + 1 + name.len();
            let Some(dst) = full.get_mut(..n + 1) else {
                continue;
            };
            dst[..dir.len()].copy_from_slice(dir);
            dst[dir.len()] = b'/';
            dst[dir.len() + 1..n].copy_from_slice(name);
            dst[n] = 0;
            match sys::execve(full.as_ptr(), argv.as_ptr(), envp.as_ptr()) {
                Err(Errno::ENOENT) | Err(Errno::ENOTDIR) => {}
                r => {
                    found = r.err();
                    break;
                }
            }
        }
    }
    match found {
        None | Some(Errno::ENOENT) | Some(Errno::ENOTDIR) => {
            say(&[b"sh: ", name, b": not found"], None);
            127
        }
        Some(e) => {
            say(&[b"sh: ", name, b": cannot run: errno "], errno(e));
            126
        }
    }
}
