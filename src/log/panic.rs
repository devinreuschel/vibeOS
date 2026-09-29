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

use vibeos::desc::InterruptFrame;
use vibeos::fmt_util::{self, StackBuf};
use vibeos::log::DUMP_LAST;
use vibeos::marker;
use vibeos::symtab;

use crate::per_cpu_init;
use crate::serial::{self, Serial};
use crate::x86;

unsafe extern "C" {
    static __kernel_vma_start: u8;
    static __kernel_vma_end: u8;
}

#[cfg(feature = "panic_exit")]
const ISA_DEBUG_EXIT: u16 = 0xF4;
#[cfg(feature = "panic_exit")]
const EXIT_PANIC: u32 = 0x11;
const BT_MAX: usize = 24;

fn kstart() -> u64 {
    // SAFETY: `__kernel_vma_start` is a linker symbol; only its address is
    // taken, never its byte; established by `linker.ld`'s definition, which
    // `panic::kstart` relies on.
    unsafe { &__kernel_vma_start as *const u8 as u64 }
}

fn kend() -> u64 {
    // SAFETY: as in `panic::kstart`: only the linker symbol's address is
    // taken, never its byte.
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
    if !crate::serial::raw::claim_dump() {
        Serial::init();
        Serial::write_line(b"vibeOS: panic: reentered\n");
        finish();
    }
    crate::ipi_init::halt_others();
    // SAFETY: DESIGN §2.5 step 1, established at `ipi_init::halt_others`:
    // the other CPUs are sent the stop IPI before the log cell is taken
    // from its holder. `halt_others` does not wait, and a CPU spinning
    // with IF=0 can miss the IPI until ROADMAP §10.7's stop primitive
    // (F135), so this is the dump's accepted risk, not a proof.
    unsafe { crate::log_init::force_unlock() };
    Serial::init();
}

/// One dump line, which `f` builds in one stack buffer, in one write.
fn dump_line(f: impl FnOnce(&mut StackBuf<'_>)) {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to Serial cannot fail (DESIGN §2.5)"
    )]
    let _ = serial::write_line_with(|w| {
        f(w);
        Ok(())
    });
}

fn hex(w: &mut StackBuf<'_>, n: u64) {
    let mut b = [0u8; 16];
    w.push_bytes(fmt_util::write_hex(n, &mut b));
}

fn dump_regs(rbp: u64, rsp: u64, rflags: u64, rip: u64) {
    dump_line(|w| {
        w.push_bytes(b"vibeOS: regs: rbp=0x");
        hex(w, rbp);
        w.push_bytes(b" rsp=0x");
        hex(w, rsp);
        w.push_bytes(b" rflags=0x");
        hex(w, rflags);
        w.push_bytes(b" rip=0x");
        hex(w, rip);
        w.push_bytes(b" cr3=0x");
        hex(w, x86::read_cr3());
    });
}

fn dump_thread() {
    let (cpu, tid, name) = match per_cpu_init::try_current() {
        None => (0u32, 0u32, "<early>"),
        Some(c) => {
            if c.current.is_null() {
                (c.cpu_id, 0, "<none>")
            } else {
                // SAFETY: invariant I9: a non-null `current` names a `Tcb`
                // that stays in `SCHED`; the other CPUs are sent the stop
                // IPI first (`panic::begin_dump`), and this reads two
                // fields set before the thread ran; established by
                // `thread_init::switch_now`.
                let t = unsafe { &*c.current };
                (c.cpu_id, t.id.raw(), t.name)
            }
        }
    };
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to Serial cannot fail (DESIGN §2.5)"
    )]
    let _ = writeln!(Serial, "vibeOS: panic: thread cpu={cpu} tid={tid} {name}");
}

fn in_image(p: u64) -> bool {
    p >= kstart() && p < kend()
}

fn hex_trim(w: &mut StackBuf<'_>, n: u64) {
    let mut b = [0u8; 16];
    let s = fmt_util::write_hex(n, &mut b);
    let mut i = 0;
    while i + 1 < s.len() && s[i] == b'0' {
        i += 1;
    }
    w.push_bytes(&s[i..]);
}

fn print_frame_addr(addr: u64) {
    dump_line(|w| {
        w.push_bytes(b"  0x");
        hex(w, addr);
        if let Some(e) = crate::log::ksyms::lookup(addr) {
            let off = symtab::offset(&e, addr);
            // Sparse tables (panic-test) would otherwise pin a RIP to the
            // previous function with a huge offset.
            if off < 0x1_0000 {
                w.push_bytes(b" ");
                w.push_bytes(e.name.as_bytes());
                if off != 0 {
                    w.push_bytes(b"+0x");
                    hex_trim(w, off);
                }
            }
        }
    });
}

