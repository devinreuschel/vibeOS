//! ACPI table parsers. DESIGN §7.1 / ROADMAP §2.4.
//!
//! Pure byte-slice logic so `cargo test --lib` (via hostlib) covers it.
//! Packed fields go through the `read_unaligned_*` helpers — ACPI tables
//! have no alignment guarantees (HPET's period tick sits at offset 53).
//! No DSDT/SSDT/AML. MCFG is stashed, not walked.
//!
//! Firmware tables are untrusted input (DESIGN §2.10), so no index or
//! arithmetic here can panic: an out-of-range access is `Truncated`.
#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use core::fmt;

pub const RSDP_SIG: &[u8; 8] = b"RSD PTR ";
pub const RSDP_V1_LEN: usize = 20;
pub const RSDP_V2_LEN: usize = 36;
pub const SDT_HEADER_LEN: usize = 36;

pub const SIG_XSDT: &[u8; 4] = b"XSDT";
pub const SIG_RSDT: &[u8; 4] = b"RSDT";
pub const SIG_MADT: &[u8; 4] = b"APIC";
pub const SIG_FADT: &[u8; 4] = b"FACP";
pub const SIG_HPET: &[u8; 4] = b"HPET";
pub const SIG_MCFG: &[u8; 4] = b"MCFG";

pub const GAS_SYSTEM_MEMORY: u8 = 0;
pub const GAS_SYSTEM_IO: u8 = 1;

pub const MADT_TYPE_LAPIC: u8 = 0;
pub const MADT_TYPE_IOAPIC: u8 = 1;
pub const MADT_TYPE_ISO: u8 = 2;
pub const MADT_TYPE_LAPIC_ADDR_OVERRIDE: u8 = 5;

pub const LAPIC_ENABLED: u32 = 1 << 0;

pub const MAX_CPUS: usize = 64;
pub const MAX_IOAPICS: usize = 16;
pub const MAX_ISOS: usize = 32;

/// Physical memory view. Kernel backs this with the physmap; host tests
/// back it with a bag of synthetic tables.
pub trait PhysMem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum AcpiError {
    Truncated,
    BadSignature,
    BadChecksum,
    HpetZeroAddress,
    HpetIoSpace,
    NoRootTable,
}

impl AcpiError {
    pub fn as_str(self) -> &'static str {
        match self {
            AcpiError::Truncated => "truncated",
            AcpiError::BadSignature => "bad signature",
            AcpiError::BadChecksum => "bad checksum",
            AcpiError::HpetZeroAddress => "hpet zero address",
            AcpiError::HpetIoSpace => "hpet io space",
            AcpiError::NoRootTable => "no xsdt/rsdt",
        }
    }
}

impl fmt::Display for AcpiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ------------------ unaligned LE loads ------------------

#[inline]
pub fn read_unaligned_u8(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}

/// The `N` bytes of `b` at `off`, or `None` past the end or on overflow.
#[inline]
fn bytes_at<const N: usize>(b: &[u8], off: usize) -> Option<[u8; N]> {
    b.get(off..off.checked_add(N)?)?.try_into().ok()
}

#[inline]
pub fn read_unaligned_u16(b: &[u8], off: usize) -> Option<u16> {
    bytes_at(b, off).map(u16::from_le_bytes)
}

#[inline]
pub fn read_unaligned_u32(b: &[u8], off: usize) -> Option<u32> {
    bytes_at(b, off).map(u32::from_le_bytes)
}

#[inline]
pub fn read_unaligned_u64(b: &[u8], off: usize) -> Option<u64> {
    bytes_at(b, off).map(u64::from_le_bytes)
}

#[inline]
pub fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b))
}

fn checksum_ok(bytes: &[u8]) -> bool {
    checksum(bytes) == 0
}

fn checksum_range<P: PhysMem>(phys: &P, addr: u64, len: u32) -> Result<u8, AcpiError> {
    if len == 0 {
        return Ok(0);
    }
    let mut sum = 0u8;
    let mut tmp = [0u8; 256];
    let mut off = 0u32;
    while let Some(left) = len.checked_sub(off).filter(|&l| l != 0) {
        let n = (left as usize).min(tmp.len());
        let at = addr
            .checked_add(u64::from(off))
            .ok_or(AcpiError::Truncated)?;
        let chunk = tmp.get_mut(..n).ok_or(AcpiError::Truncated)?;
        if !phys.read(at, chunk) {
            return Err(AcpiError::Truncated);
        }
        sum = sum.wrapping_add(checksum(chunk));
        off = off.checked_add(n as u32).ok_or(AcpiError::Truncated)?;
    }
    Ok(sum)
}

fn sig4(b: &[u8], off: usize) -> Option<[u8; 4]> {
    bytes_at(b, off)
}

/// `off + k` as an offset into a table, or `Truncated` on overflow.
fn at(off: usize, k: usize) -> Result<usize, AcpiError> {
    off.checked_add(k).ok_or(AcpiError::Truncated)
}

// ------------------ RSDP ------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RsdpInfo {
    pub revision: u8,
    pub rsdt_addr: u32,
    pub xsdt_addr: u64,
}

