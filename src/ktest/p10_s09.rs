//! In-guest tests of P10-S09, FAT inode, open-file table generations, vibefs size limits (DESIGN §8.2).

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vibeos::fs::{FsError, O_CREAT, O_EXCL, O_RDONLY, O_RDWR, O_TRUNC, SEEK_CUR, SEEK_SET};
use vibeos::limits::MAX_OPEN_FILES;
use vibeos::proc::wait_exited;

use super::user::{self, DEFAULT, Image, user_code};
use super::{Outcome, Test, test};
use crate::file_init::{self, FileId};
use crate::thread_init;
use crate::time_init;

pub(super) const TESTS: &[Test] = &[
    test("file_table_fork_churn", test_file_table_fork_churn),
    test(
        "file_table_stale_writeback_ebadf",
        test_file_table_stale_writeback_ebadf,
    ),
    test("open_creat_exists_opens", test_open_creat_exists_opens),
];

/// Each open-file slot's `(used, refs)`.
fn holders() -> [(bool, u16); MAX_OPEN_FILES] {
    file_init::testing::table().map(|(used, refs, _)| (used, refs))
}

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
    let base = holders();
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
    base: &[(bool, u16); MAX_OPEN_FILES],
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
    let now = holders();
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

fn pack(id: FileId) -> u32 {
    (u32::from(id.fid) << 16) | u32::from(id.r#gen)
}

fn unpack(v: u32) -> FileId {
    FileId {
        fid: (v >> 16) as u16,
        r#gen: v as u16,
    }
}

/// The handle the held write uses; the helper closes it.
static STALE_A: AtomicU32 = AtomicU32::new(0);
/// The handle the helper opened into the freed slot.
static STALE_B: AtomicU32 = AtomicU32::new(0);
static STALE_B_OK: AtomicBool = AtomicBool::new(false);
static STALE_DONE: AtomicBool = AtomicBool::new(false);

/// Waits (at most 10,000 yields) for the held write, closes its file,
/// opens `/f55t.txt` into the freed slot, then releases the write.
fn stale_helper() {
    let mut n = 0u32;
    while !file_init::testing::write_held() && n < 10_000 {
        thread_init::yield_now();
        n += 1;
    }
    if file_init::testing::write_held()
        && file_init::close(unpack(STALE_A.load(Ordering::Acquire))).is_ok()
        && let Ok(b) = file_init::open("/f55t.txt", O_RDWR | O_CREAT | O_TRUNC, 0)
    {
        STALE_B.store(pack(b), Ordering::Release);
        STALE_B_OK.store(true, Ordering::Release);
    }
    file_init::testing::release_write();
    STALE_DONE.store(true, Ordering::Release);
}

fn test_file_table_stale_writeback_ebadf() -> Outcome {
    let out = stale_writeback();
    let us = unlink_quiet("/f55s.txt");
    let ut = unlink_quiet("/f55t.txt");
    if !matches!(out, Outcome::Ok) {
        return out;
    }
    if us.is_err() || ut.is_err() {
        return Outcome::Fail("unlink after");
    }
    Outcome::Ok
}

fn stale_writeback() -> Outcome {
    const FL: u32 = O_RDWR | O_CREAT | O_TRUNC;
    // A handle whose slot was closed and reused.
    let a = match file_init::open("/f55s.txt", FL, 0) {
        Ok(a) => a,
        Err(e) => return crate::fail_fmt!("open s: {}", e.as_str()),
    };
    if let Err(e) = file_init::close(a) {
        return crate::fail_fmt!("close s: {}", e.as_str());
    }
    let b = match file_init::open("/f55t.txt", FL, 0) {
        Ok(b) => b,
        Err(e) => return crate::fail_fmt!("open t: {}", e.as_str()),
    };
    let stale = [
        ("write", file_init::write(a, b"x").err()),
        ("seek", file_init::seek(a, 5, SEEK_SET).err()),
        ("addref", file_init::addref(a).err()),
        ("close", file_init::close(a).err()),
    ];
    let pos = file_init::seek(b, 0, SEEK_CUR);
    let cb = file_init::close(b);
    if b.fid != a.fid || b.r#gen == a.r#gen {
        return crate::fail_fmt!("slot not reused: {a:?} then {b:?}");
    }
    for (op, e) in stale {
        if e != Some(FsError::Badf) {
            return crate::fail_fmt!("stale {op}: {e:?}, want Badf");
        }
    }
    if pos != Ok(0) {
        return crate::fail_fmt!("reused slot offset {pos:?}, want 0");
    }
    if let Err(e) = cb {
        return crate::fail_fmt!("close t: {}", e.as_str());
    }
    // A write held between its I/O and its write-back while another
    // thread closes its file and opens another into the slot.
    let a = match file_init::open("/f55s.txt", FL, 0) {
        Ok(a) => a,
        Err(e) => return crate::fail_fmt!("open s: {}", e.as_str()),
    };
    STALE_A.store(pack(a), Ordering::Release);
    STALE_B_OK.store(false, Ordering::Release);
    STALE_DONE.store(false, Ordering::Release);
    file_init::testing::hold_next_write();
    super::spawn_thread("f55-stale", stale_helper);
    let w = file_init::write(a, b"x");
    let deadline = time_init::now_ns().saturating_add(1_000_000_000);
    while !STALE_DONE.load(Ordering::Acquire) && time_init::now_ns() < deadline {
        thread_init::yield_now();
    }
    if !STALE_DONE.load(Ordering::Acquire) {
        return Outcome::Fail("helper did not finish");
    }
    if !STALE_B_OK.load(Ordering::Acquire) {
        let _ = file_init::close(a);
        return Outcome::Fail("helper: no held write, or close/open failed");
    }
    let b = unpack(STALE_B.load(Ordering::Acquire));
    let pos = file_init::seek(b, 0, SEEK_CUR);
    let cb = file_init::close(b);
    if b.fid != a.fid {
        return crate::fail_fmt!("slot not reused: {a:?} then {b:?}");
    }
    if w != Err(FsError::Badf) {
        return crate::fail_fmt!("held write {w:?}, want Badf");
    }
    if pos != Ok(0) {
        return crate::fail_fmt!("other file's offset {pos:?}, want 0");
    }
    if let Err(e) = cb {
        return crate::fail_fmt!("close t: {}", e.as_str());
    }
    Outcome::Ok
}

