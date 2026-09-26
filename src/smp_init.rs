//! AP trampoline install and bring-up. ROADMAP §4.4–4.5, DESIGN §7.3–7.4.
//!
//! One AP at a time: they share the trampoline page and its param block.
//! `write_volatile` + `compiler_fence(SeqCst)` before SIPI.

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use vibeos::apic::IpiMode;
use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::marker;
use vibeos::per_cpu::PerCpu;
use vibeos::smp::{
    INIT_WAIT_MS, PARAM_CR3, PARAM_ENTRY, PARAM_IDT, PARAM_OFF, PARAM_STACK, READY_TIMEOUT_MS,
    SIPI_VECTOR, SIPI_WAIT_MS, TRAMPOLINE_PHYS, blob_fits, pack_idtr,
};
use vibeos::thread::ThreadId;

use crate::acpi_init;
use crate::apic_init;
use crate::arch;
use crate::arch::gdt::{self, ApTables, CpuTables};
use crate::cell::IrqCell;
use crate::kva_init;
use crate::per_cpu_init;
use crate::thread_init;
use crate::time_init;
use crate::x86;

unsafe extern "C" {
    static __trampoline_start: u8;
    static __trampoline_end: u8;
}

struct Starting {
    cpu: *mut PerCpu,
    cpu_tables: *mut CpuTables,
}

// SAFETY: the BSP hands the two pointers to the one AP `start_one` starts,
// before its SIPI, and the AP reads them once in `ap_entry`; established at
// `smp_init::start_one`, which starts one AP at a time and waits for its
// `ready` before writing `STARTING` again. Neither pointee is touched by
// the BSP while the AP owns it.
unsafe impl Send for Starting {}

impl Starting {
    const fn empty() -> Self {
        Self {
            cpu: core::ptr::null_mut(),
            cpu_tables: core::ptr::null_mut(),
        }
    }
}

static STARTING: IrqCell<Starting> = IrqCell::new(Starting::empty());
static LIVE_TABLES: IrqCell<Vec<ApTables>> = IrqCell::new(Vec::new());

struct ApAlloc {
    cpu_id: u32,
    apic_id: u8,
    tables: ApTables,
    /// The idle stack's top, read before the stack moves into its TCB.
    stack_top: u64,
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
    let src = core::ptr::addr_of!(__trampoline_start);
    let n = unsafe { core::ptr::addr_of!(__trampoline_end).offset_from(src) as usize };
    let n = if blob_fits(n) { n } else { PARAM_OFF };
    let dst = tramp_page();
    let mut i = 0;
    while i < n {
        unsafe { dst.add(i).write_volatile(src.add(i).read()) };
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
        Ok(s) => s,
        Err(_) => {
            gdt::free_ap_tables(tables);
            return None;
        }
    };
    let stack_top = stack.top().as_u64();
    let idle_id = match thread_init::adopt_ap_idle(cpu_id, stack) {
        Ok(id) => id,
        Err(stack) => {
            kva_init::free_stack(stack);
            gdt::free_ap_tables(tables);
            return None;
        }
    };
    let idle_ptr = thread_init::tcb_ptr(idle_id);
    if publish {
        // SAFETY: CPU `cpu_id` is not running, `with_cpu`'s contract,
        // established at `smp_init::start_one`: no INIT or SIPI has gone to
        // it yet (`start_one` sends them after this returns).
        let _ = unsafe {
            per_cpu_init::with_cpu(cpu_id, |cpu| {
                cpu.remote.apic_id.store(apic_id as u32, Ordering::Relaxed);
                cpu.idle_id = idle_id;
                cpu.idle = idle_ptr;
                per_cpu_init::set_current_thread(cpu, idle_ptr);
                cpu.tsc_per_ms = time_init::tsc_per_ms();
                cpu.timer_mode = apic_init::timer_mode();
                cpu.remote.ready.store(false, Ordering::Relaxed);
                core::sync::atomic::compiler_fence(Ordering::SeqCst);
            })
        };
    }
    Some(ApAlloc {
        cpu_id,
        apic_id,
        tables,
        stack_top,
        idle_id,
        published: publish,
    })
}

