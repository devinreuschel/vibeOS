//! In-guest test of the open-file table under fork and dup churn
//! (ROADMAP §10.4, F055): every slot the run took is free once its
//! processes are reaped.

use vibeos::fs::{FileId, FileRef};
use vibeos::kalloc::TryVec;
use vibeos::proc::wait_exited;

use super::{hooks, read_all, unlink_quiet};
use crate::file_init;
use crate::ktest::user::{self, DEFAULT, Image, user_code};
use crate::ktest::{Outcome, fid};
use crate::time_init;

/// Each open-file slot's `(used, refs, gen)`; `None` when the table
/// cannot be allocated.
fn holders() -> Option<TryVec<(bool, u16, u16)>> {
    hooks::table()
}

// Opens /f55a.txt (O_RDWR|O_CREAT|O_TRUNC) as fd 3 and forks. The parent
// writes "P" through fd 3 1,000 times, waits for the child, and exits 0
// if the child exited 0. The child loops 1,000 times: dup(3), close the
// dup, open /f55b.txt (O_WRONLY|O_CREAT|O_APPEND), write "c", close.
// Exit codes: 1 open, 2 fork, 3 parent write, 4 wait4, 6 child signaled,
// 50 + n child exit n; the child's own: 21 dup, 22 close dup, 23 open,
// 24 write, 25 close.
user_code!(
    F55_CHURN,
    "
    lea rdi, [rip + 90f]
    mov esi, 0x242
    xor edx, edx
    mov eax, 2
    syscall
    mov edi, 1
    cmp rax, 3
    jne 80f
    mov eax, 57
    syscall
    mov edi, 2
    test rax, rax
    js 80f
    jz 10f
    mov r12, rax
    mov ebx, 1000
1:
    mov edi, 3
    lea rsi, [rip + 92f]
    mov edx, 1
    mov eax, 1
    syscall
    cmp rax, 1
    jne 3f
    dec ebx
    jnz 1b
    sub rsp, 16
    mov dword ptr [rsp], -1
    mov rdi, r12
    mov rsi, rsp
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, 4
    cmp rax, r12
    jne 80f
    mov eax, dword ptr [rsp]
    xor edi, edi
    test eax, eax
    jz 80f
    mov edi, 6
    test eax, 0x7f
    jnz 80f
    shr eax, 8
    and eax, 0xff
    lea edi, [rax + 50]
    jmp 80f
3:
    mov edi, 3
    jmp 80f
10:
    mov ebx, 1000
11:
    mov edi, 3
    mov eax, 32
    syscall
    mov edi, 21
    test rax, rax
    js 80f
    mov rdi, rax
    mov eax, 3
    syscall
    mov edi, 22
    test rax, rax
    jnz 80f
    lea rdi, [rip + 91f]
    mov esi, 0x441
    xor edx, edx
    mov eax, 2
    syscall
    mov edi, 23
    test rax, rax
    js 80f
    mov r13, rax
    mov rdi, r13
    lea rsi, [rip + 93f]
    mov edx, 1
    mov eax, 1
    syscall
    mov edi, 24
    cmp rax, 1
    jne 80f
    mov rdi, r13
    mov eax, 3
    syscall
    mov edi, 25
    test rax, rax
    jnz 80f
    dec ebx
    jnz 11b
    xor edi, edi
80:
    mov eax, 60
    syscall
    ud2
90:
    .asciz \"/f55a.txt\"
91:
    .asciz \"/f55b.txt\"
92:
    .ascii \"P\"
93:
    .ascii \"c\"
    "
);

const CHURN: usize = 1000;

