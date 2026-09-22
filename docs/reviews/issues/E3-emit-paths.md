# E3 · Make the marker-vs-log rule explicit; one macro per intent

**Status:** implemented. `marker!` in `src/serial.rs`; DESIGN §2.6 and AGENTS carry the rule.
Unused `print!`/`println!` deleted with Q4.

| | |
|---|---|
| **Area** | 4.4 Error handling, logging & observability |
| **Impact / Effort / Phase** | Low / S / II |
| **Depends on** | — |
| **Blocks** | — |
| **Review** | [ARCHITECTURE_REVIEW.md §4.4](../ARCHITECTURE_REVIEW.md#44-error-handling-logging--observability) |

## Problem

Three ways to write a line: `serial::line(marker::X)` / `writeln!(Serial, …)` (captured into the log ring, bypasses the level filter; ~40 marker call sites across `main.rs`, `smp_init.rs`, `apic_init.rs`, `time_init.rs`, `virtio*_init.rs`, `vibefs_init.rs`), `klog!(level, …)` (filtered; `src/log_init.rs:339`), and `PlainSerial` (not captured; `dmesg`). The rule "markers bypass the filter on purpose" is implicit. The printer thread is a parked stub (`start_printer_thread() {}`), documented as a Design ACK. Logging itself (`src/log.rs`, `src/log_init.rs`) is well designed and host-tested; no structural change needed.

## Recommended fix

One macro per intent, documented next to the level table.

## Implementation plan

1. **`marker!`** in `src/serial.rs`: `macro_rules! marker { ($m:expr) => { $crate::serial::line($m) }; ($fmt:literal, $($a:tt)*) => { let _ = writeln!($crate::serial::Serial, $fmt, $($a)*); } }`. Replace the ~40 `serial::line(marker::…)` and formatted marker writes with it (grep `serial::line(` and `writeln!(\n *Serial,\n *"vibeOS:`).
2. **Rule** in DESIGN §2.6: "`marker!` for contract lines (never filtered, always captured); `klog!` for everything else; `PlainSerial` only for `dmesg` and panic dumps". Add the same two lines to `AGENTS.md` (DOC3).
3. **Delete** the unused `print!` / `println!` macros (Q4).
4. **Optional:** a hostlib test that every `marker::*` constant is referenced by exactly one `marker!` site (parse `src/*.rs`), which catches a marker emitted twice or never.

## Acceptance criteria

- `grep -rn 'serial::line(marker::' src` is empty; all marker emits go through `marker!`.
- DESIGN §2.6 carries the rule.

## Tests

Optional parse test above; e2e contract unchanged.

## Risks and rollback

None; macro is a rename.
