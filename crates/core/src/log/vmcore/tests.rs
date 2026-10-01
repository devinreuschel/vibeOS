//! Host tests for the core tool's portable half, over synthetic cores
//! (`synth`) and real kernel types.

#![allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "host tests index their own synthetic buffers"
)]

extern crate std;

use std::string::String;
use std::vec::Vec;

use super::sig::*;
use super::synth::{self, SymDef, Synth};
use super::tables::*;
use super::walk::*;
use super::*;
use core::mem::{offset_of, size_of};

use crate::arch::stub::Arch;
use crate::irq::stop::StopHow;
use crate::log::trace::{self, Event, RECORD_SIZE, RECORDS_PER_CPU, RecordData, TRACE_MAGIC};
use crate::log::vmcoreinfo::Info;
use crate::log::{KernelLogger, Level, MSG_CAP, RING_CAP, Record, Ring};
use crate::paging::PageFlags;
use crate::sched::ReadyQueue;
use crate::thread::{CpuContext, Tcb, ThreadState};

type K<'a, 'b> = Kernel<'a, SliceCore<'b>, Arch>;
type KRing = Ring<RING_CAP, MSG_CAP>;
type KRecord = Record<MSG_CAP>;

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
fn page_run_agrees_with_page_probes() {
    // Two segments with a gap, the second's file bytes cut short: every
    // run answers as the one-page probe of each page in it would.
    struct Probe<'a>(&'a SliceCore<'a>);
    impl PhysMem for Probe<'_> {
        fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
            self.0.read(addr, buf)
        }
    }
    let s = Synth::new(&[(0, 0x3000), (0x5800, 0x4000)]);
    let mut bytes = s.build();
    bytes.truncate(bytes.len() - 0x2000);
    let core = SliceCore::new(&bytes).unwrap();
    let probe = Probe(&core);
    let end = 0xC000u64;
    let mut held = Vec::new();
    let mut pa = 0;
    while pa < end {
        let (h, len) = core.page_run(pa, end - pa);
        assert!(len > 0 && len % PAGE == 0, "{pa:#x}: {len:#x}");
        for q in (pa..pa + len).step_by(PAGE as usize) {
            assert_eq!(probe.page_run(q, PAGE), (h, PAGE), "{q:#x}");
        }
        if h {
            held.push((pa, len));
        }
        pa += len;
    }
    // The file keeps 0x5800..0x7800 of the second segment: the page at
    // 0x5000 starts below it and the one at 0x8000 past it.
    assert_eq!(held, [(0, 0x3000), (0x6000, 0x2000)]);
    // A run stops at `max`.
    assert_eq!(core.page_run(0, 0x1000), (true, 0x1000));
    assert_eq!(
        page_run([(0, 0x1800)].into_iter(), 0x1000, 0x8000),
        (true, 0x1000)
    );
    assert_eq!(
        page_run([(0x4000, 1)].into_iter(), 0, 0x8000),
        (false, 0x4000)
    );
    assert_eq!(page_run(core::iter::empty(), 0, 0x8000), (false, 0x8000));
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

// ------------------------------------------------------------ backtraces

/// A frame chain on a stack at `KBASE + 0x10_0000`: `rets` are the return
/// addresses, innermost first; returns the first `rbp`.
fn chain(s: &mut Synth, rets: &[u64]) -> u64 {
    let stack = KBASE + 0x10_0000;
    let mut rbp = stack + 0x100;
    let first = rbp;
    for (i, r) in rets.iter().enumerate() {
        let next = if i + 1 == rets.len() { 0 } else { rbp + 0x40 };
        let pa = s.translate(rbp).unwrap();
        s.write_phys(pa, &next.to_le_bytes());
        s.write_phys(pa + 8, &r.to_le_bytes());
        rbp = next;
    }
    first
}

fn text(a: u64) -> bool {
    (KBASE..KBASE + 0x1000).contains(&a)
}

