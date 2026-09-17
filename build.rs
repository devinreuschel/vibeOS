//! Cargo build script.
//!
//! Teach cargo about kernel inputs that live outside the crate root:
//!
//!   - linker.ld and the custom target JSON (rustc reads them; cargo
//!     would otherwise leave a stale ELF).
//!   - the AP trampoline: `nasm -f bin`, path anchored at
//!     `CARGO_MANIFEST_DIR`, assembler stderr captured (DESIGN §9.1).

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=x86_64-unknown-none-executable.json");
    println!("cargo:rerun-if-changed=build.rs");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let asm = manifest.join("src/trampoline.asm");
    println!("cargo:rerun-if-changed={}", asm.display());

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let blob = out.join("trampoline.bin");

    let output = Command::new("nasm")
        .args(["-f", "bin"])
        .arg(&asm)
        .arg("-o")
        .arg(&blob)
        .output()
        .unwrap_or_else(|e| panic!("nasm spawn failed: {e}"));

    if !output.status.success() {
        panic!(
            "nasm -f bin {} failed ({}):\nstdout:\n{}\nstderr:\n{}",
            asm.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    if !blob.is_file() {
        panic!("nasm produced no {}", blob.display());
    }
}
