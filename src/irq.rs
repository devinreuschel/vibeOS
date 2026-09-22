//! Device IRQ vector pool and MSI/MSI-X message encoding. DESIGN §5.3–5.4.
//!
//! Drivers ask [`VectorPool::allocate`] (kernel: `irq::allocate_vector`).
//! They never pick IDT slots. CPU binding is recorded so MSI-X dest and
//! Phase 19 affinity rebalance share one table.

use crate::vectors;

/// Inclusive pool. Keyboard already took [`vectors::KBD`] (`0x30`).
pub const POOL_START: u8 = 0x30;
pub const POOL_END: u8 = 0x7F;
pub const POOL_LEN: usize = (POOL_END - POOL_START) as usize + 1;

/// Fee00000h + (APIC id << 12). Physical dest, no RH.
pub const MSI_ADDR_BASE: u32 = 0xFEE0_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqError {
    Exhausted,
    InIrq,
    BadVector,
    BadCpu,
    Busy,
    NoRoute,
}

impl IrqError {
    pub fn as_str(self) -> &'static str {
        match self {
            IrqError::Exhausted => "exhausted",
            IrqError::InIrq => "in hard irq",
            IrqError::BadVector => "bad vector",
            IrqError::BadCpu => "bad cpu",
            IrqError::Busy => "busy",
            IrqError::NoRoute => "no route",
        }
    }
}

pub const fn in_pool(vec: u8) -> bool {
    vec >= POOL_START && vec <= POOL_END
}

pub const fn pool_index(vec: u8) -> Option<usize> {
    if in_pool(vec) {
        Some((vec - POOL_START) as usize)
    } else {
        None
    }
}

/// Physical-destination MSI address. DM=0, RH=0.
pub const fn msi_message_addr(apic_id: u8) -> u32 {
    MSI_ADDR_BASE | ((apic_id as u32) << 12)
}

