//! In-guest tests of P10-S09, FAT inode, open-file table generations, vibefs size limits (DESIGN §8.2).

use vibeos::fs::{FsError, O_RDONLY};
use vibeos::proc::wait_exited;

use super::user::{self, DEFAULT, Image, user_code};
use super::{Outcome, Test, test};
use crate::file_init;

pub(super) const TESTS: &[Test] = &[test("file_table_fork_churn", test_file_table_fork_churn)];

/// Read up to `out.len()` bytes of `path` from offset 0; the count read.
fn read_all(path: &str, out: &mut [u8]) -> Result<usize, FsError> {
    let fid = file_init::open(path, O_RDONLY, 0)?;
    let mut n = 0usize;
    let r = loop {
        let Some(rest) = out.get_mut(n..) else {
            break Ok(n);
        };
        if rest.is_empty() {
            break Ok(n);
        }
        match file_init::read(fid, rest) {
            Ok(0) => break Ok(n),
            Ok(k) => n = n.saturating_add(k),
            Err(e) => break Err(e),
        }
    };
    let c = file_init::close(fid);
    let n = r?;
    c?;
    Ok(n)
}

/// Unlink `path`; a missing file is not an error.
fn unlink_quiet(path: &str) -> Result<(), FsError> {
    match file_init::unlink_path(path, false) {
        Ok(()) | Err(FsError::NotFound) => Ok(()),
        Err(e) => Err(e),
    }
}

// Opens /f55a.txt (O_RDWR|O_CREAT|O_TRUNC) as fd 3 and forks. The parent
// writes "P" through fd 3 1,000 times, waits for the child, and exits 0
// if the child exited 0. The child loops 1,000 times: dup(3), close the
// dup, open /f55b.txt (O_WRONLY|O_CREAT|O_APPEND), write "c", close.
// Exit codes: 1 open, 2 fork, 3 parent write, 4 wait4, 6 child signaled,
// 50 + n child exit n; the child's own: 21 dup, 22 close dup, 23 open,
// 24 write, 25 close.
user_code!(
    F55_CHURN,
    "
    lea rdi, [rip + 90f]
    mov esi, 0x242
    xor edx, edx
    mov eax, 2
    syscall
    mov edi, 1
    cmp rax, 3
    jne 80f
    mov eax, 57
    syscall
    mov edi, 2
    test rax, rax
    js 80f
    jz 10f
    mov r12, rax
    mov ebx, 1000
1:
    mov edi, 3
    lea rsi, [rip + 92f]
    mov edx, 1
    mov eax, 1
    syscall
    cmp rax, 1
    jne 3f
    dec ebx
    jnz 1b
    sub rsp, 16
    mov dword ptr [rsp], -1
    mov rdi, r12
    mov rsi, rsp
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, 4
    cmp rax, r12
    jne 80f
    mov eax, dword ptr [rsp]
    xor edi, edi
    test eax, eax
    jz 80f
    mov edi, 6
    test eax, 0x7f
    jnz 80f
    shr eax, 8
    and eax, 0xff
    lea edi, [rax + 50]
    jmp 80f
3:
    mov edi, 3
    jmp 80f
10:
    mov ebx, 1000
11:
    mov edi, 3
    mov eax, 32
    syscall
    mov edi, 21
    test rax, rax
    js 80f
    mov rdi, rax
    mov eax, 3
    syscall
    mov edi, 22
    test rax, rax
    jnz 80f
    lea rdi, [rip + 91f]
    mov esi, 0x441
    xor edx, edx
    mov eax, 2
    syscall
    mov edi, 23
    test rax, rax
    js 80f
    mov r13, rax
    mov rdi, r13
    lea rsi, [rip + 93f]
    mov edx, 1
    mov eax, 1
    syscall
    mov edi, 24
    cmp rax, 1
    jne 80f
    mov rdi, r13
    mov eax, 3
    syscall
    mov edi, 25
    test rax, rax
    jnz 80f
    dec ebx
    jnz 11b
    xor edi, edi
80:
    mov eax, 60
    syscall
    ud2
90:
    .asciz \"/f55a.txt\"
91:
    .asciz \"/f55b.txt\"
92:
    .ascii \"P\"
93:
    .ascii \"c\"
    "
);

const CHURN: usize = 1000;

fn test_file_table_fork_churn() -> Outcome {
    if unlink_quiet("/f55a.txt").is_err() || unlink_quiet("/f55b.txt").is_err() {
        return Outcome::Fail("unlink before");
    }
    let base = file_init::testing::table();
    file_init::testing::set_write_yield(true);
    let st = user::run(&Image::Code(F55_CHURN, DEFAULT), &["f55churn"]);
    file_init::testing::set_write_yield(false);
    let out = check_churn(st, &base);
    let ua = unlink_quiet("/f55a.txt");
    let ub = unlink_quiet("/f55b.txt");
    if !matches!(out, Outcome::Ok) {
        return out;
    }
    if ua.is_err() || ub.is_err() {
        return Outcome::Fail("unlink after");
    }
    Outcome::Ok
}

fn check_churn(
    st: Result<u32, crate::user_init::LoadError>,
    base: &[(bool, u16); vibeos::limits::MAX_OPEN_FILES],
) -> Outcome {
    let st = match st {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if st != wait_exited(0) {
        return crate::fail_fmt!("status {st:#x}, want exited 0");
    }
    let mut buf = [0u8; CHURN + 64];
    match read_all("/f55a.txt", &mut buf) {
        Ok(n) if n == CHURN && buf[..n].iter().all(|&b| b == b'P') => {}
        Ok(n) => {
            let p = buf[..n].iter().filter(|&&b| b == b'P').count();
            return crate::fail_fmt!("f55a: {n} bytes, {p} P");
        }
        Err(e) => return crate::fail_fmt!("f55a: {}", e.as_str()),
    }
    match read_all("/f55b.txt", &mut buf) {
        Ok(n) if n == CHURN && buf[..n].iter().all(|&b| b == b'c') => {}
        Ok(n) => {
            let p = buf[..n].iter().filter(|&&b| b == b'P').count();
            return crate::fail_fmt!("f55b: {n} bytes, {p} P");
        }
        Err(e) => return crate::fail_fmt!("f55b: {}", e.as_str()),
    }
    let now = file_init::testing::table();
    let mut i = 0usize;
    while i < now.len() {
        if now[i] != base[i] {
            return crate::fail_fmt!(
                "slot {i}: (used, refs) {:?}, baseline {:?}",
                now[i],
                base[i]
            );
        }
        i += 1;
    }
    Outcome::Ok
}
