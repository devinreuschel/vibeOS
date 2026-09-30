//! Limine handshake captured once. ROADMAP §0.3 / D3.
//!
//! Request statics live here and nothing else reads them. [`BootInfo`]
//! hands out kernel types; no Limine type leaves this module.

use core::ops::Range;

use limine::memmap::{
    Entry, MEMMAP_ACPI_NVS, MEMMAP_ACPI_RECLAIMABLE, MEMMAP_BOOTLOADER_RECLAIMABLE,
    MEMMAP_EXECUTABLE_AND_MODULES, MEMMAP_USABLE,
};
use limine::request::{
    ExecutableAddressRequest, ExecutableCmdlineRequest, FramebufferRequest, FramebufferResponse,
    HhdmRequest, MemmapRequest, ModulesRequest, RsdpRequest, StackSizeRequest,
};

use crate::cell::BootCell;
use vibeos::boot::cmdline::{self, CMDLINE_MAX, Cmdline, CmdlineBuf, Escaped, SYSCTLS};
use vibeos::limits::MAX_BOOT_MODULES;
use vibeos::log::Level;
use vibeos::paging::HHDM_BASE;

pub mod fw_cfg_init;

#[cfg(feature = "kernel_tests")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::let_underscore_must_use,
    clippy::unused_result_ok,
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "kernel_tests-only in-guest tests: a failure ends a test, not the kernel"
)]
pub mod ktest;

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

// The files `limine.conf`'s `module_path:` keys load: the initrd on
// x86_64 (DESIGN §3.3).
#[used]
#[unsafe(link_section = ".limine_requests")]
static MODULES: ModulesRequest = ModulesRequest::new();

// The `limine.conf` entry's `cmdline:` (BOOT.md §3.2).
#[used]
#[unsafe(link_section = ".limine_requests")]
static CMDLINE_REQ: ExecutableCmdlineRequest = ExecutableCmdlineRequest::new();

// 256 KiB for the steps before `thread_init::init_bootstrap` moves boot
// onto its guarded KVA stack (MEMORY.md §4.5); Limine guarantees only
// 64 KiB without it.
#[used]
#[unsafe(link_section = ".limine_requests")]
static STACK_SIZE: StackSizeRequest = StackSizeRequest::new(256 * 1024);

/// The fw_cfg file whose text follows Limine's command line.
pub const FW_CFG_CMDLINE: &str = "opt/vibeos/cmdline";

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
    /// The AP trampoline page (DESIGN §7.3): the lowest usable 4 KiB page
    /// above frame 0 and below 1 MiB, or `None` when the map has none.
    pub trampoline_page: Option<u64>,
    pub rsdp_phys: u64,
    memmap: &'static [&'static Entry],
    fb: Option<&'static FramebufferResponse>,
    /// Physical `(base, end)` of each module, in response order; the first
    /// `nmod` are filled. Taken once in [`capture`], before anything writes
    /// module memory, so no slice over it outlives `capture`.
    modules: [(u64, u64); MAX_BOOT_MODULES],
    nmod: usize,
    /// Limine's command line, then one space and fw_cfg's.
    cmdline: CmdlineBuf,
}

impl BootInfo {
    /// USABLE memmap ranges, physical.
    pub fn usable(&self) -> impl Iterator<Item = Range<u64>> {
        self.memmap
            .iter()
            .filter(|e| e.type_ == MEMMAP_USABLE)
            .map(|e| e.base..e.base + e.length)
    }

    /// The RAM-typed memmap ranges, physical: usable, bootloader
    /// reclaimable, executable and modules, ACPI reclaimable and ACPI NVS.
    /// No device range may overlap one (DESIGN §12.3 rule 8), so
    /// `dev::Registry::claim` and `pci_init::map_mmio` check against them.
    /// Ends saturate: the map is firmware input (AGENTS.md rule 4).
    pub fn ram_ranges(&self) -> impl Iterator<Item = Range<u64>> {
        self.memmap
            .iter()
            .filter(|e| {
                matches!(
                    e.type_,
                    MEMMAP_USABLE
                        | MEMMAP_BOOTLOADER_RECLAIMABLE
                        | MEMMAP_EXECUTABLE_AND_MODULES
                        | MEMMAP_ACPI_RECLAIMABLE
                        | MEMMAP_ACPI_NVS
                )
            })
            .map(|e| e.base..e.base.saturating_add(e.length))
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

    /// The kernel command line as captured, at most `CMDLINE_MAX` bytes.
    pub fn cmdline_raw(&self) -> &[u8] {
        self.cmdline.as_bytes()
    }

    /// Length of Limine's part, a prefix of [`cmdline_raw`](Self::cmdline_raw).
    #[cfg_attr(
        not(feature = "kernel_tests"),
        expect(dead_code, reason = "the in-guest `cmdline_captured` reads it")
    )]
    pub fn cmdline_limine_len(&self) -> usize {
        self.cmdline.limine_len()
    }

    /// Every module Limine loaded, as physical ranges, in response order.
    pub fn modules(&self) -> impl Iterator<Item = Range<u64>> {
        self.modules
            .iter()
            .take(self.nmod)
            .map(|&(base, end)| base..end)
    }

    /// The initrd: the first module, if Limine loaded one.
    pub fn initrd(&self) -> Option<Range<u64>> {
        self.modules().next()
    }
}

