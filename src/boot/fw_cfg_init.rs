//! QEMU's fw_cfg device (ROADMAP §10.2, C-FWCFG): on x86_64 the selector
//! at port 0x510, the data byte at 0x511, and the DMA address at 0x514 and
//! 0x518; on aarch64 the MMIO window QEMU's `docs/specs/fw_cfg.rst`
//! defines (data at 0x0, selector BE16 at 0x8, DMA address BE64 at 0x10).
//! Encodings are in `vibeos::boot`.
//!
//! Invariant I57: nothing here touches a port before CPUID.1:ECX[31]
//! reports a hypervisor, so bare metal never sees a write to 0x510, and
//! one CPU at a time drives the device (boot before `smp: done`, then
//! boot-time callers and the in-guest registry). On aarch64 the same
//! module writes the mapped MMIO window after
//! `arch::aarch64::boot::map_early_console`. The selector is device-global
//! with no lock; a caller that can race adds a ranked lock (DESIGN §2.1).

#[cfg(target_arch = "aarch64")]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::{AtomicU8, Ordering};

#[cfg(target_arch = "aarch64")]
use vibeos::arch::Barriers;
use vibeos::boot::{
    FW_CFG_DIR_ENTRY, FW_CFG_DMA_ERROR, FW_CFG_DMA_SELECT, FW_CFG_DMA_WRITE, FW_CFG_FILE_DIR,
    FW_CFG_ID, FW_CFG_ID_DMA, FW_CFG_QEMU, FW_CFG_SIGNATURE, fw_cfg_dma_access,
    parse_fw_cfg_dir_count, parse_fw_cfg_dir_entry,
};
use vibeos::dma::{self, DmaAlloc, DmaBuffer};

pub use vibeos::boot::FwCfgFile;

use crate::arch::current::Arch;
use crate::dma_init;
#[cfg(target_arch = "x86_64")]
use crate::x86;

#[cfg(target_arch = "x86_64")]
const PORT_SELECTOR: u16 = 0x510;
#[cfg(target_arch = "x86_64")]
const PORT_DATA: u16 = 0x511;
#[cfg(target_arch = "x86_64")]
const PORT_DMA_HI: u16 = 0x514;
#[cfg(target_arch = "x86_64")]
const PORT_DMA_LO: u16 = 0x518;
/// Data byte, selector, and DMA address offsets in the MMIO window.
#[cfg(target_arch = "aarch64")]
const MMIO_DATA: u64 = 0x00;
#[cfg(target_arch = "aarch64")]
const MMIO_SELECTOR: u64 = 0x08;
#[cfg(target_arch = "aarch64")]
const MMIO_DMA: u64 = 0x10;

/// Directory entries walked at most: QEMU's machines hold far fewer.
const DIR_MAX: u32 = 1024;
/// Largest `dma_write` payload.
pub const DMA_MAX: usize = 4096 - DMA_DATA;
/// Offset of the payload in a [`transfer`] buffer, after the descriptor.
pub(super) const DMA_DATA: usize = 16;
/// Control-word polls before a DMA transfer times out. QEMU completes a
/// transfer inside the port write, so the first poll sees it done.
const DMA_POLLS: u32 = 1_000_000;

const UNPROBED: u8 = 0;
const ABSENT: u8 = 1;
const PRESENT: u8 = 2;
const PRESENT_DMA: u8 = 3;

/// Mapped fw_cfg window (HHDM + PA). Zero until `set_mmio_va`.
#[cfg(target_arch = "aarch64")]
static MMIO_VA: AtomicU64 = AtomicU64::new(0);

/// The probe's answer, set once by [`probe`]. The Release store pairs with
/// the Acquire load in [`state`], so a CPU that reads PRESENT also sees the
/// probe's port accesses as done.
static STATE: AtomicU8 = AtomicU8::new(UNPROBED);

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FwCfgError {
    /// No hypervisor bit, or no `QEMU` signature.
    Absent,
    /// The feature word lacks the DMA bit.
    NoDma,
    /// No DMA buffer.
    NoMem,
    /// The payload exceeds [`DMA_MAX`].
    TooLong,
    /// The device set the control word's error bit.
    Device,
    /// The control word did not clear within [`DMA_POLLS`] reads.
    Timeout,
}

/// A missing fw_cfg device or DMA is no device; a device failure or timeout, an I/O error.
impl From<FwCfgError> for vibeos::kerror::KError {
    fn from(e: FwCfgError) -> Self {
        match e {
            FwCfgError::Absent | FwCfgError::NoDma => Self::NoDev,
            FwCfgError::NoMem => Self::NoMem,
            FwCfgError::TooLong => Self::Inval,
            FwCfgError::Device | FwCfgError::Timeout => Self::Io,
        }
    }
}

impl FwCfgError {
    pub fn as_str(self) -> &'static str {
        match self {
            FwCfgError::Absent => "absent",
            FwCfgError::NoDma => "no dma",
            FwCfgError::NoMem => "no memory",
            FwCfgError::TooLong => "too long",
            FwCfgError::Device => "device error",
            FwCfgError::Timeout => "timeout",
        }
    }
}

