//! Host tests for the device-tree reader in `fdt.rs` (ROADMAP §11.5).

use super::*;
use crate::machine::{IrqController, MAX_INTERRUPT_MAP, MAX_MSI_MAP};
use crate::pmm::clip_usable;

const VIRT_82: &[u8] = include_bytes!("testdata/virt-8.2.dtb");
const VIRT_82_SEC: &[u8] = include_bytes!("testdata/virt-8.2-secure.dtb");
const VIRT_CUR: &[u8] = include_bytes!("testdata/virt-current.dtb");
const RESERVED: &[u8] = include_bytes!("testdata/reserved-both.dtb");
const TWO_REG: &[u8] = include_bytes!("testdata/reserved-two-reg.dtb");
const RESERVED_33: &[u8] = include_bytes!("testdata/reserved-33.dtb");
const RESERVED_32: &[u8] = include_bytes!("testdata/reserved-32.dtb");
const OVERFLOW: &[u8] = include_bytes!("testdata/reserved-overflow.dtb");
const DEEP: &[u8] = include_bytes!("testdata/reserved-deep.dtb");
const TIMER5: &[u8] = include_bytes!("testdata/timer-5irq.dtb");
const ECAM_RANGE: &[u8] = include_bytes!("testdata/ecam-bus-range.dtb");

#[test]
fn virt_dumpdtb_picks_uart_9000000() {
    assert_eq!(pick_pl011(VIRT_82), Some(0x900_0000));
    assert_eq!(pick_pl011(VIRT_82_SEC), Some(0x900_0000));
    assert_eq!(pick_pl011(VIRT_CUR), Some(0x900_0000));
    for blob in [VIRT_82, VIRT_82_SEC, VIRT_CUR] {
        let d = parse(blob).unwrap();
        assert_eq!(d.console_uart(), Some(0x900_0000));
    }
}

#[test]
fn virt_8_2_secure_skips_disabled_uart() {
    // The disabled secure UART is first; pick must not take 0x9040000.
    let d = parse(VIRT_82_SEC).unwrap();
    assert_eq!(d.console_uart(), Some(0x900_0000));
    assert_ne!(d.console_uart(), Some(0x904_0000));
}

#[test]
fn virt_current_uses_aliases_serial0() {
    let d = parse(VIRT_CUR).unwrap();
    assert_eq!(d.console_uart(), Some(0x900_0000));
    // /chosen is present and names the same UART; pick still goes
    // through aliases, not stdout-path.
    assert!(pick_pl011(VIRT_CUR).is_some());
}

fn check_virt(d: &MachineDesc) {
    assert_eq!(d.cpu_count(), 1);
    assert_eq!(d.cpus()[0].hw_id, 0);
    assert_eq!(d.enable, EnableMethod::Psci { hvc: true });
    assert!(d.psci.is_some());
    assert!(
        d.irq_controllers
            .iter()
            .take(d.irq_controller_count)
            .any(|c| matches!(c, IrqController::GicV3 { dist, .. } if dist.start == 0x800_0000))
    );
    assert!(
        d.irq_controllers
            .iter()
            .take(d.irq_controller_count)
            .any(|c| matches!(c, IrqController::GicIts { mmio, .. } if mmio.start == 0x808_0000))
    );
    match d.arm_timer() {
        Some(TimerDesc::ArmGeneric { irqs, nirq, .. }) => {
            assert_eq!(nirq, 4);
            assert_eq!(&irqs[..4], &[0xd, 0xe, 0xb, 0xa]);
        }
        other => panic!("timer {other:?}"),
    }
    let pci = d.pci_hosts();
    assert_eq!(pci.len(), 1);
    assert_eq!(pci[0].ecam_base, 0x40_1000_0000);
    assert_eq!(pci[0].ecam_size, 0x1000_0000);
    assert_eq!(pci[0].first_bus, 0);
    assert_eq!(pci[0].last_bus, 0xff);
    assert_eq!(
        crate::pci::scan_range(pci[0].first_bus, pci[0].last_bus, pci[0].ecam_size),
        Some((0, 0xff))
    );
    assert!(pci[0].dma_coherent);
    assert_eq!(pci[0].msi_map_len, 1);
    assert_eq!(pci[0].msi_map[0].length, 0x1_0000);
    assert_eq!(pci[0].interrupt_map_len, 16);
    assert_eq!(pci[0].interrupt_map[0].pin, 1);
    assert_eq!(pci[0].interrupt_map[0].irq, 3);
    assert_eq!(d.virtio_mmio_count, 32);
    assert!(d.virtio_mmio[0].dma_coherent);
    assert_eq!(d.virtio_mmio[0].base, 0xa00_0000);
    // QEMU virt: SPI 16 → INTID 48 (GIC_SPI + 16).
    assert_eq!(d.virtio_mmio[0].irq, 48);
    assert_eq!(d.virtio_mmio[1].irq, 49);
    // QEMU virt wires virtio-mmio edge rising (flags 1), the RTC level high (4).
    assert!(d.virtio_mmio[0].irq_edge);
    let fw = d.fw_cfg.unwrap();
    assert_eq!(fw.base, 0x902_0000);
    assert!(fw.dma_coherent);
    let rtc = d.rtc.unwrap();
    assert_eq!(rtc.base, 0x901_0000);
    assert!(!rtc.irq_edge);
    assert!(d.node_count >= 40);
    // memory@ must not become reserved or a console.
    assert!(!d.reserved_ranges().any(|r| r.start == 0x4000_0000));
}

