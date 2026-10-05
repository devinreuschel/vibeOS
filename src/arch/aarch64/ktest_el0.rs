//! EL0 TLS, FP, and SVE proofs (ROADMAP §11.6, F022, F069, F130).

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

use vibeos::fs::{O_CREAT, O_TRUNC, O_WRONLY};
use vibeos::limits::PID_MAX;
use vibeos::proc::{SIGILL, SIGKILL, wait_signaled};
use vibeos::thread::{Tcb, ThreadId};

use crate::arch;
use crate::arch::current::syscall_nr;
use crate::ktest::user::{self, DEFAULT, Image, TLS_MAGIC, elf_bytes, user_code};
use crate::ktest::{Outcome, fid, second_cpu, sleep_until, spawn_thread_on};
use crate::per_cpu_init;
use crate::proc_init;
use crate::thread_init;
use crate::time_init;

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

/// An SVE instruction at EL0 is `SIGILL` (F130): `CPACR_EL1.ZEN` is clear.
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

/// ROADMAP §10.3 (F039): `current` is read in one instruction that
/// preemption cannot split. The registry, with IF=1, reads its id and pid
/// through `thread_init`, which must not trip the IF=0 assertion of
/// `per_cpu_init::current`, and finds them equal to its TCB's; in a debug
/// build `per_cpu_init::current()` itself trips it.
pub(crate) fn current_at_if1() -> Outcome {
    if !crate::arch::current::interrupts_enabled() {
        return Outcome::Fail("registry runs with IF off");
    }
    if !per_cpu_init::if_checks_armed() {
        return Outcome::Fail("IF=0 checks not armed");
    }
    let mut got = (ThreadId::NONE, 0u32);
    let hit = arch::catch::catch_panic(|| {
        got = (thread_init::current_id(), thread_init::current_pid());
    });
    if hit {
        return Outcome::Fail("two-step current read tripped the IF=0 assertion");
    }
    let t = arch::current_tcb();
    if t.is_null() {
        return Outcome::Fail("current_tcb null");
    }
    // SAFETY: invariant I9: the current thread's `Tcb` stays in `SCHED`,
    // `id` changes only while its slot is Dead, and `pid` only under SCHED
    // by `set_pid_cr3`, a word this read cannot tear; established by
    // `thread_init::spawn_inner`.
    let want = unsafe { ((*t).id, (*t).pid) };
    if got != want {
        return crate::fail_fmt!(
            "current_id/current_pid ({}, {}), TCB ({}, {})",
            got.0.0,
            got.1,
            want.0.0,
            want.1
        );
    }
    if cfg!(debug_assertions) {
        let hit = arch::catch::catch_panic(|| {
            core::hint::black_box(per_cpu_init::current());
        });
        if !hit {
            return Outcome::Fail("per_cpu_init::current() at IF=1 did not assert");
        }
        if !crate::arch::current::interrupts_enabled() || per_cpu_init::irq_nest() != 0 {
            return Outcome::Fail("the caught assertion left IF or irq_nest changed");
        }
    }
    Outcome::Ok
}

/// `current_migrate_if1`'s threads.
const MIGRATE_THREADS: usize = 4;
/// How long each thread reads with IF=1.
const MIGRATE_MS: u64 = 2_000;
static MIGRATE_START: AtomicBool = AtomicBool::new(false);
static MIGRATE_EXIT: AtomicBool = AtomicBool::new(false);
static MIGRATE_TCB: [AtomicPtr<Tcb>; MIGRATE_THREADS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MIGRATE_THREADS];
static MIGRATE_PID: [AtomicU32; MIGRATE_THREADS] = [const { AtomicU32::new(0) }; MIGRATE_THREADS];
/// Each thread's verdict: one of the `MIG_*` values.
static MIGRATE_RESULT: [AtomicU32; MIGRATE_THREADS] =
    [const { AtomicU32::new(MIG_RUNNING) }; MIGRATE_THREADS];
const MIG_RUNNING: u32 = 0;
const MIG_OK: u32 = 1;
const MIG_WRONG_TCB: u32 = 2;
const MIG_WRONG_PID: u32 = 3;
const MIG_NEVER_MOVED: u32 = 4;

