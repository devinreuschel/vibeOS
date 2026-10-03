//! LAPIC / I/O APIC encodings and the ICR poll. DESIGN §5.6–5.8 / §7.2.
//!
//! Portable half: register bits, IPI ICR, IOAPIC redir (high before low),
//! ISO polarity/trigger, timer mode names. MMIO lives in the binary crate.

use crate::acpi::Iso;
use crate::time::FS_PER_MS;
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
/// ICR bits 18–19: 00 dest in ICR high, 01 self, 10 all, 11 all-ex-self.
pub const ICR_SHORTHAND_SHIFT: u32 = 18;
pub const ICR_SHORTHAND_NONE: u32 = 0;
pub const ICR_SHORTHAND_SELF: u32 = 0b01 << ICR_SHORTHAND_SHIFT;
pub const ICR_SHORTHAND_ALL: u32 = 0b10 << ICR_SHORTHAND_SHIFT;
pub const ICR_SHORTHAND_ALL_EX_SELF: u32 = 0b11 << ICR_SHORTHAND_SHIFT;
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
    /// NMI delivery: edge, no level assert, and the vector field ignored
    /// (sent as 0). The self shorthand is allowed only with Fixed delivery
    /// (Intel SDM Vol. 3A, the ICR's valid-combinations table), so a CPU
    /// sends its own NMI to its APIC id.
    Nmi,
}

