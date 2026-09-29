//! Partition children. ROADMAP §7.3.
//!
//! Each entry of a disk's partition table becomes a child in the block
//! registry (`blockdev_init`), an offset-limited window on its disk named
//! `<disk>p<N>`, whose registration prints `vibeOS: block: <name> <n>
//! sectors`. The table is read below the cache (`BlockRef::read_dev`),
//! because the stamps below write there.

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::block::blockdev::{Backing, BlockName, BlockRef, PartInfo};
use vibeos::block::{BlockError, MAX_BLOCKDEVS};
use vibeos::kalloc::{AllocError, TryVec};
use vibeos::part::{
    self, MBR_EXTENDED, MBR_LINUX, PartKind, Table, gpt_type_name, mbr_type_name, pack_ebr,
    pack_mbr,
};
#[cfg(feature = "kernel_tests")]
use vibeos::part::{
    GPT_ENTRY_SIZE, GUID_EFI, GUID_LINUX, GptHeaderInfo, entries_crc, pack_gpt_entry,
    pack_gpt_header, pack_protective_mbr,
};

use crate::block::blockdev_init;

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

/// Set once `init` has scanned; `block::ktest` reads it.
pub(super) static LIVE: AtomicBool = AtomicBool::new(false);

/// Parse `parent`'s partition table, read uncached: the stamps write below
/// the cache, so a cached page could predate them.
fn parse_dev(parent: &BlockRef) -> Result<Table, part::PartError> {
    let bs = parent
        .logical_block_size()
        .map_err(|_| part::PartError::Invalid)?;
    let cap = parent
        .capacity_sectors()
        .map_err(|_| part::PartError::Invalid)?;
    let mut sec = [0u8; 512];
    if bs as usize > sec.len() {
        return Err(part::PartError::Invalid);
    }
    let mut scratch = zeroed(128 * 128).map_err(|_| part::PartError::NoMemory)?;
    part::parse(
        cap,
        bs,
        |lba, buf| parent.read_dev(lba, buf),
        &mut sec,
        &mut scratch,
    )
}

/// `n` zero bytes on the heap: the GPT entry array is too big for a 16 KiB
/// kernel stack (DESIGN §4.5), so it is never a stack array.
fn zeroed(n: usize) -> Result<TryVec<u8>, AllocError> {
    static ZERO: [u8; 4096] = [0; 4096];
    let mut v = TryVec::try_with_capacity(n)?;
    while v.len() < n {
        let take = (n - v.len()).min(ZERO.len());
        // Within the reserved capacity: never reallocates.
        v.try_extend_from_slice(&ZERO[..take])?;
    }
    Ok(v)
}

/// Register each entry of `t` as a child of `parent`, in order, named
/// `<parent>p<N>`. The first entry that fails stops the scan with one line
/// naming it (ROADMAP §10.12, F117).
fn register_table(parent: &BlockRef, t: &Table) {
    let mut i = 0usize;
    while let Some(p) = t.get(i) {
        let res = u32::try_from(i)
            .ok()
            .and_then(|n| n.checked_add(1))
            .ok_or(BlockError::Inval)
            .and_then(|n| BlockName::child(parent.name(), n))
            .and_then(|name| {
                blockdev_init::register(
                    name.as_bytes(),
                    Backing::Part {
                        parent: parent.clone(),
                        info: PartInfo {
                            start: p.start_lba,
                            nsect: p.nsectors,
                            kind: p.kind,
                        },
                    },
                )
            });
        if let Err(e) = res {
            crate::klog!(
                vibeos::log::Level::Warn,
                "vibeOS: part: {} entry {} of {} not registered: {}",
                parent.name().as_str(),
                i.saturating_add(1),
                t.n,
                e.as_str()
            );
            return;
        }
        i = i.saturating_add(1);
    }
}

fn stamp_ram0_mbr(ram0: &BlockRef) -> Result<(), BlockError> {
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
    ram0.write_dev(0, &mbr)?;
    let mut e1 = [0u8; 512];
    pack_ebr(&mut e1, MBR_LINUX, 1, 24, RAM0_EBR2 - RAM0_EXT, 32);
    ram0.write_dev(RAM0_EXT as u64, &e1)?;
    let mut e2 = [0u8; 512];
    pack_ebr(&mut e2, MBR_LINUX, 1, 24, 0, 0);
    ram0.write_dev(RAM0_EBR2 as u64, &e2)?;
    Ok(())
}

