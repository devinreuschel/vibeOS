use super::*;
use vibeos::elf::{self, ExecArgs};
use vibeos::limits::RLIMIT_STACK_DEFAULT;

/// Append the NULL-ended vector of C strings at user address `va` to
/// `args`: `argv`'s strings, or `envp`'s when `env`. A NULL `va` is an
/// empty vector. Each pointer is one 8-byte copy; each string comes in
/// 256-byte chunks up to its NUL, which stop at page boundaries, so a
/// string that ends before an unmapped page copies. A fault is `EFAULT`;
/// a string or a block over Linux's limits is `E2BIG`, and a buffer that
/// cannot grow `ENOMEM` (SYSCALL.md §3.1).
#[inline(never)]
fn copy_cvec(va: u64, args: &mut ExecArgs, env: bool) -> Result<(), KError> {
    if va == 0 {
        return Ok(());
    }
    let mut i = 0u64;
    loop {
        let ptr_va = i
            .checked_mul(8)
            .and_then(|o| va.checked_add(o))
            .ok_or(KError::Fault)?;
        let mut raw = [0u8; 8];
        uaccess_init::copy_from_user(&mut raw, ptr_va).map_err(KError::from)?;
        let p = u64::from_le_bytes(raw);
        if p == 0 {
            return Ok(());
        }
        args.begin(env)?;
        let mut chunk = [0u8; 256];
        let mut off = 0u64;
        loop {
            let at = p.checked_add(off).ok_or(KError::Fault)?;
            let n = uaccess_init::strncpy_from_user(&mut chunk, at).map_err(KError::from)?;
            args.extend(chunk.get(..n).unwrap_or(&[]))?;
            if n < chunk.len() {
                break;
            }
            off += n as u64;
        }
        args.end()?;
        i += 1;
    }
}

// Out of line: `dispatch_frame` keeps only the running syscall's frame,
// and a preempted body carries an interrupt and a switch on top of it.
#[inline(never)]
pub(super) fn sys_fork(frame: Option<&mut UserFrame>) -> SysResult {
    let Some(frame) = frame else {
        return Err(KError::Inval);
    };
    let ppid = current_pid();
    if ppid == 0 {
        return Err(KError::Inval);
    }
    let Some(src) = current_space() else {
        return Err(KError::Fault);
    };
    let meta = with_table(|t| t.get(ppid).map(|p| (p.cwd, p.creds)));
    let Some((cwd, creds)) = meta else {
        return Err(KError::Srch);
    };
    let Some(pid) = alloc_pid(0) else {
        return Err(KError::Again);
    };
    // The parent's descriptors, copied row to row in the table, then a
    // reference taken on each open file: no row leaves the table.
    if !with_table(|t| t.copy_fds(ppid, pid)) {
        with_sched_table(|s, t| release_pid(s, t, pid));
        return Err(KError::Srch);
    }
    if !addref_fds(pid) {
        with_sched_table(|s, t| release_pid(s, t, pid));
        return Err(KError::MFile);
    }
    let Some(slot) = space_slot() else {
        close_all_fds(pid, "fork");
        with_sched_table(|s, t| release_pid(s, t, pid));
        return Err(KError::NoMem);
    };
    let Some(boxed) = clone_into(slot, src) else {
        close_all_fds(pid, "fork");
        with_sched_table(|s, t| release_pid(s, t, pid));
        return Err(KError::NoMem);
    };
    let root = boxed.root().as_u64();
    let mut child = *frame;
    child.rax = 0;
    let fs = crate::arch::current::user_tls();
    let h = match thread_init::spawn_user("user", user_thread_entry, pid, root, &child) {
        Ok(h) => h,
        Err(e) => {
            // Nothing names the clone's root yet: no thread was made.
            addr_space_init::teardown(boxed.into_inner());
            close_all_fds(pid, "fork");
            with_sched_table(|s, t| release_pid(s, t, pid));
            return Err(KError::from(e));
        }
    };
    with_table(|t| {
        if let Some(p) = t.get_mut(pid) {
            p.ppid = ppid;
            p.name = "user";
            p.cwd = cwd;
            p.creds = creds;
            p.space = Some(boxed);
            p.fs_base = fs;
            p.tid = h.id();
        }
    });
    thread_init::make_ready(h.id());
    // Child may run (and exit) before we return. POSIX allows either order.
    Ok(pid as usize)
}

/// A full copy of `src` for fork, moved into `slot`. Out of line, so the
/// clone's by-value moves are off `sys_fork`'s frame, which stays on the
/// stack under the child's spawn (DESIGN §4.5).
#[inline(never)]
fn clone_into(
    slot: TryBox<MaybeUninit<AddressSpace>>,
    src: &AddressSpace,
) -> Option<TryBox<AddressSpace>> {
    addr_space_init::clone_full(src).map(|c| slot.write(c))
}

