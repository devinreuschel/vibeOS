//! In-guest tests of P10-S21, One user frame and ring-3 traps to signals (DESIGN §8.2).

use super::user::{self, DEFAULT, Image, user_code};
use super::{Outcome, Test, test};
use crate::proc_init::testing as proc_testing;

pub(super) const TESTS: &[Test] =
    &[test("syscall_rcx_canary", test_syscall_rcx_canary).deadline(30_000)];

/// What `arm_rcx_canary` puts in the frame's `rcx`: not the return RIP.
const RCX_CANARY: u64 = 0x0C0F_FEE0_DEAD_BEEF;

// getpid with rdi = proc_init::testing::HOOK_MAGIC; exit(0) when RCX holds
// RCX_CANARY after the call, else exit(1).
user_code!(
    RCX_CANARY_PROG,
    "
    movabs rdi, 0x5EEDCA1100005A21
    mov eax, 39
    syscall
    movabs rdx, 0x0C0FFEE0DEADBEEF
    xor edi, edi
    cmp rcx, rdx
    setne dil
    mov eax, 60
    syscall
    ud2
    "
);

const _: () = assert!(proc_testing::HOOK_MAGIC == 0x5EED_CA11_0000_5A21);

/// A hook sets a `getpid`'s saved `rcx` to a canary that is not its
/// return RIP; the exit then leaves through `iretq`, and the program finds
/// the canary in RCX.
fn test_syscall_rcx_canary() -> Outcome {
    proc_testing::arm_rcx_canary(RCX_CANARY);
    let st = user::run(&Image::Code(RCX_CANARY_PROG, DEFAULT), &["rcx_canary"]);
    proc_testing::disarm();
    match st {
        Ok(0) => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("status {st:#x}, want 0"),
        Err(e) => crate::fail_fmt!("spawn: {}", e.as_str()),
    }
}
