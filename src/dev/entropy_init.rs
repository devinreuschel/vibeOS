//! `/dev/random`'s hardware fill: virtio-rng, then RDRAND, and nothing
//! else until ROADMAP §13.10's CSPRNG. A read gets a short count when they
//! supply less than it asks for, and `EAGAIN` when they supply none
//! (ROADMAP §10.12, F134). S1.

use vibeos::entropy::{self, Source};
use vibeos::log::Level;

use crate::virtio_init;
use crate::x86;

fn hw_fill(buf: &mut [u8]) -> (usize, Option<Source>) {
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
        if src.is_none() && i > 0 {
            src = Some(Source::RdRand);
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
