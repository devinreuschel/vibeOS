use super::*;
use vibeos::fmt_util;

pub(super) fn sys_exit(status: u64, _from_signal: bool) -> i64 {
    finish_exit(wait_exited(status as u32), false);
}

pub(super) fn finish_exit(wait_status: u32, _from_fault: bool) -> ! {
    let pid = current_pid();
    if pid == 0 {
        thread_init::exit_current();
    }
    let (old, ppid, fds, tid) = thread_init::with_sched(|s| {
        table_locked(|t| {
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
        addr_space_init::teardown(space.into_inner());
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

pub(super) fn sys_wait4(pid: u64, status: u64, options: u64) -> i64 {
    let self_pid = current_pid();
    if self_pid == 0 {
        return syscall::neg(ECHILD);
    }
    let want = pid as i64;
    let nohang = options & WNOHANG != 0;
    loop {
        let r = thread_init::with_sched(|s| {
            table_locked(|t| {
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
                // After `with_sched`, with no lock held: the child is
                // reaped, and a failed copy returns `EFAULT` without
                // undoing that, as Linux's does (SYSCALL.md §5).
                if status != 0 && uaccess_init::copy_to_user_val(status, &st).is_err() {
                    return syscall::neg(EFAULT);
                }
                return cpid as i64;
            }
            WaitAct::Err(e) => return syscall::neg(e),
            WaitAct::Sleep => {
                thread_init::schedule();
                if let Some(s) = current_space() {
                    set_as(s);
                }
                apply_pending(None);
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

pub(super) fn reap_zombie(t: &mut Table, pid: u32) {
    let i = pid as usize;
    t.procs[i] = Proc::empty();
}

pub(super) fn sys_kill(pid: u64, sig: u64) -> i64 {
    let sig = sig as u32;
    if sig == 0 || sig > 31 {
        return syscall::neg(EINVAL);
    }
    let target = pid as u32;
    let self_pid = current_pid();
    let r = thread_init::with_sched(|s| {
        table_locked(|t| {
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
                }
                SigAct::Cont => {
                    if p.state == ProcState::Stopped {
                        p.state = ProcState::Live;
                        p.pending &= !bit(SIGSTOP);
                        s.wake_all(&mut p.stop_wq);
                    }
                }
                SigAct::Stop => {
                    p.pending |= bit(SIGSTOP);
                    p.state = ProcState::Stopped;
                    s.wake_all(&mut p.wait_wq);
                    s.wake_all(&mut p.stop_wq);
                }
                SigAct::Term => {
                    p.pending |= bit(sig);
                    s.wake_all(&mut p.wait_wq);
                    s.wake_all(&mut p.stop_wq);
                }
            }
            Ok(())
        })
    });
    match r {
        Err(e) => syscall::neg(e),
        Ok(()) => {
            if target == self_pid && default_action(sig) == SigAct::Term {
                finish_exit(wait_signaled(sig), true);
            }
            0
        }
    }
}

// Out of line: `dispatch_frame` keeps only the running syscall's frame,
// and a preempted body carries an interrupt and a switch on top of it.
#[inline(never)]
pub(super) fn sys_psinfo(buf: u64, len: u64) -> i64 {
    if !user_range_ok(buf, len) {
        return syscall::neg(EFAULT);
    }
    let mut tmp = [0u8; 512];
    let n = format_ps(&mut tmp);
    let take = usize::try_from(len).map_or(n, |l| n.min(l));
    if take == 0 {
        return 0;
    }
    match uaccess_init::copy_to_user_partial(buf, &tmp[..take]) {
        0 => syscall::neg(EFAULT),
        c => c as i64,
    }
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
    // One `<pid> <ppid> <state> <name>\n` line per process, whole lines
    // only, written with `fmt_util` into `out` (no allocation, DESIGN §4.4).
    let mut w = 0usize;
    for &(pid, ppid, st, name) in snap.0.iter().take(snap.1) {
        let (mut a, mut b) = ([0u8; 20], [0u8; 20]);
        let parts: [&[u8]; 8] = [
            fmt_util::write_dec(u64::from(pid), &mut a),
            b" ",
            fmt_util::write_dec(u64::from(ppid), &mut b),
            b" ",
            st.name().as_bytes(),
            b" ",
            name.as_bytes(),
            b"\n",
        ];
        let len = parts.iter().map(|p| p.len()).sum::<usize>();
        let Some(mut dst) = out.get_mut(w..).and_then(|r| r.get_mut(..len)) else {
            break;
        };
        for p in parts {
            let (head, rest) = dst.split_at_mut(p.len());
            head.copy_from_slice(p);
            dst = rest;
        }
        w += len;
    }
    w
}

pub fn write_ps(w: &mut impl Write) {
    let mut tmp = [0u8; 512];
    let n = format_ps(&mut tmp);
    let s = core::str::from_utf8(&tmp[..n]).unwrap_or("");
    for line in s.lines() {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a diagnostic line to Serial or the console carries no failure anyone could act on (DESIGN §2.5)"
        )]
        let _ = writeln!(w, "vibeOS: ps: {line}");
    }
}
