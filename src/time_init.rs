//! Kernel time: PIT, TSC calibration, seqlock clock, RTC. DESIGN §6.
//!
//! Library math lives in `vibeos::time`. This module owns port I/O,
//! HPET MMIO, the IRQ0 handler body, and the BSP `tsc_per_ms`.

use core::cell::UnsafeCell;
use core::fmt::Write;

use vibeos::acpi::HpetInfo;
use vibeos::pic::{PIC1_CMD, PIC_EOI};
use vibeos::time::{
    bcd_to_bin, hpet_period_ok, next_deadline, tsc_per_ms_from_hpet, tsc_per_ms_from_pit,
    unix_from_civil, wall_unix_s, CalibSource, Instant, TickClock, WallOrigin, IO_WAIT_PORT,
    PIT_CALIB_COUNT, PIT_CALIB_MS, PIT_CH0_WRITES, PIT_CH2, PIT_CMD, PIT_CMD_CH2_ONESHOT,
    PIT_GATE, FS_PER_MS,
};

use crate::acpi_init;
use crate::paging_init;
use crate::serial::{self, Serial};
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

struct BootCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for BootCell<T> {}
impl<T> BootCell<T> {
    const fn new(v: T) -> Self {
        Self(UnsafeCell::new(v))
    }
    unsafe fn get_mut(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
    fn get(&self) -> &T {
        unsafe { &*self.0.get() }
    }
}

/// BSP time. Phase 4 puts `tsc_per_ms` in the per-CPU area; until then
/// there is one writer (IRQ0) and this one calibration sample.
struct TimeState {
    clock: TickClock,
    ticks: u64,
    tsc_per_ms: u64,
    source: CalibSource,
    use_rdtscp: bool,
    rtc: Option<WallOrigin>,
}

impl TimeState {
    const fn empty() -> Self {
        Self {
            clock: TickClock::new(),
            ticks: 0,
            tsc_per_ms: 0,
            source: CalibSource::Pit,
            use_rdtscp: false,
            rtc: None,
        }
    }
}

static STATE: BootCell<TimeState> = BootCell::new(TimeState::empty());

fn io_wait() {
    unsafe { x86::outb(IO_WAIT_PORT, 0) };
}

fn has_rdtscp() -> bool {
    let (max, _, _, _) = x86::cpuid(0x8000_0000, 0);
    if max < 0x8000_0001 {
        return false;
    }
    let (_, _, _, edx) = x86::cpuid(0x8000_0001, 0);
    edx & (1 << 27) != 0
}

fn invariant_tsc() -> bool {
    let (max, _, _, _) = x86::cpuid(0x8000_0000, 0);
    if max < 0x8000_0007 {
        return false;
    }
    let (_, _, _, edx) = x86::cpuid(0x8000_0007, 0);
    edx & (1 << 8) != 0
}

fn rdtsc_ser(use_rdtscp: bool) -> u64 {
    if use_rdtscp {
        x86::rdtscp()
    } else {
        x86::lfence_rdtsc()
    }
}

/// Serialized TSC. IRQ0 and `now_us` both use this.
pub fn read_tsc() -> u64 {
    rdtsc_ser(STATE.get().use_rdtscp)
}

fn hpet_read(va: u64, off: u64) -> u64 {
    unsafe { ((va.wrapping_add(off)) as *const u64).read_volatile() }
}

fn hpet_write(va: u64, off: u64, val: u64) {
    unsafe { ((va.wrapping_add(off)) as *mut u64).write_volatile(val) };
}

fn hpet_enable(va: u64) {
    let cfg = hpet_read(va, HPET_GEN_CFG);
    hpet_write(va, HPET_GEN_CFG, (cfg & !HPET_LEGACY) | HPET_ENABLE);
}

fn hpet_va(hpet: &HpetInfo) -> u64 {
    paging_init::HHDM_BASE.wrapping_add(hpet.base)
}

fn calibrate_hpet(hpet: &HpetInfo, use_rdtscp: bool) -> Option<u64> {
    if !hpet_period_ok(hpet.period_fs) {
        return None;
    }
    let va = hpet_va(hpet);
    let want = (PIT_CALIB_MS as u128 * FS_PER_MS) / hpet.period_fs as u128;
    let want = u64::try_from(want).ok()?;
    if want == 0 {
        return None;
    }
    hpet_enable(va);
    let probe = hpet_read(va, HPET_MAIN);
    let mut saw = false;
    for _ in 0..100_000 {
        if hpet_read(va, HPET_MAIN) != probe {
            saw = true;
            break;
        }
        core::hint::spin_loop();
    }
    if !saw {
        return None;
    }
    let start = hpet_read(va, HPET_MAIN);
    let t0 = rdtsc_ser(use_rdtscp);
    let mut spins = 0u64;
    loop {
        let now = hpet_read(va, HPET_MAIN);
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
    let elapsed = hpet_read(va, HPET_MAIN).wrapping_sub(start);
    tsc_per_ms_from_hpet(t1.wrapping_sub(t0), elapsed, hpet.period_fs)
}

/// Channel 2 one-shot, gated through 0x61. Does not touch channel 0.
#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn measure_pit_ch2() -> Option<u64> {
    let use_rdtscp = STATE.get().use_rdtscp;
    calibrate_pit(use_rdtscp)
}

fn calibrate_pit(use_rdtscp: bool) -> Option<u64> {
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
        unsafe { x86::outb(port, val) };
    }
}

