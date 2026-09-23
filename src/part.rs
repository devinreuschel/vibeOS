//! MBR/GPT partition tables. ROADMAP §7.3.
//!
//! Children are offset-limited ranges on a parent. Protective MBR (0xEE)
//! is not a data partition. GPT validates header + entry CRC and falls
//! back to the backup header. Logical EBR walk is depth-bounded.

use crate::block::BlockError;

pub const MAX_PARTS: usize = 16;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartError {
    Truncated,
    BadCrc,
    Invalid,
    Empty,
}

impl PartError {
    pub fn as_str(self) -> &'static str {
        match self {
            PartError::Truncated => "truncated",
            PartError::BadCrc => "bad crc",
            PartError::Invalid => "invalid",
            PartError::Empty => "empty",
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
            Some(self.parts[i])
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
    let mut i = 0usize;
    while i < data.len() {
        crc ^= data[i] as u32;
        let mut b = 0u8;
        while b < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
            b += 1;
        }
        i += 1;
    }
    !crc
}

fn r32(b: &[u8], o: usize) -> u32 {
    let mut x = [0u8; 4];
    x.copy_from_slice(&b[o..o + 4]);
    u32::from_le_bytes(x)
}

fn r64(b: &[u8], o: usize) -> u64 {
    let mut x = [0u8; 8];
    x.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(x)
}

fn w32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn w64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

fn push_part(t: &mut Table, p: Part) -> bool {
    if t.n >= MAX_PARTS {
        return false;
    }
    t.parts[t.n] = p;
    t.n += 1;
    true
}

fn next_index(t: &Table) -> u8 {
    (t.n as u8).saturating_add(1)
}

fn mbr_entry(sector: &[u8], i: usize) -> (u8, u64, u64) {
    let o = MBR_PART_OFF + i * MBR_ENTRY_LEN;
    let sys = sector[o + 4];
    let start = r32(sector, o + 8) as u64;
    let count = r32(sector, o + 12) as u64;
    (sys, start, count)
}

fn write_mbr_entry(sector: &mut [u8], i: usize, sys: u8, start: u32, count: u32) {
    let o = MBR_PART_OFF + i * MBR_ENTRY_LEN;
    sector[o] = 0;
    sector[o + 4] = sys;
    w32(sector, o + 8, start);
    w32(sector, o + 12, count);
}

pub fn pack_mbr(buf: &mut [u8], slots: &[(u8, u32, u32); 4]) {
    if buf.len() < 512 {
        return;
    }
    let mut i = 0usize;
    while i < 512 {
        buf[i] = 0;
        i += 1;
    }
    i = 0;
    while i < 4 {
        let (sys, start, count) = slots[i];
        if sys != 0 && count != 0 {
            write_mbr_entry(buf, i, sys, start, count);
        }
        i += 1;
    }
    buf[MBR_SIG_OFF] = 0x55;
    buf[MBR_SIG_OFF + 1] = 0xAA;
}

pub fn pack_protective_mbr(buf: &mut [u8], nsectors: u64) {
    let count = if nsectors > 1 {
        let n = nsectors - 1;
        if n > u32::MAX as u64 {
            u32::MAX
        } else {
            n as u32
        }
    } else {
        0
    };
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
    if buf.len() < 512 {
        return;
    }
    let mut i = 0usize;
    while i < 512 {
        buf[i] = 0;
        i += 1;
    }
    buf[0..8].copy_from_slice(GPT_SIG);
    w32(buf, 8, GPT_REVISION);
    w32(buf, 12, GPT_HEADER_SIZE);
    w32(buf, 16, 0);
    w64(buf, 24, h.my_lba);
    w64(buf, 32, h.alt_lba);
    w64(buf, 40, h.first_usable);
    w64(buf, 48, h.last_usable);
    buf[56..72].copy_from_slice(&h.disk_guid);
    w64(buf, 72, h.part_lba);
    w32(buf, 80, h.part_count);
    w32(buf, 84, h.part_size);
    w32(buf, 88, h.entries_crc);
    let crc = crc32_ieee(&buf[..GPT_HEADER_SIZE as usize]);
    w32(buf, 16, crc);
}

pub fn pack_gpt_entry(
    buf: &mut [u8],
    type_guid: &[u8; 16],
    uniq: &[u8; 16],
    first: u64,
    last: u64,
    name: &str,
) {
    if buf.len() < GPT_ENTRY_SIZE as usize {
        return;
    }
    let mut i = 0usize;
    while i < GPT_ENTRY_SIZE as usize {
        buf[i] = 0;
        i += 1;
    }
    buf[0..16].copy_from_slice(type_guid);
    buf[16..32].copy_from_slice(uniq);
    w64(buf, 32, first);
    w64(buf, 40, last);
    utf16le_name(&mut buf[56..56 + 72], name);
}

