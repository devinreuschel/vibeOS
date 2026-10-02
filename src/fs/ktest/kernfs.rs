//! kernfs in-guest (kernel_tests only): its node table grows past the 128
//! nodes it held as a fixed pool and reuses freed nodes, and a full `/tmp`
//! leaves `/dev`, `/proc` and `/sys` working.

use vibeos::fs::{FsError, InodeKind, O_RDWR};

use crate::fs_init;
use crate::ktest::{Outcome, fid};

/// `/tmp` names [`test_kernfs_nodes_grow`] makes: with the boot's `/dev`,
/// `/proc`, `/sys` and `/tmp` nodes, more than the 128 the node table held
/// when it was a fixed pool.
const KERNFS_GROW_NAMES: usize = 160;

/// `/tmp/kn<i>`, for `i` below [`KERNFS_GROW_NAMES`].
fn kn_path(i: usize, buf: &mut [u8; 16]) -> &str {
    let mut d = [0u8; 20];
    let digits = vibeos::fmt_util::write_dec(i as u64, &mut d);
    let n = 7 + digits.len();
    buf[..7].copy_from_slice(b"/tmp/kn");
    buf[7..n].copy_from_slice(digits);
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}

/// Make, then unlink, [`KERNFS_GROW_NAMES`] `/tmp` names, twice.
fn kernfs_grow_round(round: usize) -> Result<(), &'static str> {
    let mut buf = [0u8; 16];
    for i in 0..KERNFS_GROW_NAMES {
        if fid::creat(kn_path(i, &mut buf)).is_err() {
            return Err(if round == 0 { "creat" } else { "creat again" });
        }
    }
    let (used, _) = fs_init::KERNFS.node_counts();
    if used <= 128 {
        return Err("not past the old pool");
    }
    for i in 0..KERNFS_GROW_NAMES {
        match fid::stat_path(kn_path(i, &mut buf)) {
            Ok(s) if s.kind == InodeKind::Reg => {}
            _ => return Err("stat"),
        }
    }
    let Ok(z) = fid::open("/dev/zero", O_RDWR, 0) else {
        return Err("open zero");
    };
    let mut b = [0xFFu8; 4];
    let zero = fid::read(z, &mut b).ok() == Some(4) && b == [0u8; 4];
    let _ = fid::close(z);
    if !zero {
        return Err("read zero");
    }
    let Ok(c) = fid::open("/proc/1/cmdline", O_RDWR, 0) else {
        return Err("open cmdline");
    };
    let cmdline = matches!(fid::read(c, &mut b), Ok(1..));
    let _ = fid::close(c);
    if !cmdline {
        return Err("read cmdline");
    }
    for i in 0..KERNFS_GROW_NAMES {
        if fid::unlink_path(kn_path(i, &mut buf), false).is_err() {
            return Err("unlink");
        }
    }
    Ok(())
}

/// kernfs's node table grows past the 128 nodes it held as a fixed pool,
/// `/dev` and `/proc` work beside the new names, and a second round of
/// names reuses the freed nodes without growing the table again.
pub(crate) fn test_kernfs_nodes_grow() -> Outcome {
    if !fs_init::live() {
        return Outcome::Fail("not live");
    }
    if let Err(e) = kernfs_grow_round(0) {
        return Outcome::Fail(e);
    }
    let (_, len) = fs_init::KERNFS.node_counts();
    if let Err(e) = kernfs_grow_round(1) {
        return Outcome::Fail(e);
    }
    if fs_init::KERNFS.node_counts().1 != len {
        return Outcome::Fail("grew on reuse");
    }
    Outcome::Ok
}

/// Names [`test_tmp_full_spares_system_nodes`] lets `/tmp` make before
/// its `nr_inodes`.
const TMP_FULL_NAMES: usize = 8;

/// `/tmp/tf<i>`, for `i` up to [`TMP_FULL_NAMES`].
fn tf_path(i: usize, buf: &mut [u8; 16]) -> &str {
    let mut d = [0u8; 20];
    let digits = vibeos::fmt_util::write_dec(i as u64, &mut d);
    let n = 7 + digits.len();
    buf[..7].copy_from_slice(b"/tmp/tf");
    buf[7..n].copy_from_slice(digits);
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}

