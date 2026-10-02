//! Process table, fork/exec/wait, fd syscalls, early signals. ROADMAP §9.5–9.7.
//!
//! Table lives under SCHED (wait/zombie). Do not hold SCHED across AS
//! clone, ELF load, or heap teardown. COW is Phase 12. User handlers are
//! Phase 13.

use core::fmt::Write;

use vibeos::addr_space::{AsError, MmapError, mmap_request};
#[cfg(target_arch = "x86_64")]
use vibeos::arch::x86_64::trap as x86_trap;
use vibeos::fs::{DirRef, FileId, FileRef, FsError, OpenFlags, SeekFrom, WalkBase};
use vibeos::kalloc::{AllocError, TryVec};
use vibeos::kbd::{DecodedKey, NamedKey};
use vibeos::kerror::KError;
use vibeos::lock::RANK_SCHED;
use vibeos::paging::{PAGE_SIZE_4K, USER_MAP_END};
use vibeos::proc::pid::IdIndex;
use vibeos::proc::uaccess::user_range_ok;
use vibeos::proc::{
    Creds, FD_CLOEXEC, Fd, FdKind, FdTable, INIT_PID, InitExit, InitState, MAX_FDS, MAX_PROCS,
    ProcState, SIGCHLD, SIGCONT, SIGKILL, SIGSTOP, SigAct, WNOHANG, default_action, dumps_core,
    fd_flags_from_open, kill_delivers, reaper_for, sig_name, wait_exited, wait_signaled,
};
use vibeos::sched::FAR_DEADLINE;
use vibeos::syscall::{self, F_GETFD, F_SETFD, Handlers, SysResult, UserFrame};
use vibeos::thread::ThreadId;
use vibeos::trap::{self, FpCause, FpUnit, Ring3Action, SyscallAbi, TrapKind};
use vibeos::vectors;
use vibeos::wait::WaitQueue;

use crate::addr_space_init;
use crate::addr_space_init::Space;
use crate::arch::current::Arch;
use crate::arch::idt::TrapFrame;
use crate::console_init;
use crate::file_init;
use crate::fill_init;
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
mod floor;

pub use exit::write_ps;

use exec::{sys_brk, sys_execve, sys_fork, sys_mmap, sys_munmap};
#[cfg(not(feature = "vibefs_crash"))]
use exit::reap_zombie;
use exit::{finish_exit, sys_exit, sys_kill, sys_psinfo, sys_wait4};
use fd::{
    addref_fds, close_all_fds, close_where, lookup_fd, sys_close, sys_dup, sys_dup2, sys_fcntl,
    sys_lseek, sys_open, sys_read, sys_write,
};
use floor::{signal_acts, sys_fstat, sys_getdents64, sys_nanosleep, sys_reboot};

struct Proc {
    state: ProcState,
    pid: u32,
    ppid: u32,
    tid: ThreadId,
    name: &'static str,
    creds: Creds,
    /// The process's root and working directory: counted references to
    /// directories (DESIGN §2.11), `None` while the VFS has no root. Only
    /// the process's own thread replaces or drops them, so a syscall reads
    /// them here without taking a count; they are put outside the table
    /// lock, since a put sleeps for the VFS lock.
    root: Option<DirRef>,
    cwd: Option<DirRef>,
    fds: FdTable,
    wait_status: u32,
    pending: u32,
    /// No reaper: freed at exit, ROADMAP §10.5.
    autoreap: bool,
    /// The process's thread's `users` reference to its address space
    /// (DESIGN §2.11). Taken out under the table lock and dropped after
    /// it, since the last put sleeps for the space's `mm` lock.
    space: Option<Space>,
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
            root: None,
            cwd: None,
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

