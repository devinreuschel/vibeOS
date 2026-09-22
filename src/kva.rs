//! Kernel virtual address range allocator, portable half. DESIGN §4.5.
//!
//! First-fit over the 64 GiB window at `KVA_START`. Freed ranges go to
//! the tail of the free list so the window between unmap and TLB
//! shootdown is not immediately reused (DESIGN §4.5, §7.9).
//!
//! This type only hands out VA. Mapping, guard pages, vmap, and the
//! deferred-free list live in the binary crate — they need the buddy
//! allocator and page tables.

/// DESIGN §4.1.
pub const KVA_START: u64 = 0xFFFF_D000_0000_0000;
pub const KVA_SIZE: u64 = 64 * 1024 * 1024 * 1024;
pub const KVA_END: u64 = KVA_START + KVA_SIZE;

pub const PAGE_SIZE: u64 = 4096;
/// Default kernel stack: 4 pages (16 KiB) plus the unmapped guard.
pub const DEFAULT_STACK_PAGES: usize = 4;

const MAX_RANGES: usize = 128;

/// Free-list node pool exhausted after coalesce. Not a VA OOM (`alloc`
/// still returns `None` for that).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvaError {
    Exhausted,
}

impl KvaError {
    pub fn as_str(self) -> &'static str {
        match self {
            KvaError::Exhausted => "exhausted",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Range {
    start: u64,
    len: u64,
}

/// Snapshot for `meminfo`. `used` is bytes currently reserved (including
/// guard pages), not necessarily mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KvaStats {
    pub used: u64,
    pub capacity: u64,
    pub free_ranges: usize,
}

pub struct Kva {
    nodes: [Range; MAX_RANGES],
    next: [Option<u8>; MAX_RANGES],
    head: Option<u8>,
    tail: Option<u8>,
    /// Unused node indices, stack-allocated.
    slots: [u8; MAX_RANGES],
    nslots: u8,
    used: u64,
    capacity: u64,
}

impl Kva {
    pub const fn empty() -> Self {
        Self {
            nodes: [Range { start: 0, len: 0 }; MAX_RANGES],
            next: [None; MAX_RANGES],
            head: None,
            tail: None,
            slots: [0; MAX_RANGES],
            nslots: 0,
            used: 0,
            capacity: 0,
        }
    }

    pub fn init(&mut self, start: u64, size: u64) -> Result<(), KvaError> {
        assert!(size.is_multiple_of(PAGE_SIZE));
        assert!(start.is_multiple_of(PAGE_SIZE));
        *self = Self::empty();
        for (i, slot) in self.slots.iter_mut().enumerate() {
            *slot = i as u8;
        }
        self.nslots = MAX_RANGES as u8;
        self.capacity = size;
        let i = self.alloc_slot().ok_or(KvaError::Exhausted)?;
        self.nodes[i as usize] = Range { start, len: size };
        self.next[i as usize] = None;
        self.head = Some(i);
        self.tail = Some(i);
        Ok(())
    }

    pub fn stats(&self) -> KvaStats {
        let mut n = 0usize;
        let mut cur = self.head;
        while let Some(i) = cur {
            n += 1;
            cur = self.next[i as usize];
        }
        KvaStats {
            used: self.used,
            capacity: self.capacity,
            free_ranges: n,
        }
    }

    /// First-fit. `len` page-aligned. Returns the start VA.
    pub fn alloc(&mut self, len: u64) -> Option<u64> {
        if len == 0 || !len.is_multiple_of(PAGE_SIZE) {
            return None;
        }
        let mut prev: Option<u8> = None;
        let mut cur = self.head;
        while let Some(i) = cur {
            let r = self.nodes[i as usize];
            if r.len >= len {
                let start = r.start;
                if r.len == len {
                    self.unlink(prev, i);
                    self.free_slot(i);
                } else {
                    self.nodes[i as usize] = Range {
                        start: r.start + len,
                        len: r.len - len,
                    };
                }
                self.used += len;
                return Some(start);
            }
            prev = cur;
            cur = self.next[i as usize];
        }
        None
    }

