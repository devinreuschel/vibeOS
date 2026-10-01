//! The VMCOREINFO note (ROADMAP §10.7, C-VMCOREINFO): one ELF note named
//! `VMCOREINFO` whose descriptor is `\n`-terminated ASCII `KEY=value`
//! lines, the format every reader of a dump uses. The kernel half
//! (`log::vmcoreinfo_init`) renders it at boot and hands its physical
//! address to QEMU's `vmcoreinfo` device; the core tool reads it back.
//! `docs/VMCOREINFO.md` documents the keys.
//!
//! Sources, all documentation (DESIGN §1.5: cited, no text copied): the
//! note layout (`Elf64_Nhdr`, then the name and the descriptor, each padded
//! to 4 bytes) is the System V gABI's "Note Section"; the device payload is
//! `FWCfgVMCoreInfo` from QEMU's `docs/specs/vmcoreinfo.rst`, written through
//! the DMA interface of `docs/specs/fw_cfg.rst`; the `SYMBOL()`, `NUMBER()`
//! and `LENGTH()` key shapes and the `OSRELEASE`, `BUILD-ID` and `PAGESIZE`
//! keys are Linux's `Documentation/admin-guide/kdump/vmcoreinfo.rst`.
//!
//! No panic and no allocation: [`parse_note`], [`get`] and [`gnu_build_id`]
//! read bytes from a core or an ELF, which are untrusted (AGENTS.md rule 4).

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use core::fmt::{self, Write};

use crate::fmt_util::StackBuf;

/// The note's name, without its NUL.
pub const NOTE_NAME: &[u8] = b"VMCOREINFO";
/// The note's type: Linux's VMCOREINFO note uses 0.
pub const NOTE_TYPE: u32 = 0;
/// Largest note the kernel renders: one page, so it is physically
/// contiguous.
pub const NOTE_MAX: usize = 4096;
/// `FWCfgVMCoreInfo`'s ELF-note format (`FW_CFG_VMCOREINFO_FORMAT_ELF`).
pub const FORMAT_ELF: u16 = 1;
/// The fw_cfg file QEMU's `vmcoreinfo` device adds.
pub const FW_CFG_FILE: &str = "etc/vmcoreinfo";
/// `NT_GNU_BUILD_ID`, the type of the linker's build-id note.
pub const NT_GNU_BUILD_ID: u32 = 3;
/// The build-id note's name, without its NUL.
pub const GNU_NAME: &[u8] = b"GNU";

/// `Elf64_Nhdr`: `namesz`, `descsz`, `type`, each a 4-byte word.
const NHDR: usize = 12;
/// `namesz` of [`NOTE_NAME`], its NUL included.
const NAME_SZ: u32 = 11;
/// [`NOTE_NAME`] with its NUL, padded to 4.
const NAME_PADDED: [u8; 12] = *b"VMCOREINFO\0\0";
/// Offset of the descriptor in a rendered note.
const DESC_OFF: usize = NHDR + NAME_PADDED.len();

/// The keys [`render`] emits, in the order it emits them.
pub const KEYS: &[&str] = &[
    "OSRELEASE",
    "BUILD-ID",
    "PAGESIZE",
    "NUMBER(vibeos_pgt_root)",
    "NUMBER(vibeos_pgt_levels)",
    "SYMBOL(vibeos_log)",
    "SYMBOL(vibeos_tcbs)",
    "LENGTH(vibeos_tcbs)",
    "SYMBOL(vibeos_cpus)",
    "LENGTH(vibeos_cpus)",
];

/// What the note says. Addresses are virtual unless the key says physical.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info<'a> {
    /// `OSRELEASE`: the kernel crate's version.
    pub osrelease: &'a str,
    /// `BUILD-ID`: the descriptor of the ELF's `NT_GNU_BUILD_ID` note.
    pub build_id: &'a [u8],
    /// `PAGESIZE`.
    pub page_size: u64,
    /// `NUMBER(vibeos_pgt_root)`: the kernel page-table root's physical
    /// address.
    pub pgt_root: u64,
    /// `NUMBER(vibeos_pgt_levels)`.
    pub pgt_levels: u32,
    /// `SYMBOL(vibeos_log)`: the log ring's `KernelLog` static.
    pub log: u64,
    /// `SYMBOL(vibeos_tcbs)`, `LENGTH(vibeos_tcbs)`: the TCB slot array.
    pub tcbs: u64,
    pub tcbs_len: u64,
    /// `SYMBOL(vibeos_cpus)`, `LENGTH(vibeos_cpus)`: the `PerCpu` array.
    pub cpus: u64,
    pub cpus_len: u64,
}

