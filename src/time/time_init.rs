//! Kernel time: PIT, TSC calibration, seqlock clock, RTC. DESIGN §6.
//!
//! Library math lives in `vibeos::time`. This module owns port I/O,
//! HPET MMIO, the IRQ0 handler body, and the BSP `tsc_per_ms`.

use core::sync::atomic::{AtomicU64, Ordering};

use vibeos::acpi::HpetInfo;
use vibeos::arch::CycleCounter;
use vibeos::pic::{PIC_EOI, PIC1_CMD};
use vibeos::time::{
    CalibSource, FS_PER_MS, IO_WAIT_PORT, PIT_CALIB_COUNT, PIT_CALIB_MS, PIT_CH0_WRITES, PIT_CH2,
    PIT_CMD, PIT_CMD_CH2_ONESHOT, PIT_GATE, TickClock, WallOrigin, bcd_to_bin, hpet_period_ok,
    monotonic_max, tsc_per_ms_from_hpet, tsc_per_ms_from_pit, unix_from_civil, wall_unix_s,
};

use crate::acpi_init;
use crate::arch::current::{Arch, interrupts_enabled, wait_for_interrupt};
#[cfg(target_arch = "x86_64")]
use crate::arch::x86_64::{has_rdtscp, invariant_tsc, rdtsc_ser};
use crate::cell::BootCell;
use crate::paging_init;
use crate::x86;

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
    ticks: AtomicU64,
    tsc_per_ms: u64,
    pub(super) source: CalibSource,
    pub(super) use_rdtscp: bool,
    pub(super) invariant_tsc: bool,
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
            rtc: None,
        }
    }
}

pub(super) static STATE: BootCell<TimeState> = BootCell::new();
/// Highest `now_ns` published. TCG has no invariant TSC; `hlt` can make
/// interpolation step backwards even with a stable seqlock pair.
static LAST_NS: AtomicU64 = AtomicU64::new(0);

fn publish_ns(n: u64) -> u64 {
    monotonic_max(&LAST_NS, n)
}

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

fn program_pit_ch0() {
    for &(port, val) in PIT_CH0_WRITES {
        // SAFETY: invariant I229, established at `time::time_init::init`:
        // the PIT (and the delay port) are this module's.
        unsafe { x86::outb(port, val) };
    }
}

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

/// Tick body: increment, snapshot TSC. Caller EOIs, rearms, then
/// `sched_init::on_timer_tick` (DESIGN §5.8). No allocation, no logging.
pub fn on_hw_tick(tsc: u64) {
    let Some(st) = STATE.try_get() else {
        return;
    };
    let ticks = st.ticks.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    #[cfg(feature = "kernel_tests")]
    super::ktest::publish_tick(ticks, tsc);
    st.clock.write(ticks, tsc);
}

pub fn on_pit_tick(tsc: u64) {
    #[cfg(feature = "kernel_tests")]
    super::ktest::count_pit_irq();
    on_hw_tick(tsc);
}

/// The seqlock clock, for the in-guest clock tests' unclamped read.
#[cfg(feature = "kernel_tests")]
pub(super) fn tick_clock() -> Option<&'static TickClock> {
    STATE.try_get().map(|s| &s.clock)
}

pub fn uptime_ms() -> u64 {
    STATE.try_get().map(|s| s.clock.read().0).unwrap_or(0)
}

pub fn now_us() -> u64 {
    now_ns() / 1_000
}

pub fn now_ns() -> u64 {
    let Some(st) = STATE.try_get() else {
        return 0;
    };
    publish_ns(st.clock.now_ns::<Arch>(st.tsc_per_ms))
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
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "ROADMAP §10.4 FAT timestamps: `FatVol::now` follows `time_init::unix_time_s()`"
    )
)]
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
    let tsc0 = rdtsc_ser(use_rdtscp);
    st.clock.write(0, tsc0);

    program_pit_ch0();

    if let Some(unix) = read_rtc_unix() {
        st.rtc = Some(WallOrigin {
            unix_s: unix,
            mono_ns: 0,
        });
    }
    // Re-enable NMI after CMOS index bit 7.
    // SAFETY: invariant I229, established here: CMOS is this module's.
    unsafe { x86::outb(RTC_INDEX, 0x0D) };

    crate::marker!("vibeOS: time: calibrated {} {}/ms", source.as_str(), per_ms);
    crate::marker!("vibeOS: time: tsc {}/ms", per_ms);
    #[cfg(feature = "kernel_tests")]
    super::ktest::publish_tick(0, tsc0);
    #[cfg(target_arch = "x86_64")]
    crate::arch::x86_64::publish_rdtscp(use_rdtscp);
    // SAFETY: invariant I22, established at `cell::BootCell::set`: the one
    // write, on the BSP before SMP (`time::time_init::init`'s `# Safety`
    // runs it before IRQs are on), and no reader sees `STATE` until then.
    unsafe { STATE.set(st) };
}
