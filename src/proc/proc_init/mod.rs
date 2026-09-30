//! Process table, fork/exec/wait, fd syscalls, early signals. ROADMAP §9.5–9.7.
//!
//! Table lives under SCHED (wait/zombie). Do not hold SCHED across AS
//! clone, ELF load, or heap teardown. COW is Phase 12. User handlers are
//! Phase 13.

use core::fmt::Write;
use core::mem::MaybeUninit;

use vibeos::addr_space::{AddressSpace, AsError, MmapError, mmap_request};
use vibeos::arch::x86_64::trap::{self as x86_trap, Abi};
use vibeos::fs::{FileId, FileRef, FsError, OpenFlags, SeekFrom};
use vibeos::kalloc::{TryBox, TryVec};
use vibeos::kbd::{DecodedKey, NamedKey};
use vibeos::kerror::KError;
use vibeos::lock::RANK_SCHED;
use vibeos::paging::{PAGE_SIZE_4K, USER_MAP_END};
use vibeos::proc::pid::IdIndex;
use vibeos::proc::uaccess::user_range_ok;
use vibeos::proc::{
    Creds, Cwd, FD_CLOEXEC, Fd, FdKind, FdTable, INIT_PID, InitState, MAX_FDS, MAX_PROCS,
    ProcState, SIGCHLD, SIGCONT, SIGKILL, SIGSTOP, SigAct, WNOHANG, default_action,
    fd_flags_from_open, reaper_for, sig_name, wait_exited, wait_signaled,
};
use vibeos::sched::FAR_DEADLINE;
use vibeos::syscall::{self, F_GETFD, F_SETFD, Handlers, SysResult, UserFrame};
use vibeos::thread::ThreadId;
use vibeos::trap::{self, FpCause, FpUnit, Ring3Action, SyscallAbi, TrapKind};
use vibeos::vectors;
use vibeos::wait::WaitQueue;

use crate::addr_space_init;
use crate::arch::idt::TrapFrame;
use crate::console_init;
use crate::file_init;
use crate::proc::uaccess_init;
use crate::serial::Serial;
use crate::sync_init::SpinMutex;
use crate::syscall_init;
use crate::thread_init::{self, Sched};
use crate::user_init;
#[cfg(not(feature = "vibefs_crash"))]
use crate::user_init::{LoadError, Loaded};

mod exec;
mod exit;
mod fd;

pub use exit::write_ps;

