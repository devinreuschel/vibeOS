use super::*;
use crate::time::{civil_from_unix, unix_from_civil};

impl FatVol {
    pub(super) fn read_dirent<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        mut off: u32,
    ) -> Result<Option<(u32, Node)>, FatError> {
        let mut lfn = Lfn::EMPTY;
        loop {
            let mut ent = [0u8; ENT];
            // A damaged chain ends the walk; a sector the disk fails to
            // read is the caller's `Io`, never the directory's end.
            match self.read_dir_raw(d, dir, off, &mut ent) {
                Err(FatError::Corrupt) => return Ok(None),
                Err(e) => return Err(e),
                Ok(false) => return Ok(None),
                Ok(true) => {}
            }
            let next = off.checked_add(ENT as u32).ok_or(FatError::Corrupt)?;
            if ent[0] == ENT_FREE {
                return Ok(None);
            }
            if ent[0] == ENT_DEL {
                lfn = Lfn::EMPTY;
                off = next;
                continue;
            }
            if ent[11] == ATTR_LFN {
                lfn.gather(&ent)?;
                off = next;
                continue;
            }
            if ent[11] & ATTR_VOL != 0 {
                lfn = Lfn::EMPTY;
                off = next;
                continue;
            }
            let mut sn = [0u8; 11];
            sn.copy_from_slice(&ent[..11]);
            let mut name = [0u8; MAX_NAME];
            let len = lfn.name(lfn_checksum(&sn), &mut name).unwrap_or(0);
            let node = self.node_from_short(dir, off, &ent, name.get(..len).unwrap_or(&[]))?;
            return Ok(Some((next, node)));
        }
    }

    pub(super) fn read_dir_raw<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        off: u32,
        ent: &mut [u8; ENT],
    ) -> Result<bool, FatError> {
        let cb = self.info.clus_bytes() as u32;
        let idx = off.checked_div(cb).ok_or(FatError::Corrupt)?;
        let pin = off.checked_rem(cb).ok_or(FatError::Corrupt)? as usize;
        let clu = match self.nth_clu(d, dir, idx)? {
            None => return Ok(false),
            Some(c) => c,
        };
        let n = Self::read_cluster(&self.info, d, clu, &mut self.clbuf)?;
        let end = pin.checked_add(ENT).ok_or(FatError::Corrupt)?;
        if end > n {
            return Err(FatError::Corrupt);
        }
        ent.copy_from_slice(self.clbuf.get(pin..end).ok_or(FatError::Corrupt)?);
        Ok(true)
    }

    pub(super) fn write_dir_raw<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        off: u32,
        ent: &[u8; ENT],
    ) -> Result<(), FatError> {
        let cb = self.info.clus_bytes() as u32;
        let idx = off.checked_div(cb).ok_or(FatError::Corrupt)?;
        let pin = off.checked_rem(cb).ok_or(FatError::Corrupt)? as usize;
        let clu = self.nth_clu(d, dir, idx)?.ok_or(FatError::Corrupt)?;
        let n = Self::read_cluster(&self.info, d, clu, &mut self.clbuf)?;
        let end = pin.checked_add(ENT).ok_or(FatError::Corrupt)?;
        if end > n {
            return Err(FatError::Corrupt);
        }
        put_at(&mut self.clbuf, pin, ent)?;
        Self::write_cluster(
            &self.info,
            d,
            clu,
            self.clbuf.get(..n).ok_or(FatError::Corrupt)?,
        )
    }

    /// Find `slots` free entries in `dir` and return where they start. A
    /// run of deleted entries between live ones serves when it is long
    /// enough; otherwise the free run that reaches the first `0x00` entry,
    /// with every entry after it to the chain's end, serves, and the
    /// directory grows by one cluster only when that is too short.
    pub(super) fn dir_reserve<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        slots: usize,
    ) -> Result<(u32, u32), FatError> {
        let need = slots
            .checked_mul(ENT)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(FatError::NoSpace)?;
        let cb = self.info.clus_bytes() as u32;
        let mut off = 0u32;
        let mut run = 0usize;
        let mut run_off = 0u32;
        loop {
            let mut ent = [0u8; ENT];
            if !self.read_dir_raw(d, dir, off, &mut ent)? {
                break;
            }
            if ent[0] == ENT_FREE {
                if run == 0 {
                    run_off = off;
                }
                run = run.checked_add(1).ok_or(FatError::Corrupt)?;
                break;
            }
            if ent[0] == ENT_DEL {
                if run == 0 {
                    run_off = off;
                }
                run = run.checked_add(1).ok_or(FatError::Corrupt)?;
                if run >= slots {
                    return Ok((dir, run_off));
                }
            } else {
                run = 0;
            }
            off = off.checked_add(ENT_U32).ok_or(FatError::Corrupt)?;
        }
        let start = if run > 0 { run_off } else { off };
        let (nclu, last) = self.chain_len(d, dir)?;
        let chain = nclu.checked_mul(cb).ok_or(FatError::Corrupt)?;
        let avail = chain.checked_sub(start).ok_or(FatError::Corrupt)?;
        let end = start.checked_add(need).ok_or(FatError::NoSpace)?;
        if avail < need {
            let short = need.checked_sub(avail).ok_or(FatError::Corrupt)?;
            if short > cb || end > MAX_DIR_BYTES {
                return Err(FatError::NoSpace);
            }
            let new = self.alloc_clu(d, last)?;
            let linked = self
                .zero_cluster(d, new)
                .and_then(|()| self.fat_set(d, last, new));
            if let Err(e) = linked {
                return self
                    .fat_set(d, new, 0)
                    .and_then(|()| self.commit_fat(d))
                    .and(Err(e));
            }
            self.commit_fat(d)?;
            d.flush()?;
        }
        // A foreign image may hold garbage after its terminator: the entry
        // after the reserved ones ends the directory.
        let mut ent = [0u8; ENT];
        if self.read_dir_raw(d, dir, end, &mut ent)? && ent[0] != ENT_FREE {
            ent.fill(0);
            self.write_dir_raw(d, dir, end, &ent)?;
        }
        Ok((dir, start))
    }

    pub(super) fn mark_deleted<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        short_off: u32,
    ) -> Result<(), FatError> {
        // Walk back over LFN entries.
        let mut off = short_off;
        loop {
            let mut ent = [0u8; ENT];
            if !self.read_dir_raw(d, dir, off, &mut ent)? {
                break;
            }
            ent[0] = ENT_DEL;
            self.write_dir_raw(d, dir, off, &ent)?;
            let Some(prev) = off.checked_sub(ENT as u32) else {
                break;
            };
            let mut p = [0u8; ENT];
            if !self.read_dir_raw(d, dir, prev, &mut p)? {
                break;
            }
            if p[11] != ATTR_LFN {
                break;
            }
            off = prev;
        }
        Ok(())
    }

    pub(super) fn dir_empty<D: Disk>(&mut self, d: &mut D, dir: u32) -> Result<bool, FatError> {
        let mut off = 0u32;
        loop {
            match self.read_dirent(d, dir, off)? {
                None => return Ok(true),
                Some((next, node)) => {
                    if !name_is_dot(node.name()) && !name_is_dotdot(node.name()) {
                        return Ok(false);
                    }
                    off = next;
                }
            }
        }
    }

    pub(super) fn pick_short<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        name: &[u8],
        out: &mut [u8; 11],
    ) -> Result<bool, FatError> {
        if let Some(s) = as_pure_83(name) {
            *out = s;
            if !self.short_taken(d, dir, out)? {
                return Ok(false);
            }
        }
        let mut stem = [b' '; 11];
        make_lossy_83(name, &mut stem);
        if !self.short_taken(d, dir, &stem)? {
            *out = stem;
            return Ok(true);
        }
        for n in 1..1_000_000u32 {
            *out = stem;
            apply_tilde(out, n);
            if !self.short_taken(d, dir, out)? {
                return Ok(true);
            }
        }
        Err(FatError::NoSpace)
    }

    fn short_taken<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        short: &[u8; 11],
    ) -> Result<bool, FatError> {
        let mut off = 0u32;
        loop {
            let mut ent = [0u8; ENT];
            match self.read_dir_raw(d, dir, off, &mut ent) {
                Ok(false) => return Ok(false),
                Err(e) => return Err(e),
                Ok(true) => {
                    if ent[0] == ENT_FREE {
                        return Ok(false);
                    }
                    if ent[0] != ENT_DEL && ent[11] != ATTR_LFN && ent[..11] == short[..] {
                        return Ok(true);
                    }
                    off = off.checked_add(ENT as u32).ok_or(FatError::Corrupt)?;
                }
            }
        }
    }

    pub(super) fn init_dir_cluster<D: Disk>(
        &mut self,
        d: &mut D,
        clu: u32,
        parent: u32,
    ) -> Result<(), FatError> {
        let n = self.info.clus_bytes();
        let now = self.now;
        let buf = &mut self.clbuf;
        buf.fill(0);
        fill_dot(&mut buf[0..ENT], b".          ", clu, now)?;
        let p = if parent == self.info.root_clus {
            0
        } else {
            parent
        };
        fill_dot(&mut buf[ENT..ENT * 2], b"..         ", p, now)?;
        Self::write_cluster(
            &self.info,
            d,
            clu,
            self.clbuf.get(..n).ok_or(FatError::Corrupt)?,
        )
    }

    pub(super) fn update_short<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        off: u32,
        first: u32,
        size: u32,
    ) -> Result<(), FatError> {
        let mut ent = [0u8; ENT];
        if !self.read_dir_raw(d, dir, off, &mut ent)? {
            return Err(FatError::Corrupt);
        }
        let (date, time) = fat_datetime(self.now);
        put_le16(&mut ent, 20, (first >> 16) as u16)?;
        put_le16(&mut ent, 22, time)?;
        put_le16(&mut ent, 24, date)?;
        put_le16(&mut ent, 26, (first & 0xFFFF) as u16)?;
        put_le32(&mut ent, 28, size)?;
        self.write_dir_raw(d, dir, off, &ent)
    }
}

