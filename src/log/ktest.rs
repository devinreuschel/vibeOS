//! In-guest tests for log (kernel_tests only). Rows: [`TESTS`].

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::log::Level;
use vibeos::log::backtrace::WalkEnd;
use vibeos::paging::USER_MAP_END;

use vibeos::log::trace::{self, Event, RecordData};
use vibeos::vectors;

use crate::block::{block_init, blockdev_init};
use crate::ktest::user::{self, DEFAULT, Image, user_code};
use crate::ktest::{Outcome, Test, spin_until_ns, test};
use crate::log::trace_init::VIBEOS_TRACE;
use crate::{ipi_init, per_cpu_init, thread_init, time_init};

/// The runtime level is what `loglevel=` set at boot (BOOT.md §3.2), or
/// the default without one. It runs first in its group, before the tests
/// that set the level and restore it.
pub(crate) fn test_log_boot_level() -> Outcome {
    use vibeos::boot::cmdline::Escaped;
    use vibeos::log::{DEFAULT_RUNTIME_MAX, level_from_loglevel};
    let arg = crate::boot::cmdline().get("loglevel");
    let want = arg
        .and_then(level_from_loglevel)
        .unwrap_or(DEFAULT_RUNTIME_MAX);
    let got = crate::log_init::max_level();
    if got != want {
        return crate::fail_fmt!(
            "runtime level {} with loglevel={}, want {}",
            got.as_str(),
            Escaped(arg.unwrap_or(b"<absent>")),
            want.as_str()
        );
    }
    Outcome::Ok
}

pub(crate) fn test_log_boot_captured() -> Outcome {
    if !crate::log_init::contains_msg("serial online") {
        return Outcome::Fail("serial online missing from ring");
    }
    if !crate::log_init::contains_msg("smp: done") {
        return Outcome::Fail("smp: done missing from ring");
    }
    if !crate::log_init::contains_msg("console ok") {
        return Outcome::Fail("console ok missing from ring");
    }
    Outcome::Ok
}

pub(crate) fn test_log_runtime_filter() -> Outcome {
    use vibeos::log::Level;
    let old = crate::log_init::max_level();
    crate::log_init::set_max_level(Level::Error);
    crate::klog!(Level::Debug, "vibeOS: ktest: log-filter-hidden-xyz");
    if crate::log_init::contains_msg("log-filter-hidden-xyz") {
        crate::log_init::set_max_level(old);
        return Outcome::Fail("debug stored at error max");
    }
    crate::log_init::set_max_level(Level::Trace);
    crate::klog!(Level::Debug, "vibeOS: ktest: log-filter-visible-xyz");
    let ok = crate::log_init::contains_msg("log-filter-visible-xyz");
    crate::log_init::set_max_level(old);
    if ok {
        Outcome::Ok
    } else {
        Outcome::Fail("debug not stored after raising max")
    }
}

pub(crate) fn test_log_emit_roundtrip() -> Outcome {
    crate::klog!(vibeos::log::Level::Info, "vibeOS: ktest: log-roundtrip-abc");
    if crate::log_init::contains_msg("log-roundtrip-abc") {
        Outcome::Ok
    } else {
        Outcome::Fail("info record missing")
    }
}

pub(crate) fn test_log_dmesg_no_recapture() -> Outcome {
    let n = crate::log_init::ring_len();
    crate::log_init::dmesg(Some(vibeos::log::Level::Info));
    if crate::log_init::ring_len() != n {
        return Outcome::Fail("dmesg recaptured into ring");
    }
    if crate::log_init::contains_msg("vibeOS: dmesg:") {
        return Outcome::Fail("dmesg line stored");
    }
    Outcome::Ok
}

/// Formats as `outer`, logging a record of its own while it does.
struct LogsWhileFormatting;

impl core::fmt::Display for LogsWhileFormatting {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        crate::klog!(vibeos::log::Level::Info, "vibeOS: ktest: log-reentry-inner");
        f.write_str("outer")
    }
}

