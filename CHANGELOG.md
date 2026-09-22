# Changelog

All notable changes to **vibeOS** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- UEFI e2e (`make test-e2e-uefi`) could hang the console-input second
  boot for the full 60s with no `shell ready` after a green marker boot.
  OVMF was falling through to PXE on the default e1000 (slirp DHCP, no
  TFTP) when COM1 was an open pipe. QEMU now boots CD first and disables
  OVMF PXE / firmware setup via fw_cfg; the harness retries a silent
  marker-boot or console-input hang once (BIOS and UEFI) and prints a
  serial tail on timeout.
- In-guest ktest retries a silent 90s timeout once (`user_syscalls`
  wait4 can stall `/bin/tests` after `user: dup ok` with ~101 lines),
  the known SMP4 `msix_cpu: ap counter` timing flake, and the SMP4
  persist-reboot `ipi: ack timeout` panic. Other in-guest assertions and
  panics remain hard failures.
- In-guest `user_syscalls` could hang `wait4` on a live fork-bomb herd
  under `make test-lapic-fallback` persist reboot (periodic LAPIC, 90s
  timeout, ~101 serial lines, no dump). `/bin/tests` now yields after
  each bomb fork so children become zombies before the next fork; the
  ktest runs that path with the tick enabled; a ktest timeout prints a
  serial tail.
- QEMU window / PS/2 keyboard input never reached the shell while COM1
  (`-serial stdio`) did. Two independent kills share that symptom: 8042
  init rewrote the controller config after `DISABLE_1` without clearing
  bit 4 (keyboard clock off), and `route_keyboard` could unmask PIC IRQ1
  after LAPIC already masked the 8259. Config writes now clear bit 4;
  IRQ1 is IOAPIC-only once the LAPIC owns the tick. Soft parks from #66
  (FB sizing, cursor/`%` glitch, unused `shell_init::ready`) are
  unchanged.
- `now_us` / `now_ns` no longer go backwards under `hlt` on TCG. A wrapping
  TSC-behind-snapshot delta interpolates as 0, and the kernel never publishes
  a reading below the last one. TCG has no invariant TSC; `hlt` plus the
  Phase 7C `blk-wb` sleeper makes ticks late relative to TSC, so interpolation
  overshoots and the next tick would otherwise step backwards.
- In-guest `sleep_ms_50` and `tsc_calib_source` no longer flake under TCG
  `-smp 4`. TCG leaves the invariant-TSC CPUID bit clear, so a 10 ms PIT
  sample after AP bring-up can disagree with the boot HPET rate, and LAPIC
  ticks coalesce so `uptime_ms` is a poor sleep ruler. The tests retry PIT
  against a fresh HPET window, widen the band to 50–200% when the bit is
  clear, and accept ~50 ms of `now_us` when ticks coalesce. Invariant TSC
  (KVM, real hardware) still requires 75–125% and 50–100 ms of ticks.

### Changed

- O1 superseded: Phase 9 exit closed 2026-09-22; no "Phase 9 resumed"
  note. README / DESIGN header / §1.3 now match the tree (DOC1).
- Pin the Rust nightly date, GitHub Action SHAs, and Limine commit so CI
  cannot go red from a floating toolchain. Cargo cache keys include
  `Cargo.lock` and `rust-toolchain.toml`. Weekly smp-stress runs a
  non-blocking latest-nightly canary.
- CI: cancel superseded GitHub Actions runs for the same branch or PR so
  only the latest tip stays in the queue.

### Added

- MIT license (`LICENSE`). Both `Cargo.toml` and `tests/hostlib/Cargo.toml`
  declare `license = "MIT"`.
- Tracked `AGENTS.md` (DOC3). Cursor rules and `CLAUDE.md` point at it.

- Phase 9 slice C: Process + fork/exec/wait + early signals + userspace
  shell. `Process` (pid, parent, AS, fd table, cwd, creds, exit status).
  Threads belong to a process. Real fd table replaces B's early fd1/fd2
  console sink (`dup`/`dup2`/`CLOEXEC`, refcounted kernel files).
  `fork` is a full AS copy (COW = Phase 10). `execve` builds the new AS
  first and replaces only after load. `wait4` + `WNOHANG`, zombies,
  reparent to init. Early signals: `SIGKILL`/`SIGSTOP` plus fault
  defaults (`SIGSEGV`/`SIGILL`/`SIGFPE`) and `SIGCHLD`; no user handlers.
  DESIGN §5.2 is CPL-split: user fault → kill + diagnostic, kernel
  still panics. `/sbin/init` is the post-init kernel job; `/bin/sh` is
  the interactive shell (`vibeOS: shell ready` unchanged); `/bin/tests`
  is the userspace EFAULT/fork/exec runner. Kernel shell only under
  `kernel_shell`. `ps` lists process state (`SYS_PSINFO=500` until
  procfs). In-guest: existing ring3 tests plus `user_syscalls`. No new
  boot marker. Phase 9 exit gate closed; Phase 10 (COW) not started.

- Phase 9 slice B: syscall ABI + ELF64 + freestanding userspace. Dispatch
  table plugs into Slice A's `vibeos_syscall_stub` (same entry, no second
  path). Wired: `write`, `exit`, `getpid`, `sched_yield`. Linux errno
  names. Early fd1/fd2 → console sink until Process (no half-Process).
  User pointers validated then copied via HHDM (`EFAULT`, not panic).
  Tracing behind `set_trace`; per-TCB `syscall_count` for procfs.
  ELF64 parse in the library (host tests: good, truncated, bad
  class/endian/machine, `PT_INTERP` refused). `/hello` on the initrd is
  a nasm stub that `write`s then `exit`s 42; kernel reports `user: exit
  42`. ABI: `docs/SYSCALL.md`. In-guest: `ring3_hello_exit`,
  `syscall_dispatch`, `syscall_ptr_validate`. No new boot marker.
  Phase 5–8 contract untouched.

