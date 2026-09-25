//! In-guest tests of P10-S01, Ring-3 entry model and in-memory user programs (DESIGN §8.2).

use alloc::vec::Vec;

use vibeos::proc::wait_exited;

use super::user::{self, DEFAULT, Image, Layout, user_code};
use super::{Outcome, Test, test};

pub(super) const TESTS: &[Test] = &[
    test("user_code_exit", test_user_code_exit),
    test("user_image_elf", test_user_image_elf),
    test("user_code_layout", test_user_code_layout),
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
