//! LAPIC / I/O APIC encodings and the ICR poll. DESIGN §5.6–5.8 / §7.2.
//!
//! Portable half: register bits, IPI ICR, IOAPIC redir (high before low),
//! ISO polarity/trigger, timer mode names. MMIO lives in the binary crate.

use crate::acpi::Iso;
use crate::vectors;

/// `IA32_APIC_BASE` (MSR `0x1B`). Bit 11 global enable; bits 12+ base.
pub const IA32_APIC_BASE: u32 = 0x1B;
pub const APIC_BASE_BSP: u64 = 1 << 8;
pub const APIC_BASE_X2APIC: u64 = 1 << 10;
pub const APIC_BASE_ENABLE: u64 = 1 << 11;
pub const APIC_BASE_MASK: u64 = !0xFFF;

/// `IA32_TSC_DEADLINE`. Write 0 to disarm. DESIGN §6.1.
pub const IA32_TSC_DEADLINE: u32 = 0x6E0;

pub const DEFAULT_LAPIC_PHYS: u64 = 0xFEE0_0000;

pub const LAPIC_ID: u32 = 0x020;
pub const LAPIC_TPR: u32 = 0x080;
pub const LAPIC_EOI: u32 = 0x0B0;
pub const LAPIC_SVR: u32 = 0x0F0;
pub const LAPIC_ESR: u32 = 0x280;
pub const LAPIC_ICR_LOW: u32 = 0x300;
pub const LAPIC_ICR_HIGH: u32 = 0x310;
pub const LAPIC_LVT_TIMER: u32 = 0x320;
pub const LAPIC_LVT_THERMAL: u32 = 0x330;
pub const LAPIC_LVT_PERF: u32 = 0x340;
pub const LAPIC_LVT_LINT0: u32 = 0x350;
pub const LAPIC_LVT_LINT1: u32 = 0x360;
pub const LAPIC_LVT_ERROR: u32 = 0x370;
pub const LAPIC_TIMER_ICR: u32 = 0x380;
pub const LAPIC_TIMER_CCR: u32 = 0x390;
pub const LAPIC_TIMER_DCR: u32 = 0x3E0;

pub const SVR_ENABLE: u32 = 1 << 8;
pub const LVT_MASKED: u32 = 1 << 16;
/// LINT0 ExtINT: PIC virtual-wire when the LAPIC timer is not the tick.
pub const LVT_DELIVERY_EXTINT: u32 = 0b111 << 8;
/// LVT timer bits 17:18: 00 one-shot, 01 periodic, 10 TSC-deadline.
pub const LVT_TIMER_PERIODIC: u32 = 1 << 17;
pub const LVT_TIMER_TSC_DEADLINE: u32 = 1 << 18;

/// Divide configuration: 0b0011 = ÷16. ROADMAP §4.3.
pub const TIMER_DIV_16: u32 = 0b0011;

pub const ICR_DELIVERY_PENDING: u32 = 1 << 12;
pub const ICR_LEVEL_ASSERT: u32 = 1 << 14;
pub const ICR_TRIGGER_LEVEL: u32 = 1 << 15;
/// Bound so a stuck ICR under `cli` cannot hang silently. DESIGN §7.2.
pub const ICR_POLL_CAP: u32 = 1000;

pub const IOREGSEL: u32 = 0x00;
pub const IOWIN: u32 = 0x10;
pub const IOAPIC_ID: u8 = 0;
pub const IOAPIC_VER: u8 = 1;
pub const IOAPIC_REDIR_BASE: u8 = 0x10;

pub const REDIR_POLARITY_LOW: u32 = 1 << 13;
pub const REDIR_TRIGGER_LEVEL: u32 = 1 << 15;
pub const REDIR_MASK: u32 = 1 << 16;

/// CPUID.01H:ECX[24].
pub const CPUID_ECX_TSC_DEADLINE: u32 = 1 << 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpiMode {
    Fixed,
    Init,
    Sipi,
}

