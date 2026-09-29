//! In-guest tests for boot (kernel_tests only). Rows: the list in crate::ktest.

use vibeos::paging::VirtAddr;

use crate::ktest::Outcome;
use crate::paging_init;

/// `BootInfo` agrees with what PMM and paging built from it.
pub(crate) fn test_bootinfo_consistent() -> Outcome {
    let info = crate::boot::info();
    let k = &info.kernel_phys;
    if info.usable().any(|r| r.start < k.end && k.start < r.end) {
        return Outcome::Fail("kernel image in usable ram");
    }
    let text = VirtAddr(test_bootinfo_consistent as *const () as u64);
    match paging_init::translate(text) {
        Some((pa, _, _)) if k.contains(&pa.as_u64()) => {}
        _ => return Outcome::Fail("text outside kernel span"),
    }
    let map_end = paging_init::map_end();
    if info.framebuffers().any(|fb| fb.phys + fb.size > map_end) {
        return Outcome::Fail("fb outside physmap");
    }
    Outcome::Ok
}
