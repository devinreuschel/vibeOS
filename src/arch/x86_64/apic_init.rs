//! BSP LAPIC + I/O APIC + LAPIC timer. ROADMAP §4.1–4.3.
//!
//! Order: UC already done in `acpi_init` → enable LAPIC → program IOAPIC
//! (masked) → detect/calib/arm timer → prove → marker → mask PIC + PIT GSI.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vibeos::acpi::{IoApic, Iso, MAX_IOAPICS};
use vibeos::apic::{
    self, APIC_BASE_ENABLE, CountRead, DEFAULT_LAPIC_PHYS, EoiDomain, IA32_APIC_BASE,
    IA32_TSC_DEADLINE, ICR_POLL_CAP, IOAPIC_VER, IOREGSEL, IOWIN, IpiError, IpiMode, LAPIC_EOI,
    LAPIC_ESR, LAPIC_ICR_HIGH, LAPIC_ICR_LOW, LAPIC_ID, LAPIC_LVT_ERROR, LAPIC_LVT_LINT0,
    LAPIC_LVT_LINT1, LAPIC_LVT_PERF, LAPIC_LVT_THERMAL, LAPIC_LVT_TIMER, LAPIC_SVR,
    LAPIC_TIMER_CCR, LAPIC_TIMER_DCR, LAPIC_TIMER_ICR, LAPIC_TPR, LVT_DELIVERY_EXTINT, LVT_MASKED,
    Polarity, TIMER_DIV_16, TimerMode, TimerProof, Trigger, TscDeadlineStep, has_tsc_deadline,
    ioapic_max_index, ioapic_pin, lapic_per_ms, lvt_timer_periodic, poll_delivery_pending,
    redir_high, redir_low, redir_set_mask, svr_value, timer_proof, tsc_deadline_arm_plan,
    tsc_deadline_value, write_redir,
};
use vibeos::lock::RANK_DEVICE;
use vibeos::marker;
use vibeos::time::{FS_PER_MS, HPET_CALIB_READS, PIT_CALIB_MS, hpet_period_ok};
use vibeos::vectors;

use crate::arch;
use crate::machine_init;
use crate::paging_init;
use crate::sync_init::SpinMutex;
use crate::time_init;
use crate::x86;

const LAPIC_TICKS_PER_MS_MIN: u64 = 100;
const LAPIC_TICKS_PER_MS_MAX: u64 = 50_000_000;
const CALIB_SPIN_CAP: u64 = 1_000_000_000;

pub(crate) struct IoApicRt {
    pub(crate) va: u64,
    gsi_base: u32,
    max_index: u32,
}

pub(crate) struct ApicState {
    lapic_va: u64,
    pub(crate) ready: bool,
    mode: TimerMode,
    owns_tick: bool,
    ticks_per_ms: u64,
    ioapic_n: usize,
    ioapics: [IoApicRt; MAX_IOAPICS],
}

impl ApicState {
    const fn empty() -> Self {
        const EMPTY_IO: IoApicRt = IoApicRt {
            va: 0,
            gsi_base: 0,
            max_index: 0,
        };
        Self {
            lapic_va: 0,
            ready: false,
            mode: TimerMode::Pit,
            owns_tick: false,
            ticks_per_ms: 0,
            ioapic_n: 0,
            ioapics: [EMPTY_IO; MAX_IOAPICS],
        }
    }
}

static STATE: SpinMutex<ApicState> = SpinMutex::with_rank(ApicState::empty(), RANK_DEVICE);

/// Run `f` on the APIC state.
pub(crate) fn with_state<R>(f: impl FnOnce(&mut ApicState) -> R) -> R {
    let mut g = STATE.lock();
    f(&mut g)
}
pub(crate) static TIMER_FIRES: AtomicU64 = AtomicU64::new(0);
static LAPIC_VA: AtomicU64 = AtomicU64::new(0);
static TSC_DEADLINE: AtomicBool = AtomicBool::new(false);

fn publish_isr(st: &ApicState) {
    // Release: pairs with the Acquire loads in `send_ipi` and `send_ipi_all_ex_self`.
    LAPIC_VA.store(st.lapic_va, Ordering::Release);
    // Release: pairs with nothing; the boot CPU stores it before it starts the APs.
    TSC_DEADLINE.store(st.mode == TimerMode::TscDeadline, Ordering::Release);
}

fn lapic_mmio_va(phys: u64) -> Option<u64> {
    crate::acpi_init::lapic_va().or_else(|| {
        // SAFETY: invariant I49: the LAPIC page is device MMIO; ACPI
        // already ioremapped it when the MADT named it, and this is the
        // fallback when the MADT base was zero; established here.
        unsafe {
            paging_init::ioremap(vibeos::paging::PhysAddr(phys), vibeos::paging::PAGE_SIZE_4K)
        }
        .map(|v| v.as_u64())
    })
}

/// # Safety
/// `va` is the physmap address of a LAPIC register page mapped UC
/// (invariant I49), and `off` a register offset in it.
unsafe fn lapic_read(va: u64, off: u32) -> u32 {
    // SAFETY: this fn's `# Safety` (here): an aligned, mapped register.
    unsafe { (va.wrapping_add(off as u64) as *const u32).read_volatile() }
}

/// # Safety
/// As for [`lapic_read`].
unsafe fn lapic_write(va: u64, off: u32, val: u32) {
    // SAFETY: this fn's `# Safety` (here): an aligned, mapped register.
    unsafe { (va.wrapping_add(off as u64) as *mut u32).write_volatile(val) };
}

