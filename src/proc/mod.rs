//! Processes: the kernel half of subsystem `proc` (DESIGN §1.3).

pub(crate) mod addr_space_init;
pub(crate) mod proc_init;
pub(crate) mod syscall_init;
pub(crate) mod user_init;