- Phase 9 slice A: ring 3 plumbing + `AddressSpace`. `syscall`/`sysretq`
  (plus `iretq` when `sysret` cannot express), one `swapgs` policy
  (`vibeos_syscall_entry` first insn + `arch::gs::do_swapgs` for CPL=3
  IRQs/exceptions), TSS `RSP0` on every context switch, eager
  `fxsave`/`fxrstor` (target stays soft-float; no SSE feature flip).
  `AddressSpace` shares the kernel PML4 half, tracks user regions,
  skips CR3 when the next thread has the same root, tears down user
  frames vs a frame count, and rejects kernel/unmapped/overflow user
  pointers with `EFAULT` rather than a panic. Null page stays unmapped.
  Entry stub returns `ENOSYS` for any number; no dispatch table.
  In-guest: `star_sysret_layout`, `addrspace_map_unmap_teardown`,
  `user_ptr_helpers`, `cr3_switch_skip`, `ring3_syscall_enosys`.
  No new boot marker.
- #66 PS/2 regressions: host `cfg_run_clears_clock1_left_by_disable`;
  in-guest `kbd_gsi_unmasked`, `kbd_8042_clock`, and `kbd_ps2_irq` (8042
  `0xD2` injects set-1 `0x1E`, expects `a` on the PS/2 ring — serial
  cannot satisfy it). `make test-e2e` / `make test-ps2` type `echo
  serial-ok` on COM1 then `echo ps2-ok` via QEMU `sendkey` (same i8042 as
  the window). `0xD2` does not cover the device clock; that is
  `kbd_8042_clock` plus `sendkey`.
- Phase 8 slice D: vibefs (format version 1). CoW metadata + dual
  superblocks + generation + CRC-32; not a write-ahead journal
  (`docs/VIBEFS.md`). Extents, B-tree directories, metadata and data
  checksums, 128-byte inline files, snapshots as pinned roots. Host
  `mkfs-vibefs` / `fsck-vibefs` share `src/vibefs.rs` with the kernel.
  BSS volume at `/vibe`; optional `mount vibefs <dev> <path>`. Host
  tests cover synthetic and corrupt images plus a CrashDisk that drops
  writes mid-commit. `make test-vibefs-crash` mkfs's a virtio-blk
  image, boots a write+fsync loop, SIGKILLs QEMU, and requires
  `fsck-vibefs` clean. No new boot marker. Phase 8 exit.

- Phase 8 slice B: FAT32 read/write, kernel File API, shell file
  commands, Makefile initrd. BPB validate, FAT chain cache, 8.3 + LFN
  checksum, reads across clusters, readdir/stat/timestamps. Write
  allocates from a free-cluster hint; create/write/truncate/delete;
  mkdir/rmdir with LFN generation. Both FAT copies and FSInfo stay in
  sync. Flush order never leaves a dirent pointing at free clusters.
  `sync` issues block `Flush`, not only Barrier (DESIGN §10.2). The
  VFS lock is dropped before blocking block I/O (FAT volume stays in BSS
  behind a busy flag). FAT `symlink`/`link` return `FsError::NotSupp`
  (no POSIX perms/links on FAT). Shell: `ls -l`, `cat`, `cp`, `mv`,
  `rm -r`, `mkdir -p`, `touch`, `stat`, `df`, `mount`, `umount`,
  `sync`, plus `cd`/`pwd`; tab completes the current directory.
  Initrd is a Makefile-built FAT32 image mounted as root. Host tests
  cover generated and corrupt images; write/unmount is `fsck.fat`
  clean. No new boot marker.
- Phase 8 slice C: pseudo filesystems on a shared kernfs directory
  tree (one node table, four skins — not four dentry implementations).
  `devfs` publishes `null`, `zero`, `random`/`urandom`, `console`, `tty`,
  and block names matching `block: <name>` (`ram0`, `vda`, partitions).
  `tmpfs` stores file data in the Phase 7 page/block cache plus a fixed
  ramdisk so clock eviction works; it is not a grow-only `Vec`.
  `procfs` exposes `self` and a pid-1 stub (`cmdline`/`status`/`maps`/`fd`)
  that does not panic when only kernel threads exist (Process is Phase 9).
  sysfs-equivalent walks the Phase 6 device tree and driver bindings.
  `/dev` `/proc` `/tmp` `/sys` are mounted after FAT initrd (or ramfs
  fallback) root. No new boot marker; `/dev/random` is a non-blocking
  xorshift (not virtio-rng, not IRQ). Host tests cover kernfs/devfs/tmpfs
  eviction; in-guest `pseudo_fs`.

- Phase 8 slice A: VFS. Inode (type, size, mode, times, nlink), dentry
  cache with negative entries (invalidated on create in that dir),
  superblock + mount table, mount-point crossing (`..` from a mount
  root walks to the parent of the covered dentry). `FileSystem` /
  `InodeOps` (lookup/create/unlink/read/write/truncate/readdir/stat).
  Iterative path walk with a symlink-depth cap; a loop is `FsError::Loop`,
  not a stack smash. `File` (offset, flags) and `FdTable` for Phase 9.
  Refcount: unlinked-but-open data lives until last close. Inode and
  dentry caches are bounded with clock eviction. Dummy ramfs is enough
  to host-test walks; the kernel mounts it at `/` with no new boot
  marker. VFS lock is RANK_DEVICE (DESIGN §2.1; a numbered slot is a
  Design ACK). Host tests: `.`/`..`, symlink bound/loop, negative
  dentry, mount crossing, unlinked-open, eviction.

- Phase 7 slice C: GPT/MBR partition children and a write-back block cache.
  MBR walks primary plus extended/logical (depth-bounded; corrupt next-LBA
  stops). GPT checks header and entry CRC and falls back to the backup
  header; a protective 0xEE MBR is not treated as the disk. Children are
  offset-limited `BlockDevice`s (`block: <parent>p<N> <n> sectors`). The
  cache is 4 KiB pages keyed by `(device, offset)`: read-through,
  write-back, clock eviction, sequential readahead, dirty-ratio writeback
  thread. `flush` writes dirty pages then calls the device; `barrier`
  writes dirty only. Hit/miss/device-request counters are on `blk`. Built
  so Phase 10 can reuse the same pages. Host tests: real table blobs,
  truncated/bad CRC, eviction, and a measured device-request drop on
  repeat reads. In-guest: MBR+GPT children, cache hit vs uncached
  counters, eviction. Persist LBA moved inside the Linux GPT partition
  so it does not sit on the backup header. Panic dump tail is 24 records
  so `smp: done` still appears after the extra partition markers. ktest
  boots the ISO first (`-boot order=d`) so a protective MBR on vda does
  not steal SeaBIOS from the CD on reboot.

