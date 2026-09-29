//! In-guest tests for proc (kernel_tests only): ring-3 images, user entry and exec.
//! Rows: the list in crate::ktest.

use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vibeos::elf::ElfError;
use vibeos::fs::{FsError, O_CREAT, O_TRUNC, O_WRONLY};
use vibeos::kbd::DecodedKey;
use vibeos::paging::{PAGE_SIZE_4K, USER_MAP_END};
use vibeos::proc::{SIGSEGV, wait_exited, wait_signaled, wexitstatus, wifexited};
use vibeos::syscall::SYS_GETPID;
use vibeos::vectors;

use super::hooks as exec_testing;
use crate::addr_space_init::testing as as_testing;
use crate::apic_init;
use crate::console_init;
use crate::ktest::user::{self, DEFAULT, Image, Layout, user_code};
use crate::ktest::{Outcome, fid, sleep_until};
use crate::pmm_init;
use crate::proc_init;
use crate::syscall_init::testing as sc_testing;
use crate::thread_init;
use crate::time_init;
use crate::user_init::LoadError;

// exit(7) when getppid() is 0 (the kernel spawned it), else exit(1).
user_code!(
    EXIT7_IF_KERNEL_CHILD,
    "
    mov eax, 110
    syscall
    mov edi, 1
    mov ecx, 7
    test rax, rax
    cmovz edi, ecx
    mov eax, 60
    syscall
    ud2
    "
);

