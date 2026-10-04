//! Memory management: the portable half (`vibeos-core`) of subsystem `mm` (DESIGN §1.3).

pub mod heap;
pub mod kva;
pub mod paging;
pub mod physmap;
pub mod pmm;
