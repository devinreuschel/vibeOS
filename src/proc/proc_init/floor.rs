//! The ROADMAP §10.5 floor calls the userland needs beyond the fd and
//! process calls: `getdents64` and `fstat`. Each handler looks up the
//! descriptor, calls the File API, encodes through `vibeos::proc::uabi`,
//! and copies through `uaccess_init` (SYSCALL.md §3.1).

use vibeos::proc::uabi::{self, Dirent64Writer};

use super::*;
use crate::arch::current::UserStat;

/// The most `getdents64` writes in one call: its kernel buffer. A record
/// is at most 88 bytes (`MAX_NAME` 64), so one always fits.
const GETDENTS_MAX: usize = 512;

/// `e` as the syscall's error.
fn fs_err(e: FsError) -> KError {
    KError::from_errno(fs_errno(e))
}

/// Run `f` on open file `id` under a count of this syscall's own.
fn with_file<R>(id: FileId, f: impl FnOnce(&FileRef) -> Result<R, KError>) -> Result<R, KError> {
    let file = file_init::fget(id).map_err(fs_err)?;
    let r = f(&file);
    let c = file_init::close(file);
    let n = r?;
    c.map_err(fs_err)?;
    Ok(n)
}

/// `getdents64(fd, dirent, count)`: whole `linux_dirent64` records from
/// the directory position, at most [`GETDENTS_MAX`] bytes. The position
/// moves only after the copy succeeds.
pub(super) fn sys_getdents64(fd: u32, dirent: u64, count: u32) -> SysResult {
    let Some(slot) = lookup_fd(fd) else {
        return Err(KError::from_errno(EBADF));
    };
    match slot.kind {
        FdKind::None => Err(KError::from_errno(EBADF)),
        FdKind::Console => Err(KError::from_errno(ENOTDIR)),
        FdKind::File { fid, r#gen } => {
            with_file(FileId { fid, r#gen }, |f| getdents_on(f, dirent, count))
        }
    }
}

fn getdents_on(f: &FileRef, dirent: u64, count: u32) -> SysResult {
    let pos = file_init::seek(f, SeekFrom::Current(0)).map_err(fs_err)?;
    let mut buf = [0u8; GETDENTS_MAX];
    let cap = usize::try_from(count).map_or(GETDENTS_MAX, |c| c.min(GETDENTS_MAX));
    let mut refused = false;
    let (n, next) = {
        let mut w = Dirent64Writer::new(&mut buf[..cap]);
        let next = file_init::readdir_from(f, pos, &mut |d, next| {
            let ok = w.push(u64::from(d.ino), next, d.kind, d.name.as_bytes());
            refused = !ok;
            ok
        })
        .map_err(fs_err)?;
        (w.len(), next)
    };
    if n == 0 {
        return if refused {
            Err(KError::from_errno(EINVAL))
        } else {
            Ok(0)
        };
    }
    uaccess_init::copy_to_user(dirent, &buf[..n]).map_err(|_| KError::from_errno(EFAULT))?;
    file_init::seek(f, SeekFrom::Start(next)).map_err(fs_err)?;
    Ok(n)
}

/// `fstat(fd, statbuf)`: the port's `struct stat` for any descriptor; the
/// console reads as a character device.
pub(super) fn sys_fstat(fd: u32, statbuf: u64) -> SysResult {
    let Some(slot) = lookup_fd(fd) else {
        return Err(KError::from_errno(EBADF));
    };
    let fields = match slot.kind {
        FdKind::None => return Err(KError::from_errno(EBADF)),
        FdKind::Console => uabi::console_stat_fields(),
        FdKind::File { fid, r#gen } => {
            let st = with_file(FileId { fid, r#gen }, |f| {
                file_init::stat(f).map_err(fs_err)
            })?;
            uabi::stat_fields(&st)
        }
    };
    let out = UserStat::from_fields(&fields);
    uaccess_init::copy_to_user_val(statbuf, &out).map_err(|_| KError::from_errno(EFAULT))?;
    Ok(0)
}
