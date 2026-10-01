//! Kernel time: PIT, TSC calibration, the clocksource, RTC. DESIGN §6.
//!
//! Library math lives in `vibeos::time`. This module owns port I/O, HPET
//! MMIO, the PM timer read, the IRQ0 handler body, the BSP `tsc_per_ms`,
//! and the clocksource choice (DESIGN §6.4).

use core::sync::atomic::{AtomicU64, Ordering};

use vibeos::acpi::HpetInfo;
use vibeos::arch::CycleCounter;
use vibeos::pic::{PIC_EOI, PIC1_CMD};
use vibeos::time::{
    CalibSource, Candidates, ClockWriter, ClocksourceId, Counter, FS_PER_MS, IO_WAIT_PORT,
    PIT_CALIB_COUNT, PIT_CALIB_MS, PIT_CH0_WRITES, PIT_CH2, PIT_CMD, PIT_CMD_CH2_ONESHOT, PIT_GATE,
    PM_TIMER_HZ, Snapshot, TickClock, WallOrigin, bcd_to_bin, hpet_counter_width, hpet_hz,
    hpet_period_ok, monotonic_max, rank, tsc_per_ms_from_hpet, tsc_per_ms_from_pit,
    unix_from_civil, wall_unix_s,
};

use crate::acpi_init;
use crate::arch::current::{Arch, interrupts_enabled, wait_for_interrupt};
#[cfg(target_arch = "x86_64")]
use crate::arch::x86_64::{has_rdtscp, invariant_tsc, rdtsc_ser};
use crate::cell::{BootCell, IrqCell};
use crate::paging_init;
#[cfg(target_arch = "x86_64")]
use crate::x86;

const HPET_GCAP_ID: u64 = 0x00;
const HPET_GEN_CFG: u64 = 0x10;
const HPET_MAIN: u64 = 0xF0;
const HPET_ENABLE: u64 = 1;
const HPET_LEGACY: u64 = 2;
const CALIB_SPIN_CAP: u64 = 1_000_000_000;

const RTC_INDEX: u16 = 0x70;
const RTC_DATA: u16 = 0x71;
const RTC_SEC: u8 = 0x00;
const RTC_MIN: u8 = 0x02;
const RTC_HOUR: u8 = 0x04;
const RTC_DAY: u8 = 0x07;
const RTC_MONTH: u8 = 0x08;
const RTC_YEAR: u8 = 0x09;
const RTC_STATUS_A: u8 = 0x0A;
const RTC_STATUS_B: u8 = 0x0B;
const RTC_CENTURY: u8 = 0x32;
const RTC_UIP: u8 = 1 << 7;
const RTC_DM_BINARY: u8 = 1 << 2;
const RTC_24H: u8 = 1 << 1;
const RTC_NMI_OFF: u8 = 0x80;

pub(super) struct TimeState {
    clock: TickClock,
    /// Timer interrupts CPU 0 has taken: scheduling and diagnostics only,
    /// never the clock.
    ticks: AtomicU64,
    tsc_per_ms: u64,
    pub(super) source: CalibSource,
    pub(super) use_rdtscp: bool,
    pub(super) invariant_tsc: bool,
    /// The TSC at `tsc_per_ms * 1000` Hz, once calibrated.
    tsc: Option<Counter>,
    /// The HPET main counter and the VA of its register block.
    hpet: Option<(Counter, u64)>,
    /// The ACPI PM timer and its `TMR_VAL` port.
    pm: Option<(Counter, u16)>,
    rtc: Option<WallOrigin>,
}

impl TimeState {
    const fn empty() -> Self {
        Self {
            clock: TickClock::new(),
            ticks: AtomicU64::new(0),
            tsc_per_ms: 0,
            source: CalibSource::Pit,
            use_rdtscp: false,
            invariant_tsc: false,
            tsc: None,
            hpet: None,
            pm: None,
            rtc: None,
        }
    }

