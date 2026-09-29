//! Kernel shell thread. ROADMAP §5.4.
//!
//! Runs as a real thread (not `_start`, not an ISR, not idle). Input
//! drain is IRQ-off; we never wait for keys while holding a console lock
//! with IF=1 (DESIGN §9.4). Commands live in the registry table.
#![cfg_attr(feature = "vibefs_crash", allow(dead_code))]

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(all(not(feature = "kernel_tests"), feature = "kernel_shell"))]
use vibeos::log::Level;
use vibeos::marker;
use vibeos::shell::{
    Command, Feed, LINE_CAP, LineEditor, MAX_COMMANDS, MAX_TOKENS, PROMPT, Registry,
};

use super::cmds;
use crate::cell::IrqCell;
use crate::console_init::{self, Console};
use crate::fb_init;

static REG: IrqCell<Registry> = IrqCell::new(Registry::new());
#[cfg_attr(
    not(all(not(feature = "kernel_tests"), feature = "kernel_shell")),
    allow(dead_code)
)] // parked REPL; production is userspace /bin/sh
static READY: AtomicBool = AtomicBool::new(false);

fn with_reg<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    REG.with(f)
}

/// Subsystems register here. Not a growing `match` on the name.
pub fn register(cmd: Command) -> bool {
    with_reg(|r| r.register(cmd))
}

#[allow(dead_code)] // parked #66; userspace /bin/sh is the shell
pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn command_count() -> usize {
    with_reg(|r| r.len())
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn has_command(name: &str) -> bool {
    with_reg(|r| r.lookup(name).is_some())
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

#[cfg_attr(
    not(all(not(feature = "kernel_tests"), feature = "kernel_shell")),
    allow(dead_code)
)]
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
    READY.store(true, Ordering::Release);
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
                    let _ = dispatch_line(line);
                }
                ed.clear();
                write_prompt(&mut painted);
            }
        }
    }
}

/// The name of the `i`th registered command, for tab completion.
#[cfg_attr(
    not(all(not(feature = "kernel_tests"), feature = "kernel_shell")),
    allow(dead_code)
)]
fn command_at(i: usize) -> Option<&'static str> {
    with_reg(|r| r.get(i)).map(|c| c.name)
}

#[cfg_attr(
    not(all(not(feature = "kernel_tests"), feature = "kernel_shell")),
    allow(dead_code)
)]
fn write_prompt(painted: &mut usize) {
    console_init::write(PROMPT.as_bytes());
    *painted = PROMPT.len();
}

#[cfg_attr(
    not(all(not(feature = "kernel_tests"), feature = "kernel_shell")),
    allow(dead_code)
)]
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

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn dispatch_line(line: &str) -> Result<(), &'static str> {
    let mut toks = [""; MAX_TOKENS];
    let n = match vibeos::shell::tokenize(line, &mut toks) {
        Ok(n) => n,
        Err(e) => {
            let _ = writeln!(Console, "vibeOS: shell: {}", e.as_str());
            return Err(e.as_str());
        }
    };
    if n == 0 {
        return Ok(());
    }
    let Some(cmd) = with_reg(|r| r.lookup(toks[0])) else {
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
        let _ = writeln!(Console, "vibeOS: help: {} - {}", c.name, c.help);
        i += 1;
    }
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn builtin_names() -> [&'static str; 10] {
    [
        "help", "echo", "meminfo", "uptime", "cpus", "dmesg", "ps", "panic", "reboot", "poweroff",
    ]
}

const _: () = {
    assert!(LINE_CAP > 0);
};
