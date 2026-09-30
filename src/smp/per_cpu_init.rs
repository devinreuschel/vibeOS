//! Per-CPU areas via `GS_BASE`. DESIGN §3.3 step 11, §7.5.
//!
//! Heap array sized from the MADT CPU count. After GDT (`mov gs` zeros
//! the hidden base). Before the first timer IRQ: ISRs must not `gs:[0]`
//! until this is live. `KERNEL_GS_BASE` matches `GS_BASE`.

#[allow(
    clippy::disallowed_types,
    reason = "boot: DESIGN §3.3 step 11 per-CPU areas, before irq: enabled; sized once from the MADT, never grown"
)]
use alloc::boxed::Box;
#[allow(
    clippy::disallowed_types,
    reason = "boot: DESIGN §3.3 step 11 per-CPU areas, before irq: enabled; sized once from the MADT, never grown"
)]
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vibeos::per_cpu::{PerCpu, PerCpuRemote};
use vibeos::thread::Tcb;

use crate::acpi_init;
use crate::arch::x86_64::percpu;
use crate::cell::BootCell;
use crate::x86;
use crate::x86::InterruptGuard;

pub const IA32_GS_BASE: u32 = 0xC000_0101;
pub const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

#[allow(
    clippy::disallowed_types,
    reason = "boot: DESIGN §3.3 step 11 per-CPU areas, before irq: enabled; sized once from the MADT, never grown"
)]
static CPUS: BootCell<Box<[PerCpu]>> = BootCell::new();
/// Each CPU's remote view, apart from `CPUS` so that no `&mut PerCpu`
/// covers memory another CPU holds `&` to (DESIGN §7.5).
#[allow(
    clippy::disallowed_types,
    reason = "boot: DESIGN §3.3 step 11 per-CPU areas, before irq: enabled; sized once from the MADT, never grown"
)]
static REMOTE: BootCell<Box<[PerCpuRemote]>> = BootCell::new();

// `cpu(id)` shares `&PerCpuRemote` across CPUs, so the view must be `Sync`
// from its atomic fields alone; `scripts/check_cells.py` rejects an
// `unsafe impl` of `Send` or `Sync` for it (invariant I120).
crate::cell::assert_impl!(PerCpuRemote: Sync);
/// Bit `cpu_id`. MADTs with >64 CPUs need a wider mask later.
static ONLINE: AtomicU64 = AtomicU64::new(0);
static WITH_BUSY: [AtomicBool; 64] = [const { AtomicBool::new(false) }; 64];

fn apic_id() -> u32 {
    let (_, ebx, _, _) = x86::cpuid(1, 0);
    ebx >> 24
}

fn madt_cpu_count() -> usize {
    acpi_init::info().map(|i| i.cpu_count()).unwrap_or(0).max(1)
}

/// Allocate the heap array, install the BSP at slot 0.
///
/// `current` / `idle` stay null until [`crate::thread_init::init_bootstrap`].
///
/// # Safety
/// GDT already loaded (`mov gs` already happened). Single-CPU. IRQs
/// still masked at the controller, or at least no ISR uses `per_cpu`.
#[allow(
    clippy::disallowed_types,
    reason = "boot: DESIGN §3.3 step 11 per-CPU areas, before irq: enabled; sized once from the MADT, never grown"
)]
pub unsafe fn init_bsp() {
    let n = madt_cpu_count();
    let mut r = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        r.push(PerCpuRemote::new());
        i += 1;
    }
    // SAFETY: single writer before `smp: done`, no reader yet (BootCell's
    // set contract); established here: `init_bsp` runs once on the BSP.
    unsafe { REMOTE.set(r.into_boxed_slice()) };
    let remote: &'static [PerCpuRemote] = REMOTE.get();
    let mut v = Vec::with_capacity(n);
    i = 0;
    while i < n {
        v.push(PerCpu::new(&remote[i]));
        i += 1;
    }
    let mut boxed = v.into_boxed_slice();
    i = 0;
    while i < n {
        let p = &mut boxed[i] as *mut PerCpu;
        boxed[i].self_ptr = p;
        boxed[i].cpu_id = i as u32;
        i += 1;
    }
    remote[0].apic_id.store(apic_id(), Ordering::Relaxed);
    remote[0].ready.store(true, Ordering::Release);
    let ptr = boxed[0].self_ptr as u64;
    // SAFETY: `ptr` is slot 0's address, the BSP's `PerCpu`, and the heap
    // keeps the boxed slice in place for good once `CPUS` holds it; the GDT
    // load already did the `mov gs`, and no ISR reads `gs:[0]` yet (this
    // fn's `# Safety` contract), so both bases name this CPU's area from
    // here on (invariant I4, established here).
    unsafe {
        x86::wrmsr(IA32_GS_BASE, ptr);
        x86::wrmsr(IA32_KERNEL_GS_BASE, ptr);
    }
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    // SAFETY: single writer before `smp: done`, no reader yet (BootCell's
    // set contract); established here: `init_bsp` runs once on the BSP.
    unsafe { CPUS.set(boxed) };
    x86::set_per_cpu_hooks(irq_nest_enter, irq_nest_leave, cpu_index_hook);
    percpu::mark_live();
    ONLINE.store(1, Ordering::Release);
}

