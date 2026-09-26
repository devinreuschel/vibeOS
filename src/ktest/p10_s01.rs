//! In-guest tests of P10-S01, Ring-3 entry model and in-memory user programs (DESIGN §8.2).

use alloc::vec::Vec;
use core::fmt;

use vibeos::proc::{wait_exited, wexitstatus, wifexited};

use super::user::{self, DEFAULT, Image, Layout, user_code};
use super::{Outcome, Test, test};
use crate::proc_init;
use crate::thread_init;
use crate::time_init;

pub(super) const TESTS: &[Test] = &[
    test("user_code_exit", test_user_code_exit),
    test("user_image_elf", test_user_image_elf),
    test("user_code_layout", test_user_code_layout),
    test("orphan_freed_no_init", test_orphan_freed_no_init),
];

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

fn test_user_code_exit() -> Outcome {
    let before = super::free_frames();
    let st = match user::run(&Image::Code(EXIT7_IF_KERNEL_CHILD, DEFAULT), &["exit7"]) {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if st != wait_exited(7) {
        return crate::fail_fmt!("status {st:#x}, want exited 7");
    }
    if !user::frames_settle(before) {
        return crate::fail_fmt!("frame leak: {} -> {}", before, super::free_frames());
    }
    Outcome::Ok
}

fn test_user_image_elf() -> Outcome {
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

fn test_user_code_layout() -> Outcome {
    let layout = Layout {
        vaddr: 0x5000_0000,
        memsz: Some(0x2000),
        writable: true,
    };
    let before = super::free_frames();
    let st = match user::run(&Image::Code(LAYOUT_WRITES, layout), &["layout"]) {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if st != wait_exited(51) {
        return crate::fail_fmt!("status {st:#x}, want exited 51");
    }
    if !user::frames_settle(before) {
        return crate::fail_fmt!("frame leak: {} -> {}", before, super::free_frames());
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

fn test_orphan_freed_no_init() -> Outcome {
    let before = super::free_frames();
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
        return crate::fail_fmt!("frame leak: {} -> {}", before, super::free_frames());
    }
    Outcome::Ok
}
