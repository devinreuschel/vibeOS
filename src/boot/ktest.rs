//! In-guest tests for boot (kernel_tests only). Rows: [`TESTS`].

use vibeos::boot::cmdline::{CMDLINE_MAX, CmdlineBuf};
use vibeos::boot::{FW_CFG_DMA_READ, FW_CFG_NAME_MAX};
use vibeos::dma::DmaAlloc;
use vibeos::paging::VirtAddr;

use super::fw_cfg_init::{self, FwCfgError, FwCfgFile};
use crate::dma_init;
use crate::ktest::{Outcome, Test, test};
use crate::paging_init;

/// `BootInfo` agrees with what PMM and paging built from it.
pub(crate) fn test_bootinfo_consistent() -> Outcome {
    let info = crate::boot::info();
    let k = &info.kernel_phys;
    if info.usable().any(|r| r.start < k.end && k.start < r.end) {
        return Outcome::Fail("kernel image in usable ram");
    }
    let text = VirtAddr(test_bootinfo_consistent as *const () as u64);
    match paging_init::translate(text) {
        Some((pa, _, _)) if k.contains(&pa.as_u64()) => {}
        _ => return Outcome::Fail("text outside kernel span"),
    }
    for fb in info.framebuffers() {
        let mut p = fb.phys & !0xFFFu64;
        let end = fb.phys.saturating_add(fb.size);
        while p < end {
            let hhdm = VirtAddr(paging_init::hhdm_offset().wrapping_add(p));
            let mapped = paging_init::translate(hhdm).is_some()
                || crate::fb_init::va_for_phys(p)
                    .is_some_and(|v| paging_init::translate(VirtAddr(v)).is_some());
            if !mapped {
                return Outcome::Fail("fb page unmapped");
            }
            p = p.saturating_add(0x1000);
        }
    }
    let slot = {
        #[cfg(target_arch = "x86_64")]
        {
            vibeos::paging::PHYSMAP_X86_64
        }
        #[cfg(target_arch = "aarch64")]
        {
            vibeos::paging::PHYSMAP_AARCH64
        }
    };
    if !vibeos::paging::hhdm_in_slot(info.hhdm_offset, slot) {
        return Outcome::Fail("hhdm offset outside slot");
    }
    if info.hhdm_offset != paging_init::hhdm_offset() {
        return Outcome::Fail("hhdm offset not the one paging uses");
    }
    Outcome::Ok
}

/// Up to 16 directory entries, names NUL-padded.
fn fw_cfg_entries() -> ([(FwCfgFile, [u8; FW_CFG_NAME_MAX], usize); 16], usize) {
    let mut out = [(FwCfgFile { select: 0, size: 0 }, [0u8; FW_CFG_NAME_MAX], 0); 16];
    let mut n = 0;
    fw_cfg_init::walk(|f, name| {
        let slot = &mut out[n];
        slot.0 = f;
        slot.1[..name.len()].copy_from_slice(name);
        slot.2 = name.len();
        n += 1;
        n < out.len()
    });
    (out, n)
}

/// fw_cfg is found under QEMU, its directory entries are found again by
/// name, an absent name is not, and `read` returns `min(size, len)` bytes.
pub(crate) fn test_fw_cfg_probe() -> Outcome {
    if !fw_cfg_init::hypervisor() {
        return Outcome::Skip("no hypervisor bit");
    }
    if !fw_cfg_init::present() {
        return Outcome::Fail("fw_cfg not present under a hypervisor");
    }
    let (entries, n) = fw_cfg_entries();
    if n == 0 {
        return Outcome::Fail("empty fw_cfg directory");
    }
    for (f, name, len) in &entries[..n] {
        let Ok(name) = core::str::from_utf8(&name[..*len]) else {
            continue;
        };
        if fw_cfg_init::file(name) != Some(*f) {
            return Outcome::Fail("directory entry not found again by name");
        }
    }
    if fw_cfg_init::file("opt/vibeos/no-such-file").is_some() {
        return Outcome::Fail("absent name found");
    }
    let Some((f, _, _)) = entries[..n].iter().find(|e| e.0.size >= 4) else {
        return Outcome::Fail("no file of 4 bytes or more");
    };
    let mut small = [0u8; 3];
    if fw_cfg_init::read(f, &mut small) != 3 {
        return Outcome::Fail("read past a short buffer");
    }
    let mut big = [0u8; 64];
    let tiny = FwCfgFile {
        select: f.select,
        size: 2,
    };
    if fw_cfg_init::read(&tiny, &mut big) != 2 {
        return Outcome::Fail("read past the file size");
    }
    if big[..2] != small[..2] {
        return Outcome::Fail("two reads of one file differ");
    }
    Outcome::Ok
}

