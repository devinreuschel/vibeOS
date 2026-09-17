//! AP trampoline install and bring-up. ROADMAP §4.4–4.5, DESIGN §7.3–7.4.
//!
//! One AP at a time: they share the trampoline page and its param block.
//! `write_volatile` + `compiler_fence(SeqCst)` before SIPI.

use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::Ordering;

use vibeos::apic::IpiMode;
use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::marker;
use vibeos::per_cpu::PerCpu;
use vibeos::smp::{
    blob_fits, pack_idtr, INIT_WAIT_MS, PARAM_CR3, PARAM_ENTRY, PARAM_IDT, PARAM_STACK,
    READY_TIMEOUT_MS, SIPI_VECTOR, SIPI_WAIT_MS, TRAMPOLINE_PHYS,
};
use vibeos::thread::ThreadId;

use crate::acpi_init;
use crate::apic_init;
use crate::arch;
use crate::arch::gdt::{self, ApTables, CpuTables};
use crate::kva_init::{self, GuardedStack};
use crate::per_cpu_init;
use crate::serial::{self, Serial};
use crate::thread_init;
use crate::time_init;
use crate::x86;

const BLOB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/trampoline.bin"));
const _: () = assert!(blob_fits(BLOB.len()), "trampoline blob overlaps param block");

