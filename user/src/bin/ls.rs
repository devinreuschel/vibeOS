//! `/bin/ls [-a] [path...]` (ROADMAP §10.5): a directory's `getdents64`
//! names, sorted bytewise, one a line, dot-names only under `-a`; a
//! non-directory operand prints itself (`fstat`); several operands get
//! `<path>:` headers. No operand lists `.`. Status 0, or 2 if one failed.

#![no_std]
#![no_main]

use vibeos_user::cmd::{self, Out};
use vibeos_user::env::Env;
use vibeos_user::sys::{self, Errno};

vibeos_user::main!(main);

fn main(env: &Env) -> i32 {
    let (all, first) = match cmd::flags(env, b"a") {
        Ok(([all], first)) => (all, first),
        Err(flag) => return cmd::fail(b"ls", flag, Errno::EINVAL, 2),
    };
    let mut status = 0;
    for (i, path) in cmd::operands(env, first, &[b"."]).enumerate() {
        let header = env.argc() > first + 1;
        if let Err(e) = ls(path, all, header.then_some(i != 0)) {
            status = cmd::fail(b"ls", path, e, 2);
        }
    }
    status
}

/// List `path`; `header` is `Some(blank line first)` for several operands.
fn ls(path: &[u8], all: bool, header: Option<bool>) -> Result<(), Errno> {
    let fd = cmd::open_flags(path, sys::O_RDONLY)?;
    let names = cmd::is_dir(fd).and_then(|d| d.then(|| cmd::dir_names(fd)).transpose());
    cmd::close_arg(fd);
    let mut out = Out::new(1);
    match names? {
        None => out.put(path)?.put(b"\n")?,
        Some(mut names) => {
            names.sort_unstable();
            if let Some(blank) = header {
                out.put(if blank { b"\n" } else { b"" })?
                    .put(path)?
                    .put(b":\n")?;
            }
            for n in names.iter().filter(|n| all || !n.starts_with(b".")) {
                out.put(n)?.put(b"\n")?;
            }
            &mut out
        }
    };
    out.flush()
}
