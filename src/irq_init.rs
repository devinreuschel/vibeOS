//! Device IRQ dispatcher, vector allocation, MSI/MSI-X enable. DESIGN §5.4.
//!
//! EOI is here. Drivers register `fn()` only. Allocate is refused in a
//! hard-IRQ (the dispatcher flag, not `InterruptGuard`).

#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use vibeos::apic::{Polarity, Trigger};
use vibeos::desc::InterruptFrame;
use vibeos::dev::Device;
use vibeos::irq::{
    self, in_pool, msi_message_addr, msi_message_data, pool_index, IrqError, MsixEntry, VectorPool,
    POOL_END, POOL_START,
};
use vibeos::pci::{self, Bdf};
use vibeos::vectors;

use crate::apic_init;
use crate::arch;
use crate::pci_init;
use crate::per_cpu_init;
use crate::x86::InterruptGuard;

type Handler = fn();

struct Cell<T>(core::cell::UnsafeCell<T>);
unsafe impl<T> Sync for Cell<T> {}

static POOL: Cell<VectorPool> = Cell(core::cell::UnsafeCell::new(VectorPool::new()));
static POOL_LOCK: AtomicBool = AtomicBool::new(false);
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
    Msi,
    Msix,
}

static ROUTES: Cell<[Route; irq::POOL_LEN]> =
    Cell(core::cell::UnsafeCell::new([Route::None; irq::POOL_LEN]));

fn pool() -> &'static mut VectorPool {
    unsafe { &mut *POOL.0.get() }
}

fn routes() -> &'static mut [Route; irq::POOL_LEN] {
    unsafe { &mut *ROUTES.0.get() }
}

fn with_pool<R>(f: impl FnOnce(&mut VectorPool) -> R) -> R {
    let _irq = InterruptGuard::enter();
    while POOL_LOCK
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    let r = f(pool());
    POOL_LOCK.store(false, Ordering::Release);
    r
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

extern "x86-interrupt" fn device_irq<const N: u8>(_frame: InterruptFrame) {
    dispatch(N);
}

pub fn dispatch(vec: u8) {
    set_in_isr(true);
    if let Some(i) = handler_slot(vec) {
        let p = HANDLERS[i].load(Ordering::Acquire);
        if p != 0 {
            let h: Handler = unsafe { core::mem::transmute(p) };
            h();
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
    if let Some(i) = handler_slot(vec) {
        HANDLERS[i].store(0, Ordering::Release);
        routes()[i] = Route::None;
    }
    with_pool(|p| p.free(vec))
}

pub fn set_handler(vec: u8, h: Handler) -> Result<(), IrqError> {
    let Some(i) = handler_slot(vec) else {
        return Err(IrqError::BadVector);
    };
    HANDLERS[i].store(h as usize, Ordering::Release);
    Ok(())
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
    match routes()[i] {
        Route::IoApic {
            gsi,
            trigger,
            polarity,
        } => {
            apic_init::route_gsi(gsi, vec, dest, trigger, polarity).map_err(|_| IrqError::BadCpu)?;
            apic_init::unmask_gsi(gsi);
        }
        Route::None | Route::Msi | Route::Msix => {}
    }
    Ok(())
}

fn apic_id(cpu: u32) -> Option<u8> {
    per_cpu_init::cpu(cpu).map(|c| c.apic_id as u8)
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
    routes()[i] = Route::IoApic {
        gsi,
        trigger,
        polarity,
    };
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
        routes()[i] = Route::Msi;
    }
    Ok(())
}

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
        routes()[i] = Route::Msix;
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
    // 0x30 is the keyboard overlay. Dispatcher owns the rest of the pool.
    macro_rules! stubs {
        ($($n:literal),* $(,)?) => {
            $(arch::idt::set_handler($n, device_irq::<$n>);)*
        };
    }
    stubs!(
        49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71,
        72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94,
        95, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113,
        114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127
    );
    let _ = (POOL_START, POOL_END);
}

#[cfg(feature = "kernel_tests")]
pub fn allocated_count() -> usize {
    with_pool(|p| p.allocated())
}