- Phase 7 slice B: virtio-blk on the Phase 6 modern transport. Probe reads
  capacity, `blk_size` (512 if `F_BLK_SIZE` is missing), and topology.
  Requests are descriptor chains (hdr + data + DMA status byte); kick uses
  the existing `dma_wmb` / notify-cap formula. Completions harvest the used
  ring on the threaded IRQ into `IoWaiter` cookies — the ramdisk sync pump
  is not used for this device. `F_MQ` gets one virtqueue per CPU (single
  queue if the feature is absent). Flush and discard are issued when the
  device offers them. Marker `vibeOS: block: vda <n> sectors` (ktest adds
  `virtio-blk-pci,disable-legacy=on` + a raw `-drive`; e2e stays
  `pci: 6 devices`). Host tests pack the virtio-blk header/discard against
  spec constants. In-guest: sector roundtrip, unaligned multi-sector, deep
  queue, concurrent submitters, IRQ completions, and write → reboot → read
  back intact (`vibeOS: persist: wrote` / `intact`).

- Phase 7 slice A: block layer + ramdisk. `BlockDevice` (logical block
  size, capacity in those blocks, read/write/flush/discard) with a
  per-device request queue: adjacent merge, C-LOOK elevator, barrier vs
  flush (DESIGN §10). Completions are waiter cookies; the queue lock is
  not held across the copy so a later virtio-blk threaded IRQ can signal
  the same path. I/O errors retry a bounded number of times, then the
  device is `Failed`. Ramdisk `ram0` (256 × 512 B) is registered at boot
  with BSS backing (no heap alloc under RANK_DEVICE); discard is a
  range-checked no-op. Marker
  `vibeOS: block: <name> <n> sectors` after `pci: N devices`, before
  `shell ready`. Shell `blk`. Host tests cover merge, elevator, fences,
  retry, and 4K geometry. In-guest: ramdisk R/W, concurrent threads,
  retry-to-failed.

- Phase 6 slice C: modern virtio PCI transport, workqueue / threaded IRQ,
  and the Phase 6 exit gate. Vendor caps locate common, notify, ISR, and
  device-specific regions. `VIRTIO_F_VERSION_1` is required (probe fails
  cleanly without it). Split virtqueue kick/complete uses `dma_wmb` /
  `dma_rmb` around avail/used index publish, not a blanket
  `compiler_fence`. Notify doorbell is
  `cap.offset + queue_notify_off * notify_off_multiplier`. Indirect
  descriptors and `VIRTIO_F_EVENT_IDX` are negotiated when the device
  offers them. Packed VQ is deferred. Ring index wrap, `need_event`, and
  a simulated device live in the library half. virtio-rng binds by id
  (`1af4:1044` / `1004`), not probe order, and exercises one VQ.
  Workqueue workers consume `fn(usize)` items; the high-prio ring is the
  softirq stand-in (IRQ enqueues, does not block). Threaded IRQ: top half
  acks/wakes only; the bottom-half thread may `Box` and block. Blocking
  rules are in DESIGN §2.2. In-guest: virtio-rng (ktest adds
  `virtio-rng-pci,disable-legacy=on`; e2e stays `pci: 6 devices`),
  workqueue, threaded IRQ + softirq from the MSI-X top half. MSI-X still
  comes from the `0x30..=0x7F` pool.

- Phase 6 slice B: MSI/MSI-X and DMA. Device IRQs allocate from the
  `0x30..=0x7F` pool (`0x30` stays keyboard) bound to a chosen CPU.
  Drivers call `irq::allocate_vector` + `set_handler`; they do not pick
  IDT slots. Allocate is refused in a hard-IRQ (dispatcher `IN_ISR`,
  not `InterruptGuard` nest). MSI message address is
  `0xFEE00000 | (apic_id << 12)`; MSI-X table entries live in a BAR.
  COMMAND.INTX# is set when MSI/MSI-X is armed. INTx fallback routes
  the GSI through the I/O APIC (level, active low). `set_affinity`
  records dest CPU and rewrites IOAPIC routes; MSI callers reprogram
  the message. In-guest: e1000e MSI-X and edu INTx arrive on a chosen
  AP (ktest adds those devices; e2e stays `pci: 6 devices`).
  `DmaBuffer` is physically contiguous from the buddy with alignment
  and boundary (including 4 GiB / DMA32). Device address is
  `dma_to_device(phys)` (identity; never HHDM VA).
  `sync_for_device` / `sync_for_cpu` always run (`sfence`/`lfence` on
  x86). SG lists and Release+sfence descriptor publish. IOMMU later.

### Fixed

- `irq::free_vector` zeros threaded `top`/`work`/`pending` with the
  handler and route. Recycled vectors were still taking the threaded
  path, so a later `set_handler` was ignored (virtio probe-fail recycle).
- virtio-rng probe tears down MSI-X, the vector, and device status when
  `SplitLayout` or the virtqueue `DmaBuffer` fails after MSI-X is armed.
  Those paths used `?` and skipped the teardown used on later failures.
- `irq::free_vector` masks an I/O APIC GSI before clearing the handler
  and dropping `Route::IoApic`. A still-asserted level line no longer
  storms empty `dispatch`, and a later allocate of the same vector
  cannot inherit the old device's IRQs. MSI/MSI-X free is unchanged.
- Shell `lspci` / `devices` copy one `Device` at a time. A full
  `[Device; 64]` snapshot (and a second `Registry` on `devices`) overflowed
  the 16 KiB shell stack into the guard. RANK_DEVICE is still dropped
  before FB print.
