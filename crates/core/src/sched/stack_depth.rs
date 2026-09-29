//! Kernel stack depth measurement (DESIGN §4.5, TESTING §8.2).
//!
//! A `kernel_tests` build fills every new kernel stack with [`PATTERN`]
//! and scans it when its thread exits and at the end of the in-guest run:
//! the deepest word that no longer holds the pattern bounds what the
//! thread used. A stack's budget is its size minus [`MARGIN`], the room
//! DESIGN §4.5 keeps for a hard-IRQ top half and its entry frame.
//! [`DepthTable`] keeps the deepest use per stack size and the last few
//! records. Pure data; the kernel half fills, scans and prints.

use core::fmt;

/// What an unused stack word holds.
pub const PATTERN: u64 = 0x57AC_D3E7_57AC_D3E7;

/// What a stack keeps free for a top half and its entry frame (DESIGN §4.5).
pub const MARGIN: usize = 4096;

/// Stack sizes [`DepthTable`] tracks; a record of a further size is `lost`.
pub const SIZES: usize = 8;

/// Records [`DepthTable::recent`] can find.
pub const RECENT: usize = 16;

/// The deepest use a stack of `size` bytes may reach.
pub const fn budget(size: usize) -> usize {
    size.saturating_sub(MARGIN)
}

/// Fill `words` with [`PATTERN`].
pub fn fill(words: &mut [u64]) {
    for w in words.iter_mut() {
        *w = PATTERN;
    }
}

/// Bytes used, from the lowest word that is not [`PATTERN`] to the top.
/// `words[0]` is the lowest address; a stack grows down from the end.
pub fn used(words: &[u64]) -> usize {
    match words.iter().position(|&w| w != PATTERN) {
        Some(i) => (words.len() - i) * 8,
        None => 0,
    }
}

/// [`used`] over a stack another context may still write, with volatile
/// reads.
///
/// # Safety
/// `base` is 8-byte aligned and `[base, base + n)` words are mapped and
/// readable for the whole call.
pub unsafe fn used_volatile(base: *const u64, n: usize) -> usize {
    let mut i = 0usize;
    while i < n {
        // SAFETY: `i < n`, and `[base, base + n)` words are mapped, aligned
        // and readable for the whole call: the `# Safety` contract of this
        // unsafe fn, which its caller keeps; established here.
        let w = unsafe { core::ptr::read_volatile(base.add(i)) };
        if w != PATTERN {
            return (n - i) * 8;
        }
        i += 1;
    }
    0
}

/// One measurement: `used` bytes of a `size`-byte stack, by thread `tid`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deepest {
    pub size: usize,
    pub used: usize,
    pub tid: u32,
    pub name: &'static str,
}

impl Deepest {
    /// The use is over [`budget`] of its size.
    pub fn over(&self) -> bool {
        self.used > budget(self.size)
    }
}

/// The deepest use per stack size, and the last [`RECENT`] records.
pub struct DepthTable {
    sizes: [Option<Deepest>; SIZES],
    lost: u32,
    ring: [Option<Deepest>; RECENT],
    next: usize,
}

impl Default for DepthTable {
    fn default() -> Self {
        Self::new()
    }
}

impl DepthTable {
    pub const fn new() -> Self {
        Self {
            sizes: [None; SIZES],
            lost: 0,
            ring: [None; RECENT],
            next: 0,
        }
    }

    /// Keep `d` if it is the deepest of its size, and in the ring.
    pub fn record(&mut self, d: Deepest) {
        if let Some(slot) = self.ring.get_mut(self.next % RECENT) {
            *slot = Some(d);
        }
        self.next = (self.next + 1) % RECENT;
        let mut free = None;
        for (i, s) in self.sizes.iter_mut().enumerate() {
            match s {
                Some(cur) if cur.size == d.size => {
                    if d.used > cur.used {
                        *cur = d;
                    }
                    return;
                }
                Some(_) => {}
                None => {
                    if free.is_none() {
                        free = Some(i);
                    }
                }
            }
        }
        match free.and_then(|i| self.sizes.get_mut(i)) {
            Some(s) => *s = Some(d),
            None => self.lost = self.lost.saturating_add(1),
        }
    }

    /// The latest record of thread `tid`, if the ring still holds it.
    pub fn recent(&self, tid: u32) -> Option<Deepest> {
        let mut k = 0usize;
        while k < RECENT {
            // Newest first: the slot before `next`, going back.
            let i = (self.next + RECENT - 1 - k) % RECENT;
            if let Some(Some(d)) = self.ring.get(i)
                && d.tid == tid
            {
                return Some(*d);
            }
            k += 1;
        }
        None
    }

    /// Records dropped because [`SIZES`] sizes were already tracked.
    pub fn lost(&self) -> u32 {
        self.lost
    }

