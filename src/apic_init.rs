//! BSP LAPIC + I/O APIC + LAPIC timer. ROADMAP §4.1–4.3.
//!
//! Order: UC already done in `acpi_init` → enable LAPIC → program IOAPIC
//! (masked) → detect/calib/arm timer → prove → marker → mask PIC + PIT GSI.

use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

use vibeos::acpi::{IoApic, MAX_IOAPICS, MadtInfo};
use vibeos::apic::{
    self, APIC_BASE_ENABLE, DEFAULT_LAPIC_PHYS, EoiDomain, IA32_APIC_BASE, IA32_TSC_DEADLINE,
    ICR_POLL_CAP, IOAPIC_VER, IOREGSEL, IOWIN, IpiError, IpiMode, LAPIC_EOI, LAPIC_ESR,
    LAPIC_ICR_HIGH, LAPIC_ICR_LOW, LAPIC_ID, LAPIC_LVT_ERROR, LAPIC_LVT_LINT0, LAPIC_LVT_LINT1,
    LAPIC_LVT_PERF, LAPIC_LVT_THERMAL, LAPIC_LVT_TIMER, LAPIC_SVR, LAPIC_TIMER_CCR,
    LAPIC_TIMER_DCR, LAPIC_TIMER_ICR, LAPIC_TPR, LVT_DELIVERY_EXTINT, LVT_MASKED, Polarity,
    TIMER_DIV_16, TimerMode, Trigger, TscDeadlineStep, has_tsc_deadline, ioapic_max_index,
    ioapic_pin, lvt_timer_periodic, poll_delivery_pending, redir_high, redir_is_masked, redir_low,
    redir_set_mask, svr_value, tsc_deadline_arm_plan, tsc_deadline_value, write_redir,
};
use vibeos::fmt_util;
use vibeos::marker;
use vibeos::time::{FS_PER_MS, PIT_CALIB_MS, hpet_period_ok};
use vibeos::vectors;

use crate::acpi_init;
use crate::arch;
use crate::paging_init;
use crate::serial::{self, Serial};
use crate::time_init;
use crate::x86;

const PROVE_MS: u64 = 50;
const LAPIC_TICKS_PER_MS_MIN: u64 = 100;
const LAPIC_TICKS_PER_MS_MAX: u64 = 50_000_000;
const CALIB_SPIN_CAP: u64 = 1_000_000_000;