fn migrate_body(i: usize) {
    // Acquire: pairs with the Release store of MIGRATE_START in `current_migrate_if1`.
    while !MIGRATE_START.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let (Some(tcb), Some(pid), Some(out)) = (
        MIGRATE_TCB.get(i),
        MIGRATE_PID.get(i),
        MIGRATE_RESULT.get(i),
    ) else {
        return;
    };
    // Acquire: pairs with the Release stores of MIGRATE_TCB and MIGRATE_PID
    // in `current_migrate_if1`.
    let (tcb, pid) = (tcb.load(Ordering::Acquire), pid.load(Ordering::Acquire));
    let first = arch::cpu_id_hint();
    let mut moved = false;
    let end = time_init::uptime_ms().saturating_add(MIGRATE_MS);
    let mut verdict = MIG_OK;
    while time_init::uptime_ms() < end {
        if arch::current_tcb() != tcb {
            verdict = MIG_WRONG_TCB;
            break;
        }
        if thread_init::current_pid() != pid {
            verdict = MIG_WRONG_PID;
            break;
        }
        moved |= arch::cpu_id_hint() != first;
    }
    if verdict == MIG_OK && !moved {
        verdict = MIG_NEVER_MOVED;
    }
    // Release: pairs with the Acquire load of MIGRATE_RESULT in `current_migrate_if1`.
    out.store(verdict, Ordering::Release);
    // Acquire: pairs with the Release store of MIGRATE_EXIT in `current_migrate_if1`.
    while !MIGRATE_EXIT.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}

fn migrate_0() {
    migrate_body(0);
}

fn migrate_1() {
    migrate_body(1);
}

fn migrate_2() {
    migrate_body(2);
}

fn migrate_3() {
    migrate_body(3);
}

/// ROADMAP §10.3 (F039): four `CpuAffinity::Any` threads, each with its
/// own fake pid above `PID_MAX`, read `arch::current_tcb()` and
/// `current_pid()` with IF=1 for 2 s while C-REQUEUE-HOOK moves each
/// preempted one to the next online CPU; any value not its own fails, and
/// so does a thread that never changed CPU.
pub(crate) fn current_migrate_if1() -> Outcome {
    if second_cpu().is_none() {
        return Outcome::Skip("needs 2 CPUs");
    }
    // Release: pairs with the Acquire load of MIGRATE_START in `migrate_body`.
    MIGRATE_START.store(false, Ordering::Release);
    // Release: pairs with the Acquire load of MIGRATE_EXIT in `migrate_body`.
    MIGRATE_EXIT.store(false, Ordering::Release);
    let entries: [fn(); MIGRATE_THREADS] = [migrate_0, migrate_1, migrate_2, migrate_3];
    let mut ids = [ThreadId::NONE; MIGRATE_THREADS];
    for (i, entry) in entries.into_iter().enumerate() {
        let h = match thread_init::spawn("current-migrate", entry) {
            Ok(h) => h,
            Err(e) => {
                // Release: pairs with the Acquire load of MIGRATE_EXIT in `migrate_body`.
                MIGRATE_EXIT.store(true, Ordering::Release);
                // Release: pairs with the Acquire load of MIGRATE_START in `migrate_body`.
                MIGRATE_START.store(true, Ordering::Release);
                return crate::fail_fmt!("spawn: {}", e.as_str());
            }
        };
        let id = h.id();
        ids[i] = id;
        let pid = PID_MAX.saturating_add(1).saturating_add(i as u32);
        thread_init::set_pid_cr3(id, pid, 0);
        // Release: pairs with the Acquire load of MIGRATE_TCB in `migrate_body`.
        MIGRATE_TCB[i].store(thread_init::tcb_ptr(id), Ordering::Release);
        // Release: pairs with the Acquire load of MIGRATE_PID in `migrate_body`.
        MIGRATE_PID[i].store(pid, Ordering::Release);
        // Release: pairs with the Acquire load of MIGRATE_RESULT below.
        MIGRATE_RESULT[i].store(MIG_RUNNING, Ordering::Release);
    }
    let done = {
        let _g = thread_init::testing::RequeueGuard;
        thread_init::testing::set_requeue_next_cpu(true);
        // Release: pairs with the Acquire load of MIGRATE_START in `migrate_body`.
        MIGRATE_START.store(true, Ordering::Release);
        sleep_until(
            || {
                // Acquire: pairs with the Release store of MIGRATE_RESULT in `migrate_body`.
                MIGRATE_RESULT
                    .iter()
                    .all(|r| r.load(Ordering::Acquire) != MIG_RUNNING)
            },
            MIGRATE_MS.saturating_mul(3),
        )
    };
    for id in ids {
        thread_init::set_pid_cr3(id, 0, 0);
    }
    // Release: pairs with the Acquire load of MIGRATE_EXIT in `migrate_body`.
    MIGRATE_EXIT.store(true, Ordering::Release);
    let joined = sleep_until(|| ids.iter().all(|&id| thread_init::exited(id)), 2_000);
    if !done {
        return Outcome::Fail("a thread did not finish its 2 s");
    }
    for (i, r) in MIGRATE_RESULT.iter().enumerate() {
        // Acquire: pairs with the Release store of MIGRATE_RESULT in `migrate_body`.
        match r.load(Ordering::Acquire) {
            MIG_OK => {}
            MIG_WRONG_TCB => return crate::fail_fmt!("thread {i}: current_tcb not its own"),
            MIG_WRONG_PID => return crate::fail_fmt!("thread {i}: current_pid not its own"),
            MIG_NEVER_MOVED => return crate::fail_fmt!("thread {i}: never changed CPU"),
            v => return crate::fail_fmt!("thread {i}: verdict {v}"),
        }
    }
    if !joined {
        return Outcome::Fail("a thread did not exit");
    }
    Outcome::Ok
}

