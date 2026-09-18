//! PS/2 8042 + IRQ1. ROADMAP §5.2, DESIGN §3.3 / §5.5 / §9.4.
//!
//! Order: install the handler, init the controller, then unmask the
//! keyboard GSI. ISR only enqueues; no alloc, no log.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vibeos::acpi::Iso;
use vibeos::apic::{self, Polarity, Trigger};
use vibeos::kbd::{
    self, DecodedKey, Decoder, Ring, CFG_CLOCK2_OFF, CFG_INT1, CFG_INT2, CFG_TRANSLATE,
    CMD_DISABLE_1, CMD_DISABLE_2, CMD_ENABLE_1, CMD_READ_CFG, CMD_SELF_TEST, CMD_TEST_1,
    CMD_WRITE_CFG, DATA, KBD_ACK, KBD_BAT_OK, KBD_RESET, PORT_TEST_OK, RING_CAP, SELF_TEST_OK,
    STAT_IBF, STAT_MOUSE, STAT_OBF, STATUS,
};
use vibeos::pic::{PIC1_CMD, PIC_EOI};
use vibeos::vectors;

use crate::acpi_init;
use crate::apic_init;
use crate::arch;
use crate::per_cpu_init;
use crate::serial;
use crate::x86::{self, InterruptGuard};

const POLL_CAP: u32 = 100_000;
const GSI_NONE: u32 = u32::MAX;

struct Cell<T>(core::cell::UnsafeCell<T>);
unsafe impl<T> Sync for Cell<T> {}

static DECODER: Cell<Decoder> = Cell(core::cell::UnsafeCell::new(Decoder::new()));
static RING: Cell<Ring<DecodedKey, RING_CAP>> =
    Cell(core::cell::UnsafeCell::new(Ring::empty(DecodedKey::Char(0))));
static LIVE: AtomicBool = AtomicBool::new(false);
static GSI: AtomicU32 = AtomicU32::new(GSI_NONE);
static PIC_FALLBACK: AtomicBool = AtomicBool::new(false);

extern "x86-interrupt" fn kbd_ioapic(_frame: vibeos::desc::InterruptFrame) {
    on_irq();
    apic_init::eoi();
}

extern "x86-interrupt" fn kbd_pic(_frame: vibeos::desc::InterruptFrame) {
    on_irq();
    unsafe { x86::outb(PIC1_CMD, PIC_EOI) };
}

