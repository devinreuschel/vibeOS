use super::*;

pub const TMPFS_CACHE_PAGES: usize = 4;

pub const TMPFS_BACK_PAGES: usize = 16;

pub const TMPFS_BACK_BYTES: usize = PAGE * TMPFS_BACK_PAGES;

/// `tmp_back` as the cache's backend. A read of a page no extent holds
/// fails, so a readahead never caches a free run: [`tmp_alloc_run`] can
/// hand one out without invalidating it.
struct SliceBack<'a> {
    data: RefCell<&'a mut [u8]>,
    bits: u64,
}

impl Backend for SliceBack<'_> {
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let d = self.data.borrow();
        let o = offset as usize;
        if o.checked_add(buf.len())
            .map(|e| e > d.len())
            .unwrap_or(true)
        {
            return Err(BlockError::Inval);
        }
        let mut pg = o / PAGE;
        while pg * PAGE < o + buf.len() {
            if !bit_get(self.bits, pg) {
                return Err(BlockError::Inval);
            }
            pg += 1;
        }
        buf.copy_from_slice(&d[o..o + buf.len()]);
        Ok(())
    }

    fn write(&self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
        let mut d = self.data.borrow_mut();
        let o = offset as usize;
        if o.checked_add(buf.len())
            .map(|e| e > d.len())
            .unwrap_or(true)
        {
            return Err(BlockError::Inval);
        }
        d[o..o + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&self) -> Result<(), BlockError> {
        Ok(())
    }
}

impl<S: Guarded<KernState>> KernFs<S> {
    pub fn tmp_cache_stats(&self) -> CacheStats {
        self.with(|k| k.tmp_cache.stats)
    }
}

fn bit_get(bits: u64, i: usize) -> bool {
    i < 64 && (bits & (1u64 << i)) != 0
}

fn tmp_alloc_run(k: &mut KernState, n: usize) -> Result<u16, FsError> {
    if n == 0 {
        return Ok(0);
    }
    if n > TMPFS_BACK_PAGES {
        return Err(FsError::NoSpace);
    }
    let bits = k.tmp_bits;
    let mut i = 0usize;
    while i + n <= TMPFS_BACK_PAGES {
        let mut ok = true;
        let mut j = 0usize;
        while j < n {
            if bit_get(bits, i + j) {
                ok = false;
                break;
            }
            j += 1;
        }
        if ok {
            let mut b = bits;
            let mut j = 0usize;
            while j < n {
                b |= 1u64 << (i + j);
                j += 1;
            }
            k.tmp_bits = b;
            let start = i * PAGE;
            k.tmp_back[start..start + n * PAGE].fill(0);
            debug_assert!(
                (i..i + n).all(|p| k.tmp_cache.find(tmp_key(p)).is_none()),
                "a free tmpfs run is never cached"
            );
            return Ok(i as u16);
        }
        i += 1;
    }
    Err(FsError::NoSpace)
}

fn tmp_key(page: usize) -> CacheKey {
    CacheKey::page(TMPFS_DEV, (page * PAGE) as u64)
}

/// Free the run's pages and drop them from the cache without writeback:
/// only for backing that an unlink ([`tmp_free_extent`]) or a shrink
/// ([`tmp_truncate`]) frees.
fn tmp_free_run(k: &mut KernState, start: u16, n: u16) {
    tmp_invalidate_pages(k, start, n);
    tmp_release_bits(k, start, n);
}

/// Mark the run's pages free in the bitmap.
fn tmp_release_bits(k: &mut KernState, start: u16, n: u16) {
    if n == 0 {
        return;
    }
    let mut b = k.tmp_bits;
    let mut j = 0u16;
    while j < n {
        let i = start as usize + j as usize;
        if i < 64 {
            b &= !(1u64 << i);
        }
        j += 1;
    }
    k.tmp_bits = b;
}

fn tmp_invalidate_pages(k: &mut KernState, start: u16, n: u16) {
    let mut j = 0u16;
    while j < n {
        k.tmp_cache.invalidate(tmp_key(start as usize + j as usize));
        j += 1;
    }
}

pub(super) fn tmp_free_extent(k: &mut KernState, idx: usize) {
    let start = k.nodes[idx].extent_page;
    let n = k.nodes[idx].extent_pages;
    tmp_free_run(k, start, n);
    k.nodes[idx].extent_page = 0;
    k.nodes[idx].extent_pages = 0;
}

fn tmp_pages_for(size: u64) -> usize {
    if size == 0 {
        0
    } else {
        (size as usize).div_ceil(PAGE)
    }
}

