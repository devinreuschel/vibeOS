//! ELF64 parse + initial stack. ROADMAP §9.4. Mapping is the kernel half.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use crate::limits::EXEC_IMAGE_MAX;
use crate::paging::{NULL_GUARD_LEN, PAGE_SIZE_4K, USER_MAP_END, is_canonical};

mod stack;

pub use stack::{
    AT_BASE, AT_CLKTCK, AT_EGID, AT_ENTRY, AT_EUID, AT_EXECFN, AT_FLAGS, AT_GID, AT_NULL,
    AT_PAGESZ, AT_PHDR, AT_PHENT, AT_PHNUM, AT_RANDOM, AT_SECURE, AT_UID, ArgError, Auxv, ExecArgs,
    StackImage, arg_space_limit, initial_stack_len,
};

// ROADMAP §10.6's exec box: an image under the cap but larger than the
// default 128 MiB guest (192 MiB of `p_memsz`) must reach the loader.
const _: () = assert!(EXEC_IMAGE_MAX > 192 << 20);

pub const ELFMAG0: u8 = 0x7F;
pub const ELFCLASS64: u8 = 2;
pub const ELFDATA2LSB: u8 = 1;
pub const EV_CURRENT: u8 = 1;
pub const ET_EXEC: u16 = 2;
pub const EM_X86_64: u16 = 62;

pub const PT_LOAD: u32 = 1;
pub const PT_INTERP: u32 = 3;
pub const PT_PHDR: u32 = 6;
pub const PT_TLS: u32 = 7;
pub const PT_GNU_STACK: u32 = 0x6474_E551;

pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

pub const EI_NIDENT: usize = 16;
pub const EHDR_SIZE: usize = 64;
pub const PHDR_SIZE: usize = 56;

pub use crate::limits::MAX_ELF_LOADS as MAX_LOADS;

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    Truncated,
    BadMagic,
    BadClass,
    BadEndian,
    BadVersion,
    BadMachine,
    BadType,
    BadPhentsize,
    HasInterp,
    FileszGtMemsz,
    BadAlign,
    KernelVa,
    NullGuard,
    TooManyLoads,
    NoLoad,
    Overlap,
    Stack,
    /// Page-rounded `PT_LOAD` plus `PT_TLS` bytes above
    /// [`EXEC_IMAGE_MAX`], or a sum that overflows (F009).
    ImageTooBig,
}

/// An image the loader refuses is `ENOEXEC`.
impl From<ElfError> for crate::kerror::KError {
    fn from(e: ElfError) -> Self {
        match e {
            ElfError::Truncated
            | ElfError::BadMagic
            | ElfError::BadClass
            | ElfError::BadEndian
            | ElfError::BadVersion
            | ElfError::BadMachine
            | ElfError::BadType
            | ElfError::BadPhentsize
            | ElfError::HasInterp
            | ElfError::FileszGtMemsz
            | ElfError::BadAlign
            | ElfError::KernelVa
            | ElfError::NullGuard
            | ElfError::TooManyLoads
            | ElfError::NoLoad
            | ElfError::Overlap
            | ElfError::Stack
            | ElfError::ImageTooBig => Self::NoExec,
        }
    }
}

