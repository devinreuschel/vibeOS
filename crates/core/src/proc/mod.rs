//! Process types: pid, fd table, wait status, early signals. ROADMAP §9.5–9.7.
//!
//! Kernel table / fork / exec live in `proc_init`. COW is Phase 12.
//! User signal handlers are Phase 13.

pub mod addr_space;
pub mod elf;
pub mod pid;
pub mod syscall;
pub mod syscall_table;
pub mod uabi;
pub mod uaccess;

use crate::fs::{MAX_PATH, O_CLOEXEC};
use crate::kalloc::{AllocError, TryVec};
use crate::limits;

pub use crate::limits::MAX_FDS;
pub use crate::limits::MAX_PROCS;
pub const INIT_PID: u32 = 1;
pub const NAME_MAX: usize = 16;

/// Linux `FD_CLOEXEC`. Distinct from open `O_CLOEXEC`.
pub const FD_CLOEXEC: u32 = 1;

pub const WNOHANG: u64 = 1;

pub const SIGHUP: u32 = 1;
pub const SIGINT: u32 = 2;
pub const SIGQUIT: u32 = 3;
pub const SIGILL: u32 = 4;
pub const SIGTRAP: u32 = 5;
pub const SIGABRT: u32 = 6;
pub const SIGBUS: u32 = 7;
pub const SIGFPE: u32 = 8;
pub const SIGKILL: u32 = 9;
pub const SIGUSR1: u32 = 10;
pub const SIGSEGV: u32 = 11;
pub const SIGUSR2: u32 = 12;
pub const SIGPIPE: u32 = 13;
pub const SIGALRM: u32 = 14;
pub const SIGTERM: u32 = 15;
pub const SIGCHLD: u32 = 17;
pub const SIGCONT: u32 = 18;
pub const SIGSTOP: u32 = 19;
pub const SIGTSTP: u32 = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcState {
    Unused,
    Live,
    Stopped,
    Zombie,
}

impl ProcState {
    pub fn name(self) -> &'static str {
        match self {
            Self::Unused => "unused",
            Self::Live => "run",
            Self::Stopped => "stop",
            Self::Zombie => "zombie",
        }
    }
}

/// Pid 1's slot as the orphan reaper rule reads it (ROADMAP §10.5, F068).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitState {
    Live,
    Stopped,
    Zombie,
    Absent,
}

impl InitState {
    /// The state of pid 1's slot; an unused slot is `Absent`.
    pub const fn of(s: ProcState) -> Self {
        match s {
            ProcState::Unused => Self::Absent,
            ProcState::Live => Self::Live,
            ProcState::Stopped => Self::Stopped,
            ProcState::Zombie => Self::Zombie,
        }
    }
}

