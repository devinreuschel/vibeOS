//! The core tool's portable half (ROADMAP §10.7): it reads a physical QEMU
//! guest core (`dump-guest-memory` without paging, `tests/harness/qmp.py`)
//! with the kernel ELF it came from, and decodes the kernel's tables through
//! the roots its VMCOREINFO note names (docs/VMCOREINFO.md). The hostlib
//! `vmcore` binary formats the report; this module only reads, walks and
//! decides, and it is compiled into the kernel too, which writes the
//! [`PanicLine`] the signature reads.
//!
//! Sources, all documentation (DESIGN §1.5: cited, no code copied): the ELF
//! and core layouts (`Elf64_Ehdr`, `Elf64_Phdr`, `Elf64_Shdr`, `Elf64_Sym`,
//! notes) are the System V gABI's; the `NT_PRSTATUS` descriptor is
//! `struct elf_prstatus` as Linux's `include/uapi/linux/elfcore.h` declares
//! it, whose `pr_reg` (at byte 112) is x86-64's `struct user_regs_struct`
//! from `arch/x86/include/asm/user_64.h`, the layout QEMU's
//! `target/i386/arch_dump.c` writes; the Chrome trace-event format is
//! Google's "Trace Event Format" document.
//!
//! No panic and no allocation: every byte comes from a core or an ELF,
//! which are untrusted (AGENTS.md rule 4). Physical reads go through
//! [`PhysMem`] (the ACPI walker's trait, AGENTS.md rule 10), and the page
//! walk through the port's `PageTable` format, so nothing here names an
//! architecture's registers.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use core::fmt;

use crate::log::vmcoreinfo;
use crate::proc::elf::{EHDR_SIZE, ELFCLASS64, ELFDATA2LSB, EM_X86_64, PHDR_SIZE, PT_LOAD};

pub use crate::acpi::PhysMem;

pub mod sig;
#[cfg(any(test, feature = "std"))]
pub mod synth;
pub mod tables;
pub mod walk;

/// Frames a backtrace holds at most: the dump's own cap.
pub const BT_MAX: usize = crate::log::backtrace::DEPTH_CAP;
/// Log records the report prints at most, newest last.
pub const LOG_TAIL: usize = 64;
/// Bytes of panic text [`PanicLine`] keeps.
pub const PANIC_LINE_CAP: usize = 120;
/// Frames the signature names.
pub const SIG_FRAMES: usize = 3;
/// A 4 KiB page, the unit of the core's sparse store and of the walk's
/// output.
pub const PAGE: u64 = 4096;

/// `e_type` of a core file.
pub const ET_CORE: u16 = 4;
/// `p_type` of a note segment.
pub const PT_NOTE: u32 = 4;
/// `sh_type` of a symbol table and of a note section.
pub const SHT_SYMTAB: u32 = 2;
pub const SHT_NOTE: u32 = 7;
/// `sh_flags` bit of executable code.
pub const SHF_EXECINSTR: u64 = 4;
/// Symbol types in `st_info`'s low nibble.
pub const STT_OBJECT: u8 = 1;
pub const STT_FUNC: u8 = 2;
/// The note type of a CPU's registers in a core, name `CORE`.
pub const NT_PRSTATUS: u32 = 1;
/// `ELFCLASS32`, which QEMU writes for a guest still in 32-bit mode.
const ELFCLASS32: u8 = 1;
const SHDR_SIZE: usize = 64;
const SYM_SIZE: usize = 24;
/// The canonical kernel half: VAs a frame pointer may name.
const KERNEL_HALF: u64 = 0xFFFF_8000_0000_0000;

