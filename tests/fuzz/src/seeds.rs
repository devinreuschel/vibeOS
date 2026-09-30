//! The deterministic seed generator (C-FUZZ). `cargo run --example seeds`
//! writes [`corpus`] to `corpus/<target>/seed-<name>` and [`regressions`]
//! to `regressions/<target>/<name>`; `tests/replay.rs` checks that the
//! committed files match. Each seed is built with the parsers' own
//! builders, so it passes their checks and the fuzzer starts deep.

use vibeos::acpi;
use vibeos::block::part::{self, GptHeaderInfo};
use vibeos::boot::cmdline;
use vibeos::elf;
use vibeos::fat::{self, FatInode, FatVol};
use vibeos::fs::InodeKind;
use vibeos::log::vmcore::{
    self,
    synth::{SymDef, Synth, kernel_elf},
};
use vibeos::log::vmcoreinfo;
use vibeos::pci;
use vibeos::shell::MAX_TOKENS;
use vibeos::vibefs::{self, Vol};
use vibeos::virtio;

use crate::cfgspace::FakeCfg;
use crate::image::{FLAG_4K, FLAG_FIX_CRC, Sparse};
use crate::physmem::BASE;

/// One generated input: its target, and its file name under that target's
/// directory (a corpus name carries the `seed-` prefix).
pub struct Seed {
    pub target: &'static str,
    pub name: String,
    pub data: Vec<u8>,
}

fn seed(target: &'static str, name: &str, data: Vec<u8>) -> Seed {
    Seed {
        target,
        name: format!("seed-{name}"),
        data,
    }
}

/// Every corpus seed, in target order.
pub fn corpus() -> Vec<Seed> {
    let mut out = Vec::new();
    out.push(seed("acpi_walk", "xsdt", acpi_image(true)));
    out.push(seed("acpi_walk", "rsdt", acpi_image(false)));
    for (name, t) in [
        ("rsdp", rsdp(true, 0x40, 0x80)),
        ("madt", madt()),
        ("hpet", hpet()),
        ("fadt", fadt()),
        ("mcfg", mcfg()),
    ] {
        out.push(seed("acpi_tables", name, t));
    }
    out.push(seed("part_parse", "mbr-extended", mbr_extended()));
    out.push(seed("part_parse", "gpt", gpt(512, FLAG_FIX_CRC)));
    out.push(seed("part_parse", "gpt-4k", gpt(4096, FLAG_4K)));
    out.push(seed(
        "fat_mount",
        "fat32",
        Sparse::encode_image(0, &fat_image(), fat::SEC),
    ));
    out.push(seed(
        "vibefs_mount",
        "vibefs",
        Sparse::encode_image(0, &vibefs_image(), vibefs::BLOCK),
    ));
    out.push(seed("pci_enumerate", "bus", pci_bus()));
    out.push(seed(
        "virtio_caps",
        "virtio-blk",
        FakeCfg::encode(&[(0, 0, &virtio_blk_cfg())]),
    ));
    out.push(seed(
        "shell_tokenize",
        "quotes",
        line(MAX_TOKENS, "echo 'a b' \"c\" d"),
    ));
    out.push(seed(
        "shell_tokenize",
        "unclosed",
        line(MAX_TOKENS, "echo \"abc"),
    ));
    let words: Vec<String> = (0..=MAX_TOKENS).map(|i| format!("w{i}")).collect();
    out.push(seed(
        "shell_tokenize",
        "too-many",
        line(MAX_TOKENS, &words.join(" ")),
    ));
    out.push(seed("kbd_decode", "session", kbd_session()));
    out.push(seed("elf_parse", "tiny", tiny_elf()));
    for (name, data) in elf_fixtures() {
        out.push(seed("elf_parse", &name, data));
    }
    out.push(seed("cmdline_parse", "options", cmdline_seed()));
    out.push(seed("vmcore", "core", vmcore_core()));
    out.push(seed("vmcore", "elf", vmcore_elf()));
    out.push(seed("vmcoreinfo_parse", "note", vmcoreinfo_note()));
    out.push(seed("vmcoreinfo_parse", "build-id", build_id_note()));
    out.push(seed("vmcoreinfo_parse", "fw-cfg", fw_cfg_vmcoreinfo()));
    out
}

