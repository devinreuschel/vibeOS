//! In-guest tests of P10-S17, Syscall exit IF=0, enter_user, USER_MAP_END and the FP binding (DESIGN §8.2).

use vibeos::kbd::DecodedKey;

use super::user::{self, DEFAULT, Image, user_code};
use super::{Outcome, Test, test};
use crate::console_init;
use crate::kbd_init;
use crate::thread_init;
use crate::time_init;

pub(super) const TESTS: &[Test] =
    &[test("console_read_exit", test_console_read_exit).deadline(30_000)];

/// Sleep until `pred` holds, for at most `ms`.
fn sleep_until(pred: impl Fn() -> bool, ms: u64) -> bool {
    let deadline = time_init::now_ns().saturating_add(ms.saturating_mul(1_000_000));
    while !pred() {
        if time_init::now_ns() >= deadline {
            return false;
        }
        thread_init::sleep_ms(1);
    }
    true
}

// read(0, rsp, 1), then exit with the byte read; exit(2) if read does not
// return 1.
user_code!(
    READ_ONE_KEY,
    "
    sub rsp, 16
    xor edi, edi
    mov rsi, rsp
    mov edx, 1
    xor eax, eax
    syscall
    cmp rax, 1
    jne 1f
    movzx edi, byte ptr [rsp]
    mov eax, 60
    syscall
1:
    mov edi, 2
    mov eax, 60
    syscall
    ud2
    "
);

/// A user `read` on fd 0 blocks in `console_init::wait_key`'s halt branch
/// until a key is queued, then returns through the syscall exit, whose
/// debug-build check faults if `wait_key` left IF set.
fn test_console_read_exit() -> Outcome {
    console_init::testing::reset_halts();
    let pid = match user::spawn(&Image::Code(READ_ONE_KEY, DEFAULT), &["read_one_key"]) {
        Ok(pid) => pid,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    let halted = sleep_until(|| console_init::testing::halts() != 0, 5_000);
    kbd_init::push_for_test(DecodedKey::Char(b'k'));
    let st = user::wait(pid);
    if !halted {
        return Outcome::Fail("reader never reached wait_key's halt");
    }
    if st != 0x6B << 8 {
        return crate::fail_fmt!("status {st:#x}, want {:#x}", 0x6B << 8);
    }
    Outcome::Ok
}