/// Why the tool refuses a core or an ELF, or cannot decode a table. Each
/// has one message ([`VmError::as_str`]); the binary adds the values.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmError {
    NotElf,
    /// An `ELFCLASS32` file: QEMU dumps a guest still in 32-bit mode so.
    Elf32,
    NotElf64Le,
    NotCore,
    NotExec,
    WrongMachine,
    Truncated,
    NoVmcoreinfo,
    NoBuildId,
    BuildIdMismatch,
    MissingKey,
    BadValue,
    Levels,
    NonCanonical,
    NotMapped,
    NotInCore,
    BadLayout,
    TooManySegments,
    NoSymtab,
    NoSymbol,
    AmbiguousSymbol,
    NoTrace,
    WalkBudget,
    Write,
}

impl VmError {
    pub const fn as_str(self) -> &'static str {
        match self {
            VmError::NotElf => "not an ELF file",
            VmError::Elf32 => {
                "an ELF32 core: QEMU writes one for a guest not yet in long mode, so the kernel never ran"
            }
            VmError::NotElf64Le => "not an ELF64 little-endian file",
            VmError::NotCore => "not an ELF core (e_type is not ET_CORE)",
            VmError::NotExec => "not a kernel ELF (no program or section headers)",
            VmError::WrongMachine => "not an x86-64 ELF",
            VmError::Truncated => "truncated ELF",
            VmError::NoVmcoreinfo => "no VMCOREINFO note",
            VmError::NoBuildId => "no build-id note in ELF",
            VmError::BuildIdMismatch => "BUILD-ID mismatch",
            VmError::MissingKey => "VMCOREINFO lacks a key the tool reads",
            VmError::BadValue => "a VMCOREINFO value does not parse",
            VmError::Levels => "a page-table level count the tool cannot walk",
            VmError::NonCanonical => "non-canonical virtual address",
            VmError::NotMapped => "virtual address not mapped",
            VmError::NotInCore => "physical address not in the core",
            VmError::BadLayout => "a kernel table's length is out of range",
            VmError::TooManySegments => "too many segments for one ELF",
            VmError::NoSymtab => "no .symtab in ELF",
            VmError::NoSymbol => "missing symbol",
            VmError::AmbiguousSymbol => "ambiguous symbol",
            VmError::NoTrace => "no live flight recorder in the core",
            VmError::WalkBudget => "page walk budget spent: the tables loop or map too much",
            VmError::Write => "output write failed",
        }
    }
}

/// A core or ELF the tool cannot read is a bad argument; a failed output
/// write is an I/O error.
impl From<VmError> for crate::kerror::KError {
    fn from(e: VmError) -> Self {
        match e {
            VmError::Write => Self::Io,
            VmError::NotElf
            | VmError::Elf32
            | VmError::NotElf64Le
            | VmError::NotCore
            | VmError::NotExec
            | VmError::WrongMachine
            | VmError::Truncated
            | VmError::NoVmcoreinfo
            | VmError::NoBuildId
            | VmError::BuildIdMismatch
            | VmError::MissingKey
            | VmError::BadValue
            | VmError::Levels
            | VmError::NonCanonical
            | VmError::NotMapped
            | VmError::NotInCore
            | VmError::BadLayout
            | VmError::TooManySegments
            | VmError::NoSymtab
            | VmError::NoSymbol
            | VmError::AmbiguousSymbol
            | VmError::NoTrace
            | VmError::WalkBudget => Self::Inval,
        }
    }
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
// ------------------------------------------------------------------ bytes

fn bytes_at(b: &[u8], off: u64, len: usize) -> Result<&[u8], VmError> {
    let start = usize::try_from(off).map_err(|_| VmError::Truncated)?;
    let end = start.checked_add(len).ok_or(VmError::Truncated)?;
    b.get(start..end).ok_or(VmError::Truncated)
}

fn arr<const N: usize>(b: &[u8], off: u64) -> Result<[u8; N], VmError> {
    let s = bytes_at(b, off, N)?;
    s.try_into().map_err(|_| VmError::Truncated)
}

/// A little-endian `u16`, `u32` or `u64` at `off`.
pub fn le16(b: &[u8], off: u64) -> Result<u16, VmError> {
    Ok(u16::from_le_bytes(arr(b, off)?))
}

pub fn le32(b: &[u8], off: u64) -> Result<u32, VmError> {
    Ok(u32::from_le_bytes(arr(b, off)?))
}

pub fn le64(b: &[u8], off: u64) -> Result<u64, VmError> {
    Ok(u64::from_le_bytes(arr(b, off)?))
}

fn add(a: u64, b: u64) -> Result<u64, VmError> {
    a.checked_add(b).ok_or(VmError::Truncated)
}

fn offset(off: usize) -> u64 {
    off as u64
}
// ------------------------------------------------------------- ELF headers

/// The fields of an `Elf64_Ehdr` the tool reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ehdr {
    pub kind: u16,
    pub phoff: u64,
    pub phnum: u16,
    pub shoff: u64,
    pub shnum: u16,
    pub shstrndx: u16,
}