fn utf16le_name(dst: &mut [u8], s: &str) {
    let mut o = 0usize;
    for c in s.chars() {
        if o + 1 >= dst.len() {
            break;
        }
        let u = c as u32;
        if u > 0xFFFF {
            break;
        }
        dst[o] = u as u8;
        dst[o + 1] = (u >> 8) as u8;
        o += 2;
    }
}

pub fn entries_crc(entries: &[u8]) -> u32 {
    crc32_ieee(entries)
}

fn header_ok(sec: &[u8], nsectors: u64) -> bool {
    if sec.len() < GPT_HEADER_SIZE as usize {
        return false;
    }
    if &sec[0..8] != GPT_SIG {
        return false;
    }
    let size = r32(sec, 12);
    if size < 92 || size as usize > sec.len() {
        return false;
    }
    let stored = r32(sec, 16);
    let mut tmp = [0u8; 512];
    if size as usize > tmp.len() {
        return false;
    }
    tmp[..size as usize].copy_from_slice(&sec[..size as usize]);
    w32(&mut tmp, 16, 0);
    if crc32_ieee(&tmp[..size as usize]) != stored {
        return false;
    }
    let my = r64(sec, 24);
    if my >= nsectors {
        return false;
    }
    let psz = r32(sec, 84);
    let nent = r32(sec, 80);
    psz == GPT_ENTRY_SIZE && nent > 0 && nent <= 128
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
    let nbytes = want as u64;
    let nsec = nbytes.div_ceil(bs);
    if part_lba
        .checked_add(nsec)
        .map(|e| e > nsectors)
        .unwrap_or(true)
    {
        return Err(PartError::Truncated);
    }
    let mut done = 0usize;
    let mut lba = part_lba;
    while done < want {
        read(lba, sector_buf).map_err(|_| PartError::Truncated)?;
        let n = (want - done).min(sector_size as usize);
        scratch[done..done + n].copy_from_slice(&sector_buf[..n]);
        done += n;
        lba = lba.saturating_add(1);
    }
    Ok(nent)
}

