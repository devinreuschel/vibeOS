//! IPI protocol helpers. DESIGN §7.6–§7.9, ROADMAP §4.8–§4.10.
//!
//! Portable: inbox bits, RR placement, shootdown waiter/ack math.
//! MMIO and IDT live in the binary crate.

use crate::thread::{CpuAffinity, MAX_THREADS, ThreadId};

/// Matches the 64-bit online mask. PerCpu is heap-sized from MADT.
pub const MAX_IPI_CPUS: usize = 64;

const _: () = assert!(MAX_THREADS <= 64, "wake inbox is a u64 bitset");

pub const fn inbox_bit(id: ThreadId) -> Option<u64> {
    if id.is_none() || id.0 >= 64 {
        None
    } else {
        Some(1u64 << id.0)
    }
}

pub fn inbox_or(bits: u64, id: ThreadId) -> u64 {
    match inbox_bit(id) {
        Some(b) => bits | b,
        None => bits,
    }
}

/// CPUs that must ack, excluding the initiator.
pub const fn waiter_mask(online: u64, self_cpu: u32) -> u64 {
    if self_cpu >= 64 {
        online
    } else {
        online & !(1u64 << self_cpu)
    }
}

pub const fn all_acked(waiters: u64, acked: u64) -> bool {
    waiters & acked == waiters
}

/// Round-robin among set bits of `online`. `rr` is the cursor; updated.
pub fn pick_cpu(affinity: CpuAffinity, online: u64, rr: &mut u32) -> u32 {
    let n = online.count_ones();
    let fallback = if online & 1 != 0 {
        0
    } else {
        online.trailing_zeros()
    };
    match affinity {
        CpuAffinity::Pinned(c) => {
            if c < 64 && online & (1u64 << c) != 0 {
                c
            } else {
                fallback
            }
        }
        CpuAffinity::Any => {
            if n <= 1 {
                return fallback;
            }
            let start = *rr;
            *rr = rr.wrapping_add(1);
            let mut i = 0u32;
            while i < 64 {
                let c = start.wrapping_add(i) % 64;
                if online & (1u64 << c) != 0 {
                    return c;
                }
                i += 1;
            }
            fallback
        }
    }
}

/// Stay on last CPU for `Any` (no migration). Pinned if that CPU is online.
pub fn home_cpu(affinity: CpuAffinity, last: u32, online: u64) -> u32 {
    match affinity {
        CpuAffinity::Pinned(c) => {
            if c < 64 && online & (1u64 << c) != 0 {
                c
            } else if last < 64 && online & (1u64 << last) != 0 {
                last
            } else {
                0
            }
        }
        CpuAffinity::Any => {
            if last < 64 && online & (1u64 << last) != 0 {
                last
            } else {
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(n: u32) -> ThreadId {
        ThreadId(n)
    }

    #[test]
    fn inbox_bits_are_thread_ids() {
        assert_eq!(inbox_bit(tid(0)), Some(1));
        assert_eq!(inbox_bit(tid(3)), Some(8));
        assert_eq!(inbox_bit(tid(63)), Some(1u64 << 63));
        assert_eq!(inbox_bit(ThreadId::NONE), None);
        assert_eq!(inbox_or(0, tid(2)), 4);
        assert_eq!(inbox_or(4, tid(2)), 4);
        assert_eq!(inbox_or(4, tid(0)), 5);
    }

    #[test]
    fn shootdown_waiters_exclude_self() {
        assert_eq!(waiter_mask(0b1111, 0), 0b1110);
        assert_eq!(waiter_mask(0b1111, 3), 0b0111);
        assert_eq!(waiter_mask(0b1, 0), 0);
        assert!(all_acked(0b1110, 0b1110));
        assert!(!all_acked(0b1110, 0b0100));
        assert!(all_acked(0, 0));
    }

    #[test]
    fn rr_any_walks_online_cpus() {
        let mut rr = 0u32;
        let a = pick_cpu(CpuAffinity::Any, 0b1011, &mut rr);
        let b = pick_cpu(CpuAffinity::Any, 0b1011, &mut rr);
        let c = pick_cpu(CpuAffinity::Any, 0b1011, &mut rr);
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(c, 3);
        assert_eq!(pick_cpu(CpuAffinity::Pinned(1), 0b1011, &mut rr), 1);
        assert_eq!(pick_cpu(CpuAffinity::Pinned(2), 0b1011, &mut rr), 0);
        assert_eq!(pick_cpu(CpuAffinity::Any, 0b1, &mut rr), 0);
    }

    #[test]
    fn home_does_not_migrate_any() {
        assert_eq!(home_cpu(CpuAffinity::Any, 3, 0b1111), 3);
        assert_eq!(home_cpu(CpuAffinity::Pinned(1), 3, 0b1111), 1);
        assert_eq!(home_cpu(CpuAffinity::Pinned(5), 3, 0b1111), 3);
        assert_eq!(home_cpu(CpuAffinity::Any, 7, 0b1), 0);
    }
}
