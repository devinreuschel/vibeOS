//! GICv3 ITS command encodings from ARM IHI 0069G.
//!
//! Builders only. No queue, no redistributor, no runtime. Each command is
//! 32 bytes, four little-endian doublewords, as the architecture
//! specification defines them.

/// One 32-byte ITS command (IHI 0069G).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ItsCommand {
    pub dw: [u64; 4],
}

/// MAPD / MAPC / MAPTI / MOVI / SYNC / DISCARD encodings (IHI 0069G).
pub const ITS_CMD_MOVI: u8 = 0x01;
pub const ITS_CMD_SYNC: u8 = 0x05;
pub const ITS_CMD_MAPD: u8 = 0x08;
pub const ITS_CMD_MAPC: u8 = 0x09;
pub const ITS_CMD_MAPTI: u8 = 0x0A;
pub const ITS_CMD_DISCARD: u8 = 0x0F;

/// MAPD Size the kernel programs: EventID bits minus one, so 16 events.
pub const ITS_MAPD_SIZE: u8 = 3;

/// ITT is 256-byte aligned (IHI 0069G MAPD).
pub const ITS_ITT_ALIGN: u64 = 256;

/// Size is EventID bits minus one: five bits.
pub const ITS_SIZE_MAX: u8 = 31;

/// RDbase occupies DW2[51:16] (36 bits): PA[51:16] when PTA=1, or the
/// processor number in its low 16 bits when PTA=0 (IHI 0069G).
const RDBASE_BITS: u32 = 36;
const RDBASE_SHIFT: u32 = 16;

/// MAPC/SYNC RDbase field for one redistributor. With GITS_TYPER.PTA clear
/// the field is the processor number, GICR_TYPER[23:8]. With PTA set it is
/// the redistributor's physical address, PA[51:16], given as `redist_pa`.
pub const fn rdbase_field(pta: bool, gicr_typer: u64, redist_pa: u64) -> u64 {
    if pta {
        (redist_pa >> RDBASE_SHIFT) & ((1u64 << RDBASE_BITS) - 1)
    } else {
        (gicr_typer >> 8) & 0xFFFF
    }
}

/// Entry size in bytes of a GITS_BASER<n> table: Entry_Size (bits [52:48])
/// is the size minus one (IHI 0069G).
pub const fn baser_entry_bytes(baser: u64) -> u64 {
    ((baser >> 48) & 0x1F) + 1
}

/// GITS_BASER.Indirect (bit 62) as read back: the ITS keeps a two-level table,
/// whose first level holds pointers, where the kernel wrote one level of
/// entries (IHI 0069G). A set bit means the table written is not the one used.
pub const fn baser_indirect(baser: u64) -> bool {
    baser & (1u64 << 62) != 0
}

/// GITS_BASER.Page_Size (bits [9:8]) reads back as 0b00 when the ITS kept the
/// 4 KiB pages the kernel wrote. Any other value means the ITS uses a larger
/// page than the one allocated (IHI 0069G).
pub const fn baser_page_is_4k(baser: u64) -> bool {
    (baser >> 8) & 0x3 == 0
}

/// DeviceID bits the ITS implements: GITS_TYPER.Devbits (bits [17:13]) plus
/// one (IHI 0069G).
pub const fn gits_devbits(typer: u64) -> u32 {
    (((typer >> 13) & 0x1F) + 1) as u32
}

