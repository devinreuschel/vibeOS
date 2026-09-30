//! The filesystem error (ROADMAP §10.4, E2): one type for the VFS and
//! every backend, FAT's `FatError` and vibefs's `Error` included, which
//! converts into `KError` through one `From`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum FsError {
    NotFound,
    Exists,
    NotDir,
    IsDir,
    Inval,
    NoSpace,
    Loop,
    NameTooLong,
    NotEmpty,
    Busy,
    Badf,
    NotSupp,
    Io,
    /// Past a filesystem's maximum file size.
    FileTooBig,
    /// A kernel heap allocation failed (DESIGN §4.4).
    NoMem,
    /// Nothing to return now, and the caller may try again: `/dev/random`
    /// when no hardware source has a byte (ROADMAP §10.12).
    Again,
    /// On-disk data failed a check: a bad checksum, magic, or structure
    /// (FAT, vibefs).
    Corrupt,
    /// The system-wide open-file table is full.
    NFile,
    /// The filesystem cannot make this kind of object or link.
    Perm,
    /// The object cannot seek.
    SPipe,
    /// The two paths are on different mounts.
    XDev,
}

/// A filesystem error's Linux errno at the syscall boundary (SYSCALL.md §2).
impl From<FsError> for crate::kerror::KError {
    fn from(e: FsError) -> Self {
        match e {
            FsError::NotFound => Self::NoEnt,
            FsError::Exists => Self::Exist,
            FsError::NotDir => Self::NotDir,
            FsError::IsDir => Self::IsDir,
            FsError::Inval => Self::Inval,
            FsError::NoSpace => Self::NoSpc,
            FsError::NFile => Self::NFile,
            FsError::FileTooBig => Self::FBig,
            FsError::Loop => Self::Loop,
            FsError::NameTooLong => Self::NameTooLong,
            FsError::NotEmpty => Self::NotEmpty,
            FsError::Busy => Self::Busy,
            FsError::Badf => Self::BadF,
            FsError::NotSupp => Self::OpNotSupp,
            FsError::Io | FsError::Corrupt => Self::Io,
            FsError::Perm => Self::Perm,
            FsError::SPipe => Self::SPipe,
            FsError::XDev => Self::XDev,
            FsError::NoMem => Self::NoMem,
            FsError::Again => Self::Again,
        }
    }
}

impl FsError {
    pub fn as_str(self) -> &'static str {
        match self {
            FsError::NotFound => "not found",
            FsError::Exists => "exists",
            FsError::NotDir => "not dir",
            FsError::IsDir => "is dir",
            FsError::Inval => "inval",
            FsError::NoSpace => "no space",
            FsError::Loop => "loop",
            FsError::NameTooLong => "name too long",
            FsError::NotEmpty => "not empty",
            FsError::Busy => "busy",
            FsError::Badf => "badf",
            FsError::NotSupp => "not supp",
            FsError::Io => "io",
            FsError::FileTooBig => "file too big",
            FsError::NoMem => "no memory",
            FsError::Again => "again",
            FsError::Corrupt => "corrupt",
            FsError::NFile => "file table full",
            FsError::Perm => "not permitted",
            FsError::SPipe => "cannot seek",
            FsError::XDev => "cross-device",
        }
    }
}
