//! Host fsck.vibefs. Same format module as the kernel.

use std::env;
use std::fs::File;
use std::io::Read;
use std::process::ExitCode;

use vibeos_hostlib_tests::vibefs::{self, MemDisk};

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: fsck-vibefs <image>");
        return ExitCode::from(2);
    };
    let mut f = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("fsck-vibefs: open {path}: {e}");
            return ExitCode::from(1);
        }
    };
    let mut data = Vec::new();
    if let Err(e) = f.read_to_end(&mut data) {
        eprintln!("fsck-vibefs: read: {e}");
        return ExitCode::from(1);
    }
    let mut disk = match MemDisk::new(&mut data) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("fsck-vibefs: {}", e.as_str());
            return ExitCode::from(1);
        }
    };
    match vibefs::fsck(&mut disk) {
        Ok(r) => {
            println!(
                "fsck-vibefs: gen {} errors {} warnings {}",
                r.generation, r.errors, r.warnings
            );
            if r.errors != 0 {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("fsck-vibefs: {}", e.as_str());
            ExitCode::from(1)
        }
    }
}
