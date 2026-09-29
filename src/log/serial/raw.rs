//! Raw COM1: the lock-free bottom of `serial` (C-RAWSERIAL, DESIGN §2.5
//! step 1).
//!
//! Port I/O with a bounded THRE poll, the halt flag, and the panic dump's
//! owner. It takes no lock, captures nothing into the log ring, and calls
//! nothing in the kernel above the cpu module, so a CPU that holds any
//! lock, or that faulted inside one, can still write here. The TX lock and
//! the locked writes stay in `serial` (`serial/mod.rs`).

use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

use vibeos::uart::*;

use crate::x86;

/// Set once a CPU starts stopping the others for a panic dump
/// (`ipi_init::halt_others`) or takes the halt IPI. From then on the
/// serial writes skip the TX lock and the log capture.
pub static HALTING: AtomicBool = AtomicBool::new(false);

const NO_OWNER: u32 = u32::MAX;

/// The CPU that owns the panic dump, or `NO_OWNER`.
static DUMP_OWNER: AtomicU32 = AtomicU32::new(NO_OWNER);

/// ROADMAP §10.7's stop primitive, once it installs itself: parks a CPU
/// that must not write while the owner dumps.
static STOP_HOOK: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Bring COM1 up: DLAB dance, 115200 8N1, FIFO on. Safe to run again;
/// the panic path re-runs it since the panic may itself be in serial.
pub fn init() {
    // SAFETY: invariant: COM1's registers are the I/O ports
    // `COM1_BASE + REG_*` on every PC-compatible machine QEMU models, and
    // this sequence only programs that UART; established by `vibeos::uart`.
    unsafe {
        x86::outb(COM1_BASE + REG_IER, 0x00); // mask all interrupts
        x86::outb(COM1_BASE + REG_LCR, LCR_DLAB);
        x86::outb(COM1_BASE + REG_DLL, (BAUD_115200_DIVISOR & 0xFF) as u8);
        x86::outb(COM1_BASE + REG_DLM, (BAUD_115200_DIVISOR >> 8) as u8);
        x86::outb(COM1_BASE + REG_LCR, LCR_8N1);
        x86::outb(COM1_BASE + REG_FCR, FCR_ENABLE);
        x86::outb(COM1_BASE + REG_MCR, MCR_READY);
    }
}

fn write_byte(b: u8) {
    // Bounded THRE poll; drop on cap rather than spin forever (DESIGN §9.6).
    let mut spin = TX_POLL_CAP;
    while spin > 0 {
        // SAFETY: invariant: `COM1_BASE + REG_LSR` and `+ REG_DATA` are
        // COM1's status and data ports; established by `vibeos::uart`.
        let lsr = unsafe { x86::inb(COM1_BASE + REG_LSR) };
        if lsr & LSR_THRE != 0 {
            // SAFETY: as above.
            unsafe { x86::outb(COM1_BASE + REG_DATA, b) };
            return;
        }
        spin -= 1;
    }
}

/// Write `bytes`, `\n` as `\r\n`. No lock: a caller that must not
/// interleave with another CPU holds `serial`'s TX lock.
pub fn write_bytes(bytes: &[u8]) {
    for &b in bytes {
        if b == b'\n' {
            write_byte(b'\r');
        }
        write_byte(b);
    }
}

/// Poll COM1 RX. No lock; a caller racing another reader holds IRQs off.
pub fn try_read_byte() -> Option<u8> {
    // SAFETY: invariant: `COM1_BASE + REG_LSR` and `+ REG_DATA` are COM1's
    // status and data ports; established by `vibeos::uart`.
    let lsr = unsafe { x86::inb(COM1_BASE + REG_LSR) };
    if lsr & LSR_DR == 0 {
        return None;
    }
    // SAFETY: as above.
    Some(unsafe { x86::inb(COM1_BASE + REG_DATA) })
}

/// This CPU's index. Before the per-CPU area is live only the BSP runs.
fn this_cpu() -> u32 {
    x86::cpu_index().unwrap_or(0)
}

/// Make this CPU the panic dump's owner. True only for the first caller:
/// false for every later one, the owner re-entering included, so a
/// nested panic does not dump again.
pub fn claim_dump() -> bool {
    // AcqRel: the winner's later writes follow its claim, and a loser
    // sees the owner the winner stored.
    DUMP_OWNER
        .compare_exchange(NO_OWNER, this_cpu(), Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// The CPU that owns the panic dump, if one has claimed it.
pub fn owner_cpu() -> Option<u32> {
    // Acquire: pairs with the AcqRel exchange in `claim_dump`.
    let o = DUMP_OWNER.load(Ordering::Acquire);
    (o != NO_OWNER).then_some(o)
}

/// Install the stop hook (ROADMAP §10.7's stop primitive).
#[allow(
    dead_code,
    reason = "C-RAWSERIAL: ROADMAP §10.7's stop primitive is its first caller"
)]
pub fn set_stop_hook(f: fn()) {
    // Release: pairs with the Acquire load in `run_stop_hook`.
    STOP_HOOK.store(f as *mut (), Ordering::Release);
}

/// Run the stop hook, if one is set. A stop hook that parks this CPU
/// does not return.
fn run_stop_hook() {
    // Acquire: pairs with the Release store in `set_stop_hook`.
    let p = STOP_HOOK.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: invariant: a non-null `STOP_HOOK` holds a `fn()`; established
    // by `serial::raw::set_stop_hook`, its only store.
    let f = unsafe { core::mem::transmute::<*mut (), fn()>(p) };
    f();
}

fn is_owner() -> bool {
    owner_cpu() == Some(this_cpu())
}

/// Write one kernel line, only on the dump's owner. Any other CPU runs
/// the stop hook if one is set, and otherwise returns without writing.
#[allow(
    dead_code,
    reason = "C-RAWSERIAL: ROADMAP §10.7 routes the panic path's writes through it"
)]
pub fn write_owner(bytes: &[u8]) {
    if is_owner() {
        write_bytes(bytes);
    } else {
        run_stop_hook();
    }
}

/// A write once `HALTING` is set: on the owner it writes; elsewhere it
/// runs the stop hook, and if none is set or it returns, writes unlocked.
pub fn write_after_halt(bytes: &[u8]) {
    if !is_owner() {
        run_stop_hook();
    }
    write_bytes(bytes);
}