#[test]
fn unwind_follows_rbp() {
    let mut s = basic();
    let rbp = chain(&mut s, &[KBASE + 0x20, KBASE + 0x30, KBASE + 0x40]);
    let root = s.root;
    let bytes = s.build();
    let core = SliceCore::new(&bytes).unwrap();
    let k: K = Kernel::new(&core, root, 4).unwrap();
    let mut out = [0u64; BT_MAX];
    let n = k.unwind(KBASE + 0x10, rbp, text, &mut out);
    assert_eq!(
        &out[..n],
        &[KBASE + 0x10, KBASE + 0x20, KBASE + 0x30, KBASE + 0x40]
    );
}

#[test]
fn unwind_stops_on_bad_frames() {
    let mut s = basic();
    let root = s.root;
    // A return outside text ends the walk after the frames before it.
    let rbp = chain(&mut s, &[KBASE + 0x20, 0x4000, KBASE + 0x40]);
    let bytes = s.build();
    let core = SliceCore::new(&bytes).unwrap();
    let k: K = Kernel::new(&core, root, 4).unwrap();
    let mut out = [0u64; BT_MAX];
    assert_eq!(k.unwind(KBASE + 0x10, rbp, text, &mut out), 2);
    // Misaligned, user-half and unmapped `rbp`: only `rip`.
    for bad in [rbp + 1, 0x7000, KBASE + 0x80_0000] {
        assert_eq!(k.unwind(KBASE + 0x10, bad, text, &mut out), 1, "{bad:#x}");
    }
    // A zero `rbp` ends it at `rip`; a `rip` outside text prints once.
    assert_eq!(k.unwind(KBASE + 0x10, 0, text, &mut out), 1);
    assert_eq!(k.unwind(0x1234, rbp, text, &mut out), 1);
    assert_eq!(k.unwind(0, rbp, text, &mut out), 0);
    // A loop (saved `rbp` pointing at its own frame) stops, never spins.
    let mut s = basic();
    let root = s.root;
    let at = KBASE + 0x10_0100;
    let pa = s.translate(at).unwrap();
    s.write_phys(pa, &at.to_le_bytes());
    s.write_phys(pa + 8, &(KBASE + 0x20).to_le_bytes());
    let bytes = s.build();
    let core = SliceCore::new(&bytes).unwrap();
    let k: K = Kernel::new(&core, root, 4).unwrap();
    assert_eq!(k.unwind(KBASE + 0x10, at, text, &mut out), 1);
    // A chain longer than `BT_MAX` is cut at it.
    let mut s = basic();
    let root = s.root;
    let rets: Vec<u64> = (0..40).map(|i| KBASE + 0x100 + i).collect();
    let rbp = chain(&mut s, &rets);
    let bytes = s.build();
    let core = SliceCore::new(&bytes).unwrap();
    let k: K = Kernel::new(&core, root, 4).unwrap();
    assert_eq!(k.unwind(KBASE + 0x10, rbp, text, &mut out), BT_MAX);
}

// -------------------------------------------------------------- signature

fn norm(s: &str) -> String {
    let mut o = String::new();
    normalize_message(s, &mut o).unwrap();
    o
}

/// The vectors `tests/harness/test_run_forensics.py` repeats.
pub const NORMALIZE_VECTORS: &[(&str, &str)] = &[
    ("", ""),
    ("no numbers", "no numbers"),
    (
        "index out of bounds: the len is 3 but the index is 17",
        "index out of bounds: the len is N but the index is N",
    ),
    (
        "#GP rip=0xffffffff80123abc cs=0x8 err=0x0",
        "#GP rip=N cs=N err=N",
    ),
    ("u64 cpu1 0x 0xg 00x1f", "uN cpuN Nx Nxg NxNf"),
    ("DEADBEEF deadbeef 0XFF", "DEADBEEF deadbeef NXFF"),
    ("wait_acks late 12 s: cpu1", "wait_acks late N s: cpuN"),
    ("héllo 42 wörld", "héllo N wörld"),
];

#[test]
fn normalize_writes_numbers_as_n() {
    for (i, o) in NORMALIZE_VECTORS {
        assert_eq!(norm(i), *o, "{i:?}");
    }
}

fn sig(msg: Option<&str>, frames: &[Option<&str>]) -> String {
    let mut o = String::new();
    write_signature(&mut o, msg, frames.iter().copied()).unwrap();
    o
}