#[test]
fn virt_trees_fill_machine_desc() {
    check_virt(&parse(VIRT_82).unwrap());
    check_virt(&parse(VIRT_CUR).unwrap());
    let sec = parse(VIRT_82_SEC).unwrap();
    assert_eq!(sec.console_uart(), Some(0x900_0000));
    assert_eq!(sec.cpu_count(), 1);
}

#[test]
fn reserved_memory_both_kinds() {
    let d = parse(RESERVED).unwrap();
    assert_eq!(d.reserved_count, 2);
    let mut rs: Vec<_> = d.reserved_ranges().collect();
    rs.sort_by_key(|r| r.start);
    assert_eq!(rs[0], 0x8000_0000..0x8000_1000);
    assert_eq!(rs[1], 0x8100_0000..0x8100_2000);
    assert_eq!(d.console_uart(), Some(0x900_0000));
    // The memreserve block has no no-map flag. Only the reserved node does.
    let nomap: Vec<_> = d.nomap_ranges().collect();
    assert_eq!(nomap, vec![0x8100_0000..0x8100_2000]);
}

#[test]
fn reserved_reg_keeps_every_pair() {
    let d = parse(TWO_REG).unwrap();
    let mut rs: Vec<_> = d.reserved_ranges().collect();
    rs.sort_by_key(|r| r.start);
    assert_eq!(
        rs,
        vec![
            0x8000_0000..0x8000_1000,
            0x8100_0000..0x8100_2000,
            0x8200_0000..0x8200_1000,
        ]
    );
    let mut nomap: Vec<_> = d.nomap_ranges().collect();
    nomap.sort_by_key(|r| r.start);
    assert_eq!(
        nomap,
        vec![0x8000_0000..0x8000_1000, 0x8100_0000..0x8100_2000]
    );
    let mut phys = Vec::new();
    clip_usable(
        0x7FF0_0000..0x8300_0000,
        || d.nomap_ranges(),
        |p| {
            phys.push(p);
        },
    );
    assert_eq!(
        phys,
        vec![
            0x7FF0_0000..0x8000_0000,
            0x8000_1000..0x8100_0000,
            0x8100_2000..0x8300_0000,
        ]
    );
    let mut buddy = Vec::new();
    clip_usable(
        0x7FF0_0000..0x8300_0000,
        || d.reserved_ranges(),
        |p| {
            buddy.push(p);
        },
    );
    assert!(
        buddy
            .iter()
            .all(|p| rs.iter().all(|e| p.end <= e.start || p.start >= e.end))
    );
    assert!(buddy.contains(&(0x8200_1000..0x8300_0000)));
}

#[test]
fn reserved_past_cap_is_refused() {
    assert_eq!(parse(RESERVED_33).err(), Some(FdtError::Reserved));
    assert_eq!(parse_dtb(RESERVED_33).err(), Some(FdtError::Reserved));
    let d = parse(RESERVED_32).unwrap();
    assert_eq!(d.reserved_count, 32);
    assert_eq!(d.reserved_ranges().count(), 32);
}