use exec::{sys_brk, sys_execve, sys_fork, sys_mmap, sys_munmap};
#[cfg(not(feature = "vibefs_crash"))]
use exit::reap_zombie;
use exit::{finish_exit, sys_exit, sys_kill, sys_psinfo, sys_wait4};
use fd::{
    close_all_fds, close_dropped, dup_table, lookup_fd, sys_close, sys_dup, sys_dup2, sys_fcntl,
    sys_lseek, sys_open, sys_read, sys_write,
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

/// Entries in the pid-to-slot index: twice the process table, so probe
/// runs stay short.
const PROC_INDEX_CAP: usize = (2 * MAX_PROCS).next_power_of_two();

/// The process table. A pid is not a slot index (DESIGN §2.11 rule 4): it
/// comes from `thread_init`'s id allocator, and `index` maps it to its slot.
struct Table {
    procs: [Proc; MAX_PROCS],
    index: IdIndex<PROC_INDEX_CAP>,
    /// The kernel's wait queue, on which [`wait_kernel`] sleeps for ppid-0
    /// processes.
    kernel_wq: WaitQueue,
}

impl Table {
    const fn empty() -> Self {
        Self {
            procs: [const { Proc::empty() }; MAX_PROCS],
            index: IdIndex::new(),
            kernel_wq: WaitQueue::new(),
        }
    }

    /// `pid`'s slot, for a process that is not `Unused`.
    fn slot_of(&self, pid: u32) -> Option<usize> {
        let i = self.index.get(pid)?;
        let p = self.procs.get(i)?;
        (p.state != ProcState::Unused && p.pid == pid).then_some(i)
    }

    fn get(&self, pid: u32) -> Option<&Proc> {
        let i = self.slot_of(pid)?;
        self.procs.get(i)
    }

    fn get_mut(&mut self, pid: u32) -> Option<&mut Proc> {
        let i = self.slot_of(pid)?;
        self.procs.get_mut(i)
    }

    /// The wait queue a child of `ppid` wakes: the kernel's for ppid 0.
    fn parent_wq(&mut self, ppid: u32) -> Option<&mut WaitQueue> {
        if ppid == 0 {
            Some(&mut self.kernel_wq)
        } else {
            self.get_mut(ppid).map(|p| &mut p.wait_wq)
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

/// Run `f` on the scheduler and the process table, which a pid's
/// allocation and release need together.
fn with_sched_table<R>(f: impl FnOnce(&mut Sched, &mut Table) -> R) -> R {
    thread_init::with_sched(|s| table_locked(|t| f(s, t)))
}

fn intern_name(b: &[u8]) -> &'static str {
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
    // SAFETY: a process's space is replaced or taken only by its own thread
    // (`proc_init::sys_execve`, `proc_init::finish_exit`), and every caller
    // reads its own process's space or one whose thread is stopped, so the
    // box outlives the borrow; established by `proc_init::current_space`.
    .map(|p| unsafe { &*p })
}

fn set_as(space: &AddressSpace) {
    syscall_init::set_user_as(space);
}

fn clear_as() {
    syscall_init::clear_user_as();
}

/// Take a process-table slot and a pid for a new process, `Live`. The pid
/// is `INIT_PID` when `prefer` asks for it and no pid-1 process exists,
/// taking over `thread_init::init_bootstrap`'s hold, else a new id.
fn alloc_pid(prefer: u32) -> Option<u32> {
    with_sched_table(|s, t| {
        let slot = t.procs.iter().position(|p| p.state == ProcState::Unused)?;
        let pid = if prefer == INIT_PID && t.get(INIT_PID).is_none() {
            if !s.id_in_use(INIT_PID) && !s.hold_id(INIT_PID) {
                return None;
            }
            INIT_PID
        } else {
            s.alloc_id()?
        };
        if !t.index.insert(pid, slot) {
            s.free_id(pid);
            return None;
        }
        let p = t.procs.get_mut(slot)?;
        p.state = ProcState::Live;
        p.pid = pid;
        Some(pid)
    })
}

/// Give back `pid`'s slot and its pid: out of the index, the slot `Unused`,
/// and the process's use of the id dropped (the id is free once its thread's
/// slot is reused too).
fn release_pid(s: &mut Sched, t: &mut Table, pid: u32) {
    let Some(i) = t.index.remove(pid) else {
        return;
    };
    if let Some(p) = t.procs.get_mut(i) {
        *p = Proc::empty();
    }
    s.free_id(pid);
}

fn init_slot(t: &mut Table, pid: u32, ppid: u32, name: &'static str) {
    let Some(p) = t.get_mut(pid) else {
        return;
    };
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

#[cfg(not(any(
    feature = "kernel_tests",
    feature = "vibefs_crash",
    feature = "kernel_shell"
)))]
pub fn start_init() {
    use vibeos::boot::cmdline::{CMDLINE_MAX, INIT_ARGV_MAX, INIT_ENVP_MAX};
    // Init's argv and envp come from the kernel command line (BOOT.md §3.2).
    let mut words = [0u8; CMDLINE_MAX];
    let v = crate::boot::cmdline().init_vectors(b"/sbin/init", &mut words);
    if v.dropped != 0 {
        crate::klog!(
            vibeos::log::Level::Warn,
            "vibeOS: boot: cmdline: {} init words dropped (at most {} args, {} env)",
            v.dropped,
            INIT_ARGV_MAX,
            INIT_ENVP_MAX
        );
    }
    match spawn_elf(b"/sbin/init", v.argv(), v.envp(), INIT_PID, 0) {
        Ok(_) => {}
        Err(e) => {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a write to Serial cannot fail (DESIGN §2.5)"
            )]
            let _ = writeln!(Serial, "user: init failed: {}", e.as_str());
        }
    }
}

/// Start the ELF at `path` with `argv` (`[path]` when empty) and `envp`
/// as a new process with parent `ppid` (0: the kernel, which reaps it with
/// [`wait_kernel`]).
#[cfg(not(feature = "vibefs_crash"))]
pub(crate) fn spawn_elf(
    path: &[u8],
    argv: &[&[u8]],
    envp: &[&[u8]],
    prefer: u32,
    ppid: u32,
) -> Result<u32, LoadError> {
    start_loaded(
        user_init::load_path(path, argv, envp)?,
        prefer,
        ppid,
        intern_name(path),
    )
}

