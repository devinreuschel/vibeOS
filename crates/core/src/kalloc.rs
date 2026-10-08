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

use crate::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering, fence, statics};
use crate::sync::variant;
use alloc::{boxed::Box, collections::TryReserveError, string::String, vec::Vec};
use core::alloc::Layout;
use core::borrow::Borrow;
use core::cmp::Ordering as CmpOrdering;
use core::fmt;
use core::marker::PhantomData;
use core::mem::{self, MaybeUninit};
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};

/// A heap allocation failed: `ENOMEM` at the syscall boundary (DESIGN §4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocError;

impl From<AllocError> for crate::kerror::KError {
    fn from(_: AllocError) -> Self {
        Self::NoMem
    }
}

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
        // SAFETY: the caller meets this fn's contract; established by
        // `kalloc::TryBox::from_raw`'s `# Safety` section.
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

/// The head of every counted cell (DESIGN §2.11 rules 1 and 6): the count,
/// the deferred-release link, and the function that frees the whole cell
/// at the type it was allocated at. `#[repr(C)]` in [`ArcInner`] puts it at
/// offset 0, so a cell's address is its header's.
#[repr(C)]
struct Header {
    count: AtomicUsize,
    /// The next object on a [`DeferList`]; written only once the count
    /// has reached zero.
    next: AtomicPtr<Header>,
    /// [`release_concrete`] for the value type the cell was built with,
    /// before any unsizing, so it frees what was allocated (`dyn` included).
    release: unsafe fn(NonNull<Header>),
}

/// The counted cell a [`TryArc`] points to: the header, then the value. Its
/// fields are private and `value` stays last, so it can be unsized.
#[doc(hidden)]
#[repr(C)]
pub struct ArcInner<T: ?Sized> {
    hdr: Header,
    value: T,
}

/// Free the cell at `h` as an `ArcInner<V>`, dropping its value.
///
/// # Safety
///
/// `h` is the header of a cell [`TryArc::try_new_unsize`] built with value
/// type `V`, its count has reached zero, and nothing else reaches it.
unsafe fn release_concrete<V>(h: NonNull<Header>) {
    // SAFETY: by this fn's `# Safety` contract, `h` is the address of a
    // `Box<ArcInner<V>>` that `kalloc::TryArc::try_new_unsize` gave up with
    // `TryBox::into_raw`, and `#[repr(C)]` puts the header at offset 0; no
    // other owner remains.
    drop(unsafe { TryBox::from_raw(h.cast::<ArcInner<V>>().as_ptr()) });
}

