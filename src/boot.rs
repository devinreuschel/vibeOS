//! Limine handshake captured once. ROADMAP §0.3 / D3.
//!
//! Request statics live here. `capture` is the only reader of responses.
//! Stored write-once in a Q3 [`BootCell`] before PMM / paging / ACPI / FB.

#![cfg_attr(feature = "panic_test", allow(dead_code))]

use limine::memmap::MEMMAP_USABLE;
use limine::request::{
    ExecutableAddressRequest, FramebufferRequest, HhdmRequest, MemmapRequest, RsdpRequest,
};

use crate::cell::BootCell;
use crate::paging_init;

#[used]
#[unsafe(link_section = ".limine_requests")]
static HHDM: HhdmRequest = HhdmRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static MEMMAP: MemmapRequest = MemmapRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static RSDP: RsdpRequest = RsdpRequest::new();

// Executable address: physical + virtual base of the loaded kernel image.
// The PMM subtracts this from the free lists so we do not hand our own
// code and data back out as regular RAM.
#[used]
#[unsafe(link_section = ".limine_requests")]
static EXEC_ADDR: ExecutableAddressRequest = ExecutableAddressRequest::new();

// Framebuffer: same reasoning, plus Limine's memmap already marks the
// framebuffer non-USABLE on most firmwares, but DESIGN §4.2 asks for
// an explicit exclude so a stray USABLE entry from a quirky BIOS cannot
// hand us the scanout region.
#[used]
#[unsafe(link_section = ".limine_requests")]
static FRAMEBUFFER: FramebufferRequest = FramebufferRequest::new();

/// One Limine framebuffer, physical and virtual. Pitch is bytes/row.
#[derive(Clone, Copy, Debug)]
pub struct FbInfo {
    pub phys: u64,
    pub virt: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u16,
    pub size: u64,
}

/// Snapshot of every Limine response the kernel consumes. Write-once.
#[derive(Clone, Copy)]
pub struct BootInfo {
    pub hhdm_offset: u64,
    pub kernel_phys_base: u64,
    /// Limine executable virtual base. Captured with the rest; paging
    /// still maps from linker VMA symbols.
    #[allow(dead_code)]
    pub kernel_virt_base: u64,
    pub memmap: &'static [&'static limine::memmap::Entry],
    pub usable_high_water: u64,
    pub rsdp_phys: u64,
    pub framebuffers: [Option<FbInfo>; 2],
}

impl BootInfo {
    /// Highest `phys + size` across captured framebuffers. Zero if none.
    pub fn framebuffer_phys_end(&self) -> u64 {
        let mut hi = 0u64;
        for fb in self.framebuffers.iter().flatten() {
            let end = fb.phys.wrapping_add(fb.size);
            if end > hi {
                hi = end;
            }
        }
        hi
    }
}

static INFO: BootCell<BootInfo> = BootCell::new();

/// Read every required Limine response, stash the snapshot, return it.
///
/// First thing in `normal_boot_tail`, before PMM / paging / ACPI. Missing
/// HHDM, memmap, executable-address, or RSDP halt with the existing
/// serial lines. Framebuffer is optional.
pub fn capture() -> BootInfo {
    let hhdm = HHDM
        .response()
        .unwrap_or_else(|| halt_with("vibeOS: limine: hhdm missing"));
    // Slice B pins the physmap VA at `paging_init::HHDM_BASE`. If Limine
    // drifts to a different offset, buddy free-list nodes (reached via
    // `phys + hhdm_offset`) fault the moment we install our own PML4.
    // Fail loud here instead of chasing that later.
    paging_init::assert_limine_hhdm(hhdm.offset);
    let memmap = MEMMAP
        .response()
        .unwrap_or_else(|| halt_with("vibeOS: limine: memmap missing"));
    let exec = EXEC_ADDR
        .response()
        .unwrap_or_else(|| halt_with("vibeOS: limine: executable_address missing"));
    let rsdp = RSDP
        .response()
        .unwrap_or_else(|| halt_with("vibeOS: limine: rsdp missing"));
    let rsdp_raw = rsdp.address as u64;
    let rsdp_phys = if rsdp_raw >= paging_init::HHDM_BASE {
        rsdp_raw - paging_init::HHDM_BASE
    } else {
        rsdp_raw
    };

    let entries = memmap.entries();
    let framebuffers = capture_framebuffers(hhdm.offset);
    let info = BootInfo {
        hhdm_offset: hhdm.offset,
        kernel_phys_base: exec.physical_base,
        kernel_virt_base: exec.virtual_base,
        memmap: entries,
        usable_high_water: memmap_high_water(entries),
        rsdp_phys,
        framebuffers,
    };
    unsafe { INFO.set(info) };
    info
}

/// Captured snapshot. Panics if [`capture`] has not run.
pub fn info() -> &'static BootInfo {
    INFO.get()
}

/// Highest end address of any USABLE memmap entry, in physical bytes.
/// Zero when the map has no USABLE entries (unreachable in practice).
fn memmap_high_water(entries: &[&limine::memmap::Entry]) -> u64 {
    let mut hi = 0u64;
    for e in entries {
        if e.type_ == MEMMAP_USABLE {
            let end = e.base + e.length;
            if end > hi {
                hi = end;
            }
        }
    }
    hi
}

fn capture_framebuffers(hhdm_offset: u64) -> [Option<FbInfo>; 2] {
    let mut out = [None; 2];
    let Some(resp) = FRAMEBUFFER.response() else {
        return out;
    };
    let mut i = 0;
    for fb in resp.framebuffers() {
        if i >= out.len() {
            break;
        }
        let virt = fb.address() as u64;
        if virt == 0 {
            continue;
        }
        let phys = virt.wrapping_sub(hhdm_offset);
        out[i] = Some(FbInfo {
            phys,
            virt,
            width: fb.width as u32,
            height: fb.height as u32,
            pitch: fb.pitch as u32,
            bpp: fb.bpp,
            size: fb.size() as u64,
        });
        i += 1;
    }
    out
}

/// Halt with a serial line. Used when a Limine response we depend on is
/// missing; nothing after this point would work without it.
fn halt_with(msg: &str) -> ! {
    crate::marker!(msg);
    crate::x86::halt();
}

/// ktest `bootinfo_consistent`: captured fields match the request statics.
#[cfg(feature = "kernel_tests")]
pub fn check_consistent() -> Result<(), &'static str> {
    let info = INFO.try_get().ok_or("unset")?;
    if info.hhdm_offset != paging_init::HHDM_BASE {
        return Err("hhdm");
    }
    let exec = EXEC_ADDR.response().ok_or("exec missing")?;
    if info.kernel_phys_base != exec.physical_base {
        return Err("phys base");
    }
    if info.kernel_virt_base != exec.virtual_base {
        return Err("virt base");
    }
    if let Some(resp) = FRAMEBUFFER.response()
        && let Some(fb) = resp.framebuffers().first()
    {
        let size = fb.size() as u64;
        if size != 0 {
            match info.framebuffers[0] {
                Some(c) if c.size != 0 => {}
                _ => return Err("fb size"),
            }
        }
    }
    Ok(())
}