    /// The counter `id` names, when this boot has it.
    fn counter(&self, id: ClocksourceId) -> Option<Counter> {
        match id {
            ClocksourceId::Tsc => self.tsc,
            ClocksourceId::Hpet => self.hpet.map(|(c, _)| c),
            ClocksourceId::AcpiPm => self.pm.map(|(c, _)| c),
        }
    }

    /// The candidates [`rank`] chooses from, with the warp result so far.
    fn candidates(&self) -> Candidates {
        Candidates {
            tsc: self.tsc,
            tsc_invariant: self.invariant_tsc,
            tsc_warp_ok: tsc_warp_ok(),
            hpet: self.hpet.map(|(c, _)| c),
            pm: self.pm.map(|(c, _)| c),
        }
    }
}

pub(super) static STATE: BootCell<TimeState> = BootCell::new();
/// Highest `now_ns` published. A reader's base and delta round down
/// separately, so a read can land 1 ns above the next base; this hides it.
static LAST_NS: AtomicU64 = AtomicU64::new(0);
/// The clock's one writer. Only CPU 0 takes it: its tick, with IF off, and
/// `confirm_clocksource`, which `kmain` runs on the BSP.
static WRITER: IrqCell<Option<ClockWriter>> = IrqCell::new(None);

fn publish_ns(n: u64) -> u64 {
    monotonic_max(&LAST_NS, n)
}

#[cfg(target_arch = "x86_64")]
fn io_wait() {
    // SAFETY: invariant I229, established at `time::time_init::init`: port
    // 0x80 is the delay port, a write to it has no effect any module
    // relies on.
    unsafe { x86::outb(IO_WAIT_PORT, 0) };
}

/// The port's cycle counter (the serialized TSC). IRQ0 and `now_us` both
/// use this.
pub fn read_tsc() -> u64 {
    <Arch as CycleCounter>::now()
}

/// # Safety
/// `va` is the physmap address of the HPET register block, mapped UC
/// (invariant I228), and `off` an 8-byte-aligned register offset in it.
unsafe fn hpet_read(va: u64, off: u64) -> u64 {
    // SAFETY: this fn's `# Safety` (here): `va + off` is a mapped, aligned
    // HPET register.
    unsafe { ((va.wrapping_add(off)) as *const u64).read_volatile() }
}

/// # Safety
/// As for [`hpet_read`].
unsafe fn hpet_write(va: u64, off: u64, val: u64) {
    // SAFETY: this fn's `# Safety` (here): `va + off` is a mapped, aligned
    // HPET register.
    unsafe { ((va.wrapping_add(off)) as *mut u64).write_volatile(val) };
}

/// # Safety
/// `va` as for [`hpet_read`].
unsafe fn hpet_enable(va: u64) {
    // SAFETY: this fn's `# Safety` (here); GEN_CFG is register 0x10.
    unsafe {
        let cfg = hpet_read(va, HPET_GEN_CFG);
        hpet_write(va, HPET_GEN_CFG, (cfg & !HPET_LEGACY) | HPET_ENABLE);
    }
}

fn hpet_va(hpet: &HpetInfo) -> u64 {
    paging_init::HHDM_BASE.wrapping_add(hpet.base)
}

/// HPET main counter VA + period, after the page is UC. None if unusable.
pub(crate) fn hpet_ready() -> Option<(u64, u32)> {
    let hpet = acpi_init::info()?.hpet?;
    if !hpet_period_ok(hpet.period_fs) {
        return None;
    }
    let va = hpet_va(&hpet);
    // SAFETY: invariant I228, established at `acpi::acpi_init::init`: it
    // stores a nonzero `period_fs` only after UC-patching the HPET page,
    // and `hpet_period_ok` rejected zero just above.
    unsafe { hpet_enable(va) };
    Some((va, hpet.period_fs))
}

