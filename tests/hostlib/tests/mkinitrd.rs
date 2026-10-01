//! `mkinitrd` sizes the initrd from its contents (ROADMAP §10.5).

#![allow(
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "host test: `alloc`'s growing calls may panic, which ends the test"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use vibeos::fat::{FatInode, FatVol, INITRD_FREE_BYTES, MemDisk, Node, SEC};

fn tmp(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("mkinitrd-{}-{name}", std::process::id()))
}

/// Run `mkinitrd` with one `--add` per `(name, len)`, each file's byte `i`
/// being `i % 251`; the image's bytes.
fn build(tag: &str, files: &[(&str, usize)]) -> Vec<u8> {
    let img = tmp(&format!("{tag}.fat"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mkinitrd"));
    cmd.arg(&img).env("SOURCE_DATE_EPOCH", "1577836800");
    for &(name, len) in files {
        let src = tmp(&format!("{tag}-{name}"));
        std::fs::write(&src, body(len)).unwrap();
        cmd.arg("--add").arg(format!("{}:/{name}", src.display()));
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "mkinitrd {tag}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bytes = std::fs::read(&img).unwrap();
    fsck(&img);
    bytes
}

fn body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// `fsck.fat -n` reports the image clean, where it is installed.
fn fsck(img: &Path) {
    match Command::new("fsck.fat").arg("-n").arg(img).output() {
        Ok(o) => assert!(
            o.status.success(),
            "fsck.fat -n {}: {}",
            img.display(),
            String::from_utf8_lossy(&o.stdout)
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("fsck.fat: {e}"),
    }
}

/// Images with no file, a 100 KB file and a 300 KB file: each is a
/// multiple of 512 bytes, grows with its contents, has between
/// `INITRD_FREE_BYTES` and 4 KiB more free, and reads each file back.
#[test]
fn mkinitrd_sizes_from_contents() {
    let cases: [(&str, &[(&str, usize)]); 3] = [
        ("empty", &[]),
        ("100k", &[("a.bin", 100_000)]),
        ("300k", &[("b.bin", 300_000)]),
    ];
    let mut last = 0usize;
    for (tag, files) in cases {
        let mut img = build(tag, files);
        assert!(img.len().is_multiple_of(SEC), "{tag}: {} bytes", img.len());
        assert!(
            img.len() > last,
            "{tag}: {} bytes, not above {last}",
            img.len()
        );
        last = img.len();
        let mut disk = MemDisk::new(&mut img, SEC as u32).unwrap();
        let mut vol = FatVol::mount(&mut disk).unwrap();
        let free = vol.free_bytes();
        assert!(
            (INITRD_FREE_BYTES..=INITRD_FREE_BYTES + 4096).contains(&free),
            "{tag}: {free} bytes free"
        );
        let hello = lookup_path(&mut vol, &mut disk, b"/hello.txt");
        assert_eq!(hello.size, 18, "{tag}: /hello.txt");
        for &(name, len) in files {
            let n = lookup_path(&mut vol, &mut disk, format!("/{name}").as_bytes());
            assert_eq!(n.size as usize, len, "{tag}: /{name}");
            let mut got = vec![0u8; len];
            let got_n = vol
                .read_ino(&mut disk, &FatInode::of_node(&n), 0, &mut got)
                .unwrap();
            assert_eq!(got_n, len, "{tag}: /{name} short read");
            assert!(
                got == body(len),
                "{tag}: /{name} reads back different bytes"
            );
        }
    }
}

/// The node `path` names, one `FatVol::lookup` per component from the
/// root, as `Vfs` resolves a FAT path.
fn lookup_path(vol: &mut FatVol, disk: &mut MemDisk<'_>, path: &[u8]) -> Node {
    let mut n = vol.root();
    for comp in path.split(|&c| c == b'/').filter(|c| !c.is_empty()) {
        n = vol.lookup(disk, n.clu, comp).unwrap();
    }
    n
}
