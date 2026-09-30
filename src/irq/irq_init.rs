//! Device IRQ dispatcher, vector allocation, MSI/MSI-X enable. DESIGN §5.4.
//!
//! EOI is here. Hard-IRQ `fn()` acks and wakes; threaded handlers may
//! allocate and block. Allocate is refused in a hard-IRQ (the dispatcher
//! flag, not `InterruptGuard`).

use core::any::Any;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use vibeos::apic::{Polarity, Trigger};
use vibeos::dev::{Device, Instance};
use vibeos::irq::{
    self, IrqError, MsixEntry, VectorPool, in_pool, msi_message_addr, msi_message_data, pool_index,
};
use vibeos::lock::RANK_DEVICE;
use vibeos::pci::{self, Bdf};
use vibeos::sched::FAR_DEADLINE;
use vibeos::vectors;
use vibeos::wait::WaitQueue;

use super::hardirq;
use crate::apic_init;
use crate::arch;
use crate::pci_init;
use crate::per_cpu_init;
use crate::sync_init::SpinMutex;
use crate::thread_init;

type Handler = fn();

/// A threaded handler half. It gets the context its vector was set with
/// (a driver instance, DESIGN §12.1 rule 1), so no driver keeps a table of
/// its devices to find the one that interrupted.
pub type ThreadedFn = fn(Option<&(dyn Any + Send + Sync)>);

pub(super) struct IrqState {
    pool: VectorPool,
    routes: [Route; irq::POOL_LEN],
    pub(super) th: Threaded,
}

static IRQ: SpinMutex<IrqState> = SpinMutex::with_rank(
    IrqState {
        pool: VectorPool::new(),
        routes: [Route::None; irq::POOL_LEN],
        th: Threaded {
            wq: WaitQueue::new(),
            pending: [false; irq::POOL_LEN],
            top: [None; irq::POOL_LEN],
            work: [None; irq::POOL_LEN],
            ctx: [const { None }; irq::POOL_LEN],
            started: false,
        },
    },
    RANK_DEVICE,
);

/// Run `f` on the vector pool, routes and threaded state.
pub(super) fn with_irq<R>(f: impl FnOnce(&mut IrqState) -> R) -> R {
    let mut g = IRQ.lock();
    f(&mut g)
}
static HANDLERS: [AtomicUsize; irq::POOL_LEN] = [const { AtomicUsize::new(0) }; irq::POOL_LEN];

#[derive(Clone, Copy)]
enum Route {
    None,
    #[cfg_attr(
        not(feature = "kernel_tests"),
        expect(
            dead_code,
            reason = "ROADMAP §6.3 legacy INTx fallback; only the in-guest tests route one yet"
        )
    )]
    IoApic {
        gsi: u32,
        trigger: Trigger,
        polarity: Polarity,
    },
    #[expect(
        dead_code,
        reason = "ROADMAP §6.3 MSI configuration: only `enable_msi` builds it"
    )]
    Msi,
    Msix,
}

pub(super) struct Threaded {
    wq: WaitQueue,
    pub(super) pending: [bool; irq::POOL_LEN],
    pub(super) top: [Option<ThreadedFn>; irq::POOL_LEN],
    pub(super) work: [Option<ThreadedFn>; irq::POOL_LEN],
    /// Each vector's context, which both halves get a reference to.
    pub(super) ctx: [Option<Instance>; irq::POOL_LEN],
    started: bool,
}

static THREAD_CPU: AtomicU32 = AtomicU32::new(0);

pub(super) fn with_pool<R>(f: impl FnOnce(&mut VectorPool) -> R) -> R {
    with_irq(|s| f(&mut s.pool))
}

pub(super) fn handler_slot(vec: u8) -> Option<usize> {
    pool_index(vec)
}

pub use super::hardirq::in_hard_irq;

fn device_irq(frame: &mut arch::idt::TrapFrame) {
    dispatch(frame.vector as u8);
}

pub fn dispatch(vec: u8) {
    hardirq::set(true);
    if let Some(i) = handler_slot(vec) {
        let (top, work, ctx) = with_irq(|s| (s.th.top[i], s.th.work[i], s.th.ctx[i].clone()));
        if work.is_some() || top.is_some() {
            if let Some(h) = top {
                h(ctx.as_deref());
            }
            thread_init::with_sched(|sched| {
                with_irq(|s| {
                    s.th.pending[i] = true;
                    if s.th.started {
                        sched.wake_one(&mut s.th.wq);
                    }
                });
            });
            // A count, never the last while the vector holds its own; a
            // last put in hard IRQ defers (DESIGN §2.11 rule 6).
            drop(ctx);
        } else {
            let p = HANDLERS[i].load(Ordering::Acquire);
            if p != 0 {
                // SAFETY: invariant: a nonzero `HANDLERS` slot holds a
                // `fn()`; established by `irq::irq_init::set_handler`, its
                // only nonzero store.
                let h: Handler = unsafe { core::mem::transmute(p) };
                h();
            }
        }
    }
    apic_init::eoi_for(vec);
    hardirq::set(false);
}

