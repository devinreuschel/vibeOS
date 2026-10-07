//! Device IRQ dispatcher, `IrqId` allocation, MSI/MSI-X enable. DESIGN §5.4.
//!
//! EOI is here. Hard-IRQ `fn()` acks and wakes; threaded handlers may
//! allocate and block. Allocate is refused in a hard-IRQ (the dispatcher
//! flag, not `InterruptGuard`).

use core::any::Any;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use vibeos::apic::{EoiDomain, Polarity, Trigger};
use vibeos::dev::{Device, Instance};
use vibeos::irq::{
    self, IrqChip, IrqError, IrqId, IrqSet, IrqSpecifier, IrqTable, MsiMessage, MsixEntry,
    PlannedMsi, PlannedWired, VectorPool, in_pool, msi_message_addr, msi_message_data, pool_index,
};
use vibeos::lock::RANK_DEVICE;
use vibeos::pci::{self, Bdf};
use vibeos::sched::FAR_DEADLINE;
use vibeos::vectors;
use vibeos::wait::WaitQueue;

use super::hardirq;
use crate::apic_init;
use crate::arch;
use crate::cell::BootCell;
use crate::dev_init;
use crate::pci_init;
use crate::per_cpu_init;
use crate::sync_init::SpinMutex;
use crate::thread_init;

type Handler = fn();

/// A threaded handler half. It gets the context its vector was set with
/// (a driver instance, DESIGN §12.1 rule 1), so no driver keeps a table of
/// its devices to find the one that interrupted.
pub type ThreadedFn = fn(Option<&(dyn Any + Send + Sync)>);

#[derive(Clone, Copy)]
struct IoApicBind {
    gsi: u32,
    trigger: Trigger,
    polarity: Polarity,
}

pub(super) struct IrqState {
    pool: VectorPool,
    table: IrqTable,
    routes: [Route; irq::MAX_IRQS],
    gsi_bind: [Option<IoApicBind>; irq::POOL_LEN],
    /// MSI-X table entry `enable_msix` programmed for this `IrqId`, so a
    /// later `set_affinity` can rewrite it. `None` until then.
    msix: [Option<MsixLive>; irq::MAX_IRQS],
    pub(super) th: Threaded,
}

static IRQ: SpinMutex<IrqState> = SpinMutex::with_rank(
    IrqState {
        pool: VectorPool::new(),
        table: IrqTable::new(),
        routes: [Route::None; irq::MAX_IRQS],
        gsi_bind: [None; irq::POOL_LEN],
        msix: [None; irq::MAX_IRQS],
        th: Threaded {
            wq: WaitQueue::new(),
            pending: [false; irq::MAX_IRQS],
            top: [None; irq::MAX_IRQS],
            work: [None; irq::MAX_IRQS],
            ctx: [const { None }; irq::MAX_IRQS],
            started: false,
        },
    },
    RANK_DEVICE,
);

/// Run `f` on the IRQ table, pool, routes and threaded state.
pub(super) fn with_irq<R>(f: impl FnOnce(&mut IrqState) -> R) -> R {
    let mut g = IRQ.lock();
    f(&mut g)
}
static HANDLERS: [AtomicUsize; irq::MAX_IRQS] = [const { AtomicUsize::new(0) }; irq::MAX_IRQS];
/// Vector → [`IrqId`]. 0 is none. Release/Acquire with bind and free.
static VEC_TO_IRQ: [AtomicU32; 256] = [const { AtomicU32::new(0) }; 256];

/// One programmed MSI-X table entry. `table` is the VA `enable_msix`
/// wrote, `index` the entry in it.
#[derive(Clone, Copy)]
struct MsixLive {
    table: u64,
    index: u16,
}

#[derive(Clone, Copy)]
enum Route {
    None,
    IoApic {
        gsi: u32,
    },
    #[cfg_attr(
        any(not(feature = "kernel_tests"), not(target_arch = "aarch64")),
        expect(
            dead_code,
            reason = "ROADMAP §6.3 MSI configuration: only `enable_msi` builds it"
        )
    )]
    Msi,
    Msix,
    Gic,
}

pub(super) struct Threaded {
    wq: WaitQueue,
    pub(super) pending: [bool; irq::MAX_IRQS],
    pub(super) top: [Option<ThreadedFn>; irq::MAX_IRQS],
    pub(super) work: [Option<ThreadedFn>; irq::MAX_IRQS],
    /// Each IRQ's context, which both halves get a reference to.
    pub(super) ctx: [Option<Instance>; irq::MAX_IRQS],
    started: bool,
}

static THREAD_CPU: AtomicU32 = AtomicU32::new(0);

struct PicChip;
struct IoApicChip;
struct LapicMsiChip;

static PIC_OBJ: PicChip = PicChip;
static IOAPIC_OBJ: IoApicChip = IoApicChip;
static LAPIC_MSI_OBJ: LapicMsiChip = LapicMsiChip;

static PIC: BootCell<&'static dyn IrqChip> = BootCell::new();
static IOAPIC: BootCell<&'static dyn IrqChip> = BootCell::new();
static LAPIC_MSI: BootCell<&'static dyn IrqChip> = BootCell::new();

fn pic() -> &'static dyn IrqChip {
    *PIC.get()
}

fn ioapic() -> &'static dyn IrqChip {
    *IOAPIC.get()
}

fn lapic_msi() -> &'static dyn IrqChip {
    *LAPIC_MSI.get()
}

fn msi_chip() -> &'static dyn IrqChip {
    #[cfg(target_arch = "aarch64")]
    {
        match crate::arch::aarch64::gic::chip() {
            Some(c) => c,
            None => crate::boot::halt_with("vibeOS: irq: no gic"),
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        lapic_msi()
    }
}

