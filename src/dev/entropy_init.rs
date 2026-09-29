//! virtio-rng, then RDRAND, then kernfs xorshift. S1.

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::entropy::{self, Source};
use vibeos::log::Level;

use crate::virtio_init;
use crate::x86;

static WARNED: AtomicBool = AtomicBool::new(false);

fn hw_fill(buf: &mut [u8]) -> (usize, Source) {
    let mut i = 0usize;
    let mut src = Source::XorShift;
    if virtio_init::rng_bound() {
        let n = virtio_init::rng_take(&mut buf[i..]);
        if n > 0 {
            i += n;
            src = Source::VirtioRng;
        }
        // Fewer bytes than asked, 0 included, means the pool is empty:
        // ask for the next refill, which `rng_request` skips while one is
        // in flight, so an empty completion cannot stop refills (F121).
        if n < buf.len() {
            refill();
        }
    }
    if i < buf.len() && x86::has_rdrand() {
        while i < buf.len() {
            let Some(x) = x86::rdrand64() else {
                break;
            };
            let b = x.to_le_bytes();
            let n = (buf.len() - i).min(8);
            buf[i..i + n].copy_from_slice(&b[..n]);
            i += n;
        }
        if src == Source::XorShift && i > 0 {
            src = Source::RdRand;
        }
    }
    (i, src)
}

fn warn_once() {
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    crate::klog!(
        Level::Warn,
        "vibeOS: entropy: /dev/random using xorshift fallback"
    );
}

pub fn init() {
    entropy::set_hw_fill(hw_fill);
    entropy::set_warn(warn_once);
    if virtio_init::rng_bound() {
        refill();
    }
}

/// Queue the next virtio-rng buffer. A failure leaves the pool to drain
/// into RDRAND or xorshift; it is counted and logged at most once a second
/// (DESIGN §2.5).
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