pub fn parse_rsdp(bytes: &[u8]) -> Result<RsdpInfo, AcpiError> {
    let v1 = bytes.get(..RSDP_V1_LEN).ok_or(AcpiError::Truncated)?;
    if v1.get(0..8) != Some(&RSDP_SIG[..]) {
        return Err(AcpiError::BadSignature);
    }
    if !checksum_ok(v1) {
        return Err(AcpiError::BadChecksum);
    }
    let revision = read_unaligned_u8(bytes, 15).ok_or(AcpiError::Truncated)?;
    let rsdt_addr = read_unaligned_u32(bytes, 16).ok_or(AcpiError::Truncated)?;
    if revision == 0 {
        return Ok(RsdpInfo {
            revision,
            rsdt_addr,
            xsdt_addr: 0,
        });
    }
    // v2+: length at 20, xsdt at 24, extended checksum over `length`.
    if bytes.len() < RSDP_V2_LEN {
        return Err(AcpiError::Truncated);
    }
    let length = read_unaligned_u32(bytes, 20).ok_or(AcpiError::Truncated)? as usize;
    if length < RSDP_V2_LEN {
        return Err(AcpiError::Truncated);
    }
    if !checksum_ok(bytes.get(..length).ok_or(AcpiError::Truncated)?) {
        return Err(AcpiError::BadChecksum);
    }
    let xsdt_addr = read_unaligned_u64(bytes, 24).ok_or(AcpiError::Truncated)?;
    Ok(RsdpInfo {
        revision,
        rsdt_addr,
        xsdt_addr,
    })
}

// ------------------ GAS ------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gas {
    pub space_id: u8,
    pub bit_width: u8,
    pub bit_offset: u8,
    pub access_size: u8,
    pub address: u64,
}

impl Gas {
    pub const fn empty() -> Self {
        Self {
            space_id: 0,
            bit_width: 0,
            bit_offset: 0,
            access_size: 0,
            address: 0,
        }
    }

    pub fn is_empty(self) -> bool {
        self.address == 0 && self.space_id == 0 && self.bit_width == 0
    }
}

pub fn parse_gas(bytes: &[u8], off: usize) -> Result<Gas, AcpiError> {
    Ok(Gas {
        space_id: read_unaligned_u8(bytes, off).ok_or(AcpiError::Truncated)?,
        bit_width: read_unaligned_u8(bytes, at(off, 1)?).ok_or(AcpiError::Truncated)?,
        bit_offset: read_unaligned_u8(bytes, at(off, 2)?).ok_or(AcpiError::Truncated)?,
        access_size: read_unaligned_u8(bytes, at(off, 3)?).ok_or(AcpiError::Truncated)?,
        address: read_unaligned_u64(bytes, at(off, 4)?).ok_or(AcpiError::Truncated)?,
    })
}

// ------------------ SDT header ------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SdtHeader {
    pub signature: [u8; 4],
    pub length: u32,
    pub revision: u8,
}

pub fn parse_sdt_header(bytes: &[u8]) -> Result<SdtHeader, AcpiError> {
    if bytes.len() < SDT_HEADER_LEN {
        return Err(AcpiError::Truncated);
    }
    let length = read_unaligned_u32(bytes, 4).ok_or(AcpiError::Truncated)?;
    if length < SDT_HEADER_LEN as u32 {
        return Err(AcpiError::Truncated);
    }
    Ok(SdtHeader {
        signature: sig4(bytes, 0).ok_or(AcpiError::Truncated)?,
        length,
        revision: read_unaligned_u8(bytes, 8).ok_or(AcpiError::Truncated)?,
    })
}

pub fn validate_sdt(bytes: &[u8]) -> Result<SdtHeader, AcpiError> {
    let hdr = parse_sdt_header(bytes)?;
    let table = bytes
        .get(..hdr.length as usize)
        .ok_or(AcpiError::Truncated)?;
    if !checksum_ok(table) {
        return Err(AcpiError::BadChecksum);
    }
    Ok(hdr)
}