static PAN_OK: AtomicU32 = AtomicU32::new(0);
static PAN_DONE: AtomicBool = AtomicBool::new(false);

fn pan_on_cpu() {
    let ok = u32::from(crate::arch::aarch64::cpu::pan_is_set());
    // Relaxed: pairs with nothing.
    PAN_OK.store(ok, Ordering::Relaxed);
    // Release: pairs with the Acquire load in `test_el0_pan_every_cpu`.
    PAN_DONE.store(true, Ordering::Release);
}

/// PSTATE.PAN is set on every online CPU (ROADMAP §11.6).
pub(crate) fn test_el0_pan_every_cpu() -> Outcome {
    let n = per_cpu_init::cpu_count().min(64);
    if n == 0 {
        return Outcome::Fail("no cpus");
    }
    let mut i = 0u32;
    while (i as usize) < n {
        if !per_cpu_init::is_online(i) {
            i += 1;
            continue;
        }
        // Release: pairs with the Acquire load below.
        PAN_DONE.store(false, Ordering::Release);
        spawn_thread_on("pan_cpu", pan_on_cpu, i);
        // Acquire: pairs with the Release store in `pan_on_cpu`.
        if !sleep_until(|| PAN_DONE.load(Ordering::Acquire), 5_000) {
            return crate::fail_fmt!("cpu{i} did not report");
        }
        // Relaxed: pairs with nothing.
        if PAN_OK.load(Ordering::Relaxed) == 0 {
            return crate::fail_fmt!("cpu{i} PAN clear");
        }
        i += 1;
    }
    Outcome::Ok
}

static ENV_ST: AtomicU32 = AtomicU32::new(0);
static ENV_DONE: AtomicBool = AtomicBool::new(false);

fn env_on_cpu() {
    let st =
        user::run(&Image::UserBin("tests"), &["tests", "--case", "user_env"]).unwrap_or(u32::MAX);
    // Relaxed: pairs with nothing.
    ENV_ST.store(st, Ordering::Relaxed);
    // Release: pairs with the Acquire load in `test_el0_env_every_cpu`.
    ENV_DONE.store(true, Ordering::Release);
}

/// The `/bin/tests` `user_env` case on every online CPU (issue #207).
pub(crate) fn test_el0_env_every_cpu() -> Outcome {
    let n = per_cpu_init::cpu_count().min(64);
    if n == 0 {
        return Outcome::Fail("no cpus");
    }
    let mut i = 0u32;
    while (i as usize) < n {
        if !per_cpu_init::is_online(i) {
            i += 1;
            continue;
        }
        // Release: pairs with the Acquire load below.
        ENV_DONE.store(false, Ordering::Release);
        spawn_thread_on("env_cpu", env_on_cpu, i);
        // Acquire: pairs with the Release store in `env_on_cpu`.
        if !sleep_until(|| ENV_DONE.load(Ordering::Acquire), 15_000) {
            return crate::fail_fmt!("cpu{i} did not finish");
        }
        // Relaxed: pairs with nothing.
        let st = ENV_ST.load(Ordering::Relaxed);
        if !exited0(st) {
            return crate::fail_fmt!("cpu{i} status {st:#x}");
        }
        i += 1;
    }
    Outcome::Ok
}
