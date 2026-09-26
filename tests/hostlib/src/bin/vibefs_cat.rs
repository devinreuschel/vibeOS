//! `vibefs-cat <image> <path>`: writes one file of a vibefs image to stdout.
//! Same format module as the kernel. The crash harness reads `/w` with it
//! (ROADMAP §10.2, F080). Exits 1 on any error, 2 on bad usage.

use std::env;
use std::io::{self, Write};
use std::process::ExitCode;

use vibeos::vibefs::{self, MemDisk, Vol};

/// The bytes of `path` in the vibefs image `img`.
fn cat(img: &mut [u8], path: &[u8]) -> Result<Vec<u8>, String> {
    let mut disk = MemDisk::new(img).map_err(|e| e.as_str().to_owned())?;
    let mut vol = Vol::new();
    vibefs::mount(&mut disk, &mut vol).map_err(|e| format!("mount: {}", e.as_str()))?;
    let node = vol
        .walk(&mut disk, path)
        .map_err(|e| format!("{}: {}", String::from_utf8_lossy(path), e.as_str()))?;
    let size = usize::try_from(node.size).map_err(|_| "file too big".to_owned())?;
    let mut out = vec![0u8; size];
    let mut off = 0usize;
    while off < size {
        let n = vol
            .read(&mut disk, node.ino, off as u64, &mut out[off..])
            .map_err(|e| format!("read: {}", e.as_str()))?;
        if n == 0 {
            return Err(format!("short read at {off} of {size}"));
        }
        off += n;
    }
    Ok(out)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let [image, path] = args.as_slice() else {
        eprintln!("usage: vibefs-cat <image> <path>");
        return ExitCode::from(2);
    };
    let mut data = match std::fs::read(image) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("vibefs-cat: {image}: {e}");
            return ExitCode::from(1);
        }
    };
    match cat(&mut data, path.as_bytes()) {
        Ok(bytes) => match io::stdout().write_all(&bytes) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("vibefs-cat: {e}");
                ExitCode::from(1)
            }
        },
        Err(e) => {
            eprintln!("vibefs-cat: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibeos::fs::InodeKind;
    use vibeos::vibefs::ROOT_INO;

    #[test]
    fn cat_reads_file_and_rejects_missing() {
        let mut img = vec![0u8; 256 * 1024];
        {
            let mut d = MemDisk::new(&mut img).unwrap();
            let mut v = Vol::new();
            vibefs::mkfs(&mut d, b"t", &mut v).unwrap();
            let mut v = Vol::new();
            vibefs::mount(&mut d, &mut v).unwrap();
            v.create(&mut d, ROOT_INO, b"w", InodeKind::Reg, 0o644, None)
                .unwrap();
            let w = v.lookup(&mut d, ROOT_INO, b"w").unwrap();
            let payload: Vec<u8> = (0..300u32).map(|k| (7 + k) as u8).collect();
            v.write(&mut d, w.ino, 0, &payload).unwrap();
            v.sync(&mut d).unwrap();
        }
        let got = cat(&mut img.clone(), b"/w").unwrap();
        assert_eq!(got.len(), 300);
        assert!(got.iter().enumerate().all(|(k, &b)| b == (7 + k) as u8));
        assert!(cat(&mut img.clone(), b"/missing").is_err());
        assert!(cat(&mut vec![0u8; 256 * 1024], b"/w").is_err());
    }
}
