use super::*;

pub struct MemDisk<'a> {
    data: &'a mut [u8],
}

impl<'a> MemDisk<'a> {
    pub fn new(data: &'a mut [u8]) -> Result<Self, Error> {
        if data.len() < MIN_BLOCKS as usize * BLOCK || !data.len().is_multiple_of(BLOCK) {
            return Err(Error::Inval);
        }
        if data.len() / BLOCK > MAX_BLOCKS {
            return Err(Error::Inval);
        }
        Ok(Self { data })
    }

    pub fn bytes(&self) -> &[u8] {
        self.data
    }
}

impl Disk for MemDisk<'_> {
    fn nblocks(&self) -> u32 {
        (self.data.len() / BLOCK) as u32
    }

    fn read_block(&mut self, bno: u32, buf: &mut [u8; BLOCK]) -> Result<(), Error> {
        let off = (bno as usize).checked_mul(BLOCK).ok_or(Error::Inval)?;
        let end = off.checked_add(BLOCK).ok_or(Error::Inval)?;
        if end > self.data.len() {
            return Err(Error::Io);
        }
        buf.copy_from_slice(&self.data[off..end]);
        Ok(())
    }

    fn write_block(&mut self, bno: u32, buf: &[u8; BLOCK]) -> Result<(), Error> {
        let off = (bno as usize).checked_mul(BLOCK).ok_or(Error::Inval)?;
        let end = off.checked_add(BLOCK).ok_or(Error::Inval)?;
        if end > self.data.len() {
            return Err(Error::Io);
        }
        self.data[off..end].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// Power-loss stand-in for host tests. `new` drops every device op (write or
/// flush) after `limit`, an in-order suffix. `seeded` models a volatile write
/// cache: writes since the last flush stay pending, and at the crash a seeded
/// subset of them reaches the medium, so a later write can survive where an
/// earlier one did not (F080).
#[cfg(test)]
pub struct CrashDisk<'a> {
    data: &'a mut [u8],
    pub ops: u64,
    pub limit: u64,
    pub dropped: u64,
    /// `Some` for `seeded`: the writes since the last flush, oldest first.
    pending: Option<Vec<(u32, [u8; BLOCK])>>,
    seed: u64,
    pub crashed: bool,
}

#[cfg(test)]
impl<'a> CrashDisk<'a> {
    pub fn new(data: &'a mut [u8], limit: u64) -> Result<Self, Error> {
        if data.len() < MIN_BLOCKS as usize * BLOCK || !data.len().is_multiple_of(BLOCK) {
            return Err(Error::Inval);
        }
        Ok(Self {
            data,
            ops: 0,
            limit,
            dropped: 0,
            pending: None,
            seed: 0,
            crashed: false,
        })
    }

    /// A flush at op `limit` or earlier applies the pending writes in order.
    /// Past `limit`, and in `crash`, each pending write is kept with
    /// probability 1/2 (xorshift from `seed`), and every later op is dropped.
    pub fn seeded(data: &'a mut [u8], limit: u64, seed: u64) -> Result<Self, Error> {
        let mut c = Self::new(data, limit)?;
        c.pending = Some(Vec::new());
        c.seed = seed | 1;
        Ok(c)
    }

    /// Power loss now. Idempotent.
    pub fn crash(&mut self) {
        if self.crashed {
            return;
        }
        self.crashed = true;
        let pend = self.pending.take().unwrap_or_default();
        for (bno, buf) in &pend {
            self.seed ^= self.seed << 13;
            self.seed ^= self.seed >> 7;
            self.seed ^= self.seed << 17;
            if self.seed & 1 == 1
                && let Ok(r) = self.range(*bno)
            {
                self.data[r].copy_from_slice(buf);
            } else {
                self.dropped = self.dropped.saturating_add(1);
            }
        }
        self.pending = Some(Vec::new());
    }

    fn range(&self, bno: u32) -> Result<core::ops::Range<usize>, Error> {
        let off = (bno as usize).checked_mul(BLOCK).ok_or(Error::Inval)?;
        let end = off.checked_add(BLOCK).ok_or(Error::Inval)?;
        if end > self.data.len() {
            return Err(Error::Io);
        }
        Ok(off..end)
    }

    fn store(&mut self, bno: u32, buf: &[u8; BLOCK]) -> Result<(), Error> {
        let r = self.range(bno)?;
        self.data[r].copy_from_slice(buf);
        Ok(())
    }

    /// Counts one op; false when it is past the crash and must be dropped.
    fn tick(&mut self) -> bool {
        self.ops = self.ops.saturating_add(1);
        if self.ops > self.limit {
            if self.pending.is_some() {
                self.crash();
            }
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        !self.crashed
    }
}

#[cfg(test)]
impl Disk for CrashDisk<'_> {
    fn nblocks(&self) -> u32 {
        (self.data.len() / BLOCK) as u32
    }

    fn read_block(&mut self, bno: u32, buf: &mut [u8; BLOCK]) -> Result<(), Error> {
        let r = self.range(bno)?;
        if let Some(p) = self.pending.as_ref()
            && let Some((_, b)) = p.iter().rev().find(|(n, _)| *n == bno)
        {
            buf.copy_from_slice(b);
            return Ok(());
        }
        buf.copy_from_slice(&self.data[r]);
        Ok(())
    }

    fn write_block(&mut self, bno: u32, buf: &[u8; BLOCK]) -> Result<(), Error> {
        if !self.tick() {
            return Ok(());
        }
        self.range(bno)?;
        match self.pending.as_mut() {
            Some(p) => {
                p.push((bno, *buf));
                Ok(())
            }
            None => self.store(bno, buf),
        }
    }

    fn flush(&mut self) -> Result<(), Error> {
        if !self.tick() {
            return Ok(());
        }
        if let Some(p) = self.pending.take() {
            for (bno, buf) in &p {
                self.store(*bno, buf)?;
            }
            self.pending = Some(Vec::new());
        }
        Ok(())
    }
}