pub fn is_live() -> bool {
    percpu::is_live()
}

/// The `PerCpu` array's base address and length, which VMCOREINFO's
/// `SYMBOL(vibeos_cpus)` and `LENGTH(vibeos_cpus)` carry
/// (docs/VMCOREINFO.md); `(0, 0)` before `init_bsp`. `CPUS` keeps the boxed
/// slice in place for good once set.
pub(crate) fn table_root() -> (u64, u64) {
    CPUS.try_get()
        .map_or((0, 0), |c| (c.as_ptr().addr() as u64, c.len() as u64))
}

pub fn cpu_count() -> usize {
    CPUS.try_get().map(|c| c.len()).unwrap_or(0)
}

/// CPU `id`'s remote view: the only per-CPU state another CPU reads. It
/// is never taken `&mut`, so it may alias anything (DESIGN §7.5).
pub fn cpu(id: u32) -> Option<&'static PerCpuRemote> {
    REMOTE.try_get()?.get(id as usize)
}

/// CPU `id`'s `PerCpu` address, set once in [`init_bsp`]. For
/// `smp_init::start_one`, which hands it to the AP it starts.
pub fn slot_ptr(id: u32) -> Option<*mut PerCpu> {
    CPUS.try_get()?.get(id as usize).map(|c| c.self_ptr)
}

/// Set by [`arm_if_checks`]: from here on [`current`] and [`try_current`]
/// assert IF=0 in debug builds.
static IF_CHECKS: AtomicBool = AtomicBool::new(false);

/// Arm the IF=0 assertion of [`current`] and [`try_current`] (DESIGN §2.9
/// rule 5). `sched_init::init` calls it once, before `irq: enabled`.
pub fn arm_if_checks() {
    // Release: pairs with the Acquire load in `if_checks_armed`.
    IF_CHECKS.store(true, Ordering::Release);
}

/// Whether [`arm_if_checks`] ran.
pub fn if_checks_armed() -> bool {
    // Acquire: pairs with the Release store in `arm_if_checks`.
    IF_CHECKS.load(Ordering::Acquire)
}

/// `gs:[0]` == `self_ptr`. Only after [`init_bsp`] (and AP `install_gs`).
/// IF=0 only once [`arm_if_checks`] ran: with IF=1 the thread may move to
/// another CPU and keep a reference to the one it left.
#[track_caller]
pub fn current() -> &'static PerCpu {
    debug_assert!(
        !if_checks_armed() || !x86::interrupts_enabled(),
        "per_cpu: access with IF=1 (INVARIANTS §2.9 rule 5)"
    );
    assert!(is_live(), "per_cpu: not live");
    let p = gs_self();
    assert!(!p.is_null(), "per_cpu: gs null");
    // SAFETY: once `LIVE` is set, `gs:[0]` is this CPU's slot in `CPUS`,
    // which is never freed (invariant I4, established at
    // `smp::per_cpu_init::init_bsp` and `smp::per_cpu_init::install_gs`).
    // The shared reference may overlap a `with_current` scope's `&mut` on
    // this CPU, as invariant I36's row records (ROADMAP §10.3, F039).
    unsafe { &*p }
}

