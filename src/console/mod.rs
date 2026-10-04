//! Console: the kernel half of subsystem `console` (DESIGN §1.3).

pub(crate) mod console_init;
pub(crate) mod fb_init;
#[cfg(target_arch = "x86_64")]
pub(crate) mod kbd_init;
#[cfg(all(feature = "kernel_tests", target_arch = "aarch64"))]
pub(crate) mod kbd_init {
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use vibeos::kbd::DecodedKey;

    #[cfg_attr(
        target_arch = "aarch64",
        expect(dead_code, reason = "boot-CPU S7; unused on this path")
    )]
    pub(crate) const GSI_NONE: u32 = u32::MAX;
    #[cfg_attr(
        target_arch = "aarch64",
        expect(dead_code, reason = "boot-CPU S7; unused on this path")
    )]
    pub(crate) static LIVE: AtomicBool = AtomicBool::new(false);
    #[cfg_attr(
        target_arch = "aarch64",
        expect(dead_code, reason = "boot-CPU S7; unused on this path")
    )]
    pub(crate) static GSI: AtomicU32 = AtomicU32::new(GSI_NONE);
    #[cfg_attr(
        target_arch = "aarch64",
        expect(dead_code, reason = "boot-CPU S7; unused on this path")
    )]
    pub(crate) static PIC_FALLBACK: AtomicBool = AtomicBool::new(false);

    #[cfg_attr(
        target_arch = "aarch64",
        expect(dead_code, reason = "boot-CPU S7; unused on this path")
    )]
    pub(crate) fn pop() -> Option<DecodedKey> {
        None
    }
    #[cfg_attr(
        target_arch = "aarch64",
        expect(dead_code, reason = "boot-CPU S7; unused on this path")
    )]
    pub(crate) fn with_kbd<R>(_f: impl FnOnce(&mut Kbd) -> R) -> Option<R> {
        None
    }
    #[cfg_attr(
        target_arch = "aarch64",
        expect(dead_code, reason = "boot-CPU S7; unused on this path")
    )]
    pub(crate) fn flush_obf() {}
    #[cfg_attr(
        target_arch = "aarch64",
        expect(dead_code, reason = "boot-CPU S7; unused on this path")
    )]
    pub(crate) fn write_cmd(_c: u8) -> bool {
        false
    }
    #[cfg_attr(
        target_arch = "aarch64",
        expect(dead_code, reason = "boot-CPU S7; unused on this path")
    )]
    pub(crate) fn write_data(_c: u8) -> bool {
        false
    }
    #[cfg_attr(
        target_arch = "aarch64",
        expect(dead_code, reason = "boot-CPU S7; unused on this path")
    )]
    pub(crate) fn read_data() -> Option<u8> {
        None
    }

    #[cfg_attr(
        target_arch = "aarch64",
        expect(dead_code, reason = "boot-CPU S7; unused on this path")
    )]
    pub(crate) struct Kbd {
        pub(crate) ring: Ring,
    }
    #[cfg_attr(
        target_arch = "aarch64",
        expect(dead_code, reason = "boot-CPU S7; unused on this path")
    )]
    pub(crate) struct Ring;
    impl Ring {
        #[cfg_attr(
            target_arch = "aarch64",
            expect(dead_code, reason = "boot-CPU S7; unused on this path")
        )]
        pub(crate) fn push(&mut self, _k: DecodedKey) {}
    }

    #[allow(dead_code)]
    fn _keep_ordering() {
        // Relaxed: pairs with nothing.
        let _ = Ordering::Relaxed;
    }
}
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::let_underscore_must_use,
    clippy::unused_result_ok,
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "kernel_tests-only in-guest tests: a failure ends a test, not the kernel"
)]
pub mod ktest;
