//! In-guest tests of P10-S18, Exec image cap and anonymous memory syscalls (DESIGN §8.2).

use alloc::vec::Vec;

use vibeos::fs::{FsError, O_CREAT, O_TRUNC, O_WRONLY};
use vibeos::paging::PAGE_SIZE_4K;

use super::p10_s12::fid;
use super::user::{self, Image, Layout, user_code};
use super::{Outcome, Test, test};
use crate::addr_space_init::testing as as_testing;
use crate::pmm_init;
use crate::user_init::testing as exec_testing;

pub(super) const TESTS: &[Test] = &[test("exec_huge_memsz", test_exec_huge_memsz).deadline(60_000)];

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
fn test_exec_huge_memsz() -> Outcome {
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