/// The ELF header of an x86-64 ELF64 little-endian file, of any type.
pub fn ehdr(b: &[u8]) -> Result<Ehdr, VmError> {
    let ident = bytes_at(b, 0, 16).map_err(|_| VmError::NotElf)?;
    if ident.get(..4) != Some(&[0x7F, b'E', b'L', b'F'][..]) {
        return Err(VmError::NotElf);
    }
    match (ident.get(4), ident.get(5)) {
        (Some(&ELFCLASS32), _) => return Err(VmError::Elf32),
        (Some(&ELFCLASS64), Some(&ELFDATA2LSB)) => {}
        _ => return Err(VmError::NotElf64Le),
    }
    if b.len() < EHDR_SIZE {
        return Err(VmError::Truncated);
    }
    if le16(b, 18)? != EM_X86_64 {
        return Err(VmError::WrongMachine);
    }
    let phentsize = le16(b, 0x36)?;
    let shentsize = le16(b, 0x3A)?;
    let h = Ehdr {
        kind: le16(b, 16)?,
        phoff: le64(b, 0x20)?,
        phnum: le16(b, 0x38)?,
        shoff: le64(b, 0x28)?,
        shnum: le16(b, 0x3C)?,
        shstrndx: le16(b, 0x3E)?,
    };
    if h.phnum != 0 && usize::from(phentsize) != PHDR_SIZE {
        return Err(VmError::NotElf64Le);
    }
    if h.shnum != 0 && usize::from(shentsize) != SHDR_SIZE {
        return Err(VmError::NotElf64Le);
    }
    Ok(h)
}

/// `count` entries of `size` bytes at `off` lie inside `b`. A reader that
/// holds the whole file checks its tables with this: each lookup walks a
/// table's count, so a count the file does not hold would cost every
/// lookup up to 65,535 failed parses. [`ehdr`] cannot, since the core
/// tool reads a streamed core's header alone.
fn table_fits(b: &[u8], off: u64, count: u16, size: usize) -> Result<(), VmError> {
    if count == 0 {
        return Ok(());
    }
    let len = u64::from(count)
        .checked_mul(offset(size))
        .ok_or(VmError::Truncated)?;
    let len = usize::try_from(len).map_err(|_| VmError::Truncated)?;
    bytes_at(b, off, len).map(|_| ())
}

/// A core's ELF header: [`ehdr`] with `ET_CORE`.
pub fn core_header(b: &[u8]) -> Result<Ehdr, VmError> {
    let h = ehdr(b)?;
    if h.kind != ET_CORE {
        return Err(VmError::NotCore);
    }
    if h.phnum > crate::limits::MAX_CORE_PHDRS {
        return Err(VmError::TooManySegments);
    }
    Ok(h)
}

/// One `Elf64_Phdr`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Phdr {
    pub kind: u32,
    pub flags: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub paddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub align: u64,
}

