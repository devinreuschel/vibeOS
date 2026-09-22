//! Host mkfs.vibefs. Same format module as the kernel.

use std::env;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::process::ExitCode;

use vibeos::vibefs::{self, MemDisk, Vol};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut label: &[u8] = b"vibeos";
    let mut path: Option<&str> = None;
    let mut i = 0usize;
    while i < args.len() {
        if args[i] == "-L" {
            i += 1;
            if i >= args.len() {
                eprintln!("mkfs-vibefs: -L needs a label");
                return ExitCode::from(2);
            }
            label = args[i].as_bytes();
        } else if args[i].starts_with('-') {
            eprintln!("mkfs-vibefs: unknown flag {}", args[i]);
            return ExitCode::from(2);
        } else {
            path = Some(args[i].as_str());
        }
        i += 1;
    }
    let Some(path) = path else {
        eprintln!("usage: mkfs-vibefs [-L label] <image>");
        return ExitCode::from(2);
    };
    let mut f = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("mkfs-vibefs: open {path}: {e}");
            return ExitCode::from(1);
        }
    };
    let len = match f.metadata() {
        Ok(m) => m.len() as usize,
        Err(e) => {
            eprintln!("mkfs-vibefs: stat: {e}");
            return ExitCode::from(1);
        }
    };
    let mut data = vec![0u8; len];
    let mut disk = match MemDisk::new(&mut data) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("mkfs-vibefs: {}", e.as_str());
            return ExitCode::from(1);
        }
    };
    let mut vol = Vol::new();
    if let Err(e) = vibefs::mkfs(&mut disk, label, &mut vol) {
        eprintln!("mkfs-vibefs: {}", e.as_str());
        return ExitCode::from(1);
    }
    if let Err(e) = (|| {
        f.seek(SeekFrom::Start(0))?;
        f.write_all(disk.bytes())?;
        f.flush()?;
        Ok::<(), std::io::Error>(())
    })() {
        eprintln!("mkfs-vibefs: write: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
