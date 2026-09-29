//! In-guest tests for log (kernel_tests only). Rows: [`TESTS`].

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::log::Level;

use crate::ktest::{Outcome, Test, spin_until_ns, test};
use crate::{per_cpu_init, thread_init, time_init};

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
    per_cpu_init::try_current().map_or(0, |c| c.cpu_id)
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

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[
    test("log_boot_level", test_log_boot_level),
    test("log_boot_captured", test_log_boot_captured),
    test("log_runtime_filter", test_log_runtime_filter),
    test("log_emit_roundtrip", test_log_emit_roundtrip),
    test("log_dmesg_no_recapture", test_log_dmesg_no_recapture),
    test("log_reentry_drop_counted", test_log_reentry_drop_counted),
    test("serial_lines_whole", test_serial_lines_whole).deadline(60_000),
    test("serial_frame", test_serial_frame),
];
