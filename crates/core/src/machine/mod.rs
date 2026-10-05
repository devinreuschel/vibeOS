//! Portable machine description (DESIGN §11.1, ROADMAP §11.5).
//!
//! ACPI (MADT, HPET, MCFG) and the device tree each fill [`MachineDesc`].
//! SMP bring-up, the IRQ layer, and the device registry read only it.

pub mod fdt;

use core::fmt;
use core::ops::Range;

use crate::acpi::{self, AcpiInfo, HpetInfo, IoApic, Iso, MadtInfo, McfgInfo};

/// Hardware ids one firmware table names (the u64 online mask).
pub const MAX_CPUS: usize = acpi::MAX_CPUS;
/// I/O APICs plus one GIC and its ITS.
pub const MAX_IRQ_CONTROLLERS: usize = 18;
/// HPET plus the generic timer.
pub const MAX_TIMERS: usize = 4;
/// Consoles the UART-pick function names.
pub const MAX_CONSOLES: usize = 2;
/// ECAM host bridges one firmware table names.
pub const MAX_PCI_HOSTS: usize = 4;
/// `/reserved-memory` children plus the FDT memreserve block.
pub const MAX_RESERVED: usize = 32;
/// QEMU `virt` virtio-mmio transports.
pub const MAX_VIRTIO_MMIO: usize = 32;
/// `msi-map` tuples on one host.
pub const MAX_MSI_MAP: usize = 8;
/// `interrupt-map` rows on one host (QEMU `virt`: 16).
pub const MAX_INTERRUPT_MAP: usize = 16;

/// How secondaries start (device-tree `/psci` or `enable-method`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnableMethod {
    None,
    /// PSCI `CPU_ON`. `hvc` is the `/psci` `method` (else SMC).
    Psci {
        hvc: bool,
    },
}

/// One CPU the firmware said is startable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuDesc {
    /// APIC ID or MPIDR affinity.
    pub hw_id: u64,
}

/// A physical window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhysRange {
    pub start: u64,
    pub size: u64,
}

impl PhysRange {
    pub fn end(self) -> Option<u64> {
        self.start.checked_add(self.size)
    }

    pub fn range(self) -> Option<Range<u64>> {
        Some(self.start..self.end()?)
    }
}

/// An interrupt controller the firmware named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqController {
    Lapic {
        base: u64,
    },
    IoApic {
        id: u8,
        addr: u32,
        gsi_base: u32,
    },
    GicV2 {
        dist: PhysRange,
        cpu_if: PhysRange,
    },
    GicV3 {
        dist: PhysRange,
        redist: PhysRange,
    },
    GicIts {
        mmio: PhysRange,
        phandle: u32,
    },
    GicV2m {
        mmio: PhysRange,
        spi_base: u32,
        spi_count: u16,
    },
}

/// A timer the firmware named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerDesc {
    Hpet {
        base: u64,
        minimum_tick: u16,
        period_fs: u32,
    },
    /// `arm,armv8-timer`. `irqs` are the ID cells, tree order (4 or 5).
    ArmGeneric {
        irqs: [u32; 5],
        nirq: u8,
        clock_frequency: Option<u32>,
    },
}

/// Early console MMIO (the one [`fdt::pick_pl011`] chose).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConsoleDesc {
    pub base: u64,
    pub size: u64,
}

/// One `msi-map` tuple: RID base, ITS phandle, DeviceID base, length.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MsiMapEntry {
    pub rid_base: u32,
    pub parent: u32,
    pub msi_base: u32,
    pub length: u32,
}

/// One `interrupt-map` row (PCI child to GIC specifier).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InterruptMapEntry {
    pub child_hi: u32,
    pub pin: u32,
    pub parent: u32,
    pub irq_type: u32,
    pub irq: u32,
    pub irq_flags: u32,
}

/// An ECAM host: first-bus config-space base, as DESIGN §11.5 stores it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PciHost {
    pub segment: u16,
    pub first_bus: u8,
    pub last_bus: u8,
    pub ecam_base: u64,
    pub dma_coherent: bool,
    pub msi_parent: Option<u32>,
    pub msi_map: [MsiMapEntry; MAX_MSI_MAP],
    pub msi_map_len: usize,
    pub interrupt_map: [InterruptMapEntry; MAX_INTERRUPT_MAP],
    pub interrupt_map_len: usize,
}

