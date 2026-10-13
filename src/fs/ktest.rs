//! In-guest tests for fs (kernel_tests only). Rows: [`TESTS`].

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vibeos::dev::Instance;
use vibeos::fs::{
    FileId, FsError, InodeKind, O_APPEND, O_CREAT, O_DIRECTORY, O_EXCL, O_RDONLY, O_RDWR, O_TRUNC,
    O_WRONLY, OpenFlags, SEEK_CUR, SEEK_END, SEEK_SET, Stat,
};
use vibeos::kalloc::TryVec;
use vibeos::lock::RANK_DEVICE;
use vibeos::proc::wait_exited;

mod churn;
mod cwd;
mod eio;
mod hooks;
mod initrd;
#[cfg(feature = "irqoff")]
mod irqoff;
mod kernfs;
mod lock;
mod ops;
mod routing;
mod shell;
mod slots;
mod stack16k;
mod times;
mod umount;
mod vdap1;
mod walk;

use churn::test_file_table_fork_churn;
pub(crate) use cwd::test_cwd_per_process;
pub(crate) use eio::fat_bad_sector_eio;
use hooks::{link_path, symlink_path, truncate_path};
pub(crate) use initrd::test_initrd_module_sized;
pub(crate) use kernfs::{test_kernfs_nodes_grow, test_tmp_full_spares_system_nodes};
pub(crate) use lock::{test_fat_vol_wait_no_eio, test_vfs_io_off_lock};
pub(crate) use ops::{
    test_fat_rename_racing_writes, test_vfs_backends_via_ops, test_vfs_fat_one_inode,
};
pub(crate) use routing::{
    test_fat_initrd_dev_no_null, test_vfs_unlink_drops_parent_dentry, test_vfs_user_dev_nodes,
};
pub(crate) use shell::{
    test_shell_fat32_image, test_shell_fs_commands, test_shell_ls_subdir,
    test_shell_mount_same_path_64, test_shell_mount_umount, test_shell_rm_r_tree,
};
pub(crate) use slots::test_fs_drop_slot_waits_for_holder;
pub(crate) use stack16k::{fat_image_to, fat_vda_16k_stack, on_cache_write};
pub(crate) use times::{test_fat_times_wall_clock, test_vibefs_rename_ctime};
pub(crate) use umount::test_umount_consistent;
pub(crate) use vdap1::test_vdap1_vfs_read;
pub(crate) use walk::test_walk_path_resolution;

use crate::fat_init;
use crate::file_init;
use crate::fs_init;
use crate::ktest::user::{self, DEFAULT, Image, x86_user_code};
use crate::ktest::{Outcome, Test, fid, test};
use crate::sync_init::SpinMutex;
use crate::thread_init;
use crate::time_init;
use crate::vibefs_init;

pub(crate) fn test_vfs_walk() -> Outcome {
    if !fs_init::live() {
        return Outcome::Fail("not live");
    }
    if file_init::mkdir(b"/a", 0o755).is_err() {
        return Outcome::Fail("mkdir");
    }
    if file_init::creat(b"/a/f").is_err() {
        return Outcome::Fail("creat");
    }
    match fid::stat_path("/a/./f") {
        Ok(s) if s.kind == InodeKind::Reg => {}
        _ => return Outcome::Fail("dot walk"),
    }
    // `..` after a file is no step out of a directory (path_resolution(7)).
    match fid::stat_path("/a/f/../f") {
        Err(FsError::NotDir) => {}
        _ => return Outcome::Fail("dotdot after a file"),
    }
    match fid::stat_path("/a/../a/f") {
        Ok(s) if s.kind == InodeKind::Reg => {}
        _ => return Outcome::Fail("dotdot"),
    }
    if file_init::mkdir(b"/ram", 0o755).is_err() {
        return Outcome::Fail("ramdir");
    }
    if file_init::mount(b"none", b"/ram", b"ramfs", false).is_err() {
        return Outcome::Fail("mount");
    }
    if file_init::creat(b"/ram/f").is_err() {
        return Outcome::Fail("rf");
    }
    if symlink_path(b"/ram/l", b"/ram/f").is_err() {
        return Outcome::Fail("symlink");
    }
    match fid::stat_path("/ram/l") {
        Ok(s) if s.kind == InodeKind::Reg => {}
        _ => return Outcome::Fail("follow"),
    }
    if symlink_path(b"/ram/loop", b"/ram/loop").is_err() {
        return Outcome::Fail("loopc");
    }
    match fid::stat_path("/ram/loop") {
        Err(FsError::Loop) => {}
        _ => return Outcome::Fail("noloop"),
    }
    match fid::stat_path("/ram/..") {
        Ok(s) if s.kind == InodeKind::Dir => {}
        _ => return Outcome::Fail("cross"),
    }
    // The mount is this run's own, so a repeated run (`vibeos.ktest_repeat`)
    // mounts /ram again.
    if file_init::umount(b"/ram").is_err() {
        return Outcome::Fail("umount");
    }
    Outcome::Ok
}

fn dir_has(path: &str, want: &[u8]) -> bool {
    let flags = vibeos::fs::OpenFlags::from_bits(vibeos::fs::O_RDONLY | vibeos::fs::O_DIRECTORY);
    let Ok(f) = file_init::open(path.as_bytes(), flags, 0) else {
        return false;
    };
    let mut hit = false;
    let r = file_init::readdir(&f, &mut |d| {
        hit = d.name.eq_bytes(want);
        !hit
    });
    let _ = file_init::close(f);
    r.is_ok() && hit
}

/// The file [`test_pseudo_fs`] writes on `/tmp`, and unlinks after.
const TMP_F: &str = "/tmp/f";

/// Make [`TMP_F`], write `ok` to it, and read that back.
fn tmp_round_trip() -> Result<(), &'static str> {
    if fid::creat(TMP_F).is_err() {
        return Err("tmp creat");
    }
    let Ok(t) = fid::open(TMP_F, O_RDWR | O_CREAT, 0o644) else {
        return Err("tmp open");
    };
    let mut buf = [0u8; 4];
    let r = if fid::write(t, b"ok").ok() != Some(2) {
        Err("tmp write")
    } else if fid::seek(t, 0, vibeos::fs::SEEK_SET).is_err() {
        Err("tmp seek")
    } else if fid::read(t, &mut buf).ok() != Some(2) || &buf[..2] != b"ok" {
        Err("tmp read")
    } else {
        Ok(())
    };
    let closed = fid::close(t);
    r?;
    closed.map_err(|_| "tmp close")
}