- ECAM cfg reads/writes use the VA from the page map, including when the
  64-page cache is full (last slot is replaced). Cache-only lookup had
  returned `0xFFFF_FFFF` so later buses vanished.

### Added

- Phase 6 slice A: device model + PCI/PCIe enumeration. `Device` / `Driver`
  trait (probe/remove + id table) and a registry that matches by id and
  probes in dependency order, with exclusive BAR/resource claims. Scan
  fills the device list, then bind — not inline probe from the walk.
  Legacy config via `0xCF8`/`0xCFC` on bus 0; MCFG → ECAM beyond.
  Recursive bridge enum, BAR decode (mem/IO, 32/64, all-1s size probe),
  capability offsets recorded (MSI/MSI-X/PCIe/PM; not enabled). Memory
  BARs map through ioremap or the capped physmap; sizes above 32 MiB are
  skipped (DESIGN §4.1). VGA BAR0 is not UC-patched over the console FB;
  the BAR is mapped through the existing WB physmap alias even when it
  outruns Limine's visible surface / `map_end`.
  Memory Space + Bus Master on bind. Marker `vibeOS: pci: <n> devices`
  after `console ok`, before `shell ready`. Shell `lspci` / `devices`.
  Host tests: CF8/ECAM encoding, config R/W, BAR size (including huge-BAR
  refuse), cap walk, bridge recurse. In-guest: QEMU `pc` id set, VGA BAR
  map, config R/W, exclusive claim, bind, `lspci`. Harness asserts the
  new marker in this change set.

- Phase 5 slice C: kernel shell thread, command registry, and
  `vibeOS: shell ready` as the last boot marker (`smp: done` →
  `console ok` → `shell ready`). Line editor (echo, backspace, ctrl+C,
  ctrl+U, cursor, history) and quote/whitespace tokenizer in the library
  half with host tests. Built-ins register into a table rather than a
  `match`: `help`, `echo`, `meminfo`, `uptime`, `cpus`, `dmesg`, `ps`,
  `panic`, `reboot`, `poweroff`. `dmesg` dumps the log ring (pre-FB boot
  lines included) with a level filter, `-n` to change the runtime max,
  and `-f` follow. `panic` exercises the §5.6 dump. `reboot`/`poweroff`
  use FADT reset/S5 when present, then the 8042 pulse / QEMU ports; they
  do not busy-loop. Input drain is IRQ-off for both the PS/2 ring and
  serial RX (DESIGN §9.4). TCG remains the guest-test default
  (`VIBEOS_QEMU_ACCEL=tcg` or `VIBEOS_QEMU_EXTRA="-accel tcg"`).
  `boot: phase1 done` is retired; the constant remains as a spelling
  alias. Double buffering and FB write-back/WC physmap stay parked.

- Phase 5 slice B: framebuffer text console, PS/2 keyboard, and console
  mux. BGRX pixels at `base + y * pitch + x * 4` with release bounds
  checks (Limine pitch, not `width*4`). 8×8 font, LSB leftmost, ASCII
  32–126 plus a replacement glyph. Text grid wrap + `memmove` scroll
  with a banner row that stays put. 8042 init (self-test, port 1);
  scan-code set 1 including `0xE0`; shift/ctrl/alt/caps/num. ISR writes
  a fixed ring only (no alloc, no log); consumer drains IRQ-off.
  IRQ1 / keyboard GSI stays masked until the handler is installed, then
  the controller is initialized, then unmasked (IOAPIC vector `0x30`).
  Mux fans writes to serial + FB and merges PS/2 with polled serial RX;
  backends do not call `log!`. Pre-FB boot lines are replayed onto the
  FB when the mux comes up. Marker `vibeOS: console ok` after
  `smp: done` / `boot: phase1 done`. Host tests: font, pitch/bounds,
  scancodes, ring wrap. In-guest: BGRX round-trip, pitch, GSI unmask,
  mux enable/disable, ring drain. Double buffering parked. DESIGN §3.3
  table now matches live SMP-then-console order.

  `error|warn|info|debug|trace` with a compile-time ceiling and an
  `AtomicU8` runtime filter. Fixed ring (wrap drops oldest) stores
  `{timestamp, cpu_id, level, msg}`; serial output is captured so boot
  lines exist in the ring before a framebuffer. Host tests cover wrap,
  filter, and overflow. ISR path: no alloc, no SCHED, serial TX is
  IRQ-off or try-lock + drop. Printer thread parked (Design ACK): global
  IRQ-safe ring + serial sink. Panic order is halt IPI `0xFE` (Fixed,
  not NMI), re-init serial, dump regs/thread/last N log records,
  symbolized FP backtrace, then `hlt` or `panic_exit` isa-debug-exit.
  Bounded THRE poll still drops the byte. E2E panic scanner waits for
  `vibeOS: panic: halted` and checks dump needles; still matches `#PF`
  `#GP` `#UD` `#DF` `panicked at`, not English "page fault". No new boot
  markers; `smp: done` stays where Phase 4 put it.

- Phase 4 slice C: per-CPU scheduling, IPIs, TLB shootdown, CI matrix.
  Global TCB table with per-CPU ready queues; `CpuAffinity::{Any, Pinned}`
  (`Any` round-robins). Cross-CPU wake is the target inbox plus reschedule
  IPI `0xFD` (never a remote runq lock). Each CPU has its own idle
  (`sti; hlt` with a closed lost-wakeup window). IPI handlers are
  allocation-free. Shootdown (`0xFC`) and call-function (`0xFB`) take
  neither PT nor SCHED; reschedule (`0xFD`) runs `schedule_preempt`
  IRQ-off; panic halt is Fixed IPI `0xFE` (not NMI). Shootdown and
  call-function waits are IRQ-off and poll inbound slots so two
  concurrent shootdowns cannot deadlock; KVA is freed to the free-list tail
  only after ack. ISR-taken locks stay IRQ-off; rank order is page tables →
  buddy → heap → scheduler → device → serial (cheap held-mask tracker).
  Serial TX is byte-locked; panic broadcasts halt first and skips that lock.
  Markers: `sched: cpu<i> ready` on each AP before `smp: ap online`.
  Diagnostic `cpus` line. In-guest: cross-CPU spawn, reschedule IPI wakes
  an idle AP, remote unmap/fault/remap, allocator hammer from every CPU.
  `make test` includes `-smp 4`; `make test-smp-stress` is weekly CI, not
  every push. TCG remains the guest-test default.

