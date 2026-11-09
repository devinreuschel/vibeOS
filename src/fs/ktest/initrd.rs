//! The initrd module's in-guest test (ROADMAP §10.5).

use vibeos::fs::O_RDONLY;

use crate::fat_init;
use crate::ktest::{Outcome, fid};

/// The initrd is the Limine module `mkinitrd` sized: a multiple of 512
/// bytes, outside usable RAM, mapped by the physmap, the whole module is the
/// mounted image, and it has at most `INITRD_FREE_BYTES` + 4 KiB free.
pub(crate) fn test_initrd_module_sized() -> Outcome {
    let info = crate::boot::info();
    let Some(r) = info.initrd() else {
        return Outcome::Fail("no initrd module");
    };
    let len = r.end - r.start;
    if len == 0 || !len.is_multiple_of(512) {
        return crate::fail_fmt!("module is {} bytes", len);
    }
    if info.usable().any(|u| u.start < r.end && r.start < u.end) {
        return Outcome::Fail("module in usable ram");
    }
    for pa in [r.start, r.end - 1] {
        let va = vibeos::paging::VirtAddr(crate::paging_init::hhdm_offset() + pa);
        match crate::paging_init::translate(va) {
            Some((got, _, _)) if got.as_u64() == pa => {}
            _ => return crate::fail_fmt!("module byte {:#x} not in the physmap", pa),
        }
    }
    let Some((bytes, free)) = fat_init::initrd_geometry() else {
        return Outcome::Fail("initrd not mounted");
    };
    if bytes != len {
        return crate::fail_fmt!("image {} bytes, module {}", bytes, len);
    }
    if free > vibeos::fat::INITRD_FREE_BYTES + 4096 {
        return crate::fail_fmt!("{} bytes free", free);
    }
    let mut buf = [0u8; 32];
    match fid::open("/hello.txt", O_RDONLY, 0) {
        Ok(f) => {
            let got = fid::read(f, &mut buf);
            let _ = fid::close(f);
            match got {
                Ok(n) if buf.get(..n) == Some(&b"hello from initrd\n"[..]) => Outcome::Ok,
                _ => Outcome::Fail("/hello.txt read"),
            }
        }
        Err(_) => Outcome::Fail("/hello.txt open"),
    }
}
