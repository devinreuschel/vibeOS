//! `FlatMem`: a fuzz input as physical memory at [`BASE`] (C-FUZZ).
//!
//! `acpi::walk` skips a table address of 0, so the input maps above it. A
//! read succeeds only inside `[BASE, BASE + len)`.

use vibeos::acpi::PhysMem;

/// Where the input starts: the BIOS area the RSDP search covers.
pub const BASE: u64 = 0x000E_0000;

pub struct FlatMem<'a> {
    data: &'a [u8],
}

impl<'a> FlatMem<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }
}

impl PhysMem for FlatMem<'_> {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let src = addr
            .checked_sub(BASE)
            .and_then(|o| usize::try_from(o).ok())
            .and_then(|o| Some(o..o.checked_add(buf.len())?))
            .and_then(|r| self.data.get(r));
        match src {
            Some(s) => {
                buf.copy_from_slice(s);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_only_inside_the_input() {
        let data = [1u8, 2, 3, 4];
        let m = FlatMem::new(&data);
        let mut b = [0u8; 2];
        assert!(m.read(BASE + 2, &mut b));
        assert_eq!(b, [3, 4]);
        assert!(!m.read(BASE + 3, &mut b));
        assert!(!m.read(BASE - 1, &mut b));
        assert!(!m.read(0, &mut b));
        assert!(!m.read(u64::MAX, &mut b));
        assert!(m.read(BASE + 4, &mut []));
    }
}
