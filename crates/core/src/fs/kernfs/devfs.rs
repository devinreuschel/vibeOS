use super::*;
use crate::kalloc::TryVec;

impl<S: Guarded<KernState>> KernFs<S> {
    /// Copy the last console write into `out`; the count copied.
    pub fn cons_captured(&self, out: &mut [u8]) -> usize {
        self.with(|k| {
            let n = (k.cons_len as usize).min(out.len());
            out[..n].copy_from_slice(&k.cons_out[..n]);
            n
        })
    }

    /// Add a block node for `dev` under the mounted devfs, named as the
    /// device is and sized `capacity × block size` (`Inval` when that
    /// overflows). The store keeps a clone of the handle; a second call for
    /// the same device or name returns the node it already has.
    pub fn devfs_add_block(&self, dev: &BlockRef) -> Result<u32, FsError> {
        let size = blk_size(dev)?;
        let name = dev.name();
        self.with(|k| {
            let (inst, root) = k.skin(FsType::Dev).ok_or(FsError::Io)?;
            if let Some(ino) = kern_find_child(k, inst, root, name.as_bytes()) {
                return Ok(ino);
            }
            let slot = match k
                .blk
                .iter()
                .position(|b| b.as_ref().is_some_and(|b| b.same(dev)))
            {
                Some(i) => i,
                None => {
                    let i = k
                        .blk
                        .iter()
                        .position(Option::is_none)
                        .ok_or(FsError::NoSpace)?;
                    // A clone: the caller's handle keeps the device, so
                    // no drop of it here can be the last.
                    *k.blk.get_mut(i).ok_or(FsError::NoSpace)? = Some(dev.clone());
                    i
                }
            };
            let now = k.now;
            let ino = kern_mk_special(
                k,
                now,
                inst,
                root,
                name.as_bytes(),
                KernKind::Block,
                slot as u64,
            )?;
            if let Some(n) = kern_get_mut(k, inst, ino) {
                n.size = size;
            }
            Ok(ino)
        })
    }
}

/// A block device's size in bytes. `Inval` when it overflows `u64`.
fn blk_size(dev: &BlockRef) -> Result<u64, FsError> {
    let cap = dev.capacity_sectors().map_err(blk_err)?;
    let bs = dev.logical_block_size().map_err(blk_err)?;
    cap.checked_mul(u64::from(bs)).ok_or(FsError::Inval)
}

/// `Inval` stays `Inval`; every other block error, `Gone` included, is
/// `Io`.
fn blk_err(e: BlockError) -> FsError {
    match e {
        BlockError::Inval => FsError::Inval,
        _ => FsError::Io,
    }
}

/// A one-block bounce buffer for an unaligned head or tail.
fn bounce(bs: usize) -> Result<TryVec<u8>, FsError> {
    let mut v = TryVec::try_with_capacity(bs).map_err(|_| FsError::NoMem)?;
    while v.len() < bs {
        v.try_push(0).map_err(|_| FsError::NoMem)?;
    }
    Ok(v)
}

/// Read `buf` from block device `dev` at byte `off`: `Ok(0)` at or past
/// the end, and a read that crosses it stops there. Whole blocks go
/// straight to the device; an unaligned head or tail goes through a
/// one-block bounce buffer. Called with no lock held.
pub(super) fn blk_read(dev: &BlockRef, off: u64, buf: &mut [u8]) -> Result<usize, FsError> {
    let size = blk_size(dev)?;
    let bs = dev.logical_block_size().map_err(blk_err)?;
    let bs64 = u64::from(bs);
    let bsz = usize::try_from(bs).map_err(|_| FsError::Inval)?;
    if off >= size || buf.is_empty() || bs == 0 {
        return Ok(0);
    }
    let left = usize::try_from(size - off).unwrap_or(usize::MAX);
    let n = buf.len().min(left);
    let mut tmp: Option<TryVec<u8>> = None;
    let mut done = 0usize;
    while done < n {
        let pos = off + done as u64;
        let lba = pos / bs64;
        let inb = (pos % bs64) as usize;
        let want = n - done;
        if inb == 0 && want >= bsz {
            let len = want - want % bsz;
            dev.read(lba, &mut buf[done..done + len]).map_err(blk_err)?;
            done += len;
        } else {
            if tmp.is_none() {
                tmp = Some(bounce(bsz)?);
            }
            let b = tmp.as_mut().ok_or(FsError::NoMem)?;
            dev.read(lba, b).map_err(blk_err)?;
            let take = (bsz - inb).min(want);
            buf[done..done + take].copy_from_slice(&b[inb..inb + take]);
            done += take;
        }
    }
    Ok(n)
}

/// Write `buf` to block device `dev` at byte `off`: `NoSpace` at or past
/// the end (Linux's `ENOSPC`), and a write that crosses it stops there.
/// A partial block is read, changed and written back. Called with no lock
/// held.
pub(super) fn blk_write(dev: &BlockRef, off: u64, buf: &[u8]) -> Result<usize, FsError> {
    let size = blk_size(dev)?;
    let bs = dev.logical_block_size().map_err(blk_err)?;
    let bs64 = u64::from(bs);
    let bsz = usize::try_from(bs).map_err(|_| FsError::Inval)?;
    if buf.is_empty() {
        return Ok(0);
    }
    if off >= size || bs == 0 {
        return Err(FsError::NoSpace);
    }
    let left = usize::try_from(size - off).unwrap_or(usize::MAX);
    let n = buf.len().min(left);
    let mut tmp: Option<TryVec<u8>> = None;
    let mut done = 0usize;
    while done < n {
        let pos = off + done as u64;
        let lba = pos / bs64;
        let inb = (pos % bs64) as usize;
        let want = n - done;
        if inb == 0 && want >= bsz {
            let len = want - want % bsz;
            dev.write(lba, &buf[done..done + len]).map_err(blk_err)?;
            done += len;
        } else {
            if tmp.is_none() {
                tmp = Some(bounce(bsz)?);
            }
            let b = tmp.as_mut().ok_or(FsError::NoMem)?;
            dev.read(lba, b).map_err(blk_err)?;
            let take = (bsz - inb).min(want);
            b[inb..inb + take].copy_from_slice(&buf[done..done + take]);
            dev.write(lba, b).map_err(blk_err)?;
            done += take;
        }
    }
    Ok(n)
}

pub(super) fn mix_rng(k: &mut KernState) -> u64 {
    let mut x = k.rng;
    if x == 0 {
        x = k.now ^ 0x9E37_79B9_7F4A_7C15;
        if x == 0 {
            x = 1;
        }
    }
    // xorshift64. Not a CSPRNG.
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    k.rng = x;
    x
}