fn rtc_reg(reg: u8) -> u8 {
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
    let year = if century >= 19 && century <= 21 {
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

/// IRQ0 body: increment tick, snapshot TSC. Caller EOIs, then
/// `sched_init::on_timer_tick` (DESIGN §5.8). No allocation, no logging.
pub fn on_pit_tick(tsc: u64) {
    let st = unsafe { STATE.get_mut() };
    st.ticks = st.ticks.wrapping_add(1);
    st.clock.write(st.ticks, tsc);
}

pub fn uptime_ms() -> u64 {
    STATE.get().clock.read().0
}

pub fn now_us() -> u64 {
    let st = STATE.get();
    st.clock.now_us_with(|| rdtsc_ser(st.use_rdtscp), st.tsc_per_ms)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn now_ns() -> u64 {
    let st = STATE.get();
    st.clock.now_ns_with(|| rdtsc_ser(st.use_rdtscp), st.tsc_per_ms)
}

pub fn tsc_per_ms() -> u64 {
    STATE.get().tsc_per_ms
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn source() -> CalibSource {
    STATE.get().source
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn deadline_after(now: Instant) -> Instant {
    next_deadline(now)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn unix_time_s() -> Option<u64> {
    let st = STATE.get();
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
        if x86::interrupts_enabled() {
            let before = read_tsc();
            x86::hlt_once();
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
    unsafe { x86::outb(PIC1_CMD, PIC_EOI) };
}

/// Calibrate, program PIT channel 0, emit markers. Does not `sti`.
///
/// # Safety
/// IDT and PIC remap already done. IRQs still masked at the controller.
pub unsafe fn init() {
    let use_rdtscp = has_rdtscp();
    if !invariant_tsc() {
        serial::line("vibeOS: time: invariant tsc absent");
    }

    let st = unsafe { STATE.get_mut() };
    st.use_rdtscp = use_rdtscp;

    let mut source = CalibSource::Pit;
    let mut per_ms = None;

    if let Some(hpet) = acpi_init::info().and_then(|i| i.hpet) {
        match calibrate_hpet(&hpet, use_rdtscp) {
            Some(v) => {
                source = CalibSource::Hpet;
                per_ms = Some(v);
            }
            None => serial::line("vibeOS: time: hpet calib refused"),
        }
    }
    if per_ms.is_none() {
        match calibrate_pit(use_rdtscp) {
            Some(v) => {
                source = CalibSource::Pit;
                per_ms = Some(v);
            }
            None => halt_time("vibeOS: time: calib failed"),
        }
    }
    let per_ms = match per_ms {
        Some(v) => v,
        None => halt_time("vibeOS: time: calib failed"),
    };

    st.tsc_per_ms = per_ms;
    st.source = source;
    st.clock.write(0, rdtsc_ser(use_rdtscp));

    program_pit_ch0();

    if let Some(unix) = read_rtc_unix() {
        st.rtc = Some(WallOrigin {
            unix_s: unix,
            mono_ns: 0,
        });
    }
    // Re-enable NMI after CMOS index bit 7.
    unsafe { x86::outb(RTC_INDEX, 0x0D) };

    let _ = writeln!(
        Serial,
        "vibeOS: time: calibrated {} {}/ms",
        source.as_str(),
        per_ms
    );
    let _ = writeln!(Serial, "vibeOS: time: tsc {}/ms", per_ms);
}

fn halt_time(msg: &str) -> ! {
    serial::line(msg);
    x86::halt();
}