impl ElfError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Truncated => "truncated",
            Self::BadMagic => "bad magic",
            Self::BadClass => "bad class",
            Self::BadEndian => "bad endian",
            Self::BadVersion => "bad version",
            Self::BadMachine => "bad machine",
            Self::BadType => "bad type",
            Self::BadPhentsize => "bad phentsize",
            Self::HasInterp => "pt_interp",
            Self::FileszGtMemsz => "filesz>memsz",
            Self::BadAlign => "bad align",
            Self::KernelVa => "kernel va",
            Self::NullGuard => "null guard",
            Self::TooManyLoads => "too many loads",
            Self::NoLoad => "no pt_load",
            Self::Overlap => "overlap",
            Self::Stack => "stack",
            Self::ImageTooBig => "image too big",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadSeg {
    pub vaddr: u64,
    pub memsz: u64,
    pub offset: u64,
    pub filesz: u64,
    pub write: bool,
    pub exec: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TlsSeg {
    pub vaddr: u64,
    pub offset: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub align: u64,
}

impl TlsSeg {
    /// Bytes the loader maps for the TLS block: `memsz` rounded up to
    /// `align`, plus the 8-byte thread pointer slot, page-rounded and at
    /// least one page. `None` when that overflows.
    pub fn map_len(&self) -> Option<u64> {
        let aligned = if self.align <= 1 {
            self.memsz
        } else {
            let mask = self.align.checked_sub(1)?;
            self.memsz.checked_add(mask)? & !mask
        };
        let need = aligned.checked_add(8)?.max(PAGE_SIZE_4K);
        Some(need.checked_add(PAGE_SIZE_4K - 1)? & !(PAGE_SIZE_4K - 1))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Image {
    /// Length of the file the image was parsed against; every `PT_LOAD`'s
    /// and the `PT_TLS` init image's file bytes end at or below it.
    pub file_len: u64,
    pub entry: u64,
    pub loads: [LoadSeg; MAX_LOADS],
    pub nload: usize,
    pub stack_exec: bool,
    pub tls: Option<TlsSeg>,
    pub phoff: u64,
    pub phentsize: u16,
    pub phnum: u16,
    pub phdr_va: Option<u64>,
}

pub fn page_down(x: u64) -> u64 {
    x & !(PAGE_SIZE_4K - 1)
}

pub fn page_up(x: u64) -> u64 {
    x.saturating_add(PAGE_SIZE_4K - 1) & !(PAGE_SIZE_4K - 1)
}

/// The `N` bytes of `b` at `o`, or `Truncated` when they run past its end.
fn bytes_at<const N: usize>(b: &[u8], o: usize) -> Result<[u8; N], ElfError> {
    let end = o.checked_add(N).ok_or(ElfError::Truncated)?;
    b.get(o..end)
        .and_then(|s| s.try_into().ok())
        .ok_or(ElfError::Truncated)
}

fn le16(b: &[u8], o: usize) -> Result<u16, ElfError> {
    bytes_at(b, o).map(u16::from_le_bytes)
}

fn le32(b: &[u8], o: usize) -> Result<u32, ElfError> {
    bytes_at(b, o).map(u32::from_le_bytes)
}

fn le64(b: &[u8], o: usize) -> Result<u64, ElfError> {
    bytes_at(b, o).map(u64::from_le_bytes)
}

fn check_user_va(va: u64, len: u64) -> Result<(), ElfError> {
    if len == 0 {
        return Ok(());
    }
    let end = va.checked_add(len).ok_or(ElfError::KernelVa)?;
    if !is_canonical(va) || !is_canonical(end.wrapping_sub(1)) {
        return Err(ElfError::KernelVa);
    }
    if va >= USER_MAP_END || end > USER_MAP_END {
        return Err(ElfError::KernelVa);
    }
    if va < NULL_GUARD_LEN {
        return Err(ElfError::NullGuard);
    }
    Ok(())
}

fn ranges_overlap(a: u64, alen: u64, b: u64, blen: u64) -> bool {
    let ae = a.saturating_add(alen);
    let be = b.saturating_add(blen);
    a < be && b < ae
}

/// What the loader needs from the 64-byte ELF header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ehdr {
    pub entry: u64,
    pub phoff: u64,
    pub phnum: u16,
}

impl Ehdr {
    /// End of the program header table in the file; `parse_ehdr` checked
    /// it is at or below the file's length.
    fn ph_end(&self) -> Option<u64> {
        let bytes = u64::from(self.phnum).checked_mul(PHDR_SIZE as u64)?;
        self.phoff.checked_add(bytes)
    }
}

/// Check the ELF header `b` (its first [`EHDR_SIZE`] bytes) of a file of
/// `file_len` bytes, whose program header table must end at or below
/// `file_len`.
pub fn parse_ehdr(b: &[u8], file_len: u64) -> Result<Ehdr, ElfError> {
    if b.len() < EHDR_SIZE || file_len < EHDR_SIZE as u64 {
        return Err(ElfError::Truncated);
    }
    let [m0, m1, m2, m3, class, endian, version, ..] = bytes_at::<EI_NIDENT>(b, 0)?;
    if [m0, m1, m2, m3] != [ELFMAG0, b'E', b'L', b'F'] {
        return Err(ElfError::BadMagic);
    }
    if class != ELFCLASS64 {
        return Err(ElfError::BadClass);
    }
    if endian != ELFDATA2LSB {
        return Err(ElfError::BadEndian);
    }
    if version != EV_CURRENT {
        return Err(ElfError::BadVersion);
    }
    let typ = le16(b, 16)?;
    if typ != ET_EXEC {
        return Err(ElfError::BadType);
    }
    let machine = le16(b, 18)?;
    if machine != EM_X86_64 {
        return Err(ElfError::BadMachine);
    }
    let version = le32(b, 20)?;
    if version != 1 {
        return Err(ElfError::BadVersion);
    }
    let entry = le64(b, 24)?;
    let phoff = le64(b, 32)?;
    let ehsize = le16(b, 52)?;
    if ehsize as usize != EHDR_SIZE {
        return Err(ElfError::Truncated);
    }
    let phentsize = le16(b, 54)?;
    if phentsize as usize != PHDR_SIZE {
        return Err(ElfError::BadPhentsize);
    }
    let phnum = le16(b, 56)?;
    if phnum == 0 {
        return Err(ElfError::NoLoad);
    }
    let eh = Ehdr {
        entry,
        phoff,
        phnum,
    };
    if eh.ph_end().ok_or(ElfError::Truncated)? > file_len {
        return Err(ElfError::Truncated);
    }
    Ok(eh)
}

/// Builds an [`Image`] from the program headers of a file, pushed one
/// [`PHDR_SIZE`]-byte record at a time in table order, so a loader reading
/// from a file never holds the whole table.
#[derive(Clone, Copy, Debug)]
pub struct Builder {
    eh: Ehdr,
    file_len: u64,
    pushed: u16,
    loads: [LoadSeg; MAX_LOADS],
    nload: usize,
    stack_exec: bool,
    saw_gnu_stack: bool,
    tls: Option<TlsSeg>,
    phdr_va: Option<u64>,
}

impl Builder {
    pub fn new(eh: Ehdr, file_len: u64) -> Self {
        Self {
            eh,
            file_len,
            pushed: 0,
            loads: [LoadSeg {
                vaddr: 0,
                memsz: 0,
                offset: 0,
                filesz: 0,
                write: false,
                exec: false,
            }; MAX_LOADS],
            nload: 0,
            stack_exec: false,
            saw_gnu_stack: false,
            tls: None,
            phdr_va: None,
        }
    }

    /// Take the next program header record `ph`. `Truncated` past the
    /// header's `phnum`, or for a record shorter than [`PHDR_SIZE`].
    pub fn push(&mut self, ph: &[u8]) -> Result<(), ElfError> {
        if self.pushed >= self.eh.phnum || ph.len() < PHDR_SIZE {
            return Err(ElfError::Truncated);
        }
        self.pushed = self.pushed.checked_add(1).ok_or(ElfError::Truncated)?;
        let p_type = le32(ph, 0)?;
        let p_flags = le32(ph, 4)?;
        let p_offset = le64(ph, 8)?;
        let p_vaddr = le64(ph, 16)?;
        let p_filesz = le64(ph, 32)?;
        let p_memsz = le64(ph, 40)?;
        let p_align = le64(ph, 48)?;
        match p_type {
            PT_INTERP => return Err(ElfError::HasInterp),
            PT_LOAD => {
                if p_filesz > p_memsz {
                    return Err(ElfError::FileszGtMemsz);
                }
                if p_align != 0 && !p_align.is_power_of_two() {
                    return Err(ElfError::BadAlign);
                }
                check_user_va(p_vaddr, p_memsz.max(1))?;
                if p_align > 1 && p_vaddr.checked_rem(p_align) != p_offset.checked_rem(p_align) {
                    return Err(ElfError::BadAlign);
                }
                self.check_file_bytes(p_offset, p_filesz)?;
                if self
                    .loads
                    .iter()
                    .take(self.nload)
                    .any(|l| ranges_overlap(l.vaddr, l.memsz, p_vaddr, p_memsz))
                {
                    return Err(ElfError::Overlap);
                }
                let slot = self
                    .loads
                    .get_mut(self.nload)
                    .ok_or(ElfError::TooManyLoads)?;
                *slot = LoadSeg {
                    vaddr: p_vaddr,
                    memsz: p_memsz,
                    offset: p_offset,
                    filesz: p_filesz,
                    write: p_flags & PF_W != 0,
                    exec: p_flags & PF_X != 0,
                };
                self.nload = self.nload.checked_add(1).ok_or(ElfError::TooManyLoads)?;
            }
            PT_GNU_STACK => {
                self.saw_gnu_stack = true;
                self.stack_exec = p_flags & PF_X != 0;
            }
            PT_TLS => {
                if p_filesz > p_memsz {
                    return Err(ElfError::FileszGtMemsz);
                }
                if p_align != 0 && !p_align.is_power_of_two() {
                    return Err(ElfError::BadAlign);
                }
                self.check_file_bytes(p_offset, p_filesz)?;
                self.tls = Some(TlsSeg {
                    vaddr: p_vaddr,
                    offset: p_offset,
                    filesz: p_filesz,
                    memsz: p_memsz,
                    align: if p_align == 0 { 1 } else { p_align },
                });
            }
            PT_PHDR => {
                self.phdr_va = Some(p_vaddr);
            }
            _ => {}
        }
        Ok(())
    }

    /// `Truncated` unless `[offset, offset + filesz)` lies in the file.
    fn check_file_bytes(&self, offset: u64, filesz: u64) -> Result<(), ElfError> {
        let end = offset.checked_add(filesz).ok_or(ElfError::Truncated)?;
        if end > self.file_len {
            return Err(ElfError::Truncated);
        }
        Ok(())
    }

    /// The image, once every one of the header's `phnum` records was
    /// pushed; `Truncated` before.
    pub fn finish(self) -> Result<Image, ElfError> {
        if self.pushed != self.eh.phnum {
            return Err(ElfError::Truncated);
        }
        let Self {
            eh,
            file_len,
            loads,
            nload,
            mut stack_exec,
            saw_gnu_stack,
            tls,
            mut phdr_va,
            ..
        } = self;
        if nload == 0 {
            return Err(ElfError::NoLoad);
        }
        image_bytes(loads.get(..nload).ok_or(ElfError::TooManyLoads)?, tls)?;
        if !saw_gnu_stack {
            stack_exec = false;
        }
        check_user_va(eh.entry, 1)?;
        let ph_end = eh.ph_end().ok_or(ElfError::Truncated)?;
        if phdr_va.is_none() {
            // The first `PT_LOAD` whose file bytes hold the whole table maps it.
            phdr_va = loads.iter().take(nload).find_map(|s| {
                let delta = eh.phoff.checked_sub(s.offset)?;
                (ph_end <= s.offset.checked_add(s.filesz)?).then_some(())?;
                s.vaddr.checked_add(delta)
            });
        }
        if let Some(va) = phdr_va {
            let ph_len = u64::from(eh.phnum).saturating_mul(PHDR_SIZE as u64).max(1);
            check_user_va(va, ph_len)?;
        }
        Ok(Image {
            file_len,
            entry: eh.entry,
            loads,
            nload,
            stack_exec,
            tls,
            phoff: eh.phoff,
            phentsize: PHDR_SIZE as u16,
            phnum: eh.phnum,
            phdr_va,
        })
    }
}

/// Parse the whole ELF file `data`: [`parse_ehdr`], then every program
/// header through a [`Builder`].
pub fn parse(data: &[u8]) -> Result<Image, ElfError> {
    let file_len = data.len() as u64;
    let eh = parse_ehdr(data, file_len)?;
    let ph_end = eh.ph_end().ok_or(ElfError::Truncated)?;
    // `ph_end <= data.len()`, so both bounds fit a `usize`.
    let table = usize::try_from(eh.phoff)
        .ok()
        .zip(usize::try_from(ph_end).ok())
        .and_then(|(lo, hi)| data.get(lo..hi))
        .ok_or(ElfError::Truncated)?;
    let mut b = Builder::new(eh, file_len);
    // `table` is `phnum` entries of `PHDR_SIZE` bytes, so each chunk is
    // one whole entry.
    for ph in table.as_chunks::<PHDR_SIZE>().0 {
        b.push(ph)?;
    }
    b.finish()
}

/// The bytes the loader maps for `loads` and `tls`: each `PT_LOAD`'s
/// page-rounded span plus [`TlsSeg::map_len`]. Above [`EXEC_IMAGE_MAX`], or
/// on overflow, the image is refused before anything is mapped (F009).
fn image_bytes(loads: &[LoadSeg], tls: Option<TlsSeg>) -> Result<u64, ElfError> {
    let mut total = 0u64;
    for s in loads {
        if s.memsz == 0 {
            continue;
        }
        let end = s.vaddr.checked_add(s.memsz).ok_or(ElfError::ImageTooBig)?;
        let end = end
            .checked_add(PAGE_SIZE_4K - 1)
            .ok_or(ElfError::ImageTooBig)?
            & !(PAGE_SIZE_4K - 1);
        let span = end
            .checked_sub(page_down(s.vaddr))
            .ok_or(ElfError::ImageTooBig)?;
        total = total.checked_add(span).ok_or(ElfError::ImageTooBig)?;
    }
    if let Some(t) = tls {
        let len = t.map_len().ok_or(ElfError::ImageTooBig)?;
        total = total.checked_add(len).ok_or(ElfError::ImageTooBig)?;
    }
    if total > EXEC_IMAGE_MAX {
        return Err(ElfError::ImageTooBig);
    }
    Ok(total)
}

impl Image {
    /// The `PT_LOAD` segments; empty if `nload` is past `loads`, which
    /// `parse` never builds.
    pub fn loads(&self) -> &[LoadSeg] {
        self.loads.get(..self.nload).unwrap_or(&[])
    }
}

/// A run of whole pages the loader maps with one set of permissions
/// (ROADMAP §10.6, F031).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRun {
    pub start: u64,
    pub len: u64,
    pub write: bool,
    pub exec: bool,
}

/// The pages an image's `PT_LOAD`s cover, as disjoint runs sorted by
/// address: at most two per segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadPlan {
    runs: [PageRun; 2 * MAX_LOADS],
    n: usize,
}

