//! In-guest tests of P10-S09, FAT inode, open-file table generations, vibefs size limits (DESIGN §8.2).

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vibeos::fs::{
    FsError, O_APPEND, O_CREAT, O_EXCL, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY, SEEK_CUR, SEEK_END,
    SEEK_SET,
};
use vibeos::limits::MAX_OPEN_FILES;
use vibeos::proc::wait_exited;

use super::p10_s12::fid;
use super::user::{self, DEFAULT, Image, user_code};
use super::{Outcome, Test, test};
use crate::fat_init;
use crate::file_init;
use crate::thread_init;
use crate::time_init;
use vibeos::fs::FileId;

pub(super) const TESTS: &[Test] = &[
    test("file_table_fork_churn", test_file_table_fork_churn),
    test(
        "file_table_stale_writeback_ebadf",
        test_file_table_stale_writeback_ebadf,
    ),
    test("open_creat_exists_opens", test_open_creat_exists_opens),
    test(
        "inode_size_shared_across_opens",
        test_inode_size_shared_across_opens,
    ),
    test(
        "fat_unlinked_open_frees_at_close",
        test_fat_unlinked_open_frees_at_close,
    ),
    test("vibefs_efbig", test_vibefs_efbig),
    test("vibefs_seek_end_5gib", test_vibefs_seek_end_5gib),
];

/// Each open-file slot's `(used, refs)`.
fn holders() -> [(bool, u16); MAX_OPEN_FILES] {
    file_init::testing::table().map(|(used, refs, _)| (used, refs))
}

/// Read up to `out.len()` bytes of `path` from offset 0; the count read.
fn read_all(path: &str, out: &mut [u8]) -> Result<usize, FsError> {
    let fid = fid::open(path, O_RDONLY, 0)?;
    let mut n = 0usize;
    let r = loop {
        let Some(rest) = out.get_mut(n..) else {
            break Ok(n);
        };
        if rest.is_empty() {
            break Ok(n);
        }
        match fid::read(fid, rest) {
            Ok(0) => break Ok(n),
            Ok(k) => n = n.saturating_add(k),
            Err(e) => break Err(e),
        }
    };
    let c = fid::close(fid);
    let n = r?;
    c?;
    Ok(n)
}

