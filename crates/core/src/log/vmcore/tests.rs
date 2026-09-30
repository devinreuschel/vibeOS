//! Host tests for the core tool's portable half, over synthetic cores
//! (`synth`) and real kernel types.

#![allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "host tests index their own synthetic buffers"
)]

extern crate std;

use std::vec::Vec;

use super::synth::{self, SymDef, Synth};
use super::walk::*;
use super::*;
use crate::arch::stub::Arch;
use crate::log::vmcoreinfo::Info;
use crate::paging::PageFlags;

type K<'a, 'b> = Kernel<'a, SliceCore<'b>, Arch>;

const ID: [u8; 20] = [
    0xc6, 0x10, 0x89, 0xcb, 0x44, 0x99, 0x79, 0x49, 0x7c, 0x91, 0xe1, 0x5e, 0x26, 0xdb, 0x5b, 0x15,
    0x02, 0x15, 0xcb, 0x39,
];
const KBASE: u64 = 0xFFFF_FFFF_8000_0000;
const RAM: u64 = 16 << 20;

fn info(root: u64) -> Info<'static> {
    Info {
        osrelease: "0.8.0",
        build_id: &ID,
        page_size: 4096,
        pgt_root: root,
        pgt_levels: 4,
        log: KBASE + 0x10_0000,
        tcbs: KBASE + 0x20_0000,
        tcbs_len: 8,
        cpus: KBASE + 0x30_0000,
        cpus_len: 2,
    }
}

/// 16 MiB of RAM at 0, a 2 MiB kernel mapping at `KBASE`, a VMCOREINFO
/// note and two CPUs.
fn basic() -> Synth {
    let mut s = Synth::new(&[(0, RAM)]);
    s.map(KBASE, 0x20_0000, 0x20_0000, 0);
    let i = info(s.root);
    s.vmcoreinfo(&i);
    s.cpu(1, KBASE + 0x1000, KBASE + 0x8000, KBASE + 0x8000);
    s.cpu(2, KBASE + 0x2000, KBASE + 0x9000, 0);
    s
}

#[test]
fn core_parse_reads_loads_and_notes() {
    let bytes = basic().build();
    let core = SliceCore::new(&bytes).unwrap();
    let loads: Vec<Phdr> = core.phdrs().filter(|p| p.kind == PT_LOAD).collect();
    assert_eq!(loads.len(), 1);
    assert_eq!(loads[0].paddr, 0);
    assert_eq!(loads[0].filesz, RAM);
    assert_eq!(core.ram(), (RAM, 1));
    let notes: Vec<_> = core.notes().collect();
    assert_eq!(notes.len(), 3);
    assert_eq!(notes[0].name, b"VMCOREINFO");
    assert_eq!(notes[1].name, b"CORE");
    assert_eq!(notes[1].kind, NT_PRSTATUS);
    let mut b = [0u8; 4];
    assert!(core.read(RAM - 4, &mut b));
    assert!(!core.read(RAM - 2, &mut b));
    // Not a core, not ELF64, ELF32, another machine.
    let mut exec = bytes.clone();
    exec[16] = 2;
    assert_eq!(SliceCore::new(&exec).err(), Some(VmError::NotCore));
    let mut e32 = bytes.clone();
    e32[4] = 1;
    assert_eq!(SliceCore::new(&e32).err(), Some(VmError::Elf32));
    let mut other = bytes.clone();
    other[18] = 3;
    assert_eq!(SliceCore::new(&other).err(), Some(VmError::WrongMachine));
    assert_eq!(SliceCore::new(b"\x7fELF").err(), Some(VmError::NotElf));
    assert_eq!(SliceCore::new(&bytes[..40]).err(), Some(VmError::Truncated));
}