pub fn lfn_checksum(name: &[u8; 11]) -> u8 {
    name.iter()
        .fold(0u8, |sum, &c| sum.rotate_right(1).wrapping_add(c))
}

/// Whether two names are one FAT name: ASCII letters compare without
/// regard to case.
pub fn eq_ci(a: &[u8], b: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn to_upper(c: u8) -> u8 {
    c.to_ascii_uppercase()
}

/// The UTF-16 code units that name `name`; `Inval` when it is not UTF-8.
pub(super) fn utf16_len(name: &[u8]) -> Result<usize, FatError> {
    let s = core::str::from_utf8(name).map_err(|_| FatError::Inval)?;
    Ok(s.encode_utf16().count())
}

/// The byte offset of each of an LFN entry's 13 UTF-16 units, in name
/// order: five at 1, six at 14, two at 28.
const LFN_OFFS: [usize; LFN_CHARS] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];

/// The most LFN entries a [`MAX_NAME`]-byte UTF-8 name needs: one unit per
/// byte at most, 13 units an entry.
const LFN_MAX_ENTS: usize = MAX_NAME.div_ceil(LFN_CHARS);

/// The highest ordinal FAT allows: 20 entries of 13 units, 255 units.
const LFN_MAX_ORD: u8 = 20;

/// The long name gathered from the LFN entries before a short entry,
/// which arrive highest ordinal first.
struct Lfn {
    units: [u16; LFN_MAX_ENTS * LFN_CHARS],
    /// The unit count the first (highest) entry set; 0 when nothing is
    /// gathered.
    len: usize,
    /// The ordinal the next entry must carry; 0 once ordinal 1 is in.
    next_ord: u8,
    cs: u8,
    /// A sequence longer than [`MAX_NAME`] bytes can hold: its short name
    /// serves.
    too_long: bool,
}

