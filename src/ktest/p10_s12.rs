//! In-guest tests of P10-S12, VFS single dispatch II: File API on Vfs, counted mounts (DESIGN §8.2).

use vibeos::fs::{
    FileRef, FsError, O_CREAT, O_DIRECTORY, O_RDONLY, O_RDWR, O_TRUNC, OpenFlags, SeekFrom,
};

use super::{Outcome, Test, test};
use crate::fat_init;
use crate::file_init;
use crate::fs_init;
use crate::shell_init;
use crate::vibefs_init;

pub(super) const TESTS: &[Test] = &[
    test("vfs_backends_via_ops", test_vfs_backends_via_ops),
    test("vfs_fat_one_inode", test_vfs_fat_one_inode),
];

/// A failed step and its error.
type Step<T> = Result<T, (&'static str, FsError)>;

fn step<T>(what: &'static str, r: Result<T, FsError>) -> Step<T> {
    r.map_err(|e| (what, e))
}

fn outcome(r: Step<()>) -> Outcome {
    match r {
        Ok(()) => Outcome::Ok,
        Err((what, e)) => crate::fail_fmt!("{what}: {}", e.as_str()),
    }
}

/// Read all of `path` from offset 0 into `out`; the count read.
fn read_all(path: &[u8], out: &mut [u8]) -> Result<usize, FsError> {
    let f = file_init::open(path, OpenFlags::from_bits(O_RDONLY), 0)?;
    let mut n = 0usize;
    let r = loop {
        match out.get_mut(n..) {
            Some(rest) if !rest.is_empty() => match file_init::read(&f, rest) {
                Ok(0) => break Ok(n),
                Ok(k) => n = n.saturating_add(k),
                Err(e) => break Err(e),
            },
            _ => break Ok(n),
        }
    };
    file_init::close(f).and(r)
}

/// Whether directory `path` lists `name`.
fn lists(path: &[u8], name: &[u8]) -> Result<bool, FsError> {
    let f = file_init::open(path, OpenFlags::from_bits(O_RDONLY | O_DIRECTORY), 0)?;
    let mut hit = false;
    let r = file_init::readdir(&f, &mut |d| {
        hit = d.name.eq_bytes(name);
        !hit
    });
    file_init::close(f).and(r).map(|()| hit)
}

const LINE: &[u8] = b"vfs backends via ops\n";

fn test_vfs_backends_via_ops() -> Outcome {
    if !fat_init::live() || !vibefs_init::live() {
        return Outcome::Fail("needs FAT and vibefs live");
    }
    let (opens, routed) = file_init::testing::open_counts();
    let r = backends_via_ops();
    let (opens2, routed2) = file_init::testing::open_counts();
    let mut clean = Ok(());
    for p in [&b"/vibe/vo_src"[..], b"/vo_copy", b"/vibe/vo_h"] {
        match file_init::unlink(p) {
            Ok(()) | Err(FsError::NotFound) => {}
            Err(e) => clean = Err(("unlink after", e)),
        }
    }
    if let Err(e) = r.and(clean) {
        return outcome(Err(e));
    }
    // The source write, 2 `cat`, 2 `ls`, 2 `cp` of 2 opens each, 3
    // read-backs and 2 listings.
    let want = 1 + 2 + 2 + 4 + 3 + 2;
    if opens2.wrapping_sub(opens) < want {
        return crate::fail_fmt!(
            "Vfs::open ran {} times, want >= {want}",
            opens2.wrapping_sub(opens)
        );
    }
    if routed2 != routed {
        return crate::fail_fmt!("{} routed opens, want 0", routed2.wrapping_sub(routed));
    }
    Outcome::Ok
}

fn backends_via_ops() -> Step<()> {
    let flags = OpenFlags::from_bits(O_RDWR | O_CREAT | O_TRUNC);
    let f = step("open src", file_init::open(b"/vibe/vo_src", flags, 0o644))?;
    let w = file_init::write(&f, LINE);
    step("close src", file_init::close(f))?;
    if step("write src", w)? != LINE.len() {
        return Err(("short write", FsError::Io));
    }
    let hello = match file_init::stat_path(b"/hello.txt") {
        Ok(_) => "/hello.txt",
        Err(_) => "/HELLO.TXT",
    };
    let lines = [
        "cat /vibe/vo_src",
        if hello == "/hello.txt" {
            "cat /hello.txt"
        } else {
            "cat /HELLO.TXT"
        },
        "ls /",
        "ls /vibe",
        "cp /vibe/vo_src /vo_copy",
        if hello == "/hello.txt" {
            "cp /hello.txt /vibe/vo_h"
        } else {
            "cp /HELLO.TXT /vibe/vo_h"
        },
    ];
    for line in lines {
        if shell_init::dispatch_line(line).is_err() {
            return Err(("shell line", FsError::Inval));
        }
    }
    let mut a = [0u8; 64];
    let n = step("read /vo_copy", read_all(b"/vo_copy", &mut a))?;
    if &a[..n] != LINE {
        return Err(("/vo_copy bytes", FsError::Io));
    }
    let mut want = [0u8; 512];
    let mut got = [0u8; 512];
    let wn = step("read hello", read_all(hello.as_bytes(), &mut want))?;
    let gn = step("read /vibe/vo_h", read_all(b"/vibe/vo_h", &mut got))?;
    if wn == 0 || want[..wn] != got[..gn] {
        return Err(("/vibe/vo_h bytes", FsError::Io));
    }
    if !step("readdir /", lists(b"/", b"vo_copy"))? {
        return Err(("/ lists vo_copy", FsError::NotFound));
    }
    if !step("readdir /vibe", lists(b"/vibe", b"vo_h"))? {
        return Err(("/vibe lists vo_h", FsError::NotFound));
    }
    Ok(())
}

const ONE: &[u8] = b"/vo_one";

fn test_vfs_fat_one_inode() -> Outcome {
    if !fat_init::live() {
        return Outcome::Fail("no FAT initrd");
    }
    let r = fat_one_inode();
    let u = file_init::unlink(ONE);
    outcome(r.and(step("unlink", u)))
}

fn fat_one_inode() -> Step<()> {
    step("creat", file_init::creat(ONE))?;
    let rw = OpenFlags::from_bits(O_RDWR);
    let a = step("open a", file_init::open(ONE, rw, 0))?;
    let b = match file_init::open(ONE, rw, 0) {
        Ok(b) => b,
        Err(e) => {
            let _ = file_init::close(a);
            return Err(("open b", e));
        }
    };
    let r = two_descriptors(&a, &b);
    let ca = file_init::close(a);
    let cb = file_init::close(b);
    r?;
    step("close a", ca)?;
    step("close b", cb)
}

fn two_descriptors(a: &FileRef, b: &FileRef) -> Step<()> {
    let mut data = [0u8; 5000];
    for (i, d) in data.iter_mut().enumerate() {
        *d = (i % 253) as u8;
    }
    let mut n = 0usize;
    while n < data.len() {
        match step("write", file_init::write(a, &data[n..]))? {
            0 => return Err(("write made no progress", FsError::Io)),
            k => n += k,
        }
    }
    let sa = step("stat a", file_init::stat(a))?;
    let sb = step("stat b", file_init::stat(b))?;
    if sa.ino != sb.ino {
        return Err(("two st_ino", FsError::Io));
    }
    if (sa.size, sb.size) != (5000, 5000) {
        return Err(("size is not 5,000 on both", FsError::Io));
    }
    step("seek b", file_init::seek(b, SeekFrom::Start(0)))?;
    let mut back = [0u8; 5000];
    let mut m = 0usize;
    while m < back.len() {
        match step("read b", file_init::read(b, &mut back[m..]))? {
            0 => break,
            k => m += k,
        }
    }
    if m != back.len() || back != data {
        return Err(("read back through b", FsError::Io));
    }
    let (sbk, key) = step("inode of a", fs_init::with(|v| v.file_inode(a.id())))?;
    if fs_init::with(|v| v.inodes_with_key(sbk, key)) != 1 {
        return Err(("Vfs holds more than one inode", FsError::Io));
    }
    Ok(())
}

/// The File API with the copyable [`FileId`] handles the earlier suites
/// were written against: each call takes back, or hands out, the count a
/// [`FileRef`] carries, so their scenarios and assertions stay as they
/// were.
pub(crate) mod fid {
    use vibeos::fs::{FileId, FileRef, FsError, OpenFlags, SeekFrom, Stat};

    use crate::file_init;

    pub(crate) fn open(path: &str, flags: u32, mode: u32) -> Result<FileId, FsError> {
        file_init::open(path.as_bytes(), OpenFlags::from_bits(flags), mode).map(FileRef::into_raw)
    }

    pub(crate) fn read(id: FileId, buf: &mut [u8]) -> Result<usize, FsError> {
        file_init::read(&FileRef::from_raw(id), buf)
    }

    pub(crate) fn write(id: FileId, buf: &[u8]) -> Result<usize, FsError> {
        file_init::write(&FileRef::from_raw(id), buf)
    }

    pub(crate) fn seek(id: FileId, off: i64, whence: u32) -> Result<u64, FsError> {
        let pos = SeekFrom::from_whence(off, whence)?;
        file_init::seek(&FileRef::from_raw(id), pos)
    }

    pub(crate) fn close(id: FileId) -> Result<(), FsError> {
        file_init::close(FileRef::from_raw(id))
    }

    pub(crate) fn addref(id: FileId) -> Result<(), FsError> {
        file_init::addref(id)
    }

    pub(crate) fn stat_path(path: &str) -> Result<Stat, FsError> {
        file_init::stat_path(path.as_bytes())
    }

    /// `lstat` of absolute `path`.
    pub(crate) fn lstat_path(path: &str) -> Result<Stat, FsError> {
        crate::fs_init::api().stat_path(None, path.as_bytes(), false)
    }

    pub(crate) fn creat(path: &str) -> Result<(), FsError> {
        file_init::creat(path.as_bytes())
    }

    pub(crate) fn unlink_path(path: &str, rmdir: bool) -> Result<(), FsError> {
        if rmdir {
            file_init::rmdir(path.as_bytes())
        } else {
            file_init::unlink(path.as_bytes())
        }
    }
}
