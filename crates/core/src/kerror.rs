//! The kernel's error type at the syscall boundary (ROADMAP §10.4, E2).
//!
//! A syscall handler returns `Result<usize, KError>`, and dispatch encodes
//! an `Err` as `-errno` (SYSCALL.md §2). Every module error converts into a
//! `KError` through the `From` impl beside its type, and the one table
//! below holds the Linux errno of each variant. The table is also the
//! input `scripts/gen_syscalls.py` reads to generate SYSCALL.md §2 and the
//! user runtime's errno constants: each row is
//! `Variant = N, "ENAME", "Used text";`, one per line, and a later errno is
//! a new row, never a `const` (`scripts/check_errors.py`).
//!
//! The names and numbers are Linux's, from `include/uapi/asm-generic/
//! errno-base.h` and `include/uapi/asm-generic/errno.h` of the
//! `docs/LINUX.md` baseline, which x86_64 and arm64 share. Only the names
//! and numbers are taken.
//!
//! This module imports no other vibeos module, so `check_cycles.py` sees
//! only edges into it.

/// Expand the errno table into [`KError`] and its accessors. rustc rejects
/// a duplicate discriminant.
macro_rules! errno_table {
    ($($v:ident = $n:literal, $name:literal, $used:literal;)*) => {
        /// An error a syscall returns: one Linux errno.
        #[must_use]
        #[repr(i32)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum KError {
            $(
                #[doc = concat!("Linux `", $name, "` (", stringify!($n), "). ", $used)]
                $v = $n,
            )*
        }

        impl KError {
            /// Every variant, in table order.
            pub const ALL: &[KError] = &[$(KError::$v),*];

            /// The Linux errno, 1 to 4095.
            pub const fn errno(self) -> i32 {
                self as i32
            }

            /// Linux's name for the errno, such as `"EINVAL"`.
            pub const fn name(self) -> &'static str {
                match self {
                    $(KError::$v => $name,)*
                }
            }
        }
    };
}

