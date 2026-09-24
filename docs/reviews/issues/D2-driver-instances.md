# D2 · Driver and volume instances instead of module singletons

| | |
|---|---|
| **Area** | 4.3 Data model & state management |
| **Impact / Effort / Phase** | Medium / L / III |
| **Depends on** | A1 (directory layout), Q3, ROADMAP §10.4's `kalloc` and counted-object boxes |
| **Blocks** | ROADMAP §20.4 NVMe, §20.5 AHCI, second virtio-blk disk |
| **Review** | [ARCHITECTURE_REVIEW.md §4.3](../ARCHITECTURE_REVIEW.md#43-data-model--state-management) |

## Problem

- `src/virtio_blk_init.rs` keeps one device in ~18 module statics (`BLK: SpinMutex<Option<Box<Blk>>>`, `ISR_VA`, `LIVE`, `STATE`, `FEATURES`, `BLK_SIZE`, `CAP`, `NQ`, `TOP_HITS`, `THREAD_HITS`, `COMPLETIONS`, `PHYS_EXP`, `ALIGN_OFF`, `MIN_IO`, `OPT_IO`, `MAX_DISCARD`, `IO_REQS`); a second virtio-blk function cannot be bound.
- Fifteen `*_init.rs` modules carry a `static LIVE: AtomicBool` and/or `STATE`; the ramdisk (`block_init`), cache, FAT initrd volume ("lives in BSS"), and vibefs ("BSS image by default") follow the same one-instance shape.
- `src/dev.rs` already has a `Registry` with drivers, bind order and exclusive resource claims, and `src/block.rs` has `MAX_BLOCKDEVS = 8`, but driver state is not stored per registry entry.

## Recommended fix

Instance structs owned by the registry entry (or a block-device table), with the `LIVE` idea becoming a state on the instance. Do it for virtio-blk first (the only multi-instance-capable device today); ramdisk and cache stay singletons by nature; FAT/vibefs already have volume slots (`SLOTS`, `nvol()`).

## Implementation plan

1. **Block-device table.** In `block_init` (or a new `block/registry.rs`): `register` takes the device, its owned name (up to 32 bytes, Linux's `DISK_NAME_LEN`), and, for a partition, its disk's `BlockRef` as parent; it assigns a 64-bit id never reused within the boot and returns a `BlockRef`, a counted handle (DESIGN §12.1). `get(name)` returns a `BlockRef`. `unregister` unpublishes the device and closes its gate (DESIGN §2.11 rule 3), after which I/O through a held `BlockRef` returns `Gone`. `part_init`, `cache_init`, FAT, vibefs, and devfs block nodes hold a `BlockRef`, and the cache keys its pages by the device's id. No registry call returns `&'static`.
2. **`VirtioBlk` struct** holding everything that is a module static today plus its `Vq` array, DMA slots, MSI-X vectors, and counters. `probe(dev: &DevRef)` builds one, and the device's registry entry owns it (DESIGN §12.1); `Driver::probe` registers its block device in the block table with the device as parent. The driver module keeps no list of instances.
3. **IRQ routing per instance.** Threaded-IRQ handlers receive an `arg: usize` (they already do for workqueue items); pass the instance index so `harvest()` and `blk_work()` operate on the right device.
4. **Shell/diag:** `blk` lists every registered block device with its name, id, and parent; `block: <name> <n> sectors` marker per instance (names `vda`, `vdb`, …, built into each device's owned name).
5. **Tests:** ktest boots with two `virtio-blk-pci` devices (`tests/harness` device preset gains `num_disks`), asserts `vda` and `vdb` markers, does a write/read on each, and checks that failing one (`inject_io_fails` on `vdb`) does not affect `vda`.
6. **Volumes:** FAT and vibefs already support `mount_dev(name, at)` per volume; verify `MAX_VOLS` slot arrays follow D1 (heap) and drop the "BSS image" default in favour of an explicit `mount_mem` in `fs_init` (already exists for vibefs).

## Acceptance criteria

- `grep -c '^static' src/virtio_blk_init.rs` ≤ 3 (driver registration; the registry entry owns each instance).
- Two virtio-blk disks bind, both persist across reboot (`persist: intact` for both).
- Existing single-disk ktests unchanged.
- `src/block.rs` has no `&'static dyn BlockDevice` and `BlockDevice` has no `fn name(&self) -> &'static str`; `Device` in `src/dev.rs` is not `Copy`.
- The host test of ROADMAP §10.4's registry box: a partition unregistered while a `BlockRef` to it is held returns `Gone` for I/O through the handle, a lookup by name no longer finds it, and a new registration gets a new id.

## Tests

The two-disk ktest; e2e unchanged (`pc` set has no virtio-blk).

## Risks and rollback

Largest driver rewrite in the plan; its PR first splits the file as Q5 step 5 describes, in a commit that only moves code, so the rewrite's diff stays readable. Rollback is the previous singleton module (keep it compiling behind a feature for one release if needed).

## Out of scope

NVMe/AHCI themselves; page-cache unification.
