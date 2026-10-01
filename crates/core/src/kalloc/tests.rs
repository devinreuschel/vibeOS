//! `kalloc`'s host tests: the fallible types, under a counting allocator
//! that can fail any one allocation.

extern crate std;

use super::*;
use core::cell::Cell;
use std::alloc::{GlobalAlloc, System};
use std::vec::Vec as StdVec;

/// Counts this thread's allocations and live bytes, and fails the one
/// whose index equals `FAIL_AT`. Per-thread, so other tests running at
/// the same time do not see it; disarmed by default.
struct Counting;

std::thread_local! {
    static FAIL_AT: Cell<u64> = const { Cell::new(u64::MAX) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
    static LIVE: Cell<isize> = const { Cell::new(0) };
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Count one allocation attempt; true if it is the one to fail.
fn take_fail() -> bool {
    COUNT
        .try_with(|c| {
            let n = c.get();
            c.set(n.wrapping_add(1));
            FAIL_AT.try_with(Cell::get).unwrap_or(u64::MAX) == n
        })
        .unwrap_or(false)
}

fn add_live(d: isize) {
    let _ = LIVE.try_with(|l| l.set(l.get().wrapping_add(d)));
}

// SAFETY: every call forwards to `System` with the caller's arguments
// unchanged, or returns null, which `GlobalAlloc` allows for failure;
// established here.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if take_fail() {
            return core::ptr::null_mut();
        }
        // SAFETY: the caller meets `GlobalAlloc::alloc`'s contract, which
        // this method forwards to `System` unchanged here.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            add_live(l.size() as isize);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        add_live(-(l.size() as isize));
        // SAFETY: the caller meets `GlobalAlloc::dealloc`'s contract, which
        // this method forwards to `System` unchanged here.
        unsafe { System.dealloc(p, l) }
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        if take_fail() {
            return core::ptr::null_mut();
        }
        // SAFETY: the caller meets `GlobalAlloc::realloc`'s contract, which
        // this method forwards to `System` unchanged here.
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            add_live(new as isize - l.size() as isize);
        }
        q
    }
}

pub(crate) fn live() -> isize {
    LIVE.with(Cell::get)
}

fn count() -> u64 {
    COUNT.with(Cell::get)
}

/// Fail the `n`th allocation from now (0 = the next one), in any module.
pub(crate) fn fail_in(n: u64) {
    let c = count();
    FAIL_AT.with(|f| f.set(c + n));
}

pub(crate) fn disarm() {
    FAIL_AT.with(|f| f.set(u64::MAX));
}

/// Counts drops of a value through a shared counter.
struct Dropper<'a>(&'a Cell<u32>, u64);

impl Drop for Dropper<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

trait Speak {
    fn speak(&self) -> u64;
}

impl Speak for Dropper<'_> {
    fn speak(&self) -> u64 {
        self.1
    }
}

struct Static(u64);

impl Speak for Static {
    fn speak(&self) -> u64 {
        self.0
    }
}

