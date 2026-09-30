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
    // SAFETY: the caller owns a count, so the cell stays live until this
    // thread's decrement below; established by `kalloc::put_raw`'s
    // `# Safety` section.
    let mut c = unsafe { (*hp).count.load(Ordering::Relaxed) };
    loop {
        // At the saturation value the count sticks and the value leaks (I27).
        if c >= ARC_SATURATED {
            return;
        }
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
        let mut cur = self.head.load(Ordering::Relaxed);
        loop {
            // SAFETY: `h` is a cell whose count reached zero and which only
            // this push reaches until the compare-exchange below publishes
            // it (`kalloc::put_raw`).
            unsafe { (*h.as_ptr()).next.store(cur, Ordering::Relaxed) };
            // Release publishes `next`; Acquire, so the list stays one
            // release sequence that `release_all`'s swap reads whole.
            match self
                .head
                .compare_exchange(cur, h.as_ptr(), Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(now) => cur = now,
            }
        }
        !self.queued.swap(true, Ordering::AcqRel)
    }

    /// Take the duty to queue a release item: true if the list holds
    /// objects and no item is queued for it.
    pub fn claim(&self) -> bool {
        !self.is_empty() && !self.queued.swap(true, Ordering::AcqRel)
    }

    /// Give back the duty `push` or `claim` handed out, when queueing the
    /// item failed. A later `claim` retries.
    pub fn unclaim(&self) {
        self.queued.store(false, Ordering::Release);
    }

    /// Release every object on the list and return how many. Run where
    /// releasing is allowed (DESIGN §2.11 rule 6). It clears `queued`
    /// before it takes the list, so an object pushed after the take makes
    /// its pusher queue a new item.
    pub fn release_all(&self) -> usize {
        self.queued.store(false, Ordering::Release);
        let mut p = self.head.swap(ptr::null_mut(), Ordering::AcqRel);
        let mut n = 0usize;
        while let Some(h) = NonNull::new(p) {
            // SAFETY: every node on the list is a cell whose count reached
            // zero, and the swap above took the list whole, so only this
            // walk reaches it (`kalloc::DeferList::push`). Relaxed: the
            // swap's Acquire read the push that published `next`.
            p = unsafe { (*h.as_ptr()).next.load(Ordering::Relaxed) };
            Deferred(h).release_now();
            n = n.saturating_add(1);
        }
        n
    }

    /// Whether the list holds no object.
    pub fn is_empty(&self) -> bool {
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

    // `core`'s atomic in both configurations: loom's has no `const fn new`.
    static ARC_DROPS: crate::atomic::statics::AtomicUsize =
        crate::atomic::statics::AtomicUsize::new(0);
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
}

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

#[cfg(all(test, loom))]
mod loom_models {
    extern crate std;

    use super::*;
    use crate::sync::variant::{self, Site};
    use loom::cell::UnsafeCell;
    use loom::sync::Arc;
    use loom::thread;

    loom::lazy_static! {
        /// The sink's target: one list for the model's threads.
        static ref LIST: DeferList = DeferList::new();
    }

    /// Every put is in simulated atomic context, so every last put defers
    /// to `LIST`; the model's threads release it themselves.
    fn install() {
        set_release_context(|| false);
        set_deferral(|d| {
            LIST.push(d);
        });
    }

    /// A cell one holder writes and the release reads, and a release count.
    struct Val {
        cell: UnsafeCell<u64>,
        released: Arc<AtomicUsize>,
    }

    // SAFETY: `cell` is written by one holder before it puts its
    // reference, and read by the release after the last put; the models
    // check, through loom, that the count orders the two. Established here.
    unsafe impl Sync for Val {}

    impl Drop for Val {
        fn drop(&mut self) {
            // SAFETY: the release runs after every put, so no holder still
            // writes `cell`; loom reports it if the count fails to order the
            // write before this read. Established by `kalloc::put_raw`.
            let v = self.cell.with(|p| unsafe { *p });
            assert_eq!(v, 7);
            self.released.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// T1 writes the value and puts its reference with `put_deferred`, the
    /// main thread drops the other plainly, and W and then main release
    /// the list: exactly one release, which sees T1's write.
    fn tryarc_count_model() {
        loom::model(|| {
            install();
            let released = Arc::new(AtomicUsize::new(0));
            let a = TryArc::try_new(Val {
                cell: UnsafeCell::new(0),
                released: released.clone(),
            })
            .unwrap();
            let b = a.clone();
            let t1 = thread::spawn(move || {
                // SAFETY: this holder is the only writer, and the value is
                // live while `b` is held; established here.
                b.cell.with_mut(|p| unsafe { *p = 7 });
                b.put_deferred();
            });
            let w = thread::spawn(|| {
                LIST.release_all();
            });
            drop(a);
            t1.join().unwrap();
            w.join().unwrap();
            LIST.release_all();
            assert_eq!(released.load(Ordering::Relaxed), 1);
            assert!(LIST.is_empty());
        });
    }

    #[test]
    fn loom_tryarc_count() {
        tryarc_count_model();
    }

    #[test]
    #[should_panic(expected = "Causality violation")]
    fn loom_tryarc_count_relaxed_dec_fails() {
        let _w = variant::weaken(Site::TryArcDecrement);
        tryarc_count_model();
    }
}
