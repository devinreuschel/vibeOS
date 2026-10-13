//! Linux arm64 generation ASIDs (DESIGN §11.2, PORTABILITY §11.2).
//!
//! Each address space holds a generation above its ASID bits. A switch
//! publishes this CPU's `active_asid` with a compare-exchange that
//! succeeds only while that generation is current. A rollover starts a
//! new generation, exchanges each CPU's `active_asid` for 0, reserves
//! what it held, and marks every CPU flush-pending. ASID 0 is reserved.
//! Where `2^bits - 1` does not exceed the CPU count, ASIDs are off.

use crate::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use crate::kalloc::TryBox;
use crate::machine::MAX_CPUS;
use crate::sync::variant::{self, Site};

/// Packed `generation | asid`. Generation lives above `bits`.
pub type AsidWord = u64;

/// Why ASIDs are off: usable numbers do not exceed the CPU count.
pub const DISABLED_REASON: &str = "usable ASIDs do not exceed possible CPUs";

/// 16-bit ASIDs need 1024 words. Loom models use 2-bit ASIDs only.
#[cfg(not(loom))]
const BITMAP_WORDS: usize = 1024;
#[cfg(loom)]
const BITMAP_WORDS: usize = 1;

/// Generation ASID allocator. Host tests use 2-bit / 2-CPU; the kernel
/// uses 8- or 16-bit and `MAX_CPUS`.
pub struct AsidAlloc {
    bits: u32,
    ncpus: u32,
    /// Current generation, already shifted above `bits`.
    generation: AtomicU64,
    next: AtomicU32,
    used: [AtomicU64; BITMAP_WORDS],
    /// Full `generation|asid` reserved at the last rollover, per CPU.
    reserved: [AtomicU64; MAX_CPUS],
    active: [AtomicU64; MAX_CPUS],
    flush_pending: [AtomicBool; MAX_CPUS],
    lock: AtomicBool,
}

impl AsidAlloc {
    /// `None` when `2^bits - 1 <= ncpus` (no ASIDs).
    pub fn new(bits: u32, ncpus: u32) -> Option<Self> {
        if bits == 0 || bits > 16 || ncpus == 0 || ncpus as usize > MAX_CPUS {
            return None;
        }
        let usable = (1u32 << bits).saturating_sub(1);
        if usable <= ncpus {
            return None;
        }
        Some(Self {
            bits,
            ncpus,
            generation: AtomicU64::new(1u64 << bits),
            next: AtomicU32::new(1),
            used: core::array::from_fn(|_| AtomicU64::new(0)),
            reserved: core::array::from_fn(|_| AtomicU64::new(0)),
            active: core::array::from_fn(|_| AtomicU64::new(0)),
            flush_pending: core::array::from_fn(|_| AtomicBool::new(false)),
            lock: AtomicBool::new(false),
        })
    }

    /// Heap form of [`Self::new`]. Writes fields in place so a 16 KiB
    /// kernel stack does not hold the 16-bit bitmap.
    pub fn try_boxed(bits: u32, ncpus: u32) -> Option<TryBox<Self>> {
        if bits == 0 || bits > 16 || ncpus == 0 || ncpus as usize > MAX_CPUS {
            return None;
        }
        let usable = (1u32 << bits).saturating_sub(1);
        if usable <= ncpus {
            return None;
        }
        let slot = TryBox::<Self>::try_new_uninit().ok()?;
        let raw = TryBox::into_raw(slot);
        // SAFETY: `raw` is the exclusive `MaybeUninit<Self>` `try_new_uninit`
        // allocated. Each field is written once, then the pointer is a
        // live `Self`. established here.
        unsafe {
            let p = raw.cast::<Self>();
            core::ptr::addr_of_mut!((*p).bits).write(bits);
            core::ptr::addr_of_mut!((*p).ncpus).write(ncpus);
            core::ptr::addr_of_mut!((*p).generation).write(AtomicU64::new(1u64 << bits));
            core::ptr::addr_of_mut!((*p).next).write(AtomicU32::new(1));
            let used = core::ptr::addr_of_mut!((*p).used).cast::<AtomicU64>();
            for i in 0..BITMAP_WORDS {
                used.add(i).write(AtomicU64::new(0));
            }
            let reserved = core::ptr::addr_of_mut!((*p).reserved).cast::<AtomicU64>();
            for i in 0..MAX_CPUS {
                reserved.add(i).write(AtomicU64::new(0));
            }
            let active = core::ptr::addr_of_mut!((*p).active).cast::<AtomicU64>();
            for i in 0..MAX_CPUS {
                active.add(i).write(AtomicU64::new(0));
            }
            let flush = core::ptr::addr_of_mut!((*p).flush_pending).cast::<AtomicBool>();
            for i in 0..MAX_CPUS {
                flush.add(i).write(AtomicBool::new(false));
            }
            core::ptr::addr_of_mut!((*p).lock).write(AtomicBool::new(false));
            Some(TryBox::from_raw(p))
        }
    }

