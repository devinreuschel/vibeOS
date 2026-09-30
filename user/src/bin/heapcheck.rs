//! The in-guest test `user_heap_over_brk`'s program (ROADMAP §10.5): the
//! runtime's heap over `brk`. Only `kernel_tests` kernels embed it.
//!
//! It exits 0 when every check passes. On a failure it writes
//! `heapcheck: <check>` to fd 2 and exits 1.

#![no_std]
#![no_main]
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "a failed allocation ends this user process"
)]

extern crate alloc;

use alloc::alloc::{Layout, alloc as raw_alloc, dealloc, realloc};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::hint::black_box;

use vibeos_user::env::Env;
use vibeos_user::{eprintln, sys};

vibeos_user::main!(main);

/// A failed check: its words, for the `heapcheck: <check>` line.
type Check = Result<(), &'static str>;

fn main(_env: &Env) -> i32 {
    match run() {
        Ok(()) => 0,
        Err(check) => {
            eprintln!("heapcheck: {check}");
            1
        }
    }
}

fn ensure(ok: bool, what: &'static str) -> Check {
    if ok { Ok(()) } else { Err(what) }
}

/// The current break.
fn brk_now() -> Result<usize, &'static str> {
    // SAFETY: `brk(0)` moves nothing; established here.
    unsafe { sys::brk(0) }.map_err(|_| "brk(0) failed")
}

/// Box, a 64 KiB Vec built by push, and a 10,000-character String, and
/// 6,000 small boxes (192 KiB of one size class, more than two slabs); each
/// is checked, and dropped on return.
fn round() -> Check {
    let mut small: Vec<Box<[u8; 24]>> = Vec::with_capacity(6_000);
    for i in 0..6_000 {
        small.push(Box::new([i as u8; 24]));
    }
    ensure(
        small.iter().enumerate().all(|(i, b)| b[23] == i as u8),
        "a small box lost its contents",
    )?;
    let b = Box::new(0x1234_5678_9abc_def0u64);
    ensure(
        **black_box(&b) == 0x1234_5678_9abc_def0,
        "Box lost its value",
    )?;
    let mut v: Vec<u8> = Vec::new();
    for i in 0..64 * 1024 {
        v.push(i as u8);
    }
    ensure(v.len() == 64 * 1024, "Vec length")?;
    ensure(
        v.iter().enumerate().all(|(i, &x)| x == i as u8),
        "Vec lost its contents",
    )?;
    let mut s = String::new();
    for i in 0..10_000 {
        s.push(char::from(b'a' + (i % 26) as u8));
    }
    ensure(s.len() == 10_000, "String length")?;
    ensure(
        s.as_bytes()[9_999] == b'a' + (9_999 % 26) as u8,
        "String lost its contents",
    )
}

/// A raw allocation of `size` bytes aligned to `align` lands aligned.
fn aligned(size: usize, align: usize, what: &'static str) -> Check {
    let layout = Layout::from_size_align(size, align).map_err(|_| "layout")?;
    // SAFETY: `layout` has a non-zero size; established here.
    let p = unsafe { raw_alloc(layout) };
    ensure(!p.is_null(), "an aligned allocation returned null")?;
    let ok = (p as usize).is_multiple_of(align);
    // SAFETY: `p` came from `raw_alloc` with `layout`; established here.
    unsafe { dealloc(p, layout) };
    ensure(ok, what)
}

/// A 2 MiB Vec, then a `realloc` that moves it past what it holds, keeps
/// its contents.
fn big_realloc() -> Check {
    let n = 2 * 1024 * 1024 / 4;
    let mut v: Vec<u32> = Vec::with_capacity(n);
    for i in 0..n {
        v.push((i as u32).wrapping_mul(2_654_435_761));
    }
    ensure(v.len() == n, "2 MiB Vec length")?;
    let layout = Layout::from_size_align(4096, 16).map_err(|_| "layout")?;
    // SAFETY: `layout` has a non-zero size; established here.
    let p = unsafe { raw_alloc(layout) };
    ensure(!p.is_null(), "a 4 KiB allocation returned null")?;
    for i in 0..4096 {
        // SAFETY: `p` holds 4096 bytes; established here.
        unsafe { p.add(i).write(i as u8) };
    }
    // SAFETY: `p` came from `raw_alloc` with `layout`, and 3 MiB is a valid
    // size for its alignment; established here.
    let q = unsafe { realloc(p, layout, 3 * 1024 * 1024) };
    ensure(!q.is_null(), "realloc returned null")?;
    // SAFETY: `q` holds at least the 4096 bytes `realloc` kept; established
    // here.
    let kept = (0..4096).all(|i| unsafe { q.add(i).read() } == i as u8);
    let grown = Layout::from_size_align(3 * 1024 * 1024, 16).map_err(|_| "layout")?;
    // SAFETY: `q` came from `realloc` with the grown size; established here.
    unsafe { dealloc(q, grown) };
    ensure(kept, "realloc lost the contents")?;
    ensure(
        v.iter()
            .enumerate()
            .all(|(i, &x)| x == (i as u32).wrapping_mul(2_654_435_761)),
        "the 2 MiB Vec lost its contents",
    )
}

fn run() -> Check {
    let start = brk_now()?;
    round()?;
    let risen = brk_now()?;
    ensure(risen > start, "the break did not rise")?;
    round()?;
    ensure(brk_now()? == risen, "the same sizes again moved the break")?;
    aligned(100, 64, "a 64-byte alignment was not honoured")?;
    aligned(10, 4096, "a 4096-byte alignment was not honoured")?;
    aligned(
        8000,
        4096,
        "a 4096-byte alignment was not honoured for 8000 bytes",
    )?;
    big_realloc()?;
    let before = brk_now()?;
    let huge = Layout::from_size_align(isize::MAX as usize - 4095, 1).map_err(|_| "huge layout")?;
    // SAFETY: `huge` has a non-zero size; established here.
    let p = unsafe { raw_alloc(huge) };
    ensure(p.is_null(), "an allocation brk refuses did not return null")?;
    ensure(brk_now()? == before, "a refused allocation moved the break")
}
