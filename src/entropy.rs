//! Entropy source selection for `/dev/random`. S1.
//!
//! Hardware fill is a hook the kernel installs (`entropy_init`). Host
//! tests and the pre-hook boot path leave it unset; kernfs then uses
//! xorshift.

use core::sync::atomic::{AtomicPtr, AtomicU8, Ordering};

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    VirtioRng = 0,
    RdRand = 1,
    XorShift = 2,
}

impl Source {
    pub const fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::VirtioRng,
            1 => Self::RdRand,
            _ => Self::XorShift,
        }
    }
}

type HwFill = fn(&mut [u8]) -> (usize, Source);
type WarnFn = fn();

static HW: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static WARN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static LAST: AtomicU8 = AtomicU8::new(Source::XorShift as u8);

pub fn set_hw_fill(f: HwFill) {
    HW.store(f as *mut (), Ordering::Release);
}

pub fn set_warn(f: WarnFn) {
    WARN.store(f as *mut (), Ordering::Release);
}

pub fn last_source() -> Source {
    Source::from_u8(LAST.load(Ordering::Acquire))
}

pub fn set_last_source(src: Source) {
    LAST.store(src as u8, Ordering::Release);
}

/// Fill from virtio-rng / RDRAND. `0` means the hook is missing or dry.
pub fn hw_fill(buf: &mut [u8]) -> usize {
    let p = HW.load(Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    let f: HwFill = unsafe { core::mem::transmute(p) };
    let (n, src) = f(buf);
    if n > 0 {
        set_last_source(src);
    }
    n.min(buf.len())
}

/// One-time warning when kernfs falls back to xorshift. Host: no-op.
pub fn warn_xorshift() {
    let p = WARN.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    let f: WarnFn = unsafe { core::mem::transmute(p) };
    f();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_hook_fills_nothing() {
        let mut b = [0xAAu8; 8];
        assert_eq!(hw_fill(&mut b), 0);
        assert_eq!(b, [0xAAu8; 8]);
        assert_eq!(last_source(), Source::XorShift);
    }

    #[test]
    fn source_roundtrip() {
        assert_eq!(Source::from_u8(0), Source::VirtioRng);
        assert_eq!(Source::from_u8(1), Source::RdRand);
        assert_eq!(Source::from_u8(2), Source::XorShift);
        assert_eq!(Source::from_u8(99), Source::XorShift);
    }
}