/// A `klog!` whose argument's `Display` calls `klog!` drops the inner
/// record, and `reentry_drops` counts it (DESIGN §2.5).
pub(crate) fn test_log_reentry_drop_counted() -> Outcome {
    let before = crate::log_init::reentry_drops();
    crate::klog!(
        vibeos::log::Level::Info,
        "vibeOS: ktest: log-reentry-{}",
        LogsWhileFormatting
    );
    let after = crate::log_init::reentry_drops();
    if after.wrapping_sub(before) != 1 {
        return crate::fail_fmt!("reentry_drops {before} -> {after}, want +1");
    }
    if !crate::log_init::contains_msg("log-reentry-outer") {
        return Outcome::Fail("outer record missing from ring");
    }
    if crate::log_init::contains_msg("log-reentry-inner") {
        return Outcome::Fail("inner record stored");
    }
    Outcome::Ok
}

// ---------------------------------------------------------------------------
// serial_lines_whole (ROADMAP §10.2, F138)

/// A fixed 36-byte tail, so each line reaches the writer as several pieces.
const PAD: &str = "0123456789abcdefghijklmnopqrstuvwxyz";
/// The numbered lines CPU 0 prints; `run_ktest.py` finds each whole.
const WHOLE_LINES: u32 = 1000;
/// The pause between two noise lines, so CPU 0 still gets the TX lock.
const NOISE_PAUSE_NS: u64 = 20_000;
const NOISE_WAIT_NS: u64 = 2_000_000_000;
const WHOLE_WAIT_NS: u64 = 60_000_000_000;

static NOISE_STOP: AtomicBool = AtomicBool::new(false);
/// Noise threads that have printed their first line.
static NOISE_STARTED: AtomicU32 = AtomicU32::new(0);
/// Noise threads that have seen `NOISE_STOP` and are about to exit.
static NOISE_EXITED: AtomicU32 = AtomicU32::new(0);
/// Set by the CPU-0 printer thread once its last numbered line is out.
static WHOLE_DONE: AtomicBool = AtomicBool::new(false);

fn this_cpu() -> u32 {
    thread_init::current_cpu()
}

fn serial_noise() {
    let cpu = this_cpu();
    let mut n = 0u64;
    while !NOISE_STOP.load(Ordering::Acquire) {
        crate::marker!("vibeOS: ktest: serial noise cpu{} {} {}", cpu, n, PAD);
        crate::klog!(
            Level::Info,
            "vibeOS: ktest: serial noise klog cpu{} {} {}",
            cpu,
            n,
            PAD
        );
        if n == 0 {
            NOISE_STARTED.fetch_add(1, Ordering::AcqRel);
        }
        n = n.wrapping_add(1);
        let t0 = time_init::now_ns();
        while time_init::now_ns().saturating_sub(t0) < NOISE_PAUSE_NS {
            core::hint::spin_loop();
        }
    }
    // Release: the test reads the count with Acquire before it returns.
    NOISE_EXITED.fetch_add(1, Ordering::Release);
}

fn print_whole() {
    let mut i = 0u32;
    while i < WHOLE_LINES {
        crate::marker!(
            "vibeOS: ktest: serial whole {} of {} {}",
            i,
            WHOLE_LINES,
            PAD
        );
        i += 1;
    }
}

fn whole_printer() {
    print_whole();
    WHOLE_DONE.store(true, Ordering::Release);
}