/// The HPET main counter.
///
/// # Safety
/// `va` is the address [`hpet_ready`] returned.
pub(crate) unsafe fn hpet_read_main(va: u64) -> u64 {
    // SAFETY: this fn's `# Safety` (here): `hpet_ready` returns only a UC
    // HPET block (invariant I228).
    unsafe { hpet_read(va, HPET_MAIN) }
}

/// The ACPI PM timer's `TMR_VAL`.
#[cfg(target_arch = "x86_64")]
fn pm_read(port: u16) -> u64 {
    // SAFETY: `port` is the FADT's PM timer block, which
    // `vibeos::acpi::parse_fadt` accepted as a nonzero SystemIO port, and a
    // `TMR_VAL` read has no side effect; established at
    // `acpi::acpi_init::init`, which parsed the FADT.
    u64::from(unsafe { x86::inl(port) })
}

/// Raw read of counter `id`, masked to its width. 0 for a counter this
/// boot does not have, which no published snapshot names.
fn read_raw(st: &TimeState, id: ClocksourceId) -> u64 {
    match id {
        ClocksourceId::Tsc => read_tsc(),
        ClocksourceId::Hpet => st.hpet.map_or(0, |(c, va)| {
            // SAFETY: invariant I228, established at `acpi::acpi_init::init`:
            // `va` came from `hpet_ready` in `hpet_counter`.
            unsafe { hpet_read_main(va) & c.mask() }
        }),
        ClocksourceId::AcpiPm => st.pm.map_or(0, |(c, port)| pm_read(port) & c.mask()),
    }
}

/// The HPET main counter as a clocksource: a sane period, the width
/// `GCAP_ID` reports, and a counter that moves within 100,000 spins.
fn hpet_counter() -> Option<(Counter, u64)> {
    let (va, period_fs) = hpet_ready()?;
    // SAFETY: invariant I228, established at `acpi::acpi_init::init`:
    // `hpet_ready` returned a UC HPET block, and GCAP_ID is register 0.
    let gcap = unsafe { hpet_read(va, HPET_GCAP_ID) };
    let c = Counter::new(
        ClocksourceId::Hpet,
        hpet_hz(period_fs)?,
        hpet_counter_width(gcap),
    )?;
    let main = || {
        // SAFETY: invariant I228, established at `acpi::acpi_init::init`:
        // `va` is the UC block `hpet_ready` returned.
        unsafe { hpet_read_main(va) & c.mask() }
    };
    counter_moves(main).then_some((c, va))
}

/// The ACPI PM timer as a clocksource, when the FADT names one and it
/// moves within 100,000 spins.
fn pm_counter() -> Option<(Counter, u16)> {
    let pm = acpi_init::info()?.fadt?.pm_timer?;
    let c = Counter::new(ClocksourceId::AcpiPm, PM_TIMER_HZ, u32::from(pm.width))?;
    counter_moves(|| pm_read(pm.port) & c.mask()).then_some((c, pm.port))
}