/// A simple MMIO device (PL031, fw-cfg, virtio-mmio).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MmioDev {
    pub base: u64,
    pub size: u64,
    pub dma_coherent: bool,
    pub msi_parent: Option<u32>,
    /// GIC INTID from the node's `interrupts` specifier; 0 if absent.
    pub irq: u32,
}

/// What boot needs from firmware. No AML, no per-device `_CRS`.
#[derive(Clone, Copy, Debug)]
pub struct MachineDesc {
    pub cpus: [CpuDesc; MAX_CPUS],
    pub cpu_count: usize,
    pub enable: EnableMethod,
    pub irq_controllers: [IrqController; MAX_IRQ_CONTROLLERS],
    pub irq_controller_count: usize,
    pub timers: [TimerDesc; MAX_TIMERS],
    pub timer_count: usize,
    pub consoles: [ConsoleDesc; MAX_CONSOLES],
    pub console_count: usize,
    pub pci_hosts: [PciHost; MAX_PCI_HOSTS],
    pub pci_host_count: usize,
    pub reserved: [PhysRange; MAX_RESERVED],
    pub reserved_count: usize,
    pub isos: [Iso; acpi::MAX_ISOS],
    pub iso_count: usize,
    pub pcat_compat: bool,
    pub psci: Option<EnableMethod>,
    pub fw_cfg: Option<MmioDev>,
    pub rtc: Option<MmioDev>,
    pub virtio_mmio: [MmioDev; MAX_VIRTIO_MMIO],
    pub virtio_mmio_count: usize,
    /// Structure-block nodes. The `dt: <n> nodes` marker prints this.
    pub node_count: u32,
}

const EMPTY_IRQ: IrqController = IrqController::Lapic { base: 0 };

impl Default for MachineDesc {
    fn default() -> Self {
        Self {
            cpus: [CpuDesc::default(); MAX_CPUS],
            cpu_count: 0,
            enable: EnableMethod::None,
            irq_controllers: [EMPTY_IRQ; MAX_IRQ_CONTROLLERS],
            irq_controller_count: 0,
            timers: [TimerDesc::Hpet {
                base: 0,
                minimum_tick: 0,
                period_fs: 0,
            }; MAX_TIMERS],
            timer_count: 0,
            consoles: [ConsoleDesc::default(); MAX_CONSOLES],
            console_count: 0,
            pci_hosts: [PciHost::default(); MAX_PCI_HOSTS],
            pci_host_count: 0,
            reserved: [PhysRange::default(); MAX_RESERVED],
            reserved_count: 0,
            isos: [Iso::default(); acpi::MAX_ISOS],
            iso_count: 0,
            pcat_compat: false,
            psci: None,
            fw_cfg: None,
            rtc: None,
            virtio_mmio: [MmioDev::default(); MAX_VIRTIO_MMIO],
            virtio_mmio_count: 0,
            node_count: 0,
        }
    }
}

impl MachineDesc {
    pub fn cpu_count(&self) -> usize {
        self.cpu_count
    }

    pub fn cpus(&self) -> &[CpuDesc] {
        self.cpus.get(..self.cpu_count).unwrap_or(&[])
    }

