//! In-guest tests of path resolution through `Vfs` (ROADMAP §10.4, A3,
//! F086, F126).

use vibeos::fs::FsError;

use crate::file_init;
use crate::ktest::{Outcome, fid};

/// `unlink` drops the name from the directory the path resolved to: a
/// dentry a `stat` cached in `/s59u` is gone after the unlink, so the same
/// `stat` finds nothing (F126).
pub(crate) fn test_vfs_unlink_drops_parent_dentry() -> Outcome {
    let r = unlink_drops();
    let _ = file_init::unlink(b"/s59u/f");
    let _ = file_init::rmdir(b"/s59u");
    match r {
        Ok(()) => Outcome::Ok,
        Err(why) => Outcome::Fail(why),
    }
}

fn unlink_drops() -> Result<(), &'static str> {
    file_init::mkdir(b"/s59u", 0o755).map_err(|_| "mkdir /s59u")?;
    file_init::creat(b"/s59u/f").map_err(|_| "create /s59u/f")?;
    fid::stat_path("/s59u/f").map_err(|_| "stat before unlink")?;
    file_init::unlink(b"/s59u/f").map_err(|_| "unlink")?;
    match fid::stat_path("/s59u/f") {
        Err(FsError::NotFound) => {}
        Ok(_) => return Err("stat still finds the unlinked name"),
        Err(_) => return Err("stat after unlink: not NotFound"),
    }
    file_init::rmdir(b"/s59u").map_err(|_| "rmdir /s59u")
}