    /// Reserve `pages + 1` pages. First page is the guard (returned as
    /// `guard`); mapped region starts at `guard + PAGE_SIZE` and is
    /// `pages` pages long.
    pub fn alloc_guarded(&mut self, pages: usize) -> Option<u64> {
        if pages == 0 {
            return None;
        }
        let len = (pages as u64 + 1) * PAGE_SIZE;
        self.alloc(len)
    }

    /// Append `[start, start+len)` to the tail. No coalescing on this
    /// path: DESIGN wants recently-freed VA at the tail so it is not
    /// the next first-fit hit.
    pub fn free(&mut self, start: u64, len: u64) -> Result<(), KvaError> {
        assert!(len.is_multiple_of(PAGE_SIZE) && start.is_multiple_of(PAGE_SIZE));
        assert!(len > 0);
        assert!(
            self.used >= len,
            "kva: free {len} when used is {}",
            self.used
        );
        self.used -= len;
        if self.nslots == 0 {
            self.coalesce_all()?;
        }
        let i = self.alloc_slot().ok_or(KvaError::Exhausted)?;
        self.nodes[i as usize] = Range { start, len };
        self.next[i as usize] = None;
        if let Some(t) = self.tail {
            self.next[t as usize] = Some(i);
        } else {
            self.head = Some(i);
        }
        self.tail = Some(i);
        Ok(())
    }

    fn unlink(&mut self, prev: Option<u8>, i: u8) {
        let nxt = self.next[i as usize];
        if let Some(p) = prev {
            self.next[p as usize] = nxt;
        } else {
            self.head = nxt;
        }
        if self.tail == Some(i) {
            self.tail = prev;
        }
        self.next[i as usize] = None;
    }

    fn alloc_slot(&mut self) -> Option<u8> {
        if self.nslots == 0 {
            return None;
        }
        self.nslots -= 1;
        Some(self.slots[self.nslots as usize])
    }

    fn free_slot(&mut self, i: u8) {
        self.slots[self.nslots as usize] = i;
        self.nslots += 1;
        self.nodes[i as usize] = Range { start: 0, len: 0 };
    }

    /// Overflow valve: sort + merge adjacent, rebuild as a single
    /// address-ordered list. Normal frees never take this path.
    fn coalesce_all(&mut self) -> Result<(), KvaError> {
        let mut tmp = [Range { start: 0, len: 0 }; MAX_RANGES];
        let mut n = 0usize;
        let mut cur = self.head;
        while let Some(i) = cur {
            tmp[n] = self.nodes[i as usize];
            n += 1;
            cur = self.next[i as usize];
        }
        tmp[..n].sort_unstable_by_key(|r| r.start);
        let mut w = 0usize;
        for i in 0..n {
            if w > 0 {
                let prev = tmp[w - 1];
                if prev.start + prev.len == tmp[i].start {
                    tmp[w - 1].len += tmp[i].len;
                    continue;
                }
            }
            tmp[w] = tmp[i];
            w += 1;
        }
        self.head = None;
        self.tail = None;
        for i in 0..MAX_RANGES {
            self.slots[i] = i as u8;
            self.next[i] = None;
            self.nodes[i] = Range { start: 0, len: 0 };
        }
        self.nslots = MAX_RANGES as u8;
        for r in tmp[..w].iter() {
            let i = self.alloc_slot().ok_or(KvaError::Exhausted)?;
            self.nodes[i as usize] = *r;
            self.next[i as usize] = None;
            if let Some(t) = self.tail {
                self.next[t as usize] = Some(i);
            } else {
                self.head = Some(i);
            }
            self.tail = Some(i);
        }
        Ok(())
    }

