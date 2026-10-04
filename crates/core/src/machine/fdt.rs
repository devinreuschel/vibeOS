//! Flattened device tree walker and UART pick (Devicetree Specification v0.4
//! §§5.2–5.4; DESIGN §1.5: cited, no text copied).
//!
//! [`pick_pl011`] is the one console chooser: `/aliases` `serial0`, else the
//! first okay `arm,pl011` in tree order. [`parse`] fills [`MachineDesc`]
//! and calls it. `/chosen` is ignored. `memory@` is not RAM.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use core::fmt;

use super::{
    ConsoleDesc, CpuDesc, EnableMethod, InterruptMapEntry, IrqController, MachineDesc, MmioDev,
    MsiMapEntry, PciHost, PhysRange, TimerDesc, push_console, push_cpu, push_irq, push_pci,
    push_reserved, push_timer, push_virtio,
};

/// FDT magic (`dt_spec` v0.4 §5.2).
const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 0x1;
const FDT_END_NODE: u32 = 0x2;
const FDT_PROP: u32 = 0x3;
const FDT_NOP: u32 = 0x4;
const FDT_END: u32 = 0x9;

const HDR_LEN: usize = 40;
const DEPTH: usize = 16;
const PATH_CAP: usize = 128;
const PROP_CAP: usize = 32;
const UART_CAP: usize = 4;
const DEFAULT_ADDR_CELLS: u32 = 2;
const DEFAULT_SIZE_CELLS: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum FdtError {
    Truncated,
    BadMagic,
    BadToken,
}

impl FdtError {
    pub fn as_str(self) -> &'static str {
        match self {
            FdtError::Truncated => "truncated",
            FdtError::BadMagic => "bad magic",
            FdtError::BadToken => "bad token",
        }
    }
}

impl From<FdtError> for crate::kerror::KError {
    fn from(_: FdtError) -> Self {
        Self::Inval
    }
}

impl fmt::Display for FdtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

struct Header {
    totalsize: u32,
    off_dt_struct: u32,
    off_dt_strings: u32,
    off_mem_rsvmap: u32,
    size_dt_strings: u32,
    size_dt_struct: u32,
}

#[derive(Clone, Copy)]
struct Inherit {
    addr_cells: u32,
    size_cells: u32,
    dma_coherent: bool,
}

#[derive(Clone, Copy)]
struct Prop<'a> {
    name: &'a [u8],
    value: &'a [u8],
}

#[derive(Clone, Copy)]
struct UartCand {
    path: [u8; PATH_CAP],
    path_len: usize,
    base: u64,
    size: u64,
    okay: bool,
}

fn be_u32(b: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    let s = b.get(off..end)?;
    let a: [u8; 4] = s.try_into().ok()?;
    Some(u32::from_be_bytes(a))
}

fn be_u64(b: &[u8], off: usize) -> Option<u64> {
    let end = off.checked_add(8)?;
    let s = b.get(off..end)?;
    let a: [u8; 8] = s.try_into().ok()?;
    Some(u64::from_be_bytes(a))
}

fn align4(n: usize) -> Option<usize> {
    n.checked_add(3).map(|x| x & !3)
}

fn parse_header(dtb: &[u8]) -> Result<Header, FdtError> {
    let h = dtb.get(..HDR_LEN).ok_or(FdtError::Truncated)?;
    if be_u32(h, 0) != Some(FDT_MAGIC) {
        return Err(FdtError::BadMagic);
    }
    let totalsize = be_u32(h, 4).ok_or(FdtError::Truncated)?;
    if (totalsize as usize) > dtb.len() || (totalsize as usize) < HDR_LEN {
        return Err(FdtError::Truncated);
    }
    Ok(Header {
        totalsize,
        off_dt_struct: be_u32(h, 8).ok_or(FdtError::Truncated)?,
        off_dt_strings: be_u32(h, 12).ok_or(FdtError::Truncated)?,
        off_mem_rsvmap: be_u32(h, 16).ok_or(FdtError::Truncated)?,
        size_dt_strings: be_u32(h, 32).ok_or(FdtError::Truncated)?,
        size_dt_struct: be_u32(h, 36).ok_or(FdtError::Truncated)?,
    })
}

