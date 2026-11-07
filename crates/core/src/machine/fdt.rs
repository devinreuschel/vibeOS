//! Flattened device tree walker and UART pick (Devicetree Specification v0.4
//! §§5.2–5.4; DESIGN §1.5: cited, no text copied).
//!
//! [`pick_pl011`] is the one console chooser: `/aliases` `serial0`, else the
//! first okay `arm,pl011` in tree order. [`parse`] fills [`MachineDesc`]
//! and calls it. `/chosen` is ignored. `memory@` is not RAM.
//!
//! Limits. `reg` is read with the parent's cell counts, keeping the low 64
//! bits, and no `ranges` translation is applied. Each `interrupt-map` row is
//! read as two parent address cells and three parent interrupt cells, without
//! resolving the parent; a map whose length is not a whole number of rows is
//! left empty and counted (`refused_interrupt_maps`). Rows past the host's
//! cap are counted (`dropped_interrupt_rows` for `interrupt-map`,
//! `dropped_msi_rows` for `msi-map`). A node keeps its first 32 properties
//! and the first four `arm,pl011` nodes; the rest are counted (`dropped_props`,
//! `dropped_uarts`). A `/reserved-memory` past its table, or a walk past its
//! caps, refuses the tree.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use core::fmt;

use super::{
    ConsoleDesc, CpuDesc, EnableMethod, InterruptMapEntry, IrqController, MachineDesc, MmioDev,
    MsiMapEntry, PciHost, PhysRange, TimerDesc, push_console, push_cpu, push_irq, push_pci,
    push_reserved, push_timer, push_virtio,
};
use crate::irq::gic::gic_intid;
use crate::limits::MAX_DT_REG_BYTES;

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
    /// A reserved range would not fit in [`super::MAX_RESERVED`], or its
    /// start is the last address of the space so it cannot be a half-open
    /// range.
    Reserved,
}