/// A shared, atomically counted pointer whose constructors return
/// `Err(AllocError)`. `Clone` never allocates, so it is infallible. The
/// last put releases in place where DESIGN §2.11 rule 6 allows it
/// ([`set_release_context`]), and otherwise defers to the sink
/// [`set_deferral`] installs, which allocates nothing.
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
    /// allocate. `V: Send + 'static` because the last release may run later
    /// on another CPU's worker. On failure `v` is dropped and `f` is not
    /// called.
    pub fn try_new_unsize<V: Send + 'static>(
        v: V,
        f: impl FnOnce(Box<ArcInner<V>>) -> Box<ArcInner<T>>,
    ) -> Result<Self, AllocError> {
        let cell = TryBox::try_new_unsize(
            ArcInner {
                hdr: Header {
                    count: AtomicUsize::new(1),
                    next: AtomicPtr::new(ptr::null_mut()),
                    release: release_concrete::<V>,
                },
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
        // holds one count, and `kalloc::put_raw` releases the cell only
        // once the count reaches zero.
        unsafe { self.ptr.as_ref() }
    }

    /// The cell's header, at the cell's address.
    fn header(&self) -> NonNull<Header> {
        self.ptr.cast::<Header>()
    }

    /// Give up this reference. If it is the last, the cell goes to the
    /// deferral sink in any context, and is released in place only when no
    /// sink is installed (DESIGN §2.11 rule 6).
    pub fn put_deferred(self) {
        let h = self.header();
        mem::forget(self);
        // SAFETY: `self` held one count on the live cell at `h`
        // (`kalloc::TryArc::try_new_unsize`), and `mem::forget` just above
        // (here) keeps `Drop` from giving it up a second time.
        unsafe { put_raw(h, put_order(), true) }
    }
}

/// The ordering of a put's decrement: `Release`, the kernel's choice, so
/// this holder's uses of the value happen before its release. Only a loom
/// variant weakens it (`sync::variant::Site::TryArcDecrement`).
fn put_order() -> Ordering {
    // Release: pairs with the last holder's Acquire fence in `put_raw`; the
    // loom variant's Relaxed pairs with nothing.
    variant::pick(
        variant::Site::TryArcDecrement,
        Ordering::Release,
        Ordering::Relaxed,
    )
}

/// Give up one count on the cell at `h`, decrementing with `dec` (the
/// kernel's is `Release`). At zero, after an Acquire fence (DESIGN §2.11
/// rule 1), release it in place when `force_defer` is false and the
/// release-context hook allows it, and otherwise hand it to the deferral
/// sink. Nothing here reads the cell after the decrement unless it was the
/// last count (rust-lang/rust#55005).
///
/// # Safety
///
/// `h` is the header of a live cell that [`TryArc::try_new_unsize`] built,
/// and the caller owns one of its counts, which it gives up here.
unsafe fn put_raw(h: NonNull<Header>, dec: Ordering, force_defer: bool) {
    let hp = h.as_ptr();
    // Relaxed: the compare-exchange below rechecks it; pairs with nothing.
    // SAFETY: the caller owns a count, so the cell stays live until this
    // thread's decrement below; established by `kalloc::put_raw`'s
    // `# Safety` section.
    let mut c = unsafe { (*hp).count.load(Ordering::Relaxed) };
    loop {
        // At the saturation value the count sticks and the value leaks (I27).
        if c >= ARC_SATURATED {
            return;
        }
        // Relaxed on failure: the loop retries with the value read; pairs with nothing.
        // SAFETY: until this compare-exchange succeeds, the caller's count
        // keeps the cell live; established by `kalloc::put_raw`'s
        // `# Safety` section.
        match unsafe {
            (*hp)
                .count
                .compare_exchange_weak(c, c - 1, dec, Ordering::Relaxed)
        } {
            Ok(_) => break,
            Err(now) => c = now,
        }
    }
    if c != 1 {
        return;
    }
    // Acquire pairs with every other holder's Release decrement.
    fence(Ordering::Acquire);
    let d = Deferred(h);
    if !force_defer && release_context() {
        d.release_now();
    } else {
        defer(d);
    }
}

impl<T: ?Sized> Clone for TryArc<T> {
    fn clone(&self) -> Self {
        // Relaxed, as in `alloc::sync::Arc`: the caller's own reference keeps
        // the cell alive, so nothing is published. At the saturation value
        // the count sticks and the value leaks, instead of panicking.
        let count = &self.inner().hdr.count;
        // Relaxed: the caller's reference publishes nothing; pairs with nothing.
        let mut c = count.load(Ordering::Relaxed);
        // At the saturation value the count sticks (I27).
        while c < ARC_SATURATED {
            // Relaxed both ways, as the load; pairs with nothing.
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
        // SAFETY: this `TryArc` holds one count on the live cell it points
        // to (`kalloc::TryArc::try_new_unsize`) and gives it up here, once.
        unsafe { put_raw(self.header(), put_order(), false) }
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
// Deferred release (DESIGN §2.11 rule 6)

/// Objects whose [`Deferred`] was dropped unreleased. A statistic: Relaxed.
static DEFERRED_LEAKS: statics::AtomicUsize = statics::AtomicUsize::new(0);

/// Counted objects leaked by dropped [`Deferred`] tokens since boot.
pub fn deferred_leaks() -> usize {
    // Relaxed: a statistic; pairs with nothing.
    DEFERRED_LEAKS.load(statics::Ordering::Relaxed)
}

/// A counted object whose count has reached zero and whose release is
/// owed. [`Deferred::release_now`] or [`DeferList::push`] consumes it.
/// Dropping one leaks the object: the drop counts it in
/// [`deferred_leaks`], and in debug builds it panics.
#[must_use = "a dropped Deferred leaks its object; release it or push it on a DeferList"]
pub struct Deferred(NonNull<Header>);

// SAFETY: a `Deferred` owns a cell no one else reaches, whose value type
// is `Send + 'static`, so any thread may release it; the bound is
// established by `kalloc::TryArc::try_new_unsize`, which builds every cell.
unsafe impl Send for Deferred {}

impl Deferred {
    /// Release the object here: drop its value and free its cell.
    pub fn release_now(self) {
        let h = self.0;
        mem::forget(self);
        // SAFETY: `h` is the header of a cell whose count reached zero and
        // which only this token reached (`kalloc::put_raw`); its `release`
        // is `release_concrete` for the value type the cell was built with,
        // established by `kalloc::TryArc::try_new_unsize`.
        unsafe {
            let release = (*h.as_ptr()).release;
            release(h);
        }
    }
}

impl Drop for Deferred {
    #[allow(
        clippy::panic,
        reason = "a dropped Deferred is a kernel bug, never input: it panics in debug builds, as a dropped pmm::Frames does (DESIGN §4.2)"
    )]
    fn drop(&mut self) {
        // Relaxed: a statistic; pairs with nothing.
        DEFERRED_LEAKS.fetch_add(1, statics::Ordering::Relaxed);
        #[cfg(debug_assertions)]
        {
            // A host test that fails while it holds a token unwinds
            // through here; a second panic would abort the test binary.
            #[cfg(any(test, feature = "std"))]
            if std::thread::panicking() {
                return;
            }
            panic!("kalloc: a Deferred release was dropped");
        }
    }
}

/// Counted objects owed a release, linked through their own headers, so a
/// push allocates nothing; any CPU and context may push. `queued` says a
/// release item is queued or running, so callers queue at most one: `push`
/// and `claim` set it and say whether the caller must queue one, `unclaim`
/// hands that duty back, and `release_all` clears it before taking the list.
pub struct DeferList {
    head: AtomicPtr<Header>,
    queued: AtomicBool,
}

impl DeferList {
    /// `const`, so the kernel can keep one per CPU in a `static`. Loom's
    /// atomics have no `const fn new` (C-ATOMICS), so a model builds it at
    /// run time.
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
            queued: AtomicBool::new(false),
        }
    }

    #[cfg(loom)]
    pub fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
            queued: AtomicBool::new(false),
        }
    }

    /// Link `d` onto the list. True if the caller must queue a release
    /// item for it, because none was queued.
    pub fn push(&self, d: Deferred) -> bool {
        let h = d.0;
        mem::forget(d);
        // Relaxed: the compare-exchange below rechecks it; pairs with nothing.
        let mut cur = self.head.load(Ordering::Relaxed);
        loop {
            // Relaxed: the AcqRel compare-exchange below publishes it; pairs with nothing.
            // SAFETY: `h` is a cell whose count reached zero and which only
            // this push reaches until the compare-exchange below publishes
            // it (`kalloc::put_raw`).
            unsafe { (*h.as_ptr()).next.store(cur, Ordering::Relaxed) };
            // AcqRel: pairs with the AcqRel swap in `release_all`. Release
            // publishes `next`; Acquire, so the list stays one release
            // sequence that `release_all`'s swap reads whole.
            // Relaxed on failure: the loop retries; pairs with nothing.
            match self
                .head
                .compare_exchange(cur, h.as_ptr(), Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(now) => cur = now,
            }
        }
        // AcqRel: pairs with the Release stores of `false` in `unclaim` and `release_all`.
        !self.queued.swap(true, Ordering::AcqRel)
    }

    /// Take the duty to queue a release item: true if the list holds
    /// objects and no item is queued for it.
    pub fn claim(&self) -> bool {
        // AcqRel: pairs with the Release stores of `false` in `unclaim` and `release_all`.
        !self.is_empty() && !self.queued.swap(true, Ordering::AcqRel)
    }

    /// Give back the duty `push` or `claim` handed out, when queueing the
    /// item failed. A later `claim` retries.
    pub fn unclaim(&self) {
        // Release: pairs with the AcqRel swap in `push` or `claim`.
        self.queued.store(false, Ordering::Release);
    }

    /// Release every object on the list and return how many. Run where
    /// releasing is allowed (DESIGN §2.11 rule 6). It clears `queued`
    /// before it takes the list, so an object pushed after the take makes
    /// its pusher queue a new item.
    pub fn release_all(&self) -> usize {
        // Release: pairs with the AcqRel swap in `push` or `claim`.
        self.queued.store(false, Ordering::Release);
        // AcqRel: pairs with the compare-exchange in `push`, so every `next` is visible.
        let mut p = self.head.swap(ptr::null_mut(), Ordering::AcqRel);
        let mut n = 0usize;
        while let Some(h) = NonNull::new(p) {
            // Relaxed: the swap's Acquire read the push that stored it; pairs with nothing.
            // SAFETY: every node on the list is a cell whose count reached
            // zero, and the swap above took the list whole, so only this
            // walk reaches it (`kalloc::DeferList::push`).
            p = unsafe { (*h.as_ptr()).next.load(Ordering::Relaxed) };
            Deferred(h).release_now();
            n = n.saturating_add(1);
        }
        n
    }

    /// Whether the list holds no object.
    pub fn is_empty(&self) -> bool {
        // Acquire: pairs with the AcqRel compare-exchange in `push` and swap in `release_all`.
        self.head.load(Ordering::Acquire).is_null()
    }
}

