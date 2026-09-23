# D2 · Driver and volume instances instead of module singletons

| | |
|---|---|
| **Area** | 4.3 Data model & state management |
| **Impact / Effort / Phase** | Medium / L / III |
| **Depends on** | A1 (directory layout), Q3 |
| **Blocks** | ROADMAP §20.4 NVMe, §20.5 AHCI, second virtio-blk disk |
| **Review** | [ARCHITECTURE_REVIEW.md §4.3](../ARCHITECTURE_REVIEW.md#43-data-model--state-management) |

## Problem

- `src/virtio_blk_init.rs` keeps one device in ~18 module statics (`BLK: SpinMutex<Option<Box<Blk>>>`, `ISR_VA`, `LIVE`, `STATE`, `FEATURES`, `BLK_SIZE`, `CAP`, `NQ`, `TOP_HITS`, `THREAD_HITS`, `COMPLETIONS`, `PHYS_EXP`, `ALIGN_OFF`, `MIN_IO`, `OPT_IO`, `MAX_DISCARD`, `IO_REQS`); a second virtio-blk function cannot be bound.
- Fifteen `*_init.rs` modules carry a `static LIVE: AtomicBool` and/or `STATE`; the ramdisk (`block_init`), cache, FAT initrd volume ("lives in BSS"), and vibefs ("BSS image by default") follow the same one-instance shape.
- `src/dev.rs` already has a `Registry` with drivers, bind order and exclusive resource claims, and `src/block.rs` has `MAX_BLOCKDEVS = 8`, but driver state is not stored per registry entry.

## Recommended fix

Instance structs owned by the registry entry (or a block-device table), with the `LIVE` idea becoming a state on the instance. Do it for virtio-blk first (the only multi-instance-capable device today); ramdisk and cache stay singletons by nature; FAT/vibefs already have volume slots (`SLOTS`, `nvol()`).

## Implementation plan

1. **Block-device table.** In `block_init` (or a new `block/registry.rs`): `pub fn register(name: &'static str, dev: Box<dyn BlockDevice + Send + Sync>) -> DevId` and `pub fn get(id) -> &'static dyn BlockDevice`; `part_init` and `cache_init` key by `DevId` (they already take a `u32` dev id).
2. **`VirtioBlk` struct** holding everything that is a module static today plus its `Vq` array, DMA slots, MSI-X vectors, and counters. `probe(dev: &Device) -> Result<Box<VirtioBlk>, ProbeError>` builds one; the driver's `Driver::probe` registers it in the block table and keeps `Box<VirtioBlk>` in a `SpinMutex<Vec<…>>` inside the driver module.
3. **IRQ routing per instance.** Threaded-IRQ handlers receive an `arg: usize` (they already do for workqueue items); pass the instance index so `harvest()` and `blk_work()` operate on the right device.
4. **Shell/diag:** `blk` command lists all registered devices; `block: <name> <n> sectors` marker per instance (names `vda`, `vdb`, …).
5. **Tests:** ktest boots with two `virtio-blk-pci` devices (`tests/harness` device preset gains `num_disks`), asserts `vda` and `vdb` markers, does a write/read on each, and checks that failing one (`inject_io_fails` on `vdb`) does not affect `vda`.
6. **Volumes:** FAT and vibefs already support `mount_dev(name, at)` per volume; verify `MAX_VOLS` slot arrays follow D1 (heap) and drop the "BSS image" default in favour of an explicit `mount_mem` in `fs_init` (already exists for vibefs).

## Acceptance criteria

- `grep -c '^static' src/virtio_blk_init.rs` ≤ 3 (driver registration + instance list).
- Two virtio-blk disks bind, both persist across reboot (`persist: intact` for both).
- Existing single-disk ktests unchanged.

## Tests

The two-disk ktest; e2e unchanged (`pc` set has no virtio-blk).

## Risks and rollback

Largest driver rewrite in the plan; do it after Q5 has split the file so the diff is readable. Rollback is the previous singleton module (keep it compiling behind a feature for one release if needed).

## Out of scope

NVMe/AHCI themselves; page-cache unification.