pub(crate) fn test_user_code_exit() -> Outcome {
    let before = crate::ktest::quiescent_free_frames();
    let st = match user::run(&Image::Code(EXIT7_IF_KERNEL_CHILD, DEFAULT), &["exit7"]) {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if st != wait_exited(7) {
        return crate::fail_fmt!("status {st:#x}, want exited 7");
    }
    if !user::frames_settle(before) {
        return crate::fail_fmt!("frame leak: {} -> {}", before, crate::ktest::free_frames());
    }
    Outcome::Ok
}

pub(crate) fn test_user_image_elf() -> Outcome {
    let elf: Vec<u8> = user::elf_bytes(&Image::Code(EXIT7_IF_KERNEL_CHILD, DEFAULT));
    // One 8 KiB leak per run, kernel_tests only: `Image::Elf` holds a
    // `'static` image.
    let elf: &'static [u8] = Vec::leak(elf);
    let st = match user::run(&Image::Elf(elf), &["exit7"]) {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if st != wait_exited(7) {
        return crate::fail_fmt!("status {st:#x}, want exited 7");
    }
    Outcome::Ok
}

// Loaded at 0x5000_0000 with a 0x2000-byte writable segment: check the
// page, store to offsets 0x800 and 0x1008, and exit with the sum of the
// two values read back (0x11 + 0x22 = 51). A wrong page exits 1; a
// missing page or write bit is SIGSEGV.
user_code!(
    LAYOUT_WRITES,
    "
    lea rax, [rip]
    and rax, -4096
    mov edi, 1
    cmp rax, 0x50000000
    jne 1f
    mov qword ptr [rax + 0x800], 0x11
    mov qword ptr [rax + 0x1008], 0x22
    mov rdi, qword ptr [rax + 0x800]
    add rdi, qword ptr [rax + 0x1008]
1:
    mov eax, 60
    syscall
    ud2
    "
);

pub(crate) fn test_user_code_layout() -> Outcome {
    let layout = Layout {
        vaddr: 0x5000_0000,
        memsz: Some(0x2000),
        writable: true,
    };
    let before = crate::ktest::quiescent_free_frames();
    let st = match user::run(&Image::Code(LAYOUT_WRITES, layout), &["layout"]) {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if st != wait_exited(51) {
        return crate::fail_fmt!("status {st:#x}, want exited 51");
    }
    if !user::frames_settle(before) {
        return crate::fail_fmt!("frame leak: {} -> {}", before, crate::ktest::free_frames());
    }
    Outcome::Ok
}

// fork(); the parent exits at once with the child's pid (100 if fork
// failed). The orphaned child spins on getppid() + sched_yield() (at most
// 100,000 times) until it reads 0, then exits 0 (1 on timeout).
user_code!(
    ORPHAN_FORK,
    "
    mov eax, 57
    syscall
    test rax, rax
    js 3f
    jz 1f
    mov rdi, rax
    mov eax, 60
    syscall
    ud2
1:
    mov ebx, 100000
2:
    mov eax, 110
    syscall
    test rax, rax
    jz 4f
    mov eax, 24
    syscall
    dec ebx
    jnz 2b
    mov edi, 1
    mov eax, 60
    syscall
    ud2
3:
    mov edi, 100
    mov eax, 60
    syscall
    ud2
4:
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

/// A 1 KiB `fmt::Write` sink for `proc_init::write_ps`.
struct PsBuf {
    buf: [u8; 1024],
    len: usize,
}

impl fmt::Write for PsBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let b = s.as_bytes();
        let end = self.len.checked_add(b.len()).ok_or(fmt::Error)?;
        let dst = self.buf.get_mut(self.len..end).ok_or(fmt::Error)?;
        dst.copy_from_slice(b);
        self.len = end;
        Ok(())
    }
}

impl PsBuf {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("<invalid utf-8>")
    }

    /// The `ps` line for `pid`, if listed.
    fn line_of(&self, pid: u32) -> Option<&str> {
        self.as_str().lines().find(|l| {
            l.strip_prefix("vibeOS: ps: ")
                .and_then(|r| r.split(' ').next())
                .and_then(|p| p.parse::<u32>().ok())
                == Some(pid)
        })
    }
}

fn ps() -> PsBuf {
    let mut b = PsBuf {
        buf: [0; 1024],
        len: 0,
    };
    proc_init::write_ps(&mut b);
    b
}

pub(crate) fn test_orphan_freed_no_init() -> Outcome {
    let before = crate::ktest::quiescent_free_frames();
    let st = match user::run(&Image::Code(ORPHAN_FORK, DEFAULT), &["orphan"]) {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if !wifexited(st) {
        return crate::fail_fmt!("parent status {st:#x}, want exited");
    }
    let child = wexitstatus(st);
    if child == 100 {
        return Outcome::Fail("fork failed");
    }
    let deadline = time_init::now_ns().saturating_add(5_000_000_000);
    loop {
        let snap = ps();
        let Some(line) = snap.line_of(child) else {
            break;
        };
        if time_init::now_ns() >= deadline {
            return crate::fail_fmt!("child still listed: {line}");
        }
        thread_init::yield_now();
    }
    if !user::frames_settle(before) {
        return crate::fail_fmt!("frame leak: {} -> {}", before, crate::ktest::free_frames());
    }
    Outcome::Ok
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
pub(crate) fn test_console_read_exit() -> Outcome {
    console_init::testing::reset_halts();
    let pid = match user::spawn(&Image::Code(READ_ONE_KEY, DEFAULT), &["read_one_key"]) {
        Ok(pid) => pid,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    let halted = sleep_until(|| console_init::testing::halts() != 0, 5_000);
    crate::console::ktest::push_for_test(DecodedKey::Char(b'k'));
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

/// Forked processes enter ring 3 through `syscall_init::first_return` on CPU 0 while
/// another CPU keeps sending it reschedule IPIs; an interrupt taken there
/// with the user GS loaded halts the kernel, and the debug-build check
/// before the selector loads faults if IF is set.
pub(crate) fn test_user_entry_irq() -> Outcome {
    let Some(sender_cpu) = crate::ktest::second_cpu() else {
        return Outcome::Skip("needs 2 CPUs");
    };
    ENTRY_SPAWNED.store(false, Ordering::Release);
    ENTRY_STOP.store(false, Ordering::Release);
    ENTRY_SENDER_DONE.store(false, Ordering::Release);
    crate::ktest::spawn_thread_on("entry_ipi_sender", entry_ipi_sender, sender_cpu);
    crate::ktest::spawn_thread_on("entry_spawner", entry_spawner, 0);
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
pub(crate) fn test_exec_top_page_enoexec() -> Outcome {
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
pub(crate) fn test_noncanonical_rip_sigsegv() -> Outcome {
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

/// Write `bytes` to `path`, creating or truncating it.
fn write_file(path: &str, bytes: &[u8]) -> Result<(), FsError> {
    let f = fid::open(path, O_WRONLY | O_CREAT | O_TRUNC, 0o644)?;
    let mut done = 0usize;
    let rc = loop {
        if done == bytes.len() {
            break Ok(());
        }
        match fid::write(f, &bytes[done..]) {
            Ok(0) => break Err(FsError::Io),
            Ok(n) => done += n,
            Err(e) => break Err(e),
        }
    };
    let closed = fid::close(f);
    rc.and(closed)
}

fn unlink_quiet(path: &str) -> Result<(), FsError> {
    match fid::unlink_path(path, false) {
        Ok(()) | Err(FsError::NotFound) => Ok(()),
        Err(e) => Err(e),
    }
}

// exit(99): what a huge image runs if its load succeeds.
user_code!(
    EXIT99,
    "
    mov edi, 99
    mov eax, 60
    syscall
    ud2
    "
);

// execve(argv[1]) twice and execve(argv[2]) once, each with argv and envp
// 0; each must return -ENOMEM (exit 1, 2, 3). Then fork (exit 4 on
// failure; the child exits 0), wait4 with a status word on the stack
// (exit 5 on a wrong pid, 6 on a non-zero status), and exit 0.
user_code!(
    EXEC_HUGE,
    "
    mov r12, [rsp + 16]
    mov r13, [rsp + 24]
    mov rdi, r12
    xor esi, esi
    xor edx, edx
    mov eax, 59
    syscall
    mov edi, 1
    cmp rax, -12
    jne 9f
    mov rdi, r12
    xor esi, esi
    xor edx, edx
    mov eax, 59
    syscall
    mov edi, 2
    cmp rax, -12
    jne 9f
    mov rdi, r13
    xor esi, esi
    xor edx, edx
    mov eax, 59
    syscall
    mov edi, 3
    cmp rax, -12
    jne 9f
    mov eax, 57
    syscall
    mov edi, 4
    test rax, rax
    js 9f
    jz 8f
    mov r14, rax
    sub rsp, 16
    mov dword ptr [rsp], -1
    mov rdi, r14
    mov rsi, rsp
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, 5
    cmp rax, r14
    jne 9f
    mov edi, 6
    cmp dword ptr [rsp], 0
    jne 9f
8:
    xor edi, edi
9:
    mov eax, 60
    syscall
    ud2
    "
);

const MEMSZ_64G: &str = "/vibe/s18_memsz_64g";

const MEMSZ_192M: &str = "/vibe/s18_memsz_192m";

/// ROADMAP §10.6 (F009): an image with a 64 GiB `p_memsz` (over the cap)
/// and one with 192 MiB (under it, but over the guest's RAM) each get
/// `ENOMEM`; the warm 64 GiB load and the 192 MiB load leave the free-frame
/// count where they found it; the loader held `PT` for at most one leaf
/// table at a time; and a `fork` after them succeeds.
pub(crate) fn test_exec_huge_memsz() -> Outcome {
    let total = pmm_init::with_buddy(|b| b.stats().total_frames) as u64;
    if total * PAGE_SIZE_4K >= 192 << 20 {
        return Outcome::Skip("guest RAM >= 192 MiB");
    }
    let huge = |memsz: u64| {
        Image::Code(
            EXIT99,
            Layout {
                vaddr: 0x4000_0000,
                memsz: Some(memsz),
                writable: false,
            },
        )
    };
    for (path, memsz) in [(MEMSZ_64G, 64u64 << 30), (MEMSZ_192M, 192 << 20)] {
        if let Err(e) = write_file(path, &user::elf_bytes(&huge(memsz))) {
            return crate::fail_fmt!("write {path}: {}", e.as_str());
        }
    }
    // Each load compares the buddy count at its entry and return, so no
    // dead thread's stack an earlier test left may reach the buddy between.
    if !crate::ktest::settle_threads() {
        return Outcome::Fail("threads did not settle");
    }
    as_testing::reset_chunks();
    exec_testing::clear_exec_frames();
    let rc = user::run(
        &Image::Code(EXEC_HUGE, user::DEFAULT),
        &["exec_huge", MEMSZ_64G, MEMSZ_192M],
    );
    let recs: Vec<exec_testing::ExecFrames> = exec_testing::exec_frames();
    let (holds, max_pages) = as_testing::chunk_stats();
    for path in [MEMSZ_64G, MEMSZ_192M] {
        if let Err(e) = unlink_quiet(path) {
            return crate::fail_fmt!("unlink {path}: {}", e.as_str());
        }
    }
    let st = match rc {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if st != 0 {
        return crate::fail_fmt!("status {st:#x}, want 0");
    }
    if recs.len() != 3 || recs.iter().any(|r| r.ok) {
        return crate::fail_fmt!("{} loads recorded, want 3 failed", recs.len());
    }
    for (i, r) in recs.iter().enumerate().skip(1) {
        if r.after != r.before {
            return crate::fail_fmt!("load {i}: free frames {} -> {}", r.before, r.after);
        }
    }
    if holds < 16 || max_pages > 512 {
        return crate::fail_fmt!("PT holds {holds}, max {max_pages} pages per hold");
    }
    Outcome::Ok
}

// brk(0) = b, non-zero and page-aligned (1). brk(b+0x3010) returns it (2),
// and the new pages read 0 (3); a store and load at b and b+0x3008 (4).
// brk(b+0x1000) returns it (5); brk(b-0x1000) (6) and brk(b+0x4000_0000),
// into the stack (7), return b+0x1000. fork (8): the child exits 10 unless
// its brk(0) is b+0x1000 and 11 unless it reads b's marker; the parent
// exits 12 on a wrong wait4 pid and 13 on a non-zero status.
user_code!(
    BRK_RW,
    "
    xor edi, edi
    mov eax, 12
    syscall
    mov edi, 1
    test rax, rax
    jz 9f
    test eax, 0xfff
    jnz 9f
    mov r12, rax
    lea rdi, [r12 + 0x3010]
    mov r13, rdi
    mov eax, 12
    syscall
    mov edi, 2
    cmp rax, r13
    jne 9f
    mov edi, 3
    cmp qword ptr [r12], 0
    jne 9f
    cmp qword ptr [r12 + 0x1000], 0
    jne 9f
    cmp qword ptr [r12 + 0x2000], 0
    jne 9f
    cmp qword ptr [r12 + 0x3008], 0
    jne 9f
    movabs rax, 0x1122334455667788
    mov [r12], rax
    mov [r12 + 0x3008], rax
    mov edi, 4
    cmp [r12], rax
    jne 9f
    cmp [r12 + 0x3008], rax
    jne 9f
    lea rdi, [r12 + 0x1000]
    mov r13, rdi
    mov eax, 12
    syscall
    mov edi, 5
    cmp rax, r13
    jne 9f
    lea rdi, [r12 - 0x1000]
    mov eax, 12
    syscall
    mov edi, 6
    cmp rax, r13
    jne 9f
    lea rdi, [r12 + 0x40000000]
    mov eax, 12
    syscall
    mov edi, 7
    cmp rax, r13
    jne 9f
    mov eax, 57
    syscall
    mov edi, 8
    test rax, rax
    js 9f
    jnz 2f
    xor edi, edi
    mov eax, 12
    syscall
    mov edi, 10
    cmp rax, r13
    jne 9f
    movabs rax, 0x1122334455667788
    mov edi, 11
    cmp [r12], rax
    jne 9f
    xor edi, edi
    jmp 9f
2:
    mov r14, rax
    sub rsp, 16
    mov dword ptr [rsp], -1
    mov rdi, r14
    mov rsi, rsp
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, 12
    cmp rax, r14
    jne 9f
    mov edi, 13
    cmp dword ptr [rsp], 0
    jne 9f
    xor edi, edi
9:
    mov eax, 60
    syscall
    ud2
    "
);

// Grow the heap to b+0x3000, shrink it to b+0x1000, load b+0x2000: SIGSEGV.
// Exit 1 or 2 on a wrong brk return, 3 if the load did not fault.
user_code!(
    BRK_SHRUNK_TOUCH,
    "
    xor edi, edi
    mov eax, 12
    syscall
    mov r12, rax
    lea rdi, [r12 + 0x3000]
    mov r13, rdi
    mov eax, 12
    syscall
    mov edi, 1
    cmp rax, r13
    jne 9f
    lea rdi, [r12 + 0x1000]
    mov r13, rdi
    mov eax, 12
    syscall
    mov edi, 2
    cmp rax, r13
    jne 9f
    mov rax, [r12 + 0x2000]
    mov edi, 3
9:
    mov eax, 60
    syscall
    ud2
    "
);

// mmap(0, 0x4000, RW, PRIVATE|ANON, -1, 0) = a, page-aligned (1), with
// a+0x4000 <= 0x7FFF_F7FF_F000 (2); it reads 0 (3); a store and load (4).
// A second call lands below a (5). MAP_FIXED at a free address returns it
// (6). MAP_FIXED_NOREPLACE at a returns -17 (7). munmap(a, 0x4000) returns
// 0 (8).
user_code!(
    MMAP_RW,
    "
    xor edi, edi
    mov esi, 0x4000
    mov edx, 3
    mov r10d, 0x22
    mov r8, -1
    xor r9d, r9d
    mov eax, 9
    syscall
    mov r12, rax
    mov edi, 1
    test eax, 0xfff
    jnz 9f
    lea rax, [r12 + 0x4000]
    movabs rcx, 0x7FFFF7FFF000
    mov edi, 2
    cmp rax, rcx
    ja 9f
    mov edi, 3
    cmp qword ptr [r12], 0
    jne 9f
    cmp qword ptr [r12 + 0x3ff8], 0
    jne 9f
    mov qword ptr [r12 + 0x3ff8], 77
    mov edi, 4
    cmp qword ptr [r12 + 0x3ff8], 77
    jne 9f
    xor edi, edi
    mov esi, 0x1000
    mov edx, 3
    mov r10d, 0x22
    mov r8, -1
    xor r9d, r9d
    mov eax, 9
    syscall
    mov edi, 5
    test eax, 0xfff
    jnz 9f
    cmp rax, r12
    jae 9f
    mov edi, 0x50000000
    mov esi, 0x1000
    mov edx, 3
    mov r10d, 0x32
    mov r8, -1
    xor r9d, r9d
    mov eax, 9
    syscall
    mov edi, 6
    cmp rax, 0x50000000
    jne 9f
    mov rdi, r12
    mov esi, 0x1000
    mov edx, 3
    mov r10d, 0x100022
    mov r8, -1
    xor r9d, r9d
    mov eax, 9
    syscall
    mov edi, 7
    cmp rax, -17
    jne 9f
    mov rdi, r12
    mov esi, 0x4000
    mov eax, 11
    syscall
    mov edi, 8
    test rax, rax
    jnz 9f
    xor edi, edi
9:
    mov eax, 60
    syscall
    ud2
    "
);

// Store into a PROT_READ page: SIGSEGV. Exit 1 if mmap failed, 2 if the
// store did not fault.
user_code!(
    MMAP_RO_WRITE,
    "
    xor edi, edi
    mov esi, 0x1000
    mov edx, 1
    mov r10d, 0x22
    mov r8, -1
    xor r9d, r9d
    mov eax, 9
    syscall
    mov edi, 1
    test eax, 0xfff
    jnz 9f
    mov byte ptr [rax], 1
    mov edi, 2
9:
    mov eax, 60
    syscall
    ud2
    "
);

// Map and mark 3 pages (1), unmap the middle one (2), fork (3). The child
// exits 10 or 11 unless pages 1 and 3 hold their marks; the parent exits
// 4 on a wrong wait4 pid and 5 on a non-zero status.
user_code!(
    MUNMAP_SPLIT_FORK,
    "
    xor edi, edi
    mov esi, 0x3000
    mov edx, 3
    mov r10d, 0x22
    mov r8, -1
    xor r9d, r9d
    mov eax, 9
    syscall
    mov r12, rax
    mov edi, 1
    test eax, 0xfff
    jnz 9f
    mov qword ptr [r12], 11
    mov qword ptr [r12 + 0x1000], 22
    mov qword ptr [r12 + 0x2000], 33
    lea rdi, [r12 + 0x1000]
    mov esi, 0x1000
    mov eax, 11
    syscall
    mov edi, 2
    test rax, rax
    jnz 9f
    mov eax, 57
    syscall
    mov edi, 3
    test rax, rax
    js 9f
    jnz 2f
    mov edi, 10
    cmp qword ptr [r12], 11
    jne 9f
    mov edi, 11
    cmp qword ptr [r12 + 0x2000], 33
    jne 9f
    xor edi, edi
    jmp 9f
2:
    mov r14, rax
    sub rsp, 16
    mov dword ptr [rsp], -1
    mov rdi, r14
    mov rsi, rsp
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, 4
    cmp rax, r14
    jne 9f
    mov edi, 5
    cmp dword ptr [rsp], 0
    jne 9f
    xor edi, edi
9:
    mov eax, 60
    syscall
    ud2
    "
);

// Map (1), store, unmap (2), load: SIGSEGV. Exit 3 if the load did not
// fault.
user_code!(
    MUNMAP_TOUCH,
    "
    xor edi, edi
    mov esi, 0x1000
    mov edx, 3
    mov r10d, 0x22
    mov r8, -1
    xor r9d, r9d
    mov eax, 9
    syscall
    mov r12, rax
    mov edi, 1
    test eax, 0xfff
    jnz 9f
    mov qword ptr [r12], 5
    mov rdi, r12
    mov esi, 0x1000
    mov eax, 11
    syscall
    mov edi, 2
    test rax, rax
    jnz 9f
    mov rax, [r12]
    mov edi, 3
9:
    mov eax, 60
    syscall
    ud2
    "
);

// mmap with PROT_NONE (1), then load the address: SIGSEGV. Exit 2 if the
// load did not fault.
user_code!(
    PROT_NONE_TOUCH,
    "
    xor edi, edi
    mov esi, 0x1000
    xor edx, edx
    mov r10d, 0x22
    mov r8, -1
    xor r9d, r9d
    mov eax, 9
    syscall
    mov edi, 1
    test eax, 0xfff
    jnz 9f
    mov rax, [rax]
    mov edi, 2
9:
    mov eax, 60
    syscall
    ud2
    "
);

// Opens argv[1] (exit 16 on failure) and checks, exiting with the number of
// the first that fails: mmap returns -22 for a len of 0 (1), no map type
// (2), MAP_SHARED|ANON (3), an off of 1 (4), MAP_FIXED at 0x4000_0001 (5),
// MAP_GROWSDOWN (6), and prot 8 (7); a file mapping returns -9 with fd -1
// (8) and -19 with the open fd (9); a len of 1<<47 returns -12 (10);
// MAP_FIXED at 0 returns -1 (11); munmap returns -22 for an unaligned addr
// (12), a len of 0 (13), and a range past USER_MAP_END (14), and 0 for a
// free range (15).
user_code!(
    MMAP_ERRORS,
    "
    mov rdi, [rsp + 16]
    xor esi, esi
    xor edx, edx
    mov eax, 2
    syscall
    mov edi, 16
    test rax, rax
    js 9f
    mov r15, rax
    xor edi, edi
    xor esi, esi
    mov edx, 3
    mov r10d, 0x22
    mov r8, -1
    xor r9d, r9d
    mov eax, 9
    syscall
    mov edi, 1
    cmp rax, -22
    jne 9f
    xor edi, edi
    mov esi, 0x1000
    mov edx, 3
    mov r10d, 0x20
    mov eax, 9
    syscall
    mov edi, 2
    cmp rax, -22
    jne 9f
    xor edi, edi
    mov esi, 0x1000
    mov edx, 3
    mov r10d, 0x21
    mov eax, 9
    syscall
    mov edi, 3
    cmp rax, -22
    jne 9f
    xor edi, edi
    mov esi, 0x1000
    mov edx, 3
    mov r10d, 0x22
    mov r9d, 1
    mov eax, 9
    syscall
    mov edi, 4
    cmp rax, -22
    jne 9f
    xor r9d, r9d
    mov edi, 0x40000001
    mov esi, 0x1000
    mov edx, 3
    mov r10d, 0x32
    mov eax, 9
    syscall
    mov edi, 5
    cmp rax, -22
    jne 9f
    xor edi, edi
    mov esi, 0x1000
    mov edx, 3
    mov r10d, 0x122
    mov eax, 9
    syscall
    mov edi, 6
    cmp rax, -22
    jne 9f
    xor edi, edi
    mov esi, 0x1000
    mov edx, 8
    mov r10d, 0x22
    mov eax, 9
    syscall
    mov edi, 7
    cmp rax, -22
    jne 9f
    xor edi, edi
    mov esi, 0x1000
    mov edx, 1
    mov r10d, 0x2
    mov r8, -1
    mov eax, 9
    syscall
    mov edi, 8
    cmp rax, -9
    jne 9f
    xor edi, edi
    mov esi, 0x1000
    mov edx, 1
    mov r10d, 0x2
    mov r8, r15
    mov eax, 9
    syscall
    mov edi, 9
    cmp rax, -19
    jne 9f
    mov r8, -1
    xor edi, edi
    movabs rsi, 0x800000000000
    mov edx, 3
    mov r10d, 0x22
    mov eax, 9
    syscall
    mov edi, 10
    cmp rax, -12
    jne 9f
    xor edi, edi
    mov esi, 0x1000
    mov edx, 3
    mov r10d, 0x32
    mov eax, 9
    syscall
    mov edi, 11
    cmp rax, -1
    jne 9f
    mov edi, 0x40000001
    mov esi, 0x1000
    mov eax, 11
    syscall
    mov edi, 12
    cmp rax, -22
    jne 9f
    mov edi, 0x40000000
    xor esi, esi
    mov eax, 11
    syscall
    mov edi, 13
    cmp rax, -22
    jne 9f
    movabs rdi, 0x7FFFFFFFF000
    mov esi, 0x2000
    mov eax, 11
    syscall
    mov edi, 14
    cmp rax, -22
    jne 9f
    mov edi, 0x60000000
    mov esi, 0x1000
    mov eax, 11
    syscall
    mov edi, 15
    test rax, rax
    jnz 9f
    xor edi, edi
9:
    mov eax, 60
    syscall
    ud2
    "
);

const S18_FILE: &str = "/vibe/s18_file";

/// ROADMAP §10.5: `brk` and anonymous `mmap`/`munmap` as Linux's brk(2)
/// and mmap(2) define them, eagerly backed. Each program exits with the
/// number of its first failed check, or dies of `SIGSEGV` where it touches
/// a page it may not.
pub(crate) fn test_brk_mmap_munmap_user() -> Outcome {
    if let Err(e) = write_file(S18_FILE, b"s18 file mapping\n") {
        return crate::fail_fmt!("write {S18_FILE}: {}", e.as_str());
    }
    let segv = wait_signaled(SIGSEGV) & 0x7f;
    let progs: [(&str, &'static [u8], bool); 8] = [
        ("brk_rw", BRK_RW, false),
        ("brk_shrunk_touch", BRK_SHRUNK_TOUCH, true),
        ("mmap_rw", MMAP_RW, false),
        ("mmap_ro_write", MMAP_RO_WRITE, true),
        ("munmap_split_fork", MUNMAP_SPLIT_FORK, false),
        ("munmap_touch", MUNMAP_TOUCH, true),
        ("prot_none_touch", PROT_NONE_TOUCH, true),
        ("mmap_errors", MMAP_ERRORS, false),
    ];
    let mut out = Outcome::Ok;
    for (name, code, faults) in progs {
        let st = match user::run(&Image::Code(code, user::DEFAULT), &[name, S18_FILE]) {
            Ok(st) => st,
            Err(e) => {
                out = crate::fail_fmt!("{name}: spawn: {}", e.as_str());
                break;
            }
        };
        let good = if faults { st & 0x7f == segv } else { st == 0 };
        if !good {
            let want = if faults { "SIGSEGV" } else { "exit 0" };
            out = crate::fail_fmt!("{name}: status {st:#x} (exit {}), want {want}", st >> 8);
            break;
        }
    }
    if let Err(e) = unlink_quiet(S18_FILE) {
        return crate::fail_fmt!("unlink {S18_FILE}: {}", e.as_str());
    }
    out
}