fn publish_vec(hwirq: u32, irq: IrqId) {
    let Ok(v) = u8::try_from(hwirq) else {
        return;
    };
    if !in_pool(v) && arch::pic::line_of(v).is_none() {
        return;
    }
    if let Some(slot) = VEC_TO_IRQ.get(usize::from(v)) {
        // Release: pairs with the Acquire load in `dispatch`.
        slot.store(irq.raw(), Ordering::Release);
    }
}

fn unpublish_vec(hwirq: u32) {
    let Ok(v) = u8::try_from(hwirq) else {
        return;
    };
    if let Some(slot) = VEC_TO_IRQ.get(usize::from(v)) {
        // Release: pairs with the Acquire load in `dispatch`.
        slot.store(0, Ordering::Release);
    }
}

/// Drop the vector map and, on aarch64, the INTID → `IrqId` map.
fn unpublish_hwirq(hwirq: u32) {
    unpublish_vec(hwirq);
    #[cfg(target_arch = "aarch64")]
    crate::arch::aarch64::gic::publish(hwirq, 0);
}

fn publish_msi(plan: &PlannedMsi, set: &IrqSet) {
    let mut i = 0usize;
    while i < set.len() {
        if let (Some(irq), Some(h)) = (set.get(i), plan.hwirq_at(i)) {
            publish_vec(h, irq);
            #[cfg(target_arch = "aarch64")]
            crate::arch::aarch64::gic::publish(h, irq.raw());
        }
        i += 1;
    }
}

fn bind_msi(plan: &PlannedMsi) -> Result<IrqSet, IrqError> {
    with_irq(|s| {
        let set = s.table.commit_msi(plan)?;
        publish_msi(plan, &set);
        Ok(set)
    })
}

pub(super) fn with_pool<R>(f: impl FnOnce(&mut VectorPool) -> R) -> R {
    with_irq(|s| f(&mut s.pool))
}

pub use super::hardirq::in_hard_irq;

fn device_irq(frame: &mut arch::idt::TrapFrame) {
    #[cfg(target_arch = "x86_64")]
    dispatch(frame.vector as u8);
    #[cfg(target_arch = "aarch64")]
    dispatch(frame.slot as u8);
}

pub fn dispatch(vec: u8) {
    hardirq::set(true);
    let mut owned = false;
    let irq_raw = VEC_TO_IRQ
        .get(usize::from(vec))
        .map(|s| {
            // Acquire: pairs with the Release stores in bind (`publish_vec`)
            // and `free_vector`.
            s.load(Ordering::Acquire)
        })
        .unwrap_or(0);
    let irq = IrqId::from_raw(irq_raw);
    if let Some(i) = irq.slot() {
        let (top, work, ctx) = with_irq(|s| {
            (
                s.th.top.get(i).copied().flatten(),
                s.th.work.get(i).copied().flatten(),
                s.th.ctx.get(i).cloned().flatten(),
            )
        });
        if work.is_some() || top.is_some() {
            owned = true;
            if let Some(h) = top {
                h(ctx.as_deref());
            }
            thread_init::with_sched(|sched| {
                with_irq(|s| {
                    if let Some(p) = s.th.pending.get_mut(i) {
                        *p = true;
                    }
                    if s.th.started {
                        sched.wake_one(&mut s.th.wq);
                    }
                });
            });
            // A count, never the last while the vector holds its own; a
            // last put in hard IRQ defers (DESIGN §2.11 rule 6).
            drop(ctx);
        } else if let Some(slot) = HANDLERS.get(i) {
            // Acquire: pairs with the Release stores in `set_handler` and `free_vector`.
            let p = slot.load(Ordering::Acquire);
            if p != 0 {
                owned = true;
                // SAFETY: invariant: a nonzero `HANDLERS` slot holds a
                // `fn()`; established by `irq::irq_init::set_handler`, its
                // only nonzero store.
                let h: Handler = unsafe { core::mem::transmute(p) };
                h();
            }
        }
    }
    if owned {
        let chip = with_irq(|s| s.table.chip(irq));
        match chip {
            Some(c) => c.eoi(u32::from(vec)),
            None => apic_init::eoi_for(vec),
        }
    } else {
        // No handler: counted, EOIed where it came from, logged.
        unowned(vec);
    }
    hardirq::set(false);
}

/// Deliver a GIC INTID to its `IrqId` handler, or the timer / SGI path.
///
/// Timer and SGI may `schedule_preempt` (DESIGN §5.8). They are not a
/// device top half, so they must not set `IN_ISR`.
#[cfg(target_arch = "aarch64")]
pub fn dispatch_intid(intid: u32) {
    if intid < 16 {
        match intid {
            0 => crate::ipi_init::on_reschedule_ipi(),
            1 => crate::ipi_init::on_call_ipi(),
            2 => {
                crate::ipi_init::on_stop_ipi(crate::ipi_init::here_regs_for_stop());
            }
            _ => {}
        }
        return;
    }
    if intid == crate::arch::aarch64::timer::intid() {
        apic_init::on_timer_irq();
        return;
    }
    #[cfg(feature = "kernel_tests")]
    crate::arch::aarch64::gic::ktest_note(intid);
    hardirq::set(true);
    let irq_raw = crate::arch::aarch64::gic::lookup(intid);
    let irq = IrqId::from_raw(irq_raw);
    if let Some(i) = irq.slot() {
        if let Some(slot) = HANDLERS.get(i) {
            // Acquire: pairs with the Release store in `set_handler`.
            let p = slot.load(Ordering::Acquire);
            if p != 0 {
                // SAFETY: a nonzero `HANDLERS` slot holds a `fn()`;
                // established by `irq::irq_init::set_handler`.
                let h: Handler = unsafe { core::mem::transmute(p) };
                h();
            }
        }
        let (top, work, ctx) = with_irq(|s| {
            (
                s.th.top.get(i).copied().flatten(),
                s.th.work.get(i).copied().flatten(),
                s.th.ctx.get(i).cloned().flatten(),
            )
        });
        if work.is_some() || top.is_some() {
            if let Some(h) = top {
                h(ctx.as_deref());
            }
            thread_init::with_sched(|sched| {
                with_irq(|s| {
                    if let Some(p) = s.th.pending.get_mut(i) {
                        *p = true;
                    }
                    if s.th.started {
                        sched.wake_one(&mut s.th.wq);
                    }
                });
            });
            drop(ctx);
        }
    } else if let Some(c) = crate::arch::aarch64::gic::chip() {
        // No IrqId: a level SPI would re-pend after EOI forever.
        c.mask(intid);
    }
    hardirq::set(false);
}

