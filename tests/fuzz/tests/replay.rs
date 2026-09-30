//! Replay and structural tests for the fuzz crate (C-FUZZ, TESTING.md §8.1).
//! `make check` runs them through `make fuzz-check`; nothing here fuzzes.

#![allow(
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "host tests: `alloc`'s growing calls may panic, and a failed allocation ends the test, not the kernel (DESIGN §4.4)"
)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use vibeos_fuzz::TARGETS;

/// How long one input may run before the replay calls it a hang.
const UNIT_TIMEOUT: Duration = Duration::from_secs(10);

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The regular files in `dir`, sorted; none when it does not exist.
fn files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .map(|e| e.expect("read_dir entry").path())
        .filter(|p| p.is_file())
        .collect();
    out.sort();
    out
}

/// The names of the subdirectories of `dir`.
fn subdirs(dir: &Path) -> BTreeSet<String> {
    let Ok(rd) = fs::read_dir(dir) else {
        return BTreeSet::new();
    };
    rd.map(|e| e.expect("read_dir entry").path())
        .filter(|p| p.is_dir())
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Run `run(data)` on its own thread; `Err` with the panic message, or a
/// timeout after [`UNIT_TIMEOUT`].
fn replay_one(run: fn(&[u8]), data: Vec<u8>) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            let r = std::panic::catch_unwind(|| run(&data)).map_err(|p| panic_message(&*p));
            let _ = tx.send(r);
        })
        .expect("spawn replay thread");
    match rx.recv_timeout(UNIT_TIMEOUT) {
        Ok(r) => r,
        Err(_) => Err(format!("no result within {} s", UNIT_TIMEOUT.as_secs())),
    }
}

#[test]
fn replay_every_committed_input() {
    let mut failed = Vec::new();
    let mut n = 0usize;
    for t in TARGETS {
        for dir in ["corpus", "regressions"] {
            for f in files(&root().join(dir).join(t.name)) {
                let name = f.file_name().unwrap().to_string_lossy().into_owned();
                let data = fs::read(&f).expect("read input");
                n += 1;
                if let Err(msg) = replay_one(t.run, data) {
                    println!("replay: {}/{name}: {msg}", t.name);
                    failed.push(format!("{}/{name}", t.name));
                }
            }
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {n} inputs failed: {failed:?}",
        failed.len()
    );
}

/// The `name`s of `Cargo.toml`'s `[[bin]]` tables.
fn bin_names() -> BTreeSet<String> {
    let toml = fs::read_to_string(root().join("Cargo.toml")).expect("Cargo.toml");
    let mut out = BTreeSet::new();
    let mut in_bin = false;
    for line in toml.lines().map(str::trim) {
        if line.starts_with('[') {
            in_bin = line == "[[bin]]";
        } else if in_bin && let Some(v) = line.strip_prefix("name = ") {
            out.insert(v.trim_matches('"').to_owned());
        }
    }
    out
}

#[test]
fn targets_match_bins_and_dirs() {
    let names: BTreeSet<String> = TARGETS.iter().map(|t| t.name.to_owned()).collect();
    assert_eq!(names.len(), TARGETS.len(), "duplicate TARGETS name");
    let stems: BTreeSet<String> = files(&root().join("fuzz_targets"))
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(stems, names, "fuzz_targets/*.rs against TARGETS");
    assert_eq!(bin_names(), names, "[[bin]] names against TARGETS");
    assert_eq!(
        subdirs(&root().join("corpus")),
        names,
        "corpus/ against TARGETS"
    );
    let regs = subdirs(&root().join("regressions"));
    assert!(
        regs.is_subset(&names),
        "regressions/ names no target: {:?}",
        regs.difference(&names).collect::<Vec<_>>()
    );
    for t in TARGETS {
        let src = fs::read_to_string(root().join("fuzz_targets").join(format!("{}.rs", t.name)))
            .expect("fuzz target source");
        assert!(
            src.contains(&format!("vibeos_fuzz::{}(data)", t.name)),
            "fuzz_targets/{}.rs does not call vibeos_fuzz::{}",
            t.name,
            t.name
        );
    }
}
