//! `pci_enumerate` and `virtio_caps`: config-space parsers over [`FakeCfg`].

use vibeos::pci::{self, Bdf, FuncInfo};
use vibeos::virtio;

use crate::cfgspace::FakeCfg;

/// `pci::enumerate` from bus 0 into `MAX_SCAN` slots, then the MSI and
/// MSI-X capabilities of each function found, then `decode_bar` on the
/// input's first four `u32`s.
pub fn enumerate(data: &[u8]) {
    let mut cfg = FakeCfg::parse(data);
    let mut out = vec![FuncInfo::empty(); pci::MAX_SCAN];
    let n = pci::enumerate(&mut cfg, 0, &mut out);
    for f in out.iter().take(n) {
        if let Some(cap) = f.caps.msi {
            let _ = pci::read_msi_cap(&mut cfg, f.bdf, cap);
        }
        if let Some(cap) = f.caps.msix {
            let _ = pci::read_msix_cap(&mut cfg, f.bdf, cap);
        }
    }
    let mut w = [0u32; 4];
    for (slot, c) in w.iter_mut().zip(data.chunks(4)) {
        let mut b = [0u8; 4];
        b[..c.len()].copy_from_slice(c);
        *slot = u32::from_le_bytes(b);
    }
    let _ = pci::decode_bar(w[0], w[1], w[2], w[3]);
}

/// `virtio::read_modern_caps` for 00:00.0, then `is_complete`.
pub fn virtio_caps(data: &[u8]) {
    let mut cfg = FakeCfg::parse(data);
    let _ = virtio::read_modern_caps(&mut cfg, Bdf::new(0, 0, 0)).is_complete();
}
