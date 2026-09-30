//! The x86_64 port: its portable half (`vibeos-core`): encodings and trap decode (DESIGN §1.3).

pub mod apic;
pub mod desc;
pub mod paging;
pub mod pic;
pub mod trap;
pub mod uart;
pub mod vectors;
