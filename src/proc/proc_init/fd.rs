use super::*;

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

pub(super) fn close_all_fds(fds: &mut FdTable) {
    let mut i = 0u32;
    while i < MAX_FDS as u32 {
        if let Some(old) = fds.close(i) {
            close_dropped(old, "exit");
        }
        i += 1;
    }
}

pub(super) fn dup_table(src: FdTable) -> Option<FdTable> {
    let mut i = 0u32;
    while i < MAX_FDS as u32 {
        if let Some(id) = src.get(i).and_then(file_id)
            && file_init::addref(id).is_err()
        {
            let mut j = 0u32;
            while j < i {
                if let Some(id) = src.get(j).and_then(file_id) {
                    #[expect(
                        clippy::let_underscore_must_use,
                        reason = "cleanup after an error the caller already returns: a close that fails leaves nothing the failed call could report (DESIGN §2.5)"
                    )]
                    let _ = file_init::close(FileRef::from_raw(id));
                }
                j += 1;
            }
            return None;
        }
        i += 1;
    }
    Some(src)
}

pub(super) fn lookup_fd(fd: u64) -> Option<Fd> {
    let pid = current_pid();
    if pid == 0 {
        return None;
    }
    with_table(|t| t.get(pid).and_then(|p| p.fds.get(fd as u32)))
}

pub(super) fn sys_write(fd: u64, buf: u64, len: u64) -> i64 {
    let Some(slot) = lookup_fd(fd) else {
        return syscall::neg(EBADF);
    };
    match slot.kind {
        FdKind::None => syscall::neg(EBADF),
        FdKind::Console | FdKind::File { .. } => {
            if !user_range_ok(buf, len) {
                return syscall::neg(EFAULT);
            }
            if len == 0 {
                return 0;
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
                                        syscall::neg(fs_errno(e))
                                    } else {
                                        done as i64
                                    };
                                }
                            }
                        }
                        FdKind::None => return syscall::neg(EBADF),
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
            done as i64
        }
    }
}

/// A byte-counting call's result after a short copy: the bytes it moved,
/// or `EFAULT` when it moved none (SYSCALL.md §5).
fn byte_count(done: u64) -> i64 {
    if done == 0 {
        syscall::neg(EFAULT)
    } else {
        done as i64
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
        Ok(_) | Err(FsError::NotSupp) => {}
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

pub(super) fn sys_read(fd: u64, buf: u64, len: u64) -> i64 {
    let Some(slot) = lookup_fd(fd) else {
        return syscall::neg(EBADF);
    };
    if !user_range_ok(buf, len) {
        return syscall::neg(EFAULT);
    }
    if len == 0 {
        return 0;
    }
    match slot.kind {
        FdKind::None => syscall::neg(EBADF),
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
            n as i64
        }
        FdKind::File { fid, r#gen } => {
            let id = FileId { fid, r#gen };
            let mut scratch = [0u8; 256];
            let n = (len as usize).min(scratch.len());
            match file_read(id, &mut scratch[..n]) {
                Ok(0) => 0,
                Ok(k) => {
                    let c = uaccess_init::copy_to_user_partial(buf, &scratch[..k]);
                    if c < k {
                        unread(id, k - c);
                    }
                    byte_count(c as u64)
                }
                Err(e) => syscall::neg(fs_errno(e)),
            }
        }
    }
}

pub(super) fn sys_open(path: u64, flags: u64, _mode: u64) -> i64 {
    let mut buf = [0u8; vibeos::fs::MAX_PATH];
    let n = match copy_user_str(path, &mut buf) {
        Ok(n) => n,
        Err(e) => return syscall::neg(e),
    };
    if core::str::from_utf8(&buf[..n]).is_err() {
        return syscall::neg(EINVAL);
    }
    match file_init::open_routed(&buf[..n], OpenFlags::from_bits(flags as u32), 0) {
        Ok(f) => {
            let id = f.into_raw();
            let slot = Fd {
                kind: FdKind::File {
                    fid: id.fid,
                    r#gen: id.r#gen,
                },
                flags: fd_flags_from_open(flags as u32),
            };
            let pid = current_pid();
            let r = with_table(|t| t.get_mut(pid).and_then(|p| p.fds.alloc(slot).ok()));
            match r {
                Some(fd) => fd as i64,
                None => {
                    #[expect(
                        clippy::let_underscore_must_use,
                        reason = "cleanup after an error the caller already returns: a close that fails leaves nothing the failed call could report (DESIGN §2.5)"
                    )]
                    let _ = file_init::close(FileRef::from_raw(id));
                    syscall::neg(EMFILE)
                }
            }
        }
        Err(e) => syscall::neg(fs_errno(e)),
    }
}

