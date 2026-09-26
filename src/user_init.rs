//! Load a static ELF, from the filesystem or from memory, into a new
//! address space. ROADMAP §9.4 / §9.8.

use alloc::vec;
use alloc::vec::Vec;

use vibeos::addr_space::{AddressSpace, AsError, UserMemError, UserPerms};
use vibeos::elf::{
    self, AT_BASE, AT_CLKTCK, AT_EGID, AT_ENTRY, AT_EUID, AT_FLAGS, AT_GID, AT_PAGESZ, AT_PHDR,
    AT_PHENT, AT_PHNUM, AT_SECURE, AT_UID, Auxv, ElfError, Image,
};
use vibeos::fs::{FsError, O_RDONLY};
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

fn read_path(path: &str) -> Result<Vec<u8>, LoadError> {
    let st = file_init::stat_path(path).map_err(LoadError::Fs)?;
    if st.size == 0 {
        return Err(LoadError::Empty);
    }
    if st.size > MAX_ELF {
        return Err(LoadError::TooBig);
    }
    let fid = file_init::open(path, O_RDONLY, 0).map_err(LoadError::Fs)?;
    let mut buf = vec![0u8; st.size as usize];
    let mut n = 0usize;
    while n < buf.len() {
        match file_init::read(fid, &mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) => {
                let _ = file_init::close(fid);
                return Err(LoadError::Fs(e));
            }
        }
    }
    let _ = file_init::close(fid);
    buf.truncate(n);
    Ok(buf)
}

fn map_loads(space: &mut AddressSpace, img: &Image<'_>) -> Result<(), LoadError> {
    for seg in img.loads() {
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
        space.zero_bytes(start, len).map_err(LoadError::Mem)?;
        if seg.filesz != 0 {
            let bytes = img.file_bytes(*seg).map_err(LoadError::Elf)?;
            space
                .write_bytes(seg.vaddr, bytes)
                .map_err(LoadError::Mem)?;
        }
    }
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
    let need = aligned.saturating_add(8);
    let map_len = elf::page_up(need.max(PAGE_SIZE_4K));
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
    let mut mem = vec![0u8; len];
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
    let rsp = elf::build_initial_stack(STACK_TOP, &mut mem, argv, &[], &aux, &at_random())
        .map_err(LoadError::Elf)?;
    let base = STACK_TOP - len as u64;
    space.write_bytes(base, &mem).map_err(LoadError::Mem)?;
    Ok(rsp)
}

/// Build a new address space. Caller installs it only after this returns.
pub fn load_path(path: &str, argv: &[&str]) -> Result<Loaded, LoadError> {
    let bytes = read_path(path)?;
    let argv_b: Vec<&[u8]> = if argv.is_empty() {
        vec![path.as_bytes()]
    } else {
        argv.iter().map(|a| a.as_bytes()).collect()
    };
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
