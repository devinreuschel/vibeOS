//! In-guest tests of working-directory references (ROADMAP §10.4, F057,
//! F086): a process's relative paths resolve from its own working
//! directory, not the kernel shell's.

use vibeos::fs::{FsError, O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY, OpenFlags, WalkBase};
use vibeos::proc::{wexitstatus, wifexited};

use crate::file_init;
use crate::fs_init;
use crate::ktest::Outcome;
use crate::ktest::user::{self, Image};
use crate::shell::cmds::fs::{cd, shell_base, shell_cwd};

const CWD_PROBE: Image = Image::UserBin("cwd_probe");

/// Write `data` to `path` from `base`, made or emptied.
pub(super) fn put_file(base: Option<WalkBase>, path: &[u8], data: &[u8]) -> Result<(), FsError> {
    let flags = OpenFlags::from_bits(O_WRONLY | O_CREAT | O_TRUNC);
    let f = file_init::open_at(base, path, flags, 0o644)?;
    let w = file_init::write(&f, data);
    let c = file_init::close(f);
    match w? {
        n if n == data.len() => c,
        _ => Err(FsError::Io),
    }
}

/// Up to `out.len()` bytes of `path` from `base`, from offset 0, in one
/// read; the count.
pub(super) fn get_file(
    base: Option<WalkBase>,
    path: &[u8],
    out: &mut [u8],
) -> Result<usize, FsError> {
    let f = file_init::open_at(base, path, OpenFlags::from_bits(O_RDONLY), 0)?;
    let r = file_init::read(&f, out);
    let c = file_init::close(f);
    let n = r?;
    c?;
    Ok(n)
}

/// The kernel shell's directory is `/vibe`, and a relative open from it
/// reads `/vibe/cwdprobe`; a user program the kernel starts, and its
/// forked child, open the relative `cwdprobe` from their own working
/// directory, `/`. `fork` takes references to the root and working
/// directory and exit drops them, so `/`'s dentry count is back at its
/// baseline after the run.
pub(crate) fn test_cwd_per_process() -> Outcome {
    let r = cwd_per_process();
    // Every path, failure included: a shell left in `/vibe` would hold
    // the vibefs mount busy for later tests.
    let back = cd(b"/");
    let _ = file_init::unlink(b"/cwdprobe");
    let _ = file_init::unlink(b"/vibe/cwdprobe");
    match (r, back) {
        (Err(o), _) => o,
        (Ok(()), Err(e)) => crate::fail_fmt!("cd / after the test: {}", e.as_str()),
        (Ok(()), Ok(())) => Outcome::Ok,
    }
}

fn cwd_per_process() -> Result<(), Outcome> {
    let step = |what: &'static str, r: Result<(), FsError>| {
        r.map_err(|e| crate::fail_fmt!("{what}: {}", e.as_str()))
    };
    step("write /cwdprobe", put_file(None, b"/cwdprobe", b"root"))?;
    step(
        "write /vibe/cwdprobe",
        put_file(None, b"/vibe/cwdprobe", b"vibe"),
    )?;
    let root = fs_init::with(|v| v.root()).map_err(|_| Outcome::Fail("no root"))?;
    step("cd /vibe", cd(b"/vibe"))?;
    let cwd = shell_cwd().ok_or(Outcome::Fail("cd left no reference"))?;
    let mut path = [0u8; 32];
    let n = file_init::dir_path(None, cwd, &mut path)
        .map_err(|e| crate::fail_fmt!("dir_path: {}", e.as_str()))?;
    if path.get(..n) != Some(b"/vibe".as_slice()) {
        return Err(Outcome::Fail("the shell's directory is not /vibe"));
    }
    let mut buf = [0u8; 8];
    let n = get_file(shell_base(), b"cwdprobe", &mut buf)
        .map_err(|e| crate::fail_fmt!("shell's relative open: {}", e.as_str()))?;
    if buf.get(..n) != Some(b"vibe".as_slice()) {
        return Err(Outcome::Fail("the shell's relative open missed /vibe"));
    }
    let base = file_init::dentry_refs(root);
    match user::run(&CWD_PROBE, &["cwd_probe"]) {
        Ok(st) if wifexited(st) && wexitstatus(st) == 0 => {}
        Ok(st) => return Err(crate::fail_fmt!("cwd_probe: status {st:#x}, want exited 0")),
        Err(e) => return Err(crate::fail_fmt!("cwd_probe: spawn: {}", e.as_str())),
    }
    let after = file_init::dentry_refs(root);
    if after != base {
        return Err(crate::fail_fmt!(
            "/'s dentry has {after} holders after the run, {base} before"
        ));
    }
    Ok(())
}
