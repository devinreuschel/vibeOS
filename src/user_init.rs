//! Load a static ELF, from the filesystem or from memory, into a new
//! address space. ROADMAP §9.4 / §9.8.

use vibeos::addr_space::{AddressSpace, AsError, UserMemError, UserPerms};
use vibeos::elf::{
    self, AT_BASE, AT_CLKTCK, AT_EGID, AT_ENTRY, AT_EUID, AT_FLAGS, AT_GID, AT_PAGESZ, AT_PHDR,
    AT_PHENT, AT_PHNUM, AT_SECURE, AT_UID, Auxv, ElfError, Image,
};
use vibeos::fs::{FileRef, FsError, O_RDONLY, OpenFlags};
use vibeos::kalloc::TryVec;
use vibeos::paging::PAGE_SIZE_4K;

use crate::addr_space_init;
use crate::file_init;
use crate::thread_init::SpawnError;
use crate::x86;

use vibeos::limits::MAX_ELF;
const STACK_PAGES: u64 = 32;
const STACK_TOP: u64 = 0x0000_0000_8000_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadError {
    Fs(FsError),
    Elf(ElfError),
    As(AsError),
    Mem(UserMemError),
    TooBig,
    Empty,
    NoProc,
    /// The process's thread could not be made.
    Spawn(SpawnError),
    /// A kernel heap allocation failed (DESIGN §4.4).
    NoMem,
}

impl LoadError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fs(_) => "fs",
            Self::Elf(e) => e.as_str(),
            Self::As(_) => "as",
            Self::Mem(_) => "efault",
            Self::TooBig => "too big",
            Self::Empty => "empty",
            Self::NoProc => "eagain",
            Self::Spawn(e) => e.as_str(),
            Self::NoMem => "enomem",
        }
    }
}

pub struct Loaded {
    pub space: AddressSpace,
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

fn read_path(path: &str) -> Result<TryVec<u8>, LoadError> {
    let f = file_init::open_routed(path.as_bytes(), OpenFlags::from_bits(O_RDONLY), 0)
        .map_err(LoadError::Fs)?;
    let r = read_file(&f);
    let _ = file_init::close(f);
    r
}

/// Read `f` whole, at most its stated size, into a buffer reserved up
/// front, so the reads never reallocate.
fn read_file(f: &FileRef) -> Result<TryVec<u8>, LoadError> {
    let st = file_init::stat(f).map_err(LoadError::Fs)?;
    if st.size == 0 {
        return Err(LoadError::Empty);
    }
    if st.size > MAX_ELF {
        return Err(LoadError::TooBig);
    }
    let size = st.size as usize;
    let mut buf = TryVec::try_with_capacity(size).map_err(|_| LoadError::NoMem)?;
    let mut chunk = [0u8; 512];
    while buf.len() < size {
        let want = (size - buf.len()).min(chunk.len());
        match file_init::read(f, &mut chunk[..want]) {
            Ok(0) => break,
            Ok(k) => buf
                .try_extend_from_slice(&chunk[..k.min(want)])
                .map_err(|_| LoadError::NoMem)?,
            Err(e) => return Err(LoadError::Fs(e)),
        }
    }
    Ok(buf)
}

fn map_loads(space: &mut AddressSpace, img: &Image<'_>) -> Result<(), LoadError> {
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
        match unsafe { addr_space_init::map_anon(space, start, len, perms) } {
            Ok(()) | Err(AsError::Overlap) => {}
            Err(e) => return Err(LoadError::As(e)),
        }
        if seg.filesz != 0 {
            let bytes = img.file_bytes(*seg).map_err(LoadError::Elf)?;
            space
                .write_bytes(seg.vaddr, bytes)
                .map_err(LoadError::Mem)?;
        }
    }
    // The heap starts on the page after the image, as on Linux with
    // randomization off.
    space.set_brk_start(elf::page_up(top));
    Ok(())
}

fn map_stack(space: &mut AddressSpace, exec: bool) -> Result<(u64, u64), LoadError> {
    let len = STACK_PAGES * PAGE_SIZE_4K;
    let base = STACK_TOP - len;
    let perms = if exec { UserPerms::RWX } else { UserPerms::RW };
    unsafe { addr_space_init::map_anon(space, base, len, perms) }.map_err(LoadError::As)?;
    space.zero_bytes(base, len).map_err(LoadError::Mem)?;
    Ok((base, STACK_TOP))
}

