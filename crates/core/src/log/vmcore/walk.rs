//! The crashed kernel's address space (ROADMAP §10.7, §11.7): the page walk
//! from the root VMCOREINFO names, in the port's pure page-table format
//! ([`CoreFormat`], which each port's `arch::<port>::paging` implements:
//! [`X86_64`] for the PML4, [`Aarch64Ttbr1`] for the TTBR1 table), the
//! frame-pointer unwinder, and the virtually addressed core `gdb` opens.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use core::marker::PhantomData;

use super::{
    BT_MAX, ET_CORE, Machine, PAGE, PT_NOTE, Phdr, PhysMem, SHDR_SIZE, VmError, add, offset,
    put_all,
};
use crate::log::backtrace::{self, StackRange};
use crate::paging::PageFlags;

pub use crate::arch::X86_64;
pub use crate::arch::aarch64::paging::Aarch64Ttbr1;
use crate::proc::elf::{EHDR_SIZE, ELFCLASS64, ELFDATA2LSB, PHDR_SIZE, PT_LOAD};

// ------------------------------------------------------------------ formats

/// What a table entry at some level does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desc {
    /// Not a translation: invalid, or a type the level does not allow.
    None,
    /// Points at the next level's table.
    Table,
    /// Maps a page or a block of the level's size.
    Leaf,
}

/// A kernel page-table format the core tool walks: the pure half of a
/// port's paging (`arch::<port>::paging`) for the root VMCOREINFO's
/// `NUMBER(vibeos_pgt_root)` names, and the machine its cores are for.
pub trait CoreFormat {
    /// The `e_machine` of the kernel ELF and of the core.
    const MACHINE: Machine;
    /// Levels of the walk; the root is level `LEVELS`.
    const LEVELS: u8;
    /// Entries in one table.
    const ENTRIES: usize;
    /// Root slots `KERNEL_ROOT_FIRST..ENTRIES` map the kernel half.
    const KERNEL_ROOT_FIRST: usize;
    /// The VA root slot `KERNEL_ROOT_FIRST` maps first: the kernel half's
    /// start, and the lowest VA a frame pointer may name.
    const KERNEL_VA_START: u64;
    /// The index `va` selects in a table at `level`.
    fn index(va: u64, level: u8) -> usize;
    /// What `entry`, read at `level`, does.
    fn desc(entry: u64, level: u8) -> Desc;
    /// The physical address `entry` points at.
    fn entry_phys(entry: u64) -> u64;
    /// The portable flags of `entry` (`PageFlags`' x86_64 values).
    fn entry_flags(entry: u64) -> PageFlags;
    /// Whether the root translates `va`.
    fn va_ok(va: u64) -> bool;
    /// A table descriptor for the table at `pa`: what the kernel's mapper
    /// writes, for the synthetic cores (`synth`).
    fn table_entry(pa: u64) -> u64;
    /// A leaf that maps `pa` at `va` with `flags` (`PageFlags` bits).
    fn leaf_entry(va: u64, pa: u64, flags: u64) -> u64;
}

/// One run of the kernel's mappings: `len` bytes at `va`, backed by the
/// core from `pa`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mapping {
    pub va: u64,
    pub pa: u64,
    pub len: u64,
}

/// The crashed kernel's address space, read from the core through the root
/// its note names, in the page-table format `A`.
pub struct Kernel<'m, M: PhysMem, A: CoreFormat> {
    pub mem: &'m M,
    pub root: u64,
    /// Table entries and 4 KiB pages one [`Kernel::kernel_mappings`] walk
    /// may visit, so tables a bug corrupted into a loop end the walk.
    pub budget: u64,
    _format: PhantomData<fn() -> A>,
}

/// The default walk budget: 64 GiB of 4 KiB pages, several times what a
/// 9 GiB guest's kernel half maps.
pub const WALK_BUDGET: u64 = 1 << 24;

impl<'m, M: PhysMem, A: CoreFormat> Kernel<'m, M, A> {
    /// `levels` must be the format's: another count is a table the tool
    /// cannot walk.
    pub fn new(mem: &'m M, root: u64, levels: u8) -> Result<Self, VmError> {
        if levels != A::LEVELS {
            return Err(VmError::Levels);
        }
        Ok(Self {
            mem,
            root,
            budget: WALK_BUDGET,
            _format: PhantomData,
        })
    }