/// The physical ranges of the modules in `MODULES`' response, from each
/// file's HHDM address and length; one that is not an HHDM address, or
/// past [`MAX_BOOT_MODULES`], is skipped. Never `File::path()` or
/// `cmdline()`, which unwrap.
fn module_ranges() -> ([(u64, u64); MAX_BOOT_MODULES], usize) {
    let mut out = [(0u64, 0u64); MAX_BOOT_MODULES];
    let mut n = 0usize;
    let files = MODULES.response().map_or(&[][..], |r| r.modules());
    for f in files {
        let data = f.data();
        let range = (data.as_ptr() as u64)
            .checked_sub(HHDM_BASE)
            .and_then(|base| Some((base, base.checked_add(data.len() as u64)?)));
        let (Some(range), Some(slot)) = (range, out.get_mut(n)) else {
            continue;
        };
        *slot = range;
        n += 1;
    }
    (out, n)
}

static INFO: BootCell<BootInfo> = BootCell::new();
/// `INFO`'s command line, parsed; set right after `INFO`.
static CMDLINE: BootCell<Cmdline<'static>> = BootCell::new();

/// Append Limine's command line to `buf`: its bytes up to the NUL, at most
/// `CMDLINE_MAX`.
fn append_limine_cmdline(buf: &mut CmdlineBuf) {
    let Some(resp) = CMDLINE_REQ.response() else {
        return;
    };
    let data: &limine::request::ExecutableCmdlineRespData = resp;
    // SAFETY: limine 0.6.5's `ExecutableCmdlineRespData` is `#[repr(C)]`
    // with one field, the string's pointer (Cargo.lock pins the crate), so
    // it is read at offset 0 rather than through `cmdline()`, which unwraps
    // non-UTF-8; established here. Limine is trusted (DESIGN §2.10).
    let p =
        unsafe { *(data as *const limine::request::ExecutableCmdlineRespData as *const *const u8) };
    if p.is_null() {
        return;
    }
    let mut n = 0;
    // SAFETY: Limine's protocol, trusted here: `p` is a NUL-terminated
    // string that Limine's HHDM maps while `boot::capture` runs, first in
    // `normal_boot_tail`; the scan stops at the NUL or `CMDLINE_MAX`.
    while n < CMDLINE_MAX && unsafe { p.add(n).read() } != 0 {
        n += 1;
    }
    // SAFETY: as above, established here: the `n` bytes before the NUL are
    // the string's, and the slice does not outlive this call.
    buf.append(unsafe { core::slice::from_raw_parts(p, n) });
}

/// Limine's command line, then fw_cfg's `opt/vibeos/cmdline` when QEMU's
/// fw_cfg lists it.
fn capture_cmdline() -> CmdlineBuf {
    let mut buf = CmdlineBuf::new();
    append_limine_cmdline(&mut buf);
    buf.seal_limine();
    if let Some(f) = fw_cfg_init::file(FW_CFG_CMDLINE) {
        let mut text = [0u8; CMDLINE_MAX + 1];
        let n = fw_cfg_init::read(&f, &mut text);
        buf.append(&text[..n]);
    }
    buf
}

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
    let (modules, nmod) = module_ranges();
    let trampoline_page = vibeos::pmm::choose_trampoline_page(
        memmap
            .entries()
            .iter()
            .filter(|e| e.type_ == MEMMAP_USABLE)
            .map(|e| e.base..e.base.saturating_add(e.length)),
    );
    // SAFETY: invariant I22, established at `cell::BootCell::set`: this is
    // the one write, first thing in `normal_boot_tail` on the BSP, before
    // any reader and long before SMP.
    unsafe {
        INFO.set(BootInfo {
            kernel_phys: exec.physical_base..exec.physical_base + kernel_len,
            trampoline_page,
            rsdp_phys: rsdp_raw.checked_sub(HHDM_BASE).unwrap_or(rsdp_raw),
            memmap: memmap.entries(),
            fb: FRAMEBUFFER.response(),
            modules,
            nmod,
            cmdline: capture_cmdline(),
        })
    };
    let info = INFO.get();
    // SAFETY: invariant I22, established at `cell::BootCell::set`: the one
    // write, on the BSP right after `INFO`'s, before any reader.
    unsafe { CMDLINE.set(cmdline::parse(info.cmdline_raw())) };
    report_cmdline(info);
    info
}

/// The echo marker, then one line per ignored sysctl, dropped word, and
/// truncation.
fn report_cmdline(info: &BootInfo) {
    crate::marker!("vibeOS: boot: cmdline: {}", Escaped(info.cmdline_raw()));
    let c = cmdline();
    for s in c.sysctls().filter(|s| !s.known_in(SYSCTLS)) {
        crate::klog!(
            Level::Warn,
            "vibeOS: boot: cmdline: sysctl.{}: unknown sysctl, ignored",
            Escaped(s.path)
        );
    }
    for name in c.dropped() {
        crate::klog!(
            Level::Warn,
            "vibeOS: boot: cmdline: {}: unknown option, dropped",
            Escaped(name)
        );
    }
    if info.cmdline.truncated() {
        crate::klog!(
            Level::Warn,
            "vibeOS: boot: cmdline: truncated to {} bytes",
            CMDLINE_MAX
        );
    }
}

/// The parsed kernel command line. Panics if [`capture`] has not run.
pub fn cmdline() -> &'static Cmdline<'static> {
    CMDLINE.get()
}

/// Captured snapshot. Panics if [`capture`] has not run.
pub fn info() -> &'static BootInfo {
    INFO.get()
}

/// Print `msg` as a marker line and halt: the boot path's stop for a
/// fatal condition before the panic handler can run (C-LINTS's boot-halt
/// rule).
pub(crate) fn halt_with(msg: &str) -> ! {
    crate::marker!(msg);
    crate::x86::halt();
}
