# Changelog

All notable changes to **vibeOS** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

One or two lines per entry. Say what changed for someone running vibeOS (new command, new
marker, new device, fixed hang). Link to the ROADMAP section instead of describing the design.

## [Unreleased]

### Added

- aarch64 per-push CI on arm64 TCG, `make litmus`, and dual README quickstarts
  ([ROADMAP §11.7](docs/ROADMAP.md#117-build-harness-ci)).
- aarch64 boot CPU: GICv2/v3, generic timer, full vector table, idle `wfi`
  ([ROADMAP §11.3](docs/ROADMAP.md#113-interrupts-and-time)).
- RAM-only physmap: a 9 GiB guest puts RAM above 8 GiB in the buddy; MMIO
  is `ioremap` ([ROADMAP §11.2](docs/ROADMAP.md#112-memory)).
- One portable `MachineDesc` from the device tree and ACPI; reserved FDT
  ranges stay out of the buddy ([ROADMAP §11.5](docs/ROADMAP.md#115-devices)).
- `make check` fails a Relaxed, Acquire, Release or AcqRel ordering with no
  comment naming what it pairs with ([ROADMAP §11.7](docs/ROADMAP.md#117-build-harness-ci)).

### Changed

- Limine 12.9.1; the boot handshake is base revision 6 (`vibeOS: limine: rev 6 ok`).

### Fixed

- aarch64 EL0 cannot read or write `SCXTNUM_EL0`, and an EL0 `wfi` traps and
  returns at once ([ROADMAP §11.6](docs/ROADMAP.md#116-user-mode)).
- x86 boot under a hypervisor does not read `MSR_PLATFORM_INFO`, so a missing
  MSR's `#GP` does not halt the BSP.
- An unaligned or overlapping ACPI memory-map entry no longer halts boot at
  `physmap map failed` ([ROADMAP §11.2](docs/ROADMAP.md#112-memory)).
- x86 reset and power-off map a memory-space ACPI register before writing it, so
  that path does not page-fault ([ROADMAP §11.2](docs/ROADMAP.md#112-memory)).
- At EL2 with VHE, an EL0 read of the physical counter traps, as it does at EL1
  ([ROADMAP §11.6](docs/ROADMAP.md#116-user-mode)).
- aarch64 TLS with `p_align` above 16 is placed at `TP + align`, so an
  align-64 thread-local reads its initial value ([ROADMAP §11.6](docs/ROADMAP.md#116-user-mode)).
- `make ARCH=aarch64 check` after `./setup.sh` lints the aarch64 kernel and builds
  `vibeos-core` with the MSRV toolchain for that target.
- a GICv2 SGI end-of-interrupt keeps the source CPU the acknowledge
  returned, so that CPU's running priority drops ([ROADMAP §11.3](docs/ROADMAP.md#113-interrupts-and-time)).
- aarch64 `pvpanic-pci` with a firmware BAR at 0 is placed in the virt 32-bit MMIO
  window, so a panic reaches QEMU ([ROADMAP §11.7](docs/ROADMAP.md#117-build-harness-ci)).
- a reused call-function IPI runs the round that was published, not the
  previous one ([ROADMAP §11.4](docs/ROADMAP.md#114-smp-and-per-cpu)).
- aarch64 `munmap` and a `brk` shrink drop the page from the TLB before
  the frame is freed, so a later EL0 access is `SIGSEGV`.
- aarch64 leaf TLB invalidates drop a kernel address's high bits, so the
  TTL hint cannot skip the invalidate ([ROADMAP §11.2](docs/ROADMAP.md#112-memory)).
- a failed `RNDR` is not entropy, so `/dev/random` does not fill zeros
  ([ROADMAP §11.5](docs/ROADMAP.md#115-devices)).
- a failed `irq::set_affinity` leaves dest CPU unchanged, and GIC SPIs
  stop at INTID 1019 ([ROADMAP §11.3](docs/ROADMAP.md#113-interrupts-and-time)).
- aarch64 EL1 vector entry no longer clobbers `x16`, which panicked as
  `#DABT` in `cmdline::Words::next` ([ROADMAP §11.7](docs/ROADMAP.md#117-build-harness-ci)).
- aarch64 UEFI boots prefer AAVMF no-secboot and use `-cpu neoverse-n1`
  under TCG so AAVMF 2025.11 reaches BDS ([ROADMAP §11.7](docs/ROADMAP.md#117-build-harness-ci)).
- Every CPU clears `EFER.FFXSR`, so a context switch on AMD saves the XMM registers, and turns off
  CPUID faulting that firmware left on, so `cpuid` runs in ring 3.

## [0.10.0]

Phase 10 exit: consolidation. The 2026-09-23 kernel review's fixes, the user runtime in Rust, the
forensics tools, and the gates and CI that hold every later phase. See
[Phase 10](docs/ROADMAP.md#phase-10-consolidation).

### Added

- SMEP/SMAP/UMIP and `CR0.WP` on every CPU (`arch::cpu::init_control_regs`).
- MIT license (`LICENSE`). Every crate manifest declares `license = "MIT"`.
- Tracked `AGENTS.md` (DOC3). Cursor rules and `CLAUDE.md` point at it.
- `make check` as the fast local gate (host clippy, host units, harness, ruff/mypy).
  `make help` lists targets.
- Each `make test-*` tier writes `build/results/<arch>-<tier>.json`
  ([ROADMAP §10.2](docs/ROADMAP.md#102-build-and-harness)).
- `meminfo` prints how many frames a dropped ownership token leaked.
- `brk`, anonymous `mmap` and `munmap`, eagerly backed.
- A kernel command line from limine.conf's `cmdline:` and QEMU fw_cfg `opt/vibeos/cmdline`;
  `vibeos.strace=1` prints one line per syscall.
- `loglevel=` on the kernel command line sets the boot log level, with Linux's numbering.
- Each in-guest test prints its name and deadline before it runs and fails when it passes the
  deadline; `vibeos.ktest=` and `vibeos.ktest_repeat=` select and repeat tests.
- `vibeos.ktest_range=<from>..<to>` runs one stretch of the in-guest registry; CI's in-guest tiers
  run as shards of under 60 s (`make test-kernel-<k>`, [ROADMAP §10.1](docs/ROADMAP.md#101-gates-and-pinning)).
- `make debug` starts QEMU halted with a gdb stub, and `scripts/vibeos.gdb` loads the kernel and
  user ELFs; `make run` and `make run-panic` start QEMU through the harness.
- An `irqoff` kernel build and `make test-irqoff`, which log every interrupts-off stretch longer
  than 100,000 instructions with the code that turned interrupts off.
- A nightly workflow with an x86_64 KVM leg and a release-profile boot, weekly 20-repeat in-guest
  runs, and ten scheduled CI lanes with a ledger and a budget check.
- A counted object whose last reference drops in atomic context is released by a workqueue worker,
  and an operation gate fails new operations on a removed object.
- Block devices and partitions are counted handles in one registry, and FAT and vibefs mount any of
  them by name (`mount fat32 vdap1 /x`).
- A second virtio-blk disk binds as its own driver instance (vdb), and FAT and vibefs volumes are
  per-device instances instead of two fixed slots each.
- Loom models of the seqlock latch, the wake inbox, the log ring, the IoWaiter completion and the
  on_cpu hand-off, each with a weakened variant loom must reject.
- A no_std Rust user runtime under user/, built for x86_64-unknown-linux-musl by `make user`; rerun
  ./setup.sh to install that target.
- `getdents64`, `fstat`, `nanosleep` and `reboot` syscalls, and a heap for Rust user programs over
  `brk`.
- /bin/sh runs programs from PATH and reports a non-zero exit or a signal, with poweroff, reboot and
  ps built in; /bin/ls, cat, echo, grep, wc, true, false, sleep, yes and cmp.
- Every e2e variant asserts /bin/tests' utest results, which cover every documented errno, every
  bad-pointer form, fork limits, orphans, exec chains and job-control signals.
- The kernel publishes a VMCOREINFO note (build id, page-table root, log, thread and CPU tables)
  that QEMU copies into every guest core.
- A per-CPU flight recorder, on in every build, that records syscalls, switches, wakes, IRQs, IPIs,
  page faults and block requests.
- An AP TSC warp test at bring-up (vibeOS: smp: tsc skew <n> cycles) and a Chrome trace-event export
  that orders across CPUs only on a warp-free invariant TSC.
- `make test-forensics`, and a failed QEMU run now prints the core tool's report (per-CPU
  backtraces, threads, last log records) after the serial tail.
- A thread still blocked 5 s past its deadline is reported as `sched: overdue tid <id>`, and the
  test harness fails a run that shows it.
- Deny.toml and `cargo deny check licenses bans sources` in make check: a crate joins the dependency
  graph only when deny.toml's [bans] allow list names it.
- Every ISO carries /LICENSES/ with vibeOS's LICENSE and THIRD-PARTY-NOTICES.txt, the notices its
  third-party code requires.

### Changed

- Partitions are numbered by their place on disk, as Linux numbers them: an MBR's logical
  partitions start at `p5` (ram0's are now `ram0p5` and `ram0p6`), and a GPT entry is its index + 1.
- A GPT is read only behind a protective MBR, as Linux reads it; a plain MBR wins over stale GPT headers.
- A FAT file stops at 4 GiB as on Linux: a write is cut short there, and a truncate or seek past it fails.
- `execve` opens its file before it reads `argv`, and `getdents64` of a non-directory is `ENOTDIR`;
  `open`, `unlink`, `rename`, `read`, `write` and `lseek` return Linux's errnos in Linux's order.
- `kill` of a zombie, a process that exited and is not yet reaped, returns 0 and discards the
  signal, as on Linux, instead of ESRCH.
- A virtio-blk disk with a block size that is not a power of two from 512 to 4096 is refused at probe.
- Roadmap restructured to 40 phases in eight eras, all on free infrastructure: [Phase 10 Consolidation](docs/ROADMAP.md#phase-10-consolidation)
  and [Phase 11 Portability](docs/ROADMAP.md#phase-11-portability) added; old phases 10–20 are now 12–22.
- CI: `check` job (`make check` + `vibeos-core` llvm-cov floor 87%) runs before the QEMU ladder.
- Crate version is `0.8.0` (Phase 8 backfill). Changelog cut; entries are ≤ 2 lines.
- Nightly date, Action SHAs, and Limine commit are pinned. Weekly smp-stress runs a non-blocking
  latest-nightly canary.
- Kernel builds with built-in `x86_64-unknown-none` (no custom target JSON, no `build-std`).
- Host tests (`make test-unit`) run `vibeos-core` on the host triple; `mkfs`/`fsck-vibefs` follow.
  A daily macOS job runs `make check` and `make test` ([ROADMAP §10.2](docs/ROADMAP.md#102-build-and-harness)).
- Initrd is `build/initrd.fat` from hostlib `mkinitrd`, loaded as a Limine module. The AP
  trampoline is `global_asm!`, not nasm.
- ISO and ksyms recipes are one `KERNEL_VARIANT` template plus `scripts/mkiso.sh`.
- One QEMU launcher (`tests/harness/harness.py`) for e2e, ktest, PS/2, and vibefs-crash.
  `VIBEOS_*` is read there; drivers are `tests/harness/run_*.py`.
- README / DESIGN header / §1.3 match the tree. O1 superseded: Phase 9 closed 2026-09-22;
  no pause/resume note.
- Portable crate denies clippy `unwrap`/`expect`/`panic!` (E1), and its byte parsers deny
  `indexing_slicing` and `arithmetic_side_effects` ([ROADMAP §10.1](docs/ROADMAP.md#101-gates-and-pinning)).
- Every kernel build and ISO variant denies warnings, and CI lints each ISO's feature set (ROADMAP
  §10.1).
- In-guest tests run on a kernel thread with interrupts on, the context production kernel threads
  use, instead of on the boot thread with interrupts off.
- `vmap` returns a move-only handle that `vunmap` takes, so the span unmapped always equals the span
  freed.
- Kernel file operations reach FAT, vibefs, ramfs and kernfs only through the VFS, and a second
  mount of a mounted block device shares its superblock (EBUSY when its read-only flag differs).
- The block layer has no Barrier; a Flush is sent as soon as it is submitted, and a write can carry
  Fua, which the block layer completes with a Flush on devices without FUA.
- A user `int3` now ends the process with SIGTRAP, as on Linux, instead of SIGSEGV.
- Every CPU sets CR0.AM, so a user program that sets RFLAGS.AC gets SIGBUS for a misaligned access,
  as on Linux.
- Every entry from user mode saves a Linux user_regs_struct frame on the thread's kernel stack.
- The source tree has one directory per subsystem in `crates/core/src/` and `src/`, and
  scripts/check_module_map.py keeps DESIGN §1.3 equal to it.
- Production kernels no longer carry the in-guest test exception hook or the ramdisk fault-injection
  check.
- The panic dump's log line also counts records the serial sink or a nested log call dropped.
- The lock-rank checker ranks the heap first and every cross-CPU lock, and refuses a second lock of
  a held rank outside lock_nested.
- Every unsafe block carries a SAFETY comment, the kernel denies panicking calls, and test hooks no
  longer ship in production modules.
- E2e checks the panic exit status, exactly N-1 AP lines, the first kernel line, and meminfo; an
  early QEMU exit shows its status and stderr.
- Kernel console lines start with the invisible byte 0x1E, and a 0x1E a program writes to the
  console prints as `?`.
- `make test-kernel` fails naming the test that hung, and fails when the skipped tests differ from
  tests/harness/skips.toml.
- Build outputs live under build/: build/vibeos.iso, build/vibeos-ktest.iso, and kernel ELFs in
  build/kernels/.
- The in-guest clock tests match every unclamped read against independently published tick records
  and fail on a planted seqlock tear; pit_tick_rate counts PIT interrupts in an hpet=off boot.
- CI runs on the pinned ubuntu-26.04 image and fails when its QEMU moves; one build job feeds
  per-tier jobs.
- A release is dispatched from `main` with its tag and publishes only `vibeos.iso`, built with the
  release profile, once `ci` passed on the tagged commit ([RELEASING.md](docs/RELEASING.md)).
- The kernel holds 256 processes, 1024 threads, 256 descriptors per process and 1024 open files;
  fork on a full thread table returns EAGAIN instead of halting.
- Every path syscall resolves through the VFS, so processes reach /dev, /proc, /tmp and /sys and
  open("/dev/null") opens devfs's null.
- Open and execve accept non-UTF-8 paths and arguments; access-mode, full-table and
  unsupported-operation errors (EBADF, EMFILE, EPERM, EACCES, EXDEV) match Linux.
- /dev/random and /dev/urandom return only virtio-rng and RDRAND bytes: a short read when they run
  low, EAGAIN when they have none.
- Syscall arguments are cut to their C types as on Linux, so `wait4` with a 32-bit `-1` pid waits
  for any child.
- The initrd is a Limine module sized from its contents, no longer a 64 KiB image inside the kernel.
- /sbin/init, /bin/sh, /bin/tests and /hello are Rust programs; building vibeOS no longer needs
  nasm.
- `execve` passes the caller's environment, takes Linux's argument limits (131,072-byte strings, 2
  MiB in all), and gives an empty `argv` an empty `argv[0]`.
- Exec and fork fill a new address space in bounded chunks, one page table at a time, with
  interrupts on between chunks.
- After SMP bring-up a kernel read of address 0 faults, and boot runs on a guarded 64 KiB stack.
- Ring 3 uses Linux's selectors (CS 0x33, SS 0x2b, null DS/ES/FS/GS); fork and execve follow the
  psABI for FP state.
- Under QEMU a kernel panic now signals the pvpanic device, and the test harness keeps a
  zstd-compressed guest core of every failed run.
- `ps` shows each process's syscall count.
- CI pins `ruff` and `mypy`; `make check` fails without them, Rust 1.98, or `fsck.fat` unless
  `VIBEOS_ALLOW_MISSING_TOOLS=1`.

### Fixed

- A host that stalls QEMU no longer drops the timer tick to the PIT: the LAPIC timer is proved
  against the PIT's interrupts, and its calibration divides by the time its window really took.
- `kill` of a zombie returns 0 as on Linux; the NBD test server treats a macOS client's close as EOF.
- `fsync`-style flushes can no longer be held off by a writer that keeps dirtying pages, and a
  page being read in has one cache slot, so a later read cannot see a stale second copy.
- Partition tables on 4 KiB-sector disks are read; a FAT mount there fails with `EINVAL`, not `EIO`.
- tmpfs supports `rename`; rename replaces its target as rename(2) does on every filesystem.
- `lseek` takes `SEEK_DATA` and `SEEK_HOLE` (`ENXIO` past the end) and is `ESPIPE` on any console
  descriptor first; a file with no write is `EINVAL` before `write` checks its buffer.
- A stopped process holds every signal but `SIGKILL` until `SIGCONT`, as on Linux; `wait4` no
  longer sleeps through a pending `SIGKILL`, and signal default actions follow signal(7).
- A scan that unlinks each entry it reads sees them all on ramfs and tmpfs; `..` climbs out of
  mounts stacked on one directory; unlink and rename no longer race an inode's eviction.
- FAT counts free clusters at mount instead of trusting FSInfo; vibefs stamps inodes with the wall
  clock and a refused unlink keeps the name.
- Every CPU zeroes the SYSENTER MSRs, and a failed `ioremap` gives its window back.
- `/dev`, `/proc`, `/sys` and `/tmp` are no longer capped at 128 nodes between them: kernfs's
  node table grows on the heap, and `ENOMEM` when it cannot.
- `execve` of a program on `/tmp` stays within its thread's kernel stack budget; tmpfs's cache
  no longer puts two pages on the stack.
- `/tmp` has its own `nr_inodes` (half of RAM's pages, as Linux's tmpfs): a full `/tmp` is
  `ENOSPC` to the writer and no longer crowds out `/dev`, `/proc`, or `/sys` (#195).
- Boot with more than 8 GiB of RAM no longer triple-faults after `heap ok`. RAM above the
  8 GiB physmap cap is left unused; `make test-e2e-highmem` boots with 9 GiB.
- UEFI e2e no longer hangs on OVMF PXE after a green marker boot; a timed-out boot prints
  its serial tail.
- On macOS, `nbd-cache` (the crash test's NBD device) takes its client's close as EOF, as on Linux,
  instead of failing with the `EINVAL` Darwin gives a socket option once the peer has gone.
- A process orphaned while no init runs is freed when it exits instead of holding a process slot as
  a zombie.
- The -smp 4 in-guest msix_cpu test no longer fails at random; its interrupt observer publishes its
  hit count last.
- A TLB shootdown delayed by a CPU with interrupts off no longer panics after 1 s; it keeps waiting
  and logs the late CPUs once a second (F011).
- A block I/O completion no longer touches its waiter once the waiter can return, so it cannot write
  into a reused stack frame (F002).
- `BootCell` and `IrqCell` carry std's `Send`/`Sync` bounds, and other CPUs read per-CPU counters
  only through an atomic remote view.
- An exiting process no longer leaves its thread's saved CR3 naming the page table it frees, and
  teardown refuses a root any CPU or thread still names.
- A thread's kernel stack is freed only by the CPU that ran it, after it has switched away, and an
  exit burst no longer panics the kernel.
- Two descriptors on one FAT file no longer leak or cross-link clusters, an unlinked FAT file keeps
  its clusters until its last close, and no close takes another process's reference.
- A case-only FAT rename no longer frees the file's clusters, and moving a directory into its own
  subtree fails with EINVAL.
- Vibefs no longer leaks blocks on every commit and remount, and a volume with deeply nested
  directories mounts again.
- A filesystem mounted below a directory no longer disappears, and a path no longer resolves to
  another directory's file, when the kernel's directory-entry cache fills.
- The block cache's flush waits for writes already in flight before it sends the device Flush, so a
  vibefs commit on vda cannot make its superblock durable ahead of its metadata.
- A boot no longer writes a GPT over an attached virtio-blk disk whose table it cannot parse, which
  destroyed whole-disk vibefs and FAT32 images.
- A device or keyboard interrupt that arrives while a user program runs no longer halts the kernel.
- An unmasked x87 floating-point exception in a user program now ends it with SIGFPE instead of
  being lost, and a machine check now dumps and halts instead of resetting the machine.
- A console read or a new process's first entry no longer returns to user mode with interrupts
  enabled, which could crash the kernel.
- The top user page is never mapped, and a bad user return address kills the process with SIGSEGV
  instead of halting the kernel.
- An ELF with a huge p_memsz no longer drains physical memory under the page-table lock; execve
  returns ENOMEM.
- A SIGCONT sent while a process is stopping is no longer lost.
- A long syscall or console write no longer holds off the timer tick and TLB shootdowns on its CPU.
- `fork`, `execve` and `open` return `ENOMEM` when the kernel heap runs out, and a failed `execve`
  returns to the calling program, instead of halting the kernel.
- A user program that sets RFLAGS.TF or executes int1 is killed with SIGTRAP instead of halting
  every CPU.
- `/bin/tests` no longer stalls after `user: dup ok`: a timer interrupt during a new process's first
  switch to user mode crashed the kernel without a message.
- An interrupt during a process's exit could crash the kernel, which briefly ran with a zero GS base
  (`gs::force_kernel`).
- A driver probe that fails now logs the driver, the device and the reason, and leaves the device
  unbound.
- An MBR primary partition listed after an extended partition is no longer lost.
- A FAT boot sector whose FAT size overflows the sector count fails the mount with an error instead
  of halting the kernel.
- Unmounting a disk volume while another thread still holds it no longer frees the volume under that
  thread.
- A virtio-blk or virtio-rng queue using EVENT_IDX could lose a kick or an interrupt and stall for
  good.
- A kernel marker or log line no longer splits when another CPU writes to serial at the same time.
- A failing `/bin/tests` now fails every e2e boot, and `/sbin/init` prints `init: /bin/tests exited
  <status>` on fd 2.
- Mounting a FAT disk from the kernel shell no longer overflows the 16 KiB kernel thread stack.
- Panic backtraces name the right functions, and two builds of one commit give identical ISOs (make
  repro).
- `make test-e2e-uefi` finds OVMF or Homebrew's edk2 firmware, boots it from pflash, and skips
  visibly (failing in CI) when none is installed.
- TSC calibration brackets each PIT window and HPET read with TSC reads, so a stalled vCPU no longer
  skews the TSC rate by up to 4%.
- Time no longer stands still for up to 43 s in long runs: the HPET counter is read as 32 bits in
  one access, since an 8-byte read could tear 2^32 ticks into the future.
- `now_ns` reads one clocksource chosen at boot (TSC, HPET or ACPI PM timer), so time no longer
  stalls when CPU 0 holds interrupts off or timer ticks coalesce.
- The clock's seqlock is a latch with Release fences around each bump, so no reader waits for the
  writer or pairs a new snapshot with an old sequence.
- `idt::set_handler` refuses exception, LAPIC and IPI vectors, and the build fails unless the vector
  table has one row per vector.
- A reaped process's pid is no longer given to the next fork; pids and thread ids count up to
  32,767, then wrap to 300.
- Threads exiting out of order no longer panic the kernel with "kva: free-list" once more than 128
  stacks are free.
- Tmpfs no longer loses written data when a growing file moves, and a busy FAT or vibefs volume
  makes callers wait instead of failing with EIO.
- `umount` of a busy filesystem returns EBUSY and leaves it usable, and `open(O_TRUNC)` with no free
  file slot no longer truncates.
- Each process resolves relative paths from its own working directory, and `..`, a trailing `/`, and
  `/VIBE` on FAT resolve as on Linux.
- FAT file times are stamped from the wall clock and read back as written for 1980 through 2107.
- The kernel shell's `rm -r`, `ls` and repeated `mount` no longer overflow the stack, stop at 16
  entries, fail on FAT or vibefs subdirectories, or leak dentries.
- Reading or writing a block device node such as `/dev/vdap1` works instead of failing.
- A second virtio-rng device is refused instead of orphaning the first device's queue and vector.
- A virtio probe that fails after its queues are set up resets the device and turns off bus
  mastering before it frees their memory.
- A full disk, a full open-file table, FAT's 4 GiB limit, on-disk corruption and lseek on the
  console return ENOSPC, ENFILE, EFBIG, EIO and ESPIPE, as on Linux.
- `execve` returns `EFAULT` for an unreadable `envp` instead of ignoring it.
- Execve loads programs larger than 64 KiB.
- No process can kill or stop init, and init's exit now panics the kernel with a line naming why.
- A static ELF whose code and data segments share a page loads as on Linux instead of losing the end
  of its code.
- /sbin/init checks every fork, execve and wait4 result and restarts /bin/sh instead of spinning on
  ECHILD; three failed starts end in the pid 1 panic (F128).
- Read, wait4 and psinfo no longer write into a process's read-only or executable pages; they return
  EFAULT, or the bytes copied before an unmapped page.
- An address space is a counted object; its memory is freed when its last user (thread or pin) lets
  go, never through a dangling reference.
- APs start from a trampoline page taken from the firmware memory map instead of bootloader memory
  at 0x8000.
- SIGKILL and SIGSTOP reach a process that makes no syscalls; a stray interrupt is counted and EOIed
  instead of halting.
- A panic now stops every other CPU before the dump, so the serial dump is no longer interleaved,
  cut short, or lost to an endless `reentered` loop.
- Exception backtraces start at the faulting function and follow only kernel stacks.
- A read-only virtio-blk disk stays readable, and one unreadable sector no longer takes the whole
  disk offline.
- FAT long names with non-ASCII characters round-trip, creates no longer grow directories, a failed
  extend frees its clusters, and a crafted boot sector no longer panics the kernel.
- A vibefs write past a file's fourth extent no longer leaks a block per attempt, and a write into a
  corrupt block returns an error instead of hiding it.
- A PCI BAR is mapped only by the driver that claims it, and a BAR that overlaps RAM or another
  device's BAR is refused.
- A lookup no longer fails with `ENOSPC` once cached names hold every VFS inode slot; the dentry
  cache is shrunk to free one.

### Removed

- The test harness retries nothing; a timed-out boot, a FAIL line or a panic fails its tier
  ([ROADMAP §10.2](docs/ROADMAP.md#102-build-and-harness)).
- The `vibeos-ktest.iso` release asset (F145).

## [0.9.0]

Phase 9 exit: user mode and processes. The 2026-09-23 kernel review reopened Phase 9's gate lines
into Phase 10, so `v0.9.0` is cut from the tree that closed them, one commit before `v0.10.0`, and
it carries the changes the 0.10.0 notes list.

### Added

- [Phase 9](docs/ROADMAP.md#phase-9-user-mode-and-processes): ring 3, syscalls, ELF loading,
  `fork`/`execve`/`wait4`, `/sbin/init`, `/bin/sh`. COW is Phase 12.

## [0.8.0]

Phase 8 exit: filesystems. Phases 0–8 in one cut. See [the arc](docs/ROADMAP.md#the-arc).
The 2026-09-23 kernel review and a later design review reopened gate lines of Phases 0 to 8 into
Phase 10, so `v0.8.0` is cut from the tree that closed them, and it carries the changes the
0.10.0 notes list.

### Added

- [Phase 0](docs/ROADMAP.md#phase-0-ignition): hybrid BIOS+UEFI ISO, COM1 contract, panic halt,
  Python e2e harness, CI ladder.
- [Phase 1](docs/ROADMAP.md#phase-1-memory): buddy PMM, own page tables (`paging: cr3 ok`), heap,
  KVA, in-guest ktests (`vibeos-ktest.iso`).
- [Phase 2](docs/ROADMAP.md#phase-2-traps-acpi-and-time): GDT/IDT/PIC, ACPI, PIT/HPET/TSC
  (`time: tsc`). `int3` returns; `#GP` dumps and halts.
- [Phase 3](docs/ROADMAP.md#phase-3-threads-and-scheduling): kernel threads, preemptive RR,
  `sleep_ms`, idle, wait queues and blocking primitives.
- [Phase 4](docs/ROADMAP.md#phase-4-smp): LAPIC/IOAPIC, AP bring-up, per-CPU run queues, IPI
  shootdown. `smp: done`. TCG is the guest-test default.
- [Phase 5](docs/ROADMAP.md#phase-5-console-input-and-logging): FB console, PS/2, log ring,
  kernel shell. Markers `console ok` then `shell ready`.
- [Phase 6](docs/ROADMAP.md#phase-6-device-model-and-buses): PCI, MSI-X, DMA, modern virtio,
  workqueue/threaded IRQ. `pci: N devices`. Shell `lspci` / `devices`.
- [Phase 7](docs/ROADMAP.md#phase-7-block-storage): block layer, ramdisk, virtio-blk, GPT/MBR
  children, write-back cache. `block: <name> <n> sectors`. Persist round-trip.
- [Phase 8](docs/ROADMAP.md#phase-8-filesystems): VFS, FAT32 initrd root (`ls`/`cat`/`mkdir`/`rm`/`cp`),
  `/dev` `/proc` `/tmp` `/sys`, vibefs with host `mkfs`/`fsck` crash test. Phase 8 exit.
- `make test-ps2` types `echo ps2-ok` through QEMU `sendkey` on the same i8042 as the window.

### Changed

- `vibeOS: pic: remapped` means the PIC boot step finished (ICW ran, or FADT skipped the ports).
- `-Z build-std` lives on the Makefile `CARGO` line so hostlib does not compile a second `core`.
- ktest FAIL lines match skip: `vibeOS: ktest: FAIL <name>: <why>`.
- Boot-done marker is `vibeOS: boot: phase1 done` (was `phase0 done`); later `shell ready`.
- CI cancels superseded GitHub Actions runs for the same branch or PR.

### Fixed

- QEMU window / PS/2 keyboard reaches the shell: 8042 config clears keyboard-clock-off, and
  IRQ1 is IOAPIC-only once the LAPIC owns the tick.
- `now_us` / `now_ns` stay monotonic under TCG `hlt` (no backwards step after a late tick).
- `sleep_ms_50` and `tsc_calib_source` hold under TCG `-smp 4` (wider band when invariant TSC
  is absent; ~50 ms of `now_us` when ticks coalesce).
- Recycled IRQ vectors no longer keep a dead threaded handler after virtio probe-fail.
- virtio-rng tears down MSI-X, the vector, and device status if the virtqueue fails after arming.
- Freeing an I/O APIC vector masks the GSI first so a still-asserted level line cannot storm.
- `lspci` / `devices` copy one device at a time and no longer overflow the 16 KiB shell stack.
- ECAM reads that miss the 64-page cache still use the mapped config window (later buses stay visible).
- Framebuffer `\r` homes the column so the shell prompt does not reprint on every key.
- PS/2 typematic no longer retoggles Caps/Num or re-enqueues modifiers.
- Kernel log emit stays IRQ-off; `dmesg` uses a plain serial path and does not recapture.
- Shootdown and call-function wait IRQ-off so two concurrent unmaps cannot deadlock.
- Heap grow/retry no longer OOMs or walks page tables while holding HEAP.
- First-run threads keep IF on; `switch_context` no longer `popfq`s into a tick.
- Dead stacks reap from idle and voluntary no-switch return, never from the IRQ path.
- Seqlock timekeeping never publishes a torn (tick, tsc) pair.
- `make test-e2e-pit` uses `-machine pc,hpet=off` (QEMU 10 rejects `-no-hpet`).
- `thread_exit` into idle no longer leaves `irq_nest` raised.
- TSC-deadline arm is LVT timer write, then `MFENCE`, then `IA32_TSC_DEADLINE`.
- Condvar/RwLock timeouts wake the right waiters; spawn reuses Dead TCBs without growing the heap.

[Unreleased]: https://github.com/devinreuschel/vibeOS/compare/v0.10.0...HEAD
[0.10.0]: https://github.com/devinreuschel/vibeOS/releases/tag/v0.10.0
[0.9.0]: https://github.com/devinreuschel/vibeOS/releases/tag/v0.9.0
[0.8.0]: https://github.com/devinreuschel/vibeOS/releases/tag/v0.8.0
