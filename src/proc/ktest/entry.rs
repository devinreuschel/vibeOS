//! In-guest tests for proc (kernel_tests only): address spaces, syscall entry and register state.
//! Rows: the parent `ktest.rs`'s `TESTS`.

use core::mem::ManuallyDrop;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::addr_space::{UserMemError, UserPerms};
use vibeos::arch::PageTable;
use vibeos::kerror::KError;
use vibeos::paging::{PAGE_SIZE_4K, USER_END};
use vibeos::proc::{SIGILL, SIGKILL, SIGTRAP, wait_exited, wait_signaled, wexitstatus, wifexited};
use vibeos::syscall::SYS_KILL;
use vibeos::thread::ThreadState;
use vibeos::vectors;

use crate::addr_space_init::{self, Space};
use crate::apic_init;
use crate::arch;
use crate::arch::current::Arch;
use crate::arch::idt::testing as idt_testing;
use crate::ktest::user::{self, DEFAULT, Image, Layout, user_code};
use crate::ktest::{Outcome, quiescent_free_frames, spawn_thread};
use crate::per_cpu_init;
use crate::proc_init::{self, testing as proc_testing};
use crate::sync::blocking_init::Semaphore;
use crate::syscall_init::testing as entry_testing;
use crate::thread_init;
use crate::time_init;
#[cfg(target_arch = "x86_64")]
use crate::x86;

#[cfg(target_arch = "x86_64")]
pub(crate) fn test_addrspace_map_unmap_teardown() -> Outcome {
    let before = quiescent_free_frames();
    let Ok(space) = addr_space_init::create() else {
        return Outcome::Fail("create");
    };
    // Above the 512 MiB GLOBAL low-identity window (DESIGN §4.1).
    let va = 0x0000_0000_4000_0000u64;
    // SAFETY: `addr_space_init::map_anon` checks the range is in the user
    // half and clear of every region of this fresh space before it maps;
    // established by `addr_space_init::map_anon`.
    if unsafe { addr_space_init::map_anon(&space, va, PAGE_SIZE_4K * 2, UserPerms::RW) }.is_err() {
        return Outcome::Fail("map_anon");
    }
    let pt_frames = space.mm().pt_frames();
    // IF stays off while this thread runs on `space`'s CR3: the registry
    // thread has IF=1 and `as_cr3 == 0`, so a switch away and back in this
    // window would reload the kernel CR3 under the user VA below.
    let irqs_off = crate::arch::current::InterruptGuard::enter();
    super::load_cr3(&space);
    x86::invlpg(va);
    // User PTE: SMAP would #PF a kernel store/load via this VA.
    x86::stac();
    // SAFETY: `va` is a mapped, writable, 8-byte aligned page of `space`,
    // which CR3 holds with IF=0 and SMAP lifted, so the store and load reach
    // that page's frame and nothing else; established here.
    unsafe {
        (va as *mut u64).write_volatile(0x1111_2222_3333_4444);
    }
    // SAFETY: as for the store above; established here.
    let got = unsafe { (va as *const u64).read_volatile() };
    x86::clac();
    // The kernel root and IF on again before the unmap, which sleeps for
    // the space's `mm` lock.
    addr_space_init::load_kernel_cr3();
    drop(irqs_off);
    if got != 0x1111_2222_3333_4444 {
        return Outcome::Fail("readback");
    }
    // SAFETY: the range's leaves came from the buddy through `map_anon`
    // above, and no CPU has `space` loaded any more, so no TLB holds them
    // past this CPU's kernel-root load; established here.
    if unsafe { addr_space_init::unmap(&space, va, PAGE_SIZE_4K * 2) }.is_err() {
        return Outcome::Fail("unmap");
    }
    // Root, PDPT, PD and PT: the tables outlive the unmap.
    if pt_frames < 4 {
        return Outcome::Fail("teardown pt");
    }
    // The last `users` put tears the space down, and the core's free
    // frees the root.
    drop(space);
    if quiescent_free_frames() != before {
        return Outcome::Fail("frame leak");
    }
    Outcome::Ok
}

pub(crate) fn test_user_ptr_helpers() -> Outcome {
    let Ok(space) = addr_space_init::create() else {
        return Outcome::Fail("create");
    };
    let va = 0x0000_0000_4000_0000u64;
    // SAFETY: `addr_space_init::map_anon` checks the range is in the user
    // half and clear of every region of this fresh space before it maps;
    // established by `addr_space_init::map_anon`.
    if unsafe { addr_space_init::map_anon(&space, va, PAGE_SIZE_4K, UserPerms::RW) }.is_err() {
        return Outcome::Fail("map");
    }
    let mm = space.mm();
    if mm.check_user_range(va, 8).is_err() {
        return Outcome::Fail("mapped range");
    }
    if mm.check_user_range(0, 8) != Err(UserMemError::NullGuard) {
        return Outcome::Fail("null guard");
    }
    if mm.check_user_range(0xFFFF_8000_0000_1000, 8) != Err(UserMemError::Kernel) {
        return Outcome::Fail("kernel ptr");
    }
    if mm.check_user_range(u64::MAX, 2) != Err(UserMemError::Overflow) {
        return Outcome::Fail("overflow");
    }
    if mm.check_user_range(va + PAGE_SIZE_4K, 8) != Err(UserMemError::Unmapped) {
        return Outcome::Fail("unmapped");
    }
    if mm.check_user_range(USER_END, 8) != Err(UserMemError::NonCanonical) {
        return Outcome::Fail("user end");
    }
    Outcome::Ok
}

