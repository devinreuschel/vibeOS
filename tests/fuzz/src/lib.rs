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
        name: "cmdline_parse",
        run: cmdline_parse,
        covers: &[
            "boot/cmdline.rs",
            "boot/mod.rs::parse_fw_cfg_dir_count",
            "boot/mod.rs::parse_fw_cfg_dir_entry",
            "boot/mod.rs::fw_cfg_dma_access",
        ],
    },
    Target {
        name: "elf_parse",
        run: elf_parse,
        covers: &["proc/elf.rs"],
    },
    Target {
        name: "fat_mount",
        run: fat_mount,
        covers: &["fs/fat/mod.rs"],
    },
    Target {
        name: "fdt_parse",
        run: fdt_parse,
        covers: &["machine/fdt.rs"],
    },
    Target {
        name: "kbd_decode",
        run: kbd_decode,
        covers: &["console/kbd.rs::Decoder::feed", "shell/mod.rs::tokenize"],
    },
    Target {
        name: "part_parse",
        run: part_parse,
        covers: &["block/part.rs"],
    },
    Target {
        name: "pci_enumerate",
        run: pci_enumerate,
        covers: &["dev/pci.rs"],
    },
    Target {
        name: "shell_tokenize",
        run: shell_tokenize,
        covers: &["shell/mod.rs::tokenize"],
    },
    Target {
        name: "vibefs_mount",
        run: vibefs_mount,
        covers: &["fs/vibefs/mod.rs"],
    },
    Target {
        name: "virtio_caps",
        run: virtio_caps,
        covers: &["dev/virtio.rs::read_modern_caps", "dev/pci.rs"],
    },
    Target {
        name: "vmcore",
        run: vmcore,
        covers: &[
            "log/vmcore/mod.rs",
            "log/vmcore/walk.rs",
            "log/vmcore/tables.rs",
            "log/vmcore/sig.rs",
        ],
    },
    Target {
        name: "vmcoreinfo_parse",
        run: vmcoreinfo_parse,
        covers: &["log/vmcoreinfo.rs"],
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

/// `fdt::parse` and `fdt::pick_pl011` on the input as one blob.
pub fn fdt_parse(data: &[u8]) {
    targets::fdt::parse(data);
}

/// `vibefs::mount` over a [`image::Sparse`] image, then a walk and `fsck`.
pub fn vibefs_mount(data: &[u8]) {
    targets::vibefs::mount(data);
}

/// `cmdline::parse` and its accessors, then the fw_cfg directory encodings.
pub fn cmdline_parse(data: &[u8]) {
    targets::cmdline::parse(data);
}

/// `elf::parse` on the input as a whole file.
pub fn elf_parse(data: &[u8]) {
    targets::elf::parse(data);
}

/// Set-1 scancodes through `kbd::Decoder` into the shell's line editor.
pub fn kbd_decode(data: &[u8]) {
    targets::shell::kbd(data);
}

/// `pci::enumerate` over a [`cfgspace::FakeCfg`].
pub fn pci_enumerate(data: &[u8]) {
    targets::pci::enumerate(data);
}

/// `shell::tokenize` on the input as a line.
pub fn shell_tokenize(data: &[u8]) {
    targets::shell::tokenize(data);
}

/// `virtio::read_modern_caps` over a [`cfgspace::FakeCfg`].
pub fn virtio_caps(data: &[u8]) {
    targets::pci::virtio_caps(data);
}

/// The core tool's readers on the input as a kernel ELF and as a core.
pub fn vmcore(data: &[u8]) {
    targets::vmcore::parse(data);
}

/// The VMCOREINFO and build-id note readers.
pub fn vmcoreinfo_parse(data: &[u8]) {
    targets::vmcoreinfo::parse(data);
}
