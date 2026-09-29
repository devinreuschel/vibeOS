//! Filesystems: the kernel half of subsystem `fs` (DESIGN §1.3).

pub(crate) mod fat_init;
pub(crate) mod file_init;
pub(crate) mod fs_init;
#[cfg(feature = "kernel_tests")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::let_underscore_must_use,
    clippy::unused_result_ok,
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "kernel_tests-only in-guest tests: a failure ends a test, not the kernel"
)]
pub mod ktest;
#[cfg(feature = "vibefs_crash")]
pub(crate) mod vibefs_crash;
pub(crate) mod vibefs_init;

use vibeos::kalloc::{AllocError, TryBox};

/// A heap copy of `src`, made in place: a volume is too large for a kernel
/// stack (DESIGN §4.5), so it is never built or moved by value. `T` is
/// plain data that owns nothing, so a byte copy of a value is a value.
pub(crate) fn boxed_copy<T: 'static>(src: &'static T) -> Result<TryBox<T>, AllocError> {
    const { assert!(!core::mem::needs_drop::<T>()) };
    let p = TryBox::into_raw(TryBox::<T>::try_new_uninit()?).cast::<T>();
    // SAFETY: `p` is a fresh, unaliased allocation laid out for `T` (a
    // `MaybeUninit<T>` has `T`'s layout), and `src` a valid `T` of plain
    // data that owns nothing (checked above), so after the copy `p` holds a
    // valid `T` of its own, which `from_raw` takes back as the box
    // `into_raw` gave up; established here.
    unsafe {
        core::ptr::copy_nonoverlapping(src, p, 1);
        Ok(TryBox::from_raw(p))
    }
}

/// A zeroed heap byte array, made in place (no stack temporary).
pub(crate) fn boxed_zeroed<const N: usize>() -> Result<TryBox<[u8; N]>, AllocError> {
    let p = TryBox::into_raw(TryBox::<[u8; N]>::try_new_uninit()?).cast::<[u8; N]>();
    // SAFETY: `p` is a fresh, unaliased allocation of `N` bytes, all of
    // which the fill makes initialized, and any bytes are a valid `[u8; N]`;
    // `from_raw` takes back the box `into_raw` gave up; established here.
    unsafe {
        core::ptr::write_bytes(p.cast::<u8>(), 0, N);
        Ok(TryBox::from_raw(p))
    }
}
