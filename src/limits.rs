//! Every table size and resource cap the kernel enforces, by name (ROADMAP
//! §10.4, D1). Today's fixed tables take their lengths from these constants,
//! and the old names (`proc::MAX_FDS`, `fs::MAX_INODES`, ...) re-export them.
//! A host test in each portable module that owns a fixed table checks its
//! length against its name here (`fixed_tables_match_limits`).
//!
//! What belongs here: a bound on how many kernel objects or bytes a workload
//! can hold. What stays where it is, because hardware, a device queue, or an
//! on-disk format sets it rather than the kernel's resource policy:
//!
//! - hardware: acpi `MAX_CPUS`, `MAX_IOAPICS`, `MAX_ISOS`; ipi
//!   `MAX_IPI_CPUS`; pci `MAX_BARS`, `MAX_SCAN`, `MAX_CAP_WALK`,
//!   `MAX_BAR_MAP`; dev `MAX_DEVICES`; pmm `MAX_ORDER`; pmm_init
//!   `MAX_EXCLUDES`.
//! - device-queue geometry: block `MAX_QUEUE`, `MAX_SEGS`; dma `MAX_SG`;
//!   virtio `MAX_VENDOR_CAPS`, `MAX_CHAIN`; virtio_blk_init `MAX_VQ`,
//!   `MAX_QSIZE`.
//! - on-disk formats: vibefs `MAX_*`, fat `MAX_CLUS_BYTES`, part
//!   `MAX_EBR_DEPTH`.

/// Thread table slots (`thread_init`'s scheduler, `sched` run and timeout queues).
pub const MAX_THREADS: usize = 64;
/// Process table slots (`proc_init`). Pids 2 to 17 leave room for 16 live
/// processes besides init, as ROADMAP §10.10's `exit_burst` runs.
pub const MAX_PROCS: usize = 18;
/// File descriptors per process (`proc::FdTable`, `fs::FdTable`).
pub const MAX_FDS: usize = 16;
/// Open-file table slots, system-wide (`fs::Vfs` files, `file_init`'s table).
pub const MAX_OPEN_FILES: usize = 16;
/// In-core inode slots (`fs::Vfs`).
pub const MAX_INODES: usize = 48;
/// Dentry cache slots (`fs::Vfs`).
pub const MAX_DENTRIES: usize = 48;
/// Mount and superblock slots (`fs::Vfs`).
pub const MAX_MOUNTS: usize = 8;
/// Mapped regions per address space (`addr_space::AddressSpace`).
pub const MAX_REGIONS: usize = 32;
/// Cap on an executable image's page-rounded `PT_LOAD` plus `PT_TLS` bytes.
/// Declared here; ROADMAP §10.6's exec box sets the value and the loader reads it.
pub const EXEC_IMAGE_MAX: u64 = 256 * 1024 * 1024;
/// `pid_max`: pids and tids count up to it, Linux's default (ROADMAP §10.4).
pub const PID_MAX: u32 = 32_768;
/// Where pid allocation wraps to, Linux's `RESERVED_PIDS` (ROADMAP §10.4).
pub const PID_WRAP: u32 = 300;
/// RAM-backed node slots (`fs::Vfs` ramfs).
pub const MAX_RAM_NODES: usize = 64;
/// Kernel pseudo-filesystem node slots (`fs::kernfs`).
pub const MAX_KERN_NODES: usize = 128;
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
/// Kernel virtual-address free-list nodes (`kva::Kva`). A `u8` holds the
/// count (`kva::Kva`'s `nslots`), hence the assert below.
pub const MAX_KVA_RANGES: usize = 128;
/// Pages one deferred unmap batch holds (`kva_init`).
pub const MAX_UNMAP_PAGES: usize = 32;
/// Mounted FAT volumes (`fat_init`).
pub const MAX_FAT_VOLS: usize = 2;
/// Mounted vibefs volumes (`vibefs_init`).
pub const MAX_VIBEFS_VOLS: usize = 2;
/// In-core inode slots per FAT volume (`fat::FatVol`).
pub const MAX_FAT_INODES: usize = 96;
/// Partitions per disk (`part::Table`, `part_init`).
pub const MAX_PARTS: usize = 16;
/// Registered block devices (`block`).
pub const MAX_BLOCKDEVS: usize = 8;
/// Registered drivers (`dev::Registry`).
pub const MAX_DRIVERS: usize = 16;
/// Device claims (`dev::Registry`).
pub const MAX_CLAIMS: usize = 64;
/// Kernel shell commands (`shell::Registry`).
pub const MAX_COMMANDS: usize = 48;
/// Tokens in one kernel shell line (`shell`).
pub const MAX_TOKENS: usize = 16;
/// Bytes of an ELF file `user_init` loads from the initrd.
pub const MAX_ELF: u64 = 64 * 1024;
/// `PT_LOAD` segments in one image (`elf::Image`).
pub const MAX_ELF_LOADS: usize = 8;

const _: () = assert!(EXEC_IMAGE_MAX > 192 * 1024 * 1024);
const _: () = assert!(PID_WRAP < PID_MAX);
const _: () = assert!(MAX_KVA_RANGES <= u8::MAX as usize);
