//! Process table, fork/exec/wait, fd syscalls, early signals. ROADMAP §9.5–9.7.
//!
//! Table lives under SCHED (wait/zombie). Do not hold SCHED across AS
//! clone, ELF load, or heap teardown. COW is Phase 12. User handlers are
//! Phase 13.

#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use core::fmt::Write;
use core::mem::MaybeUninit;

use vibeos::addr_space::{AddressSpace, AsError, MmapError, mmap_request};
use vibeos::arch::x86_64::trap::{self as x86_trap, Abi};
use vibeos::elf::ElfError;
use vibeos::fs::{FileId, FileRef, FsError, OpenFlags, SeekFrom};
use vibeos::kalloc::{TryBox, TryVec};
use vibeos::kbd::{DecodedKey, NamedKey};
use vibeos::lock::RANK_SCHED;
use vibeos::paging::{PAGE_SIZE_4K, USER_MAP_END};
use vibeos::proc::{
    Creds, Cwd, FD_CLOEXEC, Fd, FdKind, FdTable, INIT_PID, InitState, MAX_FDS, MAX_PROCS,
    ProcState, SIGCHLD, SIGCONT, SIGKILL, SIGSTOP, SigAct, WNOHANG, default_action,
    fd_flags_from_open, reaper_for, sig_name, wait_exited, wait_signaled, wait_stopped,
};
use vibeos::sched::FAR_DEADLINE;
use vibeos::syscall::{
    self, E2BIG, EAGAIN, EBADF, EBUSY, ECHILD, EEXIST, EFAULT, EFBIG, EINVAL, EIO, EISDIR, EMFILE,
    ENAMETOOLONG, ENODEV, ENOENT, ENOEXEC, ENOMEM, ENOSYS, ENOTDIR, EPERM, ESRCH, F_GETFD, F_SETFD,
    SYS_BRK, SYS_CLOSE, SYS_DUP, SYS_DUP2, SYS_EXECVE, SYS_EXIT, SYS_FCNTL, SYS_FORK, SYS_GETPID,
    SYS_GETPPID, SYS_KILL, SYS_LSEEK, SYS_MMAP, SYS_MUNMAP, SYS_OPEN, SYS_PSINFO, SYS_READ,
    SYS_SCHED_YIELD, SYS_WAIT4, SYS_WRITE, UserFrame,
};
use vibeos::thread::ThreadId;
use vibeos::trap::{self, FpCause, FpUnit, Ring3Action, SyscallAbi, TrapKind};
use vibeos::vectors;
use vibeos::wait::WaitQueue;

use crate::addr_space_init;
use crate::arch::idt::TrapFrame;
use crate::console_init;
use crate::file_init;
use crate::serial::Serial;
use crate::sync_init::SpinMutex;
use crate::syscall_init;
use crate::thread_init::{self, SpawnError};
use crate::user_init::{self, LoadError, Loaded};

mod exec;
mod exit;
mod fd;

pub use exit::write_ps;

use exec::{sys_brk, sys_execve, sys_fork, sys_mmap, sys_munmap};
use exit::{finish_exit, reap_zombie, sys_exit, sys_kill, sys_psinfo, sys_wait4};
use fd::{
    close_all_fds, close_fd_slot, dup_table, lookup_fd, sys_close, sys_dup, sys_dup2, sys_fcntl,
    sys_lseek, sys_open, sys_read, sys_write, validate_buf,
};

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
    space: Option<TryBox<AddressSpace>>,
    /// `FS_BASE` for the first return to ring 3 (`user_thread_entry`).
    fs_base: u64,
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
            fs_base: 0,
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

static TABLE: SpinMutex<Table> = SpinMutex::with_rank(Table::empty(), RANK_SCHED);

/// Run `f` on the process table. Callers hold SCHED, which serializes its wait queues.
fn table_locked<R>(f: impl FnOnce(&mut Table) -> R) -> R {
    // pair order: thread_init::SCHED, then TABLE
    let mut g = TABLE.lock_nested(1);
    f(&mut g)
}

fn with_table<R>(f: impl FnOnce(&mut Table) -> R) -> R {
    thread_init::with_sched(|_| table_locked(f))
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
        FsError::NoMem => ENOMEM,
        FsError::Loop | FsError::NotEmpty | FsError::NotSupp => EINVAL,
    }
}