/// Every AP prints formatted lines in a loop while CPU 0 prints 1,000
/// numbered markers; `run_ktest.py`'s `_check_serial_whole` finds all
/// 1,000 whole (ROADMAP §10.2, F138).
pub(crate) fn test_serial_lines_whole() -> Outcome {
    let online = per_cpu_init::online_mask();
    let aps: u32 = (1u32..64).filter(|&c| online & (1u64 << c) != 0).count() as u32;
    if aps == 0 {
        return Outcome::Skip("no AP");
    }
    NOISE_STOP.store(false, Ordering::Release);
    NOISE_STARTED.store(0, Ordering::Release);
    NOISE_EXITED.store(0, Ordering::Release);
    WHOLE_DONE.store(false, Ordering::Release);
    let mut spawned = 0u32;
    for cpu in (1u32..64).filter(|&c| online & (1u64 << c) != 0) {
        if thread_init::spawn_opts(
            "serial-noise",
            serial_noise,
            thread_init::SpawnOpts {
                stack_pages: DEFAULT_STACK_PAGES,
                cpu: Some(cpu),
            },
        )
        .is_err()
        {
            break;
        }
        spawned += 1;
    }
    let outcome = if spawned != aps {
        Outcome::Fail("spawn")
    } else if !spin_until_ns(
        || NOISE_STARTED.load(Ordering::Acquire) == aps,
        NOISE_WAIT_NS,
    ) {
        Outcome::Fail("an AP printed no noise line")
    } else if this_cpu() == 0 {
        print_whole();
        Outcome::Ok
    } else if thread_init::spawn_opts(
        "serial-whole",
        whole_printer,
        thread_init::SpawnOpts {
            stack_pages: DEFAULT_STACK_PAGES,
            cpu: Some(0),
        },
    )
    .is_err()
    {
        Outcome::Fail("spawn printer")
    } else if !spin_until_ns(|| WHOLE_DONE.load(Ordering::Acquire), WHOLE_WAIT_NS) {
        Outcome::Fail("printer did not finish")
    } else {
        Outcome::Ok
    };
    NOISE_STOP.store(true, Ordering::Release);
    if !spin_until_ns(
        || NOISE_EXITED.load(Ordering::Acquire) == spawned,
        NOISE_WAIT_NS,
    ) {
        return Outcome::Fail("a noise thread did not exit");
    }
    outcome
}

// ---------------------------------------------------------------------------
// serial_frame (ROADMAP §10.2, DESIGN §2.6)

/// A kernel line with a `\n`, a `\r` and a frame byte inside, then user
/// console bytes that leave their line open, then a kernel line.
/// `run_ktest.py`'s `_check_serial_frame` finds the first framed with each
/// of the three as `?`, the user bytes unframed with the frame byte as `?`,
/// and the last framed on a line of its own.
pub(crate) fn test_serial_frame() -> Outcome {
    crate::marker!("vibeOS: ktest: serial frame a\nb\rc\x1ed");
    crate::console_init::write(b"\x1eserial-frame open");
    crate::marker!("vibeOS: ktest: serial frame after open");
    Outcome::Ok
}

/// Each CPU's ring head: the marks a flight-recorder test searches after.
fn heads() -> [u64; trace::MAX_CPUS] {
    let mut out = [0u64; trace::MAX_CPUS];
    for (c, h) in out.iter_mut().enumerate() {
        if let Some(r) = VIBEOS_TRACE.ring(c as u32) {
            *h = r.head();
        }
    }
    out
}

/// The first valid record, on any CPU, past that CPU's mark in `marks`,
/// for which `pred(cpu, record)` holds.
fn find_since(
    marks: &[u64; trace::MAX_CPUS],
    pred: impl Fn(u32, &RecordData) -> bool,
) -> Option<(u32, RecordData)> {
    for (c, mark) in marks.iter().enumerate() {
        let cpu = c as u32;
        let Some(r) = VIBEOS_TRACE.ring(cpu) else {
            continue;
        };
        let head = r.head();
        let start = (*mark).max(head.saturating_sub(trace::RECORDS_PER_CPU as u64));
        for pos in start..head {
            if let Some(d) = r.get(pos)
                && pred(cpu, &d)
            {
                return Some((cpu, d));
            }
        }
    }
    None
}

fn trace_noop(_: *mut ()) {}

/// L1415: after a call-function IPI to every other online CPU, every
/// online CPU's ring holds records, and ring `c` holds only CPU `c`'s.
pub(crate) fn test_trace_ring_own_cpu() -> Outcome {
    if !VIBEOS_TRACE.is_live() {
        return Outcome::Fail("trace not live");
    }
    let online = per_cpu_init::online_mask();
    ipi_init::call_mask(online, trace_noop, core::ptr::null_mut(), true);
    for c in 0..trace::MAX_CPUS as u32 {
        if online & (1u64 << c) == 0 {
            continue;
        }
        let Some(r) = VIBEOS_TRACE.ring(c) else {
            return crate::fail_fmt!("no ring for cpu {c}");
        };
        let head = r.head();
        if head == 0 {
            return crate::fail_fmt!("cpu {c}: ring empty");
        }
        let start = head.saturating_sub(trace::RECORDS_PER_CPU as u64);
        let mut valid = 0u32;
        for pos in start..head {
            if let Some(d) = r.get(pos) {
                valid += 1;
                if d.cpu != c {
                    return crate::fail_fmt!("ring {c} pos {pos}: record of cpu {}", d.cpu);
                }
            }
        }
        if valid == 0 {
            return crate::fail_fmt!("cpu {c}: no valid record");
        }
    }
    Outcome::Ok
}

