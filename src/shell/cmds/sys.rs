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
#[cfg(target_arch = "x86_64")]
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
        i += 1;
    }
}

#[allow(
    clippy::panic,
    reason = "the `panic` command's behaviour: an operator's deliberate panic in the kernel_shell debug build"
)]
fn cmd_panic(_args: &[&str]) {
    panic!("shell: panic");
}

#[cfg(target_arch = "x86_64")]
fn cmd_reboot(_args: &[&str]) {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
    )]
    let _ = writeln!(Console, "vibeOS: reboot");
    try_acpi_reset();
    pulse_8042();
    // SAFETY: invariant I229 names the `kernel_shell` debug build's port
    // commands as reaching any port; 0xCF9 is the chipset's reset control, and a reset is this command's purpose; established here.
    unsafe { x86::outb(0xCF9, 0x06) };
    x86::halt();
}

#[cfg(target_arch = "x86_64")]
fn cmd_poweroff(_args: &[&str]) {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
    )]
    let _ = writeln!(Console, "vibeOS: poweroff");
    try_acpi_sleep_s5();
    // SAFETY: invariant I229 names the `kernel_shell` debug build's port
    // commands as reaching any port; 0x604 and 0xB004 are QEMU's and Bochs's ACPI power-off ports, and a power-off is this command's purpose; established here.
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

#[cfg(target_arch = "x86_64")]
fn write_gas(gas: Gas, val: u8) {
    match gas.space_id {
        GAS_SYSTEM_IO => {
            let port = gas.address as u16;
            // SAFETY: invariant I229 names the `kernel_shell` debug build's port
            // commands as reaching any port; the port is the reset or sleep register the FADT names, and the write is the one ACPI defines for it; established here.
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
            // SAFETY: the FADT names this register for the reset or sleep
            // write ACPI defines, and the HHDM maps physical memory at
            // `paging_init::HHDM_BASE`; the firmware's address is trusted
            // here as the ACPI tables are (DESIGN §2.10).
            unsafe { (va as *mut u8).write_volatile(val) };
        }
        _ => {}
    }
}

#[cfg(target_arch = "x86_64")]
fn pulse_8042() {
    let mut n = 100_000u32;
    while n > 0 {
        // SAFETY: invariant I229 names the `kernel_shell` debug build's port
        // commands as reaching any port; it reads the 8042's status and pulses its reset line, which this command exists to do; established here.
        if unsafe { x86::inb(vibeos::kbd::STATUS) } & vibeos::kbd::STAT_IBF == 0 {
            // SAFETY: as for the status read above (invariant I229); the
            // 0xFE command pulses the CPU reset line; established here.
            unsafe { x86::outb(vibeos::kbd::CMD, 0xFE) };
            return;
        }
        n -= 1;
    }
}
