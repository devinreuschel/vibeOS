//! `/bin/sh` (ROADMAP §9.8): the interactive shell init starts. It prints
//! `vibeOS: shell ready` once, then the prompt `vibeos> `, and reads fd 0
//! one byte at a time, echoing what it keeps. CR or LF submits the line;
//! BS or DEL erases a byte. After leading spaces, `echo` writes the rest of
//! the line and `ps` writes the process list `psinfo` returns; any other
//! line does nothing.

#![no_std]
#![no_main]

use vibeos_user::env::Env;
use vibeos_user::sys;

vibeos_user::main!(main);

const READY: &[u8] = b"vibeOS: shell ready\n";
const PROMPT: &[u8] = b"vibeos> ";
const ERASE: &[u8] = b"\x08 \x08";
const NL: &[u8] = b"\n";

/// The bytes a line keeps; the rest are dropped.
const LINE_MAX: usize = 127;

/// Write `buf` to fd 1 in one call.
fn out(buf: &[u8]) {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5: the shell has nowhere to report a failed console write"
    )]
    let _ = sys::write(1, buf.as_ptr(), buf.len());
}

fn main(_env: &Env) -> i32 {
    out(READY);
    let mut line = [0u8; LINE_MAX];
    loop {
        out(PROMPT);
        let mut len = 0usize;
        loop {
            let mut key = 0u8;
            // SAFETY: `read` writes at most one byte into `key`, a local no
            // reference covers; established here.
            let r = unsafe { sys::read(0, &raw mut key, 1) };
            if r != Ok(1) {
                // End of input or an error: a fresh prompt and an empty line.
                break;
            }
            match key {
                b'\r' | b'\n' => {
                    out(NL);
                    run(&line[..len]);
                    break;
                }
                0x08 | 0x7f => {
                    if len > 0 {
                        len -= 1;
                        out(ERASE);
                    }
                }
                _ if len < LINE_MAX => {
                    line[len] = key;
                    len += 1;
                    out(&[key]);
                }
                _ => {}
            }
        }
    }
}

/// Run one submitted line.
fn run(line: &[u8]) {
    let cmd = skip_spaces(line);
    if let Some(rest) = cmd.strip_prefix(b"echo") {
        out(skip_spaces(rest));
        out(NL);
    } else if cmd.starts_with(b"ps") {
        let mut buf = [0u8; 512];
        // SAFETY: `psinfo` writes at most `buf.len()` bytes into `buf`, a
        // local no reference covers; established here.
        let r = unsafe { sys::psinfo(buf.as_mut_ptr(), buf.len()) };
        if let Ok(n @ 1..) = r {
            out(&buf[..n.min(buf.len())]);
        }
    }
}

fn skip_spaces(s: &[u8]) -> &[u8] {
    let n = s.iter().take_while(|&&b| b == b' ').count();
    &s[n..]
}
