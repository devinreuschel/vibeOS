//! ELF64 parse + initial stack. ROADMAP §9.4. Mapping is the kernel half.

use crate::paging::{is_canonical, NULL_GUARD_LEN, PAGE_SIZE_4K, USER_END};

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

pub const MAX_LOADS: usize = 8;

pub const AT_NULL: u64 = 0;
pub const AT_PHDR: u64 = 3;
pub const AT_PHENT: u64 = 4;
pub const AT_PHNUM: u64 = 5;
pub const AT_PAGESZ: u64 = 6;
pub const AT_BASE: u64 = 7;
pub const AT_FLAGS: u64 = 8;
pub const AT_ENTRY: u64 = 9;
pub const AT_UID: u64 = 11;
pub const AT_EUID: u64 = 12;
pub const AT_GID: u64 = 13;
pub const AT_EGID: u64 = 14;
pub const AT_CLKTCK: u64 = 17;
pub const AT_SECURE: u64 = 23;
pub const AT_RANDOM: u64 = 25;
pub const AT_EXECFN: u64 = 31;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Image<'a> {
    pub data: &'a [u8],
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Auxv {
    pub tag: u64,
    pub val: u64,
}

pub fn page_down(x: u64) -> u64 {
    x & !(PAGE_SIZE_4K - 1)
}

pub fn page_up(x: u64) -> u64 {
    x.saturating_add(PAGE_SIZE_4K - 1) & !(PAGE_SIZE_4K - 1)
}

