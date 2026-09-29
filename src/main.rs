//! vibeOS kernel entry.
//!
//! Boot order: serial, Limine, PMM, paging, ACPI parse + MMIO UC, heap,
//! KVA, then GDT/TSS/IST, PIC remap, IDT, BSP per_cpu, ACPI marker, time,
//! LAPIC+IOAPIC, timer prove, meminfo, scheduler+idle, irq enabled, SMP,
//! console, PCI scan + registry, workqueue + virtio bind, ramdisk, then
//! `/hello`, the builtins, and `/sbin/init` (DESIGN §3.3 row 18). GDT after
//! KVA because IST stacks are guarded KVA stacks. per_cpu after GDT because
//! `mov gs` zeros the hidden base. Scheduler after time so the tick can
//! preempt. SMP after irq-enabled so APs enter as idle. Console after
//! `smp: done`. PCI after console. Workqueue + virtio (rng, blk) register,
//! then bind. Ramdisk after bind. Partition scan + cache next. FAT initrd
//! is VFS root, then `/dev` `/proc` `/tmp` `/sys` (no marker). `/sbin/init`
//! last; a `kernel_shell` build spawns the kernel shell thread instead.
//! The `kernel_tests` build runs the in-guest registry after that and
//! exits through isa-debug-exit. Boot runs on Limine's stack up to the BSP
//! per_cpu step, where `thread_init::init_bootstrap` moves it onto the
//! bootstrap thread's guarded 64 KiB KVA stack for the rest ([`boot_rest`]).

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
// No panicking call on a path untrusted input reaches (AGENTS.md rule 4,
// ROADMAP §10.1). A site a kernel invariant bounds keeps an `#[allow]` naming
// the invariant; a module not yet audited carries an audit-pending allow on
// its `mod` line (C-LINTS).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

extern crate alloc;

// The panic-test build compiles out everything after `limine: ok`
// (`normal_boot_tail`), so the subsystems and root aliases that tail uses
// are dead there; each carries the panic-test allow (Q2), nothing else.
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod acpi;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod arch;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod block;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod boot;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod cell;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod console;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod dev;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod drivers;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod fs;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod irq;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod log;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod mm;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod proc;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod sched;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod shell;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod smp;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod sync;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
mod time;

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
mod ktest;

use acpi::acpi_init;
use arch::x86_64::{apic_init, cpu as x86};
use block::{block_init, cache_init, part_init};
use console::{console_init, fb_init, kbd_init};
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
use dev::{dev_init, dma_init, entropy_init, pci_init, virtio_init};
use drivers::virtio_blk_init;
use fs::{fat_init, file_init, fs_init, vibefs_init};
use irq::{ipi_init, irq_init};
use log::{diag, log_init, panic, serial};
use mm::{heap_init, kva_init, paging_init, pmm_init};
use proc::{addr_space_init, proc_init, syscall_init, user_init};
use sched::{sched_init, thread_init, work_init};
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
use shell::shell_init;
#[cfg_attr(feature = "panic_test", allow(dead_code, unused_imports))]
use smp::{per_cpu_init, smp_init};
use sync::sync_init;
use time::time_init;

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
#[unsafe(link_section = ".limine_requests_end")]
static REQ_END: RequestsEndMarker = RequestsEndMarker::new();

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // Step 1: serial. Nothing before this is debuggable.
    serial::Serial::init();
    log_init::init();
    crate::marker!(marker::SERIAL_ONLINE);

    // Step 2: base revision. DESIGN §3.3 puts this immediately after serial.
    // Missing / older Limine responds by not clearing the request, and
    // `is_supported()` returns false.
    if !BASE_REV.is_supported() {
        crate::marker!("vibeOS: limine: base revision unsupported");
        x86::halt();
    }
    crate::marker!(marker::LIMINE_OK);

    // With `--features panic_test`, prove the panic path end to end.
    // Kept before PMM init so the panic path still exercises only the
    // minimum machinery it needs to be diagnostic. Guarding both this
    // branch and the "normal path" tail avoids `unreachable_code`
    // warnings in the panic-test build.
    #[cfg(feature = "panic_test")]
    {
        crate::marker!("vibeOS: boot: panic-test armed");
        #[allow(
            clippy::panic,
            reason = "the test-only panic_test feature's purpose: prove the panic path end to end"
        )]
        {
            panic!("intentional panic-test trip");
        }
    }

    #[cfg(not(feature = "panic_test"))]
    normal_boot_tail();
}

