//! AP trampoline install and bring-up. ROADMAP §4.4–4.5, DESIGN §7.3–7.4.
//!
//! One AP at a time: they share the trampoline page and its param block.
//! The page is the one `boot::capture` chose from the memory map (DESIGN
//! §7.3); the BSP writes it only through the physmap, with
//! `write_volatile` + `compiler_fence(SeqCst)` before SIPI.

#[cfg(target_arch = "x86_64")]
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering;

#[cfg(target_arch = "x86_64")]
use vibeos::apic::IpiMode;
#[cfg(target_arch = "x86_64")]
use vibeos::arch::CycleCounter;
use vibeos::kalloc::TryVec;
use vibeos::kva::DEFAULT_STACK_PAGES;
#[cfg(target_arch = "x86_64")]
use vibeos::log::trace::{ClockInfo, WARP_MAX_ITERS, WARP_MS, WarpLine};
use vibeos::marker;
use vibeos::per_cpu::PerCpu;
#[cfg(target_arch = "x86_64")]
use vibeos::smp::{
    INIT_WAIT_MS, PARAM_CR3, PARAM_ENTRY, PARAM_OFF, PARAM_STACK, PATCH_SITES, READY_TIMEOUT_MS,
    SIPI_WAIT_MS, blob_fits, patch_blob, sipi_vector,
};
use vibeos::thread::ThreadId;

use crate::apic_init;
use crate::arch;
#[cfg(target_arch = "x86_64")]
use crate::arch::current::Arch;
use crate::arch::gdt::{self, ApTables, CpuTables};
use crate::cell::IrqCell;
use crate::kva_init;
#[cfg(target_arch = "x86_64")]
use crate::log::trace_init;
use crate::machine_init;
use crate::per_cpu_init;
use crate::thread_init::{self, SpawnError};
use crate::time_init;
use crate::work_init::{self, CpuWorkers};
#[cfg(target_arch = "x86_64")]
use crate::x86;
#[cfg(target_arch = "aarch64")]
use vibeos::smp::READY_TIMEOUT_MS;

#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    static __trampoline_start: u8;
    static __trampoline_end: u8;
    // `trampoline.S`'s patch labels, in `PATCH_SITES` order.
    static vibeos_tramp_patch_pm32: u8;
    static vibeos_tramp_patch_cr3: u8;
    static vibeos_tramp_patch_lm64: u8;
    static vibeos_tramp_patch_gdt: u8;
}

#[cfg(target_arch = "aarch64")]
mod tramp_absent {
    #[unsafe(no_mangle)]
    pub(super) static __trampoline_start: u8 = 0;
    #[unsafe(no_mangle)]
    pub(super) static __trampoline_end: u8 = 0;
    #[unsafe(no_mangle)]
    pub(super) static vibeos_tramp_patch_pm32: u8 = 0;
    #[unsafe(no_mangle)]
    pub(super) static vibeos_tramp_patch_cr3: u8 = 0;
    #[unsafe(no_mangle)]
    pub(super) static vibeos_tramp_patch_lm64: u8 = 0;
    #[unsafe(no_mangle)]
    pub(super) static vibeos_tramp_patch_gdt: u8 = 0;
}

#[cfg(target_arch = "aarch64")]
#[expect(unused_imports, reason = "x86 trampoline symbols; aarch64 has no SIPI")]
use tramp_absent::{
    __trampoline_end, __trampoline_start, vibeos_tramp_patch_cr3, vibeos_tramp_patch_gdt,
    vibeos_tramp_patch_lm64, vibeos_tramp_patch_pm32,
};

#[cfg_attr(
    target_arch = "aarch64",
    expect(dead_code, reason = "x86-only on the boot-CPU slice")
)]
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
    #[cfg_attr(
        target_arch = "aarch64",
        expect(dead_code, reason = "x86-only on the boot-CPU slice")
    )]
    const fn empty() -> Self {
        Self {
            cpu: core::ptr::null_mut(),
            cpu_tables: core::ptr::null_mut(),
        }
    }
}

#[cfg_attr(
    target_arch = "aarch64",
    expect(dead_code, reason = "x86-only on the boot-CPU slice")
)]
static STARTING: IrqCell<Starting> = IrqCell::new(Starting::empty());
/// Each online AP's GDT, TSS and IST stacks, for as long as it runs on
/// them. `start_one` reserves the slot before INIT and moves the tables in
/// before INIT too, so nothing allocates, fails or drops them once the AP
/// runs on them (DESIGN §4.4).
static LIVE_TABLES: IrqCell<TryVec<ApTables>> = IrqCell::new(TryVec::new());

