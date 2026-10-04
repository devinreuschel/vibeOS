//! No 8259 on aarch64.

/// # Safety
/// No-op.
pub unsafe fn remap_and_mask() {}

/// # Safety
/// No-op.
pub unsafe fn program() {}

pub fn line_of(_v: u8) -> Option<u8> {
    None
}

pub fn mask(_line: u8) {}
pub fn unmask(_line: u8) {}
pub fn unclaimed(_line: u8) -> bool {
    false
}
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn is_masked(_line: u8) -> bool {
    true
}
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn disable_all() {}