// ------------------ MADT ------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoApic {
    pub id: u8,
    pub addr: u32,
    pub gsi_base: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Iso {
    pub irq: u8,
    pub gsi: u32,
    pub flags: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct MadtInfo {
    pub lapic_base: u64,
    pub pcat_compat: bool,
    pub cpu_count: usize,
    pub apic_ids: [u8; MAX_CPUS],
    pub ioapic_count: usize,
    pub ioapics: [IoApic; MAX_IOAPICS],
    pub iso_count: usize,
    pub isos: [Iso; MAX_ISOS],
}

impl Default for MadtInfo {
    fn default() -> Self {
        Self {
            lapic_base: 0,
            pcat_compat: false,
            cpu_count: 0,
            apic_ids: [0; MAX_CPUS],
            ioapic_count: 0,
            ioapics: [IoApic::default(); MAX_IOAPICS],
            iso_count: 0,
            isos: [Iso::default(); MAX_ISOS],
        }
    }
}

pub fn parse_madt(bytes: &[u8]) -> Result<MadtInfo, AcpiError> {
    if bytes.len() < SDT_HEADER_LEN + 8 {
        return Err(AcpiError::Truncated);
    }
    let hdr = parse_sdt_header(bytes)?;
    if &hdr.signature != SIG_MADT {
        return Err(AcpiError::BadSignature);
    }
    let table_len = (hdr.length as usize).min(bytes.len());
    let mut info = MadtInfo {
        lapic_base: read_unaligned_u32(bytes, 36).ok_or(AcpiError::Truncated)? as u64,
        pcat_compat: read_unaligned_u32(bytes, 40).ok_or(AcpiError::Truncated)? & 1 != 0,
        ..MadtInfo::default()
    };

    // Each record is `type, length, body`; `tail` is the table from `off`.
    let mut off = 44usize;
    while let Some(tail) = bytes.get(off..table_len) {
        let (Some(&typ), Some(&len)) = (tail.first(), tail.get(1)) else {
            break;
        };
        let len = usize::from(len);
        if len < 2 {
            // zero/garbage length: stop rather than spin
            break;
        }
        let Some(rec) = tail.get(..len) else {
            // truncated entry: keep what we already parsed
            break;
        };
        // A `get_mut` that finds a slot bounds each count below its array's
        // length, so the `saturating_add`s below never saturate.
        match typ {
            MADT_TYPE_LAPIC => {
                if len >= 8 {
                    let flags = read_unaligned_u32(rec, 4).unwrap_or(0);
                    if flags & LAPIC_ENABLED != 0
                        && let Some(slot) = info.apic_ids.get_mut(info.cpu_count)
                    {
                        *slot = read_unaligned_u8(rec, 3).unwrap_or(0);
                        info.cpu_count = info.cpu_count.saturating_add(1);
                    }
                }
            }
            MADT_TYPE_IOAPIC => {
                if len >= 12
                    && let Some(slot) = info.ioapics.get_mut(info.ioapic_count)
                {
                    *slot = IoApic {
                        id: read_unaligned_u8(rec, 2).unwrap_or(0),
                        addr: read_unaligned_u32(rec, 4).unwrap_or(0),
                        gsi_base: read_unaligned_u32(rec, 8).unwrap_or(0),
                    };
                    info.ioapic_count = info.ioapic_count.saturating_add(1);
                }
            }
            MADT_TYPE_ISO => {
                if len >= 10
                    && let Some(slot) = info.isos.get_mut(info.iso_count)
                {
                    *slot = Iso {
                        irq: read_unaligned_u8(rec, 3).unwrap_or(0),
                        gsi: read_unaligned_u32(rec, 4).unwrap_or(0),
                        flags: read_unaligned_u16(rec, 8).unwrap_or(0),
                    };
                    info.iso_count = info.iso_count.saturating_add(1);
                }
            }
            MADT_TYPE_LAPIC_ADDR_OVERRIDE if len >= 12 => {
                info.lapic_base = read_unaligned_u64(rec, 4).unwrap_or(info.lapic_base);
            }
            _ => {}
        }
        let Some(next) = off.checked_add(len) else {
            break;
        };
        off = next;
    }
    Ok(info)
}

// ------------------ HPET ------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HpetInfo {
    pub base: u64,
    pub minimum_tick: u16,
    /// Femtoseconds. The ACPI table does not carry this; the kernel fills
    /// it from GEN_CAP after the page is UC. Zero means "not read yet".
    pub period_fs: u32,
}

pub fn parse_hpet(bytes: &[u8]) -> Result<HpetInfo, AcpiError> {
    // Header 36 + event_timer_block_id 4 + GAS 12 + number 1 + min tick 2 + prot 1
    if bytes.len() < 56 {
        return Err(AcpiError::Truncated);
    }
    let hdr = parse_sdt_header(bytes)?;
    if &hdr.signature != SIG_HPET {
        return Err(AcpiError::BadSignature);
    }
    let gas = parse_gas(bytes, 40)?;
    if gas.address == 0 {
        return Err(AcpiError::HpetZeroAddress);
    }
    if gas.space_id != GAS_SYSTEM_MEMORY {
        return Err(AcpiError::HpetIoSpace);
    }
    Ok(HpetInfo {
        base: gas.address,
        minimum_tick: read_unaligned_u16(bytes, 53).ok_or(AcpiError::Truncated)?,
        period_fs: 0,
    })
}

// ------------------ FADT ------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FadtInfo {
    pub iapc_boot_arch: u16,
    pub reset: Gas,
    pub reset_value: u8,
    pub sleep_control: Gas,
    pub sleep_status: Gas,
    /// The ACPI PM timer, when the FADT names a usable block.
    pub pm_timer: Option<PmTimer>,
}

/// The ACPI PM timer's `TMR_VAL` port and how many bits of it count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PmTimer {
    pub port: u16,
    /// 24, or 32 with the FADT's `TMR_VAL_EXT` flag.
    pub width: u8,
}

/// FADT `PM_TMR_BLK`, a u32 port (ACPI 6.5 §5.2.9).
const FADT_PM_TMR_BLK: usize = 76;
/// FADT `PM_TMR_LEN`, a u8: 4 when `PM_TMR_BLK` is a timer.
const FADT_PM_TMR_LEN: usize = 91;
/// FADT `Flags`, a u32.
const FADT_FLAGS: usize = 112;
/// FADT `X_PM_TMR_BLK`, a GAS.
const FADT_X_PM_TMR_BLK: usize = 208;
/// `Flags` bit 8: the PM timer counts 32 bits, not 24.
const FADT_TMR_VAL_EXT: u32 = 1 << 8;

/// The PM timer of a FADT whose first `n` bytes are in `bytes`:
/// `X_PM_TMR_BLK` when it holds a nonzero address, which must then be a
/// SystemIO port, else `PM_TMR_BLK` when `PM_TMR_LEN` is 4 (ACPI's rule for
/// a zero extended field). None for a missing, zero, non-port or truncated
/// block.
fn fadt_pm_timer(bytes: &[u8], n: usize) -> Option<PmTimer> {
    let flags = if n >= 116 {
        read_unaligned_u32(bytes, FADT_FLAGS).unwrap_or(0)
    } else {
        0
    };
    let width = if flags & FADT_TMR_VAL_EXT != 0 {
        32
    } else {
        24
    };
    let x = if n >= 220 {
        parse_gas(bytes, FADT_X_PM_TMR_BLK).ok()
    } else {
        None
    };
    let port = match x.filter(|g| g.address != 0) {
        Some(g) if g.space_id == GAS_SYSTEM_IO => u16::try_from(g.address).ok()?,
        Some(_) | None => {
            if n < 92 || read_unaligned_u8(bytes, FADT_PM_TMR_LEN)? != 4 {
                return None;
            }
            u16::try_from(read_unaligned_u32(bytes, FADT_PM_TMR_BLK)?).ok()?
        }
    };
    (port != 0).then_some(PmTimer { port, width })
}

