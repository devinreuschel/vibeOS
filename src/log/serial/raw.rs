//! Raw COM1: the lock-free bottom of `serial` (C-RAWSERIAL, DESIGN §2.5
//! step 1).
//!
//! Port I/O with a bounded THRE poll, the halt flag, and the panic dump's
//! owner. It takes no lock, captures nothing into the log ring, and calls
//! nothing in the kernel above the cpu module, so a CPU that holds any
//! lock, or that faulted inside one, can still write here. The TX lock and
//! the locked writes stay in `serial` (`serial/mod.rs`).
//!
//! It is the only code that writes the UART data register, and it frames
//! every kernel line (DESIGN §2.6): [`put_line`] writes `vibeos::log::line`'s
//! frame, the escaped content and `\r\n`, after a `\r\n` when user output
//! left a line open, and [`put_user`] writes console bytes a process wrote,
//! escaped and unframed.

use core::ptr;
use vibeos::atomic::statics::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use vibeos::log::line;
use vibeos::uart::*;

#[cfg(target_arch = "x86_64")]
use crate::x86;

/// Set by the panic dump's owner before it stops the others
/// (`ipi_init::stop_others`). From then on a serial write or log append on
/// any other CPU stops that CPU ([`stop_if_halting`]), and the owner's
/// writes skip the TX lock and the log capture.
pub static HALTING: AtomicBool = AtomicBool::new(false);

/// Set while the last bytes on the UART were user output that did not end
/// its line, so the next kernel line starts on a fresh one. It starts set:
/// the loader's last byte is unknown (OVMF may not end its line). Written
/// under the TX lock, or by the dump's owner once `HALTING` is set.
static USER_OPEN: AtomicBool = AtomicBool::new(true);

const NO_OWNER: u32 = u32::MAX;

/// The CPU that owns the panic dump, or `NO_OWNER`.
static DUMP_OWNER: AtomicU32 = AtomicU32::new(NO_OWNER);

/// The stop primitive's stop routine (`ipi_init::stop_hook`, installed by
/// `ipi_init::init`): stops a CPU that must not write while the owner
/// dumps. A fn pointer, so this layer names nothing above arch.
static STOP_HOOK: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Bring COM1 up: DLAB dance, 115200 8N1, FIFO on. Safe to run again;
/// the panic path re-runs it since the panic may itself be in serial.
#[cfg(target_arch = "x86_64")]
pub fn init() {
    // SAFETY: invariant: COM1's registers are the I/O ports
    // `COM1_BASE + REG_*` on every PC-compatible machine QEMU models, and
    // this sequence only programs that UART; established by `vibeos::uart::COM1_BASE`.
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

#[cfg(target_arch = "x86_64")]
fn write_byte(b: u8) {
    // Bounded THRE poll; drop on cap rather than spin forever (DESIGN §9.6).
    let mut spin = TX_POLL_CAP;
    while spin > 0 {
        // SAFETY: invariant: `COM1_BASE + REG_LSR` and `+ REG_DATA` are
        // COM1's status and data ports; established by `vibeos::uart::COM1_BASE`.
        let lsr = unsafe { x86::inb(COM1_BASE + REG_LSR) };
        if lsr & LSR_THRE != 0 {
            // SAFETY: as above: COM1's data port; established by
            // `vibeos::uart::COM1_BASE`.
            unsafe { x86::outb(COM1_BASE + REG_DATA, b) };
            return;
        }
        spin -= 1;
    }
}

/// Write `content` as one framed kernel line (`line::kernel_line`). No
/// lock: the caller holds `serial`'s TX lock, or is the dump's owner.
pub fn put_line(content: &[u8]) {
    // AcqRel: the flag is read and cleared in one step, so a write on the
    // halt path, which takes no lock, sees a defined value.
    let open = USER_OPEN.swap(false, Ordering::AcqRel);
    line::kernel_line(open, content, write_byte);
}

/// Write console bytes a process wrote (`line::user_bytes`): unframed,
/// each frame byte escaped, `\n` as `\r\n`. No lock: the caller holds
/// `serial`'s TX lock.
pub fn put_user(bytes: &[u8]) {
    // Acquire / Release: as in `put_line`, the value is defined on every path.
    let was = USER_OPEN.load(Ordering::Acquire);
    // The flag follows each byte, set before any byte but `\n` goes out and
    // cleared after a `\n`: a panic dump that interrupts this write on this
    // CPU (an #MC or NMI, after `force_unlock`) then starts its first line
    // on a fresh one. At worst it writes one empty line.
    let open = line::user_bytes(was, bytes, |b| {
        if b != b'\n' {
            USER_OPEN.store(true, Ordering::Release);
        }
        write_byte(b);
        if b == b'\n' {
            USER_OPEN.store(false, Ordering::Release);
        }
    });
    USER_OPEN.store(open, Ordering::Release);
}

/// Poll COM1 RX. No lock; a caller racing another reader holds IRQs off.
#[cfg(target_arch = "x86_64")]
pub fn try_read_byte() -> Option<u8> {
    // SAFETY: invariant: `COM1_BASE + REG_LSR` and `+ REG_DATA` are COM1's
    // status and data ports; established by `vibeos::uart::COM1_BASE`.
    let lsr = unsafe { x86::inb(COM1_BASE + REG_LSR) };
    if lsr & LSR_DR == 0 {
        return None;
    }
    // SAFETY: as above: COM1's data port; established by
    // `vibeos::uart::COM1_BASE`.
    Some(unsafe { x86::inb(COM1_BASE + REG_DATA) })
}

/// This CPU's index. Before the per-CPU area is live only the BSP runs.
#[cfg(target_arch = "x86_64")]
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

/// Install the stop hook (DESIGN §2.5 step 1's stop primitive).
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

/// Whether this CPU owns the panic dump.
pub fn is_owner() -> bool {
    owner_cpu() == Some(this_cpu())
}

/// Write `line` as one framed kernel line, only on the dump's owner: no
/// lock and no `InterruptGuard`, so the dump writes whatever this CPU held
/// or faulted in (DESIGN §2.5 step 1). Any other CPU runs the stop hook if
/// one is set, and otherwise returns without writing.
pub fn write_owner(line: &[u8]) {
    if is_owner() {
        put_line(line);
    } else {
        run_stop_hook();
    }
}

/// Once `HALTING` is set, stop this CPU unless it owns the dump: run the
/// stop hook, and `cli; hlt` for good if none is set or it returns. Returns
/// while `HALTING` is clear, and on the owner.
pub fn stop_if_halting() {
    // Acquire: pairs with the Release store in `ipi_init::stop_others`.
    if !HALTING.load(Ordering::Acquire) || is_owner() {
        return;
    }
    run_stop_hook();
    #[cfg(target_arch = "x86_64")]
    x86::halt();
}
