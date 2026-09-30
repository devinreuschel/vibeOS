//! `elf_parse`: the ELF loader's header and program-header parser.

use vibeos::elf;

/// `elf::parse` on the input as a whole file, then each `PT_LOAD`'s and
/// the `PT_TLS` init image's file bytes, which `parse` promises lie inside
/// the file.
pub fn parse(data: &[u8]) {
    let Ok(img) = elf::parse(data) else {
        return;
    };
    for s in img.loads() {
        assert!(
            file_bytes(data, s.offset, s.filesz).is_some(),
            "PT_LOAD file bytes {:#x}+{:#x} past a {}-byte file",
            s.offset,
            s.filesz,
            data.len()
        );
    }
    if let Some(t) = img.tls {
        assert!(
            file_bytes(data, t.offset, t.filesz).is_some(),
            "PT_TLS file bytes {:#x}+{:#x} past a {}-byte file",
            t.offset,
            t.filesz,
            data.len()
        );
        let _ = t.map_len();
    }
}

/// `filesz` bytes of `data` at `offset`, or `None` past its end.
fn file_bytes(data: &[u8], offset: u64, filesz: u64) -> Option<&[u8]> {
    let lo = usize::try_from(offset).ok()?;
    let hi = lo.checked_add(usize::try_from(filesz).ok()?)?;
    data.get(lo..hi)
}