/// Interrupts no handler owned, per CPU (`acpi::MAX_CPUS`, the cap
/// `hardirq::IN_ISR` shares) and per vector. Static: the count runs in
/// hard IRQ, where nothing allocates.
static UNOWNED: [[AtomicU64; 256]; vibeos::acpi::MAX_CPUS] =
    [const { [const { AtomicU64::new(0) }; 256] }; vibeos::acpi::MAX_CPUS];
/// Per vector: the `now_ms` of its last `irq: no handler` line, plus one
/// (0: never logged).
static UNOWNED_LOGGED: [AtomicU64; 256] = [const { AtomicU64::new(0) }; 256];
/// `irq: no handler` lines printed per vector (kernel_tests only).
#[cfg(feature = "kernel_tests")]
static UNOWNED_LINES: [AtomicU64; 256] = [const { AtomicU64::new(0) }; 256];

/// An interrupt on `vec` that no handler owns (DESIGN §5.2, §2.10): count
/// it for this CPU, EOI the controller that delivered it (the LAPIC when
/// its in-service bit for `vec` is set, else the 8259 for `0x20`-`0x2F`,
/// which masks a line no driver claims), log
/// `irq: no handler for vector 0x<v> cpu <n> count <n>` at most once a
/// second per vector unless it was a spurious IRQ7 or IRQ15, and return.
/// The vector table's default body for `0x20`-`0xFF`, [`dispatch`]'s
/// unowned case, and every 8259 line's body call it, with IF=0.
pub fn unowned(vec: u8) {
    let cpu = crate::arch::cpu_id_hint();
    // Relaxed: a count for the log line; pairs with nothing.
    let count = UNOWNED
        .get(cpu as usize)
        .and_then(|row| row.get(usize::from(vec)))
        .map_or(0, |c| c.fetch_add(1, Ordering::Relaxed).wrapping_add(1));
    let spurious = match vibeos::apic::unowned_eoi(vec, apic_init::in_service(vec)) {
        EoiDomain::Lapic => {
            apic_init::eoi();
            false
        }
        EoiDomain::Pic => arch::pic::line_of(vec).is_some_and(arch::pic::unclaimed),
        EoiDomain::None => false,
    };
    if spurious {
        return;
    }
    let now = crate::time_init::now_ns() / 1_000_000;
    let slot = &UNOWNED_LOGGED[usize::from(vec)];
    // Relaxed: a rate limit's timestamp, which orders nothing; pairs with nothing.
    let last = slot.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last - 1) < 1_000 {
        return;
    }
    // Relaxed both ways: the exchange only picks which CPU logs; pairs with nothing.
    if slot
        .compare_exchange(
            last,
            now.saturating_add(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        )
        .is_err()
    {
        return;
    }
    // Relaxed: a count `unowned_lines` reads; pairs with nothing.
    #[cfg(feature = "kernel_tests")]
    UNOWNED_LINES[usize::from(vec)].fetch_add(1, Ordering::Relaxed);
    crate::marker!(
        "vibeOS: irq: no handler for vector 0x{:02x} cpu {} count {}",
        vec,
        cpu,
        count
    );
}

/// How many interrupts on `vec` no handler owned on `cpu`.
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "ROADMAP §25.7's soak samples it; the in-guest storm test reads it today"
    )
)]
pub fn unowned_count(vec: u8, cpu: u32) -> u64 {
    // Relaxed: a count; pairs with nothing.
    UNOWNED
        .get(cpu as usize)
        .and_then(|row| row.get(usize::from(vec)))
        .map_or(0, |c| c.load(Ordering::Relaxed))
}

/// `irq: no handler` lines printed for `vec` so far.
#[cfg(feature = "kernel_tests")]
pub fn unowned_lines(vec: u8) -> u64 {
    // Relaxed: a count; pairs with nothing.
    UNOWNED_LINES[usize::from(vec)].load(Ordering::Relaxed)
}

/// Test helper: one MSI hwirq from the LAPIC chip, bound to `cpu`.
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "ROADMAP §6.3 / §11.3: in-guest tests allocate without a device"
    )
)]
pub fn allocate(cpu: u32) -> Result<IrqId, IrqError> {
    if in_hard_irq() {
        return Err(IrqError::InIrq);
    }
    let plan = PlannedMsi::claim(msi_chip(), 1, cpu)?;
    match bind_msi(&plan) {
        Ok(set) => set.get(0).ok_or(IrqError::Exhausted),
        Err(e) => {
            plan.release();
            Err(e)
        }
    }
}

