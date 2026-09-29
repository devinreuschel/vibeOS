//! Partition children. ROADMAP §7.3.
//!
//! Offset-limited windows on ram0 / vda. Marker
//! `vibeOS: block: <parent>p<N> <n> sectors`.
#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use vibeos::block::{self, BlockError, DeviceState, write_marker};
use vibeos::lock::RANK_DEVICE;
use vibeos::part::{
    self, MAX_PARTS, MBR_EXTENDED, MBR_LINUX, PartKind, Table, gpt_type_name, map_child_lba,
    mbr_type_name, pack_ebr, pack_mbr,
};
#[cfg(feature = "kernel_tests")]
use vibeos::part::{
    GPT_ENTRY_SIZE, GUID_EFI, GUID_LINUX, GptHeaderInfo, entries_crc, pack_gpt_entry,
    pack_gpt_header, pack_protective_mbr,
};

use crate::block_init;
use crate::cache_init::{self, DEV_RAM0, DEV_VDA};
use crate::serial::Serial;
use crate::sync_init::SpinMutex;
use crate::virtio_blk_init;

const RAM0_P1: u32 = 80;
const RAM0_P1_N: u32 = 32;
const RAM0_EXT: u32 = 120;
const RAM0_EXT_N: u32 = 80;
const RAM0_EBR2: u32 = 160;

#[cfg(feature = "kernel_tests")]
const VDA_P1: u64 = 256;
#[cfg(feature = "kernel_tests")]
const VDA_P1_N: u64 = 128;
#[cfg(feature = "kernel_tests")]
const VDA_P2: u64 = 512;

const NAMES_RAM: [&str; 5] = ["ram0p1", "ram0p2", "ram0p3", "ram0p4", "ram0p5"];
const NAMES_VDA: [&str; 4] = ["vdap1", "vdap2", "vdap3", "vdap4"];

#[derive(Clone, Copy)]
struct Slot {
    live: bool,
    name: &'static str,
    parent: u32,
    start: u64,
    nsect: u64,
    bs: u32,
    kind: PartKind,
}

impl Slot {
    const EMPTY: Self = Self {
        live: false,
        name: "",
        parent: 0,
        start: 0,
        nsect: 0,
        bs: 512,
        kind: PartKind::Mbr { sys: 0 },
    };
}

static SLOTS: SpinMutex<[Slot; MAX_PARTS]> =
    SpinMutex::with_rank([Slot::EMPTY; MAX_PARTS], RANK_DEVICE);
static N: AtomicU8 = AtomicU8::new(0);
static LIVE: AtomicBool = AtomicBool::new(false);

fn parent_raw_read(dev: u32, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    match dev {
        DEV_RAM0 => block_init::read(lba, buf),
        DEV_VDA => virtio_blk_init::read(lba, buf),
        _ => Err(BlockError::Inval),
    }
}

fn parent_bs_cap(dev: u32) -> Result<(u32, u64), BlockError> {
    match dev {
        DEV_RAM0 => Ok((
            block_init::logical_block_size(),
            block_init::capacity_sectors(),
        )),
        DEV_VDA => {
            if !virtio_blk_init::live() {
                return Err(BlockError::Failed);
            }
            Ok((
                virtio_blk_init::logical_block_size(),
                virtio_blk_init::capacity_sectors(),
            ))
        }
        _ => Err(BlockError::Inval),
    }
}

fn parse_dev(dev: u32) -> Result<Table, part::PartError> {
    let (bs, cap) = parent_bs_cap(dev).map_err(|_| part::PartError::Invalid)?;
    let mut sec = [0u8; 512];
    if bs as usize > sec.len() {
        return Err(part::PartError::Invalid);
    }
    let mut scratch = alloc::vec![0u8; 128 * 128];
    part::parse(
        cap,
        bs,
        |lba, buf| parent_raw_read(dev, lba, buf),
        &mut sec,
        scratch.as_mut_slice(),
    )
}

fn name_for(dev: u32, i: usize) -> Option<&'static str> {
    match dev {
        DEV_RAM0 => NAMES_RAM.get(i).copied(),
        DEV_VDA => NAMES_VDA.get(i).copied(),
        _ => None,
    }
}

fn register_table(dev: u32, t: &Table) -> usize {
    let Ok((bs, _)) = parent_bs_cap(dev) else {
        return 0;
    };
    let mut added = 0usize;
    let mut i = 0usize;
    while i < t.n {
        let p = t.parts[i];
        let Some(name) = name_for(dev, i) else {
            break;
        };
        {
            let mut g = SLOTS.lock();
            let n = N.load(Ordering::Acquire) as usize;
            if n >= MAX_PARTS {
                break;
            }
            g[n] = Slot {
                live: true,
                name,
                parent: dev,
                start: p.start_lba,
                nsect: p.nsectors,
                bs,
                kind: p.kind,
            };
            N.store((n + 1) as u8, Ordering::Release);
        }
        let _ = write_marker(&mut Serial, name, p.nsectors);
        let _ = writeln!(Serial);
        added += 1;
        i += 1;
    }
    added
}