/// `read` returns a new value within 100,000 spins.
fn counter_moves(read: impl Fn() -> u64) -> bool {
    let probe = read();
    for _ in 0..100_000 {
        if read() != probe {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

pub(super) fn calibrate_hpet(hpet: &HpetInfo, use_rdtscp: bool) -> Option<u64> {
    if !hpet_period_ok(hpet.period_fs) {
        return None;
    }
    let va = hpet_va(hpet);
    // Every HPET access below relies on invariant I228, established at
    // `acpi::acpi_init::init`: it stores a nonzero `period_fs` only after
    // UC-patching the HPET page, and `hpet_period_ok` rejected zero above.
    let main = || {
        // SAFETY: invariant I228, established at `acpi::acpi_init::init`,
        // as stated above `main`.
        unsafe { hpet_read(va, HPET_MAIN) }
    };
    let want = (PIT_CALIB_MS as u128 * FS_PER_MS) / hpet.period_fs as u128;
    let want = u64::try_from(want).ok()?;
    if want == 0 {
        return None;
    }
    // SAFETY: invariant I228, established at `acpi::acpi_init::init`, as
    // stated above `main`.
    unsafe { hpet_enable(va) };
    let probe = main();
    let mut saw = false;
    for _ in 0..100_000 {
        if main() != probe {
            saw = true;
            break;
        }
        core::hint::spin_loop();
    }
    if !saw {
        return None;
    }
    let start = main();
    let t0 = rdtsc_ser(use_rdtscp);
    let mut spins = 0u64;
    loop {
        let now = main();
        if now.wrapping_sub(start) >= want {
            break;
        }
        spins += 1;
        if spins > CALIB_SPIN_CAP {
            return None;
        }
        core::hint::spin_loop();
    }
    let t1 = rdtsc_ser(use_rdtscp);
    let elapsed = main().wrapping_sub(start);
    tsc_per_ms_from_hpet(t1.wrapping_sub(t0), elapsed, hpet.period_fs)
}

/// Channel 2 one-shot, gated through 0x61. Does not touch channel 0.
#[cfg(target_arch = "x86_64")]
pub(super) fn calibrate_pit(use_rdtscp: bool) -> Option<u64> {
    // SAFETY: invariant I229, established at `time::time_init::init`: the
    // PIT and port 0x61 are this module's, and channel 2 feeds only this
    // calibration.
    unsafe {
        let n61 = x86::inb(PIT_GATE);
        x86::outb(PIT_GATE, n61 & !0x01);
        x86::outb(PIT_CMD, PIT_CMD_CH2_ONESHOT);
        x86::outb(PIT_CH2, (PIT_CALIB_COUNT & 0xFF) as u8);
        io_wait();
        x86::outb(PIT_CH2, (PIT_CALIB_COUNT >> 8) as u8);
        let n61 = x86::inb(PIT_GATE);
        x86::outb(PIT_GATE, (n61 & !0x02) | 0x01);
    }
    let t0 = rdtsc_ser(use_rdtscp);
    let mut spins = 0u64;
    loop {
        // SAFETY: invariant I229, established at `time::time_init::init`, as
        // for the writes above.
        if unsafe { x86::inb(PIT_GATE) } & (1 << 5) != 0 {
            break;
        }
        spins += 1;
        if spins > CALIB_SPIN_CAP {
            return None;
        }
        core::hint::spin_loop();
    }
    let t1 = rdtsc_ser(use_rdtscp);
    tsc_per_ms_from_pit(t1.wrapping_sub(t0), PIT_CALIB_COUNT)
}

#[cfg(target_arch = "x86_64")]
fn program_pit_ch0() {
    for &(port, val) in PIT_CH0_WRITES {
        // SAFETY: invariant I229, established at `time::time_init::init`:
        // the PIT (and the delay port) are this module's.
        unsafe { x86::outb(port, val) };
    }
}

#[cfg(target_arch = "x86_64")]
fn rtc_reg(reg: u8) -> u8 {
    // SAFETY: invariant I229, established at `time::time_init::init`: CMOS
    // is this module's, so no one else moves the index between the writes.
    unsafe {
        x86::outb(RTC_INDEX, reg | RTC_NMI_OFF);
        x86::inb(RTC_DATA)
    }
}

fn rtc_wait_uip_clear() {
    for _ in 0..100_000 {
        if rtc_reg(RTC_STATUS_A) & RTC_UIP == 0 {
            return;
        }
        core::hint::spin_loop();
    }
}

struct RtcRaw {
    sec: u8,
    min: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
    century: u8,
    status_b: u8,
}

fn rtc_snapshot() -> RtcRaw {
    rtc_wait_uip_clear();
    RtcRaw {
        sec: rtc_reg(RTC_SEC),
        min: rtc_reg(RTC_MIN),
        hour: rtc_reg(RTC_HOUR),
        day: rtc_reg(RTC_DAY),
        month: rtc_reg(RTC_MONTH),
        year: rtc_reg(RTC_YEAR),
        century: rtc_reg(RTC_CENTURY),
        status_b: rtc_reg(RTC_STATUS_B),
    }
}

fn rtc_decode(raw: RtcRaw) -> Option<(i32, u8, u8, u8, u8, u8)> {
    let binary = raw.status_b & RTC_DM_BINARY != 0;
    let cvt = |v: u8| if binary { v } else { bcd_to_bin(v) };
    let sec = cvt(raw.sec);
    let min = cvt(raw.min);
    let day = cvt(raw.day);
    let month = cvt(raw.month);
    let year_2 = cvt(raw.year);
    let century = cvt(raw.century);
    let mut hour = raw.hour;
    if raw.status_b & RTC_24H == 0 {
        let pm = hour & 0x80 != 0;
        hour &= 0x7F;
        hour = cvt(hour);
        if pm && hour != 12 {
            hour = hour.saturating_add(12);
        } else if !pm && hour == 12 {
            hour = 0;
        }
    } else {
        hour = cvt(hour);
    }
    let year = if (19..=21).contains(&century) {
        century as i32 * 100 + year_2 as i32
    } else {
        2000 + year_2 as i32
    };
    Some((year, month, day, hour, min, sec))
}

fn read_rtc_unix() -> Option<u64> {
    let a = rtc_snapshot();
    let b = rtc_snapshot();
    // If an update landed between the two snapshots, take the second.
    let civil = rtc_decode(b).or_else(|| rtc_decode(a))?;
    unix_from_civil(civil.0, civil.1, civil.2, civil.3, civil.4, civil.5)
}

/// Tick body: count the tick, read the clocksource, publish its snapshot.
/// Caller EOIs, rearms, then `sched_init::on_timer_tick` (DESIGN §5.8). No
/// allocation, no logging. Runs on CPU 0 only, so a counter narrower than
/// 64 bits is read once a tick, far inside its half wrap.
pub fn on_hw_tick() {
    let Some(st) = STATE.try_get() else {
        return;
    };
    // Relaxed: a count for diagnostics; nothing is published through it.
    st.ticks.fetch_add(1, Ordering::Relaxed);
    WRITER.with(|w| {
        if let Some(w) = w.as_mut() {
            let raw = read_raw(st, w.counter().id);
            publish(st, w.advance(raw));
        }
    });
}

pub fn on_pit_tick() {
    #[cfg(feature = "kernel_tests")]
    super::ktest::count_pit_irq();
    on_hw_tick();
}

/// Write `snap` to the clock. The caller holds `WRITER`.
fn publish(st: &TimeState, snap: Snapshot) {
    #[cfg(feature = "kernel_tests")]
    super::ktest::publish_tick(snap);
    st.clock.write(snap);
}

/// Re-rank the clocksource once the AP warp tests have run, switch to the
/// winner if it changed, and print `time: clocksource <name>`. Halts when
/// no candidate is left. `kmain` calls it once, right after
/// `smp_init::init`, on the BSP.
#[allow(
    clippy::panic,
    reason = "kmain runs on the BSP, so the CPU 0 assertion is a kernel invariant no input reaches"
)]
pub fn confirm_clocksource() {
    let Some(st) = STATE.try_get() else {
        crate::boot::halt_with("vibeOS: time: no clocksource");
    };
    let Some(want) = rank(&st.candidates()) else {
        crate::boot::halt_with("vibeOS: time: no clocksource");
    };
    WRITER.with(|w| {
        // IF is off inside the cell, so the per-CPU read is legal.
        assert!(
            crate::per_cpu_init::try_current().is_none_or(|c| c.cpu_id == 0),
            "confirm_clocksource off the BSP"
        );
        if let Some(w) = w.as_mut()
            && w.counter().id != want.id
        {
            let raw_old = read_raw(st, w.counter().id);
            let raw_new = read_raw(st, want.id);
            publish(st, w.switch(want, raw_old, raw_new));
        }
    });
    // The line names what the clock now publishes.
    let Some(id) = clocksource() else {
        crate::boot::halt_with("vibeOS: time: no clocksource");
    };
    crate::marker!("vibeOS: time: clocksource {}", id.as_str());
}

/// The seqlock clock, for the in-guest clock tests' unclamped read.
#[cfg(feature = "kernel_tests")]
pub(super) fn tick_clock() -> Option<&'static TickClock> {
    STATE.try_get().map(|s| &s.clock)
}

