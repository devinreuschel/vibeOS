//! Kernel shell thread. ROADMAP §5.4.
//!
//! Runs as a real thread (not `_start`, not an ISR, not idle). Input
//! drain is IRQ-off; we never wait for keys while holding a console lock
//! with IF=1 (DESIGN §9.4). Commands live in the registry table.

use core::fmt::Write;

#[cfg(any(feature = "kernel_tests", feature = "kernel_shell"))]
use vibeos::shell::MAX_TOKENS;
use vibeos::shell::{Command, LINE_CAP, MAX_COMMANDS, Registry};
// The REPL's: `kernel_shell` builds that are not `kernel_tests` builds.
#[cfg(all(feature = "kernel_shell", not(feature = "kernel_tests")))]
use {
    crate::console_init,
    crate::fb_init,
    vibeos::log::Level,
    vibeos::marker,
    vibeos::shell::{Feed, LineEditor, PROMPT},
};

use super::cmds;
use crate::cell::IrqCell;
use crate::console_init::Console;

static REG: IrqCell<Registry> = IrqCell::new(Registry::new());

pub(super) fn with_reg<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    REG.with(f)
}

/// `name` is a registered command. `kernel_tests` only (AGENTS.md rule 9).
#[cfg(feature = "kernel_tests")]
pub(crate) fn has_command(name: &str) -> bool {
    with_reg(|r| r.lookup(name).is_some())
}

/// Subsystems register here. Not a growing `match` on the name.
pub fn register(cmd: Command) -> bool {
    with_reg(|r| r.register(cmd))
}

/// Register builtins. Production shell is a user process (`/bin/sh`).
/// The kernel REPL is debug-only (`kernel_shell`).
pub fn init() {
    register_builtins();
    #[cfg(all(not(feature = "kernel_tests"), feature = "kernel_shell"))]
    {
        if let Err(e) = crate::thread_init::spawn_on("shell", shell_main, 0) {
            crate::klog!(Level::Error, "shell: spawn failed: {}", e.as_str());
        }
    }
}

/// Register the commands in `help` order: the device, block and file
/// commands, then `help`, then the system commands.
fn register_builtins() {
    for c in cmds::dev::COMMANDS
        .iter()
        .chain(cmds::blk::COMMANDS)
        .chain(cmds::fs::COMMANDS)
        .chain(HELP)
        .chain(cmds::sys::COMMANDS)
    {
        let _ = register(*c);
    }
}

const HELP: &[Command] = &[Command {
    name: "help",
    help: "list commands",
    run: cmd_help,
}];

#[cfg(all(feature = "kernel_shell", not(feature = "kernel_tests")))]
fn shell_main() {
    let mut ed = LineEditor::new();
    let mut painted = 0usize;
    // Last boot marker, then the prompt. Harness treats this as the
    // trailing contract line (after smp: done / console ok).
    crate::marker!(marker::SHELL_READY);
    if fb_init::ready() {
        fb_init::write(marker::SHELL_READY.as_bytes());
        fb_init::write(b"\n");
    }
    write_prompt(&mut painted);
    loop {
        let k = console_init::wait_key();
        match ed.feed(k) {
            Feed::Pending => paint(&ed, &mut painted),
            Feed::Complete => {
                super::complete::complete_line(&mut ed, command_at);
                paint(&ed, &mut painted);
            }
            Feed::Cancel => {
                console_init::write(b"^C\n");
                ed.clear();
                painted = 0;
                write_prompt(&mut painted);
            }
            Feed::Submit => {
                console_init::write(b"\n");
                painted = 0;
                if let Ok(line) = ed.line_str() {
                    #[expect(
                        clippy::let_underscore_must_use,
                        reason = "`dispatch_line` printed its error on the console already: nothing left to act on (DESIGN §2.5)"
                    )]
                    let _ = dispatch_line(line);
                }
                ed.clear();
                write_prompt(&mut painted);
            }
        }
    }
}

/// The name of the `i`th registered command, for tab completion.
#[cfg(all(feature = "kernel_shell", not(feature = "kernel_tests")))]
fn command_at(i: usize) -> Option<&'static str> {
    with_reg(|r| r.get(i)).map(|c| c.name)
}

#[cfg(all(feature = "kernel_shell", not(feature = "kernel_tests")))]
fn write_prompt(painted: &mut usize) {
    console_init::write(PROMPT.as_bytes());
    *painted = PROMPT.len();
}

#[cfg(all(feature = "kernel_shell", not(feature = "kernel_tests")))]
fn paint(ed: &LineEditor, painted: &mut usize) {
    // `\r` homes serial and FB (column 0, same row). Do not use `\n`.
    console_init::write(b"\r");
    console_init::write(PROMPT.as_bytes());
    console_init::write(ed.line());
    let shown = PROMPT.len() + ed.len();
    if *painted > shown {
        let mut n = *painted - shown;
        while n > 0 {
            console_init::write(b" ");
            n -= 1;
        }
    }
    *painted = shown;
    console_init::write(b"\r");
    console_init::write(PROMPT.as_bytes());
    console_init::write(&ed.line()[..ed.cursor()]);
}

/// Run one command line: the REPL's and the in-guest tests'.
#[cfg(any(feature = "kernel_tests", feature = "kernel_shell"))]
pub fn dispatch_line(line: &str) -> Result<(), &'static str> {
    let mut toks = [""; MAX_TOKENS];
    let n = match vibeos::shell::tokenize(line, &mut toks) {
        Ok(n) => n,
        Err(e) => {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
            )]
            let _ = writeln!(Console, "vibeOS: shell: {}", e.as_str());
            return Err(e.as_str());
        }
    };
    if n == 0 {
        return Ok(());
    }
    let Some(cmd) = with_reg(|r| r.lookup(toks[0])) else {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
        )]
        let _ = writeln!(Console, "vibeOS: shell: unknown: {}", toks[0]);
        return Err("unknown");
    };
    (cmd.run)(&toks[..n]);
    Ok(())
}

fn cmd_help(_args: &[&str]) {
    let n = with_reg(|r| r.len());
    let mut i = 0usize;
    while i < n && i < MAX_COMMANDS {
        let Some(c) = with_reg(|r| r.get(i)) else {
            break;
        };
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
        )]
        let _ = writeln!(Console, "vibeOS: help: {} - {}", c.name, c.help);
        i += 1;
    }
}

const _: () = {
    assert!(LINE_CAP > 0);
};