/// One ELF note: its name without trailing NULs, its type, its descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Note<'a> {
    pub name: &'a [u8],
    pub kind: u32,
    pub desc: &'a [u8],
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteError {
    /// The output buffer cannot hold the note.
    TooSmall,
    /// Fewer bytes than the note header, or its name or descriptor runs
    /// past the end.
    Truncated,
    /// Not the note the caller asked for: another name.
    WrongName,
    /// Not the note the caller asked for: another type.
    WrongType,
}

/// A note that does not parse is a bad argument.
impl From<NoteError> for crate::kerror::KError {
    fn from(e: NoteError) -> Self {
        match e {
            NoteError::TooSmall
            | NoteError::Truncated
            | NoteError::WrongName
            | NoteError::WrongType => Self::Inval,
        }
    }
}

impl NoteError {
    pub fn as_str(self) -> &'static str {
        match self {
            NoteError::TooSmall => "buffer too small",
            NoteError::Truncated => "truncated note",
            NoteError::WrongName => "wrong note name",
            NoteError::WrongType => "wrong note type",
        }
    }
}

/// `n` rounded up to a multiple of 4, or `None` on overflow.
fn pad4(n: usize) -> Option<usize> {
    n.checked_add(3).map(|v| v & !3)
}

/// Lowercase hex of `bytes`, two digits per byte.
struct Hex<'a>(&'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// The descriptor text: one line per [`KEYS`] entry, in order. `SYMBOL`
/// values are lowercase hex without a prefix, `NUMBER` and `LENGTH`
/// values decimal, as Linux writes them.
fn write_desc(info: &Info<'_>, w: &mut impl Write) -> fmt::Result {
    let [
        osrelease,
        build_id,
        page_size,
        pgt_root,
        pgt_levels,
        log,
        tcbs,
        tcbs_len,
        cpus,
        cpus_len,
    ] = KEYS
    else {
        return Err(fmt::Error);
    };
    writeln!(w, "{osrelease}={}", info.osrelease)?;
    writeln!(w, "{build_id}={}", Hex(info.build_id))?;
    writeln!(w, "{page_size}={}", info.page_size)?;
    writeln!(w, "{pgt_root}={}", info.pgt_root)?;
    writeln!(w, "{pgt_levels}={}", info.pgt_levels)?;
    writeln!(w, "{log}={:x}", info.log)?;
    writeln!(w, "{tcbs}={:x}", info.tcbs)?;
    writeln!(w, "{tcbs_len}={}", info.tcbs_len)?;
    writeln!(w, "{cpus}={:x}", info.cpus)?;
    writeln!(w, "{cpus_len}={}", info.cpus_len)
}

/// Write the whole note into `out`: the little-endian `Elf64_Nhdr`
/// (`namesz` 11, `descsz`, type 0), `VMCOREINFO\0` padded to 4, then the
/// text padded to 4. Returns the note's length, a multiple of 4. The
/// padding bytes are zero.
pub fn render(info: &Info<'_>, out: &mut [u8]) -> Result<usize, NoteError> {
    let desc_len = {
        let body = out.get_mut(DESC_OFF..).ok_or(NoteError::TooSmall)?;
        let mut w = StackBuf::new(body);
        if write_desc(info, &mut w).is_err() || w.is_cut() {
            return Err(NoteError::TooSmall);
        }
        w.len()
    };
    let total = DESC_OFF
        .checked_add(pad4(desc_len).ok_or(NoteError::TooSmall)?)
        .ok_or(NoteError::TooSmall)?;
    let desc_end = DESC_OFF.checked_add(desc_len).ok_or(NoteError::TooSmall)?;
    out.get_mut(desc_end..total)
        .ok_or(NoteError::TooSmall)?
        .fill(0);
    let descsz = u32::try_from(desc_len).map_err(|_| NoteError::TooSmall)?;
    let head = NAME_SZ
        .to_le_bytes()
        .into_iter()
        .chain(descsz.to_le_bytes())
        .chain(NOTE_TYPE.to_le_bytes())
        .chain(NAME_PADDED);
    let dst = out.get_mut(..DESC_OFF).ok_or(NoteError::TooSmall)?;
    for (d, s) in dst.iter_mut().zip(head) {
        *d = s;
    }
    Ok(total)
}