fn tmp_ensure(k: &mut KernState, inst: u32, ino: u32, new_size: u64) -> Result<(), FsError> {
    let Some(idx) = kern_idx(k, ino) else {
        return Err(FsError::NotFound);
    };
    if !k.nodes[idx].used || k.nodes[idx].inst != inst {
        return Err(FsError::NotFound);
    }
    let need = tmp_pages_for(new_size);
    let have = k.nodes[idx].extent_pages as usize;
    if need <= have {
        return Ok(());
    }
    if have == 0 {
        let p = tmp_alloc_run(k, need)?;
        k.nodes[idx].extent_page = p;
        k.nodes[idx].extent_pages = need as u16;
        return Ok(());
    }
    let start = k.nodes[idx].extent_page as usize;
    let extra = need - have;
    let mut can = true;
    let mut j = 0usize;
    while j < extra {
        if start + have + j >= TMPFS_BACK_PAGES || bit_get(k.tmp_bits, start + have + j) {
            can = false;
            break;
        }
        j += 1;
    }
    if can {
        let mut b = k.tmp_bits;
        let mut j = 0usize;
        while j < extra {
            b |= 1u64 << (start + have + j);
            j += 1;
        }
        k.tmp_bits = b;
        let off = (start + have) * PAGE;
        k.tmp_back[off..off + extra * PAGE].fill(0);
        debug_assert!(
            (start + have..start + need).all(|p| k.tmp_cache.find(tmp_key(p)).is_none()),
            "a free tmpfs run is never cached"
        );
        k.nodes[idx].extent_pages = need as u16;
        return Ok(());
    }
    // Move: the old run's dirty cache pages go back to `tmp_back` and
    // leave the cache before the copy, so the copy carries every write.
    let newp = tmp_alloc_run(k, need)?;
    let old = k.nodes[idx].extent_page as usize;
    let oldn = have;
    let dst = newp as usize * PAGE;
    let src = old * PAGE;
    let nbytes = oldn * PAGE;
    let back = SliceBack {
        data: RefCell::new(&mut k.tmp_back[..]),
        bits: k.tmp_bits,
    };
    let ev = cache::cached_evict_range(
        &mut k.tmp_cache,
        &back,
        TMPFS_DEV,
        src as u64,
        nbytes as u64,
    );
    if ev.is_err() {
        tmp_release_bits(k, newp, need as u16);
        return Err(FsError::Io);
    }
    k.tmp_back.copy_within(src..src + nbytes, dst);
    tmp_release_bits(k, old as u16, oldn as u16);
    k.nodes[idx].extent_page = newp;
    k.nodes[idx].extent_pages = need as u16;
    Ok(())
}

fn tmp_byte_off(k: &KernState, inst: u32, ino: u32, logical: u64) -> Result<u64, FsError> {
    let n = kern_get(k, inst, ino).ok_or(FsError::NotFound)?;
    if n.extent_pages == 0 {
        return Err(FsError::Io);
    }
    Ok((n.extent_page as u64) * PAGE as u64 + logical)
}

fn tmp_rw_cache(
    k: &mut KernState,
    byte_off: u64,
    buf: &mut [u8],
    write: bool,
    src: &[u8],
) -> Result<(), FsError> {
    let cache = &mut k.tmp_cache;
    let scratch = &mut k.tmp_page;
    let back = SliceBack {
        data: RefCell::new(&mut k.tmp_back[..]),
        bits: k.tmp_bits,
    };
    let r = if write {
        cache::cached_write(cache, &back, TMPFS_DEV, byte_off, src, scratch)
    } else {
        cache::cached_read(cache, &back, TMPFS_DEV, byte_off, buf, scratch)
    };
    r.map_err(|_| FsError::Io)
}

pub(super) fn tmp_read(
    k: &mut KernState,
    inst: u32,
    ino: u32,
    off: u64,
    buf: &mut [u8],
) -> Result<usize, FsError> {
    let size = kern_get(k, inst, ino).ok_or(FsError::NotFound)?.size;
    if off >= size {
        return Ok(0);
    }
    let avail = (size - off) as usize;
    let n = avail.min(buf.len());
    if n == 0 {
        return Ok(0);
    }
    let byte_off = tmp_byte_off(k, inst, ino, off)?;
    tmp_rw_cache(k, byte_off, &mut buf[..n], false, b"")?;
    Ok(n)
}

pub(super) fn tmp_write(
    k: &mut KernState,
    now: u64,
    inst: u32,
    ino: u32,
    off: u64,
    buf: &[u8],
) -> Result<usize, FsError> {
    if buf.is_empty() {
        return Ok(0);
    }
    let end = off.saturating_add(buf.len() as u64);
    tmp_ensure(k, inst, ino, end)?;
    let byte_off = tmp_byte_off(k, inst, ino, off)?;
    let mut dummy = [0u8; 1];
    tmp_rw_cache(k, byte_off, &mut dummy, true, buf)?;
    let t = now;
    if let Some(n) = kern_get_mut(k, inst, ino) {
        if end > n.size {
            n.size = end;
        }
        n.mtime = t;
        n.ctime = t;
    }
    Ok(buf.len())
}

/// A page of zeros, for [`tmp_truncate`]'s tail.
static ZERO_PAGE: [u8; PAGE] = [0; PAGE];

pub(super) fn tmp_truncate(
    k: &mut KernState,
    now: u64,
    inst: u32,
    ino: u32,
    size: u64,
) -> Result<(), FsError> {
    let Some(idx) = kern_idx(k, ino) else {
        return Err(FsError::NotFound);
    };
    if !k.nodes[idx].used || k.nodes[idx].inst != inst {
        return Err(FsError::NotFound);
    }
    let old = k.nodes[idx].size;
    if size > old {
        tmp_ensure(k, inst, ino, size)?;
    } else {
        let need = tmp_pages_for(size) as u16;
        let have = k.nodes[idx].extent_pages;
        if need < have {
            let start = k.nodes[idx].extent_page;
            tmp_free_run(k, start + need, have - need);
            k.nodes[idx].extent_pages = need;
            if need == 0 {
                k.nodes[idx].extent_page = 0;
            }
        }
        // Past the size, a kept page holds zeros, as a fresh one does
        // (`tmp_alloc_run`), so the file reads zeros there when it grows
        // again, not the bytes this truncate cut.
        let kept = (need as u64).saturating_mul(PAGE as u64);
        let end = old.min(kept);
        if size < end {
            let len = (end - size) as usize;
            let byte_off = tmp_byte_off(k, inst, ino, size)?;
            let mut dummy = [0u8; 1];
            tmp_rw_cache(k, byte_off, &mut dummy, true, &ZERO_PAGE[..len])?;
        }
    }
    let t = now;
    k.nodes[idx].size = size;
    k.nodes[idx].mtime = t;
    k.nodes[idx].ctime = t;
    Ok(())
}
