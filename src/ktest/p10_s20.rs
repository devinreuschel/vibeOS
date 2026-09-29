//! In-guest tests of P10-S20, Fallible fork, execve and open (DESIGN §8.2).

use core::fmt;

use vibeos::fs::FsError;
use vibeos::kalloc::{TryBox, TryVec};
use vibeos::proc::{SIGKILL, wait_exited, wait_signaled};
use vibeos::syscall::SYS_KILL;

use super::user::{self, Image, user_code};
use super::{Outcome, free_frames, spin_until_ns};
use crate::file_init;
use crate::heap_init::fail_after::{self, Scope, Seen};
use crate::proc_init;
use crate::thread_init;
use crate::time_init;

/// Counts the lines written to it.
struct LineCount(usize);

impl fmt::Write for LineCount {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0 += s.bytes().filter(|&b| b == b'\n').count();
        Ok(())
    }
}

/// Wait up to 5 s until no process, zombies included, holds a slot. Call it
/// only while the hook is disarmed: `write_ps` allocates.
fn no_process() -> bool {
    let deadline = time_init::now_ns().saturating_add(5_000_000_000);
    loop {
        let mut w = LineCount(0);
        proc_init::write_ps(&mut w);
        if w.0 == 0 {
            return true;
        }
        if time_init::now_ns() >= deadline {
            return false;
        }
        thread_init::sleep_ms(10);
    }
}

/// Arms the hook; its `Drop` disarms, so a failing case leaves it off.
struct Armed(bool);

impl Armed {
    fn new(budget: usize, scope: Scope) -> Self {
        fail_after::arm(budget, scope);
        Self(true)
    }

    fn disarm(mut self) -> Seen {
        self.0 = false;
        fail_after::disarm()
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        if self.0 {
            // A failed case disarms only to leave the hook off.
            fail_after::disarm();
        }
    }
}

pub(super) fn test_kalloc_fail_after_hook() -> Outcome {
    if !no_process() {
        return Outcome::Fail("a process is still running");
    }
    let me = thread_init::current_id();

    let armed = Armed::new(1, Scope::Thread(me));
    let first = TryBox::try_new(1u64);
    let second = TryBox::try_new(2u64);
    let seen = armed.disarm();
    if first.is_err() || second.is_ok() {
        return crate::fail_fmt!(
            "TryBox under budget 1: first ok {}, second ok {}",
            first.is_ok(),
            second.is_ok()
        );
    }
    let want = Seen {
        counted: 2,
        refused: 1,
    };
    if seen != want {
        return crate::fail_fmt!("TryBox counts {seen:?}, want {want:?}");
    }
    drop(first);

    let armed = Armed::new(1, Scope::Thread(me));
    let mut v = match TryVec::<u64>::try_with_capacity(8) {
        Ok(v) => v,
        Err(_) => return Outcome::Fail("TryVec::try_with_capacity(8) refused under budget 1"),
    };
    let grown = v.try_reserve(4096);
    let seen = armed.disarm();
    if grown.is_ok() {
        return Outcome::Fail("TryVec::try_reserve(4096) past the budget succeeded");
    }
    if v.capacity() < 8 || !v.is_empty() {
        return Outcome::Fail("a refused try_reserve changed the vector");
    }
    if seen != want {
        return crate::fail_fmt!("TryVec counts {seen:?}, want {want:?}");
    }
    drop(v);

    let armed = Armed::new(0, Scope::Processes { from_syscall: 1 });
    let b = TryBox::try_new(3u64);
    let seen = armed.disarm();
    if b.is_err() {
        return Outcome::Fail("a kernel thread's TryBox was refused under a process scope");
    }
    let none = Seen {
        counted: 0,
        refused: 0,
    };
    if seen != none {
        return crate::fail_fmt!("process scope counted a kernel thread: {seen:?}");
    }
    Outcome::Ok
}

