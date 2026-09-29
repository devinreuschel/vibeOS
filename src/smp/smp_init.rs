//! AP trampoline install and bring-up. ROADMAP §4.4–4.5, DESIGN §7.3–7.4.
//!
//! One AP at a time: they share the trampoline page and its param block.
//! The page is the one `boot::capture` chose from the memory map (DESIGN
//! §7.3); the BSP writes it only through the physmap, with
//! `write_volatile` + `compiler_fence(SeqCst)` before SIPI.

use core::sync::atomic::Ordering;

use vibeos::apic::IpiMode;
use vibeos::kalloc::TryVec;
use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::marker;
use vibeos::paging::HHDM_BASE;
use vibeos::per_cpu::PerCpu;
use vibeos::smp::{
    INIT_WAIT_MS, PARAM_CR3, PARAM_ENTRY, PARAM_IDT, PARAM_OFF, PARAM_STACK, PATCH_SITES,
    READY_TIMEOUT_MS, SIPI_WAIT_MS, blob_fits, pack_idtr, patch_blob, sipi_vector,
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
    // `trampoline.S`'s patch labels, in `PATCH_SITES` order.
    static vibeos_tramp_patch_pm32: u8;
    static vibeos_tramp_patch_cr3: u8;
    static vibeos_tramp_patch_lm64: u8;
    static vibeos_tramp_patch_gdt: u8;
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
/// Each online AP's GDT, TSS and IST stacks, for as long as it runs on
/// them. `start_one` reserves the slot before INIT and moves the tables in
/// before INIT too, so nothing allocates, fails or drops them once the AP
/// runs on them (DESIGN §4.4).
static LIVE_TABLES: IrqCell<TryVec<ApTables>> = IrqCell::new(TryVec::new());

pub(super) struct ApAlloc {
    cpu_id: u32,
    apic_id: u8,
    tables: ApTables,
    /// The idle stack's top, read before the stack moves into its TCB.
    stack_top: u64,
    idle_id: ThreadId,
    published: bool,
}

/// The trampoline page `page`'s physmap address, through which the BSP
/// writes it (the AP reaches it at its identity address).
pub(super) fn tramp_va(page: u64) -> *mut u8 {
    HHDM_BASE.wrapping_add(page) as *mut u8
}

/// Store `val` at byte `off` of trampoline page `page`.
///
/// # Safety
/// `page` is `BootInfo.trampoline_page`, `off + 8` is at most the page
/// size, and no AP runs on the trampoline page: `start_one` starts one AP
/// at a time and writes the page only before that AP's INIT.
unsafe fn write_u64(page: u64, off: usize, val: u64) {
    // SAFETY: the physmap maps the trampoline page writable (invariant I14,
    // established at `mm::paging_init::install`: the page is usable RAM
    // below 1 MiB), the buddy never hands it out (invariant I15), and this
    // fn's `# Safety` contract keeps the store inside it with no AP reading
    // it; established here.
    unsafe {
        tramp_va(page).add(off).cast::<u64>().write_volatile(val);
    }
}

/// Whether `trampoline.S`'s exported patch labels sit at `PATCH_SITES`.
fn patch_labels_match() -> bool {
    let start = core::ptr::addr_of!(__trampoline_start) as usize;
    let labels = [
        core::ptr::addr_of!(vibeos_tramp_patch_pm32) as usize,
        core::ptr::addr_of!(vibeos_tramp_patch_cr3) as usize,
        core::ptr::addr_of!(vibeos_tramp_patch_lm64) as usize,
        core::ptr::addr_of!(vibeos_tramp_patch_gdt) as usize,
    ];
    labels.len() == PATCH_SITES.len()
        && labels
            .iter()
            .zip(PATCH_SITES)
            .all(|(&l, &at)| l.wrapping_sub(start) == at)
}

/// Copy the blob into a local buffer, rebase it onto `page`
/// (`vibeos::smp::patch_blob`), and write it to the page through the
/// physmap. False when the blob cannot be rebased there.
fn install_blob(page: u64) -> bool {
    // Kernel invariant: `.org` pins each patched operand in `trampoline.S`
    // at its `PATCH_SITES` offset.
    assert!(
        patch_labels_match(),
        "smp: trampoline patch labels differ from PATCH_SITES"
    );
    let src = core::ptr::addr_of!(__trampoline_start);
    // SAFETY: both symbols bound the one `.trampoline` section the linker
    // script places in the kernel image, end after start
    // (`smp::smp_init::__trampoline_start`).
    let n = unsafe { core::ptr::addr_of!(__trampoline_end).offset_from(src) as usize };
    let n = if blob_fits(n) { n } else { PARAM_OFF };
    let mut blob = [0u8; PARAM_OFF];
    for (i, b) in blob.iter_mut().enumerate().take(n) {
        // SAFETY: `i < n` stays inside the blob, which the kernel map covers
        // (`smp::smp_init::__trampoline_start`); established here.
        *b = unsafe { src.add(i).read() };
    }
    let Some(blob) = blob.get_mut(..n) else {
        return false;
    };
    if patch_blob(blob, page).is_err() {
        return false;
    }
    let dst = tramp_va(page);
    for (i, &b) in blob.iter().enumerate() {
        // SAFETY: `i < n <= PARAM_OFF`, inside trampoline page `page`,
        // which the physmap maps writable (invariant I14, established at
        // `mm::paging_init::install`) and the buddy never hands out
        // (invariant I15); `init` runs before any AP starts.
        unsafe { dst.add(i).write_volatile(b) };
    }
    true
}

fn patch_params(page: u64, cr3: u64, stack_top: u64, entry: u64, idt_limit: u16, idt_base: u64) {
    // SAFETY: `write_u64`'s contract; `page` is the trampoline page, each
    // `PARAM_*` offset plus 8 lies inside it, and `start_one` patches before
    // this AP's INIT with no other AP starting (`smp::smp_init::start_one`).
    unsafe {
        write_u64(page, PARAM_CR3, cr3);
        write_u64(page, PARAM_STACK, stack_top);
        write_u64(page, PARAM_ENTRY, entry);
    }
    let packed = pack_idtr(idt_limit, idt_base);
    for (i, &b) in packed.iter().enumerate() {
        // SAFETY: as for `write_u64`: `PARAM_IDT + i` is below
        // `PARAM_IDT + PARAM_IDT_LEN`, inside the trampoline page, which the
        // physmap maps writable (invariant I14), before the AP's INIT
        // (`smp::smp_init::start_one`).
        unsafe { tramp_va(page).add(PARAM_IDT + i).write_volatile(b) };
    }
}

pub(super) fn alloc_ap_resources(cpu_id: u32, apic_id: u8, publish: bool) -> Option<ApAlloc> {
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
        // it yet (`start_one` sends them after this returns). The `Option`
        // discarded carries no failure: `None` means no slot, which
        // `start_one`'s `slot_ptr` check turns into the alloc-failed path.
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
pub(super) fn free_ap_resources(a: ApAlloc, sipi_sent: bool) {
    let ApAlloc {
        cpu_id,
        tables,
        idle_id,
        published,
        ..
    } = a;
    free_ap_slot(cpu_id, idle_id, published, sipi_sent);
    gdt::free_ap_tables(tables);
}

/// [`free_ap_resources`] but for the tables, for a `start_one` failure
/// after the tables moved into `LIVE_TABLES`.
fn free_ap_slot(cpu_id: u32, idle_id: ThreadId, published: bool, sipi_sent: bool) {
    if published {
        if let Some(r) = per_cpu_init::cpu(cpu_id) {
            r.ready.store(false, Ordering::Relaxed);
            r.apic_id.store(0, Ordering::Relaxed);
        }
        if !sipi_sent {
            // SAFETY: CPU `cpu_id` is not running, `with_cpu`'s contract,
            // established at `smp_init::start_one`: no SIPI went to it. The
            // `Option` discarded carries no failure: `None` means no slot,
            // so there is nothing to clear.
            let _ = unsafe {
                per_cpu_init::with_cpu(cpu_id, |cpu| {
                    cpu.idle = core::ptr::null_mut();
                    per_cpu_init::set_current_thread(cpu, core::ptr::null_mut());
                    cpu.idle_id = ThreadId::NONE;
                })
            };
        }
    }
    if let Some(stack) = thread_init::abandon_ap_idle(idle_id) {
        kva_init::free_stack(stack);
    }
}

/// Take the tables `start_one` moved into `LIVE_TABLES` back out and free
/// them with the rest of the AP's resources.
fn free_live_ap(cpu_id: u32, idle_id: ThreadId, published: bool, sipi_sent: bool) {
    // `start_one` pushed this AP's tables last, and only it pushes, one AP
    // at a time; `None` would mean there is nothing to free.
    let tables = LIVE_TABLES.with(|live| live.pop());
    free_ap_slot(cpu_id, idle_id, published, sipi_sent);
    if let Some(t) = tables {
        gdt::free_ap_tables(t);
    }
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

/// Start the AP `a` describes from trampoline page `page` and wait for it.
/// Every failure prints its line and frees what `a` holds.
fn start_one(a: ApAlloc, page: u64) {
    let cpu_id = a.cpu_id;
    let apic_id = a.apic_id;
    let cr3 = x86::read_cr3();
    if cr3 > 0xFFFF_FFFF {
        crate::marker!("vibeOS: smp: cr3 above 4GiB");
        free_ap_resources(a, false);
        return;
    }
    let stack_top = a.stack_top;
    let entry = ap_entry as *const () as usize as u64;
    let (idt_limit, idt_base) = arch::idt::pointer();

    let cpu_ptr = match per_cpu_init::slot_ptr(cpu_id) {
        Some(p) => p,
        None => {
            crate::marker!("vibeOS: smp: apic {apic_id} alloc failed");
            free_ap_resources(a, false);
            return;
        }
    };
    // The one allocation bring-up makes after `irq: enabled`: reserve the
    // slot the tables move into now, while a failure can still free them.
    if LIVE_TABLES.with(|live| live.try_reserve(1)).is_err() {
        crate::marker!("vibeOS: smp: apic {apic_id} alloc failed");
        free_ap_resources(a, false);
        return;
    }
    let ApAlloc {
        tables,
        idle_id,
        published,
        ..
    } = a;
    // The `TryBox` keeps `CpuTables` in place when `tables` moves.
    let tables_ptr = &*tables.tables as *const CpuTables as *mut CpuTables;
    // Room for one is reserved above, so this push allocates nothing and
    // cannot fail; it moves the tables before INIT, while no AP runs on
    // them.
    if LIVE_TABLES.with(|live| live.try_push(tables)).is_err() {
        crate::marker!("vibeOS: smp: apic {apic_id} alloc failed");
        free_ap_slot(cpu_id, idle_id, published, false);
        return;
    }
    STARTING.with(|starting| {
        starting.cpu = cpu_ptr;
        starting.cpu_tables = tables_ptr;
    });
    core::sync::atomic::compiler_fence(Ordering::SeqCst);

    patch_params(page, cr3, stack_top, entry, idt_limit, idt_base);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    x86::mfence();

    if apic_init::send_ipi(apic_id, 0, IpiMode::Init).is_err() {
        crate::marker!("vibeOS: smp: INIT failed");
        free_live_ap(cpu_id, idle_id, published, false);
        return;
    }
    time_init::busy_wait_ms(INIT_WAIT_MS);
    // `init` checked the page with `install_blob`, which `patch_blob`
    // refuses for any page `sipi_vector` does.
    let vector = sipi_vector(page).unwrap_or(0);
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5 bounded retry: the second SIPI retries the first, and wait_ready gives up after READY_TIMEOUT_MS with the smp: apic N timed out line and frees the AP"
    )]
    let _ = apic_init::send_ipi(apic_id, vector, IpiMode::Sipi);
    time_init::busy_wait_ms(SIPI_WAIT_MS);
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5 bounded retry: the second SIPI retries the first, and wait_ready gives up after READY_TIMEOUT_MS with the smp: apic N timed out line and frees the AP"
    )]
    let _ = apic_init::send_ipi(apic_id, vector, IpiMode::Sipi);

    if !wait_ready(cpu_id) {
        crate::marker!("vibeOS: smp: apic {apic_id} timed out");
        free_live_ap(cpu_id, idle_id, published, true);
        return;
    }

    crate::marker!(marker::SMP_AP_ONLINE);
}