impl Lfn {
    const EMPTY: Self = Self {
        units: [0; LFN_MAX_ENTS * LFN_CHARS],
        len: 0,
        next_ord: 0,
        cs: 0,
        too_long: false,
    };

    /// Take LFN entry `ent` into the sequence. Anything out of order
    /// (an ordinal of 0 or above [`LFN_MAX_ORD`], a gap, a checksum that
    /// changes) drops what was gathered.
    fn gather(&mut self, ent: &[u8; ENT]) -> Result<(), FatError> {
        let ord = ent[0] & !LFN_LAST;
        let cs = ent[13];
        if ord == 0 || ord > LFN_MAX_ORD {
            *self = Self::EMPTY;
            return Ok(());
        }
        if ent[0] & LFN_LAST != 0 {
            *self = Self::EMPTY;
            self.cs = cs;
            self.len = usize::from(ord)
                .checked_mul(LFN_CHARS)
                .ok_or(FatError::Corrupt)?;
            self.too_long = usize::from(ord) > LFN_MAX_ENTS;
        } else if self.next_ord != ord || self.cs != cs {
            *self = Self::EMPTY;
            return Ok(());
        }
        self.next_ord = ord.checked_sub(1).ok_or(FatError::Corrupt)?;
        if self.too_long {
            return Ok(());
        }
        let base = usize::from(self.next_ord)
            .checked_mul(LFN_CHARS)
            .ok_or(FatError::Corrupt)?;
        let dst = base
            .checked_add(LFN_CHARS)
            .and_then(|end| self.units.get_mut(base..end))
            .ok_or(FatError::Corrupt)?;
        for (u, &o) in dst.iter_mut().zip(LFN_OFFS.iter()) {
            *u = le16(ent, o)?;
        }
        Ok(())
    }