fn stamp_ram0_mbr() -> Result<(), BlockError> {
    let mut mbr = [0u8; 512];
    pack_mbr(
        &mut mbr,
        &[
            (MBR_LINUX, RAM0_P1, RAM0_P1_N),
            (MBR_EXTENDED, RAM0_EXT, RAM0_EXT_N),
            (0, 0, 0),
            (0, 0, 0),
        ],
    );
    block_init::write(0, &mbr)?;
    let mut e1 = [0u8; 512];
    pack_ebr(&mut e1, MBR_LINUX, 1, 24, RAM0_EBR2 - RAM0_EXT, 32);
    block_init::write(RAM0_EXT as u64, &e1)?;
    let mut e2 = [0u8; 512];
    pack_ebr(&mut e2, MBR_LINUX, 1, 24, 0, 0);
    block_init::write(RAM0_EBR2 as u64, &e2)?;
    Ok(())
}

/// True when LBA 0 to 33 and the last 33 sectors of `vda` all read back as
/// zeros: the only disk the `kernel_tests` build stamps (F003, DESIGN §10.5).
/// A read error is returned, never read as blank.
#[cfg(feature = "kernel_tests")]
fn vda_blank(cap: u64) -> Result<bool, BlockError> {
    let mut sec = [0u8; 512];
    let tail = cap.checked_sub(33).ok_or(BlockError::Inval)?;
    let mut lba = 0u64;
    while lba < cap {
        virtio_blk_init::read(lba, &mut sec)?;
        if sec.iter().any(|&b| b != 0) {
            return Ok(false);
        }
        lba = if lba == 33 && tail > 34 {
            tail
        } else {
            lba + 1
        };
    }
    Ok(true)
}

/// Test builds only: stamps the fixed two-entry GPT the vdap1/vdap2 tests
/// read, and only on an all-zero `vda` (F003).
#[cfg(feature = "kernel_tests")]
fn stamp_vda_gpt() -> Result<(), BlockError> {
    let cap = virtio_blk_init::capacity_sectors();
    let bs = virtio_blk_init::logical_block_size();
    if bs != 512 || cap < 1024 {
        return Err(BlockError::Inval);
    }
    if !vda_blank(cap)? {
        return Err(BlockError::Inval);
    }
    let nent = 128u32;
    let esz = GPT_ENTRY_SIZE;
    let elen = nent as usize * esz as usize;
    let mut entries = alloc::vec![0u8; 128 * 128];
    if entries.len() < elen {
        return Err(BlockError::Inval);
    }
    let last_usable = cap.saturating_sub(34);
    if last_usable <= VDA_P2 {
        return Err(BlockError::Inval);
    }
    let mut uniq1 = [0u8; 16];
    uniq1[0] = 1;
    let mut uniq2 = [0u8; 16];
    uniq2[0] = 2;
    pack_gpt_entry(
        &mut entries[0..],
        &GUID_EFI,
        &uniq1,
        VDA_P1,
        VDA_P1 + VDA_P1_N - 1,
        "EFI",
    );
    pack_gpt_entry(
        &mut entries[esz as usize..],
        &GUID_LINUX,
        &uniq2,
        VDA_P2,
        last_usable,
        "Linux",
    );
    let ecrc = entries_crc(&entries[..elen]);
    let mut disk_guid = [0u8; 16];
    disk_guid[0] = 0x7B;
    disk_guid[15] = 0xC7;
    let mut pmbr = [0u8; 512];
    pack_protective_mbr(&mut pmbr, cap);
    virtio_blk_init::write(0, &pmbr)?;
    let mut ph = [0u8; 512];
    pack_gpt_header(
        &mut ph,
        &GptHeaderInfo {
            my_lba: 1,
            alt_lba: cap - 1,
            first_usable: 34,
            last_usable,
            disk_guid,
            part_lba: 2,
            part_count: nent,
            part_size: esz,
            entries_crc: ecrc,
        },
    );
    virtio_blk_init::write(1, &ph)?;
    let mut s = 0u64;
    while s < 32 {
        let o = (s as usize) * 512;
        virtio_blk_init::write(2 + s, &entries[o..o + 512])?;
        s += 1;
    }
    let back = cap - 33;
    s = 0;
    while s < 32 {
        let o = (s as usize) * 512;
        virtio_blk_init::write(back + s, &entries[o..o + 512])?;
        s += 1;
    }
    let mut bh = [0u8; 512];
    pack_gpt_header(
        &mut bh,
        &GptHeaderInfo {
            my_lba: cap - 1,
            alt_lba: 1,
            first_usable: 34,
            last_usable,
            disk_guid,
            part_lba: back,
            part_count: nent,
            part_size: esz,
            entries_crc: ecrc,
        },
    );
    virtio_blk_init::write(cap - 1, &bh)?;
    virtio_blk_init::flush()
}

fn slot(i: usize) -> Option<Slot> {
    let g = SLOTS.lock();
    if i < MAX_PARTS && g[i].live {
        Some(g[i])
    } else {
        None
    }
}