/// A little-endian `u32` at `off`.
fn le32(bytes: &[u8], off: usize) -> Result<u32, NoteError> {
    let end = off.checked_add(4).ok_or(NoteError::Truncated)?;
    let w: [u8; 4] = bytes
        .get(off..end)
        .and_then(|s| s.try_into().ok())
        .ok_or(NoteError::Truncated)?;
    Ok(u32::from_le_bytes(w))
}

/// The first note in `bytes`: the header, the name padded to 4, then
/// `descsz` descriptor bytes. The descriptor's trailing padding may be
/// absent, as at the end of a section.
pub fn parse_note(bytes: &[u8]) -> Result<Note<'_>, NoteError> {
    let namesz = usize::try_from(le32(bytes, 0)?).map_err(|_| NoteError::Truncated)?;
    let descsz = usize::try_from(le32(bytes, 4)?).map_err(|_| NoteError::Truncated)?;
    let kind = le32(bytes, 8)?;
    let name_end = NHDR.checked_add(namesz).ok_or(NoteError::Truncated)?;
    let raw = bytes.get(NHDR..name_end).ok_or(NoteError::Truncated)?;
    let desc_off = NHDR
        .checked_add(pad4(namesz).ok_or(NoteError::Truncated)?)
        .ok_or(NoteError::Truncated)?;
    let desc_end = desc_off.checked_add(descsz).ok_or(NoteError::Truncated)?;
    let desc = bytes.get(desc_off..desc_end).ok_or(NoteError::Truncated)?;
    let keep = raw
        .iter()
        .rposition(|&b| b != 0)
        .map_or(0, |i| i.saturating_add(1));
    let name = raw.get(..keep).ok_or(NoteError::Truncated)?;
    Ok(Note { name, kind, desc })
}

/// The value of `key` in a VMCOREINFO descriptor: the rest of the first
/// line that starts with `key=`, without its `\n`.
pub fn get<'a>(desc: &'a [u8], key: &str) -> Option<&'a [u8]> {
    desc.split(|&b| b == b'\n')
        .find_map(|line| line.strip_prefix(key.as_bytes())?.strip_prefix(b"="))
}

/// The build id in `bytes`, which start with a GNU `NT_GNU_BUILD_ID` note
/// (the `.note.gnu.build-id` section): that note's descriptor.
pub fn gnu_build_id(bytes: &[u8]) -> Result<&[u8], NoteError> {
    let note = parse_note(bytes)?;
    if note.name != GNU_NAME {
        return Err(NoteError::WrongName);
    }
    if note.kind != NT_GNU_BUILD_ID {
        return Err(NoteError::WrongType);
    }
    Ok(note.desc)
}

/// QEMU's `FWCfgVMCoreInfo`, the 16 bytes of fw_cfg's `etc/vmcoreinfo`, all
/// little-endian. The guest sets `guest_format`, `size` and `paddr`;
/// `host_format` says which formats the host accepts.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FwCfgVmcoreinfo {
    pub host_format: u16,
    pub guest_format: u16,
    pub size: u32,
    pub paddr: u64,
}

impl FwCfgVmcoreinfo {
    /// The file's size.
    pub const LEN: usize = 16;

    pub fn to_le_bytes(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        let src = self
            .host_format
            .to_le_bytes()
            .into_iter()
            .chain(self.guest_format.to_le_bytes())
            .chain(self.size.to_le_bytes())
            .chain(self.paddr.to_le_bytes());
        for (d, s) in out.iter_mut().zip(src) {
            *d = s;
        }
        out
    }

    pub fn from_le_bytes(b: &[u8; 16]) -> Self {
        let [
            a0,
            a1,
            b0,
            b1,
            c0,
            c1,
            c2,
            c3,
            d0,
            d1,
            d2,
            d3,
            d4,
            d5,
            d6,
            d7,
        ] = *b;
        Self {
            host_format: u16::from_le_bytes([a0, a1]),
            guest_format: u16::from_le_bytes([b0, b1]),
            size: u32::from_le_bytes([c0, c1, c2, c3]),
            paddr: u64::from_le_bytes([d0, d1, d2, d3, d4, d5, d6, d7]),
        }
    }

    /// Whether the host accepts an ELF note (`host_format` has
    /// [`FORMAT_ELF`]).
    pub fn host_takes_elf(&self) -> bool {
        self.host_format & FORMAT_ELF != 0
    }
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "host tests index fixed test buffers"
)]
mod tests {
    use super::*;

