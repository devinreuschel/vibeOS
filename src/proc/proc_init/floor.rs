//! The ROADMAP §10.5 floor calls the userland needs beyond the fd and
//! process calls: `getdents64`, `fstat`, `nanosleep` and `reboot`. Each handler looks
//! up what it needs, encodes through `vibeos::proc::uabi`, and copies
//! through `uaccess_init` (SYSCALL.md §3.1).

use vibeos::fs::InodeKind;
use vibeos::proc::uabi::{self, Dirent64Writer, RebootCmd};
use vibeos::time::Instant;

use super::*;
use crate::arch::current::UserStat;
use crate::arch::current::power;
use crate::time_init;

/// The most `getdents64` writes in one call: its kernel buffer. A record
/// is at most 88 bytes (`MAX_NAME` 64), so one always fits.
const GETDENTS_MAX: usize = 512;

/// Run `f` on open file `id` under a count of this syscall's own.
fn with_file<R>(id: FileId, f: impl FnOnce(&FileRef) -> Result<R, KError>) -> Result<R, KError> {
    let file = file_init::fget(id).map_err(KError::from)?;
    let r = f(&file);
    let c = file_init::close(file);
    let n = r?;
    c.map_err(KError::from)?;
    Ok(n)
}

/// `getdents64(fd, dirent, count)`: whole `linux_dirent64` records from
/// the directory position, at most [`GETDENTS_MAX`] bytes. The position
/// moves only after the copy succeeds.
pub(super) fn sys_getdents64(fd: u32, dirent: u64, count: u32) -> SysResult {
    let Some(slot) = lookup_fd(fd) else {
        return Err(KError::BadF);
    };
    match slot.kind {
        FdKind::None => Err(KError::BadF),
        FdKind::Console => Err(KError::NotDir),
        FdKind::File { fid, r#gen } => {
            with_file(FileId { fid, r#gen }, |f| getdents_on(f, dirent, count))
        }
    }
}

fn getdents_on(f: &FileRef, dirent: u64, count: u32) -> SysResult {
    // Anything but a directory is `ENOTDIR`, before its position is read,
    // as Linux's `iterate_dir` refuses a file with no directory ops: a
    // device that cannot seek is not `ESPIPE`.
    if file_init::stat(f).map_err(KError::from)?.kind != InodeKind::Dir {
        return Err(KError::NotDir);
    }
    let pos = file_init::seek(f, SeekFrom::Current(0)).map_err(KError::from)?;
    let mut buf = [0u8; GETDENTS_MAX];
    let cap = usize::try_from(count).map_or(GETDENTS_MAX, |c| c.min(GETDENTS_MAX));
    let mut refused = false;
    let (n, next) = {
        let mut w = Dirent64Writer::new(&mut buf[..cap]);
        let next = file_init::readdir_from(f, pos, &mut |d, next| {
            let ok = w.push(u64::from(d.ino), next, d.kind, d.name.as_bytes());
            refused = !ok;
            ok
        })
        .map_err(KError::from)?;
        (w.len(), next)
    };
    if n == 0 {
        return if refused { Err(KError::Inval) } else { Ok(0) };
    }
    uaccess_init::copy_to_user(dirent, &buf[..n]).map_err(KError::from)?;
    file_init::seek(f, SeekFrom::Start(next)).map_err(KError::from)?;
    Ok(n)
}

/// `fstat(fd, statbuf)`: the port's `struct stat` for any descriptor; the
/// console reads as a character device.
pub(super) fn sys_fstat(fd: u32, statbuf: u64) -> SysResult {
    let Some(slot) = lookup_fd(fd) else {
        return Err(KError::BadF);
    };
    let fields = match slot.kind {
        FdKind::None => return Err(KError::BadF),
        FdKind::Console => uabi::console_stat_fields(),
        FdKind::File { fid, r#gen } => {
            let st = with_file(FileId { fid, r#gen }, |f| {
                file_init::stat(f).map_err(KError::from)
            })?;
            uabi::stat_fields(&st)
        }
    };
    let out = UserStat::from_fields(&fields);
    uaccess_init::copy_to_user_val(statbuf, &out).map_err(KError::from)?;
    Ok(0)
}

/// What one pass of `nanosleep`'s loop found.
enum Nap {
    /// A signal whose action is not "ignore" is pending: act on it first.
    Signal,
    /// The thread is on `wait_wq` until the deadline.
    Wait,
}

/// True when `p` has a signal `apply_pending` acts on: a stop, or a
/// pending signal other than `SIGCHLD` and `SIGCONT`, which it ignores.
pub(super) fn signal_acts(p: &Proc) -> bool {
    p.state == ProcState::Stopped || p.pending & !(bit(SIGCHLD) | bit(SIGCONT)) != 0
}

/// `nanosleep(rqtp, rmtp)`: sleep until the `CLOCK_MONOTONIC` deadline
/// `rqtp` gives, on the caller's `wait_wq`, which `sys_kill` wakes, so a
/// `SIGKILL` ends the sleep at once. A wakeup is not the deadline (a child's
/// exit wakes `wait_wq` too), so it loops to the deadline as `sys_wait4`
/// loops. `rmtp` is never written: no handler can interrupt the sleep
/// before ROADMAP §13.8.
pub(super) fn sys_nanosleep(rqtp: u64, _rmtp: u64) -> SysResult {
    let mut ts = [0u8; 16];
    uaccess_init::copy_from_user(&mut ts, rqtp).map_err(KError::from)?;
    let (sec, nsec) = ts.split_at(8);
    let word = |b: &[u8]| b.try_into().map(i64::from_le_bytes);
    let (Ok(sec), Ok(nsec)) = (word(sec), word(nsec)) else {
        return Err(KError::Fault);
    };
    let deadline = uabi::timespec_deadline(time_init::now_ns(), sec, nsec)?;
    let pid = current_pid();
    if pid == 0 {
        // A kernel-side `dispatch` probe: no process, so no signal to end
        // the sleep early.
        thread_init::park(Some(Instant { ns: deadline }));
        return Ok(0);
    }
    while time_init::now_ns() < deadline {
        let nap = thread_init::with_sched(|s| {
            table_locked(|t| {
                let me = t.get_mut(pid)?;
                if signal_acts(me) {
                    return Some(Nap::Signal);
                }
                s.begin_wait(&mut me.wait_wq, Instant { ns: deadline });
                Some(Nap::Wait)
            })
        });
        match nap {
            Some(Nap::Signal) => apply_pending(None),
            Some(Nap::Wait) => {
                thread_init::schedule();
                apply_pending(None);
            }
            None => return Err(KError::Srch),
        }
    }
    Ok(0)
}

/// `reboot(magic1, magic2, cmd, arg)`, in Linux's order: `EPERM` unless
/// the caller's effective uid is 0 (root holds `CAP_SYS_BOOT` until ROADMAP
/// §18.6), then `uabi::reboot_decode`'s magic and command checks. A power-off
/// or restart prints its line and does not return; `CAD_ON` and `CAD_OFF`
/// change nothing, since the keyboard has no Ctrl-Alt-Del action. There is
/// no implicit sync, as on Linux (reboot(2)).
pub(super) fn sys_reboot(magic1: i32, magic2: i32, cmd: u32, arg: u64) -> SysResult {
    let pid = current_pid();
    let euid = if pid == 0 {
        0
    } else {
        with_table(|t| t.get(pid).map(|p| p.creds.euid)).ok_or(KError::Srch)?
    };
    if euid != 0 {
        return Err(KError::Perm);
    }
    match uabi::reboot_decode(magic1, magic2, cmd)? {
        RebootCmd::CadOn | RebootCmd::CadOff => Ok(0),
        RebootCmd::PowerOff => {
            crate::marker!("vibeOS: reboot: power off");
            power::power_off()
        }
        RebootCmd::Restart => {
            crate::marker!("vibeOS: reboot: restart");
            power::restart()
        }
        RebootCmd::Restart2 => {
            // The command string is read, as Linux reads it, and ignored, as
            // x86_64 ignores it.
            let mut buf = [0u8; 256];
            uaccess_init::strncpy_from_user(&mut buf, arg).map_err(KError::from)?;
            crate::marker!("vibeOS: reboot: restart");
            power::restart()
        }
    }
}