    /// The gathered name as UTF-8 in `out` and its length, when a whole
    /// sequence whose checksum is `cs` is in; `None` sends the caller to
    /// the short name.
    fn name(&self, cs: u8, out: &mut [u8; MAX_NAME]) -> Option<usize> {
        if self.len == 0 || self.next_ord != 0 || self.too_long || self.cs != cs {
            return None;
        }
        take_lfn(self.units.get(..self.len)?, out)
    }
}

/// Decode the UTF-16 `units` of a long name, up to its first `0x0000`,
/// into UTF-8 in `out`; the byte count, or `None` for an unpaired
/// surrogate, an empty name, a name over [`MAX_NAME`] bytes, or one that
/// holds `/`.
pub(super) fn take_lfn(units: &[u16], out: &mut [u8; MAX_NAME]) -> Option<usize> {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    let mut n = 0usize;
    for c in char::decode_utf16(units.get(..end)?.iter().copied()) {
        let c = c.ok()?;
        if c == '/' {
            return None;
        }
        let at = n.checked_add(c.len_utf8())?;
        c.encode_utf8(out.get_mut(n..at)?);
        n = at;
    }
    (n > 0).then_some(n)
}

/// Fill `ent` as LFN entry `ord` of `name`: its units from
/// `(ord - 1) * 13`, then one `0x0000` and `0xFFFF` padding.
pub(super) fn fill_lfn(
    ent: &mut [u8; ENT],
    ord: u8,
    last: bool,
    cs: u8,
    name: &[u8],
) -> Result<(), FatError> {
    let s = core::str::from_utf8(name).map_err(|_| FatError::Inval)?;
    ent.fill(0);
    ent[0] = if last { ord | LFN_LAST } else { ord };
    ent[11] = ATTR_LFN;
    ent[13] = cs;
    let start = (ord as usize)
        .checked_sub(1)
        .and_then(|p| p.checked_mul(LFN_CHARS))
        .ok_or(FatError::Inval)?;
    let mut units = s.encode_utf16().skip(start);
    let mut term = false;
    for &o in LFN_OFFS.iter() {
        let c = if term {
            0xFFFF
        } else if let Some(u) = units.next() {
            u
        } else {
            term = true;
            0
        };
        put_le16(ent, o, c)?;
    }
    Ok(())
}

