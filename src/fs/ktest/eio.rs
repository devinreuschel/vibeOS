//! `fat_bad_sector_eio` (kernel_tests only), re-exported from `fs::ktest`:
//! a sector of `vda` that the device fails to read, under a FAT file, a
//! FAT directory and a FAT executable, comes back to ring 3 as `EIO` from
//! `read`, `write`, `open` and `execve` (SYSCALL.md §3, the four syscalls'
//! `EIO`; ROADMAP §10.5's errno box). Opt-in: it runs in
//! `run_ktest.py`'s `_vblk_bad_sector_boot`, after `vblk_bad_sector`,
//! whose `vda` is a pattern image behind QEMU's `blkdebug`, which fails
//! every read of [`BAD_SECTOR`]; it overwrites that image.
//!
//! The volume is built in memory, with the three objects' first clusters
//! in the 4 KiB block-cache page that holds [`BAD_SECTOR`], then written to
//! `vda` below the block cache, so no cached copy hides the bad sector:
//! the mount's first read of that page goes to the device and fails.

use vibeos::fat::{FatInfo, FatInode, FatVol, MemDisk, mkfs};
use vibeos::kalloc::TryVec;
use vibeos::kerror::KError;
use vibeos::proc::wait_exited;

use crate::block::blockdev_init;
use crate::drivers::ktest::BAD_SECTOR;
use crate::file_init;
use crate::ktest::Outcome;
use crate::ktest::user::{self, DEFAULT, Image, user_code};

const SEC: usize = 512;
/// Sectors per block-cache page: the page the bad sector is in fails as
/// one read.
const PAGE_SECS: u64 = 8;
/// The image's sectors: past the bad sector's page.
const IMG_SECTORS: usize = 4352;
const MNT: &[u8] = b"/s59eio";

/// A fresh volume's state, copied to the heap (`fs::boxed_copy`).
static VOL_INIT: FatVol = FatVol::new();

// Each program exits with the errno its syscall returned, 0x80 when the
// syscall succeeded, or 0x40 | errno when its `open` of the file failed.

// open("/s59eio/data", O_RDONLY), then read 16 bytes onto the stack.
user_code!(
    EIO_READ,
    "
    lea rdi, [rip + 90f]
    xor esi, esi
    xor edx, edx
    mov eax, 2
    syscall
    test rax, rax
    js 70f
    mov rdi, rax
    sub rsp, 64
    mov rsi, rsp
    mov edx, 16
    xor eax, eax
    syscall
    test rax, rax
    js 80f
    mov edi, 0x80
    jmp 81f
70:
    neg rax
    or eax, 0x40
    mov edi, eax
    jmp 81f
80:
    neg rax
    mov rdi, rax
81:
    mov eax, 60
    syscall
    ud2
90:
    .asciz \"/s59eio/data\"
    "
);

// open("/s59eio/data", O_WRONLY), then write 1 byte at offset 0, which
// reads the file's first cluster to change it.
user_code!(
    EIO_WRITE,
    "
    lea rdi, [rip + 90f]
    mov esi, 1
    xor edx, edx
    mov eax, 2
    syscall
    test rax, rax
    js 70f
    mov rdi, rax
    lea rsi, [rip + 90f]
    mov edx, 1
    mov eax, 1
    syscall
    test rax, rax
    js 80f
    mov edi, 0x80
    jmp 81f
70:
    neg rax
    or eax, 0x40
    mov edi, eax
    jmp 81f
80:
    neg rax
    mov rdi, rax
81:
    mov eax, 60
    syscall
    ud2
90:
    .asciz \"/s59eio/data\"
    "
);

// open("/s59eio/dir/x", O_RDONLY): the lookup of `x` reads `dir`'s only
// cluster.
user_code!(
    EIO_OPEN,
    "
    lea rdi, [rip + 90f]
    xor esi, esi
    xor edx, edx
    mov eax, 2
    syscall
    test rax, rax
    js 80f
    mov edi, 0x80
    jmp 81f
80:
    neg rax
    mov rdi, rax
81:
    mov eax, 60
    syscall
    ud2
90:
    .asciz \"/s59eio/dir/x\"
    "
);

// execve("/s59eio/prog", NULL, NULL): the load reads the ELF header from
// the file's first cluster.
user_code!(
    EIO_EXEC,
    "
    lea rdi, [rip + 90f]
    xor esi, esi
    xor edx, edx
    mov eax, 59
    syscall
    test rax, rax
    js 80f
    mov edi, 0x80
    jmp 81f
80:
    neg rax
    mov rdi, rax
81:
    mov eax, 60
    syscall
    ud2
90:
    .asciz \"/s59eio/prog\"
    "
);

/// A zeroed heap buffer of `n` bytes.
fn zeroed(n: usize) -> Result<TryVec<u8>, &'static str> {
    let mut v = TryVec::try_with_capacity(n).map_err(|_| "image alloc")?;
    let zero = [0u8; SEC];
    while v.len() < n {
        v.try_extend_from_slice(&zero).map_err(|_| "image alloc")?;
    }
    Ok(v)
}

/// Whether cluster `clu`'s first sector is in the bad sector's page.
fn in_bad_page(info: FatInfo, clu: u32) -> bool {
    let page = BAD_SECTOR - BAD_SECTOR % PAGE_SECS;
    info.clus_lba(clu)
        .is_ok_and(|lba| (page..page + PAGE_SECS).contains(&u64::from(lba)))
}