pub fn allocate_vector(cpu: u32) -> Result<u8, IrqError> {
    if in_hard_irq() {
        return Err(IrqError::InIrq);
    }
    with_pool(|p| p.allocate(cpu))
}

pub fn free_vector(vec: u8) -> Result<(), IrqError> {
    if in_hard_irq() {
        return Err(IrqError::InIrq);
    }
    let gsi = with_irq(|s| {
        handler_slot(vec).and_then(|i| match s.routes[i] {
            Route::IoApic { gsi, .. } => Some(gsi),
            Route::None | Route::Msi | Route::Msix => None,
        })
    });
    if let Some(gsi) = gsi {
        apic_init::mask_gsi(gsi);
    }
    let (res, ctx) = with_irq(|s| {
        let mut ctx = None;
        if let Some(i) = handler_slot(vec) {
            HANDLERS[i].store(0, Ordering::Release);
            s.routes[i] = Route::None;
            s.th.top[i] = None;
            s.th.work[i] = None;
            ctx = s.th.ctx[i].take();
            s.th.pending[i] = false;
        }
        (s.pool.free(vec), ctx)
    });
    drop(ctx);
    res
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "ROADMAP §6.3 legacy INTx fallback; only the in-guest tests route one yet"
    )
)]
pub fn set_handler(vec: u8, h: Handler) -> Result<(), IrqError> {
    let Some(i) = handler_slot(vec) else {
        return Err(IrqError::BadVector);
    };
    HANDLERS[i].store(h as usize, Ordering::Release);
    Ok(())
}

/// Top half runs in hard IRQ (ack only). `work` runs in the IRQ thread.
/// Both get `ctx`, which the vector holds a reference to until it is set
/// again or freed.
pub fn set_threaded(
    vec: u8,
    top: Option<ThreadedFn>,
    work: ThreadedFn,
    ctx: Option<Instance>,
) -> Result<(), IrqError> {
    if in_hard_irq() {
        return Err(IrqError::InIrq);
    }
    let Some(i) = handler_slot(vec) else {
        return Err(IrqError::BadVector);
    };
    let old = thread_init::with_sched(|_| {
        with_irq(|s| {
            s.th.top[i] = top;
            s.th.work[i] = Some(work);
            s.th.pending[i] = false;
            core::mem::replace(&mut s.th.ctx[i], ctx)
        })
    });
    drop(old);
    Ok(())
}

pub fn threaded_cpu() -> u32 {
    THREAD_CPU.load(Ordering::Acquire)
}

fn last_online_cpu() -> u32 {
    let m = per_cpu_init::online_mask();
    if m == 0 { 0 } else { 63 - m.leading_zeros() }
}

/// The next pending vector's bottom half and a reference to its context.
fn take_work() -> Option<(ThreadedFn, Option<Instance>)> {
    thread_init::with_sched(|s| {
        with_irq(|st| {
            let mut i = 0usize;
            while i < irq::POOL_LEN {
                if st.th.pending[i] {
                    st.th.pending[i] = false;
                    if let Some(h) = st.th.work[i] {
                        return Some((h, st.th.ctx[i].clone()));
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

#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "ROADMAP §6.3 interrupt affinity API; only the in-guest tests call it yet"
    )
)]
pub fn cpu_of(vec: u8) -> Option<u32> {
    with_pool(|p| p.cpu_of(vec))
}

/// Record dest CPU. IOAPIC routes are rewritten. MSI/MSI-X callers
/// reprogram the message from [`cpu_of`].
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "ROADMAP §6.3 interrupt affinity API; only the in-guest tests call it yet"
    )
)]
pub fn set_affinity(vec: u8, cpu: u32) -> Result<(), IrqError> {
    if in_hard_irq() {
        return Err(IrqError::InIrq);
    }
    with_pool(|p| p.set_affinity(vec, cpu))?;
    let Some(i) = handler_slot(vec) else {
        return Err(IrqError::BadVector);
    };
    let dest = apic_id(cpu).ok_or(IrqError::BadCpu)?;
    let route = with_irq(|s| s.routes[i]);
    match route {
        Route::IoApic {
            gsi,
            trigger,
            polarity,
        } => {
            apic_init::route_gsi(gsi, vec, dest, trigger, polarity)
                .map_err(|_| IrqError::BadCpu)?;
            apic_init::unmask_gsi(gsi);
        }
        Route::None | Route::Msi | Route::Msix => {}
    }
    Ok(())
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "ROADMAP §6.3: `set_affinity` and `route_intx` resolve a CPU's APIC id"
    )
)]
fn apic_id(cpu: u32) -> Option<u8> {
    per_cpu_init::cpu(cpu).map(|c| c.apic_id.load(Ordering::Relaxed) as u8)
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "ROADMAP §6.3 legacy INTx fallback; only the in-guest tests route one yet"
    )
)]
pub fn route_intx(
    gsi: u32,
    vec: u8,
    cpu: u32,
    trigger: Trigger,
    polarity: Polarity,
) -> Result<(), IrqError> {
    let dest = apic_id(cpu).ok_or(IrqError::BadCpu)?;
    apic_init::route_gsi(gsi, vec, dest, trigger, polarity).map_err(|_| IrqError::BadCpu)?;
    let Some(i) = handler_slot(vec) else {
        return Err(IrqError::BadVector);
    };
    with_irq(|s| {
        s.routes[i] = Route::IoApic {
            gsi,
            trigger,
            polarity,
        };
    });
    apic_init::unmask_gsi(gsi);
    Ok(())
}

