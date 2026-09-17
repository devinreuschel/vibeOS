//! vibeOS: portable, host-testable half.
//!
//! Anything that has no hardware access lives here so `cargo test --lib`
//! covers it. Serial byte formatting, marker strings, small utilities.
//! Hardware pokes live in the binary crate. See DESIGN §1.1.

#![cfg_attr(not(test), no_std)]

pub mod acpi;
pub mod fmt_util;
pub mod heap;
pub mod kva;
pub mod marker;
pub mod paging;
pub mod pmm;
pub mod uart;