- Phase 4 slice B: AP trampoline, INIT/SIPI bring-up, and PerCpu expansion.
  After `irq: enabled`, the BSP copies `trampoline.asm` (`nasm -f bin` via
  `build.rs` / `CARGO_MANIFEST_DIR`) to identity-mapped `0x8000`, starts
  each enabled MADT CPU one at a time (INIT, 10 ms, SIPI, ~1 ms, SIPI),
  and waits on a ready flag (3 s timeout; timeout frees the stack, IST,
  GDT/TSS, and idle TCB). APs load a per-CPU GDT/TSS, set
  `GS_BASE`/`KERNEL_GS_BASE` before `lidt` and `sti`, enable the LAPIC, arm the
  same timer mode as the BSP, then idle. Markers: `N-1` ×
  `vibeOS: smp: ap online`, then `vibeOS: smp: done`, still before
  `boot: phase1 done`. `PerCpu` is a heap array sized from the MADT CPU
  count (`self_ptr` at 0, wake-inbox stub, timer mode, syscall scratch).
  In-guest: identity on BSP and AP, trampoline `cli` at `0x8000`, failed
  AP path restores the frame count. Host tests cover trampoline offsets
  and SIPI vector arithmetic. The trampoline clears `CR0.CD`/`CR0.NW`
  (INIT leaves caches off) and `WBINVD`s so APs are not uncached vs the
  BSP. AP timers tick locally and do not take the UP run queue (Slice C).
- Phase 4 slice A: BSP LAPIC, I/O APIC, and LAPIC timer. After `time: tsc`,
  boot enables the local APIC (`IA32_APIC_BASE` bit 11, MADT type-5 base),
  programs every I/O APIC with ISOs (high dword before low, still masked),
  then proves a tick and prints `vibeOS: time: lapic_timer ok (<mode>)`.
  Preference is TSC-deadline, then periodic (HPET, divider 16), then PIT.
  PIC and the PIT GSI are masked only after that proof (no double delivery).
  Spurious `0xFF` does not EOI. `send_ipi` polls delivery-pending with a
  cap of 1000. Host tests cover the poll (clear + timeout), redir write
  order, and ISO IRQ0→GSI. In-guest: mode matches CPUID (no silent
  downgrade), rearm across many ticks, PIT GSI masked when LAPIC owns
  the tick. `make test-lapic-fallback` (`-cpu qemu64,-tsc-deadline`) is
  in `make test` and CI. TCG cannot advertise TSC-deadline, so default
  e2e pins `periodic`; `make test-e2e-pit` pins `pit`. PIT fallback
  programs LINT0 as ExtINT so the 8259 virtual-wire still delivers IRQ0
  after LAPIC enable (a masked LINT0 swallowed the tick).
- Phase 3 slice C: `WaitQueue` plus `BlockingMutex`, `RwLock`, `Semaphore`,
  `Condvar`, and bounded MPSC `Channel<T>`. Every wait takes an optional
  deadline (`None` → far-future sentinel). Enqueue → Blocked → drop SCHED →
  schedule; wake under the same lock (DESIGN §9.4). In-guest: 1000-iter
  two-thread mutex counter, rwlock/sema/condvar/channel, mutex deadline,
  hold SCHED (ticks freeze under IF=0) then force timer IRQ after drop,
  spawn/exit 2000 with frame count back to baseline. Host: wait-queue
  FIFO, lost-wakeup protocol, timeout unlink, primitive state machines.
- Harness / `make test` default to `-accel tcg` (`VIBEOS_QEMU_ACCEL`
  overrides) so KVM timing does not flake the PIT/sleep tests.

### Fixed

- Framebuffer text: `\r` homes the column on the same row instead of
  being dropped before the text grid. The shell line editor paints
  in place with CR; serial already homed, the QEMU FB was reprinting
  the prompt on every key. Host tests cover CR overwrite and CRLF.
  In-guest: `fb_cr_home` overwrites a glyph cell after CR.

- PS/2 decoder: typematic repeats no longer retoggle Caps/Num or
  re-enqueue modifiers. Down-bits; only the first make is an edge.
  Host tests cover Caps, Num, and shift. Letters still repeat.
- Kernel log: `klog!` and serial formatted writes keep IF off for the
  whole emit so per-CPU capture/`EMITTING` cannot race a preempting
  thread. `dmesg` uses a plain serial path and does not recapture into
  the ring.
- Shootdown and call-function publish→wait→clear run under
  `InterruptGuard` so a tick cannot reuse `SHOOT[me]` or the global CALL
  slot mid-ack. `wait_acks` panics if IF is on.
- Heap `alloc`/`realloc` retry grow (capped) after a concurrent CPU
  consumes the newly extended window, instead of one refill and OOM.
- Failed guarded-stack unwind shootdowns before returning frames.
- Heap grow walked `translate` (PT, rank 1) while holding HEAP (rank 3).
  Snapshot mapped/cap, walk PT with HEAP dropped, then `extend` under HEAP.
- `spawn_here` copies `irq_nest`. `switch_to` into nest 0 would `sti` the
  worker; a tick then preempted the cooperative `switch_two_threads` chain
  (flake at `-smp 4`).
- TSC-deadline arm: `MFENCE` after the LVT timer write so `IA32_TSC_DEADLINE`
  cannot retire against the old masked one-shot (SDM Vol. 3A). Host test
  asserts LVT → MFENCE → deadline. DESIGN §3.3 lists step 13b
  (`time: lapic_timer ok`).
- `switch_context` no longer `popfq`s with IF set before `jmp`. A timer
  in that window preempted a first-run thread, overwrote the trampoline
  frame, and `iret` jumped into `schedule_inner` on `stack_top-8` (`#PF`
  in `spawn_exit_thousands`). Incoming IF uses delayed `sti` before the
  `jmp`.