/// # Safety
/// `va` is the physmap address of an I/O APIC register page mapped UC
/// (invariant I49), and the caller holds `STATE`, so no other CPU moves
/// `IOREGSEL` between the two accesses.
unsafe fn io_write(va: u64, reg: u8, val: u32) {
    // SAFETY: this fn's `# Safety` (here): `IOREGSEL` and `IOWIN` are
    // aligned, mapped registers of that page.
    unsafe {
        (va.wrapping_add(IOREGSEL as u64) as *mut u32).write_volatile(reg as u32);
        (va.wrapping_add(IOWIN as u64) as *mut u32).write_volatile(val);
    }
}

/// # Safety
/// As for [`io_write`].
pub(crate) unsafe fn io_read(va: u64, reg: u8) -> u32 {
    // SAFETY: this fn's `# Safety` (here): `IOREGSEL` and `IOWIN` are
    // aligned, mapped registers of that page.
    unsafe {
        (va.wrapping_add(IOREGSEL as u64) as *mut u32).write_volatile(reg as u32);
        (va.wrapping_add(IOWIN as u64) as *const u32).read_volatile()
    }
}

pub(crate) fn cpuid_tsc_deadline() -> bool {
    let (_, _, ecx, _) = x86::cpuid(1, 0);
    has_tsc_deadline(ecx)
}

/// # Safety
/// `va` as for [`lapic_read`].
unsafe fn local_apic_id(va: u64) -> u8 {
    // SAFETY: this fn's `# Safety` (here).
    (unsafe { lapic_read(va, LAPIC_ID) } >> 24) as u8
}

/// Enable via `IA32_APIC_BASE` bit 11, MADT type-5 base, SVR/TPR/LVT.
///
/// # Safety
/// LAPIC page already UC. IRQs off.
unsafe fn enable_lapic(lapic_base: u64) -> Option<u64> {
    let phys = if lapic_base != 0 {
        lapic_base
    } else {
        DEFAULT_LAPIC_PHYS
    };
    let cur = x86::rdmsr(IA32_APIC_BASE);
    let next = apic::apic_base_msr(cur, phys);
    // SAFETY: `apic_base_msr` keeps every reserved bit of the value read
    // and sets the enable bit and the MADT's base, the one LAPIC the MADT
    // names; IRQs are off (this fn's `# Safety`); established here.
    unsafe { x86::wrmsr(IA32_APIC_BASE, next) };
    let got = x86::rdmsr(IA32_APIC_BASE);
    if got & APIC_BASE_ENABLE == 0 {
        crate::marker!("vibeOS: lapic: enable bit clear");
        return None;
    }
    let Some(va) = lapic_mmio_va(phys) else {
        crate::marker!("vibeOS: lapic: ioremap failed");
        return None;
    };
    // SAFETY: invariant I49, established at `acpi::acpi_init::init`: the
    // MADT's LAPIC page is UC through `ioremap` (this fn's `# Safety`), and
    // `va` is that mapping.
    unsafe {
        // Probe: a disabled LAPIC reads as zero and looks like missing HW.
        lapic_write(va, LAPIC_TPR, 0);
        lapic_write(va, LAPIC_SVR, svr_value());
        let svr = lapic_read(va, LAPIC_SVR);
        if svr & apic::SVR_ENABLE == 0 {
            crate::marker!("vibeOS: lapic: svr enable failed");
            return None;
        }
        // Write, read, write clears ESR (the read's value is stale).
        lapic_write(va, LAPIC_ESR, 0);
        lapic_read(va, LAPIC_ESR);
        lapic_write(va, LAPIC_ESR, 0);
        lapic_write(va, LAPIC_LVT_ERROR, apic::lvt_error_value());
        lapic_write(va, LAPIC_LVT_THERMAL, apic::lvt_thermal_value());
        lapic_write(va, LAPIC_LVT_PERF, LVT_MASKED);
        lapic_write(va, LAPIC_LVT_LINT0, LVT_MASKED);
        lapic_write(va, LAPIC_LVT_LINT1, LVT_MASKED);
        lapic_write(
            va,
            LAPIC_LVT_TIMER,
            apic::lvt_timer_oneshot(vectors::LAPIC_TIMER, true),
        );
        lapic_write(va, LAPIC_TIMER_ICR, 0);
    }
    Some(va)
}

fn enum_ioapics(ios: impl Iterator<Item = IoApic>, st: &mut ApicState) {
    st.ioapic_n = 0;
    for IoApic { addr, gsi_base, .. } in ios {
        let phys = addr as u64;
        if phys == 0 {
            continue;
        }
        let Some(va) = crate::acpi_init::ioapic_va(phys) else {
            continue;
        };
        // SAFETY: invariant I49, established at `acpi::acpi_init::init`:
        // every MADT I/O APIC page is UC through `ioremap`, and `va` is
        // that mapping; the caller holds `STATE`.
        let ver = unsafe { io_read(va, IOAPIC_VER) };
        let max_index = ioapic_max_index(ver);
        if let Some(slot) = st.ioapics.get_mut(st.ioapic_n) {
            *slot = IoApicRt {
                va,
                gsi_base,
                max_index,
            };
            st.ioapic_n += 1;
        }
    }
}