#[test]
fn signature_skips_panic_frames() {
    let frames = [
        Some("rust_begin_unwind"),
        Some("core::panicking::panic_fmt"),
        Some("core::option::unwrap_failed"),
        Some("vibeos::fs::vfs::lookup"),
        Some("vibeos::proc::proc_init::sys_open"),
        Some("vibeos::proc::syscall_init::dispatch"),
        Some("vibeos::x"),
    ];
    assert_eq!(
        sig(Some("called `Option::unwrap()` on a `None` value"), &frames),
        "sig: called `Option::unwrap()` on a `None` value @ vibeos::fs::vfs::lookup < vibeos::proc::proc_init::sys_open < vibeos::proc::syscall_init::dispatch"
    );
    for name in [
        "rust_begin_unwind",
        "__rustc::rust_begin_unwind",
        "core::panicking::panic",
        "core::option::expect_failed",
        "core::result::unwrap_failed",
        "core::slice::index::slice_end_index_len_fail",
        "core::str::slice_error_fail",
        "core::str::slice_error_fail_rt",
        "vibeos::log::panic::panic",
        "vibeos::log::panic::begin_dump",
    ] {
        assert!(is_panic_frame(name), "{name}");
    }
    for name in [
        "vibeos::fs::vfs::lookup",
        "core::option::Option<T>::map",
        "my_rust_begin_unwind",
        "vibeos::log::panic_test::stop_trip",
    ] {
        assert!(!is_panic_frame(name), "{name}");
    }
    // Only leading frames are skipped, and a timeout skips none.
    let f = [
        Some("vibeos::a"),
        Some("core::panicking::panic"),
        Some("vibeos::b"),
    ];
    assert_eq!(
        sig(Some("m"), &f),
        "sig: m @ vibeos::a < core::panicking::panic < vibeos::b"
    );
    let f = [Some("rust_begin_unwind"), Some("vibeos::a")];
    assert_eq!(
        sig(None, &f),
        "sig: timeout @ rust_begin_unwind < vibeos::a < ?"
    );
    // A missing symbol and missing frames are `?`.
    assert_eq!(sig(Some("x 7"), &[None]), "sig: x N @ ? < ? < ?");
}

#[test]
fn timeout_signature_uses_first_busy_cpu() {
    // (cpu_id, current, idle)
    let cpus = [
        (2, 0x30, 0x30),
        (1, 0x50, 0x20),
        (0, 0x10, 0x10),
        (3, 0x70, 0x40),
    ];
    assert_eq!(pick_cpu(None, &cpus), 1);
    assert_eq!(pick_cpu(Some(3), &cpus), 3);
    let idle = [(1, 0x20, 0x20), (0, 0x10, 0x10)];
    assert_eq!(pick_cpu(None, &idle), 0);
    assert_eq!(pick_cpu(None, &[]), 0);
    assert_eq!(
        sig(
            None,
            &[
                Some("vibeos::smp::hang_test::hold"),
                Some("vibeos::smp::hang_test::arm"),
                Some("vibeos::boot_rest")
            ]
        ),
        "sig: timeout @ vibeos::smp::hang_test::hold < vibeos::smp::hang_test::arm < vibeos::boot_rest"
    );
}

#[test]
fn signature_has_no_addresses() {
    let s = sig(
        Some("page fault at 0xdeadbeef00 cr2=0x10 in 3 frames"),
        &[Some("vibeos::mm::fault"), None, Some("vibeos::main")],
    );
    assert_eq!(
        s,
        "sig: page fault at N crN=N in N frames @ vibeos::mm::fault < ? < vibeos::main"
    );
    let tail = s.split(" @ ").nth(1).unwrap();
    assert!(!tail.contains("0x") && !tail.contains('+'));
    assert!(!s.chars().any(|c| c.is_ascii_digit()));
}

