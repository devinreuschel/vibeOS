//! Device IRQ dispatcher, vector allocation, MSI/MSI-X enable. DESIGN §5.4.
//!
//! EOI is here. Hard-IRQ `fn()` acks and wakes; threaded handlers may
//! allocate and block. Allocate is refused in a hard-IRQ (the dispatcher
//! flag, not `InterruptGuard`).

#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use vibeos::apic::{Polarity, Trigger};
use vibeos::dev::Device;
use vibeos::irq::{
    self, IrqError, MsixEntry, VectorPool, in_pool, msi_message_addr, msi_message_data, pool_index,
};
use vibeos::pci::{self, Bdf};
use vibeos::sched::FAR_DEADLINE;
use vibeos::vectors;
use vibeos::wait::WaitQueue;

use crate::apic_init;
use crate::arch;
use crate::cell::IrqCell;
use crate::pci_init;
use crate::per_cpu_init;
use crate::thread_init;

type Handler = fn();

struct IrqState {
    pool: VectorPool,
    routes: [Route; irq::POOL_LEN],
    th: Threaded,
}

static IRQ: IrqCell<IrqState> = IrqCell::new(IrqState {
    pool: VectorPool::new(),
    routes: [Route::None; irq::POOL_LEN],
    th: Threaded {
        wq: WaitQueue::new(),
        pending: [false; irq::POOL_LEN],
        top: [0; irq::POOL_LEN],
        work: [0; irq::POOL_LEN],
        started: false,
    },
});
static HANDLERS: [AtomicUsize; irq::POOL_LEN] = [const { AtomicUsize::new(0) }; irq::POOL_LEN];
static IN_ISR: [AtomicBool; 64] = [const { AtomicBool::new(false) }; 64];

#[derive(Clone, Copy)]
enum Route {
    None,
    IoApic {
        gsi: u32,
        trigger: Trigger,
        polarity: Polarity,
    },
    #[allow(dead_code)] // enable_msi; virtio/C will arm it
    Msi,
    Msix,
}

struct Threaded {
    wq: WaitQueue,
    pending: [bool; irq::POOL_LEN],
    top: [usize; irq::POOL_LEN],
    work: [usize; irq::POOL_LEN],
    started: bool,
}

static THREAD_CPU: AtomicU32 = AtomicU32::new(0);

fn with_pool<R>(f: impl FnOnce(&mut VectorPool) -> R) -> R {
    IRQ.with(|s| f(&mut s.pool))
}

fn handler_slot(vec: u8) -> Option<usize> {
    pool_index(vec)
}

pub fn in_hard_irq() -> bool {
    let cpu = per_cpu_init::try_current().map(|c| c.cpu_id).unwrap_or(0);
    if (cpu as usize) < IN_ISR.len() {
        IN_ISR[cpu as usize].load(Ordering::Relaxed)
    } else {
        false
    }
}

fn set_in_isr(on: bool) {
    let cpu = per_cpu_init::try_current().map(|c| c.cpu_id).unwrap_or(0);
    if (cpu as usize) < IN_ISR.len() {
        IN_ISR[cpu as usize].store(on, Ordering::Relaxed);
    }
}

fn device_irq(frame: &mut arch::idt::TrapFrame) {
    dispatch(frame.vector as u8);
}

pub fn dispatch(vec: u8) {
    set_in_isr(true);
    if let Some(i) = handler_slot(vec) {
        let (top, work) = IRQ.with(|s| (s.th.top[i], s.th.work[i]));
        if work != 0 || top != 0 {
            if top != 0 {
                let h: Handler = unsafe { core::mem::transmute(top) };
                h();
            }
            thread_init::with_sched(|sched| {
                IRQ.with(|s| {
                    s.th.pending[i] = true;
                    if s.th.started {
                        sched.wake_one(&mut s.th.wq);
                    }
                });
            });
        } else {
            let p = HANDLERS[i].load(Ordering::Acquire);
            if p != 0 {
                let h: Handler = unsafe { core::mem::transmute(p) };
                h();
            }
        }
    }
    apic_init::eoi_for(vec);
    set_in_isr(false);
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
    let gsi = IRQ.with(|s| {
        handler_slot(vec).and_then(|i| match s.routes[i] {
            Route::IoApic { gsi, .. } => Some(gsi),
            Route::None | Route::Msi | Route::Msix => None,
        })
    });
    if let Some(gsi) = gsi {
        apic_init::mask_gsi(gsi);
    }
    IRQ.with(|s| {
        if let Some(i) = handler_slot(vec) {
            HANDLERS[i].store(0, Ordering::Release);
            s.routes[i] = Route::None;
            s.th.top[i] = 0;
            s.th.work[i] = 0;
            s.th.pending[i] = false;
        }
        s.pool.free(vec)
    })
}

pub fn set_handler(vec: u8, h: Handler) -> Result<(), IrqError> {
    let Some(i) = handler_slot(vec) else {
        return Err(IrqError::BadVector);
    };
    HANDLERS[i].store(h as usize, Ordering::Release);
    Ok(())
}