    /// Make this slot `Unused` again in place, as [`Proc::empty`] but for
    /// the descriptor row, which stays allocated and is cleared. Under the
    /// table lock, so it frees nothing: the files are closed and the space
    /// taken before a slot is released, and no thread waits on its queues.
    fn reset(&mut self) {
        debug_assert!(self.space.is_none(), "proc: slot reset with a space");
        debug_assert!(
            self.root.is_none() && self.cwd.is_none(),
            "proc: slot reset with directory references"
        );
        debug_assert!(self.wait_wq.is_empty() && self.stop_wq.is_empty());
        self.state = ProcState::Unused;
        self.pid = 0;
        self.ppid = 0;
        self.tid = ThreadId::NONE;
        self.name = "";
        self.creds = Creds::ROOT;
        self.root = None;
        self.cwd = None;
        self.fds.reset();
        self.wait_status = 0;
        self.pending = 0;
        self.autoreap = false;
        self.space = None;
        self.fs_base = 0;
        self.wait_wq = WaitQueue::new();
        self.stop_wq = WaitQueue::new();
    }

    /// Where this process's path syscalls start: its root and working
    /// directory, or `None` (the namespace root) when it has none.
    fn base(&self) -> Option<WalkBase> {
        match (&self.root, &self.cwd) {
            (Some(r), Some(c)) => Some(WalkBase {
                root: r.at(),
                cwd: c.at(),
            }),
            _ => None,
        }
    }
}

/// A root and a working-directory reference, for a new process.
type DirRefs = Option<(DirRef, DirRef)>;

/// Put the references a process no longer holds. Sleeps for the VFS
/// lock: never under the table lock or SCHED.
fn put_dir_refs(refs: DirRefs) {
    if let Some((root, cwd)) = refs {
        file_init::dir_put(root);
        file_init::dir_put(cwd);
    }
}

/// The calling process's walk base, read from the table: only its own
/// thread replaces its references, so they outlive the call.
fn current_base() -> Option<WalkBase> {
    let pid = current_pid();
    with_table(|t| t.get(pid).and_then(Proc::base))
}

/// Give `pid`'s slot the references `refs`, under the table lock; the
/// ones no slot took come back, for the caller to put after the lock.
fn install_dir_refs(t: &mut Table, pid: u32, refs: DirRefs) -> DirRefs {
    let Some(p) = t.get_mut(pid) else {
        return refs;
    };
    let (root, cwd) = refs?;
    p.root = Some(root);
    p.cwd = Some(cwd);
    None
}

/// Entries in the pid-to-slot index: twice the process table, so probe
/// runs stay short.
const PROC_INDEX_CAP: usize = (2 * MAX_PROCS).next_power_of_two();

/// The process table. A pid is not a slot index (DESIGN §2.11 rule 4): it
/// comes from `thread_init`'s id allocator, and `index` maps it to its slot.
/// `procs` is a heap table of `limits::MAX_PROCS` slots, each with its
/// `limits::MAX_FDS` descriptor row, which [`init_tables`] allocates before
/// `irq: enabled`; nothing grows or frees them after (ROADMAP §10.4, D1).
struct Table {
    procs: TryVec<Proc>,
    index: IdIndex<PROC_INDEX_CAP>,
    /// The kernel's wait queue, on which [`wait_kernel`] sleeps for ppid-0
    /// processes.
    kernel_wq: WaitQueue,
}

