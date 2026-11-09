//! BAR claim and probe-failure in-guest tests.

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::dev::{
    ClaimError, DevRef, DevState, Driver, IdMatch, Instance, ProbeError, Resource, ResourceKind,
};
use vibeos::kalloc::TryBox;
use vibeos::paging::{PageFlags, VirtAddr};
use vibeos::pci::{self, Bdf};

use crate::dev_init;
use crate::ktest::{EDU_IDENT, EDU_IDENT_VAL, Outcome, bar0_va, find_edu, mmio_r32};
use crate::log_init;
use crate::paging_init;
use crate::pci_init;

use super::{bind_bar_test_driver, find_id, usable_page};

#[cfg(target_arch = "x86_64")]
use crate::heap_init::{self, fail_after::Scope};
#[cfg(target_arch = "x86_64")]
use crate::thread_init;

/// Set while `dev_probe_alloc_fail` binds, so `ktest-nomem` matches PIIX4
/// ACPI only then and later `bind_all` calls ignore it.
static NOMEM_ARMED: AtomicBool = AtomicBool::new(false);

static NOMEM_IDS: &[IdMatch] = &[IdMatch::vid_did(0x8086, 0x7113)];

/// A driver whose probe allocates, for PIIX4 ACPI, which no other driver
/// binds (ROADMAP §10.4's fallible-allocation box).
struct NoMemDrv;

static NOMEM_DRV: NoMemDrv = NoMemDrv;

impl Driver for NoMemDrv {
    fn name(&self) -> &'static str {
        "ktest-nomem"
    }
    fn ids(&self) -> &'static [IdMatch] {
        if NOMEM_ARMED.load(Ordering::Acquire) {
            NOMEM_IDS
        } else {
            &[]
        }
    }
    fn probe(&self, _dev: &DevRef) -> Result<Option<Instance>, ProbeError> {
        let b = TryBox::try_new([0u8; 64])?;
        drop(b);
        Ok(None)
    }
    fn remove(&self, _dev: &DevRef) {}
}

/// Whether a record written after `mark` (a `log_init::written` count)
/// contains every one of `needles`.
fn logged_since(mark: u64, needles: &[&str]) -> bool {
    let new = log_init::written().saturating_sub(mark) as usize;
    let len = log_init::ring_len();
    (len.saturating_sub(new)..len).any(|i| {
        log_init::record_at(i).is_some_and(|r| {
            let m = r.msg();
            needles.iter().all(|n| {
                let n = n.as_bytes();
                m.windows(n.len()).any(|w| w == n)
            })
        })
    })
}

/// A probe whose allocation fails leaves its device unbound, with a log
/// line naming the driver and the device, and the kernel up.
#[cfg(target_arch = "x86_64")]
pub(crate) fn test_dev_probe_alloc_fail() -> Outcome {
    let registered = dev_init::register_driver(&NOMEM_DRV) || {
        let g = dev_init::REG.lock();
        (0..g.driver_count()).any(|i| g.driver_at(i).is_some_and(|d| d.name() == "ktest-nomem"))
    };
    if !registered {
        return Outcome::Fail("register");
    }
    let Some(before) = find_id(0x8086, 0x7113) else {
        return Outcome::Skip("no PIIX4 ACPI function");
    };
    if dev_init::bound(&before).is_some() {
        return Outcome::Fail("already bound");
    }
    let mark = log_init::written();
    NOMEM_ARMED.store(true, Ordering::Release);
    heap_init::fail_after::arm(0, Scope::Thread(thread_init::current_id()));
    dev_init::bind_all();
    let seen = heap_init::fail_after::disarm();
    NOMEM_ARMED.store(false, Ordering::Release);
    let Some(after) = find_id(0x8086, 0x7113) else {
        return Outcome::Fail("device gone");
    };
    if dev_init::bound(&after).is_some() {
        return Outcome::Fail("bound");
    }
    let bdf = alloc::format!("{}", after.addr);
    if !logged_since(mark, &["probe ktest-nomem", &bdf]) {
        return Outcome::Fail("no log line");
    }
    if seen.refused < 1 {
        return Outcome::Fail("not refused");
    }
    if TryBox::try_new(0u64).is_err() {
        return Outcome::Fail("alloc after disarm");
    }
    Outcome::Ok
}

/// The id of the `kernel_tests` record whose BAR0 lies in usable RAM.
static RAM_DEV_ID: vibeos::atomic::statics::AtomicU64 = vibeos::atomic::statics::AtomicU64::new(0);