impl Phdr {
    /// Parse one program header from its 56 bytes.
    pub fn parse(p: &[u8]) -> Result<Phdr, VmError> {
        Ok(Phdr {
            kind: le32(p, 0)?,
            flags: le32(p, 4)?,
            offset: le64(p, 8)?,
            vaddr: le64(p, 16)?,
            paddr: le64(p, 24)?,
            filesz: le64(p, 32)?,
            memsz: le64(p, 40)?,
            align: le64(p, 48)?,
        })
    }

    /// Its 56 bytes.
    pub fn to_le_bytes(&self) -> [u8; PHDR_SIZE] {
        let mut out = [0u8; PHDR_SIZE];
        let words: [(usize, &[u8]); 8] = [
            (0, &self.kind.to_le_bytes()),
            (4, &self.flags.to_le_bytes()),
            (8, &self.offset.to_le_bytes()),
            (16, &self.vaddr.to_le_bytes()),
            (24, &self.paddr.to_le_bytes()),
            (32, &self.filesz.to_le_bytes()),
            (40, &self.memsz.to_le_bytes()),
            (48, &self.align.to_le_bytes()),
        ];
        put_all(&mut out, &words);
        out
    }
}

fn put_all(out: &mut [u8], words: &[(usize, &[u8])]) {
    for (at, w) in words {
        for (i, v) in w.iter().enumerate() {
            if let Some(d) = at.checked_add(i).and_then(|j| out.get_mut(j)) {
                *d = *v;
            }
        }
    }
}

/// Program header `i` of `b`, whose header is `h`.
pub fn phdr(b: &[u8], h: &Ehdr, i: u16) -> Result<Phdr, VmError> {
    let at = u64::from(i)
        .checked_mul(offset(PHDR_SIZE))
        .ok_or(VmError::Truncated)?;
    Phdr::parse(bytes_at(b, add(h.phoff, at)?, PHDR_SIZE)?)
}

/// The notes of one note segment's bytes, in order; iteration stops at the
/// first note that does not parse.
pub struct Notes<'a> {
    rest: &'a [u8],
}

impl<'a> Notes<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }
}

impl<'a> Iterator for Notes<'a> {
    type Item = vmcoreinfo::Note<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let note = vmcoreinfo::parse_note(self.rest).ok()?;
        let namesz = usize::try_from(le32(self.rest, 0).ok()?).ok()?;
        let descsz = usize::try_from(le32(self.rest, 4).ok()?).ok()?;
        let len = 12usize
            .checked_add(namesz.checked_add(3)? & !3)?
            .checked_add(descsz.checked_add(3)? & !3)?;
        self.rest = self.rest.get(len..).unwrap_or(&[]);
        Some(note)
    }
}

/// x86-64 registers from one `NT_PRSTATUS` note.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PrRegs {
    /// `pr_pid`: QEMU writes the vCPU's index plus one.
    pub pid: u32,
    pub rip: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub rflags: u64,
}

/// `pr_reg`'s offset in `struct elf_prstatus` and its `user_regs_struct`
/// slots (`r15, r14, r13, r12, rbp, rbx, r11, r10, r9, r8, rax, rcx, rdx,
/// rsi, rdi, orig_rax, rip, cs, eflags, rsp, ss, …`).
const PR_PID: u64 = 32;
const PR_REG: u64 = 112;
const REG_RBP: u64 = 4;
const REG_RIP: u64 = 16;
const REG_RFLAGS: u64 = 18;
const REG_RSP: u64 = 19;

/// The registers of an `NT_PRSTATUS` descriptor.
pub fn prstatus_regs(desc: &[u8]) -> Result<PrRegs, VmError> {
    let reg = |slot: u64| le64(desc, PR_REG.saturating_add(slot.saturating_mul(8)));
    Ok(PrRegs {
        pid: le32(desc, PR_PID)?,
        rip: reg(REG_RIP)?,
        rsp: reg(REG_RSP)?,
        rbp: reg(REG_RBP)?,
        rflags: reg(REG_RFLAGS)?,
    })
}
// ------------------------------------------------------------- VMCOREINFO

