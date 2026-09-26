//! Host FAT32 initrd. Same `fat::mkinitrd` as the kernel, plus optional files.

use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use vibeos::fat::{self, FatError, FatInode, FatVol, INITRD_BYTES, MemDisk, SEC};

const NOW: u32 = 1_262_304_000;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut path: Option<&str> = None;
    let mut extras: Vec<(&str, &str)> = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        if args[i] == "--add" {
            i += 1;
            if i >= args.len() {
                eprintln!("mkinitrd: --add needs src:dest");
                return ExitCode::from(2);
            }
            let spec = args[i].as_str();
            match spec.split_once(':') {
                Some((src, dest)) if !src.is_empty() && !dest.is_empty() => {
                    extras.push((src, dest));
                }
                _ => {
                    eprintln!("mkinitrd: --add wants src:dest, got {spec}");
                    return ExitCode::from(2);
                }
            }
        } else if args[i].starts_with('-') {
            eprintln!("mkinitrd: unknown flag {}", args[i]);
            return ExitCode::from(2);
        } else if path.is_none() {
            path = Some(args[i].as_str());
        } else {
            eprintln!("mkinitrd: extra argument {}", args[i]);
            return ExitCode::from(2);
        }
        i += 1;
    }
    let Some(path) = path else {
        eprintln!("usage: mkinitrd <image> [--add src:dest]...");
        return ExitCode::from(2);
    };

    let mut buf = vec![0u8; INITRD_BYTES];
    if let Err(e) = fat::mkinitrd(&mut buf) {
        eprintln!("mkinitrd: {}", e.as_str());
        return ExitCode::from(1);
    }
    if let Err(e) = add_extras(&mut buf, &extras) {
        eprintln!("mkinitrd: {}", e.as_str());
        return ExitCode::from(1);
    }

    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = fs::create_dir_all(parent)
    {
        eprintln!("mkinitrd: mkdir {}: {e}", parent.display());
        return ExitCode::from(1);
    }
    if let Err(e) = fs::write(path, &buf) {
        eprintln!("mkinitrd: write {path}: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn add_extras(buf: &mut [u8], extras: &[(&str, &str)]) -> Result<(), FatError> {
    if extras.is_empty() {
        return Ok(());
    }
    let mut disk = MemDisk::new(buf, SEC as u32)?;
    let mut vol = FatVol::mount(&mut disk)?;
    vol.now = NOW;
    for &(src, dest) in extras {
        let data = fs::read(src).map_err(|_| FatError::Io)?;
        add_file(&mut vol, &mut disk, dest, &data)?;
    }
    vol.sync(&mut disk)
}

fn add_file(vol: &mut FatVol, disk: &mut MemDisk, dest: &str, data: &[u8]) -> Result<(), FatError> {
    let dest = dest.trim_start_matches('/');
    if dest.is_empty() {
        return Err(FatError::Inval);
    }
    let (dir_clu, name) = if let Some((dir, file)) = dest.split_once('/') {
        if file.is_empty() || file.contains('/') {
            return Err(FatError::Inval);
        }
        let dname = dir.as_bytes();
        let dclu = match vol.lookup(disk, vol.info.root_clus, dname) {
            Ok(n) if n.is_dir() => n.clu,
            Ok(_) => return Err(FatError::NotDir),
            Err(FatError::NotFound) => vol.create(disk, vol.info.root_clus, dname, true)?.clu,
            Err(e) => return Err(e),
        };
        (dclu, file.as_bytes())
    } else {
        (vol.info.root_clus, dest.as_bytes())
    };
    let node = vol.create(disk, dir_clu, name, false)?;
    if data.is_empty() {
        return Ok(());
    }
    let mut words = FatInode::of_node(&node);
    vol.write_ino(disk, &mut words, true, 0, false, data)?;
    Ok(())
}
