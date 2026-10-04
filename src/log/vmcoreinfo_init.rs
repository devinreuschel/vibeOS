//! The kernel's VMCOREINFO note (ROADMAP §10.7, C-VMCOREINFO): [`publish`]
//! renders it at boot with the portable `vibeos::log::vmcoreinfo::render`
//! into one page of the image and gives its physical address to QEMU's
//! `vmcoreinfo` device through fw_cfg, so `dump-guest-memory` copies it
//! into every core. `docs/VMCOREINFO.md` documents the note and its keys.

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use vibeos::log::Level;
use vibeos::log::vmcoreinfo::{self, FORMAT_ELF, FW_CFG_FILE, FwCfgVmcoreinfo, Info, NOTE_MAX};
use vibeos::paging::VirtAddr;

use crate::boot::fw_cfg_init::{self, FwCfgError};
use crate::cell::BootCell;
use crate::{log_init, paging_init, per_cpu_init, thread_init};

/// The kernel's page-table levels on x86_64: `paging_init::install` builds a
/// PML4, DESIGN §11.1's 4-level tables, and the kernel never sets
/// `CR4.LA57`.
const PGT_LEVELS: u32 = 4;
/// Longest build id kept: lld's sha1 id is 20 bytes.
const BUILD_ID_MAX: usize = 64;

/// One page, so the note is physically contiguous.
#[repr(C, align(4096))]
pub(crate) struct NotePage {
    bytes: [u8; NOTE_MAX],
}

/// The rendered note, set once by [`publish`] before `smp: done`.
static NOTE: BootCell<NotePage> = BootCell::new();
/// The note's length in [`NOTE`], 0 until [`publish`] sets it.
static LEN: AtomicU64 = AtomicU64::new(0);
/// The note's physical address, 0 until [`publish`] sets it.
static PA: AtomicU64 = AtomicU64::new(0);
/// [`publish`]'s outcome, a [`DeviceState`] once it has run.
static STATE: AtomicU8 = AtomicU8::new(0);

/// What [`publish`] did with the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum DeviceState {
    /// The device holds the note's address.
    Written = 1,
    /// No fw_cfg: no hypervisor bit or no `QEMU` signature.
    NoFwCfg = 2,
    /// fw_cfg lists no `etc/vmcoreinfo`: QEMU runs without the device.
    NoDevice = 3,
    /// The device's `host_format` lacks the ELF note format, or its file is
    /// short.
    Unsupported = 4,
    /// The note's page has no physical address.
    NoPhys = 5,
    /// The fw_cfg DMA write failed.
    Failed = 6,
    /// The note did not fit its page; nothing was published.
    NoNote = 7,
}

impl DeviceState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            DeviceState::Written => "written",
            DeviceState::NoFwCfg => "no fw_cfg",
            DeviceState::NoDevice => "absent",
            DeviceState::Unsupported => "unsupported",
            DeviceState::NoPhys => "no physical address",
            DeviceState::Failed => "write failed",
            DeviceState::NoNote => "no note",
        }
    }

    #[cfg(feature = "kernel_tests")]
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => DeviceState::Written,
            2 => DeviceState::NoFwCfg,
            3 => DeviceState::NoDevice,
            4 => DeviceState::Unsupported,
            5 => DeviceState::NoPhys,
            6 => DeviceState::Failed,
            7 => DeviceState::NoNote,
            _ => return None,
        })
    }
}

unsafe extern "C" {
    static __build_id_start: u8;
    static __build_id_end: u8;
}

/// The kernel ELF's GNU build id, copied out of `linker.ld`'s
/// `.note.gnu.build-id` section.
pub(crate) struct BuildId {
    bytes: [u8; BUILD_ID_MAX],
    len: usize,
}