#[expect(
    dead_code,
    reason = "ROADMAP §6.3 MSI configuration; no driver arms MSI yet"
)]
pub fn enable_msi(bdf: Bdf, cap: u8, vector: u8, apic_id: u8) -> Result<(), IrqError> {
    if !in_pool(vector) {
        return Err(IrqError::BadVector);
    }
    let mut hw = pci_init::HwCfg;
    let msi = pci::read_msi_cap(&mut hw, bdf, cap);
    pci::write_msi_message(
        &mut hw,
        bdf,
        msi,
        msi_message_addr(apic_id),
        msi_message_data(vector) as u16,
    );
    pci::set_msi_enable(&mut hw, bdf, cap, true);
    if let Some(i) = handler_slot(vector) {
        with_irq(|s| s.routes[i] = Route::Msi);
    }
    Ok(())
}

#[expect(
    dead_code,
    reason = "ROADMAP §6.3 MSI configuration; no driver arms MSI yet"
)]
pub fn disable_msi(bdf: Bdf, cap: u8) {
    let mut hw = pci_init::HwCfg;
    pci::set_msi_enable(&mut hw, bdf, cap, false);
}

fn msix_table_va(dev: &Device, cap: &pci::MsixCap, index: u16) -> Option<u64> {
    let bir = cap.table_bir as usize;
    if bir >= pci::MAX_BARS {
        return None;
    }
    let r = dev.resources[bir];
    let need = cap.table_off as u64 + (index as u64 + 1) * 16;
    if r.mapped_va == 0 || r.size < need {
        return None;
    }
    Some(r.mapped_va.wrapping_add(cap.table_off as u64))
}

/// Program MSI-X table entry `index`.
///
/// # Safety
/// `table_va` is an MSI-X table that [`msix_table_va`] returned for an
/// index of at least `index`, so entry `index` lies in a mapped UC BAR
/// (invariant I228).
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

/// Program MSI-X entry `table_index` for `vector` on `apic_id` and enable
/// MSI-X. It writes no `COMMAND` bit; the driver sets memory decode, bus
/// mastering and INTx disable (DEVICES.md §12.3).
pub fn enable_msix(
    dev: &Device,
    table_index: u16,
    vector: u8,
    apic_id: u8,
) -> Result<(), IrqError> {
    if !in_pool(vector) {
        return Err(IrqError::BadVector);
    }
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
    // SAFETY: invariant I228, established by `irq::irq_init::msix_table_va`:
    // it returned `table` for `table_index`, so the entry lies inside the
    // mapped BAR.
    unsafe {
        write_msix_entry(
            table,
            table_index,
            MsixEntry::for_lapic(vector, apic_id, false),
        );
    }
    pci::set_msix_enable(&mut hw, dev.addr, cap_off, true, false);
    if let Some(i) = handler_slot(vector) {
        with_irq(|s| s.routes[i] = Route::Msix);
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

/// Reserve keyboard `0x30`, install pool stubs `0x31..=0x7F`.
pub fn init() {
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
}

fn install_pool_stubs() {
    // 0x30 is the keyboard's. The dispatcher owns the rest of the pool.
    for v in vectors::DEVICE_VEC_START..=vectors::DEVICE_VEC_END {
        arch::idt::set_handler(v, device_irq);
    }
}
