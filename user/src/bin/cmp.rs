//! `/bin/cmp file1 file2` (ROADMAP §10.5): nothing when the files are equal;
//! otherwise `<f1> <f2> differ: char <n>, line <l>` on fd 1, or
//! `cmp: EOF on <f>` on fd 2 when `<f>` is a prefix of the other. Status 0
//! equal, 1 different, 2 on an error.

#![no_std]
#![no_main]

use vibeos_user::cmd::{self, Out, Reader};
use vibeos_user::env::Env;
use vibeos_user::sys::Errno;

vibeos_user::main!(main);

fn main(env: &Env) -> i32 {
    let (Some(a), Some(b)) = (env.arg(1), env.arg(2)) else {
        return cmd::fail(b"cmp", b"two files", Errno::EINVAL, 2);
    };
    cmd::with_reader(a, |ra| {
        let r = cmd::with_reader(b, |rb| Ok(compare([a, b], ra, rb)));
        Ok(r.unwrap_or_else(|e| cmd::fail(b"cmp", b, e, 2)))
    })
    .unwrap_or_else(|e| cmd::fail(b"cmp", a, e, 2))
}

/// Compare byte by byte, each file through its own buffer.
fn compare(name: [&[u8]; 2], ra: &mut Reader, rb: &mut Reader) -> i32 {
    let (mut n, mut line) = (1u64, 1u64);
    loop {
        let (x, y) = match (ra.byte(), rb.byte()) {
            (Ok(x), Ok(y)) => (x, y),
            (Err(e), _) => return cmd::fail(b"cmp", name[0], e, 2),
            (_, Err(e)) => return cmd::fail(b"cmp", name[1], e, 2),
        };
        let mut out = Out::new(1);
        let said = match (x, y) {
            (None, None) => return 0,
            (Some(x), Some(y)) if x == y => {
                (n, line) = (n + 1, line + u64::from(x == b'\n'));
                continue;
            }
            (Some(_), Some(_)) => (|| {
                out.put(name[0])?
                    .put(b" ")?
                    .put(name[1])?
                    .put(b" differ: char ")?;
                out.dec(n)?.put(b", line ")?.dec(line)?.put(b"\n")?.flush()
            })(),
            (None, Some(_)) | (Some(_), None) => (|| {
                let short = if x.is_none() { name[0] } else { name[1] };
                let mut err = Out::new(2);
                err.put(b"cmp: EOF on ")?.put(short)?.put(b"\n")?.flush()
            })(),
        };
        return said.map_or_else(|e| cmd::fail(b"cmp", b"write", e, 2), |()| 1);
    }
}
