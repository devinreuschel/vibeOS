//! Cargo build script.
//!
//! Its whole job is to teach cargo about kernel inputs that live outside the
//! crate root and would otherwise not fingerprint into the rebuild graph:
//!
//!   - linker.ld: rustc reads it (via the target spec's pre-link-args) at
//!     link time. Cargo does not know that, so editing the section layout
//!     would leave a stale ELF on disk.
//!   - the custom target JSON: same story, the compiler ingests it but
//!     cargo does not track it as a crate input.
//!
//! Emitting `cargo:rerun-if-changed=<path>` puts them in the dependency
//! graph so `cargo build` rebuilds when either changes.

fn main() {
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=x86_64-unknown-none-executable.json");
    println!("cargo:rerun-if-changed=build.rs");
}