struct BootCell<T>(core::cell::UnsafeCell<T>);
unsafe impl<T> Sync for BootCell<T> {}
impl<T> BootCell<T> {
    const fn new(v: T) -> Self {
        Self(core::cell::UnsafeCell::new(v))
    }
    unsafe fn get_mut(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
    fn get(&self) -> &T {
        unsafe { &*self.0.get() }
    }
}

/// What the AP reads before it has GS. One AP at a time.
struct Starting {
    cpu: *mut PerCpu,
    cpu_tables: *mut CpuTables,
}

impl Starting {
    const fn empty() -> Self {
        Self {
            cpu: core::ptr::null_mut(),
            cpu_tables: core::ptr::null_mut(),
        }
    }
}

static STARTING: BootCell<Starting> = BootCell::new(Starting::empty());
static LIVE_TABLES: BootCell<Vec<ApTables>> = BootCell::new(Vec::new());

struct ApAlloc {
    cpu_id: u32,
    apic_id: u8,
    tables: ApTables,
    stack: GuardedStack,
    idle_id: ThreadId,
    published: bool,
}

fn tramp_page() -> *mut u8 {
    TRAMPOLINE_PHYS as *mut u8
}

fn write_u64(off: usize, val: u64) {
    unsafe {
        tramp_page().add(off).cast::<u64>().write_volatile(val);
    }
}

fn install_blob() {
    let dst = tramp_page();
    let mut i = 0;
    while i < BLOB.len() {
        unsafe { dst.add(i).write_volatile(BLOB[i]) };
        i += 1;
    }
}

fn patch_params(cr3: u64, stack_top: u64, entry: u64, idt_limit: u16, idt_base: u64) {
    write_u64(PARAM_CR3, cr3);
    write_u64(PARAM_STACK, stack_top);
    write_u64(PARAM_ENTRY, entry);
    let packed = pack_idtr(idt_limit, idt_base);
    let mut i = 0;
    while i < packed.len() {
        unsafe { tramp_page().add(PARAM_IDT + i).write_volatile(packed[i]) };
        i += 1;
    }
}

fn alloc_ap_resources(cpu_id: u32, apic_id: u8, publish: bool) -> Option<ApAlloc> {
    let tables = gdt::alloc_ap_tables()?;
    let stack = match kva_init::alloc_guarded_stack(DEFAULT_STACK_PAGES) {
        Some(s) => s,
        None => {
            gdt::free_ap_tables(tables);
            return None;
        }
    };
    let Some(idle_id) = thread_init::adopt_ap_idle(cpu_id, stack) else {
        kva_init::free_stack(stack);
        gdt::free_ap_tables(tables);
        return None;
    };
    let idle_ptr = thread_init::tcb_ptr(idle_id);
    if publish {
        if let Some(cpu) = per_cpu_init::cpu_mut(cpu_id) {
            cpu.apic_id = apic_id as u32;
            cpu.idle_id = idle_id;
            cpu.idle = idle_ptr;
            cpu.current = idle_ptr;
            cpu.tsc_per_ms = time_init::tsc_per_ms();
            cpu.timer_mode = apic_init::timer_mode();
            cpu.ready.store(false, Ordering::Relaxed);
            core::sync::atomic::compiler_fence(Ordering::SeqCst);
        }
    }
    Some(ApAlloc {
        cpu_id,
        apic_id,
        tables,
        stack,
        idle_id,
        published: publish,
    })
}

fn free_ap_resources(a: ApAlloc) {
    if a.published {
        if let Some(cpu) = per_cpu_init::cpu_mut(a.cpu_id) {
            cpu.idle = core::ptr::null_mut();
            cpu.current = core::ptr::null_mut();
            cpu.idle_id = ThreadId::NONE;
            cpu.ready.store(false, Ordering::Relaxed);
            cpu.apic_id = 0;
        }
    }
    if let Some(stack) = thread_init::abandon_ap_idle(a.idle_id) {
        kva_init::free_stack(stack);
    } else {
        kva_init::free_stack(a.stack);
    }
    gdt::free_ap_tables(a.tables);
}

fn wait_ready(cpu_id: u32) -> bool {
    let mut ms = 0u64;
    while ms < READY_TIMEOUT_MS {
        if per_cpu_init::cpu(cpu_id).is_some_and(|c| c.ready.load(Ordering::Acquire)) {
            return true;
        }
        time_init::busy_wait_ms(1);
        ms += 1;
    }
    false
}

fn start_one(a: ApAlloc) -> bool {
    let cpu_id = a.cpu_id;
    let apic_id = a.apic_id;
    let cr3 = x86::read_cr3();
    if cr3 > 0xFFFF_FFFF {
        serial::line("vibeOS: smp: cr3 above 4GiB");
        free_ap_resources(a);
        return false;
    }
    let stack_top = a.stack.top().as_u64();
    let entry = ap_entry as *const () as usize as u64;
    let (idt_limit, idt_base) = arch::idt::pointer();

    let cpu_ptr = match per_cpu_init::cpu_mut(cpu_id) {
        Some(c) => c as *mut PerCpu,
        None => {
            free_ap_resources(a);
            return false;
        }
    };
    let tables_ptr = a.tables.tables.as_ref() as *const CpuTables as *mut CpuTables;
    let starting = unsafe { STARTING.get_mut() };
    starting.cpu = cpu_ptr;
    starting.cpu_tables = tables_ptr;
    core::sync::atomic::compiler_fence(Ordering::SeqCst);

    patch_params(cr3, stack_top, entry, idt_limit, idt_base);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    x86::mfence();

    if apic_init::send_ipi(apic_id, 0, IpiMode::Init).is_err() {
        serial::line("vibeOS: smp: INIT failed");
        free_ap_resources(a);
        return false;
    }
    time_init::busy_wait_ms(INIT_WAIT_MS);
    let _ = apic_init::send_ipi(apic_id, SIPI_VECTOR, IpiMode::Sipi);
    time_init::busy_wait_ms(SIPI_WAIT_MS);
    let _ = apic_init::send_ipi(apic_id, SIPI_VECTOR, IpiMode::Sipi);

    if !wait_ready(cpu_id) {
        let _ = writeln!(Serial, "vibeOS: smp: apic {apic_id} timed out");
        free_ap_resources(a);
        return false;
    }

    let ApAlloc { tables, .. } = a;
    unsafe { LIVE_TABLES.get_mut().push(tables) };
    serial::line(marker::SMP_AP_ONLINE);
    true
}

extern "C" fn ap_entry() -> ! {
    x86::cli();
    let st = STARTING.get();
    let tables = unsafe { &*st.cpu_tables };
    let cpu = unsafe { &mut *st.cpu };
    // `mov gs` zeros the hidden base. GS_BASE before any lidt so NMI
    // cannot gs:[0] a null PerCpu (DESIGN §7.4 / ROADMAP).
    unsafe { tables.load() };
    unsafe { per_cpu_init::install_gs(cpu) };
    unsafe { arch::idt::load() };
    unsafe { apic_init::enable_ap() };
    cpu.tsc_per_ms = time_init::tsc_per_ms();
    cpu.timer_mode = apic_init::timer_mode();
    apic_init::arm_ap();
    per_cpu_init::mark_online(cpu.cpu_id);
    let _ = writeln!(
        Serial,
        "{}{}{}",
        marker::SCHED_CPU_PREFIX,
        cpu.cpu_id,
        marker::SCHED_CPU_SUFFIX
    );
    cpu.ready.store(true, Ordering::Release);
    x86::sti();
    crate::sched_init::idle_loop();
}

/// Bring up every enabled MADT CPU except the BSP. Emits `smp: done`.
///
/// # Safety
/// Scheduler live, LAPIC ready, trampoline page identity-mapped and
/// excluded from the PMM.
pub unsafe fn init() {
    install_blob();
    core::sync::atomic::compiler_fence(Ordering::SeqCst);

    let bsp_apic = per_cpu_init::current().apic_id as u8;
    let Some(info) = acpi_init::info() else {
        serial::line(marker::SMP_DONE);
        return;
    };
    let Some(madt) = info.madt.as_ref() else {
        serial::line(marker::SMP_DONE);
        return;
    };

    let mut logical = 1u32;
    let mut i = 0usize;
    while i < madt.cpu_count {
        let apic_id = madt.apic_ids[i];
        i += 1;
        if apic_id == bsp_apic {
            continue;
        }
        if logical as usize >= per_cpu_init::cpu_count() {
            break;
        }
        let Some(alloc) = alloc_ap_resources(logical, apic_id, true) else {
            let _ = writeln!(Serial, "vibeOS: smp: apic {apic_id} alloc failed");
            logical += 1;
            continue;
        };
        let _ = start_one(alloc);
        logical += 1;
    }

    serial::line(marker::SMP_DONE);
}

/// Allocate the same resources as bring-up, then take the timeout free
/// path. Frame count must return to baseline (injectable fault).
#[cfg(feature = "kernel_tests")]
pub fn exercise_fail_cleanup() {
    if let Some(a) = alloc_ap_resources(0xFE, 0xFE, false) {
        free_ap_resources(a);
    }
}

#[cfg(feature = "kernel_tests")]
pub fn trampoline_installed() -> bool {
    unsafe { tramp_page().read_volatile() == 0xFA }
}