fn mask_all_pins(st: &ApicState) {
    // SAFETY: invariant I49, established at `arch::x86_64::apic_init::enable_lapic`:
    // `st.lapic_va` is set only from its return.
    let dest = unsafe { local_apic_id(st.lapic_va) };
    let mut i = 0;
    while i < st.ioapic_n {
        let io = &st.ioapics[i];
        let mut pin = 0u32;
        while pin <= io.max_index {
            let p = pin as u8;
            let high = redir_high(dest);
            let low = redir_low(
                vectors::DEVICE_VEC_START,
                Trigger::Edge,
                Polarity::High,
                true,
            );
            // SAFETY: invariant I49, established at `arch::x86_64::apic_init::enum_ioapics`:
            // `io.va` is set only there, and the caller holds `STATE`.
            write_redir(
                |reg, val| unsafe { io_write(io.va, reg, val) },
                p,
                high,
                low,
            );
            pin += 1;
        }
        i += 1;
    }
}

fn apply_isos(st: &ApicState, isos: &[Iso]) {
    // SAFETY: invariant I49, established at `arch::x86_64::apic_init::enable_lapic`:
    // `st.lapic_va` is set only from its return.
    let dest = unsafe { local_apic_id(st.lapic_va) };
    let mut unrouted = 0usize;
    let mut i = 0;
    while i < isos.len() {
        let iso = isos[i];
        let trig = apic::iso_trigger(iso.flags);
        let pol = apic::iso_polarity(iso.flags);
        // Shared placeholder vector: every ISO stays masked until a driver
        // calls `route_gsi` with a real vector.
        if route_gsi_inner(
            st,
            iso.gsi,
            vectors::DEVICE_VEC_START,
            dest,
            trig,
            pol,
            true,
        )
        .is_err()
        {
            unrouted += 1;
        }
        i += 1;
    }
    // An ISO whose GSI no I/O APIC serves is a firmware table error; its
    // line stays as the firmware left it (DESIGN §2.5: one summary line).
    if unrouted != 0 {
        crate::klog!(
            vibeos::log::Level::Warn,
            "vibeOS: ioapic: {} of {} MADT ISOs name a GSI no I/O APIC serves",
            unrouted,
            isos.len()
        );
    }
}

pub(crate) fn find_ioapic(st: &ApicState, gsi: u32) -> Option<(&IoApicRt, u8)> {
    let mut i = 0;
    while i < st.ioapic_n {
        let io = &st.ioapics[i];
        if let Some(pin) = ioapic_pin(gsi, io.gsi_base, io.max_index) {
            return Some((io, pin));
        }
        i += 1;
    }
    None
}

fn route_gsi_inner(
    st: &ApicState,
    gsi: u32,
    vector: u8,
    cpu: u8,
    trigger: Trigger,
    polarity: Polarity,
    masked: bool,
) -> Result<(), IpiError> {
    let Some((io, pin)) = find_ioapic(st, gsi) else {
        return Err(IpiError::NoRoute);
    };
    let high = redir_high(cpu);
    let low = redir_low(vector, trigger, polarity, masked);
    // SAFETY: invariant I49, established at `arch::x86_64::apic_init::enum_ioapics`:
    // `io.va` is set only there, and the caller holds `STATE`.
    write_redir(
        |reg, val| unsafe { io_write(io.va, reg, val) },
        pin,
        high,
        low,
    );
    Ok(())
}

/// High dword before low. Entries stay masked until `unmask_gsi`.
pub fn route_gsi(
    gsi: u32,
    vector: u8,
    cpu: u8,
    trigger: Trigger,
    polarity: Polarity,
) -> Result<(), IpiError> {
    with_state(|st| {
        if !st.ready {
            return Err(IpiError::NotReady);
        }
        route_gsi_inner(st, gsi, vector, cpu, trigger, polarity, true)
    })
}

pub fn mask_gsi(gsi: u32) {
    set_gsi_mask(gsi, true);
}

pub fn unmask_gsi(gsi: u32) {
    set_gsi_mask(gsi, false);
}

fn set_gsi_mask(gsi: u32, masked: bool) {
    with_state(|st| set_gsi_mask_inner(st, gsi, masked));
}

fn set_gsi_mask_inner(st: &ApicState, gsi: u32, masked: bool) {
    let Some((io, pin)) = find_ioapic(st, gsi) else {
        return;
    };
    let (lo, hi) = apic::ioapic_redir_regs(pin);
    // SAFETY: invariant I49, established at `arch::x86_64::apic_init::enum_ioapics`:
    // `io.va` is set only there, and the caller holds `STATE`.
    let (high, low) = unsafe { (io_read(io.va, hi), io_read(io.va, lo)) };
    let low = redir_set_mask(low, masked);
    // SAFETY: as just above; established at
    // `arch::x86_64::apic_init::enum_ioapics`.
    write_redir(
        |reg, val| unsafe { io_write(io.va, reg, val) },
        pin,
        high,
        low,
    );
}

pub fn eoi() {
    // Relaxed: the boot CPU publishes it before it starts the APs; pairs with nothing.
    let va = LAPIC_VA.load(Ordering::Relaxed);
    if va != 0 {
        // SAFETY: invariant I49, established at `arch::x86_64::apic_init::enable_lapic`:
        // a nonzero `LAPIC_VA` is its return, published by `publish_isr`.
        unsafe { lapic_write(va, LAPIC_EOI, 0) };
    }
}

/// Whether this CPU's LAPIC has `vec` in service (its ISR bit set): the
/// LAPIC delivered the interrupt being handled and is owed its EOI. False
/// while the LAPIC is unmapped.
pub fn in_service(vec: u8) -> bool {
    // Relaxed: the boot CPU publishes it before it starts the APs; pairs with nothing.
    let va = LAPIC_VA.load(Ordering::Relaxed);
    if va == 0 {
        return false;
    }
    let (off, mask) = apic::isr_reg(vec);
    // SAFETY: invariant I49, established at `arch::x86_64::apic_init::enable_lapic`:
    // a nonzero `LAPIC_VA` is its return, published by `publish_isr`, and
    // `isr_reg` gives an ISR register offset in that page.
    unsafe { lapic_read(va, off) & mask != 0 }
}

