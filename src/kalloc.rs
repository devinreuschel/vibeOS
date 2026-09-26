//! Fallible heap types for paths reachable from untrusted input (AGENTS.md
//! rule 4, DESIGN §4.4): a syscall, a device, a disk image, a packet, or a
//! firmware table's bounds. `alloc`'s growing calls panic on failure; these
//! return an error instead. Contents land with ROADMAP §10.4 (F010).
//!
//! DESIGN §4.4 says why fallibility is carried by type: on stable Rust
//! (§1.1) `Box`, `Arc`, and `BTreeMap` have no fallible constructor, so each
//! type here wraps an `alloc` type and exposes only operations that cannot
//! grow memory infallibly. Boxes allocate through `alloc::alloc::alloc` with
//! a null check, vectors and strings through `try_reserve`. A trait object is
//! made by `try_new_unsize(value, |b| b)`: the value is boxed at its own type
//! and the caller's closure performs the unsizing coercion, which needs no
//! unstable `CoerceUnsized`.

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::TryReserveError;
use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::borrow::Borrow;
use core::cmp::Ordering as CmpOrdering;
use core::fmt;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering, fence};

/// A heap allocation failed. Callers map it to `ENOMEM` (or the errno
/// Linux returns there) at the syscall boundary (DESIGN §4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocError;

impl fmt::Display for AllocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("out of memory")
    }
}

impl From<TryReserveError> for AllocError {
    fn from(_: TryReserveError) -> Self {
        AllocError
    }
}

// ---------------------------------------------------------------------------
// TryBox

/// An owning box whose constructors return `Err(AllocError)` instead of
/// panicking. No `Clone`: a copy would need an infallible allocation.
pub struct TryBox<T: ?Sized>(Box<T>);

impl<T> TryBox<T> {
    /// Box `v`. On failure `v` is dropped and `Err` returned.
    pub fn try_new(v: T) -> Result<Self, AllocError> {
        Ok(TryBox::try_new_uninit()?.write(v))
    }

    /// Allocate room for a `T` without initialising it.
    pub fn try_new_uninit() -> Result<TryBox<MaybeUninit<T>>, AllocError> {
        let layout = Layout::new::<MaybeUninit<T>>();
        if layout.size() == 0 {
            // A zero-sized `Box` does not allocate, so it cannot fail.
            return Ok(TryBox(Box::new(MaybeUninit::uninit())));
        }
        // SAFETY: `layout` has a non-zero size, checked just above (here),
        // which is `GlobalAlloc::alloc`'s one requirement.
        let p = unsafe { alloc::alloc::alloc(layout) }.cast::<MaybeUninit<T>>();
        if p.is_null() {
            return Err(AllocError);
        }
        // SAFETY: `p` came from the global allocator with
        // `Layout::new::<MaybeUninit<T>>()`, the layout `Box` frees with, and a
        // `MaybeUninit` needs no initialisation; both are established here.
        Ok(TryBox(unsafe { Box::from_raw(p) }))
    }

    /// Move the value out and free the allocation.
    pub fn into_inner(self) -> T {
        *self.0
    }
}

impl<T> TryBox<MaybeUninit<T>> {
    /// Initialise the box with `v`. Never allocates.
    pub fn write(self, v: T) -> TryBox<T> {
        let raw = Box::into_raw(self.0);
        // SAFETY: `raw` is the unique, live allocation `Box::into_raw` just
        // gave up here, and `MaybeUninit<T>` has `T`'s layout, so writing a
        // `T` and reboxing it as `Box<T>` frees with the same layout.
        unsafe {
            raw.cast::<T>().write(v);
            TryBox(Box::from_raw(raw.cast::<T>()))
        }
    }
}

impl<T: ?Sized> TryBox<T> {
    /// Box `v` at its own type `U`, then let `f` turn the box into a
    /// `Box<T>`. `f` only coerces (`|b| b`, DESIGN §4.4); it must not
    /// allocate. On failure `v` is dropped and `f` is not called.
    pub fn try_new_unsize<U>(v: U, f: impl FnOnce(Box<U>) -> Box<T>) -> Result<Self, AllocError> {
        let b = TryBox::try_new(v)?;
        Ok(TryBox(f(b.0)))
    }

