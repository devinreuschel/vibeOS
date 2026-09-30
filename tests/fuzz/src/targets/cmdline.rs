//! `cmdline_parse`: the kernel command line and the fw_cfg encodings it
//! arrives through.

use vibeos::boot::{self, cmdline};

/// `cmdline::parse`, then `get` and `flag` for every option BOOT.md §3.2
/// lists, and `init_args`, `init_env`, `sysctls` and `dropped` iterated to
/// their end. Then the fw_cfg directory parsers on the same bytes.
pub fn parse(data: &[u8]) {
    let c = cmdline::parse(data);
    for opt in cmdline::OPTIONS {
        let _ = c.get(opt.name);
        let _ = c.flag(opt.name);
    }
    let _ = c.init_args().map(|w| w.len()).sum::<usize>();
    let _ = c.init_env().map(|w| w.len()).sum::<usize>();
    let _ = c.sysctls().filter(|s| s.known_in(cmdline::SYSCTLS)).count();
    let _ = c.dropped().count();
    let mut buf = [0u8; 512];
    let v = c.init_vectors(b"/sbin/init", &mut buf);
    let _ = (v.argv().len(), v.envp().len());

    if let Some(n) = data.first_chunk::<4>() {
        let _ = boot::parse_fw_cfg_dir_count(n);
    }
    for e in data.as_chunks::<{ boot::FW_CFG_DIR_ENTRY }>().0 {
        let _ = boot::parse_fw_cfg_dir_entry(e);
    }
    if let Some(d) = data.first_chunk::<16>() {
        let (c, rest) = d.split_at(4);
        let (l, a) = rest.split_at(4);
        let _ = boot::fw_cfg_dma_access(
            u32::from_le_bytes(c.try_into().unwrap_or_default()),
            u32::from_le_bytes(l.try_into().unwrap_or_default()),
            u64::from_le_bytes(a.try_into().unwrap_or_default()),
        );
    }
}
