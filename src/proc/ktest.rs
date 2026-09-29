//! In-guest tests for proc (kernel_tests only). Rows: the list in crate::ktest.

mod entry;
mod exec;
mod hooks;
mod lifecycle;

pub(crate) use entry::*;
pub(crate) use exec::*;
pub(crate) use hooks::*;
pub(crate) use lifecycle::*;
