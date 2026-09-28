//! GDT / TSS / IDT descriptor encoding. No `lgdt`/`lidt` here.
//!
//! Selectors are laid out for `syscall`/`sysret` (DESIGN §5.1, ROADMAP §2.1):
//! `STAR.SYSCALL_CS = KERNEL_CS` so kernel SS is CS+8, and
//! `STAR.SYSRET_CS = KERNEL_DS` so user SS is +8 and user CS is +16.
//! That is why user *data* sits at `0x18` and user *code* at `0x20`.

#[cfg(test)]
use core::mem::offset_of;
use core::mem::size_of;

/// CPU-pushed iret frame: the hardware tail of `arch::idt::TrapFrame`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct InterruptFrame {
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// IST slots in the TSS `ist` array. Software is zero-based; the IDT
/// IST field is one-based. Getting this wrong is a triple fault
/// (DESIGN §5.1, §9.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IstSlot {
    DoubleFault,
    Nmi,
    MachineCheck,
    Debug,
}

impl IstSlot {
    pub const fn index(self) -> usize {
        match self {
            Self::DoubleFault => 0,
            Self::Nmi => 1,
            Self::MachineCheck => 2,
            Self::Debug => 3,
        }
    }

    /// Hardware IST field in an IDT gate. 1..=7.
    pub const fn hardware(self) -> u8 {
        (self.index() as u8) + 1
    }

    pub const fn from_index(i: usize) -> Option<Self> {
        match i {
            0 => Some(Self::DoubleFault),
            1 => Some(Self::Nmi),
            2 => Some(Self::MachineCheck),
            3 => Some(Self::Debug),
            _ => None,
        }
    }
}

pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_DS: u16 = 0x10;
pub const USER_DS: u16 = 0x18;
pub const USER_CS: u16 = 0x20;
pub const TSS_SEL: u16 = 0x28;

/// RPL=3 forms, used once ring 3 exists.
pub const USER_DS_RPL: u16 = USER_DS | 3;
pub const USER_CS_RPL: u16 = USER_CS | 3;

/// `IA32_STAR[63:48]` that makes SYSRET land on [`USER_DS`] / [`USER_CS`].
pub const STAR_SYSRET: u16 = KERNEL_DS;

pub const GDT_ENTRIES: usize = 7;
pub const GDT_LIMIT: u16 = (GDT_ENTRIES * 8 - 1) as u16;

#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct Gdt {
    pub entries: [u64; GDT_ENTRIES],
}

impl Gdt {
    pub const fn empty() -> Self {
        Self {
            entries: [0; GDT_ENTRIES],
        }
    }

    pub fn with_tss(tss_base: u64, tss_limit: u16) -> Self {
        let mut g = Self::empty();
        g.entries[1] = code64(0);
        g.entries[2] = data(0);
        g.entries[3] = data(3);
        g.entries[4] = code64(3);
        let tss = tss_desc(tss_base, tss_limit as u64);
        g.entries[5] = tss[0];
        g.entries[6] = tss[1];
        g
    }
}

/// 64-bit TSS. Packed: RSP0 sits at offset 4 with no padding (SDM).
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Tss {
    reserved0: u32,
    pub rsp: [u64; 3],
    reserved1: u64,
    pub ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    pub iomap_base: u16,
}

impl Tss {
    pub const fn empty() -> Self {
        Self {
            reserved0: 0,
            rsp: [0; 3],
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            iomap_base: size_of::<Self>() as u16,
        }
    }

    pub fn set_rsp0(&mut self, top: u64) {
        self.rsp[0] = top;
    }

    pub fn set_ist(&mut self, slot: IstSlot, top: u64) {
        self.ist[slot.index()] = top;
    }
}

/// 16-byte interrupt gate. `ist` is the hardware 1-based field, 0 = none;
/// `dpl` is the lowest CPL that may raise the vector with `int n`.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct IdtEntry {
    off_lo: u16,
    selector: u16,
    ist_type: u16,
    off_mid: u16,
    off_hi: u32,
    zero: u32,
}

impl IdtEntry {
    pub const EMPTY: Self = Self {
        off_lo: 0,
        selector: 0,
        ist_type: 0,
        off_mid: 0,
        off_hi: 0,
        zero: 0,
    };

    pub const fn interrupt(handler: u64, cs: u16, ist: u8, dpl: u8) -> Self {
        Self {
            off_lo: handler as u16,
            selector: cs,
            ist_type: 0x8E00 | ((dpl as u16 & 3) << 13) | (ist as u16 & 7),
            off_mid: (handler >> 16) as u16,
            off_hi: (handler >> 32) as u32,
            zero: 0,
        }
    }

    pub const fn handler(self) -> u64 {
        self.off_lo as u64 | ((self.off_mid as u64) << 16) | ((self.off_hi as u64) << 32)
    }

    pub const fn ist(self) -> u8 {
        (self.ist_type & 7) as u8
    }

    pub const fn dpl(self) -> u8 {
        ((self.ist_type >> 13) & 3) as u8
    }

    /// Present 64-bit interrupt gate at any DPL (type byte 0x8E or 0xEE).
    pub const fn present_interrupt(self) -> bool {
        (self.ist_type >> 8) & 0x9F == 0x8E
    }
}

pub const fn code64(dpl: u8) -> u64 {
    // P=1, S=1, code, readable. L=1, D=0.
    pack(0, 0, 0x9A | ((dpl & 3) << 5), 0x2)
}

