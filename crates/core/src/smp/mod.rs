//! AP trampoline layout. DESIGN §7.3 / ROADMAP §4.4.
//!
//! Portable: offsets, the SIPI vector and blob rebasing for the page
//! `boot::capture` chooses from the memory map (`vibeos::pmm::
//! choose_trampoline_page`), timeouts. The `.trampoline` blob and INIT/SIPI
//! live in the binary crate. One AP at a time: they share this page.

pub mod per_cpu;

pub const TRAMPOLINE_PAGES: u64 = 1;

/// Param block. Blob must fit strictly below this.
pub const PARAM_OFF: usize = 0xD0;
pub const PARAM_CR3: usize = 0xD0;
pub const PARAM_STACK: usize = 0xD8;
pub const PARAM_ENTRY: usize = 0xE0;
pub const PARAM_IDT: usize = 0xE8;
pub const PARAM_IDT_LEN: usize = 10;

/// Offset in the blob of each 32-bit absolute operand that `trampoline.S`
/// assembles as if the blob sat at 0 and [`patch_blob`] rebases onto the
/// chosen page (DESIGN §7.3): the far jump into 32-bit code, the CR3 load
/// from the param block, the far jump into 64-bit code, and the GDT
/// pointer's base. `trampoline.S` pins each with `.org` and exports a label
/// at it, which `smp_init` checks against this list at boot.
pub const PATCH_SITES: &[usize] = &[0x2A, 0x3D, 0x6D, 0xCA];

/// Why [`patch_blob`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchError {
    /// The page is not 4 KiB aligned.
    Unaligned,
    /// The page is frame 0 or does not end at or below 1 MiB.
    OutOfRange,
    /// The blob ends before a patch site's 4 bytes.
    Short,
    /// An operand plus the page base does not fit 32 bits.
    Overflow,
}

/// Whether `page` can hold the trampoline: 4 KiB aligned, above frame 0,
/// and wholly below 1 MiB.
fn page_ok(page: u64) -> Result<(), PatchError> {
    if !page.is_multiple_of(0x1000) {
        return Err(PatchError::Unaligned);
    }
    if !(0x1000..0x10_0000).contains(&page) {
        return Err(PatchError::OutOfRange);
    }
    Ok(())
}

/// The SIPI vector that starts an AP at `page`: its page number
/// (`CS:IP = vector << 8 : 0`), or `None` when `page` cannot hold the
/// trampoline.
pub fn sipi_vector(page: u64) -> Option<u8> {
    page_ok(page).ok()?;
    u8::try_from(page >> 12).ok()
}

/// Add `page` to each operand [`PATCH_SITES`] names in `blob`, a copy of
/// the blob assembled at base 0. Leaves `blob` unchanged on an error.
pub fn patch_blob(blob: &mut [u8], page: u64) -> Result<(), PatchError> {
    page_ok(page)?;
    let base = u32::try_from(page).map_err(|_| PatchError::Overflow)?;
    let mut patched = [0u32; PATCH_SITES.len()];
    for (out, &at) in patched.iter_mut().zip(PATCH_SITES) {
        let end = at.checked_add(4).ok_or(PatchError::Short)?;
        let bytes: [u8; 4] = blob
            .get(at..end)
            .and_then(|b| b.try_into().ok())
            .ok_or(PatchError::Short)?;
        *out = u32::from_le_bytes(bytes)
            .checked_add(base)
            .ok_or(PatchError::Overflow)?;
    }
    for (v, &at) in patched.iter().zip(PATCH_SITES) {
        if let Some(b) = blob.get_mut(at..at.saturating_add(4)) {
            b.copy_from_slice(&v.to_le_bytes());
        }
    }
    Ok(())
}

/// Intel minimum after INIT. DESIGN §7.4.
pub const INIT_WAIT_MS: u64 = 10;
/// Gap between the two SIPIs.
pub const SIPI_WAIT_MS: u64 = 1;
/// Ready-flag timeout. On expiry free the stack and per-CPU area.
pub const READY_TIMEOUT_MS: u64 = 3000;

pub const fn blob_fits(len: usize) -> bool {
    len <= PARAM_OFF
}