#[cfg(not(loom))]
impl Default for DeferList {
    fn default() -> Self {
        Self::new()
    }
}

/// The release-context hook, a `fn() -> bool`; null releases in place.
static RELEASE_CONTEXT: statics::AtomicPtr<()> = statics::AtomicPtr::new(ptr::null_mut());
/// The deferral sink, a `fn(Deferred)`; null releases in place.
static DEFERRAL: statics::AtomicPtr<()> = statics::AtomicPtr::new(ptr::null_mut());

// A hook slot holds a `fn` pointer as a data pointer.
const _: () = assert!(mem::size_of::<fn() -> bool>() == mem::size_of::<*mut ()>());
const _: () = assert!(mem::size_of::<fn(Deferred)>() == mem::size_of::<*mut ()>());

/// Install the test for "a counted object may be released here" (DESIGN
/// §2.11 rule 6). Until one is installed, every last put releases in place.
pub fn set_release_context(f: fn() -> bool) {
    // Release: pairs with the Acquire load in `release_context`.
    RELEASE_CONTEXT.store(f as *mut (), statics::Ordering::Release);
}

/// Install the sink a deferred release goes to: it links the object onto
/// this CPU's [`DeferList`] and queues the release item (C-COUNTED). Until
/// one is installed, a deferred release runs in place.
pub fn set_deferral(f: fn(Deferred)) {
    // Release: pairs with the Acquire load in `defer`.
    DEFERRAL.store(f as *mut (), statics::Ordering::Release);
}

