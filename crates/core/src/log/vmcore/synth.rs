//! Synthetic cores for the host tests, the hostlib tool's tests and the
//! fuzz seeds (C-FUZZ): physical RAM segments, a four-level page table in
//! the port's pure format built in that RAM, notes, and the ELF core QEMU's
//! `dump-guest-memory` writes around them; and a small kernel ELF with a
//! build-id note, `.text` and `.symtab`. Host only (`std`).

#![allow(
    clippy::disallowed_types,
    clippy::disallowed_macros,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "host-only test builder: `alloc`'s growing calls and indexing into its own buffers end a host process, not the kernel (DESIGN §4.4)"
)]

use std::vec::Vec;

use super::{ET_CORE, NT_PRSTATUS, PAGE, PT_NOTE, Phdr, SHT_NOTE, SHT_SYMTAB};
use crate::arch::stub::Arch;
use crate::log::vmcoreinfo;
use crate::paging::{PageFlags, PageTable, PhysAddr, VirtAddr};
use crate::proc::elf::{EHDR_SIZE, EM_X86_64, PHDR_SIZE, PT_LOAD};

/// A core under construction: RAM segments `(paddr, bytes)`, frames for
/// page tables taken from the first segment's top down, and the notes.
pub struct Synth {
    pub segs: Vec<(u64, Vec<u8>)>,
    pub notes: Vec<u8>,
    /// The next page-table frame, counting down.
    next: u64,
    pub root: u64,
}

/// Bytes a leaf at `level` maps (1: 4 KiB, 2: 2 MiB, 3: 1 GiB).
pub fn level_size(level: u8) -> u64 {
    PAGE << (9 * (u32::from(level) - 1))
}

/// One note, as a note segment holds it.
pub fn note(name: &[u8], kind: u32, desc: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&(name.len() as u32 + 1).to_le_bytes());
    v.extend_from_slice(&(desc.len() as u32).to_le_bytes());
    v.extend_from_slice(&kind.to_le_bytes());
    v.extend_from_slice(name);
    v.push(0);
    while v.len() % 4 != 0 {
        v.push(0);
    }
    v.extend_from_slice(desc);
    while v.len() % 4 != 0 {
        v.push(0);
    }
    v
}

/// An `NT_PRSTATUS` descriptor (336 bytes) with `pr_pid` and the four
/// registers the tool reads in their `pr_reg` slots.
pub fn prstatus(pid: u32, rip: u64, rsp: u64, rbp: u64) -> Vec<u8> {
    let mut d = vec![0u8; 336];
    d[32..36].copy_from_slice(&pid.to_le_bytes());
    let slot = |i: usize| 112 + i * 8;
    d[slot(4)..slot(4) + 8].copy_from_slice(&rbp.to_le_bytes());
    d[slot(16)..slot(16) + 8].copy_from_slice(&rip.to_le_bytes());
    d[slot(18)..slot(18) + 8].copy_from_slice(&0x2u64.to_le_bytes());
    d[slot(19)..slot(19) + 8].copy_from_slice(&rsp.to_le_bytes());
    d
}

impl Synth {
    /// RAM segments `(paddr, len)`, zeroed; page-table frames come from the
    /// top of the first.
    pub fn new(segs: &[(u64, u64)]) -> Self {
        let segs: Vec<(u64, Vec<u8>)> = segs
            .iter()
            .map(|&(pa, len)| (pa, vec![0u8; len as usize]))
            .collect();
        let (pa0, ref b0) = segs[0];
        let mut s = Synth {
            next: pa0 + b0.len() as u64,
            segs,
            notes: Vec::new(),
            root: 0,
        };
        s.root = s.alloc_frame();
        s
    }

    /// A zeroed frame for a page table.
    pub fn alloc_frame(&mut self) -> u64 {
        self.next -= PAGE;
        self.next
    }