#[test]
fn reserved_overflow_is_saturated() {
    let d = parse(OVERFLOW).unwrap();
    let rs: Vec<_> = d.reserved_ranges().collect();
    assert_eq!(rs, vec![0xFFFF_FFFF_FFF0_0000..u64::MAX]);
    let mut out = Vec::new();
    clip_usable(
        0xFFFF_FFFF_FFE0_0000..u64::MAX,
        || d.reserved_ranges(),
        |p| out.push(p),
    );
    assert_eq!(out, vec![0xFFFF_FFFF_FFE0_0000..0xFFFF_FFFF_FFF0_0000]);
}

#[test]
fn reserved_survives_a_walk_past_the_caps() {
    let parsed = parse_dtb(DEEP).unwrap();
    let Parsed::Incomplete(d, e) = parsed else {
        panic!("device walk should fail");
    };
    assert_ne!(e, FdtError::Reserved);
    assert!(parse(DEEP).is_err());
    let mut rs: Vec<_> = d.reserved_ranges().collect();
    rs.sort_by_key(|r| r.start);
    assert_eq!(rs, vec![0x8000_0000..0x8000_1000, 0x9000_0000..0x9000_1000]);
}

#[test]
fn ecam_bus_range_reg_is_first_bus() {
    let d = parse(ECAM_RANGE).unwrap();
    let pci = d.pci_hosts();
    assert_eq!(pci.len(), 1);
    let h = &pci[0];
    assert_eq!(h.segment, 0);
    assert_eq!(h.first_bus, 0x10);
    assert_eq!(h.last_bus, 0x1f);
    const R: u64 = 0x4000_0000;
    assert_eq!(h.ecam_base, R);
    assert_eq!(h.ecam_size, 0x0100_0000);
    assert_eq!(
        crate::pci::ecam_phys(h.ecam_base, h.first_bus, h.last_bus, 0x10, 0, 0, 0),
        Some(R)
    );
    assert_eq!(
        crate::pci::ecam_phys(h.ecam_base, h.first_bus, h.last_bus, 0x1f, 0, 0, 0),
        Some(R + (0xf << 20))
    );
    assert_eq!(
        crate::pci::ecam_phys(h.ecam_base, h.first_bus, h.last_bus, 0x0f, 0, 0, 0),
        None
    );
    assert_eq!(
        crate::pci::ecam_phys(h.ecam_base, h.first_bus, h.last_bus, 0x20, 0, 0, 0),
        None
    );
}

#[test]
fn timer_five_entry() {
    let d = parse(TIMER5).unwrap();
    match d.arm_timer() {
        Some(TimerDesc::ArmGeneric { irqs, nirq, .. }) => {
            assert_eq!(nirq, 5);
            assert_eq!(&irqs[..5], &[0xd, 0xe, 0xb, 0xa, 0x9]);
        }
        other => panic!("timer {other:?}"),
    }
    assert_eq!(pick_pl011(TIMER5), Some(0x900_0000));
}

#[test]
fn pick_is_the_one_console_function() {
    // parse must not invent a second chooser: console == pick_pl011.
    for blob in [VIRT_82, VIRT_82_SEC, VIRT_CUR, RESERVED, TIMER5] {
        assert_eq!(parse(blob).unwrap().console_uart(), pick_pl011(blob));
    }
}

fn be(cells: &[u32]) -> Vec<u8> {
    cells.iter().flat_map(|c| c.to_be_bytes()).collect()
}

#[test]
fn wide_address_cells_keep_the_low_64_bits() {
    let v = be(&[0xdead, 0xbeef, 0x1, 0x2, 0x0, 0x1000]);
    assert_eq!(addr_cells(&v, 0, 4), Some(0x1_0000_0002));
    assert_eq!(addr_cells(&v, 0, 3), Some(0x0000_beef_0000_0001));
    let r = first_reg(&v, 4, 2).unwrap();
    assert_eq!((r.start, r.size), (0x1_0000_0002, 0x1000));
    assert_eq!(addr_cells(&v, 0, 7), None);
    assert_eq!(addr_cells(&v, 5, 2), None);
}

#[test]
fn first_reg_refuses_wrapping_and_huge_windows() {
    let wraps = be(&[0xFFFF_FFFF, 0xFFFF_F000, 0x0, 0x2000]);
    assert!(first_reg(&wraps, 2, 2).is_none());
    let huge = be(&[0x0, 0x0900_0000, 0x1, 0x0]);
    assert!(first_reg(&huge, 2, 2).is_none());
    let ok = be(&[0x0, 0x0900_0000, 0x0, 0x1000]);
    assert!(first_reg(&ok, 2, 2).is_some());
}

