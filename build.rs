//! Cargo build script.
//!
//! Teach cargo about kernel inputs that live outside the crate root:
//!
//!   - linker.ld as an absolute `-T` so the link works from any cwd
//!     (DESIGN §9.1).
//!   - the AP trampoline: `nasm -f bin`, path anchored at
//!     `CARGO_MANIFEST_DIR`, assembler stderr captured (DESIGN §9.1).
//!   - `VIBEOS_KSYMS` / `VIBEOS_INITRD` staged by the Makefile. Empty
//!     fallbacks so `cargo check` works without `make`.

use std::env;
use std::path::PathBuf;
use std::process::Command;

const INITRD_BYTES: usize = 64 * 1024;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let linker = manifest.join("linker.ld");
    println!("cargo:rerun-if-changed={}", linker.display());
    // First link arg so rust-lld sees the script before other flags.
    println!("cargo:rustc-link-arg-bins=-T{}", linker.display());

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

    println!("cargo:rerun-if-env-changed=VIBEOS_KSYMS");
    let ksyms_out = out.join("ksyms.rs");
    if let Ok(src) = env::var("VIBEOS_KSYMS") {
        println!("cargo:rerun-if-changed={src}");
        let body = std::fs::read_to_string(&src).unwrap_or_else(|e| {
            panic!("read VIBEOS_KSYMS {src}: {e}");
        });
        std::fs::write(&ksyms_out, body).unwrap();
    } else {
        std::fs::write(
            &ksyms_out,
            "pub static KSYMS: &[vibeos::symtab::Entry] = &[];\n",
        )
        .unwrap();
    }

    println!("cargo:rerun-if-env-changed=VIBEOS_INITRD");
    let initrd = out.join("initrd.fat");
    if let Ok(src) = env::var("VIBEOS_INITRD") {
        println!("cargo:rerun-if-changed={src}");
        let body = std::fs::read(&src).unwrap_or_else(|e| {
            panic!("read VIBEOS_INITRD {src}: {e}");
        });
        if body.len() != INITRD_BYTES {
            panic!(
                "VIBEOS_INITRD {src} is {} bytes, expected {INITRD_BYTES}; run `make`, not bare `cargo build`",
                body.len()
            );
        }
        std::fs::write(&initrd, body).unwrap();
    } else {
        println!(
            "cargo:warning=VIBEOS_INITRD unset; embedding empty initrd. Run `make`, not bare `cargo build`."
        );
        std::fs::write(&initrd, [0u8; INITRD_BYTES]).unwrap();
    }
}
