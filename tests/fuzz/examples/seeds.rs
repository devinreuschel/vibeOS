//! Regenerate the committed seeds: every `corpus/<target>/seed-*` file and
//! the regressions the generator owns (C-FUZZ). Other files under
//! `regressions/` are crash inputs and are never touched.
//!
//! `CARGO_TARGET_DIR=target/fuzz cargo run --manifest-path tests/fuzz/Cargo.toml --locked --target "$HOST" --example seeds`

#![allow(
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "host tool: `alloc`'s growing calls may panic, and a failed allocation ends this host process, not the kernel (DESIGN §4.4)"
)]

use std::fs;
use std::path::Path;

use vibeos_fuzz::{TARGETS, seeds};

fn main() -> std::io::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for t in TARGETS {
        let dir = root.join("corpus").join(t.name);
        fs::create_dir_all(&dir)?;
        for e in fs::read_dir(&dir)? {
            let p = e?.path();
            if p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("seed-"))
            {
                fs::remove_file(p)?;
            }
        }
    }
    let mut n = 0usize;
    for s in seeds::corpus() {
        fs::write(root.join("corpus").join(s.target).join(&s.name), &s.data)?;
        n += 1;
    }
    for s in seeds::regressions() {
        let dir = root.join("regressions").join(s.target);
        fs::create_dir_all(&dir)?;
        fs::write(dir.join(&s.name), &s.data)?;
        n += 1;
    }
    println!("seeds: wrote {n} files");
    Ok(())
}