/// Create file `name` in the root and write `data` into it.
fn file_with(
    vol: &mut FatVol,
    disk: &mut MemDisk<'_>,
    name: &[u8],
    data: &[u8],
) -> Result<u32, &'static str> {
    let root = vol.info.root_clus;
    let node = vol
        .create(disk, root, name, false)
        .map_err(|_| "create file")?;
    let mut ino = FatInode::of_node(&node);
    match vol.write_ino(disk, &mut ino, true, 0, false, data) {
        Ok((n, _)) if n == data.len() => Ok(ino.first_clu),
        _ => Err("write file"),
    }
}

/// The volume: `data` (one sector of bytes), `dir` (empty) and `prog`
/// (an ELF magic and zeros), each starting in the bad sector's page.
fn build_image() -> Result<TryVec<u8>, &'static str> {
    let mut img = zeroed(IMG_SECTORS * SEC)?;
    let info = mkfs(&mut img, b"S59EIO").map_err(|_| "mkfs")?;
    let page = BAD_SECTOR - BAD_SECTOR % PAGE_SECS;
    let first = u32::try_from(page)
        .ok()
        .and_then(|p| p.checked_sub(info.data_lba))
        .and_then(|o| o.checked_div(u32::from(info.spc)))
        .and_then(|c| c.checked_add(2))
        .ok_or("bad sector before the data area")?;
    let mut disk = MemDisk::new(&mut img, SEC as u32).map_err(|_| "image disk")?;
    let mut vol = crate::fs::boxed_copy(&VOL_INIT).map_err(|_| "volume alloc")?;
    vol.mount_in(&mut disk).map_err(|_| "mount the image")?;
    // Allocation starts at the next-free hint: the bad page's first cluster.
    vol.hint = first;
    let data = file_with(&mut vol, &mut disk, b"data", &[0x5A; SEC])?;
    let root = vol.info.root_clus;
    let dir = vol
        .create(&mut disk, root, b"dir", true)
        .map_err(|_| "create dir")?
        .clu;
    let mut elf = [0u8; 64];
    elf[..4].copy_from_slice(b"\x7fELF");
    let prog = file_with(&mut vol, &mut disk, b"prog", &elf)?;
    vol.sync(&mut disk).map_err(|_| "sync the image")?;
    if ![data, dir, prog].iter().all(|&c| in_bad_page(info, c)) {
        return Err("a file's first cluster is outside the bad sector's page");
    }
    Ok(img)
}

/// Write the volume to `vda` from LBA 0 below the block cache. The
/// bad-sector boot selects this test after `vblk_bad_sector` alone, and
/// both read `vda` below the cache too (as `part_init::scan` does), so no
/// cached page of these LBAs exists for the mount to read stale.
fn write_image(img: &[u8]) -> Result<(), &'static str> {
    let vda = blockdev_init::lookup(b"vda").ok_or("no vda")?;
    for (i, chunk) in img.chunks(PAGE_SECS as usize * SEC).enumerate() {
        vda.write_dev(i as u64 * PAGE_SECS, chunk)
            .map_err(|_| "image write")?;
    }
    vda.flush_dev().map_err(|_| "image flush")
}

/// Each case: its name and program. Each must exit with `EIO`.
const CASES: [(&str, &[u8]); 4] = [
    ("read", EIO_READ),
    ("write", EIO_WRITE),
    ("open", EIO_OPEN),
    ("execve", EIO_EXEC),
];

/// Run every case; fail naming each case's wait status unless every one
/// exited with `EIO`.
fn run_cases() -> Outcome {
    let want = wait_exited(KError::Io.errno() as u32);
    let mut st = [0u32; CASES.len()];
    for ((name, prog), s) in CASES.iter().zip(st.iter_mut()) {
        *s = match user::run(&Image::Code(prog, DEFAULT), &["s59eio"]) {
            Ok(v) => v,
            Err(e) => return crate::fail_fmt!("{name}: spawn: {}", e.as_str()),
        };
    }
    if st.iter().all(|&s| s == want) {
        return Outcome::Ok;
    }
    let [a, b, c, d] = st;
    let [(na, _), (nb, _), (nc, _), (nd, _)] = CASES;
    crate::fail_fmt!(
        "status {na} {a:#x}, {nb} {b:#x}, {nc} {c:#x}, {nd} {d:#x}; want each {want:#x} (exited 5, EIO)"
    )
}

/// See the module doc. Fails unless each of `read`, `write`, `open` and
/// `execve` returns `EIO` to ring 3, and the volume unmounts.
pub(crate) fn fat_bad_sector_eio() -> Outcome {
    let img = match build_image() {
        Ok(i) => i,
        Err(why) => return Outcome::Fail(why),
    };
    if let Err(why) = write_image(&img) {
        return Outcome::Fail(why);
    }
    drop(img);
    if file_init::mkdir(MNT, 0o755).is_err() {
        return Outcome::Fail("mkdir");
    }
    if file_init::mount(b"vda", MNT, b"fat32", false).is_err() {
        return Outcome::Fail("mount");
    }
    let out = run_cases();
    let u = file_init::umount(MNT);
    if !matches!(out, Outcome::Ok) {
        return out;
    }
    if u.is_err() {
        return Outcome::Fail("umount");
    }
    match file_init::rmdir(MNT) {
        Ok(()) => Outcome::Ok,
        Err(_) => Outcome::Fail("rmdir"),
    }
}