#[test]
fn interrupt_trigger_is_parsed() {
    let p = |cells: &[u32]| {
        let v = be(cells);
        let props = [Prop {
            name: b"interrupts",
            value: &v,
        }];
        first_gic_irq(&props)
    };
    assert_eq!(p(&[0, 16, 1]), (48, true));
    assert_eq!(p(&[0, 16, 2]), (48, true));
    assert_eq!(p(&[0, 16, 4]), (48, false));
    assert_eq!(p(&[0, 16, 8]), (48, false));
    assert_eq!(p(&[0, 16]), (0, false));
    assert_eq!(p(&[]), (0, false));
}

#[test]
fn truncated_and_corrupt_blobs_never_panic() {
    for blob in [VIRT_82, VIRT_CUR, TWO_REG] {
        for n in 0..blob.len() {
            let _ = parse(&blob[..n]);
            let _ = pick_pl011(&blob[..n]);
        }
        // Every header field forced to 0 and to the extremes.
        for off in (4..40).step_by(4) {
            for v in [0u32, 1, 0x7FFF_FFFF, 0xFFFF_FFFF] {
                let mut b = blob.to_vec();
                b[off..off + 4].copy_from_slice(&v.to_be_bytes());
                let _ = parse(&b);
                let _ = pick_pl011(&b);
            }
        }
    }
}

#[test]
fn bad_magic_is_an_error() {
    assert_eq!(parse(&[0; 64]).err(), Some(FdtError::BadMagic));
    assert_eq!(pick_pl011(&[0; 64]), None);
}

/// A tree whose first PL011 is disabled and which has no `/aliases`.
#[test]
fn first_okay_pl011_in_tree_order() {
    // Build: disabled @0x9040000, then okay @0x9000000. No aliases.
    let dtb = tiny_two_uarts(false);
    assert_eq!(pick_pl011(&dtb), Some(0x900_0000));
    let with_alias = tiny_two_uarts(true);
    assert_eq!(pick_pl011(&with_alias), Some(0x900_0000));
}

fn tiny_two_uarts(aliases: bool) -> Vec<u8> {
    if aliases {
        include_bytes!("testdata/two-uarts-alias.dtb").to_vec()
    } else {
        include_bytes!("testdata/two-uarts.dtb").to_vec()
    }
}

/// A DTB built in memory, for the shapes the checked-in fixtures do not
/// reach. Every property name gets its own strings entry.
#[derive(Default)]
struct Dtb {
    st: Vec<u8>,
    strings: Vec<u8>,
}

impl Dtb {
    fn token(&mut self, t: u32) {
        self.st.extend_from_slice(&t.to_be_bytes());
    }

    fn pad(&mut self) {
        while !self.st.len().is_multiple_of(4) {
            self.st.push(0);
        }
    }

    fn begin(&mut self, name: &str) {
        self.token(FDT_BEGIN_NODE);
        self.st.extend_from_slice(format!("{name}\0").as_bytes());
        self.pad();
    }

    fn end(&mut self) {
        self.token(FDT_END_NODE);
    }

    fn prop(&mut self, name: &str, value: &[u8]) {
        let nameoff = self.strings.len() as u32;
        self.strings
            .extend_from_slice(format!("{name}\0").as_bytes());
        self.token(FDT_PROP);
        self.token(value.len() as u32);
        self.token(nameoff);
        self.st.extend_from_slice(value);
        self.pad();
    }

    /// A version 17 header, an empty memreserve block, then the blocks.
    fn finish(mut self) -> Vec<u8> {
        self.token(FDT_END);
        let off_st = HDR_LEN + 16;
        let off_str = off_st + self.st.len();
        let total = off_str + self.strings.len();
        let words = [
            FDT_MAGIC,
            total as u32,
            off_st as u32,
            off_str as u32,
            HDR_LEN as u32,
            17,
            16,
            0,
            self.strings.len() as u32,
            self.st.len() as u32,
        ];
        let mut out = Vec::with_capacity(total);
        for w in words {
            out.extend_from_slice(&w.to_be_bytes());
        }
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(&self.st);
        out.extend_from_slice(&self.strings);
        out
    }
}

/// A root with two-cell addresses and sizes, as QEMU `virt` has. The
/// caller closes it.
fn root_2_2() -> Dtb {
    let mut t = Dtb::default();
    t.begin("");
    t.prop("#address-cells", &be(&[2]));
    t.prop("#size-cells", &be(&[2]));
    t
}