    pub fn reserved_ranges(&self) -> impl Iterator<Item = Range<u64>> + '_ {
        self.reserved
            .iter()
            .take(self.reserved_count)
            .filter_map(|r| r.range())
    }

    pub fn lapic_base(&self) -> Option<u64> {
        self.irq_controllers
            .iter()
            .take(self.irq_controller_count)
            .find_map(|c| match c {
                IrqController::Lapic { base } if *base != 0 => Some(*base),
                _ => None,
            })
    }

    pub fn ioapics(&self) -> impl Iterator<Item = IoApic> + '_ {
        self.irq_controllers
            .iter()
            .take(self.irq_controller_count)
            .filter_map(|c| match c {
                IrqController::IoApic { id, addr, gsi_base } => Some(IoApic {
                    id: *id,
                    addr: *addr,
                    gsi_base: *gsi_base,
                }),
                _ => None,
            })
    }

    pub fn ioapic_count(&self) -> usize {
        self.ioapics().count()
    }

    pub fn irq_overrides(&self) -> &[Iso] {
        self.isos.get(..self.iso_count).unwrap_or(&[])
    }

    pub fn hpet_info(&self) -> Option<HpetInfo> {
        self.timers
            .iter()
            .take(self.timer_count)
            .find_map(|t| match t {
                TimerDesc::Hpet {
                    base,
                    minimum_tick,
                    period_fs,
                } => Some(HpetInfo {
                    base: *base,
                    minimum_tick: *minimum_tick,
                    period_fs: *period_fs,
                }),
                _ => None,
            })
    }

    pub fn gic_v3(&self) -> Option<(PhysRange, PhysRange)> {
        self.irq_controllers
            .iter()
            .take(self.irq_controller_count)
            .find_map(|c| match c {
                IrqController::GicV3 { dist, redist } => Some((*dist, *redist)),
                _ => None,
            })
    }

    pub fn gic_v2(&self) -> Option<(PhysRange, PhysRange)> {
        self.irq_controllers
            .iter()
            .take(self.irq_controller_count)
            .find_map(|c| match c {
                IrqController::GicV2 { dist, cpu_if } => Some((*dist, *cpu_if)),
                _ => None,
            })
    }

    pub fn gic_its(&self) -> Option<(PhysRange, u32)> {
        self.irq_controllers
            .iter()
            .take(self.irq_controller_count)
            .find_map(|c| match c {
                IrqController::GicIts { mmio, phandle } => Some((*mmio, *phandle)),
                _ => None,
            })
    }

    pub fn gic_v2m(&self) -> impl Iterator<Item = (PhysRange, u32, u16)> + '_ {
        self.irq_controllers
            .iter()
            .take(self.irq_controller_count)
            .filter_map(|c| match c {
                IrqController::GicV2m {
                    mmio,
                    spi_base,
                    spi_count,
                } => Some((*mmio, *spi_base, *spi_count)),
                _ => None,
            })
    }

    pub fn arm_timer(&self) -> Option<TimerDesc> {
        self.timers
            .iter()
            .take(self.timer_count)
            .find(|t| matches!(t, TimerDesc::ArmGeneric { .. }))
            .copied()
    }

    pub fn pci_hosts(&self) -> &[PciHost] {
        self.pci_hosts.get(..self.pci_host_count).unwrap_or(&[])
    }

    pub fn console_uart(&self) -> Option<u64> {
        self.consoles
            .first()
            .filter(|_| self.console_count != 0)
            .map(|c| c.base)
    }

    /// Fill the HPET period after the kernel reads GEN_CAP.
    pub fn set_hpet_period(&mut self, period_fs: u32) {
        let n = self.timer_count;
        if let Some(TimerDesc::Hpet { period_fs: p, .. }) = self
            .timers
            .get_mut(..n)
            .and_then(|t| t.iter_mut().find(|x| matches!(x, TimerDesc::Hpet { .. })))
        {
            *p = period_fs;
        }
    }

    /// MADT / HPET / MCFG only. FADT stays on [`AcpiInfo`].
    pub fn from_acpi(info: &AcpiInfo) -> Self {
        let mut d = Self::default();
        if let Some(m) = info.madt.as_ref() {
            fill_madt(&mut d, m);
        }
        if let Some(h) = info.hpet {
            push_timer(
                &mut d,
                TimerDesc::Hpet {
                    base: h.base,
                    minimum_tick: h.minimum_tick,
                    period_fs: h.period_fs,
                },
            );
        }
        if let Some(m) = info.mcfg {
            fill_mcfg(&mut d, m);
        }
        d
    }
}

fn fill_madt(d: &mut MachineDesc, m: &MadtInfo) {
    d.pcat_compat = m.pcat_compat;
    for id in m.apic_ids.iter().take(m.cpu_count) {
        push_cpu(
            d,
            CpuDesc {
                hw_id: u64::from(*id),
            },
        );
    }
    if m.lapic_base != 0 {
        push_irq(d, IrqController::Lapic { base: m.lapic_base });
    }
    for io in m.ioapics.iter().take(m.ioapic_count) {
        push_irq(
            d,
            IrqController::IoApic {
                id: io.id,
                addr: io.addr,
                gsi_base: io.gsi_base,
            },
        );
    }
    let n = m.iso_count.min(d.isos.len());
    if let (Some(dst), Some(src)) = (d.isos.get_mut(..n), m.isos.get(..n)) {
        dst.copy_from_slice(src);
        d.iso_count = n;
    }
}

fn fill_mcfg(d: &mut MachineDesc, m: McfgInfo) {
    // MCFG Base Address is already the first bus's config space
    // (ACPI 6.5 §5.2.27.5). `pci::ecam_phys` adds `(bus - first) << 20`.
    push_pci(
        d,
        PciHost {
            segment: m.segment,
            first_bus: m.start_bus,
            last_bus: m.end_bus,
            ecam_base: m.ecam_base,
            dma_coherent: true,
            ..PciHost::default()
        },
    );
}