#[test]
fn prstatus_regs_at_linux_offsets() {
    let d = synth::prstatus(3, 0x1111, 0x2222, 0x3333);
    assert_eq!(d.len(), 336);
    let r = prstatus_regs(&d).unwrap();
    assert_eq!(
        r,
        PrRegs {
            pid: 3,
            rip: 0x1111,
            rsp: 0x2222,
            rbp: 0x3333,
            rflags: 2
        }
    );
    // `pr_reg` starts at 112; `rip` is its 17th word, `rbp` its 5th.
    assert_eq!(&d[112 + 16 * 8..112 + 17 * 8], &0x1111u64.to_le_bytes());
    assert_eq!(&d[112 + 4 * 8..112 + 5 * 8], &0x3333u64.to_le_bytes());
    assert_eq!(prstatus_regs(&d[..200]), Err(VmError::Truncated));
}

#[test]
fn vmcoreinfo_parses_key_value_lines() {
    let bytes = basic().build();
    let core = SliceCore::new(&bytes).unwrap();
    let vi = Vmcoreinfo::find(core.notes()).unwrap();
    let r = vi.roots().unwrap();
    let want = info(r.pgt_root);
    assert_eq!(r.pgt_levels, 4);
    assert_eq!(r.log, want.log);
    assert_eq!(r.tcbs, want.tcbs);
    assert_eq!(r.tcbs_len, 8);
    assert_eq!(r.cpus, want.cpus);
    assert_eq!(r.cpus_len, 2);
    assert_eq!(vi.build_id().unwrap().as_bytes(), &ID);
    assert_eq!(
        std::format!("{}", vi.build_id().unwrap()),
        "c61089cb449979497c91e15e26db5b150215cb39"
    );
    let bad = Vmcoreinfo {
        desc: b"PAGESIZE=40x6\nSYMBOL(vibeos_log)=zz\nBUILD-ID=abc\n",
    };
    assert_eq!(bad.number("PAGESIZE"), Err(VmError::BadValue));
    assert_eq!(bad.symbol("SYMBOL(vibeos_log)"), Err(VmError::BadValue));
    assert_eq!(bad.build_id(), Err(VmError::BadValue));
    assert_eq!(bad.number("LENGTH(vibeos_cpus)"), Err(VmError::MissingKey));
    let huge = Vmcoreinfo {
        desc: b"LENGTH(vibeos_cpus)=99999999999999999999999\n",
    };
    assert_eq!(huge.number("LENGTH(vibeos_cpus)"), Err(VmError::BadValue));
    // A core without the note.
    let mut s = Synth::new(&[(0, RAM)]);
    s.cpu(1, 0, 0, 0);
    let bytes = s.build();
    let core = SliceCore::new(&bytes).unwrap();
    assert_eq!(
        Vmcoreinfo::find(core.notes()).err(),
        Some(VmError::NoVmcoreinfo)
    );
}

#[test]
fn build_id_mismatch_is_refused_by_name() {
    let elf = synth::kernel_elf(&ID, KBASE, &[0x90; 64], &[]);
    let k = KernelElf::parse(&elf).unwrap();
    let elf_id = k.build_id().unwrap();
    let core_id = BuildId::from_bytes(&ID).unwrap();
    assert_eq!(check_build_id(&core_id, &elf_id), Ok(()));
    let mut other = ID;
    other[0] ^= 1;
    let other = BuildId::from_bytes(&other).unwrap();
    assert_eq!(
        check_build_id(&other, &elf_id),
        Err(VmError::BuildIdMismatch)
    );
    assert_eq!(VmError::BuildIdMismatch.as_str(), "BUILD-ID mismatch");
    assert_eq!(VmError::NoVmcoreinfo.as_str(), "no VMCOREINFO note");
    let bare = synth::kernel_elf(&[], KBASE, &[0x90; 64], &[]);
    let k = KernelElf::parse(&bare).unwrap();
    assert_eq!(k.build_id(), Err(VmError::NoBuildId));
    assert_eq!(VmError::NoBuildId.as_str(), "no build-id note in ELF");
    // A core is not a kernel ELF.
    let core = basic().build();
    assert_eq!(KernelElf::parse(&core).err(), Some(VmError::NotExec));
}

