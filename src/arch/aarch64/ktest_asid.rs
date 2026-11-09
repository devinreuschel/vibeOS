//! aarch64 ASID proof: a 4-bit generation allocator's rollovers, checked
//! through real TTBR0 switches (ROADMAP §11.2, PORTABILITY §11.2), and the
//! TTBR0 each AP leaves the secondary stub on (ROADMAP §11.4).

use core::sync::atomic::{AtomicU64, Ordering};

use vibeos::kalloc::TryBox;
use vibeos::mm::asid::{self, AsidAlloc};
use vibeos::paging::PageFlags;
use vibeos::pmm::Frames;

use crate::arch::current::InterruptGuard;
use crate::ktest::Outcome;

const USER_VA: u64 = 0x1_0000;
const ASID_SHIFT: u64 = 48;
const ASID_SPACES: u32 = 1000;
const ASID_DATA_WINDOW: usize = 64;
/// The frames `test_asid_4bit` keeps mapped. Static, not a `TryBox`: a
/// `try_new([0; N])` builds its array on the stack first.
static ASID_WINDOW: [AtomicU64; ASID_DATA_WINDOW] = [const { AtomicU64::new(0) }; ASID_DATA_WINDOW];
fn free_pa(pa: u64) {
    if pa == 0 {
        return;
    }
    // SAFETY: `pa` is a buddy page this test allocated and no longer maps.
    // established here.
    unsafe {
        crate::pmm_init::with_buddy(|b| b.free(Frames::from_entry(pa, 0)));
    }
}

pub(crate) fn test_asid_4bit() -> Outcome {
    let ncpus = crate::per_cpu_init::cpu_count().max(2) as u32;
    let Some(alloc) = AsidAlloc::try_boxed(4, ncpus) else {
        return Outcome::Fail("4-bit AsidAlloc");
    };
    let saved = crate::arch::aarch64::cpu::read_ttbr0();
    let cpu = 0u32;
    let flags = PageFlags::empty()
        .with(PageFlags::PRESENT)
        .with(PageFlags::WRITABLE)
        .with(PageFlags::USER);
    // The allocator starts with no flush pending, so this CPU's TLB may hold
    // entries from an earlier run of this test. Flush before the first space.
    crate::arch::aarch64::cpu::tlbi_all();
    let mut i = 0u32;
    while i < ASID_SPACES {
        if crate::ktest::deadline_within(1_000) == Some(true) {
            // SAFETY: restore the empty TTBR0 this CPU had. established here.
            unsafe { crate::arch::aarch64::cpu::write_ttbr0(saved) };
            return Outcome::Fail("asid deadline");
        }
        let Some(l0) = crate::arch::aarch64::secondary::alloc_zeroed_page() else {
            return Outcome::Fail("asid l0");
        };
        let Some(data) = crate::arch::aarch64::secondary::alloc_zeroed_page() else {
            free_pa(l0);
            return Outcome::Fail("asid data");
        };
        // The word goes in through the physmap, so the read through USER_VA
        // below is the first access through this space's translation.
        let pattern = 0xA5A5_0000u64 | u64::from(i);
        let phys = crate::paging_init::hhdm_offset().wrapping_add(data) as *mut u64;
        // SAFETY: `data` is a buddy page this test owns, and HHDM maps it
        // (I14). established here.
        unsafe { phys.write_volatile(pattern) };
        let Ok(mut tables) = TryBox::try_new([0u64; 8]) else {
            free_pa(data);
            free_pa(l0);
            return Outcome::Fail("asid tables");
        };
        let mut n = 0usize;
        tables[n] = l0;
        n += 1;
        // SAFETY: `l0` is the root this test allocated, an HHDM-mapped table page. established here.
        if !unsafe {
            crate::arch::aarch64::secondary::map_va(
                l0,
                USER_VA,
                data,
                &mut tables[..],
                &mut n,
                flags,
            )
        } {
            let mut t = 0;
            while t < n {
                free_pa(tables[t]);
                t += 1;
            }
            free_pa(data);
            return Outcome::Fail("asid map");
        }
        let mut word = 0u64;
        let packed = asid::switch(&alloc, cpu, &mut word);
        if packed == 0 {
            let mut t = 0;
            while t < n {
                free_pa(tables[t]);
                t += 1;
            }
            free_pa(data);
            return Outcome::Fail("asid alloc");
        }
        // Only a rollover sets flush-pending, so only a rollover flushes.
        // Deleting this flush left the test green under TCG, where every switch
        // here changes the ASID. HVF did not reach the test with or without the
        // flush, so no accel here is shown to catch a missing flush.
        if alloc.take_flush(cpu) {
            crate::arch::aarch64::cpu::tlbi_all();
        }
        let asid = u64::from(alloc.asid_of(packed));
        let got = {
            // IRQs off while the test's TTBR0 is live: a switch away would load
            // another root under this space's ASID.
            let _irq = InterruptGuard::enter();
            // SAFETY: `l0` is this space's TTBR0 root; ASID is the allocator's.
            // established here.
            unsafe { crate::arch::aarch64::cpu::write_ttbr0(l0 | (asid << ASID_SHIFT)) };
            crate::arch::aarch64::cpu::clear_pan();
            // SAFETY: USER_VA is mapped in this TTBR0; PAN is clear. established here.
            let got = unsafe { core::ptr::read_volatile(USER_VA as *const u64) };
            crate::arch::aarch64::cpu::set_pan();
            // SAFETY: `saved` is the empty root this CPU had. established here.
            unsafe { crate::arch::aarch64::cpu::write_ttbr0(saved) };
            got
        };
        let mut t = 0;
        while t < n {
            free_pa(tables[t]);
            t += 1;
        }
        // Keep this space's frame until ASID_DATA_WINDOW later spaces have
        // taken theirs, so a stale entry never names this space's frame.
        let slot = (i as usize) % ASID_DATA_WINDOW;
        // Relaxed: this test is the only writer of the window; pairs with nothing.
        free_pa(ASID_WINDOW[slot].swap(data, Ordering::Relaxed));
        if got != pattern {
            return crate::fail_fmt!("asid {i} got {got:#x} want {pattern:#x}");
        }
        i += 1;
    }
    let mut s = 0;
    while s < ASID_DATA_WINDOW {
        // Relaxed: as above; pairs with nothing.
        free_pa(ASID_WINDOW[s].swap(0, Ordering::Relaxed));
        s += 1;
    }
    crate::arch::aarch64::cpu::tlbi_all();
    Outcome::Ok
}

