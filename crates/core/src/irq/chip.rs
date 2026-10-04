//! Portable IRQ ids and the `IrqChip` seam. DESIGN §5.4.
//!
//! An [`IrqId`] is a software number. It is not a vector or an INTID. A
//! chip owns the hardware number (hwirq) recorded beside it.

use crate::apic::{Polarity, Trigger};
use crate::vectors;

pub use crate::limits::MAX_IRQS;

/// An IRQ request's errno: no vector left is `ENOSPC`, as Linux's vector
/// matrix returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum IrqError {
    Exhausted,
    InIrq,
    BadVector,
    BadCpu,
    Busy,
    NoRoute,
}

impl From<IrqError> for crate::kerror::KError {
    fn from(e: IrqError) -> Self {
        match e {
            IrqError::Exhausted => Self::NoSpc,
            IrqError::InIrq | IrqError::BadVector | IrqError::BadCpu => Self::Inval,
            IrqError::Busy => Self::Busy,
            IrqError::NoRoute => Self::NoDev,
        }
    }
}

impl IrqError {
    pub fn as_str(self) -> &'static str {
        match self {
            IrqError::Exhausted => "exhausted",
            IrqError::InIrq => "in hard irq",
            IrqError::BadVector => "bad vector",
            IrqError::BadCpu => "bad cpu",
            IrqError::Busy => "busy",
            IrqError::NoRoute => "no route",
        }
    }
}

/// Cap on one [`IrqSet`] (one device's MSI/MSI-X allocation).
pub const IRQ_SET_MAX: usize = 8;

/// Software IRQ id. Zero is none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct IrqId(u32);

impl IrqId {
    pub const NONE: Self = Self(0);

    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Table index, or `None` for [`Self::NONE`] or a value past [`MAX_IRQS`].
    pub const fn slot(self) -> Option<usize> {
        if self.0 == 0 || (self.0 as usize) > MAX_IRQS {
            None
        } else {
            Some((self.0 as usize) - 1)
        }
    }
}

/// LAPIC LVT sources [`IrqTable::map_percpu`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LapicLvt {
    Timer,
    Error,
    Thermal,
}

impl LapicLvt {
    pub const fn hwirq(self) -> u32 {
        match self {
            Self::Timer => vectors::LAPIC_TIMER as u32,
            Self::Error => vectors::LAPIC_ERROR as u32,
            Self::Thermal => vectors::LAPIC_THERMAL as u32,
        }
    }
}

/// Firmware or CPU-local specifier a chip translates to a hwirq.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqSpecifier {
    Gsi {
        gsi: u32,
        trigger: Trigger,
        polarity: Polarity,
    },
    Isa {
        line: u8,
    },
    LapicLvt(LapicLvt),
    /// GIC INTID: SGI 0–15, PPI 16–31, SPI 32–1019, LPI ≥ 8192.
    Gic {
        intid: u32,
    },
}

/// Address and data a chip writes into an MSI/MSI-X entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiMessage {
    pub addr: u64,
    pub data: u32,
}

/// A short list of [`IrqId`]s from one MSI allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrqSet {
    irqs: [IrqId; IRQ_SET_MAX],
    len: u8,
}

impl IrqSet {
    pub const fn empty() -> Self {
        Self {
            irqs: [IrqId::NONE; IRQ_SET_MAX],
            len: 0,
        }
    }

    pub fn push(&mut self, irq: IrqId) -> Result<(), IrqError> {
        let i = self.len as usize;
        let Some(slot) = self.irqs.get_mut(i) else {
            return Err(IrqError::Exhausted);
        };
        *slot = irq;
        self.len = match self.len.checked_add(1) {
            Some(n) => n,
            None => return Err(IrqError::Exhausted),
        };
        Ok(())
    }

    pub const fn len(&self) -> usize {
        self.len as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, i: usize) -> Option<IrqId> {
        if i < self.len as usize {
            self.irqs.get(i).copied()
        } else {
            None
        }
    }
}

/// One interrupt controller. Object-safe: each port holds several as
/// `&'static dyn IrqChip` (DESIGN §5.4).
pub trait IrqChip: Send + Sync {
    fn translate(&self, spec: IrqSpecifier, cpu: u32) -> Result<u32, IrqError>;
    fn mask(&self, hwirq: u32);
    fn unmask(&self, hwirq: u32);
    fn eoi(&self, hwirq: u32);
    fn set_affinity(&self, hwirq: u32, cpu: u32) -> Result<(), IrqError>;
    /// Fill `out` with `n` MSI hwirqs targeted at `cpu`. Returns how many.
    fn alloc_msi(&self, n: u8, cpu: u32, out: &mut [u32]) -> Result<usize, IrqError>;
    fn compose_msi(&self, hwirq: u32, cpu: u32) -> Result<MsiMessage, IrqError>;
    fn free(&self, hwirq: u32);
}