// fork once. Child: exit(0). Parent: wait4(pid, 0, 0) must return pid
// (exit 4 otherwise), exit 0. On -ENOMEM: wait4(-1, 0, WNOHANG) must
// return -ECHILD (exit 3 otherwise: a child exists), exit 12. Any other
// fork error exits 1.
user_code!(
    FORK_ONCE,
    "
    mov eax, 57
    syscall
    test rax, rax
    jz 7f
    js 3f
    mov r12, rax
    mov rdi, r12
    xor esi, esi
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, 4
    cmp rax, r12
    jne 9f
    xor edi, edi
    jmp 9f
3:
    mov edi, 1
    cmp rax, -12
    jne 9f
    mov rdi, -1
    xor esi, esi
    mov edx, 1
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, 3
    cmp rax, -10
    jne 9f
    mov edi, 12
    jmp 9f
7:
    xor edi, edi
9:
    mov eax, 60
    syscall
    ud2
    "
);

// Push a canary, then execve("/hello", ["/hello", "s20", NULL], NULL).
// /hello exits 42. A return must be -ENOMEM (exit 1 otherwise) with the
// canary unchanged (exit 2 otherwise): exit 12.
user_code!(
    EXEC_HELLO,
    "
    mov rax, 0x5332305f43414e41
    push rax
    lea rdi, [rip + 6f]
    lea rax, [rip + 7f]
    push 0
    push rax
    push rdi
    mov rsi, rsp
    xor edx, edx
    mov eax, 59
    syscall
    add rsp, 24
    mov edi, 1
    cmp rax, -12
    jne 9f
    mov edi, 2
    mov rcx, 0x5332305f43414e41
    cmp [rsp], rcx
    jne 9f
    mov edi, 12
9:
    mov eax, 60
    syscall
    ud2
6:
    .asciz \"/hello\"
7:
    .asciz \"s20\"
    "
);

// open("/hello", O_RDONLY). On an fd: close it, exit 0. On -ENOMEM:
// close(3) must return -EBADF (exit 5 otherwise: an fd leaked), exit 12.
// Any other error exits 1.
user_code!(
    OPEN_RO,
    "
    lea rdi, [rip + 6f]
    xor esi, esi
    xor edx, edx
    mov eax, 2
    syscall
    test rax, rax
    js 3f
    mov rdi, rax
    mov eax, 3
    syscall
    xor edi, edi
    jmp 9f
3:
    mov edi, 1
    cmp rax, -12
    jne 9f
    mov edi, 3
    mov eax, 3
    syscall
    mov edi, 5
    cmp rax, -9
    jne 9f
    mov edi, 12
9:
    mov eax, 60
    syscall
    ud2
6:
    .asciz \"/hello\"
    "
);

// open("/vibe/s20", O_CREAT | O_RDWR, 0644), then as OPEN_RO.
user_code!(
    OPEN_CREAT,
    "
    lea rdi, [rip + 6f]
    mov esi, 0x42
    mov edx, 0x1a4
    mov eax, 2
    syscall
    test rax, rax
    js 3f
    mov rdi, rax
    mov eax, 3
    syscall
    xor edi, edi
    jmp 9f
3:
    mov edi, 1
    cmp rax, -12
    jne 9f
    mov edi, 3
    mov eax, 3
    syscall
    mov edi, 5
    cmp rax, -9
    jne 9f
    mov edi, 12
9:
    mov eax, 60
    syscall
    ud2
6:
    .asciz \"/vibe/s20\"
    "
);

// fork (#1). Child: getpid (#1), exit(42) (#2). Parent: wait4(child, &st,
// 0) (#2) must return the child (exit 7 otherwise) with st 0x2A00 (exit 6
// otherwise): exit 0. A failed fork exits 8.
user_code!(
    EXIT_WAIT,
    "
    mov eax, 57
    syscall
    test rax, rax
    js 8f
    jz 7f
    mov r12, rax
    sub rsp, 16
    mov dword ptr [rsp], -1
    mov rdi, r12
    mov rsi, rsp
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, 7
    cmp rax, r12
    jne 9f
    mov edi, 6
    cmp dword ptr [rsp], 0x2A00
    jne 9f
    xor edi, edi
    jmp 9f
7:
    mov eax, 39
    syscall
    mov edi, 42
    jmp 9f
8:
    mov edi, 8
9:
    mov eax, 60
    syscall
    ud2
    "
);