impl FadtInfo {
    /// DESIGN §7.1: `iapc_boot_arch` bit 0, used by §2.3 to skip the 8259.
    pub fn legacy_8259(self) -> bool {
        self.iapc_boot_arch & 1 != 0
    }
}

pub fn parse_fadt(bytes: &[u8]) -> Result<FadtInfo, AcpiError> {
    let hdr = parse_sdt_header(bytes)?;
    if &hdr.signature != SIG_FADT {
        return Err(AcpiError::BadSignature);
    }
    let n = (hdr.length as usize).min(bytes.len());
    let mut info = FadtInfo {
        iapc_boot_arch: 0,
        reset: Gas::empty(),
        reset_value: 0,
        sleep_control: Gas::empty(),
        sleep_status: Gas::empty(),
        pm_timer: None,
    };
    info.pm_timer = fadt_pm_timer(bytes, n);
    // IAPC_BOOT_ARCH is the u16 at 109, so it needs 111 bytes.
    if n >= 111 {
        info.iapc_boot_arch = read_unaligned_u16(bytes, 109).unwrap_or(0);
    }
    if n >= 129 {
        info.reset = parse_gas(bytes, 116).unwrap_or_else(|_| Gas::empty());
        info.reset_value = read_unaligned_u8(bytes, 128).unwrap_or(0);
    }
    // SLEEP_CONTROL GAS is 12 bytes at 244; SLEEP_STATUS at 256.
    if n >= 256 {
        info.sleep_control = parse_gas(bytes, 244).unwrap_or_else(|_| Gas::empty());
    }
    if n >= 268 {
        info.sleep_status = parse_gas(bytes, 256).unwrap_or_else(|_| Gas::empty());
    }
    Ok(info)
}

// ------------------ MCFG ------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct McfgInfo {
    pub ecam_base: u64,
    pub segment: u16,
    pub start_bus: u8,
    pub end_bus: u8,
}

pub fn parse_mcfg(bytes: &[u8]) -> Result<McfgInfo, AcpiError> {
    let hdr = parse_sdt_header(bytes)?;
    if &hdr.signature != SIG_MCFG {
        return Err(AcpiError::BadSignature);
    }
    let n = (hdr.length as usize).min(bytes.len());
    // header 36 + reserved 8 + one 16-byte allocation
    if n < 60 {
        return Err(AcpiError::Truncated);
    }
    Ok(McfgInfo {
        ecam_base: read_unaligned_u64(bytes, 44).ok_or(AcpiError::Truncated)?,
        segment: read_unaligned_u16(bytes, 52).ok_or(AcpiError::Truncated)?,
        start_bus: read_unaligned_u8(bytes, 54).ok_or(AcpiError::Truncated)?,
        end_bus: read_unaligned_u8(bytes, 55).ok_or(AcpiError::Truncated)?,
    })
}

// ------------------ walk ------------------

#[derive(Clone, Copy, Debug)]
pub struct AcpiInfo {
    pub used_xsdt: bool,
    pub table_count: usize,
    pub madt: Option<MadtInfo>,
    pub hpet: Option<HpetInfo>,
    pub fadt: Option<FadtInfo>,
    pub mcfg: Option<McfgInfo>,
}

impl AcpiInfo {
    pub fn cpu_count(&self) -> usize {
        self.madt.as_ref().map(|m| m.cpu_count).unwrap_or(0)
    }
    pub fn ioapic_count(&self) -> usize {
        self.madt.as_ref().map(|m| m.ioapic_count).unwrap_or(0)
    }
    pub fn hpet_present(&self) -> bool {
        self.hpet.is_some()
    }
}

fn read_exact<P: PhysMem>(phys: &P, addr: u64, buf: &mut [u8]) -> Result<(), AcpiError> {
    if phys.read(addr, buf) {
        Ok(())
    } else {
        Err(AcpiError::Truncated)
    }
}

fn load_sdt_header<P: PhysMem>(phys: &P, addr: u64) -> Result<SdtHeader, AcpiError> {
    let mut hdr = [0u8; SDT_HEADER_LEN];
    read_exact(phys, addr, &mut hdr)?;
    parse_sdt_header(&hdr)
}

/// Checksum a table at `addr` without keeping it, then optionally load a
/// prefix into `buf` for parsing. Returns the prefix length loaded.
fn load_checked<'a, P: PhysMem>(
    phys: &P,
    addr: u64,
    buf: &'a mut [u8],
) -> Result<(SdtHeader, &'a [u8]), AcpiError> {
    let hdr = load_sdt_header(phys, addr)?;
    if checksum_range(phys, addr, hdr.length)? != 0 {
        return Err(AcpiError::BadChecksum);
    }
    let n = (hdr.length as usize).min(buf.len());
    let prefix = buf.get_mut(..n).ok_or(AcpiError::Truncated)?;
    read_exact(phys, addr, prefix)?;
    Ok((hdr, prefix))
}

