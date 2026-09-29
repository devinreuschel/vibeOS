//! System commands: `echo`, `meminfo`, `uptime`, `cpus`, `dmesg`, `ps`,
//! `panic`, `reboot` and `poweroff` (ROADMAP §5.4).

use core::fmt::Write;

use vibeos::acpi::{GAS_SYSTEM_IO, GAS_SYSTEM_MEMORY, Gas};
use vibeos::kbd::DecodedKey;
use vibeos::log::Level;
use vibeos::shell::Command;
use vibeos::thread::{MAX_THREADS, ThreadId, ThreadState};

use crate::acpi_init;
use crate::console_init::{self, Console};
use crate::diag;
use crate::log_init;
use crate::paging_init;
use crate::per_cpu_init;
use crate::thread_init::{self, ThreadInfo};
use crate::x86;

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
                if let Some(r) = log_init::record_at(i)
                    && vibeos::log::allowed(r.level, view, log_init::compile_max())
                {
                    log_init::write_record(&mut Console, &r);
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
    crate::proc_init::write_ps(&mut Console);
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