/// [`current`], or `None` before the area is live. IF=0 only, as
/// [`current`] is.
#[track_caller]
pub fn try_current() -> Option<&'static PerCpu> {
    debug_assert!(
        !if_checks_armed() || !x86::interrupts_enabled(),
        "per_cpu: access with IF=1 (INVARIANTS §2.9 rule 5)"
    );
    if !is_live() {
        return None;
    }
    let p = gs_self();
    if p.is_null() {
        return None;
    }
    // SAFETY: as in `current`: invariant I4, established at
    // `smp::per_cpu_init::init_bsp` and `smp::per_cpu_init::install_gs`,
    // with the overlap invariant I36's row records.
    Some(unsafe { &*p })
}

/// Exclusive `&mut PerCpu` for this CPU. IRQs off. Panics on re-entry.
///
/// Do not `switch_context` inside `f`. The busy flag lives on this stack;
/// the incoming thread would see it set with IF possibly on (DESIGN §9.4).
#[inline(always)]
pub fn with_current<R>(f: impl FnOnce(&mut PerCpu) -> R) -> R {
    let _irq = InterruptGuard::enter();
    assert!(is_live(), "per_cpu: not live");
    let p = gs_self();
    assert!(!p.is_null(), "per_cpu: gs null");
    // SAFETY: `with_ptr`'s contract; `p` is this CPU's slot (invariant I4,
    // established at `smp::per_cpu_init::init_bsp`), and the guard above
    // holds IF=0 (invariant I21, established here).
    unsafe { with_ptr(p, f) }
}

/// Exclusive `&mut PerCpu` for `switch_now`'s bookkeeping. Needs IF=0.
///
/// The caller's `InterruptGuard` spans `switch_context`, which the caller
/// runs after this returns: the `&mut` and the busy flag end before the
/// switch, so the incoming thread can take IRQs and `with_current`
/// (DESIGN §7.5, §9.4).
#[inline(always)]
pub fn with_current_switch<R>(f: impl FnOnce(&mut PerCpu) -> R) -> R {
    assert!(
        !x86::interrupts_enabled(),
        "per_cpu: with_current_switch with IF on"
    );
    assert!(is_live(), "per_cpu: not live");
    let p = gs_self();
    assert!(!p.is_null(), "per_cpu: gs null");
    // SAFETY: `with_ptr`'s contract; `p` is this CPU's slot (invariant I4,
    // established at `smp::per_cpu_init::init_bsp`), and the assert above
    // found IF=0 (invariant I21, established here).
    unsafe { with_ptr(p, f) }
}

/// Exclusive `&mut PerCpu` for CPU `id`. IRQs off. BSP bring-up of an AP
/// before its SIPI. Never a remote run queue (DESIGN §7.7).
///
/// # Safety
/// CPU `id` is not running: it has not been sent a SIPI, or it never
/// accepted one. No other scope on its slot is live (the busy flag panics
/// on one on this CPU, not on another).
#[inline(always)]
pub unsafe fn with_cpu<R>(id: u32, f: impl FnOnce(&mut PerCpu) -> R) -> Option<R> {
    let _irq = InterruptGuard::enter();
    let cpus = CPUS.try_get()?;
    let cpu = cpus.get(id as usize)?;
    // SAFETY: `with_ptr`'s contract; `self_ptr` is slot `id` of `CPUS`, and
    // CPU `id` is not running by this fn's `# Safety` contract (invariants
    // I120 and I21, established here).
    Some(unsafe { with_ptr(cpu.self_ptr, f) })
}

/// Run `f` on `&mut *p` under `p`'s busy flag.
///
/// # Safety
/// `p` is a slot of `CPUS`, and the caller may take `&mut` to it: it is
/// this CPU's slot and IF=0, or its CPU is not running (invariant I21).
#[inline(always)]
#[allow(
    clippy::panic,
    reason = "invariant I21: a with_current or with_cpu scope never nests on one slot"
)]
unsafe fn with_ptr<R>(p: *mut PerCpu, f: impl FnOnce(&mut PerCpu) -> R) -> R {
    // SAFETY: `p` is a live slot of `CPUS` by this fn's `# Safety`
    // contract, established here; `cpu_id` is written once in `init_bsp`.
    let id = unsafe { (*p).cpu_id as usize }.min(63);
    if WITH_BUSY[id].swap(true, Ordering::Acquire) {
        panic!("per_cpu: with_current re-entry");
    }
    struct Unlock<'a>(&'a AtomicBool);
    impl Drop for Unlock<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    let _u = Unlock(&WITH_BUSY[id]);
    // SAFETY: invariant I21, established here by the `WITH_BUSY` swap
    // above: no other `with_current`/`with_cpu` scope on this slot is live.
    let pc = unsafe { &mut *p };
    let r = f(pc);
    pc.publish_runq_len();
    r
}

