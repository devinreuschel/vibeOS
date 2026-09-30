//! Load a static ELF, from the filesystem or from memory, into a new
//! address space. ROADMAP §9.4 / §9.8.

use vibeos::addr_space::{AddressSpace, AsError, UserMemError, UserPerms};
use vibeos::elf::{
    self, AT_BASE, AT_CLKTCK, AT_EGID, AT_ENTRY, AT_EUID, AT_FLAGS, AT_GID, AT_PAGESZ, AT_PHDR,
    AT_PHENT, AT_PHNUM, AT_SECURE, AT_UID, Auxv, Builder, EHDR_SIZE, ElfError, Image, PHDR_SIZE,
};
use vibeos::fs::{FileRef, FsError, O_RDONLY, OpenFlags, SeekFrom};
use vibeos::kalloc::{TryBox, TryVec};
use vibeos::kerror::KError;
use vibeos::paging::PAGE_SIZE_4K;

use crate::addr_space_init;
use crate::file_init;
use crate::thread_init::SpawnError;
use crate::x86;

const STACK_PAGES: u64 = 32;
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
    Mem(UserMemError),
    Empty,
    NoProc,
    /// The process's thread could not be made.
    Spawn(SpawnError),
    /// A kernel heap allocation failed (DESIGN §4.4).
    NoMem,
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
            LoadError::Mem(m) => KError::from(m),
            LoadError::Empty => KError::NoExec,
            LoadError::NoProc => KError::Again,
            LoadError::Spawn(s) => KError::from(s),
            LoadError::NoMem => KError::NoMem,
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
            Self::Mem(_) => "efault",
            Self::Empty => "empty",
            Self::NoProc => "eagain",
            Self::Spawn(e) => e.as_str(),
            Self::NoMem => "enomem",
        }
    }
}

/// A loaded image. The space stays in the box the loader built it in, so
/// the callers, whose frames stay on the stack under the new thread's
/// spawn, never hold or copy it (DESIGN §4.5).
pub struct Loaded {
    pub space: TryBox<AddressSpace>,
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
    space: &mut AddressSpace,
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
        space.write_bytes(va + done, buf).map_err(LoadError::Mem)?;
        done += n as u64;
    }
    Ok(())
}

#[inline(never)]
fn map_loads<S: ImageSource>(
    space: &mut AddressSpace,
    img: &Image,
    src: &mut S,
) -> Result<(), LoadError> {
    let mut top = 0u64;
    for seg in img.loads() {
        top = top.max(seg.vaddr.saturating_add(seg.memsz));
        if seg.memsz == 0 {
            continue;
        }
        let start = elf::page_down(seg.vaddr);
        let end = elf::page_up(seg.vaddr.saturating_add(seg.memsz));
        let len = end - start;
        let perms = UserPerms::from_elf(seg.write, seg.exec);
        // SAFETY: `addr_space_init::map_anon` checks the range is in the
        // user half and clear of every region before it maps anything, and
        // maps from the buddy; established by `addr_space_init::map_anon`.
        match unsafe { addr_space_init::map_anon(space, start, len, perms) } {
            Ok(()) | Err(AsError::Overlap) => {}
            Err(e) => return Err(LoadError::As(e)),
        }
        if seg.filesz != 0 {
            copy_file_bytes(space, src, seg.offset, seg.vaddr, seg.filesz)?;
        }
    }
    // The heap starts on the page after the image, as on Linux with
    // randomization off.
    space.set_brk_start(elf::page_up(top));
    Ok(())
}

#[inline(never)]
fn map_stack(space: &mut AddressSpace, exec: bool) -> Result<(u64, u64), LoadError> {
    let len = STACK_PAGES * PAGE_SIZE_4K;
    let base = STACK_TOP - len;
    let perms = if exec { UserPerms::RWX } else { UserPerms::RW };
    // SAFETY: `addr_space_init::map_anon` checks the range is in the user
    // half and clear of every region before it maps anything; established
    // by `addr_space_init::map_anon`.
    unsafe { addr_space_init::map_anon(space, base, len, perms) }.map_err(LoadError::As)?;
    space.zero_bytes(base, len).map_err(LoadError::Mem)?;
    Ok((base, STACK_TOP))
}