errno_table! {
    Perm = 1, "EPERM", "`mmap` with `MAP_FIXED` or `MAP_FIXED_NOREPLACE` below `NULL_GUARD_LEN` (page 0); making a symlink, a device node, or a directory, a hard link, a rename, or a removal that the filesystem cannot make, as FAT's `symlink` and `link` (no syscall makes one yet)";
    NoEnt = 2, "ENOENT", "`open`/`execve` missing path";
    Srch = 3, "ESRCH", "`kill`: no such process, a zombie, `pid` 0, or a negative 32-bit `pid` (§3.1)";
    Io = 5, "EIO", "device I/O error; on-disk corruption, a failed checksum or bad magic on FAT or vibefs";
    TooBig = 7, "E2BIG", "`execve`: a string over 131,072 bytes with its NUL, or strings and pointers together over max(128 KiB, min(`RLIMIT_STACK`/4, 6 MiB)), 2 MiB at the fixed 8 MiB `RLIMIT_STACK` (§3.1)";
    NoExec = 8, "ENOEXEC", "malformed ELF, `ET_DYN`, or `PT_INTERP`";
    BadF = 9, "EBADF", "closed / out-of-range fd; `read` on an `O_WRONLY` fd and `write` on an `O_RDONLY` one; a file `mmap` (no `MAP_ANONYMOUS`) with a bad fd";
    Child = 10, "ECHILD", "`wait4` with no matching child";
    Again = 11, "EAGAIN", "`fork` with every process-table slot in use, zombies included (`limits::MAX_PROCS` is 256), or no pid free (pids and tids share one allocator, up to 32,767, then from 300), or the thread table has no free slot (ROADMAP §10.4, F037); `read` of `/dev/random` or `/dev/urandom` when virtio-rng and `RDRAND` supply no byte (ROADMAP §10.12; until §13.10)";
    NoMem = 12, "ENOMEM", "AS clone / load; an image above `limits::EXEC_IMAGE_MAX`; `mmap` with no free range, a full region table (256 regions, `limits::MAX_REGIONS`, where Linux's `vm.max_map_count` allows 65,530; ROADMAP §10.4), a `len` past `USER_MAP_END`, or no frames; a `munmap` that must split a region when the region table is full; a kernel heap allocation that fails in `fork`, `execve`, or `open` (DESIGN §4.4), `execve` argument buffers included";
    Acces = 13, "EACCES", "`open` with `O_CREAT` of a new file in `/dev`, `/proc`, or `/sys`";
    Fault = 14, "EFAULT", "bad user pointer / length";
    Busy = 16, "EBUSY", "`dup2` onto a descriptor an `open` in progress reserved, which no process reaches while each has one thread";
    Exist = 17, "EEXIST", "`O_EXCL`; `mmap` with `MAP_FIXED_NOREPLACE` (or `MAP_FIXED`, §3.1) over a mapping";
    XDev = 18, "EXDEV", "a `rename` or `link` across mounts (no syscall makes one yet)";
    NoDev = 19, "ENODEV", "a file `mmap` (no `MAP_ANONYMOUS`) on an open fd: file mappings come in ROADMAP §12.4";
    NotDir = 20, "ENOTDIR", "";
    IsDir = 21, "EISDIR", "";
    Inval = 22, "EINVAL", "`lseek` with a bad `whence` or a resulting offset below 0, unknown `fcntl` command, `kill` signal 0 or above 31; the `mmap` and `munmap` argument checks in §3.1; `read` or `write` of an object that cannot be read or written; `open` or `execve` of the empty path (Linux: `ENOENT`); `open` with `O_TRUNC` of a `/proc` file";
    NFile = 23, "ENFILE", "`open` or `execve` with the system-wide open-file table full: 1024 open files, `limits::MAX_OPEN_FILES`";
    MFile = 24, "EMFILE", "per-process fd table full: 256 descriptors, `limits::MAX_FDS` (`open`, `dup`)";
    FBig = 27, "EFBIG", "a vibefs `write` that starts at or past the file-size limit, byte 2^44 − 4096 (VIBEFS.md §3); a FAT `write` past 4 GiB, FAT's file-size limit";
    NoSpc = 28, "ENOSPC", "`write` or `open` with `O_CREAT` on a volume out of blocks, inodes, or directory entries, or a vibefs `write` that needs a fifth extent";
    SPipe = 29, "ESPIPE", "`lseek` on the console, `/dev/console`, or `/dev/tty`";
    RoFs = 30, "EROFS", "defined; no syscall returns it: a write to a read-only virtio-blk device fails with it in the block layer";
    NameTooLong = 36, "ENAMETOOLONG", "path of 256 bytes or more; name above 64 bytes. ROADMAP §13.9 moves the path and name limits to Linux's 4096 and 255";
    NoSys = 38, "ENOSYS", "unknown number";
    NotEmpty = 39, "ENOTEMPTY", "defined; no syscall returns it";
    Loop = 40, "ELOOP", "`open` or `execve` through too many symbolic links, or a walk of more than 80 steps (`limits::MAX_WALK`)";
    OpNotSupp = 95, "EOPNOTSUPP", "defined; no syscall returns it. It is left for the cases Linux gives it, such as an extended-attribute namespace a mount refuses (ROADMAP §14.8)";
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each variant's number and name are Linux's (asm-generic `errno-base.h`
    /// and `errno.h`, the same on x86_64 and arm64).
    #[test]
    fn kerror_values_match_linux() {
        let linux: &[(KError, i32, &str)] = &[
            (KError::Perm, 1, "EPERM"),
            (KError::NoEnt, 2, "ENOENT"),
            (KError::Srch, 3, "ESRCH"),
            (KError::Io, 5, "EIO"),
            (KError::TooBig, 7, "E2BIG"),
            (KError::NoExec, 8, "ENOEXEC"),
            (KError::BadF, 9, "EBADF"),
            (KError::Child, 10, "ECHILD"),
            (KError::Again, 11, "EAGAIN"),
            (KError::NoMem, 12, "ENOMEM"),
            (KError::Acces, 13, "EACCES"),
            (KError::Fault, 14, "EFAULT"),
            (KError::Busy, 16, "EBUSY"),
            (KError::Exist, 17, "EEXIST"),
            (KError::XDev, 18, "EXDEV"),
            (KError::NoDev, 19, "ENODEV"),
            (KError::NotDir, 20, "ENOTDIR"),
            (KError::IsDir, 21, "EISDIR"),
            (KError::Inval, 22, "EINVAL"),
            (KError::NFile, 23, "ENFILE"),
            (KError::MFile, 24, "EMFILE"),
            (KError::FBig, 27, "EFBIG"),
            (KError::NoSpc, 28, "ENOSPC"),
            (KError::SPipe, 29, "ESPIPE"),
            (KError::RoFs, 30, "EROFS"),
            (KError::NameTooLong, 36, "ENAMETOOLONG"),
            (KError::NoSys, 38, "ENOSYS"),
            (KError::NotEmpty, 39, "ENOTEMPTY"),
            (KError::Loop, 40, "ELOOP"),
            (KError::OpNotSupp, 95, "EOPNOTSUPP"),
        ];
        assert_eq!(
            linux.len(),
            KError::ALL.len(),
            "a row without a Linux value here"
        );
        for &(e, n, name) in linux {
            assert_eq!(e.errno(), n, "{name}");
            assert_eq!(e.name(), name);
            assert!(KError::ALL.contains(&e), "{name} missing from ALL");
        }
        for (i, a) in KError::ALL.iter().enumerate() {
            assert!((1..=4095).contains(&a.errno()), "{a:?}");
            for b in &KError::ALL[i + 1..] {
                assert_ne!(a.name(), b.name());
            }
        }
    }
}
