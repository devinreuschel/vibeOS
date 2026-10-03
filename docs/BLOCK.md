# 10. Block I/O

Index: [DESIGN.md](DESIGN.md). This file holds DESIGN §10, and its headings keep DESIGN's numbers.

Phase 7. Portable types live in `crates/core/src/block/mod.rs`, `crates/core/src/block/part.rs`, and
`crates/core/src/block/cache.rs`. Kernel ramdisk, waiters, and the boot marker live in
`src/block/block_init.rs`. virtio-blk packing is `crates/core/src/drivers/virtio_blk.rs`;
the driver is the directory `src/drivers/virtio_blk_init/`. Partition children are
`src/block/part_init.rs`. The write-back cache is `src/block/cache_init.rs`.

## 10.1 Completions

A request carries waiter cookies, not a locked queue. Submit takes the per-device queue lock
(RANK_DEVICE), merges or enqueues, and drops the lock before any copy. The ramdisk pump runs after
that drop; virtio-blk submits to a virtqueue after the same drop and completes from a threaded IRQ.
Rule: completion begins with the claim ([§10.3](#103-failure) step 2), and only the party that
claimed a request touches it. It wakes the waiters under SCHED and then stores the status with
Release inside that section, as its last access to each waiter ([section 2.8](INVARIANTS.md#28-publish-last));
`poll()` loads it with Acquire. `IoWaiter::finish` does this, and the in-guest
`lifetime_iowaiter_publish_last`, which holds the completer just before SCHED, fails if the store
comes first (ROADMAP §10.10, F002). Never hold the queue lock across I/O or across
that wake (DEVICE then SCHED is the wrong order). Hard IRQ must not run this path: enqueue work only
(DESIGN [§2.2](INVARIANTS.md#22-interrupt-handler-rules)). Ramdisk backing is BSS, not a heap `Vec`: allocating
under RANK_DEVICE would take RANK_HEAP (lock order).

Blocking wait and async submit share the same cookie. The buffer and waiter must outlive the
completer's last access to them, which under the rule above is the status store. Once the waiter and
the buffer are counted (ROADMAP §12.5, F042), the completer drops its references to them after it
leaves SCHED, never inside it (§2.11 rules 5 and 6). The lock held across the wake and the status
store, SCHED today, lives outside the waiter, so its unlock after the store touches no waiter
memory. When wait queues get their own locks (ROADMAP §19.4 splits SCHED), a waiter that lives on a
stack, such as `IoWaiter`, is still woken under a lock outside it, or its `wait` takes that lock
before it returns and has no lock-free return.

Planned (ROADMAP §12.5): completion runs in two stages. Stage 1 runs in the party that claimed the
request ([section 10.3](#103-failure)), usually the bottom half. It records the status and does only
work that neither waits nor submits I/O: it wakes the waiters, clears a page's FILLING or WRITEBACK
state with its result (a failed fill leaves the page not up to date and wakes its waiters with the
error), records a writeback error on the mapping, and drops references through `put_deferred`
(§2.11). Everything else is stage 2: data-checksum verification, decryption, a stacked parent's
completion, and a failover or mirror retry. Stage 2 runs as a softirq-equivalent item (§2.2)
queued from the CPU that ran stage 1. The item is part of the request, so queuing it allocates
nothing and cannot fail. A stage-2 item takes no sleeping lock and never waits. Work that needs
more I/O (a read from another mirror, the rewrite of a bad copy, a stacked device's child request)
allocates its request fallibly and only enqueues it on the target's software queue, never waiting
for a descriptor, bounce slot, or tag, and that request has its own stage-2 item. If the allocation
fails, the original request completes with its own error. A page whose fill needs stage 2 stays
FILLING until stage 2 finishes. A stacked parent's completion runs in the item that completed its
member. A read loads the checksums of the blocks it reads before it submits them, in the
submitting thread, so verification at completion reads nothing.

Why: a bottom half that reads a checksum-tree block to verify a fill either waits for its own
device, whose completion only it delivers, or does not wait and leaves the page FILLING with no one
to finish it. Linux runs this work after completion in workqueues (btrfs's end-io workers,
dm-crypt's kcryptd) and looks checksums up at submission, as btrfs does. Rejected: a second,
block-only completion workqueue, which duplicates the softirq-equivalent queue (AGENTS.md rule 10);
a completion thread per queue that may block, which moves the blocked-handler problem up one level;
and Linux's concurrency-managed workqueues with rescuer threads, more machinery than a rule that
stage 2 never waits.

## 10.2 Ordering, flush, and FUA

Rule: the block layer does not order requests. Requests in flight complete in
any order, on any queue. A caller that needs one write durable, or visible to a
later read, before another starts waits for its completion before it submits the
other. The block layer keeps two orders of its own: it never dispatches a write
or discard while an older write or discard to an overlapping range is queued or
in flight, and never merges one past it; and a zoned device has at most one
write in flight per sequential zone (ROADMAP §29.1).

`Flush` makes durable every write whose completion was reported before the
`Flush` was submitted, as virtio 1.2 §5.2.6.2's FLUSH, NVMe's Flush, SCSI's
SYNCHRONIZE CACHE, and Linux's `REQ_PREFLUSH` do. It neither waits for nor
covers a write still in flight, and it holds up no later request. Ramdisk flush
is a successful no-op (the backing is already memory).

A write may carry `Fua`: its completion means it is durable. A device that
offers FUA gets the flag with the write. For any other device the block layer,
not the driver, completes a `Fua` write: it waits for the write, sends a
`Flush`, and reports the write complete when that `Flush` completes, as Linux's
flush machinery does. A driver only says whether its device offers FUA
(AGENTS.md rule 10).

A filesystem commit is built from these: write the new blocks, wait for every
completion, `Flush`, then write the block that makes the commit visible with
`Fua`. A journaling filesystem does this with its commit record. vibefs is not
journaled: it uses CoW metadata plus an atomic superblock switch
([VIBEFS.md](VIBEFS.md) §2, §10), and a generation is durable when its
superblock write, carrying `Fua`, completes.

Adjacent read, write, and discard requests merge; a `Flush`, or a write that
carries `Fua`, merges with nothing.

`block::Queue` records each request it dispatches in an in-flight table, and
both drivers retire it there: `Queue::complete` when the device reports
success, `Queue::abort` when the request fails for good, and `Queue::requeue`
when it is retried. `complete` on a `Fua` write to a device without FUA returns
`Deferred`, so the driver wakes nobody, and the queue's next dispatch is the
`Flush` that reports the write.

Why: a fence that holds every later request until the earlier ones complete
serializes the device. With a queue per CPU ([section 10.4](#104-virtio-blk)),
partitions that share a device ([section 10.5](#105-partitions)), and stacked
devices over several members (ROADMAP §29.1), one `fsync` would drain every
queue, partition, and member, reads included. A filesystem needs only the
completions of its own writes, which it can wait for. Linux replaced its ordered
barriers with completion waits, `REQ_PREFLUSH`, and `REQ_FUA` in 2.6.37 for this
reason, and virtio keeps its barrier feature only in the legacy interface.
Rejected: a device-wide `Barrier` that every later request waits behind; and a
fence per queue, since one filesystem writes from every CPU's queue and would
wait for completions anyway.

## 10.3 Failure

Rule: an I/O error is retried within the request's retry budget, `DEFAULT_RETRY_BUDGET` (3) extra
attempts, which step 5 below shares with resets. A request that exhausts it completes with its
error, and the device stays `Bound`. `Inval` (range, size) is not retried and uses no budget. No
infinite retry loop. Submits to a `Failed` device return `Failed`.

Rule: every request has a deadline, 30 s by default and settable per device, as Linux's block layer
has, and the device gives a request back before anything touches its buffer (§2.11 rule 3: stop the
device, then release). Error handling:

1. Deadline. The deadline starts when the driver dispatches the request to the device, as Linux's
   `blk_mq_start_request` starts it, and each dispatch gets a new one. A stacked device (md, dm,
   multipath, the ROADMAP §18.7 transform) has no deadline of its own. It acts on its members'
   completions, which their error handling bounds.
2. One claim. Exactly one party completes a dispatched request: the bottom half that harvests it,
   the error handler, or removal. That party claims the request before it touches the request's
   buffer or waiter, by a compare-and-swap on the request's state or by taking the request out of
   the driver's table under the queue lock. A harvest that finds its request already claimed leaves
   it alone. Completion ([section 10.1](#101-completions)) begins with the claim.
3. One error handler per resettable unit (a virtio function, an NVMe controller, an AHCI port): a
   kernel thread that sleeps until the unit's earliest deadline or a trigger. A trigger is a
   deadline passing, the device's needs-reset status (virtio's `DEVICE_NEEDS_RESET`), a used-ring
   entry that fails the transport's checks, an IOMMU translation fault, or a surprise-removal
   event. Hard IRQs, bottom halves, and waiters only wake the handler. It is a no-reclaim thread
   (§4.4 rule 4), and it never takes the device-model lock (§12.1), so removal may wait for it
   while holding that lock. `IoWaiter::wait` parks with its device's stall bound S (step 6) as its
   deadline. If S passes, the waiter logs a report naming the request and waits on. It never takes
   the buffer back itself.
4. Recovery. Where the device can abort one command (NVMe's Abort), the handler first aborts each
   timed-out command and waits for the abort. The command's own completion, with whatever status,
   is claimed as usual. Otherwise, or when an abort does not complete in its wait, the handler
   resets the unit:
   1. It marks the unit `Resetting`, so new requests stay in the software queue.
   2. It quiesces each queue: it masks the queue's interrupt through its interrupt controller
      ([section 5.4](INTERRUPTS.md#54-irq-registration)), sets the queue's quiesce flag, which ends any
      bottom-half wait on the device's resources, and waits until the queue's bottom half is idle.
      If every request whose deadline passed has now been claimed, it unquiesces and resets
      nothing.
   3. It resets the device (virtio's status 0, read back as 0; NVMe's `CC.EN` cleared and
      `CSTS.RDY` read back as 0; AHCI's port reset) with IF=1 and no spinlock held, sleeping
      between reads, for at most the unit's reset bound R (step 6).
   4. It claims every dispatched request not yet claimed.
   5. If the reset brought the unit back, it reinitializes the rings and the last-seen used index,
      resubmits the claimed requests in each queue's submission order, Flushes included, and
      unquiesces. Under [section 10.2](#102-ordering-flush-and-fua)'s ordering no request in
      flight depends on another, so the order carries no durability meaning.
   6. If it did not, it clears Bus Master Enable in the function's command register and reads it
      back, detaches the device's IOMMU domain when ROADMAP §18.1's IOMMU is on, completes every
      claimed request with `Io`, and marks the unit `Failed`. Where bus mastering cannot be
      cleared and read back (a transport without it, such as virtio-mmio or a platform device, or
      a function that is gone, whose config reads return all ones) and no IOMMU that translates
      the device blocks it, each request still completes with `Io`, but its buffer is quarantined:
      the driver keeps its frame references and counts them in `blk` and meminfo. A quarantined
      buffer is freed once the device provably cannot DMA: its downstream port reports the link
      down or its presence lost, the kernel has powered its slot off, or an IOMMU that translates
      it has its domain set to blocking and that invalidation has completed. Otherwise it is held
      for good.

   A trigger that arrives during a recovery joins it.
5. Budget. Every resubmission, after an I/O error or after a reset took the request back, uses one
   attempt of the request's retry budget. With none left, the request completes with its error,
   or with `Io` after a reset. A request whose own deadline passes a second time completes with
   `Io` at that recovery, whatever budget it has left, so a command the device never completes
   cannot hold the unit in a reset loop. A reset that brought the unit back leaves it `Bound`.
6. Stall bound. Each driver declares its reset bound R, the longest a recovery takes from the
   handler's first step to the unquiesce, the abort's wait included: virtio-blk's reset poll
   ([section 10.4](#104-virtio-blk)), NVMe's abort wait plus `CAP.TO`, AHCI's port reset. A
   request waits behind at most one recovery before its first dispatch, is dispatched at most
   1 + `DEFAULT_RETRY_BUDGET` times, and each dispatch ends within deadline + R: it completes, or
   its own deadline passes and its recovery ends, or a recovery that began before its deadline
   takes it back. So the device's stall bound S = R + (1 + `DEFAULT_RETRY_BUDGET`) × (deadline + R)
   bounds how long a request of a failing device stays incomplete and how long a thread waits for
   one: 125 s for virtio-blk at the defaults. A stacked device's S is the largest sum of member
   bounds over the members one request can try in turn: every copy of a mirror, every path of a
   multipath device, one member of a stripe. Time a request waits for a free descriptor behind
   requests the device is still completing is load, which S does not bound.
7. One quiesce. Removal, suspend (ROADMAP §20.2), and shutdown (ROADMAP §25.4) begin with step
   4.2's queue quiesce (AGENTS.md rule 10), and `free_vector` sets the same quiesce flag (§5.4).
   Removal marks the device `Removing` (§12.1), wakes the handler, and waits for the handler to
   exit before it frees the device's state. A handler that finds the device `Removing` finishes
   its current step and then does not reset: it quiesces, runs the driver's stop step (§12.2),
   claims every dispatched request, completes it with `Gone`, under step 4.6's quarantine rule
   when the stop step cannot clear bus mastering, and exits.
8. Terminal states. `Failed` and `Dead` (§12.1) are terminal for a registration: the device
   returns only as a new registration with a new id. A `Failed` device's consumers see what a
   removed device's see (below and §12.4): its filesystem never commits again, its dirty pages are
   dropped and their error is reported once to each open file description's next `fsync`, and
   `mount -o remount,rw` returns `EIO`.

A reset is not a power loss. Writes completed before it are assumed to stay in the device's volatile
cache until a later `Flush` makes them durable, as Linux's NVMe and virtio-blk drivers assume.

Planned (ROADMAP §22.2, §25.5): every hang detector waits longer than the stall bound of what it
may be waiting on, so a storage failure the kernel is handling is not taken for a hang. The
blocked-thread report waits at least the largest S among registered block devices, since a thread
waiting for a lock may wait behind another thread's I/O. The watchdog core keeps its timeout at or
above twice that S. Init's trial-boot health check and each critical service's heartbeat allowance
wait at least half the watchdog timeout, so at least that S. The kernel logs each block device's S
when it registers the device. Why: Linux's `i6300esb` driver defaults to a 30 s timeout, the block
deadline itself, so a watchdog left at such a value resets a machine whose RAID was about to absorb
a wedged disk, and a trial boot rolls back a good update because a disk stalled. Rejected: a
shorter block deadline, which times out slow but healthy devices and loaded TCG runners; relying
only on init locking its memory, which a distribution's init does not do; and leaving each timeout
to whoever implements it.

Not yet enforced: no request has a deadline, and `IoWaiter::wait` parks with `FAR_DEADLINE`, so a
lost completion, such as one after a missed kick ([section 10.4](#104-virtio-blk)), blocks its
submitter for good, and so does every thread that then waits for a lock the submitter holds
(ROADMAP §12.5).

Why: a lost completion is a device or driver bug the kernel must survive and report (§1.1
constraints 4 and 5, which already bound every poll). Freeing a timed-out request's buffer while
the device may still write it would turn a hang into memory corruption, so the device stops first.
A request has one claimant because several parties can see it end (the harvest, a timeout, a
needs-reset status, a ring violation, an IOMMU fault, and removal), and the status store of §10.1
is safe only as the last access of one of them. The handler is a thread of the unit's own because
the waiter cannot run it (writeback and readahead have none, and many waiters would race to reset
one device), a bottom half cannot (it may be the blocked party, and a device-wide reset stops every
queue's bottom half, its own included), a softirq-equivalent item may not block while an NVMe reset
poll may last `CAP.TO`'s 127.5 s, and a general workqueue item would stall unrelated items behind
that poll. Quiescing before the claim means a harvest never runs during a reset. Every
resubmission uses the one retry budget so that S exists. Adopted from Linux: the timer started at
dispatch, blk-mq's claim before completion, NVMe's reset work and its retry count for commands a
reset cancelled, and SCSI's error-handler thread per host. Rejected: a timeout that only reports
and keeps waiting, which leaves every waiter behind the request stuck, safe but not live; a
deadline on a stacked device, which would release pages a member may still write; separate stop
mechanisms for reset and for removal, two implementations of one primitive that a removal during a
reset needs both of; and freeing a removed device's buffers with no proof it cannot DMA.

A removed device is `Gone` (§12.4), and every request to it fails with `Gone`. Its consumers
see what Linux shows for a removed device. A filesystem maps `Gone` to `EIO`; it stops
committing and writing back, so its last committed generation stays the on-disk state
([VIBEFS.md](VIBEFS.md) §10), and later writes fail with `EIO`. A read of a page not in the
cache returns `EIO`, and a fault on an unpopulated page of a file mapping raises `SIGBUS`, and
logs one line naming the device, its state, the pid, and the faulting address, at most once a
second per device, so a panic that follows, such as pid 1's, shows the cause in its log tail; a
page-in from a `Failed` device logs the same line. Dirty pages are dropped, and the error is
reported once to each open file description's next `fsync`, `fdatasync`, or `msync`. `umount`
does not fail on the device's errors, and its busy rules are unchanged: `umount2` with
`MNT_DETACH` succeeds while files are open, and a plain `umount` succeeds once they are closed. A
device node's open descriptor fails as Linux's does for a removed device of its class (`ENODEV`
from an input node, for one), and it never reaches a later device. Planned (ROADMAP §20.9):
nothing is removed today.

Logical block size is per device. Do not assume 512. Capacity is in those
blocks. Discard on ramdisk validates the range and otherwise no-ops.

## 10.4 virtio-blk

Modern virtio-blk (`1af4:1042`, `VERSION_1` required) binds by id on the
Phase 6 transport. `F_RO` is accepted: on a read-only device a write or
discard fails with `ReadOnly` before it is queued, with no retry, and reads
and flushes go on (virtio 1.2 §5.2.6.1). A request that fails for good, its
retry budget spent or its error not retryable, completes with its own error
and the device stays `Ready`; until ROADMAP §12.5's error handler resets a
device, the driver fails the device and every queued request only when the
device status has `DEVICE_NEEDS_RESET` (`exhausted_fails_device`). Each bound function is its own instance (`VirtioBlk`), owned by its PCI
registry entry and named `vda`, `vdb`, … in bind order, with its own queues, bounce slots and
vectors; the driver keeps no list of them (DEVICES.md §12.1 rule 1). Config reads capacity (512-byte units), `blk_size` (512
if `F_BLK_SIZE` is absent), and topology when offered. Each request is a
descriptor chain: header + data (or discard range) + status. The status
byte is device-writable DMA, never a stack slot. Completions harvest the
used ring on the threaded IRQ and wake the same `IoWaiter` cookies as the
ramdisk. Its reset polls the status for at most 1 s, sleeping between reads, which is virtio-blk's
reset bound R ([section 10.3](#103-failure)); today `reset` spins for up to a million reads
(ROADMAP §12.5). The hard IRQ only acks ISR, reading it on every interrupt. The driver always
runs on MSI-X, where virtio 1.2 §4.1.4.5.2 says a driver should not read ISR (ROADMAP §26.4, F122).

`F_MQ`: one virtqueue per online CPU, capped by the device `num_queues` and by
`MAX_VQ` (8). Requests on different queues are not ordered against each other
([section 10.2](#102-ordering-flush-and-fua)). Without `F_MQ`, a single
request queue. Each queue gets the largest power of two no larger than the
device's queue size or 64; a device queue that gives fewer than 3 descriptors,
one read or write chain, fails the probe (`queue_size`, `MIN_QSIZE`). Data goes through 16 bounce slots of 8 KiB shared by the
device's queues; a request over 8 KiB is
`Inval`, including one the block queue merged past that size (ROADMAP §12.5,
F119). Flush and discard go to the device when those features are negotiated;
flush without `F_FLUSH` is a successful no-op (nothing to make durable).
virtio-blk has no FUA, so the block layer completes a `Fua` write with a
`Flush` after it (§10.2); without `F_FLUSH` that `Flush` is the same no-op,
since every completed write is already durable.

`issue()` publishes `avail.idx` after `dma_wmb`, then `should_kick` runs
`dma_mb` before it loads `avail_event` or `used.flags` to decide whether to
kick, and `get_used` runs `dma_mb` after its `used_event` store, so neither a
kick nor an interrupt is lost ([section 4.7](MEMORY.md#47-dma); F016). `kick` writes the doorbell at the notify formula in
[section 9.3](PITFALLS.md#93-interrupts). The doorbell
value is the queue index; `kick` writes 0 for every queue (ROADMAP §11.5, F047).

## 10.5 Partitions

MBR (primary + extended/logical) and GPT parse in `crates/core/src/block/part.rs`. Protective
MBR type `0xEE` is not a data device; GPT is. Header and entry CRCs are
checked; a bad primary falls back to the backup header at the last LBA.
EBR walk is capped at 128; a corrupt next-LBA stops the chain. `parse_mbr`
copies the four MBR entries before the EBR walk reuses its sector buffer.
Entries are checked against the disk size only: an
entry that overlaps another entry or the table itself, a GPT header whose
MyLBA is not the LBA it was read from, and a GPT entry outside the usable
range are all accepted (ROADMAP §13.9, F117).

Children are entries of the block registry ([§12.1](DEVICES.md#121-devices)), each a
`BlockRef` with its disk as parent. Child LBA `l` maps to `start + l` and
I/O past `nsectors` is `Inval`. `register_table` registers a child
`<parent>p<N>` (e.g. `ram0p1`, `vdap1`) for every parsed entry, up to
`MAX_PARTS` per table, `N` the entry's index in the table, and each
registration prints the marker `vibeOS: block: <name> <n> sectors`. An
entry it does not register, because the name does not fit in 32 bytes or
the registry refuses it, gets a warning line naming the disk, the entry,
and the reason, and the entries after it are still registered. FAT and
vibefs `mount_dev` mount any registered name, a disk or a partition. Each
registered device has a devfs block node, `/dev/<name>`, which reads and
writes through its `BlockRef` with no lock held: a read at or past the end
returns 0, a write there fails with `ENOSPC`, a partial block is read,
changed and written back, and I/O after the device is gone fails with
`EIO` (ROADMAP §10.4, F081).

`part_init::init` stamps an MBR on `ram0` (RAM) in every build. Only a
`kernel_tests` build stamps a GPT, through `stamp_vda_gpt`, and only on an
all-zero `vda`: a table that fails to parse or has no entries, a 512-byte
block size, at least 1024 sectors, and LBA 0 to 33 and the last 33 sectors
all reading back as zeros. A read error returns the error and stamps
nothing. The production build compiles neither `stamp_vda_gpt` nor its call,
so it writes a disk only for a mounted filesystem or a write to its device
node. `make test-e2e` boots the production ISO with a 1 MiB `mkfs-vibefs`
image on `vda` and with a 1 MiB image whose only non-zero bytes are `0x55AA`
at offset 510, and requires each image's SHA-256 unchanged (ROADMAP §10.11,
F003).

Rule: a lookup of a block device by an identity it carries (a filesystem
UUID or label, a GPT disk GUID, or a partition's unique GUID or label) that
matches more than one device is refused with a log line naming every device
it matched. It is never settled by probe order. Naming a device by its path
always works. The rule binds every such lookup the kernel or the initrd
makes, from `root=` (ROADMAP §14.8, §22.2) to the assembly of stacked
volumes (ROADMAP §29.1). Why: a byte copy of a disk, such as a second
instance of a cloud image or a snapshot attached for rescue, carries its
source's ids, and probe order could mount the copy read-write as root. An
image's first boot replaces the ids it was built with (ROADMAP §26.2), so
only a copy of a disk that has booted can still collide, and `tune-vibefs`
changes a copy's filesystem id offline (VIBEFS.md §15).

## 10.6 Block cache

Page-granular (4 KiB), 16 pages (`cache::DEFAULT_PAGES`), keyed by
`(id, page offset)`, where the id is the device's `BlockRef` id. Read-through,
write-back, clock eviction, sequential readahead, dirty-ratio writeback thread
(`blk-wb`). `PageCache::flush(dev)` makes durable every write to `dev` that returned before it began, in
one sweep of the slots (`Cache::flush_slot`): it writes each page of `dev` that is dirty and
waits for it, and waits for each write of `dev` in flight, once more when that write began
before the flush, since a write after its copy may predate the flush. Then it sends the device
`Flush` (§10.2). A writer that keeps dirtying pages cannot hold it off, since the sweep visits
each slot once (`cache_flush_not_starved`); a page dirtied after the sweep passed it is the next
flush's.

A slot whose device write is in flight is in WRITEBACK (`F_WB`): it keeps its
key, stays readable and writable (a write dirties it again), is never picked by
the clock or re-keyed, and gets no second write until the first completes.
Every cache write sets it: `blk-wb`'s, `flush`'s, and an eviction's, so a dirty
victim is written back in place before it is re-keyed, and the clock prefers a
clean victim. A thread that needs the slot sleeps on the slot's wait queue
until the write ends (ROADMAP §10.11, F015, F043). A slot being filled
(`F_FILL`) is the page's one slot: `find()` matches it, and a second reader or a
writer of the page sleeps on its queue until the fill ends
(`cache_read_waits_for_fill`).

Each disk's `BlockRef` carries the cache (`cache_init::PAGE_CACHE`), and a
partition's I/O goes through its disk's, so a page is keyed by its disk's
never-reused id ([§12.1](DEVICES.md#121-devices), F081). A miss, a writeback, and the
device `Flush` reach the driver through the same handle's `read_dev`,
`write_dev`, and `flush_dev`. A page whose id no longer names a registered
device, or whose write returns `Gone`, is dropped with every page of that id
instead of retried ([§12.4](DEVICES.md#124-removal) rule 9). Hit/miss/device-request
counters are in the `blk` shell command. The cache lock is RANK_DEVICE and is dropped before blocking
device I/O.

Planned (ROADMAP §12.5): one page cache made of mappings, of which this cache becomes one kind.
Every cached page belongs to exactly one mapping at one page index, and the index is its key. A
mapping is a file's (file data, tmpfs files, and the ELF pages ROADMAP §12.2 maps) or a block
device's (filesystem metadata, and reads and writes of the device node). A page's device location is
filesystem block-mapping state and is never a cache key. One frame pool, one LRU, and one set of
writeback threads serve both kinds; do not grow a second private cache. Today's
`(dev_id, page offset)` cache becomes each block device's mapping and keeps the FILLING and
WRITEBACK states and the flush wait above (F015, F043).

- A mounted filesystem reads and writes its file data only through the file's mapping, and caches
  its metadata in the device mapping by LBA. Direct I/O (ROADMAP §19.8) bypasses the cache only
  after it writes back and drops the file mapping's pages over its range, as Linux's does. Reads of
  the device node while it is mounted see the device, not dirty file pages, as on Linux.
- Copy-on-write moves a file's block map, not its cache entry: vibefs writes a dirty file page back
  to a fresh block ([VIBEFS.md](VIBEFS.md) §10) and submits the page's own frame, so the page keeps
  its mapping, index, and frame while the block it lives in changes at every commit.
- A page filled from a filesystem that checksums its data becomes up to date only after the checksum
  that covers it verifies, and a write that changes part of a checksummed block fills and verifies
  the whole block first. A failed fill leaves the page not up to date and never written back: `read`
  and such a write return `EIO` and dirty nothing, and a fault on the page raises `SIGBUS`
  ([VIBEFS.md](VIBEFS.md) §15, Corrupt data). Writeback checksums whatever a page holds, so a merge
  into an unverified page would store the corruption under a valid checksum.
- A block the filesystem allocates loses any page the device mapping holds for its LBA before its
  first write through any path: the page is dropped with its dirty state discarded, and an in-flight
  writeback of the old contents completes before the new write is submitted. A reused LBA never
  returns an old block and is never overwritten by one, as Linux's `clean_bdev_aliases` ensures.
- Index: one radix tree per mapping, keyed by page index, with 64-way nodes (Linux's XArray shape).
  Nodes are allocated before the mapping's lock is taken and freed after it is dropped. The lock is
  a RANK_DEVICE spinlock, as today's cache lock is, and is never held across I/O.
- A page's owner and index in §4.6's frame metadata name its mapping and its index there.

Why: a vibefs file page's device location changes at every commit, and a hole or a page not yet
written back has none, so a key by location moves under readers and mapped PTEs. Linux keys the same
way, with an `address_space` per file and per block device. Rejected: keying file pages by
`(device, LBA)`; a per-file index and a device index over one frame, which gives a frame two owners with no
coherent eviction; a B-tree index, whose splits allocate on insert and remove, for dense keys that
gain nothing from it.

Planned (ROADMAP §12.5): the writeback contract.

- Errors follow Linux's `errseq_t` at the [LINUX.md](LINUX.md) baseline. An error in writeback, or
  in the commit that would make a file's data durable, is recorded on the file's mapping and on its
  volume. Each open file description's next `fsync`, `fdatasync`, `msync` with `MS_SYNC`, or
  `sync_file_range` with a wait flag returns it once; a description opened later returns an error
  that no description has seen yet; `syncfs` returns the volume's errors the same way.
- vibefs keeps a failed commit's file pages dirty and writes them again, to other blocks, at its
  next commit ([VIBEFS.md](VIBEFS.md) §10), where Linux marks them clean; `docs/LINUX.md` lists the
  difference. A volume whose commits fail `DEFAULT_RETRY_BUDGET` times in a row, or whose device is
  `Failed` or `Gone` (§10.3), goes read-only, drops its dirty pages with the error recorded on each
  mapping, and logs it.
- Stable pages. A page of a file whose data is checksummed (vibefs v1's extent CRCs, v2's block
  checksums) does not change while it is written back. Writeback write-protects every user mapping
  of the page through the reverse map and completes the TLB shootdown (ROADMAP §12.1, §12.3) before
  it checksums the page. A `write()`, a write fault, or a kernel write into the page, such as
  truncate zeroing a tail, waits until that writeback completes, a level-3 wait (§2.1), as Linux's
  stable pages do. A page a device holds pinned (ROADMAP §19.8) is written back from a copy,
  checksummed over the copy, and a direct-I/O write to a file whose data is checksummed copies the
  user buffer into a kernel buffer and checksums the copy. The checksum then covers exactly the
  bytes the device writes, with or without §10.4's bounce copy. NOCOW files and unwritten ranges
  ([VIBEFS.md](VIBEFS.md) §15) have no data checksum and need none of this.

Why: an error nobody reports is data a program believes durable, which is why PostgreSQL treats an
`fsync` error as fatal; and a checksum over bytes that changed while the device wrote them makes a
block read as `Corrupt` for good with no disk fault behind it. Rejected: marking failed pages clean,
which loses data a copy-on-write retry to fresh blocks can still save; retrying without a bound,
which pins dirty memory for good behind a device whose writes all fail; skipping checksums for pages
mapped writable, a gap in exactly the files that change most; and bouncing every write, which undoes
ROADMAP §19.8's zero-copy.
