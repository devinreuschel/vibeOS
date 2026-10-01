//! `/bin/wc [-l] [-w] [-c] [file...]` (ROADMAP §10.5): `<lines> <words> <bytes>
//! <name>`, the selected counts (all with no flag), single-spaced; no name for
//! fd 0, read with no file; `total` for several. Status 0, or 1 on an error.

#![no_std]
#![no_main]

use vibeos_user::cmd::{self, Out};
use vibeos_user::env::Env;
use vibeos_user::sys::Errno;

vibeos_user::main!(main);

fn main(env: &Env) -> i32 {
    let (sel, first) = match cmd::flags(env, b"lwc") {
        Ok((sel, first)) => (if sel == [false; 3] { [true; 3] } else { sel }, first),
        Err(flag) => return cmd::fail(b"wc", flag, Errno::EINVAL, 1),
    };
    let named = env.argc() > first;
    let (mut total, mut status) = ([0u64; 3], 0);
    for path in cmd::operands(env, first, &[b"-"]) {
        let mut n = [0u64; 3];
        let counted = cmd::with_reader(path, |r| {
            let mut word = false;
            while let Some(b) = r.byte()? {
                let space = b.is_ascii_whitespace() || b == 0x0b;
                n[0] += u64::from(b == b'\n');
                n[1] += u64::from(!space && !word);
                n[2] += 1;
                word = !space;
            }
            Ok(())
        });
        match counted {
            Ok(()) => status |= show(n, sel, named.then_some(path)),
            Err(e) => status = cmd::fail(b"wc", path, e, 1),
        }
        total = core::array::from_fn(|i| total[i].saturating_add(n[i]));
    }
    if env.argc() > first + 1 {
        status |= show(total, sel, Some(b"total"));
    }
    status
}

/// One output line: 0, or 1 when the write failed.
fn show(n: [u64; 3], sel: [bool; 3], name: Option<&[u8]>) -> i32 {
    let mut out = Out::new(1);
    let mut line = || {
        for (i, (v, _)) in n.iter().zip(sel).filter(|(_, on)| *on).enumerate() {
            out.put(if i == 0 { b"" } else { b" " })?.dec(*v)?;
        }
        if let Some(p) = name {
            out.put(b" ")?.put(p)?;
        }
        out.put(b"\n")?.flush()
    };
    line().map_or_else(|e| cmd::fail(b"wc", b"write", e, 1), |()| 0)
}