    /// Give up ownership; [`TryBox::from_raw`] takes it back.
    pub fn into_raw(b: Self) -> *mut T {
        Box::into_raw(b.0)
    }

    /// Take back a pointer from [`TryBox::into_raw`].
    ///
    /// # Safety
    ///
    /// `p` came from `TryBox::<T>::into_raw` (or `Box::<T>::into_raw`), and
    /// no other `TryBox` or `Box` owns it.
    pub unsafe fn from_raw(p: *mut T) -> Self {
        // SAFETY: this fn's contract, stated in its `# Safety` section.
        TryBox(unsafe { Box::from_raw(p) })
    }
}

impl<T: ?Sized> Deref for TryBox<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: ?Sized> DerefMut for TryBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for TryBox<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

// ---------------------------------------------------------------------------
// TryVec

/// A vector whose growing operations return `Err(AllocError)`. A failed
/// call leaves the vector unchanged. No `Clone`, `Extend`, or
/// `FromIterator`: each would grow infallibly.
pub struct TryVec<T>(Vec<T>);

impl<T> TryVec<T> {
    pub const fn new() -> Self {
        TryVec(Vec::new())
    }

    pub fn try_with_capacity(n: usize) -> Result<Self, AllocError> {
        let mut v = Vec::new();
        v.try_reserve_exact(n)?;
        Ok(TryVec(v))
    }

    pub fn try_push(&mut self, v: T) -> Result<(), AllocError> {
        self.0.try_reserve(1)?;
        // Room for one more is reserved just above, so `push` cannot grow.
        self.0.push(v);
        Ok(())
    }

    pub fn try_reserve(&mut self, n: usize) -> Result<(), AllocError> {
        self.0.try_reserve(n)?;
        Ok(())
    }

    pub fn pop(&mut self) -> Option<T> {
        self.0.pop()
    }

    pub fn truncate(&mut self, len: usize) {
        self.0.truncate(len);
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }
}

impl<T: Clone> TryVec<T> {
    /// Append a copy of `s`. The whole slice is reserved first, so a
    /// failure appends nothing.
    pub fn try_extend_from_slice(&mut self, s: &[T]) -> Result<(), AllocError> {
        self.0.try_reserve(s.len())?;
        self.0.extend_from_slice(s);
        Ok(())
    }
}

impl<T> Deref for TryVec<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T> DerefMut for TryVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.0
    }
}

impl<T> Default for TryVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for TryVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

// ---------------------------------------------------------------------------
// TryString

/// A UTF-8 string whose growing operations return `Err(AllocError)`. A
/// failed call leaves the string unchanged.
pub struct TryString(String);

impl TryString {
    pub const fn new() -> Self {
        TryString(String::new())
    }

    pub fn try_push_str(&mut self, s: &str) -> Result<(), AllocError> {
        self.0.try_reserve(s.len())?;
        self.0.push_str(s);
        Ok(())
    }

    pub fn try_push(&mut self, c: char) -> Result<(), AllocError> {
        self.0.try_reserve(c.len_utf8())?;
        self.0.push(c);
        Ok(())
    }
}