    /// Test helper: walk the free list into a vec of (start, len).
    #[cfg(test)]
    fn free_list(&self) -> std::vec::Vec<(u64, u64)> {
        let mut out = std::vec::Vec::new();
        let mut cur = self.head;
        while let Some(i) = cur {
            let r = self.nodes[i as usize];
            out.push((r.start, r.len));
            cur = self.next[i as usize];
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Kva {
        let mut k = Kva::empty();
        k.init(KVA_START, 64 * PAGE_SIZE).unwrap();
        k
    }

    #[test]
    fn first_fit_from_front() {
        let mut k = fresh();
        let a = k.alloc(2 * PAGE_SIZE).unwrap();
        assert_eq!(a, KVA_START);
        let b = k.alloc(PAGE_SIZE).unwrap();
        assert_eq!(b, KVA_START + 2 * PAGE_SIZE);
        assert_eq!(k.stats().used, 3 * PAGE_SIZE);
    }

    #[test]
    fn freed_ranges_go_to_tail() {
        let mut k = fresh();
        let a = k.alloc(2 * PAGE_SIZE).unwrap();
        let _b = k.alloc(2 * PAGE_SIZE).unwrap();
        k.free(a, 2 * PAGE_SIZE).unwrap();
        // Free list head is the leftover after the two allocs (starting
        // at +4 pages). The just-freed `a` must sit at the tail, so the
        // next first-fit of 2 pages takes leftover, not `a`.
        let c = k.alloc(2 * PAGE_SIZE).unwrap();
        assert_ne!(c, a, "freshly freed range must not be the next first-fit");
        assert_eq!(c, KVA_START + 4 * PAGE_SIZE);
        let list = k.free_list();
        assert_eq!(list.last().unwrap().0, a);
    }

    #[test]
    fn alloc_free_roundtrip_restores_used() {
        let mut k = fresh();
        let s0 = k.stats();
        let a = k.alloc(8 * PAGE_SIZE).unwrap();
        assert_eq!(k.stats().used, 8 * PAGE_SIZE);
        k.free(a, 8 * PAGE_SIZE).unwrap();
        assert_eq!(k.stats().used, s0.used);
        assert_eq!(k.stats().capacity, s0.capacity);
        // Freed ranges go to the tail without coalescing, so the range
        // count may grow. used is the invariant that matters.
    }

    #[test]
    fn guarded_reserves_pages_plus_one() {
        let mut k = fresh();
        let guard = k.alloc_guarded(4).unwrap();
        assert_eq!(guard, KVA_START);
        assert_eq!(k.stats().used, 5 * PAGE_SIZE);
        k.free(guard, 5 * PAGE_SIZE).unwrap();
        assert_eq!(k.stats().used, 0);
    }

    #[test]
    fn exhaustion_returns_none() {
        let mut k = fresh();
        assert!(k.alloc(64 * PAGE_SIZE).is_some());
        assert!(k.alloc(PAGE_SIZE).is_none());
        assert!(k.alloc_guarded(1).is_none());
    }

    #[test]
    fn reject_unaligned_and_zero() {
        let mut k = fresh();
        assert!(k.alloc(0).is_none());
        assert!(k.alloc(PAGE_SIZE - 1).is_none());
        assert!(k.alloc_guarded(0).is_none());
    }

    #[test]
    fn split_leaves_remainder_in_place() {
        let mut k = fresh();
        let a = k.alloc(PAGE_SIZE).unwrap();
        assert_eq!(a, KVA_START);
        let list = k.free_list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0], (KVA_START + PAGE_SIZE, 63 * PAGE_SIZE));
    }

    #[test]
    fn init_ok_on_fresh_window() {
        let mut k = Kva::empty();
        assert_eq!(k.init(KVA_START, PAGE_SIZE), Ok(()));
        assert_eq!(k.stats().capacity, PAGE_SIZE);
        assert_eq!(k.stats().used, 0);
        assert_eq!(k.stats().free_ranges, 1);
    }
}