- `spawn` rewrites a `Dead` TCB in place instead of `Box::new` + drop,
  so spawn/exit stress does not grow the heap by a page.
- `Condvar::wait` unlocks the mutex under the same SCHED as `begin_wait`,
  so a timer cannot park a waiter that still owns the mutex.
- `RwLock::write_until` timeout wakes `read_wq` when no writer remains
  (writer preference otherwise stranded those readers).
- Dead-stack reap no longer waits for the outgoing `schedule` call to
  resume. Idle's timer resume is `from_irq` (skips reap) and idle
  `yield_now` often takes the no-switch return; both leaked the last
  worker stack until `defer_free` panicked. Drain on the voluntary
  no-switch return and in the idle loop (never on the IRQ path, never
  the stack we are on). `thread_exit` holds `InterruptGuard` across
  Dead → `defer_free` → `schedule` so a tick cannot preempt a Dead
  thread still on-CPU. In-guest `reap_many_via_idle` spawn/exits 16
  twice (parked + running) and checks the frame count.
- Seqlock `TickClock::write` odd-bumps with `fetch_add(AcqRel)` and
  stores (tick, tsc) as atomics, then Release-publishes the even
  sequence. Relaxed load/store on the odd bump let a torn pair stay
  visible while seq still looked even. The threaded tear test also
  seeds a consistent pair so the default `(0, 0)` is not counted as a
  tear (`seqlock_threaded_writer_never_tears`).
- `make test-e2e-pit` disables HPET with `-machine pc,hpet=off`. QEMU
  10.x rejects `-no-hpet`; 8.x only deprecates it.
- `switch_to` swaps `PerCpu.irq_nest` with the outgoing/incoming TCB.
  `InterruptGuard` lives on the outgoing stack, so a one-way exit
  (`thread_exit` → idle) never dropped that guard and left the CPU
  nest permanently raised. In-guest spawn/switch tests assert nest is
  unchanged.

### Added

- Phase 3 slice B: preemptive RR scheduler, `sleep_ms`, idle thread.
  After `time: tsc`, boot prints `vibeOS: sched: cpu0 ready` then
  `vibeOS: irq: enabled`. IRQ0 stays the timekeeping tick from slice C;
  the PIT handler EOIs, then `on_timer_tick` (no-op until idle exists).
  IRQ1 stays masked until the keyboard driver. Two spinning threads
  interleave without `yield_now`; `sleep_ms(50)` returns 50–100 ms
  in-guest; idle `sti; hlt`s when the ready FIFO is empty.
- Ready FIFO (idle stays off it) and one sorted timeout list under the
  IRQ-aware scheduler lock. `schedule` / `yield_now` for the voluntary
  path; preempt every 10 ticks, or every tick while idle so a sleeper
  can displace halt. Blocking APIs take `Option<deadline>` (`None` → a
  far-future sentinel). Dead stacks are reaped off the dying stack
  (trampoline / non-IRQ `schedule`); TCB slots stay `Dead` until spawn
  reuses them. Per-thread `run_tsc` and per-CPU
  idle-time accounting. `PerCpu.idle` is a real idle thread, shaped for
  one per CPU in phase 4.
- `prepare_thread` still seeds `rflags=0x2`. `schedule` applies
  `apply_if_on_resume`: IF on when `irq_nest == 0`, so first-run and
  timer-preempted threads are not tick-deaf. `irq_nest` still swaps
  with the TCB across `switch_context`.
- Host tests: ready FIFO RR, timeout ordering, sleep/block wake state
  machine. In-guest: yield, sleep, preemption, idle, reap frame count.

- Phase 3 slice A: BSP per-CPU area, kernel threads, context switch,
  `InterruptGuard` / `SpinMutex`. After `idt ok`, boot prints
  `vibeOS: per_cpu: bsp ready` (DESIGN §3.3 step 11) then the ACPI
  `xsdt` marker. Does not emit `sched: cpu0 ready` or `irq: enabled`.
- BSP `PerCpu` at `GS_BASE` / `KERNEL_GS_BASE`, `self_ptr` at offset 0,
  `current`/`idle` as `*mut Tcb`, `idle_id`, `ready_head` (null until
  Slice B; UP TCB table is the queue), `irq_nest` for `InterruptGuard`,
  switch scratch. Idle is bootstrap until Slice B's `sti; hlt`
  citizen. `per_cpu!` field access. After TSC calibration, `tsc_per_ms`
  is copied onto the BSP area (the slot phase 4 owns). Boot order is
  allocate → `wrmsr` both GS bases → bootstrap current/idle → marker;
  no switch before that.
- `ThreadId` / `Tcb` / `spawn(name, fn)`: guarded 16 KiB KVA stacks,
  states ready/running/sleeping(deadline)/blocked/dead, global TCB
  table with run-queue link fields for phase 4. New threads start on a
  synthetic frame into a trampoline; returning marks dead, parks the
  stack on the KVA deferred list (never unmaps the stack it is on), and
  hits a `schedule` stub that switches to `PerCpu.idle` (bootstrap).
  Slice B owns the ready queue and drain.
- `switch_context` in `global_asm!`: callee-saved GPRs, rflags, rsp,
  return address. No XMM (soft-float). Host unit test switches two
  stacks; in-guest tests spawn a sentinel and ping-pong two threads
  via `switch_to`.
- `InterruptGuard` nested IF save/cli/restore plus per-CPU nest.
  `SpinMutex<T>` is IRQ-aware CAS with `assert!` on recursive lock
  and wrong-owner unlock. Host tests cover the CAS core; in-guest
  covers nest and a mutex store.

- Phase 2 slice C: PIT bootstrap tick, TSC calibration, seqlock
  timekeeping, RTC wall-clock offset. After `acpi: xsdt`, boot prints
  `vibeOS: time: calibrated hpet|pit <n>/ms` then the exit-gate
  `vibeOS: time: tsc <n>/ms`. IRQ0 is unmasked and `sti` runs after
  calibration (keyboard stays masked; `irq: enabled` is still Phase 3).
  `uptime` reports tick milliseconds next to TSC microseconds.
  If FADT bit 0 skipped the boot PIC remap, the timer path still
  programs the 8259 so IRQ0 is vector `0x20` rather than `#DF`.