fn dump_backtrace(rip: u64, rbp: u64) {
    Serial::write_line(b"vibeOS: backtrace:\n");
    let mut rip = rip;
    let mut rbp = rbp;
    let mut n = 0usize;
    while n < BT_MAX {
        if !in_image(rip) {
            if n == 0 && rip != 0 {
                print_frame_addr(rip);
            }
            break;
        }
        print_frame_addr(rip);
        if !stackish(rbp) {
            break;
        }
        // SAFETY: `stackish` accepted `rbp`: 8-byte aligned and inside the
        // boot stack, the physmap, heap, KVA or the kernel image, which are
        // mapped, so the saved-RBP word reads without a fault; established
        // by `panic::stackish`.
        let prev = unsafe { core::ptr::read_volatile(rbp as *const u64) };
        // SAFETY: as above, for the return-address word 8 bytes up, in the
        // same mapped range; established by `panic::stackish`.
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
    Serial::write_line(b"vibeOS: panic: halted\n");
    #[cfg(feature = "panic_exit")]
    // SAFETY: `panic_exit` builds run under QEMU with isa-debug-exit at
    // port 0xF4, whose write ends the VM; established by the harness's
    // `-device isa-debug-exit`, which `panic::ISA_DEBUG_EXIT` names.
    unsafe {
        x86::outl(ISA_DEBUG_EXIT, EXIT_PANIC);
    }
    x86::halt();
}

fn dump_common(rip: u64, rbp: u64, rsp: u64, rflags: u64) {
    dump_regs(rbp, rsp, rflags, rip);
    dump_thread();
    // SAFETY: DESIGN §2.5 step 1, established at `ipi_init::halt_others`
    // (called by `begin_dump` before every `dump_common`): the other CPUs
    // were sent the stop IPI. `halt_others` does not wait, and a CPU
    // spinning with IF=0 can miss the IPI until ROADMAP §10.7's stop
    // primitive (F135).
    unsafe { crate::log_init::dump_tail(DUMP_LAST) };
    dump_backtrace(rip, rbp);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    #[cfg(feature = "kernel_tests")]
    crate::arch::catch::on_panic();
    let rip = x86::read_rip();
    let rbp = x86::read_rbp();
    let rsp = x86::read_rsp();
    let rflags = x86::rflags();
    begin_dump();

    Serial::write_line(marker::PANIC_BANNER.as_bytes());

    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to Serial cannot fail (DESIGN §2.5)"
    )]
    let _ = write_where(info);

    dump_common(rip, rbp, rsp, rflags);
    finish();
}

/// The panic's location and message lines, as one `fmt::Result`.
fn write_where(info: &PanicInfo) -> core::fmt::Result {
    match info.location() {
        Some(loc) => writeln!(
            Serial,
            "vibeOS: panic: at {}:{}:{}",
            loc.file(),
            loc.line(),
            loc.column()
        )?,
        None => Serial::write_line(b"vibeOS: panic: at <unknown>\n"),
    }
    writeln!(Serial, "vibeOS: panic: msg: {}", info.message())
}

/// ` rip=0x.. cs=0x.. rflags=0x.. rsp=0x.. ss=0x..[ err=0x..][ cr2=0x..]`.
pub(crate) fn frame_fields(
    w: &mut StackBuf<'_>,
    frame: &InterruptFrame,
    err: Option<u64>,
    cr2: Option<u64>,
) {
    w.push_bytes(b" rip=0x");
    hex(w, frame.rip);
    w.push_bytes(b" cs=0x");
    hex(w, frame.cs);
    w.push_bytes(b" rflags=0x");
    hex(w, frame.rflags);
    w.push_bytes(b" rsp=0x");
    hex(w, frame.rsp);
    w.push_bytes(b" ss=0x");
    hex(w, frame.ss);
    if let Some(e) = err {
        w.push_bytes(b" err=0x");
        hex(w, e);
    }
    if let Some(c) = cr2 {
        w.push_bytes(b" cr2=0x");
        hex(w, c);
    }
}

pub fn exception_halt(
    kind: &[u8],
    frame: &InterruptFrame,
    err: Option<u64>,
    cr2: Option<u64>,
) -> ! {
    begin_dump();
    dump_line(|w| {
        w.push_bytes(b"vibeOS: ");
        w.push_bytes(kind);
        frame_fields(w, frame, err, cr2);
    });
    dump_common(frame.rip, x86::read_rbp(), frame.rsp, frame.rflags);
    finish();
}

pub fn exception_vec(n: u8, frame: &InterruptFrame, err: Option<u64>, cr2: Option<u64>) -> ! {
    begin_dump();
    dump_line(|w| {
        w.push_bytes(b"vibeOS: exception: vector ");
        let mut b = [0u8; 4];
        w.push_bytes(fmt_util::write_dec(n as u64, &mut b));
        frame_fields(w, frame, err, cr2);
    });
    dump_common(frame.rip, x86::read_rbp(), frame.rsp, frame.rflags);
    finish();
}