// Out of line: `dispatch_frame` keeps only the running syscall's frame,
// and a preempted body carries an interrupt and a switch on top of it.
#[inline(never)]
pub(super) fn sys_execve(
    path: u64,
    argv: u64,
    envp: u64,
    frame: Option<&mut UserFrame>,
) -> SysResult {
    let Some(frame) = frame else {
        return Err(KError::Inval);
    };
    let pid = current_pid();
    if pid == 0 {
        return Err(KError::Inval);
    }
    let mut pbuf = [0u8; vibeos::fs::MAX_PATH];
    let n = copy_user_str(path, &mut pbuf)?;
    // The path's and the arguments' bytes go through as they are: only
    // NUL ends one, as on Linux.
    let path_b = &pbuf[..n];
    // Every argument is copied before the load starts, so a refused one
    // returns to the old image (DESIGN §4.4).
    let mut args = ExecArgs::new(elf::arg_space_limit(RLIMIT_STACK_DEFAULT));
    copy_cvec(argv, &mut args, false)?;
    args.finish_argv()?;
    copy_cvec(envp, &mut args, true)?;
    let loaded = match user_init::load_path(path_b, &args) {
        Ok(l) => l,
        Err(e) => return Err(KError::from(e)),
    };
    let name = intern_name(path_b);
    let entry = loaded.entry;
    let rsp = loaded.rsp;
    let fs = loaded.fs;
    let mut boxed = Some(loaded.space);
    let root = boxed.as_ref().map(|s| s.root().as_u64()).unwrap_or(0);
    let old = with_table(|t| {
        let p = t.get_mut(pid)?;
        p.name = name;
        p.fs_base = fs;
        let old = p.space.take();
        p.space = boxed.take();
        Some((old, p.tid, root))
    });
    let Some((old, tid, root)) = old else {
        if let Some(b) = boxed {
            addr_space_init::teardown(b.into_inner());
        }
        return Err(KError::Srch);
    };
    close_where(pid, "execve close-on-exec", Fd::cloexec);
    if let Some(s) = p_space_ref(pid) {
        set_as(s);
    }
    thread_init::set_pid_cr3(tid, pid, root);
    // SAFETY: invariant I128, established at `addr_space_init::teardown`:
    // `cr3` is the root of the space `create` built and `p.space` now owns,
    // and `set_pid_cr3` recorded it in this thread's TCB on the line above,
    // here.
    unsafe { addr_space_init::load_cr3_u64(root) };
    if let Some(old) = old {
        addr_space_init::teardown(old.into_inner());
    }
    *frame = UserFrame {
        orig_rax: frame.orig_rax,
        ..UserFrame::new_user(entry, rsp)
    };
    // SAFETY: `fs` is the new image's thread pointer, a canonical user
    // address the loader chose (`user_init::load_image`), as
    // `set_user_tls` requires, so the next ring-3 TLS access reaches its
    // block; established by `user_init::setup_tls`.
    unsafe { crate::arch::current::set_user_tls(fs) };
    Ok(0)
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
pub(super) fn sys_mmap(addr: u64, len: u64, prot: u64, flags: u64, fd: u64, off: u64) -> SysResult {
    let req = match mmap_request(addr, len, prot, flags, off) {
        Ok(r) => r,
        Err(MmapError::Inval) => return Err(KError::Inval),
        Err(MmapError::NoMem) => return Err(KError::NoMem),
        Err(MmapError::NotAnon) => {
            let e = if lookup_fd(fd as u32).is_none() {
                KError::BadF
            } else {
                KError::NoDev
            };
            return Err(e);
        }
    };
    match with_own_space(|s| addr_space_init::mmap(s, &req)) {
        None => Err(KError::Inval),
        Some(Ok(va)) => Ok(va as usize),
        Some(Err(AsError::Overlap)) => Err(KError::Exist),
        Some(Err(AsError::NullGuard)) => Err(KError::Perm),
        Some(Err(AsError::Misaligned)) => Err(KError::Inval),
        Some(Err(_)) => Err(KError::NoMem),
    }
}

/// `munmap` (SYSCALL.md §3.1): allocates nothing.
pub(super) fn sys_munmap(addr: u64, len: usize) -> SysResult {
    let len = len as u64;
    if !addr.is_multiple_of(PAGE_SIZE_4K) || len == 0 {
        return Err(KError::Inval);
    }
    let Some(len) = len
        .checked_add(PAGE_SIZE_4K - 1)
        .map(|l| l & !(PAGE_SIZE_4K - 1))
        .filter(|&l| addr.checked_add(l).is_some_and(|e| e <= USER_MAP_END))
    else {
        return Err(KError::Inval);
    };
    // SAFETY: every leaf of the caller's space was mapped from the buddy by
    // `addr_space_init::map_anon` or `addr_space_init::brk`, and `unmap`
    // supplies the flush, here.
    match with_own_space(|s| unsafe { addr_space_init::unmap(s, addr, len) }) {
        None => Err(KError::Inval),
        Some(Ok(())) => Ok(0),
        Some(Err(AsError::NoRegionSlot)) => Err(KError::NoMem),
        Some(Err(_)) => Err(KError::Inval),
    }
}

/// `brk`: the new break, or the current one on failure; 0 for pid 0.
pub(super) fn sys_brk(want: u64) -> SysResult {
    Ok(with_own_space(|s| addr_space_init::brk(s, want)).unwrap_or(0) as usize)
}

fn p_space_ref(pid: u32) -> Option<&'static AddressSpace> {
    space_of(pid)
}