/// A VMCOREINFO descriptor's `KEY=value` text.
#[derive(Clone, Copy, Debug)]
pub struct Vmcoreinfo<'a> {
    pub desc: &'a [u8],
}

/// Up to 64 bytes of build id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildId {
    bytes: [u8; 64],
    len: usize,
}

impl BuildId {
    pub fn from_bytes(b: &[u8]) -> Result<BuildId, VmError> {
        let mut bytes = [0u8; 64];
        bytes
            .get_mut(..b.len())
            .ok_or(VmError::BadValue)?
            .copy_from_slice(b);
        Ok(BuildId {
            bytes,
            len: b.len(),
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

impl fmt::Display for BuildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.as_bytes() {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// The roots and lengths the tool walks from (docs/VMCOREINFO.md, Keys).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Roots {
    pub pgt_root: u64,
    pub pgt_levels: u8,
    pub log: u64,
    pub tcbs: u64,
    pub tcbs_len: u64,
    pub cpus: u64,
    pub cpus_len: u64,
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => c.checked_sub(b'0'),
        b'a'..=b'f' => c.checked_sub(b'a' - 10),
        b'A'..=b'F' => c.checked_sub(b'A' - 10),
        _ => None,
    }
}

fn parse_radix(v: &[u8], radix: u64) -> Result<u64, VmError> {
    if v.is_empty() {
        return Err(VmError::BadValue);
    }
    let mut n = 0u64;
    for &c in v {
        let d = u64::from(hex_digit(c).ok_or(VmError::BadValue)?);
        if d >= radix {
            return Err(VmError::BadValue);
        }
        n = n
            .checked_mul(radix)
            .and_then(|n| n.checked_add(d))
            .ok_or(VmError::BadValue)?;
    }
    Ok(n)
}

impl<'a> Vmcoreinfo<'a> {
    /// The note among `notes` named `VMCOREINFO`.
    pub fn find(mut notes: impl Iterator<Item = vmcoreinfo::Note<'a>>) -> Result<Self, VmError> {
        notes
            .find(|n| n.name == vmcoreinfo::NOTE_NAME && n.kind == vmcoreinfo::NOTE_TYPE)
            .map(|n| Vmcoreinfo { desc: n.desc })
            .ok_or(VmError::NoVmcoreinfo)
    }

    pub fn get(&self, key: &str) -> Option<&'a [u8]> {
        vmcoreinfo::get(self.desc, key)
    }

    fn need(&self, key: &str) -> Result<&'a [u8], VmError> {
        self.get(key).ok_or(VmError::MissingKey)
    }

    /// A decimal `NUMBER()`/`LENGTH()` value.
    pub fn number(&self, key: &str) -> Result<u64, VmError> {
        parse_radix(self.need(key)?, 10)
    }

    /// A `SYMBOL()` value: lowercase hex without a prefix.
    pub fn symbol(&self, key: &str) -> Result<u64, VmError> {
        parse_radix(self.need(key)?, 16)
    }

    /// `BUILD-ID`, decoded from hex.
    pub fn build_id(&self) -> Result<BuildId, VmError> {
        let v = self.need("BUILD-ID")?;
        if v.len() % 2 != 0 || v.len() > 128 {
            return Err(VmError::BadValue);
        }
        let mut out = [0u8; 64];
        for (o, &[h, l]) in out.iter_mut().zip(v.as_chunks::<2>().0) {
            let (h, l) = (
                hex_digit(h).ok_or(VmError::BadValue)?,
                hex_digit(l).ok_or(VmError::BadValue)?,
            );
            *o = h.checked_mul(16).ok_or(VmError::BadValue)? | l;
        }
        BuildId::from_bytes(out.get(..v.len() / 2).ok_or(VmError::BadValue)?)
    }

    pub fn roots(&self) -> Result<Roots, VmError> {
        let levels = self.number("NUMBER(vibeos_pgt_levels)")?;
        let r = Roots {
            pgt_root: self.number("NUMBER(vibeos_pgt_root)")?,
            pgt_levels: u8::try_from(levels).map_err(|_| VmError::Levels)?,
            log: self.symbol("SYMBOL(vibeos_log)")?,
            tcbs: self.symbol("SYMBOL(vibeos_tcbs)")?,
            tcbs_len: self.number("LENGTH(vibeos_tcbs)")?,
            cpus: self.symbol("SYMBOL(vibeos_cpus)")?,
            cpus_len: self.number("LENGTH(vibeos_cpus)")?,
        };
        // A length past the flight recorder's CPUs or the thread table's
        // bound is a corrupted note, which would keep the tool reading.
        if r.cpus_len > crate::log::trace::MAX_CPUS as u64
            || r.tcbs_len > crate::limits::MAX_THREADS as u64
        {
            return Err(VmError::BadLayout);
        }
        Ok(r)
    }
}

/// Refuse a core whose note's `BUILD-ID` is not the ELF's GNU build id.
pub fn check_build_id(core: &BuildId, elf: &BuildId) -> Result<(), VmError> {
    if core.as_bytes() == elf.as_bytes() {
        Ok(())
    } else {
        Err(VmError::BuildIdMismatch)
    }
}
// --------------------------------------------------------------- kernel ELF

/// One `Elf64_Shdr`'s fields the tool reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Section {
    pub name: u32,
    pub kind: u32,
    pub flags: u64,
    pub addr: u64,
    pub offset: u64,
    pub size: u64,
    pub link: u32,
}

