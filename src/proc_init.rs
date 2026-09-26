//! Process table, fork/exec/wait, fd syscalls, early signals. ROADMAP §9.5–9.7.
//!
//! Table lives under SCHED (wait/zombie). Do not hold SCHED across AS
//! clone, ELF load, or heap teardown. COW is Phase 12. User handlers are
//! Phase 13.

#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt::Write;

use vibeos::addr_space::AddressSpace;
use vibeos::desc::InterruptFrame;
use vibeos::fs::{FileId, FileRef, FsError, OpenFlags, SeekFrom};
use vibeos::kbd::{DecodedKey, NamedKey};
use vibeos::proc::{
    Creds, Cwd, FD_CLOEXEC, Fd, FdKind, FdTable, INIT_PID, InitState, MAX_FDS, MAX_PROCS,
    ProcState, SIGBUS, SIGCHLD, SIGCONT, SIGFPE, SIGILL, SIGKILL, SIGSEGV, SIGSTOP, SIGTRAP,
    SigAct, WNOHANG, default_action, fd_flags_from_open, reaper_for, sig_name, wait_exited,
    wait_signaled, wait_stopped,
};
use vibeos::sched::FAR_DEADLINE;
use vibeos::syscall::{
    self, E2BIG, EAGAIN, EBADF, EBUSY, ECHILD, EEXIST, EFAULT, EFBIG, EINVAL, EIO, EISDIR, EMFILE,
    ENAMETOOLONG, ENOENT, ENOEXEC, ENOMEM, ENOSYS, ENOTDIR, ESRCH, F_GETFD, F_SETFD, SYS_CLOSE,
    SYS_DUP, SYS_DUP2, SYS_EXECVE, SYS_EXIT, SYS_FCNTL, SYS_FORK, SYS_GETPID, SYS_GETPPID,
    SYS_KILL, SYS_LSEEK, SYS_OPEN, SYS_PSINFO, SYS_READ, SYS_SCHED_YIELD, SYS_WAIT4, SYS_WRITE,
    SyscallFrame, UserRegs,
};
use vibeos::thread::{RFLAGS_IF, RFLAGS_RESERVED1, ThreadId};
use vibeos::vectors;
use vibeos::wait::WaitQueue;

use crate::addr_space_init;
use crate::cell::IrqCell;
use crate::console_init;
use crate::file_init;
use crate::serial::Serial;
use crate::syscall_init;
use crate::thread_init::{self, SpawnError};
use crate::user_init::{self, LoadError, Loaded};

struct Proc {
    state: ProcState,
    pid: u32,
    ppid: u32,
    tid: ThreadId,
    name: &'static str,
    creds: Creds,
    cwd: Cwd,
    fds: FdTable,
    wait_status: u32,
    pending: u32,
    /// No reaper: freed at exit, ROADMAP §10.5.
    autoreap: bool,
    space: Option<Box<AddressSpace>>,
    entry: UserRegs,
    wait_wq: WaitQueue,
    stop_wq: WaitQueue,
}

impl Proc {
    const fn empty() -> Self {
        Self {
            state: ProcState::Unused,
            pid: 0,
            ppid: 0,
            tid: ThreadId::NONE,
            name: "",
            creds: Creds::ROOT,
            cwd: Cwd::root(),
            fds: FdTable::empty(),
            wait_status: 0,
            pending: 0,
            autoreap: false,
            space: None,
            entry: UserRegs::empty(),
            wait_wq: WaitQueue::new(),
            stop_wq: WaitQueue::new(),
        }
    }
}

/// The process table. `procs[0]` is never a process: its `wait_wq` is the
/// kernel's, on which [`wait_kernel`] sleeps for ppid-0 processes.
struct Table {
    procs: [Proc; MAX_PROCS],
}

impl Table {
    const fn empty() -> Self {
        Self {
            procs: [const { Proc::empty() }; MAX_PROCS],
        }
    }

    fn get(&self, pid: u32) -> Option<&Proc> {
        let i = pid as usize;
        if i == 0 || i >= MAX_PROCS {
            return None;
        }
        let p = &self.procs[i];
        if p.state == ProcState::Unused || p.pid != pid {
            None
        } else {
            Some(p)
        }
    }

