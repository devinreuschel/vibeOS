//! `memcpy`, `memmove`, `memset`, `memcmp`, `bcmp` and `strlen` for the user
//! runtime (ROADMAP §10.5).
//!
//! The user triple's `compiler_builtins` leaves these symbols to a libc, and
//! `core` calls them. They are plain byte loops, and the crate is
//! `#![no_builtins]` so LLVM does not turn a loop back into a call to itself.

#![no_std]
#![no_builtins]

#[cfg(not(all(target_os = "linux", target_env = "musl")))]
compile_error!(
    "vibeos-user-mem builds only for the linux-musl user triple, through `make user` (ROADMAP §10.5)"
);

use core::ffi::{c_char, c_void};

/// Copy `n` bytes from `src` to `dst`; returns `dst`.
///
/// # Safety
///
/// `src` is valid for `n` byte reads, `dst` for `n` byte writes, and the two
/// ranges do not overlap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    let (d, s) = (dst.cast::<u8>(), src.cast::<u8>());
    let mut i = 0;
    while i < n {
        // SAFETY: `i < n`, and the caller's contract, stated in the `# Safety`
        // section here, makes both ranges valid for `n` bytes.
        unsafe { *d.add(i) = *s.add(i) };
        i += 1;
    }
    dst
}

/// Copy `n` bytes from `src` to `dst`, which may overlap; returns `dst`.
///
/// # Safety
///
/// `src` is valid for `n` byte reads and `dst` for `n` byte writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memmove(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    let (d, s) = (dst.cast::<u8>(), src.cast::<u8>());
    if (d as usize) <= (s as usize) {
        let mut i = 0;
        while i < n {
            // SAFETY: `i < n`, and the caller's contract, stated in the
            // `# Safety` section here, makes both ranges valid; copying
            // upwards reads each source byte before any write reaches it.
            unsafe { *d.add(i) = *s.add(i) };
            i += 1;
        }
    } else {
        let mut i = n;
        while i > 0 {
            i -= 1;
            // SAFETY: `i < n`, and the caller's contract, stated in the
            // `# Safety` section here, makes both ranges valid; copying
            // downwards reads each source byte before any write reaches it.
            unsafe { *d.add(i) = *s.add(i) };
        }
    }
    dst
}

/// Set `n` bytes at `dst` to the low byte of `c`; returns `dst`.
///
/// # Safety
///
/// `dst` is valid for `n` byte writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(dst: *mut c_void, c: i32, n: usize) -> *mut c_void {
    let d = dst.cast::<u8>();
    let mut i = 0;
    while i < n {
        // SAFETY: `i < n`, and the caller's contract, stated in the `# Safety`
        // section here, makes `dst` valid for `n` bytes.
        unsafe { *d.add(i) = c as u8 };
        i += 1;
    }
    dst
}

/// Compare `n` bytes; the difference of the first unequal pair, as unsigned
/// bytes, or 0.
///
/// # Safety
///
/// `a` and `b` are valid for `n` byte reads.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    let (a, b) = (a.cast::<u8>(), b.cast::<u8>());
    let mut i = 0;
    while i < n {
        // SAFETY: `i < n`, and the caller's contract, stated in the `# Safety`
        // section here, makes both ranges valid for `n` bytes.
        let (x, y) = unsafe { (*a.add(i), *b.add(i)) };
        if x != y {
            return i32::from(x) - i32::from(y);
        }
        i += 1;
    }
    0
}

/// Compare `n` bytes; 0 when equal, non-zero otherwise.
///
/// # Safety
///
/// `a` and `b` are valid for `n` byte reads.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    // SAFETY: the caller's contract, stated in the `# Safety` section here, is
    // `memcmp`'s.
    unsafe { memcmp(a, b, n) }
}

/// The length of the NUL-terminated string at `s`, without the NUL.
///
/// # Safety
///
/// `s` points to a readable byte string that ends in a NUL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strlen(s: *const c_char) -> usize {
    let s = s.cast::<u8>();
    let mut n = 0;
    // SAFETY: the caller's contract, stated in the `# Safety` section here,
    // makes every byte up to and including the NUL readable.
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    n
}