pub const fn data(dpl: u8) -> u64 {
    pack(0, 0xF_FFFF, 0x92 | ((dpl & 3) << 5), 0xC)
}

pub const fn tss_desc(base: u64, limit: u64) -> [u64; 2] {
    let low = pack(base & 0xFFFF_FFFF, limit, 0x89, 0);
    let high = base >> 32;
    [low, high]
}

const fn pack(base: u64, limit: u64, access: u8, flags: u8) -> u64 {
    let limit_lo = limit & 0xFFFF;
    let limit_hi = (limit >> 16) & 0xF;
    let base_lo = base & 0xFFFF;
    let base_mid = (base >> 16) & 0xFF;
    let base_hi = (base >> 24) & 0xFF;
    limit_lo
        | (base_lo << 16)
        | (base_mid << 32)
        | ((access as u64) << 40)
        | (limit_hi << 48)
        | ((flags as u64) << 52)
        | (base_hi << 56)
}

pub const fn access_byte(desc: u64) -> u8 {
    (desc >> 40) as u8
}

pub const fn flags_nibble(desc: u64) -> u8 {
    ((desc >> 52) & 0xF) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysret_selector_order() {
        assert_eq!(KERNEL_DS, KERNEL_CS + 8);
        assert_eq!(USER_DS, STAR_SYSRET + 8);
        assert_eq!(USER_CS, STAR_SYSRET + 16);
        assert_eq!(TSS_SEL, 0x28);
        assert_eq!(USER_CS_RPL, 0x23);
        assert_eq!(USER_DS_RPL, 0x1B);
    }

    #[test]
    fn kernel_code_is_long_mode() {
        let d = code64(0);
        assert_eq!(access_byte(d), 0x9A);
        assert_eq!(flags_nibble(d) & 0b0110, 0b0010, "L=1 D=0");
        let u = code64(3);
        assert_eq!(access_byte(u), 0xFA);
        assert_eq!(flags_nibble(u) & 0b0010, 0b0010);
    }

    #[test]
    fn user_data_dpl3() {
        assert_eq!(access_byte(data(3)), 0xF2);
        assert_eq!(access_byte(data(0)), 0x92);
    }

    #[test]
    fn tss_layout_matches_sdm() {
        assert_eq!(size_of::<Tss>(), 104);
        assert_eq!(offset_of!(Tss, rsp), 4);
        assert_eq!(offset_of!(Tss, ist), 36);
        assert_eq!(offset_of!(Tss, iomap_base), 102);
        let mut t = Tss::empty();
        let iomap = t.iomap_base;
        assert_eq!(iomap, 104);
        t.set_rsp0(0x1000);
        t.set_ist(IstSlot::DoubleFault, 0x2000);
        let rsp0 = t.rsp[0];
        let ist1 = t.ist[0];
        assert_eq!(rsp0, 0x1000);
        assert_eq!(ist1, 0x2000);
        assert_eq!(size_of::<InterruptFrame>(), 40);
    }

    #[test]
    fn ist_software_zero_hardware_one() {
        assert_eq!(IstSlot::DoubleFault.index(), 0);
        assert_eq!(IstSlot::DoubleFault.hardware(), 1);
        assert_eq!(IstSlot::Nmi.hardware(), 2);
        assert_eq!(IstSlot::MachineCheck.hardware(), 3);
        assert_eq!(IstSlot::Debug.hardware(), 4);
        assert_eq!(IstSlot::from_index(4), None);
        let mut seen = [false; 4];
        for slot in [
            IstSlot::DoubleFault,
            IstSlot::Nmi,
            IstSlot::MachineCheck,
            IstSlot::Debug,
        ] {
            let i = slot.index();
            assert!(!seen[i]);
            seen[i] = true;
            assert_eq!(IstSlot::from_index(i), Some(slot));
        }
    }

    #[test]
    fn tss_descriptor_is_16_bytes_and_present() {
        let [lo, hi] = tss_desc(0xFFFF_8000_1234_5000, 103);
        assert_eq!(access_byte(lo), 0x89);
        assert_eq!(hi, 0xFFFF_8000);
        let g = Gdt::with_tss(0x1000, 103);
        assert_eq!(g.entries[0], 0);
        assert_eq!(g.entries[5], tss_desc(0x1000, 103)[0]);
        assert_eq!(g.entries[6], tss_desc(0x1000, 103)[1]);
    }

    #[test]
    fn idt_gate_packs_offset_and_ist() {
        let e = IdtEntry::interrupt(0xFFFF_8000_1234_5678, KERNEL_CS, 1, 0);
        assert_eq!(e.handler(), 0xFFFF_8000_1234_5678);
        assert_eq!(e.ist(), 1);
        assert_eq!(e.dpl(), 0);
        assert!(e.present_interrupt());
        let raw = e.ist_type;
        assert_eq!(raw, 0x8E01);
        let none = IdtEntry::interrupt(0x1000, KERNEL_CS, 0, 0);
        assert_eq!(none.ist(), 0);
        assert!(!IdtEntry::EMPTY.present_interrupt());
    }

    #[test]
    fn idt_gate_dpl3_is_type_ee() {
        let e = IdtEntry::interrupt(0xFFFF_8000_0000_1000, KERNEL_CS, 0, 3);
        let raw = e.ist_type;
        assert_eq!(raw >> 8, 0xEE);
        assert_eq!(e.dpl(), 3);
        assert_eq!(e.ist(), 0);
        assert!(e.present_interrupt());
        assert_eq!(e.handler(), 0xFFFF_8000_0000_1000);
    }
}
