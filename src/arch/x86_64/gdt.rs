//! Per-CPU GDT + TSS + IST. DESIGN §5.1.
//!
//! One [`CpuTables`] per CPU: the BSP's in the `BSP` `BootCell`, each AP's
//! in a heap block `smp_init` holds as a raw pointer, so the GDT/TSS
//! addresses never move while the CPU runs. The CPU reads the TSS; after
//! `load`, software writes only its RSP0, through
//! [`CpuTables::set_rsp0`] on the owning CPU with IF=0 (ROADMAP §10.3,
//! F089). IST stacks come from the KVA allocator (guarded), which is why
//! this runs after `kva: ready` rather than before PMM.

use core::cell::UnsafeCell;
use core::mem::{offset_of, size_of};
use core::ptr::NonNull;

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

/// GDT+TSS for one CPU. The TSS sits in an `UnsafeCell`: after `load`
/// the tables are shared as `&CpuTables`, and [`CpuTables::set_rsp0`] is
/// the TSS's one writer.
#[repr(C, align(16))]
pub struct CpuTables {
    gdt: Gdt,
    tss: UnsafeCell<Tss>,
}

// SAFETY: invariant I22's `Sync` part for the `BSP` cell: after `load` the
// TSS is written only by `CpuTables::set_rsp0`, whose contract is the CPU
// that loaded the tables with IF=0, and its one caller,
// `syscall_init::set_rsp0_for`, runs on that CPU inside `on_switch` with
// IF=0; every other access reads. Established by
// `arch::x86_64::gdt::CpuTables::set_rsp0`.
unsafe impl Sync for CpuTables {}

impl CpuTables {
    pub const fn empty() -> Self {
        Self {
            gdt: Gdt::empty(),
            tss: UnsafeCell::new(Tss::empty()),
        }
    }

    /// Fill the tables for their final address `at`, where they are loaded
    /// and never move: the GDT's TSS descriptor names `at`'s TSS, so `self`
    /// may be built elsewhere and moved to `at` before `load`.
    pub fn init(&mut self, at: *const CpuTables, ist_tops: [u64; 4], rsp0: u64) {
        let tss = self.tss.get_mut();
        *tss = Tss::empty();
        tss.set_rsp0(rsp0);
        tss.set_ist(IstSlot::DoubleFault, ist_tops[0]);
        tss.set_ist(IstSlot::Nmi, ist_tops[1]);
        tss.set_ist(IstSlot::MachineCheck, ist_tops[2]);
        tss.set_ist(IstSlot::Debug, ist_tops[3]);
        let tss_base = (at.addr() as u64).wrapping_add(offset_of!(CpuTables, tss) as u64);
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

    pub fn rsp0(&self) -> u64 {
        // SAFETY: the pointer comes from the `UnsafeCell` behind `&self`, so
        // it is valid for reads, and only `set_rsp0` writes the TSS, on the
        // owning CPU with IF=0, which this read on that CPU does not
        // overlap; `read_unaligned` needs no alignment in the packed `Tss`.
        // Established by `arch::x86_64::gdt::CpuTables::set_rsp0`.
        unsafe { core::ptr::addr_of!((*self.tss.get()).rsp[0]).read_unaligned() }
    }

    /// Write TSS.RSP0, the stack the CPU loads on a ring-3 to ring-0
    /// change: the TSS's one writer after `load`.
    ///
    /// # Safety
    /// Runs on the CPU that loaded these tables, with IF=0, so no other
    /// software access to the TSS runs meanwhile, and the CPU reads RSP0
    /// only on a ring change, which this ring-0 IF=0 stretch rules out.
    pub unsafe fn set_rsp0(&self, top: u64) {
        // SAFETY: the pointer comes from the `UnsafeCell`, so it carries
        // write provenance, and this fn's `# Safety` makes the write the
        // TSS's only access; `write_unaligned` needs no alignment in the
        // packed `Tss`. Established by `CpuTables::set_rsp0`'s callers,
        // `syscall_init::set_rsp0_for`.
        unsafe { core::ptr::addr_of_mut!((*self.tss.get()).rsp[0]).write_unaligned(top) };
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

/// Per-AP GDT/TSS plus the IST/RSP0 stacks they point at. `tables` is
/// the pointer `TryBox::into_raw` returned, so every pointer the AP takes
/// from it keeps the allocation's write provenance; [`free_ap_tables`]
/// frees it with `TryBox::from_raw`.
pub struct ApTables {
    pub tables: NonNull<CpuTables>,
    pub ist: [GuardedStack; 4],
    pub rsp0: GuardedStack,
}

// SAFETY: `tables` owns its heap block as the `TryBox` it came from did,
// so moving an `ApTables` moves that ownership, which `CpuTables: Send`
// allows; the AP that runs on the tables reads them through a pointer
// `smp_init::start_one` hands it one AP at a time. Established by
// `arch::x86_64::gdt::alloc_ap_tables`.
unsafe impl Send for ApTables {}

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
    let at: *const CpuTables = &*tables;
    tables.init(
        at,
        [
            ist[0].top().as_u64(),
            ist[1].top().as_u64(),
            ist[2].top().as_u64(),
            ist[3].top().as_u64(),
        ],
        rsp0.top().as_u64(),
    );
    // A box's pointer is never null, so the `None` arm only frees.
    let Some(tables) = NonNull::new(TryBox::into_raw(tables)) else {
        kva_init::free_stack(rsp0);
        for s in ist {
            kva_init::free_stack(s);
        }
        return None;
    };
    Some(ApTables { tables, ist, rsp0 })
}

pub fn free_ap_tables(t: ApTables) {
    let ApTables { tables, ist, rsp0 } = t;
    kva_init::free_stack(rsp0);
    for s in ist {
        kva_init::free_stack(s);
    }
    // SAFETY: `tables` came from `TryBox::into_raw` in `alloc_ap_tables`,
    // and this `ApTables`, consumed here, was its only owner; no CPU runs
    // on the tables any more (`smp_init` frees them only for an AP that
    // never started or was abandoned). Established by
    // `arch::x86_64::gdt::alloc_ap_tables`.
    drop(unsafe { TryBox::from_raw(tables.as_ptr()) });
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
    let mut bsp = Bsp {
        tables: CpuTables::empty(),
        ist,
        rsp0,
    };
    // `init` writes the TSS base into the GDT: the base is the tables'
    // final address in the cell, not this stack slot, and it is filled
    // before `set`, so nothing takes `&mut` to the cell after publication.
    // SAFETY: a place expression on the cell's payload address, which
    // builds no reference and reads nothing; established here.
    let at = unsafe { core::ptr::addr_of!((*BSP.as_ptr()).tables) };
    bsp.tables.init(at, ist_tops, rsp0_top);
    // SAFETY: invariant I22, established at `cell::BootCell::set`: the one
    // write, on the BSP before SMP (this fn's `# Safety`).
    unsafe { BSP.set(bsp) };
    // SAFETY: invariant I230, established here: the tables live in the
    // `BSP` static, at the address `init` filled them for, so they never
    // move or are freed; IRQs are off (this fn's `# Safety`).
    unsafe { BSP.get().tables.load() };
}

/// The BSP's tables, for `PerCpu.tables`. Call after [`init_bsp`].
/// `syscall_init::set_rsp0_for` writes TSS.RSP0 through it with
/// [`CpuTables::set_rsp0`], which takes `&self`.
pub fn bsp_tables() -> *const CpuTables {
    core::ptr::from_ref(&BSP.get().tables)
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
