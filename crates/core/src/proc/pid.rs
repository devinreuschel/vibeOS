//! Pid and tid allocation (ROADMAP §10.4, F127; DESIGN §2.11 rule 4).
//!
//! Pids and tids come from one [`PidAlloc`]: ids `1..pid_max` in increasing
//! order, wrapping to [`PID_WRAP`] (Linux's `RESERVED_PIDS`) and skipping
//! ids in use, as Linux's `pid_max` works (proc(5)). Every carrier of an id
//! (a thread, a process, later a process group or session) holds a use on
//! it, and the id is free again only when the last use goes, so a reaped
//! pid is not handed out again until the counter comes round.
//!
//! An id is never a table index: [`IdIndex`] maps it to its table slot.

use crate::limits::{PID_MAX, PID_WRAP};

const PID_WORDS: usize = (PID_MAX as usize).div_ceil(64);

/// The wrapping id allocator (C-PIDS). Id 0, the bootstrap thread's, is
/// never handed out.
pub struct PidAlloc {
    /// One bit per id with at least one use: the scan's bitmap.
    used: [u64; PID_WORDS],
    /// Uses per id: how many carriers hold it.
    uses: [u8; PID_MAX as usize],
    /// The last id `alloc` handed out; the next scan starts after it.
    last: u32,
    /// Ids run `1..pid_max`.
    pid_max: u32,
}

impl PidAlloc {
    /// An allocator for ids `1..pid_max`, with `pid_max` clamped to
    /// `PID_WRAP + 1..=PID_MAX`.
    pub const fn new(pid_max: u32) -> Self {
        let pid_max = if pid_max <= PID_WRAP {
            PID_WRAP + 1
        } else if pid_max > PID_MAX {
            PID_MAX
        } else {
            pid_max
        };
        Self {
            used: [0; PID_WORDS],
            uses: [0; PID_MAX as usize],
            last: 0,
            pid_max,
        }
    }

    /// Ids run `1..pid_max()`.
    pub const fn pid_max(&self) -> u32 {
        self.pid_max
    }

    /// The next free id after the last one handed out, wrapping to
    /// `PID_WRAP` past `pid_max`, with one use. `None` when none is free.
    pub fn alloc(&mut self) -> Option<u32> {
        let start = self.last.saturating_add(1);
        let id = self
            .find_free(start, self.pid_max)
            .or_else(|| self.find_free(PID_WRAP, start.min(self.pid_max)))?;
        self.last = id;
        self.mark(id);
        Some(id)
    }

    /// Add a use to `id`, marking it in use if it was free: a carrier such
    /// as a process group keeps the id from being handed out. False for id
    /// 0, an id at or past `pid_max`, or one whose use count is full.
    #[must_use]
    pub fn hold(&mut self, id: u32) -> bool {
        if id == 0 || id >= self.pid_max {
            return false;
        }
        let Some(n) = self.uses.get_mut(id as usize) else {
            return false;
        };
        if *n == u8::MAX {
            return false;
        }
        if *n == 0 {
            self.mark(id);
        } else {
            *n += 1;
        }
        true
    }

    /// Drop a use of `id`; at zero the id is free. False for an id with no
    /// use, which frees nothing.
    #[must_use]
    pub fn free(&mut self, id: u32) -> bool {
        let Some(n) = self.uses.get_mut(id as usize) else {
            return false;
        };
        if *n == 0 {
            return false;
        }
        *n -= 1;
        if *n == 0
            && let Some(w) = self.used.get_mut(id as usize / 64)
        {
            *w &= !(1u64 << (id % 64));
        }
        true
    }

    /// True when `id` has a use.
    pub fn in_use(&self, id: u32) -> bool {
        self.uses.get(id as usize).is_some_and(|n| *n > 0)
    }

    /// Give a free `id` its first use.
    fn mark(&mut self, id: u32) {
        if let Some(n) = self.uses.get_mut(id as usize) {
            *n = 1;
        }
        if let Some(w) = self.used.get_mut(id as usize / 64) {
            *w |= 1u64 << (id % 64);
        }
    }

