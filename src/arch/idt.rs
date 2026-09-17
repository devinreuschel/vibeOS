//! IDT and exception handlers. DESIGN §5.2.
//!
//! 256 gates, `x86-interrupt` ABI. Unhandled vectors dump and halt.
//! `#BP` logs and returns. `#UD`/`#GP`/`#PF`/`#DF`/`#MC` dump and halt.

use vibeos::desc::{IdtEntry, InterruptFrame, IstSlot, KERNEL_CS};
use vibeos::fmt_util;
use vibeos::vectors;

use crate::arch::catch;
use crate::arch::pic;
use crate::serial::Serial;
use crate::x86::{self, DtPtr};

struct Cell<T>(core::cell::UnsafeCell<T>);
unsafe impl<T> Sync for Cell<T> {}
impl<T> Cell<T> {
    const fn new(v: T) -> Self {
        Self(core::cell::UnsafeCell::new(v))
    }
    fn ptr(&self) -> *mut T {
        self.0.get()
    }
}

#[repr(C, align(16))]
struct Idt([IdtEntry; 256]);

static IDT: Cell<Idt> = Cell::new(Idt([IdtEntry::EMPTY; 256]));

macro_rules! install_noerr {
    ($($n:literal),* $(,)?) => {
        $(
            set_noerr($n, default_noerr::<{ $n as u8 }>, ist_for($n));
        )*
    };
}

macro_rules! install_err {
    ($($n:literal),* $(,)?) => {
        $(
            set_err($n, default_err::<{ $n as u8 }>, ist_for($n));
        )*
    };
}

extern "x86-interrupt" fn default_noerr<const N: u8>(mut frame: InterruptFrame) {
    if catch::intercept(N, &mut frame, 0) {
        return;
    }
    halt_vec(N, &frame, None, None);
}

extern "x86-interrupt" fn default_err<const N: u8>(mut frame: InterruptFrame, err: u64) {
    if catch::intercept(N, &mut frame, err) {
        return;
    }
    let cr2 = if N == vectors::PF {
        Some(x86::read_cr2())
    } else {
        None
    };
    halt_vec(N, &frame, Some(err), cr2);
}

extern "x86-interrupt" fn breakpoint(mut frame: InterruptFrame) {
    if catch::intercept(vectors::BP, &mut frame, 0) {
        return;
    }
    dump(b"#BP", &frame, None, None);
}

extern "x86-interrupt" fn invalid_opcode(mut frame: InterruptFrame) {
    if catch::intercept(vectors::UD, &mut frame, 0) {
        return;
    }
    halt_named(b"#UD", &frame, None, None);
}

extern "x86-interrupt" fn nmi(mut frame: InterruptFrame) {
    if catch::intercept(vectors::NMI, &mut frame, 0) {
        return;
    }
    halt_named(b"nmi", &frame, None, None);
}

extern "x86-interrupt" fn debug_ex(mut frame: InterruptFrame) {
    if catch::intercept(vectors::DB, &mut frame, 0) {
        return;
    }
    halt_named(b"#DB", &frame, None, None);
}

extern "x86-interrupt" fn general_protection(mut frame: InterruptFrame, err: u64) {
    if catch::intercept(vectors::GP, &mut frame, err) {
        return;
    }
    halt_named(b"#GP", &frame, Some(err), None);
}

extern "x86-interrupt" fn page_fault(mut frame: InterruptFrame, err: u64) {
    if catch::intercept(vectors::PF, &mut frame, err) {
        return;
    }
    halt_named(b"#PF", &frame, Some(err), Some(x86::read_cr2()));
}

extern "x86-interrupt" fn double_fault(mut frame: InterruptFrame, err: u64) {
    if catch::intercept(vectors::DF, &mut frame, err) {
        return;
    }
    halt_named(b"#DF", &frame, Some(err), None);
}

extern "x86-interrupt" fn machine_check(mut frame: InterruptFrame) {
    if catch::intercept(vectors::MC, &mut frame, 0) {
        return;
    }
    halt_named(b"#MC", &frame, None, None);
}