    fn seg_mut(&mut self, pa: u64) -> (&mut Vec<u8>, usize) {
        let (base, bytes) = self
            .segs
            .iter_mut()
            .find(|(base, b)| pa >= *base && pa < *base + b.len() as u64)
            .unwrap_or_else(|| panic!("synth: {pa:#x} is in no segment"));
        let off = (pa - *base) as usize;
        (bytes, off)
    }

    pub fn write_phys(&mut self, pa: u64, data: &[u8]) {
        for (i, b) in data.iter().enumerate() {
            let (seg, off) = self.seg_mut(pa + i as u64);
            seg[off] = *b;
        }
    }

    pub fn read_phys(&mut self, pa: u64, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| {
                let (seg, off) = self.seg_mut(pa + i as u64);
                seg[off]
            })
            .collect()
    }

    fn pte(&mut self, table: u64, idx: usize) -> u64 {
        let b = self.read_phys(table + idx as u64 * 8, 8);
        u64::from_le_bytes(b.try_into().unwrap_or([0; 8]))
    }

    fn set_pte(&mut self, table: u64, idx: usize, e: u64) {
        self.write_phys(table + idx as u64 * 8, &e.to_le_bytes());
    }

    /// Map `va` to `pa` with one leaf of `size` (4 KiB, 2 MiB or 1 GiB)
    /// and extra `flags` (`PageFlags` bits), building tables as needed.
    pub fn map(&mut self, va: u64, pa: u64, size: u64, flags: u64) {
        let leaf = match size {
            0x1000 => 1,
            0x20_0000 => 2,
            0x4000_0000 => 3,
            _ => panic!("synth: page size {size:#x}"),
        };
        let mut table = self.root;
        let mut level = Arch::LEVELS;
        while level > leaf {
            let idx = Arch::index(VirtAddr(va), level);
            let e = self.pte(table, idx);
            table = if e & PageFlags::PRESENT != 0 {
                Arch::entry_phys(e).as_u64()
            } else {
                let t = self.alloc_frame();
                let f = PageFlags(PageFlags::PRESENT | PageFlags::WRITABLE);
                self.set_pte(table, idx, Arch::make_entry(PhysAddr(t), f));
                t
            };
            level -= 1;
        }
        let mut f = PageFlags::PRESENT | PageFlags::WRITABLE | flags;
        if leaf > 1 {
            f |= PageFlags::HUGE;
        }
        let idx = Arch::index(VirtAddr(va), leaf);
        self.set_pte(table, idx, Arch::make_entry(PhysAddr(pa), PageFlags(f)));
    }

    /// The PA of `va` under this table, if mapped.
    pub fn translate(&mut self, va: u64) -> Option<u64> {
        let mut table = self.root;
        let mut level = Arch::LEVELS;
        loop {
            let e = self.pte(table, Arch::index(VirtAddr(va), level));
            if e & PageFlags::PRESENT == 0 {
                return None;
            }
            let pa = Arch::entry_phys(e).as_u64();
            if level == 1 || e & PageFlags::HUGE != 0 {
                return Some(pa + (va & (level_size(level) - 1)));
            }
            table = pa;
            level -= 1;
        }
    }

    /// Write `data` at `va`, mapping each 4 KiB page it touches that is not
    /// mapped yet to a fresh frame from the top of the first segment.
    pub fn place(&mut self, va: u64, data: &[u8]) {
        let mut done = 0usize;
        while done < data.len() {
            let at = va + done as u64;
            let pa = match self.translate(at) {
                Some(pa) => pa,
                None => {
                    let frame = self.alloc_frame();
                    self.map(at & !(PAGE - 1), frame, PAGE, 0);
                    frame + (at & (PAGE - 1))
                }
            };
            let n = ((PAGE - (at & (PAGE - 1))) as usize).min(data.len() - done);
            self.write_phys(pa, &data[done..done + n]);
            done += n;
        }
    }

    pub fn note(&mut self, name: &[u8], kind: u32, desc: &[u8]) {
        let n = note(name, kind, desc);
        self.notes.extend_from_slice(&n);
    }

    /// A `CORE`/`NT_PRSTATUS` note.
    pub fn cpu(&mut self, pid: u32, rip: u64, rsp: u64, rbp: u64) {
        self.note(b"CORE", NT_PRSTATUS, &prstatus(pid, rip, rsp, rbp));
    }

    /// The VMCOREINFO note `info` renders, with this core's root.
    pub fn vmcoreinfo(&mut self, info: &vmcoreinfo::Info<'_>) {
        let mut buf = vec![0u8; vmcoreinfo::NOTE_MAX];
        let n = vmcoreinfo::render(info, &mut buf).unwrap_or(0);
        self.notes.extend_from_slice(&buf[..n]);
    }

    /// The ELF core: header, one `PT_NOTE`, one `PT_LOAD` per segment
    /// (`p_vaddr` 0, as a core without paging), notes, then RAM.
    pub fn build(&self) -> Vec<u8> {
        let phnum = 1 + self.segs.len();
        let heads = EHDR_SIZE + PHDR_SIZE * phnum;
        let mut out = Vec::new();
        out.extend_from_slice(&elf_header(ET_CORE, phnum as u16, 0, 0, 0));
        let mut data_at = (heads + self.notes.len()) as u64;
        let note = Phdr {
            kind: PT_NOTE,
            offset: heads as u64,
            filesz: self.notes.len() as u64,
            memsz: self.notes.len() as u64,
            ..Phdr::default()
        };
        out.extend_from_slice(&note.to_le_bytes());
        for (pa, b) in &self.segs {
            let p = Phdr {
                kind: PT_LOAD,
                offset: data_at,
                paddr: *pa,
                filesz: b.len() as u64,
                memsz: b.len() as u64,
                ..Phdr::default()
            };
            data_at += b.len() as u64;
            out.extend_from_slice(&p.to_le_bytes());
        }
        out.extend_from_slice(&self.notes);
        for (_, b) in &self.segs {
            out.extend_from_slice(b);
        }
        out
    }
}

