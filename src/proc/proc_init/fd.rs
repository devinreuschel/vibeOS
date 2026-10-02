use super::*;
use vibeos::fs::InodeKind;

/// The open-file table handle an fd names, if it names a file.
fn file_id(fd: Fd) -> Option<FileId> {
    match fd.kind {
        FdKind::File { fid, r#gen } => Some(FileId { fid, r#gen }),
        FdKind::None | FdKind::Console => None,
    }
}

/// Read through open file `id` under a count of this syscall's own.
fn file_read(id: FileId, buf: &mut [u8]) -> Result<usize, FsError> {
    let f = file_init::fget(id)?;
    let r = file_init::read(&f, buf);
    let c = file_init::close(f);
    let n = r?;
    c?;
    Ok(n)
}

/// Write through open file `id` under a count of this syscall's own.
fn file_write(id: FileId, buf: &[u8]) -> Result<usize, FsError> {
    let f = file_init::fget(id)?;
    let r = file_init::write(&f, buf);
    let c = file_init::close(f);
    let n = r?;
    c?;
    Ok(n)
}

pub(super) fn close_fd_slot(fd: Fd) -> Result<(), FsError> {
    match file_id(fd) {
        Some(id) => file_init::close(FileRef::from_raw(id)),
        None => Ok(()),
    }
}

/// Close `fd` where the call that drops it has no one to report a failed
/// close to (exit, exec's close-on-exec, the fd `dup2` displaces; Linux
/// drops these errors too): a failure is counted in a rate-limited line
/// (DESIGN §2.5).
pub(super) fn close_dropped(fd: Fd, why: &str) {
    if let Err(e) = close_fd_slot(fd) {
        crate::klog_ratelimited!(
            1000,
            vibeos::log::Level::Warn,
            "vibeOS: proc: {} close failed: {}",
            why,
            e.as_str()
        );
    }
}

/// Descriptors one batch moves in or out of a row per table-lock hold, so
/// no copy of a whole row (`limits::MAX_FDS` entries) leaves the table.
const FD_BATCH: usize = 32;

/// Close `pid`'s open descriptors that `pick` chooses, in batches: each
/// batch leaves the row under one table-lock hold and is closed with the
/// lock dropped, where the call that drops them has no one to report a
/// failed close to (`why`: exit, exec's close-on-exec, a failed fork).
pub(super) fn close_where(pid: u32, why: &str, pick: impl Fn(Fd) -> bool) {
    let mut start = 0usize;
    loop {
        let mut out = [Fd::EMPTY; FD_BATCH];
        let Some((n, next)) = with_table(|t| {
            t.get_mut(pid)
                .map(|p| p.fds.take_batch(start, &mut out, &pick))
        }) else {
            return;
        };
        for fd in out.iter().take(n) {
            close_dropped(*fd, why);
        }
        if n < out.len() {
            return;
        }
        start = next;
    }
}

/// Close all of `pid`'s descriptors ([`close_where`]).
pub(super) fn close_all_fds(pid: u32, why: &str) {
    close_where(pid, why, |_| true);
}

/// Take a reference on each open file in `pid`'s row, a copy of its
/// parent's (fork), in batches read under one table-lock hold each. On a
/// failure, the entries that got no reference are cleared and the rest
/// closed, so the row is empty, and false is returned.
pub(super) fn addref_fds(pid: u32) -> bool {
    let mut start = 0usize;
    loop {
        let mut ids = [(0u32, FileId { fid: 0, r#gen: 0 }); FD_BATCH];
        let Some((n, more)) = with_table(|t| {
            let p = t.get(pid)?;
            let mut n = 0usize;
            let mut more = false;
            for (i, fd) in p.fds.iter().filter(|(i, _)| *i as usize >= start) {
                let Some(id) = file_id(fd) else {
                    continue;
                };
                let Some(slot) = ids.get_mut(n) else {
                    more = true;
                    break;
                };
                *slot = (i, id);
                n += 1;
            }
            Some((n, more))
        }) else {
            return false;
        };
        for &(i, id) in ids.iter().take(n) {
            if file_init::addref(id).is_err() {
                // Entries from `i` on hold no reference of this row's.
                with_table(|t| {
                    if let Some(p) = t.get_mut(pid) {
                        let mut k = i;
                        while (k as usize) < p.fds.capacity() {
                            let _ = p.fds.close(k);
                            k += 1;
                        }
                    }
                });
                close_all_fds(pid, "fork");
                return false;
            }
        }
        if !more {
            return true;
        }
        start = ids
            .get(n.saturating_sub(1))
            .map_or(start, |&(i, _)| i as usize + 1);
    }
}

pub(super) fn lookup_fd(fd: u32) -> Option<Fd> {
    let pid = current_pid();
    if pid == 0 {
        return None;
    }
    with_table(|t| t.get(pid).and_then(|p| p.fds.get(fd)))
}

/// `write(fd, buf, len)`. The descriptor and its access mode come before
/// the buffer and the count, as Linux checks them: a write to an
/// `O_RDONLY` file is `EBADF` whatever `buf` and `len` are.
pub(super) fn sys_write(fd: u32, buf: u64, len: usize) -> SysResult {
    let len = len as u64;
    let Some(slot) = lookup_fd(fd) else {
        return Err(KError::BadF);
    };
    if let FdKind::File { fid, r#gen } = slot.kind {
        file_init::access(FileId { fid, r#gen }, true).map_err(KError::from)?;
    }
    match slot.kind {
        FdKind::None => Err(KError::BadF),
        FdKind::Console | FdKind::File { .. } => {
            if !user_range_ok(buf, len) {
                return Err(KError::Fault);
            }
            if len == 0 {
                return Ok(0);
            }
            #[cfg(feature = "kernel_tests")]
            if matches!(slot.kind, FdKind::Console) {
                testing::console_write_started();
            }
            let mut scratch = [0u8; 256];
            let mut done = 0u64;
            while done < len {
                let n = (len - done).min(scratch.len() as u64) as usize;
                let Some(va) = buf.checked_add(done) else {
                    return byte_count(done);
                };
                let c = uaccess_init::copy_from_user_partial(&mut scratch[..n], va);
                if c > 0 {
                    match slot.kind {
                        FdKind::Console => console_init::write(&scratch[..c]),
                        FdKind::File { fid, r#gen } => {
                            match file_write(FileId { fid, r#gen }, &scratch[..c]) {
                                Ok(k) => {
                                    if k < c {
                                        return byte_count(done + k as u64);
                                    }
                                }
                                Err(e) => {
                                    return if done == 0 {
                                        Err(KError::from(e))
                                    } else {
                                        Ok(done as usize)
                                    };
                                }
                            }
                        }
                        FdKind::None => return Err(KError::BadF),
                    }
                }
                done += c as u64;
                if c < n {
                    return byte_count(done);
                }
            }
            #[cfg(feature = "kernel_tests")]
            if matches!(slot.kind, FdKind::Console) {
                testing::console_write_returned();
            }
            Ok(done as usize)
        }
    }
}

/// A byte-counting call's result after a short copy: the bytes it moved,
/// or `EFAULT` when it moved none (SYSCALL.md §5).
fn byte_count(done: u64) -> SysResult {
    if done == 0 {
        Err(KError::Fault)
    } else {
        Ok(done as usize)
    }
}

/// Give back the `n` bytes a read consumed from `id` but could not copy
/// out, so the next read returns them. A file that cannot seek keeps them,
/// as a Linux device does.
fn unread(id: FileId, n: usize) {
    let Some(back) = i64::try_from(n).ok().and_then(i64::checked_neg) else {
        return;
    };
    let r = file_init::fget(id).and_then(|f| {
        let r = file_init::seek(&f, SeekFrom::Current(back));
        file_init::close(f).and(r)
    });
    match r {
        Ok(_) | Err(FsError::SPipe) => {}
        Err(e) => crate::klog_ratelimited!(
            1000,
            vibeos::log::Level::Warn,
            "vibeOS: proc: read rewind failed: {}",
            e.as_str()
        ),
    }
}

fn key_byte(k: DecodedKey) -> Option<u8> {
    match k {
        DecodedKey::Char(b) => Some(b),
        DecodedKey::Named(NamedKey::Enter) => Some(b'\n'),
        DecodedKey::Named(NamedKey::Backspace) => Some(0x7f),
        DecodedKey::Named(NamedKey::Tab) => Some(b'\t'),
        DecodedKey::Named(_) => None,
    }
}

/// `read(fd, buf, len)`, in Linux's order: the descriptor and its access
/// mode (`EBADF`), the buffer (`EFAULT`), then a directory's `EISDIR`
/// whatever the count, and only then a count of 0.
pub(super) fn sys_read(fd: u32, buf: u64, len: usize) -> SysResult {
    let len = len as u64;
    let Some(slot) = lookup_fd(fd) else {
        return Err(KError::BadF);
    };
    let kind = match slot.kind {
        FdKind::None => return Err(KError::BadF),
        FdKind::File { fid, r#gen } => {
            Some(file_init::access(FileId { fid, r#gen }, false).map_err(KError::from)?)
        }
        FdKind::Console => None,
    };
    if !user_range_ok(buf, len) {
        return Err(KError::Fault);
    }
    if kind == Some(InodeKind::Dir) {
        return Err(KError::IsDir);
    }
    if len == 0 {
        return Ok(0);
    }
    match slot.kind {
        FdKind::None => Err(KError::BadF),
        FdKind::Console => {
            let mut n = 0u64;
            while n < len {
                let Some(b) = key_byte(console_init::wait_key()) else {
                    continue;
                };
                let Some(va) = buf.checked_add(n) else {
                    return byte_count(n);
                };
                if uaccess_init::copy_to_user_partial(va, &[b]) == 0 {
                    return byte_count(n);
                }
                n += 1;
                if b == b'\n' {
                    break;
                }
            }
            Ok(n as usize)
        }
        FdKind::File { fid, r#gen } => {
            let id = FileId { fid, r#gen };
            let mut scratch = [0u8; 256];
            let n = (len as usize).min(scratch.len());
            match file_read(id, &mut scratch[..n]) {
                Ok(0) => Ok(0),
                Ok(k) => {
                    let c = uaccess_init::copy_to_user_partial(buf, &scratch[..k]);
                    if c < k {
                        unread(id, k - c);
                    }
                    byte_count(c as u64)
                }
                Err(e) => Err(KError::from(e)),
            }
        }
    }
}

pub(super) fn sys_open(path: u64, flags: i32, mode: u16) -> SysResult {
    let mut buf = [0u8; vibeos::fs::MAX_PATH];
    let n = copy_user_str(path, &mut buf)?;
    let mode = u32::from(mode) & 0o7777;
    // The lowest free descriptor (`EMFILE`), then in `Vfs::open` an
    // open-file slot (`ENFILE`), both before anything is created or
    // truncated; an error gives back both reservations.
    let pid = current_pid();
    let (fd, base) = with_table(|t| match t.get_mut(pid) {
        Some(p) => p
            .fds
            .reserve()
            .map(|fd| (fd, p.base()))
            .map_err(KError::from),
        None => Err(KError::MFile),
    })?;
    match file_init::open_at(base, &buf[..n], OpenFlags::from_bits(flags as u32), mode) {
        Ok(f) => {
            let id = f.into_raw();
            let slot = Fd {
                kind: FdKind::File {
                    fid: id.fid,
                    r#gen: id.r#gen,
                },
                flags: fd_flags_from_open(flags as u32),
            };
            let r = with_table(|t| match t.get_mut(pid) {
                Some(p) => p.fds.install_reserved(fd, slot).map_err(KError::from),
                None => Err(KError::BadF),
            });
            match r {
                Ok(()) => Ok(fd as usize),
                Err(e) => {
                    #[expect(
                        clippy::let_underscore_must_use,
                        reason = "cleanup after an error the caller already returns: a close that fails leaves nothing the failed call could report (DESIGN §2.5)"
                    )]
                    let _ = file_init::close(FileRef::from_raw(id));
                    Err(e)
                }
            }
        }
        Err(e) => {
            with_table(|t| {
                if let Some(p) = t.get_mut(pid) {
                    p.fds.unreserve(fd);
                }
            });
            Err(KError::from(e))
        }
    }
}

pub(super) fn sys_close(fd: u32) -> SysResult {
    let pid = current_pid();
    let old = with_table(|t| t.get_mut(pid).and_then(|p| p.fds.close(fd)));
    match old {
        Some(s) => match close_fd_slot(s) {
            Ok(()) => Ok(0),
            Err(e) => Err(KError::from(e)),
        },
        None => Err(KError::BadF),
    }
}

/// Linux's last `whence`, `SEEK_HOLE` (`include/uapi/linux/fs.h`).
const SEEK_MAX: u32 = 4;

/// `lseek(fd, off, whence)`, in Linux's order: the descriptor, then a
/// `whence` past [`SEEK_MAX`] (`EINVAL`), then what the file allows, so a
/// console is `ESPIPE` only for a `whence` Linux knows. `SEEK_DATA` and
/// `SEEK_HOLE` are `EINVAL` on a file that can seek.
pub(super) fn sys_lseek(fd: u32, off: i64, whence: u32) -> SysResult {
    let Some(slot) = lookup_fd(fd) else {
        return Err(KError::BadF);
    };
    if whence > SEEK_MAX {
        return Err(KError::Inval);
    }
    match slot.kind {
        FdKind::File { fid, r#gen } => {
            let r = SeekFrom::from_whence(off, whence).and_then(|pos| {
                let f = file_init::fget(FileId { fid, r#gen })?;
                let r = file_init::seek(&f, pos);
                file_init::close(f).and(r)
            });
            match r {
                Ok(n) => Ok(n as usize),
                Err(e) => Err(KError::from(e)),
            }
        }
        FdKind::Console => Err(KError::SPipe),
        FdKind::None => Err(KError::BadF),
    }
}

/// Drop a count [`hold_file`] took, where the call that took it failed
/// and already reports its own error (DESIGN §2.5).
fn drop_held(s: Fd) {
    if let Some(id) = file_id(s) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "cleanup after an error the caller already returns: a close that fails leaves nothing the failed call could report (DESIGN §2.5)"
        )]
        let _ = file_init::close(FileRef::from_raw(id));
    }
}

/// `old`'s slot in this process's table, with one more count on its file
/// for the copy `dup` or `dup2` makes. The count is taken with the table
/// lock dropped, since the VFS lock sleeps (DESIGN §2.1).
fn hold_file(pid: u32, old: u32) -> Option<Fd> {
    let s = with_table(|t| t.get(pid).and_then(|p| p.fds.get(old)))?;
    if let Some(id) = file_id(s) {
        file_init::addref(id).ok()?;
    }
    Some(s)
}

pub(super) fn sys_dup(old: u32) -> SysResult {
    let pid = current_pid();
    let Some(s) = hold_file(pid, old) else {
        return Err(KError::BadF);
    };
    // The slot may have changed while the table lock was dropped: the
    // copy is made only of the slot the count was taken for. A full table
    // is `EMFILE`, as Linux's.
    let r = with_table(|t| {
        let p = t.get_mut(pid).ok_or(KError::BadF)?;
        if p.fds.get(old) != Some(s) {
            return Err(KError::BadF);
        }
        p.fds.dup(old).map_err(KError::from)
    });
    match r {
        Ok(n) => Ok(n as usize),
        Err(e) => {
            drop_held(s);
            Err(e)
        }
    }
}

pub(super) fn sys_dup2(old: u32, new: u32) -> SysResult {
    let pid = current_pid();
    if old == new {
        return match with_table(|t| t.get(pid).and_then(|p| p.fds.get(old))) {
            Some(_) => Ok(new as usize),
            None => Err(KError::BadF),
        };
    }
    let Some(s) = hold_file(pid, old) else {
        return Err(KError::BadF);
    };
    let r = with_table(|t| {
        let p = t.get_mut(pid).ok_or(KError::BadF)?;
        if p.fds.get(old) != Some(s) {
            return Err(KError::BadF);
        }
        // `Busy` (a slot an `open` reserved) is EBUSY, as on Linux.
        p.fds.dup2(old, new).map_err(KError::from)
    });
    match r {
        Ok(disp) => {
            if let Some(d) = disp {
                close_dropped(d, "dup2 displaced fd");
            }
            Ok(new as usize)
        }
        Err(e) => {
            drop_held(s);
            Err(e)
        }
    }
}

pub(super) fn sys_fcntl(fd: u32, cmd: u32, arg: u64) -> SysResult {
    let pid = current_pid();
    with_table(|t| {
        let Some(p) = t.get_mut(pid) else {
            return Err(KError::Srch);
        };
        let Some(mut s) = p.fds.get(fd) else {
            return Err(KError::BadF);
        };
        match cmd {
            F_GETFD => Ok(s.flags as usize),
            F_SETFD => {
                s.flags = (arg as u32) & FD_CLOEXEC;
                match p.fds.set(fd, s) {
                    Ok(()) => Ok(0),
                    Err(_) => Err(KError::BadF),
                }
            }
            _ => Err(KError::Inval),
        }
    })
}
