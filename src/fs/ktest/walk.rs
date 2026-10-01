//! In-guest test of the walker's path_resolution(7) rules (ROADMAP
//! §10.4, F056), from ring 3.

use vibeos::fs::FsError;
use vibeos::proc::{wexitstatus, wifexited};

use super::cwd::put_file;
use crate::file_init;
use crate::ktest::Outcome;
use crate::ktest::user::{self, Image};

const WALK_PATHS: Image = Image::UserBin("walk_paths");

/// The names under `/vibe` the test makes, deepest first, for cleanup.
const FILES: [&[u8]; 3] = [b"/vibe/l", b"/vibe/a/x", b"/vibe/f"];
const DIRS: [&[u8]; 2] = [b"/vibe/a/b", b"/vibe/a"];

fn clean() {
    for f in FILES {
        let _ = file_init::unlink(f);
    }
    for d in DIRS {
        let _ = file_init::rmdir(d);
    }
}

/// `walk_paths` finds that `/./vibe/f`, `//vibe/f`, `/dev/../vibe/f` and
/// `/VIBE/f` open `/vibe/f`, `/vibe/l/../x` opens `/vibe/a/x`, `/vibe/l/`
/// opens the directory `/vibe/a/b`, and `/vibe/f/` is `ENOTDIR`.
pub(crate) fn test_walk_path_resolution() -> Outcome {
    // The `vibefs` test leaves its own `/vibe/l`.
    clean();
    let r = walk_paths();
    clean();
    match r {
        Ok(()) => Outcome::Ok,
        Err(o) => o,
    }
}

fn walk_paths() -> Result<(), Outcome> {
    let step = |what: &'static str, r: Result<(), FsError>| {
        r.map_err(|e| crate::fail_fmt!("{what}: {}", e.as_str()))
    };
    step("write /vibe/f", put_file(None, b"/vibe/f", b"F"))?;
    step("mkdir /vibe/a", file_init::mkdir(b"/vibe/a", 0o755))?;
    step("mkdir /vibe/a/b", file_init::mkdir(b"/vibe/a/b", 0o755))?;
    step("write /vibe/a/x", put_file(None, b"/vibe/a/x", b"X"))?;
    step(
        "symlink /vibe/l",
        file_init::symlink_at(None, b"/vibe/l", b"a/b"),
    )?;
    match user::run(&WALK_PATHS, &["walk_paths"]) {
        Ok(st) if wifexited(st) && wexitstatus(st) == 0 => Ok(()),
        Ok(st) if wifexited(st) => Err(crate::fail_fmt!(
            "walk_paths: case {} failed",
            wexitstatus(st)
        )),
        Ok(st) => Err(crate::fail_fmt!("walk_paths: status {st:#x}")),
        Err(e) => Err(crate::fail_fmt!("walk_paths: spawn: {}", e.as_str())),
    }
}
