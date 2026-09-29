//! Cargo build script.
//!
//! Teach cargo about kernel inputs that live outside the crate root:
//!
//!   - linker.ld as an absolute `-T` so the link works from any cwd
//!     (DESIGN §9.1).
//!   - `VIBEOS_KSYMS` staged by the Makefile. An empty fallback so
//!     `cargo check` works without `make`.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let linker = manifest.join("linker.ld");
    println!("cargo:rerun-if-changed={}", linker.display());
    // First link arg so rust-lld sees the script before other flags.
    println!("cargo:rustc-link-arg-bins=-T{}", linker.display());

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-env-changed=VIBEOS_KSYMS");
    let ksyms_out = out.join("ksyms.rs");
    if let Ok(src) = env::var("VIBEOS_KSYMS") {
        println!("cargo:rerun-if-changed={src}");
        let body = std::fs::read_to_string(&src).unwrap_or_else(|e| {
            panic!("read VIBEOS_KSYMS {src}: {e}");
        });
        std::fs::write(&ksyms_out, body).unwrap();
    } else {
        // The empty form of what `scripts/gen_ksyms.py` renders.
        std::fs::write(
            &ksyms_out,
            "#[used]\n#[unsafe(link_section = \".ksyms\")]\n\
             static KSYMS: [vibeos::symtab::Entry; 0] = [\n];\n",
        )
        .unwrap();
    }
}
