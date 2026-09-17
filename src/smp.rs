//! AP trampoline layout. DESIGN §7.3 / ROADMAP §4.4.
//!
//! Portable: offsets, SIPI vector, timeouts. The nasm blob and INIT/SIPI
//! live in the binary crate. One AP at a time: they share this page.

/// SIPI vector `0x08` → physical `0x8000`. 4 KiB aligned, below 1 MiB.
pub const TRAMPOLINE_PHYS: u64 = 0x8000;
pub const SIPI_VECTOR: u8 = 0x08;
pub const TRAMPOLINE_PAGES: u64 = 1;

/// Param block. Blob must fit strictly below this.
pub const PARAM_OFF: usize = 0xD0;
pub const PARAM_CR3: usize = 0xD0;
pub const PARAM_STACK: usize = 0xD8;
pub const PARAM_ENTRY: usize = 0xE0;
pub const PARAM_IDT: usize = 0xE8;
pub const PARAM_IDT_LEN: usize = 10;

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
    fn trampoline_page_and_sipi_vector() {
        assert_eq!(TRAMPOLINE_PHYS, 0x8000);
        assert_eq!(SIPI_VECTOR, 0x08);
        assert_eq!((SIPI_VECTOR as u64) << 12, TRAMPOLINE_PHYS);
        assert_eq!(TRAMPOLINE_PHYS % 4096, 0);
        assert!(TRAMPOLINE_PHYS < 0x100000);
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