pub(crate) fn test_cr3_switch_skip() -> Outcome {
    let Ok(a) = addr_space_init::create() else {
        return Outcome::Fail("create a");
    };
    let Ok(b) = addr_space_init::create() else {
        return Outcome::Fail("create b");
    };
    // IF stays off while this thread runs on a user CR3 (see
    // test_addrspace_map_unmap_teardown): a switch away and back would reload
    // the kernel CR3 between the load and the read.
    let irqs_off = crate::arch::current::InterruptGuard::enter();
    let r = cr3_switch_steps(&a, &b);
    // The kernel root before the spaces go: their core's free asserts no
    // CPU has them loaded.
    addr_space_init::load_kernel_cr3();
    drop(irqs_off);
    match r {
        Ok(()) => Outcome::Ok,
        Err(m) => Outcome::Fail(m),
    }
}

/// [`test_cr3_switch_skip`]'s loads, with IF off; the caller reloads the
/// kernel root after.
fn cr3_switch_steps(a: &Space, b: &Space) -> Result<(), &'static str> {
    super::load_cr3(a);
    let cr3_a = <Arch as PageTable>::root().as_u64();
    if !super::cr3_was_skipped(a) {
        return Err("a not recorded");
    }
    super::load_cr3(a);
    if (<Arch as PageTable>::root().as_u64()) != cr3_a {
        return Err("skip mutated cr3");
    }
    super::load_cr3(b);
    let cr3_b = <Arch as PageTable>::root().as_u64();
    if cr3_b == cr3_a {
        return Err("b shares a cr3");
    }
    Ok(())
}

// syscall 0xC0FFEE, then `ud2` if rax is -ENOSYS, else exit(1).
user_code!(
    ENOSYS_PROBE,
    "
    mov eax, 0xC0FFEE
    syscall
    cmp rax, -38
    jne 1f
    ud2
1:
    mov edi, 1
    mov eax, 60
    syscall
    ud2
    "
);

