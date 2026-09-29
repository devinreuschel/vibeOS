//! Filesystems: the kernel half of subsystem `fs` (DESIGN §1.3).

pub(crate) mod fat_init;
pub(crate) mod file_init;
pub(crate) mod fs_init;
pub(crate) mod vibefs_init;