    fn get_mut(&mut self, pid: u32) -> Option<&mut Proc> {
        let i = pid as usize;
        if i == 0 || i >= MAX_PROCS {
            return None;
        }
        let p = &mut self.procs[i];
        if p.state == ProcState::Unused || p.pid != pid {
            None
        } else {
            Some(p)
        }
    }
}

static TABLE: IrqCell<Table> = IrqCell::new(Table::empty());

fn with_table<R>(f: impl FnOnce(&mut Table) -> R) -> R {
    thread_init::with_sched(|_| TABLE.with(f))
}

fn intern_name(path: &str) -> &'static str {
    let b = path.as_bytes();
    let mut i = b.len();
    while i > 0 && b[i - 1] != b'/' {
        i -= 1;
    }
    match &b[i..] {
        b"init" => "init",
        b"sh" => "sh",
        b"tests" => "tests",
        b"hello" => "hello",
        _ => "user",
    }
}

fn fs_errno(e: FsError) -> i32 {
    match e {
        FsError::NotFound => ENOENT,
        FsError::Exists => EEXIST,
        FsError::NotDir => ENOTDIR,
        FsError::IsDir => EISDIR,
        FsError::Inval => EINVAL,
        FsError::NoSpace => EMFILE,
        FsError::NameTooLong => ENAMETOOLONG,
        FsError::Busy => EBUSY,
        FsError::Badf => EBADF,
        FsError::Io => EIO,
        FsError::FileTooBig => EFBIG,
        FsError::Loop | FsError::NotEmpty | FsError::NotSupp => EINVAL,
    }
}

fn load_errno(e: LoadError) -> i32 {
    match e {
        LoadError::Fs(f) => fs_errno(f),
        LoadError::Elf(_) => ENOEXEC,
        LoadError::As(_) | LoadError::TooBig => ENOMEM,
        LoadError::Mem(_) => EFAULT,
        LoadError::Empty => ENOEXEC,
        LoadError::NoProc => EAGAIN,
        LoadError::Spawn(e) => spawn_errno(e),
    }
}

/// Linux's errno for a thread `fork` or a new process could not get:
/// `EAGAIN` for a full thread table, `ENOMEM` for a kernel stack.
fn spawn_errno(e: SpawnError) -> i32 {
    match e {
        SpawnError::NoSlot => EAGAIN,
        SpawnError::NoMemory => ENOMEM,
    }
}

fn bit(sig: u32) -> u32 {
    if sig == 0 || sig > 31 { 0 } else { 1u32 << sig }
}

fn current_pid() -> u32 {
    thread_init::current_pid()
}

/// Address space for the current syscall: the process's own, or the
/// global `CURRENT_AS` when the caller has no pid.
pub fn current_space() -> Option<&'static AddressSpace> {
    let pid = current_pid();
    if pid != 0
        && let Some(s) = space_of(pid)
    {
        return Some(s);
    }
    syscall_init::peek_user_as()
}

fn space_of(pid: u32) -> Option<&'static AddressSpace> {
    with_table(|t| {
        let p = t.get(pid)?;
        p.space.as_ref().map(|s| &**s as *const AddressSpace)
    })
    .map(|p| unsafe { &*p })
}

fn set_as(space: &AddressSpace) {
    syscall_init::set_user_as(space);
}

fn clear_as() {
    syscall_init::clear_user_as();
}

fn alloc_pid(prefer: u32) -> Option<u32> {
    with_table(|t| {
        let pid = if prefer != 0 {
            let i = prefer as usize;
            if i < MAX_PROCS && t.procs[i].state == ProcState::Unused {
                Some(prefer)
            } else {
                None
            }
        } else {
            None
        };
        let pid = match pid {
            Some(p) => p,
            None => {
                if prefer == INIT_PID {
                    return None;
                }
                let mut i = 2usize;
                let mut found = None;
                while i < MAX_PROCS {
                    if t.procs[i].state == ProcState::Unused {
                        found = Some(i as u32);
                        break;
                    }
                    i += 1;
                }
                found?
            }
        };
        t.procs[pid as usize].state = ProcState::Live;
        t.procs[pid as usize].pid = pid;
        Some(pid)
    })
}

