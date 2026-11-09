//! Flight recorder: the kernel half of `vibeos::log::trace` (ROADMAP §10.7).
//!
//! One [`KernelTrace`] in `.bss`, on in every build. Each CPU records to
//! its own ring with IF=0 for the stores a record takes, and takes no
//! lock; the NMI, `#MC` and `#DB` bodies never record (DESIGN §2.2).

use vibeos::arch::{CycleCounter, InterruptMask, PerCpuBase};
use vibeos::log::trace::{self, ClockInfo, Event, KernelTrace};

use crate::arch::current::Arch;

/// The flight recorder. Unmangled so the core tool finds it by name in
/// the kernel ELF; its header says whether it is live.
#[unsafe(no_mangle)]
pub(crate) static VIBEOS_TRACE: KernelTrace = KernelTrace::new();

/// Mark the trace live and install [`record`] as the tracepoints' sink.
/// Once, on the BSP, after `per_cpu_init::init_bsp`: until then every
/// tracepoint is a no-op, so nothing reads `gs` before it is this CPU's.
pub(crate) fn init() {
    VIBEOS_TRACE.init();
    trace::set_sink(record);
}

/// The sink: push `ev` to this CPU's ring, stamped with the cycle counter.
pub(crate) fn record(ev: Event, a: u64, b: u64) {
    let saved = Arch::save_disable();
    let cpu = Arch::cpu_id();
    if let Some(ring) = VIBEOS_TRACE.ring(cpu) {
        ring.push(cpu, ev, Arch::now(), a, b);
    }
    Arch::restore(saved);
}

/// Publish the calibration and warp result into the trace's header, where
/// the core tool reads them. The BSP, once bring-up is done.
pub(crate) fn publish_clock(c: &ClockInfo) {
    VIBEOS_TRACE.publish_clock(c);
}

/// aarch64's clock for the trace's header (ROADMAP §11.7): records carry
/// raw `CNTVCT_EL0` reads, which the core tool converts with the
/// `CNTFRQ_EL0` value the kernel recorded (`Arch::freq_hz`). The physical
/// count, `CNTPCT_EL0`, is one system counter for every CPU; `CNTVCT_EL0`
/// is it minus `CNTVOFF_EL2`, so it agrees across CPUs only while every
/// CPU has the same offset. At EL1 the hypervisor sets one offset per VM;
/// at EL2 with VHE each secondary zeroes its own on entry
/// (`arch::aarch64::secondary`), as the boot CPU's is, and the
/// `sysreg_compare` ktest checks the secondaries' `CNTVOFF_EL2` against
/// the boot CPU's. So the trace orders across CPUs with no warp test. The
/// BSP, after `smp: done`.
#[cfg(target_arch = "aarch64")]
pub(crate) fn publish_cntvct_clock() {
    publish_clock(&ClockInfo {
        freq_hz: Arch::freq_hz().unwrap_or(0),
        // A CPUID bit, which aarch64 has none of; `uniform` decides.
        invariant: false,
        warp_measured: false,
        max_skew: 0,
        uniform: true,
    });
}
