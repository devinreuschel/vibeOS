//! Panic and exception dump. ROADMAP §5.6, DESIGN §2.5.
//!
//! Binding order:
//! 1. `cli`, claim the dump, stop the other CPUs (`ipi_init::stop_others`)
//! 2. Re-init serial from scratch
//! 3. Print location/message; regs, current thread, last N log records
//! 4. Symbolized backtrace when frame pointers exist
//! 5. `hlt` loop, or QEMU isa-debug-exit under `panic_exit`
//!
//! From the `cli` in `begin_dump` on, every line is one
//! `serial::raw::write_owner` call, built in a stack buffer ([`line`],
//! [`out`]): the dump takes no lock and no `InterruptGuard`, so a panic
//! inside a guard's bookkeeping or with a lock held still dumps once
//! (DESIGN §2.5 steps 1-3).

use core::fmt::{self, Write};
use core::panic::PanicInfo;

use vibeos::desc::InterruptFrame;
use vibeos::fmt_util::{self, StackBuf};
use vibeos::irq::stop::{CrashRegs, StopHow};
use vibeos::log::DUMP_LAST;
use vibeos::log::line::LINE_CAP;
use vibeos::marker;
use vibeos::symtab;

use crate::per_cpu_init;
use crate::serial::raw;
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

/// Claim the dump, then stop the others, then re-init serial (DESIGN §2.5
/// step 1). The owner re-entering writes a one-liner and halts (or
/// isa-debug-exit) without walking the ring again; any other CPU that
/// finds the dump claimed runs the stop routine with `regs` (`panic`) and
/// writes nothing.
fn begin_dump(regs: CrashRegs) {
    x86::cli();
    if !raw::claim_dump() {
        if raw::is_owner() {
            raw::write_owner(b"vibeOS: panic: reentered");
            finish();
        }
        crate::ipi_init::stop_this_cpu(StopHow::Panic, regs);
    }
    crate::ipi_init::stop_others();
    // SAFETY: DESIGN §2.5 step 1, established at `ipi_init::stop_others`:
    // every other online CPU has acknowledged its stop and halted with
    // IF=0, or is reported `not stopped` after the NMI, before the log
    // cell is taken from its holder. A CPU left `not stopped` loops
    // without polling, which DESIGN §2.9 rule 2 makes a bug; that CPU is
    // the dump's accepted risk.
    unsafe { crate::log_init::force_unlock() };
    raw::init();
}

/// Each other online CPU, in id order: `vibeOS: panic: cpu N stopped
/// (<how>)` and its `cpu N regs:` slot, or `vibeOS: panic: cpu N not
/// stopped` (DESIGN §2.5 step 1).
fn report_cpus() {
    let owner = raw::owner_cpu();
    let online = per_cpu_init::online_mask();
    for c in 0..64u32 {
        if online & (1u64 << c) == 0 || Some(c) == owner {
            continue;
        }
        match crate::ipi_init::cpu_stop_state(c) {
            Some((Some(how), r)) => {
                out(format_args!(
                    "vibeOS: panic: cpu {c} stopped ({})",
                    how.as_str()
                ));
                out(format_args!(
                    "vibeOS: panic: cpu {c} regs: rip=0x{:016x} rsp=0x{:016x} rbp=0x{:016x} rflags=0x{:016x}",
                    r.rip, r.rsp, r.rbp, r.rflags
                ));
            }
            _ => out(format_args!("vibeOS: panic: cpu {c} not stopped")),
        }
    }
}

/// One dump line, which `f` builds in a `LINE_CAP` stack buffer (cut with
/// `...` when longer), in one `raw::write_owner`: no lock, no
/// `InterruptGuard` (DESIGN §2.5 step 1).
pub(crate) fn line(f: impl FnOnce(&mut StackBuf<'_>)) {
    let mut buf = [0u8; LINE_CAP];
    let mut w = StackBuf::new(&mut buf);
    f(&mut w);
    w.mark_cut();
    raw::write_owner(w.as_bytes());
}

/// One formatted dump line, as [`line`].
pub(crate) fn out(args: fmt::Arguments<'_>) {
    line(|w| {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "`StackBuf` truncates and never fails, so only a formatter's own error lands here, leaving a shorter line and nothing to act on (DESIGN §2.5)"
        )]
        let _ = w.write_fmt(args);
    });
}

