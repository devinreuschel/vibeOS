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
    CMD_WRITE_CFG, DATA, DecodedKey, Decoder, KBD_ACK, KBD_BAT_OK, KBD_RESET, PORT_TEST_OK,
    RING_CAP, Ring, SELF_TEST_OK, STAT_IBF, STAT_MOUSE, STAT_OBF, STATUS, cfg_probe, cfg_run,
};
use vibeos::lock::RANK_DEVICE;
use vibeos::pic::{PIC_EOI, PIC1_CMD};
use vibeos::vectors;

use crate::apic_init;
use crate::arch;
use crate::machine_init;
use crate::per_cpu_init;
use crate::sync_init::SpinMutex;
use crate::x86;

const POLL_CAP: u32 = 100_000;
pub(super) const GSI_NONE: u32 = u32::MAX;

pub(super) struct Kbd {
    decoder: Decoder,
    pub(super) ring: Ring<DecodedKey, RING_CAP>,
}

static KBD: SpinMutex<Kbd> = SpinMutex::with_rank(
    Kbd {
        decoder: Decoder::new(),
        ring: Ring::empty(DecodedKey::Char(0)),
    },
    RANK_DEVICE,
);

/// Run `f` on the keyboard state.
pub(super) fn with_kbd<R>(f: impl FnOnce(&mut Kbd) -> R) -> R {
    let mut g = KBD.lock();
    f(&mut g)
}
pub(super) static LIVE: AtomicBool = AtomicBool::new(false);
pub(super) static GSI: AtomicU32 = AtomicU32::new(GSI_NONE);
pub(super) static PIC_FALLBACK: AtomicBool = AtomicBool::new(false);

fn kbd_ioapic(_frame: &mut arch::idt::TrapFrame) {
    on_irq();
    apic_init::eoi();
}

fn kbd_pic(_frame: &mut arch::idt::TrapFrame) {
    on_irq();
    // SAFETY: invariant I50 names IRQ1's master 8259 EOI as an exception to
    // `arch::x86_64::pic`'s ownership of port 0x20, and an EOI touches no
    // memory; established by `arch::x86_64::pic::program`.
    unsafe { x86::outb(PIC1_CMD, PIC_EOI) };
}

/// ISR: read 0x60, push, return. No alloc, no log.
pub fn on_irq() {
    // SAFETY: invariant I50: ports 0x60 and 0x64 belong to the 8042's owner,
    // this module, and a port read or write touches no memory; established by
    // `kbd_init::init`.
    let status = unsafe { x86::inb(STATUS) };
    if status & STAT_OBF == 0 {
        return;
    }
    // SAFETY: as for the status read above (`kbd_init::init`).
    let data = unsafe { x86::inb(DATA) };
    if status & STAT_MOUSE != 0 {
        return;
    }
    with_kbd(|k| {
        if let Some(key) = k.decoder.feed(data) {
            k.ring.push(key);
        }
    });
}

pub fn pop() -> Option<DecodedKey> {
    with_kbd(|k| k.ring.pop())
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
            // Release: pairs with the Acquire load in `console::ktest::hooks::gsi`.
            GSI.store(gsi, Ordering::Release);
        }
        None if !apic_init::owns_tick() => {
            // PIT path: 8259 still live via LINT0 ExtINT.
            arch::pic::unmask(1);
            // Release: pairs with the Acquire load in `console::ktest::hooks::pic_fallback`.
            PIC_FALLBACK.store(true, Ordering::Release);
        }
        None => {
            // LAPIC owns the tick: PIC is masked. Unmasking IRQ1 is a
            // silent no-op (window PS/2 dead, polled COM1 still works).
            crate::marker!("vibeOS: kbd: no ioapic route");
        }
    }
    // Release: pairs with the Acquire load in `console::ktest::hooks::kbd_live`.
    LIVE.store(true, Ordering::Release);
    true
}

fn route_keyboard() -> Option<u32> {
    let desc = machine_init::info()?;
    let isos = desc.irq_overrides();
    let gsi = apic::gsi_for_isa_irq(1, isos);
    let (trig, pol) = iso_irq1(isos, gsi);
    // Relaxed: set before the CPU starts, fixed while it runs; pairs with nothing.
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
        // SAFETY: invariant I50: ports 0x60 and 0x64 belong to the 8042's owner,
        // this module, and a port read or write touches no memory; established by
        // `kbd_init::init`.
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
        // SAFETY: invariant I50: ports 0x60 and 0x64 belong to the 8042's owner,
        // this module, and a port read or write touches no memory; established by
        // `kbd_init::init`.
        if unsafe { x86::inb(STATUS) } & STAT_OBF != 0 {
            return true;
        }
        n -= 1;
    }
    false
}

pub(super) fn write_cmd(cmd: u8) -> bool {
    if !wait_ibf_clear() {
        return false;
    }
    // SAFETY: invariant I50: ports 0x60 and 0x64 belong to the 8042's owner,
    // this module, and a port read or write touches no memory; established by
    // `kbd_init::init`.
    unsafe { x86::outb(kbd::CMD, cmd) };
    true
}

pub(super) fn write_data(data: u8) -> bool {
    if !wait_ibf_clear() {
        return false;
    }
    // SAFETY: invariant I50: ports 0x60 and 0x64 belong to the 8042's owner,
    // this module, and a port read or write touches no memory; established by
    // `kbd_init::init`.
    unsafe { x86::outb(DATA, data) };
    true
}

pub(super) fn read_data() -> Option<u8> {
    if !wait_obf() {
        return None;
    }
    // SAFETY: invariant I50: ports 0x60 and 0x64 belong to the 8042's owner,
    // this module, and a port read or write touches no memory; established by
    // `kbd_init::init`.
    Some(unsafe { x86::inb(DATA) })
}

pub(super) fn flush_obf() {
    let mut n = 16u32;
    // SAFETY: invariant I50: ports 0x60 and 0x64 belong to this module, and
    // a port read touches no memory; established by `kbd_init::init`. The
    // drained byte is dropped by design.
    while n > 0 && unsafe { x86::inb(STATUS) } & STAT_OBF != 0 {
        // SAFETY: as for the status read above (`kbd_init::init`).
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