#[test]
fn panic_line_publishes_len_last() {
    let l = PanicLine::new();
    assert_eq!(l.read(), None);
    l.record(3, b"index 7 out of range\nsecond line");
    let (cpu, t) = l.read().unwrap();
    assert_eq!(cpu, 3);
    assert_eq!(t.as_bytes(), b"index 7 out of range");
    // An empty message still reads as recorded.
    let e = PanicLine::new();
    e.record(0, b"");
    assert_eq!(e.read().map(|(c, t)| (c, t.len)), Some((0, 0)));
    // Cut at the cap, back to a character boundary.
    let long = [b'a'; 200];
    assert_eq!(first_line(&long).len(), PANIC_LINE_CAP);
    let mut uni = std::vec![b'a'; PANIC_LINE_CAP - 1];
    uni.extend_from_slice("é".as_bytes());
    assert_eq!(first_line(&uni).len(), PANIC_LINE_CAP - 1);
    // A cut that ends on a field's space drops it, and so does a line's
    // own trailing whitespace.
    let mut spaced = std::vec![b'x'; PANIC_LINE_CAP - 1];
    spaced.extend_from_slice(b" err=0x8");
    assert_eq!(first_line(&spaced), &spaced[..PANIC_LINE_CAP - 1]);
    assert_eq!(first_line(b"boom \t\r\nnext"), b"boom");
    // The bytes a core holds decode the same, and `len` is the word at 0:
    // before it is set the line reads as absent whatever the bytes hold.
    let mut raw = [0u8; size_of::<PanicLine>()];
    raw[8..12].copy_from_slice(b"junk");
    raw[4] = 2;
    assert_eq!(PanicLine::decode(&raw), None);
    raw[0..4].copy_from_slice(&(4u32 | (1 << 31)).to_le_bytes());
    let (cpu, t) = PanicLine::decode(&raw).unwrap();
    assert_eq!((cpu, t.as_bytes()), (2, &b"junk"[..]));
}

// ------------------------------------------------------------ kernel types

/// A zeroed, aligned allocation of `T`'s layout, whose bytes the test
/// fills field by field and reads back as bytes; never read as a `T`.
struct Raw<T> {
    p: *mut T,
}

impl<T> Raw<T> {
    fn new() -> Self {
        let layout = std::alloc::Layout::new::<T>();
        // SAFETY: `T`'s layout is not zero-sized: `Raw` is built only for
        // `Tcb` and `PerCpu` in this module; established here.
        let p = unsafe { std::alloc::alloc_zeroed(layout) } as *mut T;
        assert!(!p.is_null());
        Raw { p }
    }

    fn addr(&self) -> u64 {
        self.p as u64
    }

    fn bytes(&self) -> Vec<u8> {
        // SAFETY: `p` holds `size_of::<T>()` bytes, zeroed at allocation
        // and written only through field places; established here.
        unsafe { core::slice::from_raw_parts(self.p as *const u8, size_of::<T>()) }.to_vec()
    }
}

impl<T> Drop for Raw<T> {
    fn drop(&mut self) {
        // SAFETY: `p` was allocated in `new` with this layout, and no `T`
        // was ever formed, so nothing is dropped in place; established here.
        unsafe { std::alloc::dealloc(self.p as *mut u8, std::alloc::Layout::new::<T>()) }
    }
}

fn logger_with(n: usize) -> std::boxed::Box<KernelLogger> {
    let mut l = std::boxed::Box::new(KernelLogger::new());
    l.filter.set(Level::Trace);
    for i in 0..n {
        let msg = std::format!("vibeOS: record {i}");
        l.emit(Record::from_msg(
            i as u64 * 10,
            (i % 4) as u8,
            Level::Info,
            msg.as_bytes(),
        ));
    }
    l
}

