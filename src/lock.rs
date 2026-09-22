//! Global lock ranks. DESIGN §2.1 / §7.7.
//!
//! Acquire increasing rank, release reverse. Never take a lower rank
//! while holding a higher one. Rank 0 is untracked.

/// Page tables / KVA / ioremap.
pub const RANK_PT: u8 = 1;
/// Buddy physical allocator.
pub const RANK_BUDDY: u8 = 2;
/// Kernel heap.
pub const RANK_HEAP: u8 = 3;
/// Scheduler (TCB table, timeouts, wait queues).
pub const RANK_SCHED: u8 = 4;
/// Device / driver locks.
pub const RANK_DEVICE: u8 = 5;
/// Serial TX. Last so any holder can still log.
pub const RANK_SERIAL: u8 = 6;

/// Bit for `rank` in a held mask. Rank 0 → 0.
pub const fn rank_bit(rank: u8) -> u8 {
    if rank == 0 { 0 } else { 1u8 << (rank - 1) }
}

/// No held rank is strictly above `rank`. Rank 0 always allowed.
pub const fn can_acquire(held: u8, rank: u8) -> bool {
    if rank == 0 { true } else { held >> rank == 0 }
}

pub const fn acquire_mask(held: u8, rank: u8) -> u8 {
    held | rank_bit(rank)
}

pub const fn release_mask(held: u8, rank: u8) -> u8 {
    held & !rank_bit(rank)
}

/// Two CPU-local locks: lower `cpu_id` first. DESIGN §7.7.
pub const fn cpu_lock_order(a: u32, b: u32) -> (u32, u32) {
    if a <= b { (a, b) } else { (b, a) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_are_the_documented_order() {
        assert_eq!(RANK_PT, 1);
        assert_eq!(RANK_BUDDY, 2);
        assert_eq!(RANK_HEAP, 3);
        assert_eq!(RANK_SCHED, 4);
        assert_eq!(RANK_DEVICE, 5);
        assert_eq!(RANK_SERIAL, 6);
        assert!(RANK_PT < RANK_BUDDY);
        assert!(RANK_BUDDY < RANK_HEAP);
        assert!(RANK_HEAP < RANK_SCHED);
        assert!(RANK_SCHED < RANK_DEVICE);
        assert!(RANK_DEVICE < RANK_SERIAL);
    }

    #[test]
    fn acquire_increasing_is_ok() {
        let mut h = 0u8;
        assert!(can_acquire(h, RANK_PT));
        h = acquire_mask(h, RANK_PT);
        assert!(can_acquire(h, RANK_BUDDY));
        h = acquire_mask(h, RANK_BUDDY);
        assert!(can_acquire(h, RANK_HEAP));
        h = acquire_mask(h, RANK_HEAP);
        assert!(can_acquire(h, RANK_SCHED));
        h = acquire_mask(h, RANK_SERIAL);
        assert_eq!(h & rank_bit(RANK_PT), rank_bit(RANK_PT));
        h = release_mask(h, RANK_SERIAL);
        h = release_mask(h, RANK_HEAP);
        h = release_mask(h, RANK_BUDDY);
        h = release_mask(h, RANK_PT);
        assert_eq!(h, 0);
    }

    #[test]
    fn heap_then_buddy_is_forbidden() {
        let h = acquire_mask(0, RANK_HEAP);
        assert!(!can_acquire(h, RANK_BUDDY));
        assert!(!can_acquire(h, RANK_PT));
        assert!(can_acquire(h, RANK_SCHED));
        assert!(can_acquire(h, RANK_SERIAL));
        assert!(can_acquire(h, 0));
    }

    #[test]
    fn cpu_pair_lower_id_first() {
        assert_eq!(cpu_lock_order(0, 3), (0, 3));
        assert_eq!(cpu_lock_order(3, 0), (0, 3));
        assert_eq!(cpu_lock_order(2, 2), (2, 2));
    }
}
