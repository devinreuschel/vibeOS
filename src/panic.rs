//! Panic and exception dump. ROADMAP §5.6, DESIGN §2.5.
//!
//! Binding order:
//! 1. Broadcast halt IPI 0xFE (Fixed, not NMI)
//! 2. Re-init serial from scratch
//! 3. Print location/message; regs, current thread, last N log records
//! 4. Symbolized backtrace when frame pointers exist
//! 5. `hlt` loop, or QEMU isa-debug-exit under `panic_exit`

use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::desc::InterruptFrame;
use vibeos::fmt_util;
use vibeos::log::DUMP_LAST;
use vibeos::marker;
use vibeos::symtab;

use crate::ksyms::KSYMS;
use crate::per_cpu_init;
use crate::serial::Serial;
use crate::x86;

unsafe extern "C" {
    static __kernel_vma_start: u8;
    static __kernel_vma_end: u8;
}

static DUMPING: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "panic_exit")]
const ISA_DEBUG_EXIT: u16 = 0xF4;
#[cfg(feature = "panic_exit")]
const EXIT_PANIC: u32 = 0x11;
const BT_MAX: usize = 24;

fn kstart() -> u64 {
    unsafe { &__kernel_vma_start as *const u8 as u64 }
}

fn kend() -> u64 {
    unsafe { &__kernel_vma_end as *const u8 as u64 }
}

fn canonical_aligned(p: u64) -> bool {
    if p == 0 || p & 7 != 0 {
        return false;
    }
    let top = p >> 47;
    top == 0 || top == 0x1FFFF
}

/// Boot stack (low ident), HHDM, heap, KVA, kernel image.
fn stackish(p: u64) -> bool {
    if !canonical_aligned(p) {
        return false;
    }
    if p < 0x2000_0000 {
        return true;
    }
    if (0xFFFF_8000_0000_0000..0xFFFF_E000_1000_0000).contains(&p) {
        return true;
    }
    if p >= kstart() && p < kend() {
        return true;
    }
    false
}

/// Halt others first, then re-init serial. Re-entry dumps a one-liner and
/// `hlt`s (or isa-debug-exit) without walking the ring again.
fn begin_dump() {
    x86::cli();
    if DUMPING.swap(true, Ordering::SeqCst) {
        Serial::init();
        Serial::write_bytes(b"vibeOS: panic: reentered\n");
        finish();
    }
    crate::ipi_init::halt_others();
    crate::log_init::force_unlock();
    Serial::init();
}

fn hex(n: u64) {
    let mut b = [0u8; 16];
    Serial::write_bytes(fmt_util::write_hex(n, &mut b));
}

fn dump_regs(rbp: u64, rsp: u64, rflags: u64, rip: u64) {
    Serial::write_bytes(b"vibeOS: regs: rbp=0x");
    hex(rbp);
    Serial::write_bytes(b" rsp=0x");
    hex(rsp);
    Serial::write_bytes(b" rflags=0x");
    hex(rflags);
    Serial::write_bytes(b" rip=0x");
    hex(rip);
    Serial::write_bytes(b" cr3=0x");
    hex(x86::read_cr3());
    Serial::write_bytes(b"\n");
}

fn dump_thread() {
    let (cpu, tid, name) = match per_cpu_init::try_current() {
        None => (0u32, 0u32, "<early>"),
        Some(c) => {
            if c.current.is_null() {
                (c.cpu_id, 0, "<none>")
            } else {
                let t = unsafe { &*c.current };
                (c.cpu_id, t.id.raw(), t.name)
            }
        }
    };
    let _ = writeln!(
        Serial,
        "vibeOS: panic: thread cpu={cpu} tid={tid} {name}"
    );
}

fn print_frame_addr(addr: u64) {
    Serial::write_bytes(b"  ");
    Serial::write_bytes(b"0x");
    hex(addr);
    if let Some(e) = symtab::lookup(KSYMS, addr) {
        Serial::write_bytes(b" ");
        Serial::write_bytes(e.name.as_bytes());
        let off = symtab::offset(e, addr);
        if off != 0 {
            Serial::write_bytes(b"+0x");
            hex(off);
        }
    }
    Serial::write_bytes(b"\n");
}

