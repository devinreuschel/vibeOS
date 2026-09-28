//! In-guest tests of P10-S17, Syscall exit IF=0, enter_user_full, USER_MAP_END and the FP binding (DESIGN §8.2).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vibeos::elf::ElfError;
use vibeos::kbd::DecodedKey;
use vibeos::paging::{PAGE_SIZE_4K, USER_MAP_END};
use vibeos::proc::{SIGSEGV, wait_signaled};
use vibeos::syscall::SYS_GETPID;
use vibeos::vectors;

use super::user::{self, DEFAULT, Image, Layout, user_code};
use super::{Outcome, Test, test};
use crate::apic_init;
use crate::console_init;
use crate::kbd_init;
use crate::syscall_init::testing as sc_testing;
use crate::thread_init;
use crate::time_init;
use crate::user_init::LoadError;

pub(super) const TESTS: &[Test] = &[
    test("console_read_exit", test_console_read_exit).deadline(30_000),
    test("user_entry_irq", test_user_entry_irq).deadline(120_000),
    test("exec_top_page_enoexec", test_exec_top_page_enoexec).deadline(30_000),
    test("noncanonical_rip_sigsegv", test_noncanonical_rip_sigsegv).deadline(30_000),
];

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

// read(0, rsp, 1), then exit with the byte read; exit(2) if read does not
// return 1.
user_code!(
    READ_ONE_KEY,
    "
    sub rsp, 16
    xor edi, edi
    mov rsi, rsp
    mov edx, 1
    xor eax, eax
    syscall
    cmp rax, 1
    jne 1f
    movzx edi, byte ptr [rsp]
    mov eax, 60
    syscall
1:
    mov edi, 2
    mov eax, 60
    syscall
    ud2
    "
);