fn setup_tls(space: &mut AddressSpace, img: &Image<'_>, stack_base: u64) -> Result<u64, LoadError> {
    let Some(tls) = img.tls else {
        return Ok(0);
    };
    let aligned = align_up(tls.memsz, tls.align.max(1));
    let map_len = tls.map_len().ok_or(LoadError::Elf(ElfError::ImageTooBig))?;
    let tls_map = stack_base.saturating_sub(map_len);
    unsafe { addr_space_init::map_anon(space, tls_map, map_len, UserPerms::RW) }
        .map_err(LoadError::As)?;
    space.zero_bytes(tls_map, map_len).map_err(LoadError::Mem)?;
    let fs = tls_map + map_len - 8;
    let tls_start = fs - aligned;
    if tls.filesz != 0 {
        let start = tls.offset as usize;
        let end = start
            .checked_add(tls.filesz as usize)
            .ok_or(LoadError::Elf(ElfError::Truncated))?;
        let bytes = img
            .data
            .get(start..end)
            .ok_or(LoadError::Elf(ElfError::Truncated))?;
        space
            .write_bytes(tls_start, bytes)
            .map_err(LoadError::Mem)?;
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

fn fill_stack(space: &AddressSpace, img: &Image<'_>, argv: &[&[u8]]) -> Result<u64, LoadError> {
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
    let rsp = elf::build_initial_stack(STACK_TOP, &mut mem[..], argv, &[], &aux, &at_random())
        .map_err(LoadError::Elf)?;
    let base = STACK_TOP - len as u64;
    space.write_bytes(base, &mem).map_err(LoadError::Mem)?;
    Ok(rsp)
}

/// Build a new address space. Caller installs it only after this returns.
pub fn load_path(path: &str, argv: &[&str]) -> Result<Loaded, LoadError> {
    #[cfg(feature = "kernel_tests")]
    let before = testing::free_now();
    let r = load_path_inner(path, argv);
    #[cfg(feature = "kernel_tests")]
    testing::record(before, r.is_ok());
    r
}

fn load_path_inner(path: &str, argv: &[&str]) -> Result<Loaded, LoadError> {
    let bytes = read_path(path)?;
    let mut argv_b =
        TryVec::<&[u8]>::try_with_capacity(argv.len().max(1)).map_err(|_| LoadError::NoMem)?;
    if argv.is_empty() {
        argv_b
            .try_push(path.as_bytes())
            .map_err(|_| LoadError::NoMem)?;
    }
    for a in argv {
        argv_b
            .try_push(a.as_bytes())
            .map_err(|_| LoadError::NoMem)?;
    }
    load_image(&bytes, &argv_b)
}

/// Build a new address space from the ELF image `elf`, with `argv` on its
/// initial stack as given. Caller installs it only after this returns.
pub fn load_image(elf: &[u8], argv: &[&[u8]]) -> Result<Loaded, LoadError> {
    let img = elf::parse(elf).map_err(LoadError::Elf)?;
    let Some(mut space) = addr_space_init::create() else {
        return Err(LoadError::As(AsError::OutOfFrames));
    };
    let mapped = (|| {
        map_loads(&mut space, &img)?;
        let (stack_base, _) = map_stack(&mut space, img.stack_exec)?;
        let fs = setup_tls(&mut space, &img, stack_base)?;
        let rsp = fill_stack(&space, &img, argv)?;
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
            addr_space_init::teardown(space);
            Err(e)
        }
    }
}

#[cfg(feature = "kernel_tests")]
pub(crate) mod testing {
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use crate::pmm_init;

    /// Free-frame counts around one [`super::load_path`] call.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct ExecFrames {
        /// Buddy free frames at entry.
        pub before: usize,
        /// Buddy free frames at return, after a failed load's teardown.
        pub after: usize,
        pub ok: bool,
    }

    const SLOTS: usize = 4;
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    static BEFORE: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
    static AFTER: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
    static OK: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

    pub(super) fn free_now() -> usize {
        pmm_init::with_buddy(|b| b.stats().free_frames)
    }

    pub(super) fn record(before: usize, ok: bool) {
        let after = free_now();
        let i = NEXT.load(Ordering::Relaxed);
        BEFORE[i % SLOTS].store(before as u64, Ordering::Relaxed);
        AFTER[i % SLOTS].store(after as u64, Ordering::Relaxed);
        OK[i % SLOTS].store(u64::from(ok), Ordering::Relaxed);
        NEXT.store(i.wrapping_add(1), Ordering::Release);
    }

    /// Forget the recorded loads.
    pub(crate) fn clear_exec_frames() {
        NEXT.store(0, Ordering::Release);
    }

    /// The last four `load_path` calls since [`clear_exec_frames`], oldest
    /// first.
    pub(crate) fn exec_frames() -> Vec<ExecFrames> {
        let n = NEXT.load(Ordering::Acquire);
        let first = n.saturating_sub(SLOTS);
        (first..n)
            .map(|i| ExecFrames {
                before: BEFORE[i % SLOTS].load(Ordering::Relaxed) as usize,
                after: AFTER[i % SLOTS].load(Ordering::Relaxed) as usize,
                ok: OK[i % SLOTS].load(Ordering::Relaxed) != 0,
            })
            .collect()
    }
}