impl LoadPlan {
    /// The runs, disjoint and in address order.
    pub fn runs(&self) -> &[PageRun] {
        self.runs.get(..self.n).unwrap_or(&[])
    }
}

/// The runs that map `img`'s `PT_LOAD`s as Linux maps them: each page
/// takes the permissions of the last segment in program-header order that
/// covers it, as each later `PT_LOAD` maps over the ones before it, and
/// neighbours with equal permissions merge.
pub fn load_plan(img: &Image) -> LoadPlan {
    let empty = PageRun {
        start: 0,
        len: 0,
        write: false,
        exec: false,
    };
    let mut plan = LoadPlan {
        runs: [empty; 2 * MAX_LOADS],
        n: 0,
    };
    // Each segment's page span; `parse` checked none overflows.
    let span = |s: &LoadSeg| (page_down(s.vaddr), page_up(s.vaddr.saturating_add(s.memsz)));
    // Every span edge, sorted and unique: between two neighbours, the
    // same segments cover every page.
    let mut edges = [0u64; 2 * MAX_LOADS];
    let mut ne = 0usize;
    for s in img.loads().iter().filter(|s| s.memsz != 0) {
        let (lo, hi) = span(s);
        for e in [lo, hi] {
            if edges.get(..ne).is_some_and(|d| d.contains(&e)) {
                continue;
            }
            if let Some(slot) = edges.get_mut(ne) {
                *slot = e;
                ne = ne.saturating_add(1);
            }
        }
    }
    let edges = edges.get_mut(..ne).unwrap_or(&mut []);
    edges.sort_unstable();
    for w in edges.windows(2) {
        let (Some(&a), Some(&b)) = (w.first(), w.get(1)) else {
            continue;
        };
        let top = img.loads().iter().rev().filter(|s| s.memsz != 0).find(|s| {
            let (lo, hi) = span(s);
            lo <= a && b <= hi
        });
        let Some(top) = top else {
            continue;
        };
        let len = b.saturating_sub(a);
        let last = plan.n.checked_sub(1).and_then(|i| plan.runs.get_mut(i));
        match last {
            Some(r)
                if r.start.saturating_add(r.len) == a
                    && r.write == top.write
                    && r.exec == top.exec =>
            {
                r.len = r.len.saturating_add(len);
            }
            _ => {
                if let Some(slot) = plan.runs.get_mut(plan.n) {
                    *slot = PageRun {
                        start: a,
                        len,
                        write: top.write,
                        exec: top.exec,
                    };
                    plan.n = plan.n.saturating_add(1);
                }
            }
        }
    }
    plan
}

