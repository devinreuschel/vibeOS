//! `vmcore report --core <file|-> --elf <kernel.elf> [--virt <out>]
//! [--trace <out.json>]`: the core tool (ROADMAP §10.7). It reads a physical
//! QEMU guest core (`dump-guest-memory` without paging) from a file or from
//! stdin (the harness pipes `zstd -dc`) with the kernel ELF it came from,
//! refuses a core whose VMCOREINFO `BUILD-ID` is not the ELF's, and prints
//! the report: the `sig:` line first, then the core's RAM, each CPU's
//! current thread, run queue and symbolized frame-pointer backtrace, every
//! thread of the TCB table, the last 64 log records and the trace's
//! ordering. `--virt` writes the virtually addressed core `gdb` opens with
//! the ELF; `--trace` writes every CPU's flight-recorder ring as one Chrome
//! trace-event JSON timeline. The decoding is `vibeos::log::vmcore`'s; the
//! formats are TESTING.md §8.3's.
//!
//! The core goes into a sparse store of its non-zero 4 KiB pages, read in
//! file order, so a 9 GiB guest's core fits. Exits 0, 1 on an error, 2 on
//! bad usage, 3 on a `BUILD-ID` mismatch, 4 on a core with no VMCOREINFO.

#![allow(
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "host tool: `alloc`'s growing calls may panic, and a failed allocation ends this host process, not the kernel (DESIGN §4.4)"
)]

use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::process::ExitCode;

use vibeos::arch::stub::Arch;
use vibeos::log::trace::{self, RECORDS_PER_CPU, RecordData};
use vibeos::log::vmcore::sig::{pick_cpu, write_signature};
use vibeos::log::vmcore::tables::{CpuView, state_name};
use vibeos::log::vmcore::walk::{self, Kernel};
use vibeos::log::vmcore::{
    BT_MAX, KernelElf, LOG_TAIL, NT_PRSTATUS, Notes, PAGE, PT_NOTE, PhysMem, PrRegs, Roots,
    STT_FUNC, VmError, Vmcoreinfo, check_build_id, core_header, prstatus_regs,
};
use vibeos::proc::elf::{EHDR_SIZE, PHDR_SIZE, PT_LOAD};
use vibeos::symtab::{self, Entry};

/// The panic line's static, found by its demangled path's suffix.
const PANIC_LINE_SYM: &str = "log::panic::PANIC_LINE";
/// The flight recorder's static, exported unmangled.
const TRACE_SYM: &str = "VIBEOS_TRACE";
/// The dump's symbol rule: an address more than this past its symbol has
/// none (`log::panic::print_frame_addr`).
const SYM_REACH: u64 = 0x1_0000;

/// Why the tool stops, and its exit code.
#[derive(Debug, PartialEq, Eq)]
enum Fail {
    Usage(String),
    Mismatch { core: String, elf: String },
    NoNote,
    Other(String),
}

impl Fail {
    fn code(&self) -> u8 {
        match self {
            Fail::Other(_) => 1,
            Fail::Usage(_) => 2,
            Fail::Mismatch { .. } => 3,
            Fail::NoNote => 4,
        }
    }

    fn message(&self) -> String {
        match self {
            Fail::Usage(m) => format!("vmcore: {m}\n{USAGE}"),
            Fail::Mismatch { core, elf } => {
                format!(
                    "vmcore: {}: core {core} elf {elf}",
                    VmError::BuildIdMismatch.as_str()
                )
            }
            Fail::NoNote => format!("vmcore: {}", VmError::NoVmcoreinfo.as_str()),
            Fail::Other(m) => format!("vmcore: {m}"),
        }
    }
}

fn other(what: &str, e: impl std::fmt::Display) -> Fail {
    Fail::Other(format!("{what}: {e}"))
}

/// One report line, formatted onto `out` (a `String`, which cannot fail).
macro_rules! put {
    ($out:expr, $($arg:tt)*) => {{
        $out.push_str(&format!($($arg)*));
        $out.push('\n');
    }};
}

const USAGE: &str =
    "usage: vmcore report --core <file|-> --elf <kernel.elf> [--virt <out>] [--trace <out.json>]";

// ------------------------------------------------------------ sparse store

/// One `PT_LOAD` of the core: `memsz` bytes of RAM at `paddr`, of which the
/// first `filesz` came from the file.
#[derive(Clone, Copy, Debug)]
struct Seg {
    paddr: u64,
    memsz: u64,
}