// getpid, then a store to 0x10, which faults: `SIGSEGV` ends it.
user_code!(
    TRACE_GETPID_FAULT,
    "
    mov eax, 39
    syscall
    mov ecx, 0x10
    mov qword ptr [rcx], rax
    ud2
    "
);

/// L1416: each tracepoint the kernel has fires, with the arguments its
/// event table names.
pub(crate) fn test_trace_tracepoints_fire() -> Outcome {
    let Some(ap) = crate::ktest::second_cpu() else {
        return Outcome::Skip("needs a second cpu");
    };
    let me = u64::from(thread_init::current_id().0);
    let ev = |r: &RecordData, e: Event| r.event == e.as_u32();

    let marks = heads();
    if user::run(&Image::Code(TRACE_GETPID_FAULT, DEFAULT), &["trace_fault"]).is_err() {
        return Outcome::Fail("user program did not run");
    }
    if find_since(&marks, |_, r| ev(r, Event::SyscallEnter) && r.a == 39).is_none() {
        return Outcome::Fail("no SyscallEnter for getpid");
    }
    if find_since(&marks, |_, r| ev(r, Event::SyscallExit) && r.a == 39).is_none() {
        return Outcome::Fail("no SyscallExit for getpid");
    }
    if find_since(&marks, |_, r| ev(r, Event::PageFault) && r.a == 0x10).is_none() {
        return Outcome::Fail("no PageFault at 0x10");
    }

    let marks = heads();
    thread_init::sleep_ms(2);
    if find_since(&marks, |_, r| ev(r, Event::Wake) && r.a == me).is_none() {
        return Outcome::Fail("no Wake of the test thread");
    }
    if find_since(&marks, |_, r| {
        ev(r, Event::Switch) && (r.a == me || r.b == me)
    })
    .is_none()
    {
        return Outcome::Fail("no Switch naming the test thread");
    }
    if find_since(&marks, |_, r| ev(r, Event::IrqEnter) && r.a >= 32).is_none() {
        return Outcome::Fail("no IrqEnter");
    }
    if find_since(&marks, |_, r| ev(r, Event::IrqExit) && r.a >= 32).is_none() {
        return Outcome::Fail("no IrqExit");
    }

    let marks = heads();
    let call = u64::from(vectors::IPI_CALL);
    ipi_init::call_cpu(ap, trace_noop, core::ptr::null_mut(), true);
    if find_since(&marks, |_, r| {
        ev(r, Event::IpiSend) && r.a == call && r.b == u64::from(ap)
    })
    .is_none()
    {
        return Outcome::Fail("no IpiSend 0xFB to the second cpu");
    }
    if find_since(&marks, |c, r| {
        ev(r, Event::IpiAck) && r.a == call && c == ap
    })
    .is_none()
    {
        return Outcome::Fail("no IpiAck 0xFB on the second cpu");
    }

    let Some(d) = blockdev_init::lookup(block_init::RAM0_NAME.as_bytes()) else {
        return Outcome::Fail("no ram0");
    };
    let marks = heads();
    let mut buf = [0u8; 512];
    if d.read_dev(0, &mut buf).is_err() {
        return Outcome::Fail("ram0 read");
    }
    let Some((_, sub)) = find_since(&marks, |_, r| ev(r, Event::BlockSubmit) && r.b == 0) else {
        return Outcome::Fail("no BlockSubmit of lba 0");
    };
    if find_since(&marks, |_, r| {
        ev(r, Event::BlockComplete) && r.a == sub.a && r.b == 0
    })
    .is_none()
    {
        return Outcome::Fail("no BlockComplete for the submit");
    }
    Outcome::Ok
}