#[cfg_attr(
    target_arch = "aarch64",
    expect(dead_code, reason = "x86-only on the boot-CPU slice")
)]
pub(crate) struct ApAlloc {
    cpu_id: u32,
    apic_id: u8,
    #[cfg_attr(
        target_arch = "x86_64",
        expect(dead_code, reason = "aarch64 uses the MPIDR; x86 uses apic_id")
    )]
    hw_id: u64,
    tables: ApTables,
    /// The idle stack's top, read before the stack moves into its TCB.
    stack_top: u64,
    idle_id: ThreadId,
    /// The AP's per-CPU workers, parked until it is online.
    workers: CpuWorkers,
    published: bool,
}

/// The trampoline page `page`'s physmap address, through which the BSP
/// writes it (the AP reaches it at its identity address).
#[cfg_attr(
    target_arch = "aarch64",
    expect(dead_code, reason = "x86-only on the boot-CPU slice")
)]
pub(super) fn tramp_va(page: u64) -> *mut u8 {
    crate::paging_init::hhdm_offset().wrapping_add(page) as *mut u8
}

/// Store `val` at byte `off` of trampoline page `page`.
///
/// # Safety
/// `page` is `BootInfo.trampoline_page`, `off + 8` is at most the page
/// size, and no AP runs on the trampoline page: `start_one` starts one AP
/// at a time and writes the page only before that AP's INIT.
#[cfg(target_arch = "x86_64")]
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
#[cfg(target_arch = "x86_64")]
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
#[cfg(target_arch = "x86_64")]
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

#[cfg(target_arch = "x86_64")]
fn patch_params(page: u64, cr3: u64, stack_top: u64, entry: u64) {
    // SAFETY: `write_u64`'s contract; `page` is the trampoline page, each
    // `PARAM_*` offset plus 8 lies inside it, and `start_one` patches before
    // this AP's INIT with no other AP starting (`smp::smp_init::start_one`).
    unsafe {
        write_u64(page, PARAM_CR3, cr3);
        write_u64(page, PARAM_STACK, stack_top);
        write_u64(page, PARAM_ENTRY, entry);
    }
}

/// Everything AP `cpu_id` needs before it starts: its GDT, TSS and IST
/// stacks, its idle thread on a fresh stack, and its per-CPU workers,
/// parked. On any failure it releases what it took and returns `Err`, with
/// the `SpawnError` when a thread found no slot or no memory; the CPU then
/// stays offline (ROADMAP §10.4, F037).
pub(crate) fn alloc_ap_resources(
    cpu_id: u32,
    hw_id: u64,
    publish: bool,
) -> Result<ApAlloc, Option<SpawnError>> {
    let apic_id = hw_id as u8;
    let tables = gdt::alloc_ap_tables().ok_or(None)?;
    let stack = match kva_init::alloc_guarded_stack(DEFAULT_STACK_PAGES) {
        Ok(s) => s,
        Err(_) => {
            gdt::free_ap_tables(tables);
            return Err(None);
        }
    };
    let stack_top = stack.top().as_u64();
    let idle_id = match thread_init::adopt_ap_idle(cpu_id, stack) {
        Ok(id) => id,
        Err(stack) => {
            kva_init::free_stack(stack);
            gdt::free_ap_tables(tables);
            return Err(Some(SpawnError::NoSlot));
        }
    };
    let workers = match work_init::spawn_cpu_workers(cpu_id) {
        Ok(w) => w,
        Err(e) => {
            if let Some(stack) = thread_init::abandon_unstarted(idle_id, false) {
                kva_init::free_stack(stack);
            }
            gdt::free_ap_tables(tables);
            return Err(Some(e));
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
                // Relaxed: set before the CPU starts, fixed while it runs; pairs with nothing.
                cpu.remote.apic_id.store(hw_id as u32, Ordering::Relaxed);
                cpu.idle_id = idle_id;
                cpu.idle = idle_ptr;
                per_cpu_init::set_current_thread(cpu, idle_ptr);
                cpu.timer_mode = apic_init::timer_mode();
                // Relaxed: the AP is not running yet; pairs with nothing.
                cpu.remote.ready.store(false, Ordering::Relaxed);
                core::sync::atomic::compiler_fence(Ordering::SeqCst);
            })
        };
    }
    Ok(ApAlloc {
        cpu_id,
        apic_id,
        hw_id,
        tables,
        stack_top,
        idle_id,
        workers,
        published: publish,
    })
}