/// Fill `/tmp` to its `nr_inodes` and its data backing, each to
/// `ENOSPC`, then check that the kernel's skins still make and open
/// nodes.
fn tmp_full_round() -> Result<(), &'static str> {
    let mut buf = [0u8; 16];
    let Some((have, _)) = fs_init::KERNFS.tmp_nodes() else {
        return Err("no /tmp");
    };
    fs_init::KERNFS.set_tmp_nr_inodes(have + TMP_FULL_NAMES);
    for i in 0..TMP_FULL_NAMES {
        if fid::creat(tf_path(i, &mut buf)).is_err() {
            return Err("creat under nr_inodes");
        }
    }
    if fid::creat(tf_path(TMP_FULL_NAMES, &mut buf)) != Err(FsError::NoSpace) {
        return Err("creat past nr_inodes not ENOSPC");
    }
    // Data: the last name takes pages until the backing is full.
    let Ok(f) = fid::open(tf_path(TMP_FULL_NAMES - 1, &mut buf), O_RDWR, 0) else {
        return Err("open data file");
    };
    let page = [0x5Au8; 4096];
    let mut full = None;
    for _ in 0..64 {
        if let Err(e) = fid::write(f, &page) {
            full = Some(e);
            break;
        }
    }
    let _ = fid::close(f);
    if full != Some(FsError::NoSpace) {
        return Err("data write past the backing not ENOSPC");
    }
    // The kernel's skins are not charged to /tmp.
    if fs_init::KERNFS
        .sysfs_add_device(b"ff:1f.7", 0x1af4, 0x1001, 1, None)
        .is_err()
    {
        return Err("sysfs node");
    }
    match fid::stat_path("/sys/devices/ff:1f.7/vendor") {
        Ok(s) if s.kind == InodeKind::Reg => {}
        _ => return Err("stat sysfs node"),
    }
    let Ok(n) = fid::open("/dev/null", O_RDWR, 0) else {
        return Err("open null");
    };
    let wrote = fid::write(n, b"x").ok() == Some(1);
    let _ = fid::close(n);
    if !wrote {
        return Err("write null");
    }
    let Ok(c) = fid::open("/proc/1/cmdline", O_RDWR, 0) else {
        return Err("open cmdline");
    };
    let mut b = [0u8; 4];
    let read = matches!(fid::read(c, &mut b), Ok(1..));
    let _ = fid::close(c);
    if !read {
        return Err("read cmdline");
    }
    Ok(())
}

/// Issue #195: `/tmp` has its own `nr_inodes`, Linux's default at boot,
/// and a full `/tmp`, of names or of data, is `ENOSPC` to the writer and
/// never stops `/dev`, `/proc` or `/sys`.
pub(crate) fn test_tmp_full_spares_system_nodes() -> Outcome {
    if !fs_init::live() {
        return Outcome::Fail("not live");
    }
    let Some((have, max)) = fs_init::KERNFS.tmp_nodes() else {
        return Outcome::Fail("no /tmp");
    };
    let ram_pages = crate::pmm_init::with_buddy(|b| b.stats().total_frames) as u64;
    if max != vibeos::fs::kernfs::tmp_nr_inodes_default(ram_pages) {
        return crate::fail_fmt!("/tmp nr_inodes {max}, want half of {ram_pages} RAM pages");
    }
    let r = tmp_full_round();
    let mut buf = [0u8; 16];
    for i in 0..=TMP_FULL_NAMES {
        let _ = fid::unlink_path(tf_path(i, &mut buf), false);
    }
    fs_init::KERNFS.set_tmp_nr_inodes(max);
    if let Err(e) = r {
        return Outcome::Fail(e);
    }
    if fs_init::KERNFS.tmp_nodes() != Some((have, max)) {
        return Outcome::Fail("/tmp's nodes not back after unlink");
    }
    Outcome::Ok
}