/// A `NUMBER`/`LENGTH` (decimal) or `SYMBOL` (hex) value of the note.
fn note_u64(desc: &[u8], key: &str, radix: u32) -> Option<u64> {
    let v = vibeos::log::vmcoreinfo::get(desc, key)?;
    u64::from_str_radix(core::str::from_utf8(v).ok()?, radix).ok()
}

/// The note `vmcoreinfo_init::publish` built at boot parses, carries this
/// kernel's build id, page-table root and the three table roots, reaches
/// the tables through the portable types, and QEMU's `vmcoreinfo` device
/// holds its address (ROADMAP §10.7, docs/VMCOREINFO.md).
pub(crate) fn test_vmcoreinfo_published() -> Outcome {
    use crate::arch::current::Arch;
    use crate::boot::fw_cfg_init;
    use crate::log::vmcoreinfo_init::{self, DeviceState};
    use vibeos::fmt_util::StackBuf;
    use vibeos::log::KernelLog;
    use vibeos::log::vmcoreinfo::{self, FORMAT_ELF, FW_CFG_FILE, FwCfgVmcoreinfo};
    use vibeos::per_cpu::PerCpu;
    use vibeos::thread::{TcbSlot, ThreadId};

    let Some((note, pa, state)) = vmcoreinfo_init::published() else {
        return Outcome::Fail("no note published");
    };
    let parsed = match vmcoreinfo::parse_note(note) {
        Ok(n) => n,
        Err(e) => return crate::fail_fmt!("note does not parse: {}", e.as_str()),
    };
    if parsed.name != vmcoreinfo::NOTE_NAME || parsed.kind != vmcoreinfo::NOTE_TYPE {
        return Outcome::Fail("note name or type");
    }
    let va = vibeos::paging::VirtAddr(note.as_ptr().addr() as u64);
    match crate::paging_init::translate(va) {
        Some((p, _, _)) if p.0 == pa => {}
        _ => return crate::fail_fmt!("note pa {:#x} is not its translation", pa),
    }
    let desc = parsed.desc;

    let id = vmcoreinfo_init::build_id();
    if id.as_bytes().len() != 20 {
        return crate::fail_fmt!("build id is {} bytes, want 20", id.as_bytes().len());
    }
    let mut hex = [0u8; 40];
    let mut w = StackBuf::new(&mut hex);
    for b in id.as_bytes() {
        if core::fmt::Write::write_fmt(&mut w, format_args!("{b:02x}")).is_err() {
            return Outcome::Fail("build id hex");
        }
    }
    if w.is_cut() || vmcoreinfo::get(desc, "BUILD-ID") != Some(&hex[..]) {
        return Outcome::Fail("BUILD-ID is not this kernel's 40-digit id");
    }
    if note_u64(desc, "PAGESIZE", 10) != Some(4096) {
        return Outcome::Fail("PAGESIZE");
    }
    if note_u64(desc, "NUMBER(vibeos_pgt_root)", 10) != Some(crate::paging_init::kernel_cr3()) {
        return Outcome::Fail("NUMBER(vibeos_pgt_root) is not kernel_cr3");
    }
    if note_u64(desc, "NUMBER(vibeos_pgt_levels)", 10) != Some(4) {
        return Outcome::Fail("NUMBER(vibeos_pgt_levels)");
    }

    let log = note_u64(desc, "SYMBOL(vibeos_log)", 16);
    let tcbs = note_u64(desc, "SYMBOL(vibeos_tcbs)", 16);
    let tcbs_len = note_u64(desc, "LENGTH(vibeos_tcbs)", 10);
    let cpus = note_u64(desc, "SYMBOL(vibeos_cpus)", 16);
    let cpus_len = note_u64(desc, "LENGTH(vibeos_cpus)", 10);
    if log != Some(crate::log_init::ring_root()) {
        return Outcome::Fail("SYMBOL(vibeos_log) is not ring_root");
    }
    let (t, tn) = thread_init::table_root();
    if tcbs != Some(t) || tcbs_len != Some(tn) || tn == 0 {
        return Outcome::Fail("SYMBOL/LENGTH(vibeos_tcbs) is not table_root");
    }
    let (c, cn) = per_cpu_init::table_root();
    if cpus != Some(c) || cpus_len != Some(cn) || cn == 0 {
        return Outcome::Fail("SYMBOL/LENGTH(vibeos_cpus) is not table_root");
    }

    // SAFETY: `t` is `Sched.slots`' base (`thread_init::table_root`, checked
    // above), a static array of `tn > 0` initialized `TcbSlot`s, and SCHED is
    // held while slot 0 and its TCB are read, so no CPU writes them; the
    // borrow ends inside the closure; established by
    // `thread_init::table_root`.
    let slot0 = thread_init::with_sched_lock(|| unsafe {
        (*(t as *const TcbSlot)).as_deref().map(|tcb| tcb.id)
    });
    if slot0 != Some(ThreadId::BOOTSTRAP) {
        return crate::fail_fmt!("tcb slot 0 holds {:?}, want the bootstrap thread", slot0);
    }
    // SAFETY: `c` is the base of `CPUS`' boxed slice (`per_cpu_init::
    // table_root`, checked above), which holds `cn > 0` initialized
    // `PerCpu`s for good; `cpu_id` is set in `init_bsp`/`init_ap` before the
    // slot is published and never written after, and the read goes through
    // a raw place, not a reference; established by
    // `per_cpu_init::table_root`.
    let cpu0 = unsafe { core::ptr::read(&raw const (*(c as *const PerCpu)).cpu_id) };
    if cpu0 != 0 {
        return crate::fail_fmt!("cpus[0].cpu_id {}, want 0", cpu0);
    }
    // SAFETY: `log_init::ring_root` is `LOG`'s address (checked above), a
    // static `KernelLog` over this port that lives for the kernel's life and
    // is only ever shared as `&`; established by `log_init::ring_root`.
    let cell = unsafe { &*(crate::log_init::ring_root() as *const KernelLog<Arch>) };
    if cell.with(|l| l.ring.written()) == 0 {
        return Outcome::Fail("log ring reached through SYMBOL(vibeos_log) is empty");
    }

    if !fw_cfg_init::present() {
        return Outcome::Skip("no fw_cfg");
    }
    if state != DeviceState::Written {
        return crate::fail_fmt!("device {}", state.as_str());
    }
    let Some(file) = fw_cfg_init::file(FW_CFG_FILE) else {
        return Outcome::Fail("no etc/vmcoreinfo");
    };
    let mut raw = [0u8; FwCfgVmcoreinfo::LEN];
    if fw_cfg_init::read(&file, &mut raw) != raw.len() {
        return Outcome::Fail("etc/vmcoreinfo is short");
    }
    let dev = FwCfgVmcoreinfo::from_le_bytes(&raw);
    if dev.guest_format != FORMAT_ELF || dev.size as usize != note.len() || dev.paddr != pa {
        return crate::fail_fmt!(
            "device holds format {} size {} paddr {:#x}, want 1, {}, {:#x}",
            dev.guest_format,
            dev.size,
            dev.paddr,
            note.len(),
            pa
        );
    }
    Outcome::Ok
}

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
/// The user `rbp` `backtrace_syscall_boundary`'s program sets before its
/// syscall, which the probe keys on.
const WALK_RBP: u64 = 0x4000_0800;
static WALK_ARMED: AtomicBool = AtomicBool::new(false);
static WALK_DONE: AtomicBool = AtomicBool::new(false);
static WALK_END: AtomicU32 = AtomicU32::new(0);
static WALK_FRAMES: AtomicU32 = AtomicU32::new(0);
static WALK_LAST: AtomicU64 = AtomicU64::new(0);
static WALK_LOW: AtomicBool = AtomicBool::new(false);