extern "x86-interrupt" fn irq<const N: u8>(_frame: InterruptFrame) {
    pic::handle(N);
}

extern "x86-interrupt" fn pit_irq(_frame: InterruptFrame) {
    let tsc = crate::time_init::read_tsc();
    crate::time_init::on_pit_tick(tsc);
    crate::time_init::eoi_pit();
    crate::sched_init::on_timer_tick();
}

extern "x86-interrupt" fn lapic_timer_irq(_frame: InterruptFrame) {
    crate::apic_init::on_timer_irq();
}

extern "x86-interrupt" fn lapic_error_irq(_frame: InterruptFrame) {
    crate::apic_init::on_error_irq();
}

extern "x86-interrupt" fn lapic_thermal_irq(_frame: InterruptFrame) {
    crate::apic_init::on_thermal_irq();
}

extern "x86-interrupt" fn lapic_spurious_irq(_frame: InterruptFrame) {
    crate::apic_init::on_spurious_irq();
}

pub fn pointer() -> (u16, u64) {
    (
        (core::mem::size_of::<Idt>() - 1) as u16,
        IDT.ptr() as u64,
    )
}

/// # Safety
/// Shared IDT already filled. GDT loaded so KERNEL_CS and IST TSS match.
pub unsafe fn load() {
    let (limit, base) = pointer();
    let idtr = DtPtr { limit, base };
    unsafe { x86::lidt(&idtr) };
}

/// Fill all 256 entries, overlay named handlers, `lidt`.
///
/// # Safety
/// GDT already loaded. PIC already remapped and masked.
pub unsafe fn init() {
    unsafe {
        install_defaults();
        overlay_named();
        load();
    }
}

unsafe fn install_defaults() {
    install_noerr!(
        0, 1, 2, 3, 4, 5, 6, 7, 9, 15, 16, 18, 19, 20, 22, 23, 24, 25, 26, 27, 28, 31, 32, 33, 34,
        35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57,
        58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80,
        81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99, 100, 101, 102,
        103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120,
        121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 136, 137, 138,
        139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 152, 153, 154, 155, 156,
        157, 158, 159, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171, 172, 173, 174,
        175, 176, 177, 178, 179, 180, 181, 182, 183, 184, 185, 186, 187, 188, 189, 190, 191, 192,
        193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 203, 204, 205, 206, 207, 208, 209, 210,
        211, 212, 213, 214, 215, 216, 217, 218, 219, 220, 221, 222, 223, 224, 225, 226, 227, 228,
        229, 230, 231, 232, 233, 234, 235, 236, 237, 238, 239, 240, 241, 242, 243, 244, 245, 246,
        247, 248, 249, 250, 251, 252, 253, 254, 255
    );
    install_err!(8, 10, 11, 12, 13, 14, 17, 21, 29, 30);
}

