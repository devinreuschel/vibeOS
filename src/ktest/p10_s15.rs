//! In-guest tests of P10-S15, Generated IDT entry stubs (DESIGN §8.2).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vibeos::addr_space::UserPerms;
use vibeos::paging::PAGE_SIZE_4K;
use vibeos::proc::{SIGILL, wait_signaled};
use vibeos::vectors;

use super::user::{self, DEFAULT, Image, user_code};
use super::{Outcome, Test, test};
use crate::addr_space_init;
use crate::arch::idt::TrapFrame;
use crate::arch::idt::testing;
use crate::thread_init;
use crate::x86;

pub(super) const TESTS: &[Test] = &[
    test("ac_clear_on_exception", test_ac_clear_on_exception).deadline(30_000),
    test("ac_clear_user_popf", test_ac_clear_user_popf).deadline(30_000),
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
