//! virtio-input in-guest tests. ROADMAP §11.5.

use vibeos::dev::DevState;
use vibeos::virtio::{DEV_INPUT_MODERN, VENDOR_ID};

use crate::dev_init;
use crate::ktest::Outcome;
use crate::virtio_input_init;

use super::find_id;

/// Keyboard and pointer both bind (ROADMAP §11.5).
pub(crate) fn test_virtio_input() -> Outcome {
    let Some(d) = find_id(VENDOR_ID, DEV_INPUT_MODERN) else {
        return Outcome::Skip("no virtio-input");
    };
    let mut n = 0u64;
    let mut i = 0usize;
    while let Some(dev) = dev_init::get(i) {
        if dev.vendor == VENDOR_ID && dev.device_id == DEV_INPUT_MODERN {
            n = n.saturating_add(1);
            match dev_init::bound(&dev) {
                Some("virtio-input") => {}
                Some(_) => return Outcome::Fail("wrong driver"),
                None => return Outcome::Fail("id match"),
            }
            if dev_init::state(&dev) != Some(DevState::Bound) {
                return Outcome::Fail("not Bound");
            }
        }
        i = i.saturating_add(1);
    }
    if n < 2 {
        return Outcome::Fail("need keyboard and pointer");
    }
    if virtio_input_init::bound() != n {
        return Outcome::Fail("unbound");
    }
    match dev_init::bound(&d) {
        Some("virtio-input") => Outcome::Ok,
        Some(_) => Outcome::Fail("wrong driver"),
        None => Outcome::Fail("id match"),
    }
}
