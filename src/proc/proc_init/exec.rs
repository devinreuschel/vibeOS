use super::*;

fn copy_cvec(va: u64) -> Result<TryVec<TryVec<u8>>, i32> {
    let mut v = TryVec::new();
    if va == 0 {
        return Ok(v);
    }
    let Some(space) = current_space() else {
        return Err(EFAULT);
    };
    let mut i = 0u64;
    while i < 16 {
        let ptr_va = va + i * 8;
        syscall::check_user_ptr(|p, l| space.check_user_range(p, l), ptr_va, 8)?;
        let mut raw = [0u8; 8];
        space.read_bytes(ptr_va, &mut raw).map_err(|_| EFAULT)?;
        let p = u64::from_le_bytes(raw);
        if p == 0 {
            return Ok(v);
        }
        let mut buf = [0u8; 256];
        let n = copy_user_str(p, &mut buf)?;
        let mut s = TryVec::try_with_capacity(n).map_err(|_| ENOMEM)?;
        s.try_extend_from_slice(&buf[..n]).map_err(|_| ENOMEM)?;
        v.try_push(s).map_err(|_| ENOMEM)?;
        i += 1;
    }
    Err(E2BIG)
}

// Out of line: `dispatch_frame` keeps only the running syscall's frame,
// and a preempted body carries an interrupt and a switch on top of it.
#[inline(never)]
pub(super) fn sys_fork(frame: Option<&mut UserFrame>) -> i64 {
    let Some(frame) = frame else {
        return syscall::neg(EINVAL);
    };
    let ppid = current_pid();
    if ppid == 0 {
        return syscall::neg(EINVAL);
    }
    let Some(src) = current_space() else {
        return syscall::neg(EFAULT);
    };
    let meta = with_table(|t| t.get(ppid).map(|p| (p.fds, p.cwd, p.creds)));
    let Some((fds, cwd, creds)) = meta else {
        return syscall::neg(ESRCH);
    };
    let Some(fds) = dup_table(fds) else {
        return syscall::neg(EMFILE);
    };
    let Some(pid) = alloc_pid(0) else {
        close_all_fds(&mut { fds });
        return syscall::neg(EAGAIN);
    };
    let Some(slot) = space_slot() else {
        close_all_fds(&mut { fds });
        with_table(|t| {
            t.procs[pid as usize] = Proc::empty();
        });
        return syscall::neg(ENOMEM);
    };
    let Some(cloned) = addr_space_init::clone_full(src) else {
        close_all_fds(&mut { fds });
        with_table(|t| {
            t.procs[pid as usize] = Proc::empty();
        });
        return syscall::neg(ENOMEM);
    };
    let cr3 = cloned.root().as_u64();
    let mut child = *frame;
    child.rax = 0;
    let fs = crate::x86::rdmsr(crate::x86::IA32_FS_BASE);
    let boxed = slot.write(cloned);
    let h = match thread_init::spawn_user("user", user_thread_entry, pid, cr3, &child) {
        Ok(h) => h,
        Err(e) => {
            // Nothing names the clone's root yet: no thread was made.
            addr_space_init::teardown(boxed.into_inner());
            close_all_fds(&mut { fds });
            with_table(|t| {
                t.procs[pid as usize] = Proc::empty();
            });
            return syscall::neg(spawn_errno(e));
        }
    };
    with_table(|t| {
        init_slot(t, pid, ppid, "user");
        let p = &mut t.procs[pid as usize];
        p.fds = fds;
        p.cwd = cwd;
        p.creds = creds;
        p.space = Some(boxed);
        p.fs_base = fs;
        p.tid = h.id();
    });
    thread_init::make_ready(h.id());
    // Child may run (and exit) before we return. POSIX allows either order.
    pid as i64
}

