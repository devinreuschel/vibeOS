//! No `swapgs` on aarch64. Facade so shared kernel code keeps one name.

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn from_user(_spsr: u64) -> bool {
    false
}

pub fn force_kernel() {}