/// A physical core as its non-zero 4 KiB pages, its segments and its note
/// segments' bytes.
#[derive(Default)]
struct Store {
    pages: HashMap<u64, Box<[u8; PAGE as usize]>>,
    segs: Vec<Seg>,
    notes: Vec<Vec<u8>>,
}

impl Store {
    fn ram(&self) -> (u64, usize) {
        let bytes = self.segs.iter().map(|s| s.memsz).sum();
        (bytes, self.segs.len())
    }

    fn in_ram(&self, pa: u64) -> bool {
        self.segs
            .iter()
            .any(|s| pa >= s.paddr && pa - s.paddr < s.memsz)
    }

    /// Keep `data`, which the file holds at `pa`, page by page; an all-zero
    /// page is not kept.
    fn put(&mut self, pa: u64, data: &[u8]) {
        let mut done = 0usize;
        while done < data.len() {
            let at = pa + done as u64;
            let in_page = (at % PAGE) as usize;
            let n = (PAGE as usize - in_page).min(data.len() - done);
            let chunk = &data[done..done + n];
            if chunk.iter().any(|&b| b != 0) {
                let page = self
                    .pages
                    .entry(at / PAGE)
                    .or_insert_with(|| Box::new([0u8; PAGE as usize]));
                page[in_page..in_page + n].copy_from_slice(chunk);
            }
            done += n;
        }
    }
}

impl PhysMem for Store {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        for (i, b) in buf.iter_mut().enumerate() {
            let Some(pa) = addr.checked_add(i as u64) else {
                return false;
            };
            if !self.in_ram(pa) {
                return false;
            }
            *b = self
                .pages
                .get(&(pa / PAGE))
                .map_or(0, |p| p[(pa % PAGE) as usize]);
        }
        true
    }
}

/// A byte source that counts its position and can skip forward; a file
/// can also go back.
struct Source<R: Read> {
    r: R,
    pos: u64,
    seek: Option<fn(&mut R, u64) -> io::Result<()>>,
}

impl<R: Read> Source<R> {
    fn exact(&mut self, buf: &mut [u8]) -> Result<(), Fail> {
        self.r.read_exact(buf).map_err(|e| other("core: read", e))?;
        self.pos += buf.len() as u64;
        Ok(())
    }

    /// Move to `to`: skip forward on a stream, seek on a file. A stream
    /// cannot go back.
    fn goto(&mut self, to: u64) -> Result<(), Fail> {
        if to == self.pos {
            return Ok(());
        }
        if let Some(seek) = self.seek {
            seek(&mut self.r, to).map_err(|e| other("core: seek", e))?;
            self.pos = to;
            return Ok(());
        }
        if to < self.pos {
            return Err(Fail::Other(format!(
                "core: a segment at file offset {to:#x} comes before offset {:#x} already read: stdin needs segments in file order; pass --core <file>",
                self.pos
            )));
        }
        let skip = to - self.pos;
        let n = io::copy(&mut (&mut self.r).take(skip), &mut io::sink())
            .map_err(|e| other("core: read", e))?;
        if n != skip {
            return Err(Fail::Other("core: truncated".into()));
        }
        self.pos = to;
        Ok(())
    }
}

/// Read a core into a [`Store`]: the header, the program headers, then each
/// `PT_NOTE` and `PT_LOAD` in file-offset order.
fn load_core<R: Read>(src: &mut Source<R>) -> Result<Store, Fail> {
    let mut head = [0u8; EHDR_SIZE];
    src.exact(&mut head)?;
    let h = core_header(&head).map_err(|e| other("core", e))?;
    src.goto(h.phoff)?;
    let mut ph = vec![0u8; usize::from(h.phnum) * PHDR_SIZE];
    src.exact(&mut ph)?;
    let mut segs: Vec<_> = ph
        .as_chunks::<PHDR_SIZE>()
        .0
        .iter()
        .filter_map(|p| vibeos::log::vmcore::Phdr::parse(p).ok())
        .filter(|p| p.kind == PT_LOAD || p.kind == PT_NOTE)
        .collect();
    if src.seek.is_none()
        && let Some(w) = segs.windows(2).find(|w| w[1].offset < w[0].offset)
    {
        return Err(Fail::Other(format!(
            "core: a segment at file offset {:#x} comes before offset {:#x} already read: stdin needs segments in file order; pass --core <file>",
            w[1].offset, w[0].offset
        )));
    }
    segs.sort_by_key(|p| p.offset);
    let mut store = Store::default();
    let mut buf = vec![0u8; 1 << 20];
    for p in segs {
        src.goto(p.offset)?;
        if p.kind == PT_NOTE {
            let mut n = vec![0u8; usize::try_from(p.filesz).map_err(|e| other("core", e))?];
            src.exact(&mut n)?;
            store.notes.push(n);
            continue;
        }
        store.segs.push(Seg {
            paddr: p.paddr,
            memsz: p.memsz.max(p.filesz),
        });
        let mut left = p.filesz;
        let mut pa = p.paddr;
        while left > 0 {
            let n = left.min(buf.len() as u64) as usize;
            src.exact(&mut buf[..n])?;
            store.put(pa, &buf[..n]);
            pa += n as u64;
            left -= n as u64;
        }
    }
    Ok(store)
}