pub(crate) fn test_pseudo_fs() -> Outcome {
    if !fs_init::live() {
        return Outcome::Fail("not live");
    }
    match fid::stat_path("/dev") {
        Ok(s) if s.kind == InodeKind::Dir => {}
        _ => return Outcome::Fail("/dev"),
    }
    match fid::stat_path("/proc") {
        Ok(s) if s.kind == InodeKind::Dir => {}
        _ => return Outcome::Fail("/proc"),
    }
    match fid::stat_path("/tmp") {
        Ok(s) if s.kind == InodeKind::Dir => {}
        _ => return Outcome::Fail("/tmp"),
    }
    match fid::stat_path("/sys") {
        Ok(s) if s.kind == InodeKind::Dir => {}
        _ => return Outcome::Fail("/sys"),
    }
    if !dir_has("/dev", b"null")
        || !dir_has("/dev", b"zero")
        || !dir_has("/dev", b"random")
        || !dir_has("/dev", b"console")
        || !dir_has("/dev", b"tty")
    {
        return Outcome::Fail("dev chars");
    }
    if !dir_has("/dev", b"ram0") {
        return Outcome::Fail("dev ram0");
    }
    match fid::stat_path("/dev/null") {
        Ok(s) if s.kind == InodeKind::Chr => {}
        _ => return Outcome::Fail("null kind"),
    }
    let Ok(f) = fid::open("/dev/null", O_RDWR, 0) else {
        return Outcome::Fail("open null");
    };
    if fid::write(f, b"x").ok() != Some(1) {
        let _ = fid::close(f);
        return Outcome::Fail("write null");
    }
    let _ = fid::close(f);
    let Ok(z) = fid::open("/dev/zero", O_RDWR, 0) else {
        return Outcome::Fail("open zero");
    };
    let mut buf = [0xFFu8; 4];
    if fid::read(z, &mut buf).ok() != Some(4) || buf != [0u8; 4] {
        let _ = fid::close(z);
        return Outcome::Fail("read zero");
    }
    let _ = fid::close(z);
    let Ok(r) = fid::open("/dev/random", O_RDWR, 0) else {
        return Outcome::Fail("open rand");
    };
    // Hardware bytes only: a short count, or none (ROADMAP §10.12).
    if !matches!(fid::read(r, &mut buf), Ok(1..=4) | Err(FsError::Again)) {
        let _ = fid::close(r);
        return Outcome::Fail("read rand");
    }
    let _ = fid::close(r);
    let tmp = tmp_round_trip();
    let unlinked = unlink_quiet(TMP_F);
    if let Err(why) = tmp {
        return Outcome::Fail(why);
    }
    if unlinked.is_err() {
        return Outcome::Fail("tmp unlink");
    }
    if !dir_has("/proc", b"1") || !dir_has("/proc", b"self") {
        return Outcome::Fail("proc stubs");
    }
    if !dir_has("/proc/1", b"cmdline")
        || !dir_has("/proc/1", b"status")
        || !dir_has("/proc/1", b"maps")
        || !dir_has("/proc/1", b"fd")
    {
        return Outcome::Fail("proc/1");
    }
    let Ok(c) = fid::open("/proc/1/cmdline", O_RDWR, 0) else {
        return Outcome::Fail("cmdline");
    };
    buf = [0u8; 4];
    match fid::read(c, &mut buf) {
        Ok(n) if n > 0 => {}
        _ => {
            let _ = fid::close(c);
            return Outcome::Fail("cmdline read");
        }
    }
    let _ = fid::close(c);
    if !dir_has("/sys", b"devices") || !dir_has("/sys", b"bus") {
        return Outcome::Fail("sys skeleton");
    }
    Outcome::Ok
}

pub(crate) fn test_fat_initrd() -> Outcome {
    if !fat_init::live() {
        return Outcome::Fail("not live");
    }
    if with_root_fat(|_| Ok(())).is_err() {
        return Outcome::Fail("no root volume");
    }
    match fid::stat_path("/hello.txt") {
        Ok(s) if s.kind == InodeKind::Reg && s.size > 0 => {}
        Ok(_) => return Outcome::Fail("hello meta"),
        Err(_) => match fid::stat_path("/HELLO.TXT") {
            Ok(s) if s.kind == InodeKind::Reg && s.size > 0 => {}
            _ => return Outcome::Fail("hello"),
        },
    }
    if file_init::mkdir(b"/kt", 0o755).is_err() {
        return Outcome::Fail("mkdir");
    }
    match fid::open("/kt/w.txt", O_RDWR | O_CREAT, 0o644) {
        Ok(fid) => {
            if fid::write(fid, b"abc").ok() != Some(3) {
                let _ = fid::close(fid);
                return Outcome::Fail("write");
            }
            if fid::seek(fid, 0, vibeos::fs::SEEK_SET).is_err() {
                let _ = fid::close(fid);
                return Outcome::Fail("seek");
            }
            let mut buf = [0u8; 4];
            match fid::read(fid, &mut buf) {
                Ok(3) if &buf[..3] == b"abc" => {}
                _ => {
                    let _ = fid::close(fid);
                    return Outcome::Fail("read");
                }
            }
            let _ = fid::close(fid);
        }
        Err(_) => return Outcome::Fail("open"),
    }
    if truncate_path(b"/kt/w.txt", 1).is_err() {
        return Outcome::Fail("trunc");
    }
    if fid::unlink_path("/kt/w.txt", false).is_err() {
        return Outcome::Fail("unlink");
    }
    if symlink_path(b"/s", b"/kt").err() != Some(FsError::Perm) {
        return Outcome::Fail("symlink supp");
    }
    if link_path(b"/hello.txt", b"/h2").err() != Some(FsError::Perm) {
        return Outcome::Fail("link supp");
    }
    if file_init::sync_fs().is_err() {
        return Outcome::Fail("sync");
    }
    Outcome::Ok
}

