//! Per-CPU GDT + TSS + IST. DESIGN §5.1.
//!
//! One [`CpuTables`] instance per CPU later; today the BSP keeps its
//! tables in a static so the GDT/TSS addresses never move. IST stacks
//! come from the KVA allocator (guarded), which is why this runs after
//! `kva: ready` rather than before PMM.

use alloc::boxed::Box;
use core::mem::size_of;

use vibeos::desc::{GDT_LIMIT, Gdt, IstSlot, KERNEL_CS, KERNEL_DS, TSS_SEL, Tss};
use vibeos::paging::VirtAddr;

use crate::kva_init::{self, GuardedStack};
use crate::x86::{self, DtPtr};

/// Mapped pages on each IST stack. DESIGN mentions a single page; we
/// take the KVA default so a dump/`x86-interrupt` prologue cannot eat
/// the IST. Guard page is still unmapped below.
const IST_PAGES: usize = 4;
const RSP0_PAGES: usize = 4;

struct BootCell<T>(core::cell::UnsafeCell<T>);
unsafe impl<T> Sync for BootCell<T> {}
impl<T> BootCell<T> {
    const fn new(v: T) -> Self {
        Self(core::cell::UnsafeCell::new(v))
    }
    /// # Safety
    /// Exclusive boot/IRQ-off access; cell is initialized.
    #[allow(clippy::mut_from_ref)] // boot cell, IRQ-off exclusive
    unsafe fn get_mut(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
    /// # Safety
    /// Cell is initialized.
    unsafe fn get(&self) -> &T {
        unsafe { &*self.0.get() }
    }
}

/// GDT+TSS for one CPU. Phase 4 allocates one of these per AP.
#[repr(C, align(16))]
pub struct CpuTables {
    gdt: Gdt,
    tss: Tss,
}

impl CpuTables {
    pub const fn empty() -> Self {
        Self {
            gdt: Gdt::empty(),
            tss: Tss::empty(),
        }
    }

    pub fn init(&mut self, ist_tops: [u64; 4], rsp0: u64) {
        self.tss = Tss::empty();
        self.tss.set_rsp0(rsp0);
        self.tss.set_ist(IstSlot::DoubleFault, ist_tops[0]);
        self.tss.set_ist(IstSlot::Nmi, ist_tops[1]);
        self.tss.set_ist(IstSlot::MachineCheck, ist_tops[2]);
        self.tss.set_ist(IstSlot::Debug, ist_tops[3]);
        let tss_base = core::ptr::addr_of!(self.tss) as u64;
        self.gdt = Gdt::with_tss(tss_base, (size_of::<Tss>() - 1) as u16);
    }

    /// lgdt, reload CS/data segs, ltr. IRQs stay masked.
    ///
    /// # Safety
    /// `self` is the live tables for this CPU; IRQs off.
    pub unsafe fn load(&self) {
        let gdtr = DtPtr {
            limit: GDT_LIMIT,
            base: core::ptr::addr_of!(self.gdt) as u64,
        };
        unsafe { x86::lgdt(&gdtr) };
        unsafe { reload_cs(KERNEL_CS) };
        unsafe { load_data_segs(KERNEL_DS) };
        unsafe { x86::ltr(TSS_SEL) };
    }

    pub fn tss_ptr(&mut self) -> *mut Tss {
        core::ptr::addr_of_mut!(self.tss)
    }