#[test]
fn walk_4level_1g_2m_4k() {
    let mut s = Synth::new(&[(0, 64 << 20)]);
    let giga = 0xFFFF_8000_0000_0000u64;
    s.map(giga, 0, 1 << 30, 0);
    s.map(KBASE, 0x20_0000, 0x20_0000, 0);
    let small = 0xFFFF_C000_0000_5000u64;
    s.map(small, 0x7000, 0x1000, 0);
    s.write_phys(0x7010, b"four-kib");
    s.write_phys(0x20_0100, b"two-mib!");
    s.write_phys(0x12_3456, b"one-gib!");
    let root = s.root;
    let bytes = s.build();
    let core = SliceCore::new(&bytes).unwrap();
    let k: K = Kernel::new(&core, root, 4).unwrap();
    assert_eq!(k.translate(small + 0x10), Ok(0x7010));
    assert_eq!(k.translate(KBASE + 0x100), Ok(0x20_0100));
    assert_eq!(k.translate(giga + 0x12_3456), Ok(0x12_3456));
    let mut b = [0u8; 8];
    k.read(small + 0x10, &mut b).unwrap();
    assert_eq!(&b, b"four-kib");
    k.read(KBASE + 0x100, &mut b).unwrap();
    assert_eq!(&b, b"two-mib!");
    k.read(giga + 0x12_3456, &mut b).unwrap();
    assert_eq!(&b, b"one-gib!");
    // A read that crosses from a 4 KiB page into an unmapped one fails.
    let mut long = [0u8; 16];
    assert_eq!(k.read(small + 0xFF8, &mut long), Err(VmError::NotMapped));
    // Mappings: the 1 GiB page is backed only up to 64 MiB, then the 2 MiB
    // and 4 KiB leaves; each a run of its own.
    let mut runs = Vec::new();
    k.kernel_mappings(|m| runs.push(m)).unwrap();
    assert_eq!(
        runs,
        [
            Mapping {
                va: giga,
                pa: 0,
                len: 64 << 20
            },
            Mapping {
                va: small,
                pa: 0x7000,
                len: 0x1000
            },
            Mapping {
                va: KBASE,
                pa: 0x20_0000,
                len: 0x20_0000
            },
        ]
    );
    assert_eq!(K::new(&core, root, 5).err(), Some(VmError::Levels));
}

#[test]
fn walk_rejects_absent_and_out_of_core() {
    let mut s = Synth::new(&[(0, RAM)]);
    s.map(KBASE, 0x20_0000, 0x20_0000, 0);
    // A leaf past the end of RAM, and an uncached one (MMIO).
    s.map(KBASE + 0x40_0000, 0xFEE0_0000, 0x1000, 0);
    s.map(KBASE + 0x40_1000, 0x1000, 0x1000, PageFlags::PCD);
    let root = s.root;
    let bytes = s.build();
    let core = SliceCore::new(&bytes).unwrap();
    let k: K = Kernel::new(&core, root, 4).unwrap();
    assert_eq!(k.translate(KBASE + 0x60_0000), Err(VmError::NotMapped));
    assert_eq!(
        k.translate(0x0000_9000_0000_0000),
        Err(VmError::NonCanonical)
    );
    let mut b = [0u8; 8];
    assert_eq!(k.read(KBASE + 0x40_0000, &mut b), Err(VmError::NotInCore));
    let mut runs = Vec::new();
    k.kernel_mappings(|m| runs.push(m)).unwrap();
    assert_eq!(
        runs,
        [Mapping {
            va: KBASE,
            pa: 0x20_0000,
            len: 0x20_0000
        }]
    );
    // Tables a bug made into a loop: every root slot names the root.
    let mut s = Synth::new(&[(0, RAM)]);
    let root = s.root;
    for i in 256..512u64 {
        s.write_phys(root + i * 8, &(root | 3).to_le_bytes());
    }
    let bytes2 = s.build();
    let core2 = SliceCore::new(&bytes2).unwrap();
    let k: K = Kernel::new(&core2, root, 4).unwrap().with_budget(1 << 16);
    assert_eq!(k.kernel_mappings(|_| {}), Err(VmError::WalkBudget));
    // A root outside the core.
    let k: K = Kernel::new(&core, RAM + 0x1000, 4).unwrap();
    assert_eq!(k.translate(KBASE), Err(VmError::NotInCore));
    let mut none = Vec::new();
    k.kernel_mappings(|m| none.push(m)).unwrap();
    assert!(none.is_empty());
}

