//! EL0 TLS, FP, and SVE proofs (ROADMAP §11.6, F022, F069, F130).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vibeos::fs::{O_CREAT, O_TRUNC, O_WRONLY};
use vibeos::proc::{SIGILL, SIGKILL, wait_signaled};
use vibeos::thread::ThreadId;

use crate::arch::current::syscall_nr;
use crate::ktest::user::{self, DEFAULT, Image, TLS_MAGIC, elf_bytes, user_code};
use crate::ktest::{Outcome, fid, spawn_thread_on};
use crate::proc_init;
use crate::thread_init;

user_code!(
    TLS_LOOP,
    "
    mov w19, #80
1:
    mrs x0, tpidr_el0
    ldr x1, [x0, #16]
    movz x2, #0x7788
    movk x2, #0x5566, lsl #16
    movk x2, #0x3344, lsl #32
    movk x2, #0x1122, lsl #48
    cmp x1, x2
    b.ne 9f
    mov x8, #124
    svc #0
    subs w19, w19, #1
    b.ne 1b
    mov x0, xzr
    mov x8, #93
    svc #0
9:
    mov x0, #2
    mov x8, #93
    svc #0
    "
);

user_code!(
    TLS_ZERO,
    "
    mov w19, #80
1:
    mrs x0, tpidr_el0
    cbnz x0, 9f
    mov x8, #124
    svc #0
    subs w19, w19, #1
    b.ne 1b
    mov x0, xzr
    mov x8, #93
    svc #0
9:
    mov x0, #1
    mov x8, #93
    svc #0
    "
);

user_code!(
    TLS_MSR_A,
    "
    movz x19, #0xa000
    msr tpidr_el0, x19
    mov w20, #80
1:
    mrs x0, tpidr_el0
    cmp x0, x19
    b.ne 9f
    mov x8, #124
    svc #0
    subs w20, w20, #1
    b.ne 1b
    mov x0, xzr
    mov x8, #93
    svc #0
9:
    mov x0, #1
    mov x8, #93
    svc #0
    "
);

user_code!(
    TLS_MSR_B,
    "
    movz x19, #0xb000
    msr tpidr_el0, x19
    mov w20, #80
1:
    mrs x0, tpidr_el0
    cmp x0, x19
    b.ne 9f
    mov x8, #124
    svc #0
    subs w20, w20, #1
    b.ne 1b
    mov x0, xzr
    mov x8, #93
    svc #0
9:
    mov x0, #1
    mov x8, #93
    svc #0
    "
);

user_code!(
    YIELD_FOREVER,
    "
1:
    mov x8, #124
    svc #0
    b 1b
    "
);

user_code!(
    EXIT0,
    "
    mov x0, xzr
    mov x8, #93
    svc #0
    "
);

user_code!(
    EXEC_NOTLS,
    "
    adr x0, 1f
    mov x1, xzr
    mov x2, xzr
    mov x8, #221
    svc #0
    mov x0, #3
    mov x8, #93
    svc #0
1:
    .asciz \"/tmp/vibeos_notls\"
    "
);

user_code!(
    FP_YIELD,
    "
    // movi v0..v31.16b, #0x5a (soft-float kernel cannot write `vN`)
    .inst 0x4f02e740
    .inst 0x4f02e741
    .inst 0x4f02e742
    .inst 0x4f02e743
    .inst 0x4f02e744
    .inst 0x4f02e745
    .inst 0x4f02e746
    .inst 0x4f02e747
    .inst 0x4f02e748
    .inst 0x4f02e749
    .inst 0x4f02e74a
    .inst 0x4f02e74b
    .inst 0x4f02e74c
    .inst 0x4f02e74d
    .inst 0x4f02e74e
    .inst 0x4f02e74f
    .inst 0x4f02e750
    .inst 0x4f02e751
    .inst 0x4f02e752
    .inst 0x4f02e753
    .inst 0x4f02e754
    .inst 0x4f02e755
    .inst 0x4f02e756
    .inst 0x4f02e757
    .inst 0x4f02e758
    .inst 0x4f02e759
    .inst 0x4f02e75a
    .inst 0x4f02e75b
    .inst 0x4f02e75c
    .inst 0x4f02e75d
    .inst 0x4f02e75e
    .inst 0x4f02e75f
    mov w19, #20000
1:
    mov x8, #124
    svc #0
    subs w19, w19, #1
    b.ne 1b
    .inst 0x4e083c00
    movz x1, #0x5a5a
    movk x1, #0x5a5a, lsl #16
    movk x1, #0x5a5a, lsl #32
    movk x1, #0x5a5a, lsl #48
    cmp x0, x1
    b.ne 9f
    mov x0, xzr
    mov x8, #93
    svc #0
9:
    mov x0, #1
    mov x8, #93
    svc #0
    "
);

user_code!(
    FP_PROBE,
    "
    .inst 0x4e083c00
    movz x1, #0x5a5a
    movk x1, #0x5a5a, lsl #16
    movk x1, #0x5a5a, lsl #32
    movk x1, #0x5a5a, lsl #48
    cmp x0, x1
    b.eq 9f
    .inst 0x4e083fe0
    cmp x0, x1
    b.eq 9f
    mov x0, xzr
    mov x8, #93
    svc #0
9:
    mov x0, #1
    mov x8, #93
    svc #0
    "
);

user_code!(
    SVE_PTRUE,
    "
    .inst 0x2518e3e0
    mov x0, xzr
    mov x8, #93
    svc #0
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
    let elf = elf_bytes(&Image::Code(EXIT0, DEFAULT));
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
pub(crate) fn test_el0_tls_survive() -> Outcome {
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
    match user::run(&Image::Code(EXIT0, DEFAULT), &["tls_exit"]) {
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
    let _ = proc_init::dispatch(
        syscall_nr::SYS_KILL,
        [u64::from(q), u64::from(SIGKILL), 0, 0, 0, 0],
    );
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
    match user::run(&Image::Code(EXIT0, DEFAULT), &["tls_first"]) {
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

/// A `PT_TLS` process and a TP=0 process each keep their thread pointer
/// across `sched_yield` (F022).
pub(crate) fn test_el0_tls_yield() -> Outcome {
    let p = match user::spawn(&Image::TlsCode(TLS_LOOP, DEFAULT), &["tls_y_p"]) {
        Ok(p) => p,
        Err(e) => return crate::fail_fmt!("spawn p: {}", e.as_str()),
    };
    let r = match user::spawn(&Image::Code(TLS_ZERO, DEFAULT), &["tls_y_r"]) {
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

/// Two processes that each `msr tpidr_el0` keep their own value (F022).
pub(crate) fn test_el0_tls_tpidr() -> Outcome {
    let a = match user::spawn(&Image::Code(TLS_MSR_A, DEFAULT), &["tls_a"]) {
        Ok(p) => p,
        Err(e) => return crate::fail_fmt!("spawn a: {}", e.as_str()),
    };
    let b = match user::spawn(&Image::Code(TLS_MSR_B, DEFAULT), &["tls_b"]) {
        Ok(p) => p,
        Err(e) => {
            let _ = user::wait(a);
            return crate::fail_fmt!("spawn b: {}", e.as_str());
        }
    };
    let (sa, sb) = (user::wait(a), user::wait(b));
    if exited0(sa) && exited0(sb) {
        Outcome::Ok
    } else {
        crate::fail_fmt!("status {sa:#x} {sb:#x}")
    }
}

static FP_PIDS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static FP_SPAWNED: AtomicBool = AtomicBool::new(false);

fn fp_spawner() {
    let spawn = |code, name| match user::spawn(&Image::Code(code, DEFAULT), &[name]) {
        Ok(pid) => u64::from(pid),
        Err(_) => u64::MAX,
    };
    // Relaxed: pairs with nothing.
    FP_PIDS[0].store(spawn(FP_YIELD, "fp_p"), Ordering::Relaxed);
    thread_init::sleep_ms(20);
    // Relaxed: pairs with nothing.
    FP_PIDS[1].store(spawn(FP_PROBE, "fp_q"), Ordering::Relaxed);
    // Release: pairs with the Acquire load in `test_el0_fp_no_leak`.
    FP_SPAWNED.store(true, Ordering::Release);
}

/// Q's first instruction does not hold P's V-register pattern (F069).
pub(crate) fn test_el0_fp_no_leak() -> Outcome {
    // Release: pairs with the Acquire load below.
    FP_SPAWNED.store(false, Ordering::Release);
    spawn_thread_on("fp_spawner", fp_spawner, thread_init::current_cpu());
    // Acquire: pairs with the Release store in `fp_spawner`.
    if !crate::ktest::sleep_until(|| FP_SPAWNED.load(Ordering::Acquire), 5_000) {
        return Outcome::Fail("spawner");
    }
    // Relaxed: pairs with nothing.
    let pids = [0, 1].map(|i| u32::try_from(FP_PIDS[i].load(Ordering::Relaxed)).ok());
    let sts = pids.map(|p| p.map(user::wait));
    match sts {
        [Some(p), Some(q)] if exited0(p) && exited0(q) => Outcome::Ok,
        [Some(p), Some(q)] => crate::fail_fmt!("P {p:#x} Q {q:#x}"),
        _ => Outcome::Fail("spawn"),
    }
}

/// An SVE instruction at EL0 is `SIGILL` (F130).
pub(crate) fn test_el0_sve_sigill() -> Outcome {
    match user::run(&Image::Code(SVE_PTRUE, DEFAULT), &["sve"]) {
        Ok(st) if vibeos::proc::wifsignaled(st) && vibeos::proc::wtermsig(st) == SIGILL => {
            Outcome::Ok
        }
        Ok(st) => crate::fail_fmt!("status {st:#x}"),
        Err(e) => crate::fail_fmt!("spawn: {}", e.as_str()),
    }
}

const _: () = assert!(TLS_MAGIC == 0x1122_3344_5566_7788);
