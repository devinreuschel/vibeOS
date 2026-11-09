//! Linux arm64 generation ASIDs (DESIGN §11.2, PORTABILITY §11.2).
//!
//! Each address space holds a generation above its ASID bits. A switch
//! publishes this CPU's `active_asid` with a compare-exchange that
//! succeeds only while that generation is current. A rollover starts a
//! new generation, exchanges each CPU's `active_asid` for 0, reserves
//! what it held, and marks every CPU flush-pending. ASID 0 is reserved.
//! Where `2^bits - 1` does not exceed the CPU count, ASIDs are off.
//!
//! An address space's word is one [`AtomicU64`] that every CPU running it
//! shares: [`switch`] reads it for the fast path and, under the allocator
//! lock, reads it again and stores what it allocates, so two CPUs that
//! switch into one space at once give it one ASID.

use crate::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use crate::kalloc::TryBox;
use crate::machine::MAX_CPUS;
use crate::sync::variant::{self, Site};

/// Packed `generation | asid`. Generation lives above `bits`.
pub type AsidWord = u64;

/// Why ASIDs are off: usable numbers do not exceed the CPU count.
pub const DISABLED_REASON: &str = "usable ASIDs do not exceed possible CPUs";

/// The `kernel_tests` cap (ROADMAP §11.2): 15 usable ASIDs, so a few dozen
/// processes force rollovers.
pub const TEST_CAP_BITS: u32 = 4;

