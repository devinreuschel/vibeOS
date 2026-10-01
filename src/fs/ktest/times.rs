//! `fat_times_wall_clock` (kernel_tests only), re-exported from
//! `fs::ktest`: FAT and tmpfs stamp files from the wall clock
//! (ROADMAP §10.4 FAT timestamps, §8.2 `readdir`, `stat`, timestamp
//! conversion; F123).

use vibeos::fs::{FsError, InodeKind, O_CREAT, O_DIRECTORY, O_RDONLY, O_WRONLY, OpenFlags};

use crate::file_init;
use crate::ktest::Outcome;
use crate::time_init;

const DIR: &[u8] = b"/kt61t";
const FILE: &[u8] = b"/kt61t/f";
const TMP: &[u8] = b"/tmp/kt61t";

/// A failed step and its error.
type Step<T> = Result<T, (&'static str, FsError)>;

fn vfs<T>(what: &'static str, r: Result<T, FsError>) -> Step<T> {
    r.map_err(|e| (what, e))
}

/// Write `b"kt6"` to a new file `path`, then close it.
fn write3(path: &[u8]) -> Step<()> {
    let flags = OpenFlags::from_bits(O_WRONLY | O_CREAT);
    let f = vfs("create", file_init::open(path, flags, 0o644))?;
    let w = file_init::write(&f, b"kt6");
    let c = file_init::close(f);
    match vfs("write", w)? {
        3 => {}
        _ => return Err(("short write", FsError::Io)),
    }
    vfs("close", c)
}

/// The kind `readdir` of `dir` reports for `name`; `None` when it lists
/// no such entry.
fn listed_kind(dir: &[u8], name: &[u8]) -> Step<Option<InodeKind>> {
    let flags = OpenFlags::from_bits(O_RDONLY | O_DIRECTORY);
    let f = vfs("open dir", file_init::open(dir, flags, 0))?;
    let mut kind = None;
    let r = file_init::readdir(&f, &mut |d| {
        if d.name.as_bytes() == name {
            kind = Some(d.kind);
            return false;
        }
        true
    });
    let c = file_init::close(f);
    vfs("readdir", r)?;
    vfs("close dir", c)?;
    Ok(kind)
}

/// A file made on the FAT initrd between wall-clock reads `t0` and `t1`
/// `stat`s with an mtime in `[t0 & !1, t1]` (FAT keeps even seconds), and
/// `readdir` lists it as a regular file; a tmpfs file, stamped from
/// `Vfs::now`, `stat`s in `[t0, t1]`.
pub(crate) fn test_fat_times_wall_clock() -> Outcome {
    if !crate::fat_init::live() {
        return Outcome::Skip("no FAT initrd");
    }
    let r = times();
    let _ = file_init::unlink(FILE);
    let _ = file_init::rmdir(DIR);
    let _ = file_init::unlink(TMP);
    match r {
        Ok(()) => Outcome::Ok,
        Err((what, e)) => crate::fail_fmt!("{what}: {}", e.as_str()),
    }
}

fn times() -> Step<()> {
    let Some(t0) = time_init::unix_time_s() else {
        return Err(("no wall clock (unix_time_s is None)", FsError::Io));
    };
    vfs("mkdir /kt61t", file_init::mkdir(DIR, 0o755))?;
    write3(FILE)?;
    write3(TMP)?;
    let t1 = time_init::unix_time_s().ok_or(("no wall clock at t1", FsError::Io))?;
    let st = vfs("stat /kt61t/f", file_init::stat_path(FILE))?;
    if st.mtime < (t0 & !1) || st.mtime > t1 {
        crate::ktest_info!("fat mtime {} not in [{}, {}]", st.mtime, t0 & !1, t1);
        return Err(("FAT mtime outside the wall-clock window", FsError::Inval));
    }
    match listed_kind(DIR, b"f")? {
        Some(InodeKind::Reg) => {}
        Some(_) => return Err(("readdir lists f, not as a regular file", FsError::Inval)),
        None => return Err(("readdir does not list f", FsError::NotFound)),
    }
    let ts = vfs("stat /tmp/kt61t", file_init::stat_path(TMP))?;
    if ts.mtime < t0 || ts.mtime > t1 {
        crate::ktest_info!("tmpfs mtime {} not in [{}, {}]", ts.mtime, t0, t1);
        return Err(("tmpfs mtime outside the wall-clock window", FsError::Inval));
    }
    Ok(())
}