impl Deref for TryString {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl Default for TryString {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for TryString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

// ---------------------------------------------------------------------------
// TryArc

/// Count at which [`TryArc`]'s count sticks: a clone there leaks the value
/// instead of overflowing, as Linux's `refcount_t` saturates (I27).
const ARC_SATURATED: usize = isize::MAX as usize;

/// The counted cell a [`TryArc`] points to. Its fields are private and
/// `value` stays last, so it can be unsized and so fields can be added
/// before it.
#[doc(hidden)]
pub struct ArcInner<T: ?Sized> {
    count: AtomicUsize,
    value: T,
}

/// A shared, atomically counted pointer whose constructors return
/// `Err(AllocError)`. `Clone` never allocates, so it is infallible.
pub struct TryArc<T: ?Sized> {
    ptr: NonNull<ArcInner<T>>,
    _owns: PhantomData<ArcInner<T>>,
}

// SAFETY: a `TryArc<T>` hands out only `&T` and drops `T` on whichever
// thread releases the last reference, so, as for `alloc::sync::Arc`, it may
// cross threads when `T: Send + Sync`; the count is atomic
// (`kalloc::ArcInner`). These are std's bounds for a type that shares `&T`
// (AGENTS.md rule 6).
unsafe impl<T: ?Sized + Send + Sync> Send for TryArc<T> {}
// SAFETY: as for `Send` just above: shared `&TryArc<T>` gives only `&T` and
// atomic count updates (`kalloc::ArcInner`).
unsafe impl<T: ?Sized + Send + Sync> Sync for TryArc<T> {}

impl<T: Send + 'static> TryArc<T> {
    /// Allocate a cell holding `v` with a count of one. On failure `v` is
    /// dropped.
    pub fn try_new(v: T) -> Result<Self, AllocError> {
        TryArc::try_new_unsize(v, |b| b)
    }
}

impl<T: ?Sized> TryArc<T> {
    /// Build the cell at `V`'s own type, let `f` coerce it (`|b| b`, DESIGN
    /// §4.4), then take its raw pointer. `f` only coerces; it must not
    /// allocate. `V: Send + 'static` because the last release may run on
    /// another CPU. On failure `v` is dropped and `f` is not called.
    pub fn try_new_unsize<V: Send + 'static>(
        v: V,
        f: impl FnOnce(Box<ArcInner<V>>) -> Box<ArcInner<T>>,
    ) -> Result<Self, AllocError> {
        let cell = TryBox::try_new_unsize(
            ArcInner {
                count: AtomicUsize::new(1),
                value: v,
            },
            f,
        )?;
        let raw = TryBox::into_raw(cell);
        // SAFETY: `raw` comes from `Box::into_raw` just above (here), which
        // never returns null.
        let ptr = unsafe { NonNull::new_unchecked(raw) };
        Ok(TryArc {
            ptr,
            _owns: PhantomData,
        })
    }

