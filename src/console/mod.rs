//! Console: the kernel half of subsystem `console` (DESIGN §1.3).

pub(crate) mod console_init;
pub(crate) mod fb_init;
pub(crate) mod kbd_init;
#[cfg(feature = "kernel_tests")]
pub mod ktest;