pub fn eoi_for(vec: u8) {
    match apic::eoi_domain(vec) {
        EoiDomain::None => {}
        // PIC paths EOI the 8259 themselves (`pit_irq`, `pic::handle`).
        EoiDomain::Pic => {}
        EoiDomain::Lapic => eoi(),
    }
}

/// Write this CPU's ICR, high then low, with IF off between the two
/// writes: a handler that sends an IPI between them would rewrite ICR high,
/// and the low write would then send this IPI to the handler's
/// destination. A caller that has IF off already, as the panic dump has
/// (DESIGN §2.5 step 1), takes no guard.
///
/// # Safety
/// As for [`lapic_read`].
unsafe fn write_icr(va: u64, hi: u32, lo: u32) {
    let _irq = x86::interrupts_enabled().then(|| x86::InterruptGuard::enter());
    // SAFETY: this fn's `# Safety` (here).
    unsafe { lapic_write(va, LAPIC_ICR_HIGH, hi) };
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
    testing::between_icr_writes();
    // SAFETY: this fn's `# Safety` (here).
    unsafe { lapic_write(va, LAPIC_ICR_LOW, lo) };
}

/// ICR high then low ([`write_icr`]); bounded delivery-pending poll.
/// ROADMAP §4.1.
pub fn send_ipi(dest: u8, vector: u8, mode: IpiMode) -> Result<(), IpiError> {
    // Acquire: pairs with the Release store in `publish_isr`.
    let va = LAPIC_VA.load(Ordering::Acquire);
    if va == 0 {
        return Err(IpiError::NotReady);
    }
    // SAFETY: invariant I49, established at `arch::x86_64::apic_init::enable_lapic`:
    // a nonzero `LAPIC_VA` is its return, published by `publish_isr`.
    let icr = || unsafe { lapic_read(va, LAPIC_ICR_LOW) };
    if !poll_delivery_pending(icr, ICR_POLL_CAP) {
        return Err(IpiError::DeliveryPendingTimeout);
    }
    let (hi, lo) = apic::send_ipi_plan(dest, vector, mode);
    // SAFETY: as for `icr`; established at
    // `arch::x86_64::apic_init::enable_lapic`.
    unsafe { write_icr(va, hi, lo) };
    if !poll_delivery_pending(icr, ICR_POLL_CAP) {
        return Err(IpiError::DeliveryPendingTimeout);
    }
    Ok(())
}

pub fn send_ipi_cpu(cpu: u32, vector: u8) -> Result<(), IpiError> {
    vibeos::trace!(IpiSend, u64::from(vector), u64::from(cpu));
    let Some(c) = crate::per_cpu_init::cpu(cpu) else {
        return Err(IpiError::NotReady);
    };
    // Relaxed: set before the CPU starts, fixed while it runs; pairs with nothing.
    send_ipi(
        c.apic_id.load(Ordering::Relaxed) as u8,
        vector,
        IpiMode::Fixed,
    )
}

/// All-excluding-self shorthand. No-op with one online CPU.
pub fn send_ipi_all_ex_self(vector: u8) -> Result<(), IpiError> {
    vibeos::trace!(IpiSend, u64::from(vector), u64::MAX);
    // Acquire: pairs with the Release store in `publish_isr`.
    let va = LAPIC_VA.load(Ordering::Acquire);
    if va == 0 {
        return Err(IpiError::NotReady);
    }
    if crate::per_cpu_init::online_mask().count_ones() <= 1 {
        return Ok(());
    }
    // SAFETY: invariant I49, established at `arch::x86_64::apic_init::enable_lapic`:
    // a nonzero `LAPIC_VA` is its return, published by `publish_isr`.
    let icr = || unsafe { lapic_read(va, LAPIC_ICR_LOW) };
    if !poll_delivery_pending(icr, ICR_POLL_CAP) {
        return Err(IpiError::DeliveryPendingTimeout);
    }
    let (hi, lo) = apic::send_ipi_all_ex_self_plan(vector, IpiMode::Fixed);
    // SAFETY: as for `icr`; established at
    // `arch::x86_64::apic_init::enable_lapic`.
    unsafe { write_icr(va, hi, lo) };
    if !poll_delivery_pending(icr, ICR_POLL_CAP) {
        return Err(IpiError::DeliveryPendingTimeout);
    }
    Ok(())
}