    pub fn rsp0(&self) -> u64 {
        unsafe { core::ptr::addr_of!(self.tss.rsp[0]).read_unaligned() }
    }
}

struct Bsp {
    tables: CpuTables,
    ist: [GuardedStack; 4],
    rsp0: GuardedStack,
}

impl Bsp {
    const fn empty() -> Self {
        Self {
            tables: CpuTables::empty(),
            ist: [GuardedStack {
                guard: VirtAddr(0),
                pages: 0,
            }; 4],
            rsp0: GuardedStack {
                guard: VirtAddr(0),
                pages: 0,
            },
        }
    }
}

static BSP: BootCell<Bsp> = BootCell::new(Bsp::empty());

/// Per-AP GDT/TSS plus the IST/RSP0 stacks they point at.
pub struct ApTables {
    pub tables: Box<CpuTables>,
    pub ist: [GuardedStack; 4],
    pub rsp0: GuardedStack,
}

/// Allocate per-CPU GDT/TSS and guarded IST/RSP0 stacks. Caller `load`s.
pub fn alloc_ap_tables() -> Option<ApTables> {
    let ist0 = kva_init::alloc_guarded_stack(IST_PAGES)?;
    let ist1 = kva_init::alloc_guarded_stack(IST_PAGES).or_else(|| {
        kva_init::free_stack(ist0);
        None
    })?;
    let ist2 = kva_init::alloc_guarded_stack(IST_PAGES).or_else(|| {
        kva_init::free_stack(ist0);
        kva_init::free_stack(ist1);
        None
    })?;
    let ist3 = kva_init::alloc_guarded_stack(IST_PAGES).or_else(|| {
        kva_init::free_stack(ist0);
        kva_init::free_stack(ist1);
        kva_init::free_stack(ist2);
        None
    })?;
    let rsp0 = kva_init::alloc_guarded_stack(RSP0_PAGES).or_else(|| {
        kva_init::free_stack(ist0);
        kva_init::free_stack(ist1);
        kva_init::free_stack(ist2);
        kva_init::free_stack(ist3);
        None
    })?;
    let ist = [ist0, ist1, ist2, ist3];
    let mut tables = Box::new(CpuTables::empty());
    tables.init(
        [
            ist[0].top().as_u64(),
            ist[1].top().as_u64(),
            ist[2].top().as_u64(),
            ist[3].top().as_u64(),
        ],
        rsp0.top().as_u64(),
    );
    Some(ApTables { tables, ist, rsp0 })
}

pub fn free_ap_tables(t: ApTables) {
    kva_init::free_stack(t.rsp0);
    let mut i = 0;
    while i < t.ist.len() {
        kva_init::free_stack(t.ist[i]);
        i += 1;
    }
}

/// Allocate IST + RSP0 stacks, fill GDT/TSS, load them.
///
/// # Safety
/// Single-CPU, IRQs off, KVA already up.
pub unsafe fn init_bsp() {
    let ist = [
        kva_init::alloc_guarded_stack(IST_PAGES).expect("ist df"),
        kva_init::alloc_guarded_stack(IST_PAGES).expect("ist nmi"),
        kva_init::alloc_guarded_stack(IST_PAGES).expect("ist mc"),
        kva_init::alloc_guarded_stack(IST_PAGES).expect("ist db"),
    ];
    let rsp0 = kva_init::alloc_guarded_stack(RSP0_PAGES).expect("tss rsp0");
    let bsp = unsafe { BSP.get_mut() };
    bsp.ist = ist;
    bsp.rsp0 = rsp0;
    bsp.tables.init(
        [
            ist[0].top().as_u64(),
            ist[1].top().as_u64(),
            ist[2].top().as_u64(),
            ist[3].top().as_u64(),
        ],
        rsp0.top().as_u64(),
    );
    unsafe { bsp.tables.load() };
}

/// TSS for the BSP. Call after [`init_bsp`].
pub fn bsp_tss_ptr() -> *mut Tss {
    unsafe { core::ptr::addr_of_mut!(BSP.get_mut().tables.tss) }
}

pub fn bsp_rsp0_top() -> u64 {
    unsafe { BSP.get().rsp0.top().as_u64() }
}

/// `[mapped_base, top)` of an IST stack. Used by the in-guest DF test.
#[allow(dead_code)]
pub fn ist_span(slot: IstSlot) -> (u64, u64) {
    let s = unsafe { BSP.get().ist[slot.index()] };
    (s.mapped_base().as_u64(), s.top().as_u64())
}

/// # Safety
/// `sel` is a valid code selector in the loaded GDT.
unsafe fn reload_cs(sel: u16) {
    unsafe {
        asm_reload_cs(sel as u64);
    }
}

/// # Safety
/// `sel` is a valid 64-bit code selector; this far-returns onto it.
unsafe fn asm_reload_cs(sel: u64) {
    unsafe {
        core::arch::asm!(
            "push {sel}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            sel = in(reg) sel,
            tmp = lateout(reg) _,
            options(preserves_flags),
        );
    }
}

/// # Safety
/// `sel` is a valid data selector in the loaded GDT.
unsafe fn load_data_segs(sel: u16) {
    unsafe {
        core::arch::asm!(
            "mov ds, {0:x}",
            "mov es, {0:x}",
            "mov ss, {0:x}",
            "mov fs, {0:x}",
            "mov gs, {0:x}",
            in(reg) sel,
            options(nostack, preserves_flags),
        );
    }
}
