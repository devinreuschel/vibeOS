//! In-guest tests for proc's address spaces (kernel_tests only): the
//! two-count object (ROADMAP §10.6, F019) and the fill API. Rows: the
//! parent `ktest.rs`'s `TESTS`.

use vibeos::proc::{SIGKILL, wait_signaled};
use vibeos::syscall::SYS_KILL;

use crate::addr_space_init::testing as as_testing;
use crate::fill_init::testing as fill_testing;
use crate::ktest::user::{self, Image, Layout, x86_user_code};
use crate::ktest::{Outcome, free_frames, quiescent_free_frames, settle_threads};
use crate::proc_init::{self, testing as proc_testing};

// `sched_yield` forever: the process whose space a pin outlives.
x86_user_code!(
    AS_PIN_YIELD,
    "
1:
    mov eax, 24
    syscall
    jmp 1b
    "
);

/// A pin taken from a kernel thread keeps a process's space across that
/// process's exit: its frames return to the buddy when the pin drops, not
/// before, and a pin taken after the exit fails and finds them back
/// (ROADMAP §10.6, F019).
pub(crate) fn as_pin_across_exit() -> Outcome {
    // 1 MiB of writable bss, so the space's frames dwarf a kernel stack's.
    let layout = Layout {
        vaddr: 0x4000_0000,
        memsz: Some(1 << 20),
        writable: true,
    };
    let img = Image::Code(AS_PIN_YIELD, layout);
    // One run first, so heap pages a boot's first spawn maps stay out of
    // the baseline.
    match user::spawn(&img, &["as-pin-warm"]) {
        Ok(p) => {
            kill_and_reap(p);
        }
        Err(e) => return crate::fail_fmt!("warm spawn: {}", e.as_str()),
    }
    let f0 = quiescent_free_frames();
    let pid = match user::spawn(&img, &["as-pin"]) {
        Ok(p) => p,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    let Some(core) = proc_testing::space_core(pid) else {
        kill_and_reap(pid);
        return Outcome::Fail("no space to reference");
    };
    let r = proc_testing::with_space_of(pid, |s| {
        let n = {
            let mm = s.mm();
            mm.user_frames() + mm.pt_frames()
        };
        let f1 = free_frames();
        let st = kill_and_reap(pid);
        if st != wait_signaled(SIGKILL) {
            return Err(crate::fail_fmt!("status {st:#x}, want SIGKILL"));
        }
        // The process's kernel stack goes back too; only the space waits.
        if !settle_threads() {
            return Err(Outcome::Fail("threads did not settle"));
        }
        let held = free_frames();
        if held >= f1 + n {
            return Err(crate::fail_fmt!(
                "{n} space frames back under the pin: {f1} -> {held}"
            ));
        }
        Ok((n, f1, held))
    });
    let (n, f1, held) = match r {
        Some(Ok(v)) => v,
        Some(Err(o)) => return o,
        None => {
            kill_and_reap(pid);
            return Outcome::Fail("no pin while the process lives");
        }
    };
    // The pin's put was the last `users` put: the teardown ran on this
    // thread, and only the root, which `core` keeps, is still out.
    let after = free_frames();
    if after + 1 < f1 + n {
        return crate::fail_fmt!("{n} space frames, but {f1} -> {after} after the pin");
    }
    if core.pin().is_some() {
        return Outcome::Fail("a pin succeeded after the last users put");
    }
    if proc_testing::with_space_of(pid, |_| ()).is_some() {
        return Outcome::Fail("a pin by pid succeeded after the exit");
    }
    drop(core);
    if !user::frames_settle(f0) {
        return crate::fail_fmt!(
            "free {} after the core's free, want {f0}; n {n} f1 {f1} held {held} after {after}",
            free_frames()
        );
    }
    Outcome::Ok
}

/// `SIGKILL` `pid` through `kill`'s syscall body, then reap it; its status.
fn kill_and_reap(pid: u32) -> u32 {
    let rc = proc_init::dispatch(SYS_KILL, [u64::from(pid), u64::from(SIGKILL), 0, 0, 0, 0]);
    if rc != 0 {
        return u32::MAX;
    }
    user::wait(pid)
}

// Marks the first and last page of its 8 MiB bss, forks, and exits with
// the child's status: the child exits 0 when it sees both marks, 1 when
// not; a failed fork exits 2.
x86_user_code!(
    FILL_FORK,
    "
    mov rax, 0x40001000
    mov byte ptr [rax], 0x5a
    mov rax, 0x407ff000
    mov byte ptr [rax], 0xa5
    mov eax, 57
    syscall
    test rax, rax
    jz 2f
    js 3f
    sub rsp, 16
    mov rdi, -1
    mov rsi, rsp
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, dword ptr [rsp]
    shr edi, 8
    and edi, 0xff
    mov eax, 60
    syscall
    ud2
2:
    mov edi, 1
    mov rax, 0x40001000
    cmp byte ptr [rax], 0x5a
    jne 4f
    mov rax, 0x407ff000
    cmp byte ptr [rax], 0xa5
    jne 4f
    xor edi, edi
4:
    mov eax, 60
    syscall
    ud2
3:
    mov edi, 2
    mov eax, 60
    syscall
    ud2
    "
);

/// The fill API holds `PT` for at most one leaf table at a time, with IF
/// on between holds (ROADMAP §10.6): an exec and an 8 MiB fork copy take
/// several bounded holds, none crossing a leaf table or ending with IF
/// off, and the child sees the parent's bytes.
pub(crate) fn fill_pt_hold_bounded() -> Outcome {
    let before = quiescent_free_frames();
    let layout = Layout {
        vaddr: 0x4000_0000,
        memsz: Some(8 << 20),
        writable: true,
    };
    fill_testing::reset();
    as_testing::reset_chunks();
    let rc = user::run(&Image::Code(FILL_FORK, layout), &["fill-fork"]);
    let s = fill_testing::stats();
    let (install_holds, install_max) = as_testing::chunk_stats();
    let st = match rc {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if st != 0 {
        return crate::fail_fmt!("status {st:#x}, want 0 (the child saw both marks)");
    }
    if s.holds < 4 || s.max_pages > 512 || s.crossed != 0 || s.if_off != 0 {
        return crate::fail_fmt!("fill {s:?}");
    }
    if install_holds < 4 || install_max > 512 {
        return crate::fail_fmt!("install: {install_holds} holds, max {install_max} pages");
    }
    if !user::frames_settle(before) {
        return crate::fail_fmt!("free frames {}, want {before}", free_frames());
    }
    Outcome::Ok
}
