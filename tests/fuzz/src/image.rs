//! `Sparse`: a disk image decoded from a fuzz input (C-FUZZ).
//!
//! Byte 0 is flags: bit 0 turns on the GPT checksum fix-up
//! ([`fix_gpt_crcs`], used only by `part_parse`), bit 1 selects
//! `part_parse`'s 4096-byte sectors. Bytes 1 to 4 are a `u32` LE unit
//! count, clamped to `1..=max`. Chunks follow: each is a `u32` LE unit
//! index, taken modulo the count, then one unit of content; the last chunk
//! is zero-padded. A later chunk replaces an earlier one, and a unit no
//! chunk names reads as zero. Writes go to an overlay, which reads see.
//! An input shorter than 5 bytes is no image.
//!
//! The size caps bound the harness, not the parser (TESTING.md §8.1).

use std::collections::BTreeMap;

use vibeos::block::BlockError;
use vibeos::block::part::crc32_ieee;
use vibeos::fs::FsError;
use vibeos::{fat, vibefs};

/// Flag bit 0: recompute the GPT CRCs before parsing.
pub const FLAG_FIX_CRC: u8 = 1 << 0;
/// Flag bit 1: 4096-byte sectors for `part_parse`.
pub const FLAG_4K: u8 = 1 << 1;
/// Bytes of the header: flags and the unit count.
pub const HEADER: usize = 5;
/// The most GPT entry-array bytes [`fix_gpt_crcs`] reads.
const GPT_ENTRIES_CAP: u64 = 1 << 20;

pub struct Sparse {
    flags: u8,
    unit: usize,
    count: u32,
    base: BTreeMap<u32, Vec<u8>>,
    overlay: BTreeMap<u32, Vec<u8>>,
}

impl Sparse {
    /// Decode `data` into units of `unit` bytes, at most `max` of them;
    /// `None` for an input shorter than [`HEADER`].
    pub fn parse(data: &[u8], unit: usize, max: u32) -> Option<Self> {
        let (&flags, rest) = data.split_first()?;
        let (count, mut rest) = rest.split_first_chunk::<4>()?;
        let count = u32::from_le_bytes(*count).clamp(1, max.max(1));
        let mut base = BTreeMap::new();
        while !rest.is_empty() {
            let mut chunk = vec![0u8; unit.saturating_add(4)];
            let n = rest.len().min(chunk.len());
            let (head, tail) = rest.split_at(n);
            chunk[..n].copy_from_slice(head);
            rest = tail;
            let idx = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) % count;
            base.insert(idx, chunk.split_off(4));
        }
        Some(Self {
            flags,
            unit,
            count,
            base,
            overlay: BTreeMap::new(),
        })
    }

    /// Encode units into the format [`Self::parse`] reads: the header, then
    /// one chunk per unit, in order. The seed generator's encoder.
    pub fn encode(flags: u8, count: u32, units: &[(u32, &[u8])], unit: usize) -> Vec<u8> {
        let mut out = vec![flags];
        out.extend_from_slice(&count.to_le_bytes());
        for &(idx, content) in units {
            out.extend_from_slice(&idx.to_le_bytes());
            let n = content.len().min(unit);
            out.extend_from_slice(&content[..n]);
            out.resize(out.len() + (unit - n), 0);
        }
        out
    }

    /// Encode the non-zero `unit`-byte units of `image`.
    pub fn encode_image(flags: u8, image: &[u8], unit: usize) -> Vec<u8> {
        let count = u32::try_from(image.len().div_ceil(unit)).unwrap_or(u32::MAX);
        let units: Vec<(u32, &[u8])> = image
            .chunks(unit)
            .enumerate()
            .filter(|(_, c)| c.iter().any(|&b| b != 0))
            .map(|(i, c)| (u32::try_from(i).unwrap_or(u32::MAX), c))
            .collect();
        Self::encode(flags, count, &units, unit)
    }

    pub fn flags(&self) -> u8 {
        self.flags
    }

    pub fn unit(&self) -> usize {
        self.unit
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    /// Unit `idx`'s bytes, or `None` past the end. A unit no chunk named
    /// reads as zero.
    pub fn read_unit(&self, idx: u64, buf: &mut [u8]) -> Option<()> {
        let idx = u32::try_from(idx).ok().filter(|&i| i < self.count)?;
        let n = buf.len().min(self.unit);
        match self.overlay.get(&idx).or_else(|| self.base.get(&idx)) {
            Some(u) => buf[..n].copy_from_slice(&u[..n]),
            None => buf[..n].fill(0),
        }
        Some(())
    }

    /// Store `buf`'s first unit at `idx` in the overlay; `None` past the end.
    pub fn write_unit(&mut self, idx: u64, buf: &[u8]) -> Option<()> {
        let idx = u32::try_from(idx).ok().filter(|&i| i < self.count)?;
        let mut u = vec![0u8; self.unit];
        let n = buf.len().min(self.unit);
        u[..n].copy_from_slice(&buf[..n]);
        self.overlay.insert(idx, u);
        Some(())
    }

    /// The reader `part::parse` takes: unit `lba` into the front of `buf`.
    pub fn reader(&self) -> impl FnMut(u64, &mut [u8]) -> Result<(), BlockError> + '_ {
        |lba, buf| self.read_unit(lba, buf).ok_or(BlockError::Inval)
    }
}