/// Start the in-memory ELF image `elf` with `argv` as a new process with
/// parent `ppid` (0: the kernel, which reaps it with [`wait_kernel`]).
/// The in-guest tests' ring-3 entry (C-RING3).
#[cfg(feature = "kernel_tests")]
pub(crate) fn spawn_image(elf: &[u8], argv: &[&[u8]], ppid: u32) -> Result<u32, LoadError> {
    let name = argv.first().map_or("user", |a| intern_name(a));
    start_loaded(user_init::load_image(elf, argv)?, 0, ppid, name)
}

/// The heap slot a new address space moves into, taken before the space
/// is built: `AddressSpace` has no `Drop`, so a space that a failed
/// `TryBox::try_new` dropped would leak its frames.
fn space_slot() -> Option<TryBox<MaybeUninit<AddressSpace>>> {
    TryBox::<AddressSpace>::try_new_uninit().ok()
}

#[cfg(not(feature = "vibefs_crash"))]
fn start_loaded(
    loaded: Loaded,
    prefer: u32,
    ppid: u32,
    name: &'static str,
) -> Result<u32, LoadError> {
    let pid = match alloc_pid(prefer) {
        Some(p) => p,
        None => {
            addr_space_init::teardown(loaded.space.into_inner());
            return Err(LoadError::NoProc);
        }
    };
    let cr3 = loaded.space.root().as_u64();
    let frame = UserFrame::new_user(loaded.entry, loaded.rsp);
    let fs = loaded.fs;
    let boxed = loaded.space;
    let h = match thread_init::spawn_user(name, user_thread_entry, pid, cr3, &frame) {
        Ok(h) => h,
        Err(e) => {
            with_sched_table(|s, t| release_pid(s, t, pid));
            addr_space_init::teardown(boxed.into_inner());
            return Err(LoadError::Spawn(e));
        }
    };
    with_table(|t| {
        init_slot(t, pid, ppid, name);
        if let Some(p) = t.get_mut(pid) {
            p.space = Some(boxed);
            p.fs_base = fs;
            p.tid = h.id();
        }
    });
    thread_init::make_ready(h.id());
    Ok(pid)
}

#[cfg(not(feature = "vibefs_crash"))]
enum KernelWait {
    Done(u32),
    Sleep,
    NotKernelChild,
}

/// Block until `pid`, a process whose parent is the kernel (ppid 0),
/// exits; reap it and return its `wait4` status word. Returns once the
/// zombie is reaped, before its address space and kernel stack are freed.
#[cfg(not(feature = "vibefs_crash"))]
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
                        reap_zombie(s, t, pid);
                        KernelWait::Done(st)
                    }
                    ProcState::Live | ProcState::Stopped => {
                        s.begin_wait(&mut t.kernel_wq, FAR_DEADLINE);
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
    let ret = syscall::encode(dispatch_frame(nr, args, Some(frame)));
    if syscall_init::trace_enabled() {
        let name = syscall::x86_64::TABLE
            .lookup(nr)
            .map_or("?", |s| s.row().name);
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a write to Serial cannot fail (DESIGN §2.5)"
        )]
        let _ = writeln!(Serial, "user: syscall {name} nr={nr} = {ret}");
    }
    ret
}

/// A syscall from kernel code, with no user frame: the in-guest tests'.
#[cfg(feature = "kernel_tests")]
pub fn dispatch(nr: u64, args: [u64; 6]) -> i64 {
    syscall::encode(dispatch_frame(nr, args, None))
}

fn dispatch_frame(nr: u64, args: [u64; 6], frame: Option<&mut UserFrame>) -> SysResult {
    syscall::x86_64::dispatch(&mut Ctx { frame }, nr, &args)
}

/// The running syscall's context: the user frame, which `fork` copies and
/// `execve` rewrites; `None` for a kernel-side `dispatch` probe.
struct Ctx<'a> {
    frame: Option<&'a mut UserFrame>,
}