#[test]
fn virtual_core_roundtrip() {
    let mut s = basic();
    s.map(KBASE + 0x20_0000, 0x1000, 0x1000, 0);
    s.write_phys(0x20_0000, b"kernel text");
    s.write_phys(0x1008, b"data");
    let root = s.root;
    let bytes = s.build();
    let core = SliceCore::new(&bytes).unwrap();
    let k: K = Kernel::new(&core, root, 4).unwrap();
    let mut notes = Vec::new();
    core.note_bytes(|n| notes.push(n));
    let mut out = Vec::new();
    let st = write_virtual_core(&k, &notes, |b| {
        out.extend_from_slice(b);
        Ok(())
    })
    .unwrap();
    assert_eq!(st.segments, 2);
    assert_eq!(st.bytes, out.len() as u64);
    let h = core_header(&out).unwrap();
    assert_eq!(h.phnum, 3);
    let ph: Vec<Phdr> = (0..h.phnum).map(|i| phdr(&out, &h, i).unwrap()).collect();
    assert_eq!(ph[0].kind, PT_NOTE);
    let note_bytes = &out[ph[0].offset as usize..][..ph[0].filesz as usize];
    let names: Vec<&[u8]> = Notes::new(note_bytes).map(|n| n.name).collect();
    assert_eq!(names, [&b"VMCOREINFO"[..], b"CORE", b"CORE"]);
    for p in &ph[1..] {
        assert_eq!(p.kind, PT_LOAD);
        assert_eq!(p.offset % PAGE, 0);
        // Every byte at a VA in the virtual core is the core's at its PA.
        let data = &out[p.offset as usize..][..p.filesz as usize];
        let mut want = std::vec![0u8; p.filesz as usize];
        k.read(p.vaddr, &mut want).unwrap();
        assert_eq!(data, &want[..]);
    }
    assert_eq!(ph[1].vaddr, KBASE);
    assert_eq!(&out[ph[1].offset as usize..][..11], b"kernel text");
    assert_eq!(ph[2].vaddr, KBASE + 0x20_0000);
    assert_eq!(&out[ph[2].offset as usize + 8..][..4], b"data");
    // A failing sink stops the writer with its error.
    assert_eq!(
        write_virtual_core(&k, &notes, |_| Err(VmError::Write)),
        Err(VmError::Write)
    );
}

#[test]
fn elf_symbols_and_text() {
    let syms = [
        SymDef {
            name: "_ZN6vibeos3smp9hang_test4hold17h0123456789abcdefE",
            value: KBASE + 0x10,
            size: 0x10,
            kind: STT_FUNC,
        },
        SymDef {
            name: "VIBEOS_TRACE",
            value: KBASE + 0x80_0000,
            size: 0x100,
            kind: STT_OBJECT,
        },
    ];
    let elf = synth::kernel_elf(&ID, KBASE, &[0xCC; 0x100], &syms);
    let k = KernelElf::parse(&elf).unwrap();
    let got: Vec<(Vec<u8>, u8, u64)> = k
        .symbols()
        .unwrap()
        .filter(|s| !s.name.is_empty())
        .map(|s| (s.name.to_vec(), s.kind, s.value))
        .collect();
    assert_eq!(got.len(), 2);
    assert_eq!(
        got[1],
        (b"VIBEOS_TRACE".to_vec(), STT_OBJECT, KBASE + 0x80_0000)
    );
    assert!(k.in_text(KBASE + 0xFF));
    assert!(!k.in_text(KBASE + 0x100));
    let text = k
        .sections()
        .find(|s| k.section_name(s) == b".text")
        .unwrap();
    assert_eq!(k.data(&text).unwrap(), &[0xCC; 0x100]);
    let loads: Vec<Phdr> = k.loads().collect();
    assert_eq!(loads.len(), 1);
    assert_eq!(loads[0].vaddr, KBASE);
}