/// Top half runs in hard IRQ (ack only). `work` runs in the IRQ thread.
pub fn set_threaded(vec: u8, top: Option<Handler>, work: Handler) -> Result<(), IrqError> {
    if in_hard_irq() {
        return Err(IrqError::InIrq);
    }
    let Some(i) = handler_slot(vec) else {
        return Err(IrqError::BadVector);
    };
    thread_init::with_sched(|_| {
        IRQ.with(|s| {
            s.th.top[i] = top.map(|f| f as usize).unwrap_or(0);
            s.th.work[i] = work as usize;
            s.th.pending[i] = false;
        });
    });
    Ok(())
}

pub fn threaded_cpu() -> u32 {
    THREAD_CPU.load(Ordering::Acquire)
}

fn last_online_cpu() -> u32 {
    let m = per_cpu_init::online_mask();
    if m == 0 { 0 } else { 63 - m.leading_zeros() }
}

fn take_work() -> Option<Handler> {
    thread_init::with_sched(|s| {
        IRQ.with(|st| {
            let mut i = 0usize;
            while i < irq::POOL_LEN {
                if st.th.pending[i] {
                    st.th.pending[i] = false;
                    let p = st.th.work[i];
                    if p != 0 {
                        let h: Handler = unsafe { core::mem::transmute(p) };
                        return Some(h);
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
    loop {
        match take_work() {
            Some(h) => h(),
            None => thread_init::schedule(),
        }
    }
}

/// Bottom-half thread on the last online CPU (AP when SMP).
pub fn start_threaded() {
    let cpu = last_online_cpu();
    THREAD_CPU.store(cpu, Ordering::Release);
    let _ = thread_init::spawn_on("irqth", irq_thread, cpu);
    thread_init::with_sched(|_| {
        IRQ.with(|s| s.th.started = true);
    });
}

pub fn cpu_of(vec: u8) -> Option<u32> {
    with_pool(|p| p.cpu_of(vec))
}

/// Record dest CPU. IOAPIC routes are rewritten. MSI/MSI-X callers
/// reprogram the message from [`cpu_of`].
pub fn set_affinity(vec: u8, cpu: u32) -> Result<(), IrqError> {
    if in_hard_irq() {
        return Err(IrqError::InIrq);
    }
    with_pool(|p| p.set_affinity(vec, cpu))?;
    let Some(i) = handler_slot(vec) else {
        return Err(IrqError::BadVector);
    };
    let dest = apic_id(cpu).ok_or(IrqError::BadCpu)?;
    let route = IRQ.with(|s| s.routes[i]);
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

fn apic_id(cpu: u32) -> Option<u8> {
    per_cpu_init::cpu(cpu).map(|c| c.apic_id.load(Ordering::Relaxed) as u8)
}

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
    IRQ.with(|s| {
        s.routes[i] = Route::IoApic {
            gsi,
            trigger,
            polarity,
        };
    });
    apic_init::unmask_gsi(gsi);
    Ok(())
}

pub fn mask_intx(bdf: Bdf, disable: bool) {
    let mut cmd = pci_init::cfg_read16(bdf, pci::CFG_COMMAND);
    cmd = if disable {
        pci::with_intx_disabled(cmd)
    } else {
        cmd & !pci::CMD_INTX_DISABLE
    };
    pci_init::cfg_write_command(bdf, cmd);
}

#[allow(dead_code)]
pub fn enable_msi(bdf: Bdf, cap: u8, vector: u8, apic_id: u8) -> Result<(), IrqError> {
    if !in_pool(vector) {
        return Err(IrqError::BadVector);
    }
    pci_init::enable_mem_master(bdf);
    let mut hw = pci_init::HwCfg;
    let msi = pci::read_msi_cap(&mut hw, bdf, cap);
    pci::write_msi_message(
        &mut hw,
        bdf,
        msi,
        msi_message_addr(apic_id),
        msi_message_data(vector) as u16,
    );
    mask_intx(bdf, true);
    pci::set_msi_enable(&mut hw, bdf, cap, true);
    if let Some(i) = handler_slot(vector) {
        IRQ.with(|s| s.routes[i] = Route::Msi);
    }
    Ok(())
}

#[allow(dead_code)]
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

fn write_msix_entry(table_va: u64, index: u16, e: MsixEntry) {
    let base = table_va.wrapping_add((index as u64) * 16);
    let w = e.to_dwords();
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
    pci_init::enable_mem_master(dev.addr);
    write_msix_entry(
        table,
        table_index,
        MsixEntry::for_lapic(vector, apic_id, false),
    );
    mask_intx(dev.addr, true);
    pci::set_msix_enable(&mut hw, dev.addr, cap_off, true, false);
    if let Some(i) = handler_slot(vector) {
        IRQ.with(|s| s.routes[i] = Route::Msix);
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
    let _ = with_pool(|p| p.reserve(vectors::KBD, 0));
    install_pool_stubs();
}

fn install_pool_stubs() {
    // 0x30 is the keyboard's. The dispatcher owns the rest of the pool.
    for v in vectors::DEVICE_VEC_START..=vectors::DEVICE_VEC_END {
        arch::idt::set_handler(v, device_irq);
    }
}

#[cfg(feature = "kernel_tests")]
pub fn allocated_count() -> usize {
    with_pool(|p| p.allocated())
}

#[cfg(feature = "kernel_tests")]
pub fn has_threaded(vec: u8) -> bool {
    match handler_slot(vec) {
        Some(i) => IRQ.with(|s| {
            let t = &s.th;
            t.top[i] != 0 || t.work[i] != 0 || t.pending[i]
        }),
        None => false,
    }
}