/// The logger's bytes, built record by record at their offsets (the
/// logger's padding is never read).
fn logger_bytes(l: &KernelLogger) -> Vec<u8> {
    let mut b = std::vec![0u8; size_of::<KernelLogger>()];
    let ring = offset_of!(KernelLogger, ring);
    for (i, r) in l.ring.recs.iter().enumerate() {
        let at = ring + offset_of!(KRing, recs) + i * size_of::<KRecord>();
        b[at..at + 8].copy_from_slice(&r.timestamp.to_le_bytes());
        b[at + offset_of!(KRecord, cpu_id)] = r.cpu_id;
        b[at + offset_of!(KRecord, level)] = r.level.as_u8();
        b[at + offset_of!(KRecord, len)] = r.len;
        b[at + offset_of!(KRecord, msg)..][..MSG_CAP].copy_from_slice(&r.msg);
    }
    for (off, v) in [
        (offset_of!(KRing, head), l.ring.head as u64),
        (offset_of!(KRing, len), l.ring.len as u64),
        (offset_of!(KRing, dropped), l.ring.dropped),
        (offset_of!(KRing, written), l.ring.written),
    ] {
        b[ring + off..][..8].copy_from_slice(&v.to_le_bytes());
    }
    b
}

fn log_texts<M: PhysMem>(k: &Kernel<'_, M, Arch>, at: u64) -> Vec<Vec<u8>> {
    let h = k.log_header(at).unwrap();
    let mut got = Vec::new();
    k.log_tail(at, &h, |r| got.push(r.text().to_vec())).unwrap();
    got
}

#[test]
fn log_tail_matches_ring_last_n() {
    for n in [0usize, 5, LOG_TAIL, RING_CAP, RING_CAP + 37] {
        let l = logger_with(n);
        let mut s = basic();
        let at = KBASE + 0x40_0000;
        s.place(at, &logger_bytes(&l));
        let root = s.root;
        let bytes = s.build();
        let core = SliceCore::new(&bytes).unwrap();
        let k: K = Kernel::new(&core, root, 4).unwrap();
        let want: Vec<Vec<u8>> = l.ring.last_n(LOG_TAIL).map(|r| r.msg().to_vec()).collect();
        assert_eq!(log_texts(&k, at), want, "n={n}");
        let h = k.log_header(at).unwrap();
        assert_eq!(h.len, l.ring.len() as u64);
        assert_eq!(h.dropped, l.ring.dropped());
    }
    // A torn newest record: its level and length are not a whole record's.
    let l = logger_with(3);
    let mut b = logger_bytes(&l);
    let at = offset_of!(KernelLogger, ring) + offset_of!(KRing, recs) + 2 * size_of::<KRecord>();
    b[at + offset_of!(KRecord, level)] = 0xEE;
    let mut s = basic();
    let va = KBASE + 0x40_0000;
    s.place(va, &b);
    let root = s.root;
    let bytes = s.build();
    let core = SliceCore::new(&bytes).unwrap();
    let k: K = Kernel::new(&core, root, 4).unwrap();
    let h = k.log_header(va).unwrap();
    let mut torn = Vec::new();
    k.log_tail(va, &h, |r| torn.push(r.torn)).unwrap();
    assert_eq!(torn, [false, false, true]);
    // A length past the ring is a bad layout, not a panic.
    let mut b = logger_bytes(&l);
    let off = offset_of!(KernelLogger, ring) + offset_of!(KRing, len);
    b[off..off + 8].copy_from_slice(&(RING_CAP as u64 + 1).to_le_bytes());
    let mut s = basic();
    s.place(va, &b);
    let root = s.root;
    let bytes = s.build();
    let core = SliceCore::new(&bytes).unwrap();
    let k: K = Kernel::new(&core, root, 4).unwrap();
    assert_eq!(k.log_header(va), Err(VmError::BadLayout));
}