    /// The same view with another walk budget.
    pub fn with_budget(self, budget: u64) -> Self {
        Self { budget, ..self }
    }

    fn entry(&self, table: u64, idx: usize) -> Result<u64, VmError> {
        let at = offset(idx)
            .checked_mul(8)
            .and_then(|o| table.checked_add(o))
            .ok_or(VmError::NotInCore)?;
        let mut b = [0u8; 8];
        if !self.mem.read(at, &mut b) {
            return Err(VmError::NotInCore);
        }
        Ok(u64::from_le_bytes(b))
    }

    /// Bytes a leaf at `level` maps.
    fn leaf_size(level: u8) -> u64 {
        let shift = u32::from(level.saturating_sub(1)).saturating_mul(9);
        PAGE.checked_shl(shift).unwrap_or(0)
    }

    /// The physical address `va` maps to.
    /// The output address of a leaf at `level` whose entry names `pa`: its
    /// block's base, by the level's size. The bits below a block's size are
    /// not address bits: RES0 in an aarch64 block descriptor, and on x86_64
    /// bit 12 of a 2 MiB or 1 GiB leaf is its PAT bit, which the entry's
    /// address mask keeps.
    fn leaf_base(pa: u64, level: u8) -> u64 {
        pa & !Self::leaf_size(level).wrapping_sub(1)
    }

    pub fn translate(&self, va: u64) -> Result<u64, VmError> {
        if !A::va_ok(va) {
            return Err(VmError::NonCanonical);
        }
        let mut table = self.root;
        let mut level = A::LEVELS;
        loop {
            let e = self.entry(table, A::index(va, level))?;
            let pa = A::entry_phys(e);
            match A::desc(e, level) {
                Desc::Leaf => {
                    let size = Self::leaf_size(level);
                    return Self::leaf_base(pa, level)
                        .checked_add(va & size.wrapping_sub(1))
                        .ok_or(VmError::NotMapped);
                }
                Desc::Table if level > 1 => {
                    table = pa;
                    level = level.saturating_sub(1);
                }
                Desc::Table | Desc::None => return Err(VmError::NotMapped),
            }
        }
    }

    /// `out.len()` bytes at `va`, page by page.
    pub fn read(&self, va: u64, out: &mut [u8]) -> Result<(), VmError> {
        let mut done = 0usize;
        while done < out.len() {
            let at = va.checked_add(offset(done)).ok_or(VmError::NonCanonical)?;
            let in_page = PAGE.saturating_sub(at & (PAGE - 1));
            let want = out.len().saturating_sub(done);
            let n = usize::try_from(in_page).map_or(want, |p| p.min(want));
            let pa = self.translate(at)?;
            let dst = done
                .checked_add(n)
                .and_then(|e| out.get_mut(done..e))
                .ok_or(VmError::Truncated)?;
            if !self.mem.read(pa, dst) {
                return Err(VmError::NotInCore);
            }
            done = done.saturating_add(n);
        }
        Ok(())
    }