/// LAPIC timer counts per millisecond at divider 16, measured against the
/// HPET over [`PIT_CALIB_MS`] of its time. Each end is the count read the
/// HPET brackets most tightly of `HPET_CALIB_READS`, placed by its
/// bracket's middle, and the rate divides by the HPET ticks between the
/// two ends (`vibeos::apic::lapic_per_ms`): under TCG a host that stalls
/// QEMU past the window's planned end overruns it, and dividing by the plan
/// would stretch the period by the overrun (DESIGN §6.3).
///
/// # Safety
/// `va` as for [`lapic_read`].
unsafe fn calib_periodic(va: u64) -> Option<u64> {
    let (hpet_va, period_fs) = time_init::hpet_ready()?;
    if !hpet_period_ok(period_fs) {
        return None;
    }
    let hpet_now = || {
        // SAFETY: `hpet_va` is what `time::time_init::hpet_ready` returned,
        // `hpet_read_main`'s requirement.
        unsafe { time_init::hpet_read_main(hpet_va) }
    };
    let want = (PIT_CALIB_MS as u128 * FS_PER_MS) / period_fs as u128;
    let want = u64::try_from(want).ok()?;
    if want == 0 {
        return None;
    }
    // SAFETY: this fn's `# Safety` (here): `va` is the LAPIC's UC page.
    unsafe {
        lapic_write(va, LAPIC_TIMER_DCR, TIMER_DIV_16);
        lapic_write(
            va,
            LAPIC_LVT_TIMER,
            apic::lvt_timer_oneshot(vectors::LAPIC_TIMER, true),
        );
        lapic_write(va, LAPIC_TIMER_ICR, 0xFFFF_FFFF);
    }
    let bracketed = || {
        let read = || {
            let hpet_lo = hpet_now();
            // SAFETY: this fn's `# Safety` (here).
            let count = unsafe { lapic_read(va, LAPIC_TIMER_CCR) };
            let hpet_hi = hpet_now();
            CountRead {
                hpet_lo,
                count,
                hpet_hi,
            }
        };
        let mut best = read();
        for _ in 1..HPET_CALIB_READS {
            best = best.narrower(read());
        }
        best
    };
    let start = bracketed();
    let mut spins = 0u64;
    loop {
        let now = hpet_now();
        // The HPET reads 32 bits wide (`time_init::hpet_read_main`).
        if now.wrapping_sub(start.hpet_mid()) & u64::from(u32::MAX) >= want {
            break;
        }
        spins += 1;
        if spins > CALIB_SPIN_CAP {
            // SAFETY: this fn's `# Safety` (here).
            unsafe { lapic_write(va, LAPIC_TIMER_ICR, 0) };
            return None;
        }
        core::hint::spin_loop();
    }
    let end = bracketed();
    // SAFETY: this fn's `# Safety` (here).
    unsafe { lapic_write(va, LAPIC_TIMER_ICR, 0) };
    let per_ms = lapic_per_ms(start, end, period_fs)?;
    if !(LAPIC_TICKS_PER_MS_MIN..=LAPIC_TICKS_PER_MS_MAX).contains(&per_ms) {
        return None;
    }
    Some(per_ms)
}

/// # Safety
/// `va` as for [`lapic_read`], on a CPU whose CPUID reports TSC-deadline.
unsafe fn arm_tsc_deadline(va: u64, tsc_per_ms: u64) {
    let now = time_init::read_tsc();
    for step in tsc_deadline_arm_plan(vectors::LAPIC_TIMER, now, tsc_per_ms) {
        match step {
            // SAFETY: this fn's `# Safety` (here).
            TscDeadlineStep::Lvt(v) => unsafe { lapic_write(va, LAPIC_LVT_TIMER, v) },
            TscDeadlineStep::Mfence => x86::mfence(),
            // SAFETY: this fn's `# Safety` (here): the MSR exists, and the
            // LVT is in TSC-deadline mode, so the write only arms the timer.
            TscDeadlineStep::Deadline(d) => unsafe { x86::wrmsr(IA32_TSC_DEADLINE, d) },
        }
    }
}

/// # Safety
/// `va` as for [`lapic_read`].
unsafe fn arm_periodic(va: u64, ticks_per_ms: u64) {
    let icr = ticks_per_ms as u32;
    let icr = if icr == 0 { 1 } else { icr };
    // SAFETY: this fn's `# Safety` (here).
    unsafe {
        lapic_write(va, LAPIC_TIMER_DCR, TIMER_DIV_16);
        lapic_write(
            va,
            LAPIC_LVT_TIMER,
            lvt_timer_periodic(vectors::LAPIC_TIMER, false),
        );
        lapic_write(va, LAPIC_TIMER_ICR, icr);
    }
}

/// # Safety
/// `va` as for [`lapic_read`], on a CPU whose CPUID reports TSC-deadline.
unsafe fn disarm_timer(va: u64) {
    // SAFETY: this fn's `# Safety` (here): the MSR exists, and zero disarms.
    unsafe {
        x86::wrmsr(IA32_TSC_DEADLINE, 0);
        lapic_write(
            va,
            LAPIC_LVT_TIMER,
            apic::lvt_timer_oneshot(vectors::LAPIC_TIMER, true),
        );
        lapic_write(va, LAPIC_TIMER_ICR, 0);
    }
}

/// Whether the LAPIC timer [`prove`] just armed fires, judged by the
/// interrupts that arrive and not by the time that passes (DESIGN §6.3).
/// It unmasks the PIT on LINT0 ExtINT, the PIT fallback's path, and halts
/// until the timer has fired or the PIT has fired
/// [`vibeos::apic::PROVE_PIT_FIRES`] times without it ([`timer_proof`]);
/// IRQ0 is masked at the PIC again before it returns. Under TCG the TSC
/// and the HPET both follow host time, while QEMU's main loop raises the
/// timer's interrupt: a host that deschedules QEMU for longer than a fixed
/// window lets the window pass before the guest is given the interrupt,
/// but it holds back the PIT's interrupts with the timer's.
fn timer_fires() -> bool {
    unmask_pit_fallback();
    let pit0 = time_init::pit_fires();
    let fired = loop {
        let pit = time_init::pit_fires().wrapping_sub(pit0);
        // Relaxed: bumped by this CPU's own timer interrupt; pairs with nothing.
        match timer_proof(TIMER_FIRES.load(Ordering::Relaxed), pit) {
            TimerProof::Fires => break true,
            TimerProof::Silent => break false,
            // IF is on (`prove`'s contract): the next LAPIC timer or PIT
            // interrupt ends the halt.
            TimerProof::Pending => arch::current::wait_for_interrupt(),
        }
    };
    arch::pic::mask(0);
    fired
}

