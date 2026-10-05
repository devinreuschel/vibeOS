//! In-memory ring-3 programs for in-guest tests (ROADMAP §10.6, C-RING3).
//!
//! A test builds its program with [`user_code!`] or hands in an ELF image,
//! and [`spawn`] starts it as a process whose parent is the kernel (ppid
//! 0); [`wait`] reaps it. No initrd file is involved.

use alloc::vec;
use alloc::vec::Vec;

#[cfg(target_arch = "aarch64")]
use vibeos::elf::EM_AARCH64;
#[cfg(target_arch = "x86_64")]
use vibeos::elf::EM_X86_64;
use vibeos::elf::{
    EHDR_SIZE, ELFCLASS64, ELFDATA2LSB, ELFMAG0, ET_EXEC, EV_CURRENT, PF_R, PF_W, PF_X, PHDR_SIZE,
    PT_LOAD, PT_TLS,
};
use vibeos::fs::FsError;

use crate::proc_init;
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

/// A ring-3 program: code from [`user_code!`] at a [`Layout`], a whole ELF
/// image, or a Rust user program `make user` built, by name (C-USERBINS).
pub(crate) enum Image {
    Code(&'static [u8], Layout),
    /// Like [`Code`], plus a one-word `PT_TLS` whose init image is
    /// [`TLS_MAGIC`] (ROADMAP §11.6, F022).
    TlsCode(&'static [u8], Layout),
    #[allow(dead_code, reason = "x86 ktests construct Elf/UserBin")]
    Elf(&'static [u8]),
    #[allow(dead_code, reason = "x86 ktests construct Elf/UserBin")]
    UserBin(&'static str),
}

/// Little-endian `0x1122334455667788`, the one TLS word a `TlsCode` image
/// places in its `PT_TLS` block.
pub(crate) const TLS_MAGIC: u64 = 0x1122_3344_5566_7788;
const TLS_OFF: usize = 0x0F00;

/// The user programs this kernel embeds (`build.rs`, `VIBEOS_USER_BINS`).
mod bins {
    include!(concat!(env!("OUT_DIR"), "/user_bins.rs"));
}

/// The embedded user program called `name`.
fn user_bin(name: &str) -> Option<&'static [u8]> {
    bins::USER_BINS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, elf)| *elf)
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
    #[cfg(target_arch = "aarch64")]
    b[18..20].copy_from_slice(&EM_AARCH64.to_le_bytes());
    #[cfg(target_arch = "x86_64")]
    b[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
    b[20..24].copy_from_slice(&1u32.to_le_bytes());
    b[24..32].copy_from_slice(&layout.vaddr.to_le_bytes());
    b[32..40].copy_from_slice(&(EHDR_SIZE as u64).to_le_bytes());
    b[52..54].copy_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
    b[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
    b[56..58].copy_from_slice(&1u16.to_le_bytes());
    fill_phdrs(&mut b, code, layout, false);
    b[CODE_OFFSET..].copy_from_slice(code);
    b
}

/// [`code_elf`] plus a `PT_TLS` of 8 bytes at [`TLS_OFF`].
fn tls_code_elf(code: &[u8], layout: &Layout) -> Vec<u8> {
    let mut b = vec![0u8; CODE_OFFSET + code.len()];
    b[0] = ELFMAG0;
    b[1..4].copy_from_slice(b"ELF");
    b[4] = ELFCLASS64;
    b[5] = ELFDATA2LSB;
    b[6] = EV_CURRENT;
    b[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
    #[cfg(target_arch = "aarch64")]
    b[18..20].copy_from_slice(&EM_AARCH64.to_le_bytes());
    #[cfg(target_arch = "x86_64")]
    b[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
    b[20..24].copy_from_slice(&1u32.to_le_bytes());
    b[24..32].copy_from_slice(&layout.vaddr.to_le_bytes());
    b[32..40].copy_from_slice(&(EHDR_SIZE as u64).to_le_bytes());
    b[52..54].copy_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
    b[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
    b[56..58].copy_from_slice(&2u16.to_le_bytes());
    fill_phdrs(&mut b, code, layout, true);
    b[TLS_OFF..TLS_OFF + 8].copy_from_slice(&TLS_MAGIC.to_le_bytes());
    b[CODE_OFFSET..].copy_from_slice(code);
    b
}

fn fill_phdrs(b: &mut [u8], code: &[u8], layout: &Layout, tls: bool) {
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
    if tls {
        let t = ph + PHDR_SIZE;
        b[t..t + 4].copy_from_slice(&PT_TLS.to_le_bytes());
        b[t + 4..t + 8].copy_from_slice(&PF_R.to_le_bytes());
        b[t + 8..t + 16].copy_from_slice(&(TLS_OFF as u64).to_le_bytes());
        b[t + 32..t + 40].copy_from_slice(&8u64.to_le_bytes());
        b[t + 40..t + 48].copy_from_slice(&8u64.to_le_bytes());
        // 8: variant II puts the one-word init image at TP-8 (`fs:[-8]`).
        b[t + 48..t + 56].copy_from_slice(&8u64.to_le_bytes());
    }
}

/// The ELF bytes of `img`; empty for an unknown [`Image::UserBin`].
#[allow(dead_code, reason = "x86 ktests feed elf_bytes into load_image")]
pub(crate) fn elf_bytes(img: &Image) -> Vec<u8> {
    match img {
        Image::Code(code, layout) => code_elf(code, layout),
        Image::TlsCode(code, layout) => tls_code_elf(code, layout),
        Image::Elf(elf) => elf.to_vec(),
        Image::UserBin(name) => user_bin(name).map(<[u8]>::to_vec).unwrap_or_default(),
    }
}

/// Start `img` with `argv` as a process whose parent is the kernel (ppid
/// 0). Every caller also calls [`wait`]: an unwaited ppid-0 zombie keeps
/// its slot for the whole boot.
pub(crate) fn spawn(img: &Image, argv: &[&str]) -> Result<u32, LoadError> {
    let argv_b: Vec<&[u8]> = argv.iter().map(|a| a.as_bytes()).collect();
    match img {
        Image::Code(code, layout) => proc_init::spawn_image(&code_elf(code, layout), &argv_b, 0),
        Image::TlsCode(code, layout) => {
            proc_init::spawn_image(&tls_code_elf(code, layout), &argv_b, 0)
        }
        Image::Elf(elf) => proc_init::spawn_image(elf, &argv_b, 0),
        Image::UserBin(name) => match user_bin(name) {
            Some(elf) => proc_init::spawn_image(elf, &argv_b, 0),
            None => Err(LoadError::Fs(FsError::NotFound)),
        },
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

/// Yield until the free-frame count is back to `before`, with the running
/// row's deadline as the bound ([`super::wait_for`]): a reaped process's
/// frames come back when its CPU runs the reclaim, which a loaded host can
/// put off past any fixed time.
#[allow(dead_code, reason = "x86 ktests wait for reclaim")]
pub(crate) fn frames_settle(before: usize) -> bool {
    super::wait_for(|| super::free_frames() == before)
}

/// `user_code!(NAME, "<asm>")` assembles position-independent code into
/// one padded 4096-byte page of `.rodata` and defines `static NAME: &[u8]`
/// over it. Use numeric local labels and no braces; `NAME` is unique in
/// the crate. Pad is `int3` on x86_64 and zero on aarch64.
macro_rules! user_code {
    ($name:ident, $asm:literal) => {
        #[cfg(target_arch = "x86_64")]
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
            ".org vibeos_user_code_",
            stringify!($name),
            " + 4096, 0xcc\n.popsection\n"
        ));
        #[cfg(target_arch = "aarch64")]
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
            ".org vibeos_user_code_",
            stringify!($name),
            " + 4096, 0x00\n.popsection\n"
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

// fork; the child exits 0; the parent waits for any child, then exits 0.
#[cfg(target_arch = "x86_64")]
user_code!(
    WARM_FORK,
    "
    mov eax, 57
    syscall
    test rax, rax
    jnz 2f
    xor edi, edi
    mov eax, 60
    syscall
    ud2
2:
    mov rdi, -1
    xor esi, esi
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

// clone(SIGCHLD, 0, …); the child exits 0; the parent waits, then exits 0.
#[cfg(target_arch = "aarch64")]
user_code!(
    WARM_FORK,
    "
    mov x0, #17
    mov x1, xzr
    mov x2, xzr
    mov x3, xzr
    mov x4, xzr
    mov x8, #220
    svc #0
    cbnz x0, 2f
    mov x0, xzr
    mov x8, #93
    svc #0
2:
    mov x0, #-1
    mov x1, xzr
    mov x2, xzr
    mov x3, xzr
    mov x8, #260
    svc #0
    mov x0, xzr
    mov x8, #93
    svc #0
    "
);

/// Run a process that forks one child and reaps it, for the registry's
/// warm-up ([`super::quiesce_frames`]): the kernel heap, which never
/// shrinks, then holds what two live processes' objects take, so a test
/// that runs its first process alone does not count that growth as a
/// leak. `false` when it could not be spawned or did not exit 0.
pub(crate) fn warm_processes() -> bool {
    matches!(run(&Image::Code(WARM_FORK, DEFAULT), &["warm"]), Ok(0))
}