/// A user `read` on fd 0 blocks in `console_init::wait_key`'s halt branch
/// until a key is queued, then returns through the syscall exit, whose
/// debug-build check faults if `wait_key` left IF set.
fn test_console_read_exit() -> Outcome {
    console_init::testing::reset_halts();
    let pid = match user::spawn(&Image::Code(READ_ONE_KEY, DEFAULT), &["read_one_key"]) {
        Ok(pid) => pid,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    let halted = sleep_until(|| console_init::testing::halts() != 0, 5_000);
    kbd_init::push_for_test(DecodedKey::Char(b'k'));
    let st = user::wait(pid);
    if !halted {
        return Outcome::Fail("reader never reached wait_key's halt");
    }
    if st != 0x6B << 8 {
        return crate::fail_fmt!("status {st:#x}, want {:#x}", 0x6B << 8);
    }
    Outcome::Ok
}

// 1,000 times: fork; the child exits 0 and the parent waits for it. Exit
// 0, or 2 on a negative return.
user_code!(
    FORK_1000,
    "
    mov r12d, 1000
2:
    mov eax, 57
    syscall
    test rax, rax
    js 9f
    jz 8f
    mov rdi, rax
    xor esi, esi
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    test rax, rax
    js 9f
    dec r12d
    jnz 2b
    xor edi, edi
    mov eax, 60
    syscall
8:
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

static ENTRY_CHILD: AtomicU64 = AtomicU64::new(0);
static ENTRY_SPAWNED: AtomicBool = AtomicBool::new(false);
static ENTRY_STOP: AtomicBool = AtomicBool::new(false);
static ENTRY_SENDER_DONE: AtomicBool = AtomicBool::new(false);

/// Pinned to CPU 0, so the process and its fork children are too
/// (`thread_init::spawn_user`).
fn entry_spawner() {
    let pid = match user::spawn(&Image::Code(FORK_1000, DEFAULT), &["fork_1000"]) {
        Ok(pid) => u64::from(pid),
        Err(_) => u64::MAX,
    };
    ENTRY_CHILD.store(pid, Ordering::Relaxed);
    ENTRY_SPAWNED.store(true, Ordering::Release);
}

/// Reschedule IPIs to CPU 0 about every 20 us until `ENTRY_STOP`.
fn entry_ipi_sender() {
    while !ENTRY_STOP.load(Ordering::Acquire) {
        // A failed send only thins the IPI stream.
        let _ = apic_init::send_ipi_cpu(0, vectors::IPI_RESCHEDULE);
        let t = time_init::now_ns().saturating_add(20_000);
        while time_init::now_ns() < t {
            core::hint::spin_loop();
        }
    }
    ENTRY_SENDER_DONE.store(true, Ordering::Release);
}

/// Forked processes enter ring 3 through `enter_user_full` on CPU 0 while
/// another CPU keeps sending it reschedule IPIs; an interrupt taken there
/// with the user GS loaded halts the kernel, and the debug-build check
/// before the selector loads faults if IF is set.
fn test_user_entry_irq() -> Outcome {
    let Some(sender_cpu) = super::second_cpu() else {
        return Outcome::Skip("needs 2 CPUs");
    };
    ENTRY_SPAWNED.store(false, Ordering::Release);
    ENTRY_STOP.store(false, Ordering::Release);
    ENTRY_SENDER_DONE.store(false, Ordering::Release);
    super::spawn_thread_on("entry_ipi_sender", entry_ipi_sender, sender_cpu);
    super::spawn_thread_on("entry_spawner", entry_spawner, 0);
    let spawned = sleep_until(|| ENTRY_SPAWNED.load(Ordering::Acquire), 5_000);
    let pid = u32::try_from(ENTRY_CHILD.load(Ordering::Relaxed)).ok();
    let st = if spawned { pid.map(user::wait) } else { None };
    ENTRY_STOP.store(true, Ordering::Release);
    let stopped = sleep_until(|| ENTRY_SENDER_DONE.load(Ordering::Acquire), 5_000);
    if !spawned {
        return Outcome::Fail("spawner did not run");
    }
    let Some(st) = st else {
        return Outcome::Fail("spawn");
    };
    if !stopped {
        return Outcome::Fail("IPI sender did not stop");
    }
    if st != 0 {
        return crate::fail_fmt!("status {st:#x}, want 0");
    }
    Outcome::Ok
}

// getpid, then a jmp to a `syscall` in the page's last two bytes, whose
// return RIP is the first byte past the page.
user_code!(
    SYSCALL_AT_PAGE_END,
    "
    mov eax, 39
    jmp 1f
    .org vibeos_user_code_SYSCALL_AT_PAGE_END + 4094, 0xcc
1:
    syscall
    "
);

/// An image whose last page is the one below `USER_END` does not load:
/// its `syscall` would return to a non-canonical RIP. The same code one
/// page lower loads, and its `syscall` returns to the unmapped top page.
fn test_exec_top_page_enoexec() -> Outcome {
    let at = |vaddr| Image::Code(SYSCALL_AT_PAGE_END, Layout { vaddr, ..DEFAULT });
    match user::spawn(&at(USER_MAP_END), &["top_page"]) {
        Err(LoadError::Elf(ElfError::KernelVa)) => {}
        Err(e) => return crate::fail_fmt!("top page: {}, want kernel va", e.as_str()),
        Ok(pid) => {
            let st = user::wait(pid);
            return crate::fail_fmt!("top page loaded, status {st:#x}");
        }
    }
    let st = match user::run(&at(USER_MAP_END - PAGE_SIZE_4K), &["below_top"]) {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("page below: spawn: {}", e.as_str()),
    };
    if st != wait_signaled(SIGSEGV) {
        return crate::fail_fmt!("page below: status {st:#x}, want SIGSEGV");
    }
    Outcome::Ok
}

// getpid, then exit(0).
user_code!(
    GETPID_EXIT0,
    "
    mov eax, 39
    syscall
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

// exit(0).
user_code!(
    EXIT0,
    "
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

/// A syscall whose saved RIP a hook makes non-canonical kills the process
/// with `SIGSEGV` on the exit's own path, before `swapgs`; a process whose
/// first entry `iretq`s to a non-canonical RIP gets `SIGSEGV` too (on KVM
/// from the labeled `iretq`'s `#GP`, on TCG from the fetch).
fn test_noncanonical_rip_sigsegv() -> Outcome {
    let kills = sc_testing::bad_rip_kills();
    sc_testing::arm_noncanonical_rip(SYS_GETPID);
    let st = user::run(&Image::Code(GETPID_EXIT0, DEFAULT), &["bad_rip_exit"]);
    sc_testing::disarm_noncanonical();
    let st = match st {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("exit case: spawn: {}", e.as_str()),
    };
    if st != wait_signaled(SIGSEGV) {
        return crate::fail_fmt!("exit case: status {st:#x}, want SIGSEGV");
    }
    let got = sc_testing::bad_rip_kills().wrapping_sub(kills);
    if got != 1 {
        return crate::fail_fmt!("exit case: {got} bad-RIP kills, want 1");
    }
    sc_testing::arm_noncanonical_entry();
    let st = user::run(&Image::Code(EXIT0, DEFAULT), &["bad_rip_entry"]);
    sc_testing::disarm_noncanonical();
    let st = match st {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("entry case: spawn: {}", e.as_str()),
    };
    if st != wait_signaled(SIGSEGV) {
        return crate::fail_fmt!("entry case: status {st:#x}, want SIGSEGV");
    }
    Outcome::Ok
}
