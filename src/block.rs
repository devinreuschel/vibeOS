//! Block layer: devices, requests, merge, elevator. ROADMAP §7.1.
//!
//! Portable. Kernel `block_init` owns ramdisk memory, completions, and
//! the workqueue pump. Host tests drive [`Queue`] and [`Ramdisk`] over
//! plain buffers.

/// Barrier vs flush (journaled FS depends on this):
///
/// - [`Op::Barrier`] is an order fence in the request stream. Every
///   request submitted before it completes before any request submitted
///   after it starts. It does **not** push volatile cache to media.
/// - [`Op::Flush`] is a barrier plus a device durable-write. Completes
///   only after prior writes are as durable as the device can make them.
///   Ramdisk flush is a successful no-op (memory is the media).
///
/// Completions: the queue lock is never held across `BlockDevice` I/O
/// or a waiter wake. A later virtio-blk threaded IRQ (DESIGN §2.2 / §5.4)
/// can call the same complete path. Hard IRQ only enqueues work.
use crate::fmt_util;

pub const DEFAULT_BLOCK_SIZE: u32 = 512;
pub const DEFAULT_RETRY_BUDGET: u8 = 3;
pub const MAX_QUEUE: usize = 32;
pub const MAX_SEGS: usize = 8;
pub const MAX_BLOCKDEVS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockError {
    Inval,
    Io,
    Failed,
    QueueFull,
}

impl BlockError {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockError::Inval => "inval",
            BlockError::Io => "io",
            BlockError::Failed => "failed",
            BlockError::QueueFull => "queue full",
        }
    }

    pub fn retryable(self) -> bool {
        matches!(self, BlockError::Io)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceState {
    Ready,
    Failed,
}

impl DeviceState {
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceState::Ready => "ready",
            DeviceState::Failed => "failed",
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => DeviceState::Failed,
            _ => DeviceState::Ready,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            DeviceState::Ready => 0,
            DeviceState::Failed => 1,
        }
    }
}

/// See the module docs for barrier vs flush.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Read,
    Write,
    Flush,
    Discard,
    Barrier,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Write => "write",
            Op::Flush => "flush",
            Op::Discard => "discard",
            Op::Barrier => "barrier",
        }
    }

    pub fn can_merge(self) -> bool {
        matches!(self, Op::Read | Op::Write | Op::Discard)
    }

    pub fn is_fence(self) -> bool {
        matches!(self, Op::Flush | Op::Barrier)
    }

    pub fn needs_buf(self) -> bool {
        matches!(self, Op::Read | Op::Write)
    }
}

/// Backend a filesystem (and the request queue) talks to.
///
/// Logical block size is per device. Do not assume 512; virtio-blk may
/// advertise 4K. `capacity_sectors` is in those logical blocks.
///
/// Methods may block and allocate. Do not call them from hard IRQ.
pub trait BlockDevice: Send + Sync {
    fn name(&self) -> &'static str;
    fn logical_block_size(&self) -> u32;
    fn capacity_sectors(&self) -> u64;
    fn state(&self) -> DeviceState {
        DeviceState::Ready
    }
    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError>;
    fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError>;
    fn flush(&self) -> Result<(), BlockError>;
    fn discard(&self, lba: u64, nsectors: u64) -> Result<(), BlockError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bio {
    pub op: Op,
    pub lba: u64,
    pub nsect: u32,
}

impl Bio {
    pub const fn new(op: Op, lba: u64, nsect: u32) -> Self {
        Self { op, lba, nsect }
    }