/// Undo [`alloc_ap_resources`]. `sipi_sent`: a SIPI went to the AP, which
/// may have accepted it and stalled past the ready timeout and may still
/// run on its slot (ROADMAP §11.4, F032), so its owner-only fields are left
/// alone; nothing reads an offline slot's owner-only fields.
pub(crate) fn free_ap_resources(a: ApAlloc, sipi_sent: bool) {
    let ApAlloc {
        cpu_id,
        tables,
        idle_id,
        workers,
        published,
        ..
    } = a;
    free_ap_slot(cpu_id, idle_id, workers, published, sipi_sent);
    gdt::free_ap_tables(tables);
}

/// [`free_ap_resources`] but for the tables, for a `start_one` failure
/// after the tables moved into `LIVE_TABLES`.
fn free_ap_slot(
    cpu_id: u32,
    idle_id: ThreadId,
    workers: CpuWorkers,
    published: bool,
    sipi_sent: bool,
) {
    if published {
        if let Some(r) = per_cpu_init::cpu(cpu_id) {
            // Relaxed: the AP never started; pairs with nothing.
            r.ready.store(false, Ordering::Relaxed);
            // Relaxed: the AP never started; pairs with nothing.
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
    // The workers were never made ready, whatever the AP did.
    work_init::abandon_cpu_workers(workers);
    if let Some(stack) = thread_init::abandon_unstarted(idle_id, sipi_sent) {
        kva_init::free_stack(stack);
    }
}

/// Take the tables `start_one` moved into `LIVE_TABLES` back out and free
/// them with the rest of the AP's resources.
fn free_live_ap(
    cpu_id: u32,
    idle_id: ThreadId,
    workers: CpuWorkers,
    published: bool,
    sipi_sent: bool,
) {
    // `start_one` pushed this AP's tables last, and only it pushes, one AP
    // at a time; `None` would mean there is nothing to free.
    let tables = LIVE_TABLES.with(|live| live.pop());
    free_ap_slot(cpu_id, idle_id, workers, published, sipi_sent);
    if let Some(t) = tables {
        gdt::free_ap_tables(t);
    }
}

#[cfg(target_arch = "x86_64")]
fn wait_ready(cpu_id: u32) -> bool {
    let mut ms = 0u64;
    while ms < READY_TIMEOUT_MS {
        // Acquire: pairs with the Release store in `ap_main`.
        if per_cpu_init::cpu(cpu_id).is_some_and(|c| c.ready.load(Ordering::Acquire)) {
            return true;
        }
        time_init::busy_wait_ms(1);
        ms += 1;
    }
    false
}

/// The line the TSC warp test shares between the BSP and the AP it is
/// starting (DESIGN §7.4).
#[cfg(target_arch = "x86_64")]
static WARP: WarpLine = WarpLine::new();
/// APs whose warp test met the BSP.
#[cfg(target_arch = "x86_64")]
static WARP_RUNS: AtomicU32 = AtomicU32::new(0);

/// The warp test's barrier timeout and run span, in TSC cycles, or `None`
/// before the TSC is calibrated, when neither side runs it.
#[cfg(target_arch = "x86_64")]
fn warp_budget() -> Option<(u64, u64)> {
    let per_ms = time_init::tsc_per_ms();
    if per_ms == 0 {
        return None;
    }
    Some((
        per_ms.checked_mul(READY_TIMEOUT_MS)?,
        per_ms.checked_mul(WARP_MS)?,
    ))
}

/// The BSP's side of the warp test against the AP it just sent SIPIs.
/// Runs with IF=1, never inside an `InterruptGuard`: an interrupt only
/// delays a read and cannot fake a backward step.
#[cfg(target_arch = "x86_64")]
fn tsc_warp_source() {
    let Some((timeout, span)) = warp_budget() else {
        return;
    };
    if WARP.arrive(Arch::now, timeout) {
        time_init::note_tsc_warp(WARP.run(Arch::now, span, WARP_MAX_ITERS));
        // Relaxed: a count; pairs with nothing.
        WARP_RUNS.fetch_add(1, Ordering::Relaxed);
        if !WARP.wait_left(Arch::now, timeout) {
            crate::klog!(
                vibeos::log::Level::Warn,
                "vibeOS: smp: tsc warp: ap did not leave"
            );
        }
    }
    WARP.reset();
}

/// The AP's side of the warp test, with IF=0 before its first `sti`
/// (at most `WARP_MAX_ITERS` iterations). Alone past the timeout, it skips.
#[cfg(target_arch = "x86_64")]
fn tsc_warp_target() {
    let Some((timeout, span)) = warp_budget() else {
        return;
    };
    if WARP.arrive(Arch::now, timeout) {
        time_init::note_tsc_warp(WARP.run(Arch::now, span, WARP_MAX_ITERS));
        WARP.leave();
    }
}

/// The skew marker, once an AP ran the warp test, and the clock the core
/// tool reads from the trace's header.
#[cfg(target_arch = "x86_64")]
fn report_tsc_warp() {
    // Relaxed: a count; pairs with nothing.
    let runs = WARP_RUNS.load(Ordering::Relaxed);
    if runs > 0 {
        crate::marker!("vibeOS: smp: tsc skew {} cycles", time_init::tsc_max_skew());
    }
    if !time_init::tsc_warp_ok() {
        crate::klog!(
            vibeos::log::Level::Warn,
            "vibeOS: smp: tsc warp: traces order per cpu"
        );
    }
    trace_init::publish_clock(&ClockInfo {
        freq_hz: Arch::freq_hz().unwrap_or(0),
        invariant: time_init::tsc_invariant(),
        warp_measured: runs > 0,
        max_skew: time_init::tsc_max_skew(),
    });
}

/// Start the AP `a` describes from trampoline page `page` and wait for it.
/// Every failure prints its line and frees what `a` holds.
#[cfg(target_arch = "x86_64")]
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
        workers,
        published,
        ..
    } = a;
    // The heap block `alloc_ap_tables` leaked keeps `CpuTables` in place
    // when `tables` moves, and its pointer keeps the allocation's
    // provenance.
    let tables_ptr = tables.tables.as_ptr();
    // Room for one is reserved above, so this push allocates nothing and
    // cannot fail; it moves the tables before INIT, while no AP runs on
    // them.
    if LIVE_TABLES.with(|live| live.try_push(tables)).is_err() {
        crate::marker!("vibeOS: smp: apic {apic_id} alloc failed");
        free_ap_slot(cpu_id, idle_id, workers, published, false);
        return;
    }
    STARTING.with(|starting| {
        starting.cpu = cpu_ptr;
        starting.cpu_tables = tables_ptr;
    });
    core::sync::atomic::compiler_fence(Ordering::SeqCst);

    patch_params(page, cr3, stack_top, entry);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    x86::mfence();

    if apic_init::send_ipi(apic_id, 0, IpiMode::Init).is_err() {
        crate::marker!("vibeOS: smp: INIT failed");
        free_live_ap(cpu_id, idle_id, workers, published, false);
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

    tsc_warp_source();
    if !wait_ready(cpu_id) {
        crate::marker!("vibeOS: smp: apic {apic_id} timed out");
        #[expect(
            clippy::let_underscore_must_use,
            reason = "DESIGN §2.5: INIT parks a stalled AP; its frames are leaked (F032)"
        )]
        let _ = apic_init::send_ipi(apic_id, 0, IpiMode::Init);
        time_init::busy_wait_ms(INIT_WAIT_MS);
        per_cpu_init::mark_offline(cpu_id);
        note_stalled_leak();
        // F032: the AP may still run on these; do not abandon them.
        // CpuWorkers is ThreadId-only and has no Drop.
        let _ = (idle_id, workers, published);
        return;
    }

    work_init::start_cpu_workers(&workers);
    crate::marker!(marker::SMP_AP_ONLINE);
}