pub(crate) fn test_file_table_fork_churn() -> Outcome {
    if unlink_quiet("/f55a.txt").is_err() || unlink_quiet("/f55b.txt").is_err() {
        return Outcome::Fail("unlink before");
    }
    let Some(base) = holders() else {
        return Outcome::Fail("no memory for the file table copy");
    };
    hooks::set_write_yield(true);
    let st = user::run(&Image::Code(F55_CHURN, DEFAULT), &["f55churn"]);
    hooks::set_write_yield(false);
    let out = check_churn(st, &base);
    let ua = unlink_quiet("/f55a.txt");
    let ub = unlink_quiet("/f55b.txt");
    if !matches!(out, Outcome::Ok) {
        return out;
    }
    if ua.is_err() || ub.is_err() {
        return Outcome::Fail("unlink after");
    }
    Outcome::Ok
}

fn check_churn(st: Result<u32, crate::user_init::LoadError>, base: &[(bool, u16, u16)]) -> Outcome {
    let st = match st {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if st != wait_exited(0) {
        return crate::fail_fmt!("status {st:#x}, want exited 0");
    }
    let mut buf = [0u8; CHURN + 64];
    match read_all("/f55a.txt", &mut buf) {
        Ok(n) if n == CHURN && buf[..n].iter().all(|&b| b == b'P') => {}
        Ok(n) => {
            let p = buf[..n].iter().filter(|&&b| b == b'P').count();
            return crate::fail_fmt!("f55a: {n} bytes, {p} P");
        }
        Err(e) => return crate::fail_fmt!("f55a: {}", e.as_str()),
    }
    match read_all("/f55b.txt", &mut buf) {
        Ok(n) if n == CHURN && buf[..n].iter().all(|&b| b == b'c') => {}
        Ok(n) => {
            let p = buf[..n].iter().filter(|&&b| b == b'P').count();
            return crate::fail_fmt!("f55b: {n} bytes, {p} P");
        }
        Err(e) => return crate::fail_fmt!("f55b: {}", e.as_str()),
    }
    let Some(now) = holders() else {
        return Outcome::Fail("no memory for the file table copy");
    };
    let held = |s: &(bool, u16, u16)| (s.0, s.1);
    for (i, (n, b)) in now.iter().zip(base.iter()).enumerate() {
        if held(n) != held(b) {
            return slot_differs(i, *n, *b);
        }
    }
    if now.len() != base.len() {
        return Outcome::Fail("file table changed length");
    }
    Outcome::Ok
}

/// The failure for open-file slot `i`, `(used, refs, gen)` `n` against
/// `b` at the baseline. It names what can tell a leaked reference from
/// another opener's (ROADMAP §10.2): how often the slot was freed since
/// the baseline, the inode it holds beside the test's two, the processes
/// that can hold a file, and whether the slot matches its baseline again
/// before the run's deadline.
fn slot_differs(i: usize, n: (bool, u16, u16), b: (bool, u16, u16)) -> Outcome {
    let ino = |p: &str| fid::stat_path(p).map_or(0, |s| s.ino);
    let (fa, fb) = (ino("/f55a.txt"), ino("/f55b.txt"));
    let id = FileId {
        fid: i as u16,
        r#gen: n.2,
    };
    // A count of the test's own, so the slot's inode outlives the stat.
    let holder = fid::addref(id).map_or(0, |()| {
        let st = file_init::stat(&FileRef::from_raw(id));
        let c = fid::close(id);
        match (st, c) {
            (Ok(s), Ok(())) => s.ino,
            _ => 0,
        }
    });
    let procs = crate::proc_init::testing::holding_count();
    let t0 = time_init::now_ns();
    let back = crate::ktest::wait_for(|| {
        holders()
            .and_then(|t| t.get(i).copied())
            .is_some_and(|s| (s.0, s.1) == (b.0, b.1))
    });
    let ms = time_init::now_ns().saturating_sub(t0) / 1_000_000;
    let gens = n.2.wrapping_sub(b.2);
    let how = if back { "back" } else { "held" };
    crate::fail_fmt!(
        "slot {i} ({}, {}) gen +{gens}, was ({}, {}); ino {holder} (f55a {fa}, f55b {fb}); {procs} procs; {how} at {ms} ms",
        n.0,
        n.1,
        b.0,
        b.1
    )
}
