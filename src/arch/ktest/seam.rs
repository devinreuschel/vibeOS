//! In-guest test of the x86_64 port's seam impls (kernel_tests only).
//! Rows: the parent `ktest.rs`'s `TESTS`.

use crate::ktest::Outcome;
use crate::per_cpu_init;
use crate::time_init;

/// The x86_64 port's first seam impls, through `arch::current::Arch`
/// (ROADMAP §10.3, PORTABILITY §11.1).
pub(crate) fn test_arch_seam_core() -> Outcome {
    use vibeos::arch::x86_64::trap::{Abi, UserFrame};
    use vibeos::arch::{CycleCounter, InterruptMask, PerCpuBase, SyscallAbi};

    use crate::arch::current::Arch;

    if !Arch::enabled() {
        return Outcome::Fail("registry runs with IF off");
    }
    let outer = Arch::save_disable();
    let inner = Arch::save_disable();
    Arch::restore(inner);
    let after_inner = Arch::enabled();
    let id = Arch::cpu_id();
    let want_id = per_cpu_init::current().cpu_id;
    Arch::restore(outer);
    if after_inner {
        return Outcome::Fail("IF on after the inner restore");
    }
    if !Arch::enabled() {
        return Outcome::Fail("IF off after the outer restore");
    }
    if id != want_id {
        return crate::fail_fmt!("cpu_id {} != PerCpu cpu_id {}", id, want_id);
    }

    let want_hz = time_init::tsc_per_ms().checked_mul(1000);
    let hz = Arch::freq_hz();
    if hz != want_hz || hz.is_none() {
        return crate::fail_fmt!("freq_hz {:?} != tsc_per_ms * 1000 {:?}", hz, want_hz);
    }
    let hz = hz.unwrap_or(0);
    let t0 = Arch::now();
    time_init::busy_wait_ms(1);
    let t1 = Arch::now();
    let d = t1.wrapping_sub(t0);
    if d < hz / 2000 {
        return crate::fail_fmt!("now advanced {} over 1 ms, want >= {}", d, hz / 2000);
    }

    let mut f = UserFrame::default();
    Arch::set_ip(&mut f, 0x40_1000);
    if Arch::ip(&f) != Abi::ip(&f) || Abi::ip(&f) != 0x40_1000 {
        return crate::fail_fmt!("ip {:#x} != Abi {:#x}", Arch::ip(&f), Abi::ip(&f));
    }
    Abi::set_ip(&mut f, 0x40_2000);
    if Arch::ip(&f) != 0x40_2000 {
        return crate::fail_fmt!("ip {:#x} after Abi::set_ip", Arch::ip(&f));
    }
    Outcome::Ok
}
