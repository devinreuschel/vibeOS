//! Fuzz targets for vibeos-core's byte parsers (C-FUZZ, TESTING.md §8.1).
//!
//! Each target is a `pub fn <name>(data: &[u8])` that feeds `data` to one
//! parser family through a harness model ([`image::Sparse`],
//! [`cfgspace::FakeCfg`], [`physmem::FlatMem`]) or as raw bytes. A target
//! never panics on its own account: its bounds use `min` and `checked_*`,
//! and it ignores the parsers' `Err`s, so a panic is the parser's.
//! `fuzz_targets/<name>.rs` hands libFuzzer's input to it, and
//! `tests/replay.rs` replays every committed input through [`TARGETS`].

#![forbid(unsafe_code)]
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "host fuzz harness: `alloc`'s growing calls may panic, and a failed allocation ends this host process, not the kernel (DESIGN §4.4)"
)]

pub mod cfgspace;
pub mod image;
pub mod physmem;
pub mod seeds;
mod targets;

/// One fuzz target: `name` is its `[[bin]]`, its `fuzz_targets/<name>.rs`
/// stem and its `corpus/<name>/` and `regressions/<name>/` directories;
/// `covers` lists the rows of `scripts/check_core_stable.py`'s `PARSERS`
/// table it exercises, as `path` or, for a `fn` row, `path::target`.
pub struct Target {
    pub name: &'static str,
    pub run: fn(&[u8]),
    pub covers: &'static [&'static str],
}

/// Every target, in `fuzz_targets/` order.
pub const TARGETS: &[Target] = &[
    Target {
        name: "acpi_tables",
        run: acpi_tables,
        covers: &["acpi/mod.rs"],
    },
    Target {
        name: "acpi_walk",
        run: acpi_walk,
        covers: &["acpi/mod.rs"],
    },
    Target {
        name: "fat_mount",
        run: fat_mount,
        covers: &["fs/fat/mod.rs"],
    },
    Target {
        name: "part_parse",
        run: part_parse,
        covers: &["block/part.rs"],
    },
    Target {
        name: "vibefs_mount",
        run: vibefs_mount,
        covers: &["fs/vibefs/mod.rs"],
    },
];

/// `acpi::walk` over the input as memory, the RSDP at [`physmem::BASE`].
pub fn acpi_walk(data: &[u8]) {
    targets::acpi::walk(data);
}

/// Every ACPI table parser on the input as one table.
pub fn acpi_tables(data: &[u8]) {
    targets::acpi::tables(data);
}

/// `part::parse` over a [`image::Sparse`] image.
pub fn part_parse(data: &[u8]) {
    targets::part::parse(data);
}

/// `FatVol::mount` over a [`image::Sparse`] image, then a walk and a write.
pub fn fat_mount(data: &[u8]) {
    targets::fat::mount(data);
}

/// `vibefs::mount` over a [`image::Sparse`] image, then a walk and `fsck`.
pub fn vibefs_mount(data: &[u8]) {
    targets::vibefs::mount(data);
}