pub(super) fn decode_short(ent: &[u8; ENT]) -> ([u8; MAX_NAME], u8) {
    let mut tmp = [0u8; 11];
    tmp.copy_from_slice(&ent[..11]);
    if tmp[0] == 0x05 {
        tmp[0] = 0xE5;
    }
    let stem = tmp[..8].iter().copied().take_while(|&c| c != b' ');
    let dot = (tmp[8] != b' ').then_some(b'.');
    let ext = tmp[8..].iter().copied().take_while(|&c| c != b' ');
    let mut out = [0u8; MAX_NAME];
    let n = out
        .iter_mut()
        .zip(stem.chain(dot).chain(ext))
        .map(|(o, c)| *o = c)
        .count();
    (out, n as u8)
}

fn as_pure_83(name: &[u8]) -> Option<[u8; 11]> {
    if name
        .iter()
        .any(|&c| c != b'.' && (!is_83_char(c) || c.is_ascii_lowercase()))
    {
        return None;
    }
    let mut out = [b' '; 11];
    match name.iter().position(|&c| c == b'.') {
        None => {
            if name.is_empty() || name.len() > 8 {
                return None;
            }
            out.get_mut(..name.len())?.copy_from_slice(name);
        }
        Some(0) => return None,
        Some(d) => {
            let stem = name.get(..d)?;
            let ext = name.get(d.checked_add(1)?..)?;
            if stem.len() > 8 || ext.is_empty() || ext.len() > 3 || ext.contains(&b'.') {
                return None;
            }
            out.get_mut(..stem.len())?.copy_from_slice(stem);
            out.get_mut(8..)?.get_mut(..ext.len())?.copy_from_slice(ext);
        }
    }
    Some(out)
}

fn is_83_char(c: u8) -> bool {
    matches!(c, b'A'..=b'Z' | b'0'..=b'9' | b'!' | b'#' | b'$' | b'%' | b'&'
        | b'\'' | b'(' | b')' | b'-' | b'@' | b'^' | b'_' | b'`' | b'{' | b'}'
        | b'~')
}

/// An 8.3 stem and extension for `name`: leading dots skipped, the stem up
/// to the next dot and the extension after it with its dots dropped, each
/// upper-cased, a character 8.3 cannot hold made `_`, and cut to 8 and 3.
fn make_lossy_83(name: &[u8], out: &mut [u8; 11]) {
    out.fill(b' ');
    let rest = name
        .iter()
        .position(|&c| c != b'.')
        .and_then(|i| name.get(i..))
        .unwrap_or(&[]);
    let (stem, ext) = match rest.iter().position(|&c| c == b'.') {
        Some(d) => rest.split_at(d),
        None => (rest, &[][..]),
    };
    let lossy = |&b: &u8| {
        let c = to_upper(b);
        if is_83_char(c) { c } else { b'_' }
    };
    let (out_stem, out_ext) = out.split_at_mut(8);
    let sn = out_stem
        .iter_mut()
        .zip(stem.iter().map(lossy))
        .map(|(o, c)| *o = c)
        .count();
    if sn == 0
        && let Some(c) = out_stem.first_mut()
    {
        *c = b'_';
    }
    out_ext
        .iter_mut()
        .zip(ext.iter().filter(|&&c| c != b'.').map(lossy))
        .for_each(|(o, c)| *o = c);
}