/// The counter `id` names, for the in-guest clock tests.
#[cfg(feature = "kernel_tests")]
pub(crate) fn counter(id: ClocksourceId) -> Option<Counter> {
    STATE.try_get()?.counter(id)
}

/// One raw read of counter `id`, for the in-guest clock tests.
#[cfg(feature = "kernel_tests")]
pub(crate) fn read_counter(id: ClocksourceId) -> Option<u64> {
    let st = STATE.try_get()?;
    st.counter(id).map(|_| read_raw(st, id))
}

/// Timer interrupts CPU 0 has taken since `time_init::init`.
pub fn ticks() -> u64 {
    // Relaxed: as in `on_hw_tick`.
    STATE
        .try_get()
        .map_or(0, |s| s.ticks.load(Ordering::Relaxed))
}

/// The clocksource the clock publishes; None before `time_init::init`.
pub fn clocksource() -> Option<ClocksourceId> {
    STATE.try_get()?.clock.read().map(|s| s.id)
}

pub fn uptime_ms() -> u64 {
    now_ns() / 1_000_000
}

pub fn now_us() -> u64 {
    now_ns() / 1_000
}

/// Nanoseconds since `time_init::init`, from the clocksource (DESIGN §6.4),
/// clamped monotonic by `LAST_NS`. Safe from any context: the latch never
/// waits for its writer.
pub fn now_ns() -> u64 {
    let Some(st) = STATE.try_get() else {
        return 0;
    };
    publish_ns(
        st.clock
            .now_ns_with(|id| read_raw(st, id), |id| st.counter(id)),
    )
}

