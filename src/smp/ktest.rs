//! In-guest tests for smp (kernel_tests only). Rows: [`TESTS`].

#[cfg(target_arch = "x86_64")]
use vibeos::acpi::MAX_CPUS;
#[cfg(target_arch = "x86_64")]
use vibeos::apic::TimerMode;
#[cfg(target_arch = "x86_64")]
use vibeos::atomic::statics::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
#[cfg(target_arch = "x86_64")]
use vibeos::limits::PID_MAX;
#[cfg(target_arch = "x86_64")]
use vibeos::thread::Tcb;
use vibeos::thread::ThreadId;

#[cfg(target_arch = "x86_64")]
use crate::apic_init;
use crate::arch;
#[cfg(target_arch = "x86_64")]
use crate::ipi_init;
#[cfg(target_arch = "x86_64")]
use crate::ktest::cpu_remote;
use crate::ktest::{FrameCount, Outcome, Test, registry_tid, test};
#[cfg(target_arch = "x86_64")]
use crate::ktest::{quiescent_free_frames, sleep_until, spin_until_ns};
use crate::per_cpu_init;
use crate::sched::ktest::fill_threads;
#[cfg(target_arch = "x86_64")]
use crate::sched::ktest::{RequeueGuard, set_requeue_next_cpu};
use crate::sched_init;
use crate::smp_init;
use crate::thread_init;
#[cfg(target_arch = "x86_64")]
use crate::time_init;