    /// The lowest free id in `lo..hi`, id 0 excluded.
    fn find_free(&self, lo: u32, hi: u32) -> Option<u32> {
        let lo = lo.max(1);
        let mut id = lo;
        while id < hi {
            let w = *self.used.get(id as usize / 64)?;
            // Bits below `id` in this word are outside the range: count them used.
            let free = !(w | ((1u64 << (id % 64)) - 1));
            if free != 0 {
                let found = (id / 64) * 64 + free.trailing_zeros();
                return (found < hi).then_some(found);
            }
            id = (id / 64 + 1) * 64;
        }
        None
    }
}

/// A key slot [`IdIndex`] holds no id in. Also `ThreadId::NONE`'s value.
const EMPTY: u32 = u32::MAX;

/// A fixed open-addressed map from an id to its table slot: linear probing
/// from `id & (CAP - 1)`, removal by backward shift. `CAP` is a power of
/// two, and a slot fits a `u16`.
pub struct IdIndex<const CAP: usize> {
    keys: [u32; CAP],
    slots: [u16; CAP],
    len: usize,
}

impl<const CAP: usize> IdIndex<CAP> {
    const SHAPE: () = assert!(CAP.is_power_of_two(), "IdIndex: CAP is a power of two");
    const MASK: usize = CAP - 1;

    /// An empty index.
    pub const fn new() -> Self {
        #[allow(clippy::let_unit_value, reason = "forces the SHAPE assertion")]
        let () = Self::SHAPE;
        Self {
            keys: [EMPTY; CAP],
            slots: [0; CAP],
            len: 0,
        }
    }

    /// Ids held.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// True when no id is held.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Map `id` to `slot`, replacing an entry for `id`. False, with nothing
    /// changed, when the index is full, `id` is the sentinel or at or past
    /// `PID_MAX`, or `slot` does not fit a `u16`.
    #[must_use]
    pub fn insert(&mut self, id: u32, slot: usize) -> bool {
        if !Self::valid(id) {
            return false;
        }
        let Ok(slot) = u16::try_from(slot) else {
            return false;
        };
        if let Some(i) = self.find(id) {
            if let Some(s) = self.slots.get_mut(i) {
                *s = slot;
            }
            return true;
        }
        if self.len >= CAP {
            return false;
        }
        let mut i = id as usize & Self::MASK;
        for _ in 0..CAP {
            if self.keys.get(i) == Some(&EMPTY) {
                if let (Some(k), Some(s)) = (self.keys.get_mut(i), self.slots.get_mut(i)) {
                    *k = id;
                    *s = slot;
                    self.len += 1;
                    return true;
                }
                return false;
            }
            i = (i + 1) & Self::MASK;
        }
        false
    }

    /// `id`'s slot. `None` at once for the sentinel or an id at or past
    /// `PID_MAX`, which a user `pid` can name.
    pub fn get(&self, id: u32) -> Option<usize> {
        if !Self::valid(id) {
            return None;
        }
        let i = self.find(id)?;
        self.slots.get(i).map(|s| usize::from(*s))
    }

    /// Drop `id`'s entry and return its slot. `None` at once for the
    /// sentinel or an id at or past `PID_MAX`.
    pub fn remove(&mut self, id: u32) -> Option<usize> {
        if !Self::valid(id) {
            return None;
        }
        let mut hole = self.find(id)?;
        let slot = usize::from(*self.slots.get(hole)?);
        // Backward shift: pull each later entry of the probe run into the
        // hole unless its home lies between the hole and where it sits.
        let mut j = hole;
        for _ in 0..CAP {
            j = (j + 1) & Self::MASK;
            let key = *self.keys.get(j)?;
            if key == EMPTY {
                break;
            }
            let home = key as usize & Self::MASK;
            let from_home = j.wrapping_sub(home) & Self::MASK;
            let from_hole = j.wrapping_sub(hole) & Self::MASK;
            if from_home >= from_hole {
                let s = *self.slots.get(j)?;
                if let (Some(k), Some(v)) = (self.keys.get_mut(hole), self.slots.get_mut(hole)) {
                    *k = key;
                    *v = s;
                }
                hole = j;
            }
        }
        if let Some(k) = self.keys.get_mut(hole) {
            *k = EMPTY;
        }
        self.len -= 1;
        Some(slot)
    }