pub(crate) fn test_vibefs() -> Outcome {
    if !vibefs_init::live() {
        return Outcome::Fail("not live");
    }
    if with_vibe_mem(|_| Ok(())).is_err() {
        return Outcome::Fail("no /vibe volume");
    }
    match fid::stat_path("/vibe") {
        Ok(s) if s.kind == InodeKind::Dir => {}
        _ => return Outcome::Fail("mount"),
    }
    if file_init::mkdir(b"/vibe/d", 0o755).is_err() {
        return Outcome::Fail("mkdir");
    }
    match fid::open("/vibe/d/f", O_RDWR | O_CREAT, 0o644) {
        Ok(fid) => {
            if fid::write(fid, b"hello").ok() != Some(5) {
                let _ = fid::close(fid);
                return Outcome::Fail("write");
            }
            if fid::seek(fid, 0, vibeos::fs::SEEK_SET).is_err() {
                let _ = fid::close(fid);
                return Outcome::Fail("seek");
            }
            let mut buf = [0u8; 8];
            match fid::read(fid, &mut buf) {
                Ok(5) if &buf[..5] == b"hello" => {}
                _ => {
                    let _ = fid::close(fid);
                    return Outcome::Fail("read");
                }
            }
            let _ = fid::close(fid);
        }
        Err(_) => return Outcome::Fail("open"),
    }
    match fid::stat_path("/vibe/d/f") {
        Ok(s) if s.kind == InodeKind::Reg && (s.mode & 0o777) == 0o644 => {}
        _ => return Outcome::Fail("mode"),
    }
    if symlink_path(b"/vibe/l", b"/vibe/d/f").is_err() {
        return Outcome::Fail("symlink");
    }
    match fid::open("/vibe/big", O_RDWR | O_CREAT, 0o644) {
        Ok(fid) => {
            let payload = [b'x'; 200];
            if fid::write(fid, &payload).ok() != Some(200) {
                let _ = fid::close(fid);
                return Outcome::Fail("extent w");
            }
            if fid::seek(fid, 0, vibeos::fs::SEEK_SET).is_err() {
                let _ = fid::close(fid);
                return Outcome::Fail("extent seek");
            }
            let mut out = [0u8; 200];
            match fid::read(fid, &mut out) {
                Ok(200) if out == payload => {}
                _ => {
                    let _ = fid::close(fid);
                    return Outcome::Fail("extent r");
                }
            }
            let _ = fid::close(fid);
        }
        Err(_) => return Outcome::Fail("extent open"),
    }
    let snap = with_vibe_mem(|m| vibefs_init::with_slot(m, |v, d| v.snapshot(d, b"s0")));
    if snap.is_err() {
        return Outcome::Fail("snap");
    }
    if file_init::sync_fs().is_err() {
        return Outcome::Fail("sync");
    }
    Outcome::Ok
}

/// Read up to `out.len()` bytes of `path` from offset 0; the count read.
fn read_all(path: &str, out: &mut [u8]) -> Result<usize, FsError> {
    let fid = fid::open(path, O_RDONLY, 0)?;
    let mut n = 0usize;
    let r = loop {
        let Some(rest) = out.get_mut(n..) else {
            break Ok(n);
        };
        if rest.is_empty() {
            break Ok(n);
        }
        match fid::read(fid, rest) {
            Ok(0) => break Ok(n),
            Ok(k) => n = n.saturating_add(k),
            Err(e) => break Err(e),
        }
    };
    let c = fid::close(fid);
    let n = r?;
    c?;
    Ok(n)
}

/// Unlink `path`; a missing file is not an error.
fn unlink_quiet(path: &str) -> Result<(), FsError> {
    match fid::unlink_path(path, false) {
        Ok(()) | Err(FsError::NotFound) => Ok(()),
        Err(e) => Err(e),
    }
}

fn pack(id: FileId) -> u32 {
    (u32::from(id.fid) << 16) | u32::from(id.r#gen)
}

fn unpack(v: u32) -> FileId {
    FileId {
        fid: (v >> 16) as u16,
        r#gen: v as u16,
    }
}

/// The handle the held write uses; the helper closes it.
static STALE_A: AtomicU32 = AtomicU32::new(0);

/// The handle the helper opened into the freed slot.
static STALE_B: AtomicU32 = AtomicU32::new(0);

static STALE_B_OK: AtomicBool = AtomicBool::new(false);

static STALE_DONE: AtomicBool = AtomicBool::new(false);

/// Waits (at most 10,000 yields) for the held write, closes its file,
/// opens `/f55t.txt` into the freed slot, then releases the write.
fn stale_helper() {
    let mut n = 0u32;
    while !hooks::write_held() && n < 10_000 {
        thread_init::yield_now();
        n += 1;
    }
    if hooks::write_held()
        && fid::close(unpack(STALE_A.load(Ordering::Acquire))).is_ok()
        && let Ok(b) = fid::open("/f55t.txt", O_RDWR | O_CREAT | O_TRUNC, 0)
    {
        STALE_B.store(pack(b), Ordering::Release);
        STALE_B_OK.store(true, Ordering::Release);
    }
    hooks::release_write();
    STALE_DONE.store(true, Ordering::Release);
}

pub(crate) fn test_file_table_stale_writeback_ebadf() -> Outcome {
    let out = stale_writeback();
    let us = unlink_quiet("/f55s.txt");
    let ut = unlink_quiet("/f55t.txt");
    if !matches!(out, Outcome::Ok) {
        return out;
    }
    if us.is_err() || ut.is_err() {
        return Outcome::Fail("unlink after");
    }
    Outcome::Ok
}

fn stale_writeback() -> Outcome {
    const FL: u32 = O_RDWR | O_CREAT | O_TRUNC;
    // A handle whose slot was closed and reused.
    let a = match fid::open("/f55s.txt", FL, 0) {
        Ok(a) => a,
        Err(e) => return crate::fail_fmt!("open s: {}", e.as_str()),
    };
    if let Err(e) = fid::close(a) {
        return crate::fail_fmt!("close s: {}", e.as_str());
    }
    let b = match fid::open("/f55t.txt", FL, 0) {
        Ok(b) => b,
        Err(e) => return crate::fail_fmt!("open t: {}", e.as_str()),
    };
    let stale = [
        ("write", fid::write(a, b"x").err()),
        ("seek", fid::seek(a, 5, SEEK_SET).err()),
        ("addref", fid::addref(a).err()),
        ("close", fid::close(a).err()),
    ];
    let pos = fid::seek(b, 0, SEEK_CUR);
    let cb = fid::close(b);
    if b.fid != a.fid || b.r#gen == a.r#gen {
        return crate::fail_fmt!("slot not reused: {a:?} then {b:?}");
    }
    for (op, e) in stale {
        if e != Some(FsError::Badf) {
            return crate::fail_fmt!("stale {op}: {e:?}, want Badf");
        }
    }
    if pos != Ok(0) {
        return crate::fail_fmt!("reused slot offset {pos:?}, want 0");
    }
    if let Err(e) = cb {
        return crate::fail_fmt!("close t: {}", e.as_str());
    }
    // A write held between its I/O and its write-back while another
    // thread closes its file and opens another into the slot.
    let a = match fid::open("/f55s.txt", FL, 0) {
        Ok(a) => a,
        Err(e) => return crate::fail_fmt!("open s: {}", e.as_str()),
    };
    STALE_A.store(pack(a), Ordering::Release);
    STALE_B_OK.store(false, Ordering::Release);
    STALE_DONE.store(false, Ordering::Release);
    hooks::hold_next_write();
    crate::ktest::spawn_thread("f55-stale", stale_helper);
    let w = fid::write(a, b"x");
    let deadline =
        time_init::now_ns().saturating_add(core::time::Duration::from_secs(1).as_nanos() as u64);
    while !STALE_DONE.load(Ordering::Acquire) && time_init::now_ns() < deadline {
        thread_init::yield_now();
    }
    if !STALE_DONE.load(Ordering::Acquire) {
        return Outcome::Fail("helper did not finish");
    }
    if !STALE_B_OK.load(Ordering::Acquire) {
        let _ = fid::close(a);
        return Outcome::Fail("helper: no held write, or close/open failed");
    }
    let b = unpack(STALE_B.load(Ordering::Acquire));
    let pos = fid::seek(b, 0, SEEK_CUR);
    let cb = fid::close(b);
    if b.fid != a.fid {
        return crate::fail_fmt!("slot not reused: {a:?} then {b:?}");
    }
    if w != Err(FsError::Badf) {
        return crate::fail_fmt!("held write {w:?}, want Badf");
    }
    if pos != Ok(0) {
        return crate::fail_fmt!("other file's offset {pos:?}, want 0");
    }
    if let Err(e) = cb {
        return crate::fail_fmt!("close t: {}", e.as_str());
    }
    Outcome::Ok
}

