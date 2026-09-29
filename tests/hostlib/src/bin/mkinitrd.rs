//! Host FAT32 initrd. Same `fat::mkinitrd` as the kernel, plus optional files.
//!
//! Reproducible (ROADMAP §10.2, F152): the added files' times come from
//! `SOURCE_DATE_EPOCH` (a fixed time when it is unset), and the files are
//! added in destination order whatever the `--add` order.

#![allow(
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "host tool: `alloc`'s growing calls may panic, and a failed allocation ends this host process, not the kernel (DESIGN §4.4)"
)]

use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use vibeos::fat::{self, FatError, FatInode, FatVol, INITRD_BYTES, MemDisk, SEC};

/// The added files' time when `SOURCE_DATE_EPOCH` is unset, in
/// `FatVol::now`'s unit (seconds since 1980-01-01 UTC).
const NOW: u32 = 1_262_304_000;

/// Unix time of 1980-01-01 00:00 UTC, where `FatVol::now` counts from.
const FAT_EPOCH_UNIX: u64 = 315_532_800;

/// `SOURCE_DATE_EPOCH` (Unix seconds, decimal) in `FatVol::now`'s unit;
/// `NOW` when unset.
fn epoch(raw: Option<&str>) -> Result<u32, String> {
    let Some(raw) = raw else {
        return Ok(NOW);
    };
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "SOURCE_DATE_EPOCH={raw:?} is not a decimal number of seconds"
        ));
    }
    let unix: u64 = raw
        .parse()
        .map_err(|_| format!("SOURCE_DATE_EPOCH={raw} is out of range"))?;
    let secs = unix.checked_sub(FAT_EPOCH_UNIX).ok_or_else(|| {
        format!("SOURCE_DATE_EPOCH={raw} is before 1980, which FAT cannot record")
    })?;
    u32::try_from(secs).map_err(|_| format!("SOURCE_DATE_EPOCH={raw} is out of range"))
}

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
    let raw_epoch = env::var("SOURCE_DATE_EPOCH").ok();
    let now = match epoch(raw_epoch.as_deref()) {
        Ok(now) => now,
        Err(e) => {
            eprintln!("mkinitrd: {e}");
            return ExitCode::from(2);
        }
    };
    let mut files: Vec<(&str, Vec<u8>)> = Vec::new();
    for &(src, dest) in &extras {
        match fs::read(src) {
            Ok(data) => files.push((dest, data)),
            Err(e) => {
                eprintln!("mkinitrd: read {src}: {e}");
                return ExitCode::from(1);
            }
        }
    }

    let buf = match build_image(now, files) {
        Ok(buf) => buf,
        Err(e) => {
            eprintln!("mkinitrd: {}", e.as_str());
            return ExitCode::from(1);
        }
    };

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

/// The initrd image: `fat::mkinitrd`'s, plus `extras` (destination, bytes)
/// added in destination order with their times at `now`.
fn build_image(now: u32, mut extras: Vec<(&str, Vec<u8>)>) -> Result<Vec<u8>, FatError> {
    let mut buf = vec![0u8; INITRD_BYTES];
    fat::mkinitrd(&mut buf)?;
    if extras.is_empty() {
        return Ok(buf);
    }
    extras.sort_by(|a, b| a.0.cmp(b.0));
    let mut disk = MemDisk::new(&mut buf, SEC as u32)?;
    let mut vol = FatVol::mount(&mut disk)?;
    vol.now = now;
    for (dest, data) in &extras {
        add_file(&mut vol, &mut disk, dest, data)?;
    }
    vol.sync(&mut disk)?;
    Ok(buf)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn extras() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("/hello", b"hello".to_vec()),
            ("/sbin/init", vec![0x7f; 700]),
            ("/bin/sh", vec![0x5a; 1300]),
        ]
    }

    #[test]
    fn epoch_unset_is_fixed() {
        assert_eq!(epoch(None), Ok(NOW));
    }

    #[test]
    fn epoch_parses_decimal() {
        assert_eq!(epoch(Some("315532800")), Ok(0));
        assert_eq!(epoch(Some("1577836800")), Ok(1_262_304_000));
    }

    #[test]
    fn epoch_rejects_bad_values() {
        for bad in [
            "",
            "-1",
            "1e9",
            " 1577836800",
            "0x5e0be100",
            "315532799",
            "99999999999999999999",
        ] {
            assert!(epoch(Some(bad)).is_err(), "{bad:?}");
        }
        // Past what a u32 of seconds since 1980 holds.
        assert!(epoch(Some("4610500096")).is_err());
    }

    #[test]
    fn same_epoch_same_image() {
        let a = build_image(1_000_000, extras()).unwrap();
        let b = build_image(1_000_000, extras()).unwrap();
        assert_eq!(a, b);
        let c = build_image(2_000_000, extras()).unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn add_order_does_not_matter() {
        let mut rev = extras();
        rev.reverse();
        assert_eq!(
            build_image(NOW, extras()).unwrap(),
            build_image(NOW, rev).unwrap()
        );
    }
}
