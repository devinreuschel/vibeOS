//! Entropy source selection for `/dev/random`. S1.
//!
//! Hardware fill is a hook the kernel installs (`entropy_init`): virtio-rng,
//! then `RDRAND`, and nothing else until ROADMAP §13.10's CSPRNG. Host tests
//! and the pre-hook boot path leave it unset, and a read then gets no bytes
//! (ROADMAP §10.12, F134).

// Statics only, so `core`'s atomics from the seam's statics re-export
// (C-ATOMICS).
use crate::atomic::statics::{AtomicPtr, AtomicU8, Ordering};

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    VirtioRng = 0,
    RdRand = 1,
    Rndr = 2,
}

impl Source {
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::VirtioRng),
            1 => Some(Self::RdRand),
            2 => Some(Self::Rndr),
            _ => None,
        }
    }
}

/// Fill a buffer from hardware; returns how many bytes it wrote and the
/// source that supplied them (virtio-rng when it supplied any), `None` when
/// it wrote none.
pub type HwFill = fn(&mut [u8]) -> (usize, Option<Source>);

/// `u8::MAX`, no source: nothing has filled a byte yet.
const NO_SOURCE: u8 = u8::MAX;

static HW: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static LAST: AtomicU8 = AtomicU8::new(NO_SOURCE);

pub fn set_hw_fill(f: HwFill) {
    // Release: pairs with the Acquire load in `hw_fill`.
    HW.store(f as *mut (), Ordering::Release);
}

/// The source of the last hardware fill that returned bytes; `None` before
/// the first.
pub fn last_source() -> Option<Source> {
    // Acquire: pairs with the Release store in `set_last_source`.
    Source::from_u8(LAST.load(Ordering::Acquire))
}

pub fn set_last_source(src: Source) {
    // Release: pairs with the Acquire load in `last_source`.
    LAST.store(src as u8, Ordering::Release);
}

/// Fill from virtio-rng and RDRAND; returns how many bytes it wrote. `0`
/// means the hook is missing or both sources are dry.
pub fn hw_fill(buf: &mut [u8]) -> usize {
    // Acquire: pairs with the Release store in `set_hw_fill`.
    let p = HW.load(Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: invariant: a non-null `HW` holds a `HwFill`; established by
    // `entropy::set_hw_fill`, its only non-null store.
    let f: HwFill = unsafe { core::mem::transmute(p) };
    let (n, src) = f(buf);
    if n > 0
        && let Some(s) = src
    {
        set_last_source(s);
    }
    n.min(buf.len())
}

/// Serializes host tests that install a hook or rely on none: the hook is
/// process-global and `cargo test` runs tests in parallel.
#[cfg(test)]
static TEST_HOOK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A host test's hold on the hook, which it clears on drop.
#[cfg(test)]
pub struct HookGuard {
    _g: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for HookGuard {
    fn drop(&mut self) {
        HW.store(core::ptr::null_mut(), Ordering::Release);
        LAST.store(NO_SOURCE, Ordering::Release);
    }
}

/// Take the test lock, install `f` (or none), and reset the last source.
#[cfg(test)]
pub fn test_hook(f: Option<HwFill>) -> HookGuard {
    let g = TEST_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match f {
        Some(f) => set_hw_fill(f),
        None => HW.store(core::ptr::null_mut(), Ordering::Release),
    }
    LAST.store(NO_SOURCE, Ordering::Release);
    HookGuard { _g: g }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_hook_fills_nothing() {
        let _g = test_hook(None);
        let mut b = [0xAAu8; 8];
        assert_eq!(hw_fill(&mut b), 0);
        assert_eq!(b, [0xAAu8; 8]);
        assert_eq!(last_source(), None);
    }

    #[test]
    fn source_roundtrip() {
        assert_eq!(Source::from_u8(0), Some(Source::VirtioRng));
        assert_eq!(Source::from_u8(1), Some(Source::RdRand));
        assert_eq!(Source::from_u8(2), Some(Source::Rndr));
        assert_eq!(Source::from_u8(3), None);
        assert_eq!(Source::from_u8(u8::MAX), None);
        assert_eq!(Source::from_u8(Source::RdRand as u8), Some(Source::RdRand));
        assert_eq!(Source::from_u8(Source::Rndr as u8), Some(Source::Rndr));
    }
}
