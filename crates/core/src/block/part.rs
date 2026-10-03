//! MBR/GPT partition tables. ROADMAP §7.3.
//!
//! Children are offset-limited ranges on a parent. Protective MBR (0xEE)
//! is not a data partition. GPT validates header + entry CRC and falls
//! back to the backup header. Logical EBR walk is depth-bounded.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use crate::block::BlockError;

pub use crate::limits::MAX_PARTS;
pub const MAX_EBR_DEPTH: u32 = 128;
pub const MBR_SIG_OFF: usize = 510;
pub const MBR_PART_OFF: usize = 446;
pub const MBR_ENTRY_LEN: usize = 16;
pub const GPT_SIG: &[u8; 8] = b"EFI PART";
pub const GPT_HEADER_SIZE: u32 = 92;
pub const GPT_ENTRY_SIZE: u32 = 128;
pub const GPT_REVISION: u32 = 0x0001_0000;

/// EFI System Partition. On-disk mixed-endian GUID.
pub const GUID_EFI: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];
/// Linux filesystem.
pub const GUID_LINUX: [u8; 16] = [
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];
/// Linux swap.
pub const GUID_SWAP: [u8; 16] = [
    0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F,
];
/// Microsoft basic data.
pub const GUID_BASIC: [u8; 16] = [
    0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
];
/// Linux LVM.
pub const GUID_LVM: [u8; 16] = [
    0x79, 0xD3, 0xD6, 0xE6, 0x07, 0xF5, 0xC2, 0x44, 0xA2, 0x3C, 0x23, 0x8F, 0x2A, 0x3D, 0xF9, 0x28,
];
/// BIOS boot (GRUB).
pub const GUID_BIOS_BOOT: [u8; 16] = [
    0x48, 0x61, 0x68, 0x21, 0x49, 0x64, 0x6F, 0x6E, 0x74, 0x4E, 0x65, 0x65, 0x64, 0x45, 0x46, 0x49,
];
pub const GUID_UNUSED: [u8; 16] = [0u8; 16];

pub const MBR_LINUX: u8 = 0x83;
pub const MBR_FAT32_LBA: u8 = 0x0C;
pub const MBR_NTFS: u8 = 0x07;
pub const MBR_EFI: u8 = 0xEF;
pub const MBR_SWAP: u8 = 0x82;
pub const MBR_EXTENDED: u8 = 0x05;
pub const MBR_EXTENDED_LBA: u8 = 0x0F;
pub const MBR_LINUX_EXTENDED: u8 = 0x85;
pub const MBR_PROTECTIVE: u8 = 0xEE;

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartError {
    Truncated,
    BadCrc,
    Invalid,
    Empty,
    /// The parser's scratch could not be allocated (DESIGN §4.4).
    NoMemory,
}

/// A partition table that cannot be read is an I/O error; an empty slot, no device.
impl From<PartError> for crate::kerror::KError {
    fn from(e: PartError) -> Self {
        match e {
            PartError::Truncated | PartError::BadCrc | PartError::Invalid => Self::Io,
            PartError::Empty => Self::NoDev,
            PartError::NoMemory => Self::NoMem,
        }
    }
}

