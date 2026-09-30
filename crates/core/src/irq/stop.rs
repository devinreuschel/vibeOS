//! The stop primitive's portable half (DESIGN §2.5 step 1, ROADMAP §10.7).
//!
//! The panic dump's owner stops every other online CPU before it writes:
//! it sets [`STOP`] in each one's request word and sends it the `0xFE`
//! IPI, waits [`stop_budget`] of counter time for each `stopped` word,
//! sends NMI to each CPU still running, and waits once more. A CPU stops
//! at the first of four points, which [`StopHow`] records: the IPI's body,
//! a serviced spin's poll (or a serial write after `HALTING`), the NMI
//! body, or its own panic that found the dump claimed. The kernel half is
//! `ipi_init::{stop_others, stop_this_cpu, nmi_stop}`.

/// The request word's stop bit: set by the dump's owner (Release), read by
/// the CPU it names in `service_incoming` and its NMI body (Acquire).
pub const STOP: u32 = 1;

/// A `stopped` word's value while its CPU runs.
pub const RUNNING: u32 = 0;
/// A `stopped` word's value from the moment its CPU starts stopping until
/// it acknowledges: a later stop path (an NMI during a poll stop) finds it
/// and halts at once, so the crash slot is written once.
pub const STOPPING: u32 = 1;

/// How a CPU stopped, stored in its `stopped` word as [`StopHow::code`]:
/// that store (Release) is the acknowledgement the owner waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopHow {
    /// The `0xFE` IPI's body.
    Ipi,
    /// A serviced spin's `service_incoming`, or a serial write or log
    /// append after `HALTING` (the raw layer's stop hook).
    Poll,
    /// The NMI body, after the owner's NMI.
    Nmi,
    /// Its own panic, which found the dump claimed by another CPU.
    Panic,
}

impl StopHow {
    pub const ALL: [StopHow; 4] = [StopHow::Ipi, StopHow::Poll, StopHow::Nmi, StopHow::Panic];

    /// The word the dump prints: `vibeOS: panic: cpu N stopped (<word>)`.
    pub const fn as_str(self) -> &'static str {
        match self {
            StopHow::Ipi => "ipi",
            StopHow::Poll => "poll",
            StopHow::Nmi => "nmi",
            StopHow::Panic => "panic",
        }
    }

    /// The `stopped` word's value for a CPU that stopped this way: above
    /// [`STOPPING`], so no code reads as running or stopping.
    pub const fn code(self) -> u32 {
        match self {
            StopHow::Ipi => 2,
            StopHow::Poll => 3,
            StopHow::Nmi => 4,
            StopHow::Panic => 5,
        }
    }

    /// The way a `stopped` word's value names, or `None` for [`RUNNING`],
    /// [`STOPPING`] and any value no stop path stores.
    pub const fn from_code(code: u32) -> Option<StopHow> {
        match code {
            2 => Some(StopHow::Ipi),
            3 => Some(StopHow::Poll),
            4 => Some(StopHow::Nmi),
            5 => Some(StopHow::Panic),
            _ => None,
        }
    }
}

/// Indices of a crash-register slot's words (`PerCpuRemote.crash`).
pub const CRASH_RIP: usize = 0;
pub const CRASH_RSP: usize = 1;
pub const CRASH_RBP: usize = 2;
pub const CRASH_RFLAGS: usize = 3;
/// Words in a crash-register slot.
pub const CRASH_WORDS: usize = 4;

/// What a stopped CPU saves before it acknowledges: the interrupted
/// registers when a trap stopped it, its own otherwise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrashRegs {
    pub rip: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub rflags: u64,
}

impl CrashRegs {
    /// The slot's words, at the `CRASH_*` indices.
    pub const fn to_words(self) -> [u64; CRASH_WORDS] {
        let mut w = [0u64; CRASH_WORDS];
        w[CRASH_RIP] = self.rip;
        w[CRASH_RSP] = self.rsp;
        w[CRASH_RBP] = self.rbp;
        w[CRASH_RFLAGS] = self.rflags;
        w
    }

    /// The registers a slot's words hold.
    pub const fn from_words(w: [u64; CRASH_WORDS]) -> Self {
        Self {
            rip: w[CRASH_RIP],
            rsp: w[CRASH_RSP],
            rbp: w[CRASH_RBP],
            rflags: w[CRASH_RFLAGS],
        }
    }
}