/// Where [`load_segments`] maps an image: the kernel's address space, or a
/// host test's fake.
pub trait LoadTarget {
    type Error;
    /// Map `run`'s pages with its permissions, zeroed. Runs never overlap.
    fn map_zeroed(&mut self, run: PageRun) -> Result<(), Self::Error>;
    /// Copy `seg`'s `filesz` file bytes to its `vaddr`, into pages
    /// already mapped.
    fn copy(&mut self, seg: LoadSeg) -> Result<(), Self::Error>;
}

/// Map `img`'s `PT_LOAD`s into `t` as Linux does (ROADMAP §10.6, F031):
/// each run of [`load_plan`] mapped and zeroed once, then each segment's
/// file bytes copied in, so a page two segments share holds both
/// segments' bytes.
pub fn load_segments<T: LoadTarget>(img: &Image, t: &mut T) -> Result<(), T::Error> {
    for run in load_plan(img).runs() {
        t.map_zeroed(*run)?;
    }
    for seg in img.loads() {
        if seg.filesz != 0 {
            t.copy(*seg)?;
        }
    }
    Ok(())
}

/// Lay out the initial stack below the 16-aligned `stack_top`, top down:
/// 8 zero bytes; [`ExecArgs::strings`] at `strings_va`; the 16 `random`
/// bytes, 16-aligned; then the table at RSP, 16-aligned: `argc`, the
/// `argv` pointers and NULL, the `envp` pointers and NULL, `aux`,
/// `AT_RANDOM` and `AT_NULL`. `table` receives `[rsp, strings_va)`, the
/// random bytes and padding included, and must be exactly that long; the
/// caller writes it at RSP and the strings at `strings_va`.
pub fn build_initial_stack(
    stack_top: u64,
    args: &ExecArgs,
    aux: &[Auxv],
    random: &[u8; 16],
    table: &mut [u8],
) -> Result<StackImage, ElfError> {
    stack::build(stack_top, args, aux, random, table).ok_or(ElfError::Stack)
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "host tests: the module deny overrides the crate root's cfg(test) allow, and a failing index ends the test"
)]
mod tests {
    use super::*;

