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
}