impl FdtError {
    pub fn as_str(self) -> &'static str {
        match self {
            FdtError::Truncated => "truncated",
            FdtError::BadMagic => "bad magic",
            FdtError::BadToken => "bad token",
            FdtError::Reserved => "reserved",
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

/// First GIC INTID and whether its trigger is edge (flags cell, bits 0-1:
/// 1 rising, 2 falling; 4 and 8 are level), from a 3-cell `interrupts`
/// specifier. `(0, false)` when absent.
fn first_gic_irq(props: &[Prop<'_>]) -> (u32, bool) {
    prop_named(props, b"interrupts")
        .and_then(|v| {
            let intid = gic_intid(cell(v, 0)?, cell(v, 1)?)?;
            Some((intid, cell(v, 2)? & 3 != 0))
        })
        .unwrap_or((0, false))
}

fn addr_cells(val: &[u8], start: usize, ncells: u32) -> Option<u64> {
    match ncells {
        0 => Some(0),
        1 => Some(u64::from(cell(val, start)?)),
        // The low two cells, as the big-endian number's low 64 bits.
        n => {
            let hi_at = start.checked_add(n as usize)?.checked_sub(2)?;
            let hi = cell(val, hi_at)?;
            let lo = cell(val, hi_at.checked_add(1)?)?;
            Some(u64::from(hi).wrapping_shl(32) | u64::from(lo))
        }
    }
}

fn first_reg(val: &[u8], addr_c: u32, size_c: u32) -> Option<PhysRange> {
    let start = addr_cells(val, 0, addr_c)?;
    let size = addr_cells(val, addr_c as usize, size_c)?;
    let r = PhysRange { start, size };
    // A device window the kernel would map: no wrap, no absurd length.
    (size <= MAX_DT_REG_BYTES && r.end().is_some()).then_some(r)
}

/// Every `(addr, size)` pair in a `reg` property. A partial trailing pair
/// is [`FdtError::Truncated`]. `ncells` above 2 still contributes only its
/// low 64 bits, and the stride still advances by the full cell count, same
/// as [`first_reg`].
fn each_reg(
    val: &[u8],
    addr_c: u32,
    size_c: u32,
    mut f: impl FnMut(PhysRange) -> Result<(), FdtError>,
) -> Result<(), FdtError> {
    let stride = addr_c.checked_add(size_c).ok_or(FdtError::Truncated)?;
    if stride == 0 {
        return if val.is_empty() {
            Ok(())
        } else {
            Err(FdtError::Truncated)
        };
    }
    let mut i = 0usize;
    loop {
        let off = i.checked_mul(4).ok_or(FdtError::Truncated)?;
        let rest = val.get(off..).unwrap_or(&[]);
        if rest.is_empty() {
            return Ok(());
        }
        let pair_bytes = (stride as usize)
            .checked_mul(4)
            .ok_or(FdtError::Truncated)?;
        if rest.len() < pair_bytes {
            return Err(FdtError::Truncated);
        }
        let start = addr_cells(val, i, addr_c).ok_or(FdtError::Truncated)?;
        let size_i = i.checked_add(addr_c as usize).ok_or(FdtError::Truncated)?;
        let size = addr_cells(val, size_i, size_c).ok_or(FdtError::Truncated)?;
        f(PhysRange { start, size })?;
        i = i.checked_add(stride as usize).ok_or(FdtError::Truncated)?;
    }
}

fn is_reserved_memory(name: &[u8]) -> bool {
    name == b"reserved-memory" || name.starts_with(b"reserved-memory@")
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
    let mut dropped = 0u32;
    pick_pl011_full(dtb, &mut dropped).map(|c| c.base)
}

/// `dropped` counts the `arm,pl011` nodes past [`UART_CAP`].
fn pick_pl011_full(dtb: &[u8], dropped: &mut u32) -> Option<UartCand> {
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
    if walk(dtb, false, |path, props, inh| {
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
        {
            match cands.get_mut(n) {
                Some(slot) => {
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
                None => *dropped = dropped.saturating_add(1),
            }
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

/// A DTB whose reservations were recorded. [`Parsed::Incomplete`] means the
/// device walk hit a path or depth cap; the memreserve block and
/// `/reserved-memory` are still in the descriptor.
#[derive(Clone, Copy, Debug)]
pub enum Parsed {
    Complete(MachineDesc),
    Incomplete(MachineDesc, FdtError),
}

/// Parse a DTB. [`Err`] means the reservations themselves failed (bad
/// magic, a truncated blob, or a reserved range that will not fit). A
/// device-tree walk that cannot finish is [`Parsed::Incomplete`], with the
/// reservations already stored. RAM stays on the Limine memory map.
pub fn parse_dtb(dtb: &[u8]) -> Result<Parsed, FdtError> {
    let h = parse_header(dtb)?;
    let mut d = MachineDesc::default();
    fill_mem_rsv(&mut d, dtb, &h)?;
    walk(dtb, true, |path, props, inh| {
        take_reserved(&mut d, path, props, inh)
    })?;
    match walk(dtb, false, |path, props, inh| {
        if path_starts(path, b"/reserved-memory") || path_is(path, b"/chosen") {
            return Ok(());
        }
        fill_node(&mut d, path, props, inh);
        Ok(())
    }) {
        Ok(walked) => {
            d.node_count = walked.nodes;
            d.dropped_props = walked.dropped_props;
            if let Some(c) = pick_pl011_full(dtb, &mut d.dropped_uarts) {
                push_console(
                    &mut d,
                    ConsoleDesc {
                        base: c.base,
                        size: c.size,
                    },
                );
            }
            Ok(Parsed::Complete(d))
        }
        Err(e) => Ok(Parsed::Incomplete(d, e)),
    }
}

/// Parse a DTB into [`MachineDesc`]. A walk that cannot finish is [`Err`],
/// same as a reservation that will not fit.
pub fn parse(dtb: &[u8]) -> Result<MachineDesc, FdtError> {
    match parse_dtb(dtb)? {
        Parsed::Complete(d) => Ok(d),
        Parsed::Incomplete(_, e) => Err(e),
    }
}

fn take_reserved(
    d: &mut MachineDesc,
    path: &[u8],
    props: &[Prop<'_>],
    inh: Inherit,
) -> Result<(), FdtError> {
    if path_starts(path, b"/reserved-memory") && !path_is(path, b"/reserved-memory") {
        let no_map = has_empty(props, b"no-map");
        if let Some(v) = prop_named(props, b"reg") {
            each_reg(v, inh.addr_cells, inh.size_cells, |r| {
                push_reserved(d, r, no_map).map_err(|_| FdtError::Reserved)
            })?;
        }
    }
    Ok(())
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
            push_reserved(d, PhysRange { start: addr, size }, false)
                .map_err(|_| FdtError::Reserved)?;
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
        // A missing or unknown `method` names no conduit: no CPU can be
        // started through it, so it is not taken for HVC.
        let m = match prop_named(props, b"method").map(cstr) {
            Some(b"hvc") => EnableMethod::Psci { hvc: true },
            Some(b"smc") => EnableMethod::Psci { hvc: false },
            _ => EnableMethod::None,
        };
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
        let (irq, irq_edge) = first_gic_irq(props);
        d.rtc = Some(MmioDev {
            base: r.start,
            size: r.size,
            dma_coherent: dma,
            msi_parent,
            irq,
            irq_edge,
        });
    }

    if compatible_has(props, b"qemu,fw-cfg-mmio")
        && let Some(r) = reg
    {
        let (irq, irq_edge) = first_gic_irq(props);
        d.fw_cfg = Some(MmioDev {
            base: r.start,
            size: r.size,
            dma_coherent: dma,
            msi_parent,
            irq,
            irq_edge,
        });
    }

    if compatible_has(props, b"virtio,mmio")
        && status_okay(props)
        && let Some(r) = reg
    {
        let (irq, irq_edge) = first_gic_irq(props);
        push_virtio(
            d,
            MmioDev {
                base: r.start,
                size: r.size,
                dma_coherent: dma,
                msi_parent,
                irq,
                irq_edge,
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
            ecam_size: r.size,
            dma_coherent: dma,
            msi_parent,
            ..PciHost::default()
        };
        fill_msi_map(&mut host, props, &mut d.dropped_msi_rows);
        if fill_interrupt_map(&mut host, props, &mut d.dropped_interrupt_rows) {
            d.refused_interrupt_maps = d.refused_interrupt_maps.saturating_add(1);
        }
        push_pci(d, host);
    }
}

/// Rows past [`MAX_MSI_MAP`] are added to `dropped`.
fn fill_msi_map(host: &mut PciHost, props: &[Prop<'_>], dropped: &mut u32) {
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
    // Each row is four 32-bit cells; the rows less the ones the cap let in.
    let rows = v.len().checked_div(4 * 4).unwrap_or(0);
    let past_cap = rows.saturating_sub(host.msi_map_len);
    *dropped = dropped.saturating_add(u32::try_from(past_cap).unwrap_or(u32::MAX));
}

/// `true` when the map is refused: its length is not a whole number of rows.
/// Rows past [`MAX_INTERRUPT_MAP`] are added to `dropped`.
fn fill_interrupt_map(host: &mut PciHost, props: &[Prop<'_>], dropped: &mut u32) -> bool {
    let Some(v) = prop_named(props, b"interrupt-map") else {
        return false;
    };
    // Child unit address + interrupt + phandle + parent address + parent
    // interrupt. PCI virt: 3 + 1 + 1. GICv2/v3: 2 + 3 parent cells.
    let child_addr = u32_prop(props, b"#address-cells").unwrap_or(3);
    let child_irq = u32_prop(props, b"#interrupt-cells").unwrap_or(1);
    let Some(child_span) = child_addr.checked_add(child_irq) else {
        return true;
    };
    let Some(stride) = child_span.checked_add(6) else {
        return true;
    };
    // Any other length is not a whole number of rows of this shape.
    let Some(row_bytes) = (stride as usize).checked_mul(4) else {
        return true;
    };
    if v.len().checked_rem(row_bytes) != Some(0) {
        return true;
    }
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
    // The map's rows less the ones the cap let in.
    let rows = v.len().checked_div(row_bytes).unwrap_or(0);
    let past_cap = rows.saturating_sub(host.interrupt_map_len);
    *dropped = dropped.saturating_add(u32::try_from(past_cap).unwrap_or(u32::MAX));
    false
}

#[derive(Clone, Copy)]
struct WalkTotals {
    nodes: u32,
    /// Properties past [`PROP_CAP`] on the nodes visited, not passed on.
    dropped_props: u32,
}

fn walk(
    dtb: &[u8],
    skip_unrelated: bool,
    mut visit: impl FnMut(&[u8], &[Prop<'_>], Inherit) -> Result<(), FdtError>,
) -> Result<WalkTotals, FdtError> {
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
    let mut reserved_depth: Option<usize> = None;
    let mut nodes = 0u32;
    let mut dropped_props = 0u32;
    let mut props_buf = [Prop {
        name: &[],
        value: &[],
    }; PROP_CAP];

    loop {
        let tok = be_u32(st, off).ok_or(FdtError::Truncated)?;
        off = off.checked_add(4).ok_or(FdtError::Truncated)?;
        match tok {
            FDT_NOP => {}
            FDT_END => {
                return Ok(WalkTotals {
                    nodes,
                    dropped_props,
                });
            }
            FDT_BEGIN_NODE => {
                let name = cstr(st.get(off..).ok_or(FdtError::Truncated)?);
                let raw = name.len().checked_add(1).ok_or(FdtError::Truncated)?;
                off = align4(off.checked_add(raw).ok_or(FdtError::Truncated)?)
                    .ok_or(FdtError::Truncated)?;
                let parent = *stack.get(depth).ok_or(FdtError::BadToken)?;
                let pushed = path_push(&mut path, &mut path_len, name);
                let next = depth.checked_add(1).ok_or(FdtError::BadToken)?;
                if !pushed || next >= DEPTH {
                    if pushed {
                        path_pop(&path, &mut path_len);
                    }
                    // A root `/reserved-memory` and anything already inside it
                    // must be recorded. Anything else past the path or depth
                    // cap is skipped only on the reservation walk, so a later
                    // reserved node is still seen.
                    let inside = reserved_depth.is_some();
                    let opens = depth == 1 && is_reserved_memory(name);
                    if skip_unrelated && !inside && !opens {
                        skip_node(st, &mut off)?;
                        continue;
                    }
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
                    } else {
                        dropped_props = dropped_props.saturating_add(1);
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
                if depth == 1 && is_reserved_memory(name) {
                    reserved_depth = Some(next);
                }
                depth = next;
                nodes = nodes.saturating_add(1);
            }
            FDT_END_NODE => {
                if depth == 0 {
                    return Err(FdtError::BadToken);
                }
                if reserved_depth == Some(depth) {
                    reserved_depth = None;
                }
                path_pop(&path, &mut path_len);
                depth = depth.saturating_sub(1);
            }
            FDT_PROP => return Err(FdtError::BadToken),
            _ => return Err(FdtError::BadToken),
        }
    }
}

/// Consume the rest of the node whose name is already taken, including its
/// children. `nest` starts at 1 for that node.
fn skip_node(st: &[u8], off: &mut usize) -> Result<(), FdtError> {
    let mut nest = 1u32;
    while nest > 0 {
        let tok = be_u32(st, *off).ok_or(FdtError::Truncated)?;
        *off = (*off).checked_add(4).ok_or(FdtError::Truncated)?;
        match tok {
            FDT_NOP => {}
            FDT_END => return Err(FdtError::Truncated),
            FDT_BEGIN_NODE => {
                let name = cstr(st.get(*off..).ok_or(FdtError::Truncated)?);
                let raw = name.len().checked_add(1).ok_or(FdtError::Truncated)?;
                *off = align4((*off).checked_add(raw).ok_or(FdtError::Truncated)?)
                    .ok_or(FdtError::Truncated)?;
                nest = nest.checked_add(1).ok_or(FdtError::Truncated)?;
            }
            FDT_END_NODE => {
                nest = nest.checked_sub(1).ok_or(FdtError::BadToken)?;
            }
            FDT_PROP => {
                let plen = be_u32(st, *off).ok_or(FdtError::Truncated)? as usize;
                *off = (*off).checked_add(8).ok_or(FdtError::Truncated)?;
                let end = (*off).checked_add(plen).ok_or(FdtError::Truncated)?;
                *off = align4(end).ok_or(FdtError::Truncated)?;
            }
            _ => return Err(FdtError::BadToken),
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "host test fixtures"
)]
#[path = "fdt_tests.rs"]
mod tests;