/// CPUID.1:ECX[31], the hypervisor-present bit.
#[cfg(target_arch = "x86_64")]
pub fn hypervisor() -> bool {
    let (_, _, ecx, _) = x86::cpuid(1, 0);
    ecx & (1 << 31) != 0
}

/// QEMU `virt` is always a hypervisor; fw_cfg is MMIO (ROADMAP §11.5).
#[cfg(target_arch = "aarch64")]
pub fn hypervisor() -> bool {
    true
}

/// Point later accesses at a mapped fw_cfg window. Early console map,
/// then paging takeover.
#[cfg(target_arch = "aarch64")]
pub fn set_mmio_va(va: u64) {
    // Release: pairs with the Acquire load in `mmio_va`.
    MMIO_VA.store(va, Ordering::Release);
}

#[cfg(target_arch = "aarch64")]
fn mmio_va() -> Option<u64> {
    // Acquire: pairs with the Release store in `set_mmio_va`.
    match MMIO_VA.load(Ordering::Acquire) {
        0 => None,
        va => Some(va),
    }
}

#[cfg(target_arch = "x86_64")]
fn select(key: u16) {
    debug_assert!(hypervisor(), "fw_cfg: port access without a hypervisor");
    // SAFETY: invariant I57, established at `boot::fw_cfg_init::probe`:
    // CPUID reported a hypervisor, so port 0x510 is fw_cfg's 16-bit
    // selector or unclaimed, and one CPU at a time drives it.
    unsafe { x86::outw(PORT_SELECTOR, key) }
}

#[cfg(target_arch = "aarch64")]
fn select(key: u16) {
    let Some(va) = mmio_va() else {
        return;
    };
    let p = va.wrapping_add(MMIO_SELECTOR) as *mut u16;
    // SAFETY: invariant I57, established at `arch::aarch64::boot::map_early_console`
    // and `paging_init::install`: `va` is the Device-mapped fw_cfg window,
    // offset 8 is the 16-bit big-endian selector, and one CPU drives it.
    unsafe { Arch::mmio_write(p, key.to_be()) };
}

#[cfg(target_arch = "x86_64")]
fn read_bytes(out: &mut [u8]) {
    for b in out {
        // SAFETY: invariant I57, established at `boot::fw_cfg_init::probe`:
        // port 0x511 is fw_cfg's data byte after `select`.
        *b = unsafe { x86::inb(PORT_DATA) };
    }
}

#[cfg(target_arch = "aarch64")]
fn read_bytes(out: &mut [u8]) {
    let Some(va) = mmio_va() else {
        return;
    };
    let p = va.wrapping_add(MMIO_DATA) as *const u8;
    for b in out {
        // SAFETY: invariant I57, established at `arch::aarch64::boot::map_early_console`
        // and `paging_init::install`: offset 0 is fw_cfg's data byte after `select`.
        *b = unsafe { Arch::mmio_read(p) };
    }
}

fn probe() -> u8 {
    if !hypervisor() {
        return ABSENT;
    }
    let mut sig = [0u8; 4];
    select(FW_CFG_SIGNATURE);
    read_bytes(&mut sig);
    if sig != FW_CFG_QEMU {
        return ABSENT;
    }
    let mut id = [0u8; 4];
    select(FW_CFG_ID);
    read_bytes(&mut id);
    if u32::from_le_bytes(id) & FW_CFG_ID_DMA != 0 {
        PRESENT_DMA
    } else {
        PRESENT
    }
}

fn state() -> u8 {
    // Acquire: pairs with the Release store below.
    match STATE.load(Ordering::Acquire) {
        UNPROBED => {
            let s = probe();
            // Release: pairs with the Acquire load above.
            STATE.store(s, Ordering::Release);
            s
        }
        s => s,
    }
}

/// Whether QEMU's fw_cfg is there: the hypervisor bit, then the `QEMU`
/// signature. Probed once; no port is touched without the bit.
pub fn present() -> bool {
    state() != ABSENT
}

/// Whether fw_cfg's DMA interface is there.
pub fn has_dma() -> bool {
    state() == PRESENT_DMA
}

/// Call `f` with each directory entry, at most [`DIR_MAX`], until it
/// returns false. The directory stays selected between calls, so `f` must
/// not touch the device.
pub(super) fn walk(mut f: impl FnMut(FwCfgFile, &[u8]) -> bool) {
    if !present() {
        return;
    }
    let mut count = [0u8; 4];
    select(FW_CFG_FILE_DIR);
    read_bytes(&mut count);
    let n = parse_fw_cfg_dir_count(&count).min(DIR_MAX);
    for _ in 0..n {
        let mut e = [0u8; FW_CFG_DIR_ENTRY];
        read_bytes(&mut e);
        let (file, name) = parse_fw_cfg_dir_entry(&e);
        if !f(file, name) {
            return;
        }
    }
}