/// Unlink `path`; a missing file is not an error.
fn unlink_quiet(path: &str) -> Result<(), FsError> {
    match fid::unlink_path(path, false) {
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
        && fid::close(unpack(STALE_A.load(Ordering::Acquire))).is_ok()
        && let Ok(b) = fid::open("/f55t.txt", O_RDWR | O_CREAT | O_TRUNC, 0)
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
    let a = match fid::open("/f55s.txt", FL, 0) {
        Ok(a) => a,
        Err(e) => return crate::fail_fmt!("open s: {}", e.as_str()),
    };
    if let Err(e) = fid::close(a) {
        return crate::fail_fmt!("close s: {}", e.as_str());
    }
    let b = match fid::open("/f55t.txt", FL, 0) {
        Ok(b) => b,
        Err(e) => return crate::fail_fmt!("open t: {}", e.as_str()),
    };
    let stale = [
        ("write", fid::write(a, b"x").err()),
        ("seek", fid::seek(a, 5, SEEK_SET).err()),
        ("addref", fid::addref(a).err()),
        ("close", fid::close(a).err()),
    ];
    let pos = fid::seek(b, 0, SEEK_CUR);
    let cb = fid::close(b);
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
    let a = match fid::open("/f55s.txt", FL, 0) {
        Ok(a) => a,
        Err(e) => return crate::fail_fmt!("open s: {}", e.as_str()),
    };
    STALE_A.store(pack(a), Ordering::Release);
    STALE_B_OK.store(false, Ordering::Release);
    STALE_DONE.store(false, Ordering::Release);
    file_init::testing::hold_next_write();
    super::spawn_thread("f55-stale", stale_helper);
    let w = fid::write(a, b"x");
    let deadline = time_init::now_ns().saturating_add(1_000_000_000);
    while !STALE_DONE.load(Ordering::Acquire) && time_init::now_ns() < deadline {
        thread_init::yield_now();
    }
    if !STALE_DONE.load(Ordering::Acquire) {
        return Outcome::Fail("helper did not finish");
    }
    if !STALE_B_OK.load(Ordering::Acquire) {
        let _ = fid::close(a);
        return Outcome::Fail("helper: no held write, or close/open failed");
    }
    let b = unpack(STALE_B.load(Ordering::Acquire));
    let pos = fid::seek(b, 0, SEEK_CUR);
    let cb = fid::close(b);
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
    let r = fid::open("/f55r.txt", O_RDWR | O_CREAT, 0);
    file_init::testing::set_open_race(false);
    match r {
        Ok(id) => {
            if let Err(e) = fid::close(id) {
                return crate::fail_fmt!("close: {}", e.as_str());
            }
        }
        Err(e) => return crate::fail_fmt!("O_CREAT: {}, want open", e.as_str()),
    }
    if unlink_quiet("/f55r.txt").is_err() {
        return Outcome::Fail("unlink");
    }
    file_init::testing::set_open_race(true);
    let r = fid::open("/f55r.txt", O_RDWR | O_CREAT | O_EXCL, 0);
    file_init::testing::set_open_race(false);
    let excl = match r {
        Err(FsError::Exists) => Outcome::Ok,
        Ok(id) => {
            let _ = fid::close(id);
            Outcome::Fail("O_CREAT|O_EXCL opened, want Exists")
        }
        Err(e) => crate::fail_fmt!("O_CREAT|O_EXCL: {}, want Exists", e.as_str()),
    };
    if unlink_quiet("/f55r.txt").is_err() {
        return Outcome::Fail("unlink after");
    }
    excl
}

/// Close every handle in `ids`, then unlink `path`; the first error.
fn close_unlink(ids: &[Option<FileId>], path: &str) -> Result<(), FsError> {
    let mut r = Ok(());
    for &id in ids.iter().flatten() {
        if let Err(e) = fid::close(id)
            && r.is_ok()
        {
            r = Err(e);
        }
    }
    let u = unlink_quiet(path);
    r.and(u)
}

/// Four opens of `path`: a writer, a second descriptor, an `O_APPEND`
/// one and an `O_TRUNC` one, all see one size.
fn shared_size(path: &str) -> Outcome {
    let mut ids: [Option<FileId>; 4] = [None; 4];
    let out = shared_size_on(path, &mut ids);
    let c = close_unlink(&ids, path);
    if !matches!(out, Outcome::Ok) {
        return out;
    }
    match c {
        Ok(()) => Outcome::Ok,
        Err(e) => crate::fail_fmt!("{path}: close/unlink: {}", e.as_str()),
    }
}

fn shared_size_on(path: &str, ids: &mut [Option<FileId>; 4]) -> Outcome {
    let flags = [
        O_RDWR | O_CREAT | O_TRUNC,
        O_RDWR,
        O_WRONLY | O_APPEND,
        O_RDWR | O_TRUNC,
    ];
    let mut open = |k: usize| -> Result<FileId, Outcome> {
        match fid::open(path, flags[k], 0) {
            Ok(id) => {
                ids[k] = Some(id);
                Ok(id)
            }
            Err(e) => Err(crate::fail_fmt!("{path}: open {k}: {}", e.as_str())),
        }
    };
    let a = match open(0) {
        Ok(id) => id,
        Err(o) => return o,
    };
    let b = match open(1) {
        Ok(id) => id,
        Err(o) => return o,
    };
    if fid::write(a, &[b'a'; 100]) != Ok(100) {
        return crate::fail_fmt!("{path}: write 100");
    }
    match fid::seek(b, 0, SEEK_END) {
        Ok(100) => {}
        r => return crate::fail_fmt!("{path}: second SEEK_END {r:?}, want 100"),
    }
    let c = match open(2) {
        Ok(id) => id,
        Err(o) => return o,
    };
    if fid::write(c, b"Z") != Ok(1) {
        return crate::fail_fmt!("{path}: append write");
    }
    let mut z = [0u8; 1];
    if fid::seek(b, 100, SEEK_SET) != Ok(100) || fid::read(b, &mut z) != Ok(1) {
        return crate::fail_fmt!("{path}: read back byte 100");
    }
    if &z != b"Z" {
        return crate::fail_fmt!("{path}: byte 100 is {:#x}, want Z", z[0]);
    }
    let d = match open(3) {
        Ok(id) => id,
        Err(o) => return o,
    };
    if fid::write(b, b"xyz") != Ok(3) {
        return crate::fail_fmt!("{path}: write after O_TRUNC");
    }
    for (k, id) in [a, b, c, d].into_iter().enumerate() {
        match fid::seek(id, 0, SEEK_END) {
            Ok(104) => {}
            r => return crate::fail_fmt!("{path}: fd {k} SEEK_END {r:?}, want 104"),
        }
    }
    match fid::stat_path(path) {
        Ok(st) if st.size == 104 => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("{path}: stat size {}, want 104", st.size),
        Err(e) => crate::fail_fmt!("{path}: stat: {}", e.as_str()),
    }
}

fn test_inode_size_shared_across_opens() -> Outcome {
    for path in ["/f13s.txt", "/vibe/f13s"] {
        let out = shared_size(path);
        if !matches!(out, Outcome::Ok) {
            return out;
        }
    }
    Outcome::Ok
}

/// Free bytes on the FAT initrd.
fn fat_free() -> Result<u64, FsError> {
    fat_init::df(fat_init::VOL_INITRD).map(|(_, _, free, _)| free)
}

fn test_fat_unlinked_open_frees_at_close() -> Outcome {
    const PATH: &str = "/f13u.txt";
    const FL: u32 = O_RDWR | O_CREAT | O_TRUNC;
    if unlink_quiet(PATH).is_err() {
        return Outcome::Fail("unlink before");
    }
    let Ok(before) = fat_free() else {
        return Outcome::Fail("df");
    };
    let a = match fid::open(PATH, FL, 0) {
        Ok(a) => a,
        Err(e) => return crate::fail_fmt!("open: {}", e.as_str()),
    };
    let out = unlinked_open(PATH, a, before);
    let c = fid::close(a);
    let u = unlink_quiet(PATH);
    if !matches!(out, Outcome::Ok) {
        return out;
    }
    if let Err(e) = c.and(u) {
        return crate::fail_fmt!("close/unlink: {}", e.as_str());
    }
    match fat_free() {
        Ok(f) if f == before => Outcome::Ok,
        Ok(f) => crate::fail_fmt!("free {f} after the last close, want {before}"),
        Err(e) => crate::fail_fmt!("df: {}", e.as_str()),
    }
}

fn unlinked_open(path: &str, a: FileId, before: u64) -> Outcome {
    let mut data = [0u8; 1500];
    let mut i = 0usize;
    while i < data.len() {
        data[i] = (i % 251) as u8;
        i += 1;
    }
    if fid::write(a, &data) != Ok(data.len()) {
        return Outcome::Fail("write");
    }
    let Ok(held) = fat_free() else {
        return Outcome::Fail("df");
    };
    if held >= before {
        return crate::fail_fmt!("free {held} with the file written, before {before}");
    }
    if let Err(e) = fid::unlink_path(path, false) {
        return crate::fail_fmt!("unlink open file: {}", e.as_str());
    }
    match fat_free() {
        Ok(f) if f == held => {}
        r => return crate::fail_fmt!("free {r:?} after unlink, want {held}"),
    }
    let mut back = [0u8; 1500];
    if fid::seek(a, 0, SEEK_SET) != Ok(0) || fid::read(a, &mut back) != Ok(1500) {
        return Outcome::Fail("read back after unlink");
    }
    if back != data {
        return Outcome::Fail("data changed after unlink");
    }
    // A new file of the same name is another inode.
    let n = match fid::open(path, O_RDWR | O_CREAT | O_TRUNC, 0) {
        Ok(n) => n,
        Err(e) => return crate::fail_fmt!("open new: {}", e.as_str()),
    };
    let w = fid::write(n, b"new");
    let new_end = fid::seek(n, 0, SEEK_END);
    let old_end = fid::seek(a, 0, SEEK_END);
    let cn = fid::close(n);
    if w != Ok(3) || new_end != Ok(3) {
        return crate::fail_fmt!("new file: write {w:?}, size {new_end:?}");
    }
    if old_end != Ok(1500) {
        return crate::fail_fmt!("unlinked file size {old_end:?}, want 1500");
    }
    if let Err(e) = cn {
        return crate::fail_fmt!("close new: {}", e.as_str());
    }
    if let Err(e) = unlink_quiet(path) {
        return crate::fail_fmt!("unlink new: {}", e.as_str());
    }
    match fat_free() {
        Ok(f) if f == held => Outcome::Ok,
        r => crate::fail_fmt!("free {r:?} while the unlinked file is open, want {held}"),
    }
}

// On /vibe/efbig (O_RDWR|O_CREAT|O_TRUNC): lseek to 2^44 - 4096 returns
// it (exit 2 if not), write 1 byte there returns -EFBIG (3), lseek to
// 2^44 returns -EINVAL (4), and SEEK_END returns 0 (5). Exit 1 if the
// open fails, 0 when every step passes.
user_code!(
    VIBEFS_EFBIG,
    "
    lea rdi, [rip + 90f]
    mov esi, 0x242
    xor edx, edx
    mov eax, 2
    syscall
    mov edi, 1
    test rax, rax
    js 80f
    mov r12, rax
    mov rdi, r12
    mov rsi, 0xFFFFFFFF000
    xor edx, edx
    mov eax, 8
    syscall
    mov edi, 2
    mov rcx, 0xFFFFFFFF000
    cmp rax, rcx
    jne 80f
    mov rdi, r12
    lea rsi, [rip + 91f]
    mov edx, 1
    mov eax, 1
    syscall
    mov edi, 3
    cmp rax, -27
    jne 80f
    mov rdi, r12
    mov rsi, 0x100000000000
    xor edx, edx
    mov eax, 8
    syscall
    mov edi, 4
    cmp rax, -22
    jne 80f
    mov rdi, r12
    xor esi, esi
    mov edx, 2
    mov eax, 8
    syscall
    mov edi, 5
    test rax, rax
    jnz 80f
    xor edi, edi
80:
    mov eax, 60
    syscall
    ud2
90:
    .asciz \"/vibe/efbig\"
91:
    .ascii \"x\"
    "
);

fn test_vibefs_efbig() -> Outcome {
    let st = user::run(&Image::Code(VIBEFS_EFBIG, DEFAULT), &["efbig"]);
    let u = unlink_quiet("/vibe/efbig");
    let st = match st {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if st != wait_exited(0) {
        return crate::fail_fmt!("status {st:#x}, want exited 0 (step {})", st >> 8);
    }
    match u {
        Ok(()) => Outcome::Ok,
        Err(e) => crate::fail_fmt!("unlink: {}", e.as_str()),
    }
}

// On /vibe/big5 (O_RDWR|O_CREAT|O_TRUNC): lseek to 5 GiB, write "x",
// SEEK_END returns 5 GiB + 1 (exit 3 if not), and the byte read back at
// 5 GiB is "x" (4). Exit 1 if the open fails, 2 if the lseek or write
// fails, 0 when every step passes.
user_code!(
    VIBEFS_BIG5,
    "
    lea rdi, [rip + 90f]
    mov esi, 0x242
    xor edx, edx
    mov eax, 2
    syscall
    mov edi, 1
    test rax, rax
    js 80f
    mov r12, rax
    mov r13, 0x140000000
    mov rdi, r12
    mov rsi, r13
    xor edx, edx
    mov eax, 8
    syscall
    mov edi, 2
    cmp rax, r13
    jne 80f
    mov rdi, r12
    lea rsi, [rip + 91f]
    mov edx, 1
    mov eax, 1
    syscall
    mov edi, 2
    cmp rax, 1
    jne 80f
    mov rdi, r12
    xor esi, esi
    mov edx, 2
    mov eax, 8
    syscall
    mov edi, 3
    lea rcx, [r13 + 1]
    cmp rax, rcx
    jne 80f
    mov rdi, r12
    mov rsi, r13
    xor edx, edx
    mov eax, 8
    syscall
    mov edi, 4
    cmp rax, r13
    jne 80f
    sub rsp, 16
    mov byte ptr [rsp], 0
    mov rdi, r12
    mov rsi, rsp
    mov edx, 1
    xor eax, eax
    syscall
    mov edi, 4
    cmp rax, 1
    jne 80f
    cmp byte ptr [rsp], 0x78
    jne 80f
    xor edi, edi
80:
    mov eax, 60
    syscall
    ud2
90:
    .asciz \"/vibe/big5\"
91:
    .ascii \"x\"
    "
);

fn test_vibefs_seek_end_5gib() -> Outcome {
    const BIG: u64 = (5 << 30) + 1;
    let st = user::run(&Image::Code(VIBEFS_BIG5, DEFAULT), &["big5"]);
    let size = fid::stat_path("/vibe/big5").map(|s| s.size);
    let u = unlink_quiet("/vibe/big5");
    let st = match st {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if st != wait_exited(0) {
        return crate::fail_fmt!("status {st:#x}, want exited 0 (step {})", st >> 8);
    }
    match size {
        Ok(BIG) => {}
        Ok(n) => return crate::fail_fmt!("stat size {n}, want {BIG}"),
        Err(e) => return crate::fail_fmt!("stat: {}", e.as_str()),
    }
    match u {
        Ok(()) => Outcome::Ok,
        Err(e) => crate::fail_fmt!("unlink: {}", e.as_str()),
    }
}