fn pl011(t: &mut Dtb, name: &str, base: u32) {
    t.begin(name);
    t.prop("compatible", b"arm,pl011\0");
    t.prop("reg", &be(&[0, base, 0, 0x1000]));
    t.end();
}

/// A `/psci` node with no `method`, or one that names neither conduit,
/// gives no enable method. It is not HVC.
#[test]
fn psci_without_a_known_method_enables_nothing() {
    for method in [None, Some(&b"bogus\0"[..])] {
        let mut t = root_2_2();
        t.begin("psci");
        t.prop("compatible", b"arm,psci-1.0\0");
        if let Some(m) = method {
            t.prop("method", m);
        }
        t.end();
        t.end();
        let d = parse(&t.finish()).unwrap();
        assert_eq!(d.enable, EnableMethod::None);
        assert_eq!(d.psci, Some(EnableMethod::None));
    }
}

#[test]
fn properties_past_the_cap_are_counted() {
    // Two cell properties, then PROP_CAP + 3 more: 5 past the cap.
    let mut t = root_2_2();
    for i in 0..PROP_CAP + 3 {
        t.prop(&format!("p{i}"), &be(&[i as u32]));
    }
    pl011(&mut t, "serial@9000000", 0x900_0000);
    t.end();
    let d = parse(&t.finish()).unwrap();
    assert_eq!((d.dropped_props, d.dropped_uarts), (5, 0));
    assert_eq!(d.console_uart(), Some(0x900_0000));
}

#[test]
fn uarts_past_the_cap_are_counted() {
    let mut t = root_2_2();
    for i in 0..UART_CAP + 2 {
        let base = 0x900_0000 + (i as u32) * 0x1_0000;
        pl011(&mut t, &format!("serial@{i:x}"), base);
    }
    t.end();
    let dtb = t.finish();
    let d = parse(&dtb).unwrap();
    assert_eq!(d.dropped_uarts, 2);
    // The first okay UART in tree order stays the console.
    assert_eq!(d.console_uart(), Some(0x900_0000));
    assert_eq!(pick_pl011(&dtb), Some(0x900_0000));
}

/// One `interrupt-map` row of the shape `fill_interrupt_map` reads.
fn map_row(pin: u32) -> [u32; 10] {
    [0, 0, 0, pin, 0x8001, 0, 0, 0, 3, 4]
}

fn host_with_map(map: &[u32]) -> Vec<u8> {
    let mut t = root_2_2();
    t.begin("pcie@10000000");
    t.prop("compatible", b"pci-host-ecam-generic\0");
    t.prop("reg", &be(&[0, 0x1000_0000, 0, 0x1000_0000]));
    t.prop("bus-range", &be(&[0, 0xff]));
    t.prop("#address-cells", &be(&[3]));
    t.prop("#interrupt-cells", &be(&[1]));
    t.prop("interrupt-map", &be(map));
    t.end();
    t.end();
    t.finish()
}

#[test]
fn interrupt_map_rows_of_the_wrong_length_are_refused() {
    let two: Vec<u32> = [map_row(1), map_row(2)].concat();
    let d = parse(&host_with_map(&two)).unwrap();
    assert_eq!(d.pci_hosts()[0].interrupt_map_len, 2);
    assert_eq!(d.refused_interrupt_maps, 0);
    // One stray cell: the map does not split into rows, so none is read.
    let mut stray = two.clone();
    stray.push(0);
    let d = parse(&host_with_map(&stray)).unwrap();
    assert_eq!(
        (d.pci_hosts()[0].interrupt_map_len, d.refused_interrupt_maps),
        (0, 1)
    );
    // Eleven cells is not a row either.
    let d = parse(&host_with_map(&[0; 11])).unwrap();
    assert_eq!(
        (d.pci_hosts()[0].interrupt_map_len, d.refused_interrupt_maps),
        (0, 1)
    );
}

#[test]
fn interrupt_map_of_no_length_is_read_as_no_rows() {
    let d = parse(&host_with_map(&[])).unwrap();
    assert_eq!(
        (d.pci_hosts()[0].interrupt_map_len, d.refused_interrupt_maps),
        (0, 0)
    );
}

#[test]
fn interrupt_map_of_one_ten_cell_row_is_read() {
    let d = parse(&host_with_map(&map_row(7))).unwrap();
    let host = &d.pci_hosts()[0];
    assert_eq!((host.interrupt_map_len, d.refused_interrupt_maps), (1, 0));
    assert_eq!(host.interrupt_map[0].pin, 7);
    assert_eq!(host.interrupt_map[0].parent, 0x8001);
}