// mmap 16 MiB anonymous read-write (#1; exit 8 on failure), store to each
// of its 4,096 pages, spin 2^26 iterations, munmap it (#2; exit 7 unless
// 0), then sched_yield until killed.
user_code!(
    MUNMAP_16M,
    "
    xor edi, edi
    mov esi, 0x1000000
    mov edx, 3
    mov r10d, 0x22
    mov r8, -1
    xor r9d, r9d
    mov eax, 9
    syscall
    mov edi, 8
    cmp rax, -4096
    ja 9f
    mov r12, rax
    mov rdx, rax
    mov ecx, 4096
2:
    mov byte ptr [rdx], 1
    add rdx, 4096
    dec rcx
    jnz 2b
    mov ecx, 0x4000000
3:
    dec rcx
    jnz 3b
    mov rdi, r12
    mov esi, 0x1000000
    mov eax, 11
    syscall
    mov edi, 7
    test rax, rax
    jnz 9f
4:
    mov eax, 24
    syscall
    jmp 4b
9:
    mov eax, 60
    syscall
    ud2
    "
);

const CREAT_PATH: &[u8] = b"/vibe/s20";
const ENOMEM: u32 = 12;
const NONE: Seen = Seen {
    counted: 0,
    refused: 0,
};

/// Kill `pid` and reap it: a failing case leaves no process behind.
fn kill_and_wait(pid: u32) {
    let r = proc_init::dispatch(SYS_KILL, [u64::from(pid), u64::from(SIGKILL), 0, 0, 0, 0]);
    if r == 0 {
        user::wait(pid);
    }
}

/// The exit code of a normal exit, or `None` for a signal.
fn exit_code(st: u32) -> Option<u32> {
    if st & 0x7f == 0 {
        Some((st >> 8) & 0xff)
    } else {
        None
    }
}

/// Arm at n = 0, 1, … until `prog` runs with nothing refused. Every run
/// that the hook refused must exit ENOMEM; the unrefused run exits `ok`
/// and comes at n >= `min`, the allocations the call is known to make.
fn nomem_loop(name: &str, prog: &'static [u8], ok: u32, min: usize) -> Outcome {
    let img = Image::Code(prog, user::DEFAULT);
    for n in 0..=64usize {
        if !no_process() {
            return crate::fail_fmt!("{name} n={n}: a process is still running");
        }
        let armed = Armed::new(n, Scope::Processes { from_syscall: 1 });
        let pid = match user::spawn(&img, &["s20"]) {
            Ok(p) => p,
            Err(e) => return crate::fail_fmt!("{name} n={n}: spawn: {}", e.as_str()),
        };
        let st = user::wait(pid);
        let seen = armed.disarm();
        let Some(code) = exit_code(st) else {
            return crate::fail_fmt!("{name} n={n}: status {st:#x}, not an exit");
        };
        if seen.refused > 0 {
            if code != ENOMEM {
                return crate::fail_fmt!("{name} n={n}: exit {code} with {seen:?}, want 12");
            }
            continue;
        }
        if code != ok {
            return crate::fail_fmt!("{name} n={n}: exit {code} with nothing refused, want {ok}");
        }
        if n < min {
            return crate::fail_fmt!("{name}: {n} allocations, want at least {min}");
        }
        crate::klog!(
            vibeos::log::Level::Info,
            "ktest: kalloc_nomem: {name} makes {n} allocations"
        );
        return Outcome::Ok;
    }
    crate::fail_fmt!("{name}: still refused at n=64")
}

