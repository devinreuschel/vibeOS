//! In-guest test of `open`'s reservations (ROADMAP §10.4, F057): with
//! the system-wide open-file table full, or the process's descriptor
//! table, `open(O_TRUNC)` and `open(O_CREAT)` fail before they change a
//! file.

use vibeos::fs::{FileRef, FsError, O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY, OpenFlags};
use vibeos::kalloc::TryVec;
use vibeos::limits::MAX_OPEN_FILES;
use vibeos::proc::{wexitstatus, wifexited};

use crate::file_init;
use crate::ktest::Outcome;
use crate::ktest::user::{self, Image};

const OPEN_TRUNC: Image = Image::UserBin("open_trunc");
const TRUNC: &[u8] = b"/vibe/s60t";
const NEW: &[u8] = b"/vibe/s60n";
const SIZE: usize = 100;

/// `open(O_TRUNC)` with the open-file table full is `ENFILE`, and so is
/// `open(O_CREAT)` of a new name; with the descriptor table full both are
/// `EMFILE`. Each time the file keeps its size and the new name is not
/// made.
pub(crate) fn open_trunc_enfile() -> Outcome {
    let r = check();
    let _ = file_init::unlink(TRUNC);
    let _ = file_init::unlink(NEW);
    match r {
        Ok(()) => Outcome::Ok,
        Err(o) => o,
    }
}

fn check() -> Result<(), Outcome> {
    let _ = file_init::unlink(NEW);
    write_file().map_err(|e| crate::fail_fmt!("write {SIZE} bytes: {}", e.as_str()))?;
    let enfile = with_table_full(|| user::run(&OPEN_TRUNC, &["open_trunc", "enfile"]))?;
    exited_0("enfile", enfile)?;
    unchanged("enfile")?;
    let emfile = user::run(&OPEN_TRUNC, &["open_trunc", "emfile"]);
    exited_0("emfile", emfile)?;
    unchanged("emfile")
}

fn write_file() -> Result<(), FsError> {
    let flags = OpenFlags::from_bits(O_WRONLY | O_CREAT | O_TRUNC);
    let f = file_init::open(TRUNC, flags, 0o644)?;
    let w = file_init::write(&f, &[b's'; SIZE]);
    let c = file_init::close(f);
    match w? {
        SIZE => c,
        _ => Err(FsError::Io),
    }
}

/// Run `f` while every open-file slot holds an open of `TRUNC`; the files
/// are closed on every path.
fn with_table_full<R>(f: impl FnOnce() -> R) -> Result<R, Outcome> {
    let mut held: TryVec<FileRef> =
        TryVec::try_with_capacity(MAX_OPEN_FILES).map_err(|_| Outcome::Fail("no memory"))?;
    let full = loop {
        match file_init::open(TRUNC, OpenFlags::from_bits(O_RDONLY), 0) {
            Ok(fr) if held.len() >= MAX_OPEN_FILES => {
                let _ = file_init::close(fr);
                break Err(Outcome::Fail("more opens than the table holds"));
            }
            // Within the capacity reserved above: no allocation.
            Ok(fr) => {
                if held.try_push(fr).is_err() {
                    break Err(Outcome::Fail("no room for a FileRef"));
                }
            }
            Err(FsError::NFile) => break Ok(()),
            Err(e) => break Err(crate::fail_fmt!("filling the table: {}", e.as_str())),
        }
    };
    let r = full.map(|()| f());
    let mut closed = Ok(());
    while let Some(fr) = held.pop() {
        if let Err(e) = file_init::close(fr) {
            closed = Err(e);
        }
    }
    closed.map_err(|e| crate::fail_fmt!("close: {}", e.as_str()))?;
    r
}

fn exited_0(case: &str, st: Result<u32, crate::user_init::LoadError>) -> Result<(), Outcome> {
    match st {
        Ok(st) if wifexited(st) && wexitstatus(st) == 0 => Ok(()),
        Ok(st) if wifexited(st) => Err(crate::fail_fmt!(
            "open_trunc {case}: check {} failed",
            wexitstatus(st)
        )),
        Ok(st) => Err(crate::fail_fmt!("open_trunc {case}: status {st:#x}")),
        Err(e) => Err(crate::fail_fmt!("open_trunc {case}: spawn: {}", e.as_str())),
    }
}

fn unchanged(case: &str) -> Result<(), Outcome> {
    match file_init::stat_path(TRUNC) {
        Ok(s) if s.size == SIZE as u64 => {}
        Ok(s) => {
            return Err(crate::fail_fmt!(
                "{case}: {} bytes after the failed open, want {SIZE}",
                s.size
            ));
        }
        Err(e) => return Err(crate::fail_fmt!("{case}: stat: {}", e.as_str())),
    }
    match file_init::stat_path(NEW) {
        Err(FsError::NotFound) => Ok(()),
        Ok(_) => Err(crate::fail_fmt!("{case}: the failed O_CREAT made its file")),
        Err(e) => Err(crate::fail_fmt!("{case}: stat new: {}", e.as_str())),
    }
}