unsafe fn overlay_named() {
    set_noerr(vectors::DB, debug_ex, IstSlot::Debug.hardware());
    set_noerr(vectors::NMI, nmi, IstSlot::Nmi.hardware());
    set_noerr(vectors::BP, breakpoint, 0);
    set_noerr(vectors::UD, invalid_opcode, 0);
    set_err(
        vectors::DF,
        double_fault,
        IstSlot::DoubleFault.hardware(),
    );
    set_err(vectors::GP, general_protection, 0);
    set_err(vectors::PF, page_fault, 0);
    set_noerr(vectors::MC, machine_check, IstSlot::MachineCheck.hardware());

    set_noerr(vectors::IRQ_BASE, pit_irq, 0);
    set_noerr(vectors::IRQ_BASE + 1, irq::<{ vectors::IRQ_BASE + 1 }>, 0);
    set_noerr(vectors::IRQ_BASE + 2, irq::<{ vectors::IRQ_BASE + 2 }>, 0);
    set_noerr(vectors::IRQ_BASE + 3, irq::<{ vectors::IRQ_BASE + 3 }>, 0);
    set_noerr(vectors::IRQ_BASE + 4, irq::<{ vectors::IRQ_BASE + 4 }>, 0);
    set_noerr(vectors::IRQ_BASE + 5, irq::<{ vectors::IRQ_BASE + 5 }>, 0);
    set_noerr(vectors::IRQ_BASE + 6, irq::<{ vectors::IRQ_BASE + 6 }>, 0);
    set_noerr(vectors::IRQ_SPURIOUS_MASTER, irq::<{ vectors::IRQ_SPURIOUS_MASTER }>, 0);
    set_noerr(vectors::IRQ_SLAVE_BASE, irq::<{ vectors::IRQ_SLAVE_BASE }>, 0);
    set_noerr(vectors::IRQ_SLAVE_BASE + 1, irq::<{ vectors::IRQ_SLAVE_BASE + 1 }>, 0);
    set_noerr(vectors::IRQ_SLAVE_BASE + 2, irq::<{ vectors::IRQ_SLAVE_BASE + 2 }>, 0);
    set_noerr(vectors::IRQ_SLAVE_BASE + 3, irq::<{ vectors::IRQ_SLAVE_BASE + 3 }>, 0);
    set_noerr(vectors::IRQ_SLAVE_BASE + 4, irq::<{ vectors::IRQ_SLAVE_BASE + 4 }>, 0);
    set_noerr(vectors::IRQ_SLAVE_BASE + 5, irq::<{ vectors::IRQ_SLAVE_BASE + 5 }>, 0);
    set_noerr(vectors::IRQ_SLAVE_BASE + 6, irq::<{ vectors::IRQ_SLAVE_BASE + 6 }>, 0);
    set_noerr(vectors::IRQ_SPURIOUS_SLAVE, irq::<{ vectors::IRQ_SPURIOUS_SLAVE }>, 0);

    set_noerr(vectors::LAPIC_TIMER, lapic_timer_irq, 0);
    set_noerr(vectors::LAPIC_ERROR, lapic_error_irq, 0);
    set_noerr(vectors::LAPIC_THERMAL, lapic_thermal_irq, 0);
    set_noerr(vectors::LAPIC_SPURIOUS, lapic_spurious_irq, 0);
}

fn ist_for(vec: u8) -> u8 {
    if vec == vectors::DB {
        IstSlot::Debug.hardware()
    } else if vec == vectors::NMI {
        IstSlot::Nmi.hardware()
    } else if vec == vectors::DF {
        IstSlot::DoubleFault.hardware()
    } else if vec == vectors::MC {
        IstSlot::MachineCheck.hardware()
    } else {
        0
    }
}

fn set_noerr(vec: u8, h: extern "x86-interrupt" fn(InterruptFrame), ist: u8) {
    unsafe {
        (*IDT.ptr()).0[vec as usize] =
            IdtEntry::interrupt(fn_addr_noerr(h), KERNEL_CS, ist);
    }
}

fn set_err(vec: u8, h: extern "x86-interrupt" fn(InterruptFrame, u64), ist: u8) {
    unsafe {
        (*IDT.ptr()).0[vec as usize] = IdtEntry::interrupt(fn_addr_err(h), KERNEL_CS, ist);
    }
}

fn fn_addr_noerr(h: extern "x86-interrupt" fn(InterruptFrame)) -> u64 {
    h as usize as u64
}

fn fn_addr_err(h: extern "x86-interrupt" fn(InterruptFrame, u64)) -> u64 {
    h as usize as u64
}

fn hex(n: u64) {
    let mut b = [0u8; 16];
    Serial::write_bytes(fmt_util::write_hex(n, &mut b));
}

fn dump(kind: &[u8], frame: &InterruptFrame, err: Option<u64>, cr2: Option<u64>) {
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
}

fn halt_named(kind: &[u8], frame: &InterruptFrame, err: Option<u64>, cr2: Option<u64>) -> ! {
    dump(kind, frame, err, cr2);
    x86::halt();
}

fn halt_vec(n: u8, frame: &InterruptFrame, err: Option<u64>, cr2: Option<u64>) -> ! {
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
    x86::halt();
}
