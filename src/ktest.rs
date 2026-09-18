//! In-guest test registry. DESIGN §8.2.
//!
//! Built only with `--features kernel_tests`. After normal init this
//! module runs the registry over the real IDT, prints the serial protocol,
//! and exits QEMU through `isa-debug-exit`.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::arch::global_asm;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::apic::TimerMode;
use vibeos::desc::{IstSlot, KERNEL_CS, TSS_SEL};
use vibeos::dev::{ClaimError, Device, Driver, IdMatch, ProbeError};
use vibeos::heap::HEAP_SIZE;
use vibeos::kva::PAGE_SIZE;
use vibeos::paging::{heap_flags, PageFlags, PhysAddr, VirtAddr};
use vibeos::pci::{self, Bdf, CFG_COMMAND, CFG_VENDOR, CMD_MASTER, CMD_MEM};
use vibeos::thread::{ThreadId, ThreadState};
use vibeos::time::{CalibSource, Instant};
use vibeos::vectors;

use crate::acpi_init;
use crate::apic_init;
use crate::arch;
use crate::dev_init;
use crate::ipi_init;
use crate::kva_init;
use crate::paging_init;
use crate::pci_init;
use crate::per_cpu_init;
use crate::pmm_init;
use crate::sched_init;
use crate::serial::{self, Serial};
use crate::smp_init;
use crate::sync_init::{BlockingMutex, Channel, Condvar, RwLock, Semaphore, SpinMutex};
use crate::thread_init;
use crate::time_init;
use crate::x86;

const ISA_DEBUG_EXIT: u16 = 0xF4;
const EXIT_PASS: u32 = 0x10;
const EXIT_FAIL: u32 = 0x11;