/// Armed at zero from each process's second syscall: the parent gets
/// the child's status and the hook counts nothing.
fn exit_wait() -> Outcome {
    if !no_process() {
        return Outcome::Fail("exit_wait: a process is still running");
    }
    let armed = Armed::new(0, Scope::Processes { from_syscall: 2 });
    let pid = match user::spawn(&Image::Code(EXIT_WAIT, user::DEFAULT), &["s20"]) {
        Ok(p) => p,
        Err(e) => return crate::fail_fmt!("exit_wait: spawn: {}", e.as_str()),
    };
    let st = user::wait(pid);
    let seen = armed.disarm();
    if st != wait_exited(0) {
        return crate::fail_fmt!("exit_wait: status {st:#x}, want exit 0");
    }
    if seen != NONE {
        return crate::fail_fmt!("exit_wait: hook saw {seen:?} after the fork");
    }
    Outcome::Ok
}

/// Armed at zero from the `munmap`: it returns 0 and the free-frame
/// count rises by at least 4,096 while the program is still alive.
fn munmap_16m() -> Outcome {
    if !no_process() {
        return Outcome::Fail("munmap_16m: a process is still running");
    }
    let f0 = free_frames();
    let armed = Armed::new(0, Scope::Processes { from_syscall: 2 });
    let pid = match user::spawn(&Image::Code(MUNMAP_16M, user::DEFAULT), &["s20"]) {
        Ok(p) => p,
        Err(e) => return crate::fail_fmt!("munmap_16m: spawn: {}", e.as_str()),
    };
    if !spin_until_ns(|| free_frames() + 4096 <= f0, 30_000_000_000) {
        kill_and_wait(pid);
        return crate::fail_fmt!("munmap_16m: no dip below {f0} - 4096");
    }
    let t0 = time_init::now_ns();
    let mut fmin = free_frames();
    loop {
        let f = free_frames();
        fmin = fmin.min(f);
        if f >= fmin + 4096 {
            break;
        }
        if time_init::now_ns().saturating_sub(t0) > 30_000_000_000 {
            kill_and_wait(pid);
            return crate::fail_fmt!("munmap_16m: no rise from {fmin}");
        }
        thread_init::yield_now();
    }
    let r = proc_init::dispatch(SYS_KILL, [u64::from(pid), u64::from(SIGKILL), 0, 0, 0, 0]);
    if r != 0 {
        let st = user::wait(pid);
        return crate::fail_fmt!("munmap_16m: kill {r}: program gone, status {st:#x}");
    }
    let st = user::wait(pid);
    let seen = armed.disarm();
    if st != wait_signaled(SIGKILL) {
        return crate::fail_fmt!("munmap_16m: status {st:#x}, want SIGKILL");
    }
    if seen.refused != 0 {
        return crate::fail_fmt!("munmap_16m: hook refused {seen:?}");
    }
    Outcome::Ok
}

pub(super) fn test_kalloc_nomem() -> Outcome {
    // fork takes at least the address-space slot and execve that slot and
    // its file buffer; the in-memory opens below may take none.
    let loops: [(&str, &'static [u8], u32, usize); 4] = [
        ("fork_once", FORK_ONCE, 0, 1),
        ("exec_hello", EXEC_HELLO, 42, 2),
        ("open_ro", OPEN_RO, 0, 0),
        ("open_creat", OPEN_CREAT, 0, 0),
    ];
    for (name, prog, ok, min) in loops {
        let out = nomem_loop(name, prog, ok, min);
        if name == "open_creat" {
            match file_init::unlink(CREAT_PATH) {
                Ok(()) | Err(FsError::NotFound) => {}
                Err(e) => return crate::fail_fmt!("unlink /vibe/s20: {}", e.as_str()),
            }
        }
        if !matches!(out, Outcome::Ok) {
            return out;
        }
    }
    let out = exit_wait();
    if !matches!(out, Outcome::Ok) {
        return out;
    }
    munmap_16m()
}