fn init_slot(t: &mut Table, pid: u32, ppid: u32, name: &'static str) {
    let p = &mut t.procs[pid as usize];
    *p = Proc::empty();
    p.state = ProcState::Live;
    p.pid = pid;
    p.ppid = ppid;
    p.name = name;
    p.fds = FdTable::stdio();
}

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

fn close_fd_slot(fd: Fd) -> Result<(), FsError> {
    match file_id(fd) {
        Some(id) => file_init::close(FileRef::from_raw(id)),
        None => Ok(()),
    }
}

fn close_all_fds(fds: &mut FdTable) {
    let mut i = 0u32;
    while i < MAX_FDS as u32 {
        if let Some(old) = fds.close(i) {
            let _ = close_fd_slot(old);
        }
        i += 1;
    }
}

fn dup_table(src: FdTable) -> Option<FdTable> {
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

fn user_thread_entry() {
    let pid = current_pid();
    let regs = with_table(|t| {
        t.get(pid).map(|p| {
            if let Some(ref s) = p.space {
                set_as(s);
            }
            p.entry
        })
    });
    let Some(regs) = regs else {
        thread_init::exit_current();
    };
    unsafe { syscall_init::enter_user_full(&regs) };
}

#[cfg_attr(feature = "kernel_tests", allow(dead_code))]
pub fn start_init() {
    match spawn_elf("/sbin/init", INIT_PID, 0) {
        Ok(_) => {}
        Err(e) => {
            let _ = writeln!(Serial, "user: init failed: {}", e.as_str());
        }
    }
}

/// Start the ELF at `path` as a new process with parent `ppid` (0: the
/// kernel, which reaps it with [`wait_kernel`]).
pub(crate) fn spawn_elf(path: &str, prefer: u32, ppid: u32) -> Result<u32, LoadError> {
    start_loaded(
        user_init::load_path(path, &[path])?,
        prefer,
        ppid,
        intern_name(path),
    )
}

/// Start the in-memory ELF image `elf` with `argv` as a new process with
/// parent `ppid` (0: the kernel, which reaps it with [`wait_kernel`]).
pub(crate) fn spawn_image(elf: &[u8], argv: &[&[u8]], ppid: u32) -> Result<u32, LoadError> {
    let name = match argv.first().map(|a| core::str::from_utf8(a)) {
        Some(Ok(a)) => intern_name(a),
        _ => "user",
    };
    start_loaded(user_init::load_image(elf, argv)?, 0, ppid, name)
}

fn start_loaded(
    loaded: Loaded,
    prefer: u32,
    ppid: u32,
    name: &'static str,
) -> Result<u32, LoadError> {
    let pid = match alloc_pid(prefer) {
        Some(p) => p,
        None => {
            addr_space_init::teardown(loaded.space);
            return Err(LoadError::NoProc);
        }
    };
    let cr3 = loaded.space.root().as_u64();
    let mut entry = UserRegs::empty();
    entry.rip = loaded.entry;
    entry.rsp = loaded.rsp;
    entry.rflags = RFLAGS_RESERVED1 | RFLAGS_IF;
    entry.fs_base = loaded.fs;
    let boxed = Box::new(loaded.space);
    let h = match thread_init::spawn_user(name, user_thread_entry, pid, cr3) {
        Ok(h) => h,
        Err(e) => {
            with_table(|t| t.procs[pid as usize] = Proc::empty());
            addr_space_init::teardown(*boxed);
            return Err(LoadError::Spawn(e));
        }
    };
    with_table(|t| {
        init_slot(t, pid, ppid, name);
        t.procs[pid as usize].space = Some(boxed);
        t.procs[pid as usize].entry = entry;
    });
    with_table(|t| {
        if let Some(p) = t.get_mut(pid) {
            p.tid = h.id();
        }
    });
    thread_init::make_ready(h.id());
    Ok(pid)
}

enum KernelWait {
    Done(u32),
    Sleep,
    NotKernelChild,
}

/// Block until `pid`, a process whose parent is the kernel (ppid 0),
/// exits; reap it and return its `wait4` status word. Returns once the
/// zombie is reaped, before its address space and kernel stack are freed.
pub(crate) fn wait_kernel(pid: u32) -> u32 {
    debug_assert_eq!(current_pid(), 0);
    loop {
        let r = thread_init::with_sched(|s| {
            TABLE.with(|t| {
                let Some(p) = t.get(pid) else {
                    return KernelWait::NotKernelChild;
                };
                if p.ppid != 0 {
                    return KernelWait::NotKernelChild;
                }
                match p.state {
                    ProcState::Zombie => {
                        let st = p.wait_status;
                        reap_zombie(t, pid);
                        KernelWait::Done(st)
                    }
                    ProcState::Live | ProcState::Stopped => {
                        s.begin_wait(&mut t.procs[0].wait_wq, FAR_DEADLINE);
                        KernelWait::Sleep
                    }
                    ProcState::Unused => KernelWait::NotKernelChild,
                }
            })
        });
        assert!(
            !matches!(r, KernelWait::NotKernelChild),
            "wait_kernel({pid}): not a live kernel-parented process; only kernel code calls \
             wait_kernel, once per ppid-0 pid that spawn_elf or spawn_image returned, and only \
             wait_kernel reaps a ppid-0 process"
        );
        match r {
            KernelWait::Done(st) => return st,
            KernelWait::Sleep | KernelWait::NotKernelChild => thread_init::schedule(),
        }
    }
}

pub fn syscall(frame: *mut SyscallFrame) -> i64 {
    apply_pending(frame);
    let f = unsafe { &mut *frame };
    let nr = f.nr;
    let args = f.args();
    let ret = dispatch_frame(nr, args, f);
    if syscall_init::trace_enabled() {
        let name = syscall::info(nr).map(|i| i.name).unwrap_or("?");
        let _ = writeln!(Serial, "user: syscall {name} nr={nr} = {ret}");
    }
    ret
}

pub fn dispatch(nr: u64, args: [u64; 6]) -> i64 {
    dispatch_frame(nr, args, core::ptr::null_mut())
}

fn dispatch_frame(nr: u64, args: [u64; 6], frame: *mut SyscallFrame) -> i64 {
    match nr {
        SYS_READ => sys_read(args[0], args[1], args[2]),
        SYS_WRITE => sys_write(args[0], args[1], args[2]),
        SYS_OPEN => sys_open(args[0], args[1], args[2]),
        SYS_CLOSE => sys_close(args[0]),
        SYS_LSEEK => sys_lseek(args[0], args[1], args[2]),
        SYS_DUP => sys_dup(args[0]),
        SYS_DUP2 => sys_dup2(args[0], args[1]),
        SYS_GETPID => current_pid() as i64,
        SYS_GETPPID => with_table(|t| t.get(current_pid()).map(|p| p.ppid).unwrap_or(0)) as i64,
        SYS_SCHED_YIELD => {
            if current_pid() != 0 {
                thread_init::yield_now();
            }
            0
        }
        SYS_FORK => sys_fork(frame),
        SYS_EXECVE => sys_execve(args[0], args[1], args[2], frame),
        SYS_EXIT => {
            if current_pid() == 0 {
                0
            } else {
                sys_exit(args[0], false)
            }
        }
        SYS_WAIT4 => sys_wait4(args[0], args[1], args[2]),
        SYS_KILL => sys_kill(args[0], args[1]),
        SYS_FCNTL => sys_fcntl(args[0], args[1], args[2]),
        SYS_PSINFO => sys_psinfo(args[0], args[1]),
        _ => syscall::neg(ENOSYS),
    }
}

fn apply_pending(frame: *mut SyscallFrame) {
    let pid = current_pid();
    if pid == 0 {
        return;
    }
    loop {
        let act = with_table(|t| {
            let Some(p) = t.get_mut(pid) else {
                return Pending::None;
            };
            if p.pending & bit(SIGKILL) != 0 {
                return Pending::Die(SIGKILL);
            }
            if p.state == ProcState::Stopped || p.pending & bit(SIGSTOP) != 0 {
                p.state = ProcState::Stopped;
                p.pending &= !bit(SIGSTOP);
                return Pending::Stop;
            }
            let pend = p.pending;
            let mut s = 1u32;
            while s <= 31 {
                if pend & bit(s) != 0 && s != SIGCHLD && s != SIGCONT {
                    match default_action(s) {
                        SigAct::Term => return Pending::Die(s),
                        SigAct::Stop => {
                            p.state = ProcState::Stopped;
                            p.pending &= !bit(s);
                            return Pending::Stop;
                        }
                        SigAct::Ign | SigAct::Cont => {
                            p.pending &= !bit(s);
                        }
                    }
                }
                s += 1;
            }
            Pending::None
        });
        match act {
            Pending::None => return,
            Pending::Die(sig) => {
                let _ = frame;
                finish_exit(wait_signaled(sig), true);
            }
            Pending::Stop => {
                let wq = unsafe { &mut (*TABLE.as_ptr()).procs[pid as usize].stop_wq };
                thread_init::wait_on(wq);
            }
        }
    }
}

enum Pending {
    None,
    Die(u32),
    Stop,
}

fn lookup_fd(fd: u64) -> Option<Fd> {
    let pid = current_pid();
    if pid == 0 {
        return None;
    }
    with_table(|t| t.get(pid).and_then(|p| p.fds.get(fd as u32)))
}

fn validate_buf(buf: u64, len: u64) -> Result<(), i32> {
    let Some(space) = current_space() else {
        return Err(EFAULT);
    };
    syscall::check_user_ptr(|p, n| space.check_user_range(p, n), buf, len)
}

fn sys_write(fd: u64, buf: u64, len: u64) -> i64 {
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

fn sys_read(fd: u64, buf: u64, len: u64) -> i64 {
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

fn copy_user_str(va: u64, out: &mut [u8]) -> Result<usize, i32> {
    if va == 0 {
        return Err(EFAULT);
    }
    let Some(space) = current_space() else {
        return Err(EFAULT);
    };
    let mut n = 0usize;
    while n < out.len() {
        syscall::check_user_ptr(|p, l| space.check_user_range(p, l), va + n as u64, 1)?;
        let mut b = [0u8; 1];
        space
            .read_bytes(va + n as u64, &mut b)
            .map_err(|_| EFAULT)?;
        if b[0] == 0 {
            return Ok(n);
        }
        out[n] = b[0];
        n += 1;
    }
    Err(ENAMETOOLONG)
}

fn copy_cvec(va: u64) -> Result<Vec<Vec<u8>>, i32> {
    let mut v = Vec::new();
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
        v.push(buf[..n].to_vec());
        i += 1;
    }
    Err(E2BIG)
}

fn sys_open(path: u64, flags: u64, _mode: u64) -> i64 {
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

fn sys_close(fd: u64) -> i64 {
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

fn sys_lseek(fd: u64, off: u64, whence: u64) -> i64 {
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

fn sys_dup(old: u64) -> i64 {
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

fn sys_dup2(old: u64, new: u64) -> i64 {
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

fn sys_fcntl(fd: u64, cmd: u64, arg: u64) -> i64 {
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

fn sys_fork(frame: *mut SyscallFrame) -> i64 {
    if frame.is_null() {
        return syscall::neg(EINVAL);
    }
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
    let Some(cloned) = addr_space_init::clone_full(src) else {
        close_all_fds(&mut { fds });
        with_table(|t| {
            t.procs[pid as usize] = Proc::empty();
        });
        return syscall::neg(ENOMEM);
    };
    let cr3 = cloned.root().as_u64();
    let child_regs = UserRegs::from_syscall(unsafe { &*frame }, 0);
    let fs = crate::x86::rdmsr(crate::x86::IA32_FS_BASE);
    let mut child_regs = child_regs;
    child_regs.fs_base = fs;
    let boxed = Box::new(cloned);
    let h = match thread_init::spawn_user("user", user_thread_entry, pid, cr3) {
        Ok(h) => h,
        Err(e) => {
            // Nothing names the clone's root yet: no thread was made.
            addr_space_init::teardown(*boxed);
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
        p.entry = child_regs;
        p.tid = h.id();
    });
    thread_init::make_ready(h.id());
    // Child may run (and exit) before we return. POSIX allows either order.
    pid as i64
}

fn sys_execve(path: u64, argv: u64, envp: u64, frame: *mut SyscallFrame) -> i64 {
    if frame.is_null() {
        return syscall::neg(EINVAL);
    }
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
    let mut argv_s: Vec<&str> = Vec::new();
    if argv_v.is_empty() {
        argv_s.push(path_s);
    } else {
        for a in &argv_v {
            match core::str::from_utf8(a) {
                Ok(s) => argv_s.push(s),
                Err(_) => return syscall::neg(EINVAL),
            }
        }
    }
    let loaded = match user_init::load_path(path_s, &argv_s) {
        Ok(l) => l,
        Err(e) => return syscall::neg(load_errno(e)),
    };
    let name = intern_name(path_s);
    let entry = loaded.entry;
    let rsp = loaded.rsp;
    let fs = loaded.fs;
    let mut boxed = Some(Box::new(loaded.space));
    let cr3 = boxed.as_ref().map(|s| s.root().as_u64()).unwrap_or(0);
    let old = with_table(|t| {
        let p = t.get_mut(pid)?;
        let gone = p.fds.apply_cloexec();
        p.name = name;
        p.entry.rip = entry;
        p.entry.rsp = rsp;
        p.entry.fs_base = fs;
        p.entry.rax = 0;
        p.entry.rflags = RFLAGS_RESERVED1 | RFLAGS_IF;
        let old = p.space.take();
        p.space = boxed.take();
        Some((old, gone, p.tid, cr3))
    });
    let Some((old, gone, tid, cr3)) = old else {
        if let Some(b) = boxed {
            addr_space_init::teardown(*b);
        }
        return syscall::neg(ESRCH);
    };
    let mut i = 0usize;
    while i < MAX_FDS {
        if let Some(fd) = gone[i] {
            let _ = close_fd_slot(fd);
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
        addr_space_init::teardown(*old);
    }
    let f = unsafe { &mut *frame };
    let mut regs = UserRegs::empty();
    regs.rip = entry;
    regs.rsp = rsp;
    regs.rflags = RFLAGS_RESERVED1 | RFLAGS_IF;
    regs.fs_base = fs;
    regs.apply_to_syscall(f);
    unsafe { crate::x86::wrmsr(crate::x86::IA32_FS_BASE, fs) };
    0
}

fn p_space_ref(pid: u32) -> Option<&'static AddressSpace> {
    space_of(pid)
}

fn sys_exit(status: u64, _from_signal: bool) -> i64 {
    finish_exit(wait_exited(status as u32), false);
}

fn finish_exit(wait_status: u32, _from_fault: bool) -> ! {
    let pid = current_pid();
    if pid == 0 {
        thread_init::exit_current();
    }
    let (old, ppid, fds, tid) = thread_init::with_sched(|s| {
        TABLE.with(|t| {
            if reparent_children(t, pid) {
                s.wake_all(&mut t.procs[INIT_PID as usize].wait_wq);
            }
            let p = t.get_mut(pid);
            let (space, ppid, fds, tid, autoreap) = match p {
                Some(p) => {
                    p.state = ProcState::Zombie;
                    p.wait_status = wait_status;
                    p.pending = 0;
                    let space = p.space.take();
                    let fds = p.fds;
                    p.fds = FdTable::empty();
                    (space, p.ppid, fds, p.tid, p.autoreap)
                }
                None => (None, 0, FdTable::empty(), ThreadId::NONE, false),
            };
            if autoreap {
                // No reaper (ROADMAP §10.5): nobody waits, so free the slot now.
                reap_zombie(t, pid);
            } else {
                if let Some(par) = t.get_mut(ppid) {
                    par.pending |= bit(SIGCHLD);
                }
                // ppid 0 wakes the kernel's queue (`wait_kernel`).
                s.wake_all(&mut t.procs[ppid as usize].wait_wq);
            }
            (space, ppid, fds, tid)
        })
    });
    let mut fds = fds;
    close_all_fds(&mut fds);
    let _ = ppid;
    if let Some(space) = old {
        // The TCB stops naming the root before the kernel root is loaded,
        // so a switch back in between cannot reload it (invariant I128).
        thread_init::set_pid_cr3(tid, 0, 0);
        crate::arch::gs::force_kernel();
        addr_space_init::load_kernel_cr3();
        clear_as();
        addr_space_init::teardown(*space);
    }
    crate::arch::gs::force_kernel();
    thread_init::exit_current();
}

/// Give `dead`'s children to the reaper `reaper_for` picks (ROADMAP §10.5,
/// F068). With none, a zombie child is freed now and a live one gets
/// ppid 0 and `autoreap`, so `finish_exit` frees it. True when init
/// adopted a child and its wait queue needs a wake.
fn reparent_children(t: &mut Table, dead: u32) -> bool {
    // An exiting init is still `Live` here; it must not adopt its own children.
    let init = if dead == INIT_PID {
        InitState::Zombie
    } else {
        InitState::of(t.procs[INIT_PID as usize].state)
    };
    let reaper = reaper_for(init);
    let mut adopted = false;
    let mut i = 1usize;
    while i < MAX_PROCS {
        let p = &mut t.procs[i];
        if p.state != ProcState::Unused && p.ppid == dead && p.pid != dead {
            match reaper {
                Some(r) => {
                    p.ppid = r;
                    adopted = true;
                }
                None if p.state == ProcState::Zombie => reap_zombie(t, i as u32),
                None => {
                    p.ppid = 0;
                    p.autoreap = true;
                }
            }
        }
        i += 1;
    }
    adopted
}

fn sys_wait4(pid: u64, status: u64, options: u64) -> i64 {
    let self_pid = current_pid();
    if self_pid == 0 {
        return syscall::neg(ECHILD);
    }
    let want = pid as i64;
    let nohang = options & WNOHANG != 0;
    loop {
        let r = thread_init::with_sched(|s| {
            TABLE.with(|t| {
                if let Some((cpid, st, ztid)) = find_zombie(t, self_pid, want) {
                    reap_zombie(t, cpid);
                    let _ = ztid;
                    return WaitAct::Done(cpid, st);
                }
                if !has_child(t, self_pid, want) {
                    return WaitAct::Err(ECHILD);
                }
                if nohang {
                    return WaitAct::Done(0, 0);
                }
                s.begin_wait(&mut t.procs[self_pid as usize].wait_wq, FAR_DEADLINE);
                WaitAct::Sleep
            })
        });
        match r {
            WaitAct::Done(0, _) => return 0,
            WaitAct::Done(cpid, st) => {
                if status != 0
                    && let Some(space) = current_space()
                {
                    if syscall::check_user_ptr(|p, n| space.check_user_range(p, n), status, 4)
                        .is_err()
                    {
                        return syscall::neg(EFAULT);
                    }
                    let bytes = st.to_le_bytes();
                    if space.write_bytes(status, &bytes).is_err() {
                        return syscall::neg(EFAULT);
                    }
                }
                return cpid as i64;
            }
            WaitAct::Err(e) => return syscall::neg(e),
            WaitAct::Sleep => {
                thread_init::schedule();
                if let Some(s) = current_space() {
                    set_as(s);
                }
                apply_pending(core::ptr::null_mut());
            }
        }
    }
}

enum WaitAct {
    Done(u32, u32),
    Err(i32),
    Sleep,
}

fn find_zombie(t: &Table, parent: u32, want: i64) -> Option<(u32, u32, ThreadId)> {
    let mut i = 1usize;
    while i < MAX_PROCS {
        let p = &t.procs[i];
        if p.state == ProcState::Zombie && p.ppid == parent && (want < 0 || want == p.pid as i64) {
            return Some((p.pid, p.wait_status, p.tid));
        }
        i += 1;
    }
    None
}

fn has_child(t: &Table, parent: u32, want: i64) -> bool {
    let mut i = 1usize;
    while i < MAX_PROCS {
        let p = &t.procs[i];
        if p.state != ProcState::Unused && p.ppid == parent && (want < 0 || want == p.pid as i64) {
            return true;
        }
        i += 1;
    }
    false
}

fn reap_zombie(t: &mut Table, pid: u32) {
    let i = pid as usize;
    t.procs[i] = Proc::empty();
}

fn sys_kill(pid: u64, sig: u64) -> i64 {
    let sig = sig as u32;
    if sig == 0 || sig > 31 {
        return syscall::neg(EINVAL);
    }
    let target = pid as u32;
    let self_pid = current_pid();
    let r = with_table(|t| {
        let Some(p) = t.get_mut(target) else {
            return Err(ESRCH);
        };
        if p.state == ProcState::Unused || p.state == ProcState::Zombie {
            return Err(ESRCH);
        }
        match default_action(sig) {
            SigAct::Ign => {
                if sig == SIGCHLD {
                    p.pending |= bit(sig);
                }
                Ok((p.tid, false, false))
            }
            SigAct::Cont => {
                let was = p.state == ProcState::Stopped;
                if was {
                    p.state = ProcState::Live;
                    p.pending &= !bit(SIGSTOP);
                }
                Ok((p.tid, was, false))
            }
            SigAct::Stop => {
                p.pending |= bit(SIGSTOP);
                p.state = ProcState::Stopped;
                Ok((p.tid, false, true))
            }
            SigAct::Term => {
                p.pending |= bit(sig);
                Ok((p.tid, false, true))
            }
        }
    });
    match r {
        Err(e) => syscall::neg(e),
        Ok((tid, cont, wake)) => {
            let _ = tid;
            if cont {
                let wq = unsafe { &mut (*TABLE.as_ptr()).procs[target as usize].stop_wq };
                thread_init::wake_queue(wq);
            }
            if wake {
                let t = unsafe { &mut (*TABLE.as_ptr()).procs[target as usize] };
                thread_init::wake_queue(&mut t.wait_wq);
                thread_init::wake_queue(&mut t.stop_wq);
            }
            if target == self_pid && default_action(sig) == SigAct::Term {
                finish_exit(wait_signaled(sig), true);
            }
            0
        }
    }
}

fn sys_psinfo(buf: u64, len: u64) -> i64 {
    if let Err(e) = validate_buf(buf, len) {
        return syscall::neg(e);
    }
    let mut tmp = [0u8; 512];
    let n = format_ps(&mut tmp);
    let take = n.min(len as usize);
    let Some(space) = current_space() else {
        return syscall::neg(EFAULT);
    };
    if space.write_bytes(buf, &tmp[..take]).is_err() {
        return syscall::neg(EFAULT);
    }
    take as i64
}

fn format_ps(out: &mut [u8]) -> usize {
    let snap = with_table(|t| {
        let mut s = [(0u32, 0u32, ProcState::Unused, ""); MAX_PROCS];
        let mut n = 0usize;
        let mut i = 1usize;
        while i < MAX_PROCS {
            let p = &t.procs[i];
            if p.state != ProcState::Unused {
                s[n] = (p.pid, p.ppid, p.state, p.name);
                n += 1;
            }
            i += 1;
        }
        (s, n)
    });
    let mut w = 0usize;
    let mut i = 0usize;
    while i < snap.1 {
        let (pid, ppid, st, name) = snap.0[i];
        let line = alloc::format!("{pid} {ppid} {} {name}\n", st.name());
        let b = line.as_bytes();
        if w + b.len() > out.len() {
            break;
        }
        out[w..w + b.len()].copy_from_slice(b);
        w += b.len();
        i += 1;
    }
    w
}

pub fn write_ps(w: &mut impl Write) {
    let mut tmp = [0u8; 512];
    let n = format_ps(&mut tmp);
    let s = core::str::from_utf8(&tmp[..n]).unwrap_or("");
    for line in s.lines() {
        let _ = writeln!(w, "vibeOS: ps: {line}");
    }
}

fn sig_for_vec(vec: u8) -> Option<u32> {
    match vec {
        vectors::DE | vectors::MF | vectors::XF => Some(SIGFPE),
        vectors::UD => Some(SIGILL),
        vectors::NP | vectors::SS => Some(SIGBUS),
        vectors::GP | vectors::PF => Some(SIGSEGV),
        vectors::BP => Some(SIGTRAP),
        _ => None,
    }
}

/// User exception: default action (kill) + diagnostic. Kernel stays up.
/// No-op if this is not a user process (trampoline / no pid).
pub fn try_user_fault(vec: u8, frame: &InterruptFrame, err: u64, cr2: Option<u64>) {
    let pid = current_pid();
    if pid == 0 {
        return;
    }
    let Some(sig) = sig_for_vec(vec) else {
        return;
    };
    let _ = write!(
        Serial,
        "user: pid {pid} killed SIG{} rip=0x{:x} err=0x{err:x}",
        sig_name(sig),
        frame.rip
    );
    if let Some(c) = cr2 {
        let _ = write!(Serial, " cr2=0x{c:x}");
    }
    let _ = writeln!(Serial);
    crate::arch::gs::force_kernel();
    finish_exit(wait_signaled(sig), true);
}

const _: fn(u32) = |s| {
    let _ = wait_stopped(s);
};
