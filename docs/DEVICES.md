# 12. Device model

Index: [DESIGN.md](DESIGN.md). This file holds DESIGN §12, and its headings keep DESIGN's numbers.

A device is what a driver binds to: a PCI function, a device the device tree or ACPI describes, a
USB device, a partition, a stacked block device. [§2.11](INVARIANTS.md#211-object-lifetimes) gives every device
its lifetime rules. This section adds the tree the devices form, the order their callbacks run in,
and who owns a device's resources. It is Linux's driver model (a device tree with supplier links,
deferred probe, one lock per device) without kobjects: ROADMAP §23.3's sysfs renders this tree and
does not own it.

## 12.1 Devices

1. A device is a counted object (§2.11 rule 1) in one registry. A lookup by bus address, name, or
   device number returns a `DevRef`, a counted reference, and a block device's handle is a
   `BlockRef`. Nothing hands out `&'static` to a device or to a driver's per-device state, and no
   device record is `Copy`. A driver's static operations object may be `&'static`; the state for
   each device it binds is owned by that device's registry entry.
2. Every device but a root has a parent: a PCI function its bridge or root port, a USB device its
   hub, a partition its disk. A device may also name suppliers outside the tree: the IOMMU that
   translates it, the ITS its MSIs go through, and the members of a dm or md device, which list that
   device as a holder.
3. A device is `Present`, `Probing`, `Bound`, `Resetting`, `Failed`, `Suspended`, `Removing`, or
   `Dead`. `Resetting` and `Failed` are [§10.3](BLOCK.md#103-failure)'s error-handling states. `Failed` and
   `Dead` are terminal for a registration. One sleeping lock per device serializes `probe`,
   `remove`, `suspend`, `resume`, and `shutdown` on it, and the registry's own lock is never held
   across a driver callback.
4. A block device's id is a 64-bit sequence number never reused within a boot, as Linux's `diskseq`
   is, so the block cache ([§10.6](BLOCK.md#106-block-cache)) and any other table keyed by it cannot alias a
   later device. A device's name is owned by its registry entry.

## 12.2 Order

5. Probe and resume run a device's parent and suppliers before it; remove, suspend, and shutdown run
   its children and consumers before it. Removing a device removes its subtree, deepest device
   first. The order is per device, not per driver, because one driver sits at several depths: nested
   USB hubs, a chain of PCI bridges, dm on dm.
6. A probe whose supplier is not bound returns `Defer`. The binder retries deferred devices after
   each successful bind, and at the end of boot it logs each device still deferred with the supplier
   it waits for, as Linux's deferred probe does. A device that an IOMMU translates gets its domain
   before its first probe: when it is added if the IOMMU is registered, and otherwise when the IOMMU
   registers.
7. A driver's `shutdown` and the stop step of its `remove` are one quiesce function
   (AGENTS.md rule 10). Both begin with §10.3's queue quiesce, which suspend and a reset begin with
   too.

## 12.3 Resources

8. A probe owns its device's resources. It maps only the MMIO and I/O ranges it holds a claim for: a
   PCI BAR, a device-tree `reg` entry, or an ACPI `_CRS` range. A claim is not `Copy`, and it is the
   only way to map a range. The registry refuses a claim that overlaps another claim or a RAM-typed
   range of the boot memory map. The driver enables its device's memory decode before it touches the
   device and bus mastering after it resets it; the binder enables neither. A failed probe, and
   `remove`, reset the device and clear bus mastering before they free memory the device was given
   ([§5.4](INTERRUPTS.md#54-irq-registration)).

Why: a device is freed at its last put and never before, so a hot removal, a partition-table reread,
or a USB unplug cannot leave a handle to freed memory, and an id that is never reused cannot hand an
old device's cached pages to a new one. S3 (ROADMAP §20.2), subtree removal (§20.9), USB hubs
(§20.3), and stacked block devices (§29.1) each need an order between devices, which a flat list
cannot give. A driver that can map a range only through a claim cannot forget to check the range for
an overlap or a RAM alias. Rejected: `&'static` devices that are never freed, which leak a device
per hotplug or reread and which §2.11 rejects for every object; a flat registry ordered by a
per-driver number, which cannot express a hub tree, subtree removal, or an IOMMU before the devices
it translates; and Linux's kobject core, which is more than this needs.

Rule; not yet enforced: the registry is a fixed array of `Copy` PCI records with no parent or state,
bound in a per-driver `order()` (ROADMAP §6.1). `dev_init::bind_all` probes a copy of each record
and writes it back, and it turns on memory decode and bus mastering before `probe`, as `irq_init`'s
MSI and MSI-X setup does again. `pci_init` maps every memory BAR of every function at enumeration,
before any claim ([§3.3](BOOT.md#33-_start-order)). Block devices are named by `&'static str` and reached
by fixed ids (§10.6). ROADMAP §10.4 (D2) and §10.12 land rules 1 to 4 and 8 for PCI and block
devices, §11.5 and §20.7 apply rule 8 to device-tree and ACPI devices, §18.1 lands rule 6, and
§20.2, §20.3, §20.9, and §25.4 land rules 5 and 7 for suspend, hubs, removal, and shutdown.

## 12.4 Removal

9. Removal kills a device in §2.11 rule 3's order. It unpublishes the device from the registry, its
   `/dev` node, and its `/dev/disk/by-id` link, and from ROADMAP §23.3 on it sends a `remove`
   uevent. It closes the device's gate, so every later operation through a `DevRef` or `BlockRef`
   still held fails with `Gone`, and threads sleeping on the device wake and fail. It waits for the
   operations already inside, whose requests complete, fail at their deadline
   ([§10.3](BLOCK.md#103-failure)), or fail at once on a disconnected function. It stops the device and
   fails what the device still holds through the error handler (§10.3 step 7), and frees its
   vectors ([§5.4](INTERRUPTS.md#54-irq-registration)). It then detaches the device's IOMMU domain and completes
   the IOTLB and device-TLB invalidation, and only then frees the device's DMA buffers. A
   device-TLB invalidation is skipped for a function marked disconnected, as Linux's VT-d driver
   skips it, since a gone function never answers. Every IOMMU invalidation wait (VT-d's wait
   descriptor, SMMUv3's `CMD_SYNC`) has a bound, 1 s as Linux's SMMUv3 driver uses. When it
   expires, the device's domain is set to blocking, its IOVAs and the frames behind them stay
   reserved, and the event is logged. Its memory goes at the last put. A device's subtree is
   removed first (rule 5).
10. A surprise removal, which the slot reports through its presence-detect or link-down interrupt,
    first marks the function disconnected: its driver fails I/O at once without resetting it, and a
    register read that returns all ones ends any poll.
11. A network interface being removed also deletes its routes and neighbour entries, leaves any
    bridge, and fails sends on sockets bound to it with `ENODEV`. Its ifindex is allocated in
    increasing order and not reused at once, as a pid is (§2.11 rule 4).

Why: a surprise removal cannot be refused, and a removal that waited for every holder to close would
hang behind a shell's open descriptor, so removal fails the holders' operations and lets them close
when they will, as Linux does. The IOMMU and interrupt-remapping steps come before memory and
vectors are reused, because a device that is stopped but still translated can write a freed buffer,
and a remapping entry left behind lets it raise the next owner's vector. Rejected: refusing removal
while a filesystem is mounted, which a surprise removal cannot honor; forcing an unmount at removal,
which races open descriptors and hides the error from the programs that hold them.

Planned (ROADMAP §15.1, §20.3, §20.9): nothing is removed today.