fn parse_gpt_entries(t: &mut Table, entries: &[u8], nent: u32, nsectors: u64) {
    let mut i = 0u32;
    while i < nent {
        let o = (i as usize).saturating_mul(GPT_ENTRY_SIZE as usize);
        if o + GPT_ENTRY_SIZE as usize > entries.len() {
            break;
        }
        let e = &entries[o..o + GPT_ENTRY_SIZE as usize];
        let mut guid = [0u8; 16];
        guid.copy_from_slice(&e[0..16]);
        if guid == GUID_UNUSED {
            i += 1;
            continue;
        }
        let first = r64(e, 32);
        let last = r64(e, 40);
        if last < first {
            i += 1;
            continue;
        }
        let n = last.saturating_sub(first).saturating_add(1);
        let end = first.saturating_add(n);
        if n == 0 || end > nsectors {
            i += 1;
            continue;
        }
        let p = Part {
            index: next_index(t),
            start_lba: first,
            nsectors: n,
            kind: PartKind::Gpt { type_guid: guid },
        };
        if !push_part(t, p) {
            break;
        }
        i += 1;
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
    let part_lba = r64(sector_buf, 72);
    let nent = r32(sector_buf, 80);
    let stored_ecrc = r32(sector_buf, 88);
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
    if crc32_ieee(&scratch[..want]) != stored_ecrc {
        return Err(PartError::BadCrc);
    }
    let mut t = Table::empty(TableOrigin::Gpt { used_backup });
    parse_gpt_entries(&mut t, scratch, got, nsectors);
    Ok(t)
}

fn parse_logical<R: FnMut(u64, &mut [u8]) -> Result<(), BlockError>>(
    t: &mut Table,
    nsectors: u64,
    read: &mut R,
    sector_buf: &mut [u8],
    ext_start: u64,
    ext_count: u64,
) {
    let ext_end = ext_start.saturating_add(ext_count);
    let mut ebr = ext_start;
    let mut depth = 0u32;
    while depth < MAX_EBR_DEPTH {
        depth += 1;
        if ebr >= nsectors || ebr >= ext_end {
            return;
        }
        if read(ebr, sector_buf).is_err() {
            return;
        }
        if sector_buf.len() < 512
            || sector_buf[MBR_SIG_OFF] != 0x55
            || sector_buf[MBR_SIG_OFF + 1] != 0xAA
        {
            return;
        }
        let (sys, rel, count) = mbr_entry(sector_buf, 0);
        if sys != 0 && count != 0 && !is_extended(sys) && sys != MBR_PROTECTIVE {
            let start = match ebr.checked_add(rel) {
                Some(s) => s,
                None => return,
            };
            let end = match start.checked_add(count) {
                Some(e) => e,
                None => return,
            };
            if start >= nsectors || end > nsectors || start < ext_start || end > ext_end {
                return;
            }
            let p = Part {
                index: next_index(t),
                start_lba: start,
                nsectors: count,
                kind: PartKind::Mbr { sys },
            };
            if !push_part(t, p) {
                return;
            }
        }
        let (nsys, nrel, _) = mbr_entry(sector_buf, 1);
        if nrel == 0 || !is_extended(nsys) {
            return;
        }
        let next = match ext_start.checked_add(nrel) {
            Some(n) => n,
            None => return,
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
    let mut i = 0usize;
    while i < 4 {
        let (sys, start, count) = mbr_entry(sector_buf, i);
        if sys == 0 || count == 0 || sys == MBR_PROTECTIVE {
            i += 1;
            continue;
        }
        if is_extended(sys) {
            parse_logical(&mut t, nsectors, read, sector_buf, start, count);
            i += 1;
            continue;
        }
        let end = match start.checked_add(count) {
            Some(e) => e,
            None => {
                i += 1;
                continue;
            }
        };
        if start >= nsectors || end > nsectors {
            i += 1;
            continue;
        }
        let p = Part {
            index: next_index(&t),
            start_lba: start,
            nsectors: count,
            kind: PartKind::Mbr { sys },
        };
        if !push_part(&mut t, p) {
            break;
        }
        i += 1;
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
    let mbr_sig = sector_buf.len() >= 512
        && sector_buf[MBR_SIG_OFF] == 0x55
        && sector_buf[MBR_SIG_OFF + 1] == 0xAA;
    let mut protective = false;
    if mbr_sig {
        let mut i = 0usize;
        while i < 4 {
            let (sys, _, _) = mbr_entry(sector_buf, i);
            if sys == MBR_PROTECTIVE {
                protective = true;
            }
            i += 1;
        }
    }
    if nsectors >= 2 {
        let primary = try_gpt(
            nsectors,
            sector_size,
            &mut read,
            sector_buf,
            scratch,
            1,
            false,
        );
        match primary {
            Ok(t) => return Ok(t),
            Err(PartError::Truncated) if !protective => {}
            Err(_) => {
                let backup = try_gpt(
                    nsectors,
                    sector_size,
                    &mut read,
                    sector_buf,
                    scratch,
                    nsectors - 1,
                    true,
                );
                if let Ok(t) = backup {
                    return Ok(t);
                }
                if protective {
                    return Err(PartError::BadCrc);
                }
            }
        }
    } else if protective {
        return Err(PartError::Truncated);
    }
    read(0, sector_buf).map_err(|_| PartError::Truncated)?;
    if !mbr_sig {
        return Err(PartError::Empty);
    }
    parse_mbr(nsectors, &mut read, sector_buf)
}

/// Host helper: parse a whole-disk image.
pub fn parse_image(disk: &[u8], sector_size: u32) -> Result<Table, PartError> {
    if sector_size == 0 {
        return Err(PartError::Invalid);
    }
    let nsectors = (disk.len() as u64) / (sector_size as u64);
    let mut sec = [0u8; 512];
    if sector_size as usize > sec.len() {
        return Err(PartError::Invalid);
    }
    let mut scratch = [0u8; 128 * 128];
    parse(
        nsectors,
        sector_size,
        |lba, buf| {
            let bs = sector_size as usize;
            let off = (lba as usize).checked_mul(bs).ok_or(BlockError::Inval)?;
            if off.checked_add(bs).map(|e| e > disk.len()).unwrap_or(true) {
                return Err(BlockError::Inval);
            }
            let n = buf.len().min(bs);
            buf[..n].copy_from_slice(&disk[off..off + n]);
            Ok(())
        },
        &mut sec,
        &mut scratch,
    )
}

#[cfg(test)]
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
        assert_eq!(t.parts[2].index, 3);
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
    }
}
