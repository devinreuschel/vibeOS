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
#[cfg(target_arch = "aarch64")]
use vibeos::atomic::statics::AtomicU64;
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

/// This CPU's index. Installed by `per_cpu_init`; unset, the BSP is 0.
static CPU_INDEX_HOOK: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Port halt. Installed from `_start` so a dump can park without naming
/// the port module (DESIGN §1.2: raw talks only to the `x86` alias).
static HALT_HOOK: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// PL011 data and flag registers (ARM DDI0183).
#[cfg(target_arch = "aarch64")]
const PL011_DR: u64 = 0x00;
#[cfg(target_arch = "aarch64")]
const PL011_FR: u64 = 0x18;
#[cfg(target_arch = "aarch64")]
const PL011_TXFF: u32 = 1 << 5;
#[cfg(target_arch = "aarch64")]
const PL011_RXFE: u32 = 1 << 4;
/// QEMU `virt` UART before the device tree is walked.
#[cfg(target_arch = "aarch64")]
const PL011_EARLY: u64 = 0x0900_0000;

#[cfg(target_arch = "aarch64")]
static UART_VA: AtomicU64 = AtomicU64::new(PL011_EARLY);

/// Point later writes at a mapped PL011. After paging takeover.
#[cfg(target_arch = "aarch64")]
pub fn set_mmio(va: u64) {
    // Release: pairs with the Acquire load in `uart_va`.
    UART_VA.store(va, Ordering::Release);
}

#[cfg(target_arch = "aarch64")]
fn uart_va() -> u64 {
    // Acquire: pairs with the Release store in `set_mmio`.
    UART_VA.load(Ordering::Acquire)
}

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

#[cfg(target_arch = "aarch64")]
pub fn init() {
    // `map_early_console` already pointed `UART_VA` at `HHDM + PA`.
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

#[cfg(target_arch = "aarch64")]
fn write_byte(b: u8) {
    let va = uart_va();
    let mut spin = TX_POLL_CAP;
    while spin > 0 {
        // SAFETY: `va` is the early identity/HHDM UART or the paging-mapped
        // PL011; established by `init` / `set_mmio`. established here.
        let fr = unsafe { core::ptr::read_volatile((va.wrapping_add(PL011_FR)) as *const u32) };
        if fr & PL011_TXFF == 0 {
            // SAFETY: as above; DR is the write register. established here.
            unsafe {
                core::ptr::write_volatile((va.wrapping_add(PL011_DR)) as *mut u32, u32::from(b));
            }
            return;
        }
        spin -= 1;
    }
}

/// Write `content` as one framed kernel line (`line::kernel_line`). No
/// lock: the caller holds `serial`'s TX lock, or is the dump's owner.
pub fn put_line(content: &[u8]) {
    // AcqRel: pairs with the Acquire load and Release stores in `put_user`.
    let open = USER_OPEN.swap(false, Ordering::AcqRel);
    line::kernel_line(open, content, write_byte);
}

/// Write console bytes a process wrote (`line::user_bytes`): unframed,
/// each frame byte escaped, `\n` as `\r\n`. No lock: the caller holds
/// `serial`'s TX lock.
pub fn put_user(bytes: &[u8]) {
    // Acquire: pairs with the AcqRel swap in `put_line` and the Release stores below.
    let was = USER_OPEN.load(Ordering::Acquire);
    // The flag follows each byte, set before any byte but `\n` goes out and
    // cleared after a `\n`: a panic dump that interrupts this write on this
    // CPU (an #MC or NMI, after `force_unlock`) then starts its first line
    // on a fresh one. At worst it writes one empty line.
    let open = line::user_bytes(was, bytes, |b| {
        if b != b'\n' {
            // Release: pairs with the AcqRel swap in `put_line` and the Acquire load above.
            USER_OPEN.store(true, Ordering::Release);
        }
        write_byte(b);
        if b == b'\n' {
            // Release: pairs with the AcqRel swap in `put_line` and the Acquire load above.
            USER_OPEN.store(false, Ordering::Release);
        }
    });
    // Release: pairs with the AcqRel swap in `put_line` and the Acquire load above.
    USER_OPEN.store(open, Ordering::Release);
}

/// Poll COM1 RX. No lock; a caller racing another reader holds IRQs off.
#[cfg(target_arch = "aarch64")]
pub fn try_read_byte() -> Option<u8> {
    let va = uart_va();
    // SAFETY: as `write_byte`. established here.
    let fr = unsafe { core::ptr::read_volatile((va.wrapping_add(PL011_FR)) as *const u32) };
    if fr & PL011_RXFE != 0 {
        return None;
    }
    // SAFETY: as `write_byte`. established here.
    let dr = unsafe { core::ptr::read_volatile((va.wrapping_add(PL011_DR)) as *const u32) };
    Some(dr as u8)
}

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
fn this_cpu() -> u32 {
    // Acquire: pairs with the Release store in `set_cpu_index_hook`.
    let p = CPU_INDEX_HOOK.load(Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: a non-null hook holds `fn() -> Option<u32>`; established by
    // `serial::raw::set_cpu_index_hook`, its only store.
    let f = unsafe { core::mem::transmute::<*mut (), fn() -> Option<u32>>(p) };
    f().unwrap_or(0)
}

/// Install the CPU-index hook. `per_cpu_init::init_bsp` calls it.
pub fn set_cpu_index_hook(f: fn() -> Option<u32>) {
    // Release: pairs with the Acquire load in `this_cpu`.
    CPU_INDEX_HOOK.store(f as *mut (), Ordering::Release);
}

/// Install the port halt. `_start` calls it right after `Serial::init`.
pub fn set_halt_hook(f: fn() -> !) {
    // Release: pairs with the Acquire load in `run_halt`.
    HALT_HOOK.store(f as *mut (), Ordering::Release);
}

fn run_halt() -> ! {
    // Acquire: pairs with the Release store in `set_halt_hook`.
    let p = HALT_HOOK.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: a non-null hook holds `fn() -> !`; established by
        // `serial::raw::set_halt_hook`, its only store.
        let f = unsafe { core::mem::transmute::<*mut (), fn() -> !>(p) };
        f();
    }
    loop {
        core::hint::spin_loop();
    }
}

/// Make this CPU the panic dump's owner. True only for the first caller:
/// false for every later one, the owner re-entering included, so a
/// nested panic does not dump again.
pub fn claim_dump() -> bool {
    // AcqRel, Acquire on failure: pairs with the Acquire load in `owner_cpu`.
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
    run_halt();
}
