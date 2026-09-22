//! Process types: pid, fd table, wait status, early signals. ROADMAP §9.5–9.7.
//!
//! Kernel table / fork / exec live in `proc_init`. COW is Phase 10.
//! User signal handlers are Phase 11.

use crate::fs::{MAX_PATH, O_CLOEXEC};

pub const MAX_PROCS: usize = 16;
pub const MAX_FDS: usize = 16;
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
    File(u16),
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
            FdKind::Console | FdKind::File(_) => true,
        }
    }

    pub const fn cloexec(self) -> bool {
        self.flags & FD_CLOEXEC != 0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FdTable {
    slots: [Fd; MAX_FDS],
}

impl FdTable {
    pub const fn empty() -> Self {
        Self {
            slots: [Fd::EMPTY; MAX_FDS],
        }
    }

    pub fn stdio() -> Self {
        let mut t = Self::empty();
        t.slots[0] = Fd {
            kind: FdKind::Console,
            flags: 0,
        };
        t.slots[1] = Fd {
            kind: FdKind::Console,
            flags: 0,
        };
        t.slots[2] = Fd {
            kind: FdKind::Console,
            flags: 0,
        };
        t
    }

    pub fn get(self, fd: u32) -> Option<Fd> {
        let i = fd as usize;
        if i >= MAX_FDS || !self.slots[i].is_open() {
            None
        } else {
            Some(self.slots[i])
        }
    }

    pub fn set(&mut self, fd: u32, slot: Fd) -> Result<(), ()> {
        let i = fd as usize;
        if i >= MAX_FDS {
            return Err(());
        }
        self.slots[i] = slot;
        Ok(())
    }

    pub fn alloc(&mut self, slot: Fd) -> Result<u32, ()> {
        let mut i = 0u32;
        while i < MAX_FDS as u32 {
            if !self.slots[i as usize].is_open() {
                self.slots[i as usize] = slot;
                return Ok(i);
            }
            i += 1;
        }
        Err(())
    }

    /// Close and return the old slot. `None` if not open.
    pub fn close(&mut self, fd: u32) -> Option<Fd> {
        let i = fd as usize;
        if i >= MAX_FDS || !self.slots[i].is_open() {
            return None;
        }
        let old = self.slots[i];
        self.slots[i] = Fd::EMPTY;
        Some(old)
    }

    /// New fd, CLOEXEC cleared (Linux `dup`).
    pub fn dup(&mut self, old: u32) -> Result<u32, ()> {
        let s = self.get(old).ok_or(())?;
        self.alloc(Fd {
            kind: s.kind,
            flags: s.flags & !FD_CLOEXEC,
        })
    }

    /// Linux `dup2`: copy `old` onto `new`. CLOEXEC cleared on `new`.
    /// Returns the slot that occupied `new` (to drop the file).
    pub fn dup2(&mut self, old: u32, new: u32) -> Result<Option<Fd>, ()> {
        if old == new {
            let _ = self.get(old).ok_or(())?;
            return Ok(None);
        }
        let s = self.get(old).ok_or(())?;
        if new as usize >= MAX_FDS {
            return Err(());
        }
        let displaced = self.close(new);
        self.slots[new as usize] = Fd {
            kind: s.kind,
            flags: s.flags & !FD_CLOEXEC,
        };
        Ok(displaced)
    }

    /// Drop CLOEXEC fds on exec. Returns closed file fids for the caller
    /// to `close` in the file table.
    pub fn apply_cloexec(&mut self) -> [Option<u16>; MAX_FDS] {
        let mut gone = [None; MAX_FDS];
        let mut i = 0usize;
        while i < MAX_FDS {
            if self.slots[i].is_open() && self.slots[i].cloexec() {
                if let FdKind::File(fid) = self.slots[i].kind {
                    gone[i] = Some(fid);
                }
                self.slots[i] = Fd::EMPTY;
            }
            i += 1;
        }
        gone
    }

    pub fn iter(self) -> [Fd; MAX_FDS] {
        self.slots
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

/// Uncatchable even when Phase 11 grows handlers.
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn fd_table_stdio_dup_cloexec() {
        let mut t = FdTable::stdio();
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
                kind: FdKind::File(7),
                flags: FD_CLOEXEC,
            },
        )
        .unwrap();
        let gone = t.apply_cloexec();
        assert_eq!(gone[4], Some(7));
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
        assert_eq!(MAX_PROCS, 16);
        let c = Cwd::root();
        assert_eq!(c.as_bytes(), b"/");
        assert_eq!(default_action(SIGCONT), SigAct::Cont);
    }

    #[test]
    fn fd_table_full() {
        let mut t = FdTable::empty();
        let mut n = 0u32;
        while n < MAX_FDS as u32 {
            t.alloc(Fd {
                kind: FdKind::Console,
                flags: 0,
            })
            .unwrap();
            n += 1;
        }
        assert!(t
            .alloc(Fd {
                kind: FdKind::Console,
                flags: 0,
            })
            .is_err());
        t.close(0);
        assert_eq!(t.alloc(Fd {
            kind: FdKind::File(1),
            flags: 0,
        })
        .unwrap(), 0);
    }
}