/// Map a firmware specifier. Leaves an I/O APIC route masked until
/// [`set_handler`] or [`set_threaded`].
pub fn map_wired(spec: IrqSpecifier) -> Result<IrqId, IrqError> {
    if in_hard_irq() {
        return Err(IrqError::InIrq);
    }
    let chip = match spec {
        IrqSpecifier::Gsi { .. } => ioapic(),
        IrqSpecifier::Isa { .. } => pic(),
        IrqSpecifier::LapicLvt(_) => return Err(IrqError::BadVector),
        IrqSpecifier::Gic { .. } => {
            #[cfg(target_arch = "aarch64")]
            {
                crate::arch::aarch64::gic::chip().ok_or(IrqError::NoRoute)?
            }
            #[cfg(not(target_arch = "aarch64"))]
            {
                return Err(IrqError::NoRoute);
            }
        }
    };
    let plan = PlannedWired::claim(chip, spec, 0)?;
    if let IrqSpecifier::Gsi {
        gsi,
        trigger,
        polarity,
    } = spec
    {
        let Some(dest) = apic_id(0) else {
            plan.release();
            return Err(IrqError::BadCpu);
        };
        let Ok(vec) = u8::try_from(plan.hwirq()) else {
            plan.release();
            return Err(IrqError::BadVector);
        };
        if apic_init::route_gsi(gsi, vec, dest, trigger, polarity).is_err() {
            plan.release();
            return Err(IrqError::NoRoute);
        }
    }
    match with_irq(|s| {
        let irq = s.table.commit_wired(&plan)?;
        if let Some(i) = irq.slot()
            && let Some(r) = s.routes.get_mut(i)
        {
            match spec {
                IrqSpecifier::Gsi { gsi, .. } => *r = Route::IoApic { gsi },
                IrqSpecifier::Gic { .. } => *r = Route::Gic,
                _ => {}
            }
        }
        publish_vec(plan.hwirq(), irq);
        #[cfg(target_arch = "aarch64")]
        crate::arch::aarch64::gic::publish(plan.hwirq(), irq.raw());
        Ok(irq)
    }) {
        Ok(irq) => Ok(irq),
        Err(e) => {
            plan.release();
            Err(e)
        }
    }
}

/// Allocate `n` MSI/MSI-X [`IrqId`]s through the LAPIC MSI chip.
pub fn alloc_msi(dev: &Device, n: u8) -> Result<IrqSet, IrqError> {
    let _ = dev;
    if in_hard_irq() {
        return Err(IrqError::InIrq);
    }
    let plan = PlannedMsi::claim(msi_chip(), n, 0)?;
    match bind_msi(&plan) {
        Ok(set) => {
            #[cfg(target_arch = "aarch64")]
            {
                let id = pci_device_id(dev);
                let mut map_err = None;
                let mut i = 0usize;
                while i < plan.len() {
                    let Some(h) = plan.hwirq_at(i) else {
                        map_err = Some(IrqError::BadVector);
                        break;
                    };
                    if let Err(e) = crate::arch::aarch64::gic::map_its_event(id, h) {
                        map_err = Some(e);
                        break;
                    }
                    i += 1;
                }
                if let Some(e) = map_err {
                    let mut j = 0usize;
                    while j < set.len() {
                        if let Some(irq) = set.get(j)
                            && free_vector(irq).is_err()
                        {
                            crate::klog!(
                                vibeos::log::Level::Warn,
                                "vibeOS: irq: msi unwind failed"
                            );
                        }
                        j += 1;
                    }
                    return Err(e);
                }
            }
            Ok(set)
        }
        Err(e) => {
            plan.release();
            Err(e)
        }
    }
}

#[cfg(target_arch = "aarch64")]
fn pci_device_id(dev: &Device) -> u32 {
    let rid = (u32::from(dev.addr.bus) << 8)
        | (u32::from(dev.addr.device) << 3)
        | u32::from(dev.addr.function);
    let Some(h) = crate::machine_init::info().and_then(|d| d.pci_hosts().first()) else {
        return rid;
    };
    let mut i = 0usize;
    while i < h.msi_map_len {
        if let Some(e) = h.msi_map.get(i)
            && rid >= e.rid_base
            && rid.wrapping_sub(e.rid_base) < e.length
        {
            return e.msi_base.wrapping_add(rid.wrapping_sub(e.rid_base));
        }
        i += 1;
    }
    rid
}

/// One [`IrqId`] for a per-CPU source (LAPIC LVT or GIC PPI).
#[cfg_attr(
    not(target_arch = "aarch64"),
    expect(
        dead_code,
        reason = "ROADMAP §11.3 LAPIC LVT / GIC PPI; no production caller yet"
    )
)]
pub fn map_percpu(spec: IrqSpecifier) -> Result<IrqId, IrqError> {
    if in_hard_irq() {
        return Err(IrqError::InIrq);
    }
    let chip = match spec {
        IrqSpecifier::LapicLvt(_) => lapic_msi(),
        IrqSpecifier::Gic { .. } => {
            #[cfg(target_arch = "aarch64")]
            {
                crate::arch::aarch64::gic::chip().ok_or(IrqError::NoRoute)?
            }
            #[cfg(not(target_arch = "aarch64"))]
            {
                return Err(IrqError::NoRoute);
            }
        }
        _ => return Err(IrqError::BadVector),
    };
    let plan = PlannedWired::claim(chip, spec, 0)?;
    match with_irq(|s| {
        let irq = s.table.commit_wired(&plan)?;
        #[cfg(target_arch = "aarch64")]
        crate::arch::aarch64::gic::publish(plan.hwirq(), irq.raw());
        Ok(irq)
    }) {
        Ok(irq) => Ok(irq),
        Err(e) => {
            plan.release();
            Err(e)
        }
    }
}

/// The x86 hwirq (IDT vector) for `irq`.
pub fn vector(irq: IrqId) -> Option<u8> {
    with_irq(|s| s.table.hwirq(irq).and_then(|h| u8::try_from(h).ok()))
}

/// The chip hwirq for `irq`, including an LPI that does not fit in a `u8`.
#[cfg(feature = "kernel_tests")]
#[cfg_attr(
    not(target_arch = "aarch64"),
    expect(
        dead_code,
        reason = "aarch64 `lpi_free_realloc` reads the INTID after free"
    )
)]
pub fn hwirq_of(irq: IrqId) -> Option<u32> {
    with_irq(|s| s.table.hwirq(irq))
}

