//! Memory management: the kernel half of subsystem `mm` (DESIGN §1.3).

pub(crate) mod heap_init;
pub(crate) mod kva_init;
pub(crate) mod paging_init;
pub(crate) mod pmm_init;