pub(crate) fn test_open_creat_exists_opens() -> Outcome {
    if unlink_quiet("/f55r.txt").is_err() {
        return Outcome::Fail("unlink before");
    }
    hooks::set_open_race(true);
    let r = fid::open("/f55r.txt", O_RDWR | O_CREAT, 0);
    hooks::set_open_race(false);
    match r {
        Ok(id) => {
            if let Err(e) = fid::close(id) {
                return crate::fail_fmt!("close: {}", e.as_str());
            }
        }
        Err(e) => return crate::fail_fmt!("O_CREAT: {}, want open", e.as_str()),
    }
    if unlink_quiet("/f55r.txt").is_err() {
        return Outcome::Fail("unlink");
    }
    hooks::set_open_race(true);
    let r = fid::open("/f55r.txt", O_RDWR | O_CREAT | O_EXCL, 0);
    hooks::set_open_race(false);
    let excl = match r {
        Err(FsError::Exists) => Outcome::Ok,
        Ok(id) => {
            let _ = fid::close(id);
            Outcome::Fail("O_CREAT|O_EXCL opened, want Exists")
        }
        Err(e) => crate::fail_fmt!("O_CREAT|O_EXCL: {}, want Exists", e.as_str()),
    };
    if unlink_quiet("/f55r.txt").is_err() {
        return Outcome::Fail("unlink after");
    }
    excl
}

/// Close every handle in `ids`, then unlink `path`; the first error.
fn close_unlink(ids: &[Option<FileId>], path: &str) -> Result<(), FsError> {
    let mut r = Ok(());
    for &id in ids.iter().flatten() {
        if let Err(e) = fid::close(id)
            && r.is_ok()
        {
            r = Err(e);
        }
    }
    let u = unlink_quiet(path);
    r.and(u)
}

/// Four opens of `path`: a writer, a second descriptor, an `O_APPEND`
/// one and an `O_TRUNC` one, all see one size.
fn shared_size(path: &str) -> Outcome {
    let mut ids: [Option<FileId>; 4] = [None; 4];
    let out = shared_size_on(path, &mut ids);
    let c = close_unlink(&ids, path);
    if !matches!(out, Outcome::Ok) {
        return out;
    }
    match c {
        Ok(()) => Outcome::Ok,
        Err(e) => crate::fail_fmt!("{path}: close/unlink: {}", e.as_str()),
    }
}

fn shared_size_on(path: &str, ids: &mut [Option<FileId>; 4]) -> Outcome {
    let flags = [
        O_RDWR | O_CREAT | O_TRUNC,
        O_RDWR,
        O_WRONLY | O_APPEND,
        O_RDWR | O_TRUNC,
    ];
    let mut open = |k: usize| -> Result<FileId, Outcome> {
        match fid::open(path, flags[k], 0) {
            Ok(id) => {
                ids[k] = Some(id);
                Ok(id)
            }
            Err(e) => Err(crate::fail_fmt!("{path}: open {k}: {}", e.as_str())),
        }
    };
    let a = match open(0) {
        Ok(id) => id,
        Err(o) => return o,
    };
    let b = match open(1) {
        Ok(id) => id,
        Err(o) => return o,
    };
    if fid::write(a, &[b'a'; 100]) != Ok(100) {
        return crate::fail_fmt!("{path}: write 100");
    }
    match fid::seek(b, 0, SEEK_END) {
        Ok(100) => {}
        r => return crate::fail_fmt!("{path}: second SEEK_END {r:?}, want 100"),
    }
    let c = match open(2) {
        Ok(id) => id,
        Err(o) => return o,
    };
    if fid::write(c, b"Z") != Ok(1) {
        return crate::fail_fmt!("{path}: append write");
    }
    let mut z = [0u8; 1];
    if fid::seek(b, 100, SEEK_SET) != Ok(100) || fid::read(b, &mut z) != Ok(1) {
        return crate::fail_fmt!("{path}: read back byte 100");
    }
    if &z != b"Z" {
        return crate::fail_fmt!("{path}: byte 100 is {:#x}, want Z", z[0]);
    }
    let d = match open(3) {
        Ok(id) => id,
        Err(o) => return o,
    };
    if fid::write(b, b"xyz") != Ok(3) {
        return crate::fail_fmt!("{path}: write after O_TRUNC");
    }
    for (k, id) in [a, b, c, d].into_iter().enumerate() {
        match fid::seek(id, 0, SEEK_END) {
            Ok(104) => {}
            r => return crate::fail_fmt!("{path}: fd {k} SEEK_END {r:?}, want 104"),
        }
    }
    match fid::stat_path(path) {
        Ok(st) if st.size == 104 => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("{path}: stat size {}, want 104", st.size),
        Err(e) => crate::fail_fmt!("{path}: stat: {}", e.as_str()),
    }
}

pub(crate) fn test_inode_size_shared_across_opens() -> Outcome {
    for path in ["/f13s.txt", "/vibe/f13s"] {
        let out = shared_size(path);
        if !matches!(out, Outcome::Ok) {
            return out;
        }
    }
    Outcome::Ok
}

