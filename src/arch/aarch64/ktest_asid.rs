//! aarch64 ASID proof: a 4-bit generation allocator's rollovers, checked
//! through real TTBR0 switches (ROADMAP §11.2, PORTABILITY §11.2).

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
        if !crate::arch::aarch64::secondary::map_va(
            l0,
            USER_VA,
            data,
            &mut tables[..],
            &mut n,
            flags,
        ) {
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