impl BuildId {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

/// The build id, or an empty one when the section holds no GNU build-id
/// note or a longer id than [`BUILD_ID_MAX`].
pub(crate) fn build_id() -> BuildId {
    let start = &raw const __build_id_start;
    let len = (&raw const __build_id_end)
        .addr()
        .saturating_sub(start.addr());
    // SAFETY: `__build_id_start..__build_id_end` bound `linker.ld`'s
    // `.note.gnu.build-id` output section, initialized bytes inside
    // `__rodata_*` that paging maps read-only for the image's life, and the
    // slice stays local to this call, which returns a copy; established
    // here, through the two `extern` statics above, which name `linker.ld`'s
    // `.note.gnu.build-id` bounds.
    let sec = unsafe { core::slice::from_raw_parts(start, len) };
    let mut id = BuildId {
        bytes: [0; BUILD_ID_MAX],
        len: 0,
    };
    if let Ok(desc) = vmcoreinfo::gnu_build_id(sec)
        && let Some(dst) = id.bytes.get_mut(..desc.len())
    {
        dst.copy_from_slice(desc);
        id.len = desc.len();
    }
    id
}

/// Why [`register`] did not write the device.
struct Refused(DeviceState, Option<FwCfgError>);

/// Give QEMU's `vmcoreinfo` device the note at `pa`, `len` bytes long:
/// read `etc/vmcoreinfo`'s 16 bytes and, when `host_format` has the ELF
/// format, write them back with `guest_format` ELF, `size` and `paddr`
/// through fw_cfg's DMA interface (QEMU's `docs/specs/vmcoreinfo.rst`).
/// The file's bytes are device input (DESIGN §2.10): parsed without a panic.
fn register(pa: u64, len: usize) -> Result<(), Refused> {
    // ROADMAP §25.4: a kernel entered through the crash path builds its
    // note but never runs this step, as Linux's fw_cfg driver skips the
    // device in a kdump kernel, so a core taken after a crash jump still
    // describes the crashed kernel. No crash path exists before §25.4, and
    // `publish` is the only caller.
    if !fw_cfg_init::present() {
        return Err(Refused(DeviceState::NoFwCfg, None));
    }
    let Some(file) = fw_cfg_init::file(FW_CFG_FILE) else {
        return Err(Refused(DeviceState::NoDevice, None));
    };
    let mut raw = [0u8; FwCfgVmcoreinfo::LEN];
    if fw_cfg_init::read(&file, &mut raw) != raw.len() {
        return Err(Refused(DeviceState::Unsupported, None));
    }
    let host = FwCfgVmcoreinfo::from_le_bytes(&raw);
    if !host.host_takes_elf() {
        return Err(Refused(DeviceState::Unsupported, None));
    }
    let size = u32::try_from(len).map_err(|_| Refused(DeviceState::NoNote, None))?;
    let ours = FwCfgVmcoreinfo {
        host_format: host.host_format,
        guest_format: FORMAT_ELF,
        size,
        paddr: pa,
    };
    fw_cfg_init::dma_write(file.select, &ours.to_le_bytes())
        .map_err(|e| Refused(DeviceState::Failed, Some(e)))
}

/// Render the note, keep it in [`NOTE`], and give its physical address to
/// the device. Once, on the BSP, after `per_cpu_init::init_bsp` and
/// `thread_init::init_bootstrap` and before `smp: done`. A failure is a
/// recorded [`DeviceState`] and one log line (DESIGN §2.5).
pub(crate) fn publish() {
    let id = build_id();
    let (tcbs, tcbs_len) = thread_init::table_root();
    let (cpus, cpus_len) = per_cpu_init::table_root();
    let info = Info {
        osrelease: env!("CARGO_PKG_VERSION"),
        build_id: id.as_bytes(),
        page_size: vibeos::paging::PAGE_SIZE_4K,
        pgt_root: paging_init::kernel_cr3(),
        pgt_levels: PGT_LEVELS,
        log: log_init::ring_root(),
        tcbs,
        tcbs_len,
        cpus,
        cpus_len,
    };
    let mut page = NotePage {
        bytes: [0; NOTE_MAX],
    };
    let len = match vmcoreinfo::render(&info, &mut page.bytes) {
        Ok(n) => n,
        Err(e) => {
            // Release: pairs with the Acquire load in `published`.
            STATE.store(DeviceState::NoNote as u8, Ordering::Release);
            crate::klog!(Level::Error, "vmcoreinfo: not rendered: {}", e.as_str());
            return;
        }
    };
    if NOTE.try_get().is_some() {
        crate::klog!(Level::Error, "vmcoreinfo: already published");
        return;
    }
    // SAFETY: boot order (DESIGN §3.3): `publish` runs once on the BSP
    // before `smp: done`, and `NOTE` was unset just above, so this is its
    // one write and nothing reads it concurrently; established here, with
    // the boot order of `boot_rest` in `src/main.rs`, its only caller.
    unsafe { NOTE.set(page) };
    let note = NOTE.get();
    let va = VirtAddr(note.bytes.as_ptr().addr() as u64);
    let state = match paging_init::translate(va) {
        None => Refused(DeviceState::NoPhys, None),
        Some((pa, _, _)) => {
            // Relaxed: the Release store of `STATE` below publishes it; pairs with nothing.
            PA.store(pa.0, Ordering::Relaxed);
            match register(pa.0, len) {
                Ok(()) => Refused(DeviceState::Written, None),
                Err(r) => r,
            }
        }
    };
    // Relaxed: the Release store of `STATE` below publishes it; pairs with nothing.
    LEN.store(len as u64, Ordering::Relaxed);
    // Release: pairs with the Acquire load in `published`.
    STATE.store(state.0 as u8, Ordering::Release);
    // Relaxed: this thread stored it above; pairs with nothing.
    let pa = PA.load(Ordering::Relaxed);
    match state.1 {
        Some(e) => crate::klog!(
            Level::Info,
            "vmcoreinfo: {} bytes at {:#x}, device {} ({})",
            len,
            pa,
            state.0.as_str(),
            e.as_str()
        ),
        None => crate::klog!(
            Level::Info,
            "vmcoreinfo: {} bytes at {:#x}, device {}",
            len,
            pa,
            state.0.as_str()
        ),
    }
}

/// The published note, its physical address, and what the device got;
/// `None` before [`publish`] ran or when it rendered nothing.
#[cfg(feature = "kernel_tests")]
pub(crate) fn published() -> Option<(&'static [u8], u64, DeviceState)> {
    // Acquire: pairs with the Release stores in `publish`.
    let state = DeviceState::from_u8(STATE.load(Ordering::Acquire))?;
    let note = NOTE.try_get()?;
    // Relaxed: the Acquire of `STATE` above orders it; pairs with nothing.
    let len = usize::try_from(LEN.load(Ordering::Relaxed)).ok()?;
    // Relaxed: the Acquire of `STATE` above orders it; pairs with nothing.
    Some((note.bytes.get(..len)?, PA.load(Ordering::Relaxed), state))
}