/// The `kernel_tests` record with BAR0 on a usable RAM page, registered
/// on first use.
#[cfg(target_arch = "x86_64")]
fn ram_bar_device() -> Option<DevRef> {
    let id = RAM_DEV_ID.load(Ordering::Acquire);
    if id != 0 {
        return dev_init::REG.lock().by_id(id);
    }
    let page = usable_page()?;
    let mut d = vibeos::dev::Device::empty();
    d.addr = Bdf::new(0, 0x1f, 7);
    d.resources[0] = Resource {
        kind: ResourceKind::Memory,
        bar: 0,
        addr: page,
        size: 0x1000,
        prefetchable: false,
    };
    let id = dev_init::push_test_device(d)?;
    RAM_DEV_ID.store(id, Ordering::Release);
    dev_init::REG.lock().by_id(id)
}

/// ROADMAP §10.12 (F115): every memory BAR of every bound device is in the
/// claims table, a second claim of one is `Already`, a claim over usable
/// RAM is `Ram`, and an unbound device keeps no claim.
#[cfg(target_arch = "x86_64")]
pub(crate) fn test_dev_bar_claims() -> Outcome {
    bind_bar_test_driver();
    let mut i = 0usize;
    let mut held = 0usize;
    while let Some(d) = dev_init::get(i) {
        i += 1;
        if dev_init::state(&d) != Some(DevState::Bound) {
            continue;
        }
        for (b, r) in d.resources.iter().enumerate() {
            if r.is_empty() || r.kind != ResourceKind::Memory {
                continue;
            }
            if !dev_init::is_claimed(&d, b as u8) {
                return crate::fail_fmt!("{} bar{b} bound but unclaimed", d.addr);
            }
            held += 1;
        }
    }
    if held == 0 {
        return Outcome::Fail("no bound device holds a memory BAR");
    }
    let Some(edu) = find_edu() else {
        return Outcome::Skip("no edu");
    };
    if dev_init::bound(&edu) != Some("bar-test") {
        return Outcome::Fail("edu not bound to bar-test");
    }
    match dev_init::claim(&edu, 0) {
        Err(ClaimError::Already) => {}
        Err(e) => return crate::fail_fmt!("second claim: {}", e.as_str()),
        Ok(c) => {
            dev_init::release(c);
            return Outcome::Fail("second claim granted");
        }
    }
    let Some(ram) = ram_bar_device() else {
        return Outcome::Fail("no usable page or table full");
    };
    match dev_init::claim(&ram, 0) {
        Err(ClaimError::Ram) => {}
        Err(e) => return crate::fail_fmt!("claim over RAM: {}", e.as_str()),
        Ok(c) => {
            dev_init::release(c);
            return Outcome::Fail("claim over RAM granted");
        }
    }
    if !dev_init::unbind(&edu) {
        return Outcome::Fail("unbind edu");
    }
    let left = (0..pci::MAX_BARS as u8).find(|&b| dev_init::is_claimed(&edu, b));
    let mapped = dev_init::bar_va(&edu, 0).is_some();
    let state = dev_init::state(&edu);
    let fresh = dev_init::claim(&edu, 0);
    let fresh_ok = fresh.is_ok();
    if let Ok(c) = fresh {
        dev_init::release(c);
    }
    // Bind it again whatever the checks found, for the tests after this.
    dev_init::bind_all();
    if let Some(b) = left {
        return crate::fail_fmt!("edu bar{b} still claimed after unbind");
    }
    if mapped {
        return Outcome::Fail("edu bar0 still mapped after unbind");
    }
    if state != Some(DevState::Present) {
        return Outcome::Fail("edu not Present after unbind");
    }
    if !fresh_ok {
        return Outcome::Fail("edu bar0 not claimable after unbind");
    }
    if dev_init::bound(&edu) != Some("bar-test") {
        return Outcome::Fail("edu not bound again");
    }
    match bar0_va(&edu) {
        Some(mmio) if mmio_r32(mmio, EDU_IDENT) == EDU_IDENT_VAL => Outcome::Ok,
        Some(_) => Outcome::Fail("edu ident after rebind"),
        None => Outcome::Fail("edu bar0 after rebind"),
    }
}

/// ROADMAP §10.12 (F115): `map_mmio` refuses a page of usable RAM, and
/// that page's physmap leaf stays write-back.
#[cfg(target_arch = "x86_64")]
pub(crate) fn test_map_mmio_refuses_ram() -> Outcome {
    let Some(page) = usable_page() else {
        return Outcome::Fail("no usable page above 1 MiB");
    };
    if let Some(va) = pci_init::map_mmio(page, 0x1000) {
        return crate::fail_fmt!("usable page {page:#x} mapped at {va:#x}");
    }
    let Some((_, _, flags)) =
        paging_init::translate(VirtAddr(paging_init::hhdm_offset().wrapping_add(page)))
    else {
        return crate::fail_fmt!("usable page {page:#x} not on the physmap");
    };
    if flags.contains(PageFlags::PCD) || flags.contains(PageFlags::PWT) {
        return crate::fail_fmt!("usable page {page:#x} physmap leaf not write-back");
    }
    Outcome::Ok
}