#[test]
fn vmcore_reads_kernel_layouts() {
    use crate::per_cpu::{PerCpu, PerCpuRemote};
    use crate::thread::ThreadId;
    use core::ptr::addr_of_mut;

    // Two TCBs, built in zeroed memory at real offsets. Only fields of
    // plain integers go through typed stores: an enum's or a pointer's
    // store would leave padding or provenance in the bytes read back. The
    // state's tag (a `u32` at 0) and payload word (at 8) are written as
    // integers, as docs/VMCOREINFO.md lays them out.
    let t1: Raw<Tcb> = Raw::new();
    let t2: Raw<Tcb> = Raw::new();
    // SAFETY: each place is a field of the zeroed allocation `Raw::new`
    // made for a `Tcb`, and the state words lie inside `state` (16 bytes,
    // 8-aligned); no `Tcb` value is formed; established here.
    unsafe {
        addr_of_mut!((*t1.p).id).write(ThreadId(7));
        let st = addr_of_mut!((*t1.p).state).cast::<u32>();
        st.write(1);
        addr_of_mut!((*t1.p).context).write(CpuContext {
            rbx: 1,
            rbp: 2,
            r12: 3,
            r13: 4,
            r14: 5,
            r15: 6,
            rflags: 0x202,
            rsp: 0x8,
            rip: 0x9,
        });
        addr_of_mut!((*t1.p).cpu).write(1);
        addr_of_mut!((*t1.p).pid).write(42);
        addr_of_mut!((*t2.p).id).write(ThreadId(9));
        let st = addr_of_mut!((*t2.p).state).cast::<u32>();
        st.write(3);
        st.cast::<u8>().add(8).cast::<u64>().write(0xABCD);
        addr_of_mut!((*t2.p).context).write(CpuContext::empty());
    }
    // The state words match the enum's own layout.
    const _: () = assert!(core::mem::align_of::<ThreadState>() == 8);
    let slots: [u64; 4] = [t1.addr(), 0, t2.addr(), 0];
    // One CPU: current t1, idle t2, run queue [9, 7]. The queue is a real
    // ReadyQueue; its ring goes into the core at a chosen VA, in storage
    // order (after one push and pop the front is slot 1), and the PerCpu's
    // queue words name it, at ReadyQueue::CORE_OFFSETS.
    let mut q = ReadyQueue::try_new(4).unwrap();
    q.push_back(ThreadId(3));
    assert_eq!(q.pop_front(), Some(ThreadId(3)));
    q.push_back(ThreadId(9));
    q.push_back(ThreadId(7));
    let (head, cap) = (1usize, q.capacity());
    let mut ring = std::vec![0xFFu8; cap * 4];
    ring[..4].copy_from_slice(&3u32.to_le_bytes());
    for (k, id) in q.iter().enumerate() {
        let slot = (head + k) % cap;
        ring[slot * 4..][..4].copy_from_slice(&id.raw().to_le_bytes());
    }
    let ring_va = KBASE + 0x70_0000;
    let remote_va = KBASE + 0x71_0000;
    let cpu: Raw<PerCpu> = Raw::new();
    // SAFETY: `cpu_id` is a field of `cpu`'s zeroed allocation for a
    // `PerCpu`; no `PerCpu` value is formed; established here.
    unsafe { addr_of_mut!((*cpu.p).cpu_id).write(1) };
    let mut cpu_bytes = cpu.bytes();
    let mut put = |off: usize, v: u64| cpu_bytes[off..][..8].copy_from_slice(&v.to_le_bytes());
    put(PerCpu::CORE_CURRENT, t1.addr());
    put(PerCpu::CORE_IDLE, t2.addr());
    put(offset_of!(PerCpu, remote), remote_va);
    let [o_ids, o_cap, o_head, o_len] = ReadyQueue::CORE_OFFSETS;
    let rq = offset_of!(PerCpu, runq);
    put(rq + o_ids, ring_va);
    put(rq + o_cap, cap as u64);
    put(rq + o_head, head as u64);
    put(rq + o_len, q.len() as u64);
    // The remote view's words the tool reads, at their offsets.
    let mut remote = std::vec![0u8; size_of::<PerCpuRemote>()];
    remote[offset_of!(PerCpuRemote, apic_id)..][..4].copy_from_slice(&5u32.to_le_bytes());
    remote[offset_of!(PerCpuRemote, stopped)..][..4]
        .copy_from_slice(&StopHow::Nmi.code().to_le_bytes());
    for (i, w) in [0x11u64, 0x22, 0x33, 0x44].iter().enumerate() {
        remote[offset_of!(PerCpuRemote, crash) + 8 * i..][..8].copy_from_slice(&w.to_le_bytes());
    }
    // The real log ring.
    let l = logger_with(70);
    let mut s = basic();
    let log_va = KBASE + 0x40_0000;
    s.place(log_va, &logger_bytes(&l));
    s.place(t1.addr(), &t1.bytes());
    s.place(t2.addr(), &t2.bytes());
    let slot_va = KBASE + 0x50_0000;
    s.place(
        slot_va,
        &slots
            .iter()
            .flat_map(|a| a.to_le_bytes())
            .collect::<Vec<u8>>(),
    );
    let cpus_va = KBASE + 0x60_0000;
    s.place(cpus_va, &cpu_bytes);
    s.place(remote_va, &remote);
    s.place(ring_va, &ring);
    let root = s.root;
    let bytes = s.build();
    let core = SliceCore::new(&bytes).unwrap();
    let k: K = Kernel::new(&core, root, 4).unwrap();

    // Ring order equals `Ring::iter` for the last `LOG_TAIL` records.
    let want: Vec<Vec<u8>> = l
        .ring
        .iter()
        .skip(70 - LOG_TAIL)
        .map(|r| r.msg().to_vec())
        .collect();
    assert_eq!(log_texts(&k, log_va), want);

    // The TCB table.
    let mut threads = Vec::new();
    for i in 0..4 {
        let a = k.tcb_slot(slot_va, i).unwrap();
        if a != 0 {
            threads.push(k.thread(a).unwrap());
        }
    }
    assert_eq!(threads.len(), 2);
    let a = threads[0];
    assert_eq!(
        (a.id, state_name(a.state), a.cpu, a.pid),
        (7, "running", 1, 42)
    );
    assert_eq!((a.rbx, a.rbp, a.r12, a.r15), (1, 2, 3, 6));
    assert_eq!((a.rflags, a.rsp, a.rip), (0x202, 8, 9));
    let b = threads[1];
    assert_eq!(
        (b.id, state_name(b.state), b.state_arg),
        (9, "blocked", 0xABCD)
    );

    // The CPU: its current thread and run queue, APIC id and crash slot.
    let c = k.cpu(cpus_va, 0).unwrap();
    assert_eq!(c.cpu_id, 1);
    assert_eq!(c.apic_id, 5);
    assert_eq!(c.cur_tcb, t1.addr());
    assert_eq!(c.idle_tcb, t2.addr());
    assert_eq!(k.thread(c.cur_tcb).unwrap().id, 7);
    assert_eq!(c.runq.len, 2);
    let rq: Vec<u32> = (0..c.runq.len)
        .map(|i| k.runq_id(&c.runq, i).unwrap())
        .collect();
    let want: Vec<u32> = q.iter().map(ThreadId::raw).collect();
    assert_eq!(rq, want);
    assert_eq!(rq, [9, 7]);
    assert_eq!(k.runq_id(&c.runq, 2), Err(VmError::BadLayout));
    assert_eq!(c.stop_how(), Some(StopHow::Nmi));
    assert_eq!(c.slot(), Some((0x11, 0x22, 0x33)));
}