/// Write `GS_BASE` and `KERNEL_GS_BASE` to this CPU's slot.
///
/// # Safety
/// `cpu` is this CPU's `PerCpu`. Call after `mov gs` (GDT load) and
/// before `sti` / any ISR that reads `gs:[0]`.
pub unsafe fn install_gs(cpu: &PerCpu) {
    let ptr = cpu.self_ptr as u64;
    // SAFETY: `cpu` is this CPU's `PerCpu`, after `mov gs` and before any
    // ISR reads `gs:[0]` (this fn's `# Safety` contract), so both bases name
    // this CPU's area from here on (invariant I4, established here).
    unsafe {
        x86::wrmsr(IA32_GS_BASE, ptr);
        x86::wrmsr(IA32_KERNEL_GS_BASE, ptr);
    }
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
}

/// InterruptGuard nesting. A no-op only before [`init_bsp`] sets `LIVE`;
/// after that it loads `gs:[0]`, so `GS_BASE` must already point at this
/// CPU's `PerCpu`.
pub fn irq_nest_enter() {
    if let Some(c) = try_current() {
        c.irq_nest.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn irq_nest_leave() {
    if let Some(c) = try_current() {
        let old = c.irq_nest.fetch_sub(1, Ordering::Relaxed);
        assert!(old > 0, "irq nest underflow");
    }
}

/// `x86::cpu_index`'s hook: this CPU's `cpu_id` once the area is live,
/// read with [`percpu::cpu_id_hint`]: exact while IF=0, a hint with IF=1.
fn cpu_index_hook() -> Option<u32> {
    is_live().then(percpu::cpu_id_hint)
}

/// This CPU's `InterruptGuard` depth. 0 with IF=1: every guard holds IF=0
/// while it lives (SMP.md §7.5), so only an IF=0 caller reads the slot.
pub fn irq_nest() -> u32 {
    if x86::interrupts_enabled() {
        return 0;
    }
    try_current()
        .map(|c| c.irq_nest.load(Ordering::Relaxed))
        .unwrap_or(0)
}

pub fn gs_self() -> *mut PerCpu {
    let ptr: u64;
    // SAFETY: an 8-byte load at `GS_BASE` that touches no stack or flags;
    // once `init_bsp` (on an AP, `install_gs`) ran, `GS_BASE` is this CPU's
    // `PerCpu`, whose first field is `self_ptr` (invariant I4, established
    // at `smp::per_cpu_init::init_bsp`), and every caller runs after that.
    unsafe {
        core::arch::asm!(
            "mov {}, qword ptr gs:[0]",
            out(reg) ptr,
            options(nostack, preserves_flags),
        );
    }
    ptr as *mut PerCpu
}

pub fn set_current_thread(cpu: &mut PerCpu, tcb: *mut Tcb) {
    cpu.current = tcb;
}

pub fn current_thread() -> *mut Tcb {
    current().current
}

pub fn set_tsc_per_ms(v: u64) {
    if try_current().is_some() {
        with_current(|c| c.tsc_per_ms = v);
    }
}

pub fn set_timer_mode(mode: vibeos::apic::TimerMode) {
    if try_current().is_some() {
        with_current(|c| c.timer_mode = mode);
    }
}

pub fn mark_online(cpu_id: u32) {
    if cpu_id >= 64 {
        return;
    }
    ONLINE.fetch_or(1u64 << cpu_id, Ordering::Release);
}

pub fn online_mask() -> u64 {
    ONLINE.load(Ordering::Acquire)
}

pub fn is_online(cpu_id: u32) -> bool {
    if cpu_id >= 64 {
        return false;
    }
    online_mask() & (1u64 << cpu_id) != 0
}

/// Field access that is safe from an ISR once `GS_BASE` is live. IF=0
/// only, as [`current`] is.
#[macro_export]
macro_rules! per_cpu {
    ($field:ident) => {
        $crate::per_cpu_init::current().$field
    };
}
