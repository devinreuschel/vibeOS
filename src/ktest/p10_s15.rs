//! In-guest tests of P10-S15, Generated IDT entry stubs (DESIGN §8.2).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::addr_space::UserPerms;
use vibeos::paging::PAGE_SIZE_4K;
use vibeos::proc::{SIGILL, SIGKILL, wait_signaled};
use vibeos::syscall::SYS_KILL;
use vibeos::vectors;

use super::user::{self, DEFAULT, Image, user_code};
use super::{Outcome, Test, test};
use crate::addr_space_init;
use crate::arch::idt::TrapFrame;
use crate::arch::idt::testing;
use crate::ipi_init;
use crate::per_cpu_init;
use crate::proc_init;
use crate::thread_init;
use crate::time_init;
use crate::x86;

pub(super) const TESTS: &[Test] = &[
    test("ac_clear_on_exception", test_ac_clear_on_exception).deadline(30_000),
    test("ac_clear_user_popf", test_ac_clear_user_popf).deadline(30_000),
    test("ist_gs_sign", test_ist_gs_sign).deadline(30_000),
];

const RFLAGS_AC: u64 = 1 << 18;
/// Page-fault error code bit 0: the page was present (a protection fault).
const PF_PRESENT: u64 = 1;
/// The user page both AC tests read: `user::DEFAULT`'s load address.
const USER_VA: u64 = 0x4000_0000;

/// What an AC hook saw: the frame's AC, then its stray read's fault.
struct AcProbe {
    ran: AtomicBool,
    frame_ac: AtomicBool,
    faulted: AtomicBool,
    cr2: AtomicU64,
    error: AtomicU64,
}