impl IpiMode {
    pub const fn delivery_bits(self) -> u32 {
        match self {
            IpiMode::Fixed => 0b000,
            IpiMode::Init => 0b101,
            IpiMode::Sipi => 0b110,
            IpiMode::Nmi => 0b100,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum IpiError {
    DeliveryPendingTimeout,
    NotReady,
    NoRoute,
}

/// An IPI that timed out is a device failure; one not ready yet may be retried.
impl From<IpiError> for crate::kerror::KError {
    fn from(e: IpiError) -> Self {
        match e {
            IpiError::DeliveryPendingTimeout => Self::Io,
            IpiError::NotReady => Self::Again,
            IpiError::NoRoute => Self::NoDev,
        }
    }
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

#[repr(u8)]
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

/// The LAPIC in-service register (ISR) word that holds `vec`'s bit, as
/// (register offset, bit mask): eight 32-bit registers at `0x100` to
/// `0x170`, 16 bytes apart, vector `v` in bit `v % 32` of register `v / 32`
/// (Intel SDM Vol. 3A, 11.8.4).
pub const fn isr_reg(vec: u8) -> (u32, u32) {
    (0x100 + 0x10 * (vec as u32 / 32), 1 << (vec as u32 % 32))
}

/// Where an interrupt no handler owns gets its EOI (DESIGN §5.2): the LAPIC
/// when its in-service bit for `vec` is set (`in_service`), whatever the
/// vector, since a self-IPI or an I/O APIC route can deliver any vector
/// there; else the 8259 for its range `0x20`-`0x2F`; else none.
pub const fn unowned_eoi(vec: u8, in_service: bool) -> EoiDomain {
    if in_service {
        EoiDomain::Lapic
    } else if vec >= vectors::IRQ_BASE && vec <= vectors::IRQ_SPURIOUS_SLAVE {
        EoiDomain::Pic
    } else {
        EoiDomain::None
    }
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
        IpiMode::Nmi => {}
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

pub fn send_ipi_plan(dest: u8, vector: u8, mode: IpiMode) -> (u32, u32) {
    (icr_high(dest), icr_low(vector, mode))
}

pub const fn icr_low_shorthand(vector: u8, mode: IpiMode, shorthand: u32) -> u32 {
    icr_low(vector, mode) | shorthand
}

pub fn send_ipi_all_ex_self_plan(vector: u8, mode: IpiMode) -> (u32, u32) {
    (
        0,
        icr_low_shorthand(vector, mode, ICR_SHORTHAND_ALL_EX_SELF),
    )
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

/// One read of the LAPIC timer's current count, placed on the HPET by the
/// main-counter reads just before and just after it, each 32 bits wide as
/// the kernel reads the counter (`time_init::hpet_read_main`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CountRead {
    pub hpet_lo: u64,
    pub count: u32,
    pub hpet_hi: u64,
}

impl CountRead {
    /// HPET ticks between the reads around the count read.
    pub fn width(self) -> u64 {
        self.hpet_hi.wrapping_sub(self.hpet_lo) & u64::from(u32::MAX)
    }

    /// The HPET at the count read, as the middle of its bracket.
    pub fn hpet_mid(self) -> u64 {
        self.hpet_lo.wrapping_add(self.width() / 2) & u64::from(u32::MAX)
    }

    /// The narrower of two reads, `self` on a tie.
    pub fn narrower(self, other: CountRead) -> CountRead {
        if other.width() < self.width() {
            other
        } else {
            self
        }
    }
}

/// LAPIC timer counts per millisecond from reads `a` and `b` of one
/// one-shot count, on an HPET of `period_fs`: the counts it ran down over
/// the HPET ticks between the reads' bracket middles
/// (`apic_init::calib_periodic`). The window is the one the HPET measured,
/// so a stall that keeps the CPU away past its planned end lengthens both
/// sides of the ratio, and a stall inside a bracket widens it, so the
/// narrowest pick drops it. None when the count did not fall or no HPET
/// tick lies between the reads.
pub fn lapic_per_ms(a: CountRead, b: CountRead, period_fs: u32) -> Option<u64> {
    let counts = a.count.checked_sub(b.count)?;
    let ticks = b.hpet_mid().wrapping_sub(a.hpet_mid()) & u64::from(u32::MAX);
    let fs = u128::from(ticks).checked_mul(u128::from(period_fs))?;
    if counts == 0 || fs == 0 {
        return None;
    }
    let v = u128::from(counts).checked_mul(FS_PER_MS)? / fs;
    u64::try_from(v).ok()
}

/// PIT interrupts after which `apic_init::prove` takes a LAPIC timer it
/// armed, and which has not fired, for one that will not (DESIGN §6.3).
/// The PIT ticks at about 1 kHz and the timer is armed for 1 ms, so a timer
/// that works fires within the PIT's first few interrupts; the rest is
/// margin.
pub const PROVE_PIT_FIRES: u64 = 20;

/// What `apic_init::prove` knows of the LAPIC timer it armed, from the
/// interrupts the timer and the PIT, its witness, delivered since.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerProof {
    /// Neither has decided it: wait for the next interrupt.
    Pending,
    /// The timer fired: it is the tick.
    Fires,
    /// The PIT fired [`PROVE_PIT_FIRES`] times and the timer never.
    Silent,
}

/// The proof from `lapic_fires` timer and `pit_fires` PIT interrupts since
/// the arm. A timer fire decides it, however many PIT fires came first:
/// interrupts, not elapsed time, are the evidence, so a host that holds
/// both back, as one that deschedules QEMU does, cannot decide it.
pub const fn timer_proof(lapic_fires: u64, pit_fires: u64) -> TimerProof {
    if lapic_fires != 0 {
        TimerProof::Fires
    } else if pit_fires >= PROVE_PIT_FIRES {
        TimerProof::Silent
    } else {
        TimerProof::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isr_reg_offsets() {
        assert_eq!(isr_reg(0), (0x100, 1));
        assert_eq!(isr_reg(0x25), (0x110, 1 << 5));
        assert_eq!(isr_reg(0x85), (0x140, 1 << 5));
        assert_eq!(isr_reg(0xFF), (0x170, 1 << 31));
    }

    #[test]
    fn unowned_eoi_choice() {
        for v in 0..=255u8 {
            assert_eq!(
                unowned_eoi(v, true),
                EoiDomain::Lapic,
                "vector {v:#x} in service"
            );
            let want = if (0x20..=0x2F).contains(&v) {
                EoiDomain::Pic
            } else {
                EoiDomain::None
            };
            assert_eq!(unowned_eoi(v, false), want, "vector {v:#x}");
        }
    }
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
                if n < 4 { ICR_DELIVERY_PENDING } else { 0 }
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
        let (hi, lo) = send_ipi_all_ex_self_plan(vectors::IPI_SHOOTDOWN, IpiMode::Fixed);
        assert_eq!(hi, 0);
        assert_eq!(lo & 0xFF, vectors::IPI_SHOOTDOWN as u32);
        assert_eq!(lo & ICR_SHORTHAND_ALL_EX_SELF, ICR_SHORTHAND_ALL_EX_SELF);
    }

    #[test]
    fn icr_nmi_delivery_bits() {
        assert_eq!(IpiMode::Nmi.delivery_bits(), 0b100);
        let (hi, lo) = send_ipi_plan(3, 0, IpiMode::Nmi);
        assert_eq!(hi, 3u32 << 24);
        assert_eq!((lo >> 8) & 7, 0b100);
        assert_eq!(lo & 0xFF, 0, "the vector field is ignored and sent as 0");
        assert_eq!(
            lo & (ICR_LEVEL_ASSERT | ICR_TRIGGER_LEVEL),
            0,
            "edge, no level assert"
        );
        assert_eq!(
            lo & ICR_SHORTHAND_ALL_EX_SELF,
            0,
            "physical destination, no shorthand"
        );
    }

    #[test]
    fn redir_writes_high_dword_before_low() {
        let mut log: Vec<(u8, u32)> = Vec::new();
        let high = redir_high(1);
        let low = redir_low(0x30, Trigger::Edge, Polarity::High, true);
        write_redir(|reg, val| log.push((reg, val)), 2, high, low);
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
        assert_ne!(
            lvt_timer_oneshot(vectors::LAPIC_TIMER, true) & LVT_MASKED,
            0
        );
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

    /// QEMU's HPET: 100 MHz, a 10 ns period.
    const HPET_10NS: u32 = 10_000_000;

    fn read(hpet: u64, count: u32, width: u64) -> CountRead {
        CountRead {
            hpet_lo: hpet,
            count,
            hpet_hi: hpet.wrapping_add(width) & u64::from(u32::MAX),
        }
    }

    #[test]
    fn lapic_per_ms_over_the_measured_window() {
        // 62,500 counts per ms (QEMU's 1 GHz bus, divide by 16) over the
        // 10 ms window `calib_periodic` plans.
        let a = read(1_000, 4_000_000_000, 10);
        let b = read(1_001_000, 4_000_000_000 - 625_000, 10);
        assert_eq!(lapic_per_ms(a, b, HPET_10NS), Some(62_500));
        // The CPU came back 80 ms after the window's planned end: the HPET
        // saw 90 ms and the count fell 90 ms' worth, so the rate holds.
        let late = read(9_001_000, 4_000_000_000 - 5_625_000, 10);
        assert_eq!(lapic_per_ms(a, late, HPET_10NS), Some(62_500));
    }

    #[test]
    fn lapic_per_ms_places_each_read_by_its_bracket() {
        // A stall between the HPET read and the count read: the bracket
        // spans it, and its middle is where the count read is placed.
        let a = read(1_000, 4_000_000_000, 10);
        let stalled = read(1_000_000, 4_000_000_000 - 625_000, 2_000);
        let tight = read(1_000_995, 4_000_000_000 - 625_000, 10);
        assert_eq!(stalled.narrower(tight), tight);
        assert_eq!(tight.narrower(stalled), tight);
        assert_eq!(tight.narrower(tight), tight);
        assert_eq!(lapic_per_ms(a, tight, HPET_10NS), Some(62_500));
    }

    #[test]
    fn lapic_per_ms_across_the_32_bit_wrap() {
        let lo = u64::from(u32::MAX) - 500_000;
        let a = read(lo, 1_000_000, 10);
        let b = read(
            (lo + 1_000_000) & u64::from(u32::MAX),
            1_000_000 - 625_000,
            10,
        );
        assert!(b.hpet_lo < a.hpet_lo);
        assert_eq!(lapic_per_ms(a, b, HPET_10NS), Some(62_500));
        let straddle = read(u64::from(u32::MAX) - 4, 7, 10);
        assert_eq!(straddle.width(), 10);
        assert_eq!(straddle.hpet_mid(), 0);
    }

    #[test]
    fn lapic_per_ms_refuses_a_count_that_did_not_fall() {
        let a = read(1_000, 500, 10);
        assert_eq!(lapic_per_ms(a, read(1_001_000, 501, 10), HPET_10NS), None);
        assert_eq!(lapic_per_ms(a, read(1_001_000, 500, 10), HPET_10NS), None);
        // No HPET tick between the two middles.
        assert_eq!(lapic_per_ms(a, read(1_000, 400, 10), HPET_10NS), None);
        assert_eq!(lapic_per_ms(a, read(1_001_000, 400, 10), 0), None);
    }

    #[test]
    fn timer_proof_counts_interrupts() {
        assert_eq!(timer_proof(0, 0), TimerProof::Pending);
        assert_eq!(timer_proof(0, PROVE_PIT_FIRES - 1), TimerProof::Pending);
        assert_eq!(timer_proof(0, PROVE_PIT_FIRES), TimerProof::Silent);
        assert_eq!(timer_proof(0, u64::MAX), TimerProof::Silent);
        assert_eq!(timer_proof(1, 0), TimerProof::Fires);
        // A fire outweighs any PIT count: a late timer is not a silent one.
        assert_eq!(timer_proof(1, PROVE_PIT_FIRES), TimerProof::Fires);
        assert_eq!(timer_proof(u64::MAX, u64::MAX), TimerProof::Fires);
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
