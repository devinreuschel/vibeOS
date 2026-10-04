//! aarch64 boot helpers: exception level, ISA floor, early vectors.

use core::cell::UnsafeCell;

use vibeos::arch::BootHandover;
use vibeos::arch::aarch64::paging::{self, DESC_ADDR_MASK, DESC_TABLE, DESC_VALID};
use vibeos::paging::{PhysAddr, VirtAddr, mmio_flags};

use super::{Arch, cpu, vectors};
use crate::boot::{self, BootInfo};

/// QEMU `virt` PL011 before the device tree is walked.
const PL011_EARLY_PA: u64 = 0x0900_0000;
/// QEMU `virt` fw_cfg before the device tree is walked (`qemu,fw-cfg-mmio`).
const FW_CFG_EARLY_PA: u64 = 0x0902_0000;

#[repr(align(4096))]
struct EarlyTable {
    entries: UnsafeCell<[u64; paging::PTES_PER_TABLE]>,
}

// SAFETY: `EarlyTable` shares `&Self` (`[u64; N]: Send + Sync`) and
// mutates only through `UnsafeCell` on the boot CPU with IRQs off,
// before `irq: enabled`; established here.
unsafe impl Sync for EarlyTable {}

/// Scratch L1/L2/L3 tables for the console UART. Boot CPU, IRQs off.
static EARLY_TABLES: [EarlyTable; 3] = [
    EarlyTable {
        entries: UnsafeCell::new([0; paging::PTES_PER_TABLE]),
    },
    EarlyTable {
        entries: UnsafeCell::new([0; paging::PTES_PER_TABLE]),
    },
    EarlyTable {
        entries: UnsafeCell::new([0; paging::PTES_PER_TABLE]),
    },
];

/// Map the PL011 and fw_cfg at `HHDM + PA` in Limine's TTBR1.
///
/// Limine's HHDM is RAM-only, so identity `0x0900_0000` is unmapped at
/// entry. `VBAR` is still 0; a data abort there prefetch-aborts at `0x200`.
/// fw_cfg sits in the same 2 MiB as the UART; both L3 PTEs share the
/// scratch tables. Returns the fw_cfg VA when that page mapped, so
/// `boot::fw_cfg_init` can read `opt/vibeos/cmdline` before paging
/// takeover.
pub fn map_early_console() -> Option<u64> {
    let hhdm = boot::early_hhdm_offset()?;
    let kphys = boot::early_kernel_phys()?;
    let mut used = 0usize;
    let uart_pa = PL011_EARLY_PA & !0xFFF;
    let uart_va = hhdm.wrapping_add(uart_pa);
    if map_device_page(hhdm, kphys, uart_va, uart_pa, &mut used) {
        crate::serial::raw::set_mmio(uart_va);
    }
    let fw_pa = FW_CFG_EARLY_PA & !0xFFF;
    let fw_va = hhdm.wrapping_add(fw_pa);
    map_device_page(hhdm, kphys, fw_va, fw_pa, &mut used).then_some(fw_va)
}

fn map_device_page(hhdm: u64, kphys: u64, va: u64, pa: u64, used: &mut usize) -> bool {
    let mut table_pa = cpu::read_ttbr1() & DESC_ADDR_MASK;
    let mut level = paging::LEVELS;
    while level > 1 {
        let idx = paging::index(VirtAddr(va), level);
        let slot = match table_slot(hhdm, kphys, table_pa, idx) {
            Some(p) => p,
            None => return false,
        };
        // SAFETY: `slot` is the live TTBR1 entry (HHDM or kernel VMA).
        // established here.
        let ent = unsafe { core::ptr::read_volatile(slot) };
        if ent & DESC_VALID == 0 {
            let Some(new_pa) = alloc_early_table(kphys, used) else {
                return false;
            };
            // SAFETY: unused TTBR1 slot; `new_pa` is a zeroed kernel page.
            // established here.
            unsafe { core::ptr::write_volatile(slot, paging::make_table(PhysAddr(new_pa))) };
            table_pa = new_pa;
        } else if ent & DESC_TABLE == 0 {
            return false;
        } else {
            table_pa = ent & DESC_ADDR_MASK;
        }
        level -= 1;
    }
    let idx = paging::index(VirtAddr(va), 1);
    let slot = match table_slot(hhdm, kphys, table_pa, idx) {
        Some(p) => p,
        None => return false,
    };
    let pte = paging::make_pte(VirtAddr(va), PhysAddr(pa), mmio_flags());
    // SAFETY: L3 slot for the early Device page (UART or fw_cfg). established here.
    unsafe { core::ptr::write_volatile(slot, pte) };
    // SAFETY: order the PTE store before the local TLB invalidate.
    // established here.
    unsafe { core::arch::asm!("dsb ishst", options(nostack, preserves_flags)) };
    cpu::tlbi_va(va);
    true
}

fn table_slot(hhdm: u64, kphys: u64, table_pa: u64, idx: usize) -> Option<*mut u64> {
    if idx >= paging::PTES_PER_TABLE {
        return None;
    }
    let base = early_table_kva(kphys, table_pa).unwrap_or(hhdm.wrapping_add(table_pa));
    Some((base.wrapping_add((idx as u64) * 8)) as *mut u64)
}

fn table_va(t: &EarlyTable) -> u64 {
    t.entries.get() as u64
}

fn early_table_kva(kphys: u64, table_pa: u64) -> Option<u64> {
    let vma = boot::kernel_vma_start();
    for t in &EARLY_TABLES {
        let va = table_va(t);
        let Some(off) = va.checked_sub(vma) else {
            continue;
        };
        if kphys.wrapping_add(off) == table_pa {
            return Some(va);
        }
    }
    None
}

fn alloc_early_table(kphys: u64, used: &mut usize) -> Option<u64> {
    let t = EARLY_TABLES.get(*used)?;
    *used += 1;
    table_va(t)
        .checked_sub(boot::kernel_vma_start())
        .map(|off| kphys.wrapping_add(off))
}

/// After serial: EL marker, ISA floor, early VBAR.
pub fn early_init() {
    cpu::note_exception_level();
    super::percpu::set_tpidr_el2(cpu::el2_vhe());
    cpu::check_isa_floor();
    vectors::init_early();
}

impl BootHandover for Arch {
    type Info = BootInfo;

    #[inline]
    fn info() -> &'static BootInfo {
        boot::info()
    }
}