fn le16(b: &[u8], o: usize) -> Result<u16, ElfError> {
    let s = b.get(o..o + 2).ok_or(ElfError::Truncated)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn le32(b: &[u8], o: usize) -> Result<u32, ElfError> {
    let s = b.get(o..o + 4).ok_or(ElfError::Truncated)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn le64(b: &[u8], o: usize) -> Result<u64, ElfError> {
    let s = b.get(o..o + 8).ok_or(ElfError::Truncated)?;
    Ok(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

fn check_user_va(va: u64, len: u64) -> Result<(), ElfError> {
    if len == 0 {
        return Ok(());
    }
    let end = va.checked_add(len).ok_or(ElfError::KernelVa)?;
    if !is_canonical(va) || !is_canonical(end.wrapping_sub(1)) {
        return Err(ElfError::KernelVa);
    }
    if va >= USER_END || end > USER_END {
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

pub fn parse(data: &[u8]) -> Result<Image<'_>, ElfError> {
    if data.len() < EHDR_SIZE {
        return Err(ElfError::Truncated);
    }
    if data[0] != ELFMAG0 || data[1] != b'E' || data[2] != b'L' || data[3] != b'F' {
        return Err(ElfError::BadMagic);
    }
    if data[4] != ELFCLASS64 {
        return Err(ElfError::BadClass);
    }
    if data[5] != ELFDATA2LSB {
        return Err(ElfError::BadEndian);
    }
    if data[6] != EV_CURRENT {
        return Err(ElfError::BadVersion);
    }
    let typ = le16(data, 16)?;
    if typ != ET_EXEC {
        return Err(ElfError::BadType);
    }
    let machine = le16(data, 18)?;
    if machine != EM_X86_64 {
        return Err(ElfError::BadMachine);
    }
    let version = le32(data, 20)?;
    if version != 1 {
        return Err(ElfError::BadVersion);
    }
    let entry = le64(data, 24)?;
    let phoff = le64(data, 32)?;
    let ehsize = le16(data, 52)?;
    if ehsize as usize != EHDR_SIZE {
        return Err(ElfError::Truncated);
    }
    let phentsize = le16(data, 54)?;
    if phentsize as usize != PHDR_SIZE {
        return Err(ElfError::BadPhentsize);
    }
    let phnum = le16(data, 56)?;
    if phnum == 0 {
        return Err(ElfError::NoLoad);
    }
    let ph_bytes = (phnum as u64)
        .checked_mul(phentsize as u64)
        .ok_or(ElfError::Truncated)?;
    let ph_end = phoff.checked_add(ph_bytes).ok_or(ElfError::Truncated)?;
    if ph_end > data.len() as u64 {
        return Err(ElfError::Truncated);
    }

    let mut loads = [LoadSeg {
        vaddr: 0,
        memsz: 0,
        offset: 0,
        filesz: 0,
        write: false,
        exec: false,
    }; MAX_LOADS];
    let mut nload = 0usize;
    let mut stack_exec = false;
    let mut saw_gnu_stack = false;
    let mut tls = None;
    let mut phdr_va = None;
    let mut i = 0u16;
    while i < phnum {
        let o = (phoff as usize) + (i as usize) * PHDR_SIZE;
        let p_type = le32(data, o)?;
        let p_flags = le32(data, o + 4)?;
        let p_offset = le64(data, o + 8)?;
        let p_vaddr = le64(data, o + 16)?;
        let p_filesz = le64(data, o + 32)?;
        let p_memsz = le64(data, o + 40)?;
        let p_align = le64(data, o + 48)?;
        match p_type {
            PT_INTERP => return Err(ElfError::HasInterp),
            PT_LOAD => {
                if p_filesz > p_memsz {
                    return Err(ElfError::FileszGtMemsz);
                }
                if p_align != 0 && (p_align & (p_align - 1)) != 0 {
                    return Err(ElfError::BadAlign);
                }
                check_user_va(p_vaddr, p_memsz.max(1))?;
                if p_align > 1 && (p_vaddr & (p_align - 1)) != (p_offset & (p_align - 1)) {
                    return Err(ElfError::BadAlign);
                }
                let file_end = p_offset.checked_add(p_filesz).ok_or(ElfError::Truncated)?;
                if file_end > data.len() as u64 {
                    return Err(ElfError::Truncated);
                }
                let mut j = 0;
                while j < nload {
                    if ranges_overlap(loads[j].vaddr, loads[j].memsz, p_vaddr, p_memsz) {
                        return Err(ElfError::Overlap);
                    }
                    j += 1;
                }
                if nload >= MAX_LOADS {
                    return Err(ElfError::TooManyLoads);
                }
                loads[nload] = LoadSeg {
                    vaddr: p_vaddr,
                    memsz: p_memsz,
                    offset: p_offset,
                    filesz: p_filesz,
                    write: p_flags & PF_W != 0,
                    exec: p_flags & PF_X != 0,
                };
                nload += 1;
            }
            PT_GNU_STACK => {
                saw_gnu_stack = true;
                stack_exec = p_flags & PF_X != 0;
            }
            PT_TLS => {
                if p_filesz > p_memsz {
                    return Err(ElfError::FileszGtMemsz);
                }
                tls = Some(TlsSeg {
                    vaddr: p_vaddr,
                    offset: p_offset,
                    filesz: p_filesz,
                    memsz: p_memsz,
                    align: if p_align == 0 { 1 } else { p_align },
                });
            }
            PT_PHDR => {
                phdr_va = Some(p_vaddr);
            }
            _ => {}
        }
        i += 1;
    }
    if nload == 0 {
        return Err(ElfError::NoLoad);
    }
    if !saw_gnu_stack {
        stack_exec = false;
    }
    check_user_va(entry, 1)?;
    if phdr_va.is_none() {
        let mut j = 0;
        while j < nload {
            let s = loads[j];
            if s.offset <= phoff && phoff + (phnum as u64) * (phentsize as u64) <= s.offset + s.filesz
            {
                phdr_va = Some(s.vaddr + (phoff - s.offset));
                break;
            }
            j += 1;
        }
    }
    Ok(Image {
        data,
        entry,
        loads,
        nload,
        stack_exec,
        tls,
        phoff,
        phentsize,
        phnum,
        phdr_va,
    })
}

impl Image<'_> {
    pub fn loads(&self) -> &[LoadSeg] {
        &self.loads[..self.nload]
    }

    pub fn file_bytes(&self, seg: LoadSeg) -> Result<&[u8], ElfError> {
        let start = seg.offset as usize;
        let end = start
            .checked_add(seg.filesz as usize)
            .ok_or(ElfError::Truncated)?;
        self.data.get(start..end).ok_or(ElfError::Truncated)
    }
}

/// Fill `mem` as `[stack_top - mem.len(), stack_top)`. Returns user RSP.
pub fn build_initial_stack(
    stack_top: u64,
    mem: &mut [u8],
    argv: &[&[u8]],
    envp: &[&[u8]],
    aux: &[Auxv],
    random: &[u8; 16],
) -> Result<u64, ElfError> {
    if mem.is_empty() {
        return Err(ElfError::Stack);
    }
    mem.fill(0);
    let base = stack_top.wrapping_sub(mem.len() as u64);
    let mut sp = mem.len();

    fn push(mem: &mut [u8], sp: &mut usize, bytes: &[u8]) -> Result<usize, ElfError> {
        let n = bytes.len().checked_add(1).ok_or(ElfError::Stack)?;
        if *sp < n {
            return Err(ElfError::Stack);
        }
        *sp -= n;
        mem[*sp..*sp + bytes.len()].copy_from_slice(bytes);
        mem[*sp + bytes.len()] = 0;
        Ok(*sp)
    }

    let mut argv_off = [0usize; 8];
    if argv.len() > argv_off.len() {
        return Err(ElfError::Stack);
    }
    let mut i = 0;
    while i < argv.len() {
        argv_off[i] = push(mem, &mut sp, argv[i])?;
        i += 1;
    }
    let mut env_off = [0usize; 8];
    if envp.len() > env_off.len() {
        return Err(ElfError::Stack);
    }
    i = 0;
    while i < envp.len() {
        env_off[i] = push(mem, &mut sp, envp[i])?;
        i += 1;
    }
    if sp < 16 {
        return Err(ElfError::Stack);
    }
    sp -= 16;
    mem[sp..sp + 16].copy_from_slice(random);
    let random_off = sp;

    let naux = aux.len() + 2; // AT_RANDOM + AT_NULL
    let ptr_bytes = 8
        + 8 * (argv.len() + 1)
        + 8 * (envp.len() + 1)
        + 16 * naux;
    if sp < ptr_bytes {
        return Err(ElfError::Stack);
    }
    let mut rsp_off = sp - ptr_bytes;
    rsp_off &= !0xf;
    if rsp_off + ptr_bytes > sp {
        if rsp_off < 16 {
            return Err(ElfError::Stack);
        }
        rsp_off -= 16;
    }

    fn poke_u64(mem: &mut [u8], off: usize, v: u64) -> Result<(), ElfError> {
        let e = off.checked_add(8).ok_or(ElfError::Stack)?;
        if e > mem.len() {
            return Err(ElfError::Stack);
        }
        mem[off..e].copy_from_slice(&v.to_le_bytes());
        Ok(())
    }

    let mut o = rsp_off;
    poke_u64(mem, o, argv.len() as u64)?;
    o += 8;
    i = 0;
    while i < argv.len() {
        poke_u64(mem, o, base + argv_off[i] as u64)?;
        o += 8;
        i += 1;
    }
    poke_u64(mem, o, 0)?;
    o += 8;
    i = 0;
    while i < envp.len() {
        poke_u64(mem, o, base + env_off[i] as u64)?;
        o += 8;
        i += 1;
    }
    poke_u64(mem, o, 0)?;
    o += 8;
    i = 0;
    while i < aux.len() {
        poke_u64(mem, o, aux[i].tag)?;
        o += 8;
        poke_u64(mem, o, aux[i].val)?;
        o += 8;
        i += 1;
    }
    poke_u64(mem, o, AT_RANDOM)?;
    o += 8;
    poke_u64(mem, o, base + random_off as u64)?;
    o += 8;
    poke_u64(mem, o, AT_NULL)?;
    o += 8;
    poke_u64(mem, o, 0)?;
    let _ = o;
    Ok(base + rsp_off as u64)
}

#[cfg(test)]
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
    fn good_static_exec() {
        let code = [0x90u8, 0x90, 0xC3];
        let elf = build_elf(0x4000_0000, &code, &[]);
        let img = parse(&elf).unwrap();
        assert_eq!(img.entry, 0x4000_0000);
        assert_eq!(img.nload, 1);
        assert!(!img.stack_exec);
        assert!(img.tls.is_none());
        assert_eq!(img.loads()[0].filesz, 3);
        assert_eq!(img.file_bytes(img.loads()[0]).unwrap(), &code);
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
        let extra = [(
            PT_INTERP,
            PF_R,
            0u64,
            0u64,
            0u64,
            0u64,
            1u64,
        )];
        let elf = build_elf(0x4000_0000, &[0x90], &extra);
        assert_eq!(parse_err(&elf), ElfError::HasInterp);
    }

    #[test]
    fn gnu_stack_and_tls_and_bss() {
        let extra = [
            (
                PT_GNU_STACK,
                PF_R | PF_W,
                0u64,
                0u64,
                0,
                0,
                16u64,
            ),
            (
                PT_TLS,
                PF_R | PF_W,
                0x1000u64,
                0x4000_0000u64,
                1,
                8,
                8u64,
            ),
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
        let extra = [(PT_LOAD, PF_R, 0x1000u64, 0x4000_1000u64, 16u64, 4u64, 0x1000u64)];
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
    fn stack_argv_auxv() {
        let mut mem = [0u8; 512];
        let top = 0x0000_0000_8000_0000u64;
        let aux = [Auxv {
            tag: AT_PAGESZ,
            val: 4096,
        }, Auxv {
            tag: AT_ENTRY,
            val: 0x4000_0000,
        }];
        let rsp = build_initial_stack(
            top,
            &mut mem,
            &[b"/hello"],
            &[],
            &aux,
            &[0x11; 16],
        )
        .unwrap();
        assert_eq!(rsp & 0xf, 0);
        let base = top - mem.len() as u64;
        let off = (rsp - base) as usize;
        let argc = u64::from_le_bytes(mem[off..off + 8].try_into().unwrap());
        assert_eq!(argc, 1);
        let argv0 = u64::from_le_bytes(mem[off + 8..off + 16].try_into().unwrap());
        let s_off = (argv0 - base) as usize;
        assert_eq!(&mem[s_off..s_off + 6], b"/hello");
        assert_eq!(mem[s_off + 6], 0);
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
                | ElfError::Stack => {
                    assert!(!e.as_str().is_empty());
                }
            }
        }
    }
}
