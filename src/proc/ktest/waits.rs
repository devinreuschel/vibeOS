//! In-guest tests for the sleeps a signal ends (kernel_tests only). Rows:
//! the parent `ktest.rs`'s `TESTS`.

use vibeos::proc::{SIGKILL, wait_signaled};
use vibeos::syscall::SYS_KILL;

use crate::ktest::user::{self, DEFAULT, Image, x86_user_code};
use crate::ktest::{Outcome, sleep_for};
use crate::proc_init;
use crate::proc_init::testing as proc_testing;

// fork; the child sleeps 1000 s at a time, forever; the parent calls
// wait4(child, NULL, 0, NULL) and exits 1 if it returns. Exits 2 if the
// fork fails.
x86_user_code!(
    WAIT4_ON_SLEEPER,
    "
    mov eax, 57
    syscall
    test rax, rax
    js 9f
    jnz 2f
    sub rsp, 16
1:
    mov qword ptr [rsp], 1000
    mov qword ptr [rsp + 8], 0
    mov rdi, rsp
    xor esi, esi
    mov eax, 35
    syscall
    jmp 1b
2:
    mov rdi, rax
    xor esi, esi
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, 1
    mov eax, 60
    syscall
    ud2
9:
    mov edi, 2
    mov eax, 60
    syscall
    ud2
    "
);

fn kill(pid: u32, sig: u32) -> i64 {
    proc_init::dispatch(SYS_KILL, [u64::from(pid), u64::from(sig), 0, 0, 0, 0])
}

/// A `SIGKILL` that lands after `wait4` has entered and before it sleeps
/// ends the caller while its child still lives: the hook posts the kill
/// with no wake, as `sys_kill` finds a caller that is on no queue yet, and
/// the child sleeps until the test kills it, so a `wait4` that slept
/// through the kill would end only with its child.
pub(crate) fn wait4_ends_on_pending_kill() -> Outcome {
    proc_testing::arm_wait4_kill();
    let pid = match user::spawn(&Image::Code(WAIT4_ON_SLEEPER, DEFAULT), &["wait4_kill"]) {
        Ok(pid) => pid,
        Err(e) => {
            proc_testing::disarm_wait4_kill();
            return crate::fail_fmt!("spawn: {}", e.as_str());
        }
    };
    let ended = sleep_for(|| proc_testing::is_zombie(pid));
    proc_testing::disarm_wait4_kill();
    let (caller, child) = proc_testing::wait4_kill_taken();
    // The child ends either way: a waiter that slept through its kill wakes
    // when the child exits, then dies.
    let child = u32::try_from(child).ok().filter(|&c| c != 0);
    let killed = child.map(|c| kill(c, SIGKILL));
    let st = user::wait(pid);
    let child_gone = child.is_none_or(|c| sleep_for(|| proc_testing::tid_of(c).is_none()));
    if caller != pid {
        return crate::fail_fmt!("the wait4 hook took pid {caller}, want {pid}");
    }
    if !ended {
        return crate::fail_fmt!("wait4 slept through a pending SIGKILL until its child exited");
    }
    if st != wait_signaled(SIGKILL) {
        return crate::fail_fmt!("status {st:#x}, want {:#x}", wait_signaled(SIGKILL));
    }
    if killed.is_some_and(|rc| rc != 0) || !child_gone {
        return crate::fail_fmt!("the child {child:?} did not end: kill {killed:?}");
    }
    Outcome::Ok
}
