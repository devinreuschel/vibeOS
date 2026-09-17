# Changelog

All notable changes to **vibeOS** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
  in `make test` and CI.
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
