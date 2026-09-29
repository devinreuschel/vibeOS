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

pub(super) fn close_all_fds(fds: &mut FdTable) {
    let mut i = 0u32;
    while i < MAX_FDS as u32 {
        if let Some(old) = fds.close(i) {
            let _ = close_fd_slot(old);
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

pub(super) fn validate_buf(buf: u64, len: u64) -> Result<(), i32> {
    let Some(space) = current_space() else {
        return Err(EFAULT);
    };
    syscall::check_user_ptr(|p, n| space.check_user_range(p, n), buf, len)
}

pub(super) fn sys_write(fd: u64, buf: u64, len: u64) -> i64 {
    let Some(slot) = lookup_fd(fd) else {
        return syscall::neg(EBADF);
    };
    match slot.kind {
        FdKind::None => syscall::neg(EBADF),
        FdKind::Console | FdKind::File { .. } => {
            if let Err(e) = validate_buf(buf, len) {
                return syscall::neg(e);
            }
            if len == 0 {
                return 0;
            }
            let Some(space) = current_space() else {
                return syscall::neg(EFAULT);
            };
            let mut scratch = [0u8; 256];
            let mut done = 0u64;
            while done < len {
                let n = (len - done).min(scratch.len() as u64) as usize;
                if space.read_bytes(buf + done, &mut scratch[..n]).is_err() {
                    return syscall::neg(EFAULT);
                }
                match slot.kind {
                    FdKind::Console => console_init::write(&scratch[..n]),
                    FdKind::File { fid, r#gen } => {
                        match file_write(FileId { fid, r#gen }, &scratch[..n]) {
                            Ok(k) => {
                                if k < n {
                                    return (done + k as u64) as i64;
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
                done += n as u64;
            }
            #[cfg(feature = "kernel_tests")]
            if matches!(slot.kind, FdKind::Console) {
                testing::console_write_returned();
            }
            done as i64
        }
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
    if let Err(e) = validate_buf(buf, len) {
        return syscall::neg(e);
    }
    if len == 0 {
        return 0;
    }
    let Some(space) = current_space() else {
        return syscall::neg(EFAULT);
    };
    match slot.kind {
        FdKind::None => syscall::neg(EBADF),
        FdKind::Console => {
            let mut n = 0u64;
            while n < len {
                let Some(b) = key_byte(console_init::wait_key()) else {
                    continue;
                };
                if space.write_bytes(buf + n, &[b]).is_err() {
                    return if n == 0 {
                        syscall::neg(EFAULT)
                    } else {
                        n as i64
                    };
                }
                n += 1;
                if b == b'\n' {
                    break;
                }
            }
            n as i64
        }
        FdKind::File { fid, r#gen } => {
            let mut scratch = [0u8; 256];
            let n = (len as usize).min(scratch.len());
            match file_read(FileId { fid, r#gen }, &mut scratch[..n]) {
                Ok(k) => {
                    if k > 0 && space.write_bytes(buf, &scratch[..k]).is_err() {
                        return syscall::neg(EFAULT);
                    }
                    k as i64
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
                    let _ = file_init::close(FileRef::from_raw(id));
                }
                None
            }
        }
    });
    match r {
        Some((n, disp)) => {
            if let Some(d) = disp {
                let _ = close_fd_slot(d);
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
                let _ = p.fds.set(fd as u32, s);
                0
            }
            _ => syscall::neg(EINVAL),
        }
    })
}