/// `n` bytes on the heap, byte `i` set by `byte`. The registry stack on
/// aarch64 is 16 KiB, and DESIGN §4.5 keeps 4 KiB of it for a hard-IRQ
/// top half, so a multi-KiB buffer in a test that calls into FAT does not
/// fit the rest.
fn filled_buf(n: usize, byte: impl Fn(usize) -> u8) -> Result<TryVec<u8>, FsError> {
    let mut v = TryVec::try_with_capacity(n).map_err(|_| FsError::NoMem)?;
    let mut chunk = [0u8; 256];
    let mut filled = 0usize;
    while filled < n {
        let take = (n - filled).min(chunk.len());
        let mut i = 0usize;
        while i < take {
            chunk[i] = byte(filled + i);
            i += 1;
        }
        v.try_extend_from_slice(&chunk[..take])
            .map_err(|_| FsError::NoMem)?;
        filled += take;
    }
    Ok(v)
}

/// Free bytes on the FAT initrd.
fn fat_free() -> Result<u64, FsError> {
    with_root_fat(fat_init::df).map(|(_, _, free, _)| free)
}

/// Run `f` on the root's FAT volume, the initrd's.
fn with_root_fat<R>(
    f: impl FnOnce(&fat_init::FatVolume) -> Result<R, FsError>,
) -> Result<R, FsError> {
    let v = fat_init::root_volume()?;
    f(v.downcast_ref::<fat_init::FatVolume>()
        .ok_or(FsError::Inval)?)
}

/// Run `f` on the memory vibefs volume at `/vibe`.
pub(super) fn with_vibe_mem<R>(
    f: impl FnOnce(&vibefs_init::VibeVolume) -> Result<R, FsError>,
) -> Result<R, FsError> {
    let v = fs_init::volume_at(b"/vibe")?;
    f(v.downcast_ref::<vibefs_init::VibeVolume>()
        .ok_or(FsError::Inval)?)
}

pub(crate) fn test_fat_unlinked_open_frees_at_close() -> Outcome {
    const PATH: &str = "/f13u.txt";
    const FL: u32 = O_RDWR | O_CREAT | O_TRUNC;
    if unlink_quiet(PATH).is_err() {
        return Outcome::Fail("unlink before");
    }
    let Ok(before) = fat_free() else {
        return Outcome::Fail("df");
    };
    let a = match fid::open(PATH, FL, 0) {
        Ok(a) => a,
        Err(e) => return crate::fail_fmt!("open: {}", e.as_str()),
    };
    let out = unlinked_open(PATH, a, before);
    let c = fid::close(a);
    let u = unlink_quiet(PATH);
    if !matches!(out, Outcome::Ok) {
        return out;
    }
    if let Err(e) = c.and(u) {
        return crate::fail_fmt!("close/unlink: {}", e.as_str());
    }
    match fat_free() {
        Ok(f) if f == before => Outcome::Ok,
        Ok(f) => crate::fail_fmt!("free {f} after the last close, want {before}"),
        Err(e) => crate::fail_fmt!("df: {}", e.as_str()),
    }
}

fn unlinked_open(path: &str, a: FileId, before: u64) -> Outcome {
    let data = match filled_buf(1500, |i| (i % 251) as u8) {
        Ok(d) => d,
        Err(e) => return crate::fail_fmt!("buffer: {}", e.as_str()),
    };
    let mut back = match filled_buf(1500, |_| 0) {
        Ok(d) => d,
        Err(e) => return crate::fail_fmt!("buffer: {}", e.as_str()),
    };
    if fid::write(a, &data) != Ok(data.len()) {
        return Outcome::Fail("write");
    }
    let Ok(held) = fat_free() else {
        return Outcome::Fail("df");
    };
    if held >= before {
        return crate::fail_fmt!("free {held} with the file written, before {before}");
    }
    if let Err(e) = fid::unlink_path(path, false) {
        return crate::fail_fmt!("unlink open file: {}", e.as_str());
    }
    match fat_free() {
        Ok(f) if f == held => {}
        r => return crate::fail_fmt!("free {r:?} after unlink, want {held}"),
    }
    if fid::seek(a, 0, SEEK_SET) != Ok(0) || fid::read(a, &mut back) != Ok(1500) {
        return Outcome::Fail("read back after unlink");
    }
    if *back != *data {
        return Outcome::Fail("data changed after unlink");
    }
    // A new file of the same name is another inode.
    let n = match fid::open(path, O_RDWR | O_CREAT | O_TRUNC, 0) {
        Ok(n) => n,
        Err(e) => return crate::fail_fmt!("open new: {}", e.as_str()),
    };
    let w = fid::write(n, b"new");
    let new_end = fid::seek(n, 0, SEEK_END);
    let old_end = fid::seek(a, 0, SEEK_END);
    let cn = fid::close(n);
    if w != Ok(3) || new_end != Ok(3) {
        return crate::fail_fmt!("new file: write {w:?}, size {new_end:?}");
    }
    if old_end != Ok(1500) {
        return crate::fail_fmt!("unlinked file size {old_end:?}, want 1500");
    }
    if let Err(e) = cn {
        return crate::fail_fmt!("close new: {}", e.as_str());
    }
    if let Err(e) = unlink_quiet(path) {
        return crate::fail_fmt!("unlink new: {}", e.as_str());
    }
    match fat_free() {
        Ok(f) if f == held => Outcome::Ok,
        r => crate::fail_fmt!("free {r:?} while the unlinked file is open, want {held}"),
    }
}

// On /vibe/efbig (O_RDWR|O_CREAT|O_TRUNC): lseek to 2^44 - 4096 returns
// it (exit 2 if not), write 1 byte there returns -EFBIG (3), lseek to
// 2^44 returns -EINVAL (4), and SEEK_END returns 0 (5). Exit 1 if the
// open fails, 0 when every step passes.
x86_user_code!(
    VIBEFS_EFBIG,
    "
    lea rdi, [rip + 90f]
    mov esi, 0x242
    xor edx, edx
    mov eax, 2
    syscall
    mov edi, 1
    test rax, rax
    js 80f
    mov r12, rax
    mov rdi, r12
    mov rsi, 0xFFFFFFFF000
    xor edx, edx
    mov eax, 8
    syscall
    mov edi, 2
    mov rcx, 0xFFFFFFFF000
    cmp rax, rcx
    jne 80f
    mov rdi, r12
    lea rsi, [rip + 91f]
    mov edx, 1
    mov eax, 1
    syscall
    mov edi, 3
    cmp rax, -27
    jne 80f
    mov rdi, r12
    mov rsi, 0x100000000000
    xor edx, edx
    mov eax, 8
    syscall
    mov edi, 4
    cmp rax, -22
    jne 80f
    mov rdi, r12
    xor esi, esi
    mov edx, 2
    mov eax, 8
    syscall
    mov edi, 5
    test rax, rax
    jnz 80f
    xor edi, edi
80:
    mov eax, 60
    syscall
    ud2
90:
    .asciz \"/vibe/efbig\"
91:
    .ascii \"x\"
    "
);