/// The non-panic-test tail of `_start`, up to `per_cpu: bsp ready`'s first
/// step. Kept as a fn so a `#[cfg]` on the call site silences
/// `unreachable_code` in panic-test builds without duplicating markers. It
/// runs on Limine's stack (256 KiB, the request in `boot`) and ends in
/// `thread_init::init_bootstrap`, which moves boot onto the bootstrap
/// thread's guarded KVA stack and continues in [`boot_rest`].
#[cfg(not(feature = "panic_test"))]
fn normal_boot_tail() -> ! {
    // ---- Phase 1 slice A: physical memory manager. ----
    // Capture Limine once. Nothing else reads the request statics.
    let info = boot::capture();
    // SAFETY: boot order (DESIGN §3.3), single CPU with IF=0 until `sti`
    // below: Limine's HHDM still maps every usable range, as `pmm_init::init` requires; established here.
    let stats = unsafe { pmm_init::init(info) };

    // Exit-gate marker for phase 1 slice A. DESIGN §2.6 marker shape.
    crate::marker!("vibeOS: pmm: {} free 4KiB frames", stats.free_frames);

    // Diagnostic follow-up: totals and largest available order. Not part
    // of the exit-gate contract, but useful when the free count is
    // surprising.
    let largest = match stats.largest_free_order {
        Some(o) => o as i32,
        None => -1,
    };
    crate::marker!(
        "vibeOS: pmm: {} total, largest order {}",
        stats.total_frames,
        largest
    );

    // ---- Phase 1 slice B: page tables + MMIO attributes. ----
    // SAFETY: boot order (DESIGN §3.3), single CPU with IF=0 until `sti`
    // below: the buddy is up (`pmm_init::init` above), as `paging_init::install` requires; established here.
    let paging_report = unsafe { paging_init::install(info) };
    paging_init::report(&paging_report);

    // ---- Phase 2 slice B: ACPI discovery + MMIO UC. ----
    // Parse before heap so LAPIC/IOAPIC/HPET PTEs are uncacheable
    // before anything touches those bases (DESIGN §3.3 step 8, §4.3).
    // The `acpi: xsdt N tables` marker waits until after GDT/PIC/IDT
    // (steps 3–5 live after KVA; step 12 relative to them).
    // SAFETY: boot order (DESIGN §3.3), single CPU with IF=0 until `sti`
    // below: the kernel's page tables are live (`paging_init::install` above), as `acpi_init::init` requires; established here.
    unsafe { acpi_init::init(info.rsdp_phys) };

    // ---- Phase 1 slice C: heap, KVA, diagnostics. ----
    // SAFETY: boot order (DESIGN §3.3), single CPU with IF=0 until `sti`
    // below: the kernel's page tables are live, as `heap_init::init` requires; established here.
    unsafe { heap_init::init() };
    {
        #[allow(
            clippy::disallowed_types,
            reason = "boot step: heap probe in `_start`, before `irq: enabled`"
        )]
        let probe = alloc::boxed::Box::new(0xC0FFEEu64);
        if *probe != 0xC0FFEE {
            #[allow(
                clippy::panic,
                reason = "a read-back mismatch on fresh heap RAM before `irq: enabled` is corruption, which DESIGN §2.5 halts on"
            )]
            {
                panic!("heap probe mismatch");
            }
        }
    }
    crate::marker!(marker::HEAP_OK);

    // SAFETY: boot order (DESIGN §3.3), single CPU with IF=0 until `sti`
    // below: the kernel's page tables and heap are live, as `kva_init::init` requires; established here.
    unsafe { kva_init::init() };
    {
        #[allow(
            clippy::expect_used,
            reason = "invariant: a fresh KVA arena holds one 4-page guarded stack (MEMORY.md §4.5)"
        )]
        let stack = kva_init::alloc_guarded_stack(4).expect("kva stack probe");
        // SAFETY: `stack` is a fresh, mapped, writable 4-page KVA stack that
        // nothing else holds, and its base is 8-byte aligned; established by
        // `kva_init::alloc_guarded_stack`.
        unsafe { (stack.base().as_u64() as *mut u64).write_volatile(0x5A5A_5A5A_5A5A_5A5A) };
        kva_init::free_stack(stack);
    }
    crate::marker!(marker::KVA_READY);

    // ---- Phase 2 slice A: GDT/TSS/IST, PIC, IDT. ----
    // After KVA so IST stacks are guarded KVA stacks. PIC remap before
    // LIDT so firmware 8259 vectors cannot alias CPU exceptions. FADT
    // bit 0 may skip ICW; `pic: remapped` still means the step finished.
    // SAFETY: boot order (DESIGN §3.3), single CPU with IF=0 until `sti`
    // below: KVA is up (`kva_init::init` above), as `gdt::init_bsp` requires; established here.
    unsafe { arch::gdt::init_bsp() };
    crate::marker!(marker::GDT_OK);

    // SAFETY: boot order (DESIGN §3.3), single CPU with IF=0 until `sti`
    // below: the ACPI walk ran and no IDT is loaded yet, as `pic::remap_and_mask` requires; established here.
    unsafe { arch::pic::remap_and_mask() };
    crate::marker!(marker::PIC_REMAPPED);

    // SAFETY: boot order (DESIGN §3.3), single CPU with IF=0 until `sti`
    // below: the GDT is loaded and the PIC remapped and masked, as `idt::init` requires; established here.
    unsafe { arch::idt::init() };
    #[cfg(feature = "kernel_tests")]
    arch::catch::init();
    crate::marker!(marker::IDT_OK);

    // DESIGN §3.3 step 11. After GDT: `mov gs` already ran. Before
    // IRQ0 so ISRs can `gs:[0]`. Allocate + wrmsr GS bases, then
    // bootstrap current/idle, then the marker. No switch before that.
    // SAFETY: boot order (DESIGN §3.3), single CPU with IF=0 until `sti`
    // below: the GDT is loaded and the PIC masks every line, as `per_cpu_init::init_bsp` requires; established here.
    unsafe { per_cpu_init::init_bsp() };
    // SAFETY: boot order (DESIGN §3.3), single CPU with IF=0 until `sti`
    // in `boot_rest`: `GS_BASE` is the BSP's `PerCpu` (`per_cpu_init::init_bsp` above) and KVA is up, first call, as `thread_init::init_bootstrap` requires; established here.
    unsafe { thread_init::init_bootstrap(boot_rest) }
}