fn release_context() -> bool {
    // Acquire: pairs with the Release store in `set_release_context`.
    let p = RELEASE_CONTEXT.load(statics::Ordering::Acquire);
    if p.is_null() {
        return true;
    }
    // SAFETY: invariant: a non-null `RELEASE_CONTEXT` holds a
    // `fn() -> bool`, and a `fn` pointer is pointer-sized (the const
    // assertion above); established by `kalloc::set_release_context`, its
    // only store.
    let f = unsafe { mem::transmute::<*mut (), fn() -> bool>(p) };
    f()
}

fn defer(d: Deferred) {
    // Acquire: pairs with the Release store in `set_deferral`.
    let p = DEFERRAL.load(statics::Ordering::Acquire);
    if p.is_null() {
        d.release_now();
        return;
    }
    // SAFETY: invariant: a non-null `DEFERRAL` holds a `fn(Deferred)`, and
    // a `fn` pointer is pointer-sized (the const assertion above);
    // established by `kalloc::set_deferral`, its only store.
    let f = unsafe { mem::transmute::<*mut (), fn(Deferred)>(p) };
    f(d);
}

// ---------------------------------------------------------------------------
// The two-count object (DESIGN §2.11 rule 1)
//
// A `users` count, taken for a pin only by get-unless-zero, whose last put
// runs the value's teardown, over a `TryArc` core whose last put frees the
// value. An address space is one (ROADMAP §10.6, F019): its process's thread
// and each pin hold a `UsersArc`, and each region and each `users` holder a
// `CoreArc`.

