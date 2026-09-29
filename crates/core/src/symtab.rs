//! Sorted kernel symbol table lookup. ROADMAP §5.6.
//!
//! Binary search, no alloc. The table itself is generated at link time
//! into the binary crate; this module is host-testable.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub addr: u64,
    pub name: &'static str,
}

/// Greatest `addr <= query`. `None` if the table is empty or `query`
/// is before the first symbol.
pub fn lookup(table: &[Entry], addr: u64) -> Option<&Entry> {
    if table.is_empty() || addr < table[0].addr {
        return None;
    }
    let mut lo = 0usize;
    let mut hi = table.len();
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if table[mid].addr <= addr {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(&table[lo])
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