#[cfg(target_arch = "x86_64")]
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
    // SAFETY: `tables_ptr` is this AP's `CpuTables`, the heap block of the
    // `ApTables` that `LIVE_TABLES` holds for good, which `init` filled
    // before INIT; this AP only reads it until `set_rsp0` writes through
    // the `UnsafeCell`. Established at `smp::smp_init::start_one`.
    let tables = unsafe { &*tables_ptr };
    // SAFETY: `cpu` is this AP's `PerCpu` slot (`per_cpu_init::slot_ptr`);
    // the BSP's `with_cpu` scope on it ended before INIT, and from here this
    // AP is its one owner (invariants I43 and I21, established at
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
    // SAFETY: `init_ap`'s contract; `tables_ptr` is the `CpuTables`
    // `tables.load` just loaded on this CPU, live while it runs, and `rsp0`
    // its kernel stack top; established here.
    unsafe { crate::syscall_init::init_ap(tables_ptr, tables.rsp0()) };
    // SAFETY: `idt::load`'s contract; the BSP filled the shared IDT at boot,
    // before `smp_init::init` runs, and the GDT above matches KERNEL_CS and
    // the IST TSS; established here.
    unsafe { arch::idt::load() };
    // SAFETY: `enable_ap`'s contract; the BSP mapped the LAPIC page UC at
    // boot, and IF is 0; established here.
    unsafe { apic_init::enable_ap() };
    cpu.timer_mode = apic_init::timer_mode();
    apic_init::arm_ap();
    tsc_warp_target();
    maybe_stall_ap();
    per_cpu_init::mark_online(cpu.cpu_id);
    crate::marker!(
        "{}{}{}",
        marker::SCHED_CPU_PREFIX,
        cpu.cpu_id,
        marker::SCHED_CPU_SUFFIX
    );
    // Release: pairs with the Acquire load in `wait_ready`.
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
    #[cfg(target_arch = "aarch64")]
    {
        init_aarch64();
        crate::marker!(marker::SMP_DONE);
    }
    #[cfg(target_arch = "x86_64")]
    {
        #[cfg(feature = "kernel_tests")]
        arm_stall_from_cmdline();
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
        report_tsc_warp();
        crate::marker!(marker::SMP_DONE);
        // ROADMAP §10.6: the low identity window goes, all but the trampoline
        // page. Kernel invariant: boot runs on the bootstrap thread's KVA stack
        // (`thread_init::init_bootstrap`), outside the window.
        let window = crate::paging_init::identity_window();
        let rsp = crate::arch::current::stack_pointer();
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
}