/// What the last `users` put runs on the value, in place (DESIGN §2.11
/// rule 6). It may sleep, so the last put happens outside every spinlock;
/// debug builds assert that through [`set_release_context`]'s hook.
///
/// [`set_release_context`]: set_release_context
pub trait Teardown: Send + Sync + 'static {
    fn teardown(&self);
}

/// The counted cell: the `users` count beside the value.
struct Inner<T> {
    users: AtomicUsize,
    value: T,
}

/// One `users` reference. Not `Clone`: a second one is a [`CoreArc::pin`],
/// which fails once the count has reached zero. Dropping the last one runs
/// [`Teardown::teardown`] here; the value itself drops with the last
/// reference of either kind.
#[must_use = "dropping the last users reference runs the teardown"]
pub struct UsersArc<T: Teardown> {
    core: TryArc<Inner<T>>,
}

/// One core reference: keeps the value's memory, not its use. It has no
/// `Deref`; [`CoreArc::pin`] turns it into a [`UsersArc`] while one lives.
pub struct CoreArc<T: Teardown> {
    core: TryArc<Inner<T>>,
}

impl<T: Teardown> UsersArc<T> {
    /// Allocate the cell with `users` at one. On failure `v` is dropped
    /// and no teardown runs.
    pub fn try_new(v: T) -> Result<Self, AllocError> {
        let core = TryArc::try_new(Inner {
            users: AtomicUsize::new(1),
            value: v,
        })?;
        Ok(UsersArc { core })
    }

    /// A core reference to the same cell: a `TryArc` clone, which never
    /// allocates.
    pub fn core(&self) -> CoreArc<T> {
        CoreArc {
            core: self.core.clone(),
        }
    }
}

/// The ordering of a `users` put: `Release`, so this holder's uses of the
/// value happen before the teardown. Only a loom variant weakens it
/// (`sync::variant::Site::UsersDecrement`).
fn users_put_order() -> Ordering {
    // Release: pairs with the last holder's Acquire fence in `UsersArc::drop`; the
    // loom variant's Relaxed pairs with nothing.
    variant::pick(
        variant::Site::UsersDecrement,
        Ordering::Release,
        Ordering::Relaxed,
    )
}

impl<T: Teardown> Drop for UsersArc<T> {
    fn drop(&mut self) {
        if self.core.users.fetch_sub(1, users_put_order()) != 1 {
            return;
        }
        // Acquire pairs with every other holder's Release put, so their
        // uses of the value happen before the teardown.
        fence(Ordering::Acquire);
        debug_assert!(
            release_context(),
            "kalloc: the last users put runs its teardown where releasing is not allowed"
        );
        self.core.value.teardown();
        // The core reference this holder kept drops with `self.core`.
    }
}

