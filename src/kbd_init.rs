//! PS/2 8042 + IRQ1. ROADMAP §5.2, DESIGN §3.3 / §5.5 / §9.4.
//!
//! Order is load-bearing: handler, route ISA IRQ1 → IOAPIC GSI, init
//! 8042, then unmask the GSI. After LAPIC owns the tick the 8259 is
//! masked — do not fall back to PIC IRQ1. ISR only enqueues; no alloc,
//! no log.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vibeos::acpi::Iso;
use vibeos::apic::{self, Polarity, Trigger};
use vibeos::kbd::{
    self, CMD_DISABLE_1, CMD_DISABLE_2, CMD_ENABLE_1, CMD_READ_CFG, CMD_SELF_TEST, CMD_TEST_1,
    CMD_WRITE_CFG, CMD_WRITE_KBD_OUT, DATA, DecodedKey, Decoder, KBD_ACK, KBD_BAT_OK, KBD_RESET,
    PORT_TEST_OK, RING_CAP, Ring, SELF_TEST_OK, STAT_IBF, STAT_MOUSE, STAT_OBF, STATUS, cfg_probe,
    cfg_run,
};
use vibeos::pic::{PIC_EOI, PIC1_CMD};
use vibeos::vectors;

use crate::acpi_init;
use crate::apic_init;
use crate::arch;
use crate::cell::IrqCell;
use crate::per_cpu_init;
use crate::x86::{self, InterruptGuard};

const POLL_CAP: u32 = 100_000;
const GSI_NONE: u32 = u32::MAX;

struct Kbd {
    decoder: Decoder,
    ring: Ring<DecodedKey, RING_CAP>,
}

static KBD: IrqCell<Kbd> = IrqCell::new(Kbd {
    decoder: Decoder::new(),
    ring: Ring::empty(DecodedKey::Char(0)),
});
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
    KBD.with(|k| {
        if let Some(key) = k.decoder.feed(data) {
            k.ring.push(key);
        }
    });
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn pop() -> Option<DecodedKey> {
    KBD.with(|k| k.ring.pop())
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn gsi() -> Option<u32> {
    let g = GSI.load(Ordering::Acquire);
    if g == GSI_NONE { None } else { Some(g) }
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn pic_fallback() -> bool {
    PIC_FALLBACK.load(Ordering::Acquire)
}

/// Handler, route GSI, 8042, then unmask. Not PIC IRQ1 after PIC mask.
pub fn init() -> bool {
    arch::idt::set_handler(vectors::KBD, kbd_ioapic);
    arch::idt::set_handler(vectors::IRQ_KEYBOARD, kbd_pic);

    let routed = route_keyboard();
    if !init_8042() {
        crate::marker!("vibeOS: kbd: 8042 init failed");
        return false;
    }
    match routed {
        Some(gsi) => {
            apic_init::unmask_gsi(gsi);
            GSI.store(gsi, Ordering::Release);
        }
        None if !apic_init::owns_tick() => {
            // PIT path: 8259 still live via LINT0 ExtINT.
            arch::pic::unmask(1);
            PIC_FALLBACK.store(true, Ordering::Release);
        }
        None => {
            // LAPIC owns the tick: PIC is masked. Unmasking IRQ1 is a
            // silent no-op (window PS/2 dead, polled COM1 still works).
            crate::marker!("vibeOS: kbd: no ioapic route");
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
    let dest = per_cpu_init::cpu(0)
        .map(|c| c.apic_id.load(Ordering::Relaxed) as u8)
        .unwrap_or(0);
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
    let Some(raw) = read_data() else {
        return false;
    };
    let mut cfg = cfg_probe(raw);
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
    if write_data(KBD_RESET)
        && let Some(b) = read_data()
    {
        if b == KBD_ACK {
            let _ = read_data(); // BAT, expect 0xAA
        } else if b != KBD_BAT_OK {
            flush_obf();
        }
    }

    cfg = cfg_run(cfg);
    if !write_cmd(CMD_WRITE_CFG) || !write_data(cfg) {
        return false;
    }
    flush_obf();
    true
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn push_for_test(k: DecodedKey) {
    KBD.with(|kbd| kbd.ring.push(k));
}

/// Read the 8042 config byte. CLI so the IRQ1 ISR cannot steal it.
#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn read_cfg() -> Option<u8> {
    let _irq = InterruptGuard::enter();
    flush_obf();
    if !write_cmd(CMD_READ_CFG) {
        return None;
    }
    read_data()
}

/// Present `sc` as a keyboard byte (cmd 0xD2). IRQ1 runs after this
/// returns if INT1 is armed and the GSI is unmasked. Not the device
/// clock: that is `cfg_clock1_on` / QEMU `sendkey`.
#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn inject_scancode(sc: u8) -> bool {
    let _irq = InterruptGuard::enter();
    flush_obf();
    write_cmd(CMD_WRITE_KBD_OUT) && write_data(sc)
}