/// 10-byte IDTR image at [`PARAM_IDT`].
pub fn pack_idtr(limit: u16, base: u64) -> [u8; PARAM_IDT_LEN] {
    let mut b = [0u8; PARAM_IDT_LEN];
    b[0] = limit as u8;
    b[1] = (limit >> 8) as u8;
    let base_bytes = base.to_le_bytes();
    let mut i = 0;
    while i < 8 {
        b[2 + i] = base_bytes[i];
        i += 1;
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sipi_vector_is_page_number() {
        assert_eq!(sipi_vector(0x52000), Some(0x52));
        assert_eq!(sipi_vector(0x1000), Some(0x01));
        assert_eq!(sipi_vector(0x9F000), Some(0x9F));
        assert_eq!(sipi_vector(0xFF000), Some(0xFF));
        assert_eq!(sipi_vector(0), None);
        assert_eq!(sipi_vector(0x52001), None);
        assert_eq!(sipi_vector(0x10_0000), None);
    }

    /// A blob of `PARAM_OFF` bytes with operand `i` at each patch site.
    fn blob_with_operands() -> [u8; PARAM_OFF] {
        let mut b = [0xCCu8; PARAM_OFF];
        for (i, &at) in PATCH_SITES.iter().enumerate() {
            let v = 0x10 * (i as u32 + 1);
            b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    #[test]
    fn patch_blob_adds_page_base() {
        let mut b = blob_with_operands();
        patch_blob(&mut b, 0x52000).unwrap();
        for (i, &at) in PATCH_SITES.iter().enumerate() {
            let v = u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
            assert_eq!(v, 0x52000 + 0x10 * (i as u32 + 1));
        }
        for w in PATCH_SITES.windows(2) {
            assert!(w[0] + 4 <= w[1], "patch sites overlap or are unsorted");
        }
        for (i, byte) in b.iter().enumerate() {
            if !PATCH_SITES.iter().any(|&at| (at..at + 4).contains(&i)) {
                assert_eq!(*byte, 0xCC, "byte {i:#x} outside every site changed");
            }
        }
        assert!(PATCH_SITES.iter().all(|&at| at + 4 <= PARAM_OFF));
    }

    #[test]
    fn patch_blob_rejects_bad_page() {
        let orig = blob_with_operands();
        let mut b = orig;
        assert_eq!(patch_blob(&mut b, 0x52010), Err(PatchError::Unaligned));
        assert_eq!(patch_blob(&mut b, 0), Err(PatchError::OutOfRange));
        assert_eq!(patch_blob(&mut b, 0x10_0000), Err(PatchError::OutOfRange));
        assert_eq!(
            patch_blob(&mut b, 0xFFFF_FFFF_FFFF_F000),
            Err(PatchError::OutOfRange)
        );
        assert_eq!(b, orig);
        let mut short = [0u8; 0x40];
        assert_eq!(patch_blob(&mut short, 0x1000), Err(PatchError::Short));
        assert_eq!(short, [0u8; 0x40]);
        let mut big = orig;
        let at = PATCH_SITES[0];
        big[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let before = big;
        assert_eq!(patch_blob(&mut big, 0x1000), Err(PatchError::Overflow));
        assert_eq!(big, before);
    }

    #[test]
    fn param_block_offsets() {
        assert_eq!(PARAM_CR3, 0xD0);
        assert_eq!(PARAM_STACK, 0xD8);
        assert_eq!(PARAM_ENTRY, 0xE0);
        assert_eq!(PARAM_IDT, 0xE8);
        assert_eq!(PARAM_IDT_LEN, 10);
        assert!(blob_fits(PARAM_OFF));
        assert!(!blob_fits(PARAM_OFF + 1));
        assert!(blob_fits(0));
        assert!(blob_fits(0xD0));
    }

    #[test]
    fn idtr_pack_limit_then_base() {
        let b = pack_idtr(0x0FFF, 0xFFFF_8000_1234_5678);
        assert_eq!(u16::from_le_bytes([b[0], b[1]]), 0x0FFF);
        let mut base = [0u8; 8];
        base.copy_from_slice(&b[2..10]);
        assert_eq!(u64::from_le_bytes(base), 0xFFFF_8000_1234_5678);
    }

    #[test]
    fn bringup_delays() {
        assert_eq!(INIT_WAIT_MS, 10);
        assert_eq!(SIPI_WAIT_MS, 1);
        assert_eq!(READY_TIMEOUT_MS, 3000);
    }
}
