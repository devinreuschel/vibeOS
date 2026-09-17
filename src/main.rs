//! vibeOS kernel entry.
//!
//! Boot order: serial, Limine, PMM, paging, ACPI parse + MMIO UC, heap,
//! KVA, then GDT/TSS/IST, PIC remap, IDT, BSP per_cpu, ACPI marker, time,
//! LAPIC+IOAPIC, timer prove, scheduler+idle, irq enabled, SMP, meminfo.
//! GDT after KVA because IST stacks are guarded KVA stacks. per_cpu after
//! GDT because `mov gs` zeros the hidden base. Scheduler after time so
//! the tick can preempt. SMP after irq-enabled so APs enter as idle.
//! The `kernel_tests` build runs the in-guest registry after that and
//! exits through isa-debug-exit.

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
#![feature(abi_x86_interrupt)]
// The panic-test build gates the entire non-panic tail behind
// `#[cfg(not(feature = "panic-test"))]`, which leaves the Limine
// requests, paging init, and helpers technically dead. That is
// deliberate — silence the noise so a real warning is not lost.
#![cfg_attr(feature = "panic-test", allow(dead_code, unused_imports))]

extern crate alloc;

mod acpi_init;
mod apic_init;
mod arch;
mod diag;
mod heap_init;
mod ipi_init;
mod kva_init;
mod log_init;
mod ksyms;
mod paging_init;
mod panic;
mod per_cpu_init;
mod pmm_init;
mod sched_init;
mod serial;
mod smp_init;
mod sync_init;
mod thread_init;
mod time_init;
mod x86;

#[cfg(feature = "kernel_tests")]
mod ktest;

use core::fmt::Write;

use limine::request::{
    ExecutableAddressRequest, FramebufferRequest, HhdmRequest, MemmapRequest, RsdpRequest,
};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

use vibeos::marker;

// The linker groups these three into `.limine_requests` (see linker.ld).
// Limine walks between the start and end markers to find our requests.

#[used]
#[unsafe(link_section = ".limine_requests_start")]
static REQ_START: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static BASE_REV: BaseRevision = BaseRevision::with_revision(3);

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
pub(crate) static FRAMEBUFFER: FramebufferRequest = FramebufferRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests_end")]
static REQ_END: RequestsEndMarker = RequestsEndMarker::new();

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // Step 1: serial. Nothing before this is debuggable.
    serial::Serial::init();
    serial::line(marker::SERIAL_ONLINE);

    // Step 2: base revision. DESIGN §3.3 puts this immediately after serial.
    // Missing / older Limine responds by not clearing the request, and
    // `is_supported()` returns false.
    if !BASE_REV.is_supported() {
        serial::line("vibeOS: limine: base revision unsupported");
        x86::halt();
    }
    serial::line(marker::LIMINE_OK);

    // With `--features panic-test`, prove the panic path end to end.
    // Kept before PMM init so the panic path still exercises only the
    // minimum machinery it needs to be diagnostic. Guarding both this
    // branch and the "normal path" tail avoids `unreachable_code`
    // warnings in the panic-test build.
    #[cfg(feature = "panic-test")]
    {
        serial::line("vibeOS: boot: panic-test armed");
        panic!("intentional panic-test trip");
    }

    #[cfg(not(feature = "panic-test"))]
    {
        normal_boot_tail();
        x86::halt();
    }
}

