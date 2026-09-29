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

## 8.1 Host unit tests

Anything in `crates/core/src/lib.rs` and its submodules, compiled as `vibeos-core` on the host. No hardware
access, no `unsafe` port I/O, no MMIO. The kernel half calls into it. Each port's pure half
([§11.1](PORTABILITY.md#111-the-seam)) is part of it and runs on every host. Rule; not yet enforced: ROADMAP §10.3.
The x86-only pieces (`switch_context` in `thread.rs`, the fences in `dma.rs`) are
`cfg(target_arch = "x86_64")`, so an aarch64 host such as the dev Mac compiles them and their tests
out. ROADMAP §10.3 moves them to the kernel crate, and the host test that runs a port's switch
assembly lives in `tests/hostlib`, which includes that port's assembly when the host's architecture
matches (ROADMAP §10.2, §11.4).

Things that belong here and are easy to get wrong, so should have tests from the day they are written:

- Buddy allocator: split, merge, exhaustion, fragmentation, alignment per order, free count returning
  to its initial value after a random alloc/free sequence, double free detection.
- ACPI: RSDP v1 and v2 checksum rejection, table length validation, HPET generic address structure
  rejecting I/O space and zero addresses, MADT entry iteration over truncated tables.
- Timekeeping: the `now_us` interpolation formula, seqlock retry under a simulated concurrent writer,
  monotonicity, overflow near `u64::MAX`, HPET/PIT agreement bands (invariant vs TCG).
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
  buffer; `/dev/null` `/dev/zero` `/dev/random` (virtio-rng, then RDRAND, then a
  xorshift fallback that ROADMAP §10.12 deletes, F134); procfs stubs do not
  panic.

Two lessons about writing these:

A test that derives its expected value from the same read it is checking proves nothing. The old
seqlock test computed the expected timestamp as `tick + 100`, which is monotonic no matter how torn the
read was, so it passed against a broken implementation. The writer has to publish an independent value
the reader can compare against.

A single-threaded test cannot observe a race. Simulate the interrupt-context writer explicitly, or
accept that the real coverage is in-guest.

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

Built into a separate Cargo target directory (`target-kernel-tests`) with its own ISO. This is not
fussiness: sharing a target directory means a feature-enabled ELF can end up packaged into the
production ISO, and the difference is not visible from the outside. The panic-dump and `#GP` ISOs
are `--features panic_test --features panic_exit` and `--features gp_test --features panic_exit`
(underscores everywhere; Cargo features in this crate do not use hyphens).

```
vibeOS: ktest: begin
vibeOS: ktest: ok <name>
vibeOS: ktest: FAIL <name>: <reason>
vibeOS: ktest: skip <name>: <reason>
vibeOS: ktest: end
```

The harness requires `begin` and `end`, rejects any `FAIL` line and any panic signature, and checks
the exit status. ROADMAP §10.2 makes it read each of these lines only when framed (§2.6).
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

Planned (ROADMAP §10.2): `begin` carries the number of runs the boot will make, after the command
line's filter and repeat count, and `vibeOS: ktest: run <name> <deadline_ms>` precedes each run,
with the deadline the kernel enforces in the guest (10 s unless the registry sets another). The
harness requires one result line per run line and exactly that many results. It has no whole-run
deadline. `VIBEOS_TIMEOUT` bounds each stretch in which no test runs: from QEMU's start to `begin`,
and from `end` to QEMU's exit. From `begin` to `end`, each run gets its printed deadline plus 5 s,
and each gap between lines 5 s, all multiplied by `env_config`'s one timeout scale. That backstops
the in-guest deadline, which a CPU wedged with IF=0 never checks; a timeout names the test of the
last run line and prints the partial line the guest was writing. Adding tests changes no timeout,
and a test that needs longer carries a registry override, reviewed as code. A per-subsystem list
left out of the aggregate registry is unreferenced code, which the `kernel_tests` clippy run with
`-D warnings` rejects as dead; the rule against a blanket `allow(dead_code)` in production modules
(ROADMAP §10.2, Q2) keeps that true. The `utest_*` lines of ROADMAP §10.5 follow the same protocol.

Skips are first class and carry their reason on the `ktest: skip <name>: <reason>` line. Every skip
names what the configuration lacks: `no AP`, `no virtio-blk`, `no virtio-rng`, `no e1000e`, `no edu`,
`no smep/smap/umip`, `pit owns tick`, `pic fallback`, and `rtc unread` (the `Outcome::Skip` reasons
in the in-guest test bodies, DESIGN §1.3). Destructive exception tests run inside `arch::catch` scopes, which longjmp out or
step RIP past the faulting instruction, instead of skipping.

Planned (ROADMAP §10.2): `tests/harness/skips.toml` lists each test allowed to skip, with its reason
and the configurations it skips in (architecture, accelerator, CPU model, CPU count, memory, machine
options, and the harness host). A run fails when its skipped set differs from the rows that match
its configuration, in either direction, so a lost `-device` or a regressed detection that turns
tests into skips fails the tier. Tests a run does not select print no run line and need no row, and
a test that `VIBEOS_KTEST` names without a glob must run whatever the file says (ROADMAP §12.3).

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

This is the full contract once the kernel is complete through the console phase. It grows one phase at
a time: a phase adds its markers to the harness in the same commit that emits them, and nothing is ever
removed silently. The executable contract is `boot_contract_markers()` in `tests/harness/harness.py`;
the list below gives its order, the paragraphs after it add the lines that depend on the machine (the
calibration source, the LAPIC timer mode, the per-AP pairs, and the partition children), and the
`_start` table in [section 3.3](BOOT.md#33-_start-order) says why each step sits where it does. Every line in
it is the kernel's except `shell ready`, which `/bin/sh` prints in the production ISO. ROADMAP §10.2
makes the harness match the kernel's lines only when framed (§2.6), and a line a user program prints
(`shell ready`, the ROADMAP §10.5 `utest_*` lines, `user: tests ok`) only when unframed; today it
matches every line.

Planned (ROADMAP §10.2): one registry, `tests/contract/markers.toml`, holds every line the harness
knows (contract markers, diagnostics, failure lines and halt reasons, and the ktest and utest
protocol), each with its architecture, the program that prints it, and the configurations it holds in.
The harness builds this contract and the failing-fast list from it, `scripts/check_markers.py` fails on
a `marker!` line with no row, and this section then keeps the rules and links the file instead of
listing lines.

```
vibeOS: serial online
vibeOS: limine: rev 3 ok
vibeOS: pmm: <n> free 4KiB frames
vibeOS: paging: cr3 ok
vibeOS: paging: mmio uc
vibeOS: heap ok
vibeOS: kva: ready
vibeOS: gdt ok
vibeOS: pic: remapped
vibeOS: idt ok
vibeOS: per_cpu: bsp ready
vibeOS: acpi: xsdt <n> tables
vibeOS: time: tsc <n>/ms
vibeOS: sched: cpu0 ready
vibeOS: irq: enabled
vibeOS: smp: done
vibeOS: console ok
vibeOS: pci: <n> devices
vibeOS: block: <name> <n> sectors
vibeOS: shell ready
```

Live e2e through Phase 6 slice A asserts through `idt ok`, then `per_cpu: bsp ready`,
then `acpi: xsdt`, then `time: tsc <n>/ms`, then `time: lapic_timer ok (<mode>)`, then
`sched: cpu0 ready`, then `irq: enabled`, then for each AP `sched: cpu<i> ready`
followed by `smp: ap online`, then `smp: done`, then `console ok`, then
`pci: <n> devices`, then `block: <name> <n> sectors`, then `shell ready`.
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
after waiting for `/bin/tests`. `init` passes no status pointer to `wait4`, and the harness matches
neither `user: tests ok` nor `user: tests fail`, so a failing `/bin/tests` passes every e2e variant
(ROADMAP §10.5, F073).

`smp: done` before `shell ready` is deliberate. Put SMP bring-up after the shell starts and an AP
failure becomes invisible, because the harness sees its last marker and passes. `pci: <n> devices`
sits between `console ok` and `shell ready` so `lspci` is registered before the prompt. The ramdisk
`block: <name> <n> sectors` line sits after PCI and still before the shell. Partition children emit
`block: <parent>p<N> <n> sectors` after the parent (e2e: `ram0p1`, `ram0p2`). virtio-blk adds
`block: vda <n> sectors` and `vdapN` when the ktest disk is present (not on the production e2e `pc`
set). The same blind spot follows the last marker: writeback, deferred reclaim, and vibefs commits
keep running after `shell ready`, and a panic there is invisible to a harness that stops reading at
it. Planned (ROADMAP §10.2): the console-input boot keeps reading serial for 3 s after its last
reply and fails on a panic signature in that window.

With `-smp N`, additionally:

- for each AP `i` in `1..N`, `vibeOS: sched: cpu<i> ready` then `vibeOS: smp: ap online`, in order,
  before `smp: done`. The harness requires at least these `N-1` pairs and does not reject an extra
  `ap online` line; ROADMAP §10.2 makes it count exactly `N-1` (F141)
- `vibeOS: sched: cpu<i> ready` for every `i` in `0..N`
- `vibeOS: time: lapic_timer ok (<mode>)` naming the selected timer path
  (`tsc-deadline`, `periodic`, or `pit`) rather than inferring it

The list above is the contract of a boot through Limine. Planned (ROADMAP §25.4, §26.4): a boot
through the image's direct entry prints `vibeOS: boot: <path> entry ok`, where `<path>` is `kexec`,
`crash`, or `pvh`, in place of `limine: rev <n> ok`. A crash entry, ROADMAP §25.4's capture kernel,
boots with `maxcpus=1` and so prints no `smp: ap online` line, and its list ends at
`vibeOS: vmcore: written <n> bytes` in place of `shell ready`, after which it resets.

### Failing fast

Scan for these (`PANIC_SIGNATURES` in `tests/harness/harness.py`) and, in a run that expects no
panic, fail immediately with the captured line rather than waiting out the timeout:

```
panicked at   vibeOS: panic:   #PF   #GP   #UD   #DF   double fault   stack overflow
```

Match the exception mnemonics, not the phrase "page fault". Shell help text and log messages contain
English words, and a substring match on prose produces false failures that erode trust in the suite.

Planned (ROADMAP §10.7, §12.5, §25.5): a registered failure line reports a failure the kernel
survived, so a run that shows one would otherwise pass. The blocked-thread sweep's
`vibeOS: sched: overdue tid <id>` (ROADMAP §10.7) is the first; `vibeOS: block: <dev> timeout` and
`vibeOS: block: <dev> reset` (ROADMAP §12.5) and ROADMAP §25.5's soft lockup, hard lockup, and
hung-thread reports follow. A test that provokes one on purpose declares it; in any
other run it fails the run, since a recovery no test expected is a bug a timeout hides, such as a
lost kick ([section 10.4](BLOCK.md#104-virtio-blk)) that shows only as a 30 s pause.

User programs print these strings too: the ROADMAP §10.5 runtime reports a panic as `panicked at` on
fd 2, and a fuzzer writes random bytes. ROADMAP §10.2 makes the harness scan framed lines only
(§2.6). Before the kernel's first framed line it fails fast on Limine's panic line, the one failure
that cannot be framed.

Expected-panic e2e waits for `vibeOS: panic: halted` so the dump (regs, thread, last log records,
backtrace) is in the captured log, then checks dump needles. Planned (ROADMAP §10.7, F135): before
`panic: halted` the dump prints one `vibeOS: panic: cpu N stopped (ipi|poll|nmi|panic)` or
`vibeOS: panic: cpu N not stopped` line for each other online CPU (§2.5 step 1), and the F135
variant checks them. `panic_exit` writes isa-debug-exit
`0x11` so QEMU leaves instead of sitting in `hlt`. The harness kills QEMU at `panic: halted` instead of
waiting for that exit, so it never checks status 35. It also matches boot markers on every line,
including the dump's `vibeOS: logrec:` replay of earlier records, so a marker printed out of order
before the panic can match again in the dump (ROADMAP §10.2, F141).

Planned (ROADMAP §10.7, §11.7), the event rule. The panic path signals pvpanic
([§2.5](INVARIANTS.md#25-panic-policy) steps 6 and 7), QEMU runs with `-action panic=pause`, and the harness
reads QEMU's QMP events. Each run declares the end it expects: none, the default; a panic, for the
expected-panic e2e; `expect=reset`, for a line whose guest resets and boots again; or
`expect=capture`, for a line whose panic reaches a capture kernel. `GUEST_PANICKED` pauses the
guest: a run that expects no panic takes a guest core, quits, and fails; the expected-panic e2e
checks its dump needles and quits; an `expect=reset` run sends `cont`. QEMU reports
`GUEST_CRASHLOADED` without pausing: a run not declared `expect=capture` stops the guest, takes a
core, and fails, and an `expect=capture` run waits for the capture kernel's
`vibeOS: vmcore: written <n> bytes` line and its reset, which ends QEMU under `-no-reboot`. A panic
signature with no event, from a panic before the kernel has found its pvpanic device, fails a run
that expects no panic at once, and the harness takes the core after `vibeOS: panic: halted` or
10 s, whichever comes first. An `expect=reset` run boots without `-no-reboot`, fails on more QMP
`RESET` events than its line expects, and is judged by the markers its line names. `expect=reset`
and `expect=capture` runs take a core only when they fail: a core taken after a crash jump still
describes the crashed kernel, because a kernel entered through the crash path never writes QEMU's
`vmcoreinfo` device (ROADMAP §10.7).

On success, exit through the QEMU monitor's `quit` rather than waiting for the timeout. Two seconds
versus forty five, on every CI run and every local invocation.

### Harness

Python, standard library only. `subprocess` with its own timeout rather than shelling out to GNU
`timeout`, which does not exist on macOS. The harness helpers get their own unit tests, because a bug
in the test harness produces either false confidence or a debugging session in the wrong repository.
Those tests exercise `check_markers_in_order`, which no runner calls; `run_qemu_and_check`, the
matcher every e2e run uses, has no unit test (ROADMAP §10.2, F141).

The `vibefs_crash` build (`vibefs_init::crash_loop`) prints no boot contract past its own lines,
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
| `make run` | `-cdrom vibeos.iso -m 128M -smp 2 -cpu max -accel tcg -no-reboot -serial stdio` (Makefile `QEMU_BASE`, plus `-serial stdio` from the `run` recipe) |
| e2e | as above plus `-display none -monitor unix:...,server=on,wait=off` (`harness.qemu_argv`) |
| ktest | as e2e plus `-device isa-debug-exit,iobase=0xf4,iosize=0x04`, `-device e1000e`, `-device edu` (planned, ROADMAP §11.7: `-device edu,dma_mask=0xFFFFFFFF` on both architectures), `-device virtio-rng-pci,disable-legacy=on`, virtio-blk (`-drive file=…,if=none,id=vibehd,format=raw,cache=writeback,discard=unmap` + `-device virtio-blk-pci,drive=vibehd,disable-legacy=on,num-queues=<smp>`). Extra NICs/edu/virtio are ktest-only; e2e stays the default `pc` set (`pci: 6 devices`). After a green first boot the harness reboots the same disk and requires `vibeOS: persist: intact`. |
| vibefs crash | as e2e plus `-boot order=d` and the volatile-cache device: `-drive file.driver=nbd,file.server.type=unix,file.server.path=<sock>,format=raw,if=none,id=vibehd,cache=<writeback\|none\|writethrough>` + `-device virtio-blk-pci,drive=vibehd,disable-legacy=on,num-queues=<smp>,write-cache=on` (`harness.virtio_blk_args(..., nbd=True)`). QEMU 8.2 accepts the `file.driver=nbd` form; `cache=unsafe` is refused, since it drops flushes |
| LAPIC fallback | `-cpu qemu64,-tsc-deadline` |
| SMP stress | `-smp 4` |
| aarch64 (`ARCH=aarch64`) | Planned (ROADMAP §11.7): `qemu-system-aarch64 -machine virt,acpi=off,gic-version=3`, with `-cpu max` under TCG or `-cpu host` under HVF (`virt` defaults to the 32-bit `cortex-a15`); the §10.2 probe's firmware code read-only on pflash unit 0 and a per-run copy of its variable-store template on unit 1; the ISO on a CD-ROM, `-device virtio-scsi-pci -device scsi-cd,drive=cd0 -drive if=none,id=cd0,media=cdrom,readonly=on,file=<iso>`, so the ktest disk is the only virtio-blk device; and `-device ramfb`, `virtio-keyboard-pci`, `virtio-tablet-pci`, `pvpanic-pci`, and `vmcoreinfo` |
| Interrupt debugging | `-d int,cpu_reset`, plus `-machine q35` when chipset behavior matters |

Harness and `make test` default to `-accel tcg` so KVM does not introduce timing flakes.

`-no-reboot` matters: a triple fault otherwise reboots and loops, and the serial log fills with
repeated boot attempts instead of stopping at the interesting one. Planned (ROADMAP §10.7): a run
declared `expect=reset` (§8.3) boots without it and counts QMP `RESET` events instead, so a reset
its line does not expect still fails the run.

All `VIBEOS_*` overrides are read in `tests/harness/harness.py` (`env_config` / `env_flag` /
`env_int`). Drivers do not parse the environment. Makefile `?=` values are the `make run` source;
harness defaults match them.

| Variable | Default | Who honours it |
|----------|---------|----------------|
| `VIBEOS_ISO` | per driver (`vibeos.iso`, `vibeos-ktest.iso`, `vibeos-vibefs-crash.iso`) | all drivers |
| `VIBEOS_SMP` | `2` | all; `make run` |
| `VIBEOS_QEMU_CPU` | `max` | all; `make run` |
| `VIBEOS_MEM` | `128M` | all; `make run` |
| `VIBEOS_BIOS` | unset (SeaBIOS) | all |
| `VIBEOS_QEMU_ACCEL` | `tcg` (empty omits `-accel`) | all; `make run` |
| `VIBEOS_TIMEOUT` | `60` e2e/ps2, `90` ktest/crash; planned (ROADMAP §10.2): the §8.2 boot allowance, which bounds only the stretches of a boot in which no test runs | all drivers |
| `VIBEOS_QEMU_EXTRA` | empty | all drivers |
| `VIBEOS_TIER` | `adhoc`; each `make test-*` recipe sets its target name | all drivers, which write `build/results/<arch>-<tier>.json` (schema 1, `tests/harness/results.py`) |
| `VIBEOS_EXPECT_PANIC` | off (`""` / `0`) | `run_e2e` |
| `VIBEOS_GP_TEST` | off | `run_e2e` |
| `VIBEOS_EXPECT_PIT` | off | `run_e2e` |
| `VIBEOS_MCE_TEST` | off | `run_e2e` |
| `VIBEOS_SKIP_PERSIST` | off | `run_ktest` |
| `VIBEOS_CRASH_ROUNDS` | `8` | `run_vibefs_crash` |
| `VIBEOS_CRASH_SEED` | time-based | `run_vibefs_crash` |
| `VIBEOS_MKFS` | `mkfs-vibefs` | `run_vibefs_crash`, `run_e2e` (the `test-e2e` tier's vda images) |
| `VIBEOS_FSCK` | `fsck-vibefs` | `run_vibefs_crash` |
| `VIBEOS_NBD_CACHE` | `nbd-cache` | `run_vibefs_crash` |
| `VIBEOS_VIBEFS_CAT` | `vibefs-cat` | `run_vibefs_crash` |

`VIBEOS_BIOS` reaches QEMU as `-bios`, which accepts only an image whose size is a multiple of
64 KiB. apt's combined `/usr/share/ovmf/OVMF.fd`, the Makefile's `OVMF` default and the one CI uses,
boots. Homebrew's code-only `edk2-x86_64-code.fd` is refused and needs `-drive if=pflash` instead.
`make test-e2e-uefi` prints a skip message when `OVMF` does not exist and then runs the harness
anyway, because the check and the run are separate recipe lines (ROADMAP §10.2, F079).

## 8.5 Make targets

`make help` prints the live inventory. Do not hand-maintain a second list here.

`make check` is the fast local gate (rustfmt `--check`; clippy `-D warnings` on `vibeos-core` and hostlib
for the host, on `vibeos-core` for `x86_64-unknown-none`, and on the kernel with its default features; host
unit tests, harness unit tests, ruff/mypy when installed). CI runs it as the `check` job before QEMU (DESIGN §8.6).
`make test-e2e` is enough when only boot output or QEMU wiring changed. `make test` is the gate before
a PR. `make test-ps2` is the focused #66 sendkey boot; `make test-e2e` already runs it, so `make test`
does not boot it twice.

## 8.6 CI and coverage

Two jobs run on every push and pull request, on Linux, and `ticks` on pull requests; the other
rows below are scheduled, dispatched, or run on a tag. `concurrency` cancels superseded runs that
share a group, one per branch and event (`github.event_name` is in the key): a push run never
cancels a pull request's run or its `ticks` job, a merge to `main` still cancels the push run of
the merge before it, and a fork's pull request from its own `main` shares a slot only with other
pull requests from a branch named `main`, never with `main`'s push runs. Planned (ROADMAP §10.1): `ci` runs on pushes to
`main`, pull requests, and `workflow_dispatch`; a pull request's runs share one group per pull
request number and cancel superseded ones, and every other run has its own group, so no `main` run
is cancelled. A `pull_request` run never counts as proof of a commit (ROADMAP §10.9). The earlier
one-ladder-job rule (runner queues) was lifted on 2026-09-22: the repo is public, so Actions minutes
are free, and agents own the CI design. ROADMAP §10.1 plans a build-once job plus a tier matrix per
architecture; until that lands the ladder is one job.

| Job | When | What |
|---|---|---|
| `check` | push / PR | Installs `x86_64-unknown-none`. `make check` (fmt; clippy `-D warnings` on `vibeos-core` and hostlib for the host, `vibeos-core` for `x86_64-unknown-none`, and the kernel with default features; host units, harness, ruff/mypy, `scripts/check_*.py`) then `cargo llvm-cov -p vibeos-core --lib --features std --target $HOST --fail-under-lines 87`. No QEMU, no `setup.sh`. HTML report is a 7-day `core-coverage` artifact. |
| `phase 0 ladder` | push / PR, `needs: check` | Limine, QEMU/nasm/xorriso/OVMF, kernel clippy `-D warnings` once for each other feature set an ISO is built with (`kernel_tests`, `vibefs_crash`, `panic_test` with `panic_exit`, `gp_test` with `panic_exit`) and once with `kernel_shell` (the default set runs in `check`); ISO, e2e (BIOS/UEFI/panic/#GP/#MC/PIT/9 GiB), in-guest at `-smp 2` and `-smp 4`, LAPIC fallback, vibefs crash. Even after a failed step it writes a per-tier table and every harness retry to the job summary and uploads `build/results/` as `results-x86_64-phase0`. Green `main` uploads `vibeos.iso` (7 days). |
| `ticks` | PR, `needs: phase0`, even after it fails | `scripts/check_ticks.py --base <PR base> --head <PR head> --run-commit $GITHUB_SHA --results <downloaded results-*> --summary $GITHUB_STEP_SUMMARY`: every box a commit of the pull request ticks pairs with a `Proves:` line, its proof exists at the head and is changed by the pull request or marked `(existing: ...)`, a ktest, utest, or marker proof passed in a results file of the head or the tested merge commit, no results file lists a retry, needs and closes rows hold, `Fails-before:` lines are present, and a bracketed proof passed on a scheduled run or `ci-history` record (read through `gh`, with `contents: read` and `actions: read`). The summary lists errors, `(existing: ...)` proofs, and notes. `make check` runs the pairing and diff rules bare against `origin/main` and skips them when that ref is missing, as in the `check` job's shallow checkout. |
| `smp-stress` | weekly Monday 06:00 UTC + dispatch | `-smp 4`, longer timeout (`VIBEOS_TIMEOUT=180`); planned (ROADMAP §10.2): the §8.2 per-run deadlines, with no longer timeout |
| `nightly-canary` | same workflow, non-blocking | undated latest nightly, `make iso && make test-unit` |
| `release` | `v*` tags | `make test-e2e` (BIOS) only, then production + ktest ISO, changelog section, GitHub Release. It does not wait for `ci` at the tagged commit, and the ktest ISO writes fixed LBAs of any virtio-blk disk attached at boot (ROADMAP §10.1, F145). Planned (ROADMAP §10.1): dispatched from `main` with the release tag as input; a `build` job with `contents: read` and `actions: read`, no cache, and no persisted token, then a `publish` job that runs no repository script; from ROADMAP §14.6 a `sign` job in the `release` environment between them, and from §22.4 a keyless `verify` job on vibeOS. From ROADMAP §18.7 the `sign` job is two key jobs, `sign-files` and `sign-manifest`, with an unprivileged `assemble` job between them, since images hold the signed kernels and Limine binaries and the manifest lists the images (ROADMAP §22.1). |

The `ticks` job (ROADMAP §10.9) runs after the jobs that run the tiers, today the ladder, and reads
the `build/results/` files they upload. A pull request run tests the merge of its head with its
base, so the results files carry the merge commit, which `--run-commit` names; `check_ticks.py`
reads commits and their messages from the pull request's head.

Rule; not yet enforced: a job that holds a signing key or a write token runs no code from the
candidate commit, restores no cache, checks out nothing, and receives only artifacts and their
SHA-256 list (ROADMAP §10.1, §14.6). Today `release` builds, tests, and publishes in one job with
`contents: write`, a persisted checkout token, and restored caches.

**Runners.** Planned (ROADMAP §11.7): every job that boots an aarch64 guest runs on an arm64 runner
(`ubuntu-26.04-arm`, or the scheduled macOS job's arm64 image), never on an x86_64 one. TCG adds no
ordering to an aarch64 guest's loads and stores, so only an arm64 host lets a weak reordering reach
guest code; an x86_64 host runs them in its TSO order. `scripts/check_workflows.py` checks it. From
ROADMAP Phase 11 on, `make gate` also needs two dev-host records that loop the -smp 4 in-guest tier
and smp-stress under HVF for 30 minutes each (`tests/gates/common.toml`), the only gate that runs
those tests on a weakly ordered CPU directly. The weekly aarch64 smp-stress leg records whether TCG
there showed any weak outcome (`weak_order_probe`).

**Scheduled capacity.** Planned (ROADMAP §10.1): the Free plan's 20 concurrent jobs are the owner's
account's, shared with its other repositories, and scheduled and dispatched workflows hold 10 of
them as lanes. A lane is a job-level concurrency group, `sched-lane-<n>`, with `queue: max`: it runs
one job at a time and holds up to 100 waiting, first in first out. Without `queue: max` a group
keeps one waiting job and cancels it when another arrives, and GitHub runs no queue across
workflows, so lanes are how the split holds. This section will hold the lane map, which reserves for
jobs that end within 5.5 hours the lanes their cadence needs and names the lanes a release window
takes (ROADMAP §22.1), and a ledger row per workflow: cadence, jobs per run, job-hours per run
(estimated, then measured from `ci-history`), peak concurrent jobs, and lanes.
`ci_history.py --budget` holds every lane but the rebuilds' under 60% busy and every reserved-lane
wait under 12 hours. The 40% left absorbs GitHub's delays to scheduled runs and new workflows, and
keeps the account from running its share full around the clock, which GitHub's Actions terms count
against it when the burden is disproportionate to the benefits. The section also records each
per-push tier's median QEMU time, which `ci_history.py --tiers` keeps under 60 s, and the
`ci-history` branch's packed size (ROADMAP §10.9).

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
hostlib lint failure should go red in about a minute without starting QEMU. From ROADMAP §10.9's CI
history on, a measured number recorded in the design docs cites the commit it was measured at and the CPU
model or machine it ran on (ROADMAP, How to read this).

Line-coverage floor for `vibeos-core` is **87%** (`--fail-under-lines 87` in
`.github/workflows/ci.yml`). Measured 87.53% at `6cbe4fe` with `cargo llvm-cov -p vibeos-core --lib
--features std --target $HOST`, the command the `check` job runs. Ratchet the integer only upward.
Coverage is still not a percentage target for the kernel: every bug that gets fixed gets a test that
would have caught it, in the cheapest tier that can catch it. Every entry in
[section 9](PITFALLS.md#9-pitfalls) names the rule that guards it, and where that rule is only an invariant in
code with no test, that is a weaker guarantee and should be visible as such.

Rule; not yet enforced: a pull request does not quietly change the gates that judge it. From ROADMAP
§10.9, `tests/gates/inputs.toml` lists the gate inputs (the check scripts and their tests, the gate
maps, every expected-failure and skip list, `deny.toml`, the workflows, the Makefile's `check` and
`gate` recipes, and KERNEL_REVIEW.md) and holds the floor above; `scripts/check_gate_inputs.py`
compares each pull request with its merge base and fails when it lowers the floor, adds an
expected-failure or skip entry or edits or removes another input without a `Gate-change:` trailer, or
edits a finding's heading or severity line; and rulesets require `check` and `ci-pass`, one job that
needs every per-push job, on `main`. Today the floor is a literal in the workflow a pull request can
edit, the check scripts judge the pull request that edits them, and `main` requires no check.

Planned (ROADMAP §38.1): `make verify` checks the Verus proofs and TLA+ specifications on every push
that changes `vibeos-core` or `docs/specs/`. From the `phase-38` tag, a change that adds an operation
or a layer to a proved structure, or reorders a modelled protocol, extends its proof or specification
in the same commit, or lists the property it leaves unproved in `docs/VERIFIED.md`'s `Not proved`
section with an open ROADMAP §38.6 box (ROADMAP Phase 38, Changing proved code).
