# S1 · Finish the pre-ring-3 hardening checklist

**Status:** in the [index](README.md). #85: SMEP/SMAP/UMIP/`CR0.WP` via `arch::cpu::harden` on BSP and APs;
ktest `cpu_hardening` (skip on `qemu64`); `/dev/random` via virtio-rng then RDRAND then
xorshift (`dev_random_source`). Parked at the time: the SMAP fault test and the user-VA
`copy_from_user` rewrite, now ROADMAP §10.6 and a Phase 10 gate line (the HHDM copy ignores
PTE write permission, which COW in Phase 12 cannot tolerate); the entropy pool, now §13.10;
`VIBEOS_NO_HARDEN`.

| | |
|---|---|
| **Area** | 4.8 Security |
| **Impact / Effort / Phase** | Medium / S / II (before Phase 9.1) |
| **Depends on** | — |
| **Blocks** | Phase 9.1 ring-3 plumbing |
| **Review** | [ARCHITECTURE_REVIEW.md §4.8](../ARCHITECTURE_REVIEW.md#48-security) |

## Problem

- `src/paging_init.rs` enables `EFER.NXE` (line 481–487), maps `.text` executable-only and `.rodata`/`.data`/`.bss`/heap NX; stacks have guard pages; MMIO is UC. Good.
- No references to SMEP, SMAP, UMIP, or `CR0.WP` exist in `src/` (only the AP trampoline sets `CR0.WP` for APs, `src/arch/trampoline.S`). `src/x86.rs` has no CR4 accessors. `src/desc.rs` already lays out user selectors for `syscall`/`sysret`, so ring 3 is close.
- `/dev/random` is a non-blocking xorshift (`CHANGELOG.md`, Phase 8C entry) even though virtio-rng is bound when present (`src/virtio_init.rs::rng_*`) and `rdrand` is available on `-cpu max`.

## Recommended fix

Set SMEP/SMAP/UMIP and verify `CR0.WP` on every CPU before the first ring-3 instruction, provide `stac`/`clac` helpers for future user-copy routines, test it in-guest, and give `/dev/random` real entropy.

## Implementation plan

1. **`src/x86.rs`:** `read_cr0/write_cr0`, `read_cr4/write_cr4`, constants `CR0_WP = 1<<16`, `CR4_UMIP = 1<<11`, `CR4_SMEP = 1<<20`, `CR4_SMAP = 1<<21`; `cpuid(7,0)` feature checks `ebx[7]` SMEP, `ebx[20]` SMAP, `ecx[2]` UMIP; `stac()`/`clac()` wrappers (no-ops when SMAP is unsupported, tracked by a static).
2. **`arch::cpu::harden()`** called on the BSP right after `idt ok` and on each AP in `smp_init`'s `ap_entry` (after its GDT/IDT load): set the supported bits, assert `CR0.WP`, log one diagnostic line `vibeOS: cpu: smep=1 smap=1 umip=1 wp=1` (not a contract marker; `klog!`).
3. **ktest `cpu_hardening`:** assert CR4/CR0 bits match CPUID on every online CPU (use `call_function_ipi` to read CR4 remotely); skip with reason under a CPU model that lacks the features. Under TCG `-cpu max`, SMEP/SMAP/UMIP are emulated; the LAPIC-fallback variant uses `qemu64`, which lacks them → skip path exercised.
4. **Future user-copy contract:** add DESIGN §5.1 note: "all user-memory access goes through `copy_from_user`/`copy_to_user` which bracket with `stac`/`clac` and validate ranges (ROADMAP §9.3)".
5. **`/dev/random`:** in `src/fs/kernfs.rs` random backend, source bytes from a kernel `entropy::fill(buf)` that prefers virtio-rng (`virtio_init` exposes the data buffer already) then `rdrand` (CPUID `ecx[30]`), then the xorshift with a one-time `klog!` warning. ktest `dev_random_source` asserts the source is not xorshift when virtio-rng is bound (ktest QEMU set has it).

## Acceptance criteria

- `cpu_hardening` passes on `-smp 2`/`-smp 4`, skips on `qemu64`.
- `grep -n 'xorshift' src/fs/kernfs.rs` shows only the fallback path with the warning.

## Tests

The two ktests.

## Risks and rollback

Setting SMAP before any user memory exists is harmless; the only risk is enabling UMIP on a CPU model that mis-reports it (unlikely under QEMU). Bits can be individually disabled by a `VIBEOS_NO_HARDEN` debug feature if needed.
