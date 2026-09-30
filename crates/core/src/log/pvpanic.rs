//! QEMU's `pvpanic` device: which event the panic path writes (ROADMAP
//! §10.7; INVARIANTS §2.5 steps 6 and 7). The kernel half
//! (`log::pvpanic_init`) finds the ISA device's port through fw_cfg's
//! `etc/pvpanic-port` at boot and reads the port once: the byte a read
//! returns is the set of events the device supports. A write of one event
//! bit reports that event to the host.
//!
//! Source, documentation only (DESIGN §1.5: cited, no text copied): QEMU's
//! `docs/specs/pvpanic.rst`, which defines bit 0 (the guest panicked) and
//! bit 1 (the guest loaded a crash kernel) and says a read returns the
//! supported bits.
//!
//! The state the kernel keeps is one `u32`, [`pack`] of the port and the
//! mask, so one atomic load gives the panic path both. Port 0 means no
//! device: the ISA device never sits at port 0.

/// Bit 0: the guest panicked. INVARIANTS §2.5 step 7 writes it before the
/// final halt; the harness's QEMU pauses on it (`-action panic=pause`).
pub const PANICKED: u8 = 1 << 0;
/// Bit 1: the guest loaded a crash kernel. INVARIANTS §2.5 step 6 writes it
/// before the capture jump; a host records it without stopping the guest.
pub const CRASH_LOADED: u8 = 1 << 1;

/// The panic path's two steps that signal the device (INVARIANTS §2.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Step 6: about to jump to a loaded capture kernel (ROADMAP §25.4).
    CaptureJump,
    /// Step 7: the dump has ended and the CPU halts.
    Halt,
}

/// The kernel's pvpanic state: `port` in the low 16 bits, the `supported`
/// mask above it. 0 is no device.
pub const fn pack(port: u16, supported: u8) -> u32 {
    if port == 0 {
        return 0;
    }
    (port as u32) | ((supported as u32) << 16)
}

/// The one event `step` writes, when the device supports it: `Halt` writes
/// [`PANICKED`], `CaptureJump` [`CRASH_LOADED`]. No other bit is ever
/// written, whatever else `supported` holds.
pub fn event_for(step: Step, supported: u8) -> Option<u8> {
    let bit = match step {
        Step::Halt => PANICKED,
        Step::CaptureJump => CRASH_LOADED,
    };
    (supported & bit != 0).then_some(bit)
}

/// The port write `step` makes from `state` ([`pack`]): `None` when there
/// is no device or it does not support the step's event.
pub fn write_for(state: u32, step: Step) -> Option<(u16, u8)> {
    let port = (state & 0xFFFF) as u16;
    if port == 0 {
        return None;
    }
    let supported = ((state >> 16) & 0xFF) as u8;
    event_for(step, supported).map(|ev| (port, ev))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORT: u16 = 0x505;

    #[test]
    fn halt_writes_panicked_only_when_supported() {
        assert_eq!(
            write_for(pack(PORT, 0x3), Step::Halt),
            Some((PORT, PANICKED))
        );
        assert_eq!(
            write_for(pack(PORT, 0x1), Step::Halt),
            Some((PORT, PANICKED))
        );
        assert_eq!(write_for(pack(PORT, 0x2), Step::Halt), None);
        assert_eq!(write_for(pack(PORT, 0x0), Step::Halt), None);
    }

    #[test]
    fn capture_jump_writes_crash_loaded_only_when_supported() {
        assert_eq!(
            write_for(pack(PORT, 0x3), Step::CaptureJump),
            Some((PORT, CRASH_LOADED))
        );
        assert_eq!(
            write_for(pack(PORT, 0x2), Step::CaptureJump),
            Some((PORT, CRASH_LOADED))
        );
        assert_eq!(write_for(pack(PORT, 0x1), Step::CaptureJump), None);
    }

    #[test]
    fn unknown_bits_are_never_written() {
        for mask in [0x04u8, 0xFC] {
            assert_eq!(event_for(Step::Halt, mask), None);
            assert_eq!(event_for(Step::CaptureJump, mask), None);
        }
        for mask in 0..=u8::MAX {
            for step in [Step::Halt, Step::CaptureJump] {
                if let Some((_, ev)) = write_for(pack(PORT, mask), step) {
                    assert!(ev == PANICKED || ev == CRASH_LOADED);
                    assert_eq!(ev.count_ones(), 1);
                    assert_ne!(mask & ev, 0);
                }
            }
        }
    }

    #[test]
    fn absent_device_writes_nothing() {
        assert_eq!(pack(0, 0xFF), 0);
        for step in [Step::Halt, Step::CaptureJump] {
            assert_eq!(write_for(0, step), None);
            assert_eq!(write_for(pack(0, 0x3), step), None);
        }
    }

    #[test]
    fn pack_keeps_port_and_mask() {
        let s = pack(0xBEEF, 0xA5);
        assert_eq!(s & 0xFFFF, 0xBEEF);
        assert_eq!((s >> 16) & 0xFF, 0xA5);
        assert_eq!(s >> 24, 0);
        assert_eq!(pack(PORT, 0), PORT as u32);
    }
}
