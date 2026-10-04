//! The aarch64 port's pure half (PORTABILITY §11.1): descriptor encodings,
//! MAIR/TCR/SCTLR values, and the TLB-maintenance sequence.
//!
//! Compiles on every host. The hardware half (TTBR writes, `tlbi`, `msr`)
//! lives in the kernel crate.

pub mod paging;
pub mod sysreg;
pub mod tlb;