pub(crate) fn test_per_cpu_bsp() -> Outcome {
    if !per_cpu_init::is_live() {
        return Outcome::Fail("per_cpu not live");
    }
    // IF=0 for the per-CPU reads (DESIGN §2.9 rule 5).
    let _g = crate::arch::current::InterruptGuard::enter();
    let cpu = per_cpu_init::current();
    if cpu.cpu_id != 0 {
        return Outcome::Fail("cpu_id not 0");
    }
    let addr = cpu as *const _ as u64;
    if cpu.self_ptr as u64 != addr {
        return Outcome::Fail("self_ptr mismatch");
    }
    if per_cpu_init::gs_self() as u64 != addr {
        return Outcome::Fail("gs:[0] != PerCpu");
    }
    if crate::per_cpu!(cpu_id) != 0 {
        return Outcome::Fail("per_cpu! cpu_id");
    }
    if thread_init::current_id() != registry_tid() {
        return Outcome::Fail("current not the registry");
    }
    if cpu.idle_id == ThreadId::BOOTSTRAP {
        return Outcome::Fail("idle still bootstrap");
    }
    if thread_init::name(cpu.idle_id) != "idle" {
        return Outcome::Fail("idle name");
    }
    let current = arch::current_tcb();
    if core::ptr::eq(cpu.idle, current) {
        return Outcome::Fail("idle == current");
    }
    if current.is_null() || cpu.idle.is_null() {
        return Outcome::Fail("current or idle null");
    }
    if !sched_init::is_live() {
        return Outcome::Fail("sched not live");
    }
    Outcome::Ok
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn test_per_cpu_identity() -> Outcome {
    let n = per_cpu_init::cpu_count();
    if n == 0 {
        return Outcome::Fail("cpu array empty");
    }
    // IF=0 for the per-CPU reads (DESIGN §2.9 rule 5); the guard drops
    // before the IPI below.
    let bsp_apic = {
        let _g = crate::arch::current::InterruptGuard::enter();
        let bsp = per_cpu_init::current();
        if bsp.cpu_id != 0 {
            return Outcome::Fail("not on bsp");
        }
        if bsp.self_ptr as u64 != bsp as *const _ as u64 {
            return Outcome::Fail("bsp self_ptr");
        }
        if per_cpu_init::gs_self() as u64 != bsp.self_ptr as u64 {
            return Outcome::Fail("bsp gs:[0]");
        }
        if crate::per_cpu!(cpu_id) != 0 {
            return Outcome::Fail("per_cpu! on bsp");
        }
        bsp.remote.apic_id.load(Ordering::Relaxed)
    };
    if !per_cpu_init::is_online(0) {
        return Outcome::Fail("bsp offline");
    }
    if n < 2 {
        return Outcome::Skip("no AP");
    }
    let mut aps = 0u64;
    let mut i = 1u32;
    while i < n as u32 {
        let Some(c) = cpu_remote(i) else {
            return Outcome::Fail("missing slot");
        };
        if !c.ready.load(Ordering::Acquire) {
            return Outcome::Fail("ap not ready");
        }
        if c.apic_id.load(Ordering::Relaxed) == bsp_apic {
            return Outcome::Fail("ap apic_id");
        }
        if !per_cpu_init::is_online(i) {
            return Outcome::Fail("ap online mask");
        }
        if i < 64 {
            aps |= 1u64 << i;
        }
        i += 1;
    }
    // The owner-only checks run on each AP, which alone may read its
    // `PerCpu` (DESIGN §7.5).
    IDENTITY_BAD.store(0, Ordering::SeqCst);
    IDENTITY_SEEN.store(0, Ordering::SeqCst);
    ipi_init::call_mask(aps, identity_on_ap, core::ptr::null_mut(), true);
    if IDENTITY_SEEN.load(Ordering::Acquire) != aps {
        return Outcome::Fail("ap did not run the owner check");
    }
    if IDENTITY_BAD.load(Ordering::Acquire) != 0 {
        return Outcome::Fail("ap owner-only state");
    }
    Outcome::Ok
}

/// Bit `cpu_id` of each AP whose owner-only check failed or ran.
#[cfg(target_arch = "x86_64")]
static IDENTITY_BAD: AtomicU64 = AtomicU64::new(0);

#[cfg(target_arch = "x86_64")]
static IDENTITY_SEEN: AtomicU64 = AtomicU64::new(0);

#[cfg(target_arch = "x86_64")]
fn identity_on_ap(_: *mut ()) {
    let c = per_cpu_init::current();
    let id = c.cpu_id;
    if id >= 64 {
        return;
    }
    let ok = core::ptr::eq(c.self_ptr, per_cpu_init::gs_self())
        && per_cpu_init::slot_ptr(id) == Some(c.self_ptr)
        && !c.idle.is_null()
        && !arch::current_tcb().is_null()
        && per_cpu_init::cpu(id).is_some_and(|r| core::ptr::eq(r, c.remote));
    if !ok {
        IDENTITY_BAD.fetch_or(1u64 << id, Ordering::Release);
    }
    IDENTITY_SEEN.fetch_or(1u64 << id, Ordering::Release);
}

#[cfg(target_arch = "aarch64")]
pub(crate) fn test_trampoline_page() -> Outcome {
    Outcome::Skip("x86 AP trampoline")
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn test_trampoline_page() -> Outcome {
    let Some(page) = crate::boot::info().trampoline_page else {
        return Outcome::Fail("boot chose no trampoline page");
    };
    if !(0x1000..0x10_0000).contains(&page) || !page.is_multiple_of(0x1000) {
        return crate::fail_fmt!("trampoline page {page:#x} not a page in [0x1000, 1 MiB)");
    }
    if !trampoline_installed() {
        return crate::fail_fmt!("no cli opcode at trampoline page {page:#x}");
    }
    // INIT leaves CR0.CD|NW. Blob must AND 0x9FFFFFFF then WBINVD.
    let p = smp_init::tramp_va(page).cast_const();
    let mut and_cdnw = false;
    let mut wbinvd = false;
    let mut i = 0usize;
    while i + 1 < 0xD0 {
        // SAFETY: the physmap maps the trampoline page (invariant I14,
        // established at `mm::paging_init::install`), and every offset read
        // here is below `0xD0`, inside it; established here.
        let a = unsafe { p.add(i).read_volatile() };
        // SAFETY: as above, `i + 1 < 0xD0`; established here.
        let b = unsafe { p.add(i + 1).read_volatile() };
        if a == 0x0F && b == 0x09 {
            wbinvd = true;
        }
        if i + 4 < 0xD0
            && a == 0x25
            && b == 0xFF
            // SAFETY: as above, `i + 4 < 0xD0`; established here.
            && unsafe { p.add(i + 2).read_volatile() } == 0xFF
            // SAFETY: as above, `i + 4 < 0xD0`; established here.
            && unsafe { p.add(i + 3).read_volatile() } == 0xFF
            // SAFETY: as above, `i + 4 < 0xD0`; established here.
            && unsafe { p.add(i + 4).read_volatile() } == 0x9F
        {
            and_cdnw = true;
        }
        i += 1;
    }
    if !and_cdnw {
        return Outcome::Fail("trampoline missing CR0.CD/NW clear");
    }
    if !wbinvd {
        return Outcome::Fail("trampoline missing wbinvd");
    }
    // The GDT pointer's base was rebased onto the page (`patch_blob`).
    let gdt_site = vibeos::smp::PATCH_SITES[3];
    let mut base = [0u8; 4];
    for (k, b) in base.iter_mut().enumerate() {
        // SAFETY: as above, `gdt_site + 3 < 0xD0`; established here.
        *b = unsafe { p.add(gdt_site + k).read_volatile() };
    }
    let base = u64::from(u32::from_le_bytes(base));
    if base.wrapping_sub(page) >= 0x1000 {
        return crate::fail_fmt!("gdt base {base:#x} not inside page {page:#x}");
    }
    // The page is read-only, so the AP must never set an accessed bit: the
    // four descriptors after the null one carry it preset (0x9B, 0x93).
    let gdt = base - page;
    for d in 1..5u64 {
        let at = gdt + d * 8 + 5;
        // SAFETY: as above, `at` is below `0xD0`, inside the page;
        // established here.
        let access = unsafe { p.add(at as usize).read_volatile() };
        if access & 1 == 0 {
            return crate::fail_fmt!(
                "gdt descriptor {d} access {access:#x} lacks the accessed bit"
            );
        }
    }
    // After the identity teardown the page is the window's one leaf:
    // 4 KiB, present, read-only, executable, not global.
    let Some((pa, size, flags)) = crate::paging_init::translate(vibeos::paging::VirtAddr(page))
    else {
        return crate::fail_fmt!("trampoline page {page:#x} not identity mapped");
    };
    use vibeos::paging::{PageFlags, PageSize};
    let bad = PageFlags::WRITABLE | PageFlags::NX | PageFlags::GLOBAL;
    if pa.as_u64() != page || size != PageSize::Size4K || flags.0 & bad != 0 {
        return crate::fail_fmt!(
            "trampoline pte pa {:#x} flags {:#x}, want {page:#x} 4 KiB, not writable, NX or global",
            pa.as_u64(),
            flags.0
        );
    }
    Outcome::Ok
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn test_failed_ap_cleanup() -> Outcome {
    // First-fit KVA may map a fresh PT page on the first IST/stack wave.
    // unmap_4k does not return that PT. Warm up, then the measured wave
    // must restore the frame count (ROADMAP failed-AP exit gate).
    if !exercise_fail_cleanup() {
        return Outcome::Fail("warm-up bring-up allocation failed");
    }
    let n0 = quiescent_free_frames();
    if !exercise_fail_cleanup() {
        return Outcome::Fail("bring-up allocation failed");
    }
    let n1 = quiescent_free_frames();
    if n0 != n1 {
        crate::marker!("vibeOS: ktest:   frames {n0} -> {n1}");
        Outcome::Fail("failed AP leaked frames")
    } else {
        Outcome::Ok
    }
}

/// The stalled AP parked after losing the handshake, and every online CPU
/// has the workers bring-up started for it.
pub(crate) fn late_ap_agrees() -> Outcome {
    if !smp_init::stalled_ap_parked() {
        return Outcome::Fail("stalled AP did not park");
    }
    let online = per_cpu_init::online_mask();
    let workers = crate::work_init::started_mask();
    if online != workers {
        return crate::fail_fmt!("online {online:#x} workers {workers:#x}");
    }
    Outcome::Ok
}

/// Opt-in: `vibeos.ktest=stalled_ap_leak` holds the first AP past the ready
/// timeout, then releases it. It must park, stay offline, and leave the
/// online mask equal to the worker set (ROADMAP §11.4, F032).
#[cfg(target_arch = "x86_64")]
pub(crate) fn test_stalled_ap_leak() -> Outcome {
    if !smp_init::stalled_ap_leaked() {
        return Outcome::Fail("bring-up did not leak a stalled AP");
    }
    let n = per_cpu_init::cpu_count();
    if n < 3 {
        return Outcome::Fail("needs 3 CPUs");
    }
    if per_cpu_init::is_online(1) {
        return Outcome::Fail("stalled AP came online");
    }
    let last = n as u32 - 1;
    if !per_cpu_init::is_online(last) {
        return Outcome::Fail("next AP did not come up");
    }
    let mut online = 0u32;
    let mut i = 0u32;
    while i < n as u32 {
        if per_cpu_init::is_online(i) {
            online += 1;
        }
        i += 1;
    }
    if online + 1 != n as u32 {
        return crate::fail_fmt!("online {online} of {n}, want one hole");
    }
    let n0 = quiescent_free_frames();
    if !exercise_fail_cleanup() {
        return Outcome::Fail("pre-SIPI free alloc failed");
    }
    let n1 = quiescent_free_frames();
    if n0 != n1 {
        return crate::fail_fmt!("pre-SIPI free leaked {n0} -> {n1}");
    }
    let agreed = late_ap_agrees();
    if !matches!(agreed, Outcome::Ok) {
        return agreed;
    }
    crate::ktest_info!("stalled AP leaked and parked; online {online}/{n}");
    Outcome::Ok
}

/// AP bring-up on a full thread table (ROADMAP §10.4, F037): with no slot
/// for the idle thread, and then with one slot, which the idle thread takes
/// while its worker finds none, the allocation fails, frees what it took
/// (the idle thread's slot included, free again), and nothing panics.
pub(crate) fn ap_bringup_full_thread_table() -> Outcome {
    // Warm up as `failed_ap_cleanup` does, and fill the table once, so the
    // baseline holds every TCB box and heap page a full table takes (a Dead
    // slot keeps its box for reuse).
    if !exercise_fail_cleanup() {
        return Outcome::Fail("warm-up bring-up allocation failed");
    }
    if !fill_threads(0).release() {
        return Outcome::Fail("warm-up fillers did not exit");
    }
    let frames0 = FrameCount::quiescent();
    let fill = fill_threads(0);
    if fill.last != Some(thread_init::SpawnError::NoSlot) {
        let spawned = fill.spawned;
        if !fill.release() {
            return Outcome::Fail("fillers did not exit");
        }
        return crate::fail_fmt!("fill stopped without NoSlot after {spawned} threads");
    }
    let ok = !exercise_fail_cleanup();
    if !fill.release() {
        return Outcome::Fail("fillers did not exit");
    }
    if !ok {
        return Outcome::Fail("bring-up allocated on a full table");
    }
    let fill = fill_threads(1);
    let (used0, cap) = thread_init::table_usage();
    let ok = !exercise_fail_cleanup();
    let (used1, _) = thread_init::table_usage();
    if !fill.release() {
        return Outcome::Fail("fillers did not exit");
    }
    if used0 + 1 != cap {
        return crate::fail_fmt!("fill_threads(1) left {used0} of {cap} used");
    }
    if !ok {
        return Outcome::Fail("bring-up allocated with one free slot");
    }
    if used1 != used0 {
        return crate::fail_fmt!("failed bring-up left {used1} used, was {used0}");
    }
    frames0.unchanged(&FrameCount::quiescent())
}

/// How long [`percpu_remote_view`] waits for this CPU's `ticks` to move.
#[cfg(target_arch = "x86_64")]
const TICK_WAIT_NS: u64 = 200_000_000;

/// `per_cpu_init::cpu` hands out each CPU's `PerCpuRemote`, and the owner
/// keeps its fields current: `ready` and distinct `apic_id`s on every
/// online CPU, `runq_len` after a `with_current` scope, and `ticks` with
/// IF on.
#[cfg(target_arch = "x86_64")]
pub(crate) fn percpu_remote_view() -> Outcome {
    let n = per_cpu_init::cpu_count();
    if n == 0 {
        return Outcome::Fail("cpu array empty");
    }
    let mut i = 0u32;
    while (i as usize) < n {
        if cpu_remote(i).is_none() {
            return Outcome::Fail("cpu(i) is None below cpu_count");
        }
        i += 1;
    }
    if cpu_remote(n as u32).is_some() {
        return Outcome::Fail("cpu(cpu_count) is Some");
    }

    let mut a = 0u32;
    while (a as usize) < n {
        if per_cpu_init::is_online(a) {
            let Some(ra) = cpu_remote(a) else {
                return Outcome::Fail("online cpu has no view");
            };
            if !ra.ready.load(Ordering::Acquire) {
                return Outcome::Fail("online cpu not ready");
            }
            let mut b = a + 1;
            while (b as usize) < n {
                if per_cpu_init::is_online(b)
                    && cpu_remote(b).is_some_and(|rb| {
                        rb.apic_id.load(Ordering::Relaxed) == ra.apic_id.load(Ordering::Relaxed)
                    })
                {
                    return Outcome::Fail("two online cpus share an apic_id");
                }
                b += 1;
            }
        }
        a += 1;
    }

    {
        let _g = crate::arch::current::InterruptGuard::enter();
        let me = per_cpu_init::current();
        if !cpu_remote(me.cpu_id).is_some_and(|r| core::ptr::eq(r, me.remote)) {
            return Outcome::Fail("cpu(me) is not PerCpu.remote");
        }
        let len = per_cpu_init::with_current(|pc| pc.runq.len());
        if me.remote.runq_len.load(Ordering::Relaxed) != len {
            return Outcome::Fail("runq_len != runq.len()");
        }
    }

    if !crate::arch::current::interrupts_enabled() {
        return Outcome::Fail("registry runs with IF off");
    }
    // The registry thread is pinned to CPU 0, so the hint stays this CPU.
    let Some(me) = per_cpu_init::cpu(thread_init::current_cpu()) else {
        return Outcome::Fail("no remote view");
    };
    let t0 = me.ticks.load(Ordering::Relaxed);
    let moved = spin_until_ns(|| me.ticks.load(Ordering::Relaxed) != t0, TICK_WAIT_NS);
    if !moved {
        return Outcome::Fail("ticks did not advance with IF on");
    }
    Outcome::Ok
}

/// ROADMAP §10.3 (F039): `current` is read in one instruction that
/// preemption cannot split. The registry, with IF=1, reads its id and pid
/// through `thread_init`, which must not trip the IF=0 assertion of
/// `per_cpu_init::current`, and finds them equal to its TCB's; in a debug
/// build `per_cpu_init::current()` itself trips it.
#[cfg(target_arch = "x86_64")]
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
#[cfg(target_arch = "x86_64")]
const MIGRATE_THREADS: usize = 4;
/// How long each thread reads with IF=1.
#[cfg(target_arch = "x86_64")]
const MIGRATE_MS: u64 = 2_000;
#[cfg(target_arch = "x86_64")]
static MIGRATE_START: AtomicBool = AtomicBool::new(false);
#[cfg(target_arch = "x86_64")]
static MIGRATE_EXIT: AtomicBool = AtomicBool::new(false);
#[cfg(target_arch = "x86_64")]
static MIGRATE_TCB: [AtomicPtr<Tcb>; MIGRATE_THREADS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MIGRATE_THREADS];
#[cfg(target_arch = "x86_64")]
static MIGRATE_PID: [AtomicU32; MIGRATE_THREADS] = [const { AtomicU32::new(0) }; MIGRATE_THREADS];
/// Each thread's verdict: one of the `MIG_*` values.
#[cfg(target_arch = "x86_64")]
static MIGRATE_RESULT: [AtomicU32; MIGRATE_THREADS] =
    [const { AtomicU32::new(MIG_RUNNING) }; MIGRATE_THREADS];
#[cfg(target_arch = "x86_64")]
const MIG_RUNNING: u32 = 0;
#[cfg(target_arch = "x86_64")]
const MIG_OK: u32 = 1;
#[cfg(target_arch = "x86_64")]
const MIG_WRONG_TCB: u32 = 2;
#[cfg(target_arch = "x86_64")]
const MIG_WRONG_PID: u32 = 3;
#[cfg(target_arch = "x86_64")]
const MIG_NEVER_MOVED: u32 = 4;

#[cfg(target_arch = "x86_64")]
fn migrate_body(i: usize) {
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
    // Release: publishes the verdict to `current_migrate_if1`.
    out.store(verdict, Ordering::Release);
    while !MIGRATE_EXIT.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}

#[cfg(target_arch = "x86_64")]
fn migrate_0() {
    migrate_body(0);
}

#[cfg(target_arch = "x86_64")]
fn migrate_1() {
    migrate_body(1);
}

#[cfg(target_arch = "x86_64")]
fn migrate_2() {
    migrate_body(2);
}

#[cfg(target_arch = "x86_64")]
fn migrate_3() {
    migrate_body(3);
}

/// ROADMAP §10.3 (F039): four `CpuAffinity::Any` threads, each with its
/// own fake pid above `PID_MAX`, read `arch::current_tcb()` and
/// `current_pid()` with IF=1 for 2 s while C-REQUEUE-HOOK moves each
/// preempted one to the next online CPU; any value not its own fails, and
/// so does a thread that never changed CPU.
#[cfg(target_arch = "x86_64")]
pub(crate) fn current_migrate_if1() -> Outcome {
    if crate::ktest::second_cpu().is_none() {
        return Outcome::Skip("needs 2 CPUs");
    }
    MIGRATE_START.store(false, Ordering::Release);
    MIGRATE_EXIT.store(false, Ordering::Release);
    let entries: [fn(); MIGRATE_THREADS] = [migrate_0, migrate_1, migrate_2, migrate_3];
    let mut ids = [ThreadId::NONE; MIGRATE_THREADS];
    for (i, entry) in entries.into_iter().enumerate() {
        let h = match thread_init::spawn("current-migrate", entry) {
            Ok(h) => h,
            Err(e) => {
                MIGRATE_EXIT.store(true, Ordering::Release);
                MIGRATE_START.store(true, Ordering::Release);
                return crate::fail_fmt!("spawn: {}", e.as_str());
            }
        };
        let id = h.id();
        ids[i] = id;
        let pid = PID_MAX.saturating_add(1).saturating_add(i as u32);
        thread_init::set_pid_cr3(id, pid, 0);
        MIGRATE_TCB[i].store(thread_init::tcb_ptr(id), Ordering::Release);
        MIGRATE_PID[i].store(pid, Ordering::Release);
        MIGRATE_RESULT[i].store(MIG_RUNNING, Ordering::Release);
    }
    let done = {
        let _g = RequeueGuard;
        set_requeue_next_cpu(true);
        MIGRATE_START.store(true, Ordering::Release);
        sleep_until(
            || {
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
    MIGRATE_EXIT.store(true, Ordering::Release);
    let joined = sleep_until(|| ids.iter().all(|&id| thread_init::exited(id)), 2_000);
    if !done {
        return Outcome::Fail("a thread did not finish its 2 s");
    }
    for (i, r) in MIGRATE_RESULT.iter().enumerate() {
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

/// Ticks each online CPU must gain, and the `now_ns` bound on the wait.
#[cfg(target_arch = "x86_64")]
const PERCPU_TICKS_WANT: u64 = 10;
#[cfg(target_arch = "x86_64")]
const PERCPU_TICKS_WAIT_NS: u64 = 2_000_000_000;

/// One CPU's tick count through its remote view (C-PERCPU).
#[cfg(target_arch = "x86_64")]
fn remote_ticks(id: u32) -> Option<u64> {
    // Relaxed: a counter read that pairs with no other access; only its
    // growth is compared.
    cpu_remote(id).map(|r| r.ticks.load(Ordering::Relaxed))
}

/// Box L1198 (F078): every online CPU's `ticks` advances, so each CPU's
/// timer arm runs (`arm_tsc_deadline`, `rearm_deadline` and `arm_ap`'s
/// `TscDeadline` arm under TSC-deadline; the periodic arm otherwise). In
/// PIT mode `apic_init::arm_ap` arms nothing and APs never tick.
#[cfg(target_arch = "x86_64")]
pub(crate) fn percpu_ticks_advance() -> Outcome {
    let mode = apic_init::timer_mode();
    if mode == TimerMode::Pit {
        return Outcome::Skip("pit owns tick");
    }
    if !crate::arch::current::interrupts_enabled() {
        return Outcome::Fail("registry runs with IF off");
    }
    let n = per_cpu_init::cpu_count().min(MAX_CPUS) as u32;
    let mut start = [0u64; MAX_CPUS];
    for id in 0..n {
        if !per_cpu_init::is_online(id) {
            continue;
        }
        let (Some(slot), Some(t)) = (start.get_mut(id as usize), remote_ticks(id)) else {
            return Outcome::Fail("online cpu has no view");
        };
        *slot = t;
    }
    let gained = |id: u32| -> u64 {
        let t0 = start.get(id as usize).copied().unwrap_or(0);
        remote_ticks(id).unwrap_or(t0).wrapping_sub(t0)
    };
    let behind =
        || (0..n).find(|&id| per_cpu_init::is_online(id) && gained(id) < PERCPU_TICKS_WANT);
    // IF stays on through the wait, so this CPU's own tick is taken too.
    spin_until_ns(|| behind().is_none(), PERCPU_TICKS_WAIT_NS);
    match behind() {
        None => Outcome::Ok,
        Some(id) => {
            let d = gained(id);
            crate::fail_fmt!("cpu {id} ticks +{d} in 2 s ({})", mode.as_str())
        }
    }
}

// ------------------ hooks ------------------

/// Allocate the same resources as bring-up, then take the timeout free
/// path. Frame count must return to baseline (injectable fault).
/// Allocate and free a bring-up's resources for CPU `0xFE`, which never
/// starts. True when the allocation succeeded.
pub fn exercise_fail_cleanup() -> bool {
    match smp_init::alloc_ap_resources(0xFE, 0xFE, false) {
        Ok(a) => {
            smp_init::free_ap_resources(a, false);
            true
        }
        Err(_) => false,
    }
}

#[cfg(target_arch = "x86_64")]
pub fn trampoline_installed() -> bool {
    let Some(page) = crate::boot::info().trampoline_page else {
        return false;
    };
    // SAFETY: the physmap maps the trampoline page (invariant I14,
    // established at `mm::paging_init::install`), and `smp_init::init`
    // wrote it before any test runs; established here.
    unsafe { smp_init::tramp_va(page).read_volatile() == 0xFA }
}

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[
    test("per_cpu_bsp", test_per_cpu_bsp),
    #[cfg(target_arch = "x86_64")]
    test("per_cpu_identity", test_per_cpu_identity),
    test("trampoline_page", test_trampoline_page),
    #[cfg(target_arch = "x86_64")]
    test("failed_ap_cleanup", test_failed_ap_cleanup),
    test("ap_bringup_full_thread_table", ap_bringup_full_thread_table).deadline(60_000),
    #[cfg(target_arch = "x86_64")]
    test("percpu_remote_view", percpu_remote_view),
    #[cfg(target_arch = "x86_64")]
    test("percpu_ticks_advance", percpu_ticks_advance),
    #[cfg(target_arch = "x86_64")]
    test("current_at_if1", current_at_if1),
    #[cfg(target_arch = "x86_64")]
    test("current_migrate_if1", current_migrate_if1),
    #[cfg(target_arch = "x86_64")]
    test("stalled_ap_leak", test_stalled_ap_leak).opt_in(),
];
