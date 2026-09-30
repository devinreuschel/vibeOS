//! `lspci` and `devices`: the device registry (ROADMAP §6.1).

use core::fmt::Write;

use vibeos::dev::DevRef;
use vibeos::pci::MAX_BARS;
use vibeos::shell::Command;

use crate::console_init::Console;
use crate::dev_init;

pub(crate) const COMMANDS: &[Command] = &[
    Command {
        name: "lspci",
        help: "pci devices",
        run: cmd_lspci,
    },
    Command {
        name: "devices",
        help: "device tree",
        run: cmd_devices,
    },
];

/// One device at a time, each printed from its reference with RANK_DEVICE
/// dropped, before the FB print.
fn cmd_lspci(_args: &[&str]) {
    let mut i = 0usize;
    while let Some(d) = dev_init::get(i) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
        )]
        let _ = d.write_lspci(&mut Console);
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
        )]
        let _ = writeln!(Console);
        i += 1;
    }
}

fn cmd_devices(_args: &[&str]) {
    let mut i = 0usize;
    while let Some(d) = dev_init::get(i) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
        )]
        let _ = d.write_tree(dev_init::bound(&d), &claimed(&d), &mut Console);
        i += 1;
    }
}

/// Which of `d`'s BARs are claimed.
fn claimed(d: &DevRef) -> [bool; MAX_BARS] {
    core::array::from_fn(|b| dev_init::is_claimed(d, b as u8))
}
