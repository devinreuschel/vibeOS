//! Host wrapper. Pulls in the same portable modules as `src/lib.rs`, but
//! compiled against the host toolchain so `cargo test --lib` runs their unit
//! tests without any kernel-target machinery.

// The kernel's own lib.rs also references these modules, so this stays in
// sync automatically.
#[path = "../../../src/fmt_util.rs"]
pub mod fmt_util;

#[path = "../../../src/marker.rs"]
pub mod marker;

#[path = "../../../src/pmm.rs"]
pub mod pmm;

#[path = "../../../src/uart.rs"]
pub mod uart;

#[cfg(test)]
mod smoke {
    use super::marker;

    #[test]
    fn markers_are_lowercase_prefixed() {
        for m in [
            marker::SERIAL_ONLINE,
            marker::LIMINE_OK,
            marker::PMM_PREFIX,
            marker::BOOT_DONE,
        ] {
            assert!(m.starts_with("vibeOS: "), "marker missing prefix: {m}");
            assert!(!m.ends_with(['.', '!']), "marker has trailing punct: {m}");
        }
        // The PMM line is assembled at runtime: prefix, decimal count,
        // suffix. Confirm the fragments agree with the phase-1 exit-gate
        // string in the roadmap.
        assert_eq!(marker::PMM_PREFIX, "vibeOS: pmm: ");
        assert_eq!(marker::PMM_FREE_SUFFIX, " free 4KiB frames");
    }

    #[test]
    fn panic_banner_short_enough_for_uart() {
        // The panic path prints this before allocating anything. Keep it
        // small enough that the DESIGN §9.6 tx-poll cap never bites.
        assert!(marker::PANIC_BANNER.len() < 64);
    }
}
