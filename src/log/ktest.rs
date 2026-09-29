//! In-guest tests for log (kernel_tests only). Rows: the list in crate::ktest.

use crate::ktest::Outcome;

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
