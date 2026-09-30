//! `vmcoreinfo_parse`: the VMCOREINFO and build-id note readers, which
//! read a core or an ELF.

use vibeos::log::vmcoreinfo::{self, FwCfgVmcoreinfo};

/// `parse_note` and `get` for every key `render` emits, `gnu_build_id`,
/// then the fw_cfg payload's decode and encode on the first 16 bytes.
pub fn parse(data: &[u8]) {
    if let Ok(note) = vmcoreinfo::parse_note(data) {
        for key in vmcoreinfo::KEYS {
            let _ = vmcoreinfo::get(note.desc, key);
        }
    }
    let _ = vmcoreinfo::gnu_build_id(data);
    if let Some(b) = data.first_chunk::<{ FwCfgVmcoreinfo::LEN }>() {
        let f = FwCfgVmcoreinfo::from_le_bytes(b);
        let _ = f.host_takes_elf();
        assert_eq!(&f.to_le_bytes(), b, "FwCfgVmcoreinfo does not round-trip");
    }
}