fn rearm_deadline() {
    // Relaxed: the boot CPU stores it before it starts the APs; pairs with nothing.
    if !TSC_DEADLINE.load(Ordering::Relaxed) {
        return;
    }
    // Relaxed: the boot CPU publishes it before it starts the APs; pairs with nothing.
    if LAPIC_VA.load(Ordering::Relaxed) == 0 {
        return;
    }
    // LVT already in TSC-deadline mode. SDM fence is LVT → deadline only.
    let k = time_init::tsc_per_ms();
    let now = time_init::read_tsc();
    let d = tsc_deadline_value(now, k);
    // SAFETY: `TSC_DEADLINE` is set only once `prove` found the CPUID bit
    // and put the LVT in TSC-deadline mode, so the MSR exists and the write
    // only arms the timer; established at `arch::x86_64::apic_init::prove`.
    unsafe { x86::wrmsr(IA32_TSC_DEADLINE, d) };
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
    testing::rearmed(d.wrapping_sub(now));
}

pub fn on_timer_irq() {
    let cpu_id = crate::per_cpu_init::try_current()
        .map(|c| c.cpu_id)
        .unwrap_or(0);
    if cpu_id == 0 {
        // Relaxed: only CPU 0 counts, and `prove` reads it there; pairs with nothing.
        TIMER_FIRES.fetch_add(1, Ordering::Relaxed);
        time_init::on_hw_tick();
        #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
        testing::stamp_fire();
    }
    eoi();
    rearm_deadline();
    crate::sched_init::on_timer_tick();
}

pub fn on_spurious_irq() {
    // Must not EOI. DESIGN §5.7.
}

pub fn on_error_irq() {
    // Relaxed: the boot CPU publishes it before it starts the APs; pairs with nothing.
    let va = LAPIC_VA.load(Ordering::Relaxed);
    if va != 0 {
        // SAFETY: invariant I49, established at `arch::x86_64::apic_init::enable_lapic`:
        // a nonzero `LAPIC_VA` is its return, published by `publish_isr`.
        let esr = unsafe {
            lapic_write(va, LAPIC_ESR, 0);
            lapic_read(va, LAPIC_ESR)
        };
        crate::marker!("vibeOS: lapic: error esr={:#x}", esr);
        // SAFETY: as just above; established at
        // `arch::x86_64::apic_init::enable_lapic`.
        unsafe { lapic_write(va, LAPIC_ESR, 0) };
    }
    eoi();
}

pub fn on_thermal_irq() {
    crate::marker!("vibeOS: lapic: thermal");
    eoi();
}

fn mask_pic_and_pit(st: &ApicState, isos: &[Iso]) {
    arch::pic::disable_all();
    // SAFETY: invariant I49, established at `arch::x86_64::apic_init::enable_lapic`:
    // `st.lapic_va` is set only from its return.
    unsafe { lapic_write(st.lapic_va, LAPIC_LVT_LINT0, LVT_MASKED) };
    let gsi = apic::gsi_for_isa_irq(0, isos);
    set_gsi_mask_inner(st, gsi, true);
}

fn emit_marker(mode: TimerMode) {
    crate::marker!("{}{})", marker::TIME_LAPIC_PREFIX, mode.as_str());
}

fn unmask_pit_fallback() {
    // Relaxed: the boot CPU publishes it before it starts the APs; pairs with nothing.
    let va = LAPIC_VA.load(Ordering::Relaxed);
    if va != 0 {
        // PIC virtual-wire: ExtINT on LINT0. Masked LINT0 (enable path)
        // swallows IRQ0 even after unmasking the 8259.
        // SAFETY: invariant I49, established at `arch::x86_64::apic_init::enable_lapic`:
        // a nonzero `LAPIC_VA` is its return, published by `publish_isr`.
        unsafe { lapic_write(va, LAPIC_LVT_LINT0, LVT_DELIVERY_EXTINT) };
    }
    arch::pic::unmask(0);
}

/// Enable LAPIC and program every I/O APIC (masked). Timer armed later in [`prove`].
///
/// # Safety
/// IDT live, PIC remapped, LAPIC/IOAPIC UC, IRQs still off.
pub unsafe fn init() {
    let Some(desc) = machine_init::info() else {
        return;
    };
    // SAFETY: this fn's `# Safety` (here) is `enable_lapic`'s: the LAPIC
    // page is UC and IRQs are off.
    let Some(va) = (unsafe { enable_lapic(desc.lapic_base().unwrap_or(0)) }) else {
        return;
    };
    with_state(|st| {
        st.lapic_va = va;
        enum_ioapics(desc.ioapics(), st);
        mask_all_pins(st);
        apply_isos(st, desc.irq_overrides());
        st.ready = true;
        publish_isr(st);
    });
}