/// Undo [`alloc_ap_resources`]. `sipi_sent`: a SIPI went to the AP, which
/// may have accepted it and stalled past the ready timeout and may still
/// run on its slot (ROADMAP §11.4, F032), so its owner-only fields are left
/// alone; nothing reads an offline slot's owner-only fields.
fn free_ap_resources(a: ApAlloc, sipi_sent: bool) {
    if a.published {
        if let Some(r) = per_cpu_init::cpu(a.cpu_id) {
            r.ready.store(false, Ordering::Relaxed);
            r.apic_id.store(0, Ordering::Relaxed);
        }
        if !sipi_sent {
            // SAFETY: CPU `a.cpu_id` is not running, `with_cpu`'s contract,
            // established at `smp_init::start_one`: no SIPI went to it.
            let _ = unsafe {
                per_cpu_init::with_cpu(a.cpu_id, |cpu| {
                    cpu.idle = core::ptr::null_mut();
                    per_cpu_init::set_current_thread(cpu, core::ptr::null_mut());
                    cpu.idle_id = ThreadId::NONE;
                })
            };
        }
    }
    if let Some(stack) = thread_init::abandon_ap_idle(a.idle_id) {
        kva_init::free_stack(stack);
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
        crate::marker!("vibeOS: smp: cr3 above 4GiB");
        free_ap_resources(a, false);
        return false;
    }
    let stack_top = a.stack_top;
    let entry = ap_entry as *const () as usize as u64;
    let (idt_limit, idt_base) = arch::idt::pointer();

    let cpu_ptr = match per_cpu_init::slot_ptr(cpu_id) {
        Some(p) => p,
        None => {
            free_ap_resources(a, false);
            return false;
        }
    };
    let tables_ptr = a.tables.tables.as_ref() as *const CpuTables as *mut CpuTables;
    STARTING.with(|starting| {
        starting.cpu = cpu_ptr;
        starting.cpu_tables = tables_ptr;
    });
    core::sync::atomic::compiler_fence(Ordering::SeqCst);

    patch_params(cr3, stack_top, entry, idt_limit, idt_base);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    x86::mfence();

    if apic_init::send_ipi(apic_id, 0, IpiMode::Init).is_err() {
        crate::marker!("vibeOS: smp: INIT failed");
        free_ap_resources(a, false);
        return false;
    }
    time_init::busy_wait_ms(INIT_WAIT_MS);
    let _ = apic_init::send_ipi(apic_id, SIPI_VECTOR, IpiMode::Sipi);
    time_init::busy_wait_ms(SIPI_WAIT_MS);
    let _ = apic_init::send_ipi(apic_id, SIPI_VECTOR, IpiMode::Sipi);

    if !wait_ready(cpu_id) {
        crate::marker!("vibeOS: smp: apic {apic_id} timed out");
        free_ap_resources(a, true);
        return false;
    }

    let ApAlloc { tables, .. } = a;
    LIVE_TABLES.with(|live| live.push(tables));
    crate::marker!(marker::SMP_AP_ONLINE);
    true
}

extern "C" fn ap_entry() -> ! {
    x86::cli();
    // GS is still 0. IrqCell.with / InterruptGuard would `gs:[0]` via
    // try_current and triple-fault (IDT not loaded yet).
    let (cpu, tables_ptr) = unsafe {
        let st = &mut *STARTING.as_ptr();
        (st.cpu, st.cpu_tables)
    };
    let tables = unsafe { &mut *tables_ptr };
    let cpu = unsafe { &mut *cpu };
    // `mov gs` zeros the hidden base. GS_BASE before any lidt so NMI
    // cannot gs:[0] a null PerCpu (DESIGN §7.4 / ROADMAP).
    unsafe { tables.load() };
    unsafe { per_cpu_init::install_gs(cpu) };
    unsafe { crate::syscall_init::init_ap(tables.tss_ptr(), tables.rsp0()) };
    unsafe { arch::idt::load() };
    arch::cpu::harden();
    unsafe { apic_init::enable_ap() };
    cpu.tsc_per_ms = time_init::tsc_per_ms();
    cpu.timer_mode = apic_init::timer_mode();
    apic_init::arm_ap();
    per_cpu_init::mark_online(cpu.cpu_id);
    crate::marker!(
        "{}{}{}",
        marker::SCHED_CPU_PREFIX,
        cpu.cpu_id,
        marker::SCHED_CPU_SUFFIX
    );
    cpu.remote.ready.store(true, Ordering::Release);
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

    let bsp_apic = per_cpu_init::current()
        .remote
        .apic_id
        .load(Ordering::Relaxed) as u8;
    let Some(info) = acpi_init::info() else {
        crate::marker!(marker::SMP_DONE);
        return;
    };
    let Some(madt) = info.madt.as_ref() else {
        crate::marker!(marker::SMP_DONE);
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
            crate::marker!("vibeOS: smp: apic {apic_id} alloc failed");
            logical += 1;
            continue;
        };
        let _ = start_one(alloc);
        logical += 1;
    }

    crate::marker!(marker::SMP_DONE);
}

/// Allocate the same resources as bring-up, then take the timeout free
/// path. Frame count must return to baseline (injectable fault).
#[cfg(feature = "kernel_tests")]
pub fn exercise_fail_cleanup() {
    if let Some(a) = alloc_ap_resources(0xFE, 0xFE, false) {
        free_ap_resources(a, false);
    }
}

#[cfg(feature = "kernel_tests")]
pub fn trampoline_installed() -> bool {
    unsafe { tramp_page().read_volatile() == 0xFA }
}