// Out of line: `dispatch_frame` keeps only the running syscall's frame,
// and a preempted body carries an interrupt and a switch on top of it.
#[inline(never)]
pub(super) fn sys_execve(path: u64, argv: u64, envp: u64, frame: Option<&mut UserFrame>) -> i64 {
    let Some(frame) = frame else {
        return syscall::neg(EINVAL);
    };
    let pid = current_pid();
    if pid == 0 {
        return syscall::neg(EINVAL);
    }
    let mut pbuf = [0u8; vibeos::fs::MAX_PATH];
    let n = match copy_user_str(path, &mut pbuf) {
        Ok(n) => n,
        Err(e) => return syscall::neg(e),
    };
    let Ok(path_s) = core::str::from_utf8(&pbuf[..n]) else {
        return syscall::neg(EINVAL);
    };
    let argv_v = match copy_cvec(argv) {
        Ok(v) => v,
        Err(e) => return syscall::neg(e),
    };
    let _ = envp;
    let Ok(mut argv_s) = TryVec::<&str>::try_with_capacity(argv_v.len().max(1)) else {
        return syscall::neg(ENOMEM);
    };
    if argv_v.is_empty() {
        if argv_s.try_push(path_s).is_err() {
            return syscall::neg(ENOMEM);
        }
    } else {
        for a in argv_v.iter() {
            let Ok(s) = core::str::from_utf8(a) else {
                return syscall::neg(EINVAL);
            };
            if argv_s.try_push(s).is_err() {
                return syscall::neg(ENOMEM);
            }
        }
    }
    let Some(slot) = space_slot() else {
        return syscall::neg(ENOMEM);
    };
    let loaded = match user_init::load_path(path_s, &argv_s) {
        Ok(l) => l,
        Err(e) => return syscall::neg(load_errno(e)),
    };
    let name = intern_name(path_s);
    let entry = loaded.entry;
    let rsp = loaded.rsp;
    let fs = loaded.fs;
    let mut boxed = Some(slot.write(loaded.space));
    let cr3 = boxed.as_ref().map(|s| s.root().as_u64()).unwrap_or(0);
    let old = with_table(|t| {
        let p = t.get_mut(pid)?;
        let gone = p.fds.apply_cloexec();
        p.name = name;
        p.fs_base = fs;
        let old = p.space.take();
        p.space = boxed.take();
        Some((old, gone, p.tid, cr3))
    });
    let Some((old, gone, tid, cr3)) = old else {
        if let Some(b) = boxed {
            addr_space_init::teardown(b.into_inner());
        }
        return syscall::neg(ESRCH);
    };
    let mut i = 0usize;
    while i < MAX_FDS {
        if let Some(fd) = gone[i] {
            close_dropped(fd, "execve close-on-exec");
        }
        i += 1;
    }
    if let Some(s) = p_space_ref(pid) {
        set_as(s);
    }
    thread_init::set_pid_cr3(tid, pid, cr3);
    // SAFETY: invariant I128, established at `addr_space_init::teardown`:
    // `cr3` is the root of the space `create` built and `p.space` now owns,
    // and `set_pid_cr3` recorded it in this thread's TCB on the line above,
    // here.
    unsafe { addr_space_init::load_cr3_u64(cr3) };
    if let Some(old) = old {
        addr_space_init::teardown(old.into_inner());
    }
    *frame = UserFrame {
        orig_rax: frame.orig_rax,
        ..UserFrame::new_user(entry, rsp)
    };
    // SAFETY: FS_BASE is an architectural MSR, and `fs` is the new image's
    // thread pointer, a canonical user address the loader chose
    // (`user_init::load_image`), so the next ring-3 `fs:` access reaches its
    // TLS block; established by `user_init::setup_tls`.
    unsafe { crate::x86::wrmsr(crate::x86::IA32_FS_BASE, fs) };
    0
}

/// Run `f` on the calling process's own address space, after the table
/// lock is released. `None` for pid 0 or a process with no space.
fn with_own_space<R>(f: impl FnOnce(&mut AddressSpace) -> R) -> Option<R> {
    let pid = current_pid();
    if pid == 0 {
        return None;
    }
    let ptr = with_table(|t| {
        let p = t.get_mut(pid)?;
        p.space.as_mut().map(|b| &mut **b as *mut AddressSpace)
    })?;
    // SAFETY: a process's space is replaced or taken only by its own thread
    // (`proc_init::sys_execve`, `proc_init::finish_exit`), and that thread
    // is the caller, here, so the box outlives `f`; processes are
    // single-threaded, so nothing else reaches the space while `f` runs.
    Some(f(unsafe { &mut *ptr }))
}

/// Anonymous private `mmap` (SYSCALL.md §3.1).
pub(super) fn sys_mmap(addr: u64, len: u64, prot: u64, flags: u64, fd: u64, off: u64) -> i64 {
    let req = match mmap_request(addr, len, prot, flags, off) {
        Ok(r) => r,
        Err(MmapError::Inval) => return syscall::neg(EINVAL),
        Err(MmapError::NoMem) => return syscall::neg(ENOMEM),
        Err(MmapError::NotAnon) => {
            let e = if lookup_fd(fd).is_none() {
                EBADF
            } else {
                ENODEV
            };
            return syscall::neg(e);
        }
    };
    match with_own_space(|s| addr_space_init::mmap(s, &req)) {
        None => syscall::neg(EINVAL),
        Some(Ok(va)) => va as i64,
        Some(Err(AsError::Overlap)) => syscall::neg(EEXIST),
        Some(Err(AsError::NullGuard)) => syscall::neg(EPERM),
        Some(Err(AsError::Misaligned)) => syscall::neg(EINVAL),
        Some(Err(_)) => syscall::neg(ENOMEM),
    }
}

/// `munmap` (SYSCALL.md §3.1): allocates nothing.
pub(super) fn sys_munmap(addr: u64, len: u64) -> i64 {
    if !addr.is_multiple_of(PAGE_SIZE_4K) || len == 0 {
        return syscall::neg(EINVAL);
    }
    let Some(len) = len
        .checked_add(PAGE_SIZE_4K - 1)
        .map(|l| l & !(PAGE_SIZE_4K - 1))
        .filter(|&l| addr.checked_add(l).is_some_and(|e| e <= USER_MAP_END))
    else {
        return syscall::neg(EINVAL);
    };
    // SAFETY: every leaf of the caller's space was mapped from the buddy by
    // `addr_space_init::map_anon` or `addr_space_init::brk`, and `unmap`
    // supplies the flush, here.
    match with_own_space(|s| unsafe { addr_space_init::unmap(s, addr, len) }) {
        None => syscall::neg(EINVAL),
        Some(Ok(())) => 0,
        Some(Err(AsError::NoRegionSlot)) => syscall::neg(ENOMEM),
        Some(Err(_)) => syscall::neg(EINVAL),
    }
}

/// `brk`: the new break, or the current one on failure; 0 for pid 0.
pub(super) fn sys_brk(want: u64) -> i64 {
    with_own_space(|s| addr_space_init::brk(s, want)).unwrap_or(0) as i64
}

fn p_space_ref(pid: u32) -> Option<&'static AddressSpace> {
    space_of(pid)
}