fn dump_backtrace(rip: u64, rbp: u64) {
    Serial::write_bytes(b"vibeOS: backtrace:\n");
    let mut rip = rip;
    let mut rbp = rbp;
    let mut n = 0usize;
    while n < BT_MAX {
        if rip != 0 {
            print_frame_addr(rip);
        }
        if !stackish(rbp) {
            break;
        }
        let prev = unsafe { core::ptr::read_volatile(rbp as *const u64) };
        let ret = unsafe { core::ptr::read_volatile(rbp.wrapping_add(8) as *const u64) };
        if prev == rbp || ret == 0 {
            break;
        }
        rbp = prev;
        rip = ret;
        n += 1;
    }
}

fn finish() -> ! {
    Serial::write_bytes(b"vibeOS: panic: halted\n");
    #[cfg(feature = "panic_exit")]
    unsafe {
        x86::outl(ISA_DEBUG_EXIT, EXIT_PANIC);
    }
    x86::halt();
}

fn dump_common(rip: u64, rbp: u64, rsp: u64, rflags: u64) {
    dump_regs(rbp, rsp, rflags, rip);
    dump_thread();
    crate::log_init::dump_tail(DUMP_LAST);
    dump_backtrace(rip, rbp);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    begin_dump();

    Serial::write_bytes(marker::PANIC_BANNER.as_bytes());
    Serial::write_bytes(b"\n");

    if let Some(loc) = info.location() {
        let _ = writeln!(
            Serial,
            "vibeOS: panic: at {}:{}:{}",
            loc.file(),
            loc.line(),
            loc.column()
        );
    } else {
        Serial::write_bytes(b"vibeOS: panic: at <unknown>\n");
    }

    let _ = writeln!(Serial, "vibeOS: panic: msg: {}", info.message());

    dump_common(x86::read_rip(), x86::read_rbp(), x86::read_rsp(), x86::rflags());
    finish();
}

pub fn exception_halt(
    kind: &[u8],
    frame: &InterruptFrame,
    err: Option<u64>,
    cr2: Option<u64>,
) -> ! {
    begin_dump();
    Serial::write_bytes(b"vibeOS: ");
    Serial::write_bytes(kind);
    Serial::write_bytes(b" rip=0x");
    hex(frame.rip);
    Serial::write_bytes(b" cs=0x");
    hex(frame.cs);
    Serial::write_bytes(b" rflags=0x");
    hex(frame.rflags);
    Serial::write_bytes(b" rsp=0x");
    hex(frame.rsp);
    Serial::write_bytes(b" ss=0x");
    hex(frame.ss);
    if let Some(e) = err {
        Serial::write_bytes(b" err=0x");
        hex(e);
    }
    if let Some(c) = cr2 {
        Serial::write_bytes(b" cr2=0x");
        hex(c);
    }
    Serial::write_bytes(b"\n");
    dump_common(frame.rip, x86::read_rbp(), frame.rsp, frame.rflags);
    finish();
}

pub fn exception_vec(n: u8, frame: &InterruptFrame, err: Option<u64>, cr2: Option<u64>) -> ! {
    begin_dump();
    Serial::write_bytes(b"vibeOS: exception: vector ");
    let mut b = [0u8; 4];
    Serial::write_bytes(fmt_util::write_dec(n as u64, &mut b));
    Serial::write_bytes(b" rip=0x");
    hex(frame.rip);
    Serial::write_bytes(b" cs=0x");
    hex(frame.cs);
    Serial::write_bytes(b" rflags=0x");
    hex(frame.rflags);
    Serial::write_bytes(b" rsp=0x");
    hex(frame.rsp);
    Serial::write_bytes(b" ss=0x");
    hex(frame.ss);
    if let Some(e) = err {
        Serial::write_bytes(b" err=0x");
        hex(e);
    }
    if let Some(c) = cr2 {
        Serial::write_bytes(b" cr2=0x");
        hex(c);
    }
    Serial::write_bytes(b"\n");
    dump_common(frame.rip, x86::read_rbp(), frame.rsp, frame.rflags);
    finish();
}
