//! In-guest tests of the VFS through each backend's ops (kernel_tests
//! only). Rows: the parent `ktest.rs`'s `TESTS`.

use vibeos::fs::{
    FileRef, FsError, O_CREAT, O_DIRECTORY, O_RDONLY, O_RDWR, O_TRUNC, OpenFlags, SeekFrom,
};

use super::hooks;
use crate::fat_init;
use crate::file_init;
use crate::fs_init;
use crate::ktest::Outcome;
use crate::shell_init;
use crate::vibefs_init;

/// A failed step and its error.
type StepS12<T> = Result<T, (&'static str, FsError)>;

fn step<T>(what: &'static str, r: Result<T, FsError>) -> StepS12<T> {
    r.map_err(|e| (what, e))
}

fn outcome(r: StepS12<()>) -> Outcome {
    match r {
        Ok(()) => Outcome::Ok,
        Err((what, e)) => crate::fail_fmt!("{what}: {}", e.as_str()),
    }
}

/// Read all of `path` from offset 0 into `out`; the count read.
fn read_all_s12(path: &[u8], out: &mut [u8]) -> Result<usize, FsError> {
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

pub(crate) fn test_vfs_backends_via_ops() -> Outcome {
    if !fat_init::live() || !vibefs_init::live() {
        return Outcome::Fail("needs FAT and vibefs live");
    }
    let opens = hooks::open_counts();
    let r = backends_via_ops();
    let opens2 = hooks::open_counts();
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
    Outcome::Ok
}

fn backends_via_ops() -> StepS12<()> {
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
    let n = step("read /vo_copy", read_all_s12(b"/vo_copy", &mut a))?;
    if &a[..n] != LINE {
        return Err(("/vo_copy bytes", FsError::Io));
    }
    let mut want = [0u8; 512];
    let mut got = [0u8; 512];
    let wn = step("read hello", read_all_s12(hello.as_bytes(), &mut want))?;
    let gn = step("read /vibe/vo_h", read_all_s12(b"/vibe/vo_h", &mut got))?;
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

pub(crate) fn test_vfs_fat_one_inode() -> Outcome {
    if !fat_init::live() {
        return Outcome::Fail("no FAT initrd");
    }
    let r = fat_one_inode();
    let u = file_init::unlink(ONE);
    outcome(r.and(step("unlink", u)))
}

fn fat_one_inode() -> StepS12<()> {
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

fn two_descriptors(a: &FileRef, b: &FileRef) -> StepS12<()> {
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