pub(crate) fn push_cpu(d: &mut MachineDesc, c: CpuDesc) {
    if let Some(slot) = d.cpus.get_mut(d.cpu_count) {
        *slot = c;
        d.cpu_count = d.cpu_count.saturating_add(1);
    }
}

pub(crate) fn push_irq(d: &mut MachineDesc, c: IrqController) {
    if let Some(slot) = d.irq_controllers.get_mut(d.irq_controller_count) {
        *slot = c;
        d.irq_controller_count = d.irq_controller_count.saturating_add(1);
    }
}

pub(crate) fn push_timer(d: &mut MachineDesc, t: TimerDesc) {
    if let Some(slot) = d.timers.get_mut(d.timer_count) {
        *slot = t;
        d.timer_count = d.timer_count.saturating_add(1);
    }
}

pub(crate) fn push_console(d: &mut MachineDesc, c: ConsoleDesc) {
    if let Some(slot) = d.consoles.get_mut(d.console_count) {
        *slot = c;
        d.console_count = d.console_count.saturating_add(1);
    }
}

pub(crate) fn push_pci(d: &mut MachineDesc, h: PciHost) {
    if let Some(slot) = d.pci_hosts.get_mut(d.pci_host_count) {
        *slot = h;
        d.pci_host_count = d.pci_host_count.saturating_add(1);
    }
}

pub(crate) fn push_reserved(d: &mut MachineDesc, r: PhysRange) {
    if r.size == 0 {
        return;
    }
    if let Some(slot) = d.reserved.get_mut(d.reserved_count) {
        *slot = r;
        d.reserved_count = d.reserved_count.saturating_add(1);
    }
}

pub(crate) fn push_virtio(d: &mut MachineDesc, m: MmioDev) {
    if let Some(slot) = d.virtio_mmio.get_mut(d.virtio_mmio_count) {
        *slot = m;
        d.virtio_mmio_count = d.virtio_mmio_count.saturating_add(1);
    }
}