/// What the NMI body does, decided before it writes anything or takes any
/// lock: the owner may be inside `write_owner` when its own NMI lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NmiAction {
    /// This CPU is already stopping or stopped: halt at once, leaving its
    /// slot as the first stop path wrote it.
    Halt,
    /// The owner asked this CPU to stop: stop with [`StopHow::Nmi`].
    Stop,
    /// The dump's owner, with no request: return to the dump.
    Return,
    /// Any other NMI: dump it, as an unexpected NMI always has.
    Dump,
}

/// The NMI body's decision from the request word it swapped to 0 (`req`),
/// its CPU's `stopped` word (`state`), and whether it runs on the dump's
/// owner.
pub const fn nmi_action(req: u32, state: u32, is_owner: bool) -> NmiAction {
    if state != RUNNING {
        NmiAction::Halt
    } else if is_owner {
        NmiAction::Return
    } else if req & STOP != 0 {
        NmiAction::Stop
    } else {
        NmiAction::Dump
    }
}

/// Polls per millisecond the owner's waits assume with no measured counter
/// frequency: a bound, not a clock, so a wait still ends.
pub const SPINS_PER_MS: u64 = 100_000;

/// How long one of the owner's waits lasts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopBudget {
    /// Counter cycles (`CycleCounter::now`).
    Cycles(u64),
    /// Polls, when the counter's frequency is not known yet.
    Spins(u64),
}

/// A wait of `ms` milliseconds: cycles from the counter frequency
/// `freq_hz` (C-SEAM-CORE's `CycleCounter::freq_hz`), or [`SPINS_PER_MS`]
/// polls per millisecond when it is `None` or below 1 kHz. Saturates rather than
/// overflows: a wait never ends early.
pub const fn stop_budget(freq_hz: Option<u64>, ms: u64) -> StopBudget {
    match freq_hz {
        Some(hz) if hz >= 1000 => StopBudget::Cycles((hz / 1000).saturating_mul(ms)),
        _ => StopBudget::Spins(SPINS_PER_MS.saturating_mul(ms)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_how_text() {
        let words: [&str; 4] = StopHow::ALL.map(StopHow::as_str);
        assert_eq!(words, ["ipi", "poll", "nmi", "panic"]);
        for how in StopHow::ALL {
            assert!(how.code() > STOPPING);
            assert_eq!(StopHow::from_code(how.code()), Some(how));
        }
        assert_eq!(StopHow::from_code(RUNNING), None);
        assert_eq!(StopHow::from_code(STOPPING), None);
        assert_eq!(StopHow::from_code(6), None);
        assert_eq!(StopHow::from_code(u32::MAX), None);
        let r = CrashRegs {
            rip: 1,
            rsp: 2,
            rbp: 3,
            rflags: 4,
        };
        assert_eq!(r.to_words(), [1, 2, 3, 4]);
        assert_eq!(CrashRegs::from_words(r.to_words()), r);
    }

    #[test]
    fn nmi_action_table() {
        let stopped = StopHow::Poll.code();
        // (req, state, owner) -> action
        let rows = [
            (0, RUNNING, false, NmiAction::Dump),
            (STOP, RUNNING, false, NmiAction::Stop),
            (0, RUNNING, true, NmiAction::Return),
            (STOP, RUNNING, true, NmiAction::Return),
            (0, STOPPING, false, NmiAction::Halt),
            (STOP, STOPPING, false, NmiAction::Halt),
            (STOP, stopped, false, NmiAction::Halt),
            (0, stopped, true, NmiAction::Halt),
            // Bits other than STOP ask for nothing.
            (2, RUNNING, false, NmiAction::Dump),
        ];
        for (req, state, owner, want) in rows {
            assert_eq!(nmi_action(req, state, owner), want, "{req} {state} {owner}");
        }
    }

    #[test]
    fn stop_budget_bounds() {
        assert_eq!(
            stop_budget(Some(1_000_000_000), 100),
            StopBudget::Cycles(100_000_000)
        );
        assert_eq!(
            stop_budget(Some(2_500_000_000), 10),
            StopBudget::Cycles(25_000_000)
        );
        assert_eq!(
            stop_budget(None, 100),
            StopBudget::Spins(100 * SPINS_PER_MS)
        );
        assert_eq!(
            stop_budget(Some(0), 10),
            StopBudget::Spins(10 * SPINS_PER_MS)
        );
        assert_eq!(
            stop_budget(Some(u64::MAX), u64::MAX),
            StopBudget::Cycles(u64::MAX)
        );
        assert_eq!(stop_budget(None, u64::MAX), StopBudget::Spins(u64::MAX));
        // A sub-kHz frequency would round to 0 cycles, a wait that ends
        // at once: it counts polls instead.
        assert_eq!(
            stop_budget(Some(999), 100),
            StopBudget::Spins(100 * SPINS_PER_MS)
        );
    }
}
