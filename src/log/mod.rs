//! Logging: the kernel half of subsystem `log` (DESIGN §1.3).

pub(crate) mod diag;
pub(crate) mod ksyms;
pub(crate) mod log_init;
pub(crate) mod panic;
pub(crate) mod serial;