fn walk_root<P: PhysMem>(
    phys: &P,
    root_addr: u64,
    ptr_size: usize,
    expect_sig: &[u8; 4],
) -> Result<AcpiInfo, AcpiError> {
    let hdr = load_sdt_header(phys, root_addr)?;
    if &hdr.signature != expect_sig {
        return Err(AcpiError::BadSignature);
    }
    if checksum_range(phys, root_addr, hdr.length)? != 0 {
        return Err(AcpiError::BadChecksum);
    }
    let body = (hdr.length as usize)
        .checked_sub(SDT_HEADER_LEN)
        .ok_or(AcpiError::Truncated)?;
    let n_entries = body.checked_div(ptr_size).ok_or(AcpiError::Truncated)?;

    let mut info = AcpiInfo {
        used_xsdt: expect_sig == SIG_XSDT,
        table_count: 0,
        madt: None,
        hpet: None,
        fadt: None,
        mcfg: None,
    };
    let mut scratch = [0u8; 4096];

    for i in 0..n_entries {
        let ptr_addr = i
            .checked_mul(ptr_size)
            .and_then(|o| o.checked_add(SDT_HEADER_LEN))
            .and_then(|o| root_addr.checked_add(o as u64))
            .ok_or(AcpiError::Truncated)?;
        let table_addr = if ptr_size == 8 {
            let mut raw = [0u8; 8];
            read_exact(phys, ptr_addr, &mut raw)?;
            u64::from_le_bytes(raw)
        } else {
            let mut raw = [0u8; 4];
            read_exact(phys, ptr_addr, &mut raw)?;
            u32::from_le_bytes(raw) as u64
        };
        if table_addr == 0 {
            continue;
        }
        let (th, slice) = match load_checked(phys, table_addr, &mut scratch) {
            Ok(v) => v,
            Err(AcpiError::BadChecksum) | Err(AcpiError::Truncated) => continue,
            Err(e) => return Err(e),
        };
        // At most `n_entries`, so this never saturates.
        info.table_count = info.table_count.saturating_add(1);
        match &th.signature {
            s if s == SIG_MADT => {
                if let Ok(m) = parse_madt(slice) {
                    info.madt = Some(m);
                }
            }
            s if s == SIG_HPET => match parse_hpet(slice) {
                Ok(h) => info.hpet = Some(h),
                Err(AcpiError::HpetZeroAddress) | Err(AcpiError::HpetIoSpace) => {}
                Err(_) => {}
            },
            s if s == SIG_FADT => {
                if let Ok(f) = parse_fadt(slice) {
                    info.fadt = Some(f);
                }
            }
            s if s == SIG_MCFG => {
                if let Ok(m) = parse_mcfg(slice) {
                    info.mcfg = Some(m);
                }
            }
            _ => {}
        }
    }
    Ok(info)
}

pub fn walk<P: PhysMem>(phys: &P, rsdp_phys: u64) -> Result<AcpiInfo, AcpiError> {
    let mut rsdp_buf = [0u8; RSDP_V2_LEN];
    // Prefer a full v2 read; fall back to v1 if the extra bytes aren't there.
    let rsdp = if phys.read(rsdp_phys, &mut rsdp_buf) {
        parse_rsdp(&rsdp_buf)?
    } else {
        let v1 = rsdp_buf
            .get_mut(..RSDP_V1_LEN)
            .ok_or(AcpiError::Truncated)?;
        read_exact(phys, rsdp_phys, v1)?;
        parse_rsdp(v1)?
    };

    if rsdp.revision >= 2 && rsdp.xsdt_addr != 0 {
        match walk_root(phys, rsdp.xsdt_addr, 8, SIG_XSDT) {
            Ok(info) => return Ok(info),
            Err(AcpiError::BadChecksum) | Err(AcpiError::BadSignature) => {}
            Err(e) => return Err(e),
        }
    }
    if rsdp.rsdt_addr != 0 {
        return walk_root(phys, rsdp.rsdt_addr as u64, 4, SIG_RSDT);
    }
    Err(AcpiError::NoRootTable)
}