    const fn valid(id: u32) -> bool {
        id != EMPTY && id < PID_MAX
    }

    /// Where `id`'s key sits.
    fn find(&self, id: u32) -> Option<usize> {
        let mut i = id as usize & Self::MASK;
        for _ in 0..CAP {
            let k = *self.keys.get(i)?;
            if k == id {
                return Some(i);
            }
            if k == EMPTY {
                return None;
            }
            i = (i + 1) & Self::MASK;
        }
        None
    }
}

impl<const CAP: usize> Default for IdIndex<CAP> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(pid_max: u32) -> std::boxed::Box<PidAlloc> {
        std::boxed::Box::new(PidAlloc::new(pid_max))
    }

    #[test]
    fn pid_alloc_none_reused_before_wrap() {
        let mut p = boxed(PID_MAX);
        let mut prev = 0;
        for _ in 0..100 {
            let id = p.alloc().unwrap();
            assert!(id > prev, "{id} after {prev}: reused before the wrap");
            assert!(p.free(id));
            prev = id;
        }
        assert_eq!(prev, 100);
    }

    #[test]
    fn pid_alloc_wraps_to_300_past_pid_max() {
        let mut p = boxed(400);
        for want in 1..400 {
            let id = p.alloc().unwrap();
            assert_eq!(id, want);
            assert!(p.free(id));
        }
        assert_eq!(p.alloc(), Some(300), "the first id after the wrap");
        assert!(p.free(300));
        assert_eq!(p.alloc(), Some(301));
    }

    #[test]
    fn pid_alloc_skips_held_id_at_wrap() {
        let mut p = boxed(400);
        for _ in 1..300 {
            let id = p.alloc().unwrap();
            assert!(p.free(id));
        }
        // Pid 300's process is reaped while a process group still carries it.
        assert_eq!(p.alloc(), Some(300));
        assert!(p.hold(300));
        assert!(p.free(300));
        assert!(p.in_use(300));
        for _ in 301..400 {
            let id = p.alloc().unwrap();
            assert!(p.free(id));
        }
        assert_eq!(p.alloc(), Some(301), "the wrap skips the held id");
        assert!(p.free(301));
        // The group's use goes: at the next wrap the id comes back.
        assert!(p.free(300));
        assert!(!p.in_use(300));
        for _ in 302..400 {
            let id = p.alloc().unwrap();
            assert!(p.free(id));
        }
        assert_eq!(p.alloc(), Some(300));
    }

    #[test]
    fn pid_alloc_none_when_full() {
        let mut p = boxed(400);
        for want in 1..400 {
            assert_eq!(p.alloc(), Some(want));
        }
        assert_eq!(p.alloc(), None);
        assert!(p.free(350));
        assert_eq!(p.alloc(), Some(350));
        assert_eq!(p.alloc(), None);
        // An id below the wrap is not handed out again.
        assert!(p.free(5));
        assert_eq!(p.alloc(), None);
    }

    #[test]
    fn pid_alloc_hold_reserves_free_id() {
        let mut p = boxed(PID_MAX);
        assert!(p.hold(5));
        for want in [1, 2, 3, 4, 6] {
            assert_eq!(p.alloc(), Some(want));
        }
        assert!(p.hold(5), "a second use");
        assert!(p.free(5));
        assert!(p.in_use(5));
        assert!(p.free(5));
        assert!(!p.in_use(5));
        assert!(!p.free(5), "no use left to drop");
        assert!(!p.hold(0), "id 0 is the bootstrap's");
        assert!(!p.hold(PID_MAX));
    }

    #[test]
    fn pid_alloc_clamps_pid_max() {
        assert_eq!(PidAlloc::new(0).pid_max(), PID_WRAP + 1);
        assert_eq!(PidAlloc::new(PID_WRAP).pid_max(), PID_WRAP + 1);
        assert_eq!(PidAlloc::new(u32::MAX).pid_max(), PID_MAX);
        let mut p = boxed(0);
        for want in 1..=PID_WRAP {
            assert_eq!(p.alloc(), Some(want));
            assert!(p.free(want));
        }
        assert_eq!(p.alloc(), Some(PID_WRAP));
        let mut p = boxed(u32::MAX);
        p.last = PID_MAX - 2;
        assert_eq!(p.alloc(), Some(PID_MAX - 1));
        assert_eq!(p.alloc(), Some(PID_WRAP));
    }

    #[test]
    fn id_index_insert_get_remove() {
        let mut ix = IdIndex::<8>::new();
        assert!(ix.insert(5, 2));
        assert!(ix.insert(1000, 7));
        assert_eq!(ix.get(5), Some(2));
        assert_eq!(ix.get(1000), Some(7));
        assert_eq!(ix.get(6), None);
        assert!(ix.insert(5, 3), "replaces");
        assert_eq!(ix.get(5), Some(3));
        assert_eq!(ix.len(), 2);
        assert_eq!(ix.remove(5), Some(3));
        assert_eq!(ix.get(5), None);
        assert_eq!(ix.remove(5), None);
        assert_eq!(ix.len(), 1);
    }

    #[test]
    fn id_index_remove_keeps_probe_chain() {
        let mut ix = IdIndex::<8>::new();
        // 1, 9 and 17 share home 1; 2 lands behind them.
        for (id, slot) in [(1, 0), (9, 1), (17, 2), (2, 3)] {
            assert!(ix.insert(id, slot));
        }
        assert_eq!(ix.remove(9), Some(1));
        assert_eq!(ix.get(1), Some(0));
        assert_eq!(ix.get(17), Some(2));
        assert_eq!(ix.get(2), Some(3));
        // A run that wraps past the end: 7, 15, 23 share home 7.
        for (id, slot) in [(7, 4), (15, 5), (23, 6)] {
            assert!(ix.insert(id, slot));
        }
        assert_eq!(ix.remove(7), Some(4));
        assert_eq!(ix.get(15), Some(5));
        assert_eq!(ix.get(23), Some(6));
        assert_eq!(ix.get(1), Some(0));
        assert_eq!(ix.get(17), Some(2));
        assert_eq!(ix.get(2), Some(3));
        assert_eq!(ix.len(), 5);
    }

    #[test]
    fn id_index_rejects_sentinel_and_out_of_range() {
        let mut ix = IdIndex::<8>::new();
        assert!(!ix.insert(u32::MAX, 0));
        assert!(!ix.insert(PID_MAX, 0));
        assert!(!ix.insert(3, 1 << 16), "a slot past u16");
        assert_eq!(ix.get(u32::MAX), None);
        assert_eq!(ix.get(PID_MAX), None);
        assert_eq!(ix.remove(u32::MAX), None);
        assert_eq!(ix.remove(PID_MAX), None);
        assert!(ix.is_empty());
    }

    #[test]
    fn id_index_full_refuses_insert() {
        let mut ix = IdIndex::<4>::new();
        for id in 0..4 {
            assert!(ix.insert(id * 4, id as usize));
        }
        assert!(!ix.insert(100, 9));
        assert_eq!(ix.get(100), None);
        assert!(ix.insert(8, 5), "a held id still updates");
        for id in 0..4 {
            assert!(ix.get(id * 4).is_some());
        }
        assert_eq!(ix.remove(0), Some(0));
        assert!(ix.insert(100, 9));
        assert_eq!(ix.get(100), Some(9));
    }
}