fn load_errno(e: LoadError) -> i32 {
    match e {
        LoadError::Fs(f) => fs_errno(f),
        LoadError::Elf(ElfError::ImageTooBig) => ENOMEM,
        LoadError::Elf(_) => ENOEXEC,
        LoadError::As(_) | LoadError::TooBig => ENOMEM,
        LoadError::Mem(_) => EFAULT,
        LoadError::Empty => ENOEXEC,
        LoadError::NoProc => EAGAIN,
        LoadError::Spawn(e) => spawn_errno(e),
        LoadError::NoMem => ENOMEM,
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

fn user_thread_entry() {
    let pid = current_pid();
    let fs = with_table(|t| {
        t.get(pid).map(|p| {
            if let Some(ref s) = p.space {
                set_as(s);
            }
            p.fs_base
        })
    });
    let Some(fs) = fs else {
        thread_init::exit_current();
    };
    // SAFETY: invariant I25: this is a user thread whose kernel stack
    // holds at its top the user frame its creator wrote, and its CR3 maps
    // that frame's RIP and RSP; established by `thread_init::spawn_user`.
    unsafe { syscall_init::first_return(fs) };
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
    let slot = space_slot().ok_or(LoadError::NoMem)?;
    start_loaded(
        slot,
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
    let slot = space_slot().ok_or(LoadError::NoMem)?;
    start_loaded(slot, user_init::load_image(elf, argv)?, 0, ppid, name)
}

/// The heap slot a new address space moves into, taken before the space
/// is built: `AddressSpace` has no `Drop`, so a space that a failed
/// `TryBox::try_new` dropped would leak its frames.
fn space_slot() -> Option<TryBox<MaybeUninit<AddressSpace>>> {
    TryBox::<AddressSpace>::try_new_uninit().ok()
}

fn start_loaded(
    slot: TryBox<MaybeUninit<AddressSpace>>,
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
    let frame = UserFrame::new_user(loaded.entry, loaded.rsp);
    let fs = loaded.fs;
    let boxed = slot.write(loaded.space);
    let h = match thread_init::spawn_user(name, user_thread_entry, pid, cr3, &frame) {
        Ok(h) => h,
        Err(e) => {
            with_table(|t| t.procs[pid as usize] = Proc::empty());
            addr_space_init::teardown(boxed.into_inner());
            return Err(LoadError::Spawn(e));
        }
    };
    with_table(|t| {
        init_slot(t, pid, ppid, name);
        t.procs[pid as usize].space = Some(boxed);
        t.procs[pid as usize].fs_base = fs;
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
            table_locked(|t| {
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

/// Install the process layer's hooks in the layers below it (DESIGN §1.2):
/// the ring-3 fault hook in `arch::idt` and the syscall handler in
/// `syscall_init`. `_start` calls it right after `syscall_init::init_bsp`,
/// before the first ring-3 entry.
pub fn init() {
    crate::arch::idt::set_user_fault_hook(try_user_fault);
    syscall_init::set_syscall_handler(syscall);
}

/// A syscall from ring 3, over the user frame its entry saved.
pub fn syscall(frame: &mut UserFrame) -> i64 {
    #[cfg(feature = "kernel_tests")]
    testing::on_entry(frame);
    apply_pending(Some(&mut *frame));
    let nr = Abi::nr(frame);
    let args: [u64; 6] = core::array::from_fn(|i| Abi::arg(frame, i));
    let ret = dispatch_frame(nr, args, Some(frame));
    if syscall_init::trace_enabled() {
        let name = syscall::info(nr).map(|i| i.name).unwrap_or("?");
        let _ = writeln!(Serial, "user: syscall {name} nr={nr} = {ret}");
    }
    ret
}

pub fn dispatch(nr: u64, args: [u64; 6]) -> i64 {
    dispatch_frame(nr, args, None)
}

fn dispatch_frame(nr: u64, args: [u64; 6], frame: Option<&mut UserFrame>) -> i64 {
    match nr {
        SYS_READ => sys_read(args[0], args[1], args[2]),
        SYS_WRITE => sys_write(args[0], args[1], args[2]),
        SYS_OPEN => sys_open(args[0], args[1], args[2]),
        SYS_CLOSE => sys_close(args[0]),
        SYS_LSEEK => sys_lseek(args[0], args[1], args[2]),
        SYS_MMAP => sys_mmap(args[0], args[1], args[2], args[3], args[4], args[5]),
        SYS_MUNMAP => sys_munmap(args[0], args[1]),
        SYS_BRK => sys_brk(args[0]),
        SYS_DUP => sys_dup(args[0]),
        SYS_DUP2 => sys_dup2(args[0], args[1]),
        SYS_GETPID => sys_getpid(),
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

/// Act on this process's pending signals at syscall entry. Each turn is
/// one SCHED section: a stop sets Stopped and arms the `stop_wq` wait in
/// the section that decides it, so a `SIGCONT` that `sys_kill` sends in
/// between finds the thread on the queue (ROADMAP §10.6, F033). The loop
/// re-checks after every `schedule()`, which can return early.
fn apply_pending(frame: Option<&mut UserFrame>) {
    let pid = current_pid();
    if pid == 0 {
        return;
    }
    loop {
        let act = thread_init::with_sched(|s| {
            table_locked(|t| {
                let Some(p) = t.get_mut(pid) else {
                    return Pending::None;
                };
                if p.pending & bit(SIGKILL) != 0 {
                    return Pending::Die(SIGKILL);
                }
                let mut stop = false;
                if p.state == ProcState::Stopped || p.pending & bit(SIGSTOP) != 0 {
                    p.pending &= !bit(SIGSTOP);
                    stop = true;
                } else {
                    let pend = p.pending;
                    let mut sig = 1u32;
                    while sig <= 31 && !stop {
                        if pend & bit(sig) != 0 && sig != SIGCHLD && sig != SIGCONT {
                            match default_action(sig) {
                                SigAct::Term => return Pending::Die(sig),
                                SigAct::Stop => {
                                    p.pending &= !bit(sig);
                                    stop = true;
                                }
                                SigAct::Ign | SigAct::Cont => {
                                    p.pending &= !bit(sig);
                                }
                            }
                        }
                        sig += 1;
                    }
                }
                if !stop {
                    return Pending::None;
                }
                p.state = ProcState::Stopped;
                s.begin_wait(&mut p.stop_wq, FAR_DEADLINE);
                #[cfg(feature = "kernel_tests")]
                testing::stop_decided(pid);
                Pending::Stop
            })
        });
        match act {
            Pending::None => return,
            Pending::Die(sig) => {
                let _ = frame;
                finish_exit(wait_signaled(sig), true);
            }
            Pending::Stop => {
                #[cfg(feature = "kernel_tests")]
                testing::stop_stall(pid);
                thread_init::schedule();
            }
        }
    }
}

enum Pending {
    None,
    Die(u32),
    Stop,
}

fn sys_getpid() -> i64 {
    let pid = current_pid();
    #[cfg(feature = "kernel_tests")]
    {
        testing::getpid_spin(pid);
        testing::on_getpid(pid);
    }
    pid as i64
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

/// The signal and `si_code` for a trap raised by ring-3 code: the port's
/// decode of the vector and error code, the cause refined from the frame's
/// DR6 (`#DB`) or the thread's FSW and MXCSR (`#MF`, `#XM`), then the one
/// table in `vibeos::trap` (DESIGN §5.2). `None`: not a ring-3 fault.
fn sig_for_vec(f: &TrapFrame) -> Option<(u32, i32)> {
    let kind = match x86_trap::decode(f.vector as u8, f.error_code) {
        TrapKind::Debug(_) => TrapKind::Debug(vectors::dr6_cause(f.dr6)),
        TrapKind::FloatingPoint(unit, _) => {
            let cause =
                syscall_init::current_fp_words().map_or(FpCause::Unknown, |(fsw, fcw, mx)| {
                    match unit {
                        FpUnit::X87 => x86_trap::fp_cause(fsw, fcw),
                        FpUnit::Simd => x86_trap::fp_cause(mx & 0x3F, (mx >> 7) & 0x3F),
                    }
                });
            TrapKind::FloatingPoint(unit, cause)
        }
        k => k,
    };
    match trap::ring3_action(kind) {
        Ring3Action::Signal { sig, si_code } => Some((sig, si_code)),
        Ring3Action::NotRing3 => None,
    }
}

/// User exception: default action (kill) + diagnostic. Kernel stays up.
/// No-op if this is not a user process (trampoline / no pid), or the
/// vector is not a ring-3 fault.
pub fn try_user_fault(f: &TrapFrame) {
    let pid = current_pid();
    if pid == 0 {
        return;
    }
    let Some((sig, _si_code)) = sig_for_vec(f) else {
        return;
    };
    // One `writeln!`, so one IF-off region (`Serial::write_fmt`): the body
    // runs with IF=1, and a thread that ran on this CPU between two writes
    // would land inside this line in the CPU's log capture stage.
    let (name, rip, err) = (sig_name(sig), f.user().rip, f.error_code);
    let _ = if f.vector == u64::from(vectors::PF) {
        writeln!(
            Serial,
            "user: pid {pid} killed SIG{name} rip=0x{rip:x} err=0x{err:x} cr2=0x{:x}",
            f.cr2
        )
    } else {
        writeln!(
            Serial,
            "user: pid {pid} killed SIG{name} rip=0x{rip:x} err=0x{err:x}"
        )
    };
    #[cfg(feature = "kernel_tests")]
    {
        testing::kill_line_yield(f);
        testing::kill_line_done();
    }
    crate::arch::gs::force_kernel();
    finish_exit(wait_signaled(sig), true);
}

const _: fn(u32) = |s| {
    let _ = wait_stopped(s);
};

/// In-guest test hooks. `kernel_tests` only (AGENTS.md rule 9).
#[cfg(feature = "kernel_tests")]
pub(crate) mod testing {
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

    use vibeos::proc::MAX_PROCS;
    use vibeos::syscall::{SYS_GETPID, UserFrame};

    use crate::arch::idt::TrapFrame;
    use crate::per_cpu_init;
    use crate::thread_init;
    use crate::time_init;

    /// `cr2` of the `#PF` whose kill line yields once; 0 for none.
    static KILL_YIELD_CR2: AtomicU64 = AtomicU64::new(0);
    /// Kill lines `try_user_fault` has finished writing since boot.
    static KILL_LINES: AtomicU64 = AtomicU64::new(0);

    /// The next CPL-3 `#PF` at `cr2` that ends in a kill yields in
    /// `try_user_fault` once its kill line is written, until another kill
    /// line is written, for at most 1 s of TSC time: a kill line written
    /// in pieces would take the other line inside it.
    pub(crate) fn arm_kill_line_yield(cr2: u64) {
        KILL_YIELD_CR2.store(cr2, Ordering::Release);
    }

    pub(crate) fn disarm_kill_line_yield() {
        KILL_YIELD_CR2.store(0, Ordering::Release);
    }

    pub(super) fn kill_line_yield(f: &TrapFrame) {
        let armed = KILL_YIELD_CR2.load(Ordering::Acquire);
        if armed == 0
            || f.vector != u64::from(vibeos::vectors::PF)
            || f.cr2 != armed
            || KILL_YIELD_CR2
                .compare_exchange(armed, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let n0 = KILL_LINES.load(Ordering::Acquire);
        let t0 = time_init::now_ns();
        while KILL_LINES.load(Ordering::Acquire) == n0
            && time_init::now_ns().saturating_sub(t0) < 1_000_000_000
        {
            thread_init::yield_now();
        }
    }

    pub(super) fn kill_line_done() {
        KILL_LINES.fetch_add(1, Ordering::AcqRel);
    }

    static GETPIDS: [AtomicU64; MAX_PROCS] = [const { AtomicU64::new(0) }; MAX_PROCS];

    /// `getpid` calls `pid` has made since boot.
    pub(crate) fn getpid_count(pid: u32) -> u64 {
        GETPIDS
            .get(pid as usize)
            .map_or(0, |c| c.load(Ordering::Acquire))
    }

    pub(super) fn on_getpid(pid: u32) {
        if let Some(c) = GETPIDS.get(pid as usize) {
            c.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// The pid the next stop stall holds; 0 for none.
    static STALL_PID: AtomicU32 = AtomicU32::new(0);
    static STALL_IN: AtomicBool = AtomicBool::new(false);
    static STALL_RELEASE: AtomicBool = AtomicBool::new(false);

    /// Hold `pid` once, at its next stop, between the stop decision and
    /// its sleep, until [`release_stop_stall`] or 1 s of TSC time.
    pub(crate) fn arm_stop_stall(pid: u32) {
        STALL_IN.store(false, Ordering::Release);
        STALL_RELEASE.store(false, Ordering::Release);
        STALL_PID.store(pid, Ordering::Release);
    }

    /// The armed process has decided to stop and armed its `stop_wq` wait:
    /// it sits between its stop decision and its sleep.
    pub(crate) fn stop_stalled() -> bool {
        STALL_IN.load(Ordering::Acquire)
    }

    pub(crate) fn release_stop_stall() {
        STALL_RELEASE.store(true, Ordering::Release);
    }

    pub(crate) fn disarm_stop_stall() {
        STALL_PID.store(0, Ordering::Release);
        STALL_RELEASE.store(true, Ordering::Release);
    }

    /// Called in the SCHED section that arms a stopping process's
    /// `stop_wq` wait: marks the stall when `pid` is the armed process. A
    /// tick after that section parks the thread, since a preempted
    /// `Blocked` thread is not requeued, and [`stop_stall`] would then run
    /// only after the `SIGCONT` that the sender sends once it sees the
    /// stall; so the stall is marked here, before IF can come back on.
    pub(super) fn stop_decided(pid: u32) {
        if STALL_PID.load(Ordering::Acquire) == pid {
            STALL_IN.store(true, Ordering::Release);
        }
    }

    /// Spins on TSC time; never services IPIs.
    pub(super) fn stop_stall(pid: u32) {
        if STALL_PID
            .compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        STALL_IN.store(true, Ordering::Release);
        let t0 = time_init::now_ns();
        while !STALL_RELEASE.load(Ordering::Acquire)
            && time_init::now_ns().saturating_sub(t0) < 1_000_000_000
        {
            core::hint::spin_loop();
        }
    }

    static SPIN_ARMED: AtomicBool = AtomicBool::new(false);
    static SPIN_START_NS: AtomicU64 = AtomicU64::new(0);
    static SPIN_DONE_NS: AtomicU64 = AtomicU64::new(0);
    static SPIN_SW_START: AtomicU64 = AtomicU64::new(0);
    static SPIN_SW_END: AtomicU64 = AtomicU64::new(0);

    /// TSC time of the spin, in the body of one user `getpid`.
    pub(crate) const SPIN_NS: u64 = 50_000_000;

    /// The next `getpid` from a process on CPU 0 spins for [`SPIN_NS`].
    pub(crate) fn arm_getpid_spin() {
        SPIN_START_NS.store(0, Ordering::Release);
        SPIN_DONE_NS.store(0, Ordering::Release);
        SPIN_ARMED.store(true, Ordering::Release);
    }

    pub(crate) fn disarm_getpid_spin() {
        SPIN_ARMED.store(false, Ordering::Release);
    }

    /// `now_ns` when the spin started; 0 before.
    pub(crate) fn spin_start_ns() -> u64 {
        SPIN_START_NS.load(Ordering::Acquire)
    }

    /// `now_ns` when the spinning `getpid` was done; 0 before.
    pub(crate) fn spin_done_ns() -> u64 {
        SPIN_DONE_NS.load(Ordering::Acquire)
    }

    /// CPU 0's context switches at the spin's start and end.
    pub(crate) fn spin_switches() -> (u64, u64) {
        (
            SPIN_SW_START.load(Ordering::Acquire),
            SPIN_SW_END.load(Ordering::Acquire),
        )
    }

    fn cpu0_switches() -> u64 {
        per_cpu_init::cpu(0).map_or(0, |c| c.switches.load(Ordering::Relaxed))
    }

    /// Spins on TSC time; never services IPIs.
    pub(super) fn getpid_spin(pid: u32) {
        if pid == 0 || !SPIN_ARMED.load(Ordering::Acquire) {
            return;
        }
        if thread_init::current_cpu() != 0 || !SPIN_ARMED.swap(false, Ordering::AcqRel) {
            return;
        }
        SPIN_SW_START.store(cpu0_switches(), Ordering::Release);
        let t0 = time_init::now_ns();
        SPIN_START_NS.store(t0.max(1), Ordering::Release);
        while time_init::now_ns().saturating_sub(t0) < SPIN_NS {
            core::hint::spin_loop();
        }
        SPIN_SW_END.store(cpu0_switches(), Ordering::Release);
        SPIN_DONE_NS.store(time_init::now_ns(), Ordering::Release);
    }

    static WRITE_ARMED: AtomicBool = AtomicBool::new(false);
    static WRITE_DONE_NS: AtomicU64 = AtomicU64::new(0);

    /// Record when the next console `write` returns.
    pub(crate) fn arm_console_write_record() {
        WRITE_DONE_NS.store(0, Ordering::Release);
        WRITE_ARMED.store(true, Ordering::Release);
    }

    /// `now_ns` when the armed console `write` returned; 0 before.
    pub(crate) fn console_write_done_ns() -> u64 {
        WRITE_DONE_NS.load(Ordering::Acquire)
    }

    pub(super) fn console_write_returned() {
        if WRITE_ARMED.swap(false, Ordering::AcqRel) {
            WRITE_DONE_NS.store(time_init::now_ns().max(1), Ordering::Release);
        }
    }

    /// The `rdi` of a `getpid` that the entry hooks below act on: a test
    /// program's own call, armed before the program exists.
    pub(crate) const HOOK_MAGIC: u64 = 0x5EED_CA11_0000_5A21;

    /// The canary the next marked `getpid` writes into its frame's `rcx`;
    /// 0 for none.
    static RCX_CANARY: AtomicU64 = AtomicU64::new(0);

    /// The next `getpid` whose `rdi` is [`HOOK_MAGIC`] returns with `rcx`
    /// = `canary` in its user frame.
    pub(crate) fn arm_rcx_canary(canary: u64) {
        RCX_CANARY.store(canary, Ordering::Release);
    }

    static APIC_ARMED: AtomicBool = AtomicBool::new(false);
    static APIC_CHECKS: AtomicU32 = AtomicU32::new(0);
    static APIC_MISMATCHES: AtomicU32 = AtomicU32::new(0);

    /// At each `getpid` whose `rdi` is [`HOOK_MAGIC`], compare this CPU's
    /// `PerCpu` LAPIC ID with the one CPUID reports for the CPU running.
    pub(crate) fn arm_getpid_apic() {
        APIC_CHECKS.store(0, Ordering::Release);
        APIC_MISMATCHES.store(0, Ordering::Release);
        APIC_ARMED.store(true, Ordering::Release);
    }

    pub(crate) fn apic_checks() -> u32 {
        APIC_CHECKS.load(Ordering::Acquire)
    }

    pub(crate) fn apic_mismatches() -> u32 {
        APIC_MISMATCHES.load(Ordering::Acquire)
    }

    /// Undo every `arm_*` hook of this group.
    pub(crate) fn disarm() {
        RCX_CANARY.store(0, Ordering::Release);
        APIC_ARMED.store(false, Ordering::Release);
    }

    /// Top of `proc_init::syscall`, before the dispatch.
    pub(super) fn on_entry(frame: &mut UserFrame) {
        if frame.orig_rax != SYS_GETPID || frame.rdi != HOOK_MAGIC {
            return;
        }
        let c = RCX_CANARY.swap(0, Ordering::AcqRel);
        if c != 0 {
            frame.rcx = c;
        }
        if APIC_ARMED.load(Ordering::Acquire) {
            // One IF=0 stretch: the `PerCpu` read and CPUID see one CPU.
            let _g = crate::x86::InterruptGuard::enter();
            let mine = per_cpu_init::current()
                .remote
                .apic_id
                .load(Ordering::Relaxed);
            let (_, ebx, _, _) = crate::x86::cpuid(1, 0);
            APIC_CHECKS.fetch_add(1, Ordering::AcqRel);
            if ebx >> 24 != mine {
                APIC_MISMATCHES.fetch_add(1, Ordering::AcqRel);
            }
        }
    }
}