    fn put16(b: &mut [u8], o: usize, v: u16) {
        b[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put32(b: &mut [u8], o: usize, v: u32) {
        b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(b: &mut [u8], o: usize, v: u64) {
        b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// Minimal ET_EXEC at `load_va`. Optional extra program headers.
    fn build_elf(
        load_va: u64,
        code: &[u8],
        extra: &[(u32, u32, u64, u64, u64, u64, u64)],
    ) -> Vec<u8> {
        let nph = 1 + extra.len();
        let file_off = 0x1000u64;
        let mut b = vec![0u8; file_off as usize + code.len()];
        b[0] = 0x7F;
        b[1] = b'E';
        b[2] = b'L';
        b[3] = b'F';
        b[4] = ELFCLASS64;
        b[5] = ELFDATA2LSB;
        b[6] = EV_CURRENT;
        put16(&mut b, 16, ET_EXEC);
        put16(&mut b, 18, EM_X86_64);
        put32(&mut b, 20, 1);
        put64(&mut b, 24, load_va);
        put64(&mut b, 32, 64);
        put16(&mut b, 52, EHDR_SIZE as u16);
        put16(&mut b, 54, PHDR_SIZE as u16);
        put16(&mut b, 56, nph as u16);
        // PT_LOAD
        put32(&mut b, 64, PT_LOAD);
        put32(&mut b, 68, PF_R | PF_X);
        put64(&mut b, 72, file_off);
        put64(&mut b, 80, load_va);
        put64(&mut b, 88, load_va);
        put64(&mut b, 96, code.len() as u64);
        put64(&mut b, 104, code.len() as u64);
        put64(&mut b, 112, PAGE_SIZE_4K);
        let mut o = 64 + PHDR_SIZE;
        for &(ty, flags, off, va, filesz, memsz, align) in extra {
            put32(&mut b, o, ty);
            put32(&mut b, o + 4, flags);
            put64(&mut b, o + 8, off);
            put64(&mut b, o + 16, va);
            put64(&mut b, o + 24, va);
            put64(&mut b, o + 32, filesz);
            put64(&mut b, o + 40, memsz);
            put64(&mut b, o + 48, align);
            o += PHDR_SIZE;
        }
        b[file_off as usize..file_off as usize + code.len()].copy_from_slice(code);
        b
    }

    fn parse_err(data: &[u8]) -> ElfError {
        parse(data).expect_err("expected parse error")
    }

    #[test]
    fn load_ending_at_user_end_is_kernel_va() {
        use crate::paging::USER_END;
        let code = [0xCCu8; 4096];
        let top = build_elf(USER_END - PAGE_SIZE_4K, &code, &[]);
        assert_eq!(parse_err(&top), ElfError::KernelVa);
        let below = build_elf(USER_MAP_END - PAGE_SIZE_4K, &code, &[]);
        let img = parse(&below).unwrap();
        assert_eq!(img.loads()[0].vaddr + img.loads()[0].memsz, USER_MAP_END);
    }

    #[test]
    fn good_static_exec() {
        let code = [0x90u8, 0x90, 0xC3];
        let elf = build_elf(0x4000_0000, &code, &[]);
        let img = parse(&elf).unwrap();
        assert_eq!(img.entry, 0x4000_0000);
        assert_eq!(img.nload, 1);
        assert!(!img.stack_exec);
        assert!(img.tls.is_none());
        assert_eq!(img.loads()[0].filesz, 3);
        assert_eq!(img.loads()[0].offset, 0x1000);
        assert_eq!(img.file_len, 0x1000 + 3);
    }

    #[test]
    fn truncated_header_and_phdrs() {
        assert_eq!(parse_err(&[0x7F, b'E', b'L']), ElfError::Truncated);
        let mut short = build_elf(0x4000_0000, &[0x90], &[]);
        short.truncate(EHDR_SIZE);
        assert_eq!(parse_err(&short), ElfError::Truncated);
        let mut cut = build_elf(0x4000_0000, &[0x90], &[]);
        cut.truncate(64 + 20);
        assert_eq!(parse_err(&cut), ElfError::Truncated);
        let mut body = build_elf(0x4000_0000, &[0x90, 0x90, 0x90, 0x90], &[]);
        body.truncate(0x1000 + 1);
        assert_eq!(parse_err(&body), ElfError::Truncated);
    }

    #[test]
    fn bad_class_endian_machine_type() {
        let mut c = build_elf(0x4000_0000, &[0x90], &[]);
        c[4] = 1;
        assert_eq!(parse_err(&c), ElfError::BadClass);
        let mut e = build_elf(0x4000_0000, &[0x90], &[]);
        e[5] = 2;
        assert_eq!(parse_err(&e), ElfError::BadEndian);
        let mut m = build_elf(0x4000_0000, &[0x90], &[]);
        put16(&mut m, 18, 40);
        assert_eq!(parse_err(&m), ElfError::BadMachine);
        let mut t = build_elf(0x4000_0000, &[0x90], &[]);
        put16(&mut t, 16, 3); // ET_DYN
        assert_eq!(parse_err(&t), ElfError::BadType);
        let mut mag = build_elf(0x4000_0000, &[0x90], &[]);
        mag[1] = b'F';
        assert_eq!(parse_err(&mag), ElfError::BadMagic);
    }

    #[test]
    fn pt_interp_refused() {
        let extra = [(PT_INTERP, PF_R, 0u64, 0u64, 0u64, 0u64, 1u64)];
        let elf = build_elf(0x4000_0000, &[0x90], &extra);
        assert_eq!(parse_err(&elf), ElfError::HasInterp);
    }

    #[test]
    fn gnu_stack_and_tls_and_bss() {
        let extra = [
            (PT_GNU_STACK, PF_R | PF_W, 0u64, 0u64, 0, 0, 16u64),
            (PT_TLS, PF_R | PF_W, 0x1000u64, 0x4000_0000u64, 1, 8, 8u64),
        ];
        let elf = build_elf(0x4000_0000, &[0x90, 0, 0, 0, 0, 0, 0, 0], &extra);
        let img = parse(&elf).unwrap();
        assert!(!img.stack_exec);
        let tls = img.tls.unwrap();
        assert_eq!(tls.memsz, 8);
        assert_eq!(tls.filesz, 1);
    }

    #[test]
    fn gnu_stack_exec() {
        let extra = [(PT_GNU_STACK, PF_R | PF_W | PF_X, 0u64, 0u64, 0, 0, 16u64)];
        let elf = build_elf(0x4000_0000, &[0x90], &extra);
        assert!(parse(&elf).unwrap().stack_exec);
    }

    #[test]
    fn filesz_gt_memsz_and_kernel_va() {
        let extra = [(
            PT_LOAD,
            PF_R,
            0x1000u64,
            0x4000_1000u64,
            16u64,
            4u64,
            0x1000u64,
        )];
        // two PT_LOADs: builder always emits one; extra adds a bad one.
        // The extra is a second LOAD with filesz>memsz.
        let elf = build_elf(0x4000_0000, &[0u8; 32], &extra);
        assert_eq!(parse_err(&elf), ElfError::FileszGtMemsz);

        let bad = build_elf(0xFFFF_8000_0000_0000, &[0x90], &[]);
        assert_eq!(parse_err(&bad), ElfError::KernelVa);
        let low = build_elf(0x100, &[0x90], &[]);
        assert_eq!(parse_err(&low), ElfError::NullGuard);
        let page0 = build_elf(0, &[0x90], &[]);
        assert_eq!(parse_err(&page0), ElfError::NullGuard);
        let mis = build_elf(0x4000_0010, &[0x90], &[]);
        assert_eq!(parse_err(&mis), ElfError::BadAlign);
    }

    #[test]
    fn overlap_and_no_load() {
        let extra = [(
            PT_LOAD,
            PF_R,
            0x1000u64,
            0x4000_0000u64,
            1u64,
            1u64,
            0x1000u64,
        )];
        let elf = build_elf(0x4000_0000, &[0x90], &extra);
        assert_eq!(parse_err(&elf), ElfError::Overlap);
        let mut noload = build_elf(0x4000_0000, &[0x90], &[]);
        put32(&mut noload, 64, 0x6000_0000); // PT_LOOS-ish, not LOAD
        assert_eq!(parse_err(&noload), ElfError::NoLoad);
    }

    #[test]
    fn error_names_exhaustive() {
        for e in [
            ElfError::Truncated,
            ElfError::BadMagic,
            ElfError::BadClass,
            ElfError::BadEndian,
            ElfError::BadVersion,
            ElfError::BadMachine,
            ElfError::BadType,
            ElfError::BadPhentsize,
            ElfError::HasInterp,
            ElfError::FileszGtMemsz,
            ElfError::BadAlign,
            ElfError::KernelVa,
            ElfError::NullGuard,
            ElfError::TooManyLoads,
            ElfError::NoLoad,
            ElfError::Overlap,
            ElfError::Stack,
            ElfError::ImageTooBig,
        ] {
            match e {
                ElfError::Truncated
                | ElfError::BadMagic
                | ElfError::BadClass
                | ElfError::BadEndian
                | ElfError::BadVersion
                | ElfError::BadMachine
                | ElfError::BadType
                | ElfError::BadPhentsize
                | ElfError::HasInterp
                | ElfError::FileszGtMemsz
                | ElfError::BadAlign
                | ElfError::KernelVa
                | ElfError::NullGuard
                | ElfError::TooManyLoads
                | ElfError::NoLoad
                | ElfError::Overlap
                | ElfError::Stack
                | ElfError::ImageTooBig => {
                    assert!(!e.as_str().is_empty());
                }
            }
        }
    }

    /// Set the builder's first `PT_LOAD`'s `p_memsz`.
    fn set_memsz(b: &mut [u8], memsz: u64) {
        put64(b, 64 + 40, memsz);
    }

    #[test]
    fn exec_cap_rejects_64g_accepts_192m() {
        let code = [0x90u8, 0xC3];
        let mut e = build_elf(0x4000_0000, &code, &[]);
        set_memsz(&mut e, 64 << 30);
        assert_eq!(parse_err(&e), ElfError::ImageTooBig);
        set_memsz(&mut e, 192 << 20);
        assert!(parse(&e).is_ok());
        set_memsz(&mut e, EXEC_IMAGE_MAX);
        assert!(parse(&e).is_ok());
        set_memsz(&mut e, EXEC_IMAGE_MAX + PAGE_SIZE_4K);
        assert_eq!(parse_err(&e), ElfError::ImageTooBig);
        // One byte past the cap is one more page.
        set_memsz(&mut e, EXEC_IMAGE_MAX + 1);
        assert_eq!(parse_err(&e), ElfError::ImageTooBig);
    }

    #[test]
    fn exec_cap_counts_tls() {
        let code = [0x90u8, 0xC3];
        let tls =
            |memsz: u64, align: u64| (PT_TLS, PF_R, 0x1000u64, 0x4000_0000u64, 0, memsz, align);
        let mut e = build_elf(0x4000_0000, &code, &[tls(8 << 10, 8)]);
        set_memsz(&mut e, EXEC_IMAGE_MAX - PAGE_SIZE_4K);
        assert_eq!(parse_err(&e), ElfError::ImageTooBig);
        // A small TLS block maps one page, which fills the cap exactly.
        let mut e = build_elf(0x4000_0000, &code, &[tls(8, 8)]);
        set_memsz(&mut e, EXEC_IMAGE_MAX - PAGE_SIZE_4K);
        assert!(parse(&e).is_ok());
        let e = build_elf(0x4000_0000, &code, &[tls(8, 3)]);
        assert_eq!(parse_err(&e), ElfError::BadAlign);
        let e = build_elf(0x4000_0000, &code, &[tls(8, 1 << 40)]);
        assert_eq!(parse_err(&e), ElfError::ImageTooBig);
        let e = build_elf(0x4000_0000, &code, &[tls(8, 1 << 63)]);
        assert_eq!(parse_err(&e), ElfError::ImageTooBig);
        let seg = |memsz, align| TlsSeg {
            vaddr: 0,
            offset: 0,
            filesz: 0,
            memsz,
            align,
        };
        assert_eq!(seg(0, 1).map_len(), Some(PAGE_SIZE_4K));
        assert_eq!(seg(4088, 8).map_len(), Some(PAGE_SIZE_4K));
        assert_eq!(seg(4089, 8).map_len(), Some(2 * PAGE_SIZE_4K));
        assert_eq!(seg(8, 1 << 20).map_len(), Some((1 << 20) + PAGE_SIZE_4K));
        assert_eq!(seg(u64::MAX, 1).map_len(), None);
        assert_eq!(seg(u64::MAX - 4, 1).map_len(), None);
    }

    #[test]
    fn fixed_tables_match_limits() {
        let elf = build_elf(0x4000_0000, &[0x90, 0xC3], &[]);
        let img = parse(&elf).unwrap();
        assert_eq!(img.loads.len(), crate::limits::MAX_ELF_LOADS);
    }

    const STATIC_LLD: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/elf/static-lld"
    ));
    const STATIC_GNULD: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/elf/static-gnuld"
    ));