/// Start the preferred timer, prove it fires against the PIT
/// ([`timer_fires`]), commit the marker, mask PIC.
///
/// `sti` must already have run. TSC-deadline → periodic (HPET ÷16) → PIT.
pub fn prove() {
    let want_td = cpuid_tsc_deadline();
    let (ready, va) = with_state(|st| {
        if !st.ready {
            st.mode = TimerMode::Pit;
            publish_isr(st);
        }
        (st.ready, st.lapic_va)
    });
    if !ready {
        crate::per_cpu_init::set_timer_mode(TimerMode::Pit);
        unmask_pit_fallback();
        emit_marker(TimerMode::Pit);
        return;
    }
    let tsc_per_ms = time_init::tsc_per_ms();

    if want_td && tsc_per_ms != 0 {
        // Relaxed: only CPU 0 touches it, here and in its timer interrupt; pairs with nothing.
        TIMER_FIRES.store(0, Ordering::Relaxed);
        with_state(|st| {
            st.mode = TimerMode::TscDeadline;
            publish_isr(st);
        });
        // SAFETY: invariant I49, established at `arch::x86_64::apic_init::enable_lapic`:
        // `va` is `st.lapic_va`, set only from its return, and `want_td`
        // is the CPUID TSC-deadline bit.
        unsafe { arm_tsc_deadline(va, tsc_per_ms) };
        if timer_fires() {
            with_state(|st| commit_lapic(st, TimerMode::TscDeadline));
            return;
        }
        // SAFETY: as for `arm_tsc_deadline` just above; established at
        // `arch::x86_64::apic_init::enable_lapic`.
        unsafe { disarm_timer(va) };
        crate::marker!("vibeOS: time: tsc-deadline no ticks");
    }

    // SAFETY: invariant I49, established at `arch::x86_64::apic_init::enable_lapic`:
    // `va` is `st.lapic_va`, set only from its return.
    match unsafe { calib_periodic(va) } {
        Some(per_ms) => {
            // Relaxed: only CPU 0 touches it, here and in its timer interrupt; pairs with nothing.
            TIMER_FIRES.store(0, Ordering::Relaxed);
            with_state(|st| {
                st.ticks_per_ms = per_ms;
                st.mode = TimerMode::Periodic;
                publish_isr(st);
            });
            // SAFETY: as for `calib_periodic` above; established at
            // `arch::x86_64::apic_init::enable_lapic`.
            unsafe { arm_periodic(va, per_ms) };
            if timer_fires() {
                with_state(|st| commit_lapic(st, TimerMode::Periodic));
                return;
            }
            // SAFETY: as above. `disarm_timer`'s TSC-deadline MSR write
            // is what the periodic path always did; established at
            // `arch::x86_64::apic_init::enable_lapic`.
            unsafe { disarm_timer(va) };
            crate::marker!("vibeOS: time: periodic no ticks");
        }
        None => crate::marker!("vibeOS: time: periodic calib refused"),
    }

    with_state(|st| {
        st.mode = TimerMode::Pit;
        st.owns_tick = false;
        publish_isr(st);
    });
    crate::per_cpu_init::set_timer_mode(TimerMode::Pit);
    unmask_pit_fallback();
    emit_marker(TimerMode::Pit);
}

fn commit_lapic(st: &mut ApicState, mode: TimerMode) {
    st.mode = mode;
    st.owns_tick = true;
    publish_isr(st);
    crate::per_cpu_init::set_timer_mode(mode);
    if let Some(desc) = machine_init::info() {
        mask_pic_and_pit(st, desc.irq_overrides());
    } else {
        arch::pic::disable_all();
    }
    emit_marker(mode);
}

pub fn timer_mode() -> TimerMode {
    with_state(|st| st.mode)
}

pub fn owns_tick() -> bool {
    with_state(|st| st.owns_tick)
}

/// Enable this CPU's LAPIC (INIT resets it). Same MMIO VA as the BSP.
///
/// # Safety
/// LAPIC page already UC. IF off.
pub unsafe fn enable_ap() {
    let Some(desc) = machine_init::info() else {
        return;
    };
    // A failure has printed its line; the AP then never reports ready and
    // `smp_init::start_one` times it out.
    // SAFETY: this fn's `# Safety` (here) is `enable_lapic`'s: the LAPIC
    // page is UC and IF is off.
    let _ = unsafe { enable_lapic(desc.lapic_base().unwrap_or(0)) };
}

/// Arm this CPU's timer in the mode the BSP proved. PIT: no local tick.
pub fn arm_ap() {
    with_state(|st| {
        if st.lapic_va == 0 {
            return;
        }
        match st.mode {
            // SAFETY: invariant I49, established at
            // `arch::x86_64::apic_init::enable_lapic`: `st.lapic_va` is set
            // only from its return, every CPU maps the LAPIC at that one VA,
            // and `st.mode` is TSC-deadline only when the BSP's CPUID
            // reported it (`prove`).
            TimerMode::TscDeadline => unsafe {
                arm_tsc_deadline(st.lapic_va, time_init::tsc_per_ms())
            },
            TimerMode::Periodic => {
                if st.ticks_per_ms != 0 {
                    // SAFETY: as just above; established at
                    // `arch::x86_64::apic_init::enable_lapic`.
                    unsafe { arm_periodic(st.lapic_va, st.ticks_per_ms) };
                }
            }
            TimerMode::Pit => {}
        }
    });
}