/// Put `~` and the last eight decimal digits of `n` (1 for 0) at the end
/// of the stem, leaving at least one stem character.
fn apply_tilde(out: &mut [u8; 11], n: u32) {
    let mut digits = [0u8; 8];
    let lsd_first =
        core::iter::successors(Some(n.max(1)), |&x| (x >= 10).then_some(x / 10)).map(|x| {
            b"0123456789"
                .get((x % 10) as usize)
                .copied()
                .unwrap_or(b'0')
        });
    let dn = digits
        .iter_mut()
        .zip(lsd_first)
        .map(|(o, c)| *o = c)
        .count();
    let stem = 8usize.saturating_sub(dn.saturating_add(1)).max(1);
    if let Some((tilde, tail)) = out.get_mut(stem..).and_then(|t| t.split_first_mut()) {
        *tilde = b'~';
        let msd_first = digits.get(..dn).unwrap_or(&[]).iter().rev();
        for (o, &c) in tail.iter_mut().zip(msd_first) {
            *o = c;
        }
    }
}

fn fill_dot(ent: &mut [u8], name11: &[u8], clu: u32, now: u64) -> Result<(), FatError> {
    ent.fill(0);
    put_at(ent, 0, name11)?;
    put_at(ent, 11, &[ATTR_DIR])?;
    let (date, time) = fat_datetime(now);
    put_le16(ent, 14, time)?;
    put_le16(ent, 16, date)?;
    put_le16(ent, 18, date)?;
    put_le16(ent, 20, (clu >> 16) as u16)?;
    put_le16(ent, 22, time)?;
    put_le16(ent, 24, date)?;
    put_le16(ent, 26, (clu & 0xFFFF) as u16)?;
    Ok(())
}

/// Unix time of 1980-01-01 00:00:00 UTC, the first instant a FAT date
/// holds.
pub const FAT_EPOCH_UNIX: u64 = 315_532_800;
/// Unix time of 2107-12-31 23:59:58 UTC, the last instant a FAT date and
/// time hold.
pub const FAT_LAST_UNIX: u64 = 4_354_819_198;

/// FAT `(date, time)` of `secs` unix seconds, clamped to
/// [`FAT_EPOCH_UNIX`]..=[`FAT_LAST_UNIX`]: years from 1980 with the
/// Gregorian leap rule (`vibeos::time`'s calendar), seconds in 2 s units.
pub(super) fn fat_datetime(secs: u64) -> (u16, u16) {
    let c = civil_from_unix(secs.clamp(FAT_EPOCH_UNIX, FAT_LAST_UNIX));
    let y = c
        .year
        .checked_sub(1980)
        .and_then(|y| u16::try_from(y).ok())
        .unwrap_or(0)
        & 0x7F;
    let date = (y << 9) | (u16::from(c.month) << 5) | u16::from(c.day);
    let time = (u16::from(c.hour) << 11) | (u16::from(c.min) << 5) | u16::from(c.sec / 2);
    (date, time)
}

/// Unix seconds of FAT `(date, time)`, the inverse of [`fat_datetime`].
/// The fields are disk bytes (AGENTS rule 4): each out of range is
/// clamped (month to 1..=12, day to 1..=31, hour to 23, minute to 59, the
/// 2-second count to 29), never a panic.
pub(super) fn fat_to_unix(date: u16, time: u16) -> u64 {
    let y = i32::from((date >> 9) & 0x7F);
    let m = ((date >> 5) & 0xF).clamp(1, 12) as u8;
    let d = (date & 0x1F).clamp(1, 31) as u8;
    let h = ((time >> 11) & 0x1F).min(23) as u8;
    let mi = ((time >> 5) & 0x3F).min(59) as u8;
    let s = (time & 0x1F).min(29) as u8;
    y.checked_add(1980)
        .zip(s.checked_mul(2))
        .and_then(|(year, sec)| unix_from_civil(year, m, d, h, mi, sec))
        .unwrap_or(FAT_EPOCH_UNIX)
}