/// The pid that adopts an orphan: pid 1 while init is live or stopped,
/// otherwise none, and the orphan's zombie is freed when it exits
/// (ROADMAP §10.5, F068).
pub const fn reaper_for(init: InitState) -> Option<u32> {
    match init {
        InitState::Live | InitState::Stopped => Some(INIT_PID),
        InitState::Zombie | InitState::Absent => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Creds {
    pub uid: u32,
    pub gid: u32,
    pub euid: u32,
    pub egid: u32,
}

impl Creds {
    pub const ROOT: Self = Self {
        uid: 0,
        gid: 0,
        euid: 0,
        egid: 0,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdKind {
    None,
    Console,
    /// An open-file table slot and the generation it had when this fd was
    /// made (C-FDGEN).
    File {
        fid: u16,
        r#gen: u16,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fd {
    pub kind: FdKind,
    pub flags: u32,
}

impl Fd {
    pub const EMPTY: Self = Self {
        kind: FdKind::None,
        flags: 0,
    };

    pub const fn is_open(self) -> bool {
        match self.kind {
            FdKind::None => false,
            FdKind::Console | FdKind::File { .. } => true,
        }
    }

    pub const fn cloexec(self) -> bool {
        self.flags & FD_CLOEXEC != 0
    }
}

/// Why an fd-table call failed.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdError {
    /// The number is out of range or names no open file.
    Badf,
    /// Every slot is in use.
    Full,
}

/// A bad descriptor is `EBADF`; a full table, `EMFILE`, as Linux's.
impl From<FdError> for crate::kerror::KError {
    fn from(e: FdError) -> Self {
        match e {
            FdError::Badf => Self::BadF,
            FdError::Full => Self::MFile,
        }
    }
}

/// A process's descriptors: a heap row of a fixed length,
/// `limits::MAX_FDS` for a process-table slot's, built once by
/// [`FdTable::try_new`] and never grown; [`FdTable::empty`] has no room.
/// Not `Copy`: fork copies a row into another with [`FdTable::copy_from`].
#[derive(Debug)]
pub struct FdTable {
    slots: TryVec<Fd>,
}

impl FdTable {
    /// A row with no room, for a `const` initializer.
    pub const fn empty() -> Self {
        Self {
            slots: TryVec::new(),
        }
    }

    /// A row of `len` closed descriptors.
    pub fn try_new(len: usize) -> Result<Self, AllocError> {
        Ok(Self {
            slots: limits::table(len, || Fd::EMPTY)?,
        })
    }

    /// Descriptors the row holds.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Close every entry in place, returning nothing: for a row whose files
    /// are already closed or were never opened.
    pub fn reset(&mut self) {
        self.slots.iter_mut().for_each(|s| *s = Fd::EMPTY);
    }

    /// Make this row a copy of `src`: its entries, then closed ones up to
    /// this row's length. Entries past this row's length are dropped.
    pub fn copy_from(&mut self, src: &FdTable) {
        let mut i = 0usize;
        while i < self.slots.len() {
            self.slots[i] = src.slots.get(i).copied().unwrap_or(Fd::EMPTY);
            i += 1;
        }
    }

    /// fds 0 to 2 on the console, in a row of `len`.
    pub fn stdio(len: usize) -> Result<Self, AllocError> {
        let mut t = Self::try_new(len)?;
        t.set_stdio();
        Ok(t)
    }

    /// Put fds 0 to 2 on the console; the rest are untouched.
    pub fn set_stdio(&mut self) {
        for s in self.slots.iter_mut().take(3) {
            *s = Fd {
                kind: FdKind::Console,
                flags: 0,
            };
        }
    }

    pub fn get(&self, fd: u32) -> Option<Fd> {
        self.slots.get(fd as usize).copied().filter(|s| s.is_open())
    }

    pub fn set(&mut self, fd: u32, slot: Fd) -> Result<(), FdError> {
        let s = self.slots.get_mut(fd as usize).ok_or(FdError::Badf)?;
        *s = slot;
        Ok(())
    }

    pub fn alloc(&mut self, slot: Fd) -> Result<u32, FdError> {
        let i = self
            .slots
            .iter()
            .position(|s| !s.is_open())
            .ok_or(FdError::Full)?;
        self.slots[i] = slot;
        Ok(i as u32)
    }

    /// Close and return the old slot. `None` if not open.
    pub fn close(&mut self, fd: u32) -> Option<Fd> {
        let s = self.slots.get_mut(fd as usize)?;
        if !s.is_open() {
            return None;
        }
        Some(core::mem::replace(s, Fd::EMPTY))
    }

    /// New fd, CLOEXEC cleared (Linux `dup`).
    pub fn dup(&mut self, old: u32) -> Result<u32, FdError> {
        let s = self.get(old).ok_or(FdError::Badf)?;
        self.alloc(Fd {
            kind: s.kind,
            flags: s.flags & !FD_CLOEXEC,
        })
    }

    /// Linux `dup2`: copy `old` onto `new`. CLOEXEC cleared on `new`.
    /// Returns the slot that occupied `new` (to drop the file).
    pub fn dup2(&mut self, old: u32, new: u32) -> Result<Option<Fd>, FdError> {
        if old == new {
            let _ = self.get(old).ok_or(FdError::Badf)?;
            return Ok(None);
        }
        let s = self.get(old).ok_or(FdError::Badf)?;
        if new as usize >= self.slots.len() {
            return Err(FdError::Badf);
        }
        let displaced = self.close(new);
        self.slots[new as usize] = Fd {
            kind: s.kind,
            flags: s.flags & !FD_CLOEXEC,
        };
        Ok(displaced)
    }

    /// Drop CLOEXEC fds on exec, handing each to `gone` for the caller to
    /// close in the file table.
    pub fn apply_cloexec(&mut self, mut gone: impl FnMut(Fd)) {
        for s in self.slots.iter_mut() {
            if s.is_open() && s.cloexec() {
                gone(core::mem::replace(s, Fd::EMPTY));
            }
        }
    }

    /// Close the open entries from `start` on that `pick` chooses, at most
    /// `out.len()` of them, moving each into `out`: how many, and where the
    /// next batch starts (the row's length once it is done). The kernel
    /// closes a row in batches this way, one lock hold each, so no copy of
    /// the whole row leaves the process table.
    pub fn take_batch(
        &mut self,
        start: usize,
        out: &mut [Fd],
        mut pick: impl FnMut(Fd) -> bool,
    ) -> (usize, usize) {
        let mut n = 0usize;
        let mut i = start;
        while n < out.len() {
            let Some(s) = self.slots.get_mut(i) else {
                break;
            };
            if s.is_open()
                && pick(*s)
                && let Some(o) = out.get_mut(n)
            {
                *o = core::mem::replace(s, Fd::EMPTY);
                n += 1;
            }
            i += 1;
        }
        (n, i)
    }

    /// The open entries, with their numbers.
    pub fn iter(&self) -> impl Iterator<Item = (u32, Fd)> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_open())
            .map(|(i, s)| (i as u32, *s))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cwd {
    pub buf: [u8; MAX_PATH],
    pub len: u8,
}

impl Cwd {
    pub const fn root() -> Self {
        let mut buf = [0u8; MAX_PATH];
        buf[0] = b'/';
        Self { buf, len: 1 }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }
}

/// Linux wait(2) encoding.
pub const fn wait_exited(code: u32) -> u32 {
    (code & 0xff) << 8
}

pub const fn wait_signaled(sig: u32) -> u32 {
    sig & 0x7f
}

pub const fn wait_stopped(sig: u32) -> u32 {
    ((sig & 0xff) << 8) | 0x7f
}

pub const fn wifexited(st: u32) -> bool {
    st & 0x7f == 0
}

pub const fn wexitstatus(st: u32) -> u32 {
    (st >> 8) & 0xff
}

pub const fn wifsignaled(st: u32) -> bool {
    let s = st & 0x7f;
    s != 0 && s != 0x7f
}

pub const fn wtermsig(st: u32) -> u32 {
    st & 0x7f
}

pub const fn wifstopped(st: u32) -> bool {
    st & 0xff == 0x7f
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SigAct {
    Term,
    Ign,
    Stop,
    Cont,
}

pub const fn default_action(sig: u32) -> SigAct {
    match sig {
        SIGCHLD => SigAct::Ign,
        SIGCONT => SigAct::Cont,
        SIGSTOP | SIGTSTP => SigAct::Stop,
        SIGKILL => SigAct::Term,
        _ => SigAct::Term,
    }
}

/// Uncatchable even when Phase 13 grows handlers.
pub const fn forced(sig: u32) -> bool {
    sig == SIGKILL || sig == SIGSTOP
}

pub fn sig_name(sig: u32) -> &'static str {
    match sig {
        SIGHUP => "HUP",
        SIGINT => "INT",
        SIGQUIT => "QUIT",
        SIGILL => "ILL",
        SIGTRAP => "TRAP",
        SIGABRT => "ABRT",
        SIGBUS => "BUS",
        SIGFPE => "FPE",
        SIGKILL => "KILL",
        SIGUSR1 => "USR1",
        SIGSEGV => "SEGV",
        SIGUSR2 => "USR2",
        SIGPIPE => "PIPE",
        SIGALRM => "ALRM",
        SIGTERM => "TERM",
        SIGCHLD => "CHLD",
        SIGCONT => "CONT",
        SIGSTOP => "STOP",
        SIGTSTP => "TSTP",
        _ => "?",
    }
}

/// Open `O_CLOEXEC` becomes per-fd `FD_CLOEXEC`.
pub const fn fd_flags_from_open(oflags: u32) -> u32 {
    if oflags & O_CLOEXEC != 0 {
        FD_CLOEXEC
    } else {
        0
    }
}

/// Add each thread's syscall count to its process's sum: `threads` gives
/// `(pid, count)` per thread, and `sums` one `(pid, sum)` per process,
/// sorted by pid. A kernel thread (pid 0) and a pid `sums` does not hold are
/// skipped, and a sum saturates. `thread_init::sum_syscalls` feeds it the
/// thread table in one pass (ROADMAP §10.7).
pub fn sum_syscalls(sums: &mut [(u32, u64)], threads: impl Iterator<Item = (u32, u64)>) {
    for (pid, n) in threads {
        if pid == 0 {
            continue;
        }
        if let Ok(i) = sums.binary_search_by_key(&pid, |&(p, _)| p)
            && let Some(e) = sums.get_mut(i)
        {
            e.1 = e.1.saturating_add(n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_syscalls_per_process() {
        let mut sums = [(1u32, 0u64), (4, 0), (9, u64::MAX - 1)];
        let threads = [(4u32, 3u64), (0, 100), (1, 2), (4, 5), (7, 11), (9, 5)];
        sum_syscalls(&mut sums, threads.iter().copied());
        assert_eq!(sums, [(1, 2), (4, 8), (9, u64::MAX)]);
        let mut none: [(u32, u64); 0] = [];
        sum_syscalls(&mut none, threads.iter().copied());
    }

    /// `dup` on a full table is `EMFILE`, and on a closed fd `EBADF`, as
    /// Linux's (ROADMAP §10.4, E2).
    #[test]
    fn fd_table_full_is_emfile() {
        let mut t = FdTable::stdio(MAX_FDS).unwrap();
        for _ in 3..MAX_FDS {
            t.dup(0).unwrap();
        }
        let e = t.dup(0).unwrap_err();
        assert_eq!(e, FdError::Full);
        assert_eq!(crate::kerror::KError::from(e).errno(), 24);
        assert_eq!(t.alloc(Fd::EMPTY), Err(FdError::Full));
        assert!(t.close(5).is_some());
        let e = t.dup(5).unwrap_err();
        assert_eq!(e, FdError::Badf);
        assert_eq!(crate::kerror::KError::from(e), crate::kerror::KError::BadF);
        assert_eq!(t.dup(MAX_FDS as u32), Err(FdError::Badf));
        assert_eq!(t.dup(0), Ok(5));
    }

    #[test]
    fn wait_status_linux_shape() {
        let st = wait_exited(42);
        assert!(wifexited(st));
        assert_eq!(wexitstatus(st), 42);
        assert!(!wifsignaled(st));
        let sig = wait_signaled(SIGSEGV);
        assert!(wifsignaled(sig));
        assert_eq!(wtermsig(sig), SIGSEGV);
        assert!(!wifexited(sig));
        let stop = wait_stopped(SIGSTOP);
        assert!(wifstopped(stop));
        assert!(!wifexited(stop));
        assert!(!wifsignaled(stop));
    }

    #[test]
    fn reaper_for_init_state() {
        assert_eq!(InitState::of(ProcState::Unused), InitState::Absent);
        assert_eq!(InitState::of(ProcState::Live), InitState::Live);
        assert_eq!(InitState::of(ProcState::Stopped), InitState::Stopped);
        assert_eq!(InitState::of(ProcState::Zombie), InitState::Zombie);
        assert_eq!(reaper_for(InitState::Live), Some(INIT_PID));
        assert_eq!(reaper_for(InitState::Stopped), Some(INIT_PID));
        assert_eq!(reaper_for(InitState::Zombie), None);
        assert_eq!(reaper_for(InitState::Absent), None);
    }

    #[test]
    fn fd_table_stdio_dup_cloexec() {
        let mut t = FdTable::stdio(8).unwrap();
        assert!(matches!(t.get(0).unwrap().kind, FdKind::Console));
        assert!(matches!(t.get(1).unwrap().kind, FdKind::Console));
        assert!(matches!(t.get(2).unwrap().kind, FdKind::Console));
        assert!(t.get(3).is_none());
        let n = t.dup(1).unwrap();
        assert_eq!(n, 3);
        assert!(!t.get(n).unwrap().cloexec());
        t.set(
            4,
            Fd {
                kind: FdKind::File { fid: 7, r#gen: 2 },
                flags: FD_CLOEXEC,
            },
        )
        .unwrap();
        let mut gone = None;
        t.apply_cloexec(|f| {
            assert!(gone.is_none());
            gone = Some(f);
        });
        assert_eq!(
            gone.map(|f| f.kind),
            Some(FdKind::File { fid: 7, r#gen: 2 })
        );
        assert!(t.get(4).is_none());
        assert!(t.get(1).is_some());
        let old = t.dup2(1, 2).unwrap();
        assert!(old.is_some());
        assert!(matches!(t.get(2).unwrap().kind, FdKind::Console));
        assert!(t.dup2(1, 1).unwrap().is_none());
        assert!(t.close(99).is_none());
        assert!(t.dup(9).is_err());
        let _ = O_CLOEXEC;
        assert_eq!(fd_flags_from_open(O_CLOEXEC), FD_CLOEXEC);
        assert_eq!(fd_flags_from_open(0), 0);
    }

    #[test]
    fn signal_defaults() {
        assert_eq!(default_action(SIGCHLD), SigAct::Ign);
        assert_eq!(default_action(SIGKILL), SigAct::Term);
        assert_eq!(default_action(SIGSTOP), SigAct::Stop);
        assert_eq!(default_action(SIGSEGV), SigAct::Term);
        assert_eq!(default_action(SIGILL), SigAct::Term);
        assert!(forced(SIGKILL));
        assert!(forced(SIGSTOP));
        assert!(!forced(SIGTERM));
        assert_eq!(sig_name(SIGSEGV), "SEGV");
        assert_eq!(ProcState::Zombie.name(), "zombie");
        assert_eq!(Creds::ROOT.uid, 0);
        assert_eq!(INIT_PID, 1);
        let c = Cwd::root();
        assert_eq!(c.as_bytes(), b"/");
        assert_eq!(default_action(SIGCONT), SigAct::Cont);
    }

    #[test]
    fn fd_table_full() {
        let mut t = FdTable::try_new(5).unwrap();
        assert_eq!(t.capacity(), 5);
        let mut n = 0u32;
        while n < t.capacity() as u32 {
            t.alloc(Fd {
                kind: FdKind::Console,
                flags: 0,
            })
            .unwrap();
            n += 1;
        }
        assert!(
            t.alloc(Fd {
                kind: FdKind::Console,
                flags: 0,
            })
            .is_err()
        );
        t.close(0);
        assert_eq!(
            t.alloc(Fd {
                kind: FdKind::File { fid: 1, r#gen: 0 },
                flags: 0,
            })
            .unwrap(),
            0
        );
    }

    #[test]
    fn fd_table_rows_copy_and_batch() {
        let mut a = FdTable::stdio(6).unwrap();
        let file = |fid, flags| Fd {
            kind: FdKind::File { fid, r#gen: 0 },
            flags,
        };
        a.set(3, file(1, FD_CLOEXEC)).unwrap();
        a.set(5, file(2, 0)).unwrap();
        let mut b = FdTable::try_new(6).unwrap();
        b.set(4, file(9, 0)).unwrap();
        b.copy_from(&a);
        assert_eq!(
            b.iter().map(|(i, _)| i).collect::<Vec<_>>(),
            [0, 1, 2, 3, 5]
        );
        let mut out = [Fd::EMPTY; 2];
        assert_eq!(b.take_batch(0, &mut out, Fd::cloexec), (1, 6));
        assert_eq!(out[0], file(1, FD_CLOEXEC));
        assert_eq!(b.take_batch(0, &mut out, |_| true), (2, 2));
        assert_eq!(b.take_batch(2, &mut out, |_| true), (2, 6));
        assert_eq!(
            out,
            [
                Fd {
                    kind: FdKind::Console,
                    flags: 0
                },
                file(2, 0)
            ]
        );
        assert_eq!(b.take_batch(6, &mut out, |_| true), (0, 6));
        assert_eq!(b.iter().count(), 0);
        a.reset();
        assert_eq!(a.iter().count(), 0);
        assert_eq!(FdTable::empty().capacity(), 0);
        assert!(FdTable::empty().alloc(Fd::EMPTY).is_err());
    }

    #[test]
    fn fixed_tables_match_limits() {
        let t = FdTable::try_new(crate::limits::MAX_FDS).unwrap();
        assert_eq!(t.capacity(), crate::limits::MAX_FDS);
    }
}
