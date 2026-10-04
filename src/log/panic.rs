//! Panic and exception dump. ROADMAP §5.6, DESIGN §2.5.
//!
//! Binding order:
//! 1. `cli`, claim the dump, stop the other CPUs (`ipi_init::stop_others`)
//! 2. Re-init serial from scratch
//! 3. Print location/message; regs, current thread, last N log records
//! 4. Symbolized backtrace when frame pointers exist
//! 5. pvpanic's panicked event where boot found the device
//!    (`pvpanic_init::signal`, DESIGN §2.5 step 7), then the `hlt` loop
//!
//! From the `cli` in `begin_dump` on, every line is one
//! `serial::raw::write_owner` call, built in a stack buffer ([`line`],
//! [`out`]): the dump takes no lock and enters no interrupt guard, so a
//! panic inside a guard's bookkeeping or with a lock held still dumps once
//! (DESIGN §2.5 steps 1-3).

use core::fmt::{self, Write};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, Ordering};

use vibeos::desc::InterruptFrame;
use vibeos::fmt_util::{self, StackBuf};
use vibeos::irq::stop::{CrashRegs, StopHow};
use vibeos::log::DUMP_LAST;
use vibeos::log::backtrace::{self, StackRange, WalkEnd};
use vibeos::log::line::LINE_CAP;
use vibeos::log::vmcore::PANIC_LINE_CAP;
use vibeos::log::vmcore::sig::PanicLine;
use vibeos::marker;
use vibeos::symtab;

use crate::per_cpu_init;
use crate::serial::raw;
#[cfg(target_arch = "x86_64")]
use crate::x86;

unsafe extern "C" {
    static __kernel_vma_start: u8;
    static __kernel_vma_end: u8;
}

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

/// The first line of the dump owner's panic message, which the core tool's
/// `sig:` line reads from a guest core (ROADMAP §10.7): serial capture
/// stops while halting, so the log ring holds no panic text.
static PANIC_LINE: PanicLine = PanicLine::new();

/// The dump owner's record for the core tool: `line` (the panic message,
/// or an exception line without `vibeOS: `) in [`PANIC_LINE`], and `regs`,
/// where the dump began, in its own crash-register slot. No lock, no
/// allocation; after `begin_dump` only.
fn record_panic(line: &[u8], regs: CrashRegs) {
    let cpu = raw::owner_cpu().unwrap_or_else(crate::arch::cpu_id_hint);
    PANIC_LINE.record(cpu, line);
    crate::ipi_init::save_crash_regs(regs);
}

/// The boot stack Limine handed `_start`, `[lo, hi)`, recorded by
/// [`note_boot_stack`] before anything else runs; both 0 until then.
static BOOT_STACK_LO: AtomicU64 = AtomicU64::new(0);
static BOOT_STACK_HI: AtomicU64 = AtomicU64::new(0);

/// Record the boot stack's bounds from `_start`'s first RSP: the 4 KiB
/// page above it is the top, and the stack is the size the Limine request
/// asks for (`boot::LIMINE_STACK_BYTES`). `_start`'s first statement, so
/// a panic on Limine's stack walks it (DESIGN §2.5 step 4).
pub fn note_boot_stack(rsp: u64) {
    let hi = rsp.checked_add(0xFFF).map_or(rsp, |v| v & !0xFFF);
    let lo = hi.saturating_sub(crate::boot::LIMINE_STACK_BYTES);
    // Relaxed: written once on the BSP before any other CPU runs; pairs with nothing.
    BOOT_STACK_LO.store(lo, Ordering::Relaxed);
    // Relaxed: written once on the BSP before any other CPU runs; pairs with nothing.
    BOOT_STACK_HI.store(hi, Ordering::Relaxed);
}

/// The stacks a backtrace may follow `rbp` into (DESIGN §2.5 step 4): the
/// current thread's KVA stack, the recorded boot stack, and this CPU's
/// four IST stacks and fallback RSP0 stack. Fills `out` from the front
/// and returns how many it filled. IF=0 callers only (DESIGN §2.9 rule 5).
pub(crate) fn known_stacks(out: &mut [StackRange; 8]) -> usize {
    let mut n = 0usize;
    let mut push = |r: StackRange| {
        if r.lo < r.hi
            && let Some(slot) = out.get_mut(n)
        {
            *slot = r;
            n += 1;
        }
    };
    let cur = crate::arch::current_tcb();
    if !cur.is_null() {
        // SAFETY: invariant I9: a non-null `current` names a `Tcb` that
        // stays in `SCHED` while it runs, and its `stack` is set before it
        // first ran and dropped only after its CPU switched off it; this
        // is that CPU, so the stack is live; established by
        // `thread_init::switch_now`.
        let t = unsafe { &*cur };
        if let Some(st) = t.stack.as_ref() {
            push(StackRange::new(st.base().as_u64(), st.top().as_u64()));
        }
    }
    // Relaxed: the stop before the dump orders the BSP's stores; pairs with nothing.
    push(StackRange::new(
        BOOT_STACK_LO.load(Ordering::Relaxed),
        BOOT_STACK_HI.load(Ordering::Relaxed),
    ));
    if let Some(cpu) = per_cpu_init::try_current() {
        for (top, pages) in crate::arch::gdt::this_cpu_stacks(cpu) {
            push(StackRange::below(top, pages));
        }
    }
    n
}