/// In-guest test hooks. `kernel_tests` only (AGENTS.md rule 9).
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub(crate) mod testing {
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use vibeos::apic::{TIMER_DIV_16, lvt_timer_periodic};
    use vibeos::vectors;

    use super::{LAPIC_LVT_TIMER, LAPIC_TIMER_DCR, LAPIC_TIMER_ICR, LAPIC_VA, lapic_read};
    use crate::time_init;

    /// CPU 0 LAPIC timer fires whose TSC `lapic_timer_rearm` stamps: 20
    /// intervals.
    pub(crate) const FIRE_STAMPS: usize = 21;
    static FIRE_STAMP: [AtomicU64; FIRE_STAMPS] = [const { AtomicU64::new(0) }; FIRE_STAMPS];
    /// Stamps taken since [`arm_fire_stamps`]; `usize::MAX` while unarmed.
    static FIRE_STAMP_N: AtomicUsize = AtomicUsize::new(usize::MAX);

    /// Stamp the TSC at each of CPU 0's next [`FIRE_STAMPS`] timer fires.
    pub(crate) fn arm_fire_stamps() {
        // Release: pairs with the Acquire load in `fire_stamps` and the swap in
        // `take_fire_stamps`.
        FIRE_STAMP_N.store(0, Ordering::Release);
    }

    /// Stamps taken so far.
    pub(crate) fn fire_stamps() -> usize {
        // Acquire: pairs with the Release stores in `stamp_fire` and `arm_fire_stamps`.
        FIRE_STAMP_N.load(Ordering::Acquire)
    }

    /// Disarm and copy the stamps out: how many there were.
    pub(crate) fn take_fire_stamps(out: &mut [u64; FIRE_STAMPS]) -> usize {
        // AcqRel: pairs with the Release store in `stamp_fire`, which publishes
        // the stamps read below.
        let n = FIRE_STAMP_N.swap(usize::MAX, Ordering::AcqRel);
        for (o, s) in out.iter_mut().zip(FIRE_STAMP.iter()) {
            // Relaxed: the swap above acquired the stamps it counts; pairs with nothing.
            *o = s.load(Ordering::Relaxed);
        }
        n
    }

    /// CPU 0's timer handler, with IF off: one writer.
    pub(super) fn stamp_fire() {
        // Relaxed: the count alone picks the slot; pairs with nothing.
        let i = FIRE_STAMP_N.load(Ordering::Relaxed);
        if let Some(slot) = FIRE_STAMP.get(i) {
            // Relaxed: the Release store of the count below publishes it; pairs with nothing.
            slot.store(time_init::read_tsc(), Ordering::Relaxed);
            // Release: pairs with the Acquire load in `fire_stamps` and the swap
            // in `take_fire_stamps`, publishing the stamp with the count.
            FIRE_STAMP_N.store(i + 1, Ordering::Release);
        }
    }

    /// CPU 0's TSC-deadline rearms, and those that did not arm the next
    /// fire one tick (`tsc_per_ms`) ahead, with the last such distance.
    static REARMS: AtomicU64 = AtomicU64::new(0);
    static REARMS_OFF: AtomicU64 = AtomicU64::new(0);
    static REARM_OFF_LAST: AtomicU64 = AtomicU64::new(0);

    /// `rearm_deadline` armed the next fire `ahead` TSC cycles from now.
    pub(super) fn rearmed(ahead: u64) {
        if crate::per_cpu_init::try_current().map(|c| c.cpu_id) != Some(0) {
            return;
        }
        if ahead != time_init::tsc_per_ms() {
            // Relaxed: the Release add of `REARMS` below publishes it; pairs with nothing.
            REARM_OFF_LAST.store(ahead, Ordering::Relaxed);
            // Relaxed: as the store above; pairs with nothing.
            REARMS_OFF.fetch_add(1, Ordering::Relaxed);
        }
        // Release: pairs with the Acquire load in `rearms`.
        REARMS.fetch_add(1, Ordering::Release);
    }

    /// CPU 0's rearms, the rearms not one tick ahead, and the last such
    /// distance in TSC cycles.
    pub(crate) fn rearms() -> (u64, u64, u64) {
        // Acquire: pairs with the Release add in `rearmed`, which publishes the other two.
        // Relaxed: the Acquire load orders them; pairs with nothing.
        (
            REARMS.load(Ordering::Acquire),
            REARMS_OFF.load(Ordering::Relaxed),
            REARM_OFF_LAST.load(Ordering::Relaxed),
        )
    }

    /// The periodic timer as this CPU's LAPIC holds it, `(LVT, initial
    /// count, divide)`, against what the kernel programs for one tick:
    /// `None` when they match. `None` too while the LAPIC is unmapped.
    pub(crate) fn periodic_mismatch() -> Option<([u32; 3], [u32; 3])> {
        // Relaxed: the boot CPU publishes it before it starts the APs; pairs with nothing.
        let va = LAPIC_VA.load(Ordering::Relaxed);
        if va == 0 {
            return None;
        }
        let per_ms = super::with_state(|st| st.ticks_per_ms);
        let icr = u32::try_from(per_ms).unwrap_or(u32::MAX).max(1);
        let want = [
            lvt_timer_periodic(vectors::LAPIC_TIMER, false),
            icr,
            TIMER_DIV_16,
        ];
        let _irq = crate::arch::current::InterruptGuard::enter();
        // SAFETY: invariant I49, established at
        // `arch::x86_64::apic_init::enable_lapic`: a nonzero `LAPIC_VA` is
        // its return, published by `publish_isr`.
        // The LVT's delivery-status bit (12) is the LAPIC's, not the
        // kernel's.
        let got = unsafe {
            [
                lapic_read(va, LAPIC_LVT_TIMER) & !(1 << 12),
                lapic_read(va, LAPIC_TIMER_ICR),
                lapic_read(va, LAPIC_TIMER_DCR),
            ]
        };
        (got != want).then_some((got, want))
    }

    /// [`super::write_icr`] calls that found IF on between the two writes.
    static ICR_IF_ON: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn icr_writes_if_on() -> u64 {
        // Acquire: pairs with the AcqRel add in `between_icr_writes`.
        ICR_IF_ON.load(Ordering::Acquire)
    }

    /// No lock and no guard: the panic dump sends its NMIs through here.
    pub(super) fn between_icr_writes() {
        if super::x86::interrupts_enabled() {
            // AcqRel: pairs with the Acquire load in `icr_writes_if_on`.
            ICR_IF_ON.fetch_add(1, Ordering::AcqRel);
        }
    }
}