pub fn tsc_per_ms() -> u64 {
    STATE.try_get().map(|s| s.tsc_per_ms).unwrap_or(0)
}

/// CPUID.8000_0007H:EDX[8]. TCG leaves this clear; KVM and real silicon set it.
pub fn tsc_invariant() -> bool {
    STATE.try_get().is_some_and(|s| s.invariant_tsc)
}

/// The largest backward step the AP bring-up TSC warp test saw, in
/// cycles (DESIGN §7.4); 0 for none.
static TSC_WARP_MAX: AtomicU64 = AtomicU64::new(0);

/// One side of a warp test saw at most `backward` cycles of backward step.
pub fn note_tsc_warp(backward: u64) {
    // Release: pairs with the Acquire loads in `tsc_warp_ok` and
    // `tsc_max_skew`.
    TSC_WARP_MAX.fetch_max(backward, Ordering::Release);
}

/// No warp test has seen the TSC step backward. True on a one-CPU boot,
/// which runs none. Final once `smp_init::init` returns.
pub fn tsc_warp_ok() -> bool {
    tsc_max_skew() == 0
}

/// The largest backward step any warp test saw, in cycles; 0 for none.
pub fn tsc_max_skew() -> u64 {
    // Acquire: pairs with the Release `fetch_max` in `note_tsc_warp`.
    TSC_WARP_MAX.load(Ordering::Acquire)
}

/// Wall-clock seconds, RTC at boot plus uptime.
pub fn unix_time_s() -> Option<u64> {
    let st = STATE.try_get()?;
    st.rtc.map(|o| wall_unix_s(o, now_ns()))
}

