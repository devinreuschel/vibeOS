# 8. Testing

Index: [DESIGN.md](DESIGN.md). This file holds DESIGN §8, and its headings keep DESIGN's numbers.

Three tiers. Each catches a class of bug the others cannot, and each is progressively slower, so the
decision of where a test goes matters.

| Tier | Runs | Speed | Catches |
|------|------|-------|---------|
| Host unit | `make test-unit` (`vibeos-core`, any host triple) | milliseconds | Algorithms: allocators, parsers, state machines, encodings, arithmetic |
| In-guest (ktest) | QEMU, kernel built with the `kernel_tests` feature | seconds | Anything needing real hardware state: page tables, MMIO, interrupts, threads, SMP |
| End to end | QEMU boot of the normal ISO, serial captured | ~10 s | Boot regressions, marker ordering, panics, subsystem interaction |

The routing rule: if it can be a host test, it must be. Pushing logic into the library half of the
crate so it becomes host-testable is the highest-leverage thing available, and the old tree's biggest
weakness was that nearly everything lived behind `main.rs` and was therefore untestable.

**Loom models.** ROADMAP §10.8's loom models run the kernel's own `vibeos-core` primitives under
`--cfg loom`, where `vibeos::atomic` is loom's, and check every interleaving up to the bound each
model states. Each is a `#[cfg(all(test, loom))]` unit test named `loom_*` in a `loom_models` module
beside its primitive. Each model has a variant that weakens one ordering or moves one step at a
named `vibeos::sync::variant::Site`, whose code outside `cfg(loom)` is always the kernel's; the
variant is a `should_panic` test, named `_fails`, that passes only when loom finds the failure. The
bound is set in code through `variant::check`, so the `LOOM_MAX_*` environment cannot shrink it; the
first two models below take `LOOM_MAX_PREEMPTIONS`, which `make models-quick` sets to 3. Where a
model cannot run a kernel-half step (`switch_context`'s saves, `wake_all` under `SCHED`,
`dmesg_write`'s loop), it writes a loom `UnsafeCell` witness in its place.

| Primitive (file under `crates/core/src/`) | Base test | Variant test: the site it switches | Bound |
|---|---|---|---|
| `TryArc` count (`kalloc.rs`) | `loom_tryarc_count` | `loom_tryarc_count_relaxed_dec_fails`: `TryArcDecrement`, the put decrements Relaxed | 3 threads, `LOOM_MAX_PREEMPTIONS` |
| `OpGate` (`sync/mod.rs`) | `loom_opgate` | `loom_opgate_check_first_fails`: `OpGateCountFirst`, `enter` reads the dead mark first | 2 threads, `LOOM_MAX_PREEMPTIONS` |
| `TickClock` latch (`time/mod.rs`) | `loom_seqlock_latch`: pairs whose words all derive from one number, so a mixed read fails `seqlock: torn read` | `loom_seqlock_acqrel_bump_tears_fails`: `SeqlockBumpAcqRel`, the bump is one AcqRel `fetch_add` with no fences; `loom_seqlock_no_leading_fence_tears_fails`: `SeqlockLeadingFence`, the bump drops its leading `fence(Release)` | 2 threads (1 write, 2 reads), 3 preemptions |
| `WakeInbox` (`irq/ipi.rs`) | `loom_wake_inbox_three_pushers`: each slot delivered exactly once | `loom_wake_inbox_summary_first_loses_id_fails`: `InboxSummaryFirst`, push sets the summary bit first | 4 threads (3 pushes, 2 drains), 2 preemptions |
| `IrqCell` over the log ring (`cell.rs`, `log/mod.rs`) | `loom_log_ring_writers_dmesg`: records whole, each writer's in order, none twice | `loom_log_ring_relaxed_unlock_races_fails`: `IrqCellUnlockRelaxed`, the unlock store is Relaxed | 3 threads (2 writers of 2 emits, 1 reader), 1 preemption |
| `DoneWord` (`block/mod.rs`; model in `block/loom_models.rs`) | `loom_io_done_publish_last` | `loom_io_done_relaxed_races_fails`: `IoDoneRelaxed`, `publish` stores Relaxed | 2 threads, 3 preemptions |
| `OnCpu` (`sched/thread.rs`) | `loom_on_cpu_handoff` | `loom_on_cpu_relaxed_clear_races_fails`: `OnCpuClearRelaxed`, `clear` stores Relaxed | 2 threads, 3 preemptions |

`make models-quick`, part of `make check`, runs them all. By hand:

```sh
HOST=$(rustc -vV | sed -n 's/^host: //p')
RUSTFLAGS="--cfg loom -D warnings" CARGO_TARGET_DIR=target/loom \
  cargo test -p vibeos-core --lib --features std --target "$HOST" --release -- loom_
```

## 8.1 Host unit tests

Anything in `crates/core/src/lib.rs` and its submodules, compiled as `vibeos-core` on the host. No hardware
access, no `unsafe` port I/O, no MMIO. The kernel half calls into it. Each port's pure half
([§11.1](PORTABILITY.md#111-the-seam)) is part of it and runs on every host. Rule; not yet enforced: ROADMAP §10.3.
The core carries no assembly and no `cfg(target_arch)`, which `scripts/check_core_stable.py` enforces,
so every host runs all of its tests. A host test of a port's assembly lives in `tests/hostlib`:
`switch_context_roundtrip`, in `tests/hostlib/tests/switch_context.rs`, includes
`src/arch/x86_64/switch.rs` with empty `cli` and `sti` macros and runs on x86_64 Linux hosts, since
the assembly uses the kernel's object format (ELF); other hosts build an empty test binary.

Things that belong here and are easy to get wrong, so should have tests from the day they are written:

- Buddy allocator: split, merge, exhaustion, fragmentation, alignment per order, free count returning
  to its initial value after a random alloc/free sequence, double free detection.
- ACPI: RSDP v1 and v2 checksum rejection, table length validation, HPET generic address structure
  rejecting I/O space and zero addresses, MADT entry iteration over truncated tables.
- Timekeeping: the clocksource conversion against an independent `u128` evaluation, a 24-bit counter
  through ten wraps read every half wrap, the ranking, the latch (a reader that stops the writer between
  its copies reads the older one) and seqlock retry under a simulated concurrent writer, monotonicity,
  overflow near `u64::MAX`, HPET/PIT agreement bands (invariant vs TCG).
- ICR delivery-pending poll: returns true when the bit clears, false at the iteration cap.
- Vector table: no two named vectors are equal.
- Scan code decoding: make and break codes, `0xE0` prefixes, modifier state, unknown codes returning
  `None` rather than panicking.
- Ring buffer: wrap-around FIFO order, full and empty boundaries, overwrite-oldest semantics.
- Line editor and command tokenization: quoting, whitespace, empty input, unknown commands.
- Font: every printable ASCII code point yields eight rows.
- `align_up` and friends at 0, at exactly aligned, and near overflow.
- VFS path walk: `.` / `..`, bounded symlink depth, symlink loop → error, negative dentry
  invalidate-on-create, mount-point crossing.
- kernfs: one directory implementation shared by devfs/tmpfs/procfs/sysfs; tmpfs
  writes evict through the Phase 7 block cache rather than pinning a grow-only
  buffer; `/dev/null` `/dev/zero` `/dev/random` (virtio-rng, then RDRAND, and nothing
  else: a short count when they supply less, `EAGAIN` when they supply none,
  until ROADMAP §13.10's CSPRNG); procfs stubs do not
  panic.

Two lessons about writing these:

A test that derives its expected value from the same read it is checking proves nothing. The old
seqlock test computed the expected timestamp as `tick + 100`, which is monotonic no matter how torn the
read was, so it passed against a broken implementation. The writer has to publish an independent value
the reader can compare against.

A single-threaded test cannot observe a race. Simulate the interrupt-context writer explicitly, or
accept that the real coverage is in-guest.

**Fuzzing (C-FUZZ, ROADMAP §10.2).** `tests/fuzz` is a cargo-fuzz crate, `vibeos-fuzz`, outside the
Cargo workspace (the root `Cargo.toml` excludes it), so the kernel build and the MSRV check never
read `libfuzzer-sys`; `make check` runs `cargo deny` on it as a second workspace, against the same
`deny.toml`. It has its own `Cargo.lock` and profiles, which keep
`overflow-checks` and `debug-assertions` on so a fuzzer panics where the kernel would (DESIGN §3.5).
It holds one target per byte-slice parser in `vibeos-core`, each a `fuzz_targets/<t>.rs` over one
`pub fn <t>(data: &[u8])` and a row of `vibeos_fuzz::TARGETS`: `acpi_walk`, `acpi_tables`,
`part_parse`, `fat_mount`, `vibefs_mount`, `pci_enumerate`, `virtio_caps`, `shell_tokenize`,
`kbd_decode`, `elf_parse`, `cmdline_parse` and `vmcoreinfo_parse`. Most take the input as raw
bytes; three encodings model hardware: `Sparse`, a disk of 512- or 4096-byte units named by index
(flag bit 0 recomputes the GPT CRCs, bit 1 selects 4096-byte partition sectors), `FakeCfg`, PCI
config-space records of bus, devfn and 256 bytes, and `FlatMem`, the input as physical memory at
`0xE0000` for `acpi::walk`. `corpus/<t>/seed-*` holds only the output of `cargo run --example
seeds`, built with the parsers' own builders; `regressions/<t>/` holds crash inputs. `make check`
runs `make fuzz-check`: rustfmt, clippy, a build of every target, and `cargo test`, which replays
every committed corpus and regression input on its own thread with a 10 s bound, checks that
`TARGETS`, the `[[bin]]`s, `fuzz_targets/` and `corpus/` agree, that the generator's output is
committed and accepted, and that every row of `scripts/check_core_stable.py`'s `PARSERS` table is
named by a target's `covers`; it fuzzes nothing. `make fuzz` runs each target (`FUZZ_TARGETS`,
default all) for `FUZZ_TIME` seconds (default 60) with `FUZZ_UNIT_TIMEOUT` (10 s) per input and the
`FUZZ_SANITIZER` (`address`, and `none` on macOS, where AddressSanitizer is unusable), writing new
inputs to `build/fuzz/corpus/<t>` and crashes to `build/fuzz/artifacts/<t>`; it needs the pinned
cargo-fuzz (`CARGO_FUZZ_VERSION`) and names the install command when that is missing. The weekly
`smp-stress.yml` job runs it. A crash: reproduce it with `cargo fuzz run --fuzz-dir tests/fuzz <t>
<file>`, minimize it with `cargo fuzz tmin --fuzz-dir tests/fuzz <t> <file>`, fix the parser, and
commit the minimized input under `regressions/<t>/` (no file extension: the root `.gitignore`
drops `*.bin` and its kin) in the fix's commit. A parser added or changed later gets its target,
its `TARGETS` row, its seed and its `check_core_stable.py` row in the parser's own commit. The
harness caps (2^20 units for partitions and FAT, 4096 blocks for vibefs, 64 PCI records, 256 nodes,
depth 8 and 64 KiB per file in the filesystem walks) bound the harness, not the parser: a real disk
can be larger. `vibefs_mount` fixes no checksum, because vibefs v1 trusts any checksum-valid block
(F061, ROADMAP §14.8), so it reaches only what a valid checksum lets through.

## 8.2 In-guest tests

A second kernel build with `--features kernel_tests` that boots normally, runs a registry of test
functions after init, reports over serial, and exits QEMU through the `isa-debug-exit` device.

`ktest::run`, called on the bootstrap thread at the end of boot, spawns the registry as the kernel
thread `ktest`, pinned to CPU 0, on a guarded 64 KiB KVA stack (`thread_init::spawn_opts`; `spawn`
gives 16 KiB), and parks the bootstrap thread. The registry runs each test with IF on and
`irq_nest` 0, the context production kernel threads run in, and a `spawn_here` worker, which copies
its spawner's `irq_nest`, starts with IF on too. A test that needs interrupts off takes its own
`InterruptGuard`: the cooperative `switch_to` and `yield_now` tests, and every `arch::catch` window
that can longjmp out of an interrupt gate, since the skipped `iretq` would leave IF off. After each
test the registry fails it if IF is off or `irq_nest` is not 0, and restores both. `ktest_context`
checks the registry's context and a `spawn_here` worker's.

Each subsystem's `src/<subsystem>/ktest.rs` exports its rows as `pub(crate) const TESTS: &[Test]`,
and `src/ktest/mod.rs` runs the lists in the order of its `GROUPS` (DESIGN §1.3). A list left out
of `GROUPS` is unreferenced code, which the `kernel_tests` clippy run with `-D warnings` rejects as
dead; the rule against a blanket `allow(dead_code)` in production modules (ROADMAP §10.2, Q2) keeps
that true. The log group runs first, since `log_boot_captured` reads boot lines that the other
groups' lines push out of the log ring.

Built in the one `target/` like every variant, but copied to its own named ELF,
`build/kernels/vibeos-ktest.elf`, which only `build/vibeos-ktest.iso`'s recipe reads. This is not
fussiness: an ISO recipe that packaged whatever ELF the last build left in `target/` could put a
feature-enabled ELF into the production ISO, and the difference is not visible from the outside.
Each variant's recipe removes its named ELF first, builds with `--artifact-dir`, so a parallel
build of another variant cannot swap the file, and writes the named ELF last. The panic-dump, `#GP`,
panic-nest and panic-stop ISOs are `--features panic_test`, `--features gp_test`, `--features
panic_nest_test` and `--features panic_stop_test` (underscores in features; Cargo
features in this crate do not use hyphens, and the variant names do not use underscores).

```
vibeOS: ktest: begin <n>
vibeOS: ktest: run <name> <deadline_ms>
vibeOS: ktest: ok <name> (<us> us)
vibeOS: ktest: FAIL <name>: <reason>
vibeOS: ktest: skip <name>: <reason>
vibeOS: ktest: info <name>: <text>
vibeOS: ktest: end
```

`begin` counts the runs the boot will make, and the runner asserts that it made that many before
`end`. A `run` line precedes each run with the name and deadline of the row, and the run's result
line follows it: `ok` with the run's time in microseconds, read from the cycle counter
(`CycleCounter::now`) around the body, `FAIL` with its reason, or `skip` with its reason. A test
prints a counter or a measurement as an `info` line (`ktest_info!`), which names the running test,
or `ktest` between tests, and is never a result: it is not counted in `begin`'s `<n>`. A failure
reason formats into a fixed 120-byte `FailMsg` through `vibeos::fmt_util::StackBuf`, cut at a
character boundary. The runner's own group, `ktest_names_unique`, checks that names are unique
across `GROUPS` and match `[a-z0-9_]+`.

The harness reads each line through `harness.parse_ktest_line`, which matches `vibeOS: ktest: ` and a
protocol word at the start of a framed line's text (§2.6), so a user program's copy, such as the
unframed `?vibeOS: ktest: FAIL forged` line `/bin/tests` prints, a `dmesg:` or `logrec:` replay, and
a `vibeOS: ktest:   <detail>` line never count. It requires `begin <n>` and `end`, fails a `begin`
with no count and `begin 0` (`ktest: no test selected`), rejects any `FAIL` line and any panic
signature, and checks the exit status. `run_ktest.py` prints `[ktest] <ok> of <n> runs passed, <s>
skipped`, the ten slowest runs, and the info lines (`ktest_summary`).

The deadline (ROADMAP §10.2, T1). Each row has a deadline, `vibeos::ktest::DEFAULT_DEADLINE_MS`
(10 s) unless the registry sets another with `.deadline(ms)`, reviewed as code: twice the longest
time a tier measured, for a row over 5 s. The runner arms it just before the body runs, as
`CycleCounter::now()` plus the deadline in cycles, and clears it after. Every CPU's timer tick
(`sched_init::on_timer_tick`, which the PIT and every LAPIC path run) checks it without a lock, so a
test that hangs with IF=0 on one CPU is caught by another's tick; the first tick to claim a passed
deadline, through one compare-exchange, prints `vibeOS: ktest: FAIL <name>: deadline` from the
interrupt and panics, so the dump shows where the test stood. The opt-in `ktest_deadline_hang`
(500 ms) holds IF=0 and spins; when `VIBEOS_KTEST` is unset and the tier runs at `-smp 2` or more,
`make test-kernel`'s expect-fail boot `ktest_deadline_trip` selects it and requires, in order, its
`run` line, the deadline `FAIL` line, a panic signature and `vibeOS: panic: halted`, and no `ok`
line (`check_deadline_trip`). A CPU spinning with IF=0 never takes the panic's stop IPI, so that
boot checks only those lines, not the other CPUs' state.

Selection (BOOT.md §3.2). `vibeos.ktest=` (`VIBEOS_KTEST`) takes a comma-separated list of globs,
`*` matching any run of characters and `?` one; with no item every row not marked opt-in runs. A row
marked `.opt_in()` runs only when an item without a wildcard is its name. `vibeos.ktest_repeat=`
(`VIBEOS_KTEST_REPEAT`) runs the selection 1 to 1000 times in one boot, pass after pass, and a row
marked `.once()`, one that consumes state it cannot restore (a boot line in the log ring, a
`BootCell`, a PCI claim, a cold cache block, a fixed file or snapshot name), runs in the first pass
only. `begin` counts the runs after both, from the same predicates as the runner's loop, and no
timeout scales with the repeat count. A repeat value outside 1 to 1000, or a run count past a `u32`,
prints `vibeOS: ktest: bad option vibeos.ktest_repeat=<value>` before `begin` and exits `0x11`; a
selection with no runs prints `begin 0`, then `end`, and exits `0x11`, and the harness fails both.
The portable half, `vibeos::ktest`, parses and counts, with host tests. A boot that does not run
`block_persist` needs no persist line and gets no persist reboot. When `VIBEOS_KTEST` is unset,
`make test-kernel` also runs the proof boots, each on a fresh disk: `ktest_select_boot`
(`vibeos.ktest=ktest_*,ktest_optin_probe,log_boot_level`, repeat 3, `loglevel=8`), which requires
exactly its expected runs (`check_select_run`), the opt-in and once rows included, and no run of
`ktest_deadline_hang`; and at `-smp 2` the repeat boot, `reap_many_via_idle` 20 times (F074).
Each records its verdict in the tier's results file as a `marker`.
After its first boot, and whatever `VIBEOS_KTEST` holds, `make test-kernel` also boots with
`-machine pc,hpet=off` and `-cpu <model>,-tsc-deadline`, limited by `vibeos.ktest=` to the opt-in
tests that need the PIT tick (`pit_tick_rate`), and requires `lapic_timer ok (pit)` and an `ok` line
for each (`run_ktest.hpet_off_boot`).
`isa-debug-exit` at I/O port `0xf4` maps a written value to host exit status `(value << 1) | 1`:

| Write | Host exit | Meaning |
|-------|-----------|---------|
| `0x10` | 33 | all tests passed |
| `0x11` | 35 | at least one test failed |

The harness retries nothing (ROADMAP §10.2, F021): `run_ktest.py` boots each configuration once for
the first boot and once for the persist reboot, `run_e2e.py` boots each variant once, and a timeout, a
`FAIL` line, a panic signature, or a missing marker fails the tier, in `make test-smp-stress` as in
every other tier. The results file keeps its `retries` list, which stays empty, and `check_ticks.py`
still fails a pull request whose results list a retry (ROADMAP §10.9).

The verdict counts runs (ROADMAP §10.2). `check_ktest_output` reads `begin <n>`, then pairs each
`run <name>` with the next result line (`ok`, `FAIL` or `skip`) for the same name, by order, since a
repeated test reuses its name. A run left open by the next `run` or by `end`, a result with no open
run, a result for another name, and a count of runs or results other than `<n>` each fail, naming
the test; info lines are never results. There is no whole-run deadline: `VIBEOS_TIMEOUT` is a boot
allowance, `BOOT_ALLOWANCE_S` (60 s) in every driver, which bounds each stretch of a boot in which no
test runs, from QEMU's start to `begin` and from `end` to QEMU's exit, and the whole of an e2e, ps2
or crash boot. From `begin` to `end`, `harness.KtestDeadlines` gives a run the deadline its line
printed plus 5 s, and each gap between lines 5 s, all multiplied by `EnvConfig.timeout_scale`, which
`env_config` sets to 1 in every tier (ROADMAP §17.6 makes it a setting); other lines extend no
deadline. That backstops the in-guest deadline, which a CPU wedged with IF=0 never checks. A timeout
fails with `ktest hung in <name>` for the last run line, or names the stretch outside `begin` and
`end`, and the serial tail it prints ends with the partial line the guest was writing:
`DeadlineReader` returns buffered bytes without a newline as a `partial` event before it reports the
timeout. Adding tests changes no timeout, and a test that needs longer carries a registry override,
reviewed as code; `make test-smp-stress` sets no longer timeout. Every driver's QEMU monitor
directory (`vibeos-mon-*`) is removed when the driver exits. The `utest_*` lines of ROADMAP §10.5 follow the same protocol.

Skips are first class and carry their reason on the `ktest: skip <name>: <reason>` line. Every skip
names what the configuration lacks: `no AP`, `no virtio-blk`, `no virtio-rng`, `no e1000e`, `no edu`,
`no smep/smap/umip`, `pit owns tick`, `pic fallback`, `rtc unread`, and `no invariant tsc` (the `Outcome::Skip` reasons
in the in-guest test bodies, DESIGN §1.3). Destructive exception tests run inside `arch::catch` scopes, which longjmp out or
step RIP past the faulting instruction, instead of skipping.

Expected skips are data (ROADMAP §10.2). `tests/harness/skips.toml` holds one `[[skip]]` row per
test and condition: `name`, `reason` (the exact text of the skip line), and optional `arch`,
`accel`, `cpu`, `smp`, `mem`, `machine` and `host`, each one value or an array meaning any of them
(`smp` an integer), a field left out matching every value; an unknown key is an error.
`skips.launch_config` gives the configuration a boot launched: `arch` from
`qemu-system-<arch>`, the effective accelerator, `cpu`, `smp` and `mem` as passed (`cpu` compared as
the exact string), the `-machine` value (`pc` when there is none, `pc,hpet=off` with HPET off), and
the harness host. After `check_ktest_output`, `run_ktest.py` runs `skips.check_skips` on every ktest
boot, the persist reboot and the proof boots included: it fails, listing each problem, when a test
with a `run` line skips and no matching row names it with that reason, when a test of a matching
row runs instead of skipping, and when a test that `VIBEOS_KTEST` names without a glob character
skips, since such a test must run whatever the file says (ROADMAP §12.3). So a lost `-device` or a
regressed detection that turns tests into skips fails the tier. Tests a run does not select print
no run line and need no row. The file shrinks only by an edit, each row a `Gate-change:` trailer, and
a box that adds a test which skips somewhere adds its rows in the same commit. The first rows hold
the skips of each configuration that `make test`, the nightly KVM leg and the weekly job launch.

Besides the verdicts, `run_ktest.py` reads these lines of the in-guest tests themselves:

```
vibeOS: ktest: serial whole <i> of 1000 <pad>
vibeOS: ktest: serial noise cpu<c> <n> <pad>
vibeOS: ktest: serial noise klog cpu<c> <n> <pad>
```

`serial_lines_whole` (ROADMAP §10.2, F138) pins a thread to each AP that prints the two `serial noise`
lines in a loop, while CPU 0 prints the 1,000 numbered `serial whole` lines; `<pad>` is a fixed
36-byte string. When the run holds `vibeOS: ktest: ok serial_lines_whole`, `_check_serial_whole`
requires each numbered line exactly once and fails on any line that holds `serial whole` or
`serial noise` but is not exactly one of these lines, a fragment of a line another CPU split. It reads
kernel lines; a log-ring replay of one (a dump's `logrec`) is checked for fragments and not counted.

```
vibeOS: ktest: serial frame a?b?c?d
?serial-frame open
vibeOS: ktest: serial frame after open
```

`serial_frame` (§2.6) prints a kernel line with a `\n`, a `\r` and a 0x1E inside it, then writes
`\x1eserial-frame open` through the console `write` that user descriptors reach, then a kernel line.
When the run holds `vibeOS: ktest: ok serial_frame`, `_check_serial_frame` requires the first framed
with each of the three as `?`, the second as the exact unframed line `?serial-frame open`, and the
third framed on a later line of its own, since the kernel breaks the open user line first.

Kernel stack depth (ROADMAP §10.2, DESIGN §4.5):

```
vibeOS: stack: <size> used <used> of <budget> by tid <tid> <name>
vibeOS: stack: report <n> sizes <lost> lost
```

In `kernel_tests` builds `kva_init::alloc_guarded_stack` fills every new guarded stack with
`vibeos::sched::stack_depth::PATTERN`: `spawn_inner`'s stacks, AP idle stacks and the registry's
64 KiB stack. `cached_stack` fills a stack it takes from a CPU's stack cache again, after zeroing
it and before the new thread's first frame goes on it. The switch tail (`finish_switch`) scans the
dead-stack slot's stack before it caches it or links it onto the dead list, and records the bytes
from the lowest word that no longer holds the pattern to the top, for the thread that just switched
off it. Just before `vibeOS: ktest: end` the registry scans every live thread's stack, one `SCHED`
section per thread (`thread_init::testing::scan_live_stacks`), and prints one `stack:` line per
stack size with the deepest use seen and the thread that reached it, then the report line: `<n>`
sizes, and `<lost>` records of a ninth size the table (8 sizes) had no room for. The budget is the
stack's size minus 4 KiB, the room DESIGN §4.5 keeps for a hard-IRQ top half and its entry frame.
`run_ktest.py`'s `check_stack_depth` fails a boot that has no report line, a `lost` other than 0,
a size line count other than `<n>`, or a use over its stack's budget, and appends the lines to
`$GITHUB_STEP_SUMMARY` when it is set, one block per boot: they are the budget's evidence. A top
half with its entry frame over 4 KiB that pushes a path over is DESIGN §4.5's fallback, per-CPU
interrupt stacks. `stack_depth_exit_scan` spawns a 16 KiB worker that puts 4 KiB on its stack and
requires its exit record between 4 KiB and the budget. When `VIBEOS_KTEST` is unset, `make
test-kernel` also runs the planted stack boot (`_planted_boot`, through `_single_test_boot`, which
boots one test on a fresh disk): the opt-in `stack_depth_planted`, whose thread `stack-plant`
recurses 13 levels of a 1 KiB array with IF=0, so no top half lands at depth, and requires an exit
record of at least 13 KiB. That boot passes only when `check_planted` finds the report whole and
`stack-plant` the one use over budget; it records `stack_depth_planted` in the results file.
Then the FAT stack boot (`_fat_boot`, ROADMAP §10.4, F058): the opt-in `fat_vda_16k_stack` alone
on a fresh disk, since it overwrites `vda`'s first 256 KiB with a FAT32 image through the block
cache. Its worker, started with `spawn_on`'s 16 KiB stack on a CPU that has a virtio-blk queue
vector (`virtio_blk_init::queue_vector`), mounts `vda` through the File API, writes 64 KiB at offset
100 and reads it back, and unmounts; while it writes, `fs::ktest::on_cache_write`, which `fat_init`'s
`Io::write` calls before each block-cache write, sends that CPU a self-IPI on the vector whenever IF
is on, so the virtio-blk top half lands on the write path. The test requires at least 64 self-IPIs
and no send error, at least as many new top-half runs, and the worker's exit depth within budget;
the boot requires its `ok` line and the stack check as every boot does.
Then the two virtio-blk failure boots (ROADMAP §10.11, F046), each through `_single_test_boot`
with its own device tuple, whose `vda` is a 4 MiB pattern image (`harness.make_pattern_disk`, every
byte of sector n `(n & 0xFF) ^ 0xA5`, so no GPT is stamped and no partition marker is required):
the opt-in `vblk_readonly` on a `readonly=on` image (`_vblk_readonly_boot`), and the opt-in
`vblk_bad_sector` on an image behind QEMU's `blkdebug`, which fails every read of sector 4096
(`_vblk_bad_sector_boot`). Each boot requires its one `ok` line.

The IF-off tracer build (ROADMAP §10.3, [INVARIANTS.md §2.9](INVARIANTS.md#29-preemption-and-interrupt-state)
rule 2). The `irqoff` Cargo feature is a measurement build, never a published image: the `irqoff`
variant is production features plus `irqoff` (`build/vibeos-irqoff.iso`), and `ktest-irqoff` is
`kernel_tests` plus `irqoff` (`build/vibeos-ktest-irqoff.iso`). `make test-irqoff` runs
`run_ktest.py` on the second and `run_e2e.py` on the first, both with `VIBEOS_TIER=test-irqoff`;
it is not part of `make test`, since the nightly job runs it. Under TCG it adds `-icount shift=0`
and `VIBEOS_SMP=1`, so guest time counts instructions and 100,000 ns is exactly the rule's bound;
under KVM (`VIBEOS_QEMU_ACCEL=kvm`) it adds neither and the numbers are wall time. The kernel
prints `vibeOS: irqoff: on bound <n> ns` after `irq: enabled`, and a reporter thread prints each
changed site's cumulative `site`, `over`, `deliberate` and `unmatched` lines every 100 ms of guest
time; the ktest runner reports once more before `ktest: end`. The e2e driver quits QEMU at its last
marker, so stretches after the last report of an e2e boot are not seen. `tests/harness/irqoff.py`
reads every boot's lines: a `test-irqoff` boot without the `on` line is the wrong ISO and fails; it
appends a table to `$GITHUB_STEP_SUMMARY` (under TCG the sites over the bound and the totals; under
KVM every site's max and p99, with no threshold, since its host can deschedule a vCPU mid-stretch)
and writes the rows to the results file's `irqoff` section, with the e2e driver appending to the
ktest driver's file (`VIBEOS_RESULTS_APPEND=1`). A test or test hook that holds IF off on purpose
takes `sched::irqoff::deliberate(reason)` (C-IRQOFF-GUARD, `kernel_tests` only), which marks its
stretch so it is never over and prints it on a `deliberate` line; `irqoff_logs_long_stretch` and
`irqoff_deliberate_is_exempt`, registered only in this build, check the tracer and the guard. A
test longjmped out of by `arch::catch` skips its guards' drops, so its stretch shows as
`unmatched`. Under `-smp 1` a test that needs a second CPU skips with its "no AP" reason, and each
such skip has a `smp = 1` row in `tests/harness/skips.toml` (C-SKIPS).

When a test fails, print enough to diagnose it without a rerun. A failing test that only prints its
name costs a full debug cycle to learn anything.

Keyboard IRQ regressions (#66). `kbd_gsi_unmasked` requires a live IOAPIC route (fails if PIC IRQ1
is the fallback after the LAPIC already masked the 8259). `kbd_8042_clock` reads the live controller
byte (clock on, INT1 on). `kbd_ps2_irq` writes 8042 command `0xD2` (present the next data byte as
keyboard input) with scancode `0x1E` and expects `a` on the PS/2 ring with interrupts on — handler,
INT1, GSI, ISR, decoder. Command `0xD2` does not exercise the device clock; that is
`kbd_8042_clock` plus e2e / `make test-ps2` `sendkey`. Serial mux cannot satisfy `kbd_ps2_irq`.

## 8.3 End to end

Boot the real ISO, capture serial, assert the boot contract. This is the test that notices when
something two subsystems away breaks.

### Marker contract

The contract is the `contract` rows of the marker registry,
[`tests/contract/markers.toml`](../tests/contract/markers.toml) (ROADMAP §10.2), in their `order`:
`boot_contract_markers()` in `tests/harness/harness.py` builds each configuration's list from the rows
whose `when` holds in it, and the paragraphs below give the rules the rows encode (the calibration
source, the LAPIC timer mode, the per-AP pairs, and the partition children). The `_start` table in
[section 3.3](BOOT.md#33-_start-order) says why each step sits where it does. The contract grows one
phase at a time: a phase adds its rows in the same commit that prints the lines, and nothing is ever
removed silently. Every contract line is the kernel's except `shell ready`, which `/bin/sh` prints in
the production ISO. The harness matches a row only on its `source`'s side of the frame (§2.6): the
kernel's lines when framed, and a line a user program prints (`shell ready`, `user: tests begin`, `ok`
and `fail`, `user: dup ok`, the `init:` lines, and the console-input replies) only when unframed.

`vibeOS: serial online` is the kernel's first serial line: `run_e2e.py` fails when a kernel line (one
starting `vibeOS:`) comes before it; Limine's or the firmware's output may precede it.

Live e2e through Phase 6 slice A asserts through `idt ok`, then `per_cpu: bsp ready`,
then `acpi: xsdt`, then `time: tsc <n>/ms`, then `time: lapic_timer ok (<mode>)`, then
`sched: cpu0 ready`, then `irq: enabled`, then for each AP `sched: cpu<i> ready`
followed by `smp: ap online`, then `smp: done`, then `console ok`, then
`pci: <n> devices`, then `block: <name> <n> sectors`, then `/bin/tests`' `user: tests ok`, then
`shell ready`.
`boot: phase1 done` was a Phase 1–4 stand-in and is no longer in the contract; the
trailing marker is `shell ready`. After that, the same ISO is booted again and the
harness types `echo serial-ok` on COM1 and `echo ps2-ok` via QEMU `sendkey` (i8042 /
IRQ1, the window-keyboard path). Both replies are required. `make test-ps2` is that
second boot alone. SMP stays before console; the old
table that listed console as step 15 before SMP was drift and is gone.
The harness pins `<mode>` for the QEMU config: TCG (CI, `make test`) cannot
advertise `CPUID.01H:ECX[24]`, so `-cpu max` expects `periodic`; `-machine pc,hpet=off`
expects `pit`; KVM `-cpu max` expects `tsc-deadline`. Default QEMU also requires
the diagnostic `time: calibrated hpet <n>/ms`; `make test-e2e-pit` asserts
`calibrated pit` instead. `make test-lapic-fallback`
(`-cpu qemu64,-tsc-deadline`) runs in-guest tests on the periodic path.

In the production ISO, `shell ready` is written from ring 3 by `/bin/sh`, which `/sbin/init` starts
after waiting for `/bin/tests`. Every e2e variant that reaches the shell requires `/bin/tests`'
unframed `user: tests ok` before `shell ready` (the `user_tests_ok` row), and every driver fails the
run on an unframed `user: tests fail`, so a failing `/bin/tests` fails the boot (ROADMAP §10.2,
F073). `init` passes a status pointer to `wait4` and, when the status word is nonzero (an exit code
other than 0, or a signal), prints `init: /bin/tests exited <status>` on fd 2 before it starts
`/bin/sh`: a registered failure line, so a `/bin/tests` that dies before its last line fails the
boot too.

`smp: done` before `shell ready` is deliberate. Put SMP bring-up after the shell starts and an AP
failure becomes invisible, because the harness sees its last marker and passes. `pci: <n> devices`
sits between `console ok` and `shell ready` so `lspci` is registered before the prompt. The ramdisk
`block: <name> <n> sectors` line sits after PCI and still before the shell. Partition children emit
`block: <parent>p<N> <n> sectors` after the parent (e2e: `ram0p1`, `ram0p2`). virtio-blk adds
`block: vda <n> sectors` and `vdapN`, and `block: vdb <n> sectors`, when the ktest disks are present (not on the production e2e `pc`
set). The same blind spot follows the last marker: writeback, deferred reclaim, and vibefs commits
keep running after `shell ready`, and a panic there is invisible to a harness that stops reading at
it. So the console-input boot keeps reading serial for 3 s after its last reply (`CONSOLE_TAIL_S`)
and fails on a panic signature in that window, or on QEMU's exit, before it quits QEMU.

With `-smp N`, additionally:

- for each AP `i` in `1..N`, `vibeOS: sched: cpu<i> ready` then `vibeOS: smp: ap online`, in order,
  before `smp: done`: exactly these `N-1` `ap online` lines, and an extra one before or after
  `smp: done` fails (F141). The count is `smp_done`'s `exactly_before` in `boot_contract_markers`,
  and it covers `-smp 1`, which has no pair
- `vibeOS: sched: cpu<i> ready` for every `i` in `0..N`
- `vibeOS: time: lapic_timer ok (<mode>)` naming the selected timer path
  (`tsc-deadline`, `periodic`, or `pit`) rather than inferring it

e2e also reads the boot log's memory diagnostics, the registry's `pmm:` and `meminfo:` rows, which
print before `sched: cpu0 ready`, in every production mode (default, `EXPECT_PIT`, highmem, and UEFI;
`check_meminfo` in `run_e2e.py`). Each `meminfo:` line (told apart by its text up to the first digit) and each `pmm:` line appears
once; the `meminfo:` frame total equals the `pmm: <n> total` line's; free is at most the
`pmm: <n> free 4KiB frames` count; used is total minus free; and heap use is at most heap capacity.

Two diagnostics come from the kernel command line (BOOT.md §3.2): the kernel prints
`vibeOS: boot: cmdline: <text>` once, after `limine: rev <n> ok` and before `pmm:` (ROADMAP §10.2),
and with `vibeos.strace=1` each syscall that returns prints `user: syscall <name> nr=<n> = <ret>`
(ROADMAP §10.7). `make test-e2e-strace` boots the production ISO with `VIBEOS_CMDLINE=vibeos.strace=1`
and fails unless the echo reads `limine.conf`'s `cmdline:` value, one space, then the harness's fw_cfg
words, comes before the first trace line, every trace line has that form, and the first `write` line
reads `user: syscall write nr=1 = <int>`.

The list above is the contract of a boot through Limine. Planned (ROADMAP §25.4, §26.4): a boot
through the image's direct entry prints `vibeOS: boot: <path> entry ok`, where `<path>` is `kexec`,
`crash`, or `pvh`, in place of `limine: rev <n> ok`. A crash entry, ROADMAP §25.4's capture kernel,
boots with `maxcpus=1` and so prints no `smp: ap online` line, and its list ends at
`vibeOS: vmcore: written <n> bytes` in place of `shell ready`, after which it resets.

### Failing fast

Scan for the `failure` rows of [`tests/contract/markers.toml`](../tests/contract/markers.toml) and, in
a run that expects no panic, fail immediately with the captured line rather than waiting out the
timeout. A row whose placeholders all follow its last literal fails a line that holds its text up to
the first placeholder (`PANIC_SIGNATURES` in `tests/harness/harness.py`, and `frame.USER_FAILURES`
for the `user` rows); any other fails a line its pattern matches (`FAILURE_PATTERNS`). The rows are
the panic signatures (`panicked at`, `vibeOS: panic:`, the exception mnemonics, `double fault`,
`stack overflow`), the lines of a dump, and the halt reasons.

Match the exception mnemonics, not the phrase "page fault". Shell help text and log messages contain
English words, and a substring match on prose produces false failures that erode trust in the suite.

A registered failure line can also report a failure the kernel survived, so a run that shows one
would otherwise pass. The blocked-thread sweep's `vibeOS: sched: overdue tid <id>` (ROADMAP §10.7),
and `vibeOS: block: <dev> timeout` and `vibeOS: block: <dev> reset` (ROADMAP §12.5), are such rows;
planned (ROADMAP §25.5), the soft lockup, hard lockup, and hung-thread reports follow. A test that
provokes one on purpose declares it; in any other run it fails the run, since a recovery no test expected is a bug a timeout hides, such as a
lost kick ([section 10.4](BLOCK.md#104-virtio-blk)) that shows only as a 30 s pause.

The in-guest runner's failure lines fail a `make test-kernel` run, matched on framed lines only
(§8.2): `check_ktest_output` rejects every `vibeOS: ktest: FAIL <name>: <reason>` (a `test` row),
and two `failure` rows fail the run as soon as they print: the deadline failure signature
`vibeOS: ktest: FAIL <name>: deadline`, which a timer tick prints before it panics, and
`vibeOS: ktest: bad option <key>=<value>`, printed before `begin`. The deadline trip boot expects
both its failure and its panic (`run_qemu_until_exit(..., expect_fail=True)`, declared
`expect="panic"`) and checks them itself.

User programs print these strings too: the ROADMAP §10.5 runtime reports a panic as `panicked at` on
fd 2, and a fuzzer writes random bytes. The harness scans framed lines only (§2.6), and fails on
`user: tests fail` only when unframed. Before the kernel's first framed line it fails fast on Limine's
panic line, the one failure that cannot be framed: `PANIC`, optional ANSI colour codes, then `: `
(`frame.LIMINE_PANIC`). After the first framed line the same text is just a user line.

Expected-panic e2e matches boot markers only against the lines before the first panic signature (or
dump banner), so the dump's `vibeOS: logrec:` replay of earlier records cannot satisfy one, and a
marker still unmatched there fails the run (F141). From that line on it counts dump banners, the line
that opens a dump, and requires exactly one: the bare `vibeOS: panic:` of a Rust panic,
`vibeOS: exception: vector <n> rip=0x…`, `vibeOS: <kind> rip=0x…` for `#UD`, `nmi`, `#DB`, `#GP`,
`#PF`, `#DF`, and `#MC`, or `vibeOS: panic: reentered` (`DUMP_BANNER_RE`); the other signature lines
of a dump do not count. Before `panic: halted` the dump prints one
`vibeOS: panic: cpu N stopped (ipi|poll|nmi|panic)` line, with its `cpu N regs:` line, or one
`vibeOS: panic: cpu N not stopped` line for each other online CPU (§2.5 step 1). Two panic-path
variants boot the full contract through their armed line (`VIBEOS_PANIC_VARIANT`, `run_e2e`) and then
run `tests/harness/panic_dump.py`'s check on the dump: `make test-e2e-panic-nest` (`nest`) underflows
`irq_nest` and requires its own message, one banner, no `reentered` and one `halted` (ROADMAP §10.7,
F071); `make test-e2e-panic-stop` (`stop`, `VIBEOS_SMP=5`) panics CPUs 0 and 1 at once while CPU 2
prints `vibeOS: panic_stop: line <n>` with IF=0, CPU 3 waits with IF=0 on a lock CPU 0 holds, and CPU
4 spins with IF=0, and requires the other of CPUs 0 and 1 `stopped (panic)`, CPUs 2 and 3
`stopped (poll)`, CPU 4 `stopped (nmi)`, no numbered line after the first `vibeOS: panic:` line (a
`logrec:` replay does not count), and `vibeOS: panic_stop: owner nmi returned`, which the owner prints
once the NMI it sends itself after its message has come back (F135). After the dump the kernel
writes pvpanic's panicked event, and QEMU pauses the guest (`-action panic=pause`). The harness
reads through `vibeOS: panic: halted`, so the dump (regs, thread, last log records, backtrace) is in
the captured log, then gives QEMU up to 10 s (`PANIC_EXIT_S`) to report QMP `GUEST_PANICKED`, the
run's end; it reads serial on for 0.5 s after the event, since the QMP socket can beat the serial
pipe, checks the dump needles, and quits QEMU through QMP. A failed check keeps the guest core.

The event rule (ROADMAP §10.7), as built on x86_64 (`tests/harness/qmp.py`, C-QMP; aarch64's
`pvpanic-pci` is planned, ROADMAP §11.7). The panic path signals pvpanic
([§2.5](INVARIANTS.md#25-panic-policy) steps 6 and 7), QEMU runs with `-action panic=pause`, and the harness
reads QEMU's QMP events: every boot starts halted (`-S`) with a QMP socket, and the harness
connects, negotiates and only then sends `cont`, so no event is lost (`qmp.Session`, which all
three runners drive). Each run declares the end it expects (`QemuConfig.expect`): `none`, the
default; `panic`, for the expected-panic e2e, the `#GP` e2e, the panic-path variants and the
in-guest deadline trip; `reset`, for a line whose guest resets and boots again, with the `RESET`
events it allows in `QemuConfig.resets`; or `capture`, for a line whose panic reaches a capture
kernel. No Phase 10 run declares `reset` or `capture`; the later lines below use the machinery.
`GUEST_PANICKED` pauses the guest: a run that expects no panic takes a guest core, quits, and
fails; the expected-panic e2e checks its dump needles and quits; an `expect=reset` run sends
`cont`. QEMU reports
`GUEST_CRASHLOADED` without pausing: a run not declared `expect=capture` stops the guest, takes a
core, and fails, and an `expect=capture` run waits for the capture kernel's
`vibeOS: vmcore: written <n> bytes` line and its reset, which ends QEMU under `-no-reboot`. A panic
signature with no event, from a panic before the kernel has found its pvpanic device, fails a run
that expects no panic at once, and the harness takes the core after `vibeOS: panic: halted` or
10 s, whichever comes first. An `expect=reset` run boots without `-no-reboot`, fails on more QMP
`RESET` events than its line expects, and is judged by the markers its line names. Every timeout
takes a core. `expect=reset` and `expect=capture` runs take a core only when they fail: a core taken
after a crash jump still describes the crashed kernel, because a kernel entered through the crash
path never writes QEMU's `vmcoreinfo` device (ROADMAP §10.7). `tests/harness/test_qmp.py` replays
QMP event streams recorded from QEMU for each declaration (below).

On success, exit through the QEMU monitor's `quit` rather than waiting for the timeout. Two seconds
versus forty five, on every CI run and every local invocation.

### Harness

Python, standard library only. `subprocess` with its own timeout rather than shelling out to GNU
`timeout`, which does not exist on macOS. The harness helpers get their own unit tests, because a bug
in the test harness produces either false confidence or a debugging session in the wrong repository.
Both runners, `run_qemu_and_check` and `run_qemu_console_input`, read serial from a line source
(`tests/harness/linesource.py`): a QEMU child in a run, and in unit tests a `FakeLineSource` that
scripts the lines, the exit status, and QEMU's stderr, so the matcher every e2e run uses is tested
without QEMU (ROADMAP §10.2, F141). QEMU's stderr goes to a temporary file, apart from serial, so
serial lines carry only the guest's output. When QEMU exits before the last marker, the error names
the missing marker, QEMU's exit status, and the last 20 lines of its stderr, then the serial tail, so
a firmware QEMU could not load reads as that and not only as `missing marker 'serial_online'`
(F079); a timeout shows the stderr lines too when there are any.

When a run times out, or panics where the event rule (§8.3, Failing fast) says so, the harness
takes a guest core before it quits QEMU (`qmp.take_core`): it stops the guest, then QMP
`dump-guest-memory` with `"paging": false` and no format writes a physical ELF core, one
`NT_PRSTATUS` note per CPU and the kernel's VMCOREINFO note ([VMCOREINFO.md](VMCOREINFO.md)) beside
the memory. Never `-p`, whose mappings come from each vCPU's page tables at the time of the dump,
and never a kdump format, which `gdb` cannot read. The harness hands QEMU the write end of a pipe
through QMP `getfd` and dumps to `fd:vibeos-core` while host `zstd` compresses the read end, so no
uncompressed core touches the disk. Each such run gets a directory,
`build/cores/<arch>-<tier>/<seq>-<label>/`, with `core.zst`, `kernel.elf` (the named ELF behind the
ISO, `build/kernels/vibeos-<variant>.elf`) and `qemu-argv.txt`. `make test-qmp`
(`tests/harness/run_qmp.py`) re-records the QMP event streams `tests/harness/test_qmp.py` replays
(`tests/harness/fixtures/qmp/`, one per declaration, a `GUEST_CRASHLOADED` stream and an extra
`RESET` among them) on the QEMU it runs on and fails on any difference, checks that
`-machine q35`'s fw_cfg lists `etc/pvpanic-port`, and takes one core of the production ISO and
checks its notes. `python3 tests/harness/run_qmp.py --record tests/harness/fixtures/qmp`
regenerates the streams; the recorder drives QEMU through a qtest socket beside TCG with 64 KiB of
`hlt` as its firmware. When a tier fails, CI uploads its `build/cores/` as one artifact,
`cores-<arch>-<tier>` (the `tier` job's last step, `if: failure()`; the build job's
`prebuilt-<arch>` carries the named ELFs), which is public like every artifact of this repository
(DESIGN §1.5). So a job in a workflow that names an environment or a secret uploads no core, memory
dump or QEMU command line: `scripts/check_workflows.py` (`rule_no_core_upload_with_secrets`) fails
on one that does.

The core tool, `vmcore` (`tests/hostlib/src/bin/vmcore.rs` over `vibeos::log::vmcore`, ROADMAP
§10.7), reads such a core with the kernel ELF behind it. `make vmcore` builds it in the kernel's
profile, since debug assertions move fields of `Tcb` and `PerCpu` ([VMCOREINFO.md](VMCOREINFO.md)).
`vmcore report --core <file|-> --elf <kernel.elf> [--virt <out>] [--trace <out.json>]` reads the
core from a file or from `zstd -dc` on stdin into a sparse store of its non-zero 4 KiB pages, and
refuses by name a core whose VMCOREINFO `BUILD-ID` is not the ELF's GNU build id
(`vmcore: BUILD-ID mismatch: core <hex> elf <hex>`, exit 3), a core with no VMCOREINFO note (exit 4)
and an ELF with no build-id note (exit 1); an ELF32 core, which QEMU writes when a timeout comes
before the kernel reaches long mode, is refused too. It walks the crashed kernel's tables through
the roots the note names, decoding the portable crate's `#[repr(C)]` types at their `offset_of!`
offsets. Every run that takes a core prints the report after its serial tail
(`harness.core_report`, `failure_tail`), in this order:

- `sig: <message> @ <f0> < <f1> < <f2>`, the signature below;
- `core: <bytes> RAM in <k> segments`, `build-id: <hex>`, and `panic: cpu <n>: <line>` or
  `panic: none`;
- for each CPU, `cpu <n> apic <id> current <tid> idle <tid> runq [<tids>] regs slot|prstatus
  running|stopped (<how>)`, then its symbolized frame-pointer backtrace, one `  #<i> 0x<addr>
  <function>+0x<off>` line per frame (at most 24, walked as the dump walks, within the kernel half),
  starting from its crash-register slot when set (a CPU the dump stopped, and the dump's owner,
  which records where its dump began) and else from its `NT_PRSTATUS` note;
- `thread <tid> <state> ...` for each thread of the TCB table, with its CPU, pid and saved context;
- `log: last <k> of <n> (<d> dropped)`, then the last 64 records of the log ring, oldest first, a
  record caught between `Ring::push`'s stores printed `<torn>`;
- `trace: order global` or `trace: order per-cpu (<why>)`, the flight recorder's ordering (DESIGN
  §6.4), and the `--trace` and `--virt` files it wrote.

`--trace` writes every CPU's flight-recorder ring as one Chrome trace-event JSON timeline, which
Perfetto opens, with the timestamps in nanoseconds from the calibration the core holds. `--virt`
writes the virtually addressed core: `ET_CORE`, the core's notes, and one `PT_LOAD` per run of
present kernel-half mappings (`p_vaddr` the VA, `p_paddr` the PA; uncached mappings left out),
which `gdb` opens with the kernel ELF: `make debug CORE=<core.zst> [ELF=<kernel.elf>]` writes it to
`build/debug/core.virt`, and `gdb -x scripts/vibeos.gdb` then opens it instead of the stub.

The signature (`vibeos::log::vmcore::sig`, host-tested) is the same line for the same bug in every
build. Its message is the first line of the panic message, cut at 120 bytes without trailing
whitespace, which the dump's owner records in its `PanicLine` (an exception dump records its first
line without `vibeOS: `), with each maximal `0x[0-9A-Fa-f]+` or `[0-9]+` written `N`; with no panic
line it is `timeout`. Its CPU is the
one the panic line names, or on a timeout the lowest CPU whose current thread is not its idle
thread (CPU 0 when every CPU is idle). Its frames are the first three function names of that CPU's
backtrace, demangled without hash, `?` for an address with no symbol, never an address or offset;
on a panic, leading frames of the panic machinery are skipped (`PANIC_FRAMES`: `rust_begin_unwind`,
`core::panicking::`, `core::option::unwrap_failed`, `core::option::expect_failed`,
`core::result::unwrap_failed`, `core::slice::index::`, `core::str::slice_error_fail` and
`vibeos::log::panic::`, each a prefix).

The `hang_test` build (a test-only feature, the `hang` variant) hangs every CPU once `smp: done` and
the clocksource line have printed: one CPU prints `vibeOS: hang_test: armed` and spins holding a
spinlock with IF=0, and every other CPU spins on that lock with IF=0, so its report reads
`sig: timeout @ …hang_test::hold < …hang_test::arm < …boot_rest` with CPUs 1 up in
`hang_test::wait`. `make test-forensics` (§8.5) tests the forensics on its cores.

Every driver classifies each serial line through `tests/harness/frame.py` (DESIGN §2.6): a framed line
is the kernel's and is matched with its frame stripped, an unframed line is a user program's or the
loader's, and each driver checks the three failure tuples, the kernel's `PANIC_SIGNATURES` on framed
lines, `frame.USER_FAILURES` on unframed ones, and `frame.LIMINE_SIGNATURES` before the first framed
line. `/bin/tests` writes `\x1evibeOS: ktest: FAIL forged`, `\x1epanicked at forged` and
`\x1e#GP\x1eforged` to fd 1 and to fd 2; the run stays green, and `run_e2e.py`'s `_check_forged_lines`
requires each as an unframed line (`?vibeOS: ktest: FAIL forged`, `?panicked at forged`,
`?#GP?forged`) exactly twice and no framed line holding `forged`, and records `forged_user_lines`.

The `vibefs_crash` build (`fs::vibefs_crash::crash_loop`) prints no boot contract past its own lines,
which `run_vibefs_crash.py` knows:

| Line | Meaning |
|---|---|
| `vibeOS: vibefs: crash-ready` | `vda` is mounted at `/crash` and iteration 0 is committed |
| `vibeOS: vibefs: wr <n>` | iteration `n` (from 1) starts: `/crash/w` opened with `O_TRUNC`, 300 bytes of `(n + k) as u8` written, closed, then `sync_fs` |
| `vibeOS: vibefs: mount fail <err>` | failure line: `/crash` could not be made or `vda` not mounted; the guest halts |
| `vibeOS: vibefs: sync fail <err>` | failure line: an open, write, close or `sync_fs` of an iteration failed (`short write` for a short write); the guest halts |

`make test-vibefs-crash` first runs the hostlib tests (`nbd-cache`, `vibefs-cat`), then
`run_vibefs_crash.py` over the volatile-cache device (F080; ROADMAP §10.2). Each of 8 rounds
(`VIBEOS_CRASH_ROUNDS`; the run seed is `VIBEOS_CRASH_SEED` and every failure prints it with the
round) works in a short `mkdtemp` directory, since macOS allows 104 bytes of unix socket path:

1. `mkfs-vibefs` a 256 KiB image, the size `vibefs::tests::crash_workload_seeded_points` proves
   200 commits fit, require `fsck-vibefs` to print `errors 0 warnings 0`, read its generation `G`,
   and copy it to the served image.
2. Start `nbd-cache` on it with a seed from the run's RNG. It is an NBD server in hostlib that
   advertises only `HAS_FLAGS|SEND_FLUSH`, replies to a write when it arrives and to a flush
   `1 + xorshift(seed) % 20` ms later, and keeps reading, tracing and replying to the requests that
   arrive meanwhile, so a write sent before a flush completes is not covered by it. Its JSONL trace
   records each write and flush on arrival and each reply once sent, and `<trace>.data` holds the
   write payloads in trace order.
3. Boot the `vibefs_crash` build with the disk served over NBD (§8.4), `cache=` rotating through
   `writeback`, `none` and `writethrough`, and SIGKILL QEMU up to `CRASH_KILL_MAX_S` (0.05 s)
   after `vibeOS: vibefs: wr K`, K uniform in [1, 200]. After the kill the harness reads serial to
   EOF, so a `wr` line already in the pipe is not lost. The round fails unless QEMU died of that
   SIGKILL with the last `wr N` at least K, and unless `nbd-cache` then exits 0 (it exits 1 after
   a FUA, TRIM, unknown or out-of-range request).
4. Require that a replay of every traced write equals the served image: a kill loses no write the
   device received, under any of the three cache modes, so only images rebuilt from the trace can
   show a lost unflushed write. A write is durable once the device has replied to a flush it
   received after replying to that write. The harness rebuilds one image per write that changes a
   superblock slot (the writes durable when it arrived, plus that write; a write that changes part
   of a slot fails the round, and one that rewrites a slot unchanged, as Limine's BIOS stage does
   to LBA 0 before the kernel runs, is skipped) and one at the kill (the durable writes plus a
   seeded subset, each kept with probability 1/2, of the later ones).
5. On every image `fsck-vibefs` must print `errors 0 warnings 0`, and `vibefs-cat <img> /w` must
   return one iteration's content, iteration `i`, with `i` no older than the last iteration whose
   final flush the device replied before that image's crash point. A super write's iteration is its
   generation − `G` − 1, and its commit's final flush is the first flush that arrived after its
   reply. The kill image must also hold `N` or `N` − 1, and the trace must show `N` − 1's final
   flush replied, since `wr N` follows that commit's `sync_fs`.

A failed round keeps its directory (base and served images, trace, and the failing image named
`super@<index>.img` or `kill.img`) and prints its path. Each round logs K, N, the cache mode, the
number of images checked, and the trace's write and flush counts.

## 8.4 QEMU flags

| Context | Flags |
|---------|-------|
| e2e | `-cdrom build/vibeos.iso -m 128M -smp 2 -cpu max -no-reboot -display none -serial stdio -monitor unix:...,server=on,wait=off -qmp unix:...,server=on,wait=off -S -accel tcg -device pvpanic -device vmcoreinfo -action panic=pause` (`harness.qemu_argv`; the two forensics devices, `FORENSICS_DEVICES`, are on every x86_64 boot: `vmcoreinfo` takes the kernel's note, [VMCOREINFO.md](VMCOREINFO.md); every harness boot starts halted, `-S`, until `cont` on its QMP socket, and `-action panic=pause`, `PANIC_ACTION`, keeps a panicked guest up for its core, §8.3) |
| UEFI (`VIBEOS_BIOS=uefi`, `make test-e2e-uefi`) | as e2e plus `-drive if=pflash,format=raw,unit=0,readonly=on,file=<code>` and `-drive if=pflash,format=raw,unit=1,file=<copy>`, where `<copy>` is a fresh copy of the pair's variable-store template made for each QEMU start (`harness.new_vars_copy`, in one per-process temporary directory that exit removes), and `-boot order=d,menu=off` with `-fw_cfg` entries turning off OVMF's PXE and setup (`harness.OVMF_BOOT_ARGS`). A comma in a path is doubled. Never `-bios` |
| `make run`, `make run-panic`, `make debug` | e2e's argv, so with `-device pvpanic` and `-device vmcoreinfo`, from `tests/harness/run_interactive.py` (`run`, `panic`, `debug`), which builds it with `env_config` and `harness.qemu_argv` and adds no `-monitor`, `-qmp` or `-S`: `make run` opens a display window instead of `-display none`, `make run-panic` boots `build/vibeos-panic.iso` with `-display none`, and `make debug` is `make run` plus `-s -S`. COM1 is the terminal (`-serial stdio`), and the launcher ignores `SIGINT` while QEMU runs |
| ktest | as e2e (so with `-device pvpanic` and `-device vmcoreinfo`) plus `-device isa-debug-exit,iobase=0xf4,iosize=0x04`, `-device e1000e`, `-device edu` (planned, ROADMAP §11.7: `-device edu,dma_mask=0xFFFFFFFF` on both architectures), `-device virtio-rng-pci,disable-legacy=on`, a second virtio-rng at `00:1d.0` (`-device virtio-rng-pci,disable-legacy=on,addr=0x1d`, which the driver refuses since one is bound: `rng_second_probe_refused`), a virtio-blk at `00:1e.0` on a 1 MiB `null-co` node (`-blockdev driver=null-co,node-name=probeblk,size=1048576,read-zeroes=on` + `-device virtio-blk-pci,drive=probeblk,disable-legacy=on,addr=0x1e`), whose probe a `kernel_tests` hook fails after `QENABLE` at every boot (`virtio_probe_fail_quiesces`), two virtio-blk disks (`-drive file=…,if=none,id=vibehd,format=raw,cache=writeback,discard=unmap` + `-device virtio-blk-pci,drive=vibehd,disable-legacy=on,num-queues=<smp>`, then the same for a blank 1 MiB `vibehd1`, which binds as `vdb`; the proof boots take only the first, `harness.ktest_devices(..., extra_disks=...)`). Extra NICs/edu/virtio are ktest-only; e2e stays the default `pc` set (`pci: 6 devices`); neither forensics device is PCI. After a green first boot the harness reboots the same disk and requires `vibeOS: persist: intact`. |
| vibefs crash | as e2e plus `-boot order=d` and the volatile-cache device: `-drive file.driver=nbd,file.server.type=unix,file.server.path=<sock>,format=raw,if=none,id=vibehd,cache=<writeback\|none\|writethrough>` + `-device virtio-blk-pci,drive=vibehd,disable-legacy=on,num-queues=<smp>,write-cache=on` (`harness.virtio_blk_args(..., nbd=True)`). QEMU 8.2 accepts the `file.driver=nbd` form; `cache=unsafe` is refused, since it drops flushes |
| LAPIC fallback | `-cpu qemu64,-tsc-deadline` (`LAPIC_FALLBACK_CPU` in the Makefile) |
| KVM leg (nightly `kvm` job, §8.6) | `-accel kvm -cpu max,+invtsc` through `VIBEOS_QEMU_ACCEL=kvm` and `VIBEOS_QEMU_CPU=max,+invtsc`, since QEMU leaves invariant TSC out of its default migratable vCPU even under KVM; `/dev/kvm` is opened to the runner user by GitHub's documented udev rule; the LAPIC fallback runs on `qemu64,+invtsc,-tsc-deadline` (`make test-lapic-fallback LAPIC_FALLBACK_CPU=…`), so the invariant-TSC check still applies and the mode is `periodic` |
| SMP stress | `-smp 4` |
| aarch64 (`ARCH=aarch64`) | Planned (ROADMAP §11.7): `qemu-system-aarch64 -machine virt,acpi=off,gic-version=3`, with `-cpu max` under TCG or `-cpu host` under HVF (`virt` defaults to the 32-bit `cortex-a15`); the §10.2 probe's firmware code read-only on pflash unit 0 and a per-run copy of its variable-store template on unit 1; the ISO on a CD-ROM, `-device virtio-scsi-pci -device scsi-cd,drive=cd0 -drive if=none,id=cd0,media=cdrom,readonly=on,file=<iso>`, so the ktest disks are the only virtio-blk devices; and `-device ramfb`, `virtio-keyboard-pci`, `virtio-tablet-pci`, `pvpanic-pci`, and `vmcoreinfo` |
| Interrupt debugging | `-d int,cpu_reset`, plus `-machine q35` when chipset behavior matters |

TCG is the per-push accelerator: the harness and `make test` default to `-accel tcg`, and KVM runs
nightly on the KVM leg (§8.6). Set the accelerator with `VIBEOS_QEMU_ACCEL`, never through
`VIBEOS_QEMU_EXTRA`, so `run_ktest.py`'s `lapic_timer` mode check and the skips' `accel` rows see
it. Every ktest boot requires the `lapic_timer` mode its CPU string, HPET and accelerator imply
(`run_ktest.check_boot_cpu`, as `run_e2e.py`'s boot contract does), and a boot whose CPU string
asks for invariant TSC (`+invtsc` or `invtsc=on`) fails when the guest prints `vibeOS: time:
invariant tsc absent` (DESIGN §2.6), recording the `invariant_tsc` results marker either way.

`-no-reboot` matters: a triple fault otherwise reboots and loops, and the serial log fills with
repeated boot attempts instead of stopping at the interesting one. A run declared
`expect="reset"` (§8.3) boots without it (`qemu_argv`) and counts QMP `RESET` events instead, so a
reset beyond the run's `QemuConfig.resets` still fails it.

All `VIBEOS_*` overrides are read in `tests/harness/harness.py` (`env_config` / `env_flag` /
`env_int`), which holds their only defaults. Drivers do not parse the environment, and the Makefile
sets none of them: `make run`, `make run-panic` and `make debug` honour the same settings through
`run_interactive.py`.

| Variable | Default | Who honours it |
|----------|---------|----------------|
| `VIBEOS_ISO` | per driver, from `harness.default_iso(variant)` (`build/vibeos.iso`, `build/vibeos-ktest.iso`, `build/vibeos-vibefs-crash.iso`) | all drivers; `run_interactive` |
| `VIBEOS_SMP` | `2` | all drivers; `run_interactive` |
| `VIBEOS_QEMU_CPU` | `max` | all drivers; `run_interactive` |
| `VIBEOS_MEM` | `128M` | all drivers; `run_interactive` |
| `VIBEOS_BIOS` | unset or `seabios`: SeaBIOS; `uefi`: the x86_64 firmware pair the probe finds, on pflash; anything else fails and names `VIBEOS_FW_X86_64` | all drivers; `run_interactive` |
| `VIBEOS_FW_X86_64` | probed (the firmware table below) | all drivers and `run_interactive` under `VIBEOS_BIOS=uefi`; `make test-e2e-uefi`; `setup.sh` |
| `VIBEOS_FW_AARCH64` | probed (the firmware table below) | `run_interactive.py firmware aarch64` and `setup.sh`; planned (ROADMAP §11.7): the aarch64 QEMU line |
| `VIBEOS_QEMU_ACCEL` | `tcg` (empty omits `-accel`) | all drivers; `run_interactive` |
| `VIBEOS_TIMEOUT` | `60` in every driver: the §8.2 boot allowance: the whole of an e2e, ps2 or crash boot, and a ktest boot before `begin` and after `end` | all drivers; `run_interactive` only when set |
| `VIBEOS_QEMU_EXTRA` | empty | all drivers; `run_interactive` |
| `VIBEOS_TIER` | `adhoc`; each `make test-*` recipe sets its target name | all drivers, which write `build/results/<arch>-<tier>.json` (schema 1, `tests/harness/results.py`) |
| `VIBEOS_EXPECT_PANIC` | off (`""` / `0`) | `run_e2e` |
| `VIBEOS_GP_TEST` | off | `run_e2e` |
| `VIBEOS_EXPECT_PIT` | off | `run_e2e` |
| `VIBEOS_MCE_TEST` | off | `run_e2e` |
| `VIBEOS_SKIP_PERSIST` | off | `run_ktest` |
| `VIBEOS_KTEST` | unset; `vibeos.ktest=<value>` (BOOT.md §3.2), a comma-separated glob list that selects the in-guest tests; when set, `run_ktest` boots that selection alone | `run_ktest` (every driver's fw_cfg string) |
| `VIBEOS_KTEST_REPEAT` | 1; `vibeos.ktest_repeat=<n>`, 1 to 1000, which the kernel checks | `run_ktest` (every driver's fw_cfg string) |
| `VIBEOS_CRASH_ROUNDS` | `8` | `run_vibefs_crash` |
| `VIBEOS_CRASH_SEED` | time-based | `run_vibefs_crash` |
| `VIBEOS_MKFS` | `mkfs-vibefs` | `run_vibefs_crash`, `run_e2e` (the `test-e2e` tier's vda images) |
| `VIBEOS_FSCK` | `fsck-vibefs` | `run_vibefs_crash` |
| `VIBEOS_NBD_CACHE` | `nbd-cache` | `run_vibefs_crash` |
| `VIBEOS_VIBEFS_CAT` | `vibefs-cat` | `run_vibefs_crash` |
| `VIBEOS_PREBUILT` | unset | the Makefile: `1` makes `make test-*` use the files `make prebuilt` packed (`build/prebuilt.tar`, unpacked in place) and build nothing, as a CI tier job does (§8.6) |
| `VIBEOS_QEMU_VERSION` | unset; the QEMU version a CI job pins | `qemu_argv`, only under `CI` on Linux: it fails before the first boot when `qemu-system-x86_64 --version` differs, or when the variable is unset (§8.6, Runners) |

UEFI firmware is found by one probe, `harness.probe_firmware(arch)`, which reads one table of
(code image, variable-store template) pairs per architecture, `harness.FIRMWARE_TABLE`, in this
order (ROADMAP §10.2, I1, F079):

| Architecture | Code image | Variable-store template | Directories |
|--------------|------------|-------------------------|-------------|
| x86_64 | `OVMF_CODE_4M.fd` | `OVMF_VARS_4M.fd` | `/usr/share/OVMF` (Ubuntu's `ovmf`) |
| x86_64 | `edk2-x86_64-code.fd` | `edk2-i386-vars.fd` | `<prefix>/share/qemu` (Homebrew's `qemu`) |
| aarch64 | `AAVMF_CODE.fd` | `AAVMF_VARS.fd` | `/usr/share/AAVMF` (Ubuntu's `qemu-efi-aarch64`) |
| aarch64 | `edk2-aarch64-code.fd` | `edk2-arm-vars.fd` | `<prefix>/share/qemu` (Homebrew's `qemu`) |

Homebrew's `<prefix>` is `$HOMEBREW_PREFIX` when set, then `/opt/homebrew`, then `/usr/local`.
Homebrew ships no vars file named for either 64-bit architecture, so its 32-bit ones pair with the
64-bit code. The first row whose code image exists decides: when its template is missing the probe
fails and names both paths, and never falls through to a later row. `VIBEOS_FW_X86_64` and
`VIBEOS_FW_AARCH64` override the probe with a code image, whose template is its row's in the same
directory; a missing file, or an image no row of that architecture names, fails. Secure-boot builds
need SMM and `q35`, so the table leaves them out. The code image boots read-only from pflash, so a
code-only image boots whatever its size (Homebrew's `edk2-x86_64-code.fd` is 0x37C000 bytes, which
`-bios` refuses because it is no multiple of 64 KiB).

`python3 tests/harness/run_interactive.py firmware <arch>` prints the pair and exits 0, exits 1 and
names the directories it searched when none is installed, and exits 2 on a probe error; `setup.sh`
reports it for both architectures. `make test-e2e-uefi` runs it and the harness in one shell line:
0 runs the boot contract with `VIBEOS_BIOS=uefi` and passes its status on, 1 prints
`test-e2e-uefi: SKIP: …` and exits 0, or prints `test-e2e-uefi: FAIL: …` and fails when `CI` is
set, and 2 fails.

`make debug` builds the production ISO and starts it as `make run` does, with `-s -S`: QEMU opens a
gdb stub on TCP port 1234 (every interface, as `-s` does) and holds the CPUs until gdb continues.
The launcher first writes `build/debug/symbols.gdb`, which loads the kernel ELF
(`build/kernels/vibeos-default.elf`) with `file` and each initrd program in the Makefile's
`DEBUG_USER_ELFS` with `add-symbol-file <elf> -o 0`. From the repository root, in a second terminal,
`gdb -x scripts/vibeos.gdb` (Homebrew's `x86_64-elf-gdb` on macOS) sources that file and connects.
The kernel is not in memory when QEMU starts, so set a hardware breakpoint (`hbreak _start`) and
`continue`; a software `break` would be written into memory Limine later overwrites. Today's user
ELFs carry no symbols, so gdb warns that they add none.

## 8.5 Make targets

`make help` prints the live inventory. Do not hand-maintain a second list here.

`make check` is the fast local gate (rustfmt `--check`; clippy `-D warnings` on `vibeos-core` and hostlib
for the host, on `vibeos-core` for `x86_64-unknown-none`, and on the kernel with its default features; host
unit tests, the `release_assert_` host tests again with debug assertions off, harness unit tests,
ruff and mypy; a production-feature link under the `hookcheck`
profile, whose ELF `scripts/check_test_hooks.py` checks for test-only symbols, Q2's `nm` check; then every
`scripts/check_*.py`; then `cargo deny check licenses bans sources` against `deny.toml`, ROADMAP §10.9's
dependency policy; and the `tests/fuzz` build and replay (§8.1)). Right after the `vibeos-core` clippy lines it builds `vibeos-core` with its MSRV
(`make check-msrv`: `cargo +<MSRV> check` for the host with `std` and for `x86_64-unknown-none`, under
`RUSTFLAGS=--cap-lints=warn`, so it proves only that the crate builds). A missing `ruff`, `mypy`, `cargo-deny`,
`fsck.fat` or MSRV toolchain fails it unless `VIBEOS_ALLOW_MISSING_TOOLS=1`, which skips that check and
prints it. CI runs it as the `check` job before QEMU (DESIGN §8.6).
`make test-forensics` (`tests/harness/run_forensics.py`, in `make test`) tests the core tool on
real cores. It boots the `hang` ISO at `-smp 4` and 128 MiB, declared `expect=none`: its contract
through `smp: done`, then `vibeOS: hang_test: armed`; any panic signature or QMP event fails it at
once. Five seconds after the armed marker (`GIVE_UP_S`), never at the harness timeout, it stops the
guest, takes a core through the pipe into `build/forensics/hang/` and quits, then runs
`vmcore report --virt --trace` and checks the report (the `sig:` line, a `cpu` block with a
symbolized frame for each of the four CPUs, CPUs 1 to 3 in `hang_test::wait`, a thread line with
its state for each current thread, and min(64, ring length) log records ending with the armed
marker), the export (JSON whose `traceEvents` all carry `name`, `ph`, `ts`, `pid` and `tid`, all
four CPUs among them, and the report's ordering) and the virtual core (every `PT_LOAD` of the ELF
covered, and the ELF's `.text` bytes at `.text`'s address). It then checks the refusal (the hang
core with the `gp` ELF exits 3), boots the `gp` ISO declared `expect=panic` and takes its core at
`GUEST_PANICKED` (its `sig:` line is the serial `#GP` line as `PanicLine` keeps it, with
`gp_test_fault < gp_test_trip < boot_rest`), and boots the `hang` ISO at `-m 9G`, whose core goes
through the pipe and the tool streaming from `zstd -dc`: the same `sig:` line, and at least 9 GiB
in its `core:` line. Each check is a `forensics_<case>` row of its results file.
`make test-e2e` is enough when only boot output or QEMU wiring changed. `make test` is the gate before
a PR. `make test-ps2` is the focused #66 sendkey boot; `make test-e2e` already runs it, so `make test`
does not boot it twice.

`make gate PHASE=N` (`scripts/gate.py`) is the phase exit gate the maintainer runs before tagging
(ROADMAP §10.9). It prints one row per exit-gate line, `PASS`, `FAIL` or `TAG  L<line>  <text>`, each
followed by its entries' results, then a `BOX  ROADMAP.md:<line>  rule A|B: <text>` row per box it
rejects and `gate: phase N at <sha>: pass|fail`, and exits 0 on pass, 1 on fail and 2 on a usage
error. From Phase 10 on it runs every entry of `tests/gates/phase-<N>.toml` (§8.6) and fails when the
map is missing or `scripts/check_gates.py` rejects it; a `cmd` entry runs once per distinct command,
with its output in `build/gate/phase-<N>/<i>.log`, and one that selects in-guest tests with
`VIBEOS_KTEST=` passes only when its tier's fresh `build/results/<arch>-<tier>.json` lists each named
test as passed and each glob matches one. For a phase below 10, which has no map, it runs no entry and
needs every gate line but the tag ticked. Every phase gets two box rules: rule A rejects an open box
under a `### N.M` heading of phase N, outside a `### N.M Stretch:` subsection, whose `lands in` notes
name no `§M.x` with M > N; rule B rejects an open box anywhere in the roadmap whose `lands in` note
names a section of phase N (a `§N.x` in a code span does not count). A local run gates `HEAD` of a
work tree whose tracked files are clean, so `COMMIT=<sha>` must name `HEAD`; `python3 scripts/gate.py
--phase N --dry-run` prints the rows and the box problems and runs nothing. `RECORD=1` runs only the
map's record entries, on the Apple Silicon dev host (§8.6). `make gate PHASE=10` fails today by
design, on every open Phase 10 box and on the open earlier boxes deferred into §10; it runs in no
per-push tier.

## 8.6 CI and coverage

`ci` runs on a push to `main`, on every pull request, and on `workflow_dispatch`, never on a push to
another branch, with one temporary exception: until ROADMAP §10.1's trigger box is ticked, pushes to
the Phase 10 integration branch (`phase-10`, and the branch that stands in for it) and to the
`p10/**` slice branches run too, so a slice's race-proof test commit runs red before its fix
(`TEMPORARY` in `scripts/check_workflows.py`). A pull request's runs share the concurrency group
`ci-pr-<number>` and cancel superseded ones; every other run has a group of its own, `ci-run-<run
id>`, so no run on `main` is cancelled or dropped as pending, and a fork's pull request from its own
`main` shares nothing with `main`'s runs. `scripts/check_workflows.py` fails on `ci.yml` push branches
other than `main` and the temporary list, on a `tags`, `branches-ignore` or `paths` filter, on a
missing `pull_request` or `workflow_dispatch` trigger, or on a group that is not built that way
(`rule_ci_triggers`); on any `concurrency` group built from `github.head_ref` or `github.ref_name`
(`rule_concurrency_group`); and on a workflow a §10.9 gate entry names that has no
`workflow_dispatch` trigger (`rule_gate_dispatch`). The other rows below are scheduled,
dispatched, or run on a tag. A `pull_request` run never counts as proof of a commit (ROADMAP §10.9).

**CI budget.** Every push to `main` and every pull-request update runs `check` and, alongside it,
one `build` job per architecture (x86_64 now; from Phase 11 aarch64 on the arm64 runner, which also
runs `make test-unit` and the hostlib tests natively, §11.4's aarch64 switch roundtrip among them).
`build` runs `make prebuilt`, which builds every ISO variant and the host `mkfs`/`fsck`/`nbd-cache`/
`vibefs-cat` tools once and packs them, with the host triple they were built for, into
`build/prebuilt.tar`, uploaded as `prebuilt-<arch>`. A matrix of `tier` jobs per architecture
(`needs: [check, build]`, `fail-fast: false`, TCG) downloads it and runs the same `make test-*`
targets with `VIBEOS_PREBUILT=1`, which defines no ISO or host-tool rule, so a tier builds nothing
and a missing file fails with `No rule to make target`: the Makefile stays the one definition of
each tier. Tiers are grouped to about 40 s of QEMU each, a group over 60 s split at target
boundaries, and each is its own check name, `tier (<arch>, <tier>)`, so a red pull request names
the failing tier; the table below holds the grouping, and `scripts/check_workflows.py` fails when it
and the matrix differ (`rule_budget_doc`), and when the `tier` job lacks `needs: [check, build]`,
`fail-fast: false` or `VIBEOS_PREBUILT=1`, or a `make test` prerequisite is in no tier or in two, the
ones `check` runs through `$(MAKE)` aside (`rule_tiers`). Everything else runs on a schedule: the
macOS job, the KVM leg, the fuzzers, stress, and any job with a performance threshold. Later lines
name two scheduled workflows, both on the pinned toolchain: the nightly job, which carries the KVM
leg, and the weekly job (`smp-stress` today); the non-blocking `nightly-canary`, the one job on an
undated nightly, is neither, and a line that needs its own workflow or another cadence names it
(§20.8's `hardware-models`, §22.5's `fuzz.yml`, §24.2's rebuilds). A later line that says "in CI"
for a functional test means a ladder tier; for a benchmark or a threshold it means the KVM leg. A
red scheduled job blocks the next phase tag. The earlier no-matrix rule (runner queues) is lifted:
the repository is public, so standard runners are free and unlimited, and the limits that matter
are 20 concurrent jobs on the Free plan (at most 5 macOS; scheduled campaigns together hold at most
10, so pushes keep the other 10) and 6 hours per job: a scheduled run longer than 5.5 hours is
split into shards that hand their state on as artifacts, and the line that needs one says so;
scheduled work runs in the 10 lanes of ROADMAP §10.1's next box (Scheduled capacity, below).

Tiers, with each group's summed QEMU step time in the last green integration-branch `ci` run before
the split (run 36522096073 at `30edb3d`, one `ubuntu-latest` runner, TCG); `vibefs-crash`'s figure
includes its `cargo test` of the host tools, the one tier that needs the toolchain; `forensics`'s
is a local TCG run's, until a `ci` run measures it:

| Arch | Tier | Targets | QEMU s |
|---|---|---|---|
| x86_64 | e2e-1 | `test-e2e`, `test-e2e-uefi`, `test-e2e-panic`, `test-e2e-panic-nest`, `test-qmp` | 40 |
| x86_64 | e2e-2 | `test-e2e-gp`, `test-e2e-mce`, `test-e2e-pit`, `test-e2e-highmem`, `test-e2e-init-fault`, `test-e2e-strace`, `test-e2e-panic-stop`, `test-e2e-power` | 40 |
| x86_64 | in-guest-1 | `test-kernel` | 64 |
| x86_64 | in-guest-2 | `test-kernel-smp4` | 52 |
| x86_64 | in-guest-3 | `test-lapic-fallback` | 50 |
| x86_64 | vibefs-crash | `test-vibefs-crash` | 47 |
| x86_64 | forensics | `test-forensics` | 60 |

`test-unit` and `test-harness` run inside `make check`, in the `check` job. The in-guest tiers
each pass 40 s alone and cannot split below a target.

| Job | When | What |
|---|---|---|
| `check` | push / PR | Installs `x86_64-unknown-none`, the MSRV toolchain with the host and `x86_64-unknown-none` targets, and cargo-deny's pinned release archive, checked against the SHA-256 the step records. `make check` (fmt; clippy `-D warnings` on `vibeos-core` and hostlib for the host, `vibeos-core` for `x86_64-unknown-none`, and the kernel with default features; host units, harness, ruff and mypy at pinned versions, the MSRV build, `scripts/check_*.py`, `cargo deny check licenses bans sources`); on a pull request, `scripts/check_gate_inputs.py` against its merge base; then `cargo llvm-cov -p vibeos-core --lib --features std --target $HOST --fail-under-lines <floor>`, the floor in `tests/gates/inputs.toml`. No QEMU, no `setup.sh`. HTML report is a 7-day `core-coverage` artifact. |
| `build (<arch>)` | push / PR, beside `check` | Limine, QEMU/nasm/xorriso, kernel clippy `-D warnings` once for each other feature set an ISO is built with (`kernel_tests`, `vibefs_crash`, and each of `panic_test`, `gp_test`, `panic_nest_test`, `panic_stop_test` and `hang_test`) and once with `kernel_shell` (the default set runs in `check`); `make prebuilt`, uploaded as `prebuilt-<arch>` (1 day); the runner's CPU model to the job summary. Green `main` uploads `vibeos.iso` (7 days). |
| `tier (<arch>, <tier>)` | push / PR, `needs: [check, build]` | One job per row of the tier table above: QEMU and OVMF, `prebuilt-<arch>` unpacked, the runner's CPU model to the job summary, then `make -k -j <jobs> --output-sync=target VIBEOS_PREBUILT=1 <targets>` under TCG (`jobs` is 1 until ROADMAP §10.1's parallel QEMU runs land). Even after a failed step it writes a per-tier table and every harness retry to the job summary and uploads `build/results/` as `results-<arch>-<tier>`. |
| `ticks` | PR, `needs: tier`, even after it fails | `scripts/check_ticks.py --base <PR base> --head <PR head> --run-commit $GITHUB_SHA --results <downloaded results-*> --summary $GITHUB_STEP_SUMMARY`: every box a commit of the pull request ticks pairs with a `Proves:` line, its proof exists at the head and is changed by the pull request or marked `(existing: ...)`, a ktest, utest, or marker proof passed in a results file of the head or the tested merge commit, no results file lists a retry, needs and closes rows hold, `Fails-before:` lines are present, and a bracketed proof passed on a scheduled run or `ci-history` record (read through `gh`, with `contents: read` and `actions: read`). The summary lists errors, `(existing: ...)` proofs, and notes. `make check` runs the pairing and diff rules bare against `origin/main` and skips them when that ref is missing, as in the `check` job's shallow checkout. |
| `ci-pass` | every per-push run | `needs:` every other job, `if: always()`; fails unless each succeeded, a job gated on the event being allowed to skip (`scripts/check_gate_inputs.py --ci-pass`, which the job runs with `NEEDS: ${{ toJSON(needs) }}` and `SKIPPABLE: ticks`; its static rules fail when the job misses one, lacks `if: always()`, or lists in `SKIPPABLE` a job whose `if:` does not test `github.event_name`) |
| `smp-stress` `stress` | weekly Monday 06:00 UTC + dispatch, `sched-lane-4` | `make test-smp-stress`: the in-guest tier at `-smp 4`, under the §8.2 per-run deadlines and no whole-run timeout |
| `smp-stress` `repeat-kernel` | same workflow, `sched-lane-4` | `VIBEOS_KTEST_REPEAT=20 make test-kernel`: every in-guest test 20 times in one `-smp 2` boot |
| `smp-stress` `repeat-kernel-smp4` | same workflow, `sched-lane-5` | `VIBEOS_KTEST_REPEAT=20 make test-kernel-smp4`: the same at `-smp 4` |
| `nightly-canary` | same workflow, non-blocking, `sched-lane-5` | undated latest nightly, `make iso && make test-unit` |
| `release` | `workflow_dispatch` from `main`, with the release tag | `build` (`contents: read`, `actions: read`) fails on any ref but `refs/heads/main`, then runs `main`'s own `scripts/release_check.py` from a sparse checkout, before any code of the tag's tree: the tag is annotated, its commit is on `main`, a `ci` run that proves that commit (`gatelib.run_proves_commit`, ROADMAP §10.9) concluded `success`, and one annotated `phase-<N>` tag sits on it. It checks that commit out with `persist-credentials: false`, restores no cache, and runs `setup.sh` (a fresh Limine clone and host tool), `make gate PHASE=<N>`, `make release-artifacts OUT=dist` (the release profile), the third-party notices and the ISO's xorriso version, `make CARGO_PROFILE=release` over `test-e2e`, `test-e2e-uefi`, `test-e2e-mce`, `test-e2e-pit`, `test-e2e-highmem` and `test-e2e-strace`, and `cmp build/vibeos.iso dist/vibeos.iso`; it uploads the tag's commit as `commit-input` (CI history, below), writes the notes with `scripts/changelog_section.py`, and uploads `dist/` with its `SHA256SUMS` as `release`, and `results-x86_64-build` and `runner-build`. `publish` (`contents: write`, `needs: build`) checks out nothing and runs no repository script: `sha256sum -c SHA256SUMS`, then the pinned release action publishes `vibeos.iso` as the one image, with `vibeos.iso.xorriso-version` and `THIRD-PARTY-NOTICES.txt`, at the verified commit. `scripts/check_workflows.py` keeps that shape (`rule_release_*`): `workflow_dispatch` with a required `tag` as the one trigger, no cache, no workflow-wide write, no checkout, local action, or command outside `PRIVILEGED_COMMANDS` in a job with a write grant or the `release` environment, and `vibeos.iso` as the one published image, after `build`. The owner's steps are [RELEASING.md](RELEASING.md). From ROADMAP §14.6 a `sign` job in the `release` environment between them, and from §22.4 a keyless `verify` job on vibeOS. From ROADMAP §18.7 the `sign` job is two key jobs, `sign-files` and `sign-manifest`, with an unprivileged `assemble` job between them, since images hold the signed kernels and Limine binaries and the manifest lists the images (ROADMAP §22.1). |
| `ci-history` | `ci`, `release`, `nightly` or `smp-stress` run completes; daily 04:23 UTC; dispatch | `record` (on a completed run): the run's record on the `ci-history` branch. `daily` (schedule, dispatch): the packed size and the 500 MB rotation (`--rotate`), the backfill (`--backfill --limit 200`), then the completeness check, which turns it red on a missing record (CI history, below). Each job holds `contents: write` and `actions: read` only and checks out nothing. Both jobs run in `sched-lane-6`. |
| `macos` | daily 04:23 UTC + dispatch, `sched-lane-9` | `macos-15` arm64 with Homebrew's `qemu`, `xorriso`, `nasm` and `dosfstools`; jobs `check` (`make check`) and `test` (`make -k test-e2e-uefi test`, with Homebrew's edk2 firmware on pflash); each uploads `build/results/`. |
| `nightly` `kvm` | daily 03:17 UTC + dispatch, `sched-lane-0` | The x86_64 KVM leg (ROADMAP §10.1): `/dev/kvm` opened by GitHub's documented udev rule, job env `VIBEOS_QEMU_ACCEL=kvm` and `VIBEOS_QEMU_CPU=max,+invtsc`, then `make test-kernel`, `make test-e2e`, `VIBEOS_SMP=1 make test-e2e`, `make test-lapic-fallback LAPIC_FALLBACK_CPU=qemu64,+invtsc,-tsc-deadline`, and `VIBEOS_KTEST='lifetime_*,exit_burst,fork_oom' VIBEOS_KTEST_REPEAT=20 make test-kernel-smp4` (exit-gate line §10.10), each step run even after an earlier one failed. GitHub assigns each job's host CPU at random (AMD EPYC or Intel Xeon, several models), so `scripts/runner_info.py` writes the CPU model beside the guest's invariant-TSC bit to the job summary and to `build/runner.json`, uploaded as `runner-kvm`, which fills the CI-history record's `runner`; a regression threshold compares a number only with history from the same CPU model. `build/results/` is uploaded as `results-x86_64-kvm` (90 days). |
| `nightly` `release-profile` | daily 03:17 UTC + dispatch, `sched-lane-1` | `make CARGO_PROFILE=release test-e2e test-kernel` under TCG, in its own job, since the release and dev ISOs share their names under `build/` (ROADMAP §10.2, F137; BOOT.md §3.5); the runner record and the uploads as `results-x86_64-release-profile` and `runner-release-profile`. `release.yml` runs the production-image e2e targets on the release-profile image it publishes. |
| `nightly` `repro` | daily 03:17 UTC + dispatch, `sched-lane-2` | `make repro`: every ISO variant built twice from one commit, with a different checkout path, `CARGO_HOME` and `RUSTUP_HOME`, compared byte for byte, and no host path in any output (`scripts/repro_build.py`) |
| `nightly` `deny-advisories` | daily 03:17 UTC + dispatch, `sched-lane-3` | cargo-deny's pinned release archive, checked against its SHA-256 as in `check`, then `cargo deny check advisories`, which fetches the RustSec database and so stays out of `make check` |
| `nightly` `provenance-fetch` | daily 03:17 UTC + dispatch, `sched-lane-3` | `python3 scripts/check_provenance.py --fetch`: each provenance header's upstream file at its pinned revision (DESIGN §1.5) |
| `nightly` `budget` | daily 03:17 UTC + dispatch, `sched-lane-3` | `make ci-budget` (`ci_history.py --budget` and `--tiers`, Scheduled capacity below) against the `ci-history` branch, which it clones alone |

The `ticks` job (ROADMAP §10.9) runs after the jobs that run the tiers, the `tier` matrix, and reads
the `build/results/` files they upload. A pull request run tests the merge of its head with its
base, so the results files carry the merge commit, which `--run-commit` names; `check_ticks.py`
reads commits and their messages from the pull request's head.

**CI history.** ROADMAP §10.9's `ci-history` workflow keeps what `ci`, `release`, `nightly` and
`smp-stress` ran past
GitHub's 90-day limit on Actions logs and artifacts. When a run of either completes, its `record`
job writes one JSON record per run id, `runs/<workflow>/<run_id>.json`, to the orphan `ci-history`
branch (C-HISTORY): the run id, workflow, `attempt`, event, head SHA, branch, conclusion, start and
finish; per job its conclusion, `created`, `started`, `completed`, seconds and per-step seconds, the
results files of its `results-<arch>-<job>` artifact and the runner data of its `runner-<job>`
artifact, `<job>` naming the job by the slug of its display name; and, for a workflow that takes a
commit as input, that commit (`release`'s `commit-input` artifact). A re-run replaces the run's
record with its latest attempt, which `attempt` names. No record carries an actor, author or
e-mail: the branch is public data (DESIGN §1.5). The job holds `contents: write` and
`actions: read` only, checks out nothing but a clone of `ci-history` alone, and runs
`scripts/ci_history.py` and `scripts/gatelib.py` as fetched from the default branch at
`GITHUB_SHA`, never the triggering commit's. No field of the run reaches a shell line: the tool
reads only the run id from the event file, checks the workflow's name, path and repository against
its allowlist, takes the rest from the API, and reads artifacts as capped bytes in memory. On a
rejected push it re-applies its one file on the new tip, up to 10 times, so concurrent runs lose no
record. The `daily` job (04:23 UTC, and on dispatch) runs `--rotate`, then `--backfill --limit
200`, which records each `ci` run on `main` that the API still lists and the branch lacks, or
writes a tombstone (`"jobs": []` and `"tombstone": "<reason>"`) when the jobs API answers 404, 410
or no jobs, then the completeness check. That check, `python3 scripts/ci_history.py` with no mode,
fails on any `ci` run on `main` (a push or dispatch run of this repository's `main`) since the
history landed, that is whose head commit descends from the oldest commit on `main` that touches
`ci-history.yml`, with no record or tombstone. A tombstone passes unless a gate map's `job` entry
names the workflow, the run proves the gated commit (`--gated`, else the checkout's `HEAD`), and no
successful full record proves it. `python3 scripts/ci_history.py --series ci` prints each `main`
run's push-to-green time (its latest job end minus its earliest job creation) and their median,
the numbers ROADMAP §10.1 reads; `--job` and `--step` narrow it to a job or a step. `--rotate`
writes the branch's packed size (`size-pack` of a full clone) to the job summary; past
500,000,000 bytes it moves the oldest UTC year's records into a zstd archive, the asset of the
prerelease `ci-history-<year>`, never marked latest and tagged at the branch's tip before the
rotation, lists it in `archives.json` with its SHA-256 and run ids, and restarts the branch from an
orphan commit holding the rest, pushed with a lease on the tip it read. It refuses to archive the
current year and fails with the size instead. `ci_history.py` reads the branch and the archives
alike. The packed size is recorded here once the first daily run measures it (ROADMAP §10.9).

**Gate maps.** From Phase 10 on, `tests/gates/phase-<N>.toml` gives each exit-gate line of phase N
but the tag the entries that prove it (ROADMAP §10.9, C-GATEMAP): one `[[line]]` per line, its `key`
the line's full text after `- [ ] ` or `- [x] `, compared with whitespace collapsed, and
`[[line.entry]]` rows that each hold exactly one of `cmd` (a local command), `job = {workflow, job}`
(a job of a GitHub-hosted workflow that must be green on a run proving the gated commit, read through
`gh`) or `record = {cmd}` (a dev-host record, below), with an optional `expect = "fail"` for a command
that must fail, which counts only after a plain entry of its line passed in the same run. A line with
several entries, one per architecture or accelerator for example, passes only when all of them pass.
Each later phase adds its map in the slice that closes its gate; Phases 0 to 9 get none.
`scripts/check_gates.py`, which `make check` runs, reads text only (no entry runs, no `gh`, no
`ci-history`) and fails on a `phase-<N>.toml` with N below 10; on a key that matches no exit-gate
line of phase N, matches the tag line, or repeats another; on a gate line but the tag with no entry;
on an entry with none or two of `cmd`, `job` and `record`, or an `expect` other than `"fail"`; on an
entry that runs `make gate` or `scripts/gate.py`, so the entry for a line that names the gate runs
that line's other checks; on a line that names a `scripts/check_<x>.py` with no `cmd` or `record`
entry containing that path; and on a job entry whose workflow has a `self-hosted` label anywhere
outside a comment. Until a workflow a job entry names exists, a
`test -f .github/workflows/<wf>.yml` entry stands in for it, since `rule_gate_dispatch` rejects a
missing workflow, so the line fails rather than passes without its job.
`tests/harness/test_gates.py` holds a failing case per rule and runs the script on the tree.

**Which run proves a commit.** A run proves commit C only when its event is `push`, `schedule`, or
`workflow_dispatch` and its head SHA is C, or, for a workflow that takes a commit as input, its
CI-history record names C (`commit`); a `pull_request` run never proves a commit, since it tests the
merge with the pull request's base. `gatelib.run_proves_commit` is that rule, and `make gate`, the
`ticks` job and every other reader of runs use it. A gate map's `job` entry passes on a run of its
workflow that concluded `success`, proves the gated commit, and whose jobs named as the job id's
`name:` in the workflow at that commit (or `<name> (…)`, one per matrix leg) all concluded `success`;
`gate.py` takes the candidates from `gh api …/actions/workflows/<wf>/runs -f head_sha=<C>` and from
the workflow's `ci-history` records, merging a run's record into it for a workflow that takes a
commit as input. It starts nothing: when no run proves the commit it prints the maintainer's
commands, `git push origin <C>:refs/heads/gate/<N>` and `gh workflow run <wf> --ref gate/<N>`, or
for a workflow that takes a commit as input `gh workflow run <wf> --ref main -f commit=<C>`, so
every workflow a gate entry names has a `workflow_dispatch` trigger (`rule_gate_dispatch`). A
`record` entry's command never runs off the dev host: the entry passes only when `ci-history` holds
a dev-host record (below) with event `dev-host`, the gated commit, phase N, the line's key, the map's
command at that commit, every required field, and result `pass`.

**Dev-host records.** No hosted CI runner can run an HVF guest, so a gate line, or the part of one,
that runs under HVF has a `record` entry, proved on the Apple Silicon dev host (ROADMAP §10.9).
`make gate PHASE=N RECORD=1` refuses to run anywhere but macOS on arm64; it runs only the map's record
entries, each in a `git worktree` of the gated commit in a temporary directory, so an uncommitted
change in the maintainer's tree reaches no record, and writes one JSON record per commit and entry,
pass or fail: `schema`, `event: "dev-host"`, `commit`, `head_sha`, the fixed `host: "dev-host"`,
`mac_model` (`sysctl -n hw.model`), `macos` (`sw_vers`), `qemu` (the first `--version` line of each
`qemu-system-*` on `PATH`), `phase`, `line` (the key), `command`, `numbers` (`seconds` and any
`numbers` section of the run's results files), `result`, `started`, `finished`, and `results`, the
`build/results/*.json` files the run wrote, which `check_ticks.py` reads. Every string is scrubbed
first (the worktree becomes `<checkout>`, the temporary directory `<tmp>`, the home directory
`<home>`), and `ci_history.validate_record` then refuses any record that holds the machine's
hostname (`socket.gethostname()`, its short form, `scutil --get LocalHostName`), user name, home
directory, or serial number (`ioreg -rd1 -c IOPlatformExpertDevice`, compared and never printed or
stored), since `ci-history` is public (DESIGN §1.5). `ci_history.py --record PATH` commits the record
at `records/<yyyy-mm-dd>-dev-host-<sha>-<entry-id>.json` (C-HISTORY; the entry id is the first 12
hex digits of the SHA-256 of the phase, the key and the command) and, on a rejected push, re-applies
it on the new tip and pushes again. Two record entries of one line with one command would share a
path, so `RECORD=1` refuses them before anything runs. Everywhere else, `release.yml` included, the
records are only read (above). `tests/harness/test_gate_records.py` pushes only to a bare repository
in its own temporary directory.

**Workflow rules.** `scripts/check_workflows.py`, which `make check` runs, reads every workflow
with a stdlib YAML subset reader that fails on anything it does not parse (anchors, aliases, tags,
`---`, multi-line flow) rather than misread it, and holds one function per rule in `RULES`. Besides
the trigger, runner, tier, and upstream rules this section states where they apply, it fails on
`${{` inside an inline or block `run:` script, whose values reach a step through `env:` instead
(`rule_no_expr_in_run`, F144); on a workflow without a top-level `permissions:` mapping, on
`read-all` or `write-all`, and on any grant other than `contents: read` or `none`, at the top or in a
job, without a comment beside it naming its need (`rule_permissions`); and on a remote `uses:` not
pinned as `@<40 hex>  # <version>`, where `./` paths and `docker://…@sha256:` pass
(`rule_action_pins`). `tests/harness/test_workflows.py` holds a failing and a passing case for each
clause and runs every rule on the real files.

Rule; not yet enforced: a job that holds a signing key or a write token runs no code from the
candidate commit, restores no cache, checks out nothing (ROADMAP §10.9's history job checks out
only the `ci-history` branch and runs `main`'s `scripts/ci_history.py`, never the candidate's), and
receives only artifacts and their SHA-256 list (ROADMAP §10.1, §14.6). `release.yml`'s `publish` job follows it, and `scripts/check_workflows.py` checks that; the key jobs arrive with ROADMAP §14.6.

**Runners.** Every Linux job runs on GitHub's free `ubuntu-26.04` image (`ubuntu-26.04-arm` for
arm64 jobs), whose apt QEMU 10.2.1 (`1:10.2.1+ds-1ubuntu3`) meets every QEMU minimum ROADMAP names
(9.0 for Phase 11's EL2 boot and §20.1's boot with more than 255 vCPUs, 10.2 for §18.1's amd-iommu
`dma-remap` and Phase 25's GHES injection). A line that needs QEMU 11.1 or later builds that release
from its tarball, checked by SHA-256 and cached by version; none does yet. Every job that installs
`qemu-system-*` sets `VIBEOS_QEMU_VERSION` to the version it pins, and when `CI` is set on Linux,
`harness.qemu_argv` runs `ensure_qemu_pinned`, which compares `qemu-system-x86_64 --version` with
that pin before the first boot and fails on a mismatch or an unset pin, so an image update that
moves QEMU fails every tier loudly instead of changing what they test; move the pin and this
paragraph together. `make check`, the macOS job, and a dev host's QEMU (Homebrew's included) are
not checked. `scripts/check_workflows.py` fails on a `runs-on:` label other than these two and
`macos-*` (`rule_runs_on`, which resolves `${{ matrix.X }}` to the matrix's values), and on a job
that names `qemu-system` without a `VIBEOS_QEMU_VERSION` of the form `N.N.N` (`rule_qemu_pin`).
Planned (ROADMAP §11.7): every job that boots an aarch64 guest runs on an arm64 runner
(`ubuntu-26.04-arm`, or the scheduled macOS job's arm64 image), never on an x86_64 one. TCG adds no
ordering to an aarch64 guest's loads and stores, so only an arm64 host lets a weak reordering reach
guest code; an x86_64 host runs them in its TSO order. `scripts/check_workflows.py` will check it. From
ROADMAP Phase 11 on, `make gate` also needs two dev-host records that loop the -smp 4 in-guest tier
and smp-stress under HVF for 30 minutes each (`tests/gates/common.toml`), the only gate that runs
those tests on a weakly ordered CPU directly. The weekly aarch64 smp-stress leg records whether TCG
there showed any weak outcome (`weak_order_probe`).

**Scheduled capacity.** The Free plan's 20 concurrent jobs are the owner's account's, shared with
its other repositories, and scheduled and dispatched workflows hold 10 of them as lanes (ROADMAP
§10.1). A lane is a job-level concurrency group, `sched-lane-<n>`, declared in block style with
`queue: max`: it runs one job at a time and holds up to 100 waiting, first in first out. Without
`queue: max` a group keeps one waiting job and cancels it when another arrives, GitHub rejects
`cancel-in-progress` beside `queue: max`, and GitHub runs no queue across workflows, so lanes are how
the split holds. Every job on a GitHub-hosted runner of a workflow that runs on a schedule or a
dispatch, `ci.yml` and `release.yml` aside, runs in a lane, named by a literal group. The lane map
reserves for jobs that end within 5.5 hours (`timeout-minutes` at most 330) the lanes their cadence
needs, which no multi-day chain (a soak, a campaign, ROADMAP §24.2's rebuilds) takes, and names the
lanes a release window takes (ROADMAP §22.1). The ledger gives each scheduled or dispatched
workflow's cadence, jobs per run, job-hours per run (estimated until `ci-history` measures them),
peak concurrent jobs, and lanes. Both change in the same commit as the workflows they describe. Every job of `nightly.yml` and
`smp-stress.yml` ends with three `if: always()` steps: `scripts/runner_info.py`, then the uploads of
`build/results/` as `results-x86_64-<job>` and `build/runner.json` as `runner-<job>` (90 days), so
the `ticks` job and the CI history read a scheduled run's results and runner (C-RESULTS,
C-HISTORY).
`scripts/check_workflows.py` fails on a scheduled or dispatched workflow with no ledger row
(`rule_ledger_row`); on more than 100 jobs of one run in one lane, a matrix counting the product of
its literal axes plus its `include` entries and a matrix built from an expression failing as
uncountable, or on a row whose jobs per run pass 100 per lane (`rule_lane_capacity`); on a job
without a literal `sched-lane-<0-9>` group and `queue: max`, with `cancel-in-progress`, or in a
reserved lane without `timeout-minutes` of at most 330 (`rule_job_lane`); on a job in a lane its
row does not give it (`rule_row_lane`); and on a row's lane that the map does not list for that
workflow (`rule_lane_map`). `Reserved for` is `nightly`, `weekly`, `scheduled`, `history` or
`none`, and from ROADMAP §24.2 `rebuilds`.

| Lane | Reserved for | Jobs |
|---|---|---|
| `sched-lane-0` | nightly | `nightly.yml` `kvm` |
| `sched-lane-1` | nightly | `nightly.yml` `release-profile` |
| `sched-lane-2` | nightly | `nightly.yml` `repro`; `models` and `miri` when they land |
| `sched-lane-3` | nightly | `nightly.yml` `budget`, `deny-advisories`, `provenance-fetch`; `irqoff` when it lands |
| `sched-lane-4` | weekly | `smp-stress.yml` `stress`, `repeat-kernel` |
| `sched-lane-5` | weekly | `smp-stress.yml` `repeat-kernel-smp4`, `nightly-canary`; `fuzz` when it lands |
| `sched-lane-6` | history | `ci-history.yml` (every job) |
| `sched-lane-7` | none | multi-day chains (soaks, campaigns); a §22.1 release window |
| `sched-lane-8` | none | multi-day chains; a §22.1 release window |
| `sched-lane-9` | scheduled | `macos.yml` (P10-S47) |

Release windows: none

| Workflow | Cadence | Jobs per run | Job-hours per run | Peak concurrent jobs | Lanes |
|---|---|---|---|---|---|
| `smp-stress.yml` | weekly `0 6 * * 1` and dispatch | 4 | 4.5 (estimated) | 2 | `sched-lane-4`, `sched-lane-5` |
| `nightly.yml` | daily `17 3 * * *` and dispatch | 6 | 4.6 (estimated) | 4 | `sched-lane-0`, `sched-lane-1`, `sched-lane-2`, `sched-lane-3` |
| `ci-history.yml` | each completed `ci`, `release`, `nightly` or `smp-stress` run (`record`); daily `23 4 * * *` and dispatch (`daily`) | 1 | 0.05 per `record`, 0.3 per `daily` (estimated) | 1 | `sched-lane-6` |
| `macos.yml` | daily `23 4 * * *` and dispatch | 2 | 1.5 (estimated) | 1 | `sched-lane-9` |

`ci_history.py --budget` reads the `ci-history` records of the scheduled workflows `WORKFLOWS`
lists and takes each job's lane from its workflow file through `check_workflows.py`'s reader. Over
the last 4 complete weeks (Monday 00:00 UTC) outside a release window, it prints each workflow's
weekly job-hours and each lane's weekly busy share with its median and maximum wait (started minus
created), and fails when a lane but a `rebuilds` one was busy more than 60% of a week, or a job in a
reserved lane waited more than 12 hours to start. The 40% left absorbs GitHub's delays to
scheduled runs and new workflows, and keeps the account from running its share full around the
clock, which GitHub's Actions terms count against it when the burden is disproportionate to the
benefits. `ci_history.py --tiers` takes the last 20 `ci` runs with event `push` on `main`, sums
each `tier (<arch>, <tier>)` job's `make test-*` step seconds, prints each tier's median and run
count (`n=<k>`, fewer than 20 while the history is young), and fails on a median above 60 s, which
a new tier with its own check name fixes. Both exit 2, naming what is missing, when the history
holds no such run or a record lacks a job's `created`, `started` or `completed` time or a tier
step's seconds. `make ci-budget` runs both and fails if either fails; the nightly `budget` job runs
it, and from ROADMAP Phase 11 an entry of `tests/gates/common.toml` runs it before every phase
tag. The medians below are filled from a `make ci-budget` run on `main`, citing the commit and the
CPU model, and the section also records the `ci-history` branch's packed size (ROADMAP §10.9).

| Tier | Median QEMU s (last 20 runs on main) | Measured at |
|---|---|---|
| `tier (x86_64, e2e-1)` | pending (make ci-budget after merge) | - |
| `tier (x86_64, e2e-2)` | pending (make ci-budget after merge) | - |
| `tier (x86_64, in-guest-1)` | pending (make ci-budget after merge) | - |
| `tier (x86_64, in-guest-2)` | pending (make ci-budget after merge) | - |
| `tier (x86_64, in-guest-3)` | pending (make ci-budget after merge) | - |
| `tier (x86_64, vibefs-crash)` | pending (make ci-budget after merge) | - |

**Issues and crash records.** Planned (ROADMAP §14.10, §22.5): one `workflow_run` filer is the only
job with `issues: write`; it checks out nothing, runs no repository code, and opens or comments on
issues by kind, branch, and signature. From ROADMAP §22.5, fuzz jobs run in `fuzz.yml` under a
`fuzz-state` environment, encrypt the state their shards carry, seal each crash record to the triage
key, and publish only the target, the run, and a keyed crash id, so a crash's reproducer stays
private until its fix is published.

`-D warnings` reaches host builds through `[build] rustflags`, and every kernel build and clippy run
(`make iso`, every ISO variant, `make check`) through `[target.x86_64-unknown-none] rustflags` in
`.cargo/config.toml`, which replaces `[build] rustflags` for the kernel target, since Cargo reads one
rustflags source. A job that sets `RUSTFLAGS` drops both (ROADMAP §10.1, F147).

GitHub Actions records per-step duration. Measured on `main` at `88370e5` (run 35796216463): `check`
53 s, then the ladder 160 s, serialized by `needs: check`. The ladder spends 58 s on setup, toolchain,
kernel clippy, and ISO build before the first QEMU step, then 98 s across nine QEMU steps (longest:
vibefs crash, 22 s); about **3m40s** end to end. A fmt or
hostlib lint failure should go red in about a minute without starting QEMU. Host packages come from a
cache (ROADMAP §10.1): `check`, `build` and `tier` each name theirs in `APT_PACKAGES` and restore
`~/apt-cache` under the key `apt-<ImageOS>-<ImageVersion>-<sha256 of the list>`; a hit installs the
cached `.deb` files with `dpkg -i`, with no `apt-get update` and no download, and a miss runs
`apt-get install` with `APT::Keep-Downloaded-Packages=true` and saves what it downloaded. `build`
and `tier` write the runner's CPU model to the job summary, so the "after" figure, a `ci` run on
`main`, can cite it; each tier's `jobs` stays 1 until the timing tests and the §10.2 retried
failures are fixed. From ROADMAP §10.9's CI
history on, a measured number recorded in the design docs cites the commit it was measured at and the CPU
model or machine it ran on (ROADMAP, How to read this).

A failure traced to a QEMU bug is not retried either: the kernel or the harness's QEMU command line
(`harness.qemu_argv`) works around it, and the upstream report, with a reproducer, the QEMU version,
and the workaround, is drafted in [UPSTREAM.md](UPSTREAM.md). Filing it needs an account, so the
maintainer files it from their own and its link then replaces the draft's; the draft and the
workaround are the proof, since a gate never needs a new account (ROADMAP, How to read this).
`scripts/check_workflows.py` fails when this section does not link that file, when an entry misses
its Reproducer, Versions, Workaround, or Upstream field, or when the Workaround's path does not
exist (`rule_upstream`). No Phase 10 failure has been traced to a QEMU bug.

Line-coverage floor for `vibeos-core` is **87%** (`[coverage] floor` in `tests/gates/inputs.toml`,
read through `check_gate_inputs.py --floor`). Measured 87.53% at `6cbe4fe` with `cargo llvm-cov -p vibeos-core --lib
--features std --target $HOST`, the command the `check` job runs. Ratchet the integer only upward.
Coverage is still not a percentage target for the kernel: every bug that gets fixed gets a test that
would have caught it, in the cheapest tier that can catch it. Every entry in
[section 9](PITFALLS.md#9-pitfalls) names the rule that guards it, and where that rule is only an invariant in
code with no test, that is a weaker guarantee and should be visible as such.

**Gate inputs.** A pull request does not quietly change the gates that judge it (ROADMAP §10.9).
`tests/gates/inputs.toml` lists the gate inputs (each check script and its test, `gatelib.py`,
`ci_history.py`, `gate.py`, `release_check.py` and `doc_refs.py`, the gate maps and needs files and
itself, every expected-failure and skip list, `tests/contract/markers.toml`, `deny.toml`, the
workflows, the Makefile's `check` and `gate` recipes, KERNEL_REVIEW.md, and the lint settings:
`clippy.toml`, `[workspace.lints]`, and `pyproject.toml`'s `[tool.ruff]` and `[tool.mypy]`) and holds
the floor above. `scripts/check_gate_inputs.py` compares the pull request's head with its merge base
(the `check` job on every pull request; `make check` against `origin/main` when that ref exists,
otherwise its static rules only) and fails when the pull request lowers the floor, which no trailer
allows; adds or changes a skip or expected-failure entry with no `Gate-change: <list> <entry>
<class>: <reason>` trailer, the class one of the list's condition fields and one the entry sets;
edits or removes any other input (a recipe or table: its text) with no `Gate-change: <path>: <rule or
gate line it serves> -- <why>` trailer; or changes a `#### Fnnn` heading or `**Severity:**` line of
KERNEL_REVIEW.md outside its `## Errata` section, with or without a trailer. Adding an input, raising
the floor, and removing a list entry need no trailer. A trailer counts from any non-merge commit of
the pull request. A correction to the review is a line under `## Errata`, its last `## ` heading,
`- <YYYY-MM-DD> · <Fnnn> · <what changed> -- <why>` with dates in order, committed with a trailer;
when `<what changed>` is a `**Severity:** ...` line, `check_review_refs.py` reads it in place of the
finding's own. `release.yml`'s `build` job writes `check_gate_inputs.py --summary --tag <tag>` to its
summary right after checking out the tag's commit, before any upload and so before any key job:
every input changed since the previous `v*` tag, the floor at both ends, and the `Gate-change:` lines
of the commits that changed each. The owner applies the rulesets
([RELEASING.md](RELEASING.md#repository-rulesets)): `main` and each `release/v*` branch require a pull
request and the `check` and `ci-pass` checks, and block force pushes and deletion, with no bypass
actor; `check_gate_inputs.py --rulesets` reads them with the owner's `gh` login. Until the owner
applies them, `main` requires no check.

Planned (ROADMAP §38.1): `make verify` checks the Verus proofs and TLA+ specifications on every push
that changes `vibeos-core` or `docs/specs/`. From the `phase-38` tag, a change that adds an operation
or a layer to a proved structure, or reorders a modelled protocol, extends its proof or specification
in the same commit, or lists the property it leaves unproved in `docs/VERIFIED.md`'s `Not proved`
section with an open ROADMAP §38.6 box (ROADMAP Phase 38, Changing proved code).