/// An x86-64 ELF64 little-endian header.
pub fn elf_header(kind: u16, phnum: u16, shoff: u64, shnum: u16, shstrndx: u16) -> [u8; 64] {
    let mut h = [0u8; 64];
    h[..7].copy_from_slice(&[0x7F, b'E', b'L', b'F', 2, 1, 1]);
    h[16..18].copy_from_slice(&kind.to_le_bytes());
    h[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
    h[20..24].copy_from_slice(&1u32.to_le_bytes());
    h[0x20..0x28].copy_from_slice(&(EHDR_SIZE as u64).to_le_bytes());
    h[0x28..0x30].copy_from_slice(&shoff.to_le_bytes());
    h[0x34..0x36].copy_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
    h[0x36..0x38].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
    h[0x38..0x3A].copy_from_slice(&phnum.to_le_bytes());
    h[0x3A..0x3C].copy_from_slice(&64u16.to_le_bytes());
    h[0x3C..0x3E].copy_from_slice(&shnum.to_le_bytes());
    h[0x3E..0x40].copy_from_slice(&shstrndx.to_le_bytes());
    h
}

/// One symbol for [`kernel_elf`]: name (as the linker writes it), value,
/// size and `STT_*` type.
pub struct SymDef<'a> {
    pub name: &'a str,
    pub value: u64,
    pub size: u64,
    pub kind: u8,
}

