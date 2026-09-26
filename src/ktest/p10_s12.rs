//! In-guest tests of P10-S12, VFS single dispatch II: File API on Vfs, counted mounts (DESIGN §8.2).

#[allow(
    unused_imports,
    reason = "suite template; the first test here uses them"
)]
use super::{Outcome, Test, test};

pub(super) const TESTS: &[Test] = &[];

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