/// A DMA read equals the port read of the same file, and a DMA write to a
/// read-only file is refused by the device.
pub(crate) fn test_fw_cfg_dma() -> Outcome {
    if !fw_cfg_init::hypervisor() {
        return Outcome::Skip("no hypervisor bit");
    }
    if !fw_cfg_init::has_dma() {
        return Outcome::Skip("no fw_cfg dma");
    }
    let (entries, n) = fw_cfg_entries();
    let Some((f, _, _)) = entries[..n].iter().find(|e| e.0.size != 0) else {
        return Outcome::Fail("no non-empty fw_cfg file");
    };
    let len = (f.size as usize).min(256);
    let mut port = [0u8; 256];
    if fw_cfg_init::read(f, &mut port[..len]) != len {
        return Outcome::Fail("short port read");
    }
    let Some(buf) = dma_init::alloc(DmaAlloc::new(4096)) else {
        return Outcome::Fail("no dma buffer");
    };
    let r = fw_cfg_init::transfer(FW_CFG_DMA_READ, f.select, &buf, len as u32);
    // SAFETY: `dma_init::alloc` established it: `buf` owns 4096 bytes at
    // `as_ptr`, and the transfer above has completed or failed.
    let got = unsafe { core::slice::from_raw_parts(buf.as_ptr().add(fw_cfg_init::DMA_DATA), len) };
    let same = got == &port[..len];
    dma_init::free(buf);
    match r {
        Ok(()) if same => {}
        Ok(()) => return Outcome::Fail("dma read differs from port read"),
        Err(e) => return Outcome::Fail(e.as_str()),
    }
    match fw_cfg_init::dma_write(f.select, b"vibeOS") {
        Err(FwCfgError::Device) => Outcome::Ok,
        Ok(()) => Outcome::Fail("dma write to a read-only file succeeded"),
        Err(_) => Outcome::Fail("dma write failed without the device error bit"),
    }
}

/// `BootInfo` holds Limine's `cmdline:`, alone or followed by one space and
/// the fw_cfg file read again by port I/O, and `boot::cmdline()` parses it.
pub(crate) fn test_cmdline_captured() -> Outcome {
    let info = crate::boot::info();
    let raw = info.cmdline_raw();
    let lim = info.cmdline_limine_len();
    if lim == 0 {
        return Outcome::Fail("limine cmdline empty");
    }
    if crate::boot::cmdline().raw() != raw {
        return Outcome::Fail("boot::cmdline() is not BootInfo's text");
    }
    let Some(rest) = raw.get(lim..) else {
        return Outcome::Fail("limine part past the end");
    };
    let file = fw_cfg_init::file(crate::boot::FW_CFG_CMDLINE);
    match (rest.split_first(), file) {
        (None, _) => Outcome::Ok,
        (Some((b' ', tail)), Some(f)) => {
            let mut text = [0u8; CMDLINE_MAX + 1];
            let n = fw_cfg_init::read(&f, &mut text);
            let mut want = CmdlineBuf::new();
            want.append(&text[..n]);
            if want.as_bytes() == tail {
                Outcome::Ok
            } else {
                Outcome::Fail("appended text is not the fw_cfg file")
            }
        }
        (Some(_), Some(_)) => Outcome::Fail("no space between the two parts"),
        (Some(_), None) => Outcome::Fail("text appended without the fw_cfg file"),
    }
}

/// Tracing is on exactly when `vibeos.strace` is.
pub(crate) fn test_strace_flag_matches_cmdline() -> Outcome {
    if crate::syscall_init::trace_enabled() == crate::boot::cmdline().flag("vibeos.strace") {
        Outcome::Ok
    } else {
        Outcome::Fail("trace_enabled differs from vibeos.strace")
    }
}

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[
    test("bootinfo_consistent", test_bootinfo_consistent),
    test("fw_cfg_probe", test_fw_cfg_probe),
    test("fw_cfg_dma", test_fw_cfg_dma),
    test("cmdline_captured", test_cmdline_captured),
    test(
        "strace_flag_matches_cmdline",
        test_strace_flag_matches_cmdline,
    ),
];
