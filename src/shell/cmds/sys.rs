//! System commands: `echo`, `meminfo`, `uptime`, `cpus`, `dmesg`, `ps`,
//! `panic`, `reboot` and `poweroff` (ROADMAP §5.4).

use core::fmt::Write;

use vibeos::kbd::DecodedKey;
use vibeos::log::Level;
use vibeos::shell::Command;

use crate::console_init::{self, Console};
use crate::diag;
use crate::log_init;
use crate::per_cpu_init;
use crate::thread_init;

pub(crate) const COMMANDS: &[Command] = &[
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
                    #[expect(
                        clippy::let_underscore_must_use,
                        reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
                    )]
                    let _ = writeln!(Console, "vibeOS: dmesg: -n needs a level");
                    return;
                };
                let Some(l) = Level::from_name(s) else {
                    #[expect(
                        clippy::let_underscore_must_use,
                        reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
                    )]
                    let _ = writeln!(Console, "vibeOS: dmesg: bad level");
                    return;
                };
                log_init::set_max_level(l);
                view = Some(l);
            }
            s => match Level::from_name(s) {
                Some(l) => view = Some(l),
                None => {
                    #[expect(
                        clippy::let_underscore_must_use,
                        reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
                    )]
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
                if let Some(r) = log_init::record_at(i)
                    && vibeos::log::allowed(r.level, view, log_init::compile_max())
                {
                    log_init::write_record(&mut Console, &r);
                }
                i += 1;
            }
            seen = now;
        }
        if !per_cpu_init::with_current(|c| c.runq.is_empty()) {
            thread_init::yield_now();
        } else {
            thread_init::sleep_ms(10);
        }
    }
}

fn cmd_ps(_args: &[&str]) {
    crate::proc_init::write_ps(&mut Console);
    // A chunk of threads per SCHED section, printed with the lock dropped
    // (`thread_init::each_thread`).
    thread_init::each_thread(|t| {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
        )]
        let _ = writeln!(
            Console,
            "vibeOS: ps: tid={} cpu={} state={} name={}",
            t.id.raw(),
            t.cpu,
            t.state.name(),
            t.name
        );
    });
}

#[allow(
    clippy::panic,
    reason = "the `panic` command's behaviour: an operator's deliberate panic in the kernel_shell debug build"
)]
fn cmd_panic(_args: &[&str]) {
    panic!("shell: panic");
}

fn cmd_reboot(_args: &[&str]) {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
    )]
    let _ = writeln!(Console, "vibeOS: reboot");
    crate::arch::current::power::restart();
}

fn cmd_poweroff(_args: &[&str]) {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
    )]
    let _ = writeln!(Console, "vibeOS: poweroff");
    crate::arch::current::power::power_off();
}
