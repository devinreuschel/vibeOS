//! In-guest tests for proc (kernel_tests only). Rows: the list in crate::ktest.

mod entry;
mod exec;
mod hooks;
mod lifecycle;
mod runtime;
mod sysdecl;
mod uaccess;

pub(crate) use entry::*;
pub(crate) use exec::*;
pub(crate) use hooks::*;
pub(crate) use lifecycle::*;
pub(crate) use runtime::*;
pub(crate) use sysdecl::*;
pub(crate) use uaccess::*;