fn struct_blob<'a>(dtb: &'a [u8], h: &Header) -> Result<&'a [u8], FdtError> {
    let off = h.off_dt_struct as usize;
    let len = if h.size_dt_struct == 0 {
        (h.totalsize as usize)
            .checked_sub(off)
            .ok_or(FdtError::Truncated)?
    } else {
        h.size_dt_struct as usize
    };
    let end = off.checked_add(len).ok_or(FdtError::Truncated)?;
    dtb.get(off..end).ok_or(FdtError::Truncated)
}

fn strings_blob<'a>(dtb: &'a [u8], h: &Header) -> Result<&'a [u8], FdtError> {
    let off = h.off_dt_strings as usize;
    let len = h.size_dt_strings as usize;
    let end = off.checked_add(len).ok_or(FdtError::Truncated)?;
    dtb.get(off..end)
        .or_else(|| dtb.get(off..))
        .ok_or(FdtError::Truncated)
}

fn cstr(b: &[u8]) -> &[u8] {
    b.split(|&c| c == 0).next().unwrap_or(&[])
}

fn str_at(strings: &[u8], off: u32) -> Option<&[u8]> {
    let s = strings.get(off as usize..)?;
    Some(cstr(s))
}

fn cell(val: &[u8], i: usize) -> Option<u32> {
    let off = i.checked_mul(4)?;
    be_u32(val, off)
}

fn addr_cells(val: &[u8], start: usize, ncells: u32) -> Option<u64> {
    match ncells {
        0 => Some(0),
        1 => Some(u64::from(cell(val, start)?)),
        _ => {
            let hi = cell(val, start)?;
            let lo = cell(val, start.checked_add(1)?)?;
            Some(u64::from(hi).wrapping_shl(32) | u64::from(lo))
        }
    }
}

fn first_reg(val: &[u8], addr_c: u32, size_c: u32) -> Option<PhysRange> {
    let start = addr_cells(val, 0, addr_c)?;
    let size = addr_cells(val, addr_c as usize, size_c)?;
    Some(PhysRange { start, size })
}

fn prop_named<'a>(props: &[Prop<'a>], name: &[u8]) -> Option<&'a [u8]> {
    props.iter().find(|p| p.name == name).map(|p| p.value)
}