/// The allocator's width: 16 where `ID_AA64MMFR0_EL1.ASIDBits` reports
/// 16-bit ASIDs (and `TCR_EL1.AS` is set to match), else 8, then at most
/// `cap` when a cap is given.
pub const fn width(asid16: bool, cap: Option<u32>) -> u32 {
    let hw = if asid16 { 16 } else { 8 };
    match cap {
        Some(c) if c < hw => c,
        _ => hw,
    }
}

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
    /// tests/litmus/asid_cas.litmus
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

    /// Slow path under the allocator lock: reads the space's word again,
    /// keeps its ASID if it is current, reserved, or still free in the
    /// current generation, else allocates one, and stores the result in
    /// both the word and this CPU's `active_asid`.
    pub fn switch_locked(&self, cpu: u32, space: &AtomicU64) -> AsidWord {
        if cpu >= self.ncpus {
            return 0;
        }
        self.with_lock(|| {
            // Relaxed: pairs with nothing; every store of a space's word is
            // under the allocator lock, which orders this load.
            let word = space.load(Ordering::Relaxed);
            let out = self.switch_locked_inner(cpu, word);
            // Relaxed: pairs with nothing; under the allocator lock, as the
            // load above, and the fast path's CAS on `active` orders its reader.
            space.store(out, Ordering::Relaxed);
            out
        })
    }

    fn switch_locked_inner(&self, cpu: u32, word: AsidWord) -> AsidWord {
        let keep = self.asid_of(word);
        let out = if self.current(word) && keep != 0 {
            self.set_used(keep);
            self.pack(keep)
        } else if let Some(a) = self.keep_reserved(word) {
            self.set_used(a);
            self.pack(a)
        } else if word != 0 && keep != 0 && !self.is_used(keep) {
            // Linux's `new_context`: an old number nobody holds in this
            // generation stays with its space. Every CPU that may still map
            // it to another space is flush-pending.
            self.set_used(keep);
            self.pack(keep)
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
    /// tests/litmus/asid_cas.litmus
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
                // A CPU whose slot already reads 0 keeps the reservation it
                // had. Relaxed: the allocator lock is held; pairs with nothing.
                let keep = if old == 0 {
                    // Relaxed: the allocator lock is held; pairs with nothing.
                    self.reserved[i].load(Ordering::Relaxed)
                } else {
                    old
                };
                // Relaxed: the allocator lock is held; pairs with nothing.
                self.reserved[i].store(keep, Ordering::Relaxed);
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

    /// Clear flush-pending and report whether it was set. The caller runs
    /// its `tlbi` after this, so a rollover that sets the flag again later
    /// is seen by the next check instead of being wiped.
    pub fn take_flush(&self, cpu: u32) -> bool {
        // AcqRel: Acquire pairs with the Release store in `rollover`; the swap
        // precedes the caller's `tlbi` in program order.
        self.flush_pending
            .get(cpu as usize)
            .is_some_and(|a| a.swap(false, Ordering::AcqRel))
    }

    pub fn active(&self, cpu: u32) -> AsidWord {
        // Acquire: pairs with the Release store in `switch_locked_inner`.
        self.active
            .get(cpu as usize)
            .map(|a| a.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    /// The first clear bit at or after `next`, wrapping, never 0. Scans a
    /// word at a time, so a 16-bit space costs at most 1,024 loads under
    /// the lock's IF=0 stretch (DESIGN §2.9 rule 2).
    fn find_free(&self) -> Option<u32> {
        let max = 1u32 << self.bits;
        // Relaxed: the allocator lock is held; pairs with nothing.
        let start = self.next.load(Ordering::Relaxed).clamp(1, max - 1);
        let hit = self
            .first_clear(start, max)
            .or_else(|| self.first_clear(1, start))?;
        let n = if hit + 1 >= max { 1 } else { hit + 1 };
        // Relaxed: the allocator lock is held; pairs with nothing.
        self.next.store(n, Ordering::Relaxed);
        Some(hit)
    }

    /// The first clear bit in `[from, to)`.
    fn first_clear(&self, from: u32, to: u32) -> Option<u32> {
        let mut a = from;
        while a < to {
            let w = (a / 64) as usize;
            let b = a % 64;
            // Relaxed: the allocator lock is held; pairs with nothing.
            let word = self
                .used
                .get(w)
                .map_or(u64::MAX, |x| x.load(Ordering::Relaxed));
            let free = !word & (u64::MAX << b);
            if free != 0 {
                let hit = (w as u32) * 64 + free.trailing_zeros();
                return (hit < to).then_some(hit);
            }
            a = (w as u32 + 1) * 64;
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

/// A switch of `cpu` into the space whose word is `space`: the fast path,
/// then the lock. Returns the word to load, generation and ASID; 0 only
/// for a `cpu` the allocator has no slot for. The caller then runs
/// [`AsidAlloc::take_flush`], and the local flush when it reports one,
/// before it loads the ASID.
pub fn switch(alloc: &AsidAlloc, cpu: u32, space: &AtomicU64) -> AsidWord {
    // Relaxed: pairs with nothing; a stale word fails `current` or the CAS
    // on `active`, and the slow path reads it again under the lock.
    let w = space.load(Ordering::Relaxed);
    if alloc.switch_fast(cpu, w) {
        return w;
    }
    alloc.switch_locked(cpu, space)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_when_usable_le_cpus() {
        assert!(AsidAlloc::new(2, 3).is_none());
        assert!(AsidAlloc::new(1, 1).is_none());
        assert!(AsidAlloc::new(2, 2).is_some());
        // The 4-bit test cap has 15 usable ASIDs: off from 15 CPUs.
        assert!(AsidAlloc::new(TEST_CAP_BITS, 15).is_none());
        assert!(AsidAlloc::new(TEST_CAP_BITS, 14).is_some());
        assert!(!DISABLED_REASON.is_empty());
    }

    #[test]
    fn width_follows_mmfr0_and_cap() {
        assert_eq!(width(true, None), 16);
        assert_eq!(width(false, None), 8);
        assert_eq!(width(true, Some(TEST_CAP_BITS)), 4);
        assert_eq!(width(false, Some(TEST_CAP_BITS)), 4);
        assert_eq!(width(false, Some(12)), 8);
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
        for _ in 0..8 {
            let w = AtomicU64::new(0);
            let g = switch(&a, 0, &w);
            assert_ne!(a.asid_of(g), 0);
            assert_eq!(w.load(Ordering::Relaxed), g);
        }
    }

    /// A 16-bit allocator hands out every number once, in order, before its
    /// first rollover.
    #[test]
    fn sixteen_bit_hands_out_every_number() {
        let a = AsidAlloc::try_boxed(16, 4).unwrap();
        let g0 = a.generation();
        for want in 1..(1u32 << 16) {
            let w = AtomicU64::new(0);
            let got = switch(&a, 0, &w);
            assert_eq!(a.asid_of(got), want);
            assert_eq!(a.gen_of(got), g0);
        }
        let w = AtomicU64::new(0);
        let got = switch(&a, 0, &w);
        assert_ne!(a.gen_of(got), g0, "the 65,536th space rolls over");
        assert!(a.take_flush(0));
        assert!(a.take_flush(3));
    }

    /// Two CPUs switching into one space give it one ASID, through the
    /// shared word, across a rollover too.
    #[test]
    fn shared_space_one_asid() {
        let a = AsidAlloc::new(2, 2).unwrap();
        let s = AtomicU64::new(0);
        let w0 = switch(&a, 0, &s);
        let w1 = switch(&a, 1, &s);
        assert_eq!(w0, w1);
        a.with_lock(|| a.rollover());
        let n0 = switch(&a, 0, &s);
        let n1 = switch(&a, 1, &s);
        assert_eq!(a.asid_of(n0), a.asid_of(w0), "reserved number kept");
        assert_eq!(n0, n1);
        assert!(a.current(n1));
    }

    /// A CPU idle across two rollovers reads 0 in its `active` slot at the
    /// second one. That must keep the word reserved at the first.
    #[test]
    fn idle_cpu_keeps_reservation_across_rollovers() {
        let a = AsidAlloc::new(2, 2).unwrap();
        let s1 = AtomicU64::new(0);
        let w1 = switch(&a, 1, &s1);
        a.with_lock(|| a.rollover());
        assert_eq!(a.active(1), 0);
        a.with_lock(|| a.rollover());
        assert_eq!(a.reserved[1].load(Ordering::Relaxed), w1);
        assert!(a.is_used(a.asid_of(w1)));
        let kept = switch(&a, 1, &s1);
        assert_eq!(a.asid_of(kept), a.asid_of(w1));
    }

    #[test]
    fn rollover_preserves_running() {
        let a = AsidAlloc::new(2, 2).unwrap();
        let s0 = AtomicU64::new(0);
        let s1 = AtomicU64::new(0);
        let w0 = switch(&a, 0, &s0);
        let w1 = switch(&a, 1, &s1);
        assert_ne!(a.asid_of(w0), a.asid_of(w1));
        // The one remaining ASID, then a rollover.
        let s2 = AtomicU64::new(0);
        let _ = switch(&a, 0, &s2);
        let s3 = AtomicU64::new(0);
        let w3 = switch(&a, 0, &s3);
        assert_ne!(a.gen_of(w3), a.gen_of(w0));
        assert!(a.flush_pending(0) && a.flush_pending(1));
        let n1 = a.asid_of(w1);
        assert_ne!(a.asid_of(w3), n1, "cpu 1's running ASID was reserved");
        a.take_flush(1);
        let again = switch(&a, 1, &s1);
        assert_eq!(a.asid_of(again), n1);
        assert_ne!(a.gen_of(again), a.gen_of(w1));
    }

    /// A small xorshift, so the schedule is the same on every run.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }

    const SPACES: usize = 9;
    const NONE: usize = usize::MAX;

    /// What one simulated CPU's TLB may hold: for each ASID, the space it
    /// has named on this CPU since the CPU's last flush.
    struct Tlb([usize; 4]);

    impl Tlb {
        /// Record `asid` naming `space`, or return the other space it named.
        fn load(&mut self, asid: u32, space: usize) -> Result<(), usize> {
            let slot = &mut self.0[asid as usize];
            if *slot != NONE && *slot != space {
                return Err(*slot);
            }
            *slot = space;
            Ok(())
        }
    }

    /// 2-bit ASIDs, 2 CPUs, nine spaces, each of which may run on either
    /// CPU or on both at once. Each CPU flushes only where the kernel does:
    /// after a switch whose `take_flush` reports one, before the load.
    /// Returns the rollovers, or the first failure of either rule: one ASID
    /// names one space on a CPU between its flushes (invariant I39), and a
    /// space that is loaded runs under its word's ASID on every CPU, so a
    /// broadcast `tlbi` by that ASID reaches each of them.
    fn run_no_alias(seed: u64, steps: usize) -> Result<u64, String> {
        let a = AsidAlloc::new(2, 2).unwrap();
        let words: [AtomicU64; SPACES] = core::array::from_fn(|_| AtomicU64::new(0));
        let mut tlb = [Tlb([NONE; 4]), Tlb([NONE; 4])];
        let mut loaded: [Option<(usize, u32)>; 2] = [None, None];
        let mut rng = Rng(seed);
        let g0 = a.generation();
        for step in 0..steps {
            let cpu = rng.next(2);
            let space = rng.next(SPACES);
            let w = switch(&a, cpu as u32, &words[space]);
            if a.take_flush(cpu as u32) {
                tlb[cpu] = Tlb([NONE; 4]);
            }
            let asid = a.asid_of(w);
            if asid == 0 {
                return Err(format!("step {step}: cpu {cpu} got word {w:#x}"));
            }
            if let Err(other) = tlb[cpu].load(asid, space) {
                return Err(format!(
                    "step {step}: asid {asid} named spaces {other} and {space} on cpu {cpu}"
                ));
            }
            loaded[cpu] = Some((space, asid));
            for (c, l) in loaded.iter().enumerate() {
                let Some((s, n)) = *l else { continue };
                let now = a.asid_of(words[s].load(Ordering::Relaxed));
                if now != n {
                    return Err(format!(
                        "step {step}: space {s} runs on cpu {c} as asid {n}, its word says {now}"
                    ));
                }
            }
        }
        Ok((a.generation() - g0) >> a.bits())
    }

    #[test]
    fn thousands_of_rollovers_no_alias() {
        let mut rollovers = 0;
        for seed in 1..=8u64 {
            match run_no_alias(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15), 4_000) {
                Ok(n) => rollovers += n,
                Err(e) => panic!("seed {seed}: {e}"),
            }
        }
        assert!(rollovers >= 2_000, "only {rollovers} rollovers");
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
            let w = AtomicU64::new(0);
            let first = switch(&alloc, 0, &w);
            let a = alloc.clone();
            let t = thread::spawn(move || {
                a.force_rollover();
                let other = AtomicU64::new(0);
                let n = switch(&a, 1, &other);
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

    /// CPU 0 and CPU 1 switch into one space, sharing its word, while
    /// CPU 1 rolls over first. However the two interleave, both load the
    /// same ASID, and the word names that ASID afterwards, so a broadcast
    /// `tlbi` by the word's ASID reaches both CPUs. Bound: 2 threads (CPU 0
    /// one switch, CPU 1 one rollover and one switch), 3 preemptions.
    #[test]
    fn loom_asid_shared_space_word() {
        let bound = Bound {
            threads: 2,
            preemptions: 3,
        };
        check(None, bound, || {
            let alloc = Arc::new(AsidAlloc::new(2, 2).unwrap());
            let space = Arc::new(AtomicU64::new(0));
            let _ = switch(&alloc, 0, &space);
            let (a, s) = (alloc.clone(), space.clone());
            let t = thread::spawn(move || {
                a.force_rollover();
                switch(&a, 1, &s)
            });
            let w0 = switch(&alloc, 0, &space);
            let w1 = t.join().unwrap();
            assert_ne!(alloc.asid_of(w0), 0, "cpu 0 got no asid");
            assert_eq!(
                alloc.asid_of(w0),
                alloc.asid_of(w1),
                "one space ran under two asids"
            );
            let now = space.load(Ordering::Relaxed);
            assert_eq!(
                alloc.asid_of(now),
                alloc.asid_of(w0),
                "the word names another asid"
            );
        });
    }
}
