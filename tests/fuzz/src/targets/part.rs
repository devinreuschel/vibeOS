//! `part_parse`: MBR, EBR chains and GPT.

use vibeos::block::part;

use crate::image::{FLAG_4K, FLAG_FIX_CRC, Sparse, fix_gpt_crcs};

/// Units a `part_parse` image holds at most.
pub const MAX_UNITS: u32 = 1 << 20;

/// `part::parse` through a [`Sparse`] reader, then `map_child_lba` on each
/// entry with a range from the input's last 16 bytes.
pub fn parse(data: &[u8]) {
    let Some(&flags) = data.first() else {
        return;
    };
    let unit = if flags & FLAG_4K != 0 { 4096 } else { 512 };
    let Some(mut img) = Sparse::parse(data, unit, MAX_UNITS) else {
        return;
    };
    if flags & FLAG_FIX_CRC != 0 {
        fix_gpt_crcs(&mut img);
    }
    let mut sector = vec![0u8; 4096];
    let mut scratch = vec![0u8; 128 * 128];
    let Ok(t) = part::parse(
        u64::from(img.count()),
        unit as u32,
        img.reader(),
        &mut sector,
        &mut scratch,
    ) else {
        return;
    };
    let mut tail = [0u8; 16];
    let n = data.len().min(16);
    tail[..n].copy_from_slice(&data[data.len() - n..]);
    let (lo, hi) = tail.split_at(8);
    let lba = u64::from_le_bytes(lo.try_into().unwrap_or([0; 8]));
    let count = u64::from_le_bytes(hi.try_into().unwrap_or([0; 8]));
    for i in 0..t.n {
        if let Some(p) = t.get(i) {
            let _ = part::map_child_lba(p.start_lba, p.nsectors, lba, count);
        }
    }
}
