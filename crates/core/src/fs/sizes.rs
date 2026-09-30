//! The lengths of a VFS's heap tables (ROADMAP §10.4, D1): the kernel's
//! from `limits`, and the small ones the host tests use.

use super::Vfs;
#[cfg(test)]
use super::words_table;
use crate::limits;

/// The lengths of a [`Vfs`]'s tables. The kernel's are [`VfsSizes::KERNEL`],
/// from `limits`; host tests use small ones, so eviction pressure is easy
/// to reach. Each is at least 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VfsSizes {
    pub inodes: usize,
    pub dentries: usize,
    /// Mount and superblock slots.
    pub mounts: usize,
    /// Open-file slots, system-wide.
    pub files: usize,
}

impl VfsSizes {
    /// The kernel's: `limits::MAX_INODES`, `MAX_DENTRIES`, `MAX_MOUNTS` and
    /// `MAX_OPEN_FILES` (ROADMAP §10.4, D1).
    pub const KERNEL: Self = Self {
        inodes: limits::MAX_INODES,
        dentries: limits::MAX_DENTRIES,
        mounts: limits::MAX_MOUNTS,
        files: limits::MAX_OPEN_FILES,
    };
}

impl Vfs {
    /// The lengths of this VFS's tables.
    pub fn sizes(&self) -> VfsSizes {
        VfsSizes {
            inodes: self.inodes.len(),
            dentries: self.dentries.len(),
            mounts: self.mounts.len(),
            files: self.files.len(),
        }
    }
}

/// The table lengths the host tests' VFS gets: small, so eviction
/// pressure is easy to reach.
#[cfg(test)]
pub(crate) const SMALL: VfsSizes = VfsSizes {
    inodes: 48,
    dentries: 48,
    mounts: 8,
    files: 16,
};

/// A host test's VFS of [`SMALL`] sizes, with a words table of its own,
/// leaked.
#[cfg(test)]
pub(crate) fn host_vfs() -> Vfs {
    let words = std::boxed::Box::leak(std::boxed::Box::new(words_table(SMALL.inodes).unwrap()));
    Vfs::new(&SMALL, words).unwrap()
}