/// FAT's `Disk` over 512-byte units.
impl fat::Disk for Sparse {
    fn sector_size(&self) -> u32 {
        u32::try_from(self.unit).unwrap_or(0)
    }

    fn nsectors(&self) -> u32 {
        self.count
    }

    fn read(&mut self, lba: u32, buf: &mut [u8]) -> Result<(), FsError> {
        if buf.len() != self.unit {
            return Err(FsError::Inval);
        }
        self.read_unit(u64::from(lba), buf).ok_or(FsError::Io)
    }

    fn write(&mut self, lba: u32, buf: &[u8]) -> Result<(), FsError> {
        if buf.len() != self.unit {
            return Err(FsError::Inval);
        }
        self.write_unit(u64::from(lba), buf).ok_or(FsError::Io)
    }

    fn flush(&mut self) -> Result<(), FsError> {
        Ok(())
    }
}

/// vibefs's `Disk` over 4096-byte units.
impl vibefs::Disk for Sparse {
    fn nblocks(&self) -> u32 {
        self.count
    }

    fn read_block(&mut self, bno: u32, buf: &mut [u8; vibefs::BLOCK]) -> Result<(), FsError> {
        self.read_unit(u64::from(bno), buf).ok_or(FsError::Io)
    }

    fn write_block(&mut self, bno: u32, buf: &[u8; vibefs::BLOCK]) -> Result<(), FsError> {
        self.write_unit(u64::from(bno), buf).ok_or(FsError::Io)
    }

    fn flush(&mut self) -> Result<(), FsError> {
        Ok(())
    }
}

fn le32(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4)
        .map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn le64(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 8).map_or(0, |s| {
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        u64::from_le_bytes(a)
    })
}

