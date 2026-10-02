//! Every table size and resource cap the kernel enforces, by name (ROADMAP
//! §10.4, D1). The heap tables these lengths size are each built once by
//! [`table`] and never grown, and the old names (`proc::MAX_FDS`,
//! `fs::MAX_INODES`, ...) re-export them. A host test in each portable
//! module that owns a table checks its length against its name here
//! (`fixed_tables_match_limits`). A table that grows as it fills, with
//! only memory as its cap, has no length here: kernfs's node table
//! (`fs::kernfs::KernState`), which returns `ENOMEM` when the heap cannot
//! grow it; what bounds a tmpfs instance's share of it is
//! [`TMPFS_NODE_HEAP_BYTES`].
//!
//! What belongs here: a bound on how many kernel objects or bytes a workload
//! can hold. What stays where it is, because hardware, a device queue, or an
//! on-disk format sets it rather than the kernel's resource policy, is each
//! `MAX_*` that `scripts/check_limits.py`'s `ALLOW` names with that bound:
//! acpi, ipi, pci, dev, pmm, the block, dma and virtio queue geometry, the
//! framebuffer, and the vibefs, FAT and MBR formats. `make check` fails on
//! any other `MAX_*` outside this module.

use crate::kalloc::{AllocError, TryVec};

// The eight table lengths below are the Phase 10 exit gate's (ROADMAP
// §10.4, D1), and `scripts/check_limits.py` holds them to it: each sizes a
// heap table built once by [`table`], never an array type.