pub fn busy_wait_ms(ms: u64) {
    if ms == 0 {
        return;
    }
    let k = tsc_per_ms();
    if k == 0 {
        return;
    }
    let start = read_tsc();
    let target = (start as u128).saturating_add(ms as u128 * k as u128);
    while (read_tsc() as u128) < target {
        if interrupts_enabled() {
            let before = read_tsc();
            wait_for_interrupt();
            if (read_tsc() as u128) <= before as u128 {
                while (read_tsc() as u128) < target {
                    core::hint::spin_loop();
                }
                return;
            }
        } else {
            core::hint::spin_loop();
        }
    }
}

/// Master PIC EOI. Used by the PIT gate after `on_pit_tick`.
#[cfg(target_arch = "x86_64")]
pub fn eoi_pit() {
    // SAFETY: invariant I229, established at `arch::x86_64::pic::program`:
    // the master 8259's EOI, which the IRQ0 path writes directly, is the
    // row's named exception to the PIC's ownership.
    unsafe { x86::outb(PIC1_CMD, PIC_EOI) };
}

/// Calibrate, program PIT channel 0, emit markers. Does not `sti`.
///
/// # Safety
/// IDT and PIC remap already done. IRQs still masked at the controller.
pub unsafe fn init() {
    let use_rdtscp = has_rdtscp();
    let inv = invariant_tsc();
    if !inv {
        crate::marker!("vibeOS: time: invariant tsc absent");
    }

    let mut st = TimeState::empty();
    st.use_rdtscp = use_rdtscp;
    st.invariant_tsc = inv;

    let mut source = CalibSource::Pit;
    let mut per_ms = None;

    if let Some(hpet) = acpi_init::info().and_then(|i| i.hpet) {
        match calibrate_hpet(&hpet, use_rdtscp) {
            Some(v) => {
                source = CalibSource::Hpet;
                per_ms = Some(v);
            }
            None => crate::marker!("vibeOS: time: hpet calib refused"),
        }
    }
    if per_ms.is_none() {
        match calibrate_pit(use_rdtscp) {
            Some(v) => {
                source = CalibSource::Pit;
                per_ms = Some(v);
            }
            None => crate::boot::halt_with("vibeOS: time: calib failed"),
        }
    }
    let per_ms = match per_ms {
        Some(v) => v,
        None => crate::boot::halt_with("vibeOS: time: calib failed"),
    };

    st.tsc_per_ms = per_ms;
    #[cfg(target_arch = "x86_64")]
    crate::arch::x86_64::publish_tsc_per_ms(per_ms);
    st.source = source;
    st.tsc = Counter::new(ClocksourceId::Tsc, per_ms.saturating_mul(1_000), 64);
    st.hpet = hpet_counter();
    st.pm = pm_counter();
    // Provisional: the AP warp tests have not run yet, so
    // `confirm_clocksource` ranks again after `smp_init::init`.
    let Some(c) = rank(&st.candidates()) else {
        crate::boot::halt_with("vibeOS: time: no clocksource");
    };
    let w = ClockWriter::new(c, read_raw(&st, c.id), 0);
    publish(&st, w.snapshot());
    WRITER.with(|slot| *slot = Some(w));

    program_pit_ch0();

    if let Some(unix) = read_rtc_unix() {
        st.rtc = Some(WallOrigin {
            unix_s: unix,
            mono_ns: 0,
        });
    }
    // Re-enable NMI after CMOS index bit 7.
    #[cfg(target_arch = "x86_64")]
    // SAFETY: invariant I229, established here: CMOS is this module's.
    unsafe {
        x86::outb(RTC_INDEX, 0x0D);
    }

    crate::marker!("vibeOS: time: calibrated {} {}/ms", source.as_str(), per_ms);
    crate::marker!("vibeOS: time: tsc {}/ms", per_ms);
    #[cfg(target_arch = "x86_64")]
    crate::arch::x86_64::publish_rdtscp(use_rdtscp);
    // SAFETY: invariant I22, established at `cell::BootCell::set`: the one
    // write, on the BSP before SMP (`time::time_init::init`'s `# Safety`
    // runs it before IRQs are on), and no reader sees `STATE` until then.
    unsafe { STATE.set(st) };
}