impl fmt::Display for EnableMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnableMethod::None => f.write_str("none"),
            EnableMethod::Psci { hvc: true } => f.write_str("psci-hvc"),
            EnableMethod::Psci { hvc: false } => f.write_str("psci-smc"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acpi::{
        GAS_SYSTEM_MEMORY, SIG_HPET, SIG_MADT, SIG_MCFG, parse_hpet, parse_madt, parse_mcfg,
    };
    use crate::pmm::{PAGE_SIZE, clip_usable};

    const SDT_HEADER_LEN: usize = 36;

    fn set_sum(b: &mut [u8], off: usize) {
        b[off] = 0;
        let s = b.iter().fold(0u8, |a, x| a.wrapping_add(*x));
        b[off] = s.wrapping_neg();
    }

    fn sdt(sig: &[u8; 4], extra: &[u8]) -> Vec<u8> {
        let len = SDT_HEADER_LEN + extra.len();
        let mut b = vec![0u8; len];
        b[0..4].copy_from_slice(sig);
        b[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        b[8] = 1;
        b[SDT_HEADER_LEN..].copy_from_slice(extra);
        set_sum(&mut b, 9);
        b
    }

    fn madt_bytes() -> Vec<u8> {
        let mut rec = Vec::new();
        rec.extend_from_slice(&[0, 8, 0, 1]);
        rec.extend_from_slice(&1u32.to_le_bytes());
        rec.extend_from_slice(&[0, 8, 1, 2]);
        rec.extend_from_slice(&0u32.to_le_bytes());
        rec.extend_from_slice(&[1, 12, 0, 0]);
        rec.extend_from_slice(&0xFEC0_0000u32.to_le_bytes());
        rec.extend_from_slice(&0u32.to_le_bytes());
        rec.extend_from_slice(&[2, 10, 0, 0]);
        rec.extend_from_slice(&2u32.to_le_bytes());
        rec.extend_from_slice(&0u16.to_le_bytes());
        rec.extend_from_slice(&[5, 12, 0, 0]);
        rec.extend_from_slice(&0xFEE0_0000u64.to_le_bytes());
        let mut extra = Vec::new();
        extra.extend_from_slice(&0xFEE0_0000u32.to_le_bytes());
        extra.extend_from_slice(&1u32.to_le_bytes());
        extra.extend_from_slice(&rec);
        sdt(SIG_MADT, &extra)
    }

    fn hpet_bytes() -> Vec<u8> {
        let mut extra = vec![0u8; 20];
        extra[4] = GAS_SYSTEM_MEMORY;
        extra[8..16].copy_from_slice(&0xFED0_0000u64.to_le_bytes());
        extra[17..19].copy_from_slice(&0x1234u16.to_le_bytes());
        sdt(SIG_HPET, &extra)
    }

    fn mcfg_bytes() -> Vec<u8> {
        let mut extra = vec![0u8; 8 + 16];
        extra[8..16].copy_from_slice(&0xE000_0000u64.to_le_bytes());
        extra[16..18].copy_from_slice(&0u16.to_le_bytes());
        extra[18] = 0;
        extra[19] = 0xFF;
        sdt(SIG_MCFG, &extra)
    }

    fn acpi_info() -> AcpiInfo {
        AcpiInfo {
            used_xsdt: true,
            table_count: 3,
            madt: Some(parse_madt(&madt_bytes()).unwrap()),
            hpet: Some(parse_hpet(&hpet_bytes()).unwrap()),
            fadt: None,
            mcfg: Some(parse_mcfg(&mcfg_bytes()).unwrap()),
        }
    }

    #[test]
    fn acpi_fixtures_fill_machine_desc() {
        let info = acpi_info();
        let d = MachineDesc::from_acpi(&info);
        assert_eq!(d.cpu_count(), 1);
        assert_eq!(d.cpus()[0].hw_id, 1);
        assert_eq!(d.lapic_base(), Some(0xFEE0_0000));
        assert_eq!(d.ioapic_count(), 1);
        let io = d.ioapics().next().unwrap();
        assert_eq!(io.addr, 0xFEC0_0000);
        assert_eq!(d.irq_overrides().len(), 1);
        assert_eq!(d.irq_overrides()[0].gsi, 2);
        assert!(d.pcat_compat);
        let h = d.hpet_info().unwrap();
        assert_eq!(h.base, 0xFED0_0000);
        assert_eq!(h.minimum_tick, 0x1234);
        let pci = d.pci_hosts();
        assert_eq!(pci.len(), 1);
        assert_eq!(pci[0].ecam_base, 0xE000_0000);
        assert_eq!(pci[0].first_bus, 0);
        assert_eq!(pci[0].last_bus, 0xFF);
        assert_eq!(d.reserved_count, 0);
    }

    #[test]
    fn mcfg_stores_first_bus_config_base() {
        let mut extra = vec![0u8; 8 + 16];
        extra[8..16].copy_from_slice(&0xE000_0000u64.to_le_bytes());
        extra[16..18].copy_from_slice(&0u16.to_le_bytes());
        extra[18] = 0x10;
        extra[19] = 0x1F;
        let bytes = sdt(SIG_MCFG, &extra);
        let m = parse_mcfg(&bytes).unwrap();
        let info = AcpiInfo {
            used_xsdt: true,
            table_count: 1,
            madt: None,
            hpet: None,
            fadt: None,
            mcfg: Some(m),
        };
        let d = MachineDesc::from_acpi(&info);
        let pci = d.pci_hosts();
        assert_eq!(pci.len(), 1);
        assert_eq!(pci[0].first_bus, 0x10);
        assert_eq!(pci[0].last_bus, 0x1F);
        assert_eq!(pci[0].ecam_base, 0xE000_0000);
    }

    #[test]
    fn reserved_ranges_stay_out_of_clip_usable() {
        let dtb = include_bytes!("testdata/reserved-both.dtb");
        let d = fdt::parse(dtb).unwrap();
        assert_eq!(d.reserved_count, 2);
        let excl: Vec<Range<u64>> = d.reserved_ranges().collect();
        assert!(
            excl.iter()
                .any(|r| r.start == 0x8000_0000 && r.end == 0x8000_1000)
        );
        assert!(
            excl.iter()
                .any(|r| r.start == 0x8100_0000 && r.end == 0x8100_2000)
        );
        let mut out = Vec::new();
        clip_usable(
            0x8000_0000..0x8200_0000,
            || excl.iter().cloned(),
            |p| out.push(p),
        );
        assert!(
            out.iter()
                .all(|p| { excl.iter().all(|e| p.end <= e.start || p.start >= e.end) })
        );
        assert!(out.iter().any(|p| p.start == 0x8000_1000));
        let _ = PAGE_SIZE;
    }
}