    fn seg(vaddr: u64, offset: u64, filesz: u64, memsz: u64, write: bool, exec: bool) -> LoadSeg {
        LoadSeg {
            vaddr,
            memsz,
            offset,
            filesz,
            write,
            exec,
        }
    }

    /// The checked-in static binaries from `tests/fixtures/elf/start.S`
    /// parse to what `llvm-readobj --elf-output-style=GNU -h -l` prints for
    /// them, and each cut one byte short of its last segment's file end is
    /// `Truncated`.
    #[test]
    fn elf_parse_linked_static_binaries() {
        struct Want<'a> {
            name: &'static str,
            data: &'static [u8],
            entry: u64,
            phnum: u16,
            loads: &'a [LoadSeg],
            tls: TlsSeg,
            phdr_va: u64,
        }
        let lld = [
            seg(0x20_0000, 0x0, 0x216, 0x216, false, false),
            seg(0x20_1218, 0x218, 0x29, 0x29, false, true),
            seg(0x20_2248, 0x248, 0x8, 0xdb8, true, false),
            seg(0x20_3250, 0x250, 0x8, 0x3db0, true, false),
        ];
        let gnuld = [
            seg(0x40_0000, 0x0, 0x1c8, 0x1c8, false, false),
            seg(0x40_1000, 0x1000, 0x29, 0x29, false, true),
            seg(0x40_2000, 0x2000, 0x16, 0x16, false, false),
            seg(0x40_3ff8, 0x2ff8, 0x10, 0x4008, true, false),
        ];
        let wants = [
            Want {
                name: "static-lld",
                data: STATIC_LLD,
                entry: 0x20_1218,
                phnum: 8,
                loads: &lld,
                tls: TlsSeg {
                    vaddr: 0x20_2248,
                    offset: 0x248,
                    filesz: 0x8,
                    memsz: 0x48,
                    align: 8,
                },
                phdr_va: 0x20_0040,
            },
            Want {
                name: "static-gnuld",
                data: STATIC_GNULD,
                entry: 0x40_1000,
                phnum: 7,
                loads: &gnuld,
                tls: TlsSeg {
                    vaddr: 0x40_3ff8,
                    offset: 0x2ff8,
                    filesz: 0x8,
                    memsz: 0x48,
                    align: 8,
                },
                phdr_va: 0x40_0040,
            },
        ];
        for w in wants {
            let img = parse(w.data).unwrap_or_else(|e| panic!("{}: {}", w.name, e.as_str()));
            assert_eq!(img.entry, w.entry, "{}", w.name);
            assert_eq!(img.phnum, w.phnum, "{}", w.name);
            assert_eq!(img.loads(), w.loads, "{}", w.name);
            assert_eq!(img.tls, Some(w.tls), "{}", w.name);
            assert!(!img.stack_exec, "{}", w.name);
            assert_eq!(img.phdr_va, Some(w.phdr_va), "{}", w.name);
            let end = w.loads.iter().map(|s| s.offset + s.filesz).max().unwrap();
            assert_eq!(
                parse_err(&w.data[..end as usize - 1]),
                ElfError::Truncated,
                "{}",
                w.name
            );
            assert!(parse(&w.data[..end as usize]).is_ok(), "{}", w.name);
        }
    }

    /// `parse_ehdr` on `data`'s header and a `Builder` fed its program
    /// headers one record at a time, both against `file_len`.
    fn build_with(data: &[u8], file_len: u64) -> Result<Image, ElfError> {
        let eh = parse_ehdr(data, file_len)?;
        let mut b = Builder::new(eh, file_len);
        let lo = eh.phoff as usize;
        for i in 0..eh.phnum as usize {
            let o = lo + i * PHDR_SIZE;
            b.push(&data[o..o + PHDR_SIZE])?;
        }
        b.finish()
    }

    /// Every `PT_LOAD`'s file bytes, the `PT_TLS` init image and the
    /// program header table end at or below the file's length: one byte
    /// past it, or an `offset + filesz` that overflows, is `Truncated`.
    #[test]
    fn elf_parse_file_len_bounds() {
        let code = [0x90u8; 16];
        let elf = build_elf(0x4000_0000, &code, &[]);
        let end = elf.len() as u64;
        assert!(build_with(&elf, end).is_ok());
        assert_eq!(build_with(&elf, end - 1), Err(ElfError::Truncated));
        assert_eq!(parse_err(&elf[..elf.len() - 1]), ElfError::Truncated);

        let tls = |off: u64, filesz: u64| {
            build_elf(
                0x4000_0000,
                &code,
                &[(PT_TLS, PF_R, off, 0x4000_0000, filesz, 16, 8)],
            )
        };
        let exact = tls(0x1000, 16);
        assert!(parse(&exact).is_ok());
        assert_eq!(parse_err(&tls(0x1001, 16)), ElfError::Truncated);
        assert_eq!(parse_err(&tls(u64::MAX - 7, 16)), ElfError::Truncated);
        assert_eq!(
            build_with(&exact, exact.len() as u64 - 1),
            Err(ElfError::Truncated)
        );

        let mut over = build_elf(0x4000_0000, &code, &[]);
        // PT_LOAD p_offset, aligned like p_vaddr, and p_filesz = p_memsz
        // so that their sum is 2^64.
        put64(&mut over, 72, 0u64.wrapping_sub(0x1000));
        put64(&mut over, 96, 0x1000);
        put64(&mut over, 104, 0x1000);
        assert_eq!(parse_err(&over), ElfError::Truncated);

        // The table itself: 64 + 56 bytes ends at 120.
        let two = build_elf(0x4000_0000, &code, &[(PT_GNU_STACK, PF_R, 0, 0, 0, 0, 16)]);
        let table_end = (EHDR_SIZE + 2 * PHDR_SIZE) as u64;
        assert!(parse_ehdr(&two, table_end).is_ok());
        assert_eq!(parse_ehdr(&two, table_end - 1), Err(ElfError::Truncated));
        assert_eq!(
            parse_ehdr(&two[..EHDR_SIZE - 1], table_end),
            Err(ElfError::Truncated)
        );

        // A push past `phnum`, and a finish short of it.
        let eh = parse_ehdr(&elf, end).unwrap();
        let mut b = Builder::new(eh, end);
        assert_eq!(b.finish(), Err(ElfError::Truncated));
        b.push(&elf[64..64 + PHDR_SIZE]).unwrap();
        assert_eq!(b.push(&elf[64..64 + PHDR_SIZE]), Err(ElfError::Truncated));
        assert_eq!(
            b.push(&elf[64..64 + PHDR_SIZE - 1]),
            Err(ElfError::Truncated)
        );
    }

    /// A `Builder` fed one record at a time gives what `parse` gives, for
    /// the `build_elf` images and both linked fixtures.
    #[test]
    fn elf_builder_matches_parse() {
        let code = [0x90u8, 0xC3];
        let images = [
            build_elf(0x4000_0000, &code, &[]),
            build_elf(
                0x4000_0000,
                &[0x90, 0, 0, 0, 0, 0, 0, 0],
                &[
                    (PT_GNU_STACK, PF_R | PF_W, 0, 0, 0, 0, 16),
                    (PT_TLS, PF_R | PF_W, 0x1000, 0x4000_0000, 1, 8, 8),
                ],
            ),
            build_elf(
                0x4000_0000,
                &code,
                &[(PT_GNU_STACK, PF_R | PF_W | PF_X, 0, 0, 0, 0, 16)],
            ),
            build_elf(0x4000_0000, &code, &[(PT_INTERP, PF_R, 0, 0, 0, 0, 1)]),
            STATIC_LLD.to_vec(),
            STATIC_GNULD.to_vec(),
        ];
        for (i, img) in images.iter().enumerate() {
            assert_eq!(build_with(img, img.len() as u64), parse(img), "image {i}");
        }
    }

    /// `Image`'s fields are public, so a hand-built one can carry an
    /// `nload` past `loads`; `loads()` used to slice out of bounds.
    #[test]
    fn elf_rejects_nload_past_loads() {
        let elf = build_elf(0x4000_0000, &[0x90, 0xC3], &[]);
        let mut img = parse(&elf).unwrap();
        assert_eq!(img.loads().len(), 1);
        img.nload = MAX_LOADS + 1;
        assert!(img.loads().is_empty());
    }

    /// A fake address space: pages mapped by `map_zeroed`, which refuses
    /// a page mapped twice, and the file bytes `copy` writes.
    struct FakeTarget<'a> {
        file: &'a [u8],
        pages: std::collections::BTreeMap<u64, (bool, bool, Vec<u8>)>,
        maps: usize,
    }

    impl<'a> FakeTarget<'a> {
        fn new(file: &'a [u8]) -> Self {
            FakeTarget {
                file,
                pages: std::collections::BTreeMap::new(),
                maps: 0,
            }
        }

        fn byte(&self, va: u64) -> u8 {
            let p = &self.pages[&page_down(va)];
            p.2[(va - page_down(va)) as usize]
        }

        /// `(write, exec)` of the page holding `va`.
        fn perms(&self, va: u64) -> (bool, bool) {
            let p = &self.pages[&page_down(va)];
            (p.0, p.1)
        }
    }

    impl LoadTarget for FakeTarget<'_> {
        type Error = &'static str;

        fn map_zeroed(&mut self, run: PageRun) -> Result<(), Self::Error> {
            assert_eq!(run.start % PAGE_SIZE_4K, 0);
            assert_eq!(run.len % PAGE_SIZE_4K, 0);
            assert!(run.len > 0);
            self.maps += 1;
            let mut va = run.start;
            while va < run.start + run.len {
                let page = (run.write, run.exec, vec![0u8; PAGE_SIZE_4K as usize]);
                if self.pages.insert(va, page).is_some() {
                    return Err("overlapping map");
                }
                va += PAGE_SIZE_4K;
            }
            Ok(())
        }

        fn copy(&mut self, seg: LoadSeg) -> Result<(), Self::Error> {
            for i in 0..seg.filesz {
                let b = self.file[(seg.offset + i) as usize];
                let va = seg.vaddr + i;
                let p = self
                    .pages
                    .get_mut(&page_down(va))
                    .ok_or("copy to unmapped")?;
                p.2[(va - page_down(va)) as usize] = b;
            }
            Ok(())
        }
    }

    /// An image of `loads` over a file whose byte at offset `o` is `o`.
    fn plan_image(loads: &[LoadSeg]) -> (Image, Vec<u8>) {
        let mut img = parse(&build_elf(0x4000_0000, &[0x90, 0xC3], &[])).unwrap();
        img.loads[..loads.len()].copy_from_slice(loads);
        img.nload = loads.len();
        let len = loads.iter().map(|s| s.offset + s.filesz).max().unwrap();
        let file = (0..len).map(|o| o as u8).collect();
        (img, file)
    }

    fn run(start: u64, len: u64, write: bool, exec: bool) -> PageRun {
        PageRun {
            start,
            len,
            write,
            exec,
        }
    }

    #[test]
    fn load_plan_shared_page_rx_rw() {
        const B: u64 = 0x4000_0000;
        let (img, file) = plan_image(&[
            seg(B, 0, 0x1010, 0x1010, false, true),
            seg(B + 0x1010, 0x1010, 8, 8, true, false),
        ]);
        assert_eq!(
            load_plan(&img).runs(),
            &[
                run(B, 0x1000, false, true),
                run(B + 0x1000, 0x1000, true, false)
            ]
        );
        let mut t = FakeTarget::new(&file);
        load_segments(&img, &mut t).unwrap();
        assert_eq!(t.maps, 2);
        // The shared page is RW and not executable, and holds both
        // segments' bytes.
        assert_eq!(t.perms(B + 0x1000), (true, false));
        assert_eq!(t.perms(B), (false, true));
        for va in B..B + 0x1018 {
            assert_eq!(t.byte(va), (va - B) as u8, "{va:#x}");
        }
        assert_eq!(t.byte(B + 0x1018), 0);
    }

    #[test]
    fn load_plan_second_segment_past_shared_page() {
        const B: u64 = 0x4000_0000;
        // RX to the middle of its second page; RW from there through two
        // more pages, with bss past its file bytes.
        let (img, file) = plan_image(&[
            seg(B, 0, 0x1800, 0x1800, false, true),
            seg(B + 0x1800, 0x1800, 0x900, 0x1900, true, false),
        ]);
        assert_eq!(
            load_plan(&img).runs(),
            &[
                run(B, 0x1000, false, true),
                run(B + 0x1000, 0x3000, true, false)
            ]
        );
        let mut t = FakeTarget::new(&file);
        load_segments(&img, &mut t).unwrap();
        assert_eq!(t.maps, 2);
        for va in B..B + 0x2100 {
            assert_eq!(t.byte(va), (va - B) as u8, "{va:#x}");
        }
        for va in B + 0x2100..B + 0x4000 {
            assert_eq!(t.byte(va), 0, "{va:#x}");
        }
        assert_eq!(t.perms(B + 0x3fff), (true, false));
    }

    #[test]
    fn load_plan_shared_page_fixture() {
        const SHARED: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/elf/shared_page.elf"
        ));
        const B: u64 = 0x4000_0000;
        let img = parse(SHARED).unwrap();
        assert_eq!(img.entry, B + 0x100);
        assert_eq!(
            load_plan(&img).runs(),
            &[
                run(B, 0x1000, false, true),
                run(B + 0x1000, 0x1000, true, false)
            ]
        );
        let mut t = FakeTarget::new(SHARED);
        load_segments(&img, &mut t).unwrap();
        // The RX tail's constant and the RW value both survive.
        let word = |va: u64| u64::from_le_bytes(core::array::from_fn(|i| t.byte(va + i as u64)));
        assert_eq!(word(B + 0x1000).to_le_bytes(), *b"_RX_SHER");
        assert_eq!(word(B + 0x1010).to_le_bytes(), *b"_RW_DATA");
        assert_eq!(t.perms(B + 0x1000), (true, false));
    }

    #[test]
    fn load_plan_separate_pages() {
        // A linked binary's segments start on their own pages: every page
        // takes its one segment's permissions and file bytes, and runs
        // merge only neighbours with equal permissions (lld's two RW
        // segments).
        for bin in [STATIC_LLD, STATIC_GNULD] {
            let img = parse(bin).unwrap();
            let loads: Vec<_> = img.loads().iter().filter(|s| s.memsz != 0).collect();
            let runs = load_plan(&img).runs().to_vec();
            assert!(runs.len() <= loads.len());
            for w in runs.windows(2) {
                assert!(w[0].start + w[0].len <= w[1].start);
            }
            let mut t = FakeTarget::new(bin);
            load_segments(&img, &mut t).unwrap();
            for s in &loads {
                let mut va = page_down(s.vaddr);
                while va < page_up(s.vaddr + s.memsz) {
                    assert_eq!(t.perms(va), (s.write, s.exec), "{va:#x}");
                    va += PAGE_SIZE_4K;
                }
                for i in 0..s.filesz {
                    assert_eq!(t.byte(s.vaddr + i), bin[(s.offset + i) as usize]);
                }
            }
            let pages: u64 = runs.iter().map(|r| r.len / PAGE_SIZE_4K).sum();
            assert_eq!(pages, t.pages.len() as u64);
        }
        // Two segments a page apart with equal permissions stay two runs.
        const B: u64 = 0x4000_0000;
        let (img, _) = plan_image(&[
            seg(B, 0, 0x10, 0x10, true, false),
            seg(B + 0x2000, 0x10, 0x10, 0x10, true, false),
        ]);
        assert_eq!(
            load_plan(&img).runs(),
            &[
                run(B, 0x1000, true, false),
                run(B + 0x2000, 0x1000, true, false)
            ]
        );
    }
}
