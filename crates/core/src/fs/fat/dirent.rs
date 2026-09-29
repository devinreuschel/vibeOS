use super::*;

impl FatVol {
    pub(super) fn read_dirent<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        mut off: u32,
    ) -> Result<Option<(u32, Node)>, FatError> {
        let mut lfn = [0u8; MAX_NAME];
        let mut lfn_len = 0usize;
        let mut expect_cs: Option<u8> = None;
        loop {
            let mut ent = [0u8; ENT];
            match self.read_dir_raw(d, dir, off, &mut ent) {
                Err(FatError::Corrupt) | Err(FatError::Io) => return Ok(None),
                Err(e) => return Err(e),
                Ok(false) => return Ok(None),
                Ok(true) => {}
            }
            let next = off + ENT as u32;
            if ent[0] == ENT_FREE {
                return Ok(None);
            }
            if ent[0] == ENT_DEL {
                lfn_len = 0;
                expect_cs = None;
                off = next;
                continue;
            }
            if ent[11] == ATTR_LFN {
                let cs = ent[13];
                if ent[0] & LFN_LAST != 0 {
                    lfn = [0u8; MAX_NAME];
                    lfn_len = 0;
                    expect_cs = Some(cs);
                } else if expect_cs.map(|c| c != cs).unwrap_or(true) {
                    lfn_len = 0;
                    expect_cs = None;
                    off = next;
                    continue;
                }
                let n = take_lfn(&ent, &mut lfn, lfn_len);
                if n > lfn_len {
                    lfn_len = n;
                }
                off = next;
                continue;
            }
            if ent[11] & ATTR_VOL != 0 {
                lfn_len = 0;
                expect_cs = None;
                off = next;
                continue;
            }
            let short_ok = match expect_cs {
                Some(cs) => {
                    let mut sn = [0u8; 11];
                    sn.copy_from_slice(&ent[..11]);
                    cs == lfn_checksum(&sn)
                }
                None => false,
            };
            let use_lfn = short_ok && lfn_len > 0;
            let name = if use_lfn { &lfn[..lfn_len] } else { &[] };
            let node = self.node_from_short(dir, off, &ent, name)?;
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
        if cb == 0 {
            return Err(FatError::Corrupt);
        }
        let idx = off / cb;
        let pin = (off % cb) as usize;
        let clu = match self.nth_clu(d, dir, idx)? {
            None => return Ok(false),
            Some(c) => c,
        };
        let mut clbuf = [0u8; MAX_CLUS_BYTES];
        let n = self.read_cluster(d, clu, &mut clbuf)?;
        if pin + ENT > n {
            return Err(FatError::Corrupt);
        }
        ent.copy_from_slice(&clbuf[pin..pin + ENT]);
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
        let idx = off / cb;
        let pin = (off % cb) as usize;
        let clu = self.nth_clu(d, dir, idx)?.ok_or(FatError::Corrupt)?;
        let mut clbuf = [0u8; MAX_CLUS_BYTES];
        let n = self.read_cluster(d, clu, &mut clbuf)?;
        if pin + ENT > n {
            return Err(FatError::Corrupt);
        }
        clbuf[pin..pin + ENT].copy_from_slice(ent);
        self.write_cluster(d, clu, &clbuf[..n])
    }

    pub(super) fn dir_reserve<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        slots: usize,
    ) -> Result<(u32, u32), FatError> {
        let need = slots * ENT;
        let cb = self.info.clus_bytes();
        let mut off = 0u32;
        let mut run = 0usize;
        let mut run_off = 0u32;
        loop {
            let mut ent = [0u8; ENT];
            match self.read_dir_raw(d, dir, off, &mut ent) {
                Ok(false) => break,
                Err(e) => return Err(e),
                Ok(true) => {
                    if ent[0] == ENT_FREE || ent[0] == ENT_DEL {
                        if run == 0 {
                            run_off = off;
                        }
                        run += 1;
                        if run >= slots {
                            return Ok((dir, run_off));
                        }
                    } else {
                        run = 0;
                    }
                    off += ENT as u32;
                    if ent[0] == ENT_FREE {
                        break;
                    }
                }
            }
        }
        if run >= slots {
            return Ok((dir, run_off));
        }
        // extend directory
        let last = self.last_clu(d, dir)?;
        let new = self.alloc_clu(d, last)?;
        self.zero_cluster(d, new)?;
        self.fat_set(d, last, new)?;
        self.fat_set(d, new, EOC_MIN)?;
        self.commit_fat(d)?;
        d.flush()?;
        let start = if run > 0 { run_off } else { off };
        let have = run * ENT;
        if have + cb < need {
            return Err(FatError::NoSpace);
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
            if off < ENT as u32 {
                break;
            }
            let prev = off - ENT as u32;
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
        let mut n = 1u32;
        while n < 1_000_000 {
            *out = stem;
            apply_tilde(out, n);
            if !self.short_taken(d, dir, out)? {
                return Ok(true);
            }
            n += 1;
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
                    off += ENT as u32;
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
        let mut buf = [0u8; MAX_CLUS_BYTES];
        let n = self.info.clus_bytes();
        fill_dot(&mut buf[0..ENT], b".          ", clu, self.now);
        let p = if parent == self.info.root_clus {
            0
        } else {
            parent
        };
        fill_dot(&mut buf[ENT..ENT * 2], b"..         ", p, self.now);
        self.write_cluster(d, clu, &buf[..n])
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
        put_le16(&mut ent, 20, (first >> 16) as u16);
        put_le16(&mut ent, 22, time);
        put_le16(&mut ent, 24, date);
        put_le16(&mut ent, 26, (first & 0xFFFF) as u16);
        put_le32(&mut ent, 28, size);
        self.write_dir_raw(d, dir, off, &ent)
    }
}

pub fn lfn_checksum(name: &[u8; 11]) -> u8 {
    let mut sum = 0u8;
    let mut i = 0usize;
    while i < 11 {
        sum = sum.rotate_right(1).wrapping_add(name[i]);
        i += 1;
    }
    sum
}

pub(super) fn eq_ci(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0usize;
    while i < a.len() {
        if to_upper(a[i]) != to_upper(b[i]) {
            return false;
        }
        i += 1;
    }
    true
}

fn to_upper(c: u8) -> u8 {
    if c.is_ascii_lowercase() {
        c - b'a' + b'A'
    } else {
        c
    }
}

pub(super) fn utf16_len(name: &[u8]) -> usize {
    name.len()
}

fn take_lfn(ent: &[u8; ENT], out: &mut [u8; MAX_NAME], _len: usize) -> usize {
    let ord = ent[0] & !LFN_LAST;
    if ord == 0 {
        return 0;
    }
    let base = ((ord as usize) - 1) * LFN_CHARS;
    let slots = [(1usize, 5usize), (14usize, 6usize), (28usize, 2usize)];
    let mut i = 0usize;
    let mut n = 0usize;
    while i < 3 {
        let (off, cnt) = slots[i];
        let mut k = 0usize;
        while k < cnt {
            let p = off + k * 2;
            let ch = le16(ent, p);
            if ch == 0 || ch == 0xFFFF {
                return if base + n > MAX_NAME {
                    MAX_NAME
                } else {
                    base + n
                };
            }
            let at = base + n;
            if at < MAX_NAME {
                out[at] = if ch < 0x80 { ch as u8 } else { b'?' };
            }
            n += 1;
            k += 1;
        }
        i += 1;
    }
    let total = base + n;
    if total > MAX_NAME { MAX_NAME } else { total }
}

pub(super) fn fill_lfn(ent: &mut [u8; ENT], ord: u8, last: bool, cs: u8, name: &[u8]) {
    ent.fill(0);
    ent[0] = if last { ord | LFN_LAST } else { ord };
    ent[11] = ATTR_LFN;
    ent[13] = cs;
    let start = (ord as usize - 1) * LFN_CHARS;
    let mut chars = [0xFFFFu16; LFN_CHARS];
    let mut i = 0usize;
    let mut term = false;
    while i < LFN_CHARS {
        let p = start + i;
        if term {
            chars[i] = 0xFFFF;
        } else if p < name.len() {
            chars[i] = name[p] as u16;
        } else {
            chars[i] = 0;
            term = true;
        }
        i += 1;
    }
    let mut k = 0usize;
    while k < 5 {
        put_le16(ent, 1 + k * 2, chars[k]);
        k += 1;
    }
    k = 0;
    while k < 6 {
        put_le16(ent, 14 + k * 2, chars[5 + k]);
        k += 1;
    }
    put_le16(ent, 28, chars[11]);
    put_le16(ent, 30, chars[12]);
}

pub(super) fn decode_short(ent: &[u8; ENT]) -> ([u8; MAX_NAME], u8) {
    let mut out = [0u8; MAX_NAME];
    let mut n = 0usize;
    let mut i = 0usize;
    let mut name0 = ent[0];
    if name0 == 0x05 {
        name0 = 0xE5;
    }
    let mut tmp = [0u8; 11];
    tmp.copy_from_slice(&ent[..11]);
    tmp[0] = name0;
    while i < 8 && tmp[i] != b' ' {
        out[n] = tmp[i];
        n += 1;
        i += 1;
    }
    if tmp[8] != b' ' {
        out[n] = b'.';
        n += 1;
        i = 8;
        while i < 11 && tmp[i] != b' ' {
            out[n] = tmp[i];
            n += 1;
            i += 1;
        }
    }
    (out, n as u8)
}

fn as_pure_83(name: &[u8]) -> Option<[u8; 11]> {
    let mut out = [b' '; 11];
    let mut dot = None;
    let mut i = 0usize;
    while i < name.len() {
        if name[i] == b'.' {
            if dot.is_some() {
                return None;
            }
            dot = Some(i);
        } else if !is_83_char(name[i]) || name[i].is_ascii_lowercase() {
            return None;
        }
        i += 1;
    }
    match dot {
        None => {
            if name.is_empty() || name.len() > 8 {
                return None;
            }
            out[..name.len()].copy_from_slice(name);
        }
        Some(0) => return None,
        Some(d) => {
            let stem = &name[..d];
            let ext = &name[d + 1..];
            if stem.is_empty() || stem.len() > 8 || ext.is_empty() || ext.len() > 3 {
                return None;
            }
            out[..stem.len()].copy_from_slice(stem);
            out[8..8 + ext.len()].copy_from_slice(ext);
        }
    }
    Some(out)
}

fn is_83_char(c: u8) -> bool {
    matches!(c, b'A'..=b'Z' | b'0'..=b'9' | b'!' | b'#' | b'$' | b'%' | b'&'
        | b'\'' | b'(' | b')' | b'-' | b'@' | b'^' | b'_' | b'`' | b'{' | b'}'
        | b'~')
}

fn make_lossy_83(name: &[u8], out: &mut [u8; 11]) {
    out.fill(b' ');
    let mut stem = [0u8; 8];
    let mut sn = 0usize;
    let mut ext = [0u8; 3];
    let mut en = 0usize;
    let mut in_ext = false;
    let mut i = 0usize;
    while i < name.len() {
        let mut c = to_upper(name[i]);
        if c == b'.' {
            if !in_ext && sn > 0 {
                in_ext = true;
            }
            i += 1;
            continue;
        }
        if !is_83_char(c) {
            c = b'_';
        }
        if in_ext {
            if en < 3 {
                ext[en] = c;
                en += 1;
            }
        } else if sn < 8 {
            stem[sn] = c;
            sn += 1;
        }
        i += 1;
    }
    if sn == 0 {
        stem[0] = b'_';
        sn = 1;
    }
    out[..sn].copy_from_slice(&stem[..sn]);
    if en > 0 {
        out[8..8 + en].copy_from_slice(&ext[..en]);
    }
}

fn apply_tilde(out: &mut [u8; 11], n: u32) {
    let mut digits = [0u8; 8];
    let mut dn = 0usize;
    let mut x = n;
    if x == 0 {
        x = 1;
    }
    while x > 0 && dn < 8 {
        digits[dn] = b'0' + (x % 10) as u8;
        dn += 1;
        x /= 10;
    }
    let need = dn + 1;
    let stem = 8usize.saturating_sub(need).max(1);
    out[stem] = b'~';
    let mut i = 0usize;
    while i < dn {
        out[stem + 1 + i] = digits[dn - 1 - i];
        i += 1;
    }
}

fn fill_dot(ent: &mut [u8], name11: &[u8], clu: u32, now: u32) {
    ent.fill(0);
    ent[..11].copy_from_slice(name11);
    ent[11] = ATTR_DIR;
    let (date, time) = fat_datetime(now);
    put_le16(ent, 14, time);
    put_le16(ent, 16, date);
    put_le16(ent, 18, date);
    put_le16(ent, 20, (clu >> 16) as u16);
    put_le16(ent, 22, time);
    put_le16(ent, 24, date);
    put_le16(ent, 26, (clu & 0xFFFF) as u16);
}

pub(super) fn fat_datetime(secs: u32) -> (u16, u16) {
    let s = (secs % 60) / 2;
    let mi = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let mut days = secs / 86400;
    let mut y = 0u16;
    loop {
        let ly = if y.is_multiple_of(4) { 366 } else { 365 };
        if days < ly {
            break;
        }
        days -= ly;
        y += 1;
        if y > 127 {
            y = 127;
            days = 0;
            break;
        }
    }
    let leap = y.is_multiple_of(4);
    let md = [
        31u32,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0u16;
    while m < 12 && days >= md[m as usize] {
        days -= md[m as usize];
        m += 1;
    }
    let date = (y << 9) | ((m + 1) << 5) | ((days as u16) + 1);
    let time = ((h as u16) << 11) | ((mi as u16) << 5) | (s as u16);
    (date, time)
}

pub(super) fn fat_to_unix(date: u16, time: u16) -> u64 {
    let y = ((date >> 9) & 0x7F) as u64;
    let m = ((date >> 5) & 0xF) as u64;
    let d = (date & 0x1F) as u64;
    let h = ((time >> 11) & 0x1F) as u64;
    let mi = ((time >> 5) & 0x3F) as u64;
    let s = (time & 0x1F) as u64 * 2;
    y * 365 * 86400
        + m.saturating_sub(1) * 30 * 86400
        + d.saturating_sub(1) * 86400
        + h * 3600
        + mi * 60
        + s
}
