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

    // FAT32 initrd (ROADMAP §8.6 / §9.8). Always wrap user ELFs so a stale
    // workspace initrd.fat cannot boot a kernel without them.
    let script = manifest.join("scripts/mkinitrd.py");
    println!("cargo:rerun-if-changed={}", script.display());
    println!("cargo:rerun-if-changed=scripts/mkuserelf.py");
    println!("cargo:rerun-if-changed=user/hello.asm");
    println!("cargo:rerun-if-changed=user/init.asm");
    println!("cargo:rerun-if-changed=user/sh.asm");
    println!("cargo:rerun-if-changed=user/tests.asm");
    println!("cargo:rerun-if-changed=user/sys.inc");
    let initrd = out.join("initrd.fat");
    let wrap = manifest.join("scripts/mkuserelf.py");
    let user_dir = manifest.join("user");

    let mut mk = Command::new("python3");
    mk.arg(&script).arg(&initrd);

    for (stem, dest) in [
        ("hello", "/hello"),
        ("init", "/sbin/init"),
        ("sh", "/bin/sh"),
        ("tests", "/bin/tests"),
    ] {
        let asm = user_dir.join(format!("{stem}.asm"));
        let blob = out.join(format!("{stem}.bin"));
        let elf = out.join(format!("{stem}.elf"));
        let nasm = Command::new("nasm")
            .args(["-f", "bin", "-I"])
            .arg(format!("{}/", user_dir.display()))
            .arg(&asm)
            .arg("-o")
            .arg(&blob)
            .output()
            .unwrap_or_else(|e| panic!("nasm {stem}.asm: {e}"));
        if !nasm.status.success() {
            panic!(
                "nasm {stem}.asm failed:\n{}",
                String::from_utf8_lossy(&nasm.stderr)
            );
        }
        let wrap_out = Command::new("python3")
            .arg(&wrap)
            .arg(&blob)
            .arg(&elf)
            .output()
            .unwrap_or_else(|e| panic!("mkuserelf.py {stem}: {e}"));
        if !wrap_out.status.success() {
            panic!(
                "mkuserelf.py {stem} failed:\n{}",
                String::from_utf8_lossy(&wrap_out.stderr)
            );
        }
        mk.arg("--add").arg(format!("{}:{dest}", elf.display()));
    }

    let output = mk.output().unwrap_or_else(|e| panic!("mkinitrd.py spawn: {e}"));
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
