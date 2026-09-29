//! Per-CPU GDT + TSS + IST. DESIGN §5.1.
//!
//! One [`CpuTables`] instance per CPU later; today the BSP keeps its
//! tables in a static so the GDT/TSS addresses never move. IST stacks
//! come from the KVA allocator (guarded), which is why this runs after
//! `kva: ready` rather than before PMM.

use core::mem::size_of;

use vibeos::desc::{GDT_LIMIT, Gdt, IstSlot, KERNEL_CS, KERNEL_DS, TSS_SEL, Tss};
use vibeos::kalloc::TryBox;

use crate::cell::BootCell;
use crate::kva_init::{self, GuardedStack};
use crate::x86::{self, DtPtr};

/// Mapped pages on each IST stack: four, as DESIGN §5.1 gives, so a dump
/// or a handler prologue cannot eat the IST. Guard page is still
/// unmapped below.
const IST_PAGES: usize = 4;
const RSP0_PAGES: usize = 4;

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
        // SAFETY: invariant I230, established at `arch::x86_64::gdt::init_bsp`
        // and `smp::smp_init::start_one`: `self` is this CPU's live tables
        // (this fn's `# Safety`) and never moves or is freed while the CPU is
        // online, so the GDT and the TSS its descriptor names stay valid.
        // `init` built both: `KERNEL_CS`, `KERNEL_DS` and `TSS_SEL` index
        // them, and IRQs are off, so the `GS_BASE` that `mov gs` zeroes is
        // written back (`per_cpu_init::install_gs`) before any `gs:` read.
        unsafe {
            x86::lgdt(&gdtr);
            reload_cs(KERNEL_CS);
            x86::load_data_segs(KERNEL_DS);
            x86::ltr(TSS_SEL);
        }
    }

    pub fn tss_ptr(&mut self) -> *mut Tss {
        core::ptr::addr_of_mut!(self.tss)
    }

    pub fn rsp0(&self) -> u64 {
        // SAFETY: the pointer comes from `&self`, so it is valid for reads;
        // `read_unaligned` needs no alignment in the packed `Tss`;
        // established here.
        unsafe { core::ptr::addr_of!(self.tss.rsp[0]).read_unaligned() }
    }
}

pub(crate) struct Bsp {
    tables: CpuTables,
    /// Owns the IST stacks the BSP's TSS names; only the in-guest tests
    /// read it.
    #[cfg_attr(
        not(feature = "kernel_tests"),
        expect(
            dead_code,
            reason = "invariant I230 (DESIGN §2.7): holds the IST stacks the TSS names for the BSP's life"
        )
    )]
    pub(crate) ist: [GuardedStack; 4],
    rsp0: GuardedStack,
}

pub(crate) static BSP: BootCell<Bsp> = BootCell::new();

/// Per-AP GDT/TSS plus the IST/RSP0 stacks they point at.
pub struct ApTables {
    pub tables: TryBox<CpuTables>,
    pub ist: [GuardedStack; 4],
    pub rsp0: GuardedStack,
}

/// Allocate per-CPU GDT/TSS and guarded IST/RSP0 stacks. Caller `load`s.
pub fn alloc_ap_tables() -> Option<ApTables> {
    let ist0 = kva_init::alloc_guarded_stack(IST_PAGES).ok()?;
    let ist1 = match kva_init::alloc_guarded_stack(IST_PAGES) {
        Ok(s) => s,
        Err(_) => {
            kva_init::free_stack(ist0);
            return None;
        }
    };
    let ist2 = match kva_init::alloc_guarded_stack(IST_PAGES) {
        Ok(s) => s,
        Err(_) => {
            kva_init::free_stack(ist0);
            kva_init::free_stack(ist1);
            return None;
        }
    };
    let ist3 = match kva_init::alloc_guarded_stack(IST_PAGES) {
        Ok(s) => s,
        Err(_) => {
            kva_init::free_stack(ist0);
            kva_init::free_stack(ist1);
            kva_init::free_stack(ist2);
            return None;
        }
    };
    let rsp0 = match kva_init::alloc_guarded_stack(RSP0_PAGES) {
        Ok(s) => s,
        Err(_) => {
            kva_init::free_stack(ist0);
            kva_init::free_stack(ist1);
            kva_init::free_stack(ist2);
            kva_init::free_stack(ist3);
            return None;
        }
    };
    let ist = [ist0, ist1, ist2, ist3];
    // AP bring-up runs after `irq: enabled`, so the allocation is fallible
    // (DESIGN §4.4). The tables are filled in place: `init` writes the TSS
    // address into the GDT, so they must not move after it.
    let Ok(mut tables) = TryBox::try_new(CpuTables::empty()) else {
        kva_init::free_stack(rsp0);
        for s in ist {
            kva_init::free_stack(s);
        }
        return None;
    };
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
    let ApTables { tables, ist, rsp0 } = t;
    kva_init::free_stack(rsp0);
    for s in ist {
        kva_init::free_stack(s);
    }
    drop(tables);
}