/// Thread table slots (`thread_init`'s scheduler, `sched` run and timeout queues).
pub const MAX_THREADS: usize = 1024;
/// Process table slots (`proc_init`), init's and zombies included.
pub const MAX_PROCS: usize = 256;
/// File descriptors per process (`proc::FdTable`, `fs::FdTable`).
pub const MAX_FDS: usize = 256;
/// Open-file table slots, system-wide (`fs::Vfs` files).
pub const MAX_OPEN_FILES: usize = 1024;
/// In-core inode slots (`fs::Vfs`).
pub const MAX_INODES: usize = 1024;
/// Dentry cache slots (`fs::Vfs`).
pub const MAX_DENTRIES: usize = 1024;
/// Mount and superblock slots (`fs::Vfs`).
pub const MAX_MOUNTS: usize = 16;
/// Mapped regions per address space (`addr_space::AddressSpace`): Linux's
/// `vm.max_map_count` is far larger, and `mmap` past this returns `ENOMEM`
/// as Linux's does past its count.
pub const MAX_REGIONS: usize = 256;
/// Cap on an executable image's page-rounded `PT_LOAD` plus `PT_TLS` bytes,
/// which `elf::parse` checks before anything is mapped (ROADMAP §10.6,
/// F009). 1 GiB: above the 192 MiB that the exec test loads into its
/// 128 MiB guest, and the size of the 1-2 GiB window that static `ET_EXEC`
/// images link into.
pub const EXEC_IMAGE_MAX: u64 = 1 << 30;
/// `pid_max`, Linux's default: pids and tids run `1..PID_MAX`
/// (`proc::pid::PidAlloc`, ROADMAP §10.4). ROADMAP §23.4 makes the maximum
/// tunable as `kernel/pid_max`.
pub const PID_MAX: u32 = 32_768;
/// Where pid allocation wraps to past `PID_MAX`, Linux's `RESERVED_PIDS`
/// (`proc::pid::PidAlloc`, ROADMAP §10.4).
pub const PID_WRAP: u32 = 300;
/// RAM-backed node slots (`fs::Vfs` ramfs).
pub const MAX_RAM_NODES: usize = 64;
/// Mounted instances one kernfs store holds, over its four skins
/// (`fs::kernfs::KernState`).
pub const MAX_KERN_MOUNTS: usize = 8;
/// Heap bytes a tmpfs instance's nodes may take at most by default, an
/// eighth of the heap: its `nr_inodes` is half of RAM's pages, as Linux's,
/// but no more nodes than this holds (`fs::kernfs::tmp_nr_inodes_default`),
/// so a full `/tmp` leaves the heap to the kernel.
pub const TMPFS_NODE_HEAP_BYTES: usize = (crate::heap::HEAP_SIZE / 8) as usize;
/// Bytes one ramfs file holds (`fs::RamNode` data).
pub const MAX_TMPFS_FILE_BYTES: usize = 256;
/// Entries one ramfs directory holds (`fs::RamNode` dents).
pub const MAX_TMPFS_DIR_ENTS: usize = 32;
/// Bytes in a path (`fs`, `proc`).
pub const MAX_PATH: usize = 256;
/// Bytes in one path component or file name (`fs::Name`, `fat::Node`).
pub const MAX_NAME: usize = 64;
/// Symlinks one lookup follows (`fs`).
pub const MAX_SYMLINK: u32 = 8;
/// Components one lookup walks, symlinks included (`fs`).
pub const MAX_WALK: u32 = 80;
/// Kernel virtual-address free-list nodes (`kva::Kva`): one per thread stack
/// (`MAX_THREADS`); 8 per CPU (`acpi::MAX_CPUS`), for its 4 IST stacks, its
/// RSP0 stack, its 2 cached stacks and its 1 dead-stack slot; and 64 for
/// `vmap` and tests. `Kva::alloc` refuses once this minus 2 ranges are live,
/// so `Kva::free` never runs out of nodes. `u16` links hold an index, hence
/// the assert below.
pub const MAX_KVA_RANGES: usize = MAX_THREADS + 8 * crate::acpi::MAX_CPUS + 64;
/// Most pages one guarded kernel stack maps, the guard page not counted
/// (`thread::GuardedStack`, `kva_init`).
pub const MAX_STACK_PAGES: usize = 32;
/// virtio-blk disks the driver names, `vda` to `vdz`
/// (`drivers::virtio_blk::disk_name`); a disk past them gets no name.
pub const MAX_VIRTIO_DISKS: u8 = 26;
/// Lock ranks the per-CPU held set tracks: ranks 1 to `MAX_RANK`, rank 0
/// untracked (`sync::lock`).
pub const MAX_RANK: u8 = 8;
/// Pages one deferred unmap batch holds (`kva_init`).
pub const MAX_UNMAP_PAGES: usize = 32;
/// Partitions per disk (`part::Table`, `part_init`).
pub const MAX_PARTS: usize = 16;
/// Registered block devices, partitions included (`block`): boot's two
/// disks with their children, plus a disk and the `MAX_PARTS` children an
/// in-guest partition test registers beside them.
pub const MAX_BLOCKDEVS: usize = 32;
/// Registered drivers (`dev::Registry`).
pub const MAX_DRIVERS: usize = 16;
/// Device claims (`dev::Registry`).
pub const MAX_CLAIMS: usize = 64;
/// Kernel shell commands (`shell::Registry`).
pub const MAX_COMMANDS: usize = 48;
/// Tokens in one kernel shell line (`shell`).
pub const MAX_TOKENS: usize = 16;
/// Limine modules `boot::BootInfo` records; the initrd is the first.
pub const MAX_BOOT_MODULES: usize = 4;
/// `PT_LOAD` segments in one image (`elf::Image`).
pub const MAX_ELF_LOADS: usize = 8;
/// Bytes in one `execve` argument or environment string, its NUL
/// included: Linux's `MAX_ARG_STRLEN`, as execve(2) states it
/// (`elf::ExecArgs`). A longer string is `E2BIG`.
pub const MAX_ARG_STRLEN: usize = 131_072;
/// The least room `execve`'s strings and pointers get together, whatever
/// `RLIMIT_STACK` is (execve(2); `elf::arg_space_limit`).
pub const ARG_SPACE_MIN: usize = 128 << 10;
/// The most room `execve`'s strings and pointers get together, whatever
/// `RLIMIT_STACK` is (execve(2); `elf::arg_space_limit`).
pub const ARG_SPACE_MAX: usize = 6 << 20;
/// `RLIMIT_STACK`, fixed at Linux's 8 MiB default (getrlimit(2)) until
/// ROADMAP §13.9's `setrlimit`: `execve`'s arguments get a quarter of it.
pub const RLIMIT_STACK_DEFAULT: u64 = 8 << 20;

/// A table of `len` entries, each from `fill`, in one allocation of
/// exactly that capacity: the one constructor of every heap table these
/// limits size (ROADMAP §10.4, D1). A table is built once at its full
/// length and never grown, so a cap stays a constant and never becomes an
/// array type.
pub fn table<T>(len: usize, mut fill: impl FnMut() -> T) -> Result<TryVec<T>, AllocError> {
    let mut v = TryVec::try_with_capacity(len)?;
    let mut i = 0usize;
    while i < len {
        // The capacity reserved above holds `len` entries, so no push
        // reallocates.
        v.try_push(fill())?;
        i += 1;
    }
    Ok(v)
}

const _: () = assert!(EXEC_IMAGE_MAX > 192 * 1024 * 1024);
const _: () = assert!(PID_WRAP < PID_MAX);
const _: () = assert!(ARG_SPACE_MIN <= ARG_SPACE_MAX);
const _: () = assert!(MAX_KVA_RANGES <= u16::MAX as usize);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_full_length_and_exact() {
        let mut n = 0u32;
        let t = table(5, || {
            n += 1;
            n
        })
        .unwrap();
        assert_eq!(&t[..], &[1, 2, 3, 4, 5]);
        assert!(t.capacity() >= 5);
        assert!(table(0, || 0u8).unwrap().is_empty());
    }
}