pub(super) fn sys_close(fd: u64) -> i64 {
    let pid = current_pid();
    let old = with_table(|t| t.get_mut(pid).and_then(|p| p.fds.close(fd as u32)));
    match old {
        Some(s) => match close_fd_slot(s) {
            Ok(()) => 0,
            Err(e) => syscall::neg(fs_errno(e)),
        },
        None => syscall::neg(EBADF),
    }
}

pub(super) fn sys_lseek(fd: u64, off: u64, whence: u64) -> i64 {
    let Some(slot) = lookup_fd(fd) else {
        return syscall::neg(EBADF);
    };
    match slot.kind {
        FdKind::File { fid, r#gen } => {
            let r = SeekFrom::from_whence(off as i64, whence as u32).and_then(|pos| {
                let f = file_init::fget(FileId { fid, r#gen })?;
                let r = file_init::seek(&f, pos);
                file_init::close(f).and(r)
            });
            match r {
                Ok(n) => n as i64,
                Err(e) => syscall::neg(fs_errno(e)),
            }
        }
        FdKind::Console => syscall::neg(EINVAL),
        FdKind::None => syscall::neg(EBADF),
    }
}

pub(super) fn sys_dup(old: u64) -> i64 {
    let pid = current_pid();
    let r = with_table(|t| {
        let p = t.get_mut(pid)?;
        let s = p.fds.get(old as u32)?;
        if let Some(id) = file_id(s) {
            file_init::addref(id).ok()?;
        }
        match p.fds.dup(old as u32) {
            Ok(n) => Some(n),
            Err(_) => {
                if let Some(id) = file_id(s) {
                    #[expect(
                        clippy::let_underscore_must_use,
                        reason = "cleanup after an error the caller already returns: a close that fails leaves nothing the failed call could report (DESIGN §2.5)"
                    )]
                    let _ = file_init::close(FileRef::from_raw(id));
                }
                None
            }
        }
    });
    match r {
        Some(n) => n as i64,
        None => syscall::neg(EBADF),
    }
}

pub(super) fn sys_dup2(old: u64, new: u64) -> i64 {
    let pid = current_pid();
    let r = with_table(|t| {
        let p = t.get_mut(pid)?;
        if old as u32 == new as u32 {
            let _ = p.fds.get(old as u32)?;
            return Some((new as u32, None));
        }
        let s = p.fds.get(old as u32)?;
        if let Some(id) = file_id(s)
            && file_init::addref(id).is_err()
        {
            return None;
        }
        match p.fds.dup2(old as u32, new as u32) {
            Ok(displaced) => Some((new as u32, displaced)),
            Err(_) => {
                if let Some(id) = file_id(s) {
                    #[expect(
                        clippy::let_underscore_must_use,
                        reason = "cleanup after an error the caller already returns: a close that fails leaves nothing the failed call could report (DESIGN §2.5)"
                    )]
                    let _ = file_init::close(FileRef::from_raw(id));
                }
                None
            }
        }
    });
    match r {
        Some((n, disp)) => {
            if let Some(d) = disp {
                close_dropped(d, "dup2 displaced fd");
            }
            n as i64
        }
        None => syscall::neg(EBADF),
    }
}

pub(super) fn sys_fcntl(fd: u64, cmd: u64, arg: u64) -> i64 {
    let pid = current_pid();
    with_table(|t| {
        let Some(p) = t.get_mut(pid) else {
            return syscall::neg(ESRCH);
        };
        let Some(mut s) = p.fds.get(fd as u32) else {
            return syscall::neg(EBADF);
        };
        match cmd {
            F_GETFD => s.flags as i64,
            F_SETFD => {
                s.flags = (arg as u32) & FD_CLOEXEC;
                match p.fds.set(fd as u32, s) {
                    Ok(()) => 0,
                    Err(_) => syscall::neg(EBADF),
                }
            }
            _ => syscall::neg(EINVAL),
        }
    })
}
