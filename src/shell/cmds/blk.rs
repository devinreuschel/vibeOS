//! `blk`: the block devices, their partitions and the cache (ROADMAP §7.1).

use core::fmt::Write;

use vibeos::shell::Command;

use crate::block_init::{self, RAM0_BLOCK_SIZE, RAM0_NAME, RAM0_SECTORS};
use crate::console_init::Console;

pub(crate) const COMMANDS: &[Command] = &[Command {
    name: "blk",
    help: "block devices",
    run: cmd_blk,
}];

fn cmd_blk(_args: &[&str]) {
    let st = block_init::state().as_str();
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
    )]
    let _ = writeln!(
        Console,
        "vibeOS: blk: {} {} {} sectors {st} io {}",
        RAM0_NAME,
        RAM0_BLOCK_SIZE,
        RAM0_SECTORS,
        block_init::io_reqs()
    );
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
    )]
    let _ = crate::virtio_blk_init::shell_line(&mut Console);
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
    )]
    let _ = crate::part_init::shell_lines(&mut Console);
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
    )]
    let _ = crate::cache_init::shell_line(&mut Console);
}
