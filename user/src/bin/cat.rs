//! `/bin/cat [file...]` (ROADMAP §10.5): each file's bytes in order; no
//! operand, or `-`, reads fd 0. Status 0, or 1 when any file failed.

#![no_std]
#![no_main]

use vibeos_user::cmd;
use vibeos_user::env::Env;
use vibeos_user::io::write_all;

vibeos_user::main!(main);

fn main(env: &Env) -> i32 {
    let mut status = 0;
    for path in cmd::operands(env, 1, &[b"-"]) {
        let mut wrote = Ok(());
        let read = cmd::with_reader(path, |r| {
            let mut buf = [0u8; 256];
            loop {
                match r.read(&mut buf)? {
                    0 => return Ok(()),
                    n => wrote = write_all(1, buf.get(..n).unwrap_or(&[])),
                }
                if wrote.is_err() {
                    return Ok(());
                }
            }
        });
        if let Err(e) = read {
            status = cmd::fail(b"cat", path, e, 1);
        }
        if let Err(e) = wrote {
            status = cmd::fail(b"cat", b"write", e, 1);
        }
    }
    status
}