/// One `Elf64_Sym`, its name resolved in its string table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sym<'a> {
    pub name: &'a [u8],
    pub kind: u8,
    pub value: u64,
    pub size: u64,
}

/// The kernel ELF the core came from: sections, symbols and build id.
pub struct KernelElf<'a> {
    pub bytes: &'a [u8],
    pub hdr: Ehdr,
}

impl<'a> KernelElf<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, VmError> {
        let hdr = ehdr(bytes)?;
        if hdr.kind == ET_CORE || hdr.shnum == 0 {
            return Err(VmError::NotExec);
        }
        table_fits(bytes, hdr.phoff, hdr.phnum, PHDR_SIZE)?;
        table_fits(bytes, hdr.shoff, hdr.shnum, SHDR_SIZE)?;
        Ok(Self { bytes, hdr })
    }

    pub fn section(&self, i: u16) -> Result<Section, VmError> {
        let at = add(
            self.hdr.shoff,
            u64::from(i)
                .checked_mul(offset(SHDR_SIZE))
                .ok_or(VmError::Truncated)?,
        )?;
        let s = bytes_at(self.bytes, at, SHDR_SIZE)?;
        Ok(Section {
            name: le32(s, 0)?,
            kind: le32(s, 4)?,
            flags: le64(s, 8)?,
            addr: le64(s, 16)?,
            offset: le64(s, 24)?,
            size: le64(s, 32)?,
            link: le32(s, 40)?,
        })
    }

    pub fn sections(&self) -> impl Iterator<Item = Section> + '_ {
        (0..self.hdr.shnum).filter_map(|i| self.section(i).ok())
    }

    /// A section's bytes in the file.
    pub fn data(&self, s: &Section) -> Result<&'a [u8], VmError> {
        let len = usize::try_from(s.size).map_err(|_| VmError::Truncated)?;
        bytes_at(self.bytes, s.offset, len)
    }

    fn cstr(bytes: &'a [u8], at: u64) -> &'a [u8] {
        let rest = usize::try_from(at)
            .ok()
            .and_then(|a| bytes.get(a..))
            .unwrap_or(&[]);
        let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
        rest.get(..end).unwrap_or(&[])
    }

    /// A section's name, from the section-name string table.
    pub fn section_name(&self, s: &Section) -> &'a [u8] {
        match self.section(self.hdr.shstrndx).and_then(|t| self.data(&t)) {
            Ok(t) => Self::cstr(t, u64::from(s.name)),
            Err(_) => &[],
        }
    }

    /// The GNU build id: the `NT_GNU_BUILD_ID` note of any note section.
    pub fn build_id(&self) -> Result<BuildId, VmError> {
        for s in self.sections().filter(|s| s.kind == SHT_NOTE) {
            let Ok(data) = self.data(&s) else { continue };
            for n in Notes::new(data) {
                if n.name == vmcoreinfo::GNU_NAME && n.kind == vmcoreinfo::NT_GNU_BUILD_ID {
                    return BuildId::from_bytes(n.desc);
                }
            }
        }
        Err(VmError::NoBuildId)
    }

    /// Every symbol of `.symtab`, names raw (mangled).
    pub fn symbols(&self) -> Result<impl Iterator<Item = Sym<'a>> + '_, VmError> {
        let tab = self
            .sections()
            .find(|s| s.kind == SHT_SYMTAB)
            .ok_or(VmError::NoSymtab)?;
        let data = self.data(&tab)?;
        let strtab = u16::try_from(tab.link)
            .map_err(|_| VmError::NoSymtab)
            .and_then(|i| self.section(i))
            .and_then(|s| self.data(&s))?;
        Ok(data.as_chunks::<SYM_SIZE>().0.iter().filter_map(move |e| {
            let info = *e.get(4)?;
            Some(Sym {
                name: Self::cstr(strtab, u64::from(le32(e, 0).ok()?)),
                kind: info & 0xF,
                value: le64(e, 8).ok()?,
                size: le64(e, 16).ok()?,
            })
        }))
    }

    /// Whether `addr` lies in an executable section: the unwinder's "text"
    /// rule.
    pub fn in_text(&self, addr: u64) -> bool {
        self.sections().any(|s| {
            s.flags & SHF_EXECINSTR != 0
                && s.addr != 0
                && addr >= s.addr
                && s.addr.checked_add(s.size).is_some_and(|end| addr < end)
        })
    }

    /// The program headers, for the virtual core's check.
    pub fn loads(&self) -> impl Iterator<Item = Phdr> + '_ {
        (0..self.hdr.phnum)
            .filter_map(|i| phdr(self.bytes, &self.hdr, i).ok())
            .filter(|p| p.kind == PT_LOAD)
    }
}
// ------------------------------------------------------------------- cores