/// MAPD V=0 for `dev` after its LPIs have been freed. No ITS is a no-op.
pub fn release_its_device(dev: &Device) {
    #[cfg(target_arch = "aarch64")]
    crate::arch::aarch64::gic::remove_its_device(pci_device_id(dev));
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = dev;
    }
}

pub fn free_vector(irq: IrqId) -> Result<(), IrqError> {
    if in_hard_irq() {
        return Err(IrqError::InIrq);
    }
    let gsi = with_irq(|s| {
        if s.table.hwirq(irq).is_none() {
            return Err(IrqError::BadVector);
        }
        Ok(irq.slot().and_then(|i| match s.routes.get(i) {
            Some(Route::IoApic { gsi, .. }) => Some(*gsi),
            _ => None,
        }))
    })?;
    if let Some(gsi) = gsi {
        apic_init::mask_gsi(gsi);
    }
    let (ctx, freed) = with_irq(|s| {
        let mut ctx = None;
        if let Some(i) = irq.slot() {
            if let Some(h) = HANDLERS.get(i) {
                // Release: pairs with the Acquire load in `dispatch`.
                h.store(0, Ordering::Release);
            }
            if let Some(r) = s.routes.get_mut(i) {
                *r = Route::None;
            }
            if let Some(m) = s.msix.get_mut(i) {
                *m = None;
            }
            if let Some(t) = s.th.top.get_mut(i) {
                *t = None;
            }
            if let Some(w) = s.th.work.get_mut(i) {
                *w = None;
            }
            if let Some(c) = s.th.ctx.get_mut(i) {
                ctx = c.take();
            }
            if let Some(p) = s.th.pending.get_mut(i) {
                *p = false;
            }
        }
        let freed = s.table.commit_free(irq)?;
        unpublish_hwirq(freed.hwirq());
        Ok((ctx, freed))
    })?;
    freed.release();
    drop(ctx);
    Ok(())
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "ROADMAP §6.3 legacy INTx fallback; only the in-guest tests route one yet"
    )
)]
pub fn set_handler(irq: IrqId, h: Handler) -> Result<(), IrqError> {
    let i = irq.slot().ok_or(IrqError::BadVector)?;
    let (chip, hwirq, wired) = with_irq(|s| {
        if s.table.hwirq(irq).is_none() {
            return Err(IrqError::BadVector);
        }
        let wired = matches!(s.routes.get(i), Some(Route::IoApic { .. } | Route::Gic));
        Ok((s.table.chip(irq), s.table.hwirq(irq), wired))
    })?;
    let Some(slot) = HANDLERS.get(i) else {
        return Err(IrqError::BadVector);
    };
    // Release: pairs with the Acquire load in `dispatch`.
    slot.store(h as usize, Ordering::Release);
    if wired && let (Some(chip), Some(hwirq)) = (chip, hwirq) {
        chip.unmask(hwirq);
    }
    Ok(())
}

/// Top half runs in hard IRQ (ack only). `work` runs in the IRQ thread.
/// Both get `ctx`, which the IRQ holds a reference to until it is set
/// again or freed.
pub fn set_threaded(
    irq: IrqId,
    top: Option<ThreadedFn>,
    work: ThreadedFn,
    ctx: Option<Instance>,
) -> Result<(), IrqError> {
    if in_hard_irq() {
        return Err(IrqError::InIrq);
    }
    let i = irq.slot().ok_or(IrqError::BadVector)?;
    let (chip, hwirq, wired, old) = thread_init::with_sched(|_| {
        with_irq(|s| {
            if s.table.hwirq(irq).is_none() {
                return Err(IrqError::BadVector);
            }
            if let Some(t) = s.th.top.get_mut(i) {
                *t = top;
            }
            if let Some(w) = s.th.work.get_mut(i) {
                *w = Some(work);
            }
            if let Some(p) = s.th.pending.get_mut(i) {
                *p = false;
            }
            let old = s.th.ctx.get_mut(i).and_then(Option::take);
            if let Some(c) = s.th.ctx.get_mut(i) {
                *c = ctx;
            }
            let wired = matches!(s.routes.get(i), Some(Route::IoApic { .. } | Route::Gic));
            Ok((s.table.chip(irq), s.table.hwirq(irq), wired, old))
        })
    })?;
    if wired && let (Some(chip), Some(hwirq)) = (chip, hwirq) {
        chip.unmask(hwirq);
    }
    drop(old);
    Ok(())
}

pub fn threaded_cpu() -> u32 {
    // Acquire: pairs with the Release store in `start_threaded`.
    THREAD_CPU.load(Ordering::Acquire)
}

fn last_online_cpu() -> u32 {
    let m = per_cpu_init::online_mask();
    if m == 0 { 0 } else { 63 - m.leading_zeros() }
}

/// The next pending IRQ's bottom half and a reference to its context.
fn take_work() -> Option<(ThreadedFn, Option<Instance>)> {
    thread_init::with_sched(|s| {
        with_irq(|st| {
            let mut i = 0usize;
            while i < irq::MAX_IRQS {
                if st.th.pending.get(i).copied() == Some(true) {
                    if let Some(p) = st.th.pending.get_mut(i) {
                        *p = false;
                    }
                    if let Some(h) = st.th.work.get(i).copied().flatten() {
                        return Some((h, st.th.ctx.get(i).cloned().flatten()));
                    }
                }
                i += 1;
            }
            s.begin_wait(&mut st.th.wq, FAR_DEADLINE);
            None
        })
    })
}

fn irq_thread() {
    let _nr = crate::sync_init::no_reclaim();
    loop {
        match take_work() {
            Some((h, ctx)) => {
                h(ctx.as_deref());
                drop(ctx);
            }
            None => thread_init::schedule(),
        }
    }
}