pub fn count() -> usize {
    N.load(Ordering::Acquire) as usize
}

pub fn info(i: usize) -> Option<(&'static str, u64, u32, PartKind)> {
    let s = slot(i)?;
    Some((s.name, s.nsect, s.bs, s.kind))
}

pub fn read(i: usize, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    let s = slot(i).ok_or(BlockError::Failed)?;
    if s.bs == 0 || !buf.len().is_multiple_of(s.bs as usize) {
        return Err(BlockError::Inval);
    }
    let nsect = (buf.len() / s.bs as usize) as u64;
    let plba = map_child_lba(s.start, s.nsect, lba, nsect)?;
    cache_init::read(s.parent, plba, buf)
}

pub fn write(i: usize, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    let s = slot(i).ok_or(BlockError::Failed)?;
    if s.bs == 0 || !buf.len().is_multiple_of(s.bs as usize) {
        return Err(BlockError::Inval);
    }
    let nsect = (buf.len() / s.bs as usize) as u64;
    let plba = map_child_lba(s.start, s.nsect, lba, nsect)?;
    cache_init::write(s.parent, plba, buf)
}

pub fn flush(i: usize) -> Result<(), BlockError> {
    let s = slot(i).ok_or(BlockError::Failed)?;
    cache_init::flush(s.parent)
}

pub fn find_name(name: &str) -> Option<usize> {
    let n = count();
    let mut i = 0usize;
    while i < n {
        if let Some(s) = slot(i)
            && s.name == name
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

struct P0;
struct P1;
struct P2;
struct P3;
struct P4;
struct P5;
struct P6;
struct P7;

macro_rules! impl_p {
    ($ty:ident, $i:expr) => {
        impl block::BlockDevice for $ty {
            fn name(&self) -> &'static str {
                slot($i).map(|s| s.name).unwrap_or("part")
            }
            fn logical_block_size(&self) -> u32 {
                slot($i).map(|s| s.bs).unwrap_or(512)
            }
            fn capacity_sectors(&self) -> u64 {
                slot($i).map(|s| s.nsect).unwrap_or(0)
            }
            fn state(&self) -> DeviceState {
                DeviceState::Ready
            }
            fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
                read($i, lba, buf)
            }
            fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
                write($i, lba, buf)
            }
            fn flush(&self) -> Result<(), BlockError> {
                flush($i)
            }
            fn discard(&self, lba: u64, nsectors: u64) -> Result<(), BlockError> {
                let s = slot($i).ok_or(BlockError::Failed)?;
                let _ = map_child_lba(s.start, s.nsect, lba, nsectors)?;
                Ok(())
            }
        }
    };
}

impl_p!(P0, 0);
impl_p!(P1, 1);
impl_p!(P2, 2);
impl_p!(P3, 3);
impl_p!(P4, 4);
impl_p!(P5, 5);
impl_p!(P6, 6);
impl_p!(P7, 7);

static DP0: P0 = P0;
static DP1: P1 = P1;
static DP2: P2 = P2;
static DP3: P3 = P3;
static DP4: P4 = P4;
static DP5: P5 = P5;
static DP6: P6 = P6;
static DP7: P7 = P7;

pub fn device(i: usize) -> Option<&'static dyn block::BlockDevice> {
    if i >= count() {
        return None;
    }
    match i {
        0 => Some(&DP0),
        1 => Some(&DP1),
        2 => Some(&DP2),
        3 => Some(&DP3),
        4 => Some(&DP4),
        5 => Some(&DP5),
        6 => Some(&DP6),
        7 => Some(&DP7),
        _ => None,
    }
}

pub fn type_str(k: PartKind) -> &'static str {
    match k {
        PartKind::Mbr { sys } => mbr_type_name(sys),
        PartKind::Gpt { type_guid } => gpt_type_name(&type_guid),
    }
}

pub fn shell_lines(f: &mut impl core::fmt::Write) -> core::fmt::Result {
    let n = count();
    let mut i = 0usize;
    while i < n {
        if let Some(s) = slot(i) {
            writeln!(
                f,
                "vibeOS: blk: {} {} {} sectors ready {}",
                s.name,
                s.bs,
                s.nsect,
                type_str(s.kind)
            )?;
        }
        i += 1;
    }
    Ok(())
}

pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

pub fn init() {
    if block_init::live()
        && stamp_ram0_mbr().is_ok()
        && let Ok(t) = parse_dev(DEV_RAM0)
    {
        let _ = register_table(DEV_RAM0, &t);
    }
    if virtio_blk_init::live() {
        match parse_dev(DEV_VDA) {
            Ok(t) if t.n > 0 => {
                let _ = register_table(DEV_VDA, &t);
            }
            #[cfg(feature = "kernel_tests")]
            _ => {
                if stamp_vda_gpt().is_ok()
                    && let Ok(t) = parse_dev(DEV_VDA)
                {
                    let _ = register_table(DEV_VDA, &t);
                }
            }
            #[cfg(not(feature = "kernel_tests"))]
            _ => {}
        }
    }
    LIVE.store(true, Ordering::Release);
}
