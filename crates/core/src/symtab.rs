//! Sorted kernel symbol table lookup. ROADMAP §5.6.
//!
//! Binary search, no alloc. The table itself is generated at link time
//! into the binary crate's `.ksyms` section, which the kernel finds through
//! linker bounds (DESIGN §2.5); this module is host-testable.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub addr: u64,
    pub name: &'static str,
}

/// How many whole `Entry` values fill the byte span `start..end`. `None`
/// if `end < start` or the span is not a whole number of entries.
pub fn entries_between(start: usize, end: usize) -> Option<usize> {
    let bytes = end.checked_sub(start)?;
    let size = core::mem::size_of::<Entry>();
    if !bytes.is_multiple_of(size) {
        return None;
    }
    bytes.checked_div(size)
}

/// Greatest `addr <= query` in a table sorted by address. `None` if the
/// table is empty or `query` is before the first symbol.
pub fn lookup(table: &[Entry], addr: u64) -> Option<&Entry> {
    let after = table.partition_point(|e| e.addr <= addr);
    table.get(after.checked_sub(1)?)
}

/// Offset from the looked-up symbol, if the gap is plausible.
pub fn offset(entry: &Entry, addr: u64) -> u64 {
    addr.saturating_sub(entry.addr)
}

/// How far past its symbol a frame may lie and still be named: a sparse
/// table (the panic-test kernel's) would otherwise pin an address to an
/// earlier function with a huge offset.
pub const FRAME_REACH: u64 = 0x1_0000;

/// The function a backtrace frame at `addr` is in, and `addr`'s offset
/// into it, as the dump, the in-guest tests and the core tool name it
/// (DESIGN §2.5 step 4). `ret` marks a return address, every frame but the
/// first: it is named from `addr - 1`, inside its call, since a call to a
/// function that never returns is often its caller's last instruction and
/// the address after it is the next symbol's (an aarch64 `bl` to
/// `panic_fmt`). The first frame is a PC and is named from `addr` itself.
/// `None` past [`FRAME_REACH`].
pub fn frame_name(table: &[Entry], addr: u64, ret: bool) -> Option<(&'static str, u64)> {
    let at = if ret { addr.saturating_sub(1) } else { addr };
    let e = lookup(table, at)?;
    let off = offset(e, addr);
    (off < FRAME_REACH).then_some((e.name, off))
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &[Entry] = &[
        Entry {
            addr: 0x1000,
            name: "start",
        },
        Entry {
            addr: 0x1100,
            name: "mid",
        },
        Entry {
            addr: 0x2000,
            name: "end",
        },
    ];

    #[test]
    fn exact_and_interior() {
        assert_eq!(lookup(T, 0x1000).unwrap().name, "start");
        assert_eq!(lookup(T, 0x10FF).unwrap().name, "start");
        assert_eq!(lookup(T, 0x1100).unwrap().name, "mid");
        assert_eq!(lookup(T, 0x1FFF).unwrap().name, "mid");
        assert_eq!(lookup(T, 0x2000).unwrap().name, "end");
        assert_eq!(lookup(T, 0x2001).unwrap().name, "end");
        assert_eq!(offset(&T[1], 0x1105), 5);
    }

    #[test]
    fn before_first_is_none() {
        assert!(lookup(T, 0x0FFF).is_none());
        assert!(lookup(&[], 0x1000).is_none());
    }

    const SIZE: usize = core::mem::size_of::<Entry>();

    #[test]
    fn entries_between_empty() {
        assert_eq!(entries_between(0x1000, 0x1000), Some(0));
        assert!(lookup(&T[..0], 0x1000).is_none());
    }

    #[test]
    fn entries_between_three() {
        let base = T.as_ptr() as usize;
        assert_eq!(entries_between(base, base + 3 * SIZE), Some(T.len()));
    }

    #[test]
    fn entries_between_reversed() {
        assert_eq!(entries_between(0x1000 + SIZE, 0x1000), None);
    }

    #[test]
    fn entries_between_ragged() {
        assert_eq!(entries_between(0x1000, 0x1000 + SIZE + 1), None);
        assert_eq!(entries_between(0x1000, 0x1000 + SIZE - 1), None);
    }

    #[test]
    fn single_entry() {
        let t = [Entry {
            addr: 0x8000,
            name: "only",
        }];
        assert!(lookup(&t, 0x7FFF).is_none());
        assert_eq!(lookup(&t, 0x8000).unwrap().name, "only");
        assert_eq!(lookup(&t, 0x9000).unwrap().name, "only");
    }

    #[test]
    fn frame_name_names_a_return_address_from_its_call() {
        // A call that is `start`'s last instruction returns to `mid`'s
        // first byte: as a return address it is `start`'s, as a PC `mid`'s.
        assert_eq!(frame_name(T, 0x1100, true), Some(("start", 0x100)));
        assert_eq!(frame_name(T, 0x1100, false), Some(("mid", 0)));
        assert_eq!(frame_name(T, 0x1104, true), Some(("mid", 4)));
        assert_eq!(frame_name(T, 0x1000, true), None);
        assert_eq!(frame_name(T, 0xFFF, false), None);
    }

    #[test]
    fn frame_name_reach() {
        let far = T[T.len() - 1].addr + FRAME_REACH;
        assert_eq!(frame_name(T, far, false), None);
        assert!(frame_name(T, far - 1, false).is_some());
    }
}
