//! Global lock ranks and the per-CPU held word. DESIGN §2.1 to §2.3, §7.7.
//!
//! Acquire increasing rank, release reverse. Never take a lower rank
//! while holding a higher one. Rank 0 is untracked.

/// Kernel heap. First: growing it takes PT and then BUDDY after dropping it.
pub const RANK_HEAP: u8 = 1;
/// Page tables / KVA / ioremap.
pub const RANK_PT: u8 = 2;
/// Buddy physical allocator.
pub const RANK_BUDDY: u8 = 3;
/// Scheduler (TCB table, timeouts, wait queues).
pub const RANK_SCHED: u8 = 4;
/// Device / driver locks.
pub const RANK_DEVICE: u8 = 5;
/// Serial TX. Last so any holder can still log.
pub const RANK_SERIAL: u8 = 6;

/// Highest rank `Held` counts. Ranks 1 to `MAX_RANK`; rank 0 is untracked.
pub use crate::limits::MAX_RANK;

/// Bits of one rank's count in [`Held`].
const COUNT_BITS: u32 = 4;
/// Largest count a rank holds before [`RankError::Overflow`].
const COUNT_MAX: u64 = (1 << COUNT_BITS) - 1;
/// First bit of the lockless depth in [`Held`].
const DEPTH_SHIFT: u32 = 32;
/// Largest lockless depth before [`Held::enter_lockless`] saturates.
const DEPTH_MAX: u64 = 0xFF;

/// Why the rank checker refused a lock. DESIGN §2.1 to §2.3.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RankError {
    /// This CPU already holds a lock of `rank`; `lock_nested` names a pair.
    SameRank { rank: u8 },
    /// This CPU holds a lock ranked above `rank` (`held` is the rank mask).
    Order { rank: u8, held: u8 },
    /// This CPU runs code that takes no lock: call-function or shootdown
    /// work, or an NMI, `#MC`, or CPL-0 `#DB` body (DESIGN §2.2).
    Lockless,
    /// `rank` is already held `COUNT_MAX` times.
    Overflow { rank: u8 },
}

/// A lock-rank violation is a bad argument.
impl From<RankError> for crate::kerror::KError {
    fn from(e: RankError) -> Self {
        match e {
            RankError::SameRank { .. }
            | RankError::Order { .. }
            | RankError::Lockless
            | RankError::Overflow { .. } => Self::Inval,
        }
    }
}

impl core::fmt::Display for RankError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            RankError::SameRank { rank } => write!(f, "rank {rank} already held"),
            RankError::Order { rank, held } => {
                write!(f, "rank {rank} while holding {held:#x}")
            }
            RankError::Lockless => f.write_str("lock in a lockless section"),
            RankError::Overflow { rank } => write!(f, "rank {rank} nested too deep"),
        }
    }
}

/// One CPU's held locks: a 4-bit count per rank (rank `r` at bits
/// `4 * (r - 1)`) and the lockless depth (bits 32 to 39). The kernel keeps
/// one per CPU and changes it with atomic adds, since an NMI may raise the
/// depth between a load and a store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Held(u64);

impl Held {
    pub const EMPTY: Held = Held(0);

