//! In-memory ring-3 programs for in-guest tests (ROADMAP §10.6, C-RING3).
//!
//! A test builds its program with [`user_code!`] or hands in an ELF image,
//! and [`spawn`] starts it as a process whose parent is the kernel (ppid
//! 0); [`wait`] reaps it. No initrd file and no `user/*.asm` is involved.

use alloc::vec;
use alloc::vec::Vec;

use vibeos::elf::{
    EHDR_SIZE, ELFCLASS64, ELFDATA2LSB, ELFMAG0, EM_X86_64, ET_EXEC, EV_CURRENT, PF_R, PF_W, PF_X,
    PHDR_SIZE, PT_LOAD,
};

use crate::proc_init;
use crate::thread_init;
use crate::time_init;
use crate::user_init::LoadError;

/// Where a [`user_code!`] program is loaded.
#[derive(Clone, Copy)]
pub(crate) struct Layout {
    /// Load address of the program's page, and its entry point.
    pub vaddr: u64,
    /// Segment size in memory when larger than the 4096-byte program.
    pub memsz: Option<u64>,
    /// Map the segment writable as well as readable and executable.
    pub writable: bool,
}

pub(crate) const DEFAULT: Layout = Layout {
    vaddr: 0x4000_0000,
    memsz: None,
    writable: false,
};

impl Default for Layout {
    fn default() -> Self {
        DEFAULT
    }
}

/// A ring-3 program: code from [`user_code!`] at a [`Layout`], or a whole
/// ELF image.
pub(crate) enum Image {
    Code(&'static [u8], Layout),
    Elf(&'static [u8]),
}

/// File offset of the one `PT_LOAD` segment: the code starts on its own page.
const CODE_OFFSET: usize = 0x1000;

/// A minimal `ET_EXEC` image: header, one `PT_LOAD` for `code` at
/// `layout.vaddr`, zero padding to [`CODE_OFFSET`], then the code.
fn code_elf(code: &[u8], layout: &Layout) -> Vec<u8> {
    let mut b = vec![0u8; CODE_OFFSET + code.len()];
    b[0] = ELFMAG0;
    b[1..4].copy_from_slice(b"ELF");
    b[4] = ELFCLASS64;
    b[5] = ELFDATA2LSB;
    b[6] = EV_CURRENT;
    b[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
    b[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
    b[20..24].copy_from_slice(&1u32.to_le_bytes());
    b[24..32].copy_from_slice(&layout.vaddr.to_le_bytes());
    b[32..40].copy_from_slice(&(EHDR_SIZE as u64).to_le_bytes());
    b[52..54].copy_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
    b[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
    b[56..58].copy_from_slice(&1u16.to_le_bytes());
    let mut flags = PF_R | PF_X;
    if layout.writable {
        flags |= PF_W;
    }
    let len = code.len() as u64;
    let ph = EHDR_SIZE;
    b[ph..ph + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
    b[ph + 4..ph + 8].copy_from_slice(&flags.to_le_bytes());
    b[ph + 8..ph + 16].copy_from_slice(&(CODE_OFFSET as u64).to_le_bytes());
    b[ph + 16..ph + 24].copy_from_slice(&layout.vaddr.to_le_bytes());
    b[ph + 24..ph + 32].copy_from_slice(&layout.vaddr.to_le_bytes());
    b[ph + 32..ph + 40].copy_from_slice(&len.to_le_bytes());
    b[ph + 40..ph + 48].copy_from_slice(&len.max(layout.memsz.unwrap_or(0)).to_le_bytes());
    b[ph + 48..ph + 56].copy_from_slice(&0x1000u64.to_le_bytes());
    b[CODE_OFFSET..].copy_from_slice(code);
    b
}

/// The ELF bytes of `img`.
pub(crate) fn elf_bytes(img: &Image) -> Vec<u8> {
    match img {
        Image::Code(code, layout) => code_elf(code, layout),
        Image::Elf(elf) => elf.to_vec(),
    }
}

/// Start `img` with `argv` as a process whose parent is the kernel (ppid
/// 0). Every caller also calls [`wait`]: an unwaited ppid-0 zombie keeps
/// its slot for the whole boot.
pub(crate) fn spawn(img: &Image, argv: &[&str]) -> Result<u32, LoadError> {
    let argv_b: Vec<&[u8]> = argv.iter().map(|a| a.as_bytes()).collect();
    match img {
        Image::Code(code, layout) => proc_init::spawn_image(&code_elf(code, layout), &argv_b, 0),
        Image::Elf(elf) => proc_init::spawn_image(elf, &argv_b, 0),
    }
}

/// Block until `pid` exits, reap it, and return its `wait4` status word.
/// Returns before its address space and kernel stack are freed; a test
/// that counts frames calls [`frames_settle`] next.
pub(crate) fn wait(pid: u32) -> u32 {
    proc_init::wait_kernel(pid)
}

/// [`spawn`], then [`wait`].
pub(crate) fn run(img: &Image, argv: &[&str]) -> Result<u32, LoadError> {
    Ok(wait(spawn(img, argv)?))
}

/// Yield until the free-frame count is back to `before`, for at most 1 s.
/// TSC time, since ticks stop while the registry holds IF off.
pub(crate) fn frames_settle(before: usize) -> bool {
    let deadline = time_init::now_ns().saturating_add(1_000_000_000);
    loop {
        if super::free_frames() == before {
            return true;
        }
        if time_init::now_ns() >= deadline {
            return false;
        }
        thread_init::yield_now();
    }
}

/// `user_code!(NAME, "<intel asm>")` assembles position-independent code
/// into one int3-padded 4096-byte page of `.rodata` and defines
/// `static NAME: &[u8]` over it. Use numeric local labels and no braces;
/// `NAME` is unique in the crate.
macro_rules! user_code {
    ($name:ident, $asm:literal) => {
        ::core::arch::global_asm!(concat!(
            ".pushsection .rodata.vibeos_user_code, \"a\", @progbits\n.balign 16\n",
            ".global vibeos_user_code_",
            stringify!($name),
            "\n",
            "vibeos_user_code_",
            stringify!($name),
            ":\n",
            $asm,
            "\n",
            // One page, padded with int3; the assembler rejects a program over 4096 bytes.
            ".org vibeos_user_code_",
            stringify!($name),
            " + 4096, 0xcc\n.popsection\n"
        ));
        static $name: &[u8] = {
            unsafe extern "C" {
                #[link_name = concat!("vibeos_user_code_", stringify!($name))]
                static CODE: [u8; 4096];
            }
            // SAFETY: the symbol names 4096 initialized read-only bytes that
            // nothing writes, established here by the `global_asm!` above.
            unsafe { &CODE }
        };
    };
}
pub(crate) use user_code;
