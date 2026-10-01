//! Load a static ELF, from the filesystem or from memory, into a new
//! address space. ROADMAP §9.4 / §9.8.

use vibeos::addr_space::{AsError, UserPerms};
use vibeos::arch::CycleCounter;
use vibeos::elf::{
    self, AT_BASE, AT_CLKTCK, AT_EGID, AT_ENTRY, AT_EUID, AT_FLAGS, AT_GID, AT_PAGESZ, AT_PHDR,
    AT_PHENT, AT_PHNUM, AT_SECURE, AT_UID, ArgError, Auxv, Builder, EHDR_SIZE, ElfError, ExecArgs,
    Image, LoadSeg, LoadTarget, PHDR_SIZE, PageRun,
};
use vibeos::fs::{FileRef, FsError, O_RDONLY, OpenFlags, SeekFrom, WalkBase};
use vibeos::kalloc::TryVec;
use vibeos::kerror::KError;
use vibeos::limits::RLIMIT_STACK_DEFAULT;
use vibeos::paging::PAGE_SIZE_4K;
use vibeos::proc::fill::FillError;

use crate::addr_space_init;
use crate::addr_space_init::{NewSpace, Space};
use crate::arch::current::Arch;
use crate::file_init;
use crate::fill_init;
use crate::thread_init::SpawnError;

/// Pages mapped below the initial stack's arguments and table, for the
/// program's own frames: 128 KiB.
const STACK_HEADROOM_PAGES: u64 = 32;
const STACK_TOP: u64 = 0x0000_0000_8000_0000;

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "vibefs_crash",
    allow(dead_code, reason = "the vibefs_crash build spawns no process")
)]
pub enum LoadError {
    Fs(FsError),
    Elf(ElfError),
    As(AsError),
    /// A fill into the new space failed (`fill_init`).
    Fill(FillError),
    Empty,
    NoProc,
    /// The process's thread could not be made.
    Spawn(SpawnError),
    /// A kernel heap allocation failed (DESIGN §4.4).
    NoMem,
    /// The arguments were over a limit, or their buffer could not grow.
    Args(ArgError),
}

/// A load's errno: the loader's, the filesystem's, or the address space's
/// error, as `execve` returns it.
impl From<LoadError> for KError {
    fn from(e: LoadError) -> Self {
        match e {
            LoadError::Fs(f) => KError::from(f),
            LoadError::Elf(ElfError::ImageTooBig) => KError::NoMem,
            LoadError::Elf(e) => KError::from(e),
            LoadError::As(_) => KError::NoMem,
            LoadError::Fill(f) => KError::from(f),
            LoadError::Empty => KError::NoExec,
            LoadError::NoProc => KError::Again,
            LoadError::Spawn(s) => KError::from(s),
            LoadError::NoMem => KError::NoMem,
            LoadError::Args(a) => KError::from(a),
        }
    }
}

impl LoadError {
    #[cfg_attr(
        feature = "vibefs_crash",
        allow(dead_code, reason = "the vibefs_crash build spawns no process")
    )]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fs(_) => "fs",
            Self::Elf(e) => e.as_str(),
            Self::As(_) => "as",
            Self::Fill(FillError::NoMem) => "enomem",
            Self::Fill(_) => "efault",
            Self::Empty => "empty",
            Self::NoProc => "eagain",
            Self::Spawn(e) => e.as_str(),
            Self::NoMem => "enomem",
            Self::Args(ArgError::TooBig) => "e2big",
            Self::Args(ArgError::NoMem) => "enomem",
        }
    }
}

/// A loaded image, its space published for the caller to install. The
/// space is one counted reference, so the callers, whose frames stay on
/// the stack under the new thread's spawn, hold a pointer (DESIGN §4.5).
pub struct Loaded {
    pub space: Space,
    pub entry: u64,
    pub rsp: u64,
    pub fs: u64,
}

fn align_up(x: u64, a: u64) -> u64 {
    if a <= 1 {
        x
    } else {
        x.saturating_add(a - 1) & !(a - 1)
    }
}

/// Where the loader reads an ELF file from: the open file, or an image in
/// memory (C-RING3's `load_image`). One loader serves both (AGENTS.md
/// rule 10).
trait ImageSource {
    /// The file's length in bytes.
    fn len(&self) -> u64;
    /// Fill `buf` with the bytes at `off`. A short read, where the file
    /// ended first, is `ElfError::Truncated`.
    fn read_exact_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), LoadError>;
}

/// An ELF image in memory: the in-guest tests' ring-3 images (C-RING3).
#[cfg(feature = "kernel_tests")]
struct MemImage<'a>(&'a [u8]);