/// The committed regressions the generator owns: box 1234's F064 BPBs,
/// which panicked the FAT mount before `parse_bpb` checked `data_lba`.
pub fn regressions() -> Vec<Seed> {
    let img = fat_image();
    let count = u32::try_from(img.len() / fat::SEC).unwrap_or(u32::MAX);
    let bpb = |fatsz: u32, nfats: u8| {
        let mut boot = img[..fat::SEC].to_vec();
        boot[36..40].copy_from_slice(&fatsz.to_le_bytes());
        boot[16] = nfats;
        Sparse::encode(0, count, &[(0, &boot)], fat::SEC)
    };
    vec![
        Seed {
            target: "fat_mount",
            name: "f064-fatsz32-80000000-two-fats".to_owned(),
            data: bpb(0x8000_0000, 2),
        },
        Seed {
            target: "fat_mount",
            name: "f064-fatsz32-ffffffff-one-fat".to_owned(),
            data: bpb(0xFFFF_FFFF, 1),
        },
    ]
}

// ------------------ ACPI ------------------

/// The table offsets inside an ACPI image, from [`BASE`].
const XSDT_OFF: usize = 0x40;
const RSDT_OFF: usize = 0x80;
const MADT_OFF: usize = 0x100;
const HPET_OFF: usize = 0x200;
const FADT_OFF: usize = 0x300;
const MCFG_OFF: usize = 0x500;
const ACPI_LEN: usize = 0x600;

/// Set byte 9, the SDT checksum, so the table sums to zero.
fn sdt_checksum(t: &mut [u8]) {
    t[9] = 0;
    t[9] = 0u8.wrapping_sub(acpi::checksum(t));
}

