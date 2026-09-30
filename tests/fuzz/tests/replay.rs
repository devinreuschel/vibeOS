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

use vibeos::block::part;
use vibeos::boot::cmdline;
use vibeos::pci::Bdf;
use vibeos::{acpi, elf, fat, pci, shell, vibefs, virtio};
use vibeos_fuzz::cfgspace::FakeCfg;
use vibeos_fuzz::image::{FLAG_4K, Sparse};
use vibeos_fuzz::physmem::{BASE, FlatMem};
use vibeos_fuzz::{TARGETS, seeds};

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

#[test]
fn seeds_are_committed_and_accepted() {
    // The generator's output is committed byte for byte, with no stray seed.
    let mut want = BTreeSet::new();
    for s in seeds::corpus().into_iter().chain(seeds::regressions()) {
        let dir = if s.name.starts_with("seed-") {
            "corpus"
        } else {
            "regressions"
        };
        let path = root().join(dir).join(s.target).join(&s.name);
        let have = fs::read(&path).unwrap_or_default();
        assert!(
            have == s.data,
            "{dir}/{}/{} differs from the generator: run the seeds example",
            s.target,
            s.name
        );
        want.insert(format!("{}/{}", s.target, s.name));
    }
    for t in subdirs(&root().join("corpus")) {
        for f in files(&root().join("corpus").join(&t)) {
            let name = f.file_name().unwrap().to_string_lossy().into_owned();
            if name.starts_with("seed-") {
                assert!(
                    want.contains(&format!("{t}/{name}")),
                    "corpus/{t}/{name}: no generator seed"
                );
            }
        }
    }
    for t in TARGETS {
        assert!(
            seeds::corpus().iter().any(|s| s.target == t.name),
            "{}: no seed",
            t.name
        );
    }
    accept_storage();
    accept_devices();
}

/// The data of the seed `target/seed-<name>`.
fn seed_data(target: &str, name: &str) -> Vec<u8> {
    seeds::corpus()
        .into_iter()
        .find(|s| s.target == target && s.name == format!("seed-{name}"))
        .unwrap_or_else(|| panic!("no seed {target}/seed-{name}"))
        .data
}

/// The ACPI, partition and filesystem seeds reach the parsers' results.
fn accept_storage() {
    // ACPI: each root table yields the MADT, HPET, FADT PM timer and MCFG.
    for (name, xsdt) in [("xsdt", true), ("rsdt", false)] {
        let data = seed_data("acpi_walk", name);
        let info = acpi::walk(&FlatMem::new(&data), BASE).expect("acpi walk");
        assert_eq!(info.used_xsdt, xsdt, "{name}");
        let madt = info.madt.expect("madt");
        assert_eq!(
            (madt.cpu_count, madt.ioapic_count, madt.iso_count),
            (2, 1, 2)
        );
        assert_eq!(madt.lapic_base, 0xFEE0_0000);
        assert!(info.hpet.is_some() && info.mcfg.is_some(), "{name}");
        let pm = info.fadt.and_then(|f| f.pm_timer).expect("pm timer");
        assert_eq!((pm.port, pm.width), (0x608, 32));
    }
    assert!(acpi::parse_madt(&seed_data("acpi_tables", "madt")).is_ok());
    assert!(acpi::parse_rsdp(&seed_data("acpi_tables", "rsdp")).is_ok());
    // Partitions: the EBR chain's two logicals after three primaries, and
    // both GPTs' two entries.
    let table = |name: &str| {
        let data = seed_data("part_parse", name);
        let unit = if data[0] & FLAG_4K != 0 { 4096 } else { 512 };
        let img = Sparse::parse(&data, unit, 1 << 20).unwrap();
        let mut sector = vec![0u8; 4096];
        let mut scratch = vec![0u8; 128 * 128];
        part::parse(
            u64::from(img.count()),
            unit as u32,
            img.reader(),
            &mut sector,
            &mut scratch,
        )
        .unwrap_or_else(|e| panic!("{name}: {e:?}"))
    };
    let t = table("mbr-extended");
    assert_eq!(t.origin, part::TableOrigin::Mbr);
    assert_eq!(t.n, 5);
    for name in ["gpt", "gpt-4k"] {
        let t = table(name);
        assert_eq!(
            t.origin,
            part::TableOrigin::Gpt { used_backup: false },
            "{name}"
        );
        assert_eq!(t.n, 2, "{name}");
    }
    // FAT and vibefs mount, and hold their files.
    let data = seed_data("fat_mount", "fat32");
    let mut d = Sparse::parse(&data, fat::SEC, 1 << 20).unwrap();
    let mut vol = Box::new(fat::FatVol::new());
    vol.mount_in(&mut d).expect("fat mount");
    let root = vol.root().clu;
    for name in [
        &b"BIG.BIN"[..],
        b"A long file name.txt",
        "fichier-\u{e9}t\u{e9}.txt".as_bytes(),
    ] {
        vol.lookup(&mut d, root, name).expect("fat lookup");
    }
    let data = seed_data("vibefs_mount", "vibefs");
    let mut d = Sparse::parse(&data, vibefs::BLOCK, 4096).unwrap();
    let mut v = Box::new(vibefs::Vol::new());
    vibefs::mount(&mut d, &mut v).expect("vibefs mount");
    v.walk(&mut d, b"/dir/inner.txt").expect("vibefs walk");
    let r = vibefs::fsck(&mut d).expect("fsck");
    assert_eq!(r.errors, 0);
}

