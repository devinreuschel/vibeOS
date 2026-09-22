# P2 · Record the known single-lock hotspots as Phase 17 items

**Status:** implemented. ROADMAP §17.4/§17.5/§17.8 and DESIGN §7.7 note the hotspots.
`kernel_tests` spin counters dump per lock rank from ktest `lock_spins`.

| | |
|---|---|
| **Area** | 4.7 Performance & scalability |
| **Impact / Effort / Phase** | Low / S / II |
| **Depends on** | — |
| **Blocks** | — |
| **Review** | [ARCHITECTURE_REVIEW.md §4.7](../ARCHITECTURE_REVIEW.md#47-performance--scalability) |

## Problem

By design and appropriately for 2–4 vCPUs under QEMU: one SCHED lock for the TCB table and timeouts (`src/thread_init.rs:1–6`), one `SpinMutex<Cache>` for the block cache (`src/cache_init.rs:25`), one VFS lock (`src/fs_init.rs:21`), one log-ring TAS (`src/log_init.rs:40`), and virtio-blk bounce buffers on every request (`BOUNCE = 8192`, `copy_to_bounce` in `src/virtio_blk_init.rs`). None of these is a problem now; the risk is rediscovering them without a plan when Phase 17 arrives.

## Recommended fix

No code change. Write them down where Phase 17 will look.

## Implementation plan

1. **ROADMAP §17.4 (Scheduler)** add: "per-CPU TCB ownership or a sharded TCB table; timeouts per CPU (timing wheel, `TimeoutQueue` replacement already anticipated in `src/sched.rs`)".
2. **ROADMAP §17.5 (Scalability)** add: "block cache: per-device or hashed locks; VFS: RCU-style dentry lookup or per-mount locks; log ring: per-CPU staging with a printer thread (ROADMAP §5.5 item already open)".
3. **ROADMAP §17.8 (I/O)** add: "virtio-blk zero-copy: DMA directly from page-cache pages once ROADMAP §10.6 unifies the caches; drop the bounce path".
4. **DESIGN §7.7** add one sentence: "the global locks listed here are known scale limits; see ROADMAP §17".
5. **Counters:** the `blk` shell command already prints hit/miss/request counters; add lock-contention counters (spin iterations per lock rank) behind `kernel_tests` so Phase 17 has a baseline.

## Acceptance criteria

- The four ROADMAP/DESIGN edits exist.

## Tests

None.

## Risks and rollback

None.