/// Bottom-half thread on the last online CPU (AP when SMP).
pub fn start_threaded() {
    let cpu = last_online_cpu();
    // Release: pairs with the Acquire load in `threaded_cpu`.
    THREAD_CPU.store(cpu, Ordering::Release);
    if let Err(e) = thread_init::spawn_on("irqth", irq_thread, cpu) {
        crate::klog!(
            vibeos::log::Level::Error,
            "irq: threaded bottom half on cpu{cpu} not started: {}",
            e.as_str()
        );
        return;
    }
    thread_init::with_sched(|_| {
        with_irq(|s| s.th.started = true);
    });
}

pub fn cpu_of(irq: IrqId) -> Option<u32> {
    with_irq(|s| s.table.cpu_of(irq))
}

/// Ask the chip to move the interrupt, then record dest CPU on success.
/// The chip call stays outside the IRQ lock so chip code can take it.
pub fn set_affinity(irq: IrqId, cpu: u32) -> Result<(), IrqError> {
    if in_hard_irq() {
        return Err(IrqError::InIrq);
    }
    let step = with_irq(|s| s.table.plan_affinity(irq, cpu))?;
    step.chip().set_affinity(step.hwirq(), step.cpu())?;
    with_irq(|s| s.table.set_cpu(irq, step.cpu()))
}

fn apic_id(cpu: u32) -> Option<u8> {
    // Relaxed: set before the CPU starts, fixed while it runs; pairs with nothing.
    per_cpu_init::cpu(cpu).map(|c| c.apic_id.load(Ordering::Relaxed) as u8)
}

#[cfg_attr(
    any(not(feature = "kernel_tests"), not(target_arch = "aarch64")),
    expect(
        dead_code,
        reason = "ROADMAP §6.3 MSI configuration; no driver arms MSI yet"
    )
)]
pub fn enable_msi(bdf: Bdf, cap: u8, irq: IrqId) -> Result<(), IrqError> {
    let cpu = cpu_of(irq).ok_or(IrqError::BadVector)?;
    let (chip, hwirq) = with_irq(|s| {
        Ok((
            s.table.chip(irq).ok_or(IrqError::BadVector)?,
            s.table.hwirq(irq).ok_or(IrqError::BadVector)?,
        ))
    })?;
    let msg = chip.compose_msi(hwirq, cpu)?;
    let mut hw = pci_init::HwCfg;
    let msi = pci::read_msi_cap(&mut hw, bdf, cap);
    pci::write_msi_message(&mut hw, bdf, msi, msg.addr as u32, msg.data as u16);
    pci::set_msi_enable(&mut hw, bdf, cap, true);
    if let Some(i) = irq.slot() {
        with_irq(|s| {
            if let Some(r) = s.routes.get_mut(i) {
                *r = Route::Msi;
            }
        });
    }
    Ok(())
}

#[cfg_attr(
    any(not(feature = "kernel_tests"), not(target_arch = "aarch64")),
    expect(
        dead_code,
        reason = "ROADMAP §6.3 MSI configuration; no driver arms MSI yet"
    )
)]
pub fn disable_msi(bdf: Bdf, cap: u8) {
    let mut hw = pci_init::HwCfg;
    pci::set_msi_enable(&mut hw, bdf, cap, false);
}

/// The VA of `dev`'s MSI-X table, through the claim its driver holds on
/// the BAR the table's BIR names; `None` when that BAR is not mapped
/// through a claim or is too small for entry `index`.
fn msix_table_va(dev: &Device, cap: &pci::MsixCap, index: u16) -> Option<u64> {
    let bir = cap.table_bir as usize;
    if bir >= pci::MAX_BARS {
        return None;
    }
    let r = dev.resources[bir];
    let need = cap.table_off as u64 + (index as u64 + 1) * 16;
    if r.size < need {
        return None;
    }
    let entry = dev_init::find_bdf(dev.addr)?;
    let va = dev_init::bar_va(&entry, cap.table_bir)?;
    Some(va.wrapping_add(cap.table_off as u64))
}

/// Program MSI-X table entry `index`.
///
/// # Safety
/// `table_va` is an MSI-X table that [`msix_table_va`] returned for an
/// index of at least `index`, so entry `index` lies in a mapped UC BAR
/// (invariant I49).
unsafe fn write_msix_entry(table_va: u64, index: u16, e: MsixEntry) {
    let base = table_va.wrapping_add((index as u64) * 16);
    let w = e.to_dwords();
    // SAFETY: this fn's `# Safety` (here): the entry's four dwords are
    // mapped, aligned device registers.
    unsafe {
        let p = base as *mut u32;
        // Mask first, then addr/data, then the caller's mask bit.
        p.add(3).write_volatile(1);
        p.add(0).write_volatile(w[0]);
        p.add(1).write_volatile(w[1]);
        p.add(2).write_volatile(w[2]);
        p.add(3).write_volatile(w[3]);
    }
}