/// Start each MADT CPU but the BSP, one at a time, from `page`.
#[cfg(target_arch = "x86_64")]
fn start_aps(page: u64) {
    // Relaxed: set before the CPU starts, fixed while it runs; pairs with nothing.
    let bsp_apic = per_cpu_init::with_current(|c| c.remote.apic_id.load(Ordering::Relaxed)) as u8;
    let Some(desc) = machine_init::info() else {
        return;
    };

    let mut logical = 1u32;
    let mut i = 0usize;
    while i < desc.cpu_count() {
        let Some(cpu) = desc.cpus().get(i) else {
            break;
        };
        let apic_id = cpu.hw_id as u8;
        i += 1;
        if apic_id == bsp_apic {
            continue;
        }
        if logical as usize >= per_cpu_init::cpu_count() {
            break;
        }
        let alloc = match alloc_ap_resources(logical, cpu.hw_id, true) {
            Ok(a) => a,
            Err(e) => {
                crate::marker!("vibeOS: smp: apic {apic_id} alloc failed");
                if let Some(e) = e {
                    crate::klog!(
                        vibeos::log::Level::Warn,
                        "smp: apic {apic_id} stays offline: {}",
                        e.as_str()
                    );
                }
                logical += 1;
                continue;
            }
        };
        start_one(alloc, page);
        logical += 1;
    }
}

#[cfg(feature = "kernel_tests")]
static STALL_ONE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
#[cfg(feature = "kernel_tests")]
static STALL_LEAKED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "kernel_tests")]
fn arm_stall_from_cmdline() {
    let sel = vibeos::ktest::Selection::parse(crate::boot::cmdline().get("vibeos.ktest"));
    if sel.selects("stalled_ap_leak", true) {
        // Release: pairs with the Acquire swap in `maybe_stall_ap`.
        STALL_ONE.store(true, Ordering::Release);
    }
}

fn maybe_stall_ap() {
    #[cfg(feature = "kernel_tests")]
    {
        // AcqRel: pairs with the Release store in `arm_stall_from_cmdline`.
        if STALL_ONE.swap(false, Ordering::AcqRel) {
            loop {
                core::hint::spin_loop();
            }
        }
    }
}

fn note_stalled_leak() {
    #[cfg(feature = "kernel_tests")]
    {
        // Release: pairs with the Acquire load in the stalled-AP test.
        STALL_LEAKED.store(true, Ordering::Release);
    }
}