// ----------------------------------------------------------------- symbols

/// The ELF's function symbols, demangled without hash (rustc-demangle's
/// alternate form) and sorted by address for `symtab::lookup`; names are
/// leaked once for `Entry`'s `&'static str`. `statics` keeps every symbol
/// by demangled name for [`Symbols::find`].
struct Symbols {
    funcs: Vec<Entry>,
    all: Vec<(String, u64)>,
}

fn demangle(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    format!("{:#}", rustc_demangle::demangle(&s))
}

impl Symbols {
    fn new(elf: &KernelElf<'_>) -> Result<Self, Fail> {
        let mut funcs = Vec::new();
        let mut all = Vec::new();
        for s in elf.symbols().map_err(|e| other("elf", e))? {
            if s.name.is_empty() || s.value == 0 {
                continue;
            }
            let name = demangle(s.name);
            if s.kind == STT_FUNC {
                funcs.push(Entry {
                    addr: s.value,
                    name: Box::leak(name.clone().into_boxed_str()),
                });
            }
            all.push((name, s.value));
        }
        funcs.sort_by_key(|e| e.addr);
        Ok(Self { funcs, all })
    }

    /// The function holding `addr` and the offset into it, by the dump's
    /// rule.
    fn name(&self, addr: u64) -> Option<(&'static str, u64)> {
        let e = symtab::lookup(&self.funcs, addr)?;
        let off = symtab::offset(e, addr);
        (off < SYM_REACH).then_some((e.name, off))
    }

    /// The one symbol whose demangled name is `path` or ends in `::path`.
    fn find(&self, path: &str) -> Result<u64, String> {
        let tail = format!("::{path}");
        let hits: Vec<u64> = self
            .all
            .iter()
            .filter(|(n, _)| n == path || n.ends_with(&tail))
            .map(|(_, v)| *v)
            .collect();
        match hits.as_slice() {
            [v] => Ok(*v),
            [] => Err(format!("{}: {path}", VmError::NoSymbol.as_str())),
            _ => Err(format!("{}: {path}", VmError::AmbiguousSymbol.as_str())),
        }
    }
}

// ------------------------------------------------------------------ report

