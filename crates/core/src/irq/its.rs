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

/// After DISCARD / MAPD V=0 / SYNC, wait this long before freeing the ITT
/// (DESIGN §5.4).
pub const ITS_FREE_WAIT_NS: u64 = 1_000_000_000;

/// ITT is 256-byte aligned (IHI 0069G MAPD).
pub const ITS_ITT_ALIGN: u64 = 256;

/// Size is EventID bits minus one: five bits.
pub const ITS_SIZE_MAX: u8 = 31;

/// RDbase occupies DW2[50:16] (35 bits).
const RDBASE_BITS: u32 = 35;
const RDBASE_SHIFT: u32 = 16;

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
        // DW1[51:8] = ITT_addr[51:8]; DW1[4:0] = Size.
        if let Some(dw1) = cmd.dw.get_mut(1) {
            *dw1 = (itt_addr & (((1u64 << 52) - 1) & !0xFF)) | u64::from(size);
        }
        if valid && let Some(dw2) = cmd.dw.get_mut(2) {
            *dw2 |= 1u64 << 63;
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

/// Commands that free a device's LPIs: DISCARD each event, MAPD V=0,
/// SYNC. The caller then waits [`ITS_FREE_WAIT_NS`] (DESIGN §5.4).
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
        assert_eq!(mapd.dw[1], 0x1000 | 7);
        assert_eq!(mapd.dw[2], 1u64 << 63);
        assert_eq!(mapd.dw[3], 0);

        let mapd_inv = ItsCommand::mapd(0xA, 0x20000, 0, false).unwrap();
        assert_eq!(mapd_inv.dw[0], (0xAu64 << 32) | 0x08);
        assert_eq!(mapd_inv.dw[1], 0x20000);
        assert_eq!(mapd_inv.dw[2], 0);

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
        assert_eq!(ITS_FREE_WAIT_NS, 1_000_000_000);
        assert_eq!(
            encode_free_sequence(1, &[1], 0, &mut out[..2]),
            Err(ItsError::BadSize)
        );
    }
}
