//! Limine handshake captured once. ROADMAP §0.3 / D3.
//!
//! Request statics live here and nothing else reads them. [`BootInfo`]
//! hands out kernel types; no Limine type leaves this module.

use core::ops::Range;

use limine::memmap::{Entry, MEMMAP_USABLE};
use limine::request::{
    ExecutableAddressRequest, FramebufferRequest, FramebufferResponse, HhdmRequest, MemmapRequest,
    RsdpRequest,
};

use crate::cell::BootCell;
use crate::paging_init::HHDM_BASE;

#[used]
#[unsafe(link_section = ".limine_requests")]
static HHDM: HhdmRequest = HhdmRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static MEMMAP: MemmapRequest = MemmapRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static RSDP: RsdpRequest = RsdpRequest::new();

// Physical base of the loaded image, so the PMM does not hand our own
// code and data back out as RAM.
#[used]
#[unsafe(link_section = ".limine_requests")]
static EXEC_ADDR: ExecutableAddressRequest = ExecutableAddressRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static FRAMEBUFFER: FramebufferRequest = FramebufferRequest::new();

// Kernel image bounds from linker.ld (DESIGN §3.4). Virtual.
unsafe extern "C" {
    static __kernel_vma_start: u8;
    static __kernel_vma_end: u8;
}

/// One framebuffer, physical and HHDM virtual. Pitch is bytes/row.
#[derive(Clone, Copy, Debug)]
pub struct FbInfo {
    pub phys: u64,
    pub virt: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u64,
    pub bpp: u16,
    pub size: u64,
}

/// What the kernel takes from Limine. Write-once.
pub struct BootInfo {
    /// Physical span of the loaded kernel image.
    pub kernel_phys: Range<u64>,
    pub rsdp_phys: u64,
    memmap: &'static [&'static Entry],
    fb: Option<&'static FramebufferResponse>,
}

impl BootInfo {
    /// USABLE memmap ranges, physical.
    pub fn usable(&self) -> impl Iterator<Item = Range<u64>> {
        self.memmap
            .iter()
            .filter(|e| e.type_ == MEMMAP_USABLE)
            .map(|e| e.base..e.base + e.length)
    }

    /// Every framebuffer Limine mapped through the HHDM, in response order.
    pub fn framebuffers(&self) -> impl Iterator<Item = FbInfo> {
        let fbs = self.fb.map_or(&[][..], |r| r.framebuffers());
        fbs.iter().filter_map(|fb| {
            let virt = fb.address() as u64;
            Some(FbInfo {
                phys: virt.checked_sub(HHDM_BASE)?,
                virt,
                width: fb.width as u32,
                height: fb.height as u32,
                pitch: fb.pitch,
                bpp: fb.bpp,
                size: fb.size() as u64,
            })
        })
    }
}

static INFO: BootCell<BootInfo> = BootCell::new();

/// Read every Limine response we need and stash it. First thing in
/// `normal_boot_tail`, before PMM / paging / ACPI. A missing required
/// response halts with a serial line. Framebuffers are optional.
pub fn capture() -> &'static BootInfo {
    let hhdm = HHDM
        .response()
        .unwrap_or_else(|| halt_with("vibeOS: limine: hhdm missing"));
    // Everything downstream computes `phys + HHDM_BASE`, buddy free-list
    // nodes included, and those fault the moment our own PML4 goes in if
    // Limine's offset drifted. Fail loud here instead.
    assert!(
        hhdm.offset == HHDM_BASE,
        "paging: limine hhdm offset {:#x} != expected {:#x}; buddy nodes would fault after cr3",
        hhdm.offset,
        HHDM_BASE,
    );
    let memmap = MEMMAP
        .response()
        .unwrap_or_else(|| halt_with("vibeOS: limine: memmap missing"));
    let exec = EXEC_ADDR
        .response()
        .unwrap_or_else(|| halt_with("vibeOS: limine: executable_address missing"));
    let rsdp = RSDP
        .response()
        .unwrap_or_else(|| halt_with("vibeOS: limine: rsdp missing"));
    // Base revision 3 hands back a physical RSDP, other revisions an HHDM VA.
    let rsdp_raw = rsdp.address as u64;
    let kernel_len = (&raw const __kernel_vma_end as u64) - (&raw const __kernel_vma_start as u64);
    unsafe {
        INFO.set(BootInfo {
            kernel_phys: exec.physical_base..exec.physical_base + kernel_len,
            rsdp_phys: rsdp_raw.checked_sub(HHDM_BASE).unwrap_or(rsdp_raw),
            memmap: memmap.entries(),
            fb: FRAMEBUFFER.response(),
        })
    };
    INFO.get()
}

/// Captured snapshot. Panics if [`capture`] has not run.
pub fn info() -> &'static BootInfo {
    INFO.get()
}

fn halt_with(msg: &str) -> ! {
    crate::marker!(msg);
    crate::x86::halt();
}