impl AcProbe {
    const fn new() -> Self {
        Self {
            ran: AtomicBool::new(false),
            frame_ac: AtomicBool::new(false),
            faulted: AtomicBool::new(false),
            cr2: AtomicU64::new(0),
            error: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        self.frame_ac.store(false, Ordering::Relaxed);
        self.faulted.store(false, Ordering::Relaxed);
        self.cr2.store(0, Ordering::Relaxed);
        self.error.store(0, Ordering::Relaxed);
        self.ran.store(false, Ordering::Release);
    }

    /// Record the frame's AC, then read `USER_VA` from the body's context,
    /// where the stub has cleared AC, so SMAP must fault. Publishes `ran`
    /// last.
    fn probe(&self, frame: &TrapFrame) {
        self.frame_ac
            .store(frame.iret.rflags & RFLAGS_AC != 0, Ordering::Relaxed);
        // SAFETY: invariant: `USER_VA` is a present user page in the loaded
        // address space, so the read is a SMAP `#PF` that `catch_fault`
        // catches, or a plain read if AC leaked; established by the test
        // that installs the hook (`test_ac_clear_on_exception` loads its
        // space; in `test_ac_clear_user_popf` it is the child's code page).
        let hit = super::catch_fault(|| unsafe {
            core::ptr::read_volatile(USER_VA as *const u8);
        });
        if let Some(f) = hit {
            self.faulted.store(true, Ordering::Relaxed);
            self.cr2.store(f.cr2, Ordering::Relaxed);
            self.error.store(f.error, Ordering::Relaxed);
        }
        self.ran.store(true, Ordering::Release);
    }

    /// `None` when the hook saw AC set in the frame and its read took a
    /// SMAP protection fault at `USER_VA`.
    fn verdict(&self) -> Option<Outcome> {
        if !self.ran.load(Ordering::Acquire) {
            return Some(Outcome::Fail("hook did not run"));
        }
        if !self.frame_ac.load(Ordering::Relaxed) {
            return Some(Outcome::Fail("frame AC clear"));
        }
        if !self.faulted.load(Ordering::Relaxed) {
            return Some(Outcome::Fail("stray user read did not fault: AC still set"));
        }
        let (cr2, error) = (
            self.cr2.load(Ordering::Relaxed),
            self.error.load(Ordering::Relaxed),
        );
        if cr2 != USER_VA || error & PF_PRESENT == 0 {
            return Some(crate::fail_fmt!(
                "fault cr2={cr2:#x} err={error:#x}, want {USER_VA:#x} present"
            ));
        }
        None
    }
}

/// Free frames once the stacks of threads that earlier tests left dead are
/// freed, since the next thread exit would free them inside this test.
fn settled_frames() -> usize {
    thread_init::reap_zombies();
    super::free_frames()
}

static BP_PROBE: AcProbe = AcProbe::new();

fn bp_hook(frame: &mut TrapFrame) -> bool {
    BP_PROBE.probe(frame);
    false
}

fn test_ac_clear_on_exception() -> Outcome {
    if !x86::smap_live() {
        return Outcome::Skip("no SMAP");
    }
    let before = settled_frames();
    let Some(mut space) = addr_space_init::create() else {
        return Outcome::Fail("create");
    };
    // SAFETY: invariant: `space` is a fresh address space that no CPU has
    // loaded, and the range is page-aligned user space; established here.
    if unsafe { addr_space_init::map_anon(&mut space, USER_VA, PAGE_SIZE_4K, UserPerms::RW) }
        .is_err()
    {
        addr_space_init::teardown(space);
        return Outcome::Fail("map_anon");
    }
    BP_PROBE.reset();
    let ac_after = {
        let _g = x86::InterruptGuard::enter();
        addr_space_init::load_cr3(&space);
        x86::invlpg(USER_VA);
        testing::set_hook(vectors::BP, Some(bp_hook));
        x86::stac();
        // SAFETY: invariant: `int3` at CPL 0 reaches `breakpoint`, which
        // logs and returns; established by `arch::idt::init`.
        unsafe { core::arch::asm!("int3", options(nomem, nostack)) };
        let ac = x86::rflags() & RFLAGS_AC != 0;
        x86::clac();
        testing::set_hook(vectors::BP, None);
        addr_space_init::load_kernel_cr3();
        ac
    };
    addr_space_init::teardown(space);
    if let Some(fail) = BP_PROBE.verdict() {
        return fail;
    }
    if !ac_after {
        return Outcome::Fail("iretq did not restore AC");
    }
    if !user::frames_settle(before) {
        return crate::fail_fmt!("frame leak: {} -> {}", before, super::free_frames());
    }
    Outcome::Ok
}

// Set RFLAGS.AC with popf, then raise #UD; exit(1) if ud2 returns.
user_code!(
    POPF_AC_UD2,
    "
    pushfq
    or dword ptr [rsp], 0x40000
    popfq
    ud2
    mov edi, 1
    mov eax, 60
    syscall
    "
);

static UD_PROBE: AcProbe = AcProbe::new();

fn ud_hook(frame: &mut TrapFrame) -> bool {
    if frame.user_mode() {
        UD_PROBE.probe(frame);
    }
    false
}

fn test_ac_clear_user_popf() -> Outcome {
    if !x86::smap_live() {
        return Outcome::Skip("no SMAP");
    }
    let before = settled_frames();
    UD_PROBE.reset();
    testing::set_hook(vectors::UD, Some(ud_hook));
    let st = user::run(&Image::Code(POPF_AC_UD2, DEFAULT), &["popf_ac"]);
    testing::set_hook(vectors::UD, None);
    let st = match st {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if let Some(fail) = UD_PROBE.verdict() {
        return fail;
    }
    if st != wait_signaled(SIGILL) {
        return crate::fail_fmt!("status {st:#x}, want SIGILL");
    }
    if !user::frames_settle(before) {
        return crate::fail_fmt!("frame leak: {} -> {}", before, super::free_frames());
    }
    Outcome::Ok
}

// Hardware execute breakpoints on the three syscall instructions that run
// at CPL 0 with the user GS base loaded (DESIGN §5.10 rule 3): DR0 on the
// entry `swapgs`, DR1 on the `sysretq` and DR2 on the `iretq` after the
// exits' `swapgs`. Test-only helpers; nothing else in the kernel uses the
// debug registers.

unsafe extern "C" {
    static vibeos_syscall_entry: [u8; 3];
    static vibeos_syscall_exit_swapgs: [u8; 5];
    static vibeos_syscall_iret_swapgs: [u8; 5];
}

const SWAPGS: [u8; 3] = [0x0F, 0x01, 0xF8];
const SYSRETQ: [u8; 3] = [0x48, 0x0F, 0x07];
const IRETQ: [u8; 2] = [0x48, 0xCF];
const RFLAGS_RF: u64 = 1 << 16;
/// DR7 L0, L1 and L2; R/W and LEN zero: 1-byte execute breakpoints.
const DR7_ARM: u64 = 0b01_0101;

static DB_ADDR: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static DB_HITS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static DB_BAD: AtomicU64 = AtomicU64::new(0);
static DB_ENTRIES: AtomicU32 = AtomicU32::new(0);

/// Write DR0 to DR2 from `DB_ADDR`, then DR7.
fn arm_db(_: *mut ()) {
    let a = |i: usize| DB_ADDR[i].load(Ordering::Acquire);
    // SAFETY: invariant: the addresses are kernel text and DR7 enables
    // only execute breakpoints on them, which `db_hook` resumes past;
    // established by `test_ist_gs_sign`, which clears them on every path.
    unsafe {
        core::arch::asm!(
            "mov dr0, {0}",
            "mov dr1, {1}",
            "mov dr2, {2}",
            "mov dr7, {3}",
            in(reg) a(0),
            in(reg) a(1),
            in(reg) a(2),
            in(reg) DR7_ARM,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Clear DR7, then DR0 to DR2.
fn disarm_db(_: *mut ()) {
    // SAFETY: invariant: zero in DR7 disables every breakpoint, and the
    // kernel keeps no other state in DR0 to DR2; established here.
    unsafe {
        core::arch::asm!(
            "mov dr7, {0}",
            "mov dr0, {0}",
            "mov dr1, {0}",
            "mov dr2, {0}",
            in(reg) 0u64,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// On every CPU, this one included.
fn on_all_cpus(f: fn(*mut ())) {
    ipi_init::call_mask(per_cpu_init::online_mask(), f, core::ptr::null_mut(), true);
    let _g = x86::InterruptGuard::enter();
    f(core::ptr::null_mut());
}

/// This CPU's `PerCpu`, found through GS, is the one its APIC ID names.
fn percpu_ok() -> bool {
    let Some(cpu) = per_cpu_init::try_current() else {
        return false;
    };
    if !core::ptr::eq(cpu.self_ptr.cast_const(), cpu) {
        return false;
    }
    let apic = x86::cpuid(1, 0).1 >> 24;
    super::cpu_remote(cpu.cpu_id).is_some_and(|c| c.apic_id == apic)
}

fn db_hook(frame: &mut TrapFrame) -> bool {
    let Some(slot) = (0..3).find(|&i| frame.dr6 & (1 << i) != 0) else {
        return false;
    };
    if !percpu_ok() {
        DB_BAD.fetch_add(1, Ordering::Relaxed);
    }
    if slot == 0 && DB_ENTRIES.fetch_add(1, Ordering::Relaxed) % 2 == 1 {
        // The user RFLAGS that `syscall` left in r11: RF sends this call's
        // exit down the `iretq` path.
        frame.user_mut().r11 |= RFLAGS_RF;
    }
    // Resume past the breakpoint instead of taking it again.
    frame.iret.rflags |= RFLAGS_RF;
    DB_HITS[slot].fetch_add(1, Ordering::Release);
    true
}

// getpid forever, with a short spin between calls. Ends by SIGKILL, which
// acts at its next syscall entry.
user_code!(
    GETPID_LOOP,
    "
2:
    mov ecx, 2000
1:
    pause
    dec ecx
    jnz 1b
    mov eax, 39
    syscall
    jmp 2b
    "
);

/// CPL-3 entries on every vector so far.
fn cpl3_total() -> u64 {
    (0..=255u8).fold(0, |n, v| n.wrapping_add(testing::cpl3_hits(v)))
}

/// Yield until `pred` holds, for at most `ms`.
fn wait_until(pred: impl Fn() -> bool, ms: u64) -> bool {
    let deadline = time_init::now_ns().saturating_add(ms.saturating_mul(1_000_000));
    while !pred() {
        if time_init::now_ns() >= deadline {
            return false;
        }
        thread_init::yield_now();
    }
    true
}

fn test_ist_gs_sign() -> Outcome {
    let entry = (&raw const vibeos_syscall_entry).cast::<u8>();
    let sysret = (&raw const vibeos_syscall_exit_swapgs)
        .cast::<u8>()
        .wrapping_add(3);
    let iret = (&raw const vibeos_syscall_iret_swapgs)
        .cast::<u8>()
        .wrapping_add(3);
    // SAFETY: invariant: each symbol is a label in `syscall_init`'s entry
    // asm with at least that many bytes of kernel text after it, mapped
    // for the kernel's life; established by `syscall_init`'s `global_asm!`.
    let bytes = unsafe {
        (
            core::slice::from_raw_parts(entry, 3),
            core::slice::from_raw_parts(sysret.wrapping_sub(3), 6),
            core::slice::from_raw_parts(iret.wrapping_sub(3), 5),
        )
    };
    if bytes.0 != SWAPGS
        || bytes.1[..3] != SWAPGS
        || bytes.1[3..] != SYSRETQ
        || bytes.2[..3] != SWAPGS
        || bytes.2[3..] != IRETQ
    {
        return Outcome::Fail("syscall labels moved off swapgs; sysretq / swapgs; iretq");
    }
    for (slot, a) in [entry, sysret, iret].into_iter().enumerate() {
        DB_ADDR[slot].store(a as u64, Ordering::Release);
        DB_HITS[slot].store(0, Ordering::Relaxed);
    }
    DB_BAD.store(0, Ordering::Relaxed);
    DB_ENTRIES.store(0, Ordering::Relaxed);
    let cpl3_before = cpl3_total();
    let pid = match user::spawn(&Image::Code(GETPID_LOOP, DEFAULT), &["getpid_loop"]) {
        Ok(pid) => pid,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    // Arm once the child has taken an interrupt in ring 3, so it is past
    // `enter_user_full`: with breakpoints armed, TCG takes interrupts at
    // every instruction, which widens that function's IF=1 window with the
    // user GS base loaded (F006, ROADMAP §10.6).
    let in_ring3 = wait_until(|| cpl3_total() != cpl3_before, 5_000);
    if in_ring3 {
        testing::set_hook(vectors::DB, Some(db_hook));
        on_all_cpus(arm_db);
        // A shortfall shows in the hit counts below.
        let _ = wait_until(
            || {
                let h = |i: usize| DB_HITS[i].load(Ordering::Acquire);
                h(0) >= 4 && h(1) >= 1 && h(2) >= 1
            },
            5_000,
        );
        on_all_cpus(disarm_db);
        testing::set_hook(vectors::DB, None);
    }
    let _ = proc_init::dispatch(SYS_KILL, [pid as u64, SIGKILL as u64, 0, 0, 0, 0]);
    let st = user::wait(pid);
    if !in_ring3 {
        return Outcome::Fail("child took no interrupt in ring 3");
    }
    if st != wait_signaled(SIGKILL) {
        return crate::fail_fmt!("status {st:#x}, want SIGKILL");
    }
    let hits = [0, 1, 2].map(|i| DB_HITS[i].load(Ordering::Acquire));
    let bad = DB_BAD.load(Ordering::Relaxed);
    if hits.contains(&0) || bad != 0 {
        return crate::fail_fmt!(
            "hits entry {} sysretq {} iretq {}, bad PerCpu lookups {bad}",
            hits[0],
            hits[1],
            hits[2]
        );
    }
    Outcome::Ok
}