/// The non-panic-test tail of `_start`. Kept as a fn so a `#[cfg]` on
/// the call site silences `unreachable_code` in panic-test builds
/// without duplicating markers.
#[cfg(not(feature = "panic-test"))]
fn normal_boot_tail() {
    // ---- Phase 1 slice A: physical memory manager. ----
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

    let stats = unsafe { pmm_init::init(memmap.entries(), hhdm.offset, exec.physical_base) };

    // Exit-gate marker for phase 1 slice A. DESIGN §2.6 marker shape.
    let _ = writeln!(
        serial::Serial,
        "vibeOS: pmm: {} free 4KiB frames",
        stats.free_frames
    );

    // Diagnostic follow-up: totals and largest available order. Not part
    // of the exit-gate contract, but useful when the free count is
    // surprising.
    let largest = match stats.largest_free_order {
        Some(o) => o as i32,
        None => -1,
    };
    let _ = writeln!(
        serial::Serial,
        "vibeOS: pmm: {} total, largest order {}",
        stats.total_frames,
        largest
    );

    // ---- Phase 1 slice B: page tables + MMIO attributes. ----
    // Feed the physmap extent computation from what we already have:
    // usable-RAM high water from the memmap, plus each framebuffer's
    // `base + size` so scanout lands inside the physmap. DESIGN §4.1
    // caps at 8 GiB regardless.
    let ram_high_water = memmap_high_water(memmap.entries());
    let fb_phys_end = framebuffer_phys_end(hhdm.offset);
    let paging_report =
        unsafe { paging_init::install(exec.physical_base, ram_high_water, fb_phys_end) };
    paging_init::report(&paging_report);

    // ---- Phase 2 slice B: ACPI discovery + MMIO UC. ----
    // Parse before heap so LAPIC/IOAPIC/HPET PTEs are uncacheable
    // before anything touches those bases (DESIGN §3.3 step 8, §4.3).
    // The `acpi: xsdt N tables` marker waits until after GDT/PIC/IDT
    // (steps 3–5 live after KVA; step 12 relative to them).
    let rsdp = RSDP
        .response()
        .unwrap_or_else(|| halt_with("vibeOS: limine: rsdp missing"));
    let rsdp_raw = rsdp.address as u64;
    let rsdp_phys = if rsdp_raw >= paging_init::HHDM_BASE {
        rsdp_raw - paging_init::HHDM_BASE
    } else {
        rsdp_raw
    };
    unsafe { acpi_init::init(rsdp_phys) };

    // ---- Phase 1 slice C: heap, KVA, diagnostics. ----
    unsafe { heap_init::init() };
    {
        let probe = alloc::boxed::Box::new(0xC0FFEEu64);
        if *probe != 0xC0FFEE {
            panic!("heap probe mismatch");
        }
    }
    serial::line(marker::HEAP_OK);

    unsafe { kva_init::init() };
    {
        let stack = kva_init::alloc_guarded_stack(4).expect("kva stack probe");
        unsafe { (stack.mapped_base().as_u64() as *mut u64).write_volatile(0x5A5A_5A5A_5A5A_5A5A) };
        kva_init::free_stack(stack);
    }
    serial::line(marker::KVA_READY);

    // ---- Phase 2 slice A: GDT/TSS/IST, PIC, IDT. ----
    // After KVA so IST stacks are guarded KVA stacks. PIC remap before
    // LIDT so firmware 8259 vectors cannot alias CPU exceptions. FADT
    // bit 0 may skip ICW; `pic: remapped` still means the step finished.
    unsafe { arch::gdt::init_bsp() };
    serial::line(marker::GDT_OK);

    unsafe { arch::pic::remap_and_mask() };
    serial::line(marker::PIC_REMAPPED);

    unsafe { arch::idt::init() };
    serial::line(marker::IDT_OK);

    // DESIGN §3.3 step 11. After GDT: `mov gs` already ran. Before
    // IRQ0 so ISRs can `gs:[0]`. Allocate + wrmsr GS bases, then
    // bootstrap current/idle, then the marker. No switch before that.
    unsafe { per_cpu_init::init_bsp() };
    unsafe { thread_init::init_bootstrap() };
    serial::line(marker::PER_CPU_BSP);

    acpi_init::report();

    // ---- Phase 2 slice C: PIT, TSC calibration, timekeeping. ----
    // After IDT so IRQ0 has a gate. The handler does not schedule until
    // `sched_init` sets LIVE. Keyboard stays masked until phase 5.
    unsafe { time_init::init() };
    per_cpu_init::set_tsc_per_ms(time_init::tsc_per_ms());

    // FADT bit 0 may have skipped the boot remap (QEMU clears it). IRQ0
    // still needs the 8259 at 0x20, not 0x08, if we fall back to the PIT.
    unsafe { arch::pic::program() };

    // ---- Phase 4 slice A: LAPIC + I/O APIC + timer (BSP). ----
    // UC already done. Enable LAPIC, program IOAPIC masked, then sti and
    // prove the timer before masking PIC (DESIGN §5.5 double-delivery).
    unsafe { apic_init::init() };
    x86::sti();
    apic_init::prove();
    time_init::busy_wait_ms(20);
    diag::uptime();

    diag::meminfo();

    // DESIGN §3.3 steps 14 then 16. Idle must exist before the timer
    // can preempt. `irq: enabled` is IF-on + scheduler armed; IRQ0 was
    // already live for the calib proof.
    unsafe { sched_init::init() };
    serial::line(marker::SCHED_CPU0);
    serial::line(marker::IRQ_ENABLED);

    // DESIGN §3.3 step 17. After the scheduler: APs enter as idle.
    // IPI vectors are in the shared IDT; install the shootdown hook
    // before the first AP is live.
    crate::ipi_init::init();
    unsafe { smp_init::init() };
    diag::cpus();

    serial::line(marker::BOOT_DONE);

    #[cfg(feature = "gp-test")]
    gp_test_trip();

    #[cfg(feature = "kernel_tests")]
    crate::ktest::run();

    #[cfg(not(feature = "kernel_tests"))]
    {
        thread_init::park(None);
        x86::halt();
    }
}

/// Highest end address of any USABLE memmap entry, in physical bytes.
/// Zero when the map has no USABLE entries (unreachable in practice).
#[cfg(not(feature = "panic-test"))]
fn memmap_high_water(entries: &[&limine::memmap::Entry]) -> u64 {
    let mut hi = 0u64;
    for e in entries {
        if e.type_ == limine::memmap::MEMMAP_USABLE {
            let end = e.base + e.length;
            if end > hi {
                hi = end;
            }
        }
    }
    hi
}

/// Highest `base + size` across all framebuffers, in physical bytes.
/// Zero when Limine returns no framebuffers.
#[cfg(not(feature = "panic-test"))]
fn framebuffer_phys_end(hhdm_offset: u64) -> u64 {
    let Some(resp) = FRAMEBUFFER.response() else {
        return 0;
    };
    let mut hi = 0u64;
    for fb in resp.framebuffers() {
        let virt = fb.address() as u64;
        if virt == 0 {
            continue;
        }
        let phys = virt.wrapping_sub(hhdm_offset);
        let end = phys + fb.size() as u64;
        if end > hi {
            hi = end;
        }
    }
    hi
}

/// Halt with a serial line. Used when a Limine response we depend on is
/// missing; nothing after this point would work without it.
#[cfg(not(feature = "panic-test"))]
fn halt_with(msg: &str) -> ! {
    serial::line(msg);
    x86::halt();
}

#[cfg(feature = "gp-test")]
fn gp_test_trip() {
    serial::line("vibeOS: boot: gp-test armed");
    // Kernel code selector with RPL=3 into DS: not a data segment, #GP.
    unsafe {
        core::arch::asm!(
            "mov ds, {0:x}",
            in(reg) 0x0Bu16,
            options(nostack, preserves_flags)
        );
    }
}