#[test]
fn interrupt_map_whose_parent_has_other_cell_counts_is_read_in_ten_cell_rows() {
    // The parent is not resolved, so its own cell counts do not change the
    // row shape: a map whose length is a multiple of 40 bytes is accepted.
    let mut t = root_2_2();
    t.begin("intc@8000000");
    t.prop("phandle", &be(&[1]));
    t.prop("#address-cells", &be(&[0]));
    t.prop("#interrupt-cells", &be(&[1]));
    t.end();
    t.begin("pcie@10000000");
    t.prop("compatible", b"pci-host-ecam-generic\0");
    t.prop("reg", &be(&[0, 0x1000_0000, 0, 0x1000_0000]));
    t.prop("bus-range", &be(&[0, 0xff]));
    t.prop("#address-cells", &be(&[3]));
    t.prop("#interrupt-cells", &be(&[1]));
    t.prop("interrupt-parent", &be(&[1]));
    t.prop("interrupt-map", &be(&[map_row(1), map_row(2)].concat()));
    t.end();
    t.end();
    let d = parse(&t.finish()).unwrap();
    let host = &d.pci_hosts()[0];
    assert_eq!((host.interrupt_map_len, d.refused_interrupt_maps), (2, 0));
    assert_eq!(
        (host.interrupt_map[0].pin, host.interrupt_map[1].pin),
        (1, 2)
    );
}

#[test]
fn interrupt_map_rows_past_the_cap_are_counted() {
    // Seventeen rows, 680 bytes: a whole number of rows, one more than the
    // host keeps.
    let rows: Vec<u32> = (0..MAX_INTERRUPT_MAP as u32 + 1)
        .flat_map(map_row)
        .collect();
    assert_eq!(rows.len() * 4, 680);
    let d = parse(&host_with_map(&rows)).unwrap();
    let host = &d.pci_hosts()[0];
    assert_eq!(
        (
            host.interrupt_map_len,
            d.refused_interrupt_maps,
            d.dropped_interrupt_rows
        ),
        (MAX_INTERRUPT_MAP, 0, 1)
    );
    assert_eq!(host.interrupt_map[MAX_INTERRUPT_MAP - 1].pin, 15);
}

/// A PCI host whose `msi-map` is `map`, four cells per row.
fn host_with_msi_map(map: &[u32]) -> Vec<u8> {
    let mut t = root_2_2();
    t.begin("pcie@10000000");
    t.prop("compatible", b"pci-host-ecam-generic\0");
    t.prop("reg", &be(&[0, 0x1000_0000, 0, 0x1000_0000]));
    t.prop("bus-range", &be(&[0, 0xff]));
    t.prop("msi-map", &be(map));
    t.end();
    t.end();
    t.finish()
}

#[test]
fn msi_map_rows_past_the_cap_are_counted() {
    // One row more than the host keeps: rid, phandle, device id, length.
    let rows: Vec<u32> = (0..MAX_MSI_MAP as u32 + 1)
        .flat_map(|i| [i << 3, 1, i << 3, 0x8])
        .collect();
    let d = parse(&host_with_msi_map(&rows)).unwrap();
    let host = &d.pci_hosts()[0];
    assert_eq!((host.msi_map_len, d.dropped_msi_rows), (MAX_MSI_MAP, 1));
    assert_eq!(
        host.msi_map[MAX_MSI_MAP - 1].rid_base,
        ((MAX_MSI_MAP as u32) - 1) << 3
    );
}

#[test]
fn checked_in_trees_drop_and_refuse_nothing() {
    for blob in [
        VIRT_82,
        VIRT_82_SEC,
        VIRT_CUR,
        RESERVED,
        TWO_REG,
        TIMER5,
        ECAM_RANGE,
    ] {
        let d = parse(blob).unwrap();
        assert_eq!(
            (
                d.dropped_props,
                d.dropped_uarts,
                d.refused_interrupt_maps,
                d.dropped_interrupt_rows,
                d.dropped_pci_hosts,
                d.dropped_msi_rows,
            ),
            (0, 0, 0, 0, 0, 0)
        );
    }
    // QEMU virt's PCI map is 16 rows of ten cells, as the fixture holds.
    assert_eq!(parse(VIRT_82).unwrap().pci_hosts()[0].interrupt_map_len, 16);
}