#[cfg(feature = "kernel_tests")]
impl ImageSource for MemImage<'_> {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_exact_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), LoadError> {
        let bytes = usize::try_from(off)
            .ok()
            .and_then(|lo| Some(lo..lo.checked_add(buf.len())?))
            .and_then(|r| self.0.get(r))
            .ok_or(LoadError::Elf(ElfError::Truncated))?;
        buf.copy_from_slice(bytes);
        Ok(())
    }
}

/// An open ELF file of `len` bytes, whose file position is `pos`.
struct FileImage {
    file: FileRef,
    len: u64,
    pos: u64,
}

impl ImageSource for FileImage {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_exact_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), LoadError> {
        if off != self.pos {
            self.pos = file_init::seek(&self.file, SeekFrom::Start(off)).map_err(LoadError::Fs)?;
        }
        let mut done = 0usize;
        while let Some(rest) = buf.get_mut(done..).filter(|r| !r.is_empty()) {
            // A file that shrank since `stat` ends early: not an ELF image.
            let n = match file_init::read(&self.file, rest).map_err(LoadError::Fs)? {
                0 => return Err(LoadError::Elf(ElfError::Truncated)),
                n => n.min(rest.len()),
            };
            done += n;
            self.pos += n as u64;
        }
        Ok(())
    }
}

/// Program headers read per batch: 8 records, 448 bytes on the stack
/// whatever `phnum` the file claims.
const PH_BATCH: usize = 8;

/// Read the ELF header and program headers of `src`, in batches of
/// [`PH_BATCH`], and check them against its length.
#[inline(never)]
fn read_image<S: ImageSource>(src: &mut S) -> Result<Image, LoadError> {
    let file_len = src.len();
    let mut eh = [0u8; EHDR_SIZE];
    src.read_exact_at(0, &mut eh)?;
    let eh = elf::parse_ehdr(&eh, file_len).map_err(LoadError::Elf)?;
    let mut b = Builder::new(eh, file_len);
    let mut batch = [0u8; PH_BATCH * PHDR_SIZE];
    let mut done = 0usize;
    let phnum = usize::from(eh.phnum);
    while done < phnum {
        let n = (phnum - done).min(PH_BATCH);
        let off = (done * PHDR_SIZE) as u64;
        let off = eh
            .phoff
            .checked_add(off)
            .ok_or(LoadError::Elf(ElfError::Truncated))?;
        let buf = &mut batch[..n * PHDR_SIZE];
        src.read_exact_at(off, buf)?;
        for ph in buf.as_chunks::<PHDR_SIZE>().0 {
            b.push(ph).map_err(LoadError::Elf)?;
        }
        done += n;
    }
    b.finish().map_err(LoadError::Elf)
}

/// Copy `len` file bytes at `off` of `src` to user address `va` of
/// `space`, through a 512-byte stack buffer.
#[inline(never)]
fn copy_file_bytes<S: ImageSource>(
    space: &mut NewSpace,
    src: &mut S,
    off: u64,
    va: u64,
    len: u64,
) -> Result<(), LoadError> {
    let mut chunk = [0u8; 512];
    let mut done = 0u64;
    while done < len {
        let n = (len - done).min(chunk.len() as u64) as usize;
        let buf = &mut chunk[..n];
        src.read_exact_at(off + done, buf)?;
        fill_init::write(space, va + done, buf).map_err(LoadError::Fill)?;
        done += n as u64;
    }
    Ok(())
}

/// The loader's [`LoadTarget`]: a new address space, filled from `src`.
struct SpaceTarget<'a, S> {
    space: &'a mut NewSpace,
    src: &'a mut S,
}

impl<S: ImageSource> LoadTarget for SpaceTarget<'_, S> {
    type Error = LoadError;

    fn map_zeroed(&mut self, run: PageRun) -> Result<(), LoadError> {
        let perms = UserPerms::from_elf(run.write, run.exec);
        fill_init::map(self.space, run.start, run.len, perms).map_err(LoadError::As)
    }

    fn copy(&mut self, seg: LoadSeg) -> Result<(), LoadError> {
        copy_file_bytes(self.space, self.src, seg.offset, seg.vaddr, seg.filesz)
    }
}

