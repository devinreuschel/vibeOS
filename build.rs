//! Cargo build script.
//!
//! Teach cargo about kernel inputs that live outside the crate root:
//!
//!   - linker.ld as an absolute `-T` so the link works from any cwd
//!     (DESIGN §9.1).
//!   - `VIBEOS_KSYMS` staged by the Makefile. An empty fallback so
//!     `cargo check` works without `make`.
//!   - `VIBEOS_USER_BINS` and `VIBEOS_USER_DIR`: in `kernel_tests` builds,
//!     the user programs `make user` built, copied into OUT_DIR and
//!     embedded by bare name for `Image::UserBin` (C-USERBINS). An empty
//!     table otherwise, so bare `cargo clippy --features kernel_tests`
//!     works without `make`.

use std::env;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let target = env::var("TARGET").unwrap_or_default();
    let linker = if target.starts_with("aarch64") {
        manifest.join("linker-aarch64.ld")
    } else {
        manifest.join("linker.ld")
    };
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

    println!("cargo:rerun-if-env-changed=VIBEOS_USER_BINS");
    println!("cargo:rerun-if-env-changed=VIBEOS_USER_DIR");
    let mut bins = std::fs::File::create(out.join("user_bins.rs")).unwrap();
    writeln!(bins, "pub(crate) static USER_BINS: &[(&str, &[u8])] = &[").unwrap();
    if env::var_os("CARGO_FEATURE_KERNEL_TESTS").is_some()
        && let (Ok(names), Ok(dir)) = (env::var("VIBEOS_USER_BINS"), env::var("VIBEOS_USER_DIR"))
    {
        for name in names.split_whitespace() {
            let path = PathBuf::from(&dir).join(name);
            if !path.is_file() {
                panic!(
                    "VIBEOS_USER_BINS names {name}, but {} is missing; run `make user`",
                    path.display()
                );
            }
            println!("cargo:rerun-if-changed={}", path.display());
            // A copy beside user_bins.rs, included by its bare name: an
            // absolute path in `include_bytes!` is the checkout's, and it
            // reaches the `.llvm.<hash>` suffixes of the symbols LLVM
            // promotes, so two checkouts built different kernels.
            std::fs::copy(&path, out.join(name)).unwrap();
            writeln!(bins, "    ({name:?}, include_bytes!({name:?})),").unwrap();
        }
    }
    writeln!(bins, "];").unwrap();
}
