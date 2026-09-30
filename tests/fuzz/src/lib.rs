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
pub const TARGETS: &[Target] = &[];