fn walk_end_code(e: WalkEnd) -> u32 {
    match e {
        WalkEnd::NullRbp => 1,
        WalkEnd::UnknownStack => 2,
        WalkEnd::OutsideImage => 3,
        WalkEnd::NotRising => 4,
        WalkEnd::DepthCap => 5,
    }
}

/// `syscall_init::vibeos_syscall_stub`'s first line under `kernel_tests`:
/// on the first syscall whose saved user `rbp` is [`WALK_RBP`] while the
/// test is armed, walk the frame-pointer chain from here as the panic
/// backtrace does (`panic::walk_known`) and record how it ended, the last
/// frame, and whether any frame was a user address.
#[inline(never)]
pub(crate) fn syscall_walk_probe(user_rbp: u64) {
    // Acquire: pairs with the test's Release store.
    if user_rbp != WALK_RBP || !WALK_ARMED.load(Ordering::Acquire) {
        return;
    }
    if WALK_DONE.load(Ordering::Acquire) {
        return;
    }
    // IF=0: the known stacks read this CPU's per-CPU state.
    let _irq = crate::x86::InterruptGuard::enter();
    let rip = crate::x86::read_rip();
    let rbp = crate::x86::read_rbp();
    let mut last = 0u64;
    let mut low = false;
    let (n, end) = crate::panic::walk_known(rip, rbp, |a| {
        last = a;
        low |= a < USER_MAP_END;
    });
    WALK_END.store(walk_end_code(end), Ordering::Relaxed);
    WALK_FRAMES.store(n as u32, Ordering::Relaxed);
    WALK_LAST.store(last, Ordering::Relaxed);
    WALK_LOW.store(low, Ordering::Relaxed);
    // Release: the test reads the words above after it (Acquire).
    WALK_DONE.store(true, Ordering::Release);
}