- PIT channel 0 mode 2, divisor 1193 (~1 kHz), `io_wait` between
  divisor bytes. IRQ0 handler increments the tick, snapshots TSC, EOI,
  returns — no alloc, no logging. Channel 2 one-shot via port `0x61` is
  the HPET-less calibration path (count 11932, ~10 ms).
- TSC: invariant-TSC CPUID check (loud log if absent), `lfence`/`rdtscp`,
  HPET main counter over ~10 ms when the ACPI table is usable, PIT
  channel 2 fallback. `tsc_per_ms` lives on the BSP `TimeState` (the
  per-CPU slot Phase 4 will own). Poison frequencies are refused.
  `busy_wait_ms` spins on TSC and `hlt`s when IF is set.
- `vibeos::time` in the library half: interpolation including near
  `u64::MAX`, seqlock retry under a simulated concurrent writer with an
  independent published timestamp, `next_deadline`. Host tests cover
  those plus PIT/HPET calib math and wall-clock offset.
- In-guest: PIT ~1 kHz, `now_us` monotonic over 10k reads (straight-line
  and under `hlt` yields), HPET vs PIT-ch2 agreement, uptime sides, RTC
  offset. `make test-e2e-pit` boots the production ISO with HPET off.

- Phase 2 slice A: GDT/TSS/IST, IDT/exceptions, 8259 PIC. Boot prints
  `vibeOS: gdt ok`, `vibeOS: pic: remapped`, `vibeOS: idt ok` after
  `kva: ready` (IST stacks come from KVA guarded stacks; relative order
  matches DESIGN §3.3), then the ACPI `xsdt` marker from slice B. PIC
  reads FADT `iapc_boot_arch` bit 0 and skips the ICW sequence when the
  legacy 8259 is absent; missing FADT still remaps+masks. `pic: remapped`
  is emitted after that step either way.
- Flat GDT with sysret selector order (null, kernel code/data, user
  data, user code, TSS). Per-CPU `CpuTables` (GDT+TSS) with RSP0 and
  IST1–4 (DF/NMI/MC/debug). 256-entry IDT, `x86-interrupt` handlers;
  `#BP` logs and returns; `#UD`/`#GP`/`#PF`/`#DF`/`#MC` dump RIP/CS/
  RFLAGS/RSP/SS/error/CR2 and halt. Scoped catcher for tests (longjmp
  or step RIP). Named vector constants with a host uniqueness test.
- 8259 remap to 0x20/0x28 with `io_wait`, mask-all, `mask`/`unmask`/
  `disable_all`, spurious IRQ7/15 without a bogus EOI.
- In-guest: `int3` roundtrip, scoped `#PF` skip, `#GP` catch, DF-on-IST
  via a poisoned RSP. `make test-e2e-gp` boots a `gp-test` kernel that
  dumps `#GP` and halts.
- Host tests for GDT/TSS/IDT packing, sysret selector arithmetic, PIC
  ICW plan, FADT skip policy, spurious EOI policy, and vector uniqueness.
- Phase 2 slice B: ACPI discovery. `vibeos::acpi` in the library half
  validates RSDP (signature, v1 20-byte checksum, v2 extended checksum),
  walks XSDT with per-table checksums (RSDT fallback), and parses MADT
  (LAPIC base, type 5 override, I/O APIC+GSI, type 2 ISOs, enabled APIC
  IDs), HPET (rejecting zero addresses and I/O-space GAS), FADT
  (`iapc_boot_arch` bit 0, reset/sleep GAS), and MCFG (ECAM base stored
  for phase 6; ECAM is not walked). Packed fields go through
  `read_unaligned_*`. Host tests cover RSDP checksum rejection, HPET
  validation, and MADT iteration over truncated / zero-length input.
- Kernel `acpi_init` reads Limine's RSDP (physical at base revision 3),
  maps any ACPI-table pages that sit outside `map_end`, UC-patches
  discovered LAPIC / I/O APIC / HPET physmap leaves via
  `patch_physmap_uc` (mapping missing 4 KiB leaves first — those bases
  sit above a 128 MiB physmap), then reads the HPET GEN_CAP period.
  Emits `vibeOS: paging: mmio uc` only when a real leaf was patched, and
  `vibeOS: acpi: xsdt <n> tables` after GDT/PIC/IDT, plus a summary of
  CPU count, I/O APIC count, and HPET presence. In-guest `acpi_discovery`
  checks table counts against QEMU and that LAPIC/IOAPIC/HPET physmap
  leaves are UC.

- Phase 1 slice C: kernel heap, KVA allocator, in-guest tests, meminfo.
  Free-list heap at `HEAP_START` (1 MiB initial, grows in 4 KiB steps to
  the 64 MiB cap), `GlobalAlloc` with interrupts off, and
  `#[alloc_error_handler]` panicking with the failed `Layout`. KVA is a
  first-fit range allocator over the 64 GiB window; `alloc_guarded_stack`
  maps order-0 frames above an unmapped guard, `vmap` presents
  non-contiguous frames contiguously, and a deferred-free list has an
  explicit drain. Boot prints `vibeOS: heap ok` then `vibeOS: kva: ready`,
  then a shell-less meminfo dump (PMM totals, heap used/capacity, KVA
  used) plus a coalesced page-table range walk.
- `kernel_tests` feature builds a second kernel into `target-kernel-tests`
  / `vibeos-ktest.iso`. After init it runs a registry over serial
  (`ktest: begin` / `ok` / `FAIL` / `skip` / `end`) and exits QEMU via
  `isa-debug-exit` at `0xf4` (`0x10` pass, `0x11` fail).
  `make test-kernel` drives `tests/kernel_boot.py`. In-guest coverage:
  map/unmap, NX instruction-fetch, heap Box/growth/align/reuse/OOM,
  stack guard-page fault, KVA frame-count roundtrip, deferred drain,
  vmap, and physmap UC PTE flag read-back (§1.3).
