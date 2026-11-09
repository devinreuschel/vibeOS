//! `/dev/random`'s hardware fill: virtio-rng then RDRAND on x86_64, and
//! RNDR then virtio-rng on aarch64, and nothing else until ROADMAP §13.10's
//! CSPRNG. A read gets a short count when they supply less than it asks
//! for, and `EAGAIN` when they supply none (ROADMAP §10.12, F134). S1.

use vibeos::entropy::{self, Source};
use vibeos::log::Level;

use crate::arch::current::hw_rng64;
use crate::virtio_init;

fn hw_fill(buf: &mut [u8]) -> (usize, Option<Source>) {
    #[cfg(feature = "kernel_tests")]
    if testing::dry() {
        return (0, None);
    }
    #[cfg(target_arch = "aarch64")]
    {
        let r = rng64_fill(buf, 0);
        let i = virtio_fill(buf, r);
        let src = if r > 0 {
            Some(Source::Rndr)
        } else if i > 0 {
            Some(Source::VirtioRng)
        } else {
            None
        };
        (i, src)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let v = virtio_fill(buf, 0);
        let i = rng64_fill(buf, v);
        let src = if v > 0 {
            Some(Source::VirtioRng)
        } else if i > 0 {
            Some(Source::RdRand)
        } else {
            None
        };
        (i, src)
    }
}

/// Fill `buf[i..]` from `hw_rng64` until it is full or the CPU gives up;
/// returns the new fill point.
fn rng64_fill(buf: &mut [u8], mut i: usize) -> usize {
    while i < buf.len() {
        let Some(x) = hw_rng64() else {
            break;
        };
        let b = x.to_le_bytes();
        let n = (buf.len() - i).min(8);
        buf[i..i + n].copy_from_slice(&b[..n]);
        i += n;
    }
    i
}

/// Fill `buf[i..]` from virtio-rng; returns the new fill point.
fn virtio_fill(buf: &mut [u8], i: usize) -> usize {
    if i >= buf.len() || !virtio_init::rng_bound() {
        return i;
    }
    let n = virtio_init::rng_take(&mut buf[i..]);
    // Fewer bytes than asked, 0 included, means the pool is empty:
    // ask for the next refill, which `rng_request` skips while one is
    // in flight, so an empty completion cannot stop refills (F121).
    if n < buf.len() - i {
        refill();
    }
    i + n
}

pub fn init() {
    entropy::set_hw_fill(hw_fill);
    if virtio_init::rng_bound() {
        refill();
    }
}
/// Queue the next virtio-rng buffer. A failure leaves reads to RDRAND
/// alone; it is logged at most once a second (DESIGN §2.5).
fn refill() {
    if let Err(e) = virtio_init::rng_request() {
        crate::klog_ratelimited!(
            1000,
            Level::Warn,
            "vibeOS: entropy: virtio-rng request failed: {}",
            e.as_str()
        );
    }
}

/// A machine with no hardware entropy, for `dev_random_eagain`.
#[cfg(feature = "kernel_tests")]
pub(crate) mod testing {
    use core::sync::atomic::{AtomicBool, Ordering};

    static DRY: AtomicBool = AtomicBool::new(false);

    /// While `on`, the hardware fill supplies no byte, as with neither
    /// virtio-rng nor `RDRAND`.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn set_dry(on: bool) {
        // Release: pairs with the Acquire load in `dry`.
        DRY.store(on, Ordering::Release);
    }

    pub(super) fn dry() -> bool {
        // Acquire: pairs with the Release store in `set_dry`.
        DRY.load(Ordering::Acquire)
    }
}