    const ID: [u8; 20] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
        0x77, 0x88, 0x99, 0xaa, 0xbb,
    ];

    fn info() -> Info<'static> {
        Info {
            osrelease: "0.1.0",
            build_id: &ID,
            page_size: 4096,
            pgt_root: 0x1234_5000,
            pgt_levels: 4,
            log: 0xffff_ffff_8012_3440,
            tcbs: 0xffff_ffff_8020_0000,
            tcbs_len: 256,
            cpus: 0xffff_8000_0100_0000,
            cpus_len: 4,
        }
    }

    fn rendered() -> ([u8; NOTE_MAX], usize) {
        let mut buf = [0xAAu8; NOTE_MAX];
        let n = render(&info(), &mut buf).unwrap();
        (buf, n)
    }

    #[test]
    fn render_note_header_and_padding() {
        let (buf, n) = rendered();
        assert_eq!(n % 4, 0);
        assert_eq!(&buf[0..4], &11u32.to_le_bytes());
        let descsz = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        assert_eq!(&buf[8..12], &0u32.to_le_bytes());
        assert_eq!(&buf[12..24], b"VMCOREINFO\0\0");
        assert_eq!(n, 24 + descsz.next_multiple_of(4));
        assert_eq!(buf[24 + descsz - 1], b'\n');
        assert!(buf[24 + descsz..n].iter().all(|&b| b == 0));
        assert!(buf[24..24 + descsz].iter().all(|b| b.is_ascii()));
    }

    #[test]
    fn render_keys_in_order() {
        let (buf, n) = rendered();
        let note = parse_note(&buf[..n]).unwrap();
        let text = core::str::from_utf8(note.desc).unwrap();
        let keys: std::vec::Vec<&str> =
            text.lines().map(|l| l.split_once('=').unwrap().0).collect();
        assert_eq!(keys, KEYS);
        assert_eq!(
            text,
            "OSRELEASE=0.1.0\n\
             BUILD-ID=0123456789abcdef00112233445566778899aabb\n\
             PAGESIZE=4096\n\
             NUMBER(vibeos_pgt_root)=305418240\n\
             NUMBER(vibeos_pgt_levels)=4\n\
             SYMBOL(vibeos_log)=ffffffff80123440\n\
             SYMBOL(vibeos_tcbs)=ffffffff80200000\n\
             LENGTH(vibeos_tcbs)=256\n\
             SYMBOL(vibeos_cpus)=ffff800001000000\n\
             LENGTH(vibeos_cpus)=4\n"
        );
    }

    #[test]
    fn render_rejects_small_buffer() {
        let (_, n) = rendered();
        for len in [0, 11, 24, 40, n - 1] {
            let mut buf = std::vec![0u8; len];
            assert_eq!(render(&info(), &mut buf), Err(NoteError::TooSmall), "{len}");
        }
        let mut exact = std::vec![0u8; n];
        assert_eq!(render(&info(), &mut exact), Ok(n));
    }

    #[test]
    fn parse_note_roundtrip() {
        let (buf, n) = rendered();
        let note = parse_note(&buf[..n]).unwrap();
        assert_eq!(note.name, NOTE_NAME);
        assert_eq!(note.kind, NOTE_TYPE);
        assert_eq!(get(note.desc, "PAGESIZE"), Some(&b"4096"[..]));
        assert_eq!(get(note.desc, "NUMBER(vibeos_pgt_levels)"), Some(&b"4"[..]));
        assert_eq!(
            get(note.desc, "BUILD-ID"),
            Some(&b"0123456789abcdef00112233445566778899aabb"[..])
        );
        assert_eq!(
            get(note.desc, "SYMBOL(vibeos_log)"),
            Some(&b"ffffffff80123440"[..])
        );
        assert_eq!(get(note.desc, "PAGE"), None);
        assert_eq!(get(note.desc, "NO-SUCH-KEY"), None);
        // Trailing descriptor padding may be absent.
        let descsz = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        assert_eq!(parse_note(&buf[..24 + descsz]).unwrap(), note);
    }

    #[test]
    fn parse_note_rejects_malformed() {
        let (buf, n) = rendered();
        // Short: less than a header.
        for len in [0, 4, 11] {
            assert_eq!(parse_note(&buf[..len]), Err(NoteError::Truncated), "{len}");
        }
        // Misaligned: the name's padding is cut off.
        assert_eq!(parse_note(&buf[..22]), Err(NoteError::Truncated));
        // descsz past the end.
        let mut long = buf[..n].to_vec();
        long[4..8].copy_from_slice(&(n as u32).to_le_bytes());
        assert_eq!(parse_note(&long), Err(NoteError::Truncated));
        let mut huge = buf[..n].to_vec();
        huge[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_note(&huge), Err(NoteError::Truncated));
        huge[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_note(&huge), Err(NoteError::Truncated));
    }

    /// A `.note.gnu.build-id` section's bytes.
    fn gnu_note(name: &[u8], kind: u32, desc: &[u8]) -> std::vec::Vec<u8> {
        let mut v = std::vec::Vec::new();
        v.extend_from_slice(&(name.len() as u32 + 1).to_le_bytes());
        v.extend_from_slice(&(desc.len() as u32).to_le_bytes());
        v.extend_from_slice(&kind.to_le_bytes());
        v.extend_from_slice(name);
        v.push(0);
        while v.len() % 4 != 0 {
            v.push(0);
        }
        v.extend_from_slice(desc);
        v
    }

    #[test]
    fn gnu_build_id_reads_descriptor() {
        let sec = gnu_note(b"GNU", NT_GNU_BUILD_ID, &ID);
        assert_eq!(sec.len(), 16 + 20);
        assert_eq!(gnu_build_id(&sec), Ok(&ID[..]));
        let fast = gnu_note(b"GNU", NT_GNU_BUILD_ID, &ID[..8]);
        assert_eq!(gnu_build_id(&fast), Ok(&ID[..8]));
    }

    #[test]
    fn gnu_build_id_rejects_wrong_name_or_type() {
        assert_eq!(
            gnu_build_id(&gnu_note(b"GNX", NT_GNU_BUILD_ID, &ID)),
            Err(NoteError::WrongName)
        );
        assert_eq!(
            gnu_build_id(&gnu_note(b"VMCOREINFO", NT_GNU_BUILD_ID, &ID)),
            Err(NoteError::WrongName)
        );
        assert_eq!(
            gnu_build_id(&gnu_note(b"GNU", 1, &ID)),
            Err(NoteError::WrongType)
        );
        let sec = gnu_note(b"GNU", NT_GNU_BUILD_ID, &ID);
        assert_eq!(
            gnu_build_id(&sec[..sec.len() - 1]),
            Err(NoteError::Truncated)
        );
        assert_eq!(gnu_build_id(&[]), Err(NoteError::Truncated));
    }

    #[test]
    fn fw_cfg_vmcoreinfo_is_little_endian() {
        let v = FwCfgVmcoreinfo {
            host_format: FORMAT_ELF,
            guest_format: FORMAT_ELF,
            size: 0x0102_0304,
            paddr: 0x1122_3344_5566_7788,
        };
        let b = v.to_le_bytes();
        assert_eq!(
            b,
            [
                0x01, 0x00, 0x01, 0x00, 0x04, 0x03, 0x02, 0x01, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33,
                0x22, 0x11
            ]
        );
        assert_eq!(FwCfgVmcoreinfo::from_le_bytes(&b), v);
        assert_eq!(
            core::mem::size_of::<FwCfgVmcoreinfo>(),
            FwCfgVmcoreinfo::LEN
        );
        assert!(v.host_takes_elf());
        let none = FwCfgVmcoreinfo::from_le_bytes(&[0; 16]);
        assert!(!none.host_takes_elf());
        assert!(
            !FwCfgVmcoreinfo {
                host_format: 2,
                ..none
            }
            .host_takes_elf()
        );
    }

    #[test]
    fn keys_documented() {
        const DOC: &str = include_str!("../../../../docs/VMCOREINFO.md");
        let table: std::vec::Vec<&str> = DOC.lines().filter(|l| l.starts_with("| `")).collect();
        for key in KEYS {
            let cell = std::format!("`{key}`");
            assert!(
                table.iter().any(|row| row.contains(&cell)),
                "docs/VMCOREINFO.md's key table lacks {cell}"
            );
        }
        let (buf, n) = rendered();
        let note = parse_note(&buf[..n]).unwrap();
        let emitted: std::vec::Vec<&[u8]> = note
            .desc
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| l.split(|&b| b == b'=').next().unwrap())
            .collect();
        let want: std::vec::Vec<&[u8]> = KEYS.iter().map(|k| k.as_bytes()).collect();
        assert_eq!(emitted, want);
    }
}