fn hex(w: &mut StackBuf<'_>, n: u64) {
    let mut b = [0u8; 16];
    w.push_bytes(fmt_util::write_hex(n, &mut b));
}

fn dump_regs(rbp: u64, rsp: u64, rflags: u64, rip: u64) {
    line(|w| {
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
    // The panic may come with IF=1: the id is a hint and the thread one
    // load (DESIGN §2.9 rule 5).
    let cpu = crate::arch::cpu_id_hint();
    let cur = crate::arch::current_tcb();
    let (tid, name) = if !per_cpu_init::is_live() {
        (0u32, "<early>")
    } else if cur.is_null() {
        (0, "<none>")
    } else {
        // SAFETY: invariant I9: a non-null `current` names a `Tcb` that
        // stays in `SCHED`; the other CPUs are sent the stop IPI first
        // (`panic::begin_dump`), and this reads two fields set before the
        // thread ran; established by `thread_init::switch_now`.
        let t = unsafe { &*cur };
        (t.id.raw(), t.name)
    };
    out(format_args!(
        "vibeOS: panic: thread cpu={cpu} tid={tid} {name}"
    ));
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
    line(|w| {
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
    raw::write_owner(b"vibeOS: backtrace:");
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
    raw::write_owner(b"vibeOS: panic: halted");
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
    // SAFETY: DESIGN §2.5 step 1, established at `ipi_init::stop_others`
    // (called by `begin_dump` before every `dump_common`): every other
    // CPU has stopped, or is reported `not stopped`, and `begin_dump`
    // took the log cell from its holder.
    unsafe { crate::log_init::dump_tail(DUMP_LAST, out) };
    dump_backtrace(rip, rbp);
    report_cpus();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    #[cfg(feature = "kernel_tests")]
    crate::arch::catch::on_panic();
    // IF=0 before anything else: a second panicking CPU must reach
    // `begin_dump` without taking an interrupt (DESIGN §2.5 step 1).
    x86::cli();
    let rip = x86::read_rip();
    let rbp = x86::read_rbp();
    let rsp = x86::read_rsp();
    let rflags = x86::rflags();
    begin_dump(CrashRegs {
        rip,
        rsp,
        rbp,
        rflags,
    });

    raw::write_owner(marker::PANIC_BANNER.as_bytes());
    write_where(info);
    #[cfg(feature = "panic_stop_test")]
    crate::log::panic_test::after_panic_message();

    dump_common(rip, rbp, rsp, rflags);
    finish();
}

/// The panic's location and message lines.
fn write_where(info: &PanicInfo) {
    match info.location() {
        Some(loc) => out(format_args!(
            "vibeOS: panic: at {}:{}:{}",
            loc.file(),
            loc.line(),
            loc.column()
        )),
        None => raw::write_owner(b"vibeOS: panic: at <unknown>"),
    }
    out(format_args!("vibeOS: panic: msg: {}", info.message()));
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
    begin_dump(CrashRegs {
        rip: frame.rip,
        rsp: frame.rsp,
        rbp: x86::read_rbp(),
        rflags: frame.rflags,
    });
    line(|w| {
        w.push_bytes(b"vibeOS: ");
        w.push_bytes(kind);
        frame_fields(w, frame, err, cr2);
    });
    dump_common(frame.rip, x86::read_rbp(), frame.rsp, frame.rflags);
    finish();
}

pub fn exception_vec(n: u8, frame: &InterruptFrame, err: Option<u64>, cr2: Option<u64>) -> ! {
    begin_dump(CrashRegs {
        rip: frame.rip,
        rsp: frame.rsp,
        rbp: x86::read_rbp(),
        rflags: frame.rflags,
    });
    line(|w| {
        w.push_bytes(b"vibeOS: exception: vector ");
        let mut b = [0u8; 4];
        w.push_bytes(fmt_util::write_dec(n as u64, &mut b));
        frame_fields(w, frame, err, cr2);
    });
    dump_common(frame.rip, x86::read_rbp(), frame.rsp, frame.rflags);
    finish();
}