// ------------------ host tests ------------------

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "host test fixtures"
)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    struct Mem(Vec<(u64, Vec<u8>)>);

    impl PhysMem for Mem {
        fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
            if buf.is_empty() {
                return true;
            }
            let end = match addr.checked_add(buf.len() as u64) {
                Some(e) => e,
                None => return false,
            };
            for (base, data) in &self.0 {
                let b_end = *base + data.len() as u64;
                if addr >= *base && end <= b_end {
                    let off = (addr - base) as usize;
                    buf.copy_from_slice(&data[off..off + buf.len()]);
                    return true;
                }
            }
            false
        }
    }

    fn set_sum(buf: &mut [u8], off: usize) {
        buf[off] = 0;
        let s = checksum(buf);
        buf[off] = 0u8.wrapping_sub(s);
    }

    fn rsdp_v1(rsdt: u32) -> Vec<u8> {
        let mut b = vec![0u8; RSDP_V1_LEN];
        b[0..8].copy_from_slice(RSDP_SIG);
        b[15] = 0;
        b[16..20].copy_from_slice(&rsdt.to_le_bytes());
        set_sum(&mut b, 8);
        b
    }

    fn rsdp_v2(xsdt: u64, rsdt: u32) -> Vec<u8> {
        let mut b = vec![0u8; RSDP_V2_LEN];
        b[0..8].copy_from_slice(RSDP_SIG);
        b[15] = 2;
        b[16..20].copy_from_slice(&rsdt.to_le_bytes());
        b[20..24].copy_from_slice(&(RSDP_V2_LEN as u32).to_le_bytes());
        b[24..32].copy_from_slice(&xsdt.to_le_bytes());
        set_sum(&mut b[..RSDP_V1_LEN], 8);
        set_sum(&mut b, 32);
        b
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

    fn xsdt(ptrs: &[u64]) -> Vec<u8> {
        let mut extra = Vec::new();
        for p in ptrs {
            extra.extend_from_slice(&p.to_le_bytes());
        }
        sdt(SIG_XSDT, &extra)
    }

    fn rsdt(ptrs: &[u32]) -> Vec<u8> {
        let mut extra = Vec::new();
        for p in ptrs {
            extra.extend_from_slice(&p.to_le_bytes());
        }
        sdt(SIG_RSDT, &extra)
    }

    #[test]
    fn rsdp_v1_checksum_reject() {
        let mut b = rsdp_v1(0x1000);
        b[9] ^= 0xFF; // OEMID, inside the v1 span
        assert_eq!(parse_rsdp(&b), Err(AcpiError::BadChecksum));
    }

    #[test]
    fn rsdp_v2_extended_checksum_reject() {
        let mut b = rsdp_v2(0x2000, 0x1000);
        // Flip a v2-only byte so the 20-byte checksum still passes.
        b[33] ^= 0xFF;
        assert_eq!(
            checksum(&b[..RSDP_V1_LEN]),
            0,
            "v1 checksum must still pass"
        );
        assert_eq!(parse_rsdp(&b), Err(AcpiError::BadChecksum));
    }

    #[test]
    fn rsdp_v2_ok_and_bad_signature() {
        let b = rsdp_v2(0x2000, 0x1000);
        let info = parse_rsdp(&b).unwrap();
        assert_eq!(info.revision, 2);
        assert_eq!(info.xsdt_addr, 0x2000);
        let mut bad = b;
        bad[0] = b'X';
        set_sum(&mut bad[..RSDP_V1_LEN], 8);
        set_sum(&mut bad, 32);
        assert_eq!(parse_rsdp(&bad), Err(AcpiError::BadSignature));
    }

    #[test]
    fn hpet_rejects_zero_and_io_space() {
        let mut extra = vec![0u8; 20];
        // GAS at offset 40 of the full table = offset 4 of extra
        extra[4] = GAS_SYSTEM_MEMORY;
        extra[8..16].copy_from_slice(&0u64.to_le_bytes());
        let zero = sdt(SIG_HPET, &extra);
        assert_eq!(parse_hpet(&zero), Err(AcpiError::HpetZeroAddress));

        extra[4] = GAS_SYSTEM_IO;
        extra[8..16].copy_from_slice(&0xFED0_0000u64.to_le_bytes());
        let io = sdt(SIG_HPET, &extra);
        assert_eq!(parse_hpet(&io), Err(AcpiError::HpetIoSpace));
    }

    #[test]
    fn hpet_accepts_memory_gas_unaligned_tick() {
        let mut extra = vec![0u8; 20];
        extra[4] = GAS_SYSTEM_MEMORY;
        extra[8..16].copy_from_slice(&0xFED0_0000u64.to_le_bytes());
        // minimum_tick at table offset 53 = extra offset 17, deliberately
        // unaligned. 0x1234 would be wrong if we did an aligned u16 load
        // at 52.
        extra[17..19].copy_from_slice(&0x1234u16.to_le_bytes());
        let t = sdt(SIG_HPET, &extra);
        let h = parse_hpet(&t).unwrap();
        assert_eq!(h.base, 0xFED0_0000);
        assert_eq!(h.minimum_tick, 0x1234);
    }

    fn madt_bytes(lapic: u32, flags: u32, records: &[u8]) -> Vec<u8> {
        let mut extra = Vec::new();
        extra.extend_from_slice(&lapic.to_le_bytes());
        extra.extend_from_slice(&flags.to_le_bytes());
        extra.extend_from_slice(records);
        sdt(SIG_MADT, &extra)
    }

    #[test]
    fn madt_type5_override_and_enabled_ids() {
        let mut rec = Vec::new();
        // type 0, len 8, uid 0, apic 1, flags enabled
        rec.extend_from_slice(&[0, 8, 0, 1]);
        rec.extend_from_slice(&1u32.to_le_bytes());
        // type 0 disabled
        rec.extend_from_slice(&[0, 8, 1, 2]);
        rec.extend_from_slice(&0u32.to_le_bytes());
        // type 1 ioapic id 0 addr 0xFEC00000 gsi 0
        rec.extend_from_slice(&[1, 12, 0, 0]);
        rec.extend_from_slice(&0xFEC0_0000u32.to_le_bytes());
        rec.extend_from_slice(&0u32.to_le_bytes());
        // type 2 iso irq 0 -> gsi 2
        rec.extend_from_slice(&[2, 10, 0, 0]);
        rec.extend_from_slice(&2u32.to_le_bytes());
        rec.extend_from_slice(&0u16.to_le_bytes());
        // type 5 override to 0xDEAD_BEEF_0000
        rec.extend_from_slice(&[5, 12, 0, 0]);
        rec.extend_from_slice(&0xDEAD_BEEF_0000u64.to_le_bytes());

        let t = madt_bytes(0xFEE0_0000, 1, &rec);
        let m = parse_madt(&t).unwrap();
        assert_eq!(m.lapic_base, 0xDEAD_BEEF_0000);
        assert!(m.pcat_compat);
        assert_eq!(m.cpu_count, 1);
        assert_eq!(m.apic_ids[0], 1);
        assert_eq!(m.ioapic_count, 1);
        assert_eq!(m.ioapics[0].addr, 0xFEC0_0000);
        assert_eq!(m.iso_count, 1);
        assert_eq!(m.isos[0].irq, 0);
        assert_eq!(m.isos[0].gsi, 2);
    }

    #[test]
    fn madt_truncated_entry_keeps_prior() {
        let mut rec = Vec::new();
        rec.extend_from_slice(&[0, 8, 0, 7]);
        rec.extend_from_slice(&1u32.to_le_bytes());
        // claims len 12 but only 3 bytes follow
        rec.extend_from_slice(&[1, 12, 0]);
        let t = madt_bytes(0xFEE0_0000, 0, &rec);
        let m = parse_madt(&t).unwrap();
        assert_eq!(m.cpu_count, 1);
        assert_eq!(m.apic_ids[0], 7);
        assert_eq!(m.ioapic_count, 0);
    }

    #[test]
    fn madt_zero_length_entry_does_not_spin() {
        let mut rec = Vec::new();
        rec.extend_from_slice(&[0, 8, 0, 3]);
        rec.extend_from_slice(&1u32.to_le_bytes());
        rec.extend_from_slice(&[0xFF, 0]); // type garbage, length 0
        rec.extend_from_slice(&[0, 8, 0, 4]);
        rec.extend_from_slice(&1u32.to_le_bytes());
        let t = madt_bytes(0xFEE0_0000, 0, &rec);
        let m = parse_madt(&t).unwrap();
        assert_eq!(m.cpu_count, 1);
        assert_eq!(m.apic_ids[0], 3);
    }

    #[test]
    fn madt_header_truncated() {
        let t = sdt(SIG_MADT, &[0, 1, 2]); // not even lapic+flags
        assert!(matches!(parse_madt(&t), Err(AcpiError::Truncated)));
    }

    #[test]
    fn fadt_iapc_and_reset() {
        let mut extra = vec![0u8; 268 - SDT_HEADER_LEN];
        // iapc_boot_arch at 109 -> extra offset 73
        extra[73..75].copy_from_slice(&1u16.to_le_bytes());
        // RESET_REG GAS at 116 -> extra 80; address at 120 -> extra 84
        extra[80] = GAS_SYSTEM_IO;
        extra[84..92].copy_from_slice(&0xCFu64.to_le_bytes());
        extra[92] = 0x06; // reset_value at 128
        // SLEEP_CONTROL at 244 -> extra 208
        extra[208] = GAS_SYSTEM_IO;
        extra[212..220].copy_from_slice(&0x404u64.to_le_bytes());
        let t = sdt(SIG_FADT, &extra);
        let f = parse_fadt(&t).unwrap();
        assert!(f.legacy_8259());
        assert_eq!(f.reset.address, 0xCF);
        assert_eq!(f.reset_value, 0x06);
        assert_eq!(f.sleep_control.address, 0x404);
    }

    /// A FADT of `len` bytes with `PM_TMR_BLK` = `blk`, `PM_TMR_LEN` = 4
    /// and `Flags` = `flags` where they fit.
    fn fadt_pm(len: usize, blk: u32, flags: u32) -> Vec<u8> {
        let mut extra = vec![0u8; len - SDT_HEADER_LEN];
        let at = |off: usize| off - SDT_HEADER_LEN;
        extra[at(76)..at(80)].copy_from_slice(&blk.to_le_bytes());
        extra[at(91)] = 4;
        if len >= 116 {
            extra[at(112)..at(116)].copy_from_slice(&flags.to_le_bytes());
        }
        extra
    }

    fn set_x_pm(extra: &mut [u8], space: u8, addr: u64) {
        let at = 208 - SDT_HEADER_LEN;
        extra[at] = space;
        extra[at + 1] = 32;
        extra[at + 4..at + 12].copy_from_slice(&addr.to_le_bytes());
    }

    #[test]
    fn fadt_pm_timer_x_block_preferred() {
        let mut extra = fadt_pm(268, 0x608, 0);
        set_x_pm(&mut extra, GAS_SYSTEM_IO, 0xB008);
        let f = parse_fadt(&sdt(SIG_FADT, &extra)).unwrap();
        assert_eq!(
            f.pm_timer,
            Some(PmTimer {
                port: 0xB008,
                width: 24
            })
        );
    }

    #[test]
    fn fadt_pm_timer_legacy_block_rev1() {
        // QEMU `pc` (PIIX4): a 116-byte revision-1 FADT with no `X_` fields.
        let f = parse_fadt(&sdt(SIG_FADT, &fadt_pm(116, 0x608, 0))).unwrap();
        assert_eq!(
            f.pm_timer,
            Some(PmTimer {
                port: 0x608,
                width: 24
            })
        );
        // A zero `X_PM_TMR_BLK` falls back to the legacy block too.
        let f = parse_fadt(&sdt(SIG_FADT, &fadt_pm(268, 0x608, 0))).unwrap();
        assert_eq!(
            f.pm_timer,
            Some(PmTimer {
                port: 0x608,
                width: 24
            })
        );
    }

    #[test]
    fn fadt_pm_timer_width_ext() {
        let f = parse_fadt(&sdt(SIG_FADT, &fadt_pm(244, 0x608, 1 << 8))).unwrap();
        assert_eq!(
            f.pm_timer,
            Some(PmTimer {
                port: 0x608,
                width: 32
            })
        );
    }

    #[test]
    fn fadt_pm_timer_rejects_bad_blocks() {
        let none = |extra: &[u8]| parse_fadt(&sdt(SIG_FADT, extra)).unwrap().pm_timer;
        // Zero block.
        assert_eq!(none(&fadt_pm(116, 0, 0)), None);
        // Above 0xFFFF, legacy and extended.
        assert_eq!(none(&fadt_pm(116, 0x1_0000, 0)), None);
        let mut x_big = fadt_pm(268, 0x608, 0);
        set_x_pm(&mut x_big, GAS_SYSTEM_IO, 0x1_0000);
        assert_eq!(none(&x_big), None);
        // PM_TMR_LEN != 4.
        let mut short_len = fadt_pm(116, 0x608, 0);
        short_len[91 - SDT_HEADER_LEN] = 2;
        assert_eq!(none(&short_len), None);
        // A memory-space `X_` block with no legacy one.
        let mut mem = fadt_pm(268, 0, 0);
        set_x_pm(&mut mem, GAS_SYSTEM_MEMORY, 0xFED0_0000);
        assert_eq!(none(&mem), None);
        // Truncated before `PM_TMR_LEN`, and a table whose length overstates
        // its bytes.
        assert_eq!(none(&fadt_pm(116, 0x608, 0)[..50]), None);
        let mut lying = sdt(SIG_FADT, &fadt_pm(268, 0x608, 0));
        lying.truncate(80);
        assert_eq!(parse_fadt(&lying).map(|f| f.pm_timer), Ok(None));
    }

    #[test]
    fn mcfg_stores_first_ecam() {
        let mut extra = vec![0u8; 8 + 16];
        extra[8..16].copy_from_slice(&0xE000_0000u64.to_le_bytes());
        extra[16..18].copy_from_slice(&0u16.to_le_bytes());
        extra[18] = 0;
        extra[19] = 0xFF;
        let t = sdt(SIG_MCFG, &extra);
        let m = parse_mcfg(&t).unwrap();
        assert_eq!(m.ecam_base, 0xE000_0000);
        assert_eq!(m.end_bus, 0xFF);
    }

    #[test]
    fn walk_xsdt_counts_valid_and_skips_bad_checksum() {
        let madt = madt_bytes(0xFEE0_0000, 0, &[0, 8, 0, 0, 1, 0, 0, 0]);
        let mut hpet_extra = vec![0u8; 20];
        hpet_extra[4] = GAS_SYSTEM_MEMORY;
        hpet_extra[8..16].copy_from_slice(&0xFED0_0000u64.to_le_bytes());
        let hpet = sdt(SIG_HPET, &hpet_extra);
        let mut garbage = sdt(b"OEMX", &[1, 2, 3, 4]);
        garbage[10] ^= 0xFF; // break checksum, keep length

        let madt_addr = 0x3000u64;
        let hpet_addr = 0x4000u64;
        let junk_addr = 0x5000u64;
        let xsdt_addr = 0x2000u64;
        let root = xsdt(&[madt_addr, hpet_addr, junk_addr]);
        let rsdp = rsdp_v2(xsdt_addr, 0);

        let mem = Mem(vec![
            (0x1000, rsdp),
            (xsdt_addr, root),
            (madt_addr, madt),
            (hpet_addr, hpet),
            (junk_addr, garbage),
        ]);
        let info = walk(&mem, 0x1000).unwrap();
        assert!(info.used_xsdt);
        assert_eq!(info.table_count, 2); // junk skipped
        assert_eq!(info.cpu_count(), 1);
        assert!(info.hpet_present());
        assert_eq!(info.hpet.unwrap().base, 0xFED0_0000);
    }

    #[test]
    fn walk_rsdt_fallback_when_xsdt_zero() {
        let madt = madt_bytes(0xFEE0_0000, 0, &[0, 8, 0, 5, 1, 0, 0, 0]);
        let madt_addr = 0x3000u32;
        let rsdt_addr = 0x2000u32;
        let root = rsdt(&[madt_addr]);
        let rsdp = rsdp_v2(0, rsdt_addr);

        let mem = Mem(vec![
            (0x1000, rsdp),
            (rsdt_addr as u64, root),
            (madt_addr as u64, madt),
        ]);
        let info = walk(&mem, 0x1000).unwrap();
        assert!(!info.used_xsdt);
        assert_eq!(info.cpu_count(), 1);
        assert_eq!(info.madt.unwrap().apic_ids[0], 5);
    }

    #[test]
    fn sdt_checksum_reject() {
        let mut t = sdt(SIG_MADT, &[0; 8]);
        t[10] ^= 0xFF;
        assert_eq!(validate_sdt(&t), Err(AcpiError::BadChecksum));
    }

    #[test]
    fn read_unaligned_at_usize_max_is_none() {
        let b = [0u8; 16];
        assert_eq!(read_unaligned_u8(&b, usize::MAX), None);
        assert_eq!(read_unaligned_u16(&b, usize::MAX), None);
        assert_eq!(read_unaligned_u32(&b, usize::MAX), None);
        assert_eq!(read_unaligned_u64(&b, usize::MAX), None);
        assert_eq!(sig4(&b, usize::MAX), None);
    }

    /// Answers every read: the RSDP and the XSDT header at their addresses,
    /// zeros elsewhere.
    struct AnyMem {
        rsdp_at: u64,
        rsdp: Vec<u8>,
        xsdt_at: u64,
        xsdt: Vec<u8>,
    }

    impl PhysMem for AnyMem {
        fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
            buf.fill(0);
            let src = if addr == self.rsdp_at {
                &self.rsdp
            } else if addr == self.xsdt_at {
                &self.xsdt
            } else {
                return true;
            };
            let n = buf.len().min(src.len());
            buf[..n].copy_from_slice(&src[..n]);
            true
        }
    }

    #[test]
    fn walk_root_table_wrapping_address_space_is_truncated() {
        let xsdt_at = u64::MAX - 10;
        let mut hdr = sdt(SIG_XSDT, &[]);
        hdr[4..8].copy_from_slice(&300u32.to_le_bytes());
        let mem = AnyMem {
            rsdp_at: 0x1000,
            rsdp: rsdp_v2(xsdt_at, 0),
            xsdt_at,
            xsdt: hdr,
        };
        assert_eq!(walk(&mem, 0x1000).unwrap_err(), AcpiError::Truncated);
    }
}