/// A kernel ELF (`ET_EXEC`): one `PT_LOAD` for `text` at `text_addr`, and
/// sections `.text` (executable), `.note.gnu.build-id` (`build_id`, none
/// when empty), `.symtab`, `.strtab` and `.shstrtab`.
pub fn kernel_elf(build_id: &[u8], text_addr: u64, text: &[u8], syms: &[SymDef<'_>]) -> Vec<u8> {
    let mut shstr = vec![0u8];
    let mut name = |s: &str| {
        let at = shstr.len() as u32;
        shstr.extend_from_slice(s.as_bytes());
        shstr.push(0);
        at
    };
    let n_text = name(".text");
    let n_note = name(".note.gnu.build-id");
    let n_symtab = name(".symtab");
    let n_strtab = name(".strtab");
    let n_shstr = name(".shstrtab");
    let mut strtab = vec![0u8];
    let mut symtab = vec![0u8; 24];
    for s in syms {
        let at = strtab.len() as u32;
        strtab.extend_from_slice(s.name.as_bytes());
        strtab.push(0);
        let mut e = [0u8; 24];
        e[0..4].copy_from_slice(&at.to_le_bytes());
        e[4] = 0x10 | s.kind;
        e[6..8].copy_from_slice(&1u16.to_le_bytes());
        e[8..16].copy_from_slice(&s.value.to_le_bytes());
        e[16..24].copy_from_slice(&s.size.to_le_bytes());
        symtab.extend_from_slice(&e);
    }
    let note_sec = if build_id.is_empty() {
        Vec::new()
    } else {
        note(vmcoreinfo::GNU_NAME, vmcoreinfo::NT_GNU_BUILD_ID, build_id)
    };
    // Layout: header, one phdr, then each section's bytes, then the
    // section headers.
    let mut body = Vec::new();
    let base = (EHDR_SIZE + PHDR_SIZE) as u64;
    let mut place = |b: &[u8]| {
        while body.len() % 8 != 0 {
            body.push(0);
        }
        let at = base + body.len() as u64;
        body.extend_from_slice(b);
        at
    };
    let o_text = place(text);
    let o_note = place(&note_sec);
    let o_symtab = place(&symtab);
    let o_strtab = place(&strtab);
    let o_shstr = place(&shstr);
    let mut out = Vec::new();
    let shoff = (base + body.len() as u64 + 7) & !7;
    out.extend_from_slice(&elf_header(2, 1, shoff, 6, 5));
    let load = Phdr {
        kind: PT_LOAD,
        flags: 5,
        offset: o_text,
        vaddr: text_addr,
        paddr: text_addr,
        filesz: text.len() as u64,
        memsz: text.len() as u64,
        align: PAGE,
    };
    out.extend_from_slice(&load.to_le_bytes());
    out.extend_from_slice(&body);
    while out.len() as u64 != shoff {
        out.push(0);
    }
    let sh = |name: u32, kind: u32, flags: u64, addr: u64, off: u64, size: u64, link: u32| {
        let mut e = [0u8; 64];
        e[0..4].copy_from_slice(&name.to_le_bytes());
        e[4..8].copy_from_slice(&kind.to_le_bytes());
        e[8..16].copy_from_slice(&flags.to_le_bytes());
        e[16..24].copy_from_slice(&addr.to_le_bytes());
        e[24..32].copy_from_slice(&off.to_le_bytes());
        e[32..40].copy_from_slice(&size.to_le_bytes());
        e[40..44].copy_from_slice(&link.to_le_bytes());
        e[56..64].copy_from_slice(&(if kind == SHT_SYMTAB { 24u64 } else { 0 }).to_le_bytes());
        e
    };
    out.extend_from_slice(&[0u8; 64]);
    out.extend_from_slice(&sh(n_text, 1, 6, text_addr, o_text, text.len() as u64, 0));
    let note_kind = if build_id.is_empty() { 1 } else { SHT_NOTE };
    out.extend_from_slice(&sh(
        n_note,
        note_kind,
        2,
        0,
        o_note,
        note_sec.len() as u64,
        0,
    ));
    out.extend_from_slice(&sh(
        n_symtab,
        SHT_SYMTAB,
        0,
        0,
        o_symtab,
        symtab.len() as u64,
        4,
    ));
    out.extend_from_slice(&sh(n_strtab, 3, 0, 0, o_strtab, strtab.len() as u64, 0));
    out.extend_from_slice(&sh(n_shstr, 3, 0, 0, o_shstr, shstr.len() as u64, 0));
    out
}