extern "C" fn ap_entry() -> ! {
    x86::cli();
    // GS is still 0. IrqCell.with / InterruptGuard would `gs:[0]` via
    // try_current and triple-fault (IDT not loaded yet).
    // SAFETY: the BSP wrote `STARTING` before this AP's INIT and touches it
    // again only after this AP stores `ready` (`smp::smp_init::start_one`,
    // one AP at a time), so no `&mut` to it is live.
    let (cpu, tables_ptr) = unsafe {
        let st = &mut *STARTING.as_ptr();
        (st.cpu, st.cpu_tables)
    };
    // SAFETY: `tables_ptr` is this AP's `CpuTables`, boxed in the
    // `ApTables` that `LIVE_TABLES` holds for good, and no other CPU touches
    // it (`smp::smp_init::start_one`).
    let tables = unsafe { &mut *tables_ptr };
    // SAFETY: `cpu` is this AP's `PerCpu` slot (`per_cpu_init::slot_ptr`);
    // the BSP's `with_cpu` scope on it ended before INIT, and from here this
    // AP is its one owner (invariants I120 and I21, established at
    // `smp::smp_init::start_one`).
    let cpu = unsafe { &mut *cpu };
    // `mov gs` zeros the hidden base. GS_BASE before any lidt so NMI
    // cannot gs:[0] a null PerCpu (DESIGN §7.4 / ROADMAP).
    // SAFETY: `CpuTables::load`'s contract; `tables` is this CPU's, and
    // IF=0 since the `cli` above; established here.
    unsafe { tables.load() };
    // SAFETY: `install_gs`'s contract; `cpu` is this CPU's `PerCpu`, the
    // GDT load above did the `mov gs`, and IF stays 0 until the `sti` below,
    // so no ISR reads `gs:[0]` first (invariant I4, established here).
    unsafe { per_cpu_init::install_gs(cpu) };
    // `init_ap` writes this CPU's CR0 and CR4 (`arch::cpu::init_control_regs`).
    // SAFETY: `init_ap`'s contract; the TSS `tables.load` just loaded is this
    // CPU's live TSS, and `rsp0` its kernel stack top; established here.
    unsafe { crate::syscall_init::init_ap(tables.tss_ptr(), tables.rsp0()) };
    // SAFETY: `idt::load`'s contract; the BSP filled the shared IDT at boot,
    // before `smp_init::init` runs, and the GDT above matches KERNEL_CS and
    // the IST TSS; established here.
    unsafe { arch::idt::load() };
    // SAFETY: `enable_ap`'s contract; the BSP mapped the LAPIC page UC at
    // boot, and IF is 0; established here.
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

/// Bring up every enabled MADT CPU except the BSP from the trampoline page
/// `boot::capture` chose, or none when it chose none. Emits `smp: done`.
///
/// # Safety
/// Scheduler live, LAPIC ready, trampoline page identity-mapped and
/// excluded from the PMM.
pub unsafe fn init() {
    let keep = match crate::boot::info().trampoline_page {
        Some(page) if install_blob(page) => {
            crate::marker!("vibeOS: smp: trampoline page {page:#x}");
            core::sync::atomic::compiler_fence(Ordering::SeqCst);
            start_aps(page);
            Some(page)
        }
        _ => {
            crate::marker!("vibeOS: smp: no trampoline page");
            None
        }
    };
    crate::marker!(marker::SMP_DONE);
    // ROADMAP §10.6: the low identity window goes, all but the trampoline
    // page. Kernel invariant: boot runs on the bootstrap thread's KVA stack
    // (`thread_init::init_bootstrap`), outside the window.
    let window = crate::paging_init::identity_window();
    let rsp = x86::read_rsp();
    assert!(
        !window.contains(&rsp),
        "smp: rsp {rsp:#x} in the identity window"
    );
    let stack = thread_init::bootstrap_stack().map(|(r, _, _)| r);
    assert!(
        stack
            .as_ref()
            .is_some_and(|r| r.end <= window.start || r.start >= window.end),
        "smp: bootstrap stack {stack:#x?} not outside the identity window"
    );
    // SAFETY: every AP is up or abandoned (`start_aps` returned), nothing
    // uses an identity address but the trampoline page, which `keep`
    // keeps, and this CPU's stack is outside the window (checked above);
    // established here.
    unsafe { crate::paging_init::teardown_identity(keep) };
}

/// Start each MADT CPU but the BSP, one at a time, from `page`.
fn start_aps(page: u64) {
    let bsp_apic = per_cpu_init::current()
        .remote
        .apic_id
        .load(Ordering::Relaxed) as u8;
    let Some(info) = acpi_init::info() else {
        return;
    };
    let Some(madt) = info.madt.as_ref() else {
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
        start_one(alloc, page);
        logical += 1;
    }
}
