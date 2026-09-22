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

    // FAT32 initrd (ROADMAP §8.6 / §9.8). Always wrap /hello so a stale
    // workspace initrd.fat cannot boot a kernel without the user ELF.
    let script = manifest.join("scripts/mkinitrd.py");
    println!("cargo:rerun-if-changed={}", script.display());
    println!("cargo:rerun-if-changed=scripts/mkuserelf.py");
    println!("cargo:rerun-if-changed=user/hello.asm");
    let initrd = out.join("initrd.fat");
    let hello_asm = manifest.join("user/hello.asm");
    let wrap = manifest.join("scripts/mkuserelf.py");
    let blob = out.join("hello.bin");
    let elf = out.join("hello.elf");
    let nasm = Command::new("nasm")
        .args(["-f", "bin"])
        .arg(&hello_asm)
        .arg("-o")
        .arg(&blob)
        .output()
        .unwrap_or_else(|e| panic!("nasm hello.asm: {e}"));
    if !nasm.status.success() {
        panic!(
            "nasm hello.asm failed:\n{}",
            String::from_utf8_lossy(&nasm.stderr)
        );
    }
    let wrap_out = Command::new("python3")
        .arg(&wrap)
        .arg(&blob)
        .arg(&elf)
        .output()
        .unwrap_or_else(|e| panic!("mkuserelf.py: {e}"));
    if !wrap_out.status.success() {
        panic!(
            "mkuserelf.py failed:\n{}",
            String::from_utf8_lossy(&wrap_out.stderr)
        );
    }
    let output = Command::new("python3")
        .arg(&script)
        .arg(&initrd)
        .arg("--add")
        .arg(format!("{}:/hello", elf.display()))
        .output()
        .unwrap_or_else(|e| panic!("mkinitrd.py spawn: {e}"));
    if !output.status.success() {
        panic!(
            "mkinitrd.py failed ({}):\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    if !initrd.is_file() {
        panic!("no {}", initrd.display());
    }
}
