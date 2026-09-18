//! Kernel shell thread. ROADMAP §5.4.
//!
//! Runs as a real thread (not `_start`, not an ISR, not idle). Input
//! drain is IRQ-off; we never wait for keys while holding a console lock
//! with IF=1 (DESIGN §9.4). Commands live in the registry table.

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::acpi::{Gas, GAS_SYSTEM_IO, GAS_SYSTEM_MEMORY};
use vibeos::kbd::DecodedKey;
use vibeos::log::Level;
use vibeos::marker;
use vibeos::shell::{
    Command, Feed, LineEditor, Registry, LINE_CAP, MAX_COMMANDS, MAX_TOKENS, PROMPT,
};
use vibeos::thread::{ThreadId, ThreadState, MAX_THREADS};

use crate::acpi_init;
use crate::console_init::{self, Console};
use crate::diag;
use crate::fb_init;
use crate::log_init;
use crate::paging_init;
use crate::per_cpu_init;
use crate::serial;
use crate::thread_init::{self, ThreadInfo};
use crate::x86::{self, InterruptGuard};

struct Cell<T>(core::cell::UnsafeCell<T>);
unsafe impl<T> Sync for Cell<T> {}

static REG: Cell<Registry> = Cell(core::cell::UnsafeCell::new(Registry::new()));
static LOCK: AtomicBool = AtomicBool::new(false);
#[cfg_attr(feature = "kernel_tests", allow(dead_code))]
static READY: AtomicBool = AtomicBool::new(false);

fn with_reg<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    let _irq = InterruptGuard::enter();
    while LOCK
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    let r = f(unsafe { &mut *REG.0.get() });
    LOCK.store(false, Ordering::Release);
    r
}

/// Subsystems register here. Not a growing `match` on the name.
pub fn register(cmd: Command) -> bool {
    with_reg(|r| r.register(cmd))
}

#[cfg_attr(feature = "kernel_tests", allow(dead_code))]
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

/// Register builtins, then spawn the shell thread (not in ktest: the
/// interactive loop would sit on the runq under IF-off tests).
pub fn init() {
    register_builtins();
    #[cfg(not(feature = "kernel_tests"))]
    {
        let _ = thread_init::spawn_on("shell", shell_main, 0);
    }
}

fn register_builtins() {
    let cmds = [
        Command {
            name: "help",
            help: "list commands",
            run: cmd_help,
        },
        Command {
            name: "echo",
            help: "print arguments",
            run: cmd_echo,
        },
        Command {
            name: "meminfo",
            help: "memory totals",
            run: cmd_meminfo,
        },
        Command {
            name: "uptime",
            help: "tick ms and tsc us",
            run: cmd_uptime,
        },
        Command {
            name: "cpus",
            help: "per-cpu identity",
            run: cmd_cpus,
        },
        Command {
            name: "dmesg",
            help: "log ring; -n <level>, -f follow",
            run: cmd_dmesg,
        },
        Command {
            name: "ps",
            help: "kernel threads",
            run: cmd_ps,
        },
        Command {
            name: "panic",
            help: "deliberate panic dump",
            run: cmd_panic,
        },
        Command {
            name: "reboot",
            help: "reset the machine",
            run: cmd_reboot,
        },
        Command {
            name: "poweroff",
            help: "acpi / qemu power off",
            run: cmd_poweroff,
        },
    ];
    for c in cmds {
        let _ = register(c);
    }
}