impl Handlers for Ctx<'_> {
    fn read(&mut self, fd: u32, buf: u64, count: usize) -> SysResult {
        sys_read(fd, buf, count)
    }

    fn write(&mut self, fd: u32, buf: u64, count: usize) -> SysResult {
        sys_write(fd, buf, count)
    }

    fn open(&mut self, pathname: u64, flags: i32, mode: u16) -> SysResult {
        sys_open(pathname, flags, mode)
    }

    fn close(&mut self, fd: u32) -> SysResult {
        sys_close(fd)
    }

    fn lseek(&mut self, fd: u32, offset: i64, whence: u32) -> SysResult {
        sys_lseek(fd, offset, whence)
    }

    fn mmap(
        &mut self,
        addr: u64,
        length: u64,
        prot: u64,
        flags: u64,
        fd: u64,
        offset: u64,
    ) -> SysResult {
        sys_mmap(addr, length, prot, flags, fd, offset)
    }

    fn munmap(&mut self, addr: u64, length: usize) -> SysResult {
        sys_munmap(addr, length)
    }

    fn brk(&mut self, addr: u64) -> SysResult {
        sys_brk(addr)
    }

    fn sched_yield(&mut self) -> SysResult {
        if current_pid() != 0 {
            thread_init::yield_now();
        }
        Ok(0)
    }

    fn dup(&mut self, oldfd: u32) -> SysResult {
        sys_dup(oldfd)
    }

    fn dup2(&mut self, oldfd: u32, newfd: u32) -> SysResult {
        sys_dup2(oldfd, newfd)
    }

    fn getpid(&mut self) -> SysResult {
        sys_getpid()
    }

    fn fork(&mut self) -> SysResult {
        sys_fork(self.frame.as_deref_mut())
    }

    fn execve(&mut self, pathname: u64, argv: u64, envp: u64) -> SysResult {
        sys_execve(pathname, argv, envp, self.frame.as_deref_mut())
    }

    fn exit(&mut self, status: i32) -> SysResult {
        if current_pid() == 0 {
            Ok(0)
        } else {
            sys_exit(status, false)
        }
    }

    fn wait4(&mut self, pid: i32, wstatus: u64, options: i32, _rusage: u64) -> SysResult {
        sys_wait4(pid, wstatus, options)
    }

    fn kill(&mut self, pid: i32, sig: i32) -> SysResult {
        sys_kill(pid, sig)
    }

    fn fcntl(&mut self, fd: u32, cmd: u32, arg: u64) -> SysResult {
        sys_fcntl(fd, cmd, arg)
    }

    fn getppid(&mut self) -> SysResult {
        Ok(with_table(|t| t.get(current_pid()).map(|p| p.ppid).unwrap_or(0)) as usize)
    }

    fn psinfo(&mut self, buf: u64, len: usize) -> SysResult {
        sys_psinfo(buf, len)
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

fn sys_getpid() -> SysResult {
    let pid = current_pid();
    #[cfg(feature = "kernel_tests")]
    {
        testing::getpid_spin(pid);
        testing::on_getpid(pid);
    }
    Ok(pid as usize)
}

fn copy_user_str(va: u64, out: &mut [u8]) -> Result<usize, KError> {
    match uaccess_init::strncpy_from_user(out, va) {
        Ok(n) if n == out.len() => Err(KError::NameTooLong),
        Ok(n) => Ok(n),
        Err(f) => Err(KError::from(f)),
    }
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
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to Serial cannot fail (DESIGN §2.5)"
    )]
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

    /// `getpid` counts per bucket `pid % MAX_PROCS` (a pid is not a slot
    /// index), each tagged with the pid it counts for: a pid that finds
    /// another's tag starts the bucket again.
    static GETPID_TAGS: [AtomicU32; MAX_PROCS] = [const { AtomicU32::new(0) }; MAX_PROCS];
    static GETPIDS: [AtomicU64; MAX_PROCS] = [const { AtomicU64::new(0) }; MAX_PROCS];

    fn getpid_bucket(pid: u32) -> usize {
        pid as usize % MAX_PROCS
    }

    /// `getpid` calls `pid` has made since its bucket last changed hands.
    pub(crate) fn getpid_count(pid: u32) -> u64 {
        let b = getpid_bucket(pid);
        match (GETPID_TAGS.get(b), GETPIDS.get(b)) {
            (Some(tag), Some(c)) if tag.load(Ordering::Acquire) == pid => c.load(Ordering::Acquire),
            _ => 0,
        }
    }

    pub(super) fn on_getpid(pid: u32) {
        let b = getpid_bucket(pid);
        let (Some(tag), Some(c)) = (GETPID_TAGS.get(b), GETPIDS.get(b)) else {
            return;
        };
        if tag.load(Ordering::Acquire) != pid {
            c.store(0, Ordering::Release);
            tag.store(pid, Ordering::Release);
        }
        c.fetch_add(1, Ordering::AcqRel);
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
