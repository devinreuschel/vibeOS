//! In-guest tests of P10-S18, Exec image cap and anonymous memory syscalls (DESIGN §8.2).

use alloc::vec::Vec;

use vibeos::fs::{FsError, O_CREAT, O_TRUNC, O_WRONLY};
use vibeos::paging::PAGE_SIZE_4K;
use vibeos::proc::{SIGSEGV, wait_signaled};

use super::Outcome;
use super::p10_s12::fid;
use super::user::{self, Image, Layout, user_code};
use crate::addr_space_init::testing as as_testing;
use crate::pmm_init;
use crate::user_init::testing as exec_testing;

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
pub(super) fn test_exec_huge_memsz() -> Outcome {
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
    if !super::settle_threads() {
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
pub(super) fn test_brk_mmap_munmap_user() -> Outcome {
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