#[inline(never)]
fn setup_tls<S: ImageSource>(
    space: &mut AddressSpace,
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
    // SAFETY: `addr_space_init::map_anon` checks the range is in the user
    // half and clear of every region before it maps anything; established
    // by `addr_space_init::map_anon`.
    unsafe { addr_space_init::map_anon(space, tls_map, map_len, UserPerms::RW) }
        .map_err(LoadError::As)?;
    space.zero_bytes(tls_map, map_len).map_err(LoadError::Mem)?;
    let fs = tls_map + map_len - 8;
    let tls_start = fs - aligned;
    if tls.filesz != 0 {
        copy_file_bytes(space, src, tls.offset, tls_start, tls.filesz)?;
    }
    space
        .write_bytes(fs, &fs.to_le_bytes())
        .map_err(LoadError::Mem)?;
    Ok(fs)
}

fn at_random() -> [u8; 16] {
    let t = x86::lfence_rdtsc();
    let mix = t.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&t.to_le_bytes());
    b[8..].copy_from_slice(&mix.to_le_bytes());
    b
}

#[inline(never)]
fn fill_stack(
    space: &AddressSpace,
    img: &Image,
    argv: &[&[u8]],
    envp: &[&[u8]],
) -> Result<u64, LoadError> {
    let len = (STACK_PAGES * PAGE_SIZE_4K) as usize;
    let mut mem = TryVec::try_with_capacity(len).map_err(|_| LoadError::NoMem)?;
    let zero = [0u8; 256];
    while mem.len() < len {
        let n = (len - mem.len()).min(zero.len());
        mem.try_extend_from_slice(&zero[..n])
            .map_err(|_| LoadError::NoMem)?;
    }
    let mut aux = [
        Auxv {
            tag: AT_PAGESZ,
            val: PAGE_SIZE_4K,
        },
        Auxv {
            tag: AT_ENTRY,
            val: img.entry,
        },
        Auxv {
            tag: AT_PHENT,
            val: img.phentsize as u64,
        },
        Auxv {
            tag: AT_PHNUM,
            val: img.phnum as u64,
        },
        Auxv {
            tag: AT_PHDR,
            val: img.phdr_va.unwrap_or(0),
        },
        Auxv {
            tag: AT_BASE,
            val: 0,
        },
        Auxv {
            tag: AT_FLAGS,
            val: 0,
        },
        Auxv {
            tag: AT_UID,
            val: 0,
        },
        Auxv {
            tag: AT_EUID,
            val: 0,
        },
        Auxv {
            tag: AT_GID,
            val: 0,
        },
        Auxv {
            tag: AT_EGID,
            val: 0,
        },
        Auxv {
            tag: AT_CLKTCK,
            val: 100,
        },
        Auxv {
            tag: AT_SECURE,
            val: 0,
        },
    ];
    if img.phdr_va.is_none() {
        aux[4].val = 0;
    }
    let rsp = elf::build_initial_stack(STACK_TOP, &mut mem[..], argv, envp, &aux, &at_random())
        .map_err(LoadError::Elf)?;
    let base = STACK_TOP - len as u64;
    space.write_bytes(base, &mem).map_err(LoadError::Mem)?;
    Ok(rsp)
}

/// Build a new address space from the file at `path`, with `argv` (or
/// `[path]` when empty) and `envp` on its initial stack. Caller installs
/// it only after this returns.
pub fn load_path<A: AsRef<[u8]>>(
    path: &[u8],
    argv: &[A],
    envp: &[&[u8]],
) -> Result<Loaded, LoadError> {
    #[cfg(feature = "kernel_tests")]
    let before = crate::proc::ktest::free_now();
    let r = load_path_inner(path, argv, envp);
    #[cfg(feature = "kernel_tests")]
    crate::proc::ktest::record(before, r.is_ok());
    r
}