    /// The deepest use of each size seen, in the order sizes first came.
    pub fn deepest(&self) -> impl Iterator<Item = Deepest> + '_ {
        self.sizes.iter().filter_map(|s| *s)
    }

    /// How many sizes have a record.
    pub fn len(&self) -> usize {
        self.deepest().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `vibeOS: stack: <size> used <used> of <budget> by tid <tid> <name>`.
pub struct Line<'a>(pub &'a Deepest);

impl fmt::Display for Line<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = self.0;
        write!(
            f,
            "vibeOS: stack: {} used {} of {} by tid {} {}",
            d.size,
            d.used,
            budget(d.size),
            d.tid,
            d.name
        )
    }
}

/// `vibeOS: stack: report <n> sizes <lost> lost`.
pub struct Report<'a>(pub &'a DepthTable);

impl fmt::Display for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "vibeOS: stack: report {} sizes {} lost",
            self.0.len(),
            self.0.lost()
        )
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::format;
    use std::vec;

    use super::*;

    fn d(size: usize, used: usize, tid: u32) -> Deepest {
        Deepest {
            size,
            used,
            tid,
            name: "t",
        }
    }

    #[test]
    fn budget_saturates() {
        assert_eq!(budget(16384), 12288);
        assert_eq!(budget(4096), 0);
        assert_eq!(budget(100), 0);
    }

    #[test]
    fn fresh_stack_uses_nothing() {
        let mut s = vec![0u64; 512];
        fill(&mut s);
        assert_eq!(used(&s), 0);
        // SAFETY: `s` is a live, aligned Vec of 512 words; established here.
        assert_eq!(unsafe { used_volatile(s.as_ptr(), s.len()) }, 0);
    }

    #[test]
    fn deepest_word_counts_from_the_top() {
        let mut s = vec![0u64; 512];
        fill(&mut s);
        // A thread wrote the top 100 words, and one stray word deeper.
        for w in s[412..].iter_mut() {
            *w = 1;
        }
        assert_eq!(used(&s), 100 * 8);
        s[300] = 0;
        assert_eq!(used(&s), 212 * 8);
        // SAFETY: `s` is a live, aligned Vec of 512 words; established here.
        assert_eq!(unsafe { used_volatile(s.as_ptr(), s.len()) }, 212 * 8);
        s[0] = 7;
        assert_eq!(used(&s), 512 * 8);
    }

    #[test]
    fn table_keeps_max_per_size() {
        let mut t = DepthTable::new();
        assert!(t.is_empty());
        t.record(d(16384, 3000, 1));
        t.record(d(16384, 5000, 2));
        t.record(d(16384, 4000, 3));
        t.record(d(65536, 9000, 4));
        assert_eq!(t.len(), 2);
        let v: std::vec::Vec<_> = t.deepest().collect();
        assert_eq!(v[0], d(16384, 5000, 2));
        assert_eq!(v[1], d(65536, 9000, 4));
        assert_eq!(t.lost(), 0);
    }

    #[test]
    fn ninth_size_is_lost() {
        let mut t = DepthTable::new();
        for i in 0..SIZES {
            t.record(d(4096 * (i + 1), 8, i as u32));
        }
        assert_eq!(t.lost(), 0);
        t.record(d(4096 * 100, 8, 99));
        assert_eq!(t.lost(), 1);
        assert_eq!(t.len(), SIZES);
        // A known size still records.
        t.record(d(4096, 16, 50));
        assert_eq!(t.lost(), 1);
    }

    #[test]
    fn recent_finds_newest_until_overwritten() {
        let mut t = DepthTable::new();
        t.record(d(16384, 100, 7));
        t.record(d(16384, 200, 7));
        assert_eq!(t.recent(7).map(|x| x.used), Some(200));
        assert_eq!(t.recent(8), None);
        for i in 0..RECENT as u32 {
            t.record(d(16384, 1, 100 + i));
        }
        assert_eq!(t.recent(7), None);
        assert_eq!(t.recent(100 + RECENT as u32 - 1).map(|x| x.used), Some(1));
    }

    #[test]
    fn over_budget() {
        assert!(!d(16384, 12288, 1).over());
        assert!(d(16384, 12289, 1).over());
        assert!(d(16384, 13312, 1).over());
    }

    #[test]
    fn lines_render() {
        let x = Deepest {
            size: 16384,
            used: 13560,
            tid: 42,
            name: "stack-plant",
        };
        assert_eq!(
            format!("{}", Line(&x)),
            "vibeOS: stack: 16384 used 13560 of 12288 by tid 42 stack-plant"
        );
        let mut t = DepthTable::new();
        t.record(x);
        assert_eq!(
            format!("{}", Report(&t)),
            "vibeOS: stack: report 1 sizes 0 lost"
        );
    }
}