// A nonzero user rbp, getpid, then exit(0).
user_code!(
    WALK_GETPID,
    "
    mov rbp, 0x40000800
    mov eax, 39
    syscall
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

/// L1422: a walk from inside a syscall made with a nonzero user `rbp` ends
/// on the null `rbp` the entry stored, at a last frame in the entry
/// (`vibeos_syscall_entry`), with no frame below `USER_MAP_END`.
pub(crate) fn test_backtrace_syscall_boundary() -> Outcome {
    WALK_DONE.store(false, Ordering::Relaxed);
    // Release: pairs with the probe's Acquire load.
    WALK_ARMED.store(true, Ordering::Release);
    let st = user::run(&Image::Code(WALK_GETPID, DEFAULT), &["walk_boundary"]);
    WALK_ARMED.store(false, Ordering::Release);
    match st {
        Ok(0) => {}
        Ok(s) => return crate::fail_fmt!("user program status {s:#x}, want 0"),
        Err(_) => return Outcome::Fail("user program did not run"),
    }
    // Acquire: pairs with the probe's Release store.
    if !WALK_DONE.load(Ordering::Acquire) {
        return Outcome::Fail("the probe did not fire");
    }
    let end = WALK_END.load(Ordering::Relaxed);
    let n = WALK_FRAMES.load(Ordering::Relaxed);
    if end != walk_end_code(WalkEnd::NullRbp) {
        return crate::fail_fmt!("walk ended with code {end} after {n} frames, want NullRbp");
    }
    if WALK_LOW.load(Ordering::Relaxed) {
        return Outcome::Fail("a frame below USER_MAP_END");
    }
    let last = WALK_LAST.load(Ordering::Relaxed);
    match crate::panic::symbol_name(last) {
        Some("vibeos_syscall_entry") => Outcome::Ok,
        other => {
            crate::fail_fmt!("last of {n} frames {last:#x} is {other:?}, want vibeos_syscall_entry")
        }
    }
}

pub(crate) const TESTS: &[Test] = &[
    test("log_boot_level", test_log_boot_level),
    test("log_boot_captured", test_log_boot_captured).once(),
    test("log_runtime_filter", test_log_runtime_filter),
    test("log_emit_roundtrip", test_log_emit_roundtrip),
    test("log_dmesg_no_recapture", test_log_dmesg_no_recapture),
    test("log_reentry_drop_counted", test_log_reentry_drop_counted),
    test("serial_lines_whole", test_serial_lines_whole).deadline(60_000),
    test("serial_frame", test_serial_frame),
    test("trace_ring_own_cpu", test_trace_ring_own_cpu),
    test("trace_tracepoints_fire", test_trace_tracepoints_fire),
    test("vmcoreinfo_published", test_vmcoreinfo_published),
    test(
        "backtrace_syscall_boundary",
        test_backtrace_syscall_boundary,
    ),
];