#[cfg(feature = "kernel_tests")]
pub fn stalled_ap_leaked() -> bool {
    // Acquire: pairs with the Release store in `note_stalled_leak`.
    STALL_LEAKED.load(Ordering::Acquire)
}

#[cfg(target_arch = "aarch64")]
fn init_aarch64() {
    #[cfg(feature = "kernel_tests")]
    arm_stall_from_cmdline();
    arch::aarch64::cpu::apply_computed_sysregs();
    arch::aarch64::cpu::release_debug_os_lock();
    let Some(entry) = arch::aarch64::secondary::entry_pa() else {
        crate::klog!(vibeos::log::Level::Error, "vibeOS: smp: no secondary entry");
        return;
    };
    let Some((stub_va, stub_len)) = arch::aarch64::secondary::stub_range() else {
        return;
    };
    let mut pas = TryVec::new();
    let stub_pa = arch::aarch64::secondary::va_to_pa(stub_va).unwrap_or(stub_va) & !0xFFF;
    let stub_end = stub_pa.saturating_add(stub_len as u64);
    let mut p = stub_pa;
    while p < stub_end {
        if pas.try_push(p).is_err() {
            break;
        }
        p = p.saturating_add(4096);
    }
    let Some((ttbr0_id, ttbr0_empty, mut id_tables)) =
        arch::aarch64::secondary::build_identity(&pas[..])
    else {
        crate::klog!(vibeos::log::Level::Error, "vibeOS: smp: identity map");
        return;
    };
    let mut n_id = 0usize;
    for &pa in id_tables.iter() {
        if pa != 0 {
            n_id += 1;
        }
    }
    clean_identity(&id_tables, n_id);
    arch::aarch64::secondary::clean_poc(stub_va, stub_len);
    let Some(desc) = machine_init::info() else {
        return;
    };
    let bsp = per_cpu_init::cpu(0)
        .map(|c| {
            // Relaxed: the BSP's MPIDR is fixed after `init_bsp`; pairs with nothing.
            u64::from(c.apic_id.load(Ordering::Relaxed))
        })
        .unwrap_or(0);
    let mut logical = 1u32;
    let mut i = 0usize;
    while i < desc.cpu_count() {
        let Some(cpu) = desc.cpus().get(i) else {
            break;
        };
        i += 1;
        if cpu.hw_id == bsp {
            continue;
        }
        if logical as usize >= per_cpu_init::cpu_count() {
            break;
        }
        match start_one_aarch64(
            logical,
            cpu.hw_id,
            entry,
            ttbr0_id,
            ttbr0_empty,
            &mut id_tables,
            &mut n_id,
        ) {
            StartAp::Online => {}
            StartAp::Failed => {}
        }
        logical += 1;
    }
}

#[cfg(target_arch = "aarch64")]
enum StartAp {
    Online,
    Failed,
}

#[cfg(target_arch = "aarch64")]
fn clean_identity(tables: &TryBoxWrap, n: usize) {
    let hhdm = crate::paging_init::hhdm_offset();
    for i in 0..n {
        let pa = tables[i];
        if pa != 0 {
            arch::aarch64::secondary::clean_poc(hhdm.wrapping_add(pa), 4096);
        }
    }
}

#[cfg(target_arch = "aarch64")]
type TryBoxWrap = vibeos::kalloc::TryBox<[u64; 8]>;