#[derive(Clone, Copy)]
struct IrqSlot {
    chip: Option<&'static dyn IrqChip>,
    hwirq: u32,
    cpu: u32,
    used: bool,
}

/// Software IRQ table: bind, lookup, free. Host tests drive it against a
/// stub chip; the kernel wraps one in the IRQ lock.
pub struct IrqTable {
    slots: [IrqSlot; MAX_IRQS],
}

impl IrqTable {
    pub const fn new() -> Self {
        Self {
            slots: [IrqSlot {
                chip: None,
                hwirq: 0,
                cpu: 0,
                used: false,
            }; MAX_IRQS],
        }
    }

    pub fn free_slots(&self) -> usize {
        self.slots.iter().filter(|s| !s.used).count()
    }

    pub fn bind(
        &mut self,
        chip: &'static dyn IrqChip,
        hwirq: u32,
        cpu: u32,
    ) -> Result<IrqId, IrqError> {
        let mut i = 0usize;
        while i < MAX_IRQS {
            if let Some(s) = self.slots.get_mut(i)
                && !s.used
            {
                s.used = true;
                s.chip = Some(chip);
                s.hwirq = hwirq;
                s.cpu = cpu;
                return Ok(IrqId::from_raw((i as u32).saturating_add(1)));
            }
            i += 1;
        }
        Err(IrqError::Exhausted)
    }

    fn slot(&self, irq: IrqId) -> Result<&IrqSlot, IrqError> {
        let i = irq.slot().ok_or(IrqError::BadVector)?;
        let s = self.slots.get(i).ok_or(IrqError::BadVector)?;
        if !s.used {
            return Err(IrqError::BadVector);
        }
        Ok(s)
    }

    fn slot_mut(&mut self, irq: IrqId) -> Result<&mut IrqSlot, IrqError> {
        let i = irq.slot().ok_or(IrqError::BadVector)?;
        let s = self.slots.get_mut(i).ok_or(IrqError::BadVector)?;
        if !s.used {
            return Err(IrqError::BadVector);
        }
        Ok(s)
    }

    pub fn chip(&self, irq: IrqId) -> Option<&'static dyn IrqChip> {
        self.slot(irq).ok().and_then(|s| s.chip)
    }

    pub fn hwirq(&self, irq: IrqId) -> Option<u32> {
        self.slot(irq).ok().map(|s| s.hwirq)
    }

    pub fn cpu_of(&self, irq: IrqId) -> Option<u32> {
        self.slot(irq).ok().map(|s| s.cpu)
    }

    /// Record dest CPU only. The kernel calls the chip after dropping the
    /// IRQ lock so chip code can take it.
    pub fn set_cpu(&mut self, irq: IrqId, cpu: u32) -> Result<(), IrqError> {
        self.slot_mut(irq)?.cpu = cpu;
        Ok(())
    }

    pub fn unbind(&mut self, irq: IrqId) -> Result<(Option<&'static dyn IrqChip>, u32), IrqError> {
        let s = self.slot_mut(irq)?;
        let chip = s.chip;
        let hwirq = s.hwirq;
        s.used = false;
        s.chip = None;
        s.hwirq = 0;
        s.cpu = 0;
        Ok((chip, hwirq))
    }

    pub fn map_wired(
        &mut self,
        chip: &'static dyn IrqChip,
        spec: IrqSpecifier,
        cpu: u32,
    ) -> Result<IrqId, IrqError> {
        let hwirq = chip.translate(spec, cpu)?;
        match self.bind(chip, hwirq, cpu) {
            Ok(irq) => Ok(irq),
            Err(e) => {
                chip.free(hwirq);
                Err(e)
            }
        }
    }

    pub fn alloc_msi(
        &mut self,
        chip: &'static dyn IrqChip,
        n: u8,
        cpu: u32,
    ) -> Result<IrqSet, IrqError> {
        if n == 0 || (n as usize) > IRQ_SET_MAX {
            return Err(IrqError::BadVector);
        }
        let mut hw = [0u32; IRQ_SET_MAX];
        let Some(out) = hw.get_mut(..n as usize) else {
            return Err(IrqError::BadVector);
        };
        let got = chip.alloc_msi(n, cpu, out)?;
        if self.free_slots() < got {
            for h in hw.iter().take(got) {
                chip.free(*h);
            }
            return Err(IrqError::Exhausted);
        }
        let mut set = IrqSet::empty();
        let mut i = 0usize;
        while i < got {
            let Some(h) = hw.get(i).copied() else {
                self.unwind_set(&set, chip);
                return Err(IrqError::BadVector);
            };
            match self.bind(chip, h, cpu) {
                Ok(irq) => {
                    if set.push(irq).is_err() {
                        if let Ok((_, h2)) = self.unbind(irq) {
                            chip.free(h2);
                        }
                        self.unwind_set(&set, chip);
                        return Err(IrqError::Exhausted);
                    }
                }
                Err(e) => {
                    chip.free(h);
                    self.unwind_set(&set, chip);
                    return Err(e);
                }
            }
            i += 1;
        }
        Ok(set)
    }

    fn unwind_set(&mut self, set: &IrqSet, chip: &'static dyn IrqChip) {
        let mut i = 0usize;
        while i < set.len() {
            if let Some(irq) = set.get(i)
                && let Ok((_, h)) = self.unbind(irq)
            {
                chip.free(h);
            }
            i += 1;
        }
    }

    pub fn map_percpu(
        &mut self,
        chip: &'static dyn IrqChip,
        spec: IrqSpecifier,
    ) -> Result<IrqId, IrqError> {
        self.map_wired(chip, spec, 0)
    }

    pub fn set_affinity(&mut self, irq: IrqId, cpu: u32) -> Result<(), IrqError> {
        let (chip, hwirq) = {
            let s = self.slot(irq)?;
            (s.chip.ok_or(IrqError::BadVector)?, s.hwirq)
        };
        self.set_cpu(irq, cpu)?;
        chip.set_affinity(hwirq, cpu)
    }

    pub fn free(&mut self, irq: IrqId) -> Result<(), IrqError> {
        let (chip, hwirq) = self.unbind(irq)?;
        if let Some(chip) = chip {
            chip.free(hwirq);
        }
        Ok(())
    }
}