impl<T: Teardown> Deref for UsersArc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.core.value
    }
}

impl<T: Teardown + fmt::Debug> fmt::Debug for UsersArc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: Teardown> CoreArc<T> {
    /// Get-unless-zero: a new `users` reference while one is live, `None`
    /// once the last put has run (or is running) the teardown. It never
    /// raises a zero count, so a teardown runs once and no pin overlaps it.
    pub fn pin(&self) -> Option<UsersArc<T>> {
        let users = &self.core.users;
        if variant::pick(variant::Site::UsersBlindPin, false, true) {
            // Relaxed: the loom variant's blind pin; pairs with nothing.
            users.fetch_add(1, Ordering::Relaxed);
            return Some(UsersArc {
                core: self.core.clone(),
            });
        }
        // Relaxed: the compare-exchange below rechecks it; pairs with nothing.
        let mut c = users.load(Ordering::Relaxed);
        loop {
            if c == 0 {
                return None;
            }
            let n = c.checked_add(1)?;
            // Acquire: pairs with the Release put in `UsersArc::drop`; the
            // pin's uses of the value come after the increment that found it
            // live. Relaxed on failure: the loop retries; pairs with nothing.
            match users.compare_exchange_weak(c, n, Ordering::Acquire, Ordering::Relaxed) {
                Ok(_) => {
                    return Some(UsersArc {
                        core: self.core.clone(),
                    });
                }
                Err(now) => c = now,
            }
        }
    }

    /// The `users` count now: a snapshot, for tests and statistics.
    pub fn users(&self) -> usize {
        // Relaxed: a snapshot for tests and statistics; pairs with nothing.
        self.core.users.load(Ordering::Relaxed)
    }
}

impl<T: Teardown> Clone for CoreArc<T> {
    fn clone(&self) -> Self {
        CoreArc {
            core: self.core.clone(),
        }
    }
}

impl<T: Teardown> fmt::Debug for CoreArc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreArc")
            .field("users", &self.users())
            .finish_non_exhaustive()
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
pub(crate) mod tests;

#[cfg(all(test, not(loom)))]
mod counted_tests {
    extern crate std;

    use super::*;
    use core::cell::Cell;
    use std::sync::Arc as StdArc;
    use std::sync::Once;
    use std::sync::atomic::{AtomicUsize as StdAtomicUsize, Ordering as StdOrdering};

    std::thread_local! {
        /// Whether this test thread simulates atomic context. Default
        /// false, so other tests on their own threads still release in
        /// place.
        static ATOMIC: Cell<bool> = const { Cell::new(false) };
        /// This thread's deferred-release list, the sink's target.
        static LIST: DeferList = const { DeferList::new() };
        /// Release items the sink was told to queue.
        static QUEUED: Cell<usize> = const { Cell::new(0) };
    }

    fn may_release() -> bool {
        !ATOMIC.get()
    }

    fn sink(d: Deferred) {
        if LIST.with(|l| l.push(d)) {
            QUEUED.set(QUEUED.get() + 1);
        }
    }

    fn install() {
        static HOOKS: Once = Once::new();
        HOOKS.call_once(|| {
            set_release_context(may_release);
            set_deferral(sink);
        });
    }

