//! Host fsck.vibefs. Same format module as the kernel.

#![allow(
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "host tool: `alloc`'s growing calls may panic, and a failed allocation ends this host process, not the kernel (DESIGN §4.4)"
)]

use std::env;
use std::fs::File;
use std::io::Read;
use std::process::ExitCode;

use vibeos::vibefs::{self, Defect, MemDisk};

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
            for d in Defect::ALL {
                let n = r.count(d);
                if n != 0 {
                    println!("fsck-vibefs: {} {n}", d.as_str());
                }
            }
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