/// True when LBA 0 to 33 and the last 33 sectors of `vda` all read back as
/// zeros: the only disk the `kernel_tests` build stamps (F003, DESIGN §10.5).
/// A read error is returned, never read as blank.
#[cfg(feature = "kernel_tests")]
fn vda_blank(vda: &BlockRef, cap: u64) -> Result<bool, BlockError> {
    let mut sec = [0u8; 512];
    let tail = cap.checked_sub(33).ok_or(BlockError::Inval)?;
    let mut lba = 0u64;
    while lba < cap {
        vda.read_dev(lba, &mut sec)?;
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
fn stamp_vda_gpt(vda: &BlockRef) -> Result<(), BlockError> {
    let cap = vda.capacity_sectors()?;
    let bs = vda.logical_block_size()?;
    if bs != 512 || cap < 1024 {
        return Err(BlockError::Inval);
    }
    if !vda_blank(vda, cap)? {
        return Err(BlockError::Inval);
    }
    let nent = 128u32;
    let esz = GPT_ENTRY_SIZE;
    let elen = nent as usize * esz as usize;
    let mut entries = zeroed(128 * 128).map_err(|_| BlockError::NoMem)?;
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
    vda.write_dev(0, &pmbr)?;
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
    vda.write_dev(1, &ph)?;
    let mut s = 0u64;
    while s < 32 {
        let o = (s as usize) * 512;
        vda.write_dev(2 + s, &entries[o..o + 512])?;
        s += 1;
    }
    let back = cap - 33;
    s = 0;
    while s < 32 {
        let o = (s as usize) * 512;
        vda.write_dev(back + s, &entries[o..o + 512])?;
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
    vda.write_dev(cap - 1, &bh)?;
    vda.flush_dev()
}

pub fn type_str(k: PartKind) -> &'static str {
    match k {
        PartKind::Mbr { sys } => mbr_type_name(sys),
        PartKind::Gpt { type_guid } => gpt_type_name(&type_guid),
    }
}

pub fn shell_lines(f: &mut impl core::fmt::Write) -> core::fmt::Result {
    let mut all: [Option<BlockRef>; MAX_BLOCKDEVS] = [const { None }; MAX_BLOCKDEVS];
    let n = blockdev_init::snapshot(&mut all);
    for r in all.iter().take(n).flatten() {
        let Some(info) = r.part() else {
            continue;
        };
        writeln!(
            f,
            "vibeOS: blk: {} {} {} sectors ready {}",
            r.name().as_str(),
            r.logical_block_size().unwrap_or(0),
            info.nsect,
            type_str(info.kind)
        )?;
    }
    Ok(())
}

/// Stamp ram0's MBR, and in test builds a blank vda's GPT, then register
/// the partitions of every disk.
pub fn init() {
    if let Some(ram0) = blockdev_init::lookup(b"ram0")
        && let Err(e) = stamp_ram0_mbr(&ram0)
    {
        crate::klog!(
            vibeos::log::Level::Warn,
            "vibeOS: part: ram0 table not written: {}",
            e.as_str()
        );
    }
    #[cfg(feature = "kernel_tests")]
    if let Some(vda) = blockdev_init::lookup(b"vda")
        && !matches!(parse_dev(&vda), Ok(t) if t.n > 0)
    {
        // Only a blank vda is stamped (F003); any other is left as it is.
        let _stamped = stamp_vda_gpt(&vda).is_ok();
    }
    let mut all: [Option<BlockRef>; MAX_BLOCKDEVS] = [const { None }; MAX_BLOCKDEVS];
    let n = blockdev_init::snapshot(&mut all);
    for d in all.iter().take(n).flatten() {
        if d.parent().is_none()
            && let Ok(t) = parse_dev(d)
        {
            register_table(d, &t);
        }
    }
    LIVE.store(true, Ordering::Release);
}