/// The device, shell, ELF and command-line seeds reach the parsers' results.
fn accept_devices() {
    let mut cfg = FakeCfg::parse(&seed_data("pci_enumerate", "bus"));
    let mut out = vec![pci::FuncInfo::empty(); pci::MAX_SCAN];
    let n = pci::enumerate(&mut cfg, 0, &mut out);
    assert_eq!(
        n, 6,
        "host bridge, bridge, NIC behind it, two functions, virtio-blk"
    );
    assert!(
        out[..n]
            .iter()
            .any(|f| f.bdf == Bdf::new(1, 0, 0) && f.caps.msi.is_some())
    );
    assert!(
        out[..n]
            .iter()
            .any(|f| f.bars[0].kind == pci::BarKind::Mem64)
    );
    let mut cfg = FakeCfg::parse(&seed_data("virtio_caps", "virtio-blk"));
    let caps = virtio::read_modern_caps(&mut cfg, Bdf::new(0, 0, 0));
    assert!(caps.is_complete() && caps.device.is_some());
    assert_eq!(caps.notify.map(|c| c.notify_off_multiplier), Some(4));
    let mut slots = [""; shell::MAX_TOKENS];
    let data = seed_data("shell_tokenize", "quotes");
    let line = std::str::from_utf8(&data[1..]).unwrap();
    assert_eq!(shell::tokenize(line, &mut slots), Ok(4));
    assert_eq!(&slots[..4], ["echo", "a b", "c", "d"]);
    for s in seeds::corpus().iter().filter(|s| s.target == "elf_parse") {
        elf::parse(&s.data).unwrap_or_else(|e| panic!("{}: {e:?}", s.name));
    }
    let tiny = elf::parse(&seed_data("elf_parse", "tiny")).unwrap();
    assert!(tiny.tls.is_some() && !tiny.stack_exec);
    let data = seed_data("cmdline_parse", "options");
    let c = cmdline::parse(&data);
    for (i, o) in cmdline::OPTIONS.iter().enumerate() {
        assert_eq!(
            c.get(o.name),
            Some(format!("{}", i + 1).as_bytes()),
            "{}",
            o.name
        );
    }
    assert_eq!(c.init_env().count(), 2);
}

/// The quoted rows of `scripts/check_core_stable.py`'s `PARSERS` table, as
/// `path`, or `path::target` for a `fn` row.
fn listed_parsers() -> Vec<String> {
    let py = fs::read_to_string(root().join("../../scripts/check_core_stable.py"))
        .expect("scripts/check_core_stable.py");
    let mut rows = Vec::new();
    let mut inside = false;
    for line in py.lines() {
        if line.starts_with("PARSERS:") {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if line.starts_with(')') {
            break;
        }
        let q: Vec<&str> = line.split('"').skip(1).step_by(2).collect();
        let [path, kind, target] = q[..] else {
            panic!("check_core_stable.py: PARSERS row this scan cannot read: {line}");
        };
        rows.push(if kind == "fn" {
            format!("{path}::{target}")
        } else {
            path.to_owned()
        });
    }
    assert!(!rows.is_empty(), "check_core_stable.py: no PARSERS table");
    rows
}

#[test]
fn every_listed_parser_has_a_target() {
    let covered: BTreeSet<&str> = TARGETS
        .iter()
        .flat_map(|t| t.covers.iter().copied())
        .collect();
    let listed = listed_parsers();
    let missing: Vec<&String> = listed
        .iter()
        .filter(|p| !covered.contains(p.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "PARSERS rows no Target.covers names (C-FUZZ: add a target): {missing:?}"
    );
    for c in &covered {
        assert!(
            listed.iter().any(|p| p == c),
            "Target.covers names {c}, which is not a PARSERS row"
        );
    }
}
