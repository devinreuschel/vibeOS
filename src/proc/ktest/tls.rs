//! Per-thread `FS_BASE` (ROADMAP §11.6, F022).

use crate::arch::current::syscall_nr::SYS_KILL;
use vibeos::fs::{O_CREAT, O_TRUNC, O_WRONLY};
use vibeos::proc::{SIGKILL, wait_signaled};
use vibeos::thread::ThreadId;

use crate::ktest::user::{self, DEFAULT, Image, elf_bytes, user_code};
use crate::ktest::{Outcome, fid};
use crate::proc_init;
use crate::thread_init;

// ebx, not ecx: `syscall` overwrites rcx with the return RIP.
user_code!(
    TLS_LOOP,
    "
    mov ebx, 80
1:
    mov rax, qword ptr fs:[-8]
    mov r12, 0x1122334455667788
    cmp rax, r12
    jne 9f
    mov eax, 24
    syscall
    dec ebx
    jnz 1b
    xor edi, edi
    mov eax, 60
    syscall
9:
    mov edi, 2
    mov eax, 60
    syscall
    ud2
    "
);

user_code!(
    YIELD_LOOP,
    "
    mov ebx, 80
1:
    mov eax, 24
    syscall
    dec ebx
    jnz 1b
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

user_code!(
    YIELD_FOREVER,
    "
1:
    mov eax, 24
    syscall
    jmp 1b
    "
);

user_code!(
    TLS_EXIT0,
    "
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

user_code!(
    EXEC_NOTLS,
    "
    lea rdi, [rip + 1f]
    xor esi, esi
    xor edx, edx
    mov eax, 59
    syscall
    mov edi, 3
    mov eax, 60
    syscall
1:
    .asciz \"/tmp/vibeos_notls\"
    "
);

fn exited0(st: u32) -> bool {
    vibeos::proc::wifexited(st) && vibeos::proc::wexitstatus(st) == 0
}

fn tls_base_of(pid: u32) -> Option<u64> {
    let tid = proc_init::tid_of(pid)?;
    let t = thread_init::tcb_ptr(ThreadId(tid));
    if t.is_null() {
        None
    } else {
        // SAFETY: invariant I9: the TCB lives until wait reaps it;
        // established by `proc_init::tid_of`.
        Some(unsafe { (*t).tls_base })
    }
}

fn write_notls() -> Result<(), Outcome> {
    let elf = elf_bytes(&Image::Code(TLS_EXIT0, DEFAULT));
    let f = fid::open("/tmp/vibeos_notls", O_WRONLY | O_CREAT | O_TRUNC, 0o755)
        .map_err(|_| Outcome::Fail("creat notls"))?;
    let mut off = 0;
    while off < elf.len() {
        let n = fid::write(f, &elf[off..]).map_err(|_| Outcome::Fail("write notls"))?;
        if n == 0 {
            let _ = fid::close(f);
            return Err(Outcome::Fail("short write"));
        }
        off += n;
    }
    fid::close(f).map_err(|_| Outcome::Fail("close notls"))?;
    Ok(())
}

fn unlink_notls() {
    let _ = fid::unlink_path("/tmp/vibeos_notls", false);
}

/// A `PT_TLS` process keeps its TLS across another process's exit, kill,
/// execve of a non-TLS image, and a non-TLS first entry (F022).
pub(crate) fn test_tls_survive() -> Outcome {
    let out = tls_survive_body();
    unlink_notls();
    out
}

fn tls_survive_body() -> Outcome {
    if let Err(e) = write_notls() {
        return e;
    }
    let a = match user::spawn(&Image::TlsCode(TLS_LOOP, DEFAULT), &["tls_p"]) {
        Ok(p) => p,
        Err(e) => return crate::fail_fmt!("spawn p: {}", e.as_str()),
    };
    if tls_base_of(a).unwrap_or(0) == 0 {
        let _ = user::wait(a);
        return Outcome::Fail("pt_tls base 0");
    }
    match user::run(&Image::Code(TLS_EXIT0, DEFAULT), &["tls_exit"]) {
        Ok(st) if exited0(st) => {}
        Ok(st) => {
            let _ = user::wait(a);
            return crate::fail_fmt!("exit status {st:#x}");
        }
        Err(e) => {
            let _ = user::wait(a);
            return crate::fail_fmt!("exit spawn: {}", e.as_str());
        }
    }
    let q = match user::spawn(&Image::Code(YIELD_FOREVER, DEFAULT), &["tls_kill"]) {
        Ok(p) => p,
        Err(e) => {
            let _ = user::wait(a);
            return crate::fail_fmt!("kill spawn: {}", e.as_str());
        }
    };
    let _ = proc_init::dispatch(SYS_KILL, [u64::from(q), u64::from(SIGKILL), 0, 0, 0, 0]);
    let kst = user::wait(q);
    if kst != wait_signaled(SIGKILL) {
        let _ = user::wait(a);
        return crate::fail_fmt!("kill status {kst:#x}");
    }
    match user::run(&Image::Code(EXEC_NOTLS, DEFAULT), &["tls_exec"]) {
        Ok(st) if exited0(st) => {}
        Ok(st) => {
            let _ = user::wait(a);
            return crate::fail_fmt!("exec status {st:#x}");
        }
        Err(e) => {
            let _ = user::wait(a);
            return crate::fail_fmt!("exec spawn: {}", e.as_str());
        }
    }
    match user::run(&Image::Code(TLS_EXIT0, DEFAULT), &["tls_first"]) {
        Ok(st) if exited0(st) => {}
        Ok(st) => {
            let _ = user::wait(a);
            return crate::fail_fmt!("first status {st:#x}");
        }
        Err(e) => {
            let _ = user::wait(a);
            return crate::fail_fmt!("first spawn: {}", e.as_str());
        }
    }
    let st = user::wait(a);
    if !exited0(st) {
        return crate::fail_fmt!("pt_tls status {st:#x}");
    }
    Outcome::Ok
}

/// A `PT_TLS` process and a TP=0 process each keep their base across
/// `sched_yield` (F022).
pub(crate) fn test_tls_yield() -> Outcome {
    let p = match user::spawn(&Image::TlsCode(TLS_LOOP, DEFAULT), &["tls_y_p"]) {
        Ok(p) => p,
        Err(e) => return crate::fail_fmt!("spawn p: {}", e.as_str()),
    };
    let r = match user::spawn(&Image::Code(YIELD_LOOP, DEFAULT), &["tls_y_r"]) {
        Ok(p) => p,
        Err(e) => {
            let _ = user::wait(p);
            return crate::fail_fmt!("spawn r: {}", e.as_str());
        }
    };
    thread_init::sleep_ms(20);
    if tls_base_of(p).unwrap_or(0) == 0 || tls_base_of(r).unwrap_or(1) != 0 {
        let _ = user::wait(p);
        let _ = user::wait(r);
        return Outcome::Fail("bases crossed");
    }
    let (sp, sr) = (user::wait(p), user::wait(r));
    if exited0(sp) && exited0(sr) {
        Outcome::Ok
    } else {
        crate::fail_fmt!("status {sp:#x} {sr:#x}")
    }
}