    pub fn bits(&self) -> u32 {
        self.bits
    }

    pub fn asid_mask(&self) -> u64 {
        (1u64 << self.bits) - 1
    }

    pub fn generation(&self) -> u64 {
        // Acquire: pairs with the Release store in `rollover`.
        self.generation.load(Ordering::Acquire)
    }

    pub fn pack(&self, asid: u32) -> AsidWord {
        self.generation() | (asid as u64 & self.asid_mask())
    }

    pub fn asid_of(&self, word: AsidWord) -> u32 {
        (word & self.asid_mask()) as u32
    }

    pub fn gen_of(&self, word: AsidWord) -> u64 {
        word & !self.asid_mask()
    }

    pub fn current(&self, word: AsidWord) -> bool {
        word != 0 && self.gen_of(word) == self.generation()
    }

    /// Fast path: CAS `active[cpu]` to `word` while `word`'s generation
    /// is current. A store instead of CAS is `Site::AsidFastStore`.
    pub fn switch_fast(&self, cpu: u32, word: AsidWord) -> bool {
        if cpu >= self.ncpus || !self.current(word) {
            return false;
        }
        let slot = &self.active[cpu as usize];
        // Acquire: pairs with the Release store in `switch_locked_inner`
        // and the AcqRel swap in `rollover`.
        let cur = slot.load(Ordering::Acquire);
        // Rollover exchanges this slot for 0. A CAS from 0 would
        // republish a word whose generation is already dead.
        if cur == 0 {
            return false;
        }
        if variant::pick(Site::AsidFastStore, false, true) {
            // Relaxed: the weakened fast path; pairs with nothing.
            slot.store(word, Ordering::Relaxed);
            return self.current(word);
        }
        // AcqRel: pairs with the Acquire load in `switch_fast` and the
        // AcqRel swap in `rollover`. Acquire: the CAS failure load; pairs
        // with the same.
        slot.compare_exchange(cur, word, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            && self.current(word)
    }

    /// Slow path under the allocator lock. Allocates or keeps `word` if
    /// it is still free or reserved in the current generation.
    pub fn switch_locked(&self, cpu: u32, word: AsidWord) -> AsidWord {
        self.with_lock(|| self.switch_locked_inner(cpu, word))
    }

    fn switch_locked_inner(&self, cpu: u32, word: AsidWord) -> AsidWord {
        if cpu >= self.ncpus {
            return 0;
        }
        let keep = self.asid_of(word);
        let out = if self.current(word) && keep != 0 {
            self.set_used(keep);
            self.pack(keep)
        } else if let Some(a) = self.keep_reserved(word) {
            self.set_used(a);
            self.pack(a)
        } else {
            self.alloc_fresh()
        };
        // Release: pairs with the Acquire load in `switch_fast` and `active`.
        self.active[cpu as usize].store(out, Ordering::Release);
        out
    }

    /// A space running at rollover keeps its number when `word` is the
    /// exact reserved value (old generation included).
    fn keep_reserved(&self, word: AsidWord) -> Option<u32> {
        if word == 0 {
            return None;
        }
        let keep = self.asid_of(word);
        if keep == 0 {
            return None;
        }
        let mut hit = false;
        for i in 0..self.ncpus as usize {
            // Relaxed: the allocator lock is held; pairs with nothing.
            if self.reserved[i].load(Ordering::Relaxed) == word {
                // Relaxed: the allocator lock is held; pairs with nothing.
                self.reserved[i].store(self.pack(keep), Ordering::Relaxed);
                hit = true;
            }
        }
        if hit { Some(keep) } else { None }
    }

    /// Allocate a fresh ASID. Caller holds the lock.
    fn alloc_fresh(&self) -> AsidWord {
        if let Some(a) = self.find_free() {
            self.set_used(a);
            return self.pack(a);
        }
        self.rollover();
        if let Some(a) = self.find_free() {
            self.set_used(a);
            return self.pack(a);
        }
        0
    }

    /// New generation; exchange each `active` for 0 and reserve it.
    /// `Site::AsidSkipReserve` skips the reservation.
    fn rollover(&self) {
        let mask = self.asid_mask();
        let step = mask + 1;
        // Release: pairs with the Acquire load in `generation`.
        let _ = self.generation.fetch_add(step, Ordering::Release);
        for i in 0..self.ncpus as usize {
            // AcqRel: pairs with the Acquire load and AcqRel CAS in `switch_fast`.
            let old = self.active[i].swap(0, Ordering::AcqRel);
            if variant::pick(Site::AsidSkipReserve, false, true) {
                // Relaxed: the allocator lock is held; pairs with nothing.
                self.reserved[i].store(0, Ordering::Relaxed);
            } else {
                // Relaxed: the allocator lock is held; pairs with nothing.
                self.reserved[i].store(old, Ordering::Relaxed);
            }
            // Release: pairs with the Acquire load in `flush_pending`.
            self.flush_pending[i].store(true, Ordering::Release);
        }
        for a in self.used.iter().take(self.bitmap_words()) {
            // Relaxed: the allocator lock is held; pairs with nothing.
            a.store(0, Ordering::Relaxed);
        }
        for i in 0..self.ncpus as usize {
            // Relaxed: the allocator lock is held; pairs with nothing.
            let asid = (self.reserved[i].load(Ordering::Relaxed) & mask) as u32;
            if asid != 0 {
                self.set_used(asid);
            }
        }
        // Relaxed: the allocator lock is held; pairs with nothing.
        self.next.store(1, Ordering::Relaxed);
    }

    /// Rollover under the allocator lock. Loom races this with `switch_fast`.
    #[cfg(all(test, loom))]
    fn force_rollover(&self) {
        self.with_lock(|| self.rollover());
    }

    pub fn flush_pending(&self, cpu: u32) -> bool {
        // Acquire: pairs with the Release store in `rollover`.
        self.flush_pending
            .get(cpu as usize)
            .is_some_and(|a| a.load(Ordering::Acquire))
    }

    /// Clear flush-pending after the local `vmalle1` sequence.
    pub fn clear_flush(&self, cpu: u32) {
        if let Some(a) = self.flush_pending.get(cpu as usize) {
            // Release: pairs with the Acquire load in `flush_pending`.
            a.store(false, Ordering::Release);
        }
    }

    pub fn active(&self, cpu: u32) -> AsidWord {
        // Acquire: pairs with the Release store in `switch_locked_inner`.
        self.active
            .get(cpu as usize)
            .map(|a| a.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    fn find_free(&self) -> Option<u32> {
        let max = 1u32 << self.bits;
        // Relaxed: the allocator lock is held; pairs with nothing.
        let start = self.next.load(Ordering::Relaxed).max(1);
        let mut a = start;
        for _ in 1..max {
            if a >= max {
                a = 1;
            }
            if !self.is_used(a) {
                let n = if a + 1 >= max { 1 } else { a + 1 };
                // Relaxed: the allocator lock is held; pairs with nothing.
                self.next.store(n, Ordering::Relaxed);
                return Some(a);
            }
            a += 1;
        }
        None
    }

    fn bitmap_words(&self) -> usize {
        let bits = 1usize << self.bits;
        bits.div_ceil(64)
    }

    fn is_used(&self, asid: u32) -> bool {
        self.bit(&self.used, asid)
    }
    fn set_used(&self, asid: u32) {
        self.set_bit(&self.used, asid);
    }

    fn bit(&self, map: &[AtomicU64; BITMAP_WORDS], asid: u32) -> bool {
        let i = asid as usize;
        let w = i / 64;
        let b = i % 64;
        // Relaxed: the allocator lock is held; pairs with nothing.
        map.get(w)
            .is_some_and(|a| a.load(Ordering::Relaxed) & (1u64 << b) != 0)
    }

    fn set_bit(&self, map: &[AtomicU64; BITMAP_WORDS], asid: u32) {
        let i = asid as usize;
        let w = i / 64;
        let b = i % 64;
        if let Some(a) = map.get(w) {
            // Relaxed: the allocator lock is held; pairs with nothing.
            let _ = a.fetch_or(1u64 << b, Ordering::Relaxed);
        }
    }

    fn with_lock<R>(&self, f: impl FnOnce() -> R) -> R {
        while self
            .lock
            // Acquire: pairs with the Release store in `with_lock`. Relaxed:
            // the failure load; pairs with nothing.
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            crate::atomic::spin_loop();
        }
        let r = f();
        // Release: pairs with the Acquire compare-exchange in `with_lock`.
        self.lock.store(false, Ordering::Release);
        r
    }
}

/// A switch: try the fast path, then the lock.
pub fn switch(alloc: &AsidAlloc, cpu: u32, word: &mut AsidWord) -> AsidWord {
    if alloc.switch_fast(cpu, *word) {
        return *word;
    }
    let n = alloc.switch_locked(cpu, *word);
    *word = n;
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_when_usable_le_cpus() {
        assert!(AsidAlloc::new(2, 3).is_none());
        assert!(AsidAlloc::new(1, 1).is_none());
        assert!(AsidAlloc::new(2, 2).is_some());
        assert!(!DISABLED_REASON.is_empty());
    }

    #[test]
    fn try_boxed_matches_new() {
        assert!(AsidAlloc::try_boxed(2, 3).is_none());
        let a = AsidAlloc::try_boxed(4, 2).expect("4-bit boxed");
        assert_eq!(a.bits(), 4);
        assert_eq!(a.asid_mask(), 0xF);
    }

    #[test]
    fn asid_zero_never_allocated() {
        let a = AsidAlloc::new(2, 2).unwrap();
        let mut w = 0;
        for _ in 0..8 {
            let g = switch(&a, 0, &mut w);
            assert_ne!(a.asid_of(g), 0);
        }
    }

    #[test]
    fn rollover_preserves_running() {
        let a = AsidAlloc::new(2, 2).unwrap();
        let mut s0 = 0;
        let mut s1 = 0;
        let w0 = switch(&a, 0, &mut s0);
        let w1 = switch(&a, 1, &mut s1);
        assert_ne!(a.asid_of(w0), a.asid_of(w1));
        // Exhaust the one remaining ASID, then rollover.
        let mut s2 = 0;
        let _ = switch(&a, 0, &mut s2);
        let mut s3 = 0;
        let w3 = switch(&a, 0, &mut s3);
        assert!(a.flush_pending(0) || a.flush_pending(1) || a.generation() != a.gen_of(w0));
        let n1 = a.asid_of(w1);
        a.clear_flush(1);
        let again = switch(&a, 1, &mut s1);
        assert_eq!(a.asid_of(again), n1);
        assert_ne!(a.gen_of(again), a.gen_of(w1));
        let _ = w3;
    }

    /// 2-bit ASIDs, 2 CPUs, many spaces: one ASID never names two
    /// spaces on one CPU since that CPU's last flush.
    #[test]
    fn thousands_of_rollovers_no_alias() {
        let a = AsidAlloc::new(2, 2).unwrap();
        let mut words = [0u64; 64];
        let mut seen0: [(u32, usize); 4] = [(0, usize::MAX); 4];
        let mut seen1 = seen0;
        let mut gen0 = a.generation();
        let mut gen1 = gen0;
        for i in 0..4000 {
            let cpu = (i % 2) as u32;
            let slot = i % words.len();
            if a.flush_pending(cpu) {
                a.clear_flush(cpu);
                if cpu == 0 {
                    seen0 = [(0, usize::MAX); 4];
                    gen0 = a.generation();
                } else {
                    seen1 = [(0, usize::MAX); 4];
                    gen1 = a.generation();
                }
            }
            let w = switch(&a, cpu, &mut words[slot]);
            let asid = a.asid_of(w) as usize;
            assert!(asid < 4 && asid != 0);
            let seen = if cpu == 0 { &mut seen0 } else { &mut seen1 };
            let cur_gen = if cpu == 0 { gen0 } else { gen1 };
            if a.gen_of(w) == cur_gen && seen[asid].1 != usize::MAX && seen[asid].1 != slot {
                panic!(
                    "asid {asid} named spaces {} and {slot} on cpu {cpu}",
                    seen[asid].1
                );
            }
            seen[asid] = (a.asid_of(w), slot);
        }
    }
}

#[cfg(all(test, loom))]
mod loom_models {
    use super::*;
    use crate::sync::variant::{Bound, Site, check};
    use loom::sync::Arc;
    use loom::thread;

    /// CPU 0 publishes `first` on the fast path while CPU 1 rolls over
    /// and allocates a new space. A store instead of CAS can leave a
    /// stale generation in `active[0]`; skipping reserve can give the
    /// new space CPU 0's running ASID. Bound: 2 threads (CPU 0 one
    /// fast switch, CPU 1 one rollover and one alloc), 3 preemptions.
    fn model(v: Option<Site>) {
        let bound = Bound {
            threads: 2,
            preemptions: 3,
        };
        check(v, bound, || {
            let alloc = Arc::new(AsidAlloc::new(2, 2).unwrap());
            let mut w = 0u64;
            let first = switch(&alloc, 0, &mut w);
            let a = alloc.clone();
            let t = thread::spawn(move || {
                a.force_rollover();
                let mut other = 0u64;
                let n = switch(&a, 1, &mut other);
                assert_ne!(
                    a.asid_of(n),
                    a.asid_of(first),
                    "asid aliased across rollover"
                );
            });
            let _ = alloc.switch_fast(0, first);
            t.join().unwrap();
            let act = alloc.active(0);
            assert!(
                act == 0 || alloc.current(act),
                "stale generation in active[0]"
            );
        });
    }

    #[test]
    fn loom_asid_switch_vs_rollover() {
        model(None);
    }

    #[test]
    #[should_panic(expected = "stale generation in active[0]")]
    fn loom_asid_fast_store_fails() {
        model(Some(Site::AsidFastStore));
    }

    #[test]
    #[should_panic(expected = "asid aliased across rollover")]
    fn loom_asid_skip_reserve_fails() {
        model(Some(Site::AsidSkipReserve));
    }
}