    pub fn u64_at(&self, va: u64) -> Result<u64, VmError> {
        let mut b = [0u8; 8];
        self.read(va, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    pub fn u32_at(&self, va: u64) -> Result<u32, VmError> {
        let mut b = [0u8; 4];
        self.read(va, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    /// Every present kernel-half mapping (root slots `KERNEL_ROOT_FIRST`
    /// up), coalesced where both the VA and the PA run on, 4 KiB pages
    /// that the core does not hold and uncached (MMIO) leaves left out.
    pub fn kernel_mappings(&self, mut out: impl FnMut(Mapping)) -> Result<(), VmError> {
        let mut run: Option<Mapping> = None;
        let mut emit = |va: u64, pa: u64, len: u64| {
            if let Some(r) = run.as_mut()
                && r.va.checked_add(r.len) == Some(va)
                && r.pa.checked_add(r.len) == Some(pa)
            {
                r.len = r.len.saturating_add(len);
                return;
            }
            if let Some(r) = run.replace(Mapping { va, pa, len }) {
                out(r);
            }
        };
        let top = A::LEVELS;
        let mut left = self.budget;
        for i4 in A::KERNEL_ROOT_FIRST..A::ENTRIES {
            let base = offset(i4.saturating_sub(A::KERNEL_ROOT_FIRST))
                .checked_mul(Self::leaf_size(top))
                .and_then(|o| A::KERNEL_VA_START.checked_add(o))
                .ok_or(VmError::Levels)?;
            self.walk_table(self.root, top, i4, base, &mut left, &mut emit)?;
        }
        if let Some(r) = run {
            out(r);
        }
        Ok(())
    }

    /// Slot `idx` of the table at `table` (level `level`), which maps
    /// `va` up.
    fn walk_table(
        &self,
        table: u64,
        level: u8,
        idx: usize,
        va: u64,
        left: &mut u64,
        emit: &mut impl FnMut(u64, u64, u64),
    ) -> Result<(), VmError> {
        *left = left.checked_sub(1).ok_or(VmError::WalkBudget)?;
        let Ok(e) = self.entry(table, idx) else {
            return Ok(());
        };
        let desc = A::desc(e, level);
        if desc == Desc::None || (desc == Desc::Table && level <= 1) {
            return Ok(());
        }
        let flags = A::entry_flags(e);
        let pa = A::entry_phys(e);
        let size = Self::leaf_size(level);
        if desc == Desc::Leaf {
            let pa = Self::leaf_base(pa, level);
            // Not a mapping of the virtual core: an uncached leaf, which is
            // x86_64's PCD or PWT and aarch64's Device memory (`PCD | PWT`)
            // or Normal non-cacheable (`PWT`). Device leaves are MMIO, whose
            // registers the core does not hold. The kernel maps no
            // Normal-NC memory today (`paging::mmio_flags` sets both bits);
            // one it adds would be memory a device shares, such as a
            // framebuffer or a DMA buffer, which no report reads, so one
            // rule covers both ports. `translate` still reads through
            // either kind.
            if flags.0 & (PageFlags::PCD | PageFlags::PWT) != 0 {
                return Ok(());
            }
            // Runs of 4 KiB pages the core does or does not hold, each
            // page counted against the budget.
            let mut off = 0u64;
            while off < size {
                let (Some(v), Some(p)) = (va.checked_add(off), pa.checked_add(off)) else {
                    break;
                };
                let (held, len) = self.mem.page_run(p, size.saturating_sub(off));
                let len = (len & !(PAGE - 1)).max(PAGE);
                *left = left
                    .checked_sub(len.div_ceil(PAGE))
                    .ok_or(VmError::WalkBudget)?;
                if held {
                    emit(v, p, len);
                }
                off = off.saturating_add(len);
            }
            return Ok(());
        }
        let child = Self::leaf_size(level.saturating_sub(1));
        for i in 0..A::ENTRIES {
            let Some(v) = offset(i).checked_mul(child).and_then(|o| va.checked_add(o)) else {
                break;
            };
            self.walk_table(pa, level.saturating_sub(1), i, v, left, emit)?;
        }
        Ok(())
    }

    /// The frame-pointer backtrace from `rip`/`rbp` (`pc`/`x29` on
    /// aarch64, whose frame record has x86_64's layout: the caller's frame
    /// pointer, then the return address): at most [`BT_MAX`] return
    /// addresses into `out`, `rip` first. The walk is the dump's
    /// (`backtrace::walk`): `rbp` only rising, 8-aligned and inside the
    /// kernel half, a read the core cannot serve ending it as a zero
    /// return does, and a return outside `in_text` ending it.
    pub fn unwind(
        &self,
        rip: u64,
        rbp: u64,
        in_text: impl Fn(u64) -> bool,
        out: &mut [u64; BT_MAX],
    ) -> usize {
        let half = [StackRange::new(A::KERNEL_VA_START, u64::MAX)];
        let mut n = 0usize;
        backtrace::walk(
            rip,
            rbp,
            &half,
            in_text,
            |a| self.u64_at(a).unwrap_or(0),
            |f| {
                if let Some(slot) = out.get_mut(n) {
                    *slot = f;
                    n = n.saturating_add(1);
                }
            },
        );
        n
    }
}
// ----------------------------------------------------------- virtual core

/// What [`write_virtual_core`] wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VirtStats {
    pub segments: usize,
    pub bytes: u64,
}

fn virt_header(machine: Machine, phnum: u16) -> [u8; EHDR_SIZE] {
    let mut h = [0u8; EHDR_SIZE];
    let ident: [u8; 7] = [0x7F, b'E', b'L', b'F', ELFCLASS64, ELFDATA2LSB, 1];
    let words: [(usize, &[u8]); 9] = [
        (0, &ident),
        (16, &ET_CORE.to_le_bytes()),
        (18, &machine.em().to_le_bytes()),
        (20, &1u32.to_le_bytes()),
        (0x20, &offset(EHDR_SIZE).to_le_bytes()),
        (0x34, &(EHDR_SIZE as u16).to_le_bytes()),
        (0x36, &(PHDR_SIZE as u16).to_le_bytes()),
        (0x38, &phnum.to_le_bytes()),
        (0x3A, &(SHDR_SIZE as u16).to_le_bytes()),
    ];
    put_all(&mut h, &words);
    h
}

fn round_up(n: u64, to: u64) -> Result<u64, VmError> {
    let m = to.saturating_sub(1);
    Ok(add(n, m)? & !m)
}

/// Write the virtually addressed core, which `gdb` opens with the kernel
/// ELF: an `ET_CORE` header, one `PT_NOTE` holding `notes` (the physical
/// core's notes, registers and VMCOREINFO included), then one `PT_LOAD`
/// per [`Kernel::kernel_mappings`] run, `p_vaddr` its VA and `p_paddr` its
/// PA, with the bytes read from the core. Two passes over the mappings:
/// one counts them for the header, one writes; `sink` gets the file in
/// order.
pub fn write_virtual_core<M: PhysMem, A: CoreFormat>(
    k: &Kernel<'_, M, A>,
    notes: &[&[u8]],
    mut sink: impl FnMut(&[u8]) -> Result<(), VmError>,
) -> Result<VirtStats, VmError> {
    let mut count = 0usize;
    k.kernel_mappings(|_| count = count.saturating_add(1))?;
    let phnum = u16::try_from(count.saturating_add(1)).map_err(|_| VmError::TooManySegments)?;
    let note_len = notes
        .iter()
        .try_fold(0u64, |a, n| add(a, offset(n.len())))?;
    let heads = offset(EHDR_SIZE)
        .checked_add(offset(PHDR_SIZE).saturating_mul(u64::from(phnum)))
        .ok_or(VmError::TooManySegments)?;
    let data_start = round_up(add(heads, note_len)?, PAGE)?;
    sink(&virt_header(A::MACHINE, phnum))?;
    let note = Phdr {
        kind: PT_NOTE,
        offset: heads,
        filesz: note_len,
        memsz: note_len,
        align: 4,
        ..Phdr::default()
    };
    sink(&note.to_le_bytes())?;
    let mut at = data_start;
    let mut err = Ok(());
    let mut seen = 0usize;
    k.kernel_mappings(|m| {
        seen = seen.saturating_add(1);
        let p = Phdr {
            kind: PT_LOAD,
            flags: 7,
            offset: at,
            vaddr: m.va,
            paddr: m.pa,
            filesz: m.len,
            memsz: m.len,
            align: PAGE,
        };
        at = at.saturating_add(m.len);
        if err.is_ok() {
            err = sink(&p.to_le_bytes());
        }
    })?;
    err?;
    if seen != count {
        return Err(VmError::BadLayout);
    }
    for n in notes {
        sink(n)?;
    }
    let pad = [0u8; PAGE as usize];
    let gap = usize::try_from(data_start.saturating_sub(add(heads, note_len)?))
        .map_err(|_| VmError::Truncated)?;
    sink(pad.get(..gap).ok_or(VmError::Truncated)?)?;
    let mut page = [0u8; PAGE as usize];
    let mut bytes = data_start;
    let mut err = Ok(());
    k.kernel_mappings(|m| {
        let mut off = 0u64;
        while off < m.len && err.is_ok() {
            let pa = m.pa.saturating_add(off);
            err = if k.mem.read(pa, &mut page) {
                sink(&page)
            } else {
                Err(VmError::NotInCore)
            };
            off = off.saturating_add(PAGE);
        }
        bytes = bytes.saturating_add(m.len);
    })?;
    err?;
    Ok(VirtStats {
        segments: count,
        bytes,
    })
}