/// Recompute the primary (unit 1) and backup (last unit) GPT headers' CRCs
/// and their entry arrays' CRCs with `part::crc32_ieee`, so a fuzzed GPT
/// gets past its checksums. Each entry array is read only as far as the
/// image, and at most [`GPT_ENTRIES_CAP`] bytes.
pub fn fix_gpt_crcs(img: &mut Sparse) {
    let last = u64::from(img.count.saturating_sub(1));
    for lba in [1u64, last] {
        if lba == 0 {
            continue;
        }
        let mut hdr = vec![0u8; img.unit];
        if img.read_unit(lba, &mut hdr).is_none() || hdr.get(..8) != Some(&b"EFI PART"[..]) {
            continue;
        }
        let part_lba = le64(&hdr, 72);
        let want = u64::from(le32(&hdr, 80)).saturating_mul(u64::from(le32(&hdr, 84)));
        let unit = img.unit as u64;
        let room = u64::from(img.count)
            .saturating_sub(part_lba)
            .saturating_mul(unit);
        let len = want.min(room).min(GPT_ENTRIES_CAP);
        let mut entries = vec![0u8; usize::try_from(len).unwrap_or(0)];
        for (i, chunk) in entries.chunks_mut(img.unit).enumerate() {
            let _ = img.read_unit(part_lba.saturating_add(i as u64), chunk);
        }
        hdr[88..92].copy_from_slice(&crc32_ieee(&entries).to_le_bytes());
        let hsize = usize::try_from(le32(&hdr, 12))
            .unwrap_or(0)
            .clamp(92, img.unit);
        hdr[16..20].fill(0);
        let crc = crc32_ieee(&hdr[..hsize]);
        hdr[16..20].copy_from_slice(&crc.to_le_bytes());
        let _ = img.write_unit(lba, &hdr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(img: &Sparse, idx: u64) -> Vec<u8> {
        let mut b = vec![0xAAu8; img.unit()];
        img.read_unit(idx, &mut b).unwrap();
        b
    }

    #[test]
    fn short_input_is_no_image() {
        assert!(Sparse::parse(&[0, 1, 0, 0], 512, 16).is_none());
        assert!(Sparse::parse(&[0, 1, 0, 0, 0], 512, 16).is_some());
    }

    #[test]
    fn count_clamps_to_one_and_max() {
        assert_eq!(Sparse::parse(&[0, 0, 0, 0, 0], 512, 16).unwrap().count(), 1);
        let big = Sparse::parse(&[0, 0xFF, 0xFF, 0xFF, 0xFF], 512, 16).unwrap();
        assert_eq!(big.count(), 16);
        assert!(big.read_unit(16, &mut [0u8; 512]).is_none());
        assert!(big.read_unit(15, &mut [0u8; 512]).is_some());
    }

    #[test]
    fn unnamed_unit_reads_zero() {
        let img = Sparse::parse(&[0, 4, 0, 0, 0], 16, 16).unwrap();
        assert_eq!(read(&img, 3), vec![0u8; 16]);
    }

    #[test]
    fn later_chunk_replaces_earlier_and_index_wraps() {
        let a = [1u8; 16];
        let b = [2u8; 16];
        // Index 6 modulo a count of 4 is unit 2.
        let data = Sparse::encode(0, 4, &[(2, &a), (6, &b)], 16);
        let img = Sparse::parse(&data, 16, 16).unwrap();
        assert_eq!(read(&img, 2), vec![2u8; 16]);
    }

    #[test]
    fn last_chunk_is_zero_padded() {
        let mut data = vec![0, 4, 0, 0, 0, 1, 0, 0, 0];
        data.extend_from_slice(&[7, 7, 7]);
        let img = Sparse::parse(&data, 8, 16).unwrap();
        assert_eq!(read(&img, 1), vec![7, 7, 7, 0, 0, 0, 0, 0]);
        // A chunk cut inside its index still names a unit.
        let img = Sparse::parse(&[0, 4, 0, 0, 0, 3], 8, 16).unwrap();
        assert_eq!(read(&img, 3), vec![0u8; 8]);
    }

    #[test]
    fn overlay_reads_see_writes() {
        let data = Sparse::encode(0, 2, &[(0, &[5u8; 8])], 8);
        let mut img = Sparse::parse(&data, 8, 16).unwrap();
        img.write_unit(0, &[9u8; 8]).unwrap();
        img.write_unit(1, &[3u8; 4]).unwrap();
        assert_eq!(read(&img, 0), vec![9u8; 8]);
        assert_eq!(read(&img, 1), vec![3, 3, 3, 3, 0, 0, 0, 0]);
        assert!(img.write_unit(2, &[1u8; 8]).is_none());
    }

    #[test]
    fn encode_image_keeps_nonzero_units() {
        let mut image = vec![0u8; 64];
        image[20] = 1;
        let data = Sparse::encode_image(FLAG_4K, &image, 16);
        assert_eq!(data.len(), HEADER + 4 + 16);
        let img = Sparse::parse(&data, 16, 16).unwrap();
        assert_eq!(img.flags(), FLAG_4K);
        assert_eq!(img.count(), 4);
        assert_eq!(read(&img, 1)[4], 1);
    }

    #[test]
    fn gpt_crc_fixup_matches_packer() {
        use vibeos::block::part::{GptHeaderInfo, entries_crc, pack_gpt_header};
        let mut hdr = [0u8; 512];
        let entries = [0x5Au8; 1024];
        let h = GptHeaderInfo {
            my_lba: 1,
            alt_lba: 7,
            first_usable: 4,
            last_usable: 5,
            disk_guid: [1; 16],
            part_lba: 2,
            part_count: 8,
            part_size: 128,
            entries_crc: entries_crc(&entries),
        };
        pack_gpt_header(&mut hdr, &h);
        let mut bad = hdr;
        bad[16] ^= 1;
        bad[88] ^= 1;
        let data = Sparse::encode(
            FLAG_FIX_CRC,
            8,
            &[(1, &bad), (2, &entries[..512]), (3, &entries[512..])],
            512,
        );
        let mut img = Sparse::parse(&data, 512, 8).unwrap();
        fix_gpt_crcs(&mut img);
        assert_eq!(read(&img, 1), hdr.to_vec());
    }
}