/// Map `img`'s `PT_LOAD`s as Linux does: `elf::load_plan`'s runs, each
/// mapped zeroed once with the permissions of the last segment covering
/// it, then every segment's file bytes, so a page two segments share holds
/// both (ROADMAP §10.6, F031). A run over any mapping is an error.
#[inline(never)]
fn map_loads<S: ImageSource>(
    space: &mut NewSpace,
    img: &Image,
    src: &mut S,
) -> Result<(), LoadError> {
    elf::load_segments(img, &mut SpaceTarget { space, src })?;
    let top = img
        .loads()
        .iter()
        .map(|seg| seg.vaddr.saturating_add(seg.memsz))
        .max()
        .unwrap_or(0);
    // The heap starts on the page after the image, as on Linux with
    // randomization off.
    space.mm().set_brk_start(elf::page_up(top));
    Ok(())
}

/// Map `len` bytes of stack below `STACK_TOP`, zeroed. Returns its base
/// and top.
#[inline(never)]
fn map_stack(space: &mut NewSpace, exec: bool, len: u64) -> Result<(u64, u64), LoadError> {
    let base = STACK_TOP
        .checked_sub(len)
        .ok_or(LoadError::Elf(ElfError::Stack))?;
    let perms = if exec { UserPerms::RWX } else { UserPerms::RW };
    fill_init::map(space, base, len, perms).map_err(LoadError::As)?;
    fill_init::zero(space, base, len).map_err(LoadError::Fill)?;
    Ok((base, STACK_TOP))
}

#[inline(never)]
fn setup_tls<S: ImageSource>(
    space: &mut NewSpace,
    img: &Image,
    stack_base: u64,
    src: &mut S,
) -> Result<u64, LoadError> {
    let Some(tls) = img.tls else {
        return Ok(0);
    };
    let aligned = align_up(tls.memsz, tls.align.max(1));
    let map_len = tls.map_len().ok_or(LoadError::Elf(ElfError::ImageTooBig))?;
    let tls_map = stack_base.saturating_sub(map_len);
    fill_init::map(space, tls_map, map_len, UserPerms::RW).map_err(LoadError::As)?;
    fill_init::zero(space, tls_map, map_len).map_err(LoadError::Fill)?;
    let fs = tls_map + map_len - 8;
    let tls_start = fs - aligned;
    if tls.filesz != 0 {
        copy_file_bytes(space, src, tls.offset, tls_start, tls.filesz)?;
    }
    fill_init::write(space, fs, &fs.to_le_bytes()).map_err(LoadError::Fill)?;
    Ok(fs)
}

fn at_random() -> [u8; 16] {
    let t = <Arch as CycleCounter>::now();
    let mix = t.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&t.to_le_bytes());
    b[8..].copy_from_slice(&mix.to_le_bytes());
    b
}

/// The auxiliary vector's entries before `AT_RANDOM` and `AT_NULL`.
const NAUX: usize = 13;

fn auxv(img: &Image) -> [Auxv; NAUX] {
    let a = |tag, val| Auxv { tag, val };
    [
        a(AT_PAGESZ, PAGE_SIZE_4K),
        a(AT_ENTRY, img.entry),
        a(AT_PHENT, img.phentsize as u64),
        a(AT_PHNUM, img.phnum as u64),
        a(AT_PHDR, img.phdr_va.unwrap_or(0)),
        a(AT_BASE, 0),
        a(AT_FLAGS, 0),
        a(AT_UID, 0),
        a(AT_EUID, 0),
        a(AT_GID, 0),
        a(AT_EGID, 0),
        a(AT_CLKTCK, 100),
        a(AT_SECURE, 0),
    ]
}

/// Bytes the stack maps for `args`: the arguments, their pointers and the
/// auxiliary vector, page-rounded, plus [`STACK_HEADROOM_PAGES`].
fn stack_len(args: &ExecArgs) -> Result<u64, LoadError> {
    elf::initial_stack_len(args, NAUX)
        .and_then(|n| u64::try_from(n).ok())
        .and_then(|n| elf::page_up(n).checked_add(STACK_HEADROOM_PAGES * PAGE_SIZE_4K))
        .ok_or(LoadError::Elf(ElfError::Stack))
}

/// Write the initial stack for `args` below `STACK_TOP`: the table built
/// in a per-call buffer at RSP, then the strings straight from `args`, so
/// no second copy of them is made. Returns RSP.
#[inline(never)]
fn fill_stack(space: &mut NewSpace, img: &Image, args: &ExecArgs) -> Result<u64, LoadError> {
    let aux = auxv(img);
    let table_len = elf::initial_stack_len(args, NAUX)
        .and_then(|n| n.checked_sub(8)?.checked_sub(args.strings().len()))
        .ok_or(LoadError::Elf(ElfError::Stack))?;
    let mut table = TryVec::try_with_capacity(table_len).map_err(|_| LoadError::NoMem)?;
    let zero = [0u8; 256];
    while table.len() < table_len {
        let n = (table_len - table.len()).min(zero.len());
        table
            .try_extend_from_slice(&zero[..n])
            .map_err(|_| LoadError::NoMem)?;
    }
    let st = elf::build_initial_stack(STACK_TOP, args, &aux, &at_random(), &mut table)
        .map_err(LoadError::Elf)?;
    fill_init::write(space, st.rsp, &table).map_err(LoadError::Fill)?;
    fill_init::write(space, st.strings_va, args.strings()).map_err(LoadError::Fill)?;
    Ok(st.rsp)
}

