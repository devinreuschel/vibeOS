//! The process-side tests of a full table (ROADMAP §10.4).

use vibeos::proc::wait_exited;

use crate::ktest::user::{self, DEFAULT, Image, user_code};
use crate::ktest::{Outcome, quiesce, quiescent_free_frames};
use crate::proc_init;
use crate::sched::ktest::fill_threads;
use crate::thread_init;

// fork(): exit 0 when it returns -EAGAIN, 1 for any other error or a pid;
// a child exits 2.
user_code!(
    FORK_EAGAIN,
    "
    mov eax, 57
    syscall
    mov edi, 2
    test rax, rax
    jz 1f
    xor edi, edi
    cmp rax, -11
    je 1f
    mov edi, 1
1:
    mov eax, 60
    syscall
    ud2
    "
);

/// Run [`FORK_EAGAIN`] and check its fork returned `-EAGAIN`.
fn fork_once() -> Result<(), Outcome> {
    let st = match user::run(&Image::Code(FORK_EAGAIN, DEFAULT), &["fork-full"]) {
        Ok(st) => st,
        Err(e) => return Err(crate::fail_fmt!("spawn: {}", e.as_str())),
    };
    match st {
        s if s == wait_exited(0) => Ok(()),
        s if s == wait_exited(2) => Err(Outcome::Fail("fork made a child on a full table")),
        s => Err(crate::fail_fmt!(
            "status {s:#x}, want exited 0 (fork gave EAGAIN)"
        )),
    }
}

/// `fork` on a full thread table returns `-EAGAIN` (ROADMAP §10.4, F037):
/// with all but one slot held by parked kernel threads, the process's own
/// thread takes the last one, and its fork finds none. What the fork took
/// (a pid and proc slot, a cloned address space, descriptor references) is
/// all given back while the fillers still hold the table.
pub(crate) fn fork_full_thread_table() -> Outcome {
    if !quiesce() {
        return Outcome::Fail("threads did not settle");
    }
    let fill = fill_threads(1);
    let r = fork_full_filled();
    if !fill.release() {
        return Outcome::Fail("fillers did not exit");
    }
    match r {
        Ok(()) => Outcome::Ok,
        Err(o) => o,
    }
}

fn fork_full_filled() -> Result<(), Outcome> {
    let (used, cap) = thread_init::table_usage();
    if used + 1 != cap {
        return Err(crate::fail_fmt!("fill left {used} of {cap} slots used"));
    }
    // The first run warms what a process start maps for good.
    fork_once()?;
    let frames0 = quiescent_free_frames();
    let (procs0, _) = proc_init::table_usage();
    fork_once()?;
    let frames1 = quiescent_free_frames();
    let (procs1, _) = proc_init::table_usage();
    if procs1 != procs0 {
        return Err(crate::fail_fmt!("proc slots used {procs0} -> {procs1}"));
    }
    if frames1 != frames0 {
        return Err(crate::fail_fmt!("frames {frames0} -> {frames1}"));
    }
    Ok(())
}