    /// Counts its drops through a shared counter.
    struct Probe(StdArc<StdAtomicUsize>);

    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.fetch_add(1, StdOrdering::SeqCst);
        }
    }

    fn probe() -> (Probe, StdArc<StdAtomicUsize>) {
        let n = StdArc::new(StdAtomicUsize::new(0));
        (Probe(n.clone()), n)
    }

    fn drops(n: &StdAtomicUsize) -> usize {
        n.load(StdOrdering::SeqCst)
    }

    fn released_all() -> usize {
        LIST.with(DeferList::release_all)
    }

    fn list_empty() -> bool {
        LIST.with(DeferList::is_empty)
    }

    #[test]
    fn tryarc_atomic_drop_defers_until_release_all() {
        install();
        let (p, n) = probe();
        let a = TryArc::try_new(p).unwrap();
        let b = a.clone();
        let q0 = QUEUED.get();
        ATOMIC.set(true);
        drop(a);
        assert_eq!(drops(&n), 0);
        assert!(list_empty(), "a put that is not the last deferred");
        drop(b);
        ATOMIC.set(false);
        assert_eq!(drops(&n), 0, "released in simulated atomic context");
        assert!(!list_empty());
        assert_eq!(QUEUED.get(), q0 + 1);
        assert_eq!(released_all(), 1);
        assert_eq!(drops(&n), 1);
        assert!(list_empty());
        assert_eq!(released_all(), 0);
    }

    #[test]
    fn tryarc_drop_releases_in_place_when_allowed() {
        install();
        let (p, n) = probe();
        let a = TryArc::try_new(p).unwrap();
        let b = a.clone();
        drop(a);
        assert_eq!(drops(&n), 0);
        drop(b);
        assert_eq!(drops(&n), 1);
        assert!(list_empty());
    }

    #[test]
    fn tryarc_put_deferred_defers_in_any_context() {
        install();
        let (p, n) = probe();
        let a = TryArc::try_new(p).unwrap();
        let b = a.clone();
        a.put_deferred();
        assert_eq!(drops(&n), 0);
        assert!(list_empty());
        let q0 = QUEUED.get();
        b.put_deferred();
        assert_eq!(drops(&n), 0, "put_deferred released in place");
        assert_eq!(QUEUED.get(), q0 + 1);
        assert_eq!(released_all(), 1);
        assert_eq!(drops(&n), 1);
    }

    trait Named: Send + Sync {
        fn name(&self) -> u64;
    }

    /// Larger than its trait object's pointer: a wrong-layout free shows.
    struct Big([u64; 16], Probe);

    impl Named for Big {
        fn name(&self) -> u64 {
            self.0[15] + drops(&self.1.0) as u64
        }
    }

    #[test]
    fn tryarc_dyn_deferred_release_drops_value() {
        install();
        let (p, n) = probe();
        let a: TryArc<dyn Named> =
            TryArc::<dyn Named>::try_new_unsize(Big([77; 16], p), |b| b).unwrap();
        let b = a.clone();
        assert_eq!(b.name(), 77);
        drop(a);
        ATOMIC.set(true);
        drop(b);
        ATOMIC.set(false);
        assert_eq!(drops(&n), 0);
        assert_eq!(released_all(), 1);
        assert_eq!(drops(&n), 1, "the concrete value was not dropped");
    }

    #[test]
    fn deferlist_claims_one_release_item() {
        install();
        let list = DeferList::new();
        assert!(!list.claim(), "an empty list claimed");
        let (p1, n1) = probe();
        let (p2, n2) = probe();
        assert!(list.push(owed(p1)), "the first push must queue an item");
        assert!(!list.push(owed(p2)), "a second push queued another item");
        assert!(!list.claim(), "claimed while an item is queued");
        list.unclaim();
        assert!(list.claim(), "no claim after unclaim");
        assert!(!list.claim());
        assert_eq!(list.release_all(), 2);
        assert_eq!((drops(&n1), drops(&n2)), (1, 1));
        assert!(list.is_empty());
        assert!(!list.claim(), "an empty list claimed after release_all");
        let (p3, n3) = probe();
        assert!(list.push(owed(p3)), "release_all left the list claimed");
        assert_eq!(list.release_all(), 1);
        assert_eq!(drops(&n3), 1);
    }

    /// A release owed for a fresh cell holding `p`, as if its count had
    /// just reached zero.
    fn owed(p: Probe) -> Deferred {
        let a = TryArc::try_new(p).unwrap();
        let h = a.header();
        mem::forget(a);
        Deferred(h)
    }
}

#[cfg(all(test, not(loom)))]
mod users_tests;

#[cfg(all(test, loom))]
mod loom_models;
