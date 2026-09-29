//! Boot inputs, portable half: the kernel command line ([`cmdline`]) and
//! QEMU's fw_cfg encodings, which the kernel half (`boot::fw_cfg_init`)
//! drives through ports. Constants and layouts are those of QEMU's
//! `docs/specs/fw_cfg.rst` (DESIGN §1.5: cited, no text copied).

pub mod cmdline;

/// Selector key of the `QEMU` signature.
pub const FW_CFG_SIGNATURE: u16 = 0x0000;
/// Selector key of the feature word (little-endian `u32`).
pub const FW_CFG_ID: u16 = 0x0001;
/// Selector key of the file directory.
pub const FW_CFG_FILE_DIR: u16 = 0x0019;
/// Feature bit: the DMA interface exists.
pub const FW_CFG_ID_DMA: u32 = 1 << 1;
/// The signature read from [`FW_CFG_SIGNATURE`].
pub const FW_CFG_QEMU: [u8; 4] = *b"QEMU";

/// DMA control bits (`FWCfgDmaAccess.control`).
pub const FW_CFG_DMA_ERROR: u32 = 0x01;
pub const FW_CFG_DMA_READ: u32 = 0x02;
pub const FW_CFG_DMA_SELECT: u32 = 0x08;
pub const FW_CFG_DMA_WRITE: u32 = 0x10;

/// Size of one directory entry (`FWCfgFile`).
pub const FW_CFG_DIR_ENTRY: usize = 64;
/// Longest file name, NUL included.
pub const FW_CFG_NAME_MAX: usize = 56;

/// One fw_cfg file: its selector key and size in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FwCfgFile {
    pub select: u16,
    pub size: u32,
}

/// The directory's entry count, the big-endian word that heads it.
#[deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
pub fn parse_fw_cfg_dir_count(b: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*b)
}

/// One directory entry: big-endian size, big-endian select, two reserved
/// bytes, then a NUL-terminated name of at most [`FW_CFG_NAME_MAX`] bytes.
/// A name with no NUL is taken whole.
#[deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
pub fn parse_fw_cfg_dir_entry(e: &[u8; FW_CFG_DIR_ENTRY]) -> (FwCfgFile, &[u8]) {
    let (size, rest) = e.split_first_chunk::<4>().unwrap_or((&[0; 4], &[]));
    let (select, rest) = rest.split_first_chunk::<2>().unwrap_or((&[0; 2], &[]));
    let name = rest.get(2..).unwrap_or(&[]);
    let name = name.split(|&b| b == 0).next().unwrap_or(&[]);
    (
        FwCfgFile {
            select: u16::from_be_bytes(*select),
            size: u32::from_be_bytes(*size),
        },
        name,
    )
}

/// An `FWCfgDmaAccess` descriptor: big-endian control, length, address.
/// A select goes in `control`'s bits 31:16 with [`FW_CFG_DMA_SELECT`].
#[deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
pub fn fw_cfg_dma_access(control: u32, len: u32, addr: u64) -> [u8; 16] {
    let mut d = [0u8; 16];
    let (c, rest) = d.split_at_mut(4);
    c.copy_from_slice(&control.to_be_bytes());
    let (l, a) = rest.split_at_mut(4);
    l.copy_from_slice(&len.to_be_bytes());
    a.copy_from_slice(&addr.to_be_bytes());
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fw_cfg_dir_count_be() {
        assert_eq!(parse_fw_cfg_dir_count(&[0, 0, 0x01, 0x02]), 0x102);
        assert_eq!(parse_fw_cfg_dir_count(&[0xff; 4]), u32::MAX);
    }

    #[test]
    fn fw_cfg_dir_entry_parse() {
        let mut e = [0u8; FW_CFG_DIR_ENTRY];
        e[..4].copy_from_slice(&0x1234u32.to_be_bytes());
        e[4..6].copy_from_slice(&0x0025u16.to_be_bytes());
        e[6..8].copy_from_slice(&[0xaa, 0xbb]);
        e[8..8 + 18].copy_from_slice(b"opt/vibeos/cmdline");
        let (f, name) = parse_fw_cfg_dir_entry(&e);
        assert_eq!(
            f,
            FwCfgFile {
                select: 0x25,
                size: 0x1234
            }
        );
        assert_eq!(name, b"opt/vibeos/cmdline");

        // No NUL: all 56 bytes.
        e[8..].fill(b'x');
        assert_eq!(parse_fw_cfg_dir_entry(&e).1.len(), FW_CFG_NAME_MAX);
        e[8..].fill(0);
        assert_eq!(parse_fw_cfg_dir_entry(&e).1, b"");
    }

    #[test]
    fn fw_cfg_dma_access_be() {
        let d = fw_cfg_dma_access(
            (0x19 << 16) | FW_CFG_DMA_SELECT | FW_CFG_DMA_READ,
            4,
            0x1122_3344_5566_7788,
        );
        assert_eq!(
            d,
            [
                0x00, 0x19, 0x00, 0x0a, 0, 0, 0, 4, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88
            ]
        );
    }
}