impl Drop for Static {
    fn drop(&mut self) {
        ARC_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

// `core`'s atomic in both configurations: loom's has no `const fn new`.
static ARC_DROPS: crate::atomic::statics::AtomicUsize = crate::atomic::statics::AtomicUsize::new(0);
static ARC_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn trybox_try_new_fails_cleanly() {
    let base = live();
    let drops = Cell::new(0);
    fail_in(0);
    let r = TryBox::try_new(Dropper(&drops, 7));
    disarm();
    assert!(matches!(r, Err(AllocError)));
    assert_eq!(drops.get(), 1);
    assert_eq!(live(), base);
    let b = TryBox::try_new(Dropper(&drops, 9)).unwrap();
    assert_eq!(b.1, 9);
    drop(b);
    assert_eq!(drops.get(), 2);
    assert_eq!(live(), base);
}

#[test]
fn trybox_zst_needs_no_allocation() {
    let base = live();
    fail_in(0);
    let n = count();
    let b = TryBox::try_new(());
    let u = TryBox::<[u8; 0]>::try_new_uninit();
    assert_eq!(count(), n);
    disarm();
    assert!(b.is_ok());
    assert!(u.is_ok());
    drop((b, u));
    assert_eq!(live(), base);
}

#[test]
fn trybox_raw_roundtrip() {
    let base = live();
    let mut b = TryBox::try_new(41u64).unwrap();
    *b += 1;
    let p = TryBox::into_raw(b);
    // SAFETY: `p` came from `kalloc::TryBox::into_raw` just above, and
    // nothing else owns it.
    let b = unsafe { TryBox::from_raw(p) };
    assert_eq!(*b, 42);
    assert_eq!(b.into_inner(), 42);
    assert_eq!(live(), base);
}

#[test]
fn trybox_uninit_write_into_inner() {
    let base = live();
    fail_in(0);
    let r = TryBox::<[u64; 4]>::try_new_uninit();
    disarm();
    assert!(r.is_err());
    assert_eq!(live(), base);
    let u = TryBox::<[u64; 4]>::try_new_uninit().unwrap();
    assert!(live() > base);
    let b = u.write([1, 2, 3, 4]);
    assert_eq!(b[2], 3);
    assert_eq!(b.into_inner(), [1, 2, 3, 4]);
    assert_eq!(live(), base);
}

#[test]
fn trybox_dyn_trait_unsize() {
    let base = live();
    let drops = Cell::new(0);
    let b: TryBox<dyn Speak + '_> =
        TryBox::<dyn Speak + '_>::try_new_unsize(Dropper(&drops, 5), |b| b).unwrap();
    assert_eq!(b.speak(), 5);
    drop(b);
    assert_eq!(drops.get(), 1);
    assert_eq!(live(), base);
    fail_in(0);
    let r: Result<TryBox<dyn Speak + '_>, _> =
        TryBox::<dyn Speak + '_>::try_new_unsize(Dropper(&drops, 6), |b| b);
    disarm();
    assert!(r.is_err());
    assert_eq!(drops.get(), 2);
    assert_eq!(live(), base);
}

#[test]
fn tryvec_push_fails_unchanged() {
    let base = live();
    let mut v: TryVec<u32> = TryVec::new();
    for i in 0..100u32 {
        if v.len() == v.capacity() {
            fail_in(0);
            assert_eq!(v.try_push(i), Err(AllocError));
            disarm();
            assert_eq!(v.len(), i as usize);
            assert!(v.iter().copied().eq(0..i));
        }
        v.try_push(i).unwrap();
    }
    assert_eq!(v.pop(), Some(99));
    v.truncate(10);
    assert_eq!(&v[..], &(0..10).collect::<StdVec<_>>()[..]);
    v[0] = 7;
    assert_eq!(v[0], 7);
    v.clear();
    assert!(v.is_empty());
    drop(v);
    assert_eq!(live(), base);
}

#[test]
fn tryvec_reserve_extend_capacity_fail_unchanged() {
    let base = live();
    fail_in(0);
    assert!(TryVec::<u8>::try_with_capacity(64).is_err());
    disarm();
    assert!(TryVec::<u8>::try_with_capacity(usize::MAX).is_err());
    let mut v = TryVec::<u8>::try_with_capacity(4).unwrap();
    assert!(v.capacity() >= 4);
    v.try_extend_from_slice(&[1, 2, 3]).unwrap();
    let cap = v.capacity();
    fail_in(0);
    assert_eq!(v.try_extend_from_slice(&[9; 100]), Err(AllocError));
    fail_in(0);
    assert_eq!(v.try_reserve(1000), Err(AllocError));
    disarm();
    assert_eq!(&v[..], &[1, 2, 3]);
    assert_eq!(v.capacity(), cap);
    assert_eq!(v.try_reserve(usize::MAX), Err(AllocError));
    v.try_reserve(1000).unwrap();
    assert!(v.capacity() >= 1003);
    v.try_extend_from_slice(&[4; 100]).unwrap();
    assert_eq!(v.len(), 103);
    drop(v);
    assert_eq!(live(), base);
}

#[test]
fn trystring_push_fails_unchanged() {
    let base = live();
    let mut s = TryString::default();
    s.try_push_str("vibe").unwrap();
    s.try_push('é').unwrap();
    assert_eq!(&*s, "vibeé");
    let long = "x".repeat(4096);
    fail_in(0);
    assert_eq!(s.try_push_str(&long), Err(AllocError));
    disarm();
    assert_eq!(&*s, "vibeé");
    while s.len() < 4096 {
        if s.len() + 4 > s.0.capacity() {
            fail_in(0);
            assert_eq!(s.try_push('😀'), Err(AllocError));
            disarm();
        }
        let before = s.len();
        s.try_push('😀').unwrap();
        assert_eq!(s.len(), before + 4);
    }
    drop(long);
    drop(s);
    assert_eq!(live(), base);
}

#[test]
fn tryarc_clone_is_infallible_and_last_drop_frees() {
    let _g = ARC_LOCK.lock().unwrap();
    let d0 = ARC_DROPS.load(Ordering::Relaxed);
    let base = live();
    fail_in(0);
    assert!(TryArc::try_new(Static(1)).is_err());
    disarm();
    assert_eq!(ARC_DROPS.load(Ordering::Relaxed), d0 + 1);
    assert_eq!(live(), base);

    let a = TryArc::try_new(Static(3)).unwrap();
    let n = count();
    fail_in(0);
    let b = a.clone();
    let c = b.clone();
    disarm();
    assert_eq!(count(), n);
    assert_eq!(c.0, 3);
    drop(a);
    drop(c);
    assert_eq!(ARC_DROPS.load(Ordering::Relaxed), d0 + 1);
    assert!(live() > base);
    drop(b);
    assert_eq!(ARC_DROPS.load(Ordering::Relaxed), d0 + 2);
    assert_eq!(live(), base);

    // Saturation: the count sticks and no drop frees the value.
    let a = TryArc::try_new(Static(4)).unwrap();
    a.inner().hdr.count.store(ARC_SATURATED, Ordering::Relaxed);
    let b = a.clone();
    drop(b);
    assert_eq!(a.inner().hdr.count.load(Ordering::Relaxed), ARC_SATURATED);
    let c = a.clone();
    drop(c);
    assert_eq!(ARC_DROPS.load(Ordering::Relaxed), d0 + 2);
    // Unstick it by hand so the last drop frees and live bytes return.
    a.inner().hdr.count.store(1, Ordering::Relaxed);
    drop(a);
    assert_eq!(ARC_DROPS.load(Ordering::Relaxed), d0 + 3);
    assert_eq!(live(), base);
}

#[test]
fn tryarc_dyn_trait_unsize() {
    let _g = ARC_LOCK.lock().unwrap();
    let d0 = ARC_DROPS.load(Ordering::Relaxed);
    let base = live();
    let cell = core::mem::size_of::<ArcInner<Static>>() as isize;
    let a: TryArc<dyn Speak + Send + Sync> =
        TryArc::<dyn Speak + Send + Sync>::try_new_unsize(Static(8), |b| b).unwrap();
    let b = a.clone();
    assert_eq!(b.speak(), 8);
    assert_eq!(live(), base + cell);
    let t = std::thread::spawn(move || b.speak());
    assert_eq!(t.join().unwrap(), 8);
    // The spawn allocates and frees on both threads; count from here.
    let mid = live();
    drop(a);
    assert_eq!(ARC_DROPS.load(Ordering::Relaxed), d0 + 1);
    assert_eq!(live(), mid - cell);
    let base = live();
    fail_in(0);
    let r: Result<TryArc<dyn Speak + Send + Sync>, _> =
        TryArc::<dyn Speak + Send + Sync>::try_new_unsize(Static(9), |b| b);
    disarm();
    assert!(r.is_err());
    assert_eq!(ARC_DROPS.load(Ordering::Relaxed), d0 + 2);
    assert_eq!(live(), base);
}

#[test]
fn trybtreemap_ordered_ops() {
    let base = live();
    let mut m: TryBTreeMap<u32, &str> = TryBTreeMap::new();
    assert!(m.is_empty());
    for (k, v) in [(5, "e"), (1, "a"), (3, "c"), (4, "d"), (2, "b")] {
        assert_eq!(m.try_insert(k, v), Ok(None));
    }
    assert_eq!(m.len(), 5);
    assert!(m.iter().map(|(k, _)| *k).eq(1..=5));
    assert_eq!(m.get(&3), Some(&"c"));
    assert_eq!(m.get(&9), None);
    let n = count();
    fail_in(0);
    assert_eq!(m.try_insert(3, "C"), Ok(Some("c")));
    disarm();
    assert_eq!(count(), n);
    *m.get_mut(&4).unwrap() = "D";
    assert_eq!(m.remove(&1), Some("a"));
    assert_eq!(m.remove(&1), None);
    let got: StdVec<_> = m.iter().map(|(k, v)| (*k, *v)).collect();
    assert_eq!(got, [(2, "b"), (3, "C"), (4, "D"), (5, "e")]);
    drop(got);
    drop(m);
    assert_eq!(live(), base);
}

#[test]
fn trybtreemap_insert_fails_unchanged_at_every_count() {
    let base = live();
    let mut m: TryBTreeMap<u64, u64> = TryBTreeMap::new();
    let mut failures = 0;
    // Insert keys in a scrambled order so inserts land mid-vector.
    for i in 0..64u64 {
        let k = (i * 37) % 64;
        let snap: StdVec<(u64, u64)> = m.iter().map(|(k, v)| (*k, *v)).collect();
        fail_in(0);
        let n = count();
        match m.try_insert(k, k * 10) {
            Err(AllocError) => {
                disarm();
                failures += 1;
                assert!(m.iter().map(|(k, v)| (*k, *v)).eq(snap.iter().copied()));
                assert_eq!(m.try_insert(k, k * 10), Ok(None));
            }
            Ok(prev) => {
                disarm();
                assert_eq!(prev, None);
                assert_eq!(count(), n, "an Ok insert under an armed failure allocated");
            }
        }
        assert_eq!(m.len() as u64, i + 1);
        drop(snap);
    }
    assert!(failures > 0);
    assert!(m.iter().map(|(k, _)| *k).eq(0..64));
    assert!(m.iter().all(|(k, v)| *v == k * 10));
    drop(m);
    assert_eq!(live(), base);
}