/// Allocate IST + RSP0 stacks, fill GDT/TSS, load them.
///
/// # Safety
/// Single-CPU, IRQs off, KVA already up.
pub unsafe fn init_bsp() {
    // A boot-time resource failure no invariant bounds: halt with a line
    // (C-LINTS's boot-halt rule).
    let stack = |pages, msg| {
        kva_init::alloc_guarded_stack(pages).unwrap_or_else(|_| crate::boot::halt_with(msg))
    };
    let ist = [
        stack(IST_PAGES, "vibeOS: gdt: ist df stack allocation failed"),
        stack(IST_PAGES, "vibeOS: gdt: ist nmi stack allocation failed"),
        stack(IST_PAGES, "vibeOS: gdt: ist mc stack allocation failed"),
        stack(IST_PAGES, "vibeOS: gdt: ist db stack allocation failed"),
    ];
    let rsp0 = stack(RSP0_PAGES, "vibeOS: gdt: tss rsp0 stack allocation failed");
    let ist_tops = [
        ist[0].top().as_u64(),
        ist[1].top().as_u64(),
        ist[2].top().as_u64(),
        ist[3].top().as_u64(),
    ];
    let rsp0_top = rsp0.top().as_u64();
    let bsp = Bsp {
        tables: CpuTables::empty(),
        ist,
        rsp0,
    };
    // `tables.init` writes the TSS base into the GDT. Do that after
    // `set` so the base is the BootCell address, not this stack slot.
    // SAFETY: invariant I22, established at `cell::BootCell::set`: the one
    // write, on the BSP before SMP (this fn's `# Safety`).
    unsafe { BSP.set(bsp) };
    // SAFETY: no reader has seen `BSP` yet (single CPU, IRQs off, the cell
    // set just above), so this is the only reference to it while `init`
    // fills the tables in place; established here.
    let p = unsafe { &mut *BSP.as_ptr() };
    p.tables.init(ist_tops, rsp0_top);
    // SAFETY: invariant I230, established here: `p.tables` lives in the
    // `BSP` static, so it never moves or is freed, and `init` just filled
    // it; IRQs are off (this fn's `# Safety`).
    unsafe { p.tables.load() };
}

/// TSS for the BSP. Call after [`init_bsp`]. The CPU only reads
/// `TSS.RSP0`; software writes it on each switch through this pointer
/// (`syscall_init::set_rsp0_for`).
pub fn bsp_tss_ptr() -> *mut Tss {
    core::ptr::addr_of!(BSP.get().tables.tss) as *mut Tss
}

pub fn bsp_rsp0_top() -> u64 {
    BSP.get().rsp0.top().as_u64()
}

/// # Safety
/// `sel` is a valid code selector in the loaded GDT.
unsafe fn reload_cs(sel: u16) {
    // SAFETY: this fn's `# Safety` (here): `sel` is a valid code selector.
    unsafe {
        asm_reload_cs(sel as u64);
    }
}

/// # Safety
/// `sel` is a valid 64-bit code selector; this far-returns onto it.
unsafe fn asm_reload_cs(sel: u64) {
    // SAFETY: this fn's `# Safety` (here): the far return lands on the next
    // instruction through a valid 64-bit code selector, and pops exactly the
    // two words it pushed.
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