/// `true` when `device_id` indexes inside a one-level device table of
/// `table_bytes` with `entry_bytes` per entry, and fits in the `devbits`
/// DeviceID bits the ITS implements. A zero size or width refuses every ID.
pub const fn its_device_fits(
    device_id: u32,
    table_bytes: u64,
    entry_bytes: u64,
    devbits: u32,
) -> bool {
    if entry_bytes == 0 || devbits == 0 {
        return false;
    }
    let entries = table_bytes / entry_bytes;
    let id_limit = match 1u64.checked_shl(devbits) {
        Some(v) => v,
        None => u64::MAX,
    };
    let id = device_id as u64;
    id < entries && id < id_limit
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItsError {
    BadItt,
    BadSize,
}

impl From<ItsError> for crate::kerror::KError {
    fn from(e: ItsError) -> Self {
        match e {
            ItsError::BadItt | ItsError::BadSize => Self::Inval,
        }
    }
}

impl ItsCommand {
    const fn empty(op: u8) -> Self {
        Self {
            dw: [op as u64, 0, 0, 0],
        }
    }

    fn with_device(mut self, device_id: u32) -> Self {
        if let Some(dw0) = self.dw.get_mut(0) {
            *dw0 |= u64::from(device_id) << 32;
        }
        self
    }

    fn with_rdbase(mut self, rdbase: u64) -> Self {
        let mask = (1u64 << RDBASE_BITS) - 1;
        if let Some(dw2) = self.dw.get_mut(2) {
            *dw2 |= (rdbase & mask) << RDBASE_SHIFT;
        }
        self
    }

    /// MAPD: bind DeviceID to an ITT. `size` is EventID bits minus one.
    /// `itt_addr` is the ITT physical address (256-byte aligned).
    pub fn mapd(device_id: u32, itt_addr: u64, size: u8, valid: bool) -> Result<Self, ItsError> {
        if size > ITS_SIZE_MAX {
            return Err(ItsError::BadSize);
        }
        if itt_addr.trailing_zeros() < 8 || itt_addr >= (1u64 << 52) {
            return Err(ItsError::BadItt);
        }
        let mut cmd = Self::empty(ITS_CMD_MAPD).with_device(device_id);
        // Size in DW1[4:0]. ITT_addr[51:8] and V in DW2: QEMU's ITS and
        // KVM vgic-its read them from that doubleword.
        if let Some(dw1) = cmd.dw.get_mut(1) {
            *dw1 = u64::from(size);
        }
        if let Some(dw2) = cmd.dw.get_mut(2) {
            *dw2 = itt_addr & (((1u64 << 52) - 1) & !0xFF);
            if valid {
                *dw2 |= 1u64 << 63;
            }
        }
        Ok(cmd)
    }

    /// MAPC: bind ICID to a redistributor. `rdbase` is the spec's RDbase
    /// field (processor number, or PA[51:16] when PTA=1).
    pub fn mapc(icid: u16, rdbase: u64, valid: bool) -> Self {
        let mut cmd = Self::empty(ITS_CMD_MAPC).with_rdbase(rdbase);
        if let Some(dw2) = cmd.dw.get_mut(2) {
            *dw2 |= u64::from(icid);
            if valid {
                *dw2 |= 1u64 << 63;
            }
        }
        cmd
    }

    /// MAPTI: EventID on DeviceID → pINTID in collection ICID.
    pub fn mapti(device_id: u32, event_id: u32, pintid: u32, icid: u16) -> Self {
        let mut cmd = Self::empty(ITS_CMD_MAPTI).with_device(device_id);
        if let Some(dw1) = cmd.dw.get_mut(1) {
            *dw1 = (u64::from(pintid) << 32) | u64::from(event_id);
        }
        if let Some(dw2) = cmd.dw.get_mut(2) {
            *dw2 = u64::from(icid);
        }
        cmd
    }

    /// MOVI: move EventID on DeviceID to collection ICID.
    pub fn movi(device_id: u32, event_id: u32, icid: u16) -> Self {
        let mut cmd = Self::empty(ITS_CMD_MOVI).with_device(device_id);
        if let Some(dw1) = cmd.dw.get_mut(1) {
            *dw1 = u64::from(event_id);
        }
        if let Some(dw2) = cmd.dw.get_mut(2) {
            *dw2 = u64::from(icid);
        }
        cmd
    }

    /// SYNC: wait until commands for `rdbase` have completed.
    pub fn sync(rdbase: u64) -> Self {
        Self::empty(ITS_CMD_SYNC).with_rdbase(rdbase)
    }

    /// DISCARD: drop EventID on DeviceID (IHI 0069G).
    pub fn discard(device_id: u32, event_id: u32) -> Self {
        let mut cmd = Self::empty(ITS_CMD_DISCARD).with_device(device_id);
        if let Some(dw1) = cmd.dw.get_mut(1) {
            *dw1 = u64::from(event_id);
        }
        cmd
    }
}

/// EventIDs a MAPD `size` field admits. `size` is EventID bits minus one
/// (IHI 0069G). `None` when `size` is out of range or the count does not
/// fit in a `u32`.
pub fn its_event_limit(size: u8) -> Option<u32> {
    if size > ITS_SIZE_MAX {
        return None;
    }
    let bits = u32::from(size).checked_add(1)?;
    1u32.checked_shl(bits)
}

/// DISCARD one (DeviceID, EventID), then SYNC. No MAPD: the device stays
/// mapped (DESIGN §5.4).
pub fn encode_lpi_free(device_id: u32, event_id: u32, rdbase: u64) -> [ItsCommand; 2] {
    [
        ItsCommand::discard(device_id, event_id),
        ItsCommand::sync(rdbase),
    ]
}

/// MAPD with V=0, then SYNC. Only after that device's events were discarded.
pub fn encode_device_unmap(device_id: u32, rdbase: u64) -> Result<[ItsCommand; 2], ItsError> {
    Ok([
        ItsCommand::mapd(device_id, 0, 0, false)?,
        ItsCommand::sync(rdbase),
    ])
}

/// Drop every listed event and the device: DISCARD each, MAPD V=0, SYNC.
/// Per-LPI free uses [`encode_lpi_free`]; removal after those discards
/// uses [`encode_device_unmap`] (DESIGN §5.4).
pub fn encode_free_sequence(
    device_id: u32,
    event_ids: &[u32],
    rdbase: u64,
    out: &mut [ItsCommand],
) -> Result<usize, ItsError> {
    let n = event_ids.len().checked_add(2).ok_or(ItsError::BadSize)?;
    if out.len() < n {
        return Err(ItsError::BadSize);
    }
    let mut i = 0usize;
    while i < event_ids.len() {
        let Some(ev) = event_ids.get(i) else {
            return Err(ItsError::BadSize);
        };
        if let Some(slot) = out.get_mut(i) {
            *slot = ItsCommand::discard(device_id, *ev);
        }
        i = i.checked_add(1).ok_or(ItsError::BadSize)?;
    }
    if let Some(slot) = out.get_mut(i) {
        *slot = ItsCommand::mapd(device_id, 0, 0, false)?;
    }
    i = i.checked_add(1).ok_or(ItsError::BadSize)?;
    if let Some(slot) = out.get_mut(i) {
        *slot = ItsCommand::sync(rdbase);
    }
    i.checked_add(1).ok_or(ItsError::BadSize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn its_command_encodings() {
        // IHI 0069G field layout, checked as literals.
        let mapd = ItsCommand::mapd(1, 0x1000, 7, true).unwrap();
        assert_eq!(mapd.dw[0], (1u64 << 32) | 0x08);
        assert_eq!(mapd.dw[1], 7);
        assert_eq!(mapd.dw[2], 0x1000 | (1u64 << 63));
        assert_eq!(mapd.dw[3], 0);

        let mapd_inv = ItsCommand::mapd(0xA, 0x20000, 0, false).unwrap();
        assert_eq!(mapd_inv.dw[0], (0xAu64 << 32) | 0x08);
        assert_eq!(mapd_inv.dw[1], 0);
        assert_eq!(mapd_inv.dw[2], 0x20000);

        assert_eq!(ItsCommand::mapd(0, 0x80, 0, true), Err(ItsError::BadItt));
        assert_eq!(
            ItsCommand::mapd(0, 0x1000, 32, true),
            Err(ItsError::BadSize)
        );

        let mapc = ItsCommand::mapc(2, 3, true);
        assert_eq!(mapc.dw[0], 0x09);
        assert_eq!(mapc.dw[1], 0);
        assert_eq!(mapc.dw[2], (1u64 << 63) | (3u64 << 16) | 2);
        assert_eq!(mapc.dw[3], 0);

        let mapti = ItsCommand::mapti(1, 5, 8192, 2);
        assert_eq!(mapti.dw[0], (1u64 << 32) | 0x0A);
        assert_eq!(mapti.dw[1], (8192u64 << 32) | 5);
        assert_eq!(mapti.dw[2], 2);
        assert_eq!(mapti.dw[3], 0);

        let movi = ItsCommand::movi(1, 5, 3);
        assert_eq!(movi.dw[0], (1u64 << 32) | 0x01);
        assert_eq!(movi.dw[1], 5);
        assert_eq!(movi.dw[2], 3);
        assert_eq!(movi.dw[3], 0);

        let sync = ItsCommand::sync(3);
        assert_eq!(sync.dw[0], 0x05);
        assert_eq!(sync.dw[1], 0);
        assert_eq!(sync.dw[2], 3u64 << 16);
        assert_eq!(sync.dw[3], 0);

        let discard = ItsCommand::discard(1, 5);
        assert_eq!(discard.dw[0], (1u64 << 32) | 0x0F);
        assert_eq!(discard.dw[1], 5);
        assert_eq!(discard.dw[2], 0);
        assert_eq!(discard.dw[3], 0);
    }

    #[test]
    fn its_free_sequence() {
        let mut out = [ItsCommand { dw: [0; 4] }; 8];
        let n = encode_free_sequence(1, &[5, 6], 3, &mut out).unwrap();
        assert_eq!(n, 4);
        assert_eq!(out[0], ItsCommand::discard(1, 5));
        assert_eq!(out[1], ItsCommand::discard(1, 6));
        assert_eq!(out[2], ItsCommand::mapd(1, 0, 0, false).unwrap());
        assert_eq!(out[3], ItsCommand::sync(3));
        assert_eq!(
            encode_free_sequence(1, &[1], 0, &mut out[..2]),
            Err(ItsError::BadSize)
        );
    }

    #[test]
    fn its_lpi_free_is_discard_sync() {
        let cmds = encode_lpi_free(1, 5, 3);
        assert_eq!(cmds[0], ItsCommand::discard(1, 5));
        assert_eq!(cmds[1], ItsCommand::sync(3));
        assert_ne!(cmds[0].dw[0] & 0xFF, u64::from(ITS_CMD_MAPD));
        assert_ne!(cmds[1].dw[0] & 0xFF, u64::from(ITS_CMD_MAPD));
    }

    #[test]
    fn its_device_unmap_is_mapd_sync() {
        let cmds = encode_device_unmap(1, 3).unwrap();
        assert_eq!(cmds[0], ItsCommand::mapd(1, 0, 0, false).unwrap());
        assert_eq!(cmds[1], ItsCommand::sync(3));
    }

    #[test]
    fn its_event_limit_matches_mapd_size() {
        assert_eq!(its_event_limit(ITS_MAPD_SIZE), Some(16));
        assert_eq!(its_event_limit(32), None);
        assert_eq!(its_event_limit(31), None);
    }

    #[test]
    fn rdbase_is_processor_number_or_pa() {
        // PTA=0: GICR_TYPER[23:8] is the processor number; the rest of
        // GICR_TYPER does not leak in.
        let typer = (0x7u64 << 8) | (0x1u64 << 4) | (0xABu64 << 32);
        assert_eq!(rdbase_field(false, typer, 0x080A_0000), 7);
        // PTA=1: PA[51:16] of the redistributor frame.
        assert_eq!(
            rdbase_field(true, typer, 0x0801_2340_0000),
            0x0801_2340_0000 >> 16
        );
        assert_eq!(rdbase_field(true, typer, 0x0801_0000), 0x0801);
        // PA bit 51 is the top bit of the 36-bit field.
        assert_eq!(rdbase_field(true, 0, 1u64 << 51), 1u64 << 35);
        // A MAPC built from the helper puts the field at DW2[51:16].
        let mapc = ItsCommand::mapc(3, rdbase_field(true, 0, 1u64 << 51), true);
        assert_eq!(mapc.dw[2], (1u64 << 63) | (1u64 << 51) | 3);
        let mapc = ItsCommand::mapc(3, rdbase_field(false, 0x7 << 8, 0), true);
        assert_eq!(mapc.dw[2], (1u64 << 63) | (7u64 << 16) | 3);
    }

    #[test]
    fn baser_and_typer_fields() {
        // Entry_Size (bits [52:48]) is minus one: 0 is 1 byte, 7 is 8 bytes.
        assert_eq!(baser_entry_bytes(7u64 << 48), 8);
        assert_eq!(baser_entry_bytes(0), 1);
        // Devbits (bits [17:13]) is minus one: 0x0F is 16 bits.
        assert_eq!(gits_devbits(0x0F << 13), 16);
        assert_eq!(gits_devbits(0), 1);
    }

    #[test]
    fn baser_indirect_is_bit_62() {
        // Indirect is bit 62; Valid (63) and the type field (58:56) do not set it.
        assert!(baser_indirect(1u64 << 62));
        assert!(baser_indirect((1u64 << 62) | (1u64 << 63) | (1u64 << 56)));
        assert!(!baser_indirect(0));
        assert!(!baser_indirect(1u64 << 63));
        assert!(!baser_indirect((0x7u64 << 56) | (0x7u64 << 48)));
        assert!(!baser_indirect((1u64 << 61) - 1));
    }

    #[test]
    fn baser_page_size_must_read_back_4k() {
        // Page_Size is bits [9:8]: 0b00 is 4 KiB, 0b01 16 KiB, 0b10 64 KiB.
        assert!(baser_page_is_4k(0));
        assert!(baser_page_is_4k(
            (1u64 << 63) | (1u64 << 62) | (0x7u64 << 56)
        ));
        assert!(!baser_page_is_4k(1u64 << 8));
        assert!(!baser_page_is_4k(2u64 << 8));
        assert!(!baser_page_is_4k(3u64 << 8));
    }

    #[test]
    fn device_table_refuses_ids_it_cannot_index() {
        // One 4 KiB page of 8-byte entries holds 512 DeviceIDs, and 16 bits
        // are implemented.
        assert!(its_device_fits(0, 4096, 8, 16));
        assert!(its_device_fits(511, 4096, 8, 16));
        assert!(!its_device_fits(512, 4096, 8, 16));
        assert!(!its_device_fits(0x1_0000, 65536, 8, 16));
        // The implemented DeviceID width caps a larger table.
        assert!(!its_device_fits(16, 4096, 1, 4));
        assert!(its_device_fits(15, 4096, 1, 4));
        // No device table, zero entry size, or zero width refuses every ID.
        assert!(!its_device_fits(0, 0, 8, 16));
        assert!(!its_device_fits(0, 4096, 0, 16));
        assert!(!its_device_fits(0, 4096, 8, 0));
        // A width of 32 or more does not overflow the shift.
        assert!(its_device_fits(7, 4096, 8, 32));
    }
}