/// Program MSI-X entry `table_index` from the chip's `compose_msi` and
/// enable MSI-X. It writes no `COMMAND` bit; the driver sets memory
/// decode, bus mastering and INTx disable (DEVICES.md §12.3).
pub fn enable_msix(dev: &Device, table_index: u16, irq: IrqId) -> Result<(), IrqError> {
    let cpu = cpu_of(irq).ok_or(IrqError::BadVector)?;
    let (chip, hwirq) = with_irq(|s| {
        Ok((
            s.table.chip(irq).ok_or(IrqError::BadVector)?,
            s.table.hwirq(irq).ok_or(IrqError::BadVector)?,
        ))
    })?;
    let msg = chip.compose_msi(hwirq, cpu)?;
    let Some(cap_off) = dev.caps.msix else {
        return Err(IrqError::NoRoute);
    };
    let mut hw = pci_init::HwCfg;
    let cap = pci::read_msix_cap(&mut hw, dev.addr, cap_off);
    if table_index as u32 >= cap.table_size as u32 {
        return Err(IrqError::BadVector);
    }
    let Some(table) = msix_table_va(dev, &cap, table_index) else {
        return Err(IrqError::NoRoute);
    };
    // SAFETY: invariant I49, established by `irq::irq_init::msix_table_va`:
    // it returned `table` for `table_index`, so the entry lies inside the
    // mapped BAR.
    unsafe {
        write_msix_entry(
            table,
            table_index,
            MsixEntry::new(msg.addr, msg.data, false),
        );
    }
    pci::set_msix_enable(&mut hw, dev.addr, cap_off, true, false);
    chip.unmask(hwirq);
    if let Some(i) = irq.slot() {
        with_irq(|s| {
            if let Some(m) = s.msix.get_mut(i) {
                *m = Some(MsixLive {
                    table,
                    index: table_index,
                });
            }
            if let Some(r) = s.routes.get_mut(i) {
                *r = Route::Msix;
            }
        });
    }
    Ok(())
}

pub fn disable_msix(dev: &Device) {
    let Some(cap_off) = dev.caps.msix else {
        return;
    };
    let mut hw = pci_init::HwCfg;
    pci::set_msix_enable(&mut hw, dev.addr, cap_off, false, true);
}

