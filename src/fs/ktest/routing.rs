//! In-guest tests of path resolution through `Vfs` (ROADMAP §10.4, A3,
//! F086, F126).

use vibeos::fs::FsError;
use vibeos::proc::wait_exited;

use crate::fat_init;
use crate::file_init;
use crate::ktest::user::{self, DEFAULT, Image, x86_user_code};
use crate::ktest::{Outcome, fid};

/// `unlink` drops the name from the directory the path resolved to: a
/// dentry a `stat` cached in `/s59u` is gone after the unlink, so the same
/// `stat` finds nothing (F126).
pub(crate) fn test_vfs_unlink_drops_parent_dentry() -> Outcome {
    let r = unlink_drops();
    let _ = file_init::unlink(b"/s59u/f");
    let _ = file_init::rmdir(b"/s59u");
    match r {
        Ok(()) => Outcome::Ok,
        Err(why) => Outcome::Fail(why),
    }
}

fn unlink_drops() -> Result<(), &'static str> {
    file_init::mkdir(b"/s59u", 0o755).map_err(|_| "mkdir /s59u")?;
    file_init::creat(b"/s59u/f").map_err(|_| "create /s59u/f")?;
    fid::stat_path("/s59u/f").map_err(|_| "stat before unlink")?;
    file_init::unlink(b"/s59u/f").map_err(|_| "unlink")?;
    match fid::stat_path("/s59u/f") {
        Err(FsError::NotFound) => {}
        Ok(_) => return Err("stat still finds the unlinked name"),
        Err(_) => return Err("stat after unlink: not NotFound"),
    }
    file_init::rmdir(b"/s59u").map_err(|_| "rmdir /s59u")
}

// `vfs_user_dev_nodes`: open /dev/null (O_WRONLY|O_CREAT|O_TRUNC, 0666)
// and write 4 bytes; read 8 zero bytes from /dev/zero; read 1 to 16
// bytes from /dev/random, sleeping through EAGAIN a bounded number of
// times; close each. Exit 0, or the number of the step that failed.
x86_user_code!(
    VFS_DEV_NODES,
    "
    lea rdi, [rip + 90f]
    mov esi, 0x241
    mov edx, 0x1b6
    mov eax, 2
    syscall
    mov edi, 1
    test rax, rax
    js 80f
    mov r12, rax
    mov rdi, r12
    lea rsi, [rip + 93f]
    mov edx, 4
    mov eax, 1
    syscall
    mov edi, 2
    cmp rax, 4
    jne 80f
    mov rdi, r12
    mov eax, 3
    syscall
    mov edi, 3
    test rax, rax
    jnz 80f
    lea rdi, [rip + 91f]
    xor esi, esi
    xor edx, edx
    mov eax, 2
    syscall
    mov edi, 4
    test rax, rax
    js 80f
    mov r12, rax
    sub rsp, 32
    mov qword ptr [rsp], -1
    mov rdi, r12
    mov rsi, rsp
    mov edx, 8
    xor eax, eax
    syscall
    mov edi, 5
    cmp rax, 8
    jne 80f
    mov edi, 6
    cmp qword ptr [rsp], 0
    jne 80f
    mov rdi, r12
    mov eax, 3
    syscall
    mov edi, 7
    test rax, rax
    jnz 80f
    lea rdi, [rip + 92f]
    xor esi, esi
    xor edx, edx
    mov eax, 2
    syscall
    mov edi, 8
    test rax, rax
    js 80f
    mov r12, rax
    mov ebx, 100000
1:
    mov rdi, r12
    mov rsi, rsp
    mov edx, 16
    xor eax, eax
    syscall
    cmp rax, -11
    jne 2f
    mov eax, 24
    syscall
    dec ebx
    jnz 1b
    mov edi, 9
    jmp 80f
2:
    mov edi, 10
    cmp rax, 1
    jl 80f
    cmp rax, 16
    jg 80f
    mov rdi, r12
    mov eax, 3
    syscall
    mov edi, 11
    test rax, rax
    jnz 80f
    xor edi, edi
80:
    mov eax, 60
    syscall
    ud2
90:
    .asciz \"/dev/null\"
91:
    .asciz \"/dev/zero\"
92:
    .asciz \"/dev/random\"
93:
    .ascii \"s59!\"
    "
);

/// A ring-3 program opens, writes or reads, and closes `/dev/null`,
/// `/dev/zero` and `/dev/random`: `open` resolves through `Vfs`, so the
/// path reaches devfs (F086).
pub(crate) fn test_vfs_user_dev_nodes() -> Outcome {
    match user::run(&Image::Code(VFS_DEV_NODES, DEFAULT), &["devnodes"]) {
        Ok(st) if st == wait_exited(0) => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("status {st:#x}, want exited 0 (step {})", st >> 8),
        Err(e) => crate::fail_fmt!("spawn: {}", e.as_str()),
    }
}

/// After `vfs_user_dev_nodes`, the FAT initrd's own `/dev` directory,
/// which devfs's mount hides, holds no `null` entry in any case: the
/// ring-3 `open(O_CREAT)` reached devfs, not FAT.
pub(crate) fn test_fat_initrd_dev_no_null() -> Outcome {
    let Ok(root) = fat_init::root_volume() else {
        return Outcome::Fail("no FAT root volume");
    };
    match fat_init::ktest_dir_has(&root, &[b"dev"], b"null") {
        Ok(false) => Outcome::Ok,
        Ok(true) => Outcome::Fail("the FAT initrd's /dev holds a null entry"),
        Err(e) => crate::fail_fmt!("FAT lookup in /dev: {}", e.as_str()),
    }
}