fn compatible_has(props: &[Prop<'_>], want: &[u8]) -> bool {
    let Some(v) = prop_named(props, b"compatible") else {
        return false;
    };
    let mut rest = v;
    while !rest.is_empty() {
        let one = cstr(rest);
        if one == want {
            return true;
        }
        let skip = one.len().checked_add(1).unwrap_or(rest.len());
        rest = rest.get(skip..).unwrap_or(&[]);
    }
    false
}

fn status_okay(props: &[Prop<'_>]) -> bool {
    match prop_named(props, b"status") {
        None => true,
        Some(v) => {
            let s = cstr(v);
            s == b"okay" || s == b"ok"
        }
    }
}

fn has_empty(props: &[Prop<'_>], name: &[u8]) -> bool {
    prop_named(props, name).is_some()
}

fn phandle(props: &[Prop<'_>]) -> Option<u32> {
    prop_named(props, b"phandle")
        .or_else(|| prop_named(props, b"linux,phandle"))
        .and_then(|v| be_u32(v, 0))
}

fn u32_prop(props: &[Prop<'_>], name: &[u8]) -> Option<u32> {
    prop_named(props, name).and_then(|v| be_u32(v, 0))
}

fn path_push(path: &mut [u8], len: &mut usize, name: &[u8]) -> bool {
    if name.is_empty() {
        return true;
    }
    let Some(need) = name.len().checked_add(1) else {
        return false;
    };
    let Some(end) = len.checked_add(need) else {
        return false;
    };
    if end > path.len() {
        return false;
    }
    if let Some(slot) = path.get_mut(*len) {
        *slot = b'/';
    }
    let Some(start) = len.checked_add(1) else {
        return false;
    };
    if let Some(dst) = path.get_mut(start..end) {
        dst.copy_from_slice(name);
        *len = end;
        true
    } else {
        false
    }
}

fn path_pop(path: &[u8], len: &mut usize) {
    let cur = path.get(..*len).unwrap_or(&[]);
    *len = cur.iter().rposition(|&c| c == b'/').unwrap_or(0);
}

fn path_eq(path: &[u8], alias: &[u8]) -> bool {
    let a = cstr(alias);
    if path == a {
        return true;
    }
    if let Some(rest) = a.strip_prefix(b"/") {
        return path.get(1..) == Some(rest);
    }
    false
}

fn path_is(path: &[u8], abs: &[u8]) -> bool {
    path == abs
}

fn path_starts(path: &[u8], prefix: &[u8]) -> bool {
    path.get(..prefix.len()) == Some(prefix) && path.get(prefix.len()).is_none_or(|&c| c == b'/')
}

/// The one UART chooser. `/aliases` `serial0`, else the first okay `arm,pl011`.
pub fn pick_pl011(dtb: &[u8]) -> Option<u64> {
    pick_pl011_full(dtb).map(|c| c.base)
}

fn pick_pl011_full(dtb: &[u8]) -> Option<UartCand> {
    let mut alias = [0u8; PATH_CAP];
    let mut alias_len = 0usize;
    let mut cands = [UartCand {
        path: [0; PATH_CAP],
        path_len: 0,
        base: 0,
        size: 0,
        okay: false,
    }; UART_CAP];
    let mut n = 0usize;
    if walk(dtb, |path, props, inh| {
        if path_is(path, b"/aliases")
            && let Some(v) = prop_named(props, b"serial0")
        {
            let s = cstr(v);
            let copy = s.len().min(alias.len());
            if let Some(dst) = alias.get_mut(..copy) {
                dst.copy_from_slice(s.get(..copy).unwrap_or(&[]));
                alias_len = copy;
            }
        }
        if compatible_has(props, b"arm,pl011")
            && let Some(r) =
                prop_named(props, b"reg").and_then(|v| first_reg(v, inh.addr_cells, inh.size_cells))
            && let Some(slot) = cands.get_mut(n)
        {
            let plen = path.len().min(PATH_CAP);
            if let Some(dst) = slot.path.get_mut(..plen) {
                dst.copy_from_slice(path.get(..plen).unwrap_or(&[]));
            }
            slot.path_len = plen;
            slot.base = r.start;
            slot.size = r.size;
            slot.okay = status_okay(props);
            n = n.saturating_add(1);
        }
        Ok(())
    })
    .is_err()
    {
        return None;
    }
    let alias = alias.get(..alias_len).unwrap_or(&[]);
    if !alias.is_empty() {
        for c in cands.iter().take(n) {
            let p = c.path.get(..c.path_len).unwrap_or(&[]);
            if c.okay && path_eq(p, alias) {
                return Some(*c);
            }
        }
    }
    cands.iter().take(n).find(|c| c.okay).copied()
}

/// Parse a DTB into [`MachineDesc`]. RAM stays on the Limine memory map.
pub fn parse(dtb: &[u8]) -> Result<MachineDesc, FdtError> {
    let h = parse_header(dtb)?;
    let mut d = MachineDesc::default();
    fill_mem_rsv(&mut d, dtb, &h)?;
    d.node_count = walk(dtb, |path, props, inh| {
        if path_is(path, b"/reserved-memory") {
            return Ok(());
        }
        if path_starts(path, b"/reserved-memory") {
            if let Some(r) =
                prop_named(props, b"reg").and_then(|v| first_reg(v, inh.addr_cells, inh.size_cells))
            {
                push_reserved(&mut d, r);
            }
            return Ok(());
        }
        if path_is(path, b"/chosen") {
            return Ok(());
        }
        fill_node(&mut d, path, props, inh);
        Ok(())
    })?;
    if let Some(c) = pick_pl011_full(dtb) {
        push_console(
            &mut d,
            ConsoleDesc {
                base: c.base,
                size: c.size,
            },
        );
    }
    Ok(d)
}

fn fill_mem_rsv(d: &mut MachineDesc, dtb: &[u8], h: &Header) -> Result<(), FdtError> {
    let mut off = h.off_mem_rsvmap as usize;
    loop {
        let addr = be_u64(dtb, off).ok_or(FdtError::Truncated)?;
        let size = be_u64(dtb, off.checked_add(8).ok_or(FdtError::Truncated)?)
            .ok_or(FdtError::Truncated)?;
        if addr == 0 && size == 0 {
            return Ok(());
        }
        if size != 0 {
            push_reserved(d, PhysRange { start: addr, size });
        }
        off = off.checked_add(16).ok_or(FdtError::Truncated)?;
    }
}

fn is_cpu(path: &[u8], props: &[Prop<'_>]) -> bool {
    let Some(name) = path.rsplit(|c| *c == b'/').next() else {
        return false;
    };
    if !name.starts_with(b"cpu@") {
        return false;
    }
    match prop_named(props, b"device_type") {
        Some(v) if cstr(v) == b"cpu" => true,
        _ => path_starts(path, b"/cpus/"),
    }
}

fn fill_node(d: &mut MachineDesc, path: &[u8], props: &[Prop<'_>], inh: Inherit) {
    let dma = inh.dma_coherent || has_empty(props, b"dma-coherent");
    let reg = prop_named(props, b"reg").and_then(|v| first_reg(v, inh.addr_cells, inh.size_cells));
    let msi_parent = u32_prop(props, b"msi-parent");

    if is_cpu(path, props) && status_okay(props) {
        let hw = reg
            .map(|r| r.start)
            .or_else(|| prop_named(props, b"reg").and_then(|v| addr_cells(v, 0, inh.addr_cells)));
        if let Some(hw_id) = hw {
            push_cpu(d, CpuDesc { hw_id });
        }
    }

    if path_is(path, b"/psci")
        || compatible_has(props, b"arm,psci-1.0")
        || compatible_has(props, b"arm,psci-0.2")
    {
        let hvc = match prop_named(props, b"method").map(cstr) {
            Some(b"hvc") => true,
            Some(b"smc") => false,
            _ => true,
        };
        let m = EnableMethod::Psci { hvc };
        d.psci = Some(m);
        d.enable = m;
    }

    if compatible_has(props, b"arm,gic-v3")
        && let Some(dist) = reg
    {
        let redist = prop_named(props, b"reg")
            .and_then(|v| {
                let stride = (inh.addr_cells.checked_add(inh.size_cells)?).checked_mul(4)?;
                let second = v.get(stride as usize..)?;
                first_reg(second, inh.addr_cells, inh.size_cells)
            })
            .unwrap_or(PhysRange { start: 0, size: 0 });
        push_irq(d, IrqController::GicV3 { dist, redist });
    } else if (compatible_has(props, b"arm,gic-v2")
        || compatible_has(props, b"arm,gic-400")
        || compatible_has(props, b"arm,cortex-a15-gic"))
        && let Some(dist) = reg
    {
        let cpu_if = prop_named(props, b"reg")
            .and_then(|v| {
                let stride = (inh.addr_cells.checked_add(inh.size_cells)?).checked_mul(4)?;
                first_reg(v.get(stride as usize..)?, inh.addr_cells, inh.size_cells)
            })
            .unwrap_or(PhysRange { start: 0, size: 0 });
        push_irq(d, IrqController::GicV2 { dist, cpu_if });
    }

    if compatible_has(props, b"arm,gic-v3-its")
        && let Some(mmio) = reg
    {
        push_irq(
            d,
            IrqController::GicIts {
                mmio,
                phandle: phandle(props).unwrap_or(0),
            },
        );
    }

    if compatible_has(props, b"arm,gic-v2m-frame")
        && let Some(mmio) = reg
    {
        let spi_base = u32_prop(props, b"arm,msi-base-spi").unwrap_or(0);
        let n = u32_prop(props, b"arm,msi-num-spis").unwrap_or(0);
        let spi_count = u16::try_from(n).unwrap_or(0);
        push_irq(
            d,
            IrqController::GicV2m {
                mmio,
                spi_base,
                spi_count,
            },
        );
    }

    if compatible_has(props, b"arm,armv8-timer") || compatible_has(props, b"arm,armv7-timer") {
        let mut irqs = [0u32; 5];
        let mut nirq = 0u8;
        if let Some(v) = prop_named(props, b"interrupts") {
            // GIC specifier: 3 cells. ID is the second.
            let mut i = 0usize;
            while nirq < 5 {
                let id_at = i.checked_add(1);
                let Some(id_at) = id_at else { break };
                let Some(id) = cell(v, id_at) else { break };
                if let Some(slot) = irqs.get_mut(nirq as usize) {
                    *slot = id;
                    nirq = nirq.saturating_add(1);
                }
                i = match i.checked_add(3) {
                    Some(n) => n,
                    None => break,
                };
            }
        }
        push_timer(
            d,
            TimerDesc::ArmGeneric {
                irqs,
                nirq,
                clock_frequency: u32_prop(props, b"clock-frequency"),
            },
        );
    }

    if compatible_has(props, b"arm,pl031")
        && let Some(r) = reg
    {
        d.rtc = Some(MmioDev {
            base: r.start,
            size: r.size,
            dma_coherent: dma,
            msi_parent,
        });
    }

    if compatible_has(props, b"qemu,fw-cfg-mmio")
        && let Some(r) = reg
    {
        d.fw_cfg = Some(MmioDev {
            base: r.start,
            size: r.size,
            dma_coherent: dma,
            msi_parent,
        });
    }

    if compatible_has(props, b"virtio,mmio")
        && status_okay(props)
        && let Some(r) = reg
    {
        push_virtio(
            d,
            MmioDev {
                base: r.start,
                size: r.size,
                dma_coherent: dma,
                msi_parent,
            },
        );
    }

    if compatible_has(props, b"pci-host-ecam-generic")
        && let Some(r) = reg
    {
        let (first, last) = prop_named(props, b"bus-range")
            .and_then(|v| Some((be_u32(v, 0)?, be_u32(v, 4)?)))
            .map(|(a, b)| (a as u8, b as u8))
            .unwrap_or((0, 0xff));
        let segment = u32_prop(props, b"linux,pci-domain").unwrap_or(0) as u16;
        let mut host = PciHost {
            segment,
            first_bus: first,
            last_bus: last,
            ecam_base: r.start,
            dma_coherent: dma,
            msi_parent,
            ..PciHost::default()
        };
        fill_msi_map(&mut host, props);
        fill_interrupt_map(&mut host, props);
        push_pci(d, host);
    }
}

fn fill_msi_map(host: &mut PciHost, props: &[Prop<'_>]) {
    let Some(v) = prop_named(props, b"msi-map") else {
        return;
    };
    let mut i = 0usize;
    while host.msi_map_len < host.msi_map.len() {
        let Some(rid) = cell(v, i) else { break };
        let Some(parent) = cell(v, i.saturating_add(1)) else {
            break;
        };
        let Some(base) = cell(v, i.saturating_add(2)) else {
            break;
        };
        let Some(len) = cell(v, i.saturating_add(3)) else {
            break;
        };
        if let Some(slot) = host.msi_map.get_mut(host.msi_map_len) {
            *slot = MsiMapEntry {
                rid_base: rid,
                parent,
                msi_base: base,
                length: len,
            };
            host.msi_map_len = host.msi_map_len.saturating_add(1);
        }
        i = match i.checked_add(4) {
            Some(n) => n,
            None => break,
        };
    }
}

fn fill_interrupt_map(host: &mut PciHost, props: &[Prop<'_>]) {
    let Some(v) = prop_named(props, b"interrupt-map") else {
        return;
    };
    // Child unit address + interrupt + phandle + parent address + parent
    // interrupt. PCI virt: 3 + 1 + 1. GICv2/v3: 2 + 3 parent cells.
    let child_addr = u32_prop(props, b"#address-cells").unwrap_or(3);
    let child_irq = u32_prop(props, b"#interrupt-cells").unwrap_or(1);
    let Some(child_span) = child_addr.checked_add(child_irq) else {
        return;
    };
    let Some(stride) = child_span.checked_add(6) else {
        return;
    };
    let mut i = 0usize;
    while host.interrupt_map_len < host.interrupt_map.len() {
        let Some(child_hi) = cell(v, i) else { break };
        let Some(pin_at) = i.checked_add(child_addr as usize) else {
            break;
        };
        let Some(pin) = cell(v, pin_at) else { break };
        let Some(parent_at) = pin_at.checked_add(child_irq as usize) else {
            break;
        };
        let Some(parent) = cell(v, parent_at) else {
            break;
        };
        let Some(irq_type_at) = parent_at.checked_add(3) else {
            break;
        };
        let Some(irq_type) = cell(v, irq_type_at) else {
            break;
        };
        let Some(irq) = cell(v, irq_type_at.saturating_add(1)) else {
            break;
        };
        let Some(irq_flags) = cell(v, irq_type_at.saturating_add(2)) else {
            break;
        };
        if let Some(slot) = host.interrupt_map.get_mut(host.interrupt_map_len) {
            *slot = InterruptMapEntry {
                child_hi,
                pin,
                parent,
                irq_type,
                irq,
                irq_flags,
            };
            host.interrupt_map_len = host.interrupt_map_len.saturating_add(1);
        }
        i = match i.checked_add(stride as usize) {
            Some(n) => n,
            None => break,
        };
    }
}

fn walk(
    dtb: &[u8],
    mut visit: impl FnMut(&[u8], &[Prop<'_>], Inherit) -> Result<(), FdtError>,
) -> Result<u32, FdtError> {
    let h = parse_header(dtb)?;
    let st = struct_blob(dtb, &h)?;
    let strings = strings_blob(dtb, &h)?;
    let mut off = 0usize;
    let mut path = [0u8; PATH_CAP];
    let mut path_len = 0usize;
    let mut stack = [Inherit {
        addr_cells: DEFAULT_ADDR_CELLS,
        size_cells: DEFAULT_SIZE_CELLS,
        dma_coherent: false,
    }; DEPTH];
    let mut depth = 0usize;
    let mut nodes = 0u32;
    let mut props_buf = [Prop {
        name: &[],
        value: &[],
    }; PROP_CAP];

    loop {
        let tok = be_u32(st, off).ok_or(FdtError::Truncated)?;
        off = off.checked_add(4).ok_or(FdtError::Truncated)?;
        match tok {
            FDT_NOP => {}
            FDT_END => return Ok(nodes),
            FDT_BEGIN_NODE => {
                let name = cstr(st.get(off..).ok_or(FdtError::Truncated)?);
                let raw = name.len().checked_add(1).ok_or(FdtError::Truncated)?;
                off = align4(off.checked_add(raw).ok_or(FdtError::Truncated)?)
                    .ok_or(FdtError::Truncated)?;
                let parent = *stack.get(depth).ok_or(FdtError::BadToken)?;
                if !path_push(&mut path, &mut path_len, name) {
                    return Err(FdtError::Truncated);
                }
                let next = depth.checked_add(1).ok_or(FdtError::BadToken)?;
                if next >= DEPTH {
                    return Err(FdtError::BadToken);
                }
                // Collect this node's properties before visiting.
                let mut np = 0usize;
                loop {
                    let t = be_u32(st, off).ok_or(FdtError::Truncated)?;
                    if t != FDT_PROP && t != FDT_NOP {
                        break;
                    }
                    off = off.checked_add(4).ok_or(FdtError::Truncated)?;
                    if t == FDT_NOP {
                        continue;
                    }
                    let plen = be_u32(st, off).ok_or(FdtError::Truncated)? as usize;
                    let nameoff = be_u32(st, off.checked_add(4).ok_or(FdtError::Truncated)?)
                        .ok_or(FdtError::Truncated)?;
                    off = off.checked_add(8).ok_or(FdtError::Truncated)?;
                    let end = off.checked_add(plen).ok_or(FdtError::Truncated)?;
                    let value = st.get(off..end).ok_or(FdtError::Truncated)?;
                    off = align4(end).ok_or(FdtError::Truncated)?;
                    let pname = str_at(strings, nameoff).ok_or(FdtError::Truncated)?;
                    if let Some(slot) = props_buf.get_mut(np) {
                        *slot = Prop { name: pname, value };
                        np = np.saturating_add(1);
                    }
                }
                let props = props_buf.get(..np).unwrap_or(&[]);
                let mut child = parent;
                if let Some(v) = u32_prop(props, b"#address-cells") {
                    child.addr_cells = v;
                }
                if let Some(v) = u32_prop(props, b"#size-cells") {
                    child.size_cells = v;
                }
                if has_empty(props, b"dma-coherent") {
                    child.dma_coherent = true;
                }
                if let Some(slot) = stack.get_mut(next) {
                    *slot = child;
                }
                // This node's `reg` uses the parent's cells.
                visit(path.get(..path_len).unwrap_or(&[]), props, parent)?;
                depth = next;
                nodes = nodes.saturating_add(1);
            }
            FDT_END_NODE => {
                if depth == 0 {
                    return Err(FdtError::BadToken);
                }
                path_pop(&path, &mut path_len);
                depth = depth.saturating_sub(1);
            }
            FDT_PROP => return Err(FdtError::BadToken),
            _ => return Err(FdtError::BadToken),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "host test fixtures"
)]
mod tests {
    use super::*;
    use crate::machine::IrqController;

    const VIRT_82: &[u8] = include_bytes!("testdata/virt-8.2.dtb");
    const VIRT_82_SEC: &[u8] = include_bytes!("testdata/virt-8.2-secure.dtb");
    const VIRT_CUR: &[u8] = include_bytes!("testdata/virt-current.dtb");
    const RESERVED: &[u8] = include_bytes!("testdata/reserved-both.dtb");
    const TIMER5: &[u8] = include_bytes!("testdata/timer-5irq.dtb");

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
            d.irq_controllers.iter().take(d.irq_controller_count).any(
                |c| matches!(c, IrqController::GicV3 { dist, .. } if dist.start == 0x800_0000)
            )
        );
        assert!(
            d.irq_controllers.iter().take(d.irq_controller_count).any(
                |c| matches!(c, IrqController::GicIts { mmio, .. } if mmio.start == 0x808_0000)
            )
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
        assert_eq!(pci[0].first_bus, 0);
        assert_eq!(pci[0].last_bus, 0xff);
        assert!(pci[0].dma_coherent);
        assert_eq!(pci[0].msi_map_len, 1);
        assert_eq!(pci[0].msi_map[0].length, 0x1_0000);
        assert_eq!(pci[0].interrupt_map_len, 16);
        assert_eq!(pci[0].interrupt_map[0].pin, 1);
        assert_eq!(pci[0].interrupt_map[0].irq, 3);
        assert_eq!(d.virtio_mmio_count, 32);
        assert!(d.virtio_mmio[0].dma_coherent);
        assert_eq!(d.virtio_mmio[0].base, 0xa00_0000);
        let fw = d.fw_cfg.unwrap();
        assert_eq!(fw.base, 0x902_0000);
        assert!(fw.dma_coherent);
        let rtc = d.rtc.unwrap();
        assert_eq!(rtc.base, 0x901_0000);
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
}