/// Fixed, edge, vector in the low 8 bits.
pub const fn msi_message_data(vector: u8) -> u32 {
    vector as u32
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsixEntry {
    pub addr: u64,
    pub data: u32,
    pub masked: bool,
}

impl MsixEntry {
    pub const fn new(addr: u64, data: u32, masked: bool) -> Self {
        Self { addr, data, masked }
    }

    pub const fn for_lapic(vector: u8, apic_id: u8, masked: bool) -> Self {
        Self::new(
            msi_message_addr(apic_id) as u64,
            msi_message_data(vector),
            masked,
        )
    }

    /// Dwords 0..3 as the table stores them. Control last when unmasking.
    pub const fn to_dwords(self) -> [u32; 4] {
        [
            self.addr as u32,
            (self.addr >> 32) as u32,
            self.data,
            if self.masked { 1 } else { 0 },
        ]
    }

    pub const fn from_dwords(w: [u32; 4]) -> Self {
        Self {
            addr: (w[0] as u64) | ((w[1] as u64) << 32),
            data: w[2],
            masked: w[3] & 1 != 0,
        }
    }
}

pub struct VectorPool {
    used: [u64; 2],
    cpu: [u32; POOL_LEN],
}

impl VectorPool {
    pub const fn new() -> Self {
        Self {
            used: [0; 2],
            cpu: [0; POOL_LEN],
        }
    }

    fn bit(i: usize) -> (usize, u64) {
        (i / 64, 1u64 << (i % 64))
    }

    fn is_used(&self, i: usize) -> bool {
        let (w, b) = Self::bit(i);
        self.used[w] & b != 0
    }

    fn set_used(&mut self, i: usize, on: bool) {
        let (w, b) = Self::bit(i);
        if on {
            self.used[w] |= b;
        } else {
            self.used[w] &= !b;
        }
    }

    pub fn allocate(&mut self, cpu: u32) -> Result<u8, IrqError> {
        let mut i = 0usize;
        while i < POOL_LEN {
            if !self.is_used(i) {
                self.set_used(i, true);
                self.cpu[i] = cpu;
                return Ok(POOL_START + i as u8);
            }
            i += 1;
        }
        Err(IrqError::Exhausted)
    }

    /// Claim a specific slot (keyboard `0x30`).
    pub fn reserve(&mut self, vec: u8, cpu: u32) -> Result<(), IrqError> {
        let Some(i) = pool_index(vec) else {
            return Err(IrqError::BadVector);
        };
        if self.is_used(i) {
            return Err(IrqError::Busy);
        }
        self.set_used(i, true);
        self.cpu[i] = cpu;
        Ok(())
    }

    pub fn free(&mut self, vec: u8) -> Result<(), IrqError> {
        let Some(i) = pool_index(vec) else {
            return Err(IrqError::BadVector);
        };
        if !self.is_used(i) {
            return Err(IrqError::BadVector);
        }
        self.set_used(i, false);
        self.cpu[i] = 0;
        Ok(())
    }

    pub fn cpu_of(&self, vec: u8) -> Option<u32> {
        let i = pool_index(vec)?;
        if self.is_used(i) {
            Some(self.cpu[i])
        } else {
            None
        }
    }

    /// Phase 19 rebalance: record dest CPU. Caller reprograms MSI/IOAPIC.
    pub fn set_affinity(&mut self, vec: u8, cpu: u32) -> Result<(), IrqError> {
        let Some(i) = pool_index(vec) else {
            return Err(IrqError::BadVector);
        };
        if !self.is_used(i) {
            return Err(IrqError::BadVector);
        }
        self.cpu[i] = cpu;
        Ok(())
    }

    pub fn allocated(&self) -> usize {
        self.used[0].count_ones() as usize + self.used[1].count_ones() as usize
    }
}

impl Default for VectorPool {
    fn default() -> Self {
        Self::new()
    }
}

const _: () = {
    assert!(POOL_START == vectors::KBD);
    assert!(POOL_END == vectors::DEVICE_VEC_END);
    assert!(POOL_LEN == 80);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_is_the_design_range() {
        assert!(in_pool(0x30));
        assert!(in_pool(0x7F));
        assert!(!in_pool(0x2F));
        assert!(!in_pool(0x80));
        assert_eq!(pool_index(0x30), Some(0));
        assert_eq!(pool_index(0x31), Some(1));
        assert_eq!(pool_index(0x7F), Some(79));
        assert_eq!(vectors::DEVICE_VEC_START, 0x31);
    }

    #[test]
    fn allocate_records_cpu_and_skips_reserved() {
        let mut p = VectorPool::new();
        p.reserve(vectors::KBD, 0).unwrap();
        assert_eq!(p.allocate(1).unwrap(), 0x31);
        assert_eq!(p.cpu_of(0x31), Some(1));
        assert_eq!(p.cpu_of(vectors::KBD), Some(0));
        p.set_affinity(0x31, 3).unwrap();
        assert_eq!(p.cpu_of(0x31), Some(3));
        p.free(0x31).unwrap();
        assert_eq!(p.cpu_of(0x31), None);
        assert_eq!(p.allocate(0).unwrap(), 0x31);
        assert_eq!(p.reserve(vectors::KBD, 0), Err(IrqError::Busy));
        assert_eq!(p.reserve(0x20, 0), Err(IrqError::BadVector));
        assert_eq!(IrqError::InIrq.as_str(), "in hard irq");
    }

    #[test]
    fn exhausts_the_pool() {
        let mut p = VectorPool::new();
        let mut n = 0u32;
        while p.allocate(n).is_ok() {
            n += 1;
        }
        assert_eq!(n as usize, POOL_LEN);
        assert_eq!(p.allocate(0), Err(IrqError::Exhausted));
        assert_eq!(p.allocated(), POOL_LEN);
        p.free(0x40).unwrap();
        assert_eq!(p.allocate(7).unwrap(), 0x40);
    }

    #[test]
    fn msi_addr_is_fee_plus_apic_id() {
        assert_eq!(msi_message_addr(0), 0xFEE0_0000);
        assert_eq!(msi_message_addr(1), 0xFEE0_1000);
        assert_eq!(msi_message_addr(0x12), 0xFEE1_2000);
        assert_eq!(msi_message_data(0x3A), 0x3A);
        let e = MsixEntry::for_lapic(0x40, 2, true);
        assert_eq!(e.addr, 0xFEE0_2000);
        assert_eq!(e.data, 0x40);
        assert!(e.masked);
        let w = e.to_dwords();
        assert_eq!(w[0], 0xFEE0_2000);
        assert_eq!(w[1], 0);
        assert_eq!(w[2], 0x40);
        assert_eq!(w[3], 1);
        let live = MsixEntry::for_lapic(0x41, 0, false);
        assert_eq!(live.to_dwords()[3], 0);
        assert_eq!(MsixEntry::from_dwords(w), e);
    }
}