impl Default for IrqTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::{msi_message_addr, msi_message_data};
    use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};

    const STUB_HW: usize = 256;
    const STUB_MSI_BASE: u32 = 16;

    struct StubChip {
        next: AtomicU32,
        cpu: [AtomicU32; STUB_HW],
        live: [AtomicU8; STUB_HW],
    }

    impl StubChip {
        const fn new() -> Self {
            Self {
                next: AtomicU32::new(STUB_MSI_BASE),
                cpu: [const { AtomicU32::new(0) }; STUB_HW],
                live: [const { AtomicU8::new(0) }; STUB_HW],
            }
        }

        fn at<T>(row: &[T], hwirq: u32) -> Result<&T, IrqError> {
            row.get(hwirq as usize).ok_or(IrqError::BadVector)
        }

        fn claim(&self, hwirq: u32, cpu: u32) -> Result<(), IrqError> {
            let live = Self::at(&self.live, hwirq)?;
            if live.swap(1, Ordering::Relaxed) != 0 {
                return Err(IrqError::Busy);
            }
            Self::at(&self.cpu, hwirq)?.store(cpu, Ordering::Relaxed);
            Ok(())
        }

        fn cpu_of(&self, hwirq: u32) -> Option<u32> {
            let live = Self::at(&self.live, hwirq).ok()?;
            if live.load(Ordering::Relaxed) == 0 {
                None
            } else {
                Some(Self::at(&self.cpu, hwirq).ok()?.load(Ordering::Relaxed))
            }
        }

        fn is_free(&self, hwirq: u32) -> bool {
            Self::at(&self.live, hwirq)
                .map(|l| l.load(Ordering::Relaxed) == 0)
                .unwrap_or(true)
        }
    }

    impl IrqChip for StubChip {
        fn translate(&self, spec: IrqSpecifier, cpu: u32) -> Result<u32, IrqError> {
            let hwirq = match spec {
                IrqSpecifier::Gsi { gsi, .. } => gsi,
                IrqSpecifier::Isa { line } => 0x20u32.saturating_add(u32::from(line)),
                IrqSpecifier::LapicLvt(lvt) => lvt.hwirq(),
                IrqSpecifier::Gic { intid } => intid,
            };
            self.claim(hwirq, cpu)?;
            Ok(hwirq)
        }

        fn mask(&self, _hwirq: u32) {}

        fn unmask(&self, _hwirq: u32) {}

        fn eoi(&self, _hwirq: u32) {}

        fn set_affinity(&self, hwirq: u32, cpu: u32) -> Result<(), IrqError> {
            let live = Self::at(&self.live, hwirq)?;
            if live.load(Ordering::Relaxed) == 0 {
                return Err(IrqError::BadVector);
            }
            Self::at(&self.cpu, hwirq)?.store(cpu, Ordering::Relaxed);
            Ok(())
        }

        fn alloc_msi(&self, n: u8, cpu: u32, out: &mut [u32]) -> Result<usize, IrqError> {
            if n == 0 || out.len() < n as usize {
                return Err(IrqError::BadVector);
            }
            let mut got = 0usize;
            while got < n as usize {
                let h = self.next.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = self.claim(h, cpu) {
                    let mut j = 0usize;
                    while j < got {
                        if let Some(prev) = out.get(j) {
                            self.free(*prev);
                        }
                        j += 1;
                    }
                    return Err(e);
                }
                if let Some(slot) = out.get_mut(got) {
                    *slot = h;
                }
                got += 1;
            }
            Ok(got)
        }

        fn compose_msi(&self, hwirq: u32, cpu: u32) -> Result<MsiMessage, IrqError> {
            let apic = u8::try_from(cpu).map_err(|_| IrqError::BadCpu)?;
            let vec = u8::try_from(hwirq).map_err(|_| IrqError::BadVector)?;
            Ok(MsiMessage {
                addr: u64::from(msi_message_addr(apic)),
                data: msi_message_data(vec),
            })
        }

        fn free(&self, hwirq: u32) {
            if let Ok(live) = Self::at(&self.live, hwirq) {
                live.store(0, Ordering::Relaxed);
            }
            if let Ok(cpu) = Self::at(&self.cpu, hwirq) {
                cpu.store(0, Ordering::Relaxed);
            }
        }
    }

    #[test]
    fn fixed_tables_match_limits() {
        assert_eq!(MAX_IRQS, crate::limits::MAX_IRQS);
        assert_eq!(IrqTable::new().free_slots(), crate::limits::MAX_IRQS);
    }

    #[test]
    fn stub_chip_map_alloc_affinity_free() {
        let chip: &'static StubChip = Box::leak(Box::new(StubChip::new()));
        let mut table = IrqTable::new();
        let spec = IrqSpecifier::Gsi {
            gsi: 5,
            trigger: Trigger::Level,
            polarity: Polarity::Low,
        };
        let irq = table.map_wired(chip, spec, 0).unwrap();
        assert_ne!(irq, IrqId::NONE);
        assert_eq!(irq.slot(), Some(0));
        assert_eq!(table.cpu_of(irq), Some(0));
        assert_eq!(table.hwirq(irq), Some(5));
        assert_eq!(chip.cpu_of(5), Some(0));

        table.set_affinity(irq, 3).unwrap();
        assert_eq!(table.cpu_of(irq), Some(3));
        assert_eq!(chip.cpu_of(5), Some(3));

        let set = table.alloc_msi(chip, 2, 1).unwrap();
        assert_eq!(set.len(), 2);
        let a = set.get(0).unwrap();
        let b = set.get(1).unwrap();
        assert_ne!(a, irq);
        assert_ne!(a, b);
        assert_eq!(table.cpu_of(a), Some(1));
        assert_eq!(table.cpu_of(b), Some(1));
        let ha = table.hwirq(a).unwrap();
        let hb = table.hwirq(b).unwrap();
        assert_eq!(ha, STUB_MSI_BASE);
        assert_eq!(hb, STUB_MSI_BASE + 1);
        assert_eq!(chip.cpu_of(ha), Some(1));

        let gic = table
            .map_wired(chip, IrqSpecifier::Gic { intid: 27 }, 0)
            .unwrap();
        assert_eq!(table.hwirq(gic), Some(27));
        table.free(gic).unwrap();

        let tick = table
            .map_percpu(chip, IrqSpecifier::LapicLvt(LapicLvt::Timer))
            .unwrap();
        assert_eq!(table.hwirq(tick), Some(u32::from(vectors::LAPIC_TIMER)));

        table.free(irq).unwrap();
        assert_eq!(table.cpu_of(irq), None);
        assert!(chip.is_free(5));
        table.free(a).unwrap();
        table.free(b).unwrap();
        table.free(tick).unwrap();
        assert!(chip.is_free(ha));
        assert!(chip.is_free(u32::from(vectors::LAPIC_TIMER)));

        let again = table.map_wired(chip, spec, 2).unwrap();
        assert_eq!(table.hwirq(again), Some(5));
        assert_eq!(table.cpu_of(again), Some(2));
        table.free(again).unwrap();
    }
}
