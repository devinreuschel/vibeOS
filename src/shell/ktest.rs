//! In-guest tests for shell (kernel_tests only). Rows: the list in crate::ktest.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::ktest::Outcome;
use crate::shell_init;
use crate::thread_init;
use crate::time_init;

/// Registered commands.
pub(crate) fn command_count() -> usize {
    shell_init::with_reg(|r| r.len())
}

/// `name` is a registered command.
pub(crate) fn has_command(name: &str) -> bool {
    shell_init::with_reg(|r| r.lookup(name).is_some())
}

/// The builtins `shell_init::init` registers.
pub(crate) fn builtin_names() -> [&'static str; 10] {
    [
        "help", "echo", "meminfo", "uptime", "cpus", "dmesg", "ps", "panic", "reboot", "poweroff",
    ]
}

pub(crate) fn test_shell_registry() -> Outcome {
    for name in builtin_names() {
        if !has_command(name) {
            return Outcome::Fail("missing builtin");
        }
    }
    if command_count() < 10 {
        return Outcome::Fail("registry short");
    }
    if has_command("not-a-cmd") {
        return Outcome::Fail("unknown present");
    }
    Outcome::Ok
}

pub(crate) fn test_shell_dispatch() -> Outcome {
    if crate::shell_init::dispatch_line("echo ktest-shell-echo").is_err() {
        return Outcome::Fail("echo");
    }
    if crate::shell_init::dispatch_line("").is_err() {
        return Outcome::Fail("empty");
    }
    if crate::shell_init::dispatch_line("not-a-cmd").is_ok() {
        return Outcome::Fail("unknown succeeded");
    }
    if crate::shell_init::dispatch_line("dmesg info").is_err() {
        return Outcome::Fail("dmesg");
    }
    Outcome::Ok
}

pub(crate) fn test_shell_dmesg_level() -> Outcome {
    use vibeos::log::Level;
    let old = crate::log_init::max_level();
    if crate::shell_init::dispatch_line("dmesg -n error").is_err() {
        crate::log_init::set_max_level(old);
        return Outcome::Fail("dmesg -n");
    }
    crate::klog!(Level::Debug, "vibeOS: ktest: shell-level-hidden");
    if crate::log_init::contains_msg("shell-level-hidden") {
        crate::log_init::set_max_level(old);
        return Outcome::Fail("debug stored at error");
    }
    if crate::shell_init::dispatch_line("dmesg -n trace").is_err() {
        crate::log_init::set_max_level(old);
        return Outcome::Fail("dmesg -n trace");
    }
    crate::klog!(Level::Debug, "vibeOS: ktest: shell-level-visible");
    let ok = crate::log_init::contains_msg("shell-level-visible");
    crate::log_init::set_max_level(old);
    if ok {
        Outcome::Ok
    } else {
        Outcome::Fail("debug missing after -n trace")
    }
}

static LSPCI_STACK: AtomicU32 = AtomicU32::new(0);

/// How long [`test_lspci_cmd`] yields for its worker.
const LSPCI_WAIT_NS: u64 = 2_000_000_000;

fn lspci_stack_entry() {
    let ok = crate::shell_init::dispatch_line("lspci").is_ok()
        && crate::shell_init::dispatch_line("devices").is_ok();
    LSPCI_STACK.store(if ok { 1 } else { 2 }, Ordering::SeqCst);
}

pub(crate) fn test_lspci_cmd() -> Outcome {
    if !has_command("lspci") {
        return Outcome::Fail("no lspci");
    }
    if !has_command("devices") {
        return Outcome::Fail("no devices");
    }
    // Shell stacks are 16 KiB. lspci on _start would miss a full
    // [Device; 64] snapshot overflowing the guard.
    LSPCI_STACK.store(0, Ordering::SeqCst);
    let Ok(h) = thread_init::spawn_here("lspci-stk", lspci_stack_entry) else {
        return Outcome::Fail("spawn");
    };
    thread_init::switch_to(h.id());
    // The worker runs with IF on, so a tick can hand the CPU back first.
    let t0 = time_init::now_ns();
    while LSPCI_STACK.load(Ordering::SeqCst) == 0
        && time_init::now_ns().saturating_sub(t0) < LSPCI_WAIT_NS
    {
        thread_init::yield_now();
    }
    match LSPCI_STACK.load(Ordering::SeqCst) {
        1 => Outcome::Ok,
        2 => Outcome::Fail("lspci/devices"),
        _ => Outcome::Fail("did not run"),
    }
}