pub(crate) fn test_ring3_syscall_enosys() -> Outcome {
    let before = quiescent_free_frames();
    let st = match user::run(&user::Image::Code(ENOSYS_PROBE, user::DEFAULT), &["enosys"]) {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if st == wait_exited(1) {
        return Outcome::Fail("rax not -ENOSYS");
    }
    if st != wait_signaled(SIGILL) {
        return crate::fail_fmt!("status {st:#x}, want SIGILL");
    }
    let after = quiescent_free_frames();
    if after != before {
        crate::marker!("vibeOS: ktest:   frames {before} -> {after}");
        return Outcome::Fail("enosys frame leak");
    }
    Outcome::Ok
}

pub(crate) fn test_ring3_hello_exit() -> Outcome {
    let before = quiescent_free_frames();
    let pid = match proc_init::spawn_elf(b"/hello", &[&b"/hello"[..]], &[], 0, 0) {
        Ok(pid) => pid,
        Err(e) => return crate::fail_fmt!("spawn /hello: {}", e.as_str()),
    };
    let st = proc_init::wait_kernel(pid);
    if st != wait_exited(42) {
        return crate::fail_fmt!("hello status {st:#x}, want exited 42");
    }
    let after = quiescent_free_frames();
    if after != before {
        crate::marker!("vibeOS: ktest:   frames {before} -> {after}");
        return Outcome::Fail("hello frame leak");
    }
    Outcome::Ok
}

pub(crate) fn test_syscall_dispatch() -> Outcome {
    if proc_init::dispatch(vibeos::syscall::SYS_GETPID, [0; 6]) != 0 {
        return Outcome::Fail("getpid");
    }
    if proc_init::dispatch(vibeos::syscall::SYS_SCHED_YIELD, [0; 6]) != 0 {
        return Outcome::Fail("yield");
    }
    if proc_init::dispatch(vibeos::syscall::SYS_WRITE, [3, 0, 1, 0, 0, 0])
        != vibeos::syscall::encode(Err(KError::BadF))
    {
        return Outcome::Fail("ebadf");
    }
    if proc_init::dispatch(0xC0FFEE, [0; 6]) != vibeos::syscall::encode(Err(KError::NoSys)) {
        return Outcome::Fail("enosys");
    }
    // The number is `eax` sign-extended (SYSCALL.md §1): the high half of
    // `rax` is ignored, and a negative `eax` names no call.
    if proc_init::dispatch(0xFFFF_FFFF_0000_0000 | vibeos::syscall::SYS_GETPID, [0; 6]) != 0 {
        return Outcome::Fail("getpid, high half set");
    }
    if proc_init::dispatch(
        0x1_0000_0000 | vibeos::syscall::SYS_WRITE,
        [3, 0, 1, 0, 0, 0],
    ) != vibeos::syscall::encode(Err(KError::BadF))
    {
        return Outcome::Fail("write, high half set");
    }
    if proc_init::dispatch(0x8000_0000, [0; 6]) != vibeos::syscall::encode(Err(KError::NoSys)) {
        return Outcome::Fail("negative eax");
    }
    Outcome::Ok
}

// write(1, "hi\n" in its page, 3) must return 3; then NULL/8, a kernel
// pointer/8, the unmapped page after its own/8, and -1/2 must each return
// -EFAULT. Exits 11 to 15 at the first mismatch, else 0.
user_code!(
    PTR_VALIDATE,
    "
    lea rbx, [rip]
    and rbx, -4096
    mov edi, 1
    lea rsi, [rip + 9f]
    mov edx, 3
    mov eax, 1
    syscall
    mov r12d, 11
    cmp rax, 3
    jne 8f
    mov edi, 1
    xor esi, esi
    mov edx, 8
    mov eax, 1
    syscall
    mov r12d, 12
    cmp rax, -14
    jne 8f
    mov edi, 1
    mov rsi, 0xFFFF800000001000
    mov edx, 8
    mov eax, 1
    syscall
    mov r12d, 13
    cmp rax, -14
    jne 8f
    mov edi, 1
    lea rsi, [rbx + 0x1000]
    mov edx, 8
    mov eax, 1
    syscall
    mov r12d, 14
    cmp rax, -14
    jne 8f
    mov edi, 1
    mov rsi, -1
    mov edx, 2
    mov eax, 1
    syscall
    mov r12d, 15
    cmp rax, -14
    jne 8f
    xor r12d, r12d
8:
    mov edi, r12d
    mov eax, 60
    syscall
    ud2
9:
    .byte 0x68, 0x69, 0x0a
    "
);

pub(crate) fn test_syscall_ptr_validate() -> Outcome {
    let st = match user::run(&user::Image::Code(PTR_VALIDATE, user::DEFAULT), &["ptrs"]) {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if !wifexited(st) {
        return crate::fail_fmt!("status {st:#x}, want exited");
    }
    match wexitstatus(st) {
        0 => Outcome::Ok,
        11 => Outcome::Fail("good write"),
        12 => Outcome::Fail("null"),
        13 => Outcome::Fail("kernel ptr"),
        14 => Outcome::Fail("unmapped"),
        15 => Outcome::Fail("overflow"),
        code => crate::fail_fmt!("exit {code}"),
    }
}

/// Grow the kernel heap to what `/bin/tests`' `exec_args` cases take, so
/// the count below sees no growth: argument blocks filled to Linux's
/// 2 MiB limit in `copy_cvec`'s 256-byte chunks (ROADMAP §10.5), two at
/// once, the headroom first-fit placement needs when the heap's free
/// space is split. The heap keeps the pages it maps, so without this the
/// first such `execve` in the window reads as frames lost.
fn warm_exec_args() {
    use vibeos::elf::{ExecArgs, arg_space_limit};
    use vibeos::limits::{MAX_ARG_STRLEN, RLIMIT_STACK_DEFAULT};
    let fill = |args: &mut ExecArgs| {
        let chunk = [b'x'; 256];
        loop {
            if args.begin(false).is_err() {
                return;
            }
            let mut len = 0usize;
            while len + chunk.len() < MAX_ARG_STRLEN {
                if args.extend(&chunk).is_err() {
                    return;
                }
                len += chunk.len();
            }
            if args.end().is_err() {
                return;
            }
        }
    };
    let limit = arg_space_limit(RLIMIT_STACK_DEFAULT);
    let mut a = ExecArgs::new(limit);
    let mut b = ExecArgs::new(limit);
    fill(&mut a);
    fill(&mut b);
}

pub(crate) fn test_user_syscalls() -> Outcome {
    warm_exec_args();
    let before = quiescent_free_frames();
    let pid = match proc_init::spawn_elf(b"/bin/tests", &[&b"/bin/tests"[..]], &[], 0, 0) {
        Ok(pid) => pid,
        Err(e) => return crate::fail_fmt!("spawn /bin/tests: {}", e.as_str()),
    };
    let st = proc_init::wait_kernel(pid);
    if st != wait_exited(0) {
        return crate::fail_fmt!("tests status {st:#x}, want exited 0");
    }
    let after = quiescent_free_frames();
    if after != before {
        crate::marker!("vibeOS: ktest:   frames {before} -> {after}");
        return Outcome::Fail("tests frame leak");
    }
    Outcome::Ok
}

/// Yield until `pred` holds or `ms` pass.
fn wait_ms(pred: impl Fn() -> bool, ms: u64) -> bool {
    let t0 = time_init::now_ns();
    while !pred() {
        if time_init::now_ns().saturating_sub(t0) > ms.saturating_mul(1_000_000) {
            return false;
        }
        thread_init::yield_now();
    }
    true
}

// ---------------------------------------------------------------------------
// teardown_live_root_asserts (ROADMAP §10.3, F018, F106)

/// The parked thread waits here until the test has cleared its `as_cr3`.
static PARK: Semaphore = Semaphore::new(0);

fn parked() {
    PARK.acquire();
}

/// The free of a root that a parked TCB's `as_cr3` still names hits the
/// assertion at the root's free (invariant I128), `SpaceCore`'s drop, which
/// the space's last reference reaches, instead of freeing the root.
pub(crate) fn teardown_live_root_asserts() -> Outcome {
    let space = match addr_space_init::create() {
        Ok(s) => s.publish(),
        Err(_) => return Outcome::Fail("create"),
    };
    let root = space.root().as_u64();
    let th = spawn_thread("s07-park", parked);
    let id = th.id();
    if !wait_ms(
        || {
            matches!(
                thread_init::try_state(id),
                Some(ThreadState::Blocked { .. })
            )
        },
        1_000,
    ) {
        PARK.release();
        return Outcome::Fail("parked thread did not block");
    }
    thread_init::set_pid_cr3(id, 0, root);
    let keep = ManuallyDrop::new(space);
    let nest0 = per_cpu_init::irq_nest();
    let if0 = crate::arch::current::interrupts_enabled();
    let hit = arch::catch::catch_panic(|| {
        // SAFETY: `keep` is never used again, so this copy is the space's
        // only `users` and core reference; its drop is the last put of
        // both. The assertion fires inside the core's free, before the root
        // is freed, so the longjmp leaves the root frame and the cell
        // leaked, never freed twice; established here.
        let last = unsafe { ptr::read(&*keep) };
        drop(last);
    });
    crate::ktest::restore_irq_nest(nest0);
    thread_init::set_pid_cr3(id, 0, 0);
    PARK.release();
    let died = wait_ms(
        || matches!(thread_init::try_state(id), None | Some(ThreadState::Dead)),
        2_000,
    );
    if !hit {
        return Outcome::Fail("the core's free freed a root a parked TCB names");
    }
    if crate::arch::current::interrupts_enabled() != if0 {
        return Outcome::Fail("IF changed across the caught assertion");
    }
    if !died {
        return Outcome::Fail("parked thread did not exit");
    }
    Outcome::Ok
}

/// What `arm_rcx_canary` puts in the frame's `rcx`: not the return RIP.
const RCX_CANARY: u64 = 0x0C0F_FEE0_DEAD_BEEF;

// getpid with rdi = proc_init::testing::HOOK_MAGIC; exit(0) when RCX holds
// RCX_CANARY after the call, else exit(1).
user_code!(
    RCX_CANARY_PROG,
    "
    movabs rdi, 0x5EEDCA1100005A21
    mov eax, 39
    syscall
    movabs rdx, 0x0C0FFEE0DEADBEEF
    xor edi, edi
    cmp rcx, rdx
    setne dil
    mov eax, 60
    syscall
    ud2
    "
);

const _: () = assert!(proc_testing::HOOK_MAGIC == 0x5EED_CA11_0000_5A21);

/// A hook sets a `getpid`'s saved `rcx` to a canary that is not its
/// return RIP; the exit then leaves through `iretq`, and the program finds
/// the canary in RCX.
pub(crate) fn test_syscall_rcx_canary() -> Outcome {
    proc_testing::arm_rcx_canary(RCX_CANARY);
    let st = user::run(&Image::Code(RCX_CANARY_PROG, DEFAULT), &["rcx_canary"]);
    proc_testing::disarm();
    match st {
        Ok(0) => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("status {st:#x}, want 0"),
        Err(e) => crate::fail_fmt!("spawn: {}", e.as_str()),
    }
}

// fork with 12 syscall-preserved GPRs holding canaries. The child exits 0
// iff all 12 survived its first return, else 1; the parent exits 2 if its
// own 12 changed, else waits for the child and exits with its code.
user_code!(
    FORK_GPRS,
    "
    movabs rbx, 0xC0DE0100F00DF001
    movabs rbp, 0xC0DE0200F00DF002
    movabs rdx, 0xC0DE0300F00DF003
    movabs rsi, 0xC0DE0400F00DF004
    movabs rdi, 0xC0DE0500F00DF005
    movabs r8, 0xC0DE0600F00DF006
    movabs r9, 0xC0DE0700F00DF007
    movabs r10, 0xC0DE0800F00DF008
    movabs r12, 0xC0DE0900F00DF009
    movabs r13, 0xC0DE0A00F00DF00A
    movabs r14, 0xC0DE0B00F00DF00B
    movabs r15, 0xC0DE0C00F00DF00C
    mov eax, 57
    syscall
    test rax, rax
    jz 5f
    js 8f
    movabs rcx, 0xC0DE0100F00DF001
    cmp rbx, rcx
    jne 8f
    movabs rcx, 0xC0DE0200F00DF002
    cmp rbp, rcx
    jne 8f
    movabs rcx, 0xC0DE0300F00DF003
    cmp rdx, rcx
    jne 8f
    movabs rcx, 0xC0DE0400F00DF004
    cmp rsi, rcx
    jne 8f
    movabs rcx, 0xC0DE0500F00DF005
    cmp rdi, rcx
    jne 8f
    movabs rcx, 0xC0DE0600F00DF006
    cmp r8, rcx
    jne 8f
    movabs rcx, 0xC0DE0700F00DF007
    cmp r9, rcx
    jne 8f
    movabs rcx, 0xC0DE0800F00DF008
    cmp r10, rcx
    jne 8f
    movabs rcx, 0xC0DE0900F00DF009
    cmp r12, rcx
    jne 8f
    movabs rcx, 0xC0DE0A00F00DF00A
    cmp r13, rcx
    jne 8f
    movabs rcx, 0xC0DE0B00F00DF00B
    cmp r14, rcx
    jne 8f
    movabs rcx, 0xC0DE0C00F00DF00C
    cmp r15, rcx
    jne 8f
    mov rdi, rax
    lea rsi, [rip + 7f]
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, dword ptr [rip + 7f]
    shr edi, 8
    and edi, 0xff
    mov eax, 60
    syscall
    ud2
8:
    mov edi, 2
    mov eax, 60
    syscall
    ud2
5:
    movabs rcx, 0xC0DE0100F00DF001
    cmp rbx, rcx
    jne 9f
    movabs rcx, 0xC0DE0200F00DF002
    cmp rbp, rcx
    jne 9f
    movabs rcx, 0xC0DE0300F00DF003
    cmp rdx, rcx
    jne 9f
    movabs rcx, 0xC0DE0400F00DF004
    cmp rsi, rcx
    jne 9f
    movabs rcx, 0xC0DE0500F00DF005
    cmp rdi, rcx
    jne 9f
    movabs rcx, 0xC0DE0600F00DF006
    cmp r8, rcx
    jne 9f
    movabs rcx, 0xC0DE0700F00DF007
    cmp r9, rcx
    jne 9f
    movabs rcx, 0xC0DE0800F00DF008
    cmp r10, rcx
    jne 9f
    movabs rcx, 0xC0DE0900F00DF009
    cmp r12, rcx
    jne 9f
    movabs rcx, 0xC0DE0A00F00DF00A
    cmp r13, rcx
    jne 9f
    movabs rcx, 0xC0DE0B00F00DF00B
    cmp r14, rcx
    jne 9f
    movabs rcx, 0xC0DE0C00F00DF00C
    cmp r15, rcx
    jne 9f
    xor edi, edi
    mov eax, 60
    syscall
    ud2
9:
    mov edi, 1
    mov eax, 60
    syscall
    ud2
    .balign 8
7:
    .quad 0
    "
);

/// A forked child's first return to ring 3 is the syscall exit over the
/// parent's frame with `rax` = 0: the 12 GPRs a syscall preserves reach
/// the child intact, and the parent keeps its own.
pub(crate) fn test_fork_child_gprs() -> Outcome {
    let img = Image::Code(
        FORK_GPRS,
        Layout {
            writable: true,
            ..DEFAULT
        },
    );
    match user::run(&img, &["fork_gprs"]) {
        Ok(0) => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("status {st:#x}, want 0 (exit 1: child GPRs, 2: parent GPRs)"),
        Err(e) => crate::fail_fmt!("spawn: {}", e.as_str()),
    }
}

// 15 GPR canaries (arch::idt::testing::GPR_CANARIES), then a spin at
// +0x100; the hook sends the frame's RIP to +0x200, which exits 0 iff
// every GPR still holds its canary, else 1.
user_code!(
    GPR_CANARY_SPIN,
    "
    movabs r15, 0xC0DE000000000101
    movabs r14, 0xC0DE000000000202
    movabs r13, 0xC0DE000000000303
    movabs r12, 0xC0DE000000000404
    movabs rbp, 0xC0DE000000000505
    movabs rbx, 0xC0DE000000000606
    movabs r11, 0xC0DE000000000707
    movabs r10, 0xC0DE000000000808
    movabs r9, 0xC0DE000000000909
    movabs r8, 0xC0DE000000000A0A
    movabs rax, 0xC0DE000000000B0B
    movabs rcx, 0xC0DE000000000C0C
    movabs rdx, 0xC0DE000000000D0D
    movabs rsi, 0xC0DE000000000E0E
    movabs rdi, 0xC0DE000000000F0F
    jmp 2f
    .org vibeos_user_code_GPR_CANARY_SPIN + 0x100, 0xcc
2:
    jmp 2b
    .org vibeos_user_code_GPR_CANARY_SPIN + 0x200, 0xcc
    cmp r15, qword ptr [rip + 3f + 0]
    jne 9f
    cmp r14, qword ptr [rip + 3f + 8]
    jne 9f
    cmp r13, qword ptr [rip + 3f + 16]
    jne 9f
    cmp r12, qword ptr [rip + 3f + 24]
    jne 9f
    cmp rbp, qword ptr [rip + 3f + 32]
    jne 9f
    cmp rbx, qword ptr [rip + 3f + 40]
    jne 9f
    cmp r11, qword ptr [rip + 3f + 48]
    jne 9f
    cmp r10, qword ptr [rip + 3f + 56]
    jne 9f
    cmp r9, qword ptr [rip + 3f + 64]
    jne 9f
    cmp r8, qword ptr [rip + 3f + 72]
    jne 9f
    cmp rax, qword ptr [rip + 3f + 80]
    jne 9f
    cmp rcx, qword ptr [rip + 3f + 88]
    jne 9f
    cmp rdx, qword ptr [rip + 3f + 96]
    jne 9f
    cmp rsi, qword ptr [rip + 3f + 104]
    jne 9f
    cmp rdi, qword ptr [rip + 3f + 112]
    jne 9f
    xor edi, edi
    mov eax, 60
    syscall
    ud2
9:
    mov edi, 1
    mov eax, 60
    syscall
    ud2
    .balign 8
3:
    .quad 0xC0DE000000000101
    .quad 0xC0DE000000000202
    .quad 0xC0DE000000000303
    .quad 0xC0DE000000000404
    .quad 0xC0DE000000000505
    .quad 0xC0DE000000000606
    .quad 0xC0DE000000000707
    .quad 0xC0DE000000000808
    .quad 0xC0DE000000000909
    .quad 0xC0DE000000000A0A
    .quad 0xC0DE000000000B0B
    .quad 0xC0DE000000000C0C
    .quad 0xC0DE000000000D0D
    .quad 0xC0DE000000000E0E
    .quad 0xC0DE000000000F0F
    "
);

/// Reschedule IPIs the canary hook must see the child take in its loop.
const CANARY_IPIS: u64 = 1_000;

static CANARY_CHILD: AtomicU64 = AtomicU64::new(0);

static CANARY_SPAWNED: AtomicBool = AtomicBool::new(false);

static CANARY_STOP: AtomicBool = AtomicBool::new(false);

static CANARY_SENT: AtomicBool = AtomicBool::new(false);

/// Pinned to CPU 0, so the child is too (`thread_init::spawn_user`).
fn canary_spawner() {
    let pid = match user::spawn(&Image::Code(GPR_CANARY_SPIN, DEFAULT), &["gpr_canary"]) {
        Ok(pid) => u64::from(pid),
        Err(_) => u64::MAX,
    };
    CANARY_CHILD.store(pid, Ordering::Relaxed);
    CANARY_SPAWNED.store(true, Ordering::Release);
}

/// Pinned to CPU 0: a ready thread, so each reschedule IPI switches the
/// child out.
fn canary_yielder() {
    while !CANARY_STOP.load(Ordering::Acquire) {
        thread_init::yield_now();
    }
}

/// Sends CPU 0 a reschedule IPI, waits up to 10 ms for the hook's hit
/// count to move, and sends again, until [`CANARY_IPIS`] hits or 30 s.
fn canary_sender() {
    let deadline = time_init::now_ns().saturating_add(30_000_000_000);
    while idt_testing::hits() < CANARY_IPIS && time_init::now_ns() < deadline {
        let h = idt_testing::hits();
        // A failed send shows as a shortfall in the hit count.
        let _ = apic_init::send_ipi_cpu(0, vectors::IPI_RESCHEDULE);
        let t = time_init::now_ns().saturating_add(10_000_000);
        while idt_testing::hits() == h && time_init::now_ns() < t {
            core::hint::spin_loop();
        }
    }
    CANARY_SENT.store(true, Ordering::Release);
}

/// A ring-3 loop holding canaries in all 15 GPRs takes 1,000 reschedule
/// IPIs on CPU 0, and at each one the hook finds the canaries in the user
/// frame at the top of the preempted thread's kernel stack.
pub(crate) fn test_preempt_gpr_canaries() -> Outcome {
    let Some(sender_cpu) = crate::ktest::second_cpu() else {
        return Outcome::Skip("needs 2 CPUs");
    };
    let base = DEFAULT.vaddr;
    CANARY_SPAWNED.store(false, Ordering::Release);
    CANARY_STOP.store(false, Ordering::Release);
    CANARY_SENT.store(false, Ordering::Release);
    idt_testing::arm_canaries(base + 0x100, base + 0x102, base + 0x200, CANARY_IPIS);
    crate::ktest::spawn_thread_on("canary_yielder", canary_yielder, 0);
    crate::ktest::spawn_thread_on("canary_spawner", canary_spawner, 0);
    let spawned = sleep_until(|| CANARY_SPAWNED.load(Ordering::Acquire), 5_000);
    let pid = u32::try_from(CANARY_CHILD.load(Ordering::Relaxed)).ok();
    let st = match (spawned, pid) {
        (true, Some(pid)) => {
            crate::ktest::spawn_thread_on("canary_sender", canary_sender, sender_cpu);
            let sent = sleep_until(|| CANARY_SENT.load(Ordering::Acquire), 35_000);
            if !sent || idt_testing::hits() < CANARY_IPIS {
                let _ =
                    proc_init::dispatch(SYS_KILL, [u64::from(pid), u64::from(SIGKILL), 0, 0, 0, 0]);
            }
            Some(user::wait(pid))
        }
        _ => None,
    };
    CANARY_STOP.store(true, Ordering::Release);
    idt_testing::disarm_canaries();
    let Some(st) = st else {
        return Outcome::Fail("spawn");
    };
    let (hits, bad, misplaced) = (
        idt_testing::hits(),
        idt_testing::bad(),
        idt_testing::misplaced(),
    );
    if hits != CANARY_IPIS || bad != 0 || misplaced != 0 || st != 0 {
        return crate::fail_fmt!(
            "hits {hits} bad {bad} misplaced {misplaced} status {st:#x}, want {CANARY_IPIS} 0 0 0"
        );
    }
    Outcome::Ok
}

/// Sleep until `pred` holds, for at most `ms`.
fn sleep_until(pred: impl Fn() -> bool, ms: u64) -> bool {
    let deadline = time_init::now_ns().saturating_add(ms.saturating_mul(1_000_000));
    while !pred() {
        if time_init::now_ns() >= deadline {
            return false;
        }
        thread_init::sleep_ms(1);
    }
    true
}

const _: () = {
    // The program's literals are the hook's canaries.
    assert!(idt_testing::GPR_CANARIES[0] == 0xC0DE_0000_0000_0101);
    assert!(idt_testing::GPR_CANARIES[14] == 0xC0DE_0000_0000_0F0F);
};

// Set RFLAGS.TF with popf: the single-step trap follows the nop.
user_code!(
    SINGLE_STEP,
    "
    pushfq
    or qword ptr [rsp], 0x100
    popfq
    nop
    nop
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

// int1 (ICEBP).
user_code!(
    INT1,
    "
    .byte 0xf1
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

/// Run `code` and expect `SIGTRAP` in its `wait4` status, the kernel up.
fn expect_sigtrap(code: &'static [u8], name: &str) -> Outcome {
    match user::run(&Image::Code(code, DEFAULT), &[name]) {
        Ok(st) if st == wait_signaled(SIGTRAP) => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("status {st:#x}, want SIGTRAP"),
        Err(e) => crate::fail_fmt!("spawn: {}", e.as_str()),
    }
}

/// A user `popf` that sets TF ends in `SIGTRAP`, not a halt.
pub(crate) fn test_user_single_step() -> Outcome {
    expect_sigtrap(SINGLE_STEP, "single_step")
}

/// A user `int1` ends in `SIGTRAP`, not a halt. TCG (QEMU 8.2) does not
/// deliver `int1` as `#DB`, and the child dies of `#UD`'s `SIGILL`
/// instead, so off KVM either signal passes, with the kernel up.
pub(crate) fn test_user_int1() -> Outcome {
    if crate::ktest::on_kvm() {
        return expect_sigtrap(INT1, "int1");
    }
    match user::run(&Image::Code(INT1, DEFAULT), &["int1"]) {
        Ok(st) if st == wait_signaled(SIGTRAP) => Outcome::Ok,
        Ok(st) if st == wait_signaled(SIGILL) => {
            crate::marker!(
                "vibeOS: ktest:   user_int1: SIGILL off KVM (TCG takes int1 as an invalid opcode)"
            );
            Outcome::Ok
        }
        Ok(st) => crate::fail_fmt!("status {st:#x}, want SIGTRAP (or SIGILL off KVM)"),
        Err(e) => crate::fail_fmt!("spawn: {}", e.as_str()),
    }
}

// Set TF; the #DB after the nop re-pins the thread and clears TF. Then 100
// getpid calls with rdi = HOOK_MAGIC, and exit(0). The instruction after
// the popf is a nop, never a syscall.
user_code!(
    TF_REPIN,
    "
    pushfq
    or qword ptr [rsp], 0x100
    popfq
    nop
    nop
    mov r12d, 100
1:
    movabs rdi, 0x5EEDCA1100005A21
    mov eax, 39
    syscall
    dec r12d
    jnz 1b
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

static REPIN_CHILD: AtomicU64 = AtomicU64::new(0);

static REPIN_SPAWNED: AtomicBool = AtomicBool::new(false);

static REPIN_STOP: AtomicBool = AtomicBool::new(false);

/// Pinned to CPU 0, so the child is too (`thread_init::spawn_user`).
fn repin_spawner() {
    let pid = match user::spawn(&Image::Code(TF_REPIN, DEFAULT), &["tf_repin"]) {
        Ok(pid) => u64::from(pid),
        Err(_) => u64::MAX,
    };
    REPIN_CHILD.store(pid, Ordering::Relaxed);
    REPIN_SPAWNED.store(true, Ordering::Release);
}

/// Pinned to CPU 0: the thread the `#DB` body's yield switches to.
fn repin_yielder() {
    while !REPIN_STOP.load(Ordering::Acquire) {
        thread_init::yield_now();
    }
}

/// A CPL-3 `#DB` body that yields moves to another CPU; the program then
/// runs there with `PerCpu` matching the CPU at each of 100 `getpid`s, and
/// exits 0: the moved frame returned through the common exit.
pub(crate) fn test_user_tf_repin() -> Outcome {
    if crate::ktest::second_cpu().is_none() {
        return Outcome::Skip("needs 2 CPUs");
    }
    let base = DEFAULT.vaddr;
    REPIN_SPAWNED.store(false, Ordering::Release);
    REPIN_STOP.store(false, Ordering::Release);
    idt_testing::arm_db_repin(base, base + 0x1000);
    proc_testing::arm_getpid_apic();
    crate::ktest::spawn_thread_on("repin_yielder", repin_yielder, 0);
    crate::ktest::spawn_thread_on("repin_spawner", repin_spawner, 0);
    let spawned = sleep_until(|| REPIN_SPAWNED.load(Ordering::Acquire), 5_000);
    let pid = u32::try_from(REPIN_CHILD.load(Ordering::Relaxed)).ok();
    let st = match (spawned, pid) {
        (true, Some(pid)) => Some(user::wait(pid)),
        _ => None,
    };
    REPIN_STOP.store(true, Ordering::Release);
    idt_testing::disarm_db_repin();
    proc_testing::disarm();
    let Some(st) = st else {
        return Outcome::Fail("spawn");
    };
    let (from, to) = idt_testing::repin_cpus();
    let (checks, bad) = (proc_testing::apic_checks(), proc_testing::apic_mismatches());
    if st != 0 || from == u32::MAX || from == to || checks != 100 || bad != 0 {
        return crate::fail_fmt!(
            "status {st:#x}, cpus {from} -> {to}, {checks} apic checks, {bad} mismatches; \
             want 0, two CPUs, 100, 0"
        );
    }
    Outcome::Ok
}

// The assembly `/bin/tests`' sequence from its `dup(1)` on (ROADMAP §10.2, F021), for
// FORK_WAIT_ROUNDS rounds: (1) dup(1), a zero-length write, close; (2) fork,
// the child exits 7, wait4(pid) wants 0x0700; (3) fork, the child execs
// /hello, wait4(pid) wants 0x2A00; (4) fork, the child loads from address
// 0, wait4(pid) wants SIGSEGV; (5) up to 32 forks of exit(0) children with
// two sched_yield each until fork fails, then wait4(-1, NULL) until
// -ECHILD. Exits 0; a failed check exits 10 + its step. Raise the round
// count only locally, for soak runs.
user_code!(
    FORK_WAIT,
    "
    .set FORK_WAIT_ROUNDS, 4
    sub rsp, 32
    mov r15d, FORK_WAIT_ROUNDS
1:
    mov edi, 1
    mov eax, 32
    syscall
    test rax, rax
    js 81f
    mov r12, rax
    mov rdi, r12
    lea rsi, [rip + 90f]
    xor edx, edx
    mov eax, 1
    syscall
    test rax, rax
    jnz 81f
    mov rdi, r12
    mov eax, 3
    syscall
    test rax, rax
    jnz 81f

    mov eax, 57
    syscall
    test rax, rax
    js 82f
    jnz 2f
    mov edi, 7
    mov eax, 60
    syscall
    ud2
2:
    mov r12, rax
    mov dword ptr [rsp], -1
    mov rdi, r12
    mov rsi, rsp
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    cmp rax, r12
    jne 82f
    cmp dword ptr [rsp], 0x0700
    jne 82f

    mov eax, 57
    syscall
    test rax, rax
    js 83f
    jnz 3f
    lea rdi, [rip + 91f]
    mov [rsp + 8], rdi
    mov qword ptr [rsp + 16], 0
    lea rsi, [rsp + 8]
    xor edx, edx
    mov eax, 59
    syscall
    mov edi, 127
    mov eax, 60
    syscall
    ud2
3:
    mov r12, rax
    mov dword ptr [rsp], -1
    mov rdi, r12
    mov rsi, rsp
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    cmp rax, r12
    jne 83f
    cmp dword ptr [rsp], 0x2A00
    jne 83f

    mov eax, 57
    syscall
    test rax, rax
    js 84f
    jnz 4f
    xor eax, eax
    mov rax, [rax]
    ud2
4:
    mov r12, rax
    mov dword ptr [rsp], -1
    mov rdi, r12
    mov rsi, rsp
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    cmp rax, r12
    jne 84f
    mov eax, dword ptr [rsp]
    and eax, 0x7f
    cmp eax, 11
    jne 84f

    xor r13d, r13d
5:
    mov eax, 57
    syscall
    test rax, rax
    js 6f
    jnz 7f
    xor edi, edi
    mov eax, 60
    syscall
    ud2
7:
    mov eax, 24
    syscall
    mov eax, 24
    syscall
    inc r13
    cmp r13, 32
    jb 5b
    jmp 8f
6:
    cmp rax, -11
    je 8f
    test r13, r13
    jz 85f
8:
    mov rdi, -1
    xor esi, esi
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    cmp rax, -10
    jne 8b

    dec r15d
    jnz 1b
    xor edi, edi
    mov eax, 60
    syscall
    ud2
81:
    mov edi, 11
    jmp 89f
82:
    mov edi, 12
    jmp 89f
83:
    mov edi, 13
    jmp 89f
84:
    mov edi, 14
    jmp 89f
85:
    mov edi, 15
89:
    mov eax, 60
    syscall
    ud2
90:
    .byte 0
91:
    .asciz \"/hello\"
    "
);

/// How long the registry waits for one run of [`FORK_WAIT`].
const RUN_MS: u64 = 4_000;

/// First ring-3 entries each run holds in the entry window: the
/// program's own and its first forked children's.
const HELD_ENTRIES: u32 = 4;

/// [`RUN_ST`] before the run's thread has published.
const PENDING: u32 = u32::MAX;

/// [`RUN_ST`] when the spawn failed.
const SPAWN_FAILED: u32 = u32::MAX - 1;

/// The status word of the current run, published by its thread.
static RUN_ST: AtomicU32 = AtomicU32::new(PENDING);

/// The body of the thread pinned to the CPU under test: spawn the
/// program there (`spawn_user` pins a process to its spawner's CPU),
/// wait for it, and publish the status as its last access.
fn fork_wait_runner() {
    let st = match user::spawn(&Image::Code(FORK_WAIT, DEFAULT), &["fork_wait"]) {
        Ok(pid) => user::wait(pid),
        Err(_) => SPAWN_FAILED,
    };
    RUN_ST.store(st, Ordering::Release);
}

/// The processes of a [`FORK_WAIT`] run that are not Dead, with their
/// state and CPU.
struct Stuck;

impl core::fmt::Display for Stuck {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut r = Ok(());
        thread_init::each_thread(|t| {
            if r.is_err() || !matches!(t.name, "fork_wait" | "user" | "/hello") {
                return;
            }
            let st = match t.state {
                ThreadState::Ready => "R",
                ThreadState::Running => "run",
                ThreadState::Sleeping { .. } => "S",
                ThreadState::Blocked { .. } => "B",
                ThreadState::Dead => return,
            };
            r = write!(f, " {}:{}{}@{}", t.id.raw(), t.name, st, t.cpu);
        });
        r
    }
}

/// Run [`FORK_WAIT`] from a thread pinned to `cpu`, with its first
/// [`HELD_ENTRIES`] ring-3 entries held in the window, waiting at most
/// [`RUN_MS`].
fn run_on(cpu: u32) -> Outcome {
    RUN_ST.store(PENDING, Ordering::Relaxed);
    entry_testing::arm_fork_wait_stall(HELD_ENTRIES);
    let out = wait_run(cpu);
    entry_testing::arm_fork_wait_stall(0);
    out
}

fn wait_run(cpu: u32) -> Outcome {
    crate::ktest::spawn_thread_on("fork_wait_run", fork_wait_runner, cpu);
    let t0 = time_init::uptime_ms();
    loop {
        let st = RUN_ST.load(Ordering::Acquire);
        match st {
            PENDING => {}
            0 => return Outcome::Ok,
            SPAWN_FAILED => return crate::fail_fmt!("spawn failed cpu{cpu}"),
            st => return crate::fail_fmt!("status {st:#x} cpu{cpu}"),
        }
        if time_init::uptime_ms().saturating_sub(t0) >= RUN_MS {
            return crate::fail_fmt!("stalled {RUN_MS} ms cpu{cpu}:{}", Stuck);
        }
        thread_init::sleep_ms(10);
    }
}

/// The assembly `/bin/tests`' fork, wait4, execve, fault and fork-bomb sequence
/// after `user: dup ok` finishes on every spawning CPU, the registry's and
/// a second one, while `syscall_init::testing::fork_wait_stall_point`
/// holds each run's first ring-3 entries in the window after GS is
/// loaded for ring 3 (ROADMAP §10.2, F021).
pub(crate) fn user_fork_wait_stall() -> Outcome {
    // The registry is pinned, so the hint is its CPU.
    let here = thread_init::current_cpu();
    let out = run_on(here);
    if !matches!(out, Outcome::Ok) {
        return out;
    }
    match crate::ktest::second_cpu() {
        Some(cpu) if cpu != here => run_on(cpu),
        _ => Outcome::Ok,
    }
}

// Fork a child that exits 0 and reap it as c1; fork again as c2. Exit 2 if
// c2 took c1's pid, 3 unless kill(c1, SIGCONT) is -ESRCH, 4 unless
// wait4(c1) is -ECHILD, 5 unless wait4(c2) returns c2, 6 if a fork or the
// first wait4 failed; else 0.
user_code!(
    PID_REUSE_PROG,
    "
    mov eax, 57
    syscall
    test rax, rax
    js 9f
    jnz 1f
    xor edi, edi
    mov eax, 60
    syscall
    ud2
1:
    mov r12, rax
    mov rdi, r12
    xor esi, esi
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    cmp rax, r12
    jne 9f
    mov eax, 57
    syscall
    test rax, rax
    js 9f
    jnz 2f
    xor edi, edi
    mov eax, 60
    syscall
    ud2
2:
    mov r13, rax
    mov edi, 2
    cmp r13, r12
    je 8f
    mov rdi, r12
    mov esi, 18
    mov eax, 62
    syscall
    mov edi, 3
    cmp rax, -3
    jne 8f
    mov rdi, r12
    xor esi, esi
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, 4
    cmp rax, -10
    jne 8f
    mov rdi, r13
    xor esi, esi
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, 5
    cmp rax, r13
    jne 8f
    xor edi, edi
8:
    mov eax, 60
    syscall
    ud2
9:
    mov edi, 6
    jmp 8b
    "
);

/// A reaped process's pid is not the next fork's (ROADMAP §10.4, F127):
/// after a child is reaped, a second fork gets a new pid, and `kill` and
/// `wait4` on the old one fail with `ESRCH` and `ECHILD`.
pub(crate) fn pid_not_reused_after_reap() -> Outcome {
    match user::run(&Image::Code(PID_REUSE_PROG, DEFAULT), &["pidreuse"]) {
        Ok(st) if st == wait_exited(0) => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("status {st:#x}"),
        Err(e) => crate::fail_fmt!("spawn: {}", e.as_str()),
    }
}