/// ISR: read 0x60, push, return. No alloc, no log.
pub fn on_irq() {
    let status = unsafe { x86::inb(STATUS) };
    if status & STAT_OBF == 0 {
        return;
    }
    let data = unsafe { x86::inb(DATA) };
    if status & STAT_MOUSE != 0 {
        return;
    }
    let dec = unsafe { &mut *DECODER.0.get() };
    if let Some(k) = dec.feed(data) {
        unsafe { (*RING.0.get()).push(k) };
    }
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn pop() -> Option<DecodedKey> {
    let _irq = InterruptGuard::enter();
    unsafe { (*RING.0.get()).pop() }
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn gsi() -> Option<u32> {
    let g = GSI.load(Ordering::Acquire);
    if g == GSI_NONE {
        None
    } else {
        Some(g)
    }
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn pic_fallback() -> bool {
    PIC_FALLBACK.load(Ordering::Acquire)
}

/// Install handler, init 8042, then unmask.
pub fn init() -> bool {
    arch::idt::set_handler(vectors::KBD, kbd_ioapic);
    arch::idt::set_handler(vectors::IRQ_KEYBOARD, kbd_pic);

    let routed = route_keyboard();
    if !init_8042() {
        serial::line("vibeOS: kbd: 8042 init failed");
        return false;
    }
    match routed {
        Some(gsi) => {
            apic_init::unmask_gsi(gsi);
            GSI.store(gsi, Ordering::Release);
        }
        None => {
            arch::pic::unmask(1);
            PIC_FALLBACK.store(true, Ordering::Release);
        }
    }
    LIVE.store(true, Ordering::Release);
    true
}

fn route_keyboard() -> Option<u32> {
    let info = acpi_init::info()?;
    let madt = info.madt.as_ref()?;
    let isos = &madt.isos[..madt.iso_count];
    let gsi = apic::gsi_for_isa_irq(1, isos);
    let (trig, pol) = iso_irq1(isos, gsi);
    let dest = per_cpu_init::cpu(0).map(|c| c.apic_id as u8).unwrap_or(0);
    if apic_init::route_gsi(gsi, vectors::KBD, dest, trig, pol).is_err() {
        return None;
    }
    Some(gsi)
}

fn iso_irq1(isos: &[Iso], gsi: u32) -> (Trigger, Polarity) {
    let mut i = 0;
    while i < isos.len() {
        if isos[i].irq == 1 || isos[i].gsi == gsi {
            return (
                apic::iso_trigger(isos[i].flags),
                apic::iso_polarity(isos[i].flags),
            );
        }
        i += 1;
    }
    (Trigger::Edge, Polarity::High)
}

fn wait_ibf_clear() -> bool {
    let mut n = POLL_CAP;
    while n > 0 {
        if unsafe { x86::inb(STATUS) } & STAT_IBF == 0 {
            return true;
        }
        n -= 1;
    }
    false
}

fn wait_obf() -> bool {
    let mut n = POLL_CAP;
    while n > 0 {
        if unsafe { x86::inb(STATUS) } & STAT_OBF != 0 {
            return true;
        }
        n -= 1;
    }
    false
}

fn write_cmd(cmd: u8) -> bool {
    if !wait_ibf_clear() {
        return false;
    }
    unsafe { x86::outb(kbd::CMD, cmd) };
    true
}

fn write_data(data: u8) -> bool {
    if !wait_ibf_clear() {
        return false;
    }
    unsafe { x86::outb(DATA, data) };
    true
}

fn read_data() -> Option<u8> {
    if !wait_obf() {
        return None;
    }
    Some(unsafe { x86::inb(DATA) })
}

fn flush_obf() {
    let mut n = 16u32;
    while n > 0 && unsafe { x86::inb(STATUS) } & STAT_OBF != 0 {
        let _ = unsafe { x86::inb(DATA) };
        n -= 1;
    }
}

fn init_8042() -> bool {
    if !write_cmd(CMD_DISABLE_1) {
        return false;
    }
    let _ = write_cmd(CMD_DISABLE_2);
    flush_obf();

    if !write_cmd(CMD_READ_CFG) {
        return false;
    }
    let Some(mut cfg) = read_data() else {
        return false;
    };
    cfg &= !(CFG_INT1 | CFG_INT2);
    cfg |= CFG_CLOCK2_OFF | CFG_TRANSLATE;
    if !write_cmd(CMD_WRITE_CFG) || !write_data(cfg) {
        return false;
    }

    if !write_cmd(CMD_SELF_TEST) {
        return false;
    }
    match read_data() {
        Some(SELF_TEST_OK) => {}
        _ => return false,
    }

    if !write_cmd(CMD_TEST_1) {
        return false;
    }
    match read_data() {
        Some(PORT_TEST_OK) => {}
        _ => return false,
    }

    if !write_cmd(CMD_ENABLE_1) {
        return false;
    }

    // Device reset is best-effort; the controller is already live.
    if write_data(KBD_RESET) {
        if let Some(b) = read_data() {
            if b == KBD_ACK {
                let _ = read_data(); // BAT, expect 0xAA
            } else if b != KBD_BAT_OK {
                flush_obf();
            }
        }
    }

    cfg |= CFG_INT1;
    cfg &= !CFG_INT2;
    cfg |= CFG_TRANSLATE | CFG_CLOCK2_OFF;
    if !write_cmd(CMD_WRITE_CFG) || !write_data(cfg) {
        return false;
    }
    flush_obf();
    true
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn push_for_test(k: DecodedKey) {
    let _irq = InterruptGuard::enter();
    unsafe { (*RING.0.get()).push(k) };
}