#[derive(Clone, Copy)]
enum Outcome {
    Ok,
    Fail(&'static str),
    Skip(&'static str),
}

type TestFn = fn() -> Outcome;

const TESTS: &[(&str, TestFn)] = &[
    ("map_unmap", test_map_unmap),
    ("nx_enforcement", test_nx_enforcement),
    ("heap_box", test_heap_box),
    ("heap_reuse", test_heap_reuse),
    ("heap_align", test_heap_align),
    ("heap_growth", test_heap_growth),
    ("heap_oom", test_heap_oom),
    ("stack_guard", test_stack_guard),
    ("kva_roundtrip", test_kva_roundtrip),
    ("kva_deferred", test_kva_deferred),
    ("vmap", test_vmap),
    ("mmio_uc_flags", test_mmio_uc_flags),
    ("acpi_discovery", test_acpi_discovery),
    ("gdt_selectors", test_gdt_selectors),
    ("int3_roundtrip", test_int3_roundtrip),
    ("scoped_pf", test_scoped_pf),
    ("gp_catch", test_gp_catch),
    ("df_on_ist", test_df_on_ist),
    ("pit_tick_rate", test_pit_tick_rate),
    ("now_us_monotonic", test_now_us_monotonic),
    ("now_us_under_yields", test_now_us_under_yields),
    ("tsc_calib_source", test_tsc_calib_source),
    ("uptime_sides", test_uptime_sides),
    ("rtc_offset", test_rtc_offset),
    ("lapic_timer_mode", test_lapic_timer_mode),
    ("lapic_timer_rearm", test_lapic_timer_rearm),
    ("ioapic_pit_gsi_masked", test_ioapic_pit_gsi_masked),
    ("per_cpu_bsp", test_per_cpu_bsp),
    ("per_cpu_identity", test_per_cpu_identity),
    ("trampoline_page", test_trampoline_page),
    ("failed_ap_cleanup", test_failed_ap_cleanup),
    ("spawn_sentinel", test_spawn_sentinel),
    ("switch_two_threads", test_switch_two_threads),
    ("irq_guard_nest", test_irq_guard_nest),
    ("spin_mutex", test_spin_mutex),
    ("yield_now_switches", test_yield_now_switches),
    ("sleep_ms_50", test_sleep_ms_50),
    ("preempt_two_threads", test_preempt_two_threads),
    ("idle_runs", test_idle_runs),
    ("reap_returns_frames", test_reap_returns_frames),
    ("reap_many_via_idle", test_reap_many_via_idle),
    ("blocking_mutex_counter", test_blocking_mutex_counter),
    ("rwlock_exclusion", test_rwlock_exclusion),
    ("rwlock_writer_timeout", test_rwlock_writer_timeout),
    ("semaphore_wake", test_semaphore_wake),
    ("condvar_signal", test_condvar_signal),
    ("condvar_wait_releases", test_condvar_wait_releases),
    ("channel_mpsc", test_channel_mpsc),
    ("mutex_deadline", test_mutex_deadline),
    ("sync_try_paths", test_sync_try_paths),
    ("sched_lock_timer_irq", test_sched_lock_timer_irq),
    ("spawn_exit_thousands", test_spawn_exit_thousands),
    ("cross_cpu_spawn", test_cross_cpu_spawn),
    ("reschedule_ipi_wake_ap", test_reschedule_ipi_wake_ap),
    ("call_function_ipi", test_call_function_ipi),
    ("tlb_shootdown_remote", test_tlb_shootdown_remote),
    ("alloc_stress_smp", test_alloc_stress_smp),
    ("log_boot_captured", test_log_boot_captured),
    ("log_runtime_filter", test_log_runtime_filter),
    ("log_emit_roundtrip", test_log_emit_roundtrip),
    ("log_dmesg_no_recapture", test_log_dmesg_no_recapture),
    ("fb_bgrx_roundtrip", test_fb_bgrx_roundtrip),
    ("fb_pitch", test_fb_pitch),
    ("fb_cr_home", test_fb_cr_home),
    ("kbd_gsi_unmasked", test_kbd_gsi_unmasked),
    ("console_mux", test_console_mux),
    ("kbd_ring_drain", test_kbd_ring_drain),
    ("shell_registry", test_shell_registry),
    ("shell_dispatch", test_shell_dispatch),
    ("shell_dmesg_level", test_shell_dmesg_level),
    ("pci_qemu_set", test_pci_qemu_set),
    ("pci_bar_map", test_pci_bar_map),
    ("pci_cfg_rw", test_pci_cfg_rw),
    ("pci_claim_exclusive", test_pci_claim_exclusive),
    ("pci_bind_order", test_pci_bind_order),
    ("lspci_cmd", test_lspci_cmd),
];

pub fn run() -> ! {
    // IRQs off: PIC is masked, but page-table walks should not race a
    // stray spurious line.
    let _cli = x86::InterruptGuard::enter();
    serial::line("vibeOS: ktest: begin");
    let mut failed = false;
    for &(name, f) in TESTS {
        match f() {
            Outcome::Ok => {
                let _ = writeln!(Serial, "vibeOS: ktest: ok {name}");
            }
            Outcome::Fail(why) => {
                // Same shape as skip: reason on the protocol line so
                // check_ktest_output (which raises on that line alone)
                // is enough to diagnose (DESIGN §8.2).
                let _ = writeln!(Serial, "vibeOS: ktest: FAIL {name}: {why}");
                failed = true;
            }
            Outcome::Skip(reason) => {
                let _ = writeln!(Serial, "vibeOS: ktest: skip {name}: {reason}");
            }
        }
    }
    serial::line("vibeOS: ktest: end");
    qemu_exit(if failed { EXIT_FAIL } else { EXIT_PASS });
}

fn qemu_exit(code: u32) -> ! {
    unsafe { x86::outl(ISA_DEBUG_EXIT, code) };
    x86::halt();
}

fn free_frames() -> usize {
    pmm_init::with_buddy(|b| b.stats().free_frames)
}

fn alloc_frame() -> Option<PhysAddr> {
    pmm_init::with_buddy(|b| b.allocate_frame()).map(PhysAddr)
}

fn free_frame(pa: PhysAddr) {
    pmm_init::with_buddy(|b| unsafe { b.deallocate_frame(pa.as_u64()) });
}

struct Fault {
    cr2: u64,
    error: u64,
}

fn catch_fault<F: FnOnce()>(f: F) -> Option<Fault> {
    arch::catch::catch(vectors::PF, f).map(|c| Fault {
        cr2: c.cr2,
        error: c.error,
    })
}

fn catch_alloc_error<F: FnOnce()>(f: F) -> bool {
    arch::catch::catch_alloc(f)
}

unsafe extern "C" {
    fn vibeos_write_u8_1(addr: u64);
    fn vibeos_fault_on_bad_stack(rsp: u64) -> !;
}

// Known-length store (`C6 07 01`, 3 bytes) for the skip-RIP catcher.
// `ud2` on an unmapped RSP forces #UD delivery to fail into #DF on IST1.
global_asm!(
    r#"
    .pushsection .text
    .global vibeos_write_u8_1
    vibeos_write_u8_1:
        mov byte ptr [rdi], 1
        ret
    .global vibeos_fault_on_bad_stack
    vibeos_fault_on_bad_stack:
        mov rsp, rdi
        ud2
    .popsection
    "#
);

const WRITE_U8_1_LEN: u8 = 3;

// ------------------ tests ------------------

fn test_map_unmap() -> Outcome {
    let Some(va) = kva_init::alloc_va(PAGE_SIZE) else {
        return Outcome::Fail("kva alloc");
    };
    let Some(pa) = alloc_frame() else {
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("frame alloc");
    };
    if unsafe { paging_init::map_4k(va, pa, heap_flags()) }.is_err() {
        free_frame(pa);
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("map_4k");
    }
    unsafe { (va.as_u64() as *mut u64).write_volatile(0xAABB_CCDD_EEFF_0011) };
    let got = unsafe { (va.as_u64() as *const u64).read_volatile() };
    if got != 0xAABB_CCDD_EEFF_0011 {
        return Outcome::Fail("readback mismatch");
    }
    let Some((unmapped, _)) = (unsafe { paging_init::unmap_4k(va) }) else {
        return Outcome::Fail("unmap returned none");
    };
    if unmapped != pa {
        return Outcome::Fail("unmap phys mismatch");
    }
    free_frame(pa);
    kva_init::free_va(va, PAGE_SIZE);
    let fault = catch_fault(|| unsafe {
        (va.as_u64() as *mut u8).write_volatile(1);
    });
    match fault {
        Some(_) => Outcome::Ok,
        None => Outcome::Fail("access after unmap did not fault"),
    }
}

fn test_nx_enforcement() -> Outcome {
    let Some(va) = kva_init::alloc_va(PAGE_SIZE) else {
        return Outcome::Fail("kva alloc");
    };
    let Some(pa) = alloc_frame() else {
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("frame alloc");
    };
    if unsafe { paging_init::map_4k(va, pa, heap_flags()) }.is_err() {
        free_frame(pa);
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("map_4k");
    }
    unsafe { (va.as_u64() as *mut u8).write_volatile(0xC3) };
    let f: unsafe extern "C" fn() = unsafe { core::mem::transmute(va.as_u64()) };
    core::hint::black_box(f);
    let fault = catch_fault(|| unsafe { f() });
    let _ = unsafe { paging_init::unmap_4k(va) };
    free_frame(pa);
    kva_init::free_va(va, PAGE_SIZE);
    let Some(fault) = fault else {
        return Outcome::Fail("NX execute did not fault");
    };
    // Error-code bit 4 is instruction-fetch (Intel SDM).
    if fault.error & (1 << 4) == 0 {
        let _ = writeln!(
            Serial,
            "vibeOS: ktest:   nx err={:#x} cr2={:#x}",
            fault.error, fault.cr2
        );
        return Outcome::Fail("PF was not instruction-fetch");
    }
    Outcome::Ok
}

fn test_heap_box() -> Outcome {
    let b = Box::new(0xDEAD_BEEFu64);
    if *b != 0xDEAD_BEEF {
        return Outcome::Fail("box payload");
    }
    drop(b);
    Outcome::Ok
}

fn test_heap_growth() -> Outcome {
    let mut v = Vec::new();
    v.resize(2 * 1024 * 1024, 0xABu8);
    if v.len() != 2 * 1024 * 1024 {
        return Outcome::Fail("vec len");
    }
    if v[0] != 0xAB || v[v.len() - 1] != 0xAB {
        return Outcome::Fail("vec pattern");
    }
    drop(v);
    Outcome::Ok
}

fn test_heap_align() -> Outcome {
    let mut align = 1usize;
    while align <= 4096 {
        let Ok(layout) = Layout::from_size_align(align, align) else {
            return Outcome::Fail("layout");
        };
        let p = unsafe { alloc::alloc::alloc(layout) };
        if p.is_null() {
            return Outcome::Fail("alloc null");
        }
        if p as usize % align != 0 {
            unsafe { alloc::alloc::dealloc(p, layout) };
            return Outcome::Fail("alignment");
        }
        unsafe { p.write(0x5A) };
        unsafe { alloc::alloc::dealloc(p, layout) };
        align *= 2;
    }
    Outcome::Ok
}

fn test_heap_reuse() -> Outcome {
    let Ok(small) = Layout::from_size_align(16, 8) else {
        return Outcome::Fail("layout");
    };
    let Ok(layout) = Layout::from_size_align(64, 8) else {
        return Outcome::Fail("layout");
    };
    // Sandwich: live blocks on both sides so the hole cannot coalesce
    // with a larger neighbour (first-fit would then carve a different VA).
    let pad = unsafe { alloc::alloc::alloc(small) };
    let a = unsafe { alloc::alloc::alloc(layout) };
    let keep = unsafe { alloc::alloc::alloc(layout) };
    if pad.is_null() || a.is_null() || keep.is_null() {
        return Outcome::Fail("setup alloc");
    }
    unsafe { alloc::alloc::dealloc(a, layout) };
    let b = unsafe { alloc::alloc::alloc(layout) };
    if b.is_null() {
        return Outcome::Fail("second alloc");
    }
    // Compare as usize through black_box: LLVM treats GlobalAlloc like
    // malloc and will fold `a == b` after free at opt-level 1.
    let reused = core::hint::black_box(a as usize) == core::hint::black_box(b as usize);
    if !reused {
        let _ = writeln!(
            Serial,
            "vibeOS: ktest:   reuse pad={pad:p} a={a:p} keep={keep:p} b={b:p}"
        );
        unsafe {
            alloc::alloc::dealloc(b, layout);
            alloc::alloc::dealloc(keep, layout);
            alloc::alloc::dealloc(pad, small);
        };
        return Outcome::Fail("did not reuse freed block");
    }
    let c = unsafe { alloc::alloc::realloc(b, layout, 32) };
    let same = core::hint::black_box(c as usize) == core::hint::black_box(b as usize);
    if !c.is_null() {
        unsafe { alloc::alloc::dealloc(c, Layout::from_size_align(32, 8).unwrap()) };
    }
    unsafe { alloc::alloc::dealloc(keep, layout) };
    unsafe { alloc::alloc::dealloc(pad, small) };
    if !same {
        return Outcome::Fail("realloc shrink moved");
    }
    Outcome::Ok
}

fn test_heap_oom() -> Outcome {
    let hit = catch_alloc_error(|| {
        let _v: Vec<u8> = Vec::with_capacity((HEAP_SIZE as usize) + 4096);
    });
    if !hit {
        return Outcome::Fail("error handler not reached");
    }
    let b = Box::new(1u32);
    if *b != 1 {
        return Outcome::Fail("heap unusable after oom");
    }
    Outcome::Ok
}

fn test_stack_guard() -> Outcome {
    let Some(stack) = kva_init::alloc_guarded_stack(4) else {
        return Outcome::Fail("alloc_guarded_stack");
    };
    unsafe { (stack.mapped_base().as_u64() as *mut u64).write_volatile(0x1111_2222) };
    let got = unsafe { (stack.mapped_base().as_u64() as *const u64).read_volatile() };
    if got != 0x1111_2222 {
        kva_init::free_stack(stack);
        return Outcome::Fail("mapped stack not writable");
    }
    let guard = stack.guard.as_u64();
    let fault = catch_fault(|| unsafe {
        (guard as *mut u8).write_volatile(1);
    });
    kva_init::free_stack(stack);
    match fault {
        Some(f) if (f.cr2 & !0xFFF) == (guard & !0xFFF) => Outcome::Ok,
        Some(_) => Outcome::Fail("fault cr2 was not the guard page"),
        None => Outcome::Fail("guard write did not fault"),
    }
}

fn test_kva_roundtrip() -> Outcome {
    let before = free_frames();
    let Some(stack) = kva_init::alloc_guarded_stack(4) else {
        return Outcome::Fail("alloc_guarded_stack");
    };
    let mid = free_frames();
    if mid + 4 != before {
        kva_init::free_stack(stack);
        let _ = writeln!(Serial, "vibeOS: ktest:   before={before} mid={mid}");
        return Outcome::Fail("stack did not take 4 frames");
    }
    kva_init::free_stack(stack);
    let after = free_frames();
    if after != before {
        let _ = writeln!(Serial, "vibeOS: ktest:   before={before} after={after}");
        return Outcome::Fail("free did not restore frame count");
    }
    Outcome::Ok
}

fn test_kva_deferred() -> Outcome {
    let before = free_frames();
    let Some(stack) = kva_init::alloc_guarded_stack(4) else {
        return Outcome::Fail("alloc_guarded_stack");
    };
    let mid = free_frames();
    kva_init::defer_free(stack);
    if free_frames() != mid {
        kva_init::drain_deferred();
        return Outcome::Fail("defer freed too early");
    }
    kva_init::drain_deferred();
    let after = free_frames();
    if after != before {
        let _ = writeln!(Serial, "vibeOS: ktest:   before={before} after={after}");
        return Outcome::Fail("drain did not free");
    }
    Outcome::Ok
}

fn test_vmap() -> Outcome {
    let Some(a) = alloc_frame() else {
        return Outcome::Fail("frame a");
    };
    let Some(b) = alloc_frame() else {
        free_frame(a);
        return Outcome::Fail("frame b");
    };
    let Some(va) = kva_init::vmap(&[a, b]) else {
        free_frame(a);
        free_frame(b);
        return Outcome::Fail("vmap");
    };
    unsafe { (va.as_u64() as *mut u64).write_volatile(0x100) };
    unsafe { ((va.as_u64() + PAGE_SIZE) as *mut u64).write_volatile(0x200) };
    let ga = unsafe { (va.as_u64() as *const u64).read_volatile() };
    let gb = unsafe { ((va.as_u64() + PAGE_SIZE) as *const u64).read_volatile() };
    kva_init::vunmap(va, 2);
    free_frame(a);
    free_frame(b);
    if ga != 0x100 || gb != 0x200 {
        return Outcome::Fail("vmap readback");
    }
    Outcome::Ok
}

fn test_mmio_uc_flags() -> Outcome {
    // LAPIC (0xFEE0_0000) sits above QEMU's 128 MiB map_end, so the
    // generic patch API is still proven on a leaf we know exists:
    // 2 MiB, inside the identity rest / physmap. ACPI's real bases
    // are checked by `acpi_discovery`.
    let phys = PhysAddr(0x0020_0000);
    if unsafe { paging_init::patch_physmap_uc(phys, 4096) }.is_err() {
        return Outcome::Fail("patch_physmap_uc");
    }
    let va = VirtAddr(paging_init::HHDM_BASE + phys.as_u64());
    let Some((_, _, flags)) = paging_init::translate(va) else {
        return Outcome::Fail("translate");
    };
    if !flags.contains(PageFlags::PCD | PageFlags::PWT) {
        return Outcome::Fail("PCD/PWT not set on physmap leaf");
    }
    Outcome::Ok
}

fn leaf_is_uc(phys: u64) -> bool {
    if phys == 0 {
        return false;
    }
    let va = VirtAddr(paging_init::HHDM_BASE.wrapping_add(phys));
    match paging_init::translate(va) {
        Some((_, _, flags)) => flags.contains(PageFlags::PCD | PageFlags::PWT),
        None => false,
    }
}

fn test_acpi_discovery() -> Outcome {
    let Some(info) = acpi_init::info() else {
        return Outcome::Fail("no acpi info");
    };
    if info.table_count == 0 {
        return Outcome::Fail("zero tables");
    }
    if info.cpu_count() == 0 {
        return Outcome::Fail("no enabled cpus");
    }
    if info.ioapic_count() == 0 {
        return Outcome::Fail("no ioapic");
    }
    if !info.hpet_present() {
        return Outcome::Fail("no hpet");
    }
    if !acpi_init::mmio_uc_patched() {
        return Outcome::Fail("mmio uc not patched");
    }
    let Some(madt) = info.madt.as_ref() else {
        return Outcome::Fail("no madt");
    };
    if !leaf_is_uc(madt.lapic_base) {
        return Outcome::Fail("lapic not uc");
    }
    for i in 0..madt.ioapic_count {
        if !leaf_is_uc(madt.ioapics[i].addr as u64) {
            return Outcome::Fail("ioapic not uc");
        }
    }
    let Some(hpet) = info.hpet else {
        return Outcome::Fail("no hpet");
    };
    if !leaf_is_uc(hpet.base) {
        return Outcome::Fail("hpet not uc");
    }
    if hpet.period_fs == 0 {
        return Outcome::Fail("hpet period unread");
    }
    Outcome::Ok
}

fn test_gdt_selectors() -> Outcome {
    if x86::read_cs() != KERNEL_CS {
        return Outcome::Fail("cs not kernel code");
    }
    if x86::read_tr() != TSS_SEL {
        return Outcome::Fail("tr not tss");
    }
    Outcome::Ok
}

fn test_int3_roundtrip() -> Outcome {
    unsafe { core::arch::asm!("int3", options(nomem, nostack)) };
    Outcome::Ok
}

fn test_scoped_pf() -> Outcome {
    let Some(va) = kva_init::alloc_va(PAGE_SIZE) else {
        return Outcome::Fail("kva alloc");
    };
    let caught = arch::catch::catch_skip(vectors::PF, WRITE_U8_1_LEN, || unsafe {
        vibeos_write_u8_1(va.as_u64());
    });
    kva_init::free_va(va, PAGE_SIZE);
    let Some(c) = caught else {
        return Outcome::Fail("store did not fault");
    };
    if c.vector != vectors::PF {
        return Outcome::Fail("wrong vector");
    }
    if (c.cr2 & !0xFFF) != (va.as_u64() & !0xFFF) {
        return Outcome::Fail("cr2 not the unmapped page");
    }
    Outcome::Ok
}

fn test_gp_catch() -> Outcome {
    let caught = arch::catch::catch(vectors::GP, || unsafe {
        core::arch::asm!(
            "mov ds, {0:x}",
            in(reg) 0x0Bu16,
            options(nostack, preserves_flags)
        );
    });
    match caught {
        Some(c)
            if c.vector == vectors::GP && c.frame.cs == KERNEL_CS as u64 && c.frame.rip != 0 =>
        {
            Outcome::Ok
        }
        Some(_) => Outcome::Fail("wrong vector or frame"),
        None => Outcome::Fail("no gp"),
    }
}

fn test_df_on_ist() -> Outcome {
    let Some(stack) = kva_init::alloc_guarded_stack(1) else {
        return Outcome::Fail("guarded stack");
    };
    let poison = stack.guard.as_u64() + 0x800;
    let caught = arch::catch::catch(vectors::DF, || unsafe {
        vibeos_fault_on_bad_stack(poison);
    });
    kva_init::free_stack(stack);
    let Some(c) = caught else {
        return Outcome::Fail("did not reach df handler");
    };
    let (lo, hi) = arch::gdt::ist_span(IstSlot::DoubleFault);
    if c.handler_rsp >= lo && c.handler_rsp < hi {
        Outcome::Ok
    } else {
        let _ = writeln!(
            Serial,
            "vibeOS: ktest:   rsp={:#x} lo={:#x} hi={:#x}",
            c.handler_rsp, lo, hi
        );
        Outcome::Fail("handler rsp not on ist1")
    }
}

struct IrqsOffOnDrop;
impl Drop for IrqsOffOnDrop {
    fn drop(&mut self) {
        x86::cli();
    }
}

fn with_timer<F: FnOnce() -> Outcome>(f: F) -> Outcome {
    let _off = IrqsOffOnDrop;
    x86::sti();
    f()
}

fn test_pit_tick_rate() -> Outcome {
    with_timer(|| {
        let t0 = time_init::uptime_ms();
        time_init::busy_wait_ms(80);
        let t1 = time_init::uptime_ms();
        let dt = t1.saturating_sub(t0);
        if (40..=160).contains(&dt) {
            Outcome::Ok
        } else {
            let _ = writeln!(Serial, "vibeOS: ktest:   ticks {t0} -> {t1} dt={dt}");
            Outcome::Fail("pit not ~1 kHz")
        }
    })
}

fn test_now_us_monotonic() -> Outcome {
    with_timer(|| {
        let mut last = time_init::now_us();
        let mut i = 0u32;
        while i < 10_000 {
            let n = time_init::now_us();
            if n < last {
                let _ = writeln!(Serial, "vibeOS: ktest:   now_us {last} -> {n} at {i}");
                return Outcome::Fail("now_us went backwards");
            }
            last = n;
            i += 1;
        }
        Outcome::Ok
    })
}

fn test_now_us_under_yields() -> Outcome {
    with_timer(|| {
        let mut last = time_init::now_us();
        let mut i = 0u32;
        while i < 10_000 {
            let n = time_init::now_us();
            if n < last {
                let _ = writeln!(Serial, "vibeOS: ktest:   yield now_us {last} -> {n} at {i}");
                return Outcome::Fail("now_us went backwards under yield");
            }
            last = n;
            if i % 200 == 0 {
                x86::hlt_once();
            }
            i += 1;
        }
        Outcome::Ok
    })
}

fn test_tsc_calib_source() -> Outcome {
    let present = acpi_init::info().is_some_and(|i| i.hpet_present());
    match time_init::source() {
        CalibSource::Hpet => {
            if !present {
                return Outcome::Fail("hpet source without table");
            }
            let k = time_init::tsc_per_ms();
            if k < 50_000 || k > 10_000_000 {
                return Outcome::Fail("tsc_per_ms out of range");
            }
            let Some(pit) = time_init::measure_pit_ch2() else {
                return Outcome::Fail("pit ch2 calib failed");
            };
            let lo = k.saturating_mul(75) / 100;
            let hi = k.saturating_mul(125) / 100;
            if (lo..=hi).contains(&pit) {
                Outcome::Ok
            } else {
                let _ = writeln!(Serial, "vibeOS: ktest:   hpet {k}/ms pit {pit}/ms");
                Outcome::Fail("pit ch2 disagreed with hpet")
            }
        }
        CalibSource::Pit => {
            if present {
                return Outcome::Fail("pit source despite hpet table");
            }
            let k = time_init::tsc_per_ms();
            if k < 50_000 || k > 10_000_000 {
                return Outcome::Fail("tsc_per_ms out of range");
            }
            Outcome::Ok
        }
    }
}

fn test_uptime_sides() -> Outcome {
    with_timer(|| {
        time_init::busy_wait_ms(30);
        let tick = time_init::uptime_ms();
        let us = time_init::now_us();
        if tick == 0 {
            return Outcome::Fail("tick still 0");
        }
        let tick_us = tick.saturating_mul(1000);
        let lo = tick_us.saturating_mul(50) / 100;
        let hi = tick_us.saturating_mul(150) / 100 + 2000;
        if us >= lo && us <= hi {
            Outcome::Ok
        } else {
            let _ = writeln!(Serial, "vibeOS: ktest:   tick {tick} ms tsc {us} us");
            Outcome::Fail("tick and tsc sides diverged")
        }
    })
}

fn test_rtc_offset() -> Outcome {
    let Some(a) = time_init::unix_time_s() else {
        return Outcome::Skip("rtc unread");
    };
    with_timer(|| {
        time_init::busy_wait_ms(20);
        let Some(b) = time_init::unix_time_s() else {
            return Outcome::Fail("rtc lost");
        };
        if b < a {
            return Outcome::Fail("wall clock went backwards");
        }
        let _ = time_init::deadline_after(Instant {
            ns: time_init::now_ns(),
        });
        Outcome::Ok
    })
}

fn test_lapic_timer_mode() -> Outcome {
    if !apic_init::is_ready() {
        return Outcome::Fail("lapic not ready");
    }
    if !apic_init::owns_tick() && apic_init::timer_mode() != TimerMode::Pit {
        return Outcome::Fail("lapic mode without owning tick");
    }
    let mode = apic_init::timer_mode();
    let cpuid = apic_init::cpuid_has_tsc_deadline();
    match (cpuid, mode) {
        (true, TimerMode::TscDeadline) => Outcome::Ok,
        (true, TimerMode::Periodic | TimerMode::Pit) => {
            Outcome::Fail("silent downgrade from tsc-deadline")
        }
        (false, TimerMode::Periodic) => Outcome::Ok,
        (false, TimerMode::Pit) => {
            if acpi_init::info().is_some_and(|i| i.hpet_present()) {
                Outcome::Fail("pit despite hpet")
            } else {
                Outcome::Ok
            }
        }
        (false, TimerMode::TscDeadline) => Outcome::Fail("tsc-deadline without cpuid"),
    }
}

fn test_lapic_timer_rearm() -> Outcome {
    with_timer(|| {
        match apic_init::timer_mode() {
            TimerMode::Pit => {
                let t0 = time_init::uptime_ms();
                time_init::busy_wait_ms(50);
                let dt = time_init::uptime_ms().saturating_sub(t0);
                if (20..=100).contains(&dt) {
                    Outcome::Ok
                } else {
                    let _ = writeln!(Serial, "vibeOS: ktest:   pit dt={dt}");
                    Outcome::Fail("pit ticks stalled")
                }
            }
            TimerMode::TscDeadline | TimerMode::Periodic => {
                let t0 = apic_init::timer_fires();
                time_init::busy_wait_ms(50);
                let n = apic_init::timer_fires().saturating_sub(t0);
                if n >= 20 {
                    Outcome::Ok
                } else {
                    let _ = writeln!(Serial, "vibeOS: ktest:   lapic fires {n}");
                    Outcome::Fail("rearm stalled")
                }
            }
        }
    })
}

fn test_ioapic_pit_gsi_masked() -> Outcome {
    match apic_init::timer_mode() {
        TimerMode::Pit => Outcome::Skip("pit owns tick"),
        TimerMode::TscDeadline | TimerMode::Periodic => {
            let Some(info) = acpi_init::info() else {
                return Outcome::Fail("no acpi");
            };
            let Some(madt) = info.madt.as_ref() else {
                return Outcome::Fail("no madt");
            };
            let gsi = vibeos::apic::gsi_for_isa_irq(0, &madt.isos[..madt.iso_count]);
            match apic_init::gsi_masked(gsi) {
                Some(true) => Outcome::Ok,
                Some(false) => Outcome::Fail("pit gsi unmasked"),
                None => Outcome::Fail("pit gsi not on ioapic"),
            }
        }
    }
}

fn test_per_cpu_bsp() -> Outcome {
    if !per_cpu_init::is_live() {
        return Outcome::Fail("per_cpu not live");
    }
    let cpu = per_cpu_init::current();
    if cpu.cpu_id != 0 {
        return Outcome::Fail("cpu_id not 0");
    }
    let addr = cpu as *const _ as u64;
    if cpu.self_ptr as u64 != addr {
        return Outcome::Fail("self_ptr mismatch");
    }
    if per_cpu_init::gs_self() as u64 != addr {
        return Outcome::Fail("gs:[0] != PerCpu");
    }
    if crate::per_cpu!(cpu_id) != 0 {
        return Outcome::Fail("per_cpu! cpu_id");
    }
    if thread_init::current_id() != ThreadId::BOOTSTRAP {
        return Outcome::Fail("current not bootstrap");
    }
    if cpu.idle_id == ThreadId::BOOTSTRAP {
        return Outcome::Fail("idle still bootstrap");
    }
    if thread_init::name(cpu.idle_id) != "idle" {
        return Outcome::Fail("idle name");
    }
    if cpu.idle as *const _ == cpu.current as *const _ {
        return Outcome::Fail("idle == current");
    }
    if cpu.current.is_null() || cpu.idle.is_null() {
        return Outcome::Fail("current or idle null");
    }
    if !cpu.ready_head.is_null() {
        return Outcome::Fail("ready_head should be empty");
    }
    if !sched_init::is_live() {
        return Outcome::Fail("sched not live");
    }
    Outcome::Ok
}

fn test_per_cpu_identity() -> Outcome {
    let n = per_cpu_init::cpu_count();
    if n == 0 {
        return Outcome::Fail("cpu array empty");
    }
    let bsp = per_cpu_init::current();
    if bsp.cpu_id != 0 {
        return Outcome::Fail("not on bsp");
    }
    if bsp.self_ptr as u64 != bsp as *const _ as u64 {
        return Outcome::Fail("bsp self_ptr");
    }
    if per_cpu_init::gs_self() as u64 != bsp.self_ptr as u64 {
        return Outcome::Fail("bsp gs:[0]");
    }
    if crate::per_cpu!(cpu_id) != 0 {
        return Outcome::Fail("per_cpu! on bsp");
    }
    if !per_cpu_init::is_online(0) {
        return Outcome::Fail("bsp offline");
    }
    if n < 2 {
        return Outcome::Skip("no AP");
    }
    let mut i = 1u32;
    while i < n as u32 {
        let Some(c) = per_cpu_init::cpu(i) else {
            return Outcome::Fail("missing slot");
        };
        if !c.ready.load(Ordering::Acquire) {
            return Outcome::Fail("ap not ready");
        }
        if c.cpu_id != i {
            return Outcome::Fail("ap cpu_id");
        }
        if c.self_ptr as u64 != c as *const _ as u64 {
            return Outcome::Fail("ap self_ptr");
        }
        if c.idle.is_null() || c.current.is_null() {
            return Outcome::Fail("ap idle/current");
        }
        if c.apic_id == bsp.apic_id {
            return Outcome::Fail("ap apic_id");
        }
        if !per_cpu_init::is_online(i) {
            return Outcome::Fail("ap online mask");
        }
        if c.tsc_per_ms == 0 {
            return Outcome::Fail("ap tsc_per_ms");
        }
        i += 1;
    }
    Outcome::Ok
}

fn test_trampoline_page() -> Outcome {
    if !smp_init::trampoline_installed() {
        return Outcome::Fail("no cli opcode at 0x8000");
    }
    // INIT leaves CR0.CD|NW. Blob must AND 0x9FFFFFFF then WBINVD.
    let p = 0x8000 as *const u8;
    let mut and_cdnw = false;
    let mut wbinvd = false;
    let mut i = 0usize;
    while i + 1 < 0xD0 {
        let a = unsafe { p.add(i).read_volatile() };
        let b = unsafe { p.add(i + 1).read_volatile() };
        if a == 0x0F && b == 0x09 {
            wbinvd = true;
        }
        if i + 4 < 0xD0
            && a == 0x25
            && b == 0xFF
            && unsafe { p.add(i + 2).read_volatile() } == 0xFF
            && unsafe { p.add(i + 3).read_volatile() } == 0xFF
            && unsafe { p.add(i + 4).read_volatile() } == 0x9F
        {
            and_cdnw = true;
        }
        i += 1;
    }
    if !and_cdnw {
        return Outcome::Fail("trampoline missing CR0.CD/NW clear");
    }
    if !wbinvd {
        return Outcome::Fail("trampoline missing wbinvd");
    }
    Outcome::Ok
}

fn test_failed_ap_cleanup() -> Outcome {
    // First-fit KVA may map a fresh PT page on the first IST/stack wave.
    // unmap_4k does not return that PT. Warm up, then the measured wave
    // must restore the frame count (ROADMAP failed-AP exit gate).
    smp_init::exercise_fail_cleanup();
    let n0 = free_frames();
    smp_init::exercise_fail_cleanup();
    let n1 = free_frames();
    if n0 != n1 {
        let _ = writeln!(Serial, "vibeOS: ktest:   frames {n0} -> {n1}");
        Outcome::Fail("failed AP leaked frames")
    } else {
        Outcome::Ok
    }
}

static SENTINEL: AtomicU64 = AtomicU64::new(0);

fn sentinel_entry() {
    SENTINEL.store(0xC0FFEE, Ordering::SeqCst);
}

fn test_spawn_sentinel() -> Outcome {
    SENTINEL.store(0, Ordering::SeqCst);
    let nest0 = per_cpu_init::irq_nest();
    let h = thread_init::spawn_here("sentinel", sentinel_entry);
    if h.id() == ThreadId::BOOTSTRAP {
        return Outcome::Fail("spawned bootstrap id");
    }
    thread_init::switch_to(h.id());
    if SENTINEL.load(Ordering::SeqCst) != 0xC0FFEE {
        return Outcome::Fail("sentinel not written");
    }
    if thread_init::name(h.id()) != "sentinel" {
        return Outcome::Fail("name lost");
    }
    if thread_init::current_id() != ThreadId::BOOTSTRAP {
        return Outcome::Fail("did not return to bootstrap");
    }
    if thread_init::state(h.id()) != ThreadState::Dead {
        return Outcome::Fail("returned thread not dead");
    }
    if per_cpu_init::irq_nest() != nest0 {
        return Outcome::Fail("irq_nest leaked across spawn");
    }
    Outcome::Ok
}

static STEPS: AtomicU64 = AtomicU64::new(0);
static A_ID: AtomicU32 = AtomicU32::new(0);
static B_ID: AtomicU32 = AtomicU32::new(0);

fn thread_a() {
    STEPS.fetch_add(1, Ordering::SeqCst);
    thread_init::switch_to(ThreadId(B_ID.load(Ordering::SeqCst)));
    STEPS.fetch_add(1, Ordering::SeqCst);
}

fn thread_b() {
    STEPS.fetch_add(1, Ordering::SeqCst);
    thread_init::switch_to(ThreadId::BOOTSTRAP);
}

fn test_switch_two_threads() -> Outcome {
    STEPS.store(0, Ordering::SeqCst);
    let nest0 = per_cpu_init::irq_nest();
    let a = thread_init::spawn_here("a", thread_a);
    let b = thread_init::spawn_here("b", thread_b);
    A_ID.store(a.id().raw(), Ordering::SeqCst);
    B_ID.store(b.id().raw(), Ordering::SeqCst);
    thread_init::switch_to(a.id());
    if STEPS.load(Ordering::SeqCst) != 2 {
        return Outcome::Fail("expected a then b (2 steps)");
    }
    if thread_init::state(a.id()) != ThreadState::Ready {
        return Outcome::Fail("a should still be ready");
    }
    thread_init::switch_to(a.id());
    if STEPS.load(Ordering::SeqCst) != 3 {
        return Outcome::Fail("a did not resume");
    }
    if thread_init::state(a.id()) != ThreadState::Dead {
        return Outcome::Fail("a not dead after return");
    }
    if per_cpu_init::irq_nest() != nest0 {
        return Outcome::Fail("irq_nest leaked across switch");
    }
    Outcome::Ok
}

fn test_irq_guard_nest() -> Outcome {
    let _off = IrqsOffOnDrop;
    x86::sti();
    if !x86::interrupts_enabled() {
        return Outcome::Fail("sti did not set IF");
    }
    let nest0 = per_cpu_init::irq_nest();
    {
        let g1 = x86::InterruptGuard::enter();
        if x86::interrupts_enabled() {
            return Outcome::Fail("g1 left IF on");
        }
        if per_cpu_init::irq_nest() != nest0 + 1 {
            return Outcome::Fail("g1 nest");
        }
        {
            let g2 = x86::InterruptGuard::enter();
            if x86::interrupts_enabled() {
                return Outcome::Fail("g2 left IF on");
            }
            if per_cpu_init::irq_nest() != nest0 + 2 {
                return Outcome::Fail("g2 nest");
            }
            core::mem::drop(g2);
        }
        if x86::interrupts_enabled() {
            return Outcome::Fail("after g2 IF on");
        }
        if per_cpu_init::irq_nest() != nest0 + 1 {
            return Outcome::Fail("after g2 nest");
        }
        core::mem::drop(g1);
    }
    if !x86::interrupts_enabled() {
        return Outcome::Fail("outer drop did not restore IF");
    }
    if per_cpu_init::irq_nest() != nest0 {
        return Outcome::Fail("nest not restored");
    }
    Outcome::Ok
}

fn test_spin_mutex() -> Outcome {
    let m = SpinMutex::new(0u64);
    {
        let mut g = m.lock();
        if x86::interrupts_enabled() {
            return Outcome::Fail("lock left IF on");
        }
        *g = 42;
    }
    if *m.lock() != 42 {
        return Outcome::Fail("value lost");
    }
    Outcome::Ok
}

static YIELD_FLAG: AtomicU64 = AtomicU64::new(0);

fn yielder_entry() {
    YIELD_FLAG.store(1, Ordering::SeqCst);
    thread_init::yield_now();
    YIELD_FLAG.store(2, Ordering::SeqCst);
}

fn test_yield_now_switches() -> Outcome {
    YIELD_FLAG.store(0, Ordering::SeqCst);
    let _h = thread_init::spawn_here("yielder", yielder_entry);
    thread_init::yield_now();
    if YIELD_FLAG.load(Ordering::SeqCst) != 1 {
        return Outcome::Fail("yielder did not run");
    }
    thread_init::yield_now();
    if YIELD_FLAG.load(Ordering::SeqCst) != 2 {
        return Outcome::Fail("yielder did not resume");
    }
    Outcome::Ok
}

fn test_sleep_ms_50() -> Outcome {
    with_timer(|| {
        let t0 = time_init::uptime_ms();
        thread_init::sleep_ms(50);
        let dt = time_init::uptime_ms().saturating_sub(t0);
        if (50..=100).contains(&dt) {
            Outcome::Ok
        } else {
            let _ = writeln!(Serial, "vibeOS: ktest:   sleep_ms dt={dt}");
            Outcome::Fail("sleep_ms not 50-100ms")
        }
    })
}

static PREEMPT_A: AtomicU64 = AtomicU64::new(0);
static PREEMPT_B: AtomicU64 = AtomicU64::new(0);
static PREEMPT_STOP: AtomicBool = AtomicBool::new(false);

fn preempt_a() {
    while !PREEMPT_STOP.load(Ordering::Relaxed) {
        PREEMPT_A.fetch_add(1, Ordering::Relaxed);
        core::hint::spin_loop();
    }
}

fn preempt_b() {
    while !PREEMPT_STOP.load(Ordering::Relaxed) {
        PREEMPT_B.fetch_add(1, Ordering::Relaxed);
        core::hint::spin_loop();
    }
}

fn test_preempt_two_threads() -> Outcome {
    PREEMPT_A.store(0, Ordering::SeqCst);
    PREEMPT_B.store(0, Ordering::SeqCst);
    PREEMPT_STOP.store(false, Ordering::SeqCst);
    let _a = thread_init::spawn("preempt-a", preempt_a);
    let _b = thread_init::spawn("preempt-b", preempt_b);
    with_timer(|| {
        let t0 = time_init::uptime_ms();
        loop {
            let a = PREEMPT_A.load(Ordering::Relaxed);
            let b = PREEMPT_B.load(Ordering::Relaxed);
            if a > 0 && b > 0 {
                PREEMPT_STOP.store(true, Ordering::SeqCst);
                let t1 = time_init::uptime_ms();
                while time_init::uptime_ms().saturating_sub(t1) < 50 {
                    core::hint::spin_loop();
                }
                let _ = writeln!(Serial, "vibeOS: ktest:   preempt a={a} b={b}");
                return Outcome::Ok;
            }
            if time_init::uptime_ms().saturating_sub(t0) > 500 {
                PREEMPT_STOP.store(true, Ordering::SeqCst);
                let _ = writeln!(Serial, "vibeOS: ktest:   preempt a={a} b={b}");
                return Outcome::Fail("no preemption");
            }
            core::hint::spin_loop();
        }
    })
}

fn test_idle_runs() -> Outcome {
    with_timer(|| {
        let t0 = sched_init::idle_tsc();
        thread_init::sleep_ms(20);
        let t1 = sched_init::idle_tsc();
        if t1 > t0 {
            Outcome::Ok
        } else {
            let _ = writeln!(Serial, "vibeOS: ktest:   idle_tsc {t0} -> {t1}");
            Outcome::Fail("idle did not run")
        }
    })
}

fn dying_entry() {}

fn test_reap_returns_frames() -> Outcome {
    let before = free_frames();
    let h = thread_init::spawn_here("dying", dying_entry);
    thread_init::yield_now();
    if thread_init::current_id() != ThreadId::BOOTSTRAP {
        return Outcome::Fail("did not return to bootstrap");
    }
    if thread_init::try_state(h.id()) != Some(ThreadState::Dead) {
        return Outcome::Fail("returned thread not dead");
    }
    let after = free_frames();
    if after != before {
        let _ = writeln!(Serial, "vibeOS: ktest:   frames {before} -> {after}");
        return Outcome::Fail("reap did not restore frames");
    }
    Outcome::Ok
}

const REAP_MANY: usize = 16;

fn test_reap_many_via_idle() -> Outcome {
    with_timer(|| {
        let before = free_frames();
        let mut ids = [ThreadId::NONE; REAP_MANY];

        // Park bootstrap so the last death switches to idle. Idle's
        // from_irq resume skips reap; the idle loop must drain.
        let mut i = 0;
        while i < REAP_MANY {
            ids[i] = thread_init::spawn_here("dying", dying_entry).id();
            i += 1;
        }
        thread_init::sleep_ms(30);
        i = 0;
        while i < REAP_MANY {
            if thread_init::try_state(ids[i]) != Some(ThreadState::Dead) {
                return Outcome::Fail("parked wave not dead");
            }
            i += 1;
        }

        // Stay Running. Last death resumes us on the IRQ path (no reap).
        // yield_now no-switch must drain.
        i = 0;
        while i < REAP_MANY {
            ids[i] = thread_init::spawn_here("dying", dying_entry).id();
            i += 1;
        }
        let t0 = time_init::uptime_ms();
        loop {
            let mut n = 0usize;
            i = 0;
            while i < REAP_MANY {
                if thread_init::try_state(ids[i]) == Some(ThreadState::Dead) {
                    n += 1;
                }
                i += 1;
            }
            if n == REAP_MANY {
                break;
            }
            if time_init::uptime_ms().saturating_sub(t0) > 200 {
                return Outcome::Fail("running wave not dead");
            }
            core::hint::spin_loop();
        }
        thread_init::yield_now();

        let after = free_frames();
        if after != before {
            let _ = writeln!(Serial, "vibeOS: ktest:   frames {before} -> {after}");
            return Outcome::Fail("reap did not restore frames");
        }
        Outcome::Ok
    })
}

const MUTEX_ITERS: u64 = 1000;
static COUNTER: BlockingMutex<u64> = BlockingMutex::new(0);
static MUTEX_DONE: AtomicU32 = AtomicU32::new(0);

fn mutex_worker() {
    let mut i = 0u64;
    while i < MUTEX_ITERS {
        let mut g = COUNTER.lock();
        *g = (*g).wrapping_add(1);
        i += 1;
    }
    MUTEX_DONE.fetch_add(1, Ordering::SeqCst);
}

fn test_blocking_mutex_counter() -> Outcome {
    MUTEX_DONE.store(0, Ordering::SeqCst);
    *COUNTER.lock() = 0;
    let _a = thread_init::spawn("mu-a", mutex_worker);
    let _b = thread_init::spawn("mu-b", mutex_worker);
    with_timer(|| {
        let t0 = time_init::uptime_ms();
        loop {
            if MUTEX_DONE.load(Ordering::SeqCst) == 2 {
                break;
            }
            if time_init::uptime_ms().saturating_sub(t0) > 8_000 {
                let n = MUTEX_DONE.load(Ordering::SeqCst);
                let c = *COUNTER.lock();
                let _ = writeln!(Serial, "vibeOS: ktest:   mutex done={n} count={c}");
                return Outcome::Fail("mutex stall");
            }
            thread_init::yield_now();
        }
        let c = *COUNTER.lock();
        if c != MUTEX_ITERS * 2 {
            let _ = writeln!(Serial, "vibeOS: ktest:   mutex count={c}");
            return Outcome::Fail("mutex count");
        }
        Outcome::Ok
    })
}

static RW: RwLock<u64> = RwLock::new(0);
static RW_DONE: AtomicU32 = AtomicU32::new(0);

fn rw_writer() {
    let mut g = RW.write();
    *g = (*g).wrapping_add(1);
    RW_DONE.fetch_add(1, Ordering::SeqCst);
}

fn test_rwlock_exclusion() -> Outcome {
    RW_DONE.store(0, Ordering::SeqCst);
    *RW.write() = 0;
    with_timer(|| {
        {
            let r = RW.read();
            let _a = thread_init::spawn("rw-a", rw_writer);
            let _b = thread_init::spawn("rw-b", rw_writer);
            thread_init::yield_now();
            thread_init::sleep_ms(5);
            if RW_DONE.load(Ordering::SeqCst) != 0 {
                return Outcome::Fail("writer ran under read");
            }
            if *r != 0 {
                return Outcome::Fail("reader saw writer");
            }
            drop(r);
        }
        let t0 = time_init::uptime_ms();
        loop {
            if RW_DONE.load(Ordering::SeqCst) == 2 {
                break;
            }
            if time_init::uptime_ms().saturating_sub(t0) > 2_000 {
                return Outcome::Fail("rwlock stall");
            }
            thread_init::yield_now();
        }
        if *RW.read() != 2 {
            return Outcome::Fail("rwlock count");
        }
        Outcome::Ok
    })
}

static RW_TO: RwLock<u64> = RwLock::new(0);
static RW_WR_OUT: AtomicU32 = AtomicU32::new(0);
static RW_RD_GOT: AtomicU32 = AtomicU32::new(0);

fn rw_timeout_writer() {
    let ns = time_init::now_ns().saturating_add(15_000_000);
    match RW_TO.write_until(Some(Instant { ns })) {
        None => RW_WR_OUT.store(1, Ordering::SeqCst),
        Some(_g) => RW_WR_OUT.store(2, Ordering::SeqCst),
    }
}

fn rw_pref_reader() {
    let _g = RW_TO.read();
    RW_RD_GOT.store(1, Ordering::SeqCst);
}

fn test_rwlock_writer_timeout() -> Outcome {
    RW_WR_OUT.store(0, Ordering::SeqCst);
    RW_RD_GOT.store(0, Ordering::SeqCst);
    *RW_TO.write() = 0;
    with_timer(|| {
        let r = RW_TO.read();
        let _w = thread_init::spawn("rw-to-w", rw_timeout_writer);
        thread_init::yield_now();
        thread_init::sleep_ms(5);
        let _rd = thread_init::spawn("rw-to-r", rw_pref_reader);
        thread_init::yield_now();
        thread_init::sleep_ms(40);
        if RW_WR_OUT.load(Ordering::SeqCst) != 1 {
            drop(r);
            return Outcome::Fail("writer did not timeout");
        }
        let t0 = time_init::uptime_ms();
        loop {
            if RW_RD_GOT.load(Ordering::SeqCst) == 1 {
                drop(r);
                return Outcome::Ok;
            }
            if time_init::uptime_ms().saturating_sub(t0) > 2_000 {
                drop(r);
                return Outcome::Fail("reader stranded after writer timeout");
            }
            thread_init::yield_now();
        }
    })
}

static SEM: Semaphore = Semaphore::new(0);
static SEM_N: AtomicU32 = AtomicU32::new(0);

fn sem_waiter() {
    SEM.acquire();
    SEM_N.fetch_add(1, Ordering::SeqCst);
}

fn test_semaphore_wake() -> Outcome {
    SEM_N.store(0, Ordering::SeqCst);
    let _a = thread_init::spawn("sem-a", sem_waiter);
    let _b = thread_init::spawn("sem-b", sem_waiter);
    with_timer(|| {
        thread_init::yield_now();
        thread_init::sleep_ms(5);
        if SEM_N.load(Ordering::SeqCst) != 0 {
            return Outcome::Fail("sema acquired empty");
        }
        SEM.release();
        SEM.release();
        let t0 = time_init::uptime_ms();
        loop {
            if SEM_N.load(Ordering::SeqCst) == 2 {
                return Outcome::Ok;
            }
            if time_init::uptime_ms().saturating_sub(t0) > 2_000 {
                return Outcome::Fail("sema stall");
            }
            thread_init::yield_now();
        }
    })
}

static CM: BlockingMutex<bool> = BlockingMutex::new(false);
static CV: Condvar = Condvar::new();
static CV_DONE: AtomicBool = AtomicBool::new(false);

fn cv_waiter() {
    let mut g = CM.lock();
    while !*g {
        g = CV.wait(g);
    }
    CV_DONE.store(true, Ordering::SeqCst);
}

fn test_condvar_signal() -> Outcome {
    CV_DONE.store(false, Ordering::SeqCst);
    *CM.lock() = false;
    let _h = thread_init::spawn("cv", cv_waiter);
    with_timer(|| {
        thread_init::sleep_ms(10);
        {
            let mut g = CM.lock();
            *g = true;
            CV.notify_one();
        }
        let t0 = time_init::uptime_ms();
        loop {
            if CV_DONE.load(Ordering::SeqCst) {
                return Outcome::Ok;
            }
            if time_init::uptime_ms().saturating_sub(t0) > 2_000 {
                return Outcome::Fail("condvar stall");
            }
            thread_init::yield_now();
        }
    })
}

static CVREL_M: BlockingMutex<u32> = BlockingMutex::new(0);
static CVREL_CV: Condvar = Condvar::new();
static CVREL_WAITING: AtomicBool = AtomicBool::new(false);
static CVREL_DONE: AtomicBool = AtomicBool::new(false);

fn cvrel_waiter() {
    let g = CVREL_M.lock();
    CVREL_WAITING.store(true, Ordering::SeqCst);
    let _g = CVREL_CV.wait(g);
    CVREL_DONE.store(true, Ordering::SeqCst);
}

fn test_condvar_wait_releases() -> Outcome {
    CVREL_WAITING.store(false, Ordering::SeqCst);
    CVREL_DONE.store(false, Ordering::SeqCst);
    *CVREL_M.lock() = 0;
    let _h = thread_init::spawn("cvrel", cvrel_waiter);
    with_timer(|| {
        let t0 = time_init::uptime_ms();
        loop {
            if CVREL_WAITING.load(Ordering::SeqCst) {
                break;
            }
            if time_init::uptime_ms().saturating_sub(t0) > 2_000 {
                return Outcome::Fail("waiter never locked");
            }
            thread_init::yield_now();
        }
        {
            let mut g = CVREL_M.lock();
            *g = 1;
            CVREL_CV.notify_one();
        }
        let t1 = time_init::uptime_ms();
        loop {
            if CVREL_DONE.load(Ordering::SeqCst) {
                return Outcome::Ok;
            }
            if time_init::uptime_ms().saturating_sub(t1) > 2_000 {
                return Outcome::Fail("condvar wait held mutex");
            }
            thread_init::yield_now();
        }
    })
}

static CH: Channel<u64, 4> = Channel::new();
static CH_SUM: AtomicU64 = AtomicU64::new(0);

fn ch_consumer() {
    let mut i = 0u64;
    let mut sum = 0u64;
    while i < 32 {
        sum = sum.wrapping_add(CH.recv());
        i += 1;
    }
    CH_SUM.store(sum, Ordering::SeqCst);
}

fn test_channel_mpsc() -> Outcome {
    CH_SUM.store(0, Ordering::SeqCst);
    let _c = thread_init::spawn("ch-rx", ch_consumer);
    with_timer(|| {
        let mut i = 1u64;
        while i <= 32 {
            CH.send(i);
            i += 1;
        }
        let t0 = time_init::uptime_ms();
        loop {
            let s = CH_SUM.load(Ordering::SeqCst);
            if s != 0 {
                if s != 32 * 33 / 2 {
                    let _ = writeln!(Serial, "vibeOS: ktest:   chan sum={s}");
                    return Outcome::Fail("channel sum");
                }
                return Outcome::Ok;
            }
            if time_init::uptime_ms().saturating_sub(t0) > 2_000 {
                return Outcome::Fail("channel stall");
            }
            thread_init::yield_now();
        }
    })
}

static TM: BlockingMutex<u64> = BlockingMutex::new(0);
static TM_OUT: AtomicU32 = AtomicU32::new(0);

fn timeout_waiter() {
    let ns = time_init::now_ns().saturating_add(15_000_000);
    match TM.lock_until(Some(Instant { ns })) {
        None => TM_OUT.store(1, Ordering::SeqCst),
        Some(_g) => TM_OUT.store(2, Ordering::SeqCst),
    }
}

fn test_mutex_deadline() -> Outcome {
    TM_OUT.store(0, Ordering::SeqCst);
    with_timer(|| {
        let g = TM.lock();
        let _h = thread_init::spawn("tm", timeout_waiter);
        thread_init::sleep_ms(40);
        if TM_OUT.load(Ordering::SeqCst) != 1 {
            drop(g);
            return Outcome::Fail("deadline did not fire");
        }
        drop(g);
        Outcome::Ok
    })
}

fn test_sync_try_paths() -> Outcome {
    let m = BlockingMutex::new(1u64);
    {
        let _g = m.lock();
        if m.try_lock().is_some() {
            return Outcome::Fail("try_lock while held");
        }
    }
    if m.try_lock().is_none() {
        return Outcome::Fail("try_lock free");
    }
    let ch = Channel::<u64, 2>::new();
    if ch.try_send(3).is_err() {
        return Outcome::Fail("try_send");
    }
    if ch.try_recv() != Some(3) {
        return Outcome::Fail("try_recv");
    }
    if ch.try_recv().is_some() {
        return Outcome::Fail("try_recv empty");
    }
    CV.notify_all();
    Outcome::Ok
}

fn test_sched_lock_timer_irq() -> Outcome {
    with_timer(|| {
        let nest0 = per_cpu_init::irq_nest();
        let t0 = per_cpu_init::current().ticks;
        let wall0 = time_init::uptime_ms();
        loop {
            if per_cpu_init::current().ticks != t0 {
                break;
            }
            if time_init::uptime_ms().saturating_sub(wall0) > 200 {
                return Outcome::Fail("no ticks before lock");
            }
            core::hint::spin_loop();
        }
        let held = per_cpu_init::current().ticks;
        let inner = thread_init::with_sched_lock(|| {
            if x86::interrupts_enabled() {
                return Outcome::Fail("SCHED left IF on");
            }
            time_init::busy_wait_ms(20);
            if x86::interrupts_enabled() {
                return Outcome::Fail("IF on during hold");
            }
            if per_cpu_init::current().ticks != held {
                return Outcome::Fail("timer ran under SCHED");
            }
            Outcome::Ok
        });
        match inner {
            Outcome::Ok => {}
            other => return other,
        }
        match apic_init::timer_mode() {
            TimerMode::Pit => unsafe {
                core::arch::asm!("int $0x20");
            },
            TimerMode::TscDeadline | TimerMode::Periodic => unsafe {
                core::arch::asm!("int $0xF0");
            },
        }
        if per_cpu_init::current().ticks <= held {
            return Outcome::Fail("forced timer IRQ did not run");
        }
        if per_cpu_init::irq_nest() != nest0 {
            return Outcome::Fail("irq_nest leaked");
        }
        Outcome::Ok
    })
}

const SPAWN_EXIT_N: usize = 2000;
const SPAWN_EXIT_WARMUP: usize = 256;

fn spawn_until_dead(name: &'static str) -> Outcome {
    let h = thread_init::spawn_here(name, dying_entry);
    thread_init::yield_now();
    if thread_init::try_state(h.id()) != Some(ThreadState::Dead) {
        thread_init::yield_now();
    }
    if thread_init::try_state(h.id()) != Some(ThreadState::Dead) {
        Outcome::Fail("returned thread not dead")
    } else {
        Outcome::Ok
    }
}

fn test_spawn_exit_thousands() -> Outcome {
    // First-fit KVA walks new VA until coalesce. Mapping a fresh 2MiB
    // window allocates a PT page that unmap_4k does not free. Warm up
    // past one free-list overflow so the 2000 recycle already-mapped VA.
    let mut i = 0usize;
    while i < SPAWN_EXIT_WARMUP {
        match spawn_until_dead("die") {
            Outcome::Ok => {}
            other => return other,
        }
        i += 1;
    }
    thread_init::reap_zombies();

    let before = free_frames();
    i = 0;
    while i < SPAWN_EXIT_N {
        match spawn_until_dead("die") {
            Outcome::Ok => {}
            other => return other,
        }
        i += 1;
    }
    thread_init::reap_zombies();
    let after = free_frames();
    if after != before {
        let h = crate::heap_init::stats();
        let k = kva_init::stats();
        let _ = writeln!(
            Serial,
            "vibeOS: ktest:   frames {before} -> {after} n={SPAWN_EXIT_N} heap {}/{} kva {}",
            h.used, h.capacity, k.used
        );
        return Outcome::Fail("spawn/exit leaked frames");
    }
    Outcome::Ok
}

fn second_cpu() -> Option<u32> {
    let mask = per_cpu_init::online_mask();
    let mut i = 1u32;
    while i < 64 {
        if mask & (1u64 << i) != 0 {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn spin_until_ns(pred: impl Fn() -> bool, ns: u64) -> bool {
    let t0 = time_init::now_ns();
    while !pred() {
        if time_init::now_ns().saturating_sub(t0) > ns {
            return false;
        }
        ipi_init::service_incoming();
        core::hint::spin_loop();
    }
    true
}

static XCPU_FLAG: AtomicU64 = AtomicU64::new(0);
static XCPU_CPU: AtomicU32 = AtomicU32::new(0xFFFF);

fn xcpu_entry() {
    XCPU_CPU.store(per_cpu_init::current().cpu_id, Ordering::SeqCst);
    XCPU_FLAG.store(1, Ordering::SeqCst);
}

fn test_cross_cpu_spawn() -> Outcome {
    let Some(ap) = second_cpu() else {
        return Outcome::Skip("no AP");
    };
    XCPU_FLAG.store(0, Ordering::SeqCst);
    XCPU_CPU.store(0xFFFF, Ordering::SeqCst);
    let h = thread_init::spawn_on("xcpu", xcpu_entry, ap);
    if !spin_until_ns(|| XCPU_FLAG.load(Ordering::SeqCst) != 0, 500_000_000) {
        return Outcome::Fail("AP thread did not run");
    }
    if XCPU_CPU.load(Ordering::SeqCst) != ap {
        return Outcome::Fail("thread ran on wrong cpu");
    }
    if !spin_until_ns(
        || thread_init::try_state(h.id()) == Some(ThreadState::Dead),
        500_000_000,
    ) {
        return Outcome::Fail("AP thread did not exit");
    }
    if thread_init::cpu_of(h.id()) != ap {
        return Outcome::Fail("tcb.cpu != ap");
    }
    Outcome::Ok
}

static WAKE_FLAG: AtomicU64 = AtomicU64::new(0);

fn wake_ap_entry() {
    WAKE_FLAG.store(1, Ordering::SeqCst);
}

fn test_reschedule_ipi_wake_ap() -> Outcome {
    let Some(ap) = second_cpu() else {
        return Outcome::Skip("no AP");
    };
    WAKE_FLAG.store(0, Ordering::SeqCst);
    let before = ipi_init::reschedule_count();
    let _h = thread_init::spawn_on("wake-ap", wake_ap_entry, ap);
    if !spin_until_ns(|| WAKE_FLAG.load(Ordering::SeqCst) != 0, 500_000_000) {
        return Outcome::Fail("idle AP not woken");
    }
    let after = ipi_init::reschedule_count();
    if after <= before {
        return Outcome::Fail("no reschedule IPI");
    }
    Outcome::Ok
}

static CALL_CPU: AtomicU32 = AtomicU32::new(0xFFFF);

fn call_mark(arg: *mut ()) {
    let _ = arg;
    CALL_CPU.store(per_cpu_init::current().cpu_id, Ordering::SeqCst);
}

fn test_call_function_ipi() -> Outcome {
    let Some(ap) = second_cpu() else {
        return Outcome::Skip("no AP");
    };
    CALL_CPU.store(0xFFFF, Ordering::SeqCst);
    let before = ipi_init::call_count();
    ipi_init::call_cpu(ap, call_mark, core::ptr::null_mut(), true);
    if CALL_CPU.load(Ordering::SeqCst) != ap {
        return Outcome::Fail("call-function did not run on AP");
    }
    if ipi_init::call_count() <= before {
        return Outcome::Fail("call count stuck");
    }
    Outcome::Ok
}

struct ShootProbe {
    va: u64,
    /// 0 idle, 1 access ok, 2 fault.
    result: AtomicU64,
}

fn shoot_touch(arg: *mut ()) {
    let p = unsafe { &*(arg as *const ShootProbe) };
    let fault = catch_fault(|| unsafe {
        core::ptr::read_volatile(p.va as *const u64);
    });
    p.result.store(
        if fault.is_some() { 2 } else { 1 },
        Ordering::SeqCst,
    );
}

fn test_tlb_shootdown_remote() -> Outcome {
    let Some(ap) = second_cpu() else {
        return Outcome::Skip("no AP");
    };
    let Some(va) = kva_init::alloc_va(PAGE_SIZE) else {
        return Outcome::Fail("kva alloc");
    };
    let Some(pa) = alloc_frame() else {
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("frame alloc");
    };
    if unsafe { paging_init::map_4k(va, pa, heap_flags()) }.is_err() {
        free_frame(pa);
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("map");
    }
    unsafe { (va.as_u64() as *mut u64).write_volatile(0xD15EA5E) };

    let probe = ShootProbe {
        va: va.as_u64(),
        result: AtomicU64::new(0),
    };
    ipi_init::call_cpu(ap, shoot_touch, &probe as *const _ as *mut (), true);
    if probe.result.load(Ordering::SeqCst) != 1 {
        let _ = unsafe { paging_init::unmap_4k(va) };
        free_frame(pa);
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("AP could not read mapped page");
    }

    let before = ipi_init::shootdown_count();
    let _ = unsafe { paging_init::unmap_4k(va) };
    probe.result.store(0, Ordering::SeqCst);
    ipi_init::call_cpu(ap, shoot_touch, &probe as *const _ as *mut (), true);
    if probe.result.load(Ordering::SeqCst) != 2 {
        let _ = unsafe { paging_init::map_4k(va, pa, heap_flags()) };
        free_frame(pa);
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("AP did not fault after unmap");
    }
    if per_cpu_init::online_mask().count_ones() > 1 && ipi_init::shootdown_count() <= before {
        let _ = unsafe { paging_init::map_4k(va, pa, heap_flags()) };
        free_frame(pa);
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("no shootdown IPI");
    }

    if unsafe { paging_init::map_4k(va, pa, heap_flags()) }.is_err() {
        free_frame(pa);
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("remap");
    }
    unsafe { (va.as_u64() as *mut u64).write_volatile(0xD15EA5E) };
    probe.result.store(0, Ordering::SeqCst);
    ipi_init::call_cpu(ap, shoot_touch, &probe as *const _ as *mut (), true);
    let ok = probe.result.load(Ordering::SeqCst) == 1;
    let _ = unsafe { paging_init::unmap_4k(va) };
    free_frame(pa);
    kva_init::free_va(va, PAGE_SIZE);
    if !ok {
        return Outcome::Fail("AP could not read after remap");
    }
    Outcome::Ok
}

static HAMMER_DONE: AtomicU32 = AtomicU32::new(0);

fn alloc_hammer() {
    let mut i = 0u32;
    while i < 128 {
        let b = Box::new([i; 16]);
        if b[0] != i {
            return;
        }
        i += 1;
    }
    HAMMER_DONE.fetch_add(1, Ordering::SeqCst);
}

fn test_alloc_stress_smp() -> Outcome {
    let mask = per_cpu_init::online_mask();
    let n = mask.count_ones();
    if n < 2 {
        return Outcome::Skip("no AP");
    }
    HAMMER_DONE.store(0, Ordering::SeqCst);
    let mut c = 1u32;
    while c < 64 {
        if mask & (1u64 << c) != 0 {
            let _ = thread_init::spawn_on("hammer", alloc_hammer, c);
        }
        c += 1;
    }
    alloc_hammer();
    if !spin_until_ns(|| HAMMER_DONE.load(Ordering::SeqCst) >= n, 2_000_000_000) {
        let _ = writeln!(
            Serial,
            "vibeOS: ktest:   hammers {}",
            HAMMER_DONE.load(Ordering::SeqCst)
        );
        return Outcome::Fail("allocator stress hung");
    }
    Outcome::Ok
}

fn test_log_boot_captured() -> Outcome {
    if !crate::log_init::contains_msg("serial online") {
        return Outcome::Fail("serial online missing from ring");
    }
    if !crate::log_init::contains_msg("smp: done") {
        return Outcome::Fail("smp: done missing from ring");
    }
    if !crate::log_init::contains_msg("console ok") {
        return Outcome::Fail("console ok missing from ring");
    }
    Outcome::Ok
}

fn test_log_runtime_filter() -> Outcome {
    use vibeos::log::Level;
    let old = crate::log_init::max_level();
    crate::log_init::set_max_level(Level::Error);
    crate::klog!(Level::Debug, "vibeOS: ktest: log-filter-hidden-xyz");
    if crate::log_init::contains_msg("log-filter-hidden-xyz") {
        crate::log_init::set_max_level(old);
        return Outcome::Fail("debug stored at error max");
    }
    crate::log_init::set_max_level(Level::Trace);
    crate::klog!(Level::Debug, "vibeOS: ktest: log-filter-visible-xyz");
    let ok = crate::log_init::contains_msg("log-filter-visible-xyz");
    crate::log_init::set_max_level(old);
    if ok {
        Outcome::Ok
    } else {
        Outcome::Fail("debug not stored after raising max")
    }
}

fn test_log_emit_roundtrip() -> Outcome {
    crate::klog!(vibeos::log::Level::Info, "vibeOS: ktest: log-roundtrip-abc");
    if crate::log_init::contains_msg("log-roundtrip-abc") {
        Outcome::Ok
    } else {
        Outcome::Fail("info record missing")
    }
}

fn test_log_dmesg_no_recapture() -> Outcome {
    let n = crate::log_init::ring_len();
    crate::log_init::dmesg(Some(vibeos::log::Level::Info));
    if crate::log_init::ring_len() != n {
        return Outcome::Fail("dmesg recaptured into ring");
    }
    if crate::log_init::contains_msg("vibeOS: dmesg:") {
        return Outcome::Fail("dmesg line stored");
    }
    Outcome::Ok
}

fn test_fb_bgrx_roundtrip() -> Outcome {
    if !crate::fb_init::ready() {
        return Outcome::Fail("no framebuffer");
    }
    let color = vibeos::fb::pack_bgrx(0x11, 0x22, 0x33);
    if !crate::fb_init::put_pixel(0, 0, color) {
        return Outcome::Fail("put origin");
    }
    match crate::fb_init::get_pixel(0, 0) {
        Some(got) if got == color => Outcome::Ok,
        Some(_) => Outcome::Fail("pixel mismatch"),
        None => Outcome::Fail("get origin"),
    }
}

fn test_fb_pitch() -> Outcome {
    let Some(pitch) = crate::fb_init::pitch() else {
        return Outcome::Fail("no pitch");
    };
    let Some(width) = crate::fb_init::width() else {
        return Outcome::Fail("no width");
    };
    // Must not assume pitch == width*4. QEMU often equals; still use pitch.
    if pitch < (width as u64) * 4 {
        return Outcome::Fail("pitch smaller than width*4");
    }
    let color = vibeos::fb::pack_bgrx(0x44, 0x55, 0x66);
    if !crate::fb_init::put_pixel(0, 1, color) {
        return Outcome::Fail("put row1");
    }
    match crate::fb_init::get_pixel(0, 1) {
        Some(got) if got == color => Outcome::Ok,
        Some(_) => Outcome::Fail("row1 mismatch"),
        None => Outcome::Fail("get row1"),
    }
}

fn test_fb_cr_home() -> Outcome {
    if !crate::fb_init::ready() {
        return Outcome::Fail("no framebuffer");
    }
    crate::fb_init::write(b"\n");
    let Some((col, row)) = crate::fb_init::cursor() else {
        return Outcome::Fail("no cursor");
    };
    if col != 0 {
        return Outcome::Fail("newline not col0");
    }
    crate::fb_init::write(b"X");
    let mut hit: Option<(u32, u32)> = None;
    let mut gy = 0u8;
    while gy < vibeos::font::FONT_H as u8 && hit.is_none() {
        let mut gx = 0u8;
        while gx < vibeos::font::FONT_W as u8 {
            if vibeos::font::glyph_pixel(b'X', gx, gy) {
                hit = Some((gx as u32, gy as u32));
                break;
            }
            gx += 1;
        }
        gy += 1;
    }
    let Some((gx, gy)) = hit else {
        return Outcome::Fail("X glyph empty");
    };
    let (ox, oy) = vibeos::fb::glyph_origin(0, row);
    let Some(lit) = crate::fb_init::get_pixel(ox + gx, oy + gy) else {
        return Outcome::Fail("get lit");
    };
    crate::fb_init::write(b"\r ");
    match crate::fb_init::get_pixel(ox + gx, oy + gy) {
        Some(after) if after != lit => Outcome::Ok,
        Some(_) => Outcome::Fail("CR did not home"),
        None => Outcome::Fail("get after"),
    }
}

fn test_kbd_gsi_unmasked() -> Outcome {
    if crate::kbd_init::pic_fallback() {
        return Outcome::Skip("pic fallback");
    }
    let Some(gsi) = crate::kbd_init::gsi() else {
        return Outcome::Fail("no keyboard gsi");
    };
    match crate::apic_init::gsi_masked(gsi) {
        Some(false) => Outcome::Ok,
        Some(true) => Outcome::Fail("keyboard gsi still masked"),
        None => Outcome::Fail("gsi not on ioapic"),
    }
}

fn test_console_mux() -> Outcome {
    use vibeos::console::BackendId;
    if !crate::console_init::live() {
        return Outcome::Fail("mux not live");
    }
    if !crate::console_init::enabled(BackendId::Serial) {
        return Outcome::Fail("serial off");
    }
    if crate::fb_init::ready() && !crate::console_init::enabled(BackendId::Framebuffer) {
        return Outcome::Fail("fb off");
    }
    crate::console_init::write(b"");
    crate::console_init::set_enabled(BackendId::Framebuffer, false);
    if crate::console_init::enabled(BackendId::Framebuffer) {
        crate::console_init::set_enabled(BackendId::Framebuffer, true);
        return Outcome::Fail("disable failed");
    }
    crate::console_init::set_enabled(BackendId::Framebuffer, true);
    if crate::fb_init::ready() && !crate::console_init::enabled(BackendId::Framebuffer) {
        return Outcome::Fail("re-enable failed");
    }
    Outcome::Ok
}

fn test_kbd_ring_drain() -> Outcome {
    if !crate::kbd_init::live() {
        return Outcome::Fail("kbd not live");
    }
    crate::kbd_init::push_for_test(vibeos::kbd::DecodedKey::Char(b'q'));
    match crate::console_init::read() {
        Some(vibeos::kbd::DecodedKey::Char(b'q')) => Outcome::Ok,
        Some(_) => Outcome::Fail("wrong key"),
        None => Outcome::Fail("ring empty"),
    }
}

fn test_shell_registry() -> Outcome {
    for name in crate::shell_init::builtin_names() {
        if !crate::shell_init::has_command(name) {
            return Outcome::Fail("missing builtin");
        }
    }
    if crate::shell_init::command_count() < 10 {
        return Outcome::Fail("registry short");
    }
    if crate::shell_init::has_command("not-a-cmd") {
        return Outcome::Fail("unknown present");
    }
    Outcome::Ok
}

fn test_shell_dispatch() -> Outcome {
    if crate::shell_init::dispatch_line("echo ktest-shell-echo").is_err() {
        return Outcome::Fail("echo");
    }
    if crate::shell_init::dispatch_line("").is_err() {
        return Outcome::Fail("empty");
    }
    if crate::shell_init::dispatch_line("not-a-cmd").is_ok() {
        return Outcome::Fail("unknown succeeded");
    }
    if crate::shell_init::dispatch_line("dmesg info").is_err() {
        return Outcome::Fail("dmesg");
    }
    Outcome::Ok
}

fn test_shell_dmesg_level() -> Outcome {
    use vibeos::log::Level;
    let old = crate::log_init::max_level();
    if crate::shell_init::dispatch_line("dmesg -n error").is_err() {
        crate::log_init::set_max_level(old);
        return Outcome::Fail("dmesg -n");
    }
    crate::klog!(Level::Debug, "vibeOS: ktest: shell-level-hidden");
    if crate::log_init::contains_msg("shell-level-hidden") {
        crate::log_init::set_max_level(old);
        return Outcome::Fail("debug stored at error");
    }
    if crate::shell_init::dispatch_line("dmesg -n trace").is_err() {
        crate::log_init::set_max_level(old);
        return Outcome::Fail("dmesg -n trace");
    }
    crate::klog!(Level::Debug, "vibeOS: ktest: shell-level-visible");
    let ok = crate::log_init::contains_msg("shell-level-visible");
    crate::log_init::set_max_level(old);
    if ok {
        Outcome::Ok
    } else {
        Outcome::Fail("debug missing after -n trace")
    }
}

const PCI_QEMU_IDS: &[(u16, u16)] = &[
    (0x8086, 0x1237), // 440FX
    (0x8086, 0x7000), // PIIX3 ISA
    (0x8086, 0x7010), // PIIX3 IDE
    (0x8086, 0x7113), // PIIX4 ACPI
    (0x1234, 0x1111), // Bochs VGA
    (0x8086, 0x100e), // e1000
];

fn test_pci_qemu_set() -> Outcome {
    if !pci_init::live() {
        return Outcome::Fail("pci not live");
    }
    if dev_init::len() < PCI_QEMU_IDS.len() {
        return Outcome::Fail("device count");
    }
    let mut i = 0usize;
    while i < PCI_QEMU_IDS.len() {
        let (v, d) = PCI_QEMU_IDS[i];
        if dev_init::find_id(v, d).is_none() {
            return Outcome::Fail("missing qemu id");
        }
        i += 1;
    }
    Outcome::Ok
}

fn test_pci_bar_map() -> Outcome {
    let Some((_, d)) = dev_init::find_id(0x1234, 0x1111) else {
        return Outcome::Fail("no vga");
    };
    let mut i = 0usize;
    while i < pci::MAX_BARS {
        let r = d.resources[i];
        if !r.is_empty()
            && r.size > 0
            && r.size <= pci::MAX_BAR_MAP
            && r.mapped_va != 0
        {
            return Outcome::Ok;
        }
        i += 1;
    }
    Outcome::Fail("vga bar unmapped")
}

fn test_pci_cfg_rw() -> Outcome {
    let bdf = Bdf::new(0, 0, 0);
    let id = pci_init::cfg_read32(bdf, CFG_VENDOR);
    if id as u16 != 0x8086 {
        return Outcome::Fail("host vendor");
    }
    if (id >> 16) as u16 != 0x1237 {
        return Outcome::Fail("host device");
    }
    let prev = pci_init::cfg_read32(bdf, CFG_COMMAND) as u16;
    pci_init::enable_mem_master(bdf);
    let now = pci_init::cfg_read32(bdf, CFG_COMMAND) as u16;
    pci_init::cfg_write32(bdf, CFG_COMMAND, prev as u32);
    if now & (CMD_MEM | CMD_MASTER) != CMD_MEM | CMD_MASTER {
        return Outcome::Fail("cmd bits");
    }
    Outcome::Ok
}

fn test_pci_claim_exclusive() -> Outcome {
    let Some((i, d)) = dev_init::find_id(0x8086, 0x100e) else {
        return Outcome::Fail("no e1000");
    };
    let mut b = 0u8;
    let mut found = false;
    while (b as usize) < pci::MAX_BARS {
        if !d.resources[b as usize].is_empty() {
            found = true;
            break;
        }
        b += 1;
    }
    if !found {
        return Outcome::Fail("e1000 no bar");
    }
    if let Err(e) = dev_init::claim(i, b) {
        return Outcome::Fail(e.as_str());
    }
    match dev_init::claim(i, b) {
        Err(ClaimError::Already) => Outcome::Ok,
        Err(_) => Outcome::Fail("wrong claim err"),
        Ok(()) => Outcome::Fail("double claim"),
    }
}

struct HostBridgeDrv;

static HOST_BRIDGE_IDS: &[IdMatch] = &[IdMatch::vid_did(0x8086, 0x1237)];
static HOST_BRIDGE_DRV: HostBridgeDrv = HostBridgeDrv;

impl Driver for HostBridgeDrv {
    fn name(&self) -> &'static str {
        "host-bridge"
    }
    fn ids(&self) -> &'static [IdMatch] {
        HOST_BRIDGE_IDS
    }
    fn order(&self) -> u8 {
        1
    }
    fn probe(&self, _dev: &mut Device) -> Result<(), ProbeError> {
        Ok(())
    }
    fn remove(&self, _dev: &mut Device) {}
}

fn test_pci_bind_order() -> Outcome {
    if !dev_init::register_driver(&HOST_BRIDGE_DRV) {
        return Outcome::Fail("register");
    }
    dev_init::bind_all();
    let Some((_, d)) = dev_init::find_id(0x8086, 0x1237) else {
        return Outcome::Fail("no host");
    };
    match d.bound {
        Some("host-bridge") => Outcome::Ok,
        Some(_) => Outcome::Fail("wrong driver"),
        None => Outcome::Fail("unbound"),
    }
}

fn test_lspci_cmd() -> Outcome {
    if !crate::shell_init::has_command("lspci") {
        return Outcome::Fail("no lspci");
    }
    if !crate::shell_init::has_command("devices") {
        return Outcome::Fail("no devices");
    }
    if crate::shell_init::dispatch_line("lspci").is_err() {
        return Outcome::Fail("lspci");
    }
    if crate::shell_init::dispatch_line("devices").is_err() {
        return Outcome::Fail("devices");
    }
    Outcome::Ok
}