- Host tests for the heap (alignment, reuse, OOM-without-corruption,
  extend, coalesce, double-free), the KVA first-fit/tail-free rules, and
  page-table range walking.
- Harness contract picks up `vibeOS: heap ok` and `vibeOS: kva: ready`
  between `paging: cr3 ok` and boot-done.

### Changed

- `vibeOS: pic: remapped` means the PIC boot step finished: ICW
  remap+mask ran, or FADT skipped the ports. Unlike `paging: mmio uc`,
  it is not a claim that hardware was programmed.

- `-Z build-std` moved off `.cargo/config.toml` onto the Makefile `CARGO`
  line. Cargo merges parent config into `tests/hostlib`, and an inherited
  `build-std` compiles a second `core` that collides with std.
- `ktest` FAIL lines match skip: `vibeOS: ktest: FAIL <name>: <why>`.
- Boot-done marker is `vibeOS: boot: phase1 done` (was `phase0 done`).

- Phase 1 slice B: page tables + MMIO attributes. `vibeos::paging` in the
  library half carries typed `PhysAddr` / `VirtAddr` newtypes, a
  `PageFlags` bitset, a 4-level walk over 4 KiB and 2 MiB leaves, and
  `map_page` / `map_range` / `unmap_page` / `translate` plus
  `patch_physmap_uc` and an `IoremapWindow` bump reservation. The
  mapper refuses to silently overwrite a present leaf (`MapMode::Fresh`
  vs `Remap`) and refuses to split a 2 MiB leaf on a 4 KiB request. Host
  tests cover index math, canonicalization, PTE round-trip, 4 KiB and
  2 MiB round-trips, `map_range` picking 2 MiB when aligned, overlap
  rejection, size-mismatch rejection, `patch_physmap_uc` preserving the
  2 MiB page size, and the ioremap window's bump/exhaustion behavior.
- Kernel-side `paging_init::install` builds a fresh PML4 from buddy
  frames per DESIGN §4.3: kernel `.text` / `.rodata` / `.limine_requests`
  / `.data+bss` mapped with per-section permissions, physmap at HHDM
  with 2 MiB pages over `[0, map_end)` (capped at 8 GiB), 512 MiB low
  identity with the first 2 MiB left executable for the AP trampoline,
  and — when `rsp` falls outside our own map — a duplicate of Limine's
  covering PML4 entry so the stack survives `mov cr3`. `EFER.NXE`
  is set before install, `mov cr3` loads the fresh root, and the exit
  marker `vibeOS: paging: cr3 ok` (DESIGN §3.3 step 7) fires.
- `paging_init::patch_physmap_uc` and `paging_init::ioremap` wire the
  §1.3 API to the kernel; both `invlpg` locally after every edit and
  call `paging::tlb_shootdown_others`, a single-CPU no-op today that
  phase 4 replaces with the IPI 0xFC path (DESIGN §7.9).
- Harness contract picks up `vibeOS: paging: cr3 ok` between the PMM
  free-frames line and the boot-done marker (DESIGN §8.3), still using
  `Marker.and_contains` for the PMM line's shape assertion.
- `x86::read_cr3`, `write_cr3`, `invlpg`, `rdmsr`, `wrmsr`, `read_rsp`,
  plus `IA32_EFER` / `EFER_NXE` constants. All in the binary crate.
- Phase 1 slice A: buddy physical allocator. `vibeos::pmm::Buddy` lives in
  the library half with intrusive doubly-linked free lists inside the free
  pages, splitting on allocate and merging on free up to 4 MiB blocks
  (`MAX_ORDER = 10`). O(1) running free-frame counter; `stats()` reports
  total, free, and the largest available order. Double-free is caught even
  after coalescing by scanning every covering order. Host tests cover the
  full phase-1 checklist: exhaustion, per-order alignment, coalescing after
  freeing alternate blocks, a random alloc/free stream that restores the
  initial free count, and double-free panics.
- Kernel-side `pmm_init` walks Limine's memmap and hands `USABLE` regions
  to the buddy after subtracting frame 0, the loaded kernel image (via
  Limine's executable-address response), the AP trampoline page at
  `0x8000`, and every framebuffer. Prints `vibeOS: pmm: <n> free 4KiB
  frames` as the phase-1 slice-A marker, followed by a diagnostic
  totals/largest-order line.
- Phase 0 kernel: `_start` verifies the Limine base revision, brings up COM1,
  and prints the phase 0 marker contract before halting.
- `x86_64-unknown-none-executable.json` custom target: no PIE, static reloc,
  `code-model: kernel`, `disable-redzone`, `+soft-float`, no RELRO.
- `linker.ld` at `0xFFFF_FFFF_8000_0000` with `.got` inside the mapped image,
  page-aligned sections, and exported `__kernel_vma_{start,end}`.
- Library / binary split from the first kernel commit: `src/lib.rs` holds
  portable modules (`fmt_util`, `marker`, `uart`) with unit tests, `src/main.rs`
  and the binary-only modules (`serial`, `panic`, `x86`) hold hardware pokes.
- `panic-test` build feature + `make run-panic` / `make test-e2e-panic` for
  exercising the panic path end to end (file, line, message, halt without reboot).
- Hybrid BIOS + UEFI ISO built via `xorriso` + `limine bios-install`. `make run`
  boots BIOS by default at `-smp 2`; `make test-e2e-uefi` covers the OVMF path.
- Python e2e harness (stdlib only) with ordered marker assertion, panic-signature
  fast-fail, monitor-quit early exit, and its own unittest suite under
  `make test-harness`.
- `setup.sh` fetches the pinned Limine binary tag and builds the `limine`
  host tool; verifies `qemu-system-x86_64`, `xorriso`, `nasm`, `python3`.
- `make layout` for section table + exported symbols via the toolchain's
  bundled `llvm-objdump` / `llvm-nm`.
- GitHub Actions workflow on push and pull request running the phase 0 ladder.
- `docs/DESIGN.md` and `docs/ROADMAP.md`. Root keeps only the readme and this file.
- Design doc section 9 lists bugs with the rule that prevents each one. Worth
  reading before boot, paging, interrupt, or SMP work.