struct BootCell<T>(core::cell::UnsafeCell<T>);
unsafe impl<T> Sync for BootCell<T> {}
impl<T> BootCell<T> {
    const fn new(v: T) -> Self {
        Self(core::cell::UnsafeCell::new(v))
    }
    unsafe fn get_mut(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
    fn get(&self) -> &T {
        unsafe { &*self.0.get() }
    }
}

struct IoApicRt {
    va: u64,
    gsi_base: u32,
    max_index: u32,
}

struct ApicState {
    lapic_va: u64,
    ready: bool,
    mode: TimerMode,
    owns_tick: bool,
    #[allow(dead_code)]
    ticks_per_ms: u64,
    ioapic_n: usize,
    ioapics: [IoApicRt; MAX_IOAPICS],
}

impl ApicState {
    const fn empty() -> Self {
        const EMPTY_IO: IoApicRt = IoApicRt {
            va: 0,
            gsi_base: 0,
            max_index: 0,
        };
        Self {
            lapic_va: 0,
            ready: false,
            mode: TimerMode::Pit,
            owns_tick: false,
            ticks_per_ms: 0,
            ioapic_n: 0,
            ioapics: [EMPTY_IO; MAX_IOAPICS],
        }
    }
}

static STATE: BootCell<ApicState> = BootCell::new(ApicState::empty());
static TIMER_FIRES: AtomicU64 = AtomicU64::new(0);

fn phys_va(phys: u64) -> u64 {
    paging_init::HHDM_BASE.wrapping_add(phys)
}

fn lapic_read(va: u64, off: u32) -> u32 {
    unsafe { (va.wrapping_add(off as u64) as *const u32).read_volatile() }
}

fn lapic_write(va: u64, off: u32, val: u32) {
    unsafe { (va.wrapping_add(off as u64) as *mut u32).write_volatile(val) };
}

fn io_write(va: u64, reg: u8, val: u32) {
    unsafe {
        (va.wrapping_add(IOREGSEL as u64) as *mut u32).write_volatile(reg as u32);
        (va.wrapping_add(IOWIN as u64) as *mut u32).write_volatile(val);
    }
}

fn io_read(va: u64, reg: u8) -> u32 {
    unsafe {
        (va.wrapping_add(IOREGSEL as u64) as *mut u32).write_volatile(reg as u32);
        (va.wrapping_add(IOWIN as u64) as *const u32).read_volatile()
    }
}

fn cpuid_tsc_deadline() -> bool {
    let (_, _, ecx, _) = x86::cpuid(1, 0);
    has_tsc_deadline(ecx)
}

fn local_apic_id(va: u64) -> u8 {
    (lapic_read(va, LAPIC_ID) >> 24) as u8
}

/// Enable via `IA32_APIC_BASE` bit 11, MADT type-5 base, SVR/TPR/LVT.
///
/// # Safety
/// LAPIC page already UC. IRQs off.
unsafe fn enable_lapic(madt: &MadtInfo) -> Option<u64> {
    let phys = if madt.lapic_base != 0 {
        madt.lapic_base
    } else {
        DEFAULT_LAPIC_PHYS
    };
    let cur = x86::rdmsr(IA32_APIC_BASE);
    let next = apic::apic_base_msr(cur, phys);
    unsafe { x86::wrmsr(IA32_APIC_BASE, next) };
    let got = x86::rdmsr(IA32_APIC_BASE);
    if got & APIC_BASE_ENABLE == 0 {
        serial::line("vibeOS: lapic: enable bit clear");
        return None;
    }
    let va = phys_va(phys);
    // Probe: a disabled LAPIC reads as zero and looks like missing HW.
    lapic_write(va, LAPIC_TPR, 0);
    lapic_write(va, LAPIC_SVR, svr_value());
    let svr = lapic_read(va, LAPIC_SVR);
    if svr & apic::SVR_ENABLE == 0 {
        serial::line("vibeOS: lapic: svr enable failed");
        return None;
    }
    lapic_write(va, LAPIC_ESR, 0);
    let _ = lapic_read(va, LAPIC_ESR);
    lapic_write(va, LAPIC_ESR, 0);
    lapic_write(va, LAPIC_LVT_ERROR, apic::lvt_error_value());
    lapic_write(va, LAPIC_LVT_THERMAL, apic::lvt_thermal_value());
    lapic_write(va, LAPIC_LVT_PERF, LVT_MASKED);
    lapic_write(va, LAPIC_LVT_LINT0, LVT_MASKED);
    lapic_write(va, LAPIC_LVT_LINT1, LVT_MASKED);
    lapic_write(
        va,
        LAPIC_LVT_TIMER,
        apic::lvt_timer_oneshot(vectors::LAPIC_TIMER, true),
    );
    lapic_write(va, LAPIC_TIMER_ICR, 0);
    Some(va)
}

fn enum_ioapics(madt: &MadtInfo, st: &mut ApicState) {
    st.ioapic_n = 0;
    let mut i = 0;
    while i < madt.ioapic_count {
        let IoApic { addr, gsi_base, .. } = madt.ioapics[i];
        let phys = addr as u64;
        if phys == 0 {
            i += 1;
            continue;
        }
        let va = phys_va(phys);
        let ver = io_read(va, IOAPIC_VER);
        let max_index = ioapic_max_index(ver);
        if let Some(slot) = st.ioapics.get_mut(st.ioapic_n) {
            *slot = IoApicRt {
                va,
                gsi_base,
                max_index,
            };
            st.ioapic_n += 1;
        }
        i += 1;
    }
}

fn mask_all_pins(st: &ApicState) {
    let dest = local_apic_id(st.lapic_va);
    let mut i = 0;
    while i < st.ioapic_n {
        let io = &st.ioapics[i];
        let mut pin = 0u32;
        while pin <= io.max_index {
            let p = pin as u8;
            let high = redir_high(dest);
            let low = redir_low(
                vectors::DEVICE_VEC_START,
                Trigger::Edge,
                Polarity::High,
                true,
            );
            write_redir(|reg, val| io_write(io.va, reg, val), p, high, low);
            pin += 1;
        }
        i += 1;
    }
}

fn apply_isos(st: &ApicState, madt: &MadtInfo) {
    let dest = local_apic_id(st.lapic_va);
    let mut i = 0;
    while i < madt.iso_count {
        let iso = madt.isos[i];
        let trig = apic::iso_trigger(iso.flags);
        let pol = apic::iso_polarity(iso.flags);
        // Shared placeholder vector: every ISO stays masked until a driver
        // calls `route_gsi` with a real vector.
        let _ = route_gsi_inner(
            st,
            iso.gsi,
            vectors::DEVICE_VEC_START,
            dest,
            trig,
            pol,
            true,
        );
        i += 1;
    }
}

fn find_ioapic(st: &ApicState, gsi: u32) -> Option<(&IoApicRt, u8)> {
    let mut i = 0;
    while i < st.ioapic_n {
        let io = &st.ioapics[i];
        if let Some(pin) = ioapic_pin(gsi, io.gsi_base, io.max_index) {
            return Some((io, pin));
        }
        i += 1;
    }
    None
}

fn route_gsi_inner(
    st: &ApicState,
    gsi: u32,
    vector: u8,
    cpu: u8,
    trigger: Trigger,
    polarity: Polarity,
    masked: bool,
) -> Result<(), IpiError> {
    let Some((io, pin)) = find_ioapic(st, gsi) else {
        return Err(IpiError::NoRoute);
    };
    let high = redir_high(cpu);
    let low = redir_low(vector, trigger, polarity, masked);
    write_redir(|reg, val| io_write(io.va, reg, val), pin, high, low);
    Ok(())
}

/// High dword before low. Entries stay masked until `unmask_gsi`.
pub fn route_gsi(
    gsi: u32,
    vector: u8,
    cpu: u8,
    trigger: Trigger,
    polarity: Polarity,
) -> Result<(), IpiError> {
    let st = STATE.get();
    if !st.ready {
        return Err(IpiError::NotReady);
    }
    route_gsi_inner(st, gsi, vector, cpu, trigger, polarity, true)
}

pub fn mask_gsi(gsi: u32) {
    set_gsi_mask(gsi, true);
}

pub fn unmask_gsi(gsi: u32) {
    set_gsi_mask(gsi, false);
}

fn set_gsi_mask(gsi: u32, masked: bool) {
    let st = STATE.get();
    let Some((io, pin)) = find_ioapic(st, gsi) else {
        return;
    };
    let (lo, hi) = apic::ioapic_redir_regs(pin);
    let high = io_read(io.va, hi);
    let low = redir_set_mask(io_read(io.va, lo), masked);
    write_redir(|reg, val| io_write(io.va, reg, val), pin, high, low);
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn gsi_masked(gsi: u32) -> Option<bool> {
    let st = STATE.get();
    let (io, pin) = find_ioapic(st, gsi)?;
    let (lo, _) = apic::ioapic_redir_regs(pin);
    Some(redir_is_masked(io_read(io.va, lo)))
}

pub fn eoi() {
    let st = STATE.get();
    if st.lapic_va != 0 {
        lapic_write(st.lapic_va, LAPIC_EOI, 0);
    }
}

#[allow(dead_code)]
pub fn eoi_for(vec: u8) {
    match apic::eoi_domain(vec) {
        EoiDomain::None => {}
        // PIC paths EOI the 8259 themselves (`pit_irq`, `pic::handle`).
        EoiDomain::Pic => {}
        EoiDomain::Lapic => eoi(),
    }
}

/// ICR high then low; bounded delivery-pending poll. ROADMAP §4.1.
pub fn send_ipi(dest: u8, vector: u8, mode: IpiMode) -> Result<(), IpiError> {
    let st = STATE.get();
    if st.lapic_va == 0 {
        return Err(IpiError::NotReady);
    }
    let va = st.lapic_va;
    if !poll_delivery_pending(|| lapic_read(va, LAPIC_ICR_LOW), ICR_POLL_CAP) {
        return Err(IpiError::DeliveryPendingTimeout);
    }
    let (hi, lo) = apic::send_ipi_plan(dest, vector, mode);
    lapic_write(va, LAPIC_ICR_HIGH, hi);
    lapic_write(va, LAPIC_ICR_LOW, lo);
    if !poll_delivery_pending(|| lapic_read(va, LAPIC_ICR_LOW), ICR_POLL_CAP) {
        return Err(IpiError::DeliveryPendingTimeout);
    }
    Ok(())
}

pub fn send_ipi_cpu(cpu: u32, vector: u8) -> Result<(), IpiError> {
    let Some(c) = crate::per_cpu_init::cpu(cpu) else {
        return Err(IpiError::NotReady);
    };
    send_ipi(c.apic_id as u8, vector, IpiMode::Fixed)
}

/// All-excluding-self shorthand. No-op with one online CPU.
pub fn send_ipi_all_ex_self(vector: u8) -> Result<(), IpiError> {
    let st = STATE.get();
    if st.lapic_va == 0 {
        return Err(IpiError::NotReady);
    }
    if crate::per_cpu_init::online_mask().count_ones() <= 1 {
        return Ok(());
    }
    let va = st.lapic_va;
    if !poll_delivery_pending(|| lapic_read(va, LAPIC_ICR_LOW), ICR_POLL_CAP) {
        return Err(IpiError::DeliveryPendingTimeout);
    }
    let (hi, lo) = apic::send_ipi_all_ex_self_plan(vector, IpiMode::Fixed);
    lapic_write(va, LAPIC_ICR_HIGH, hi);
    lapic_write(va, LAPIC_ICR_LOW, lo);
    if !poll_delivery_pending(|| lapic_read(va, LAPIC_ICR_LOW), ICR_POLL_CAP) {
        return Err(IpiError::DeliveryPendingTimeout);
    }
    Ok(())
}

fn calib_periodic(va: u64) -> Option<u64> {
    let (hpet_va, period_fs) = time_init::hpet_ready()?;
    if !hpet_period_ok(period_fs) {
        return None;
    }
    let want = (PIT_CALIB_MS as u128 * FS_PER_MS) / period_fs as u128;
    let want = u64::try_from(want).ok()?;
    if want == 0 {
        return None;
    }
    lapic_write(va, LAPIC_TIMER_DCR, TIMER_DIV_16);
    lapic_write(
        va,
        LAPIC_LVT_TIMER,
        apic::lvt_timer_oneshot(vectors::LAPIC_TIMER, true),
    );
    lapic_write(va, LAPIC_TIMER_ICR, 0xFFFF_FFFF);
    let start_h = time_init::hpet_read_main(hpet_va);
    let start_c = lapic_read(va, LAPIC_TIMER_CCR);
    let mut spins = 0u64;
    loop {
        let now = time_init::hpet_read_main(hpet_va);
        if now.wrapping_sub(start_h) >= want {
            break;
        }
        spins += 1;
        if spins > CALIB_SPIN_CAP {
            lapic_write(va, LAPIC_TIMER_ICR, 0);
            return None;
        }
        core::hint::spin_loop();
    }
    let end_c = lapic_read(va, LAPIC_TIMER_CCR);
    lapic_write(va, LAPIC_TIMER_ICR, 0);
    if end_c >= start_c {
        return None;
    }
    let delta = (start_c - end_c) as u64;
    let per_ms = delta / PIT_CALIB_MS;
    if per_ms < LAPIC_TICKS_PER_MS_MIN || per_ms > LAPIC_TICKS_PER_MS_MAX {
        return None;
    }
    Some(per_ms)
}

fn arm_tsc_deadline(va: u64, tsc_per_ms: u64) {
    let now = time_init::read_tsc();
    for step in tsc_deadline_arm_plan(vectors::LAPIC_TIMER, now, tsc_per_ms) {
        match step {
            TscDeadlineStep::Lvt(v) => lapic_write(va, LAPIC_LVT_TIMER, v),
            TscDeadlineStep::Mfence => x86::mfence(),
            TscDeadlineStep::Deadline(d) => unsafe { x86::wrmsr(IA32_TSC_DEADLINE, d) },
        }
    }
}

fn arm_periodic(va: u64, ticks_per_ms: u64) {
    lapic_write(va, LAPIC_TIMER_DCR, TIMER_DIV_16);
    lapic_write(
        va,
        LAPIC_LVT_TIMER,
        lvt_timer_periodic(vectors::LAPIC_TIMER, false),
    );
    let icr = ticks_per_ms as u32;
    let icr = if icr == 0 { 1 } else { icr };
    lapic_write(va, LAPIC_TIMER_ICR, icr);
}

fn disarm_timer(va: u64) {
    unsafe { x86::wrmsr(IA32_TSC_DEADLINE, 0) };
    lapic_write(
        va,
        LAPIC_LVT_TIMER,
        apic::lvt_timer_oneshot(vectors::LAPIC_TIMER, true),
    );
    lapic_write(va, LAPIC_TIMER_ICR, 0);
}

fn wait_fires(prev: u64, ms: u64) -> bool {
    let k = time_init::tsc_per_ms();
    let start = time_init::read_tsc();
    let target = (start as u128).saturating_add(ms as u128 * k as u128);
    while TIMER_FIRES.load(Ordering::Relaxed) == prev {
        if (time_init::read_tsc() as u128) >= target {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

fn rearm_deadline() {
    let st = STATE.get();
    if st.mode != TimerMode::TscDeadline || st.lapic_va == 0 {
        return;
    }
    // LVT already in TSC-deadline mode. SDM fence is LVT → deadline only.
    let k = time_init::tsc_per_ms();
    let d = tsc_deadline_value(time_init::read_tsc(), k);
    unsafe { x86::wrmsr(IA32_TSC_DEADLINE, d) };
}

pub fn on_timer_irq() {
    let cpu_id = crate::per_cpu_init::try_current()
        .map(|c| c.cpu_id)
        .unwrap_or(0);
    let tsc = time_init::read_tsc();
    if cpu_id == 0 {
        TIMER_FIRES.fetch_add(1, Ordering::Relaxed);
        time_init::on_hw_tick(tsc);
    }
    eoi();
    rearm_deadline();
    crate::sched_init::on_timer_tick();
}

pub fn on_spurious_irq() {
    // Must not EOI. DESIGN §5.7.
}

pub fn on_error_irq() {
    let st = STATE.get();
    if st.lapic_va != 0 {
        lapic_write(st.lapic_va, LAPIC_ESR, 0);
        let esr = lapic_read(st.lapic_va, LAPIC_ESR);
        Serial::write_bytes(b"vibeOS: lapic: error esr=0x");
        let mut buf = [0u8; 16];
        Serial::write_bytes(fmt_util::write_hex(esr as u64, &mut buf));
        Serial::write_bytes(b"\n");
        lapic_write(st.lapic_va, LAPIC_ESR, 0);
    }
    eoi();
}

pub fn on_thermal_irq() {
    serial::line("vibeOS: lapic: thermal");
    eoi();
}

fn mask_pic_and_pit(st: &ApicState, madt: &MadtInfo) {
    arch::pic::disable_all();
    lapic_write(st.lapic_va, LAPIC_LVT_LINT0, LVT_MASKED);
    let gsi = apic::gsi_for_isa_irq(0, &madt.isos[..madt.iso_count]);
    mask_gsi(gsi);
}

fn emit_marker(mode: TimerMode) {
    let _ = writeln!(Serial, "{}{})", marker::TIME_LAPIC_PREFIX, mode.as_str());
}

fn unmask_pit_fallback() {
    let st = STATE.get();
    if st.lapic_va != 0 {
        // PIC virtual-wire: ExtINT on LINT0. Masked LINT0 (enable path)
        // swallows IRQ0 even after unmasking the 8259.
        lapic_write(st.lapic_va, LAPIC_LVT_LINT0, LVT_DELIVERY_EXTINT);
    }
    arch::pic::unmask(0);
}

/// Enable LAPIC and program every I/O APIC (masked). Timer armed later in [`prove`].
///
/// # Safety
/// IDT live, PIC remapped, LAPIC/IOAPIC UC, IRQs still off.
pub unsafe fn init() {
    let Some(info) = acpi_init::info() else {
        return;
    };
    let Some(madt) = info.madt.as_ref() else {
        return;
    };
    let Some(va) = (unsafe { enable_lapic(madt) }) else {
        return;
    };
    let st = unsafe { STATE.get_mut() };
    st.lapic_va = va;
    enum_ioapics(madt, st);
    mask_all_pins(st);
    apply_isos(st, madt);
    st.ready = true;
}

/// Start the preferred timer, wait for a fire, commit the marker, mask PIC.
///
/// `sti` must already have run. TSC-deadline → periodic (HPET ÷16) → PIT.
pub fn prove() {
    let want_td = cpuid_tsc_deadline();
    let st = unsafe { STATE.get_mut() };
    if !st.ready {
        st.mode = TimerMode::Pit;
        crate::per_cpu_init::set_timer_mode(TimerMode::Pit);
        unmask_pit_fallback();
        emit_marker(TimerMode::Pit);
        return;
    }
    let va = st.lapic_va;
    let tsc_per_ms = time_init::tsc_per_ms();

    if want_td && tsc_per_ms != 0 {
        TIMER_FIRES.store(0, Ordering::Relaxed);
        st.mode = TimerMode::TscDeadline;
        arm_tsc_deadline(va, tsc_per_ms);
        if wait_fires(0, PROVE_MS) {
            commit_lapic(st, TimerMode::TscDeadline);
            return;
        }
        disarm_timer(va);
        serial::line("vibeOS: time: tsc-deadline no ticks");
    }

    match calib_periodic(va) {
        Some(per_ms) => {
            TIMER_FIRES.store(0, Ordering::Relaxed);
            st.ticks_per_ms = per_ms;
            st.mode = TimerMode::Periodic;
            arm_periodic(va, per_ms);
            if wait_fires(0, PROVE_MS) {
                commit_lapic(st, TimerMode::Periodic);
                return;
            }
            disarm_timer(va);
            serial::line("vibeOS: time: periodic no ticks");
        }
        None => serial::line("vibeOS: time: periodic calib refused"),
    }

    st.mode = TimerMode::Pit;
    st.owns_tick = false;
    crate::per_cpu_init::set_timer_mode(TimerMode::Pit);
    unmask_pit_fallback();
    emit_marker(TimerMode::Pit);
}

fn commit_lapic(st: &mut ApicState, mode: TimerMode) {
    st.mode = mode;
    st.owns_tick = true;
    crate::per_cpu_init::set_timer_mode(mode);
    if let Some(madt) = acpi_init::info().and_then(|i| i.madt.as_ref()) {
        mask_pic_and_pit(st, madt);
    } else {
        arch::pic::disable_all();
    }
    emit_marker(mode);
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn timer_mode() -> TimerMode {
    STATE.get().mode
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn owns_tick() -> bool {
    STATE.get().owns_tick
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn timer_fires() -> u64 {
    TIMER_FIRES.load(Ordering::Relaxed)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn cpuid_has_tsc_deadline() -> bool {
    cpuid_tsc_deadline()
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn is_ready() -> bool {
    STATE.get().ready
}

/// Enable this CPU's LAPIC (INIT resets it). Same MMIO VA as the BSP.
///
/// # Safety
/// LAPIC page already UC. IF off.
pub unsafe fn enable_ap() {
    let Some(info) = acpi_init::info() else {
        return;
    };
    let Some(madt) = info.madt.as_ref() else {
        return;
    };
    let _ = unsafe { enable_lapic(madt) };
}

/// Arm this CPU's timer in the mode the BSP proved. PIT: no local tick.
pub fn arm_ap() {
    let st = STATE.get();
    if st.lapic_va == 0 {
        return;
    }
    match st.mode {
        TimerMode::TscDeadline => arm_tsc_deadline(st.lapic_va, time_init::tsc_per_ms()),
        TimerMode::Periodic => {
            if st.ticks_per_ms != 0 {
                arm_periodic(st.lapic_va, st.ticks_per_ms);
            }
        }
        TimerMode::Pit => {}
    }
}
