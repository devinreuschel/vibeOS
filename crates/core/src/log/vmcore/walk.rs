//! The crashed kernel's address space (ROADMAP §10.7): the page walk from
//! the root VMCOREINFO names, in the port's `PageTable` format, the
//! frame-pointer unwinder, and the virtually addressed core `gdb` opens.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use core::marker::PhantomData;

use super::{ET_CORE, PAGE, PT_NOTE, Phdr, PhysMem, SHDR_SIZE, VmError, add, offset, put_all};
use crate::paging::{PageFlags, PageTable, VirtAddr, is_canonical};
use crate::proc::elf::{EHDR_SIZE, ELFCLASS64, ELFDATA2LSB, EM_X86_64, PHDR_SIZE, PT_LOAD};

// -------------------------------------------------------------------- walk

/// One run of the kernel's mappings: `len` bytes at `va`, backed by the
/// core from `pa`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mapping {
    pub va: u64,
    pub pa: u64,
    pub len: u64,
}

/// The crashed kernel's address space, read from the core through the root
/// its note names, in the page-table format `A` (the port's pure half).
pub struct Kernel<'m, M: PhysMem, A: PageTable> {
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

/// The canonical VA of `idx`'s kernel-half slot range.
fn sign_extend(va: u64) -> u64 {
    if va & (1 << 47) != 0 {
        va | 0xFFFF_0000_0000_0000
    } else {
        va
    }
}

impl<'m, M: PhysMem, A: PageTable> Kernel<'m, M, A> {
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
    pub fn translate(&self, va: u64) -> Result<u64, VmError> {
        if !is_canonical(va) {
            return Err(VmError::NonCanonical);
        }
        let mut table = self.root;
        let mut level = A::LEVELS;
        loop {
            let e = self.entry(table, A::index(VirtAddr(va), level))?;
            let flags = A::entry_flags(e);
            if !flags.contains(PageFlags::PRESENT) {
                return Err(VmError::NotMapped);
            }
            let pa = A::entry_phys(e).as_u64();
            if level == 1 || flags.contains(PageFlags::HUGE) {
                let size = Self::leaf_size(level);
                return pa
                    .checked_add(va & size.wrapping_sub(1))
                    .ok_or(VmError::NotMapped);
            }
            table = pa;
            level = level.saturating_sub(1);
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

    /// Whether the 4 KiB page at `pa` is in the core.
    fn backed(&self, pa: u64) -> bool {
        let mut b = [0u8; 1];
        self.mem.read(pa, &mut b)
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
            let base = sign_extend(
                offset(i4)
                    .checked_mul(Self::leaf_size(top))
                    .ok_or(VmError::Levels)?,
            );
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
        let flags = A::entry_flags(e);
        if !flags.contains(PageFlags::PRESENT) {
            return Ok(());
        }
        let pa = A::entry_phys(e).as_u64();
        let size = Self::leaf_size(level);
        if level == 1 || flags.contains(PageFlags::HUGE) {
            if flags.0 & (PageFlags::PCD | PageFlags::PWT) != 0 {
                return Ok(());
            }
            let mut off = 0u64;
            while off < size {
                let (Some(v), Some(p)) = (va.checked_add(off), pa.checked_add(off)) else {
                    break;
                };
                *left = left.checked_sub(1).ok_or(VmError::WalkBudget)?;
                if self.backed(p) {
                    emit(v, p, PAGE);
                }
                off = off.saturating_add(PAGE);
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
}
// ----------------------------------------------------------- virtual core

/// What [`write_virtual_core`] wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VirtStats {
    pub segments: usize,
    pub bytes: u64,
}

fn virt_header(phnum: u16) -> [u8; EHDR_SIZE] {
    let mut h = [0u8; EHDR_SIZE];
    let ident: [u8; 7] = [0x7F, b'E', b'L', b'F', ELFCLASS64, ELFDATA2LSB, 1];
    let words: [(usize, &[u8]); 9] = [
        (0, &ident),
        (16, &ET_CORE.to_le_bytes()),
        (18, &EM_X86_64.to_le_bytes()),
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
pub fn write_virtual_core<M: PhysMem, A: PageTable>(
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
    sink(&virt_header(phnum))?;
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