pub(crate) fn test_vibefs_efbig() -> Outcome {
    let st = user::run(&Image::Code(VIBEFS_EFBIG, DEFAULT), &["efbig"]);
    let u = unlink_quiet("/vibe/efbig");
    let st = match st {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if st != wait_exited(0) {
        return crate::fail_fmt!("status {st:#x}, want exited 0 (step {})", st >> 8);
    }
    match u {
        Ok(()) => Outcome::Ok,
        Err(e) => crate::fail_fmt!("unlink: {}", e.as_str()),
    }
}

// On /vibe/big5 (O_RDWR|O_CREAT|O_TRUNC): lseek to 5 GiB, write "x",
// SEEK_END returns 5 GiB + 1 (exit 3 if not), and the byte read back at
// 5 GiB is "x" (4). Exit 1 if the open fails, 2 if the lseek or write
// fails, 0 when every step passes.
x86_user_code!(
    VIBEFS_BIG5,
    "
    lea rdi, [rip + 90f]
    mov esi, 0x242
    xor edx, edx
    mov eax, 2
    syscall
    mov edi, 1
    test rax, rax
    js 80f
    mov r12, rax
    mov r13, 0x140000000
    mov rdi, r12
    mov rsi, r13
    xor edx, edx
    mov eax, 8
    syscall
    mov edi, 2
    cmp rax, r13
    jne 80f
    mov rdi, r12
    lea rsi, [rip + 91f]
    mov edx, 1
    mov eax, 1
    syscall
    mov edi, 2
    cmp rax, 1
    jne 80f
    mov rdi, r12
    xor esi, esi
    mov edx, 2
    mov eax, 8
    syscall
    mov edi, 3
    lea rcx, [r13 + 1]
    cmp rax, rcx
    jne 80f
    mov rdi, r12
    mov rsi, r13
    xor edx, edx
    mov eax, 8
    syscall
    mov edi, 4
    cmp rax, r13
    jne 80f
    sub rsp, 16
    mov byte ptr [rsp], 0
    mov rdi, r12
    mov rsi, rsp
    mov edx, 1
    xor eax, eax
    syscall
    mov edi, 4
    cmp rax, 1
    jne 80f
    cmp byte ptr [rsp], 0x78
    jne 80f
    xor edi, edi
80:
    mov eax, 60
    syscall
    ud2
90:
    .asciz \"/vibe/big5\"
91:
    .ascii \"x\"
    "
);

pub(crate) fn test_vibefs_seek_end_5gib() -> Outcome {
    const BIG: u64 = (5 << 30) + 1;
    let st = user::run(&Image::Code(VIBEFS_BIG5, DEFAULT), &["big5"]);
    let size = fid::stat_path("/vibe/big5").map(|s| s.size);
    let u = unlink_quiet("/vibe/big5");
    let st = match st {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if st != wait_exited(0) {
        return crate::fail_fmt!("status {st:#x}, want exited 0 (step {})", st >> 8);
    }
    match size {
        Ok(BIG) => {}
        Ok(n) => return crate::fail_fmt!("stat size {n}, want {BIG}"),
        Err(e) => return crate::fail_fmt!("stat: {}", e.as_str()),
    }
    match u {
        Ok(()) => Outcome::Ok,
        Err(e) => crate::fail_fmt!("unlink: {}", e.as_str()),
    }
}

/// A failed step and its error.
type Step<T> = Result<T, (&'static str, FsError)>;

/// Run one File API call on `Vfs`, naming it for the failure line.
fn vfs<T>(what: &'static str, r: Result<T, FsError>) -> Step<T> {
    r.map_err(|e| (what, e))
}

fn vstat(path: &str) -> Result<Stat, FsError> {
    fid::stat_path(path)
}

fn failed(r: Step<()>) -> Outcome {
    match r {
        Ok(()) => Outcome::Ok,
        Err((what, e)) => crate::fail_fmt!("{what}: {}", e.as_str()),
    }
}

/// How often `dir` lists `.`, `..` and `named` through `Vfs`.
fn count_names(dir: &str, named: &[u8]) -> Step<(u32, u32, u32)> {
    let flags = OpenFlags::from_bits(O_RDONLY | O_DIRECTORY);
    let f = vfs("open dir", file_init::open(dir.as_bytes(), flags, 0))?;
    let (mut dot, mut dotdot, mut hit) = (0u32, 0u32, 0u32);
    let r = file_init::readdir(&f, &mut |d| {
        let n = d.name.as_bytes();
        if n == b"." {
            dot += 1;
        } else if n == b".." {
            dotdot += 1;
        } else if n.eq_ignore_ascii_case(named) {
            hit += 1;
        }
        true
    });
    let c = file_init::close(f);
    vfs("readdir", r)?;
    vfs("close dir", c)?;
    Ok((dot, dotdot, hit))
}

fn mkdir_s11() -> Step<()> {
    vfs("mkdir /s11", file_init::mkdir(b"/s11", 0o755))
}

/// Free bytes on the initrd volume, read outside the VFS lock.
fn initrd_free() -> Step<u64> {
    with_root_fat(fat_init::df)
        .map(|(_, _, free, _)| free)
        .map_err(|e| ("df", e))
}

pub(crate) fn test_vfs_fat_ops_initrd() -> Outcome {
    if !fat_init::live() {
        return Outcome::Skip("no FAT initrd");
    }
    failed(fat_ops_initrd())
}

fn fat_ops_initrd() -> Step<()> {
    let name = match fid::stat_path("/hello.txt") {
        Ok(_) => "/hello.txt",
        Err(_) => "/HELLO.TXT",
    };
    let api = fid::stat_path(name).map_err(|e| ("file api stat", e))?;
    let vs = vstat(name).map_err(|e| ("vfs stat", e))?;
    if (vs.size, vs.ino, vs.kind) != (api.size, api.ino, InodeKind::Reg) {
        return Err(("hello.txt size or ino", FsError::Io));
    }
    if count_names("/", b"hello.txt")? != (1, 1, 1) {
        return Err(("readdir / names", FsError::Io));
    }
    mkdir_s11()?;
    vfs("creat", fid::creat("/s11/f"))?;
    let one = vfs("open 1", fid::open("/s11/f", O_RDWR, 0))?;
    let two = vfs("open 2", fid::open("/s11/f", O_RDWR, 0))?;
    let mut data = [0u8; 300];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(13);
    }
    let mut back = [0u8; 300];
    let wrote = vfs("write", fid::write(one, &data));
    let read = vfs("read", fid::read(two, &mut back));
    let size = vstat("/s11/f").map(|s| s.size).map_err(|e| ("stat", e));
    let c1 = vfs("close 1", fid::close(one));
    let c2 = vfs("close 2", fid::close(two));
    if wrote? != 300 || read? != 300 || back != data {
        return Err(("300-byte round trip", FsError::Io));
    }
    if size? != 300 {
        return Err(("size after write", FsError::Io));
    }
    c1?;
    c2?;
    vfs("unlink", fid::unlink_path("/s11/f", false))?;
    match (vstat("/s11/f"), fid::stat_path("/s11/f")) {
        (Err(FsError::NotFound), Err(FsError::NotFound)) => Ok(()),
        _ => Err(("unlinked file still found", FsError::Io)),
    }
}

pub(crate) fn test_vfs_fat_unlinked_open_inode() -> Outcome {
    if !fat_init::live() {
        return Outcome::Skip("no FAT initrd");
    }
    failed(fat_unlinked_open_inode())
}

fn fat_unlinked_open_inode() -> Step<()> {
    mkdir_s11()?;
    let base = initrd_free()?;
    let old = filled_buf(1500, |_| 0x5A).map_err(|e| ("buffer", e))?;
    let f = vfs("open", fid::open("/s11/u", O_RDWR | O_CREAT, 0o644))?;
    let r = unlinked_open_body(f, &old);
    let c = vfs("close", fid::close(f));
    r?;
    c?;
    let after = initrd_free()?;
    vfs("cleanup unlink", fid::unlink_path("/s11/u", false))?;
    if after != base {
        return Err(("free bytes after close differ from baseline", FsError::Io));
    }
    Ok(())
}

fn unlinked_open_body(f: FileId, old: &[u8]) -> Step<()> {
    if vfs("write", fid::write(f, old))? != old.len() {
        return Err(("short write", FsError::Io));
    }
    vfs("unlink", fid::unlink_path("/s11/u", false))?;
    vfs("creat again", fid::creat("/s11/u"))?;
    if vstat("/s11/u").map_err(|e| ("stat new", e))?.size != 0 {
        return Err(("new file not empty", FsError::Io));
    }
    vfs("seek", fid::seek(f, 0, SEEK_SET))?;
    let mut back = filled_buf(old.len(), |_| 0).map_err(|e| ("buffer", e))?;
    if vfs("read old", fid::read(f, &mut back))? != old.len() || *back != *old {
        return Err(("old bytes", FsError::Io));
    }
    Ok(())
}

pub(crate) fn test_vfs_fat_file_api_one_inode() -> Outcome {
    if !fat_init::live() {
        return Outcome::Skip("no FAT initrd");
    }
    failed(fat_file_api_one_inode())
}

fn fat_file_api_one_inode() -> Step<()> {
    mkdir_s11()?;
    let f = fid::open("/s11/g", O_RDWR | O_CREAT, 0o644).map_err(|e| ("file api open", e))?;
    let data = filled_buf(5000, |_| 0xC3).map_err(|e| ("buffer", e))?;
    let wrote = fid::write(f, &data).map_err(|e| ("file api write", e));
    let vs = vstat("/s11/g").map_err(|e| ("vfs stat", e));
    let closed = fid::close(f).map_err(|e| ("file api close", e));
    let api = fid::stat_path("/s11/g").map_err(|e| ("file api stat", e));
    let gone = fid::unlink_path("/s11/g", false).map_err(|e| ("file api unlink", e));
    let (wrote, vs, api) = (wrote?, vs?, api?);
    closed?;
    gone?;
    if wrote != 5000 || vs.size != 5000 || api.size != 5000 {
        return Err(("size is not 5000 on both paths", FsError::Io));
    }
    if vs.ino != api.ino {
        return Err(("st_ino differs between paths", FsError::Io));
    }
    Ok(())
}

pub(crate) fn test_vfs_vibe_ops_mem() -> Outcome {
    if !vibefs_init::live() {
        return Outcome::Skip("no vibefs");
    }
    failed(vibe_ops_mem())
}

fn vibe_ops_mem() -> Step<()> {
    vfs("mkdir", file_init::mkdir(b"/vibe/s11", 0o755))?;
    vfs("creat", fid::creat("/vibe/s11/f"))?;
    let f = vfs("open", fid::open("/vibe/s11/f", O_RDWR, 0))?;
    let mut back = [0u8; 8];
    let wrote = vfs("write", fid::write(f, b"vibe ops"));
    let sought = vfs("seek", fid::seek(f, 0, SEEK_SET));
    let read = vfs("read", fid::read(f, &mut back));
    let closed = vfs("close", fid::close(f));
    if wrote? != 8 || read? != 8 || &back != b"vibe ops" {
        return Err(("round trip", FsError::Io));
    }
    sought?;
    closed?;
    if vstat("/vibe/s11/f").map_err(|e| ("stat", e))?.size != 8 {
        return Err(("size after write", FsError::Io));
    }
    if count_names("/vibe/s11", b"f")? != (1, 1, 1) {
        return Err(("readdir names", FsError::Io));
    }
    symlink_path(b"/vibe/s11/l", b"/vibe/s11/f").map_err(|e| ("symlink", e))?;
    let l = vstat("/vibe/s11/l").map_err(|e| ("stat link", e))?;
    let f = vstat("/vibe/s11/f").map_err(|e| ("stat file", e))?;
    let k = vfs("lstat", fid::lstat_path("/vibe/s11/l"))?;
    if l.ino != f.ino || l.kind != InodeKind::Reg || k.kind != InodeKind::Lnk {
        return Err(("symlink not followed", FsError::Io));
    }
    Ok(())
}

/// Take `fat_init::INITRD` as a volume read does (`cross_cpu_cells_ranked`).
pub(crate) fn probe_initrd() {
    super::fat_init::with_initrd(|_| ());
}

/// Armed by `vfs_io_off_lock`: the next FAT read op on a device volume
/// waits at [`fat_read_hook`], holding only that volume's lock.
pub(crate) static FAT_READ_HOLD: AtomicBool = AtomicBool::new(false);
/// Set while a FAT read waits at [`fat_read_hook`].
pub(crate) static FAT_READ_HELD: AtomicBool = AtomicBool::new(false);
/// Ends a wait at [`fat_read_hook`].
pub(crate) static FAT_READ_RELEASE: AtomicBool = AtomicBool::new(false);
/// Milliseconds each device-backed FAT block request sleeps first, 0 for
/// none (`fat_vol_wait_no_eio`).
pub(crate) static BLK_DELAY_MS: AtomicU32 = AtomicU32::new(0);
/// Block requests [`blk_request_hook`] has delayed.
pub(crate) static BLK_DELAYED: AtomicU32 = AtomicU32::new(0);

/// `FatOps`' read on a device volume, under the volume lock and with the
/// VFS lock dropped: when [`FAT_READ_HOLD`] is armed, disarm it and wait,
/// at most 10 s, for [`FAT_READ_RELEASE`].
pub(crate) fn fat_read_hook() {
    if !FAT_READ_HOLD.swap(false, Ordering::AcqRel) {
        return;
    }
    FAT_READ_HELD.store(true, Ordering::Release);
    let _released = crate::ktest::sleep_until(|| FAT_READ_RELEASE.load(Ordering::Acquire), 10_000);
    FAT_READ_HELD.store(false, Ordering::Release);
}

/// Armed by `fat_rename_racing_writes`: the next [`fat_rename_hook`]
/// runs that test's writes.
pub(crate) static FAT_RENAME_RACE: AtomicBool = AtomicBool::new(false);

/// `FatOps::rename` after its volume section and before `Vfs` commits
/// the move, with neither lock held: when [`FAT_RENAME_RACE`] is armed,
/// disarm it and write through both names, as a write racing the rename
/// would.
pub(crate) fn fat_rename_hook() {
    if FAT_RENAME_RACE.swap(false, Ordering::AcqRel) {
        ops::rename_window_writes();
    }
}

/// `fat_init`'s `Io::read` and `Io::write` on a block device, under the
/// volume lock: sleep [`BLK_DELAY_MS`] first.
pub(crate) fn blk_request_hook() {
    let ms = BLK_DELAY_MS.load(Ordering::Acquire);
    if ms != 0 {
        BLK_DELAYED.fetch_add(1, Ordering::Relaxed);
        thread_init::sleep_ms(u64::from(ms));
    }
}

/// The `/vibe` memory volume, recorded at its mount for [`probe_image`],
/// which runs with IRQs off and so cannot take the sleeping VFS lock.
pub(crate) static VIBE_MEM: SpinMutex<Option<Instance>> = SpinMutex::with_rank(None, RANK_DEVICE);

/// Take the `/vibe` memory volume's image lock as a volume read does
/// (`cross_cpu_cells_ranked`).
pub(crate) fn probe_image() {
    let v = VIBE_MEM.lock().clone();
    if let Some(i) = v {
        let _ = super::vibefs_init::probe_image_of(&i);
    }
}

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[
    test("vfs_walk", test_vfs_walk),
    test("pseudo_fs", test_pseudo_fs),
    test("kernfs_nodes_grow", test_kernfs_nodes_grow),
    test(
        "tmp_full_spares_system_nodes",
        test_tmp_full_spares_system_nodes,
    ),
    test("fat_initrd", test_fat_initrd),
    test("initrd_module_sized", test_initrd_module_sized),
    test("vibefs", test_vibefs).once(),
    test("file_table_fork_churn", test_file_table_fork_churn),
    test(
        "file_table_stale_writeback_ebadf",
        test_file_table_stale_writeback_ebadf,
    ),
    test("open_creat_exists_opens", test_open_creat_exists_opens),
    test(
        "inode_size_shared_across_opens",
        test_inode_size_shared_across_opens,
    ),
    test(
        "fat_unlinked_open_frees_at_close",
        test_fat_unlinked_open_frees_at_close,
    ),
    test("vibefs_efbig", test_vibefs_efbig),
    test("vibefs_seek_end_5gib", test_vibefs_seek_end_5gib),
    test("vfs_fat_ops_initrd", test_vfs_fat_ops_initrd),
    test(
        "vfs_fat_unlinked_open_inode",
        test_vfs_fat_unlinked_open_inode,
    ),
    test(
        "vfs_fat_file_api_one_inode",
        test_vfs_fat_file_api_one_inode,
    ),
    test("vfs_vibe_ops_mem", test_vfs_vibe_ops_mem).once(),
    test("vfs_backends_via_ops", test_vfs_backends_via_ops),
    test("vfs_fat_one_inode", test_vfs_fat_one_inode),
    test("fat_rename_racing_writes", test_fat_rename_racing_writes),
    test(
        "fs_drop_slot_waits_for_holder",
        test_fs_drop_slot_waits_for_holder,
    ),
    test("fat_vda_16k_stack", fat_vda_16k_stack)
        .deadline(30_000)
        .opt_in()
        .once(),
    test("fat_bad_sector_eio", fat_bad_sector_eio).opt_in(),
    test(
        "vfs_unlink_drops_parent_dentry",
        test_vfs_unlink_drops_parent_dentry,
    ),
    test("vfs_user_dev_nodes", test_vfs_user_dev_nodes),
    test("fat_initrd_dev_no_null", test_fat_initrd_dev_no_null),
    test("vfs_io_off_lock", test_vfs_io_off_lock),
    test("fat_vol_wait_no_eio", test_fat_vol_wait_no_eio).deadline(60_000),
    test("vdap1_vfs_read", test_vdap1_vfs_read),
    test("cwd_per_process", test_cwd_per_process),
    test("walk_path_resolution", test_walk_path_resolution),
    test("umount_consistent", test_umount_consistent).deadline(60_000),
    test("fat_times_wall_clock", test_fat_times_wall_clock),
    test("vibefs_rename_ctime", test_vibefs_rename_ctime),
    test("shell_rm_r_tree", test_shell_rm_r_tree),
    test("shell_ls_subdir", test_shell_ls_subdir),
    test("shell_mount_same_path_64", test_shell_mount_same_path_64),
    test("shell_mount_umount", test_shell_mount_umount),
    test("shell_fs_commands", test_shell_fs_commands),
    test("shell_fat32_image", test_shell_fat32_image),
    #[cfg(feature = "irqoff")]
    test("irqoff_big_tmp_dir", irqoff::irqoff_big_tmp_dir).deadline(30_000),
];