/// The file called `name`, if fw_cfg is present and lists it.
pub fn file(name: &str) -> Option<FwCfgFile> {
    let mut found = None;
    walk(|f, n| {
        if n == name.as_bytes() {
            found = Some(f);
        }
        found.is_none()
    });
    found
}

/// Read the first `min(file.size, out.len())` bytes of `file` into `out`
/// by port I/O; that count.
pub fn read(file: &FwCfgFile, out: &mut [u8]) -> usize {
    if !present() {
        return 0;
    }
    let n = out.len().min(file.size as usize);
    select(file.select);
    read_bytes(out.get_mut(..n).unwrap_or(&mut []));
    n
}

/// Run one DMA transfer: select `select`, then `control` (READ, WRITE)
/// over `len` bytes at `buf`'s offset [`DMA_DATA`], the descriptor at its
/// start. `buf` must hold `DMA_DATA + len` bytes.
pub(super) fn transfer(
    control: u32,
    select: u16,
    buf: &DmaBuffer,
    len: u32,
) -> Result<(), FwCfgError> {
    if !has_dma() {
        return Err(if present() {
            FwCfgError::NoDma
        } else {
            FwCfgError::Absent
        });
    }
    let need = (len as u64)
        .checked_add(DMA_DATA as u64)
        .ok_or(FwCfgError::TooLong)?;
    if need > buf.len() {
        return Err(FwCfgError::TooLong);
    }
    let dev = buf.device().as_u64();
    let data = dev
        .checked_add(DMA_DATA as u64)
        .ok_or(FwCfgError::TooLong)?;
    let ctl = control | FW_CFG_DMA_SELECT | (u32::from(select) << 16);
    let desc = fw_cfg_dma_access(ctl, len, data);
    let base = buf.as_ptr();
    // SAFETY: `dma_init::alloc` established it: `buf` owns at least
    // `DMA_DATA + len` bytes (checked above) mapped at `as_ptr`, and no
    // device reaches them before the port write below.
    unsafe { core::ptr::copy_nonoverlapping(desc.as_ptr(), base, desc.len()) };
    dma::dma_wmb::<Arch>();
    // SAFETY: invariant I57, established at `boot::fw_cfg_init::probe`:
    // `has_dma` found the signature and the DMA feature, so 0x514 and
    // 0x518 are the DMA address register, big-endian in two halves; the
    // low write starts the transfer.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        x86::outl(PORT_DMA_HI, ((dev >> 32) as u32).to_be());
        x86::outl(PORT_DMA_LO, (dev as u32).to_be());
    }
    #[cfg(target_arch = "aarch64")]
    {
        let Some(va) = mmio_va() else {
            return Err(FwCfgError::Absent);
        };
        let p = va.wrapping_add(MMIO_DMA) as *mut u64;
        // SAFETY: invariant I57, established at `boot::fw_cfg_init::probe`:
        // `has_dma` found the signature and the DMA feature, so offset
        // 0x10 is the 64-bit big-endian DMA address register; the write
        // starts the transfer. The window is the one `set_mmio_va` published.
        unsafe { Arch::mmio_write(p, dev.to_be()) };
    }
    for _ in 0..DMA_POLLS {
        let mut word = [0u8; 4];
        for (i, b) in word.iter_mut().enumerate() {
            // SAFETY: `dma_init::alloc` established it: the control word
            // is `buf`'s first four bytes, which the device writes, so they
            // are read volatile.
            *b = unsafe { core::ptr::read_volatile(base.add(i)) };
        }
        let c = u32::from_be_bytes(word);
        if c & FW_CFG_DMA_ERROR != 0 {
            return Err(FwCfgError::Device);
        }
        if c == 0 {
            dma::dma_rmb::<Arch>();
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(FwCfgError::Timeout)
}

/// Write `data` to the item at `select` by DMA. A read-only item gives
/// [`FwCfgError::Device`].
pub fn dma_write(select: u16, data: &[u8]) -> Result<(), FwCfgError> {
    if !present() {
        return Err(FwCfgError::Absent);
    }
    if !has_dma() {
        return Err(FwCfgError::NoDma);
    }
    if data.len() > DMA_MAX {
        return Err(FwCfgError::TooLong);
    }
    let buf =
        dma_init::alloc(DmaAlloc::new((DMA_DATA + data.len()) as u64)).ok_or(FwCfgError::NoMem)?;
    // SAFETY: `dma_init::alloc` established it: `buf` owns `DMA_DATA +
    // data.len()` bytes at `as_ptr`, and no device reaches them yet.
    unsafe {
        core::ptr::copy_nonoverlapping(data.as_ptr(), buf.as_ptr().add(DMA_DATA), data.len())
    };
    let r = transfer(FW_CFG_DMA_WRITE, select, &buf, data.len() as u32);
    if r == Err(FwCfgError::Timeout) {
        // The device may still write the buffer: keep its frames out of
        // the buddy for good rather than hand them back.
        core::mem::forget(buf);
    } else {
        dma_init::free(buf);
    }
    r
}