#[cfg(target_arch = "aarch64")]
fn start_one_aarch64(
    cpu_id: u32,
    hw_id: u64,
    entry_pa: u64,
    ttbr0_id: u64,
    ttbr0_empty: u64,
    id_tables: &mut TryBoxWrap,
    n_id: &mut usize,
) -> StartAp {
    let alloc = match alloc_ap_resources(cpu_id, hw_id, true) {
        Ok(a) => a,
        Err(e) => {
            crate::marker!("vibeOS: smp: apic {hw_id} alloc failed");
            if let Some(e) = e {
                crate::klog!(
                    vibeos::log::Level::Warn,
                    "smp: cpu {hw_id:#x} stays offline: {}",
                    e.as_str()
                );
            }
            return StartAp::Failed;
        }
    };
    let Some(cpu_ptr) = per_cpu_init::slot_ptr(cpu_id) else {
        crate::marker!("vibeOS: smp: apic {hw_id} alloc failed");
        free_ap_resources(alloc, false);
        return StartAp::Failed;
    };
    if LIVE_TABLES.with(|live| live.try_reserve(1)).is_err() {
        crate::marker!("vibeOS: smp: apic {hw_id} alloc failed");
        free_ap_resources(alloc, false);
        return StartAp::Failed;
    }
    let overflow_top = alloc.tables.overflow_top();
    // SAFETY: CPU is not running. established at `smp_init::start_one_aarch64`.
    let _ = unsafe {
        per_cpu_init::with_cpu(cpu_id, |cpu| {
            cpu.overflow_sp = overflow_top;
        })
    };
    let ApAlloc {
        tables,
        idle_id,
        workers,
        published,
        stack_top,
        ..
    } = alloc;
    if LIVE_TABLES.with(|live| live.try_push(tables)).is_err() {
        crate::marker!("vibeOS: smp: apic {hw_id} alloc failed");
        free_ap_slot(cpu_id, idle_id, workers, published, false);
        return StartAp::Failed;
    }
    let Some(param_pa) = alloc_param_page() else {
        crate::marker!("vibeOS: smp: apic {hw_id} alloc failed");
        free_live_ap(cpu_id, idle_id, workers, published, false);
        return StartAp::Failed;
    };
    if !arch::aarch64::secondary::map_identity(ttbr0_id, param_pa, id_tables, n_id) {
        crate::marker!("vibeOS: smp: apic {hw_id} alloc failed");
        free_live_ap(cpu_id, idle_id, workers, published, false);
        return StartAp::Failed;
    }
    clean_identity(id_tables, *n_id);
    let hhdm = crate::paging_init::hhdm_offset();
    let param_va = hhdm.wrapping_add(param_pa);
    let mut param = vibeos::arch::aarch64::psci::SecondaryParam::empty();
    arch::aarch64::secondary::fill_computed(&mut param);
    arch::aarch64::secondary::capture_el2(&mut param);
    param.ttbr0_id = ttbr0_id;
    param.ttbr1 = arch::aarch64::cpu::read_ttbr1();
    param.ttbr0_empty = ttbr0_empty;
    param.stack_top = stack_top;
    param.cpu_ptr = cpu_ptr as u64;
    param.entry_va = ap_entry_aarch64 as *const () as u64;
    param.continue_va = arch::aarch64::secondary::continue_va();
    param.param_va = param_va;
    param.conduit_hvc =
        u64::from(crate::machine_init::info().is_some_and(|d| {
            matches!(d.enable, vibeos::machine::EnableMethod::Psci { hvc: true })
        }));
    // SAFETY: `param_va` is the HHDM alias of a page we own. established here.
    unsafe { (param_va as *mut vibeos::arch::aarch64::psci::SecondaryParam).write_volatile(param) };
    arch::aarch64::secondary::clean_poc(
        param_va,
        core::mem::size_of::<vibeos::arch::aarch64::psci::SecondaryParam>(),
    );
    let on = crate::arch::aarch64::power::cpu_on(hw_id, entry_pa, param_pa);
    if on == vibeos::arch::aarch64::psci::ALREADY_ON {
        crate::marker!("vibeOS: smp: apic {hw_id} already on");
        free_live_ap(cpu_id, idle_id, workers, published, false);
        return StartAp::Failed;
    }
    if on != 0 {
        crate::marker!("vibeOS: smp: apic {hw_id} cpu_on {on}");
        free_live_ap(cpu_id, idle_id, workers, published, false);
        return StartAp::Failed;
    }
    let status = wait_secondary_status(param_va);
    if status == vibeos::arch::aarch64::psci::STATUS_FEATURE {
        crate::klog!(
            vibeos::log::Level::Error,
            "vibeOS: smp: cpu {hw_id:#x} isa floor (lse+pan) missing"
        );
        finish_aarch64_failure(cpu_id, hw_id, idle_id, workers, published);
        return StartAp::Failed;
    }
    if status != vibeos::arch::aarch64::psci::STATUS_ARRIVED {
        crate::marker!("vibeOS: smp: apic {hw_id} timed out");
        finish_aarch64_failure(cpu_id, hw_id, idle_id, workers, published);
        return StartAp::Failed;
    }
    if !wait_ready_ms(cpu_id) {
        crate::marker!("vibeOS: smp: apic {hw_id} timed out");
        finish_aarch64_failure(cpu_id, hw_id, idle_id, workers, published);
        return StartAp::Failed;
    }
    work_init::start_cpu_workers(&workers);
    crate::marker!(marker::SMP_AP_ONLINE);
    StartAp::Online
}

