//! Everything in the runtime that names an architecture (ROADMAP §10.5): the
//! entry point, the system call instruction and numbers, and the calls only
//! some architectures have. A port adds one directory here.

#[cfg(all(target_arch = "x86_64", target_os = "linux", target_env = "musl"))]
mod x86_64;
#[cfg(all(target_arch = "x86_64", target_os = "linux", target_env = "musl"))]
pub use x86_64::*;

#[cfg(all(target_arch = "aarch64", target_os = "linux", target_env = "musl"))]
mod aarch64;
#[cfg(all(target_arch = "aarch64", target_os = "linux", target_env = "musl"))]
pub use aarch64::*;

#[cfg(not(any(
    all(target_arch = "x86_64", target_os = "linux", target_env = "musl"),
    all(target_arch = "aarch64", target_os = "linux", target_env = "musl"),
)))]
compile_error!(
    "vibeos-user builds only for <arch>-unknown-linux-musl, through `make user` (ROADMAP §10.5)"
);