fn load_path_inner<A: AsRef<[u8]>>(
    path: &[u8],
    argv: &[A],
    envp: &[&[u8]],
) -> Result<Loaded, LoadError> {
    let file = file_init::open(path, OpenFlags::from_bits(O_RDONLY), 0).map_err(LoadError::Fs)?;
    let mut src = FileImage {
        file,
        len: 0,
        pos: 0,
    };
    let r = load_file(&mut src, path, argv, envp);
    match file_init::close(src.file) {
        Ok(()) => r,
        Err(e) => {
            if let Ok(loaded) = r {
                drop_space(loaded.space);
            }
            Err(LoadError::Fs(e))
        }
    }
}

/// Load the open file `src` of `path`, mapping each segment from it.
#[inline(never)]
fn load_file<A: AsRef<[u8]>>(
    src: &mut FileImage,
    path: &[u8],
    argv: &[A],
    envp: &[&[u8]],
) -> Result<Loaded, LoadError> {
    let st = file_init::stat(&src.file).map_err(LoadError::Fs)?;
    if st.size == 0 {
        return Err(LoadError::Empty);
    }
    src.len = st.size;
    let mut argv_b =
        TryVec::<&[u8]>::try_with_capacity(argv.len().max(1)).map_err(|_| LoadError::NoMem)?;
    if argv.is_empty() {
        argv_b.try_push(path).map_err(|_| LoadError::NoMem)?;
    }
    for a in argv {
        argv_b.try_push(a.as_ref()).map_err(|_| LoadError::NoMem)?;
    }
    load_from(src, &argv_b, envp)
}

/// Build a new address space from the ELF image `elf`, with `argv` on its
/// initial stack as given. Caller installs it only after this returns.
/// The in-guest tests' loader (C-RING3), over the same `load_from`.
#[cfg(feature = "kernel_tests")]
pub fn load_image(elf: &[u8], argv: &[&[u8]]) -> Result<Loaded, LoadError> {
    load_from(&mut MemImage(elf), argv, &[])
}

/// Build a new address space from the ELF file `src`, with `argv` and
/// `envp` on its initial stack, reading its headers and each segment's file
/// bytes from it as it maps them.
fn load_from<S: ImageSource>(
    src: &mut S,
    argv: &[&[u8]],
    envp: &[&[u8]],
) -> Result<Loaded, LoadError> {
    let img = read_image(src)?;
    // The new space stays on the heap while the image loads, off the
    // stack under which each file read runs its filesystem's frames.
    let mut space = new_space()?;
    let mapped = (|| {
        map_loads(&mut space, &img, src)?;
        let (stack_base, _) = map_stack(&mut space, img.stack_exec)?;
        let fs = setup_tls(&mut space, &img, stack_base, src)?;
        let rsp = fill_stack(&space, &img, argv, envp)?;
        Ok((img.entry, rsp, fs))
    })();
    match mapped {
        Ok((entry, rsp, fs)) => Ok(Loaded {
            space,
            entry,
            rsp,
            fs,
        }),
        Err(e) => {
            drop_space(space);
            Err(e)
        }
    }
}

/// A new user address space, on the heap.
#[inline(never)]
fn new_space() -> Result<TryBox<AddressSpace>, LoadError> {
    // The box first, so a refused allocation leaves nothing to tear down.
    let slot = TryBox::<AddressSpace>::try_new_uninit().map_err(|_| LoadError::NoMem)?;
    let space = addr_space_init::create().ok_or(LoadError::As(AsError::OutOfFrames))?;
    Ok(slot.write(space))
}

/// Unmap and free what a failed load mapped.
#[inline(never)]
fn drop_space(space: TryBox<AddressSpace>) {
    addr_space_init::teardown(space.into_inner());
}