#[cfg_attr(feature = "kernel_tests", allow(dead_code))]
fn shell_main() {
    let mut ed = LineEditor::new();
    let mut painted = 0usize;
    // Last boot marker, then the prompt. Harness treats this as the
    // trailing contract line (after smp: done / console ok).
    serial::line(marker::SHELL_READY);
    if fb_init::ready() {
        fb_init::write(marker::SHELL_READY.as_bytes());
        fb_init::write(b"\n");
    }
    READY.store(true, Ordering::Release);
    write_prompt(&mut painted);
    loop {
        let k = wait_key();
        match ed.feed(k) {
            Feed::Pending => paint(&ed, &mut painted),
            Feed::Complete => {
                crate::file_init::complete_line(&mut ed);
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

#[cfg_attr(feature = "kernel_tests", allow(dead_code))]
fn write_prompt(painted: &mut usize) {
    console_init::write(PROMPT.as_bytes());
    *painted = PROMPT.len();
}

#[cfg_attr(feature = "kernel_tests", allow(dead_code))]
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

/// Block for one key. Drain with IRQs off; never hold the FB/serial lock
/// across the wait. `sti; hlt` is one instruction so a keyboard IRQ
/// cannot slip between enable and halt.
#[cfg_attr(feature = "kernel_tests", allow(dead_code))]
fn wait_key() -> DecodedKey {
    loop {
        if let Some(k) = console_init::read() {
            return k;
        }
        if !per_cpu_init::current().runq.is_empty() {
            thread_init::yield_now();
            continue;
        }
        unsafe {
            core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
        }
        if let Some(k) = console_init::read() {
            unsafe {
                core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
            }
            return k;
        }
        if !per_cpu_init::current().runq.is_empty() {
            unsafe {
                core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
            }
            thread_init::yield_now();
            continue;
        }
        unsafe {
            core::arch::asm!("sti; hlt", options(nomem, nostack));
        }
    }
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

fn cmd_echo(args: &[&str]) {
    let mut i = 1usize;
    while i < args.len() {
        if i > 1 {
            console_init::write(b" ");
        }
        console_init::write(args[i].as_bytes());
        i += 1;
    }
    console_init::write(b"\n");
}

fn cmd_meminfo(_args: &[&str]) {
    diag::meminfo_to(&mut Console);
}

fn cmd_uptime(_args: &[&str]) {
    diag::uptime_to(&mut Console);
}

fn cmd_cpus(_args: &[&str]) {
    diag::cpus_to(&mut Console);
}

fn cmd_dmesg(args: &[&str]) {
    let mut view: Option<Level> = None;
    let mut follow = false;
    let mut i = 1usize;
    while i < args.len() {
        match args[i] {
            "-f" | "follow" => follow = true,
            "-n" => {
                i += 1;
                let Some(s) = args.get(i).copied() else {
                    let _ = writeln!(Console, "vibeOS: dmesg: -n needs a level");
                    return;
                };
                let Some(l) = Level::from_name(s) else {
                    let _ = writeln!(Console, "vibeOS: dmesg: bad level");
                    return;
                };
                log_init::set_max_level(l);
                view = Some(l);
            }
            s => match Level::from_name(s) {
                Some(l) => view = Some(l),
                None => {
                    let _ = writeln!(Console, "vibeOS: dmesg: bad arg");
                    return;
                }
            },
        }
        i += 1;
    }
    log_init::dmesg_write(&mut Console, view);
    if follow {
        dmesg_follow(view.unwrap_or_else(log_init::max_level));
    }
}

fn dmesg_follow(view: Level) {
    let mut seen = log_init::written();
    loop {
        if let Some(DecodedKey::Char(3)) = console_init::read() {
            console_init::write(b"^C\n");
            return;
        }
        let now = log_init::written();
        if now > seen {
            let extra = (now - seen) as usize;
            let len = log_init::ring_len();
            let start = len.saturating_sub(extra);
            let mut i = start;
            while i < len {
                if let Some(r) = log_init::record_at(i) {
                    if vibeos::log::allowed(r.level, view, log_init::compile_max()) {
                        log_init::write_record(&mut Console, &r);
                    }
                }
                i += 1;
            }
            seen = now;
        }
        if !per_cpu_init::current().runq.is_empty() {
            thread_init::yield_now();
        } else {
            thread_init::sleep_ms(10);
        }
    }
}

fn cmd_ps(_args: &[&str]) {
    let mut buf = [ThreadInfo {
        id: ThreadId::NONE,
        name: "",
        state: ThreadState::Dead,
        cpu: 0,
    }; MAX_THREADS];
    let n = thread_init::snapshot(&mut buf);
    let mut i = 0usize;
    while i < n {
        let t = buf[i];
        let _ = writeln!(
            Console,
            "vibeOS: ps: tid={} cpu={} state={} name={}",
            t.id.raw(),
            t.cpu,
            t.state.name(),
            t.name
        );
        i += 1;
    }
}

fn cmd_panic(_args: &[&str]) {
    panic!("shell: panic");
}

fn cmd_reboot(_args: &[&str]) {
    let _ = writeln!(Console, "vibeOS: reboot");
    try_acpi_reset();
    pulse_8042();
    unsafe { x86::outb(0xCF9, 0x06) };
    x86::halt();
}

fn cmd_poweroff(_args: &[&str]) {
    let _ = writeln!(Console, "vibeOS: poweroff");
    try_acpi_sleep_s5();
    unsafe {
        x86::outw(0x604, 0x2000);
        x86::outw(0xB004, 0x2000);
    }
    x86::halt();
}

fn try_acpi_reset() {
    let Some(info) = acpi_init::info() else {
        return;
    };
    let Some(fadt) = info.fadt else {
        return;
    };
    if fadt.reset.is_empty() {
        return;
    }
    write_gas(fadt.reset, fadt.reset_value);
}

fn try_acpi_sleep_s5() {
    let Some(info) = acpi_init::info() else {
        return;
    };
    let Some(fadt) = info.fadt else {
        return;
    };
    if fadt.sleep_control.is_empty() {
        return;
    }
    // SLP_TYPx = 5 in bits 2..4, SLP_EN bit 5.
    write_gas(fadt.sleep_control, (5 << 2) | (1 << 5));
}

fn write_gas(gas: Gas, val: u8) {
    match gas.space_id {
        GAS_SYSTEM_IO => {
            let port = gas.address as u16;
            unsafe {
                match gas.access_size {
                    2 => x86::outw(port, val as u16),
                    3 => x86::outl(port, val as u32),
                    _ => x86::outb(port, val),
                }
            }
        }
        GAS_SYSTEM_MEMORY => {
            if gas.address == 0 {
                return;
            }
            let va = paging_init::HHDM_BASE.wrapping_add(gas.address);
            unsafe { (va as *mut u8).write_volatile(val) };
        }
        _ => {}
    }
}

fn pulse_8042() {
    let mut n = 100_000u32;
    while n > 0 {
        if unsafe { x86::inb(vibeos::kbd::STATUS) } & vibeos::kbd::STAT_IBF == 0 {
            unsafe { x86::outb(vibeos::kbd::CMD, 0xFE) };
            return;
        }
        n -= 1;
    }
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn builtin_names() -> [&'static str; 10] {
    [
        "help", "echo", "meminfo", "uptime", "cpus", "dmesg", "ps", "panic", "reboot",
        "poweroff",
    ]
}

const _: () = {
    assert!(LINE_CAP > 0);
};