    fn inner(&self) -> &ArcInner<T> {
        // SAFETY: the cell is live while any `TryArc` to it exists: each
        // holds one count, and `TryArc::drop` frees only when the count
        // reaches zero (`kalloc::TryArc::drop`).
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: ?Sized> Clone for TryArc<T> {
    fn clone(&self) -> Self {
        // Relaxed, as in `alloc::sync::Arc`: the caller's own reference keeps
        // the cell alive, so nothing is published. At the saturation value
        // the count sticks and the value leaks, instead of panicking.
        let count = &self.inner().count;
        let mut c = count.load(Ordering::Relaxed);
        // At the saturation value the count sticks (I27).
        while c < ARC_SATURATED {
            match count.compare_exchange_weak(c, c + 1, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(now) => c = now,
            }
        }
        TryArc {
            ptr: self.ptr,
            _owns: PhantomData,
        }
    }
}

impl<T: ?Sized> Drop for TryArc<T> {
    fn drop(&mut self) {
        // Release, so this holder's uses of the value happen before the
        // last holder frees it; a saturated count never drops.
        let count = &self.inner().count;
        let mut c = count.load(Ordering::Relaxed);
        loop {
            if c >= ARC_SATURATED {
                return;
            }
            match count.compare_exchange_weak(c, c - 1, Ordering::Release, Ordering::Relaxed) {
                Ok(_) => break,
                Err(now) => c = now,
            }
        }
        if c != 1 {
            return;
        }
        // Acquire pairs with every other holder's Release decrement.
        fence(Ordering::Acquire);
        // SAFETY: the count went from one to zero here, so this was the last
        // `TryArc` to a cell that `TryArc::try_new_unsize` made with
        // `TryBox::into_raw` (`kalloc::TryArc::try_new_unsize`).
        drop(unsafe { TryBox::from_raw(self.ptr.as_ptr()) });
    }
}

impl<T: ?Sized> Deref for TryArc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner().value
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for TryArc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

// ---------------------------------------------------------------------------
// TryBTreeMap

/// An ordered map with fallible insertion. It is a sorted `Vec<(K, V)>`:
/// lookups are binary searches, and an insert or remove shifts the tail, so
/// it costs O(n). Meant for the small maps kernel paths keep; a failed
/// insert leaves the map unchanged.
pub struct TryBTreeMap<K: Ord, V> {
    entries: Vec<(K, V)>,
}

impl<K: Ord, V> TryBTreeMap<K, V> {
    pub const fn new() -> Self {
        TryBTreeMap {
            entries: Vec::new(),
        }
    }

    fn find<Q: Ord + ?Sized>(&self, k: &Q) -> Result<usize, usize>
    where
        K: Borrow<Q>,
    {
        self.entries
            .binary_search_by(|(e, _)| -> CmpOrdering { e.borrow().cmp(k) })
    }

    /// Insert `k` → `v`. Returns the value it replaced, if any. Replacing
    /// never allocates.
    pub fn try_insert(&mut self, k: K, v: V) -> Result<Option<V>, AllocError> {
        match self.find(&k) {
            Ok(i) => Ok(self
                .entries
                .get_mut(i)
                .map(|(_, old)| core::mem::replace(old, v))),
            Err(i) => {
                self.entries.try_reserve(1)?;
                self.entries.insert(i, (k, v));
                Ok(None)
            }
        }
    }

    pub fn get<Q: Ord + ?Sized>(&self, k: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        let i = self.find(k).ok()?;
        self.entries.get(i).map(|(_, v)| v)
    }

    pub fn get_mut<Q: Ord + ?Sized>(&mut self, k: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
    {
        let i = self.find(k).ok()?;
        self.entries.get_mut(i).map(|(_, v)| v)
    }

    pub fn remove<Q: Ord + ?Sized>(&mut self, k: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        let i = self.find(k).ok()?;
        Some(self.entries.remove(i).1)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }
}

impl<K: Ord, V> Default for TryBTreeMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord + fmt::Debug, V: fmt::Debug> fmt::Debug for TryBTreeMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
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
    // unchanged, or returns null, which `GlobalAlloc` allows for failure
    // (`std::alloc::System`).
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            if take_fail() {
                return core::ptr::null_mut();
            }
            // SAFETY: the caller's `GlobalAlloc::alloc` contract, forwarded.
            let p = unsafe { System.alloc(l) };
            if !p.is_null() {
                add_live(l.size() as isize);
            }
            p
        }

        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            add_live(-(l.size() as isize));
            // SAFETY: the caller's `GlobalAlloc::dealloc` contract, forwarded.
            unsafe { System.dealloc(p, l) }
        }

        unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            if take_fail() {
                return core::ptr::null_mut();
            }
            // SAFETY: the caller's `GlobalAlloc::realloc` contract, forwarded.
            let q = unsafe { System.realloc(p, l, new) };
            if !q.is_null() {
                add_live(new as isize - l.size() as isize);
            }
            q
        }
    }

    fn live() -> isize {
        LIVE.with(Cell::get)
    }

    fn count() -> u64 {
        COUNT.with(Cell::get)
    }

    /// Fail the `n`th allocation from now (0 = the next one).
    fn fail_in(n: u64) {
        let c = count();
        FAIL_AT.with(|f| f.set(c + n));
    }

    fn disarm() {
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

    static ARC_DROPS: AtomicUsize = AtomicUsize::new(0);
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
        // SAFETY: `p` came from `TryBox::into_raw` just above, and nothing
        // else owns it.
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
        a.inner().count.store(ARC_SATURATED, Ordering::Relaxed);
        let b = a.clone();
        drop(b);
        assert_eq!(a.inner().count.load(Ordering::Relaxed), ARC_SATURATED);
        let c = a.clone();
        drop(c);
        assert_eq!(ARC_DROPS.load(Ordering::Relaxed), d0 + 2);
        // Unstick it by hand so the last drop frees and live bytes return.
        a.inner().count.store(1, Ordering::Relaxed);
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
}