fn test_open_creat_exists_opens() -> Outcome {
    if unlink_quiet("/f55r.txt").is_err() {
        return Outcome::Fail("unlink before");
    }
    file_init::testing::set_open_race(true);
    let r = file_init::open("/f55r.txt", O_RDWR | O_CREAT, 0);
    file_init::testing::set_open_race(false);
    match r {
        Ok(id) => {
            if let Err(e) = file_init::close(id) {
                return crate::fail_fmt!("close: {}", e.as_str());
            }
        }
        Err(e) => return crate::fail_fmt!("O_CREAT: {}, want open", e.as_str()),
    }
    if unlink_quiet("/f55r.txt").is_err() {
        return Outcome::Fail("unlink");
    }
    file_init::testing::set_open_race(true);
    let r = file_init::open("/f55r.txt", O_RDWR | O_CREAT | O_EXCL, 0);
    file_init::testing::set_open_race(false);
    let excl = match r {
        Err(FsError::Exists) => Outcome::Ok,
        Ok(id) => {
            let _ = file_init::close(id);
            Outcome::Fail("O_CREAT|O_EXCL opened, want Exists")
        }
        Err(e) => crate::fail_fmt!("O_CREAT|O_EXCL: {}, want Exists", e.as_str()),
    };
    if unlink_quiet("/f55r.txt").is_err() {
        return Outcome::Fail("unlink after");
    }
    excl
}