impl PartError {
    pub fn as_str(self) -> &'static str {
        match self {
            PartError::Truncated => "truncated",
            PartError::BadCrc => "bad crc",
            PartError::Invalid => "invalid",
            PartError::Empty => "empty",
            PartError::NoMemory => "no memory",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableOrigin {
    Mbr,
    Gpt { used_backup: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartKind {
    Mbr { sys: u8 },
    Gpt { type_guid: [u8; 16] },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    pub index: u8,
    pub start_lba: u64,
    pub nsectors: u64,
    pub kind: PartKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Table {
    pub origin: TableOrigin,
    pub n: usize,
    pub parts: [Part; MAX_PARTS],
}

impl Table {
    pub const fn empty(origin: TableOrigin) -> Self {
        Self {
            origin,
            n: 0,
            parts: [Part {
                index: 0,
                start_lba: 0,
                nsectors: 0,
                kind: PartKind::Mbr { sys: 0 },
            }; MAX_PARTS],
        }
    }

    pub fn get(self, i: usize) -> Option<Part> {
        if i < self.n {
            self.parts.get(i).copied()
        } else {
            None
        }
    }
}

/// Map a child LBA range onto the parent. Rejects overflow and past-end.
pub fn map_child_lba(start: u64, nsect: u64, lba: u64, count: u64) -> Result<u64, BlockError> {
    let end = lba.checked_add(count).ok_or(BlockError::Inval)?;
    if count == 0 || end > nsect {
        return Err(BlockError::Inval);
    }
    start.checked_add(lba).ok_or(BlockError::Inval)
}

pub fn is_extended(sys: u8) -> bool {
    matches!(sys, MBR_EXTENDED | MBR_EXTENDED_LBA | MBR_LINUX_EXTENDED)
}

pub fn gpt_type_name(guid: &[u8; 16]) -> &'static str {
    if guid == &GUID_UNUSED {
        "unused"
    } else if guid == &GUID_EFI {
        "efi"
    } else if guid == &GUID_LINUX {
        "linux"
    } else if guid == &GUID_SWAP {
        "swap"
    } else if guid == &GUID_BASIC {
        "basic"
    } else if guid == &GUID_LVM {
        "lvm"
    } else if guid == &GUID_BIOS_BOOT {
        "bios"
    } else {
        "other"
    }
}

pub fn mbr_type_name(sys: u8) -> &'static str {
    match sys {
        MBR_LINUX => "linux",
        MBR_FAT32_LBA => "fat",
        MBR_NTFS => "ntfs",
        MBR_EFI => "efi",
        MBR_SWAP => "swap",
        MBR_PROTECTIVE => "protective",
        s if is_extended(s) => "extended",
        _ => "other",
    }
}

/// IEEE 802.3 / GPT CRC-32.
pub fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// `b[o..o + N]`, or `None` past the end.
fn field<const N: usize>(b: &[u8], o: usize) -> Option<[u8; N]> {
    b.get(o..o.checked_add(N)?)?.try_into().ok()
}

fn r32(b: &[u8], o: usize) -> Option<u32> {
    field(b, o).map(u32::from_le_bytes)
}

fn r64(b: &[u8], o: usize) -> Option<u64> {
    field(b, o).map(u64::from_le_bytes)
}

fn r8(b: &[u8], o: usize) -> Option<u8> {
    b.get(o).copied()
}

/// Store `src` at `b[o..]`. The packers check their buffer's length first,
/// so a store past the end does not happen; it is skipped rather than
/// panicking.
fn put(b: &mut [u8], o: usize, src: &[u8]) {
    if let Some(dst) = o.checked_add(src.len()).and_then(|end| b.get_mut(o..end)) {
        dst.copy_from_slice(src);
    }
}

fn w32(b: &mut [u8], o: usize, v: u32) {
    put(b, o, &v.to_le_bytes());
}

fn w64(b: &mut [u8], o: usize, v: u64) {
    put(b, o, &v.to_le_bytes());
}

fn push_part(t: &mut Table, p: Part) -> bool {
    let Some(slot) = t.parts.get_mut(t.n) else {
        return false;
    };
    *slot = p;
    t.n = t.n.saturating_add(1);
    true
}

/// The number Linux gives the `k`th logical partition (from 0): 5 on,
/// after the four primary slots.
fn logical_index(k: usize) -> Option<u8> {
    u8::try_from(k).ok()?.checked_add(5)
}

/// Byte offset of MBR entry `i` (0 to 3).
fn mbr_entry_off(i: usize) -> Option<usize> {
    i.checked_mul(MBR_ENTRY_LEN)?.checked_add(MBR_PART_OFF)
}

/// `(sys, start, count)` of MBR entry `i`, or `None` past the sector.
fn mbr_entry(sector: &[u8], i: usize) -> Option<(u8, u64, u64)> {
    let o = mbr_entry_off(i)?;
    let sys = r8(sector, o.checked_add(4)?)?;
    let start = r32(sector, o.checked_add(8)?)? as u64;
    let count = r32(sector, o.checked_add(12)?)? as u64;
    Some((sys, start, count))
}

fn write_mbr_entry(sector: &mut [u8], i: usize, sys: u8, start: u32, count: u32) {
    let Some(o) = mbr_entry_off(i) else {
        return;
    };
    put(sector, o, &[0]);
    if let Some(at) = o.checked_add(4) {
        put(sector, at, &[sys]);
    }
    if let Some(at) = o.checked_add(8) {
        w32(sector, at, start);
    }
    if let Some(at) = o.checked_add(12) {
        w32(sector, at, count);
    }
}

/// Whether `sector` ends in the 0x55AA boot signature.
fn has_mbr_sig(sector: &[u8]) -> bool {
    field::<2>(sector, MBR_SIG_OFF) == Some([0x55, 0xAA])
}

pub fn pack_mbr(buf: &mut [u8], slots: &[(u8, u32, u32); 4]) {
    let Some(sector) = buf.get_mut(..512) else {
        return;
    };
    sector.fill(0);
    for (i, &(sys, start, count)) in slots.iter().enumerate() {
        if sys != 0 && count != 0 {
            write_mbr_entry(sector, i, sys, start, count);
        }
    }
    put(sector, MBR_SIG_OFF, &[0x55, 0xAA]);
}

pub fn pack_protective_mbr(buf: &mut [u8], nsectors: u64) {
    let count = u32::try_from(nsectors.saturating_sub(1)).unwrap_or(u32::MAX);
    pack_mbr(
        buf,
        &[(MBR_PROTECTIVE, 1, count), (0, 0, 0), (0, 0, 0), (0, 0, 0)],
    );
}

pub fn pack_ebr(
    buf: &mut [u8],
    sys: u8,
    first_rel: u32,
    first_count: u32,
    next_rel: u32,
    next_count: u32,
) {
    let next_sys = if next_rel == 0 { 0 } else { MBR_EXTENDED };
    pack_mbr(
        buf,
        &[
            (sys, first_rel, first_count),
            (next_sys, next_rel, next_count),
            (0, 0, 0),
            (0, 0, 0),
        ],
    );
}

#[derive(Clone, Copy)]
pub struct GptHeaderInfo {
    pub my_lba: u64,
    pub alt_lba: u64,
    pub first_usable: u64,
    pub last_usable: u64,
    pub disk_guid: [u8; 16],
    pub part_lba: u64,
    pub part_count: u32,
    pub part_size: u32,
    pub entries_crc: u32,
}

pub fn pack_gpt_header(buf: &mut [u8], h: &GptHeaderInfo) {
    let Some(sector) = buf.get_mut(..512) else {
        return;
    };
    sector.fill(0);
    put(sector, 0, GPT_SIG);
    w32(sector, 8, GPT_REVISION);
    w32(sector, 12, GPT_HEADER_SIZE);
    w32(sector, 16, 0);
    w64(sector, 24, h.my_lba);
    w64(sector, 32, h.alt_lba);
    w64(sector, 40, h.first_usable);
    w64(sector, 48, h.last_usable);
    put(sector, 56, &h.disk_guid);
    w64(sector, 72, h.part_lba);
    w32(sector, 80, h.part_count);
    w32(sector, 84, h.part_size);
    w32(sector, 88, h.entries_crc);
    let crc = crc32_ieee(sector.get(..GPT_HEADER_SIZE as usize).unwrap_or(&[]));
    w32(sector, 16, crc);
}

pub fn pack_gpt_entry(
    buf: &mut [u8],
    type_guid: &[u8; 16],
    uniq: &[u8; 16],
    first: u64,
    last: u64,
    name: &str,
) {
    let Some(e) = buf.get_mut(..GPT_ENTRY_SIZE as usize) else {
        return;
    };
    e.fill(0);
    put(e, 0, type_guid);
    put(e, 16, uniq);
    w64(e, 32, first);
    w64(e, 40, last);
    if let Some(dst) = e.get_mut(56..56 + 72) {
        utf16le_name(dst, name);
    }
}

fn utf16le_name(dst: &mut [u8], s: &str) {
    for (c, pair) in s.chars().zip(dst.as_chunks_mut::<2>().0) {
        let Ok(u) = u16::try_from(c as u32) else {
            break;
        };
        pair.copy_from_slice(&u.to_le_bytes());
    }
}

pub fn entries_crc(entries: &[u8]) -> u32 {
    crc32_ieee(entries)
}

fn header_ok(sec: &[u8], nsectors: u64) -> bool {
    header_fields_ok(sec, nsectors).unwrap_or(false)
}

/// `None` when a field lies past `sec`.
fn header_fields_ok(sec: &[u8], nsectors: u64) -> Option<bool> {
    if field::<8>(sec, 0)? != *GPT_SIG {
        return Some(false);
    }
    let size = r32(sec, 12)? as usize;
    if size < GPT_HEADER_SIZE as usize {
        return Some(false);
    }
    let stored = r32(sec, 16)?;
    let mut tmp = [0u8; 512];
    let dst = tmp.get_mut(..size)?;
    dst.copy_from_slice(sec.get(..size)?);
    w32(dst, 16, 0);
    if crc32_ieee(dst) != stored {
        return Some(false);
    }
    if r64(sec, 24)? >= nsectors {
        return Some(false);
    }
    let psz = r32(sec, 84)?;
    let nent = r32(sec, 80)?;
    Some(psz == GPT_ENTRY_SIZE && nent > 0 && nent <= 128)
}

fn load_entries<R: FnMut(u64, &mut [u8]) -> Result<(), BlockError>>(
    read: &mut R,
    part_lba: u64,
    nent: u32,
    nsectors: u64,
    sector_size: u32,
    sector_buf: &mut [u8],
    scratch: &mut [u8],
) -> Result<u32, PartError> {
    let want = (nent as usize).saturating_mul(GPT_ENTRY_SIZE as usize);
    if scratch.len() < want {
        return Err(PartError::Truncated);
    }
    let bs = sector_size as u64;
    if bs == 0 {
        return Err(PartError::Invalid);
    }
    let nsec = (want as u64).div_ceil(bs);
    if part_lba
        .checked_add(nsec)
        .map(|e| e > nsectors)
        .unwrap_or(true)
    {
        return Err(PartError::Truncated);
    }
    let mut lba = part_lba;
    for chunk in scratch
        .get_mut(..want)
        .ok_or(PartError::Truncated)?
        .chunks_mut(sector_size as usize)
    {
        read(lba, sector_buf).map_err(|_| PartError::Truncated)?;
        let src = sector_buf.get(..chunk.len()).ok_or(PartError::Truncated)?;
        chunk.copy_from_slice(src);
        lba = lba.saturating_add(1);
    }
    Ok(nent)
}

fn parse_gpt_entries(t: &mut Table, entries: &[u8], nent: u32, nsectors: u64) {
    let nent = nent as usize;
    // An entry's number is its place in the array plus one, used or not,
    // as Linux names a GPT partition.
    for (i, e) in entries
        .as_chunks::<{ GPT_ENTRY_SIZE as usize }>()
        .0
        .iter()
        .take(nent)
        .enumerate()
    {
        let Some(index) = i.checked_add(1).and_then(|n| u8::try_from(n).ok()) else {
            break;
        };
        let (Some(guid), Some(first), Some(last)) = (field::<16>(e, 0), r64(e, 32), r64(e, 40))
        else {
            break;
        };
        if guid == GUID_UNUSED || last < first {
            continue;
        }
        let n = last.saturating_sub(first).saturating_add(1);
        let end = first.saturating_add(n);
        if n == 0 || end > nsectors {
            continue;
        }
        let p = Part {
            index,
            start_lba: first,
            nsectors: n,
            kind: PartKind::Gpt { type_guid: guid },
        };
        if !push_part(t, p) {
            break;
        }
    }
}

fn try_gpt<R: FnMut(u64, &mut [u8]) -> Result<(), BlockError>>(
    nsectors: u64,
    sector_size: u32,
    read: &mut R,
    sector_buf: &mut [u8],
    scratch: &mut [u8],
    header_lba: u64,
    used_backup: bool,
) -> Result<Table, PartError> {
    if header_lba >= nsectors {
        return Err(PartError::Truncated);
    }
    read(header_lba, sector_buf).map_err(|_| PartError::Truncated)?;
    if !header_ok(sector_buf, nsectors) {
        return Err(PartError::BadCrc);
    }
    let part_lba = r64(sector_buf, 72).ok_or(PartError::Truncated)?;
    let nent = r32(sector_buf, 80).ok_or(PartError::Truncated)?;
    let stored_ecrc = r32(sector_buf, 88).ok_or(PartError::Truncated)?;
    let got = load_entries(
        read,
        part_lba,
        nent,
        nsectors,
        sector_size,
        sector_buf,
        scratch,
    )?;
    let want = (got as usize).saturating_mul(GPT_ENTRY_SIZE as usize);
    let entries = scratch.get(..want).ok_or(PartError::Truncated)?;
    if crc32_ieee(entries) != stored_ecrc {
        return Err(PartError::BadCrc);
    }
    let mut t = Table::empty(TableOrigin::Gpt { used_backup });
    parse_gpt_entries(&mut t, entries, got, nsectors);
    Ok(t)
}

/// Walk the EBR chain of the extended partition at `ext_start`. `logical`
/// counts the disk's logical partitions so far, across every extended
/// entry of its MBR: Linux numbers them from 5 for the whole disk, so a
/// second extended entry's logicals go on from the first's.
fn parse_logical<R: FnMut(u64, &mut [u8]) -> Result<(), BlockError>>(
    t: &mut Table,
    nsectors: u64,
    read: &mut R,
    sector_buf: &mut [u8],
    ext_start: u64,
    ext_count: u64,
    logical: &mut usize,
) {
    let ext_end = ext_start.saturating_add(ext_count);
    let mut ebr = ext_start;
    for _ in 0..MAX_EBR_DEPTH {
        if ebr >= nsectors || ebr >= ext_end {
            return;
        }
        if read(ebr, sector_buf).is_err() || !has_mbr_sig(sector_buf) {
            return;
        }
        let Some((sys, rel, count)) = mbr_entry(sector_buf, 0) else {
            return;
        };
        if sys != 0 && count != 0 && !is_extended(sys) && sys != MBR_PROTECTIVE {
            let Some(start) = ebr.checked_add(rel) else {
                return;
            };
            let Some(end) = start.checked_add(count) else {
                return;
            };
            if start >= nsectors || end > nsectors || start < ext_start || end > ext_end {
                return;
            }
            let Some(index) = logical_index(*logical) else {
                return;
            };
            *logical = logical.saturating_add(1);
            let p = Part {
                index,
                start_lba: start,
                nsectors: count,
                kind: PartKind::Mbr { sys },
            };
            if !push_part(t, p) {
                return;
            }
        }
        let Some((nsys, nrel, _)) = mbr_entry(sector_buf, 1) else {
            return;
        };
        if nrel == 0 || !is_extended(nsys) {
            return;
        }
        let Some(next) = ext_start.checked_add(nrel) else {
            return;
        };
        if next >= nsectors || next == ebr || next < ext_start || next >= ext_end {
            return;
        }
        ebr = next;
    }
}

fn parse_mbr<R: FnMut(u64, &mut [u8]) -> Result<(), BlockError>>(
    nsectors: u64,
    read: &mut R,
    sector_buf: &mut [u8],
) -> Result<Table, PartError> {
    let mut t = Table::empty(TableOrigin::Mbr);
    // `parse_logical` reads EBRs into `sector_buf`, so the four primary
    // entries are copied out before the first of them is walked (F117).
    let mut prim = [(0u8, 0u64, 0u64); 4];
    for (i, e) in prim.iter_mut().enumerate() {
        *e = mbr_entry(sector_buf, i).ok_or(PartError::Truncated)?;
    }
    // A primary partition is numbered by its slot, 1 to 4, and the
    // logical ones from 5, as Linux numbers them: one count for the disk.
    let mut logical = 0usize;
    for (slot, (sys, start, count)) in (1u8..).zip(prim) {
        if sys == 0 || count == 0 || sys == MBR_PROTECTIVE {
            continue;
        }
        if is_extended(sys) {
            parse_logical(
                &mut t,
                nsectors,
                read,
                sector_buf,
                start,
                count,
                &mut logical,
            );
            continue;
        }
        let Some(end) = start.checked_add(count) else {
            continue;
        };
        if start >= nsectors || end > nsectors {
            continue;
        }
        let p = Part {
            index: slot,
            start_lba: start,
            nsectors: count,
            kind: PartKind::Mbr { sys },
        };
        if !push_part(&mut t, p) {
            break;
        }
    }
    Ok(t)
}

/// Parse MBR and/or GPT. `sector_buf` ≥ sector size. `scratch` holds the
/// GPT entry array when present (typically 128 × 128 bytes).
pub fn parse<R: FnMut(u64, &mut [u8]) -> Result<(), BlockError>>(
    nsectors: u64,
    sector_size: u32,
    mut read: R,
    sector_buf: &mut [u8],
    scratch: &mut [u8],
) -> Result<Table, PartError> {
    if sector_size == 0 || sector_buf.len() < sector_size as usize {
        return Err(PartError::Invalid);
    }
    if nsectors == 0 {
        return Err(PartError::Empty);
    }
    read(0, sector_buf).map_err(|_| PartError::Truncated)?;
    let mbr_sig = has_mbr_sig(sector_buf);
    let protective = mbr_sig
        && (0..4)
            .any(|i| mbr_entry(sector_buf, i).is_some_and(|(sys, _, _)| sys == MBR_PROTECTIVE));
    // A GPT counts only behind a protective MBR, as Linux reads one
    // without `gpt` on its command line: GPT headers left on a disk that
    // was later given a plain MBR name partitions that no longer exist.
    if !protective {
        if !mbr_sig {
            return Err(PartError::Empty);
        }
        return parse_mbr(nsectors, &mut read, sector_buf);
    }
    if nsectors < 2 {
        return Err(PartError::Truncated);
    }
    let primary = try_gpt(
        nsectors,
        sector_size,
        &mut read,
        sector_buf,
        scratch,
        1,
        false,
    );
    if let Ok(t) = primary {
        return Ok(t);
    }
    try_gpt(
        nsectors,
        sector_size,
        &mut read,
        sector_buf,
        scratch,
        nsectors.saturating_sub(1),
        true,
    )
    .map_err(|_| PartError::BadCrc)
}

/// Host helper: parse a whole-disk image.
pub fn parse_image(disk: &[u8], sector_size: u32) -> Result<Table, PartError> {
    if sector_size == 0 {
        return Err(PartError::Invalid);
    }
    let nsectors = (disk.len() as u64)
        .checked_div(sector_size as u64)
        .ok_or(PartError::Invalid)?;
    let mut buf = [0u8; crate::limits::MAX_BLOCK_SIZE as usize];
    let sec = buf
        .get_mut(..sector_size as usize)
        .ok_or(PartError::Invalid)?;
    let mut scratch = [0u8; 128 * 128];
    parse(
        nsectors,
        sector_size,
        |lba, buf| {
            let bs = sector_size as usize;
            let off = usize::try_from(lba)
                .ok()
                .and_then(|l| l.checked_mul(bs))
                .ok_or(BlockError::Inval)?;
            let n = buf.len().min(bs);
            let src = off
                .checked_add(n)
                .and_then(|end| disk.get(off..end))
                .filter(|_| off.checked_add(bs).is_some_and(|e| e <= disk.len()))
                .ok_or(BlockError::Inval)?;
            buf.get_mut(..n)
                .ok_or(BlockError::Inval)?
                .copy_from_slice(src);
            Ok(())
        },
        sec,
        &mut scratch,
    )
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "host tests: a failure ends the test, not the kernel"
)]
mod tests {
    use super::*;

    fn disk(n: usize) -> Vec<u8> {
        vec![0u8; n]
    }

    #[test]
    fn crc32_known() {
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_ieee(b""), 0);
    }

    #[test]
    fn child_lba_clip() {
        assert_eq!(map_child_lba(100, 50, 0, 10).unwrap(), 100);
        assert_eq!(map_child_lba(100, 50, 40, 10).unwrap(), 140);
        assert_eq!(map_child_lba(100, 50, 41, 10), Err(BlockError::Inval));
        assert_eq!(map_child_lba(100, 50, 50, 1), Err(BlockError::Inval));
        assert_eq!(
            map_child_lba(u64::MAX - 5, 10, 6, 1),
            Err(BlockError::Inval)
        );
        assert_eq!(map_child_lba(100, 50, 0, 0), Err(BlockError::Inval));
    }

    #[test]
    fn guid_names() {
        assert_eq!(gpt_type_name(&GUID_EFI), "efi");
        assert_eq!(gpt_type_name(&GUID_LINUX), "linux");
        assert_eq!(gpt_type_name(&GUID_SWAP), "swap");
        assert_eq!(gpt_type_name(&GUID_BASIC), "basic");
        assert_eq!(gpt_type_name(&GUID_LVM), "lvm");
        assert_eq!(gpt_type_name(&GUID_BIOS_BOOT), "bios");
        assert_eq!(gpt_type_name(&GUID_UNUSED), "unused");
        assert_eq!(mbr_type_name(MBR_LINUX), "linux");
        assert_eq!(mbr_type_name(MBR_PROTECTIVE), "protective");
        assert_eq!(mbr_type_name(MBR_EXTENDED_LBA), "extended");
    }

    fn put(disk: &mut [u8], lba: u64, sec: &[u8]) {
        let o = lba as usize * 512;
        disk[o..o + 512].copy_from_slice(sec);
    }

    #[test]
    fn mbr_primaries() {
        let mut d = disk(64 * 512);
        let mut mbr = [0u8; 512];
        pack_mbr(
            &mut mbr,
            &[
                (MBR_LINUX, 8, 16),
                (MBR_FAT32_LBA, 32, 16),
                (0, 0, 0),
                (0, 0, 0),
            ],
        );
        put(&mut d, 0, &mbr);
        let t = parse_image(&d, 512).unwrap();
        assert_eq!(t.origin, TableOrigin::Mbr);
        assert_eq!(t.n, 2);
        assert_eq!(t.parts[0].start_lba, 8);
        assert_eq!(t.parts[0].nsectors, 16);
        assert_eq!(t.parts[0].index, 1);
        assert_eq!(t.parts[1].start_lba, 32);
        assert_eq!(
            mbr_type_name(match t.parts[0].kind {
                PartKind::Mbr { sys } => sys,
                PartKind::Gpt { .. } => panic!("gpt"),
            }),
            "linux"
        );
    }

    #[test]
    fn mbr_extended_logical() {
        let mut d = disk(256 * 512);
        let mut mbr = [0u8; 512];
        pack_mbr(
            &mut mbr,
            &[
                (MBR_LINUX, 8, 16),
                (MBR_EXTENDED, 40, 80),
                (0, 0, 0),
                (0, 0, 0),
            ],
        );
        put(&mut d, 0, &mbr);
        let mut e1 = [0u8; 512];
        pack_ebr(&mut e1, MBR_LINUX, 1, 16, 32, 24);
        put(&mut d, 40, &e1);
        let mut e2 = [0u8; 512];
        pack_ebr(&mut e2, MBR_LINUX, 1, 16, 0, 0);
        put(&mut d, 72, &e2);
        let t = parse_image(&d, 512).unwrap();
        assert_eq!(t.n, 3);
        assert_eq!(t.parts[0].start_lba, 8);
        assert_eq!(t.parts[1].start_lba, 41);
        assert_eq!(t.parts[1].nsectors, 16);
        assert_eq!(t.parts[2].start_lba, 73);
        // The primary keeps its slot's number, and the logicals are 5 on.
        let idx: Vec<u8> = t.parts[..t.n].iter().map(|p| p.index).collect();
        assert_eq!(idx, [1, 5, 6]);
    }

    /// Two extended entries: the second's logicals go on from the
    /// first's, as Linux numbers them for the disk, where each chain once
    /// began again at 5 and the second `p5` was refused as a duplicate.
    #[test]
    fn logicals_of_two_extended_entries_number_on() {
        let mut d = disk(256 * 512);
        let mut mbr = [0u8; 512];
        pack_mbr(
            &mut mbr,
            &[
                (MBR_EXTENDED, 10, 40),
                (MBR_EXTENDED, 100, 40),
                (0, 0, 0),
                (0, 0, 0),
            ],
        );
        put(&mut d, 0, &mbr);
        let mut e = [0u8; 512];
        pack_ebr(&mut e, MBR_LINUX, 1, 16, 0, 0);
        put(&mut d, 10, &e);
        put(&mut d, 100, &e);
        let t = parse_image(&d, 512).unwrap();
        let got: Vec<(u8, u64)> = t.parts[..t.n]
            .iter()
            .map(|p| (p.index, p.start_lba))
            .collect();
        assert_eq!(got, [(5, 11), (6, 101)]);
    }

    #[test]
    fn mbr_corrupt_next_lba_fails_safe() {
        let mut d = disk(128 * 512);
        let mut mbr = [0u8; 512];
        pack_mbr(
            &mut mbr,
            &[(MBR_EXTENDED, 8, 40), (0, 0, 0), (0, 0, 0), (0, 0, 0)],
        );
        put(&mut d, 0, &mbr);
        let mut e1 = [0u8; 512];
        pack_ebr(&mut e1, MBR_LINUX, 1, 8, 0, 0);
        // next-LBA past the disk
        write_mbr_entry(&mut e1, 1, MBR_EXTENDED, 10_000, 8);
        put(&mut d, 8, &e1);
        let t = parse_image(&d, 512).unwrap();
        assert_eq!(t.n, 1);
        assert_eq!(t.parts[0].start_lba, 9);
    }

    /// A primary entry after the extended one is read from the MBR, not
    /// from the last EBR the logical walk left in the sector buffer
    /// (F117). Two layouts, so "slot 1, slot 2" holds counted from 0 or 1.
    #[test]
    fn mbr_primary_after_extended() {
        let image = |slots: &[(u8, u32, u32); 4]| {
            let mut d = disk(128 * 512);
            let mut mbr = [0u8; 512];
            pack_mbr(&mut mbr, slots);
            put(&mut d, 0, &mbr);
            // EBRs at 8 and 24, one 8-sector logical each; the last EBR's
            // entries 1 to 3 are empty.
            let mut e1 = [0u8; 512];
            pack_ebr(&mut e1, MBR_LINUX, 1, 8, 16, 8);
            put(&mut d, 8, &e1);
            let mut e2 = [0u8; 512];
            pack_ebr(&mut e2, MBR_LINUX, 1, 8, 0, 0);
            put(&mut d, 24, &e2);
            d
        };
        let got = |d: &[u8]| {
            let t = parse_image(d, 512).unwrap();
            let mut v: Vec<(u64, u64, PartKind)> = (0..t.n)
                .map(|i| (t.parts[i].start_lba, t.parts[i].nsectors, t.parts[i].kind))
                .collect();
            v.sort_by_key(|&(s, n, _)| (s, n));
            v
        };
        let linux = PartKind::Mbr { sys: MBR_LINUX };
        // (a) extended in slot 0, the primary in slot 1.
        let a = image(&[
            (MBR_EXTENDED, 8, 40),
            (MBR_LINUX, 64, 16),
            (0, 0, 0),
            (0, 0, 0),
        ]);
        assert_eq!(
            got(&a),
            vec![(9, 8, linux), (25, 8, linux), (64, 16, linux)]
        );
        // (b) a primary, the extended in slot 1, the primary in slot 2.
        let b = image(&[
            (MBR_LINUX, 4, 4),
            (MBR_EXTENDED, 8, 40),
            (MBR_LINUX, 64, 16),
            (0, 0, 0),
        ]);
        assert_eq!(
            got(&b),
            vec![
                (4, 4, linux),
                (9, 8, linux),
                (25, 8, linux),
                (64, 16, linux)
            ]
        );
    }

    /// Crafted GPT headers (ROADMAP §10.1): each field out of range gives
    /// an error or drops the entry, never a panic.
    #[test]
    fn parse_rejects_crafted_gpt_header() {
        const NSECT: u64 = 64;
        let base = GptHeaderInfo {
            my_lba: 1,
            alt_lba: NSECT - 1,
            first_usable: 34,
            last_usable: NSECT - 34,
            disk_guid: [7; 16],
            part_lba: 2,
            part_count: 128,
            part_size: GPT_ENTRY_SIZE,
            entries_crc: entries_crc(&[0u8; 128 * 128]),
        };
        let image = |h: GptHeaderInfo, entries: &[u8]| {
            let mut d = disk(NSECT as usize * 512);
            let mut mbr = [0u8; 512];
            pack_protective_mbr(&mut mbr, NSECT);
            put(&mut d, 0, &mbr);
            let mut hdr = [0u8; 512];
            pack_gpt_header(&mut hdr, &h);
            put(&mut d, 1, &hdr);
            d[2 * 512..2 * 512 + entries.len()].copy_from_slice(entries);
            d
        };
        let bad = [
            GptHeaderInfo {
                part_count: 0,
                ..base
            },
            GptHeaderInfo {
                part_count: u32::MAX,
                ..base
            },
            GptHeaderInfo {
                part_size: 0,
                ..base
            },
            GptHeaderInfo {
                part_size: u32::MAX,
                ..base
            },
            GptHeaderInfo {
                part_lba: u64::MAX - 1,
                ..base
            },
            GptHeaderInfo {
                part_lba: NSECT - 2,
                ..base
            },
            GptHeaderInfo {
                my_lba: NSECT,
                ..base
            },
            GptHeaderInfo {
                my_lba: u64::MAX,
                ..base
            },
        ];
        for h in bad {
            assert!(matches!(
                parse_image(&image(h, &[]), 512),
                Err(PartError::BadCrc | PartError::Truncated)
            ));
        }
        // An entry whose first and last LBA lie past the capacity, or
        // whose `last + 1` overflows, is dropped.
        let mut entries = vec![0u8; 128 * 128];
        pack_gpt_entry(
            &mut entries,
            &GUID_LINUX,
            &[1; 16],
            NSECT,
            NSECT + 8,
            "past",
        );
        pack_gpt_entry(
            &mut entries[128..],
            &GUID_LINUX,
            &[2; 16],
            40,
            u64::MAX,
            "wrap",
        );
        let h = GptHeaderInfo {
            entries_crc: entries_crc(&entries),
            ..base
        };
        let t = parse_image(&image(h, &entries), 512).unwrap();
        assert_eq!(t.origin, TableOrigin::Gpt { used_backup: false });
        assert_eq!(t.n, 0);
    }

    /// Crafted EBR chains (ROADMAP §10.1): offsets at the top of the u32
    /// range and a logical start that overflows the extended partition give
    /// no partition and no panic.
    #[test]
    fn parse_rejects_crafted_ebr_chain() {
        let mut d = disk(64 * 512);
        let mut mbr = [0u8; 512];
        pack_mbr(
            &mut mbr,
            &[
                (MBR_EXTENDED, u32::MAX, u32::MAX),
                (MBR_EXTENDED_LBA, 8, 16),
                (0, 0, 0),
                (0, 0, 0),
            ],
        );
        put(&mut d, 0, &mbr);
        let mut e1 = [0u8; 512];
        pack_ebr(&mut e1, MBR_LINUX, u32::MAX, u32::MAX, u32::MAX, u32::MAX);
        put(&mut d, 8, &e1);
        let t = parse_image(&d, 512).unwrap();
        assert_eq!(t.n, 0);
    }

    #[test]
    fn mbr_ebr_cycle_bounded() {
        let mut d = disk(64 * 512);
        let mut mbr = [0u8; 512];
        pack_mbr(
            &mut mbr,
            &[(MBR_EXTENDED, 8, 32), (0, 0, 0), (0, 0, 0), (0, 0, 0)],
        );
        put(&mut d, 0, &mbr);
        let mut e1 = [0u8; 512];
        pack_ebr(&mut e1, MBR_LINUX, 1, 4, 0, 0);
        write_mbr_entry(&mut e1, 1, MBR_EXTENDED, 0, 8); // next = ext_start + 0 = 8 (self)
        put(&mut d, 8, &e1);
        let t = parse_image(&d, 512).unwrap();
        assert_eq!(t.n, 1);
    }

    fn gpt_disk(nsect: u64, parts: &[([u8; 16], u64, u64, &str)]) -> Vec<u8> {
        let mut d = disk(nsect as usize * 512);
        let nent = 128u32;
        let esz = GPT_ENTRY_SIZE;
        let elen = nent as usize * esz as usize;
        let mut entries = vec![0u8; elen];
        let mut i = 0usize;
        while i < parts.len() {
            let (guid, first, last, name) = parts[i];
            let mut uniq = [0u8; 16];
            uniq[0] = (i as u8) + 1;
            pack_gpt_entry(
                &mut entries[i * esz as usize..],
                &guid,
                &uniq,
                first,
                last,
                name,
            );
            i += 1;
        }
        let ecrc = entries_crc(&entries);
        let mut guid = [0u8; 16];
        guid[0] = 0x11;
        let first_usable = 34u64;
        let last_usable = nsect - 34;
        let mut ph = [0u8; 512];
        pack_gpt_header(
            &mut ph,
            &GptHeaderInfo {
                my_lba: 1,
                alt_lba: nsect - 1,
                first_usable,
                last_usable,
                disk_guid: guid,
                part_lba: 2,
                part_count: nent,
                part_size: esz,
                entries_crc: ecrc,
            },
        );
        let mut bh = [0u8; 512];
        pack_gpt_header(
            &mut bh,
            &GptHeaderInfo {
                my_lba: nsect - 1,
                alt_lba: 1,
                first_usable,
                last_usable,
                disk_guid: guid,
                part_lba: nsect - 33,
                part_count: nent,
                part_size: esz,
                entries_crc: ecrc,
            },
        );
        let mut pmbr = [0u8; 512];
        pack_protective_mbr(&mut pmbr, nsect);
        put(&mut d, 0, &pmbr);
        put(&mut d, 1, &ph);
        let mut s = 0usize;
        while s < 32 {
            put(&mut d, 2 + s as u64, &entries[s * 512..s * 512 + 512]);
            s += 1;
        }
        s = 0;
        while s < 32 {
            put(
                &mut d,
                nsect - 33 + s as u64,
                &entries[s * 512..s * 512 + 512],
            );
            s += 1;
        }
        put(&mut d, nsect - 1, &bh);
        d
    }

    #[test]
    fn gpt_roundtrip_and_types() {
        let d = gpt_disk(
            1024,
            &[(GUID_EFI, 34, 97, "EFI"), (GUID_LINUX, 98, 500, "Linux")],
        );
        let t = parse_image(&d, 512).unwrap();
        assert_eq!(t.origin, TableOrigin::Gpt { used_backup: false });
        assert_eq!(t.n, 2);
        assert_eq!(t.parts[0].start_lba, 34);
        assert_eq!(t.parts[0].nsectors, 64);
        match t.parts[0].kind {
            PartKind::Gpt { type_guid } => assert_eq!(gpt_type_name(&type_guid), "efi"),
            PartKind::Mbr { .. } => panic!("mbr"),
        }
        match t.parts[1].kind {
            PartKind::Gpt { type_guid } => assert_eq!(gpt_type_name(&type_guid), "linux"),
            PartKind::Mbr { .. } => panic!("mbr"),
        }
        // protective 0xEE is not a child
        let mut saw_ee = false;
        let mut i = 0usize;
        while i < t.n {
            if let PartKind::Mbr { sys } = t.parts[i].kind
                && sys == MBR_PROTECTIVE
            {
                saw_ee = true;
            }
            i += 1;
        }
        assert!(!saw_ee);
    }

    /// GPT headers left on a disk that was later given a plain MBR, with
    /// no protective entry, name partitions that no longer exist: the MBR
    /// is the table, as Linux reads it; with no MBR signature at all there
    /// is none.
    #[test]
    fn gpt_without_protective_mbr_is_ignored() {
        let mut d = gpt_disk(1024, &[(GUID_LINUX, 34, 200, "L")]);
        let mut mbr = [0u8; 512];
        pack_mbr(
            &mut mbr,
            &[(MBR_LINUX, 300, 100), (0, 0, 0), (0, 0, 0), (0, 0, 0)],
        );
        put(&mut d, 0, &mbr);
        let t = parse_image(&d, 512).unwrap();
        assert_eq!(t.origin, TableOrigin::Mbr);
        assert_eq!((t.n, t.parts[0].start_lba), (1, 300));
        put(&mut d, 0, &[0u8; 512]);
        assert_eq!(parse_image(&d, 512).unwrap_err(), PartError::Empty);
    }

    /// A partition keeps the number its place on disk gives it, as Linux
    /// names it: a GPT entry's place in the array plus one, used or not,
    /// and a primary MBR entry's slot, so a gap shifts no later name.
    #[test]
    fn partitions_keep_their_on_disk_numbers() {
        let d = gpt_disk(1024, &[(GUID_UNUSED, 0, 0, ""), (GUID_LINUX, 34, 200, "L")]);
        let t = parse_image(&d, 512).unwrap();
        assert_eq!((t.n, t.parts[0].index), (1, 2));
        let mut d = disk(64 * 512);
        let mut mbr = [0u8; 512];
        pack_mbr(
            &mut mbr,
            &[(0, 0, 0), (0, 0, 0), (MBR_LINUX, 8, 16), (0, 0, 0)],
        );
        put(&mut d, 0, &mbr);
        let t = parse_image(&d, 512).unwrap();
        assert_eq!((t.n, t.parts[0].index), (1, 3));
    }

    #[test]
    fn gpt_bad_primary_crc_uses_backup() {
        let mut d = gpt_disk(1024, &[(GUID_LINUX, 34, 200, "L")]);
        d[512 + 16] ^= 0xFF;
        let t = parse_image(&d, 512).unwrap();
        assert_eq!(t.origin, TableOrigin::Gpt { used_backup: true });
        assert_eq!(t.n, 1);
        assert_eq!(t.parts[0].nsectors, 167);
    }

    #[test]
    fn gpt_bad_entry_crc_uses_backup() {
        let mut d = gpt_disk(1024, &[(GUID_LINUX, 34, 80, "L")]);
        // smash primary entry array, leave backup
        d[2 * 512] ^= 0xA5;
        let t = parse_image(&d, 512).unwrap();
        assert_eq!(t.origin, TableOrigin::Gpt { used_backup: true });
        assert_eq!(t.n, 1);
    }

    #[test]
    fn gpt_both_headers_bad() {
        let mut d = gpt_disk(1024, &[(GUID_LINUX, 34, 80, "L")]);
        d[512 + 16] ^= 0xFF;
        let last = d.len() - 512;
        d[last + 16] ^= 0xFF;
        assert_eq!(parse_image(&d, 512), Err(PartError::BadCrc));
    }

    #[test]
    fn truncated_table() {
        let d = vec![0u8; 200];
        assert!(matches!(
            parse_image(&d, 512),
            Err(PartError::Empty) | Err(PartError::Truncated) | Err(PartError::Invalid)
        ));
        let mut mbr = disk(512);
        pack_mbr(
            &mut mbr,
            &[(MBR_LINUX, 8, 16), (0, 0, 0), (0, 0, 0), (0, 0, 0)],
        );
        // table claims partitions past the truncated image
        assert_eq!(parse_image(&mbr, 512).unwrap().n, 0);
    }

    #[test]
    fn protective_mbr_alone_is_not_the_disk() {
        let mut d = disk(64 * 512);
        let mut mbr = [0u8; 512];
        pack_protective_mbr(&mut mbr, 64);
        put(&mut d, 0, &mbr);
        // no GPT headers → error, not a 0xEE child
        assert_eq!(parse_image(&d, 512), Err(PartError::BadCrc));
    }

    #[test]
    fn empty_is_empty() {
        let d = disk(32 * 512);
        assert_eq!(parse_image(&d, 512), Err(PartError::Empty));
    }

    #[test]
    fn error_strings() {
        assert_eq!(PartError::BadCrc.as_str(), "bad crc");
        assert_eq!(PartError::Truncated.as_str(), "truncated");
        assert_eq!(PartError::NoMemory.as_str(), "no memory");
    }

    #[test]
    fn fixed_tables_match_limits() {
        assert_eq!(
            Table::empty(TableOrigin::Mbr).parts.len(),
            crate::limits::MAX_PARTS
        );
    }

    /// A 4 KiB-sector disk's tables count in 4 KiB blocks: its GPT header
    /// is in block 1, at byte 4096, and its entries and partitions are in
    /// blocks too. A block larger than a page is refused.
    #[test]
    fn tables_on_4k_blocks() {
        const BS: usize = 4096;
        const N: u64 = 64;
        let put4k = |d: &mut [u8], lba: u64, b: &[u8]| {
            let o = lba as usize * BS;
            d[o..o + b.len()].copy_from_slice(b);
        };
        // MBR: a primary in slot 1 and an extended in slot 2 with one logical.
        let mut d = disk(N as usize * BS);
        let mut mbr = [0u8; 512];
        pack_mbr(
            &mut mbr,
            &[
                (MBR_LINUX, 4, 8),
                (MBR_EXTENDED, 20, 16),
                (0, 0, 0),
                (0, 0, 0),
            ],
        );
        put4k(&mut d, 0, &mbr);
        let mut e = [0u8; 512];
        pack_ebr(&mut e, MBR_LINUX, 1, 8, 0, 0);
        put4k(&mut d, 20, &e);
        let t = parse_image(&d, BS as u32).unwrap();
        let got: Vec<(u8, u64, u64)> = t.parts[..t.n]
            .iter()
            .map(|p| (p.index, p.start_lba, p.nsectors))
            .collect();
        assert_eq!(got, [(1, 4, 8), (5, 21, 8)]);

        // GPT: 128 entries fill four blocks from block 2.
        let mut d = disk(N as usize * BS);
        let mut entries = vec![0u8; 128 * GPT_ENTRY_SIZE as usize];
        pack_gpt_entry(&mut entries[..], &GUID_LINUX, &[1; 16], 6, 13, "a");
        pack_gpt_entry(
            &mut entries[2 * GPT_ENTRY_SIZE as usize..],
            &GUID_LINUX,
            &[3; 16],
            20,
            27,
            "c",
        );
        let ecrc = entries_crc(&entries);
        let last = N - 1;
        let back = last - 4;
        let hdr = |my, alt, part_lba| GptHeaderInfo {
            my_lba: my,
            alt_lba: alt,
            first_usable: 6,
            last_usable: back - 1,
            disk_guid: [9; 16],
            part_lba,
            part_count: 128,
            part_size: GPT_ENTRY_SIZE,
            entries_crc: ecrc,
        };
        let mut pmbr = [0u8; 512];
        pack_protective_mbr(&mut pmbr, N);
        put4k(&mut d, 0, &pmbr);
        let mut h = [0u8; 512];
        pack_gpt_header(&mut h, &hdr(1, last, 2));
        put4k(&mut d, 1, &h);
        for k in 0..4u64 {
            let o = k as usize * BS;
            put4k(&mut d, 2 + k, &entries[o..o + BS]);
            put4k(&mut d, back + k, &entries[o..o + BS]);
        }
        let mut b = [0u8; 512];
        pack_gpt_header(&mut b, &hdr(last, 1, back));
        put4k(&mut d, last, &b);
        let t = parse_image(&d, BS as u32).unwrap();
        let got: Vec<(u8, u64, u64)> = t.parts[..t.n]
            .iter()
            .map(|p| (p.index, p.start_lba, p.nsectors))
            .collect();
        assert_eq!(got, [(1, 6, 8), (3, 20, 8)]);
        // Read as 512-byte sectors, the header is not at LBA 1.
        assert_ne!(parse_image(&d, 512).map(|t| t.n), Ok(2));
        assert_eq!(
            parse_image(&disk(N as usize * 8192), 8192),
            Err(PartError::Invalid)
        );
    }
}
