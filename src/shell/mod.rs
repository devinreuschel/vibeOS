//! Shell: the kernel half of subsystem `shell` (DESIGN §1.3).

pub(crate) mod cmds;
#[cfg(feature = "kernel_tests")]
pub mod ktest;
pub(crate) mod shell_init;