#[cfg(target_arch = "aarch64")]
fn alloc_param_page() -> Option<u64> {
    let f = crate::pmm_init::with_buddy(|b| b.alloc(0))?;
    let pa = f.into_entry();
    let va = crate::paging_init::hhdm_offset().wrapping_add(pa);
    // SAFETY: buddy page, HHDM maps it (I14). established here.
    unsafe { core::ptr::write_bytes(va as *mut u8, 0, 4096) };
    Some(pa)
}

#[cfg(target_arch = "aarch64")]
fn wait_secondary_status(param_va: u64) -> u64 {
    let status = param_va.wrapping_add(vibeos::arch::aarch64::psci::SecondaryParam::STATUS as u64)
        as *const u64;
    let t0 = time_init::now_ns();
    loop {
        // SAFETY: HHDM of this core's param page. established here.
        let s = unsafe { core::ptr::read_volatile(status) };
        if s != vibeos::arch::aarch64::psci::STATUS_NONE {
            return s;
        }
        if time_init::now_ns().saturating_sub(t0) > READY_TIMEOUT_MS.saturating_mul(1_000_000) {
            return vibeos::arch::aarch64::psci::STATUS_NONE;
        }
        core::hint::spin_loop();
    }
}

#[cfg(target_arch = "aarch64")]
fn wait_ready_ms(cpu_id: u32) -> bool {
    let t0 = time_init::now_ns();
    loop {
        // Acquire: pairs with the Release store in `ap_entry_aarch64`.
        if per_cpu_init::cpu(cpu_id).is_some_and(|c| c.ready.load(Ordering::Acquire)) {
            return true;
        }
        if time_init::now_ns().saturating_sub(t0) > READY_TIMEOUT_MS.saturating_mul(1_000_000) {
            return false;
        }
        core::hint::spin_loop();
    }
}

#[cfg(target_arch = "aarch64")]
fn finish_aarch64_failure(
    cpu_id: u32,
    hw_id: u64,
    idle_id: ThreadId,
    workers: CpuWorkers,
    published: bool,
) {
    per_cpu_init::mark_offline(cpu_id);
    let aff = crate::arch::aarch64::power::affinity_info(hw_id);
    if aff == vibeos::arch::aarch64::psci::AFF_OFF {
        free_live_ap(cpu_id, idle_id, workers, published, false);
    } else {
        note_stalled_leak();
        // F032: AFFINITY_INFO is not OFF; a late core may still start.
        // CpuWorkers is ThreadId-only and has no Drop.
        let _ = (idle_id, workers, published);
    }
}

#[cfg(target_arch = "aarch64")]
extern "C" fn ap_entry_aarch64(cpu: *mut PerCpu) -> ! {
    arch::aarch64::cpu::cli();
    // SAFETY: `cpu` is this AP's slot; the BSP's `with_cpu` ended before
    // CPU_ON (I43, I21, established at `smp_init::start_one_aarch64`).
    let cpu = unsafe { &mut *cpu };
    // SAFETY: this CPU's `PerCpu`, IRQs masked. established here.
    unsafe { per_cpu_init::install_gs(cpu) };
    // BSP published `cpu.current` before CPU_ON; `SP_EL0` is this CPU's
    // and still 0 (`mark_live` ran on the BSP).
    per_cpu_init::set_current_thread(cpu, cpu.current);
    arch::aarch64::vectors::load();
    // SAFETY: VBAR is live; `crate::syscall_init::init_ap` writes this
    // CPU's empty TTBR0; established here.
    unsafe { crate::syscall_init::init_ap(core::ptr::null(), 0) };
    arch::aarch64::cpu::release_debug_os_lock();
    arch::aarch64::cpu::print_exception_level();
    // SAFETY: this CPU's GIC; IRQs masked. established here.
    unsafe { apic_init::enable_ap() };
    apic_init::arm_ap();
    maybe_stall_ap();
    per_cpu_init::mark_online(cpu.cpu_id);
    crate::marker!(
        "{}{}{}",
        marker::SCHED_CPU_PREFIX,
        cpu.cpu_id,
        marker::SCHED_CPU_SUFFIX
    );
    // Release: pairs with the Acquire load in `wait_ready_ms`.
    cpu.remote.ready.store(true, Ordering::Release);
    arch::aarch64::cpu::sti();
    crate::sched_init::idle_loop();
}