    pub const fn from_raw(raw: u64) -> Held {
        Held(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    /// What adding one lock of `rank` adds to the raw word. 0 for rank 0
    /// or a rank above `MAX_RANK`.
    pub const fn count_unit(rank: u8) -> u64 {
        if rank == 0 || rank > MAX_RANK {
            0
        } else {
            1 << (COUNT_BITS * (rank as u32 - 1))
        }
    }

    /// What one lockless level adds to the raw word.
    pub const fn depth_unit() -> u64 {
        1 << DEPTH_SHIFT
    }

    /// Locks of `rank` held. 0 for rank 0.
    pub const fn count(self, rank: u8) -> u8 {
        if rank == 0 || rank > MAX_RANK {
            0
        } else {
            ((self.0 >> (COUNT_BITS * (rank as u32 - 1))) & COUNT_MAX) as u8
        }
    }

    /// Bit `rank - 1` set for each rank held at least once.
    pub const fn mask(self) -> u8 {
        let mut m = 0u8;
        let mut r = 1u8;
        while r <= MAX_RANK {
            if self.count(r) != 0 {
                m |= 1 << (r - 1);
            }
            r += 1;
        }
        m
    }

    pub const fn lockless_depth(self) -> u8 {
        ((self.0 >> DEPTH_SHIFT) & DEPTH_MAX) as u8
    }

    /// No lock held and no lockless section entered.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Take one lock of `rank`: refused when `rank` or a higher rank is
    /// held, or inside a lockless section. DESIGN §2.3's nesting rule.
    pub const fn acquire(self, rank: u8) -> Result<Held, RankError> {
        if rank != 0 && self.count(rank) != 0 {
            if let Err(e) = self.check_cell() {
                return Err(e);
            }
            if let Err(e) = self.check_order(rank) {
                return Err(e);
            }
            return Err(RankError::SameRank { rank });
        }
        self.acquire_nested(rank)
    }

    /// Take one lock of `rank`, which may already be held (a named pair).
    pub const fn acquire_nested(self, rank: u8) -> Result<Held, RankError> {
        if let Err(e) = self.check_cell() {
            return Err(e);
        }
        if rank == 0 || rank > MAX_RANK {
            return Ok(self);
        }
        if let Err(e) = self.check_order(rank) {
            return Err(e);
        }
        if self.count(rank) as u64 == COUNT_MAX {
            return Err(RankError::Overflow { rank });
        }
        Ok(Held(self.0 + Held::count_unit(rank)))
    }

    /// Drop one lock of `rank`. Saturates at 0.
    pub const fn release(self, rank: u8) -> Held {
        if self.count(rank) == 0 {
            self
        } else {
            Held(self.0 - Held::count_unit(rank))
        }
    }

    /// An IRQ-off cell may be taken: no lockless section is active.
    pub const fn check_cell(self) -> Result<(), RankError> {
        if self.lockless_depth() != 0 {
            Err(RankError::Lockless)
        } else {
            Ok(())
        }
    }

    /// Raise the lockless depth. Saturates.
    pub const fn enter_lockless(self) -> Held {
        if self.lockless_depth() as u64 == DEPTH_MAX {
            self
        } else {
            Held(self.0 + Held::depth_unit())
        }
    }

    /// Lower the lockless depth. Saturates at 0.
    pub const fn leave_lockless(self) -> Held {
        if self.lockless_depth() == 0 {
            self
        } else {
            Held(self.0 - Held::depth_unit())
        }
    }

    /// No held rank is strictly above `rank`.
    const fn check_order(self, rank: u8) -> Result<(), RankError> {
        let held = self.mask();
        if (held as u32) >> rank != 0 {
            Err(RankError::Order { rank, held })
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_are_the_documented_order() {
        assert_eq!(RANK_HEAP, 1);
        assert_eq!(RANK_PT, 2);
        assert_eq!(RANK_BUDDY, 3);
        assert_eq!(RANK_SCHED, 4);
        assert_eq!(RANK_DEVICE, 5);
        assert_eq!(RANK_SERIAL, 6);
        const {
            assert!(RANK_HEAP < RANK_PT);
            assert!(RANK_PT < RANK_BUDDY);
            assert!(RANK_BUDDY < RANK_SCHED);
            assert!(RANK_SCHED < RANK_DEVICE);
            assert!(RANK_DEVICE < RANK_SERIAL);
            assert!(RANK_SERIAL <= MAX_RANK);
        }
    }

    #[test]
    fn acquire_increasing_is_ok() {
        let mut h = Held::EMPTY;
        for r in [RANK_HEAP, RANK_PT, RANK_BUDDY, RANK_SCHED, RANK_SERIAL] {
            h = h.acquire(r).unwrap();
        }
        assert_eq!(h.count(RANK_HEAP), 1);
        assert_eq!(h.count(RANK_DEVICE), 0);
        for r in [RANK_SERIAL, RANK_SCHED, RANK_BUDDY, RANK_PT, RANK_HEAP] {
            h = h.release(r);
        }
        assert!(h.is_empty());
        assert_eq!(h.release(RANK_HEAP), Held::EMPTY);
    }

    #[test]
    fn pt_then_heap_is_forbidden() {
        let h = Held::EMPTY.acquire(RANK_PT).unwrap();
        assert_eq!(
            h.acquire(RANK_HEAP),
            Err(RankError::Order {
                rank: RANK_HEAP,
                held: 1 << (RANK_PT - 1),
            })
        );
        assert!(h.acquire_nested(RANK_HEAP).is_err());
        assert!(h.acquire(RANK_BUDDY).is_ok());
        assert!(h.acquire(RANK_SERIAL).is_ok());
        assert_eq!(h.acquire(0), Ok(h));
    }

    #[test]
    fn buddy_then_heap_is_forbidden() {
        let h = Held::EMPTY.acquire(RANK_BUDDY).unwrap();
        assert!(matches!(
            h.acquire(RANK_HEAP),
            Err(RankError::Order {
                rank: RANK_HEAP,
                ..
            })
        ));
        assert!(h.acquire(RANK_PT).is_err());
        assert!(h.acquire_nested(RANK_HEAP).is_err());
        assert!(h.acquire(RANK_SCHED).is_ok());
        let dev = Held::EMPTY.acquire(RANK_DEVICE).unwrap();
        assert!(dev.acquire(RANK_HEAP).is_err());
    }

    #[test]
    fn nested_inner_release_keeps_outer_rank() {
        let outer = Held::EMPTY.acquire(RANK_DEVICE).unwrap();
        let inner = outer.acquire_nested(RANK_DEVICE).unwrap();
        assert_eq!(inner.count(RANK_DEVICE), 2);
        let after = inner.release(RANK_DEVICE);
        assert_eq!(after, outer);
        assert_eq!(after.count(RANK_DEVICE), 1);
        assert_eq!(after.mask(), 1 << (RANK_DEVICE - 1));
        assert!(after.release(RANK_DEVICE).is_empty());
        let mut h = Held::EMPTY;
        for _ in 0..15 {
            h = h.acquire_nested(RANK_SCHED).unwrap();
        }
        assert_eq!(
            h.acquire_nested(RANK_SCHED),
            Err(RankError::Overflow { rank: RANK_SCHED })
        );
        assert_eq!(h.count(RANK_DEVICE), 0);
    }

    #[test]
    fn same_rank_lock_is_refused() {
        let h = Held::EMPTY.acquire(RANK_DEVICE).unwrap();
        assert_eq!(
            h.acquire(RANK_DEVICE),
            Err(RankError::SameRank { rank: RANK_DEVICE })
        );
        assert!(h.acquire_nested(RANK_DEVICE).is_ok());
        assert_eq!(h.acquire(0), Ok(h));
        assert_eq!(h.acquire(0).unwrap().acquire(0), Ok(h));
    }

    #[test]
    fn lock_in_lockless_depth_is_refused() {
        let h = Held::EMPTY.enter_lockless();
        assert_eq!(h.lockless_depth(), 1);
        assert!(h.mask() == 0 && !h.is_empty());
        assert_eq!(h.acquire(RANK_DEVICE), Err(RankError::Lockless));
        assert_eq!(h.acquire_nested(RANK_DEVICE), Err(RankError::Lockless));
        assert_eq!(h.acquire(0), Err(RankError::Lockless));
        assert_eq!(h.check_cell(), Err(RankError::Lockless));
        let h2 = h.enter_lockless().leave_lockless();
        assert_eq!(h2, h);
        let out = h.leave_lockless();
        assert!(out.is_empty());
        assert_eq!(out.leave_lockless(), out);
        assert!(out.check_cell().is_ok());
        assert!(out.acquire(RANK_DEVICE).is_ok());
    }
}