static TTBR0_ID_LIVE: AtomicU64 = AtomicU64::new(0);
static TTBR0_SEEN: AtomicU64 = AtomicU64::new(0);

fn ttbr0_on_ap(_: *mut ()) {
    let id = crate::per_cpu_init::current().cpu_id;
    if id >= 64 {
        return;
    }
    let empty = crate::paging_init::empty_user_root();
    let live =
        crate::arch::aarch64::cpu::read_ttbr0() & vibeos::arch::aarch64::paging::DESC_ADDR_MASK;
    // A user thread's root may map any low address; test only the empty one.
    if live == empty
        && crate::arch::aarch64::secondary::entry_pa()
            .is_some_and(crate::arch::aarch64::cpu::stage1_read_translates)
    {
        // Relaxed: pairs with nothing; the Release `fetch_or` on `TTBR0_SEEN` below publishes it.
        TTBR0_ID_LIVE.fetch_or(1u64 << id, Ordering::Relaxed);
    }
    // Release: pairs with the Acquire load in `test_ap_ttbr0_empty`.
    TTBR0_SEEN.fetch_or(1u64 << id, Ordering::Release);
}

/// Every AP left the secondary stub with TTBR0 at the empty user root, and
/// the stub's identity address faults there, on arrival and now (ROADMAP
/// §11.4).
pub(crate) fn test_ap_ttbr0_empty() -> Outcome {
    let empty = crate::paging_init::empty_user_root();
    if empty == 0 {
        return Outcome::Fail("no empty TTBR0");
    }
    let Some(stub) = crate::arch::aarch64::secondary::entry_pa() else {
        return Outcome::Fail("no stub pa");
    };
    let aps = super::ktest::ap_mask();
    if aps == 0 {
        return Outcome::Skip("no AP");
    }
    let mut i = 1u32;
    while i < 64 {
        if aps & (1u64 << i) != 0 {
            let Some((ttbr0, id_live)) = crate::arch::aarch64::secondary::ap_entry_ttbr0(i) else {
                return crate::fail_fmt!("cpu {i} recorded no arrival ttbr0");
            };
            let root = ttbr0 & vibeos::arch::aarch64::paging::DESC_ADDR_MASK;
            if root != empty {
                return crate::fail_fmt!("cpu {i} arrived on ttbr0 {root:#x} != empty {empty:#x}");
            }
            if id_live {
                return crate::fail_fmt!("cpu {i} arrived with stub pa {stub:#x} mapped");
            }
        }
        i += 1;
    }
    TTBR0_ID_LIVE.store(0, Ordering::SeqCst);
    TTBR0_SEEN.store(0, Ordering::SeqCst);
    crate::ipi_init::call_mask(aps, ttbr0_on_ap, core::ptr::null_mut(), true);
    // Acquire: pairs with the Release `fetch_or` in `ttbr0_on_ap`.
    if TTBR0_SEEN.load(Ordering::Acquire) != aps {
        return Outcome::Fail("ap did not probe ttbr0");
    }
    // Relaxed: pairs with nothing; the Acquire load of `TTBR0_SEEN` above orders it.
    let bad = TTBR0_ID_LIVE.load(Ordering::Relaxed);
    if bad != 0 {
        return crate::fail_fmt!("stub pa {stub:#x} translates on aps {bad:#x}");
    }
    Outcome::Ok
}