/// Walk the frame-pointer chain from `rip` and `rbp` through the known
/// stacks only (`vibeos::log::backtrace::walk`), handing each frame's
/// address to `out`. IF=0 callers only.
pub(crate) fn walk_known(rip: u64, rbp: u64, out: impl FnMut(u64)) -> (usize, WalkEnd) {
    let mut stacks = [StackRange::EMPTY; 8];
    let n = known_stacks(&mut stacks);
    let known = stacks.get(..n).unwrap_or(&[]);
    let read = |a: u64| {
        // SAFETY: `backtrace::walk` reads only a word of a frame record it
        // found inside one of `known`, each a mapped stack: the current
        // thread's (live while it runs), the boot stack Limine mapped, and
        // this CPU's IST and RSP0 stacks, which invariant I51 keeps mapped
        // while it is online; established by `panic::known_stacks` and
        // `vibeos::log::backtrace::on_known_stack`.
        unsafe { core::ptr::read_volatile(a as *const u64) }
    };
    backtrace::walk(rip, rbp, known, in_image, read, out)
}

/// The kernel symbol that holds `addr`, as the dump prints it: `None` for
/// an address the table places more than 64 KiB past its symbol.
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "the in-guest backtrace test is its only caller")
)]
pub(crate) fn symbol_name(addr: u64) -> Option<&'static str> {
    let e = crate::log::ksyms::lookup(addr)?;
    (symtab::offset(&e, addr) < 0x1_0000).then_some(e.name)
}

/// Claim the dump, then stop the others, then re-init serial (DESIGN §2.5
/// step 1). The owner re-entering writes a one-liner and halts (after the
/// pvpanic write) without walking the ring again; any other CPU that
/// finds the dump claimed runs the stop routine with `regs` (`panic`) and
/// writes nothing.
fn begin_dump(regs: CrashRegs) {
    crate::arch::current::irq_disable();
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
/// `...` when longer), in one `raw::write_owner`: no lock, no interrupt
/// guard (DESIGN §2.5 step 1).
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

#[cfg(target_arch = "x86_64")]
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
    walk_known(rip, rbp, print_frame_addr);
}

/// The dump's end, then DESIGN §2.5 step 7: pvpanic's panicked event,
/// written only after the whole dump is on COM1, since the host may pause
/// or end the guest on it, then the halt.
fn finish() -> ! {
    raw::write_owner(b"vibeOS: panic: halted");
    #[cfg(target_arch = "x86_64")]
    crate::log::pvpanic_init::signal(vibeos::log::pvpanic::Step::Halt);
    crate::arch::current::halt();
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
    crate::arch::current::irq_disable();
    #[cfg(target_arch = "x86_64")]
    let (rip, rbp, rsp, rflags) = (
        x86::read_rip(),
        x86::read_rbp(),
        x86::read_rsp(),
        x86::rflags(),
    );
    let regs = CrashRegs {
        rip,
        rsp,
        rbp,
        rflags,
    };
    begin_dump(regs);
    {
        let mut msg = [0u8; PANIC_LINE_CAP];
        let mut w = StackBuf::new(&mut msg);
        #[expect(
            clippy::let_underscore_must_use,
            reason = "`StackBuf` truncates and never fails, so only a formatter's own error lands here, leaving a shorter line and nothing to act on (DESIGN §2.5)"
        )]
        let _ = write!(w, "{}", info.message());
        record_panic(w.as_bytes(), regs);
    }

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
#[cfg(target_arch = "x86_64")]
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

/// The interrupted context's registers, where an exception dump begins.
#[cfg(target_arch = "x86_64")]
fn exception_regs(frame: &InterruptFrame, rbp: u64) -> CrashRegs {
    CrashRegs {
        rip: frame.rip,
        rsp: frame.rsp,
        rbp,
        rflags: frame.rflags,
    }
}

/// An exception dump's first line, `vibeOS: ` and what `f` builds, formatted
/// once: written as [`line`] writes, and recorded without the prefix for the
/// core tool (`record_panic`) with the interrupted `regs`.
#[cfg(target_arch = "x86_64")]
fn first_line(regs: CrashRegs, f: impl FnOnce(&mut StackBuf<'_>)) {
    const PREFIX: &[u8] = b"vibeOS: ";
    let mut buf = [0u8; LINE_CAP];
    let mut w = StackBuf::new(&mut buf);
    w.push_bytes(PREFIX);
    f(&mut w);
    w.mark_cut();
    raw::write_owner(w.as_bytes());
    record_panic(w.as_bytes().get(PREFIX.len()..).unwrap_or(&[]), regs);
}

/// An exception's dump: `frame` is the interrupted context's hardware
/// frame and `rbp` its `rbp` as the entry stub saved it (the trap frame's
/// user words), where the `vibeOS: regs:` line and the backtrace start
/// (DESIGN §2.5 step 4, F070).
#[cfg(target_arch = "x86_64")]
pub fn exception_halt(
    kind: &[u8],
    frame: &InterruptFrame,
    rbp: u64,
    err: Option<u64>,
    cr2: Option<u64>,
) -> ! {
    let regs = exception_regs(frame, rbp);
    begin_dump(regs);
    first_line(regs, |w| {
        w.push_bytes(kind);
        frame_fields(w, frame, err, cr2);
    });
    dump_common(frame.rip, rbp, frame.rsp, frame.rflags);
    finish();
}

#[cfg(target_arch = "x86_64")]
/// [`exception_halt`] for a vector with no mnemonic.
pub fn exception_vec(
    n: u8,
    frame: &InterruptFrame,
    rbp: u64,
    err: Option<u64>,
    cr2: Option<u64>,
) -> ! {
    let regs = exception_regs(frame, rbp);
    begin_dump(regs);
    first_line(regs, |w| {
        w.push_bytes(b"exception: vector ");
        let mut b = [0u8; 4];
        w.push_bytes(fmt_util::write_dec(n as u64, &mut b));
        frame_fields(w, frame, err, cr2);
    });
    dump_common(frame.rip, rbp, frame.rsp, frame.rflags);
    finish();
}
