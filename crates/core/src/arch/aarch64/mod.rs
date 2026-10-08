//! The aarch64 port's pure half (PORTABILITY §11.1): descriptor encodings,
//! MAIR/TCR/SCTLR values, the TLB-maintenance sequence, and trap decode.
//!
//! Compiles on every host. The hardware half (TTBR writes, `tlbi`, `msr`)
//! lives in the kernel crate.

pub mod fcntl;
pub mod paging;
pub mod psci;
pub mod stat;
pub mod sysreg;
pub mod tlb;
pub mod trap;
