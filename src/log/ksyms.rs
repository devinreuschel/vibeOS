//! In-image symbol table (DESIGN §2.5). `scripts/gen_ksyms.py` fills it,
//! via `build.rs`, as a `#[used]` static in its own `.ksyms` section that no
//! code here names: `lookup` reaches it only through `linker.ld`'s
//! `__ksyms_start` and `__ksyms_end`, so the code that reads it is the same
//! size whether the table is empty (the first link) or filled (the second),
//! and the second link leaves `.text` in place.

use vibeos::symtab::{self, Entry};

include!(concat!(env!("OUT_DIR"), "/ksyms.rs"));

unsafe extern "C" {
    static __ksyms_start: u8;
    static __ksyms_end: u8;
}

/// The symbol at or before `addr`, copied out of the table.
pub(crate) fn lookup(addr: u64) -> Option<Entry> {
    let start = (&raw const __ksyms_start).cast::<Entry>();
    let end = (&raw const __ksyms_end).addr();
    let n = symtab::entries_between(start.addr(), end)?;
    if !start.is_aligned() {
        return None;
    }
    // SAFETY: `__ksyms_start..__ksyms_end` holds exactly the initialised,
    // aligned `Entry` values of `KSYMS`, read-only and mapped for the image's
    // life, and the span is whole entries; the slice stays local to this
    // call, which returns a copy; established by `log::ksyms::KSYMS` (which
    // `scripts/gen_ksyms.py` generates), `linker.ld`'s `.ksyms` section, and
    // `vibeos::symtab::entries_between` for the span.
    let table = unsafe { core::slice::from_raw_parts(start, n) };
    symtab::lookup(table, addr).copied()
}