/// A whole core in memory: the tests' and the fuzz target's [`PhysMem`].
/// The binary's streaming store implements the trait over the same rule:
/// a physical address reads from the `PT_LOAD` whose `p_paddr` range holds
/// it, and bytes past a segment's `p_filesz` read as zero up to `p_memsz`.
pub struct SliceCore<'a> {
    pub bytes: &'a [u8],
    pub hdr: Ehdr,
}

impl<'a> SliceCore<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, VmError> {
        let hdr = core_header(bytes)?;
        table_fits(bytes, hdr.phoff, hdr.phnum, PHDR_SIZE)?;
        Ok(Self { bytes, hdr })
    }

    pub fn phdrs(&self) -> impl Iterator<Item = Phdr> + '_ {
        (0..self.hdr.phnum).filter_map(|i| phdr(self.bytes, &self.hdr, i).ok())
    }

    /// Every note of every `PT_NOTE` segment, in file order.
    pub fn notes(&self) -> impl Iterator<Item = vmcoreinfo::Note<'a>> + '_ {
        self.phdrs()
            .filter(|p| p.kind == PT_NOTE)
            .filter_map(|p| {
                let len = usize::try_from(p.filesz).ok()?;
                bytes_at(self.bytes, p.offset, len).ok()
            })
            .flat_map(Notes::new)
    }

    /// The `PT_NOTE` segments' bytes, back to back, as the virtual core
    /// copies them; `out` gets each segment in turn.
    pub fn note_bytes(&self, mut out: impl FnMut(&'a [u8])) {
        for p in self.phdrs().filter(|p| p.kind == PT_NOTE) {
            if let Some(b) = usize::try_from(p.filesz)
                .ok()
                .and_then(|n| bytes_at(self.bytes, p.offset, n).ok())
            {
                out(b);
            }
        }
    }

    /// RAM bytes and `PT_LOAD` segments.
    pub fn ram(&self) -> (u64, usize) {
        self.phdrs()
            .filter(|p| p.kind == PT_LOAD)
            .fold((0u64, 0usize), |(b, n), p| {
                (b.saturating_add(p.memsz), n.saturating_add(1))
            })
    }
}