/// An argument block holding `argv` and `envp` as given, at the default
/// limit (SYSCALL.md §3.1): the kernel's own spawns' (`spawn_elf`, and
/// C-RING3's `load_image`).
#[cfg_attr(
    feature = "vibefs_crash",
    allow(dead_code, reason = "the vibefs_crash build spawns no process")
)]
pub fn exec_args(argv: &[&[u8]], envp: &[&[u8]]) -> Result<ExecArgs, LoadError> {
    let mut args = ExecArgs::new(elf::arg_space_limit(RLIMIT_STACK_DEFAULT));
    for a in argv {
        args.push_arg(a).map_err(LoadError::Args)?;
    }
    args.finish_argv().map_err(LoadError::Args)?;
    for e in envp {
        args.push_env(e).map_err(LoadError::Args)?;
    }
    Ok(args)
}

/// Build a new address space from the file at `path`, resolved from
/// `base`, with `args` on its initial stack. Caller installs it only after this returns.
pub fn load_path(
    base: Option<WalkBase>,
    path: &[u8],
    args: &ExecArgs,
) -> Result<Loaded, LoadError> {
    #[cfg(feature = "kernel_tests")]
    let before = crate::proc::ktest::free_now();
    let r = load_path_inner(base, path, args);
    #[cfg(feature = "kernel_tests")]
    crate::proc::ktest::record(before, r.is_ok());
    r
}

fn load_path_inner(
    base: Option<WalkBase>,
    path: &[u8],
    args: &ExecArgs,
) -> Result<Loaded, LoadError> {
    let file =
        file_init::open_at(base, path, OpenFlags::from_bits(O_RDONLY), 0).map_err(LoadError::Fs)?;
    let mut src = FileImage {
        file,
        len: 0,
        pos: 0,
    };
    let r = load_file(&mut src, args);
    match file_init::close(src.file) {
        Ok(()) => r,
        Err(e) => {
            // A loaded space's last `users` put tears it down.
            drop(r);
            Err(LoadError::Fs(e))
        }
    }
}

/// Load the open file `src`, mapping each segment from it.
#[inline(never)]
fn load_file(src: &mut FileImage, args: &ExecArgs) -> Result<Loaded, LoadError> {
    let st = file_init::stat(&src.file).map_err(LoadError::Fs)?;
    if st.size == 0 {
        return Err(LoadError::Empty);
    }
    src.len = st.size;
    load_from(src, args)
}

/// Build a new address space from the ELF image `elf`, with `argv` on its
/// initial stack as given and an empty environment. Caller installs it
/// only after this returns. The in-guest tests' loader (C-RING3), over
/// the same `load_from`.
#[cfg(feature = "kernel_tests")]
pub fn load_image(elf: &[u8], argv: &[&[u8]]) -> Result<Loaded, LoadError> {
    let args = exec_args(argv, &[])?;
    load_from(&mut MemImage(elf), &args)
}

/// Build a new address space from the ELF file `src`, with `args` on its
/// initial stack, reading its headers and each segment's file bytes from
/// it as it maps them.
fn load_from<S: ImageSource>(src: &mut S, args: &ExecArgs) -> Result<Loaded, LoadError> {
    let img = read_image(src)?;
    let stack = stack_len(args)?;
    // The new space is one heap object; this frame holds a pointer to it,
    // off the stack under which each file read runs its filesystem's frames.
    let mut space = addr_space_init::create().map_err(LoadError::As)?;
    let mapped = (|| {
        map_loads(&mut space, &img, src)?;
        let (stack_base, _) = map_stack(&mut space, img.stack_exec, stack)?;
        let fs = setup_tls(&mut space, &img, stack_base, src)?;
        let rsp = fill_stack(&mut space, &img, args)?;
        Ok((img.entry, rsp, fs))
    })();
    // A failure drops `space` here: its last `users` put unmaps and frees
    // what the load mapped.
    let (entry, rsp, fs) = mapped?;
    Ok(Loaded {
        space: space.publish(),
        entry,
        rsp,
        fs,
    })
}