/// The rest of boot, from `per_cpu: bsp ready` on, on the bootstrap
/// thread's guarded 64 KiB KVA stack (MEMORY.md §4.5). Entered once, by
/// `thread_init::init_bootstrap`'s switch; it never returns, since nothing
/// is left on the stack below it.
#[cfg(not(feature = "panic_test"))]
extern "C" fn boot_rest() -> ! {
    // SAFETY: boot order (DESIGN §3.3), single CPU with IF=0 until `sti`
    // below: the GDT is loaded and `GS_BASE` is the BSP's `PerCpu`, as `syscall_init::init_bsp` requires; established here.
    unsafe { syscall_init::init_bsp() };
    proc_init::init();
    crate::marker!(marker::PER_CPU_BSP);

    acpi_init::report();

    // ---- Phase 2 slice C: PIT, TSC calibration, timekeeping. ----
    // After IDT so IRQ0 has a gate. The handler does not schedule until
    // `sched_init` sets LIVE. Keyboard stays masked until phase 5.
    // SAFETY: boot order (DESIGN §3.3), single CPU with IF=0 until `sti`
    // below: the IDT is loaded, the PIC remapped and its lines masked, as `time_init::init` requires; established here.
    unsafe { time_init::init() };
    per_cpu_init::set_tsc_per_ms(time_init::tsc_per_ms());

    // FADT bit 0 may have skipped the boot remap (QEMU clears it). IRQ0
    // still needs the 8259 at 0x20, not 0x08, if we fall back to the PIT.
    // SAFETY: boot order (DESIGN §3.3), single CPU with IF=0 until `sti`
    // below: the IDT is loaded, so vector 0x20 is the PIT's, as `pic::program` requires; established here.
    unsafe { arch::pic::program() };

    // ---- Phase 4 slice A: LAPIC + I/O APIC + timer (BSP). ----
    // UC already done. Enable LAPIC, program IOAPIC masked, then sti and
    // prove the timer before masking PIC (DESIGN §5.5 double-delivery).
    // SAFETY: boot order (DESIGN §3.3), single CPU with IF=0 until `sti`
    // below: the IDT is live, the PIC remapped, the LAPIC and I/O APIC pages uncached (`acpi_init::init` above), as `apic_init::init` requires; established here.
    unsafe { apic_init::init() };
    crate::irq_init::init();
    x86::sti();
    apic_init::prove();
    time_init::busy_wait_ms(20);
    diag::uptime();

    diag::meminfo();

    // DESIGN §3.3 steps 14 then 16. Idle must exist before the timer
    // can preempt. `irq: enabled` is IF-on + scheduler armed; IRQ0 was
    // already live for the calib proof.
    // SAFETY: boot order (DESIGN §3.3): once, on the BSP, after
    // `thread_init::init_bootstrap` with the bootstrap thread current and
    // before `irq: enabled`, as `sched_init::init` requires; established
    // here.
    unsafe { sched_init::init() };
    // DESIGN §2.11 rule 6: from here a last put where it may not release
    // defers to this CPU's list, and a worker releases it.
    vibeos::kalloc::set_release_context(sync_init::may_release_here);
    vibeos::kalloc::set_deferral(work_init::defer_release);
    // DESIGN §2.11 rule 3: `OpGate::kill` sleeps on SCHED from here.
    vibeos::sync::set_gate_wait(
        sync::blocking_init::gate_sleep,
        sync::blocking_init::gate_wake,
    );
    crate::marker!(marker::SCHED_CPU0);
    crate::marker!(marker::IRQ_ENABLED);

    // DESIGN §3.3 step 17. After the scheduler: APs enter as idle.
    // IPI vectors are in the shared IDT; install the shootdown hook
    // before the first AP is live.
    crate::ipi_init::init();
    // SAFETY: boot order (DESIGN §3.3): the scheduler is live, the LAPIC
    // ready, and the trampoline page identity-mapped and kept from the PMM
    // (`pmm_init::init`), as `smp_init::init` requires; established here.
    unsafe { smp_init::init() };
    diag::cpus();

    // DESIGN §3.3 live: after smp: done. Handler, 8042, then unmask IRQ1.
    crate::console_init::init();

    // Phase 6 slice A: scan → list → bind. Marker before `shell ready`
    // so lspci is available once the shell thread runs.
    crate::pci_init::init(crate::dev_init::push);
    crate::work_init::init();
    crate::virtio_init::init();
    crate::virtio_blk_init::init();
    crate::dev_init::init();
    crate::entropy_init::init();
    crate::block_init::init();
    crate::cache_init::init();
    crate::part_init::init();
    crate::file_init::init();

    // ROADMAP §10.6: `/hello` runs as a process the kernel spawns and
    // waits for. Diagnostic only, not a `vibeOS:` marker.
    #[cfg(not(feature = "vibefs_crash"))]
    {
        use crate::serial::Serial;
        use core::fmt::Write;
        match crate::proc_init::spawn_elf("/hello", &[], &[], 0, 0) {
            Ok(pid) => {
                let st = crate::proc_init::wait_kernel(pid);
                let code = if vibeos::proc::wifsignaled(st) {
                    128 + vibeos::proc::wtermsig(st)
                } else {
                    vibeos::proc::wexitstatus(st)
                };
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "a write to Serial cannot fail (DESIGN §2.5)"
                )]
                let _ = writeln!(Serial, "user: exit {code}");
            }
            Err(e) => {
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "a write to Serial cannot fail (DESIGN §2.5)"
                )]
                let _ = writeln!(Serial, "user: hello failed: {}", e.as_str());
            }
        }
    }

    #[cfg(feature = "gp_test")]
    gp_test_trip();

    // `shell ready` is last. gp-test trips after ramdisk so a #GP dump
    // still has a clean contract through `block: …`. Every build registers
    // the builtins; only `kernel_shell` starts the REPL. Crash-consistency
    // builds then write a vibefs virtio image until killed.
    crate::shell_init::init();

    #[cfg(all(
        not(feature = "kernel_tests"),
        not(feature = "vibefs_crash"),
        not(feature = "kernel_shell")
    ))]
    crate::proc_init::start_init();

    #[cfg(feature = "vibefs_crash")]
    crate::fs::vibefs_crash::crash_loop();

    #[cfg(feature = "kernel_tests")]
    crate::ktest::run();

    #[cfg(not(any(feature = "kernel_tests", feature = "vibefs_crash")))]
    {
        thread_init::park(None);
        x86::halt();
    }
}

#[cfg(feature = "gp_test")]
fn gp_test_trip() {
    crate::marker!("vibeOS: boot: gp-test armed");
    // Kernel code selector with RPL=3 into DS: not a data segment, #GP.
    // SAFETY: the test-only gp_test build's purpose: the load faults before
    // DS changes, and the #GP handler dumps and halts; established here.
    unsafe {
        core::arch::asm!(
            "mov ds, {0:x}",
            in(reg) 0x0Bu16,
            options(nostack, preserves_flags)
        );
    }
}