/// [`PhysMem::page_run`] for a memory that holds the bytes of its
/// `(start, len)` extents: a page is held when its first byte lies in one.
pub fn page_run(extents: impl Iterator<Item = (u64, u64)>, addr: u64, max: u64) -> (bool, u64) {
    // The furthest end of an extent holding `addr`, and the lowest start
    // of one above it.
    let mut held_to: Option<u64> = None;
    let mut next = u64::MAX;
    for (start, len) in extents {
        let end = start.saturating_add(len);
        if addr >= start && addr < end {
            held_to = Some(held_to.map_or(end, |h| h.max(end)));
        } else if start > addr {
            next = next.min(start);
        }
    }
    let to = held_to.unwrap_or(next);
    // The pages from `addr` whose first byte lies below `to`.
    let pages = to.saturating_sub(addr).div_ceil(PAGE);
    (held_to.is_some(), pages.saturating_mul(PAGE).min(max))
}

impl PhysMem for SliceCore<'_> {
    fn page_run(&self, addr: u64, max: u64) -> (bool, u64) {
        // `read` holds a segment's bytes the file has and the zeros past
        // `filesz`; a truncated file drops the bytes it lacks.
        let have = |p: &Phdr| {
            let avail = offset(self.bytes.len()).saturating_sub(p.offset);
            p.filesz.min(avail).min(p.memsz)
        };
        let extents = self.phdrs().filter(|p| p.kind == PT_LOAD).flat_map(|p| {
            let tail = p.filesz.min(p.memsz);
            [
                (p.paddr, have(&p)),
                (p.paddr.saturating_add(tail), p.memsz.saturating_sub(tail)),
            ]
        });
        page_run(extents, addr, max)
    }

    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let mut pa = addr;
        let mut done = 0usize;
        while done < buf.len() {
            let Some(p) = self.phdrs().find(|p| {
                p.kind == PT_LOAD
                    && pa >= p.paddr
                    && p.paddr.checked_add(p.memsz).is_some_and(|e| pa < e)
            }) else {
                return false;
            };
            let into = pa.saturating_sub(p.paddr);
            let seg_left = p.memsz.saturating_sub(into);
            let want = buf.len().saturating_sub(done);
            let n = usize::try_from(seg_left).map_or(want, |s| s.min(want));
            let Some(dst) = done.checked_add(n).and_then(|end| buf.get_mut(done..end)) else {
                return false;
            };
            // The file holds the segment's first `filesz` bytes; the rest
            // of `memsz` reads as zero.
            let in_file = p.filesz.saturating_sub(into);
            let k = usize::try_from(in_file).map_or(n, |f| f.min(n));
            let (file, zero) = dst.split_at_mut(k);
            match add(p.offset, into).and_then(|o| bytes_at(self.bytes, o, k)) {
                Ok(src) => file.copy_from_slice(src),
                Err(_) => return false,
            }
            zero.fill(0);
            done = done.saturating_add(n);
            pa = pa.saturating_add(offset(n));
        }
        true
    }
}

// The tests build `static` kernel views, which need the `const`
// constructors (C-ATOMICS).
#[cfg(all(test, not(loom)))]
mod tests;