    pub fn end_lba(self) -> u64 {
        self.lba.saturating_add(self.nsect as u64)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seg {
    pub ptr: usize,
    pub len: usize,
}

impl Seg {
    pub const EMPTY: Self = Self { ptr: 0, len: 0 };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub bio: Bio,
    pub segs: [Seg; MAX_SEGS],
    pub nseg: u8,
    pub retries_left: u8,
    pub seq: u32,
    /// Caller cookies (kernel: `IoWaiter` pointers). 0 = none.
    pub waiters: [usize; MAX_SEGS],
    pub nwait: u8,
}

impl Request {
    pub fn new(op: Op, lba: u64, nsect: u32) -> Self {
        Self {
            bio: Bio::new(op, lba, nsect),
            segs: [Seg::EMPTY; MAX_SEGS],
            nseg: 0,
            retries_left: DEFAULT_RETRY_BUDGET,
            seq: 0,
            waiters: [0; MAX_SEGS],
            nwait: 0,
        }
    }

    pub fn with_seg(mut self, ptr: usize, len: usize) -> Self {
        if (self.nseg as usize) < MAX_SEGS {
            self.segs[self.nseg as usize] = Seg { ptr, len };
            self.nseg += 1;
        }
        self
    }

    pub fn with_waiter(mut self, cookie: usize) -> Self {
        if cookie != 0 && (self.nwait as usize) < MAX_SEGS {
            self.waiters[self.nwait as usize] = cookie;
            self.nwait += 1;
        }
        self
    }

    pub fn end_lba(self) -> u64 {
        self.bio.end_lba()
    }

    pub fn bytes(self) -> usize {
        let mut n = 0usize;
        let mut i = 0u8;
        while i < self.nseg {
            n = n.saturating_add(self.segs[i as usize].len);
            i += 1;
        }
        n
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MergeKind {
    None,
    Back,
    Front,
}

fn merge_kind(a: &Request, b: &Request) -> MergeKind {
    if a.bio.op != b.bio.op || !a.bio.op.can_merge() {
        return MergeKind::None;
    }
    if a.end_lba() == b.bio.lba {
        MergeKind::Back
    } else if b.end_lba() == a.bio.lba {
        MergeKind::Front
    } else {
        MergeKind::None
    }
}

fn merge_into(dst: &mut Request, src: Request, kind: MergeKind) -> bool {
    match kind {
        MergeKind::None => false,
        MergeKind::Back | MergeKind::Front => {
            if (dst.nseg as usize) + (src.nseg as usize) > MAX_SEGS {
                return false;
            }
            if (dst.nwait as usize) + (src.nwait as usize) > MAX_SEGS {
                return false;
            }
            let nsect = dst.bio.nsect.checked_add(src.bio.nsect);
            let Some(nsect) = nsect else {
                return false;
            };
            if matches!(kind, MergeKind::Front) {
                let mut segs = [Seg::EMPTY; MAX_SEGS];
                let mut i = 0u8;
                while i < src.nseg {
                    segs[i as usize] = src.segs[i as usize];
                    i += 1;
                }
                let mut j = 0u8;
                while j < dst.nseg {
                    segs[i as usize] = dst.segs[j as usize];
                    i += 1;
                    j += 1;
                }
                dst.segs = segs;
                dst.nseg = i;
                dst.bio.lba = src.bio.lba;
                let mut waits = [0usize; MAX_SEGS];
                i = 0;
                while i < src.nwait {
                    waits[i as usize] = src.waiters[i as usize];
                    i += 1;
                }
                j = 0;
                while j < dst.nwait {
                    waits[i as usize] = dst.waiters[j as usize];
                    i += 1;
                    j += 1;
                }
                dst.waiters = waits;
                dst.nwait = i;
            } else {
                let mut i = 0u8;
                while i < src.nseg {
                    dst.segs[dst.nseg as usize] = src.segs[i as usize];
                    dst.nseg += 1;
                    i += 1;
                }
                i = 0;
                while i < src.nwait {
                    dst.waiters[dst.nwait as usize] = src.waiters[i as usize];
                    dst.nwait += 1;
                    i += 1;
                }
            }
            dst.bio.nsect = nsect;
            if src.seq < dst.seq {
                dst.seq = src.seq;
            }
            if src.retries_left < dst.retries_left {
                dst.retries_left = src.retries_left;
            }
            true
        }
    }
}

/// Per-device pending list. C-LOOK elevator (one-way, wrap to lowest LBA).
/// Fences ([`Op::Barrier`] / [`Op::Flush`]) hold back later seq numbers.
pub struct Queue {
    slots: [Option<Request>; MAX_QUEUE],
    n: usize,
    next_seq: u32,
    last_lba: u64,
    pub running: bool,
    pub failed: bool,
}

impl Queue {
    pub const fn new() -> Self {
        Self {
            slots: [None; MAX_QUEUE],
            n: 0,
            next_seq: 0,
            last_lba: 0,
            running: false,
            failed: false,
        }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    fn first_fence_seq(&self) -> u32 {
        let mut fence = u32::MAX;
        let mut i = 0usize;
        while i < MAX_QUEUE {
            if let Some(r) = self.slots[i]
                && r.bio.op.is_fence()
                && r.seq < fence
            {
                fence = r.seq;
            }
            i += 1;
        }
        fence
    }

    fn try_merge(&mut self, req: &Request) -> bool {
        if !req.bio.op.can_merge() {
            return false;
        }
        let fence = self.first_fence_seq();
        let mut i = 0usize;
        while i < MAX_QUEUE {
            let slot = match self.slots[i] {
                Some(s) => s,
                None => {
                    i += 1;
                    continue;
                }
            };
            let after_fence = fence == u32::MAX || slot.seq > fence;
            if after_fence {
                let kind = merge_kind(&slot, req);
                if !matches!(kind, MergeKind::None)
                    && let Some(dst) = self.slots[i].as_mut()
                    && merge_into(dst, *req, kind)
                {
                    return true;
                }
            }
            i += 1;
        }
        false
    }

    fn insert_slot(&mut self, req: Request) -> Result<(), BlockError> {
        let mut i = 0usize;
        while i < MAX_QUEUE {
            if self.slots[i].is_none() {
                self.slots[i] = Some(req);
                self.n += 1;
                return Ok(());
            }
            i += 1;
        }
        Err(BlockError::QueueFull)
    }

    /// Merge if possible, else enqueue. Assigns `seq`.
    pub fn submit(&mut self, mut req: Request) -> Result<(), BlockError> {
        if self.failed {
            return Err(BlockError::Failed);
        }
        if self.n >= MAX_QUEUE {
            return Err(BlockError::QueueFull);
        }
        req.seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        if self.try_merge(&req) {
            return Ok(());
        }
        self.insert_slot(req)
    }

    /// Put a failed I/O back without a new seq (stay on this side of a fence).
    pub fn requeue(&mut self, req: Request) -> Result<(), BlockError> {
        if self.failed {
            return Err(BlockError::Failed);
        }
        if self.n >= MAX_QUEUE {
            return Err(BlockError::QueueFull);
        }
        self.insert_slot(req)
    }

    fn take(&mut self, i: usize) -> Request {
        let r = self.slots[i].take().expect("empty slot");
        self.n -= 1;
        r
    }

    /// Next dispatchable request. None if idle.
    pub fn pick(&mut self) -> Option<Request> {
        if self.n == 0 {
            return None;
        }
        let fence = self.first_fence_seq();
        let mut best_fwd: Option<(u64, usize)> = None;
        let mut best_wrap: Option<(u64, usize)> = None;
        let mut fence_i: Option<usize> = None;
        let mut i = 0usize;
        while i < MAX_QUEUE {
            if let Some(r) = self.slots[i]
                && r.seq <= fence
            {
                if r.bio.op.is_fence() {
                    if r.seq == fence {
                        fence_i = Some(i);
                    }
                } else {
                    let lba = r.bio.lba;
                    if lba >= self.last_lba {
                        match best_fwd {
                            Some((b, _)) if lba >= b => {}
                            _ => best_fwd = Some((lba, i)),
                        }
                    } else {
                        match best_wrap {
                            Some((b, _)) if lba >= b => {}
                            _ => best_wrap = Some((lba, i)),
                        }
                    }
                }
            }
            i += 1;
        }
        let idx = if let Some((_, i)) = best_fwd {
            i
        } else if let Some((_, i)) = best_wrap {
            i
        } else {
            fence_i?
        };
        let r = self.take(idx);
        self.last_lba = r.end_lba();
        Some(r)
    }

    /// Drain everything. Caller completes waiters after dropping the lock.
    pub fn drain(&mut self, out: &mut [Option<Request>; MAX_QUEUE]) -> usize {
        let mut n = 0usize;
        let mut i = 0usize;
        while i < MAX_QUEUE {
            if let Some(r) = self.slots[i].take()
                && n < MAX_QUEUE
            {
                out[n] = Some(r);
                n += 1;
            }
            i += 1;
        }
        self.n = 0;
        n
    }

    pub fn fail(&mut self) {
        self.failed = true;
    }
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

/// Geometry + copy over a caller-owned buffer. Discard is a range check
/// and a no-op (must not panic). Flush is a successful no-op.
#[derive(Clone, Copy, Debug)]
pub struct Ramdisk {
    name: &'static str,
    block_size: u32,
    nsectors: u64,
}

impl Ramdisk {
    pub fn new(name: &'static str, block_size: u32, nsectors: u64) -> Result<Self, BlockError> {
        if name.is_empty() || block_size == 0 || nsectors == 0 {
            return Err(BlockError::Inval);
        }
        if (nsectors as u128).checked_mul(block_size as u128).is_none() {
            return Err(BlockError::Inval);
        }
        Ok(Self {
            name,
            block_size,
            nsectors,
        })
    }

    pub fn name(self) -> &'static str {
        self.name
    }

    pub fn logical_block_size(self) -> u32 {
        self.block_size
    }

    pub fn capacity_sectors(self) -> u64 {
        self.nsectors
    }

    pub fn byte_len(self) -> usize {
        (self.nsectors as usize).saturating_mul(self.block_size as usize)
    }

    fn check_buf(self, lba: u64, buf: &[u8]) -> Result<(usize, u32), BlockError> {
        let bs = self.block_size as usize;
        if bs == 0 || !buf.len().is_multiple_of(bs) {
            return Err(BlockError::Inval);
        }
        let nsect = (buf.len() / bs) as u32;
        self.check_range(lba, nsect as u64)?;
        let off = (lba as usize).checked_mul(bs).ok_or(BlockError::Inval)?;
        Ok((off, nsect))
    }

    fn check_range(self, lba: u64, nsect: u64) -> Result<(), BlockError> {
        let end = lba.checked_add(nsect).ok_or(BlockError::Inval)?;
        if end > self.nsectors {
            Err(BlockError::Inval)
        } else {
            Ok(())
        }
    }

    pub fn read(self, data: &[u8], lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let (off, _) = self.check_buf(lba, buf)?;
        if data.len() < off + buf.len() {
            return Err(BlockError::Inval);
        }
        buf.copy_from_slice(&data[off..off + buf.len()]);
        Ok(())
    }

    pub fn write(self, data: &mut [u8], lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        let (off, _) = self.check_buf(lba, buf)?;
        if data.len() < off + buf.len() {
            return Err(BlockError::Inval);
        }
        data[off..off + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    pub fn flush(self) -> Result<(), BlockError> {
        Ok(())
    }

    /// No-op on ramdisk. Still rejects a range past the end.
    pub fn discard(self, lba: u64, nsectors: u64) -> Result<(), BlockError> {
        self.check_range(lba, nsectors)
    }

    /// Run one queued request against `data`. Seg pointers must be valid
    /// for `Read`/`Write`.
    pub fn apply(self, data: &mut [u8], req: &Request) -> Result<(), BlockError> {
        match req.bio.op {
            Op::Flush => self.flush(),
            Op::Barrier => Ok(()),
            Op::Discard => self.discard(req.bio.lba, req.bio.nsect as u64),
            Op::Read | Op::Write => {
                let bs = self.block_size as usize;
                if req.bytes() != (req.bio.nsect as usize).saturating_mul(bs) {
                    return Err(BlockError::Inval);
                }
                let mut lba = req.bio.lba;
                let mut i = 0u8;
                while i < req.nseg {
                    let s = req.segs[i as usize];
                    if s.len == 0 || !s.len.is_multiple_of(bs) {
                        return Err(BlockError::Inval);
                    }
                    if req.bio.op == Op::Read {
                        let buf =
                            unsafe { core::slice::from_raw_parts_mut(s.ptr as *mut u8, s.len) };
                        self.read(data, lba, buf)?;
                    } else {
                        let buf = unsafe { core::slice::from_raw_parts(s.ptr as *const u8, s.len) };
                        self.write(data, lba, buf)?;
                    }
                    lba = lba.saturating_add((s.len / bs) as u64);
                    i += 1;
                }
                Ok(())
            }
        }
    }
}

/// `vibeOS: block: <name> <n> sectors` without allocating.
pub fn write_marker(f: &mut impl core::fmt::Write, name: &str, sectors: u64) -> core::fmt::Result {
    let mut nbuf = [0u8; 20];
    let n = fmt_util::write_dec(sectors, &mut nbuf);
    f.write_str("vibeOS: block: ")?;
    f.write_str(name)?;
    f.write_str(" ")?;
    f.write_str(core::str::from_utf8(n).unwrap_or("0"))?;
    f.write_str(" sectors")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn wr(lba: u64, n: u32, ptr: usize) -> Request {
        Request::new(Op::Write, lba, n).with_seg(ptr, n as usize * 512)
    }

    fn rd(lba: u64, n: u32, ptr: usize) -> Request {
        Request::new(Op::Read, lba, n).with_seg(ptr, n as usize * 512)
    }

    #[test]
    fn merge_adjacent_writes() {
        let mut q = Queue::new();
        q.submit(wr(0, 1, 100)).unwrap();
        q.submit(wr(1, 1, 200)).unwrap();
        assert_eq!(q.len(), 1);
        let r = q.pick().unwrap();
        assert_eq!(r.bio.lba, 0);
        assert_eq!(r.bio.nsect, 2);
        assert_eq!(r.nseg, 2);
        assert_eq!(r.segs[0].ptr, 100);
        assert_eq!(r.segs[1].ptr, 200);
        assert!(q.is_empty());
    }

    #[test]
    fn merge_front() {
        let mut q = Queue::new();
        q.submit(wr(4, 1, 1)).unwrap();
        q.submit(wr(3, 1, 2)).unwrap();
        assert_eq!(q.len(), 1);
        let r = q.pick().unwrap();
        assert_eq!(r.bio.lba, 3);
        assert_eq!(r.bio.nsect, 2);
        assert_eq!(r.segs[0].ptr, 2);
        assert_eq!(r.segs[1].ptr, 1);
    }

    #[test]
    fn no_merge_read_write() {
        let mut q = Queue::new();
        q.submit(wr(0, 1, 1)).unwrap();
        q.submit(rd(1, 1, 2)).unwrap();
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn no_merge_gap() {
        let mut q = Queue::new();
        q.submit(wr(0, 1, 1)).unwrap();
        q.submit(wr(2, 1, 2)).unwrap();
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn no_merge_flush_or_barrier() {
        let mut q = Queue::new();
        q.submit(wr(0, 1, 1)).unwrap();
        q.submit(Request::new(Op::Flush, 0, 0)).unwrap();
        q.submit(wr(1, 1, 2)).unwrap();
        assert_eq!(q.len(), 3);
        q.submit(Request::new(Op::Barrier, 0, 0)).unwrap();
        assert_eq!(q.len(), 4);
    }

    #[test]
    fn merge_discard() {
        let mut q = Queue::new();
        q.submit(Request::new(Op::Discard, 8, 2)).unwrap();
        q.submit(Request::new(Op::Discard, 10, 3)).unwrap();
        assert_eq!(q.len(), 1);
        let r = q.pick().unwrap();
        assert_eq!(r.bio.lba, 8);
        assert_eq!(r.bio.nsect, 5);
        assert_eq!(r.nseg, 0);
    }

    #[test]
    fn clook_order() {
        let mut q = Queue::new();
        q.submit(wr(10, 1, 1)).unwrap();
        q.submit(wr(2, 1, 2)).unwrap();
        q.submit(wr(20, 1, 3)).unwrap();
        q.submit(wr(5, 1, 4)).unwrap();
        assert_eq!(q.pick().unwrap().bio.lba, 2);
        assert_eq!(q.pick().unwrap().bio.lba, 5);
        assert_eq!(q.pick().unwrap().bio.lba, 10);
        assert_eq!(q.pick().unwrap().bio.lba, 20);
    }

    #[test]
    fn clook_wraps() {
        let mut q = Queue::new();
        q.last_lba = 8;
        q.submit(wr(10, 1, 1)).unwrap();
        q.submit(wr(2, 1, 2)).unwrap();
        assert_eq!(q.pick().unwrap().bio.lba, 10);
        assert_eq!(q.pick().unwrap().bio.lba, 2);
    }

    #[test]
    fn barrier_holds_later_requests() {
        let mut q = Queue::new();
        q.submit(wr(10, 1, 1)).unwrap();
        q.submit(wr(0, 1, 2)).unwrap();
        q.submit(Request::new(Op::Barrier, 0, 0)).unwrap();
        q.submit(wr(5, 1, 3)).unwrap();
        assert_eq!(q.pick().unwrap().bio.lba, 0);
        assert_eq!(q.pick().unwrap().bio.lba, 10);
        assert_eq!(q.pick().unwrap().bio.op, Op::Barrier);
        assert_eq!(q.pick().unwrap().bio.lba, 5);
    }

    #[test]
    fn flush_is_fence_then_device_op() {
        let mut q = Queue::new();
        q.submit(wr(3, 1, 1)).unwrap();
        q.submit(Request::new(Op::Flush, 0, 0)).unwrap();
        q.submit(wr(1, 1, 2)).unwrap();
        assert_eq!(q.pick().unwrap().bio.lba, 3);
        assert_eq!(q.pick().unwrap().bio.op, Op::Flush);
        assert_eq!(q.pick().unwrap().bio.lba, 1);
    }

    #[test]
    fn no_merge_across_barrier() {
        let mut q = Queue::new();
        q.submit(wr(0, 1, 1)).unwrap();
        q.submit(Request::new(Op::Barrier, 0, 0)).unwrap();
        q.submit(wr(1, 1, 2)).unwrap();
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn queue_full() {
        let mut q = Queue::new();
        let mut i = 0u64;
        while i < MAX_QUEUE as u64 {
            q.submit(wr(i * 2, 1, 1)).unwrap();
            i += 1;
        }
        assert_eq!(q.submit(wr(999, 1, 1)), Err(BlockError::QueueFull));
    }

    #[test]
    fn failed_rejects_submit() {
        let mut q = Queue::new();
        q.fail();
        assert_eq!(q.submit(wr(0, 1, 1)), Err(BlockError::Failed));
    }

    #[test]
    fn retry_then_fail_device() {
        let mut q = Queue::new();
        let mut r = wr(0, 1, 1);
        r.retries_left = DEFAULT_RETRY_BUDGET;
        q.submit(r).unwrap();
        let mut cur = q.pick().unwrap();
        let mut tries = 0u8;
        loop {
            tries += 1;
            assert!(BlockError::Io.retryable());
            if cur.retries_left == 0 {
                q.fail();
                let mut dump = [None; MAX_QUEUE];
                q.drain(&mut dump);
                assert!(q.failed);
                break;
            }
            cur.retries_left -= 1;
            q.requeue(cur).unwrap();
            cur = q.pick().unwrap();
        }
        assert_eq!(tries, DEFAULT_RETRY_BUDGET + 1);
        assert_eq!(q.submit(wr(1, 1, 2)), Err(BlockError::Failed));
    }

    #[test]
    fn ramdisk_rw_and_4k() {
        let rd = Ramdisk::new("ram0", 4096, 4).unwrap();
        let mut data = vec![0u8; rd.byte_len()];
        let mut buf = vec![0u8; 4096];
        buf[0] = 0xAB;
        buf[4095] = 0xCD;
        rd.write(&mut data, 2, &buf).unwrap();
        let mut out = vec![0u8; 4096];
        rd.read(&data, 2, &mut out).unwrap();
        assert_eq!(out[0], 0xAB);
        assert_eq!(out[4095], 0xCD);
        rd.read(&data, 0, &mut out).unwrap();
        assert_eq!(out[0], 0);
        assert_eq!(rd.flush(), Ok(()));
        assert_eq!(rd.discard(1, 1), Ok(()));
        assert_eq!(rd.discard(3, 2), Err(BlockError::Inval));
        assert_eq!(rd.write(&mut data, 4, &buf), Err(BlockError::Inval));
        let mut odd = [0u8; 100];
        assert_eq!(rd.read(&data, 0, &mut odd), Err(BlockError::Inval));
    }

    #[test]
    fn ramdisk_512_roundtrip() {
        let rd = Ramdisk::new("r", DEFAULT_BLOCK_SIZE, 8).unwrap();
        let mut data = vec![0u8; rd.byte_len()];
        let buf = [0x5Au8; 512];
        rd.write(&mut data, 7, &buf).unwrap();
        let mut out = [0u8; 512];
        rd.read(&data, 7, &mut out).unwrap();
        assert_eq!(out, buf);
        assert_eq!(rd.discard(7, 1), Ok(()));
        rd.read(&data, 7, &mut out).unwrap();
        assert_eq!(out, buf);
    }

    struct HeapDisk {
        ram: Ramdisk,
        data: Mutex<Vec<u8>>,
        fails: Mutex<u32>,
        state: Mutex<DeviceState>,
    }

    impl HeapDisk {
        fn new(bs: u32, n: u64) -> Self {
            let ram = Ramdisk::new("heap", bs, n).unwrap();
            let data = vec![0u8; ram.byte_len()];
            Self {
                ram,
                data: Mutex::new(data),
                fails: Mutex::new(0),
                state: Mutex::new(DeviceState::Ready),
            }
        }
    }

    impl BlockDevice for HeapDisk {
        fn name(&self) -> &'static str {
            self.ram.name()
        }
        fn logical_block_size(&self) -> u32 {
            self.ram.logical_block_size()
        }
        fn capacity_sectors(&self) -> u64 {
            self.ram.capacity_sectors()
        }
        fn state(&self) -> DeviceState {
            *self.state.lock().unwrap()
        }
        fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            if *self.state.lock().unwrap() == DeviceState::Failed {
                return Err(BlockError::Failed);
            }
            let mut f = self.fails.lock().unwrap();
            if *f > 0 {
                *f -= 1;
                return Err(BlockError::Io);
            }
            self.ram.read(&self.data.lock().unwrap(), lba, buf)
        }
        fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
            if *self.state.lock().unwrap() == DeviceState::Failed {
                return Err(BlockError::Failed);
            }
            let mut f = self.fails.lock().unwrap();
            if *f > 0 {
                *f -= 1;
                return Err(BlockError::Io);
            }
            self.ram.write(&mut self.data.lock().unwrap(), lba, buf)
        }
        fn flush(&self) -> Result<(), BlockError> {
            self.ram.flush()
        }
        fn discard(&self, lba: u64, nsectors: u64) -> Result<(), BlockError> {
            self.ram.discard(lba, nsectors)
        }
    }

    #[test]
    fn trait_and_retry_budget() {
        let d = HeapDisk::new(512, 4);
        *d.fails.lock().unwrap() = 2;
        let mut buf = [0u8; 512];
        buf[0] = 1;
        assert_eq!(d.write(0, &buf), Err(BlockError::Io));
        assert_eq!(d.write(0, &buf), Err(BlockError::Io));
        d.write(0, &buf).unwrap();
        let mut out = [0u8; 512];
        d.read(0, &mut out).unwrap();
        assert_eq!(out[0], 1);
        d.flush().unwrap();
        d.discard(0, 1).unwrap();
        *d.state.lock().unwrap() = DeviceState::Failed;
        assert_eq!(d.read(0, &mut out), Err(BlockError::Failed));
        assert_eq!(BlockError::Inval.as_str(), "inval");
        assert_eq!(Op::Flush.as_str(), "flush");
        assert_eq!(DeviceState::Failed.as_str(), "failed");
        assert_eq!(d.name(), "heap");
        assert_eq!(d.logical_block_size(), 512);
        assert_eq!(d.capacity_sectors(), 4);
    }

    #[test]
    fn marker_shape() {
        let mut s = String::new();
        write_marker(&mut s, "ram0", 256).unwrap();
        assert_eq!(s, "vibeOS: block: ram0 256 sectors");
    }

    #[test]
    fn segs_cap_blocks_merge() {
        let mut q = Queue::new();
        let mut a = Request::new(Op::Write, 0, MAX_SEGS as u32);
        let mut i = 0usize;
        while i < MAX_SEGS {
            a = a.with_seg(i + 1, 512);
            i += 1;
        }
        q.submit(a).unwrap();
        q.submit(wr(MAX_SEGS as u64, 1, 99)).unwrap();
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn waiter_cookies_follow_merge() {
        let mut q = Queue::new();
        q.submit(wr(0, 1, 1).with_waiter(11)).unwrap();
        q.submit(wr(1, 1, 2).with_waiter(22)).unwrap();
        let r = q.pick().unwrap();
        assert_eq!(r.nwait, 2);
        assert_eq!(r.waiters[0], 11);
        assert_eq!(r.waiters[1], 22);
    }
}