struct Opts {
    core: String,
    elf: String,
    virt: Option<String>,
    trace: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Opts, Fail> {
    let mut it = args.iter();
    if it.next().map(String::as_str) != Some("report") {
        return Err(Fail::Usage("the only command is `report`".into()));
    }
    let (mut core, mut elf, mut virt, mut trace) = (None, None, None, None);
    while let Some(a) = it.next() {
        let slot = match a.as_str() {
            "--core" => &mut core,
            "--elf" => &mut elf,
            "--virt" => &mut virt,
            "--trace" => &mut trace,
            _ => return Err(Fail::Usage(format!("unknown argument {a:?}"))),
        };
        let v = it
            .next()
            .ok_or_else(|| Fail::Usage(format!("{a} needs a value")))?;
        *slot = Some(v.clone());
    }
    Ok(Opts {
        core: core.ok_or_else(|| Fail::Usage("--core is required".into()))?,
        elf: elf.ok_or_else(|| Fail::Usage("--elf is required".into()))?,
        virt,
        trace,
    })
}

/// Each CPU's registers from its `NT_PRSTATUS`: QEMU writes one per vCPU,
/// `pr_pid` the vCPU's index plus one, which is the APIC id under QEMU's
/// default topology.
fn prstatus(store: &Store) -> Vec<PrRegs> {
    store
        .notes
        .iter()
        .flat_map(|n| Notes::new(n))
        .filter(|n| n.name == b"CORE" && n.kind == NT_PRSTATUS)
        .filter_map(|n| prstatus_regs(n.desc).ok())
        .collect()
}

/// Where a CPU's walk starts: its crash slot when set (a stopped CPU, the
/// dump owner), else its `NT_PRSTATUS`.
fn start_regs(c: &CpuView, i: usize, regs: &[PrRegs]) -> Option<(u64, u64, u64, &'static str)> {
    if let Some((rip, rsp, rbp)) = c.slot() {
        return Some((rip, rsp, rbp, "slot"));
    }
    regs.iter()
        .find(|r| r.pid.checked_sub(1) == Some(c.apic_id))
        .or_else(|| regs.get(i))
        .map(|r| (r.rip, r.rsp, r.rbp, "prstatus"))
}

fn tid_of(k: &Kernel<'_, Store, Arch>, tcb: u64) -> String {
    if tcb == 0 {
        return "none".into();
    }
    k.thread(tcb)
        .map_or_else(|_| "?".into(), |t| t.id.to_string())
}

/// What `report` produced: the text, and the files it wrote.
struct Output {
    text: String,
}

fn report(store: &Store, elf_bytes: &[u8], opts: &Opts) -> Result<Output, Fail> {
    let elf = KernelElf::parse(elf_bytes).map_err(|e| other("elf", e))?;
    let elf_id = elf.build_id().map_err(|e| Fail::Other(e.as_str().into()))?;
    let notes: Vec<_> = store.notes.iter().flat_map(|n| Notes::new(n)).collect();
    let vi = Vmcoreinfo::find(notes.iter().copied()).map_err(|_| Fail::NoNote)?;
    let core_id = vi.build_id().map_err(|e| other("VMCOREINFO BUILD-ID", e))?;
    if check_build_id(&core_id, &elf_id).is_err() {
        return Err(Fail::Mismatch {
            core: core_id.to_string(),
            elf: elf_id.to_string(),
        });
    }
    let roots: Roots = vi.roots().map_err(|e| other("VMCOREINFO", e))?;
    let k: Kernel<'_, Store, Arch> =
        Kernel::new(store, roots.pgt_root, roots.pgt_levels).map_err(|e| other("core", e))?;
    let syms = Symbols::new(&elf)?;
    let regs = prstatus(store);
    let in_text = |a: u64| elf.in_text(a);

    // Each CPU and its backtrace.
    let mut cpus = Vec::new();
    for i in 0..roots.cpus_len {
        cpus.push(
            k.cpu(roots.cpus, i)
                .map_err(|e| other(&format!("cpu {i}"), e))?,
        );
    }
    let mut frames: Vec<Vec<u64>> = Vec::new();
    for (i, c) in cpus.iter().enumerate() {
        let mut bt = [0u64; BT_MAX];
        let n = match start_regs(c, i, &regs) {
            Some((rip, _, rbp, _)) => k.unwind(rip, rbp, in_text, &mut bt),
            None => 0,
        };
        frames.push(bt[..n].to_vec());
    }

    // The panic line, when the kernel recorded one.
    let (panic, panic_note) = match syms.find(PANIC_LINE_SYM) {
        Ok(at) => match k.panic_line(at) {
            Ok(p) => (p, None),
            Err(e) => (None, Some(format!("panic line: {e}"))),
        },
        Err(e) => (None, Some(format!("panic line: {e}"))),
    };
    let panic_text = panic
        .as_ref()
        .map(|(_, t)| String::from_utf8_lossy(t.as_bytes()).into_owned());
    let triples: Vec<(u32, u64, u64)> = cpus
        .iter()
        .map(|c| (c.cpu_id, c.cur_tcb, c.idle_tcb))
        .collect();
    let sig_cpu = pick_cpu(panic.as_ref().map(|(c, _)| *c), &triples);
    let sig_frames = cpus
        .iter()
        .position(|c| c.cpu_id == sig_cpu)
        .and_then(|i| frames.get(i))
        .cloned()
        .unwrap_or_default();

    let mut out = String::new();
    write_signature(
        &mut out,
        panic_text.as_deref(),
        sig_frames.iter().map(|&a| syms.name(a).map(|(n, _)| n)),
    )
    .map_err(|e| other("report", e))?;
    out.push('\n');
    let (ram, nsegs) = store.ram();
    put!(out, "core: {ram} RAM in {nsegs} segments");
    put!(out, "build-id: {core_id}");
    match (&panic, &panic_note) {
        (Some((c, _)), _) => {
            put!(
                out,
                "panic: cpu {c}: {}",
                panic_text.as_deref().unwrap_or("")
            );
        }
        (None, Some(n)) => {
            put!(out, "panic: none ({n})");
        }
        (None, None) => {
            put!(out, "panic: none");
        }
    }
    for (i, c) in cpus.iter().enumerate() {
        let runq: Vec<String> = (0..c.runq.len.min(64))
            .map(|q| {
                k.runq_id(&c.runq, q)
                    .map_or("?".into(), |id| id.to_string())
            })
            .collect();
        let src = start_regs(c, i, &regs).map_or("none", |r| r.3);
        let stop = c.stop_how().map_or("running".to_string(), |h| {
            format!("stopped ({})", h.as_str())
        });
        put!(
            out,
            "cpu {} apic {} current {} idle {} runq [{}] regs {src} {stop}",
            c.cpu_id,
            c.apic_id,
            tid_of(&k, c.cur_tcb),
            tid_of(&k, c.idle_tcb),
            runq.join(", ")
        );
        for (n, &a) in frames[i].iter().enumerate() {
            match syms.name(a) {
                Some((name, 0)) => {
                    put!(out, "  #{n} 0x{a:016x} {name}");
                }
                Some((name, off)) => {
                    put!(out, "  #{n} 0x{a:016x} {name}+0x{off:x}");
                }
                None => {
                    put!(out, "  #{n} 0x{a:016x} ?");
                }
            }
        }
    }

    // Every thread of the TCB table.
    for i in 0..roots.tcbs_len {
        let at = k
            .tcb_slot(roots.tcbs, i)
            .map_err(|e| other("tcb table", e))?;
        if at == 0 {
            continue;
        }
        match k.thread(at) {
            Ok(t) => {
                let arg = match t.state {
                    2 => format!(" (deadline {} ns)", t.state_arg),
                    3 => format!(" (wq 0x{:x})", t.state_arg),
                    _ => String::new(),
                };
                put!(
                    out,
                    "thread {} {}{arg} cpu {} pid {} rip=0x{:x} rsp=0x{:x} rbp=0x{:x} rbx=0x{:x} r12=0x{:x} r13=0x{:x} r14=0x{:x} r15=0x{:x} rflags=0x{:x}",
                    t.id,
                    state_name(t.state),
                    t.cpu,
                    t.pid,
                    t.rip,
                    t.rsp,
                    t.rbp,
                    t.rbx,
                    t.r12,
                    t.r13,
                    t.r14,
                    t.r15,
                    t.rflags
                );
            }
            Err(e) => {
                put!(out, "thread ? at 0x{at:x}: {e}");
            }
        }
    }

    // The log tail.
    match k.log_header(roots.log) {
        Ok(h) => {
            let take = h.len.min(LOG_TAIL as u64);
            put!(out, "log: last {take} of {} ({} dropped)", h.len, h.dropped);
            let mut err = None;
            let r = k.log_tail(roots.log, &h, |r| {
                if r.torn {
                    put!(out, "  [{:>8}] cpu{} <torn>", r.timestamp, r.cpu);
                } else {
                    let level = r.level.map_or("?", |l| l.as_str());
                    put!(
                        out,
                        "  [{:>8}] cpu{} {level} {}",
                        r.timestamp,
                        r.cpu,
                        String::from_utf8_lossy(r.text())
                    );
                }
            });
            if let Err(e) = r {
                err = Some(e);
            }
            if let Some(e) = err {
                put!(out, "  <log read failed: {e}>");
            }
        }
        Err(e) => {
            put!(out, "log: none ({e})");
        }
    }

    // The flight recorder.
    match syms.find(TRACE_SYM).map_err(Fail::Other).and_then(|at| {
        k.trace_clock(at)
            .map(|c| (at, c))
            .map_err(|e| other("trace", e))
    }) {
        Ok((at, clock)) => {
            match clock.order() {
                trace::Order::Global => {
                    put!(out, "trace: order global");
                }
                trace::Order::PerCpu(why) => {
                    put!(out, "trace: order per-cpu ({why})");
                }
            }
            if let Some(path) = &opts.trace {
                let mut bufs = Vec::new();
                for c in &cpus {
                    let mut b = Box::new([RecordData::default(); RECORDS_PER_CPU]);
                    let n = k
                        .trace_ring(at, c.cpu_id, &mut b)
                        .map_err(|e| other(&format!("trace ring {}", c.cpu_id), e))?;
                    bufs.push((c.cpu_id, b, n));
                }
                let rings: Vec<(u32, &[RecordData])> =
                    bufs.iter().map(|(c, b, n)| (*c, &b[..*n])).collect();
                let mut json = String::new();
                trace::export_chrome(&rings, &clock, &mut json)
                    .map_err(|e| other("trace export", e))?;
                std::fs::write(path, json).map_err(|e| other(path, e))?;
                let events: usize = rings.iter().map(|(_, r)| r.len()).sum();
                put!(out, "trace: {events} events to {path}");
            }
        }
        Err(e) => {
            put!(
                out,
                "trace: none ({})",
                e.message().trim_start_matches("vmcore: ")
            );
        }
    }

    // The virtually addressed core.
    if let Some(path) = &opts.virt {
        let f = File::create(path).map_err(|e| other(path, e))?;
        let mut w = BufWriter::new(f);
        let mut notes: Vec<&[u8]> = store.notes.iter().map(Vec::as_slice).collect();
        notes.retain(|n| !n.is_empty());
        let st =
            walk::write_virtual_core(&k, &notes, |b| w.write_all(b).map_err(|_| VmError::Write))
                .map_err(|e| other(path, e))?;
        w.flush().map_err(|e| other(path, e))?;
        put!(
            out,
            "virt: {} segments, {} bytes to {path}",
            st.segments,
            st.bytes
        );
    }
    Ok(Output { text: out })
}

fn run(args: &[String]) -> Result<Output, Fail> {
    let opts = parse_args(args)?;
    let elf_bytes = std::fs::read(&opts.elf).map_err(|e| other(&opts.elf, e))?;
    let store = if opts.core == "-" {
        let mut src = Source {
            r: BufReader::with_capacity(1 << 20, io::stdin().lock()),
            pos: 0,
            seek: None,
        };
        load_core(&mut src)?
    } else {
        let f = File::open(&opts.core).map_err(|e| other(&opts.core, e))?;
        let mut src = Source {
            r: BufReader::with_capacity(1 << 20, f),
            pos: 0,
            seek: Some(|r: &mut BufReader<File>, to| r.seek(SeekFrom::Start(to)).map(|_| ())),
        };
        load_core(&mut src)?
    };
    report(&store, &elf_bytes, &opts)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(o) => {
            let mut stdout = io::stdout().lock();
            if stdout.write_all(o.text.as_bytes()).is_err() {
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Err(f) => {
            eprintln!("{}", f.message());
            ExitCode::from(f.code())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibeos::log::vmcore::synth::{SymDef, Synth, kernel_elf};
    use vibeos::log::vmcoreinfo::Info;

    const KBASE: u64 = 0xFFFF_FFFF_8000_0000;
    const ID: [u8; 20] = [7; 20];

    fn from_bytes(b: &[u8], seekable: bool) -> Result<Store, Fail> {
        let mut src = Source {
            r: io::Cursor::new(b.to_vec()),
            pos: 0,
            seek: if seekable {
                Some(|r: &mut io::Cursor<Vec<u8>>, to| r.seek(SeekFrom::Start(to)).map(|_| ()))
            } else {
                None
            },
        };
        load_core(&mut src)
    }

    #[test]
    fn sparse_store_skips_zero_pages() {
        let mut s = Synth::new(&[(0, 4 << 20), (0x1_0000_0000, 1 << 20)]);
        s.write_phys(0x3000, b"hello");
        s.write_phys(0x1_0000_0FF8, &[1; 16]);
        let st = from_bytes(&s.build(), false).unwrap();
        // One page for `hello`, two for the run across a page boundary; the
        // empty page-table root is all zero, so it is not kept.
        assert_eq!(st.pages.len(), 3);
        assert_eq!(st.ram(), ((4 << 20) + (1 << 20), 2));
        let mut b = [0u8; 5];
        assert!(st.read(0x3000, &mut b));
        assert_eq!(&b, b"hello");
        let mut z = [9u8; 8];
        assert!(st.read(0x20_0000, &mut z));
        assert_eq!(z, [0; 8]);
        let mut x = [0u8; 16];
        assert!(st.read(0x1_0000_0FF8, &mut x));
        assert_eq!(x, [1; 16]);
        // A hole between the segments reads as absent.
        assert!(!st.read(0x80_0000, &mut z));
        assert!(!st.read((4 << 20) - 4, &mut z));
    }

    #[test]
    fn stdin_requires_offset_order() {
        let mut s = Synth::new(&[(0, 1 << 20), (0x20_0000, 1 << 20)]);
        s.write_phys(0x20_0010, b"second");
        let mut core = s.build();
        // Swap the two PT_LOAD headers: offsets no longer rise in header order.
        let at = |i: usize| EHDR_SIZE + i * PHDR_SIZE;
        let (a, b) = (at(1), at(2));
        let first: Vec<u8> = core[a..a + PHDR_SIZE].to_vec();
        core.copy_within(b..b + PHDR_SIZE, a);
        core[b..b + PHDR_SIZE].copy_from_slice(&first);
        match from_bytes(&core, false) {
            Err(Fail::Other(m)) => {
                assert!(m.contains("stdin needs segments in file order"), "{m}");
            }
            Ok(_) => panic!("an out-of-order core was read from a stream"),
            Err(f) => panic!("{f:?}"),
        }
        // A file can seek back.
        let st = from_bytes(&core, true).unwrap();
        let mut b = [0u8; 6];
        assert!(st.read(0x20_0010, &mut b));
        assert_eq!(&b, b"second");
    }

    #[test]
    fn demangled_names_have_no_hash() {
        assert_eq!(
            demangle(b"_ZN6vibeos3smp9hang_test4hold17h0123456789abcdefE"),
            "vibeos::smp::hang_test::hold"
        );
        assert_eq!(
            demangle(b"_RNvNtNtCs1234_6vibeos3smp9hang_test4hold"),
            "vibeos::smp::hang_test::hold"
        );
        assert_eq!(demangle(b"VIBEOS_TRACE"), "VIBEOS_TRACE");
        assert_eq!(demangle(b"rust_begin_unwind"), "rust_begin_unwind");
    }

    /// A core of one CPU spinning in `hold` < `arm` < `boot_rest`, and its ELF.
    fn hang_core(id: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut s = Synth::new(&[(0, 16 << 20)]);
        s.map(KBASE, 0x20_0000, 0x20_0000, 0);
        // Text at KBASE; a stack at KBASE + 0x10_0000: rbp -> [next, ret].
        let stack = KBASE + 0x10_0000;
        let f0 = stack + 0x100;
        let f1 = stack + 0x140;
        let pa = |va: u64| va - KBASE + 0x20_0000;
        s.write_phys(pa(f0), &f1.to_le_bytes());
        s.write_phys(pa(f0) + 8, &(KBASE + 0x48).to_le_bytes());
        s.write_phys(pa(f1), &0u64.to_le_bytes());
        s.write_phys(pa(f1) + 8, &(KBASE + 0x88).to_le_bytes());
        // One PerCpu: current thread 5, idle thread 6, its remote view
        // zeroed (APIC id 0, no crash slot), at their real offsets.
        use core::mem::offset_of;
        use vibeos::per_cpu::PerCpu;
        use vibeos::thread::Tcb;
        let (cpus, remote, tcbs) = (KBASE + 0x16_0000, KBASE + 0x15_0000, KBASE + 0x17_0000);
        let (cur, idle) = (KBASE + 0x17_1000, KBASE + 0x17_2000);
        s.write_phys(
            pa(cpus + offset_of!(PerCpu, current) as u64),
            &cur.to_le_bytes(),
        );
        s.write_phys(
            pa(cpus + offset_of!(PerCpu, idle) as u64),
            &idle.to_le_bytes(),
        );
        s.write_phys(
            pa(cpus + offset_of!(PerCpu, remote) as u64),
            &remote.to_le_bytes(),
        );
        s.write_phys(pa(tcbs), &cur.to_le_bytes());
        s.write_phys(pa(tcbs + 8), &idle.to_le_bytes());
        for (t, id, state) in [(cur, 5u32, 1u32), (idle, 6, 0)] {
            s.write_phys(pa(t + offset_of!(Tcb, id) as u64), &id.to_le_bytes());
            s.write_phys(pa(t + offset_of!(Tcb, state) as u64), &state.to_le_bytes());
        }
        let info = Info {
            osrelease: "0.8.0",
            build_id: id,
            page_size: 4096,
            pgt_root: s.root,
            pgt_levels: 4,
            log: KBASE + 0x18_0000,
            tcbs,
            tcbs_len: 2,
            cpus,
            cpus_len: 1,
        };
        s.vmcoreinfo(&info);
        s.cpu(1, KBASE + 0x14, f0 - 8, f0);
        let syms = [
            SymDef {
                name: "_ZN6vibeos3smp9hang_test4hold17h0000000000000001E",
                value: KBASE + 0x10,
                size: 0x20,
                kind: STT_FUNC,
            },
            SymDef {
                name: "_ZN6vibeos3smp9hang_test3arm17h0000000000000002E",
                value: KBASE + 0x40,
                size: 0x20,
                kind: STT_FUNC,
            },
            SymDef {
                name: "_ZN6vibeos9boot_rest17h0000000000000003E",
                value: KBASE + 0x80,
                size: 0x20,
                kind: STT_FUNC,
            },
        ];
        (s.build(), kernel_elf(&ID, KBASE, &[0xCC; 0x100], &syms))
    }

    fn opts() -> Opts {
        Opts {
            core: "-".into(),
            elf: "kernel.elf".into(),
            virt: None,
            trace: None,
        }
    }

    #[test]
    fn report_lines_in_order() {
        let (core, elf) = hang_core(&ID);
        let st = from_bytes(&core, false).unwrap();
        let out = report(&st, &elf, &opts()).unwrap().text;
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            "sig: timeout @ vibeos::smp::hang_test::hold < vibeos::smp::hang_test::arm < vibeos::boot_rest",
            "{out}"
        );
        assert!(
            lines[1].starts_with("core: 16777216 RAM in 1 segments"),
            "{out}"
        );
        assert!(lines[2].starts_with("build-id: 0707"), "{out}");
        assert!(lines[3].starts_with("panic: none"), "{out}");
        let cpu = lines.iter().position(|l| l.starts_with("cpu 0 ")).unwrap();
        assert!(
            lines[cpu].starts_with("cpu 0 apic 0 current 5 idle 6 runq [] regs prstatus running"),
            "{out}"
        );
        assert_eq!(
            lines[cpu + 1],
            "  #0 0xffffffff80000014 vibeos::smp::hang_test::hold+0x4"
        );
        assert_eq!(
            lines[cpu + 3],
            "  #2 0xffffffff80000088 vibeos::boot_rest+0x8"
        );
        let th = lines
            .iter()
            .position(|l| l.starts_with("thread 5 running"))
            .unwrap();
        assert!(lines[th + 1].starts_with("thread 6 ready"), "{out}");
        assert!(cpu < th, "{out}");
        let log = lines.iter().position(|l| l.starts_with("log: ")).unwrap();
        assert!(th < log, "{out}");
        let trace = lines.iter().position(|l| l.starts_with("trace: ")).unwrap();
        assert!(log < trace, "{out}");
        // A mismatched build id and a core without the note.
        let (core2, _) = hang_core(&[8; 20]);
        let st2 = from_bytes(&core2, false).unwrap();
        match report(&st2, &elf, &opts()) {
            Err(f @ Fail::Mismatch { .. }) => {
                assert_eq!(f.code(), 3);
                assert_eq!(
                    f.message(),
                    format!(
                        "vmcore: BUILD-ID mismatch: core {} elf {}",
                        "08".repeat(20),
                        "07".repeat(20)
                    )
                );
            }
            _ => panic!("mismatch not refused"),
        }
        let bare = Synth::new(&[(0, 1 << 20)]).build();
        let st3 = from_bytes(&bare, false).unwrap();
        let f = report(&st3, &elf, &opts()).err().unwrap();
        assert_eq!(
            (f.code(), f.message()),
            (4, "vmcore: no VMCOREINFO note".to_string())
        );
        let noid = kernel_elf(&[], KBASE, &[0xCC; 0x100], &[]);
        let f = report(&st, &noid, &opts()).err().unwrap();
        assert_eq!(
            (f.code(), f.message()),
            (1, "vmcore: no build-id note in ELF".to_string())
        );
    }

    #[test]
    fn usage_errors_exit_2() {
        for a in [
            &[][..],
            &["report"],
            &["dump", "--core", "-"],
            &["report", "--core"],
        ] {
            let args: Vec<String> = a.iter().map(|s| s.to_string()).collect();
            assert_eq!(parse_args(&args).err().map(|f| f.code()), Some(2), "{a:?}");
        }
    }
}