#[test]
fn vmcore_trace_export_fields() {
    use crate::log::trace::{
        CLOCK_PUBLISHED, RING_HEAD_OFF, RING_RECORDS_OFF, RING_SIZE, TRACE_FLAGS_OFF,
        TRACE_FREQ_OFF, TRACE_MAGIC_OFF, TRACE_RINGS_OFF, TRACE_SKEW_OFF, TSC_INVARIANT,
        WARP_BACKWARD, WARP_MEASURED,
    };
    let rec = |seq: u64, tsc: u64, cpu: u32, ev: Event| RecordData {
        seq,
        tsc,
        a: 1,
        b: 2,
        cpu,
        event: ev.as_u32(),
    };
    for (flags, skew, order) in [
        (CLOCK_PUBLISHED | TSC_INVARIANT | WARP_MEASURED, 0, "global"),
        (
            CLOCK_PUBLISHED | TSC_INVARIANT | WARP_MEASURED | WARP_BACKWARD,
            9,
            "per-cpu",
        ),
        (CLOCK_PUBLISHED, 0, "per-cpu"),
    ] {
        let mut s = basic();
        let at = KBASE + 0x80_0000;
        let hdr = TRACE_RINGS_OFF + 2 * RING_SIZE;
        let mut b = std::vec![0u8; hdr];
        b[TRACE_MAGIC_OFF..][..8].copy_from_slice(&TRACE_MAGIC);
        b[TRACE_FREQ_OFF..][..8].copy_from_slice(&1_000_000_000u64.to_le_bytes());
        b[TRACE_SKEW_OFF..][..8].copy_from_slice(&(skew as u64).to_le_bytes());
        b[TRACE_FLAGS_OFF..][..4].copy_from_slice(&flags.to_le_bytes());
        // CPU 0: three records, the last torn (seq 0); CPU 1: one.
        let rings: [(u64, Vec<RecordData>); 2] = [
            (
                3,
                std::vec![
                    rec(1, 100, 0, Event::Switch),
                    rec(2, 300, 0, Event::Wake),
                    RecordData {
                        seq: 0,
                        ..rec(3, 400, 0, Event::IrqEnter)
                    },
                ],
            ),
            (1, std::vec![rec(1, 200, 1, Event::IpiAck)]),
        ];
        for (cpu, (head, recs)) in rings.iter().enumerate() {
            let r = TRACE_RINGS_OFF + cpu * RING_SIZE;
            b[r + RING_HEAD_OFF..][..8].copy_from_slice(&head.to_le_bytes());
            for (i, d) in recs.iter().enumerate() {
                b[r + RING_RECORDS_OFF + i * RECORD_SIZE..][..RECORD_SIZE]
                    .copy_from_slice(&d.to_le_bytes());
            }
        }
        s.place(at, &b);
        let root = s.root;
        let bytes = s.build();
        let core = SliceCore::new(&bytes).unwrap();
        let k: K = Kernel::new(&core, root, 4).unwrap();
        let clock = k.trace_clock(at).unwrap();
        assert_eq!(clock.freq_hz, 1_000_000_000);
        let mut bufs = [[RecordData::default(); RECORDS_PER_CPU]; 2];
        let mut lens = [0usize; 2];
        for cpu in 0..2 {
            lens[cpu] = k.trace_ring(at, cpu as u32, &mut bufs[cpu]).unwrap();
        }
        assert_eq!(lens, [2, 1], "the torn record is dropped");
        let cpus: Vec<(u32, &[RecordData])> =
            (0..2).map(|c| (c as u32, &bufs[c][..lens[c]])).collect();
        let mut j = String::new();
        trace::export_chrome(&cpus, &clock, &mut j).unwrap();
        assert!(j.starts_with("{\"traceEvents\":["), "{j}");
        let body = &j["{\"traceEvents\":[".len()..j.find("],\"displayTimeUnit\"").unwrap()];
        let events: Vec<&str> = body.split("},{").collect();
        assert!(!events.is_empty());
        let mut tids = std::collections::BTreeSet::new();
        for e in &events {
            for key in ["\"name\":", "\"ph\":", "\"ts\":", "\"pid\":", "\"tid\":"] {
                assert!(e.contains(key), "{key} missing in {e}");
            }
            if e.contains("\"ph\":\"i\"") {
                let tid = e.split("\"tid\":").nth(1).unwrap();
                tids.insert(tid.split(',').next().unwrap().to_string());
            }
        }
        assert_eq!(tids.len(), 2, "both CPUs appear: {j}");
        assert_eq!(
            events.iter().filter(|e| e.contains("\"ph\":\"i\"")).count(),
            3
        );
        assert!(j.contains(&std::format!("\"order\":\"{order}\"")), "{j}");
        // The ordering statement follows the warp result.
        let want = if order == "global" {
            trace::Order::Global
        } else {
            clock.order()
        };
        assert_eq!(clock.order(), want);
    }
    // No magic: no live trace.
    let mut s = basic();
    let at = KBASE + 0x80_0000;
    s.place(at, &[0u8; 64]);
    let root = s.root;
    let bytes = s.build();
    let core = SliceCore::new(&bytes).unwrap();
    let k: K = Kernel::new(&core, root, 4).unwrap();
    assert_eq!(k.trace_clock(at), Err(VmError::NoTrace));
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