impl Table {
    const fn empty() -> Self {
        Self {
            procs: TryVec::new(),
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

    /// Copy `from`'s descriptor row into `to`'s, both processes in the
    /// table. False if either is not.
    fn copy_fds(&mut self, from: u32, to: u32) -> bool {
        let (Some(i), Some(j)) = (self.slot_of(from), self.slot_of(to)) else {
            return false;
        };
        if i == j {
            return true;
        }
        let (lo, hi) = self.procs.split_at_mut(i.max(j));
        let (a, b) = match (lo.get_mut(i.min(j)), hi.first_mut()) {
            (Some(a), Some(b)) => (a, b),
            _ => return false,
        };
        let (src, dst) = if i < j { (&*a, b) } else { (&*b, a) };
        dst.fds.copy_from(&src.fds);
        true
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

/// Allocate the process table: `limits::MAX_PROCS` slots, each with a
/// descriptor row of `limits::MAX_FDS`, and install it. Once, on the BSP
/// before `irq: enabled`, before any process exists. On failure nothing is
/// installed, and the caller halts the boot.
pub fn init_tables() -> Result<(), AllocError> {
    let mut procs = TryVec::try_with_capacity(MAX_PROCS)?;
    let mut i = 0usize;
    while i < MAX_PROCS {
        let mut p = Proc::empty();
        p.fds = FdTable::try_new(MAX_FDS)?;
        procs.try_push(p)?;
        i += 1;
    }
    let old = with_table(|t| core::mem::replace(&mut t.procs, procs));
    // The empty table, dropped after the locks: no heap free under them.
    drop(old);
    Ok(())
}

/// The descriptor row's length in each process-table slot (the first's;
/// `init_tables` gives each the same).
#[cfg(feature = "kernel_tests")]
pub(crate) fn fd_row_capacity() -> usize {
    with_table(|t| t.procs.first().map_or(0, |p| p.fds.capacity()))
}

/// The process table's use: slots not `Unused`, and its length.
#[cfg(feature = "kernel_tests")]
pub(crate) fn table_usage() -> (usize, usize) {
    with_table(|t| {
        let used = t
            .procs
            .iter()
            .filter(|p| p.state != ProcState::Unused)
            .count();
        (used, t.procs.len())
    })
}

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

/// Run `f` on the calling process's address space, pinned: the pin is
/// taken under the table lock (get-unless-zero), `f` runs with the lock
/// released, and the pin drops after it. `None` for a thread with no
/// process or a process with no space. Never call it under the table lock
/// or SCHED: it takes them, and `f` may sleep on the space's `mm` lock.
pub fn with_current_space<R>(f: impl FnOnce(&Space) -> R) -> Option<R> {
    let pid = current_pid();
    if pid == 0 {
        return None;
    }
    let pin = with_table(|t| t.get(pid)?.space.as_ref()?.core().pin())?;
    let r = f(&pin);
    // The process's own reference outlives this call: only its thread,
    // the caller, drops it. So this put is never the last.
    drop(pin);
    Some(r)
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
        p.reset();
    }
    s.free_id(pid);
}

/// Set up `pid`'s slot, which `alloc_pid` took, for a new image: fds 0 to
/// 2 on the console and the rest closed.
#[cfg(not(feature = "vibefs_crash"))]
fn init_slot(t: &mut Table, pid: u32, ppid: u32, name: &'static str) {
    let Some(p) = t.get_mut(pid) else {
        return;
    };
    p.reset();
    p.state = ProcState::Live;
    p.pid = pid;
    p.ppid = ppid;
    p.name = name;
    p.fds.set_stdio();
}

fn user_thread_entry() {
    let pid = current_pid();
    let fs = with_table(|t| t.get(pid).map(|p| p.fs_base));
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

/// Start the ELF at `path` with `argv` and `envp` as a new process with
/// parent `ppid` (0: the kernel, which reaps it with [`wait_kernel`]). An
/// empty `argv` starts it with `argc` 1 and an empty `argv[0]`, as
/// `execve` does.
#[cfg(not(feature = "vibefs_crash"))]
pub(crate) fn spawn_elf(
    path: &[u8],
    argv: &[&[u8]],
    envp: &[&[u8]],
    prefer: u32,
    ppid: u32,
) -> Result<u32, LoadError> {
    let args = user_init::exec_args(argv, envp)?;
    start_loaded(
        user_init::load_path(None, path, &args)?,
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

#[cfg(not(feature = "vibefs_crash"))]
fn start_loaded(
    loaded: Loaded,
    prefer: u32,
    ppid: u32,
    name: &'static str,
) -> Result<u32, LoadError> {
    // A process the kernel starts has the namespace root as its root and
    // working directory; the references are taken before the table lock.
    let refs = file_init::ns_refs();
    let pid = match alloc_pid(prefer) {
        Some(p) => p,
        None => {
            // The space's last `users` put: no thread was made for it.
            drop(loaded.space);
            put_dir_refs(refs);
            return Err(LoadError::NoProc);
        }
    };
    let root = loaded.space.root().as_u64();
    let frame = UserFrame::new_user(loaded.entry, loaded.rsp);
    let fs = loaded.fs;
    let space = loaded.space;
    let h = match thread_init::spawn_user(name, user_thread_entry, pid, root, &frame) {
        Ok(h) => h,
        Err(e) => {
            with_sched_table(|s, t| release_pid(s, t, pid));
            drop(space);
            put_dir_refs(refs);
            return Err(LoadError::Spawn(e));
        }
    };
    let mut space = Some(space);
    let left = with_table(|t| {
        init_slot(t, pid, ppid, name);
        if let Some(p) = t.get_mut(pid) {
            p.space = space.take();
            p.fs_base = fs;
            p.tid = h.id();
        }
        install_dir_refs(t, pid, refs)
    });
    put_dir_refs(left);
    // `alloc_pid` took the slot for this call, and only this thread frees
    // it, so it took the space; a space left here would be named by the
    // new thread's TCB, so it is leaked rather than torn down.
    debug_assert!(space.is_none(), "proc: spawned slot vanished");
    core::mem::forget(space);
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
    syscall_init::set_exit_work_hooks(exit_work_pending, do_exit_work);
}

/// Whether the current process has a kill or a stop for the exit work to
/// act on (DESIGN §5.10 rule 11): it is `Stopped`, or `SIGKILL`, `SIGSTOP`,
/// or a signal whose default action is Term or Stop is pending. Reads the
/// table under its lock with IF as the caller has it, off at the exit.
pub fn exit_work_pending() -> bool {
    let pid = current_pid();
    pid != 0 && with_table(|t| t.get(pid).is_some_and(signal_acts))
}

/// The exit work itself, with IF=1: [`apply_pending`]. A kill runs
/// `finish_exit` on this kernel stack and does not return; a stop waits on
/// `stop_wq` without losing its wakeup.
pub fn do_exit_work(frame: &mut UserFrame) {
    apply_pending(Some(frame));
}

/// A syscall from ring 3, over the user frame its entry saved.
pub fn syscall(frame: &mut UserFrame) -> i64 {
    #[cfg(feature = "kernel_tests")]
    testing::on_entry(frame);
    apply_pending(Some(&mut *frame));
    let nr = Arch::nr(frame);
    let args: [u64; 6] = core::array::from_fn(|i| Arch::arg(frame, i));
    let ret = syscall::encode(dispatch_frame(nr, args, Some(frame)));
    if syscall_init::trace_enabled() {
        let name = Arch::table().lookup(nr).map_or("?", |s| s.row().name);
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
    Arch::dispatch(&mut Ctx { frame }, nr, &args)
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

    fn fstat(&mut self, fd: u32, statbuf: u64) -> SysResult {
        sys_fstat(fd, statbuf)
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

    fn nanosleep(&mut self, rqtp: u64, rmtp: u64) -> SysResult {
        sys_nanosleep(rqtp, rmtp)
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

    fn reboot(&mut self, magic1: i32, magic2: i32, cmd: u32, arg: u64) -> SysResult {
        sys_reboot(magic1, magic2, cmd, arg)
    }

    fn getdents64(&mut self, fd: u32, dirent: u64, count: u32) -> SysResult {
        sys_getdents64(fd, dirent, count)
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
///
/// A pending fatal signal that writes no core ends the process first,
/// stopped or not: Linux makes one a group `SIGKILL` when it is sent,
/// which wakes a stopped process. Then a stop, then the rest, lowest
/// number first, as Linux dequeues them; a Core signal therefore waits
/// for `SIGCONT` in a stopped process.
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
                let mut sig = 1u32;
                while sig <= 31 {
                    if p.pending & bit(sig) != 0
                        && default_action(sig) == SigAct::Term
                        && !dumps_core(sig)
                    {
                        return Pending::Die(sig);
                    }
                    sig += 1;
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
                finish_exit(wait_signaled(sig), None);
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
#[cfg(target_arch = "x86_64")]
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
    #[cfg(target_arch = "x86_64")]
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
    // The faulting address, for pid 1's line: CR2 for `#PF`, else the RIP.
    #[cfg(target_arch = "x86_64")]
    let addr = if f.vector == u64::from(vectors::PF) {
        f.cr2
    } else {
        rip
    };
    #[cfg(not(target_arch = "x86_64"))]
    let addr = rip;
    crate::arch::gs::force_kernel();
    finish_exit(wait_signaled(sig), Some(addr));
}

/// In-guest test hooks. `kernel_tests` only (AGENTS.md rule 9).
#[cfg(feature = "kernel_tests")]
pub(crate) mod testing {
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

    use vibeos::syscall::{SYS_GETPID, UserFrame};

    use crate::arch::idt::TrapFrame;
    use crate::per_cpu_init;
    use crate::thread_init;
    use crate::time_init;

    /// Set `sig`'s pending bit on `pid` and nothing else: no wake, no IPI,
    /// and no exit for the caller, as the exit-work hook needs.
    pub(crate) fn post_pending(pid: u32, sig: u32) {
        super::with_table(|t| {
            if let Some(p) = t.get_mut(pid) {
                p.pending |= super::bit(sig);
            }
        });
    }

    /// Run `f` on `pid`'s address space, pinned: the pin is taken under the
    /// table lock (get-unless-zero), and dropped after `f`, with no lock
    /// held, where its put may be the last. `None` when `pid` has no space,
    /// or its last `users` put has run.
    pub(crate) fn with_space_of<R>(
        pid: u32,
        f: impl FnOnce(&crate::addr_space_init::Space) -> R,
    ) -> Option<R> {
        let pin = super::with_table(|t| t.get(pid)?.space.as_ref()?.core().pin())?;
        let r = f(&pin);
        drop(pin);
        Some(r)
    }

    /// A memory-only reference to `pid`'s address space: it keeps the
    /// root, not the space's use.
    pub(crate) fn space_core(pid: u32) -> Option<crate::addr_space_init::CoreRef> {
        super::with_table(|t| t.get(pid)?.space.as_ref().map(|s| s.core()))
    }

    /// The tid of `pid`'s thread, while its slot lives (until it is
    /// reaped).
    pub(crate) fn tid_of(pid: u32) -> Option<u32> {
        super::with_table(|t| t.get(pid).map(|p| p.tid.0))
    }

    /// Whether `pid` is a zombie, waiting to be reaped.
    pub(crate) fn is_zombie(pid: u32) -> bool {
        super::with_table(|t| {
            t.get(pid)
                .is_some_and(|p| p.state == super::ProcState::Zombie)
        })
    }

    /// Whether `pid` is `Stopped`.
    pub(crate) fn is_stopped(pid: u32) -> bool {
        super::with_table(|t| {
            t.get(pid)
                .is_some_and(|p| p.state == super::ProcState::Stopped)
        })
    }

    /// Processes that can hold an open file: `Live` or `Stopped`, since a
    /// zombie closed its descriptors before it became one.
    pub(crate) fn holding_count() -> usize {
        super::with_table(|t| {
            t.procs
                .iter()
                .filter(|p| matches!(p.state, super::ProcState::Live | super::ProcState::Stopped))
                .count()
        })
    }

    /// The fault address of the `#PF` whose kill line yields once; 0 for
    /// none.
    static KILL_YIELD_CR2: AtomicU64 = AtomicU64::new(0);
    /// Kill lines `try_user_fault` has finished writing since boot.
    static KILL_LINES: AtomicU64 = AtomicU64::new(0);

    /// The next CPL-3 `#PF` at `fault_addr` that ends in a kill yields in
    /// `try_user_fault` once its kill line is written, until another kill
    /// line is written, for at most 1 s of TSC time: a kill line written
    /// in pieces would take the other line inside it.
    pub(crate) fn arm_kill_line_yield(fault_addr: u64) {
        KILL_YIELD_CR2.store(fault_addr, Ordering::Release);
    }

    pub(crate) fn disarm_kill_line_yield() {
        KILL_YIELD_CR2.store(0, Ordering::Release);
    }

    #[cfg(target_arch = "x86_64")]
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

    /// Buckets `getpid` counts in.
    const GETPID_BUCKETS: usize = 64;

    /// `getpid` counts per bucket `pid % GETPID_BUCKETS` (a pid is not a
    /// slot index), each tagged with the pid it counts for: a pid that
    /// finds another's tag starts the bucket again.
    static GETPID_TAGS: [AtomicU32; GETPID_BUCKETS] = [const { AtomicU32::new(0) }; GETPID_BUCKETS];
    static GETPIDS: [AtomicU64; GETPID_BUCKETS] = [const { AtomicU64::new(0) }; GETPID_BUCKETS];

    fn getpid_bucket(pid: u32) -> usize {
        pid as usize % GETPID_BUCKETS
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

    static WAIT4_KILL_ARMED: AtomicBool = AtomicBool::new(false);
    /// The caller and `pid` argument of the `wait4` that took the kill; 0
    /// before.
    static WAIT4_KILL_CALLER: AtomicU32 = AtomicU32::new(0);
    static WAIT4_KILL_WANT: AtomicU64 = AtomicU64::new(0);

    /// The next `wait4` from a process posts that process a `SIGKILL`
    /// once it has entered, as [`post_pending`] does: no wake and no IPI,
    /// as a kill that lands before the wait is armed finds the caller on
    /// no queue.
    pub(crate) fn arm_wait4_kill() {
        WAIT4_KILL_CALLER.store(0, Ordering::Release);
        WAIT4_KILL_WANT.store(0, Ordering::Release);
        WAIT4_KILL_ARMED.store(true, Ordering::Release);
    }

    pub(crate) fn disarm_wait4_kill() {
        WAIT4_KILL_ARMED.store(false, Ordering::Release);
    }

    /// The caller of the `wait4` that took the kill, and its `pid`
    /// argument; `(0, 0)` before one did.
    pub(crate) fn wait4_kill_taken() -> (u32, i64) {
        (
            WAIT4_KILL_CALLER.load(Ordering::Acquire),
            WAIT4_KILL_WANT.load(Ordering::Acquire) as i64,
        )
    }

    pub(super) fn wait4_entered(pid: u32, want: i64) {
        if !WAIT4_KILL_ARMED.swap(false, Ordering::AcqRel) {
            return;
        }
        post_pending(pid, vibeos::proc::SIGKILL);
        WAIT4_KILL_WANT.store(want as u64, Ordering::Release);
        WAIT4_KILL_CALLER.store(pid, Ordering::Release);
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
        if STOPS_PID.load(Ordering::Acquire) == pid {
            STOPS.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// The pid whose stop decisions [`stops`] counts; 0 for none.
    static STOPS_PID: AtomicU32 = AtomicU32::new(0);
    static STOPS: AtomicU32 = AtomicU32::new(0);

    /// Count `pid`'s stop decisions from 0; 0 counts none.
    pub(crate) fn watch_stops(pid: u32) {
        STOPS.store(0, Ordering::Release);
        STOPS_PID.store(pid, Ordering::Release);
    }

    /// How many times the watched process has decided to stop and armed
    /// its `stop_wq` wait.
    pub(crate) fn stops() -> u32 {
        STOPS.load(Ordering::Acquire)
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
        // With IF=0 the stall is a deliberate IF-off stretch; with IF=1 it
        // holds none and takes no guard.
        let _hold = (!crate::arch::current::interrupts_enabled())
            .then(|| crate::sched::irqoff::deliberate("stop stall"));
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
        let me = thread_init::current_id().raw();
        if TICKS_TID
            .compare_exchange(me, u32::MAX, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            let (cpu, ticks) = cpu_ticks();
            TICKS_END_CPU.store(cpu, Ordering::Relaxed);
            TICKS_END.store(ticks, Ordering::Relaxed);
            // Release: publishes the record; pairs with `console_write_ticks`.
            TICKS_DONE.store(true, Ordering::Release);
        }
    }

    /// Set until a console `write` claims the tick record.
    static TICKS_ARMED: AtomicBool = AtomicBool::new(false);
    /// The thread whose console `write` holds the claim, or `u32::MAX`.
    static TICKS_TID: AtomicU32 = AtomicU32::new(u32::MAX);
    static TICKS_START_CPU: AtomicU32 = AtomicU32::new(0);
    static TICKS_START: AtomicU64 = AtomicU64::new(0);
    static TICKS_END_CPU: AtomicU32 = AtomicU32::new(0);
    static TICKS_END: AtomicU64 = AtomicU64::new(0);
    static TICKS_DONE: AtomicBool = AtomicBool::new(false);

    /// Record the next console `write` that copies all it was given: the
    /// CPU it runs on and that CPU's timer ticks at its first copy and at
    /// its return (ROADMAP §10.2: what happened during the write, not what
    /// a watcher saw later).
    pub(crate) fn arm_console_write_ticks() {
        TICKS_DONE.store(false, Ordering::Release);
        TICKS_TID.store(u32::MAX, Ordering::Release);
        TICKS_ARMED.store(true, Ordering::Release);
    }

    /// The recorded write's `(cpu, ticks)` at its first copy and at its
    /// return, once it has returned.
    pub(crate) fn console_write_ticks() -> Option<((u32, u64), (u32, u64))> {
        // Acquire: pairs with the Release store in `console_write_returned`.
        TICKS_DONE.load(Ordering::Acquire).then(|| {
            (
                (
                    TICKS_START_CPU.load(Ordering::Relaxed),
                    TICKS_START.load(Ordering::Relaxed),
                ),
                (
                    TICKS_END_CPU.load(Ordering::Relaxed),
                    TICKS_END.load(Ordering::Relaxed),
                ),
            )
        })
    }

    /// Called by `sys_write` before a console write's first copy.
    pub(super) fn console_write_started() {
        if !TICKS_ARMED.swap(false, Ordering::AcqRel) {
            return;
        }
        let (cpu, ticks) = cpu_ticks();
        TICKS_START_CPU.store(cpu, Ordering::Relaxed);
        TICKS_START.store(ticks, Ordering::Relaxed);
        // Release: the start's fields before the claim that ends them.
        TICKS_TID.store(thread_init::current_id().raw(), Ordering::Release);
    }

    /// This CPU's id and timer ticks, read with IF off so both are this
    /// CPU's.
    fn cpu_ticks() -> (u32, u64) {
        let _g = crate::arch::current::InterruptGuard::enter();
        let me = per_cpu_init::current().cpu_id;
        let ticks = per_cpu_init::cpu(me).map_or(0, |c| c.ticks.load(Ordering::Relaxed));
        (me, ticks)
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
        #[cfg(target_arch = "x86_64")]
        if APIC_ARMED.load(Ordering::Acquire) {
            // One IF=0 stretch: the `PerCpu` read and CPUID see one CPU.
            let _g = crate::arch::current::InterruptGuard::enter();
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