/// A table of `len` bytes with its SDT header filled in, checksum unset.
fn sdt(sig: &[u8; 4], len: usize, revision: u8) -> Vec<u8> {
    let mut t = vec![0u8; len];
    t[0..4].copy_from_slice(sig);
    t[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    t[8] = revision;
    t[10..16].copy_from_slice(b"VIBEOS");
    t[16..24].copy_from_slice(b"FUZZSEED");
    t[24..28].copy_from_slice(&1u32.to_le_bytes());
    t[28..32].copy_from_slice(b"VBOS");
    t[32..36].copy_from_slice(&1u32.to_le_bytes());
    t
}

/// A GAS: space, bit width, bit offset, access size, address.
fn gas(t: &mut [u8], off: usize, space: u8, width: u8, addr: u64) {
    t[off] = space;
    t[off + 1] = width;
    t[off + 2] = 0;
    t[off + 3] = 0;
    t[off + 4..off + 12].copy_from_slice(&addr.to_le_bytes());
}

/// An RSDP: v2 naming the XSDT at `xsdt`, or v1 naming the RSDT at `rsdt`,
/// both offsets from [`BASE`].
fn rsdp(v2: bool, xsdt: usize, rsdt: usize) -> Vec<u8> {
    let mut r = vec![0u8; acpi::RSDP_V2_LEN];
    r[0..8].copy_from_slice(acpi::RSDP_SIG);
    r[9..15].copy_from_slice(b"VIBEOS");
    if v2 {
        r[15] = 2;
        r[20..24].copy_from_slice(&(acpi::RSDP_V2_LEN as u32).to_le_bytes());
        r[24..32].copy_from_slice(&(BASE + xsdt as u64).to_le_bytes());
    } else {
        r[16..20].copy_from_slice(&((BASE as u32) + rsdt as u32).to_le_bytes());
    }
    r[8] = 0u8.wrapping_sub(acpi::checksum(&r[..acpi::RSDP_V1_LEN]));
    if v2 {
        r[32] = 0u8.wrapping_sub(acpi::checksum(&r));
    }
    r
}

/// A MADT: two enabled LAPICs, an I/O APIC, two ISOs and a LAPIC address
/// override.
fn madt() -> Vec<u8> {
    let mut recs: Vec<u8> = Vec::new();
    for id in 0u8..2 {
        recs.extend_from_slice(&[acpi::MADT_TYPE_LAPIC, 8, id, id]);
        recs.extend_from_slice(&acpi::LAPIC_ENABLED.to_le_bytes());
    }
    recs.extend_from_slice(&[acpi::MADT_TYPE_IOAPIC, 12, 2, 0]);
    recs.extend_from_slice(&0xFEC0_0000u32.to_le_bytes());
    recs.extend_from_slice(&0u32.to_le_bytes());
    for (irq, gsi, flags) in [(0u8, 2u32, 0u16), (9, 9, 0x000D)] {
        recs.extend_from_slice(&[acpi::MADT_TYPE_ISO, 10, 0, irq]);
        recs.extend_from_slice(&gsi.to_le_bytes());
        recs.extend_from_slice(&flags.to_le_bytes());
    }
    recs.extend_from_slice(&[acpi::MADT_TYPE_LAPIC_ADDR_OVERRIDE, 12, 0, 0]);
    recs.extend_from_slice(&0xFEE0_0000u64.to_le_bytes());
    let mut t = sdt(acpi::SIG_MADT, 44 + recs.len(), 4);
    t[36..40].copy_from_slice(&0xFEE0_0000u32.to_le_bytes());
    t[40..44].copy_from_slice(&1u32.to_le_bytes());
    t[44..].copy_from_slice(&recs);
    sdt_checksum(&mut t);
    t
}

/// An HPET at 0xFED0_0000 in system memory.
fn hpet() -> Vec<u8> {
    let mut t = sdt(acpi::SIG_HPET, 56, 1);
    t[36..40].copy_from_slice(&0x8086_A201u32.to_le_bytes());
    gas(&mut t, 40, acpi::GAS_SYSTEM_MEMORY, 64, 0xFED0_0000);
    t[53..55].copy_from_slice(&0x0080u16.to_le_bytes());
    sdt_checksum(&mut t);
    t
}

/// An ACPI 6 FADT: a 32-bit PM timer at port 0x608 in both `PM_TMR_BLK`
/// and `X_PM_TMR_BLK` (P10-S49's fields), the 8042 flag, the reset
/// register and the sleep registers.
fn fadt() -> Vec<u8> {
    let mut t = sdt(acpi::SIG_FADT, 276, 6);
    t[76..80].copy_from_slice(&0x608u32.to_le_bytes());
    t[91] = 4;
    t[109..111].copy_from_slice(&0x0003u16.to_le_bytes());
    t[112..116].copy_from_slice(&(1u32 << 8).to_le_bytes());
    gas(&mut t, 116, acpi::GAS_SYSTEM_IO, 8, 0xCF9);
    t[128] = 0x06;
    gas(&mut t, 208, acpi::GAS_SYSTEM_IO, 32, 0x608);
    gas(&mut t, 244, acpi::GAS_SYSTEM_IO, 8, 0x604);
    gas(&mut t, 256, acpi::GAS_SYSTEM_IO, 8, 0x600);
    sdt_checksum(&mut t);
    t
}

/// An MCFG: one ECAM window, segment 0, buses 0 to 255.
fn mcfg() -> Vec<u8> {
    let mut t = sdt(acpi::SIG_MCFG, 60, 1);
    t[44..52].copy_from_slice(&0xB000_0000u64.to_le_bytes());
    t[55] = 0xFF;
    sdt_checksum(&mut t);
    t
}

/// A root table naming the four tables, with `ptr`-byte pointers.
fn root_table(sig: &[u8; 4], ptr: usize) -> Vec<u8> {
    let offs = [MADT_OFF, HPET_OFF, FADT_OFF, MCFG_OFF];
    let mut t = sdt(sig, acpi::SDT_HEADER_LEN + ptr * offs.len(), 1);
    for (i, off) in offs.iter().enumerate() {
        let addr = (BASE + *off as u64).to_le_bytes();
        let at = acpi::SDT_HEADER_LEN + i * ptr;
        t[at..at + ptr].copy_from_slice(&addr[..ptr]);
    }
    sdt_checksum(&mut t);
    t
}

/// The memory at [`BASE`]: an RSDP (v2 naming the XSDT, or v1 naming the
/// RSDT), then the root table and its four tables.
pub fn acpi_image(v2: bool) -> Vec<u8> {
    let mut m = vec![0u8; ACPI_LEN];
    let mut put = |off: usize, t: &[u8]| m[off..off + t.len()].copy_from_slice(t);
    put(0, &rsdp(v2, XSDT_OFF, RSDT_OFF));
    if v2 {
        put(XSDT_OFF, &root_table(acpi::SIG_XSDT, 8));
    } else {
        put(RSDT_OFF, &root_table(acpi::SIG_RSDT, 4));
    }
    put(MADT_OFF, &madt());
    put(HPET_OFF, &hpet());
    put(FADT_OFF, &fadt());
    put(MCFG_OFF, &mcfg());
    m
}

// ------------------ partitions ------------------

/// Sectors in each partition seed's disk.
const DISK_SECTORS: u64 = 2048;

/// An MBR with three primaries and an extended partition whose EBR chain
/// holds two logicals, two primaries after it (P10-S66's case).
fn mbr_extended() -> Vec<u8> {
    let mut mbr = [0u8; 512];
    part::pack_mbr(
        &mut mbr,
        &[
            (part::MBR_LINUX, 1, 100),
            (part::MBR_EXTENDED, 200, 300),
            (part::MBR_FAT32_LBA, 600, 100),
            (part::MBR_SWAP, 800, 100),
        ],
    );
    // EBRs at 200 and 300: each logical starts one sector past its EBR,
    // and the link is relative to the extended partition's start.
    let mut e1 = [0u8; 512];
    part::pack_ebr(&mut e1, part::MBR_LINUX, 1, 50, 100, 60);
    let mut e2 = [0u8; 512];
    part::pack_ebr(&mut e2, part::MBR_NTFS, 1, 40, 0, 0);
    let count = DISK_SECTORS as u32;
    Sparse::encode(0, count, &[(0, &mbr), (200, &e1), (300, &e2)], 512)
}

/// A protective MBR, then a primary and a backup GPT naming two
/// partitions, on `ss`-byte sectors.
fn gpt(ss: usize, flags: u8) -> Vec<u8> {
    let nsect = DISK_SECTORS;
    let esz = part::GPT_ENTRY_SIZE as usize;
    let nent = 128usize;
    let mut entries = vec![0u8; nent * esz];
    let parts = [
        (part::GUID_EFI, 64u64, 127u64, "EFI system"),
        (part::GUID_LINUX, 128, 1023, "vibeos root"),
    ];
    for (i, (guid, first, last, name)) in parts.iter().enumerate() {
        let mut uniq = [0u8; 16];
        uniq[0] = i as u8 + 1;
        part::pack_gpt_entry(&mut entries[i * esz..], guid, &uniq, *first, *last, name);
    }
    let ecrc = part::entries_crc(&entries);
    let esec = (entries.len() / ss) as u64;
    let hdr = |my: u64, alt: u64, part_lba: u64| {
        let mut h = vec![0u8; ss];
        part::pack_gpt_header(
            &mut h,
            &GptHeaderInfo {
                my_lba: my,
                alt_lba: alt,
                first_usable: 2 + esec,
                last_usable: nsect - 2 - esec,
                disk_guid: [0x11; 16],
                part_lba,
                part_count: nent as u32,
                part_size: esz as u32,
                entries_crc: ecrc,
            },
        );
        h
    };
    let mut pmbr = vec![0u8; ss];
    part::pack_protective_mbr(&mut pmbr, nsect);
    let primary = hdr(1, nsect - 1, 2);
    let backup = hdr(nsect - 1, 1, nsect - 1 - esec);
    let mut units: Vec<(u32, &[u8])> = vec![(0, &pmbr), (1, &primary)];
    for (i, s) in entries.chunks(ss).enumerate() {
        if s.iter().any(|&b| b != 0) {
            units.push((2 + i as u32, s));
            units.push(((nsect - 1 - esec) as u32 + i as u32, s));
        }
    }
    units.push(((nsect - 1) as u32, &backup));
    Sparse::encode(flags, nsect as u32, &units, ss)
}

// ------------------ FAT ------------------

/// Sectors in the FAT seed.
const FAT_SECTORS: usize = 2048;

/// `fat::mkfs`, then a multi-cluster file, a directory three deep holding
/// a file, and files with ASCII and UTF-8 long names (P10-S95).
pub fn fat_image() -> Vec<u8> {
    let mut img = vec![0u8; FAT_SECTORS * fat::SEC];
    fat::mkfs(&mut img, b"FUZZSEED").expect("mkfs");
    {
        let mut d = fat::MemDisk::new(&mut img, fat::SEC as u32).expect("MemDisk");
        let mut vol = Box::new(FatVol::new());
        vol.mount_in(&mut d).expect("mount");
        let root = vol.root().clu;
        let file = |vol: &mut FatVol, d: &mut fat::MemDisk, dir: u32, name: &[u8], body: &[u8]| {
            let n = vol.create(d, dir, name, false).expect("create");
            let mut ino = FatInode::of_node(&n);
            vol.write_ino(d, &mut ino, true, 0, false, body)
                .expect("write");
        };
        let big: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        file(&mut vol, &mut d, root, b"BIG.BIN", &big);
        file(
            &mut vol,
            &mut d,
            root,
            b"A long file name.txt",
            b"long name\n",
        );
        file(
            &mut vol,
            &mut d,
            root,
            "fichier-\u{e9}t\u{e9}.txt".as_bytes(),
            b"utf-8\n",
        );
        let mut dir = root;
        for name in [&b"one"[..], b"two", b"three"] {
            dir = vol.create(&mut d, dir, name, true).expect("mkdir").clu;
        }
        file(&mut vol, &mut d, dir, b"DEEP.TXT", b"three deep\n");
        vol.sync(&mut d).expect("sync");
    }
    img
}

// ------------------ vibefs ------------------

/// Blocks in the vibefs seed.
const VIBEFS_BLOCKS: usize = 64;

/// `vibefs::mkfs`, then an inline file, an extent file and a directory.
pub fn vibefs_image() -> Vec<u8> {
    let mut img = vec![0u8; VIBEFS_BLOCKS * vibefs::BLOCK];
    {
        let mut d = vibefs::MemDisk::new(&mut img).expect("MemDisk");
        let mut v = Box::new(Vol::new());
        vibefs::mkfs(&mut d, b"fuzzseed", &mut v).expect("mkfs");
        let root = v.root_ino;
        let small = v
            .create(&mut d, root, b"inline.txt", InodeKind::Reg, 0o644, None)
            .expect("create");
        v.write(&mut d, small.ino, 0, b"inline data\n")
            .expect("write");
        let big: Vec<u8> = (0..10_000u32).map(|i| (i % 253) as u8).collect();
        let ext = v
            .create(&mut d, root, b"extent.bin", InodeKind::Reg, 0o644, None)
            .expect("create");
        v.write(&mut d, ext.ino, 0, &big).expect("write");
        let dir = v
            .create(&mut d, root, b"dir", InodeKind::Dir, 0o755, None)
            .expect("mkdir");
        let inner = v
            .create(&mut d, dir.ino, b"inner.txt", InodeKind::Reg, 0o644, None)
            .expect("create");
        v.write(&mut d, inner.ino, 0, b"inner\n").expect("write");
        v.sync(&mut d).expect("sync");
    }
    img
}

// ------------------ PCI and virtio ------------------

/// A type-0 or type-1 header: vendor, device, class, subclass, header type.
fn cfg_header(vendor: u16, device: u16, class: u8, sub: u8, header: u8) -> [u8; 256] {
    let mut c = [0u8; 256];
    c[0..2].copy_from_slice(&vendor.to_le_bytes());
    c[2..4].copy_from_slice(&device.to_le_bytes());
    c[0x0A] = sub;
    c[0x0B] = class;
    c[0x0E] = header;
    c
}

fn put32(c: &mut [u8; 256], off: usize, v: u32) {
    c[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// Chain the capability at `at` (id, next) into `c`'s list after `prev`:
/// sets STATUS.CAPS, and CAP_PTR when `prev` is 0.
fn cap(c: &mut [u8; 256], prev: usize, at: usize, id: u8) {
    c[0x06] |= 0x10;
    if prev == 0 {
        c[0x34] = at as u8;
    } else {
        c[prev + 1] = at as u8;
    }
    c[at] = id;
    c[at + 1] = 0;
}

/// A modern virtio-blk function (1AF4:1042): a 64-bit BAR 4, its four
/// vendor capabilities (common, notify, ISR, device) and an MSI-X cap.
pub fn virtio_blk_cfg() -> [u8; 256] {
    let mut c = cfg_header(virtio::VENDOR_ID, virtio::DEV_BLK_MODERN, 0x01, 0x00, 0);
    c[0x2C..0x2E].copy_from_slice(&virtio::VENDOR_ID.to_le_bytes());
    c[0x2E..0x30].copy_from_slice(&2u16.to_le_bytes());
    put32(&mut c, 0x14, 0xFEBD_0000);
    put32(&mut c, 0x20, 0xFE00_000C);
    put32(&mut c, 0x24, 0);
    c[0x3D] = 1;
    let caps: [(u8, u32, u32, u32); 4] = [
        (virtio::PCI_CAP_COMMON, 0x0000, 0x1000, 0),
        (virtio::PCI_CAP_ISR, 0x1000, 0x1000, 0),
        (virtio::PCI_CAP_DEVICE, 0x2000, 0x1000, 0),
        (virtio::PCI_CAP_NOTIFY, 0x3000, 0x1000, 4),
    ];
    let mut prev = 0usize;
    let mut at = 0x40usize;
    for (ty, off, len, mult) in caps {
        cap(&mut c, prev, at, pci::CAP_VENDOR);
        let clen = if ty == virtio::PCI_CAP_NOTIFY { 20 } else { 16 };
        c[at + 2] = clen;
        c[at + 3] = ty;
        c[at + 4] = 4;
        put32(&mut c, at + 8, off);
        put32(&mut c, at + 12, len);
        if ty == virtio::PCI_CAP_NOTIFY {
            put32(&mut c, at + 16, mult);
        }
        prev = at;
        at += 0x14;
    }
    cap(&mut c, prev, at, pci::CAP_MSIX);
    c[at + 2..at + 4].copy_from_slice(&1u16.to_le_bytes());
    put32(&mut c, at + 4, 0x0000_0001);
    put32(&mut c, at + 8, 0x0000_0801);
    c
}

/// Bus 0: a host bridge, a PCI-to-PCI bridge to bus 1 with a NIC behind it
/// (an MSI cap), a function with a 64-bit BAR, and the virtio-blk function.
fn pci_bus() -> Vec<u8> {
    let host = cfg_header(0x8086, 0x29C0, 0x06, 0x00, 0);
    let mut bridge = cfg_header(0x1B36, 0x000C, 0x06, 0x04, pci::HEADER_BRIDGE);
    bridge[0x18] = 0;
    bridge[0x19] = 1;
    bridge[0x1A] = 1;
    let mut nic = cfg_header(0x8086, 0x100E, 0x02, 0x00, 0);
    put32(&mut nic, 0x10, 0xFEB8_0000);
    put32(&mut nic, 0x14, 0xC001);
    cap(&mut nic, 0, 0x50, pci::CAP_MSI);
    nic[0x52..0x54].copy_from_slice(&pci::MSI_CTL_64BIT.to_le_bytes());
    let mut wide = cfg_header(0x1234, 0x1111, 0x03, 0x00, pci::HEADER_MULTI);
    put32(&mut wide, 0x10, 0xFD00_000C);
    put32(&mut wide, 0x14, 0x0000_0010);
    let wide_fn1 = cfg_header(0x1234, 0x1112, 0x03, 0x80, 0);
    let blk = virtio_blk_cfg();
    FakeCfg::encode(&[
        (0, 0x00, &host),
        (0, 0x08, &bridge),
        (1, 0x00, &nic),
        (0, 0x10, &wide),
        (0, 0x11, &wide_fn1),
        (0, 0x18, &blk),
    ])
}

// ------------------ shell and keyboard ------------------

/// A `shell_tokenize` input: the slot count, then the line.
fn line(slots: usize, s: &str) -> Vec<u8> {
    let mut v = vec![slots as u8];
    v.extend_from_slice(s.as_bytes());
    v
}

/// The set-1 make and break of each key in `keys`.
fn taps(keys: &[u8]) -> Vec<u8> {
    keys.iter().flat_map(|&k| [k, k | 0x80]).collect()
}

/// `ls -l` and Enter; shift; E0 Up to recall it and Enter; E1 Pause;
/// Ctrl-C; Caps Lock around a letter.
fn kbd_session() -> Vec<u8> {
    const L: u8 = 0x26;
    const S: u8 = 0x1F;
    const SPACE: u8 = 0x39;
    const MINUS: u8 = 0x0C;
    const ENTER: u8 = 0x1C;
    const LSHIFT: u8 = 0x2A;
    const CTRL: u8 = 0x1D;
    const C: u8 = 0x2E;
    const A: u8 = 0x1E;
    const CAPS: u8 = 0x3A;
    let mut v = taps(&[L, S, SPACE, MINUS, L, ENTER]);
    v.push(LSHIFT);
    v.extend(taps(&[L, S]));
    v.push(LSHIFT | 0x80);
    v.extend(taps(&[ENTER]));
    v.extend_from_slice(&[0xE0, 0x48, 0xE0, 0xC8]);
    v.extend_from_slice(&[0xE0, 0x4B, 0xE0, 0xCB]);
    v.extend(taps(&[ENTER]));
    v.extend_from_slice(&[0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5]);
    v.push(CTRL);
    v.extend(taps(&[C]));
    v.push(CTRL | 0x80);
    v.extend(taps(&[CAPS, A, CAPS, A, ENTER]));
    v
}

// ------------------ ELF ------------------

/// A static ELF: a `PT_LOAD` of the whole file at 0x40_0000, a `PT_TLS`
/// inside it, and a non-executable `PT_GNU_STACK`.
fn tiny_elf() -> Vec<u8> {
    let phnum = 3u16;
    let code_off = elf::EHDR_SIZE + elf::PHDR_SIZE * usize::from(phnum);
    let tls_off = code_off + 16;
    let len = tls_off + 16;
    let base = 0x40_0000u64;
    let mut f = vec![0u8; len];
    f[0..4].copy_from_slice(&[elf::ELFMAG0, b'E', b'L', b'F']);
    f[4] = elf::ELFCLASS64;
    f[5] = elf::ELFDATA2LSB;
    f[6] = elf::EV_CURRENT;
    f[16..18].copy_from_slice(&elf::ET_EXEC.to_le_bytes());
    f[18..20].copy_from_slice(&elf::EM_X86_64.to_le_bytes());
    f[20..24].copy_from_slice(&1u32.to_le_bytes());
    f[24..32].copy_from_slice(&(base + code_off as u64).to_le_bytes());
    f[32..40].copy_from_slice(&(elf::EHDR_SIZE as u64).to_le_bytes());
    f[52..54].copy_from_slice(&(elf::EHDR_SIZE as u16).to_le_bytes());
    f[54..56].copy_from_slice(&(elf::PHDR_SIZE as u16).to_le_bytes());
    f[56..58].copy_from_slice(&phnum.to_le_bytes());
    // (type, flags, offset, vaddr, filesz, memsz, align)
    let phdrs: [(u32, u32, u64, u64, u64, u64, u64); 3] = [
        (
            elf::PT_LOAD,
            elf::PF_R | elf::PF_X,
            0,
            base,
            len as u64,
            len as u64,
            0x1000,
        ),
        (
            elf::PT_TLS,
            elf::PF_R,
            tls_off as u64,
            base + tls_off as u64,
            16,
            32,
            8,
        ),
        (elf::PT_GNU_STACK, elf::PF_R | elf::PF_W, 0, 0, 0, 0, 16),
    ];
    for (i, (ty, fl, off, va, fsz, msz, al)) in phdrs.into_iter().enumerate() {
        let p = elf::EHDR_SIZE + i * elf::PHDR_SIZE;
        f[p..p + 4].copy_from_slice(&ty.to_le_bytes());
        f[p + 4..p + 8].copy_from_slice(&fl.to_le_bytes());
        f[p + 8..p + 16].copy_from_slice(&off.to_le_bytes());
        f[p + 16..p + 24].copy_from_slice(&va.to_le_bytes());
        f[p + 24..p + 32].copy_from_slice(&va.to_le_bytes());
        f[p + 32..p + 40].copy_from_slice(&fsz.to_le_bytes());
        f[p + 40..p + 48].copy_from_slice(&msz.to_le_bytes());
        f[p + 48..p + 56].copy_from_slice(&al.to_le_bytes());
    }
    // `jmp .` at the entry, then the TLS init image.
    f[code_off] = 0xEB;
    f[code_off + 1] = 0xFE;
    f[tls_off..tls_off + 8].copy_from_slice(b"tlsinit!");
    f
}

/// P10-S72's fixture binaries, `tests/fixtures/elf/*` files that start with
/// the ELF magic, by name.
fn elf_fixtures() -> Vec<(String, Vec<u8>)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/elf");
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .expect("tests/fixtures/elf")
        .map(|e| e.expect("read_dir entry").path())
        .filter_map(|p| {
            let data = std::fs::read(&p).ok()?;
            let name = p.file_name()?.to_string_lossy().into_owned();
            data.starts_with(b"\x7fELF").then_some((name, data))
        })
        .collect();
    out.sort();
    out
}

// ------------------ command line and notes ------------------

/// Every option `cmdline::OPTIONS` lists (BOOT.md §3.2) with a value, a
/// `sysctl.` word, a dotted unknown word, undotted `name=value` words, a
/// quoted value, then `--` and init's arguments.
pub fn cmdline_seed() -> Vec<u8> {
    let mut s = String::new();
    for (i, o) in cmdline::OPTIONS.iter().enumerate() {
        s.push_str(&format!("{}={} ", o.name, i + 1));
    }
    s.push_str("sysctl.kernel.printk=4 unknown.dotted=1 vendor.flag HOME=/ TERM=\"vt 100\" ");
    s.push_str("single -- --init-arg vibeos.strace=0");
    s.into_bytes()
}

/// The kernel VA of the core seed's one mapped page run.
const VMCORE_KBASE: u64 = 0xFFFF_FFFF_8000_0000;

/// A 64 KiB physical core: a four-level table mapping 16 KiB at
/// [`VMCORE_KBASE`], a two-frame `rbp` chain there, a VMCOREINFO note whose
/// roots lie in that run, and two `NT_PRSTATUS` notes.
fn vmcore_core() -> Vec<u8> {
    let mut s = Synth::new(&[(0, 0x1_0000)]);
    for i in 0..4u64 {
        s.map(VMCORE_KBASE + i * 0x1000, 0x1000 + i * 0x1000, 0x1000, 0);
    }
    let fp = VMCORE_KBASE + 0x100;
    s.write_phys(0x1100, &(fp + 0x40).to_le_bytes());
    s.write_phys(0x1108, &(VMCORE_KBASE + 0x20).to_le_bytes());
    s.write_phys(0x1148, &(VMCORE_KBASE + 0x30).to_le_bytes());
    let info = vmcoreinfo::Info {
        osrelease: "0.8.0",
        build_id: &[0xAB; 20],
        page_size: 4096,
        pgt_root: s.root,
        pgt_levels: 4,
        log: VMCORE_KBASE + 0x10,
        tcbs: fp,
        tcbs_len: 4,
        cpus: VMCORE_KBASE + 0x2000,
        cpus_len: 2,
    };
    s.vmcoreinfo(&info);
    s.cpu(1, VMCORE_KBASE + 0x10, fp + 0x800, fp);
    s.cpu(2, VMCORE_KBASE + 0x18, fp + 0x900, 0);
    s.build()
}

/// A kernel ELF with a build id, `.text` at [`VMCORE_KBASE`] and two
/// symbols.
fn vmcore_elf() -> Vec<u8> {
    let syms = [
        SymDef {
            name: "_ZN6vibeos3smp9hang_test4hold17h0123456789abcdefE",
            value: VMCORE_KBASE + 0x10,
            size: 0x10,
            kind: vmcore::STT_FUNC,
        },
        SymDef {
            name: "VIBEOS_TRACE",
            value: VMCORE_KBASE + 0x2000,
            size: 0x40,
            kind: vmcore::STT_OBJECT,
        },
    ];
    kernel_elf(&[0xAB; 20], VMCORE_KBASE, &[0xC3; 0x40], &syms)
}

/// A VMCOREINFO note as `render` writes it.
fn vmcoreinfo_note() -> Vec<u8> {
    let info = vmcoreinfo::Info {
        osrelease: "0.8.0",
        build_id: &[0xAB; 20],
        page_size: 4096,
        pgt_root: 0x10_0000,
        pgt_levels: 4,
        log: 0xFFFF_FFFF_8010_0000,
        tcbs: 0xFFFF_FFFF_8020_0000,
        tcbs_len: 64,
        cpus: 0xFFFF_FFFF_8030_0000,
        cpus_len: 64,
    };
    let mut out = vec![0u8; vmcoreinfo::NOTE_MAX];
    let n = vmcoreinfo::render(&info, &mut out).expect("render");
    out.truncate(n);
    out
}

/// A GNU build-id note: `namesz` 4, `descsz` 20, type 3, `GNU\0`, the id.
fn build_id_note() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&4u32.to_le_bytes());
    v.extend_from_slice(&20u32.to_le_bytes());
    v.extend_from_slice(&vmcoreinfo::NT_GNU_BUILD_ID.to_le_bytes());
    v.extend_from_slice(b"GNU\0");
    v.extend_from_slice(&[0x5A; 20]);
    v
}

/// QEMU's `etc/vmcoreinfo` payload naming an ELF note at 0x1000.
fn fw_cfg_vmcoreinfo() -> Vec<u8> {
    vmcoreinfo::FwCfgVmcoreinfo {
        host_format: vmcoreinfo::FORMAT_ELF,
        guest_format: vmcoreinfo::FORMAT_ELF,
        size: 256,
        paddr: 0x1000,
    }
    .to_le_bytes()
    .to_vec()
}
