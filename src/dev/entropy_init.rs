//! `/dev/random`'s hardware fill: virtio-rng, then RDRAND, and nothing
//! else until ROADMAP §13.10's CSPRNG. A read gets a short count when they
//! supply less than it asks for, and `EAGAIN` when they supply none
//! (ROADMAP §10.12, F134). S1.

use vibeos::entropy::{self, Source};
use vibeos::log::Level;

use crate::arch::current::hw_rng64;
use crate::virtio_init;

fn hw_fill(buf: &mut [u8]) -> (usize, Option<Source>) {
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
    if testing::dry() {
        return (0, None);
    }
    let mut i = 0usize;
    let mut src = None;
    if virtio_init::rng_bound() {
        let n = virtio_init::rng_take(&mut buf[i..]);
        if n > 0 {
            i += n;
            src = Some(Source::VirtioRng);
        }
        // Fewer bytes than asked, 0 included, means the pool is empty:
        // ask for the next refill, which `rng_request` skips while one is
        // in flight, so an empty completion cannot stop refills (F121).
        if n < buf.len() {
            refill();
        }
    }
    if i < buf.len() {
        while i < buf.len() {
            let Some(x) = hw_rng64() else {
                break;
            };
            let b = x.to_le_bytes();
            let n = (buf.len() - i).min(8);
            buf[i..i + n].copy_from_slice(&b[..n]);
            i += n;
        }
        if src.is_none() && i > 0 {
            #[cfg(target_arch = "x86_64")]
            {
                src = Some(Source::RdRand);
            }
            #[cfg(target_arch = "aarch64")]
            {
                src = Some(Source::Rndr);
            }
        }
    }
    (i, src)
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
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub(crate) mod testing {
    use core::sync::atomic::{AtomicBool, Ordering};

    static DRY: AtomicBool = AtomicBool::new(false);

    /// While `on`, the hardware fill supplies no byte, as with neither
    /// virtio-rng nor `RDRAND`.
    pub(crate) fn set_dry(on: bool) {
        // Release: pairs with the Acquire load in `dry`.
        DRY.store(on, Ordering::Release);
    }

    pub(super) fn dry() -> bool {
        // Acquire: pairs with the Release store in `set_dry`.
        DRY.load(Ordering::Acquire)
    }
}
