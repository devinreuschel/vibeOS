//! `/bin/tests`' suites (ROADMAP §10.5, C-USERTESTS): one module per area,
//! each exporting `pub fn run(t: &mut utest::Runner)`, run in `SUITES`'
//! order. A case name is unique across every module.

use core::sync::atomic::AtomicUsize;

use vibeos_user::utest;

mod console;
mod exec_args;
mod pid1;
mod process;
mod syscalls;
mod utils;

/// `/bin/tests`' first line.
pub const BANNER: &[u8] = b"user: tests begin\n";

/// What `/bin/tests`' write of [`BANNER`] returned, or `usize::MAX` on an
/// error; `write_count` checks it.
pub static BANNER_WRITE: AtomicUsize = AtomicUsize::new(usize::MAX);

/// The suites, in the order they run.
pub const SUITES: &[fn(&mut utest::Runner)] = &[
    syscalls::run,  // write, getpid, dup
    process::run,   // fork, execve, wait4, a fault, the table's limit
    exec_args::run, // execve's argv and envp, and their limits
    console::run,   // forged kernel lines
    pid1::run,      // init cannot be killed or stopped
    utils::run,     // the /bin utilities and /bin/sh
];