/// Install chips, reserve keyboard `0x30`, install pool stubs `0x31..=0x7F`.
pub fn init() {
    // SAFETY: invariant I22, established at `cell::BootCell::set`: this fn
    // runs once on the BSP before `smp: done`; established here.
    unsafe {
        PIC.set(&PIC_OBJ as &'static dyn IrqChip);
        IOAPIC.set(&IOAPIC_OBJ as &'static dyn IrqChip);
        LAPIC_MSI.set(&LAPIC_MSI_OBJ as &'static dyn IrqChip);
    }
    // A fresh pool always has the keyboard's vector free; a failure means
    // the pool is wrong, and the keyboard alone is left without its vector.
    if let Err(e) = with_pool(|p| p.reserve(vectors::KBD, 0)) {
        crate::klog!(
            vibeos::log::Level::Error,
            "vibeOS: irq: keyboard vector {:#x} not reserved: {:?}",
            vectors::KBD,
            e
        );
    }
    install_pool_stubs();
    #[cfg(target_arch = "aarch64")]
    {
        crate::arch::aarch64::gic::set_dispatch(dispatch_intid);
        let intid = crate::arch::aarch64::timer::intid();
        if intid != 0 {
            match map_percpu(IrqSpecifier::Gic { intid }) {
                Ok(_) => {}
                Err(e) => crate::klog!(
                    vibeos::log::Level::Error,
                    "vibeOS: irq: timer ppi {intid}: {}",
                    e.as_str()
                ),
            }
        }
    }
}

fn install_pool_stubs() {
    // 0x30 is the keyboard's. The dispatcher owns the rest of the pool.
    for v in vectors::DEVICE_VEC_START..=vectors::DEVICE_VEC_END {
        arch::idt::set_handler(v, device_irq);
    }
}

fn gsi_of(hwirq: u32) -> Option<IoApicBind> {
    let vec = u8::try_from(hwirq).ok()?;
    let i = pool_index(vec)?;
    with_irq(|s| s.gsi_bind.get(i).copied().flatten())
}

impl IrqChip for PicChip {
    fn translate(&self, spec: IrqSpecifier, _cpu: u32) -> Result<u32, IrqError> {
        match spec {
            IrqSpecifier::Isa { line } if line < 16 => {
                Ok(u32::from(vectors::IRQ_BASE).saturating_add(u32::from(line)))
            }
            _ => Err(IrqError::NoRoute),
        }
    }

    fn mask(&self, hwirq: u32) {
        if let Ok(v) = u8::try_from(hwirq)
            && let Some(line) = arch::pic::line_of(v)
        {
            arch::pic::mask(line);
        }
    }

    fn unmask(&self, hwirq: u32) {
        if let Ok(v) = u8::try_from(hwirq)
            && let Some(line) = arch::pic::line_of(v)
        {
            arch::pic::unmask(line);
        }
    }

    fn eoi(&self, hwirq: u32) {
        if let Ok(v) = u8::try_from(hwirq) {
            apic_init::eoi_for(v);
        }
    }

    fn set_affinity(&self, _hwirq: u32, _cpu: u32) -> Result<(), IrqError> {
        Err(IrqError::BadCpu)
    }

    fn alloc_msi(&self, _n: u8, _cpu: u32, _out: &mut [u32]) -> Result<usize, IrqError> {
        Err(IrqError::NoRoute)
    }

    fn compose_msi(&self, _hwirq: u32, _cpu: u32) -> Result<MsiMessage, IrqError> {
        Err(IrqError::NoRoute)
    }

    fn free(&self, _hwirq: u32) {}
}

impl IrqChip for IoApicChip {
    fn translate(&self, spec: IrqSpecifier, cpu: u32) -> Result<u32, IrqError> {
        let IrqSpecifier::Gsi {
            gsi,
            trigger,
            polarity,
        } = spec
        else {
            return Err(IrqError::NoRoute);
        };
        with_irq(|s| {
            let vec = s.pool.allocate(cpu)?;
            if let Some(i) = pool_index(vec)
                && let Some(b) = s.gsi_bind.get_mut(i)
            {
                *b = Some(IoApicBind {
                    gsi,
                    trigger,
                    polarity,
                });
            }
            Ok(u32::from(vec))
        })
    }

    fn mask(&self, hwirq: u32) {
        if let Some(b) = gsi_of(hwirq) {
            apic_init::mask_gsi(b.gsi);
        }
    }

    fn unmask(&self, hwirq: u32) {
        if let Some(b) = gsi_of(hwirq) {
            apic_init::unmask_gsi(b.gsi);
        }
    }

    fn eoi(&self, _hwirq: u32) {
        apic_init::eoi();
    }

    fn set_affinity(&self, hwirq: u32, cpu: u32) -> Result<(), IrqError> {
        let vec = u8::try_from(hwirq).map_err(|_| IrqError::BadVector)?;
        let bind = with_irq(|s| {
            s.pool.set_affinity(vec, cpu)?;
            let i = pool_index(vec).ok_or(IrqError::BadVector)?;
            s.gsi_bind
                .get(i)
                .copied()
                .flatten()
                .ok_or(IrqError::BadVector)
        })?;
        let dest = apic_id(cpu).ok_or(IrqError::BadCpu)?;
        apic_init::route_gsi(bind.gsi, vec, dest, bind.trigger, bind.polarity)
            .map_err(|_| IrqError::BadCpu)?;
        apic_init::unmask_gsi(bind.gsi);
        Ok(())
    }

    fn alloc_msi(&self, _n: u8, _cpu: u32, _out: &mut [u32]) -> Result<usize, IrqError> {
        Err(IrqError::NoRoute)
    }

    fn compose_msi(&self, _hwirq: u32, _cpu: u32) -> Result<MsiMessage, IrqError> {
        Err(IrqError::NoRoute)
    }

    fn free(&self, hwirq: u32) {
        let Ok(vec) = u8::try_from(hwirq) else {
            return;
        };
        let gsi = with_irq(|s| {
            let gsi = pool_index(vec)
                .and_then(|i| s.gsi_bind.get_mut(i).and_then(Option::take).map(|b| b.gsi));
            #[expect(
                clippy::let_underscore_must_use,
                reason = "pool free on teardown: a slot that fails to free stays allocated (DESIGN §2.5)"
            )]
            let _ = s.pool.free(vec);
            gsi
        });
        if let Some(gsi) = gsi {
            apic_init::mask_gsi(gsi);
        }
    }
}

impl IrqChip for LapicMsiChip {
    fn translate(&self, spec: IrqSpecifier, _cpu: u32) -> Result<u32, IrqError> {
        match spec {
            IrqSpecifier::LapicLvt(lvt) => Ok(lvt.hwirq()),
            _ => Err(IrqError::NoRoute),
        }
    }

    fn mask(&self, _hwirq: u32) {}

    fn unmask(&self, _hwirq: u32) {}

    fn eoi(&self, _hwirq: u32) {
        apic_init::eoi();
    }

    fn set_affinity(&self, hwirq: u32, cpu: u32) -> Result<(), IrqError> {
        let vec = u8::try_from(hwirq).map_err(|_| IrqError::BadVector)?;
        if !in_pool(vec) {
            return Ok(());
        }
        // A CPU with no APIC id cannot be the message's destination.
        // Updating the pool anyway would make `cpu_of` lie.
        if apic_id(cpu).is_none() {
            return Err(IrqError::BadCpu);
        }
        let msg = self.compose_msi(hwirq, cpu)?;
        let live = with_irq(|s| {
            s.pool.set_affinity(vec, cpu)?;
            // Acquire: pairs with the Release store in `publish_vec`.
            let raw = VEC_TO_IRQ
                .get(usize::from(vec))
                .map(|a| a.load(Ordering::Acquire))
                .unwrap_or(0);
            let slot = IrqId::from_raw(raw).slot();
            Ok(slot.and_then(|i| s.msix.get(i).copied().flatten()))
        })?;
        if let Some(live) = live {
            // SAFETY: invariant I49, established by `irq::irq_init::enable_msix`:
            // `live.table` is the MSI-X table VA that call wrote for `live.index`.
            unsafe {
                write_msix_entry(
                    live.table,
                    live.index,
                    MsixEntry::new(msg.addr, msg.data, false),
                );
            }
        }
        Ok(())
    }

    fn alloc_msi(&self, n: u8, cpu: u32, out: &mut [u32]) -> Result<usize, IrqError> {
        if n == 0 || out.len() < n as usize {
            return Err(IrqError::BadVector);
        }
        with_pool(|p| {
            let mut got = 0usize;
            while got < n as usize {
                match p.allocate(cpu) {
                    Ok(v) => {
                        if let Some(slot) = out.get_mut(got) {
                            *slot = u32::from(v);
                        }
                        got += 1;
                    }
                    Err(e) => {
                        let mut j = 0usize;
                        while j < got {
                            if let Some(h) = out.get(j)
                                && let Ok(v) = u8::try_from(*h)
                            {
                                #[expect(
                                    clippy::let_underscore_must_use,
                                    reason = "unwind of a failed MSI alloc: a slot that fails to free stays allocated (DESIGN §2.5)"
                                )]
                                let _ = p.free(v);
                            }
                            j += 1;
                        }
                        return Err(e);
                    }
                }
            }
            Ok(got)
        })
    }

    fn compose_msi(&self, hwirq: u32, cpu: u32) -> Result<MsiMessage, IrqError> {
        let dest = apic_id(cpu).ok_or(IrqError::BadCpu)?;
        let vec = u8::try_from(hwirq).map_err(|_| IrqError::BadVector)?;
        Ok(MsiMessage {
            addr: u64::from(msi_message_addr(dest)),
            data: msi_message_data(vec),
        })
    }

    fn free(&self, hwirq: u32) {
        let Ok(vec) = u8::try_from(hwirq) else {
            return;
        };
        if in_pool(vec) {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "pool free on teardown: a slot that fails to free stays allocated (DESIGN §2.5)"
            )]
            let _ = with_pool(|p| p.free(vec));
        }
    }
}