impl IpiMode {
    pub const fn delivery_bits(self) -> u32 {
        match self {
            IpiMode::Fixed => 0b000,
            IpiMode::Init => 0b101,
            IpiMode::Sipi => 0b110,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpiError {
    DeliveryPendingTimeout,
    NotReady,
    NoRoute,
}

impl IpiError {
    pub const fn as_str(self) -> &'static str {
        match self {
            IpiError::DeliveryPendingTimeout => "delivery pending timeout",
            IpiError::NotReady => "not ready",
            IpiError::NoRoute => "no ioapic route",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    High,
    Low,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Edge,
    Level,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerMode {
    TscDeadline,
    Periodic,
    Pit,
}

impl TimerMode {
    /// ROADMAP / #26 marker payload. Not DESIGN's older synonyms.
    pub const fn as_str(self) -> &'static str {
        match self {
            TimerMode::TscDeadline => "tsc-deadline",
            TimerMode::Periodic => "periodic",
            TimerMode::Pit => "pit",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EoiDomain {
    None,
    Pic,
    Lapic,
}

/// PIC vs APIC EOI. Spurious `0xFF` never EOIs. DESIGN §5.7.
pub const fn eoi_domain(vec: u8) -> EoiDomain {
    if vec == vectors::LAPIC_SPURIOUS {
        EoiDomain::None
    } else if vec >= vectors::IRQ_BASE && vec <= vectors::IRQ_SPURIOUS_SLAVE {
        EoiDomain::Pic
    } else if vec >= vectors::IRQ_BASE {
        EoiDomain::Lapic
    } else {
        EoiDomain::None
    }
}

pub const fn svr_value() -> u32 {
    SVR_ENABLE | vectors::LAPIC_SPURIOUS as u32
}

pub const fn lvt_error_value() -> u32 {
    vectors::LAPIC_ERROR as u32
}

pub const fn lvt_thermal_value() -> u32 {
    vectors::LAPIC_THERMAL as u32
}

pub const fn lvt_timer_oneshot(vec: u8, masked: bool) -> u32 {
    let mut v = vec as u32;
    if masked {
        v |= LVT_MASKED;
    }
    v
}

pub const fn lvt_timer_periodic(vec: u8, masked: bool) -> u32 {
    lvt_timer_oneshot(vec, masked) | LVT_TIMER_PERIODIC
}

pub const fn lvt_timer_tsc_deadline(vec: u8, masked: bool) -> u32 {
    lvt_timer_oneshot(vec, masked) | LVT_TIMER_TSC_DEADLINE
}

/// Merge MADT type-5 / header base into `IA32_APIC_BASE`. Keeps flag bits.
pub const fn apic_base_msr(current: u64, phys: u64) -> u64 {
    let flags = current & 0xFFF;
    let flags = (flags & !APIC_BASE_X2APIC) | APIC_BASE_ENABLE;
    flags | (phys & APIC_BASE_MASK)
}

pub const fn icr_high(dest: u8) -> u32 {
    (dest as u32) << 24
}

pub const fn icr_low(vector: u8, mode: IpiMode) -> u32 {
    let mut v = vector as u32 | (mode.delivery_bits() << 8);
    match mode {
        IpiMode::Fixed => {}
        IpiMode::Init => {
            v |= ICR_LEVEL_ASSERT | ICR_TRIGGER_LEVEL;
        }
        IpiMode::Sipi => {}
    }
    v
}

/// Poll ICR delivery-pending (bit 12). `true` if it cleared, `false` at `cap`.
pub fn poll_delivery_pending<F>(mut read_icr_low: F, cap: u32) -> bool
where
    F: FnMut() -> u32,
{
    let mut i = 0;
    while i < cap {
        if read_icr_low() & ICR_DELIVERY_PENDING == 0 {
            return true;
        }
        i += 1;
    }
    false
}

pub fn send_ipi_plan(
    dest: u8,
    vector: u8,
    mode: IpiMode,
) -> (u32, u32) {
    (icr_high(dest), icr_low(vector, mode))
}

/// VER bits 16–23 are the last redirection index, not the count.
pub const fn ioapic_max_index(ver: u32) -> u32 {
    (ver >> 16) & 0xFF
}

pub const fn ioapic_redir_regs(pin: u8) -> (u8, u8) {
    let low = IOAPIC_REDIR_BASE.wrapping_add(pin.wrapping_mul(2));
    (low, low.wrapping_add(1))
}

/// High dword (dest) before low (mask lives in low). DESIGN §5.6.
pub fn write_redir<W>(mut write_reg: W, pin: u8, high: u32, low: u32)
where
    W: FnMut(u8, u32),
{
    let (lo, hi) = ioapic_redir_regs(pin);
    write_reg(hi, high);
    write_reg(lo, low);
}

pub const fn redir_high(apic_id: u8) -> u32 {
    (apic_id as u32) << 24
}

pub const fn redir_low(vector: u8, trigger: Trigger, polarity: Polarity, masked: bool) -> u32 {
    let mut v = vector as u32;
    match polarity {
        Polarity::High => {}
        Polarity::Low => v |= REDIR_POLARITY_LOW,
    }
    match trigger {
        Trigger::Edge => {}
        Trigger::Level => v |= REDIR_TRIGGER_LEVEL,
    }
    if masked {
        v |= REDIR_MASK;
    }
    v
}

pub const fn redir_set_mask(low: u32, masked: bool) -> u32 {
    if masked {
        low | REDIR_MASK
    } else {
        low & !REDIR_MASK
    }
}

pub const fn redir_is_masked(low: u32) -> bool {
    low & REDIR_MASK != 0
}

/// MPS INTI: 00 conform (ISA high/edge), 01 high/edge, 11 low/level.
pub const fn iso_polarity(flags: u16) -> Polarity {
    match flags & 0b11 {
        0b11 => Polarity::Low,
        _ => Polarity::High,
    }
}

pub const fn iso_trigger(flags: u16) -> Trigger {
    match (flags >> 2) & 0b11 {
        0b11 => Trigger::Level,
        _ => Trigger::Edge,
    }
}

/// Never IRQ *n* = GSI *n* without looking at ISOs. DESIGN §5.6.
pub fn gsi_for_isa_irq(irq: u8, isos: &[Iso]) -> u32 {
    let mut i = 0;
    while i < isos.len() {
        if isos[i].irq == irq {
            return isos[i].gsi;
        }
        i += 1;
    }
    irq as u32
}

pub fn ioapic_pin(gsi: u32, gsi_base: u32, max_index: u32) -> Option<u8> {
    if gsi < gsi_base {
        return None;
    }
    let pin = gsi - gsi_base;
    if pin > max_index || pin > u8::MAX as u32 {
        None
    } else {
        Some(pin as u8)
    }
}

pub const fn has_tsc_deadline(ecx: u32) -> bool {
    ecx & CPUID_ECX_TSC_DEADLINE != 0
}

/// 0 disarms TSC-deadline. Nudge a wrap to 1.
pub const fn tsc_deadline_value(now: u64, tsc_per_ms: u64) -> u64 {
    let d = now.wrapping_add(tsc_per_ms);
    if d == 0 { 1 } else { d }
}

/// Arm sequence for TSC-deadline. SDM Vol. 3A (local APIC timer,
/// TSC-deadline mode): LVT write, then `MFENCE` (or another serializing
/// insn), then `IA32_TSC_DEADLINE`. `lfence;rdtsc` / `rdtscp` do not
/// drain the UC LVT store, so the MSR write can retire against the old
/// masked one-shot and the first deadline is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TscDeadlineStep {
    Lvt(u32),
    Mfence,
    Deadline(u64),
}

pub fn tsc_deadline_arm_plan(vec: u8, now: u64, tsc_per_ms: u64) -> [TscDeadlineStep; 3] {
    [
        TscDeadlineStep::Lvt(lvt_timer_tsc_deadline(vec, false)),
        TscDeadlineStep::Mfence,
        TscDeadlineStep::Deadline(tsc_deadline_value(now, tsc_per_ms)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    #[test]
    fn poll_clears_on_first_read() {
        let mut n = 0u32;
        let ok = poll_delivery_pending(
            || {
                n += 1;
                0
            },
            ICR_POLL_CAP,
        );
        assert!(ok);
        assert_eq!(n, 1);
    }

    #[test]
    fn poll_clears_after_pending() {
        let mut n = 0u32;
        let ok = poll_delivery_pending(
            || {
                n += 1;
                if n < 4 {
                    ICR_DELIVERY_PENDING
                } else {
                    0
                }
            },
            ICR_POLL_CAP,
        );
        assert!(ok);
        assert_eq!(n, 4);
    }

    #[test]
    fn poll_timeout_at_cap() {
        let mut n = 0u32;
        let ok = poll_delivery_pending(
            || {
                n += 1;
                ICR_DELIVERY_PENDING
            },
            7,
        );
        assert!(!ok);
        assert_eq!(n, 7);
    }

    #[test]
    fn poll_cap_zero_is_timeout() {
        let mut n = 0u32;
        assert!(!poll_delivery_pending(
            || {
                n += 1;
                0
            },
            0,
        ));
        assert_eq!(n, 0);
    }

    #[test]
    fn icr_high_then_low_send_order() {
        let (hi, lo) = send_ipi_plan(4, vectors::IPI_RESCHEDULE, IpiMode::Fixed);
        assert_eq!(hi, 4u32 << 24);
        assert_eq!(lo & 0xFF, vectors::IPI_RESCHEDULE as u32);
        assert_eq!(lo & ICR_DELIVERY_PENDING, 0);
        let init = icr_low(0, IpiMode::Init);
        assert_eq!((init >> 8) & 7, 0b101);
        assert_ne!(init & ICR_LEVEL_ASSERT, 0);
        let sipi = icr_low(0x08, IpiMode::Sipi);
        assert_eq!(sipi & 0xFF, 0x08);
        assert_eq!((sipi >> 8) & 7, 0b110);
    }

    #[test]
    fn redir_writes_high_dword_before_low() {
        let mut log: Vec<(u8, u32)> = Vec::new();
        let high = redir_high(1);
        let low = redir_low(0x30, Trigger::Edge, Polarity::High, true);
        write_redir(
            |reg, val| log.push((reg, val)),
            2,
            high,
            low,
        );
        assert_eq!(log.len(), 2);
        let (lo, hi) = ioapic_redir_regs(2);
        assert_eq!(log[0], (hi, high));
        assert_eq!(log[1], (lo, low));
        assert!(redir_is_masked(low));
        assert_eq!(lo, 0x14);
        assert_eq!(hi, 0x15);
    }

    #[test]
    fn iso_irq0_is_not_gsi0() {
        let isos = [Iso {
            irq: 0,
            gsi: 2,
            flags: 0,
        }];
        assert_eq!(gsi_for_isa_irq(0, &isos), 2);
        assert_eq!(gsi_for_isa_irq(1, &isos), 1);
        assert_eq!(gsi_for_isa_irq(0, &[]), 0);
    }

    #[test]
    fn iso_flags_polarity_trigger() {
        assert_eq!(iso_polarity(0), Polarity::High);
        assert_eq!(iso_trigger(0), Trigger::Edge);
        assert_eq!(iso_polarity(0b01), Polarity::High);
        assert_eq!(iso_trigger(0b01 << 2), Trigger::Edge);
        assert_eq!(iso_polarity(0b11), Polarity::Low);
        assert_eq!(iso_trigger(0b11 << 2), Trigger::Level);
        let low = redir_low(0x21, Trigger::Level, Polarity::Low, false);
        assert_ne!(low & REDIR_POLARITY_LOW, 0);
        assert_ne!(low & REDIR_TRIGGER_LEVEL, 0);
        assert!(!redir_is_masked(low));
        assert!(redir_is_masked(redir_set_mask(low, true)));
    }

    #[test]
    fn eoi_domain_pic_vs_apic_vs_spurious() {
        assert_eq!(eoi_domain(vectors::IRQ_PIT), EoiDomain::Pic);
        assert_eq!(eoi_domain(vectors::IRQ_SPURIOUS_SLAVE), EoiDomain::Pic);
        assert_eq!(eoi_domain(vectors::LAPIC_TIMER), EoiDomain::Lapic);
        assert_eq!(eoi_domain(vectors::LAPIC_ERROR), EoiDomain::Lapic);
        assert_eq!(eoi_domain(vectors::DEVICE_VEC_START), EoiDomain::Lapic);
        assert_eq!(eoi_domain(vectors::LAPIC_SPURIOUS), EoiDomain::None);
        assert_eq!(eoi_domain(vectors::DF), EoiDomain::None);
    }

    #[test]
    fn svr_and_lvt_encodings() {
        assert_eq!(svr_value() & 0xFF, 0xFF);
        assert_ne!(svr_value() & SVR_ENABLE, 0);
        assert_eq!(lvt_error_value() & 0xFF, vectors::LAPIC_ERROR as u32);
        assert_eq!(
            lvt_timer_tsc_deadline(vectors::LAPIC_TIMER, false) & LVT_TIMER_TSC_DEADLINE,
            LVT_TIMER_TSC_DEADLINE,
        );
        assert_eq!(
            lvt_timer_periodic(vectors::LAPIC_TIMER, false) & LVT_TIMER_PERIODIC,
            LVT_TIMER_PERIODIC,
        );
        assert_ne!(lvt_timer_oneshot(vectors::LAPIC_TIMER, true) & LVT_MASKED, 0);
        assert_eq!(LVT_DELIVERY_EXTINT, 0b111 << 8);
    }

    #[test]
    fn apic_base_msr_enables_and_honors_override() {
        let cur = APIC_BASE_BSP | DEFAULT_LAPIC_PHYS;
        let next = apic_base_msr(cur, 0xDEAD_BEEF_0000);
        assert_ne!(next & APIC_BASE_ENABLE, 0);
        assert_eq!(next & APIC_BASE_X2APIC, 0);
        assert_eq!(next & APIC_BASE_MASK, 0xDEAD_BEEF_0000);
        assert_ne!(next & APIC_BASE_BSP, 0);
    }

    #[test]
    fn timer_mode_marker_spellings() {
        assert_eq!(TimerMode::TscDeadline.as_str(), "tsc-deadline");
        assert_eq!(TimerMode::Periodic.as_str(), "periodic");
        assert_eq!(TimerMode::Pit.as_str(), "pit");
        assert!(has_tsc_deadline(CPUID_ECX_TSC_DEADLINE));
        assert!(!has_tsc_deadline(0));
        assert_eq!(tsc_deadline_value(0, 0), 1);
        assert_eq!(tsc_deadline_value(10, 5), 15);
    }

    #[test]
    fn tsc_deadline_arm_mfence_before_wrmsr() {
        let plan = tsc_deadline_arm_plan(vectors::LAPIC_TIMER, 10, 5);
        assert_eq!(
            plan,
            [
                TscDeadlineStep::Lvt(lvt_timer_tsc_deadline(vectors::LAPIC_TIMER, false)),
                TscDeadlineStep::Mfence,
                TscDeadlineStep::Deadline(15),
            ]
        );
    }

    #[test]
    fn ipi_error_variants() {
        assert_eq!(
            IpiError::DeliveryPendingTimeout.as_str(),
            "delivery pending timeout",
        );
        assert_eq!(IpiError::NotReady.as_str(), "not ready");
        assert_eq!(IpiError::NoRoute.as_str(), "no ioapic route");
    }

    #[test]
    fn ioapic_pin_range() {
        assert_eq!(ioapic_pin(2, 0, 23), Some(2));
        assert_eq!(ioapic_pin(24, 0, 23), None);
        assert_eq!(ioapic_pin(24, 24, 23), Some(0));
        assert_eq!(ioapic_max_index(0x0017_0000), 23);
    }
}
