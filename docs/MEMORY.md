# 4. Memory

Index: [DESIGN.md](DESIGN.md). This file holds DESIGN §4, and its headings keep DESIGN's numbers.

Three allocators, one address map. Physical frames come from a buddy allocator. Kernel virtual
addresses come from a range allocator. Small objects come from a heap layered on the other two.

## 4.1 Virtual address map

x86_64 canonical addressing splits at bit 47. Low half is user, high half is kernel, with a
non-canonical hole between. The map assumes 4-level paging (48-bit virtual addresses) on x86_64, as
§11.2 does on aarch64; 5-level paging (LA57, and LPA2 on aarch64) is ROADMAP §27.6's stretch, and
adopting it re-plans this table. Kernel regions are fixed, not discovered, except the physmap base
(below the table):

| Range | Size | Role |
|-------|------|------|
| `0x0000_0000_0000_0000` – `0x0000_7FFF_FFFF_FFFF` | 128 TiB | User address space, one PML4 per process (`AddressSpace`), below `USER_END`. Page 0 is never mapped (`NULL_GUARD_LEN`). The top 4 KiB page is never mapped either: user mappings end at `USER_MAP_END` (`0x0000_7FFF_FFFF_F000`), which the ELF loader and every address-space range check use, so a `syscall` in the last mappable page returns to a canonical RIP. The syscall exit still sends a non-canonical saved RIP to `SIGSEGV` (§5.10 rule 2). Each user PML4 copies the kernel's PML4[256..512) at creation, so the whole kernel half stays mapped, supervisor-only, while ring 3 runs: no KPTI (ROADMAP §18.3, F024, F133). |
| `0x0000_0000_0000_0000` – `0x0000_0000_2000_0000` | 512 MiB | Low identity window, kernel PML4 only (user PML4s do not copy slot 0). 2 MiB pages, GLOBAL; the first 2 MiB supervisor writable and executable. |
| *hole* | | Non-canonical. Any pointer here is a bug. |
| Limine's HHDM offset +, inside the slot `0xFFFF_8000_0000_0000` – `0xFFFF_C000_0000_0000` | 64 TiB slot; today `map_end` ≤ 8 GiB, plus leaves added above it | Physmap, `virt = phys + ` the HHDM offset, discovered at boot (below the table); today the constant `HHDM_BASE`, which `boot::capture` asserts Limine's offset equals. 2 MiB pages up to `map_end`. Above it: 4 KiB leaves from `acpi_init::map_gap` (no cap), and write-back leaves for a display BAR0 from `paging_init::ensure_physmap_wb` (below `PHYSMAP_CAP`). The physmap never leaves its slot (below the table). |
| `0xFFFF_C000_0000_0000` – `0xFFFF_C000_0400_0000` | 64 MiB | Kernel heap. Starts at 1 MiB mapped and grows. Planned (ROADMAP §12.6): the region's size is set at boot from installed memory, up to the 16 TiB below the KVA region, so the heap can grow as far as RAM does ([§4.4](#44-kernel-heap)). |
| `0xFFFF_D000_0000_0000` – `0xFFFF_D010_0000_0000` | 64 GiB | Kernel VA allocator: guarded stacks, `vmap`, large transient mappings. |
| `0xFFFF_E000_0000_0000` – `0xFFFF_E000_1000_0000` | 256 MiB | `ioremap` window for device MMIO that should not be reached through the physmap. Today a bump allocator that never frees; planned (ROADMAP §20.1): §4.5's range allocator over its slot, with `iounmap`. Its slot ends at `0xFFFF_EA00_0000_0000` (10 TiB); ROADMAP §20.1 sizes the window inside it. |
| `0xFFFF_EA00_0000_0000` – `0xFFFF_EB00_0000_0000` | 1 TiB | Frame metadata (ROADMAP §12.1): one `Frame` per 4 KiB of physical memory up to 64 TiB, indexed by physical frame number, virtually contiguous, and populated one memory section at a time, so a hole costs page tables only. A const assertion holds `size_of::<Frame>()` to 64 bytes. |
| `0xFFFF_EC00_0000_0000` – `0xFFFF_FC00_0000_0000` | 16 TiB | KASAN shadow, in the ROADMAP §12.1 KASAN build only: one shadow byte per 8 bytes of the kernel half, at LLVM's x86_64 kernel-address offset `0xDFFF_FC00_0000_0000` (aarch64: §11.2). |
| `0xFFFF_FFFF_8000_0000` – `0xFFFF_FFFF_FFFF_FFFF` | 2 GiB | Kernel image. Matches the `kernel` code model so `.text` relocations fit in 32-bit displacements. |

Regions must not overlap and every one asserts that its range is unmapped before claiming it. This is
a real failure mode: two subsystems in the old tree were both designed at `0xFFFF_C000_*` and only one
noticed.

Inside the user half, the loader maps each `PT_LOAD` at its `p_vaddr`, and the `brk` heap starts on
the page after the highest segment's end (`AddressSpace::set_brk_start`), as on Linux with
randomization off; it is one region that grows and shrinks at its top. The 32 stack pages end at
`0x8000_0000`, with the TLS block, when the image has one, in the pages just below them. Anonymous
`mmap` places a request with no usable hint top-down from `MMAP_TOP` (`USER_MAP_END` − 128 MiB,
`0x7FFF_F7FF_F000`, Linux's base without randomization), one region per call, never merged. Every
page of the image, the heap, and an `mmap` is allocated and zeroed at the call until ROADMAP §12.4
makes them lazy; a `PROT_NONE` mapping is a region with no frames. `munmap` trims, splits, or
removes the regions in its range, so `fork`'s copy (`clone_anon`) sees exactly the pages that are
mapped.

The KASAN build passes each architecture's shadow offset to LLVM explicitly
(`-Cllvm-args=-asan-mapping-offset=`), never LLVM's default: LLVM picks its Linux kernel offset only
for a Linux x86_64 triple, and for both kernel targets its default is a user-space offset whose
shadow of the kernel half is non-canonical. In that build every kernel address's shadow reads as
accessible from the first instruction: before any Rust runs, `_start` points the whole shadow range
at one read-only zero page through three shared table pages, in uninstrumented `arch` asm, as
Linux's early shadow does, and each direct entry (ROADMAP §25.4) does the same.
`paging_init::install` carries these entries into the kernel's tables, and since it gives every
kernel-half PML4 slot its own PDPT (the next paragraph), each PML4 slot of the heap's and the KVA
region's shadow has a PDPT of its own, so real shadow later changes no PML4 entry (I12). From
`kva: ready`, heap growth and every KVA map allocate and map their own shadow in the same operation,
under the locks that operation already takes, and fail with `ENOMEM` when they cannot; a KVA free
poisons its shadow and unmaps it after the range's shootdown. The heap region's shadow grows with
the heap. The physmap and the image keep the zero page, so their accesses are not checked.

Every kernel-half PML4 slot exists before the first user address space (I12): `AddressSpace::new`
copies PML4[256..512) once, so a slot added later is missing from every address space made before
it. Planned (ROADMAP §12.1): on x86_64 `paging_init::install` allocates all 256 kernel-half PDPTs,
1 MiB, so no region, randomized base, shadow, or hot-added range ever needs a PML4-level table, and
the kernel mapper panics if it would change a kernel-half PML4 entry once an `AddressSpace` exists.
aarch64 needs no counterpart, since no address space copies TTBR1's tables.

The physmap base is the one region the kernel discovers. It is Limine's HHDM offset, which the Limine
protocol says "may vary between boots, including for randomisation", and which an executable "must
not assume". The kernel adopts that offset as its own physmap base, so a physical address has the
same alias before and after its `mov cr3`, and reads it once into `BootInfo`; every physical-to-virtual
translation uses that one value. `boot::capture` checks that the HHDM offset lies in the physmap's
slot (below) and halts with a named reason if it does not. Rule; not yet enforced:
`paging_init::HHDM_BASE` is a constant, and `boot::capture` asserts that Limine's offset equals it
(ROADMAP §11.1). ROADMAP §18.2 later draws the other bases from entropy too.

Planned (ROADMAP §25.4, §26.4): the kernel base and the HHDM offset have two sources, Limine's
responses and the image's own direct entry, which every boot without Limine takes. The direct entry,
one per architecture, calls one portable, host-tested module in `vibeos-core`, the one place outside
Limine that computes a base: it draws both inside this table's slots, applies the image's
relocations, and plans the tables the kernel starts on. The entry builds those tables and records
the base, the offset, and a seed for the other bases in `BootInfo`, where `boot::capture` reads them
on every path. A loader that starts another kernel, kexec's included, places bytes and writes a
serialized `BootInfo`, and draws no layout: it runs the release before, which does not know the next
release's table. So this table may change between releases without breaking an update reboot
(ROADMAP §30.4). Rejected: the loader drawing the layout, which freezes the old release's table into
every later kernel.

The physmap's slot is `0xFFFF_8000_0000_0000` – `0xFFFF_C000_0000_0000` on x86_64 (aarch64: §11.2).
Limine's `randomise_hhdm_base` (ROADMAP §18.2) raises the offset above the slot's base by a draw in
1 GiB steps below 2^(VA bits − 3), 32 TiB with 48-bit VAs, so RAM that ends at or below 32 TiB fits
under every draw and RAM up to 64 TiB fits as the draw allows. Planned (ROADMAP §11.1, §11.2): the
physmap builder leaves a RAM-typed range whose alias would pass the slot's end out of the physmap and
the buddy, with the registered marker `vibeOS: pmm: <n> MiB past the physmap slot ignored`, as Linux
drops RAM past `MAXMEM`, and hot-added RAM past it is refused the same way (ROADMAP §27.6). One
`vibeos-core` function answers whether a physical range lies in the physmap; the builder and every
`HhdmPhys` translation use it (ROADMAP §20.1, F136).

The low identity window exists for one reason: an AP starting from SIPI runs in real mode and then
32-bit protected mode in the trampoline page below 1 MiB (§7.3), so that page must be identity
mapped and executable. All 512 MiB stay mapped and GLOBAL for the life of the kernel CR3, so a
NULL-plus-offset access from a kernel thread reads low RAM instead of faulting, and buddy frames
below 2 MiB have a supervisor writable, executable alias. Planned (ROADMAP §10.6, F085): the window
is torn down after `smp: done`, keeping only the trampoline page (4 KiB, read-only, executable, not
global).

The physmap is capped at 8 GiB (`PHYSMAP_CAP`) regardless of what the memory map says. Some firmware
describes MMIO BARs as multi-terabyte regions, and walking that to build page tables at boot does not
finish. `paging_init::physmap_extent` sets `map_end` to the 2 MiB-rounded maximum of the usable-RAM
end, the kernel image end, and each framebuffer's end, capped at 8 GiB, and ignores raw memory map
entries. `acpi_init::map_gap` then adds 4 KiB leaves above `map_end` for ACPI tables (write-back) and
for the LAPIC, I/O APIC, and HPET (UC), with no cap. RAM above the cap never enters the buddy:
free-list nodes, page tables, and heap pages are all reached through the physmap after `mov cr3`, so a
frame past it triple-faults on first touch. The physmap covers a framebuffer only below the cap, so for a
framebuffer that extends past 8 GiB `fb_init` writes through an unmapped address and boot halts at
console init (ROADMAP §11.2, F020).

Planned (ROADMAP §11.2): one physmap policy on both architectures. The physmap maps only the
RAM-typed ranges of the memory map (usable, bootloader-reclaimable, executable and modules, ACPI
reclaimable, ACPI NVS), inside its slot, and nothing else. It maps the kernel image's physical span
at 4 KiB on both architectures, since ROADMAP §18.1 gives that span per-section permissions and a
live aarch64 block is never split (ROADMAP §11.2); every other range takes the largest page its
alignment allows. After boot the physmap changes only in ROADMAP §12.1's `debug_mm` build, which maps
it at 4 KiB and unmaps or remaps a frame by one atomic exchange of its existing leaf. Device MMIO is
reached only through `ioremap`. Memory the kernel does not own that is not device MMIO (a firmware
table or an AML `SystemMemory` region outside the RAM-typed ranges, a framebuffer, and a capture
kernel's view of the crashed kernel's RAM, ROADMAP §25.4) is reached through `memremap`, which maps
the range in the KVA region through §4.5's allocator with the memory type the range needs: write-back
on x86_64, where MTRRs keep device memory uncached whatever the page attribute says; on aarch64 the
EFI memory map's attribute for the range (ROADMAP §20.9), and Device through `ioremap` where no map
describes it, as Linux's `acpi_os_ioremap` does; for a framebuffer, the type ROADMAP §11.1 gives its
location; for the crashed kernel's RAM, write-back. `memunmap` frees the range after §4.5's
shootdown. A firmware region described as terabytes of MMIO then costs nothing, which
removes the reason for the cap, so the cap goes and RAM above 8 GiB joins the buddy. Rejected:
keeping x86_64's whole-range physmap with in-place UC patches beside aarch64's RAM-only one, which
would leave the portable page-table code two policies for one primitive (AGENTS.md rule 10) and keeps
the UC-alias bug class (F104) alive. Rejected: a physmap leaf added when a firmware table is first
read, which allocates page tables at run time under `PT`, is always write-back, and aliases another
region for an address past the slot.

## 4.2 Physical memory: buddy allocator

Free blocks of order *k* cover 2^k contiguous 4 KiB frames. Split on allocation, merge with the buddy
on free. Free list nodes live inside the free pages themselves, so there is no bitmap and no
allocation needed to run the allocator.

That last property has a sharp edge: a stray write into a freed page corrupts the allocator's linked
lists, and the resulting crash happens later, somewhere unrelated. This is exactly why kernel stacks
get guard pages.

```rust
pmm_init::with_buddy(|b| ..)                   // the global `Buddy`, RANK_BUDDY, IRQ-off
const Buddy::new(hhdm: u64) -> Buddy           // free-list nodes at phys + hhdm
Buddy::alloc(order: u8) -> Option<Frames>      // order <= MAX_ORDER (10)
Buddy::alloc_constrained(order, max_phys) -> Option<Frames>  // ends at or below max_phys
Buddy::free(f: Frames)                         // safe
Buddy::order_for(bytes, align) -> Option<u8>   // DMA sizing, §4.7
Buddy::stats() -> PmmStats                     // total, free, largest free order
Frames::base() -> PhysAddr, order() -> u8, count() -> usize
Frames::into_entry(self) -> PhysAddr           // ownership moves into a page-table entry
unsafe Frames::from_entry(pa, order) -> Frames // only after clearing that entry
pmm::leaked_frames() -> usize                  // dropped tokens; `meminfo` prints it
```

Initialization walks the Limine memory map and ingests every `USABLE` region as power-of-two aligned
blocks, excluding:

- physical frame 0
- the loaded kernel image span
- the AP trampoline page, `0x8000` today (§7.3)
- the framebuffer
- anything not marked `USABLE`, including bootloader and ACPI reclaimable
- anything above the 8 GiB physmap cap (§4.1)

Planned (ROADMAP §10.6): the exclusions are §2.4's, clipped from each usable range as `BootInfo` is
read, with no fixed-size list.

`stats().free_frames` is a running counter, so `meminfo` costs O(1). `deallocate` does not: its
double-free check and its buddy lookup walk the free lists, O(`MAX_ORDER` × list length) per call, so
tearing down a large address space holds PT with IRQs off for that long (ROADMAP §12.1, F029).

Planned (ROADMAP §12.1): physical memory is counted in sections of 128 MiB (2^27 bytes, 32,768
frames) on both architectures, as on Linux x86_64 and arm64 with 4 KiB pages. At 64 bytes per frame
([§4.6](#46-what-comes-later)'s frame model), one section's metadata is exactly one 2 MiB leaf, and no
buddy block (at most 4 MiB, `MAX_ORDER` 10) or its buddy crosses a section boundary.

Planned (ROADMAP §27.3): a section joins its node's buddy on demand rather than at boot, so the
initialization above runs a section at a time and boot touches only the memory it uses.

- Metadata. Before the buddy is built, boot reserves each node's frame metadata from that node's own
  RAM, one 2 MiB-aligned run per section that holds RAM, packed from the top of the node's RAM
  downward, and maps all of it then, with 2 MiB leaves, in a frame-metadata region §4.1 reserves.
  Nothing writes it until its section joins, so the reservation touches no memory, a join writes no
  page table, and low memory and long free runs stay for DMA ([§4.7](#47-dma)) and huge pages. The
  runs are recorded per node, and a read of a frame's metadata (ROADMAP §12.1) reports a frame in
  them as kernel memory whatever its section's state. Hot-added memory (ROADMAP §27.6) takes its
  metadata from its own first 2 MiB and maps it under PT from a context that may sleep.
- Who joins. Before `irq: enabled`, boot joins the sections its allocations reach, as bring-up
  ([§2.9](INVARIANTS.md#29-preemption-and-interrupt-state) rule 1). After it, an allocation outside §4.4's atomic
  class that would take free memory below the reserve joins the next section of a node it may use
  and retries (§4.4). An atomic-class allocation never joins: the background reclaim thread
  (ROADMAP §19.10), one per node, joins a section of its node before it reclaims anything once free
  memory falls below the low watermark. A join takes no sleeping lock and allocates nothing, so a
  no-reclaim thread may join without recursing into reclaim.
- How. One compare-and-swap of the section's state, from not joined to joining, gives one caller
  the join; a caller that finds only sections being joined waits for one of those joins to finish.
  The joiner writes the section's `Frame` entries with IF=1 and no lock held, every frame marked in
  use, and marks the section joined, after which reads of the section's metadata use its entries.
  Then it frees the section's RAM into the buddy a bounded chunk per buddy-lock hold
  ([§2.9](INVARIANTS.md#29-preemption-and-interrupt-state) rule 2), skipping the metadata runs and the frames
  ROADMAP §25.3 has retired, and wakes any waiter. No free bit is set before its block is on a free
  list, so a merge never meets a half-joined buddy, and a join takes no lock but BUDDY and writes no
  page-table entry.
- Counting. The reserve and the watermarks (§4.4) count joined free frames, and R is sized from all
  RAM, joined or not. `meminfo` counts the frames of sections not yet joined as free, except those
  in metadata runs.

Rejected: joining inside the buddy under its lock, which takes PT under BUDDY against §2.1, calls up
from the buddy against §1.1 constraint 6, and writes 2 MiB of metadata with IF=0; carving each
section's metadata from the section itself, which leaves a metadata block inside every section,
needs a boot pool for sections with holes, and writes a kernel-half leaf outside PT at run time;
Linux's deferred-init threads, which join everything during boot and so write 16 GiB of metadata in
a 1 TiB guest; a joiner thread per node beside the background reclaim thread, which is a second
thread and a second threshold for one job.

Ownership. `PhysAddr` is an address: `Copy`, and it owns nothing. It carries PTE contents, DMA
addresses, and arithmetic. What owns free-list memory is `Frames`, a base and an order with private
fields, neither `Copy` nor `Clone`, and `#[must_use]`. Only `Buddy::alloc(order)` and
`Buddy::alloc_constrained` build one, and §4.6's frame metadata does when a unit's last count drops.
`Buddy::free(Frames)` is a safe fn, and `deallocate(PhysAddr, order)` is private to the `pmm` module.
Dropping a `Frames` never frees it: a free on drop would take BUDDY at whatever rank the drop site
holds, which §2.1 forbids under HEAP, SCHED, or DEVICE, and could free a frame before its TLB
invalidation. A dropped `Frames` leaks, `meminfo` counts it as leaked, and a debug build panics
naming the allocation site. `GuardedStack`, `DmaBuffer`, the `vmap` handle, and heap growth hold the
`Frames` they were built from. A frame that a page-table entry maps, a user leaf or a table page, is
consumed into that entry, which is its owner record until §4.6's frame metadata exists, and only the
page-table code that removes the entry takes it back, through an `unsafe fn` whose safety comment
names the entry.

## 4.3 Page tables

The kernel builds its own PML4 from buddy frames rather than editing Limine's. Contents at install
time:

1. Kernel image, mapped per section with correct permissions.
2. Physmap over `[0, map_end)` at the HHDM offset, using 2 MiB pages.
3. Low identity window, 512 MiB, 2 MiB pages, first 2 MiB executable.
4. The bootloader stack window, duplicated out of Limine's active tables so `_start`'s own stack keeps
   working across the `mov cr3`.

Then set `EFER.NXE` if it is not already on, load CR3, and print `paging: cr3 ok`. Immediately after,
`acpi_init` patches the physmap PTEs covering the LAPIC, I/O APIC, and HPET to PCD + PWT. An address
above `map_end` first gets fresh 4 KiB UC leaves (`map_gap`). Inside `map_end`,
`Mapper::patch_physmap_uc` marks the whole covering 2 MiB leaf UC without splitting it, so RAM that
shares the leaf becomes UC too, and its walk can skip a trailing leaf of an unaligned range (ROADMAP
§11.2, F104). Planned (ROADMAP §11.2): these devices move to `ioremap`, the physmap maps no device
memory, and this patch and `map_gap` are deleted (§4.1).

The kernel tables are reached only through `paging_init::current_mapper()`, which takes the PT lock
and returns a `MapperGuard` that holds it for as long as the guard lives and derefs to the `Mapper`
over the kernel root. No code builds an unlocked kernel `Mapper`. `with_pt` hands its closure that
guard, and the `_locked` map and unmap helpers take `&mut MapperGuard`, so a caller that holds PT
proves it by the argument it passes rather than by a comment. A shootdown waits for other CPUs, so it
runs after the guard drops (§7.9).

### PTE flag policy

| Mapping | Flags |
|---------|-------|
| Kernel `.text` | present, global, read-only, executable |
| Kernel `.rodata` | present, global, read-only, NX |
| Kernel `.data` / `.bss` | present, global, writable, NX |
| Physmap | present, global, writable, NX |
| Heap | present, global, writable, NX |
| Kernel stacks | present, global, writable, NX, guard unmapped below (§4.5) |
| MMIO | present, global, writable, NX, PCD + PWT |
| Low identity, first 2 MiB | present, global, writable, executable (trampoline) |
| Low identity, rest | present, global, writable, NX |

NX everywhere by default. The one thing that must stay executable low down is the trampoline page;
mapping the whole identity window NX is how the old tree produced a page fault during AP bring-up that
looked exactly like a hang.

The physmap covers the kernel image's frames, so it is a writable alias of `.text` and `.rodata`:
W^X holds per virtual address, not per frame (ROADMAP §18.1, F105).

MMIO gets PCD + PWT unconditionally. QEMU ignores cache attributes and write-back MMIO appears to
work, so this bug only shows up on real hardware, months later, as inexplicable device behavior.
The Limine framebuffer stays write-back on the HHDM physmap for now (QEMU-tolerant). WC/UC remap of
scanout is a later polish pass; double buffering is also parked (ROADMAP §5.1).

### TLB

- `invlpg` after any single-PTE edit, including MMIO attribute patches.
- Kernel mappings are `GLOBAL`, and every CPU sets `CR4.PGE` (`arch::cpu::init_control_regs`,
  §11.4), so they survive a CR3 reload. Unmapping one requires a shootdown on every online CPU
  before the VA or the frame behind it can be reused (§2.4). See [section 7.9](SMP.md#79-tlb-shootdown).
- A kernel-half edit runs a local `invlpg`, drops PT, then calls `paging::tlb_shootdown_others(va)`,
  a hook that `ipi_init::init` points at `ipi_init::shootdown_va` before the first AP starts (§7.9).
  Host tests, and boot before `ipi_init::init`, leave the hook unset; the local `invlpg` is enough
  there.
- Frames and page-table pages that a PTE change drops go into a per-operation gather, Linux's
  `mmu_gather` shape. The gather owns them (`Frames` or `FrameRef`, §4.2, §4.6) and releases them
  only after the invalidation round that covers the change completes. It holds at most 64 units, in
  storage on the operation's kernel stack, and never allocates. When it is full, the walk records
  its position as a virtual address, drops the page-table lock, completes a round for the units it
  holds (with §7.9's freed-tables flag when a page-table page is among them), releases them, and goes
  on from its position, rereading each PTE there. So an operation sends one round for every 64 units
  it drops, and freeing memory never needs memory (§4.4). Planned (ROADMAP §12.3): user unmaps use
  it; `kva_init::unmap_shootdown` already frees its frames after `wait_acks`.
- A page's dirty state lives in its PTEs as well as in the page. A page counts as clean only after
  every PTE that maps it has been write-protected or had its dirty bit cleared, each old dirty bit
  has been read by atomic exchange through the ROADMAP §10.3 page-table seam and folded into the
  page, and the invalidation has completed (§2.4). Writeback then writes it, and a later store either
  faults on the write-protected PTE or sets the dirty bit again. Reclaim harvests dirty bits the same
  way before it decides (§4.4). The MMU sets accessed and dirty bits in live entries: always on
  x86_64, and on aarch64 when ROADMAP §12.2 sets `TCR_EL1.HA` and `HD`, where a write through a leaf
  with `DBM` set clears its AP[2]. So every software store to a live PTE is an atomic exchange or
  compare-and-swap, and an operation that removes write permission or the mapping (`mprotect`,
  `munmap`, reclaim) folds the old dirty bit into the page as cleaning does. `DBM` is set only on a
  user leaf its process may write and that is not COW-shared, never on a kernel leaf. Planned (ROADMAP
  §12.2, §12.4, §12.6).
- Write-notify. A writable `MAP_SHARED` mapping of a file whose pages are written back to a device
  (a vibefs or FAT file, or a block device) maps a clean page read-only on both architectures,
  whatever the hardware's dirty management. The first store faults. The fault takes the page busy,
  waits for any writeback of the page when the file's data is checksummed, reserves the page's
  space with its filesystem, marks the page dirty and counts it against ROADMAP §12.5's dirty limit,
  updates the file's mtime and ctime, and only then makes the PTE writable. This is Linux's
  `page_mkwrite` path. None of these steps waits while the fault holds the address-space lock
  (§2.1): one that must wait, such as a reservation that waits for a commit to free space, runs
  after the lock is dropped, and the fault restarts. A failed reservation ends a user store with
  `SIGBUS` (§5.2) and a store inside a user-memory accessor with `EFAULT`. Cleaning such a page
  write-protects each PTE that maps it, never only clears its dirty bit, so the next store faults
  again. A shared mapping of tmpfs, which backs anonymous shared memory (§4.6), keeps dirty bits
  instead, charges tmpfs's size limit when the fault allocates a page, and never counts against the
  dirty limit, since nothing writes it back before swap; Linux leaves shmem out of write-notify for
  the same reason. Planned (ROADMAP §12.2, §12.4).

## 4.4 Kernel heap

A free-list heap at `HEAP_START`, backed by buddy frames mapped writable + NX. Initial mapping is
1 MiB; the allocator grows in page-sized increments up to the 64 MiB region limit. It never returns
a page to the buddy, so it reports the pages it holds and its bytes in use apart, and a leak check
reads the bytes in use (ROADMAP §12.1). `GlobalAlloc` takes the heap lock, a `SpinMutex`, so
interrupts are off for each `alloc` and `dealloc`, and the rank check refuses an allocation or a
free made while a spinlock ranked after the heap is held (§2.1). Planned (ROADMAP §12.6): the free
list becomes a two-level segregated-fit allocator (TLSF), whose `alloc` and `free` take constant
time with boundary-tag coalescing, and a moving `realloc` copies with the heap lock dropped, so
every HEAP hold is bounded however fragmented the heap is
([§2.9](INVARIANTS.md#29-preemption-and-interrupt-state) rule 2). Today `alloc` walks the address-ordered free
list first-fit, `dealloc` walks it to insert, and `realloc` copies under the lock.

Planned (ROADMAP §12.6): the heap region is sized at boot from installed memory, so a heap allocation
fails only when frames run out. The two limits must be one because the failure policy below treats
every failure as a shortage of memory. A fixed region smaller than RAM would fail allocations while
frames are free, and reclaim and the OOM killer would then kill processes to make room that
memory already had.

Allocation failure has two policies, chosen by when it happens:

- After `irq: enabled`, allocation is fallible on every path, not only on those that untrusted input
  reaches ([§2.10](INVARIANTS.md#210-trust-boundaries)): through `vibeos::kalloc`'s owning types, whose failure
  becomes `ENOMEM` (or the errno Linux returns there, such as `EAGAIN` from `fork`). Where the
  context may sleep ([§2.9](INVARIANTS.md#29-preemption-and-interrupt-state) rule 4), ROADMAP §12.6's direct
  reclaim and OOM killer run before the allocation reports failure. A user who exhausts memory gets
  an errno or the OOM killer's verdict, never a kernel halt. A bound on an allocation's size and
  count does not make its failure a broken invariant: any process can exhaust memory first, and the
  heap fails when frames do (ROADMAP §12.6), so after boot a failure means memory is short.
- Before `irq: enabled`, while no process exists and no device is bound, the infallible `alloc` API
  (`Box::new`, `Vec::push`, `vec!`, `format!`, `String` growth, `Arc::new`) is allowed for a size
  that no device or disk image supplies. Its failure reaches `#[alloc_error_handler]`, which panics
  with the requested layout: there, a failure means the machine has too little memory to boot or a
  kernel invariant is false, and a halt that names the layout says which. Reaching the handler after
  `irq: enabled` means an infallible call escaped the lints below.

Fallibility is carried by type, not by a list of methods. The infallible surface of `alloc` is
large: `BTreeMap::insert`, `extend`, `collect`, `clone`, `to_vec`, `String::from`, and every other
growing call. On stable Rust, which `vibeos-core` uses (§1.1), `Box`, `Arc`, and `BTreeMap` have no
fallible constructor at all. So `vibeos-core` has a `kalloc` module of owning types (`TryBox`,
`TryVec`, `TryString`, `TryArc`, and an ordered map) whose every growing operation returns `Result`.
They are built on stable Rust: `alloc::alloc::alloc` with a null check for boxes, and `try_reserve`
for vectors. `TryBox<T: ?Sized>` wraps `alloc`'s `Box`, and a trait object is made through a closure
the caller writes: `TryBox::try_new_unsize(value, |b| b)` allocates a `Box` of the value's own type,
and the closure coerces it to `Box<dyn Trait>`, a coercion stable Rust performs. `TryArc<T: ?Sized>`
builds its counted cell as such a `Box`, lets the same kind of closure coerce it, and only then
takes its raw pointer. Neither needs `CoerceUnsized`, `Unsize`, or pointer metadata, which are
unstable, and neither constructor is `unsafe`. Clippy's `disallowed-types` denies `alloc`'s owning
types (`Box`, `Vec`, `String`, `Arc`, `Rc`, and the `alloc::collections` types) in `vibeos-core` and
the kernel binary outside `kalloc`; hostlib's tools, the user runtime, and test-only code allow them
at their root with a comment that says why, and `disallowed-macros` denies `vec!` and `format!`. A
boot-time site that keeps an `alloc` type carries an `#[allow]` whose comment names the boot step
that runs it, and what it builds does not grow after `irq: enabled`. This is the shape
Rust-for-Linux settled on (`KBox`, `KVec`) after starting from `alloc`'s collections. Rejected: a
`disallowed-methods` list of infallible constructors, which misses the calls it does not name and
leaves `Box` and `Arc` with no fallible path on stable Rust.

Rule; enforced: the lints above leave `alloc`'s infallible API only to `kalloc`, boot-time sites and
test-only code, so every allocation after `irq: enabled` is fallible (ROADMAP §10.4). `fork`,
`execve`, `open`, and thread creation in `spawn_inner` return `ENOMEM`, which the in-guest
`kalloc_nomem` test checks; a driver probe whose allocation fails leaves its device unbound, which
`dev_probe_alloc_fail` checks; a kernel stack that cannot be allocated is `SpawnError::NoMemory`
(F010). Rejected: making small allocations never fail by having the allocator wait
until the OOM killer frees memory (Linux's "too small to fail"), because an allocation made with a
spinlock held, or on a path the OOM victim needs in order to exit, cannot wait, and a failed
`Box::new` cannot be handled by its caller; AGENTS.md rule 4 forbids a user-triggerable panic.

Planned (ROADMAP §12.6): one allocation entry applies the rules below. It is the `#[global_allocator]`
that `kalloc`'s types allocate through and the frame entry that the fault path, page tables, kernel
stacks, and DMA use. It is a module of its own, not `heap_init` or `pmm_init`: it calls down into
the heap and the buddy, which report failure and never reclaim, wait for memory, or call up beyond
the heap's §4.3 shootdown when it grows, and it reaches reclaim, the writeback wait, and the OOM
killer through the hooks §1.2 lists. Where ROADMAP §27.3's joining applies, it joins a memory section before
it goes below the reserve (§4.2). Today the `#[global_allocator]` is `heap_init::KernelAlloc`, and
`kva_init`, `dma_init`, and `addr_space_init` take frames from `pmm_init::with_buddy` directly.

Direct reclaim (ROADMAP §12.6) runs inside an allocation that failed, on the allocating thread, so it
must need nothing that thread might hold. The thread may hold a filesystem's inode or block-mapping
lock, its own address-space lock, or a busy page-cache page (§2.1's sleeping tier). Rules:

1. Direct reclaim runs only for an allocation that began with IF=1, this CPU's `HELD` rank mask
   empty, and a calling thread that is not a no-reclaim thread and is outside any RCU read-side
   section ([§2.12](INVARIANTS.md#212-rcu)). The allocation entry reads all four at entry, before it takes the heap
   lock. Any other allocation draws on the reserve below, as deep as its class allows, and then
   fails.
2. It frees clean pages, and from ROADMAP §19.9 on the unused slab objects that a shrinker meeting
   this rule gives up, and nothing else. It drops clean page-cache pages that nothing maps. It
   unmaps a mapped one through the reverse map, taking only each address space's page-table spinlock
   and try-locks of the page and of its reverse-map lock (§4.6), and no count on the space
   ([§2.11](INVARIANTS.md#211-object-lifetimes)): it exchanges each PTE to empty, folds each old dirty bit into
   the page, and completes the invalidation (§2.4) before it decides. A page found dirty stays in
   the cache, unmapped and dirty, for the writeback threads; a clean page's count drops. It skips
   any page it cannot take at once. It skips a page that `mlock` holds, which is off the LRU
   (ROADMAP §12.4), and a page that a device has pinned (ROADMAP §19.8). It takes a sleeping lock
   only by try-lock, which never waits, and never takes the address-space lock.
3. It writes no page. The ROADMAP §12.5 writeback threads write dirty file pages, and ROADMAP
   §12.7's swap-out thread writes anonymous pages. When a pass frees too little, direct reclaim
   wakes those threads, waits up to 100 ms for writeback progress (any page cleaned or freed), and
   reclaims again. A pass that frees nothing counts toward a limit of 16 in a row, and one that
   frees anything resets the count. After 16 the OOM killer runs, whatever writeback is still
   pending or in flight, so an allocation waits at most about 1.6 s for that decision, and a device
   that has stopped completing delays it no longer than a slow one. These are Linux's figures
   (`MAX_RECLAIM_RETRIES` and its 100 ms reclaim throttle). A thread that holds a lock writeback may
   need, a level-4 lock or a reverse-map lock (§2.1), runs the same passes, but each waits only
   until a write already submitted to a device completes or 100 ms pass, and after 16 its
   allocation fails with `ENOMEM` instead of running the OOM killer, since the writeback it would
   wait for may need that lock. The allocation entry reads the thread's count of such locks at entry, with
   rule 1's conditions; each of those locks raises the count when it is taken and lowers it when it
   is released. That wait stands outside §2.1's order, though the reclaiming thread may hold a
   block-mapping or volume lock: the completion that ends it takes no sleeping-tier lock, in the
   device's bottom half ([§5.4](INTERRUPTS.md#54-irq-registration)) or in any later completion stage, and a
   filesystem's end-of-write work that needs its own locks runs in its writeback thread after the
   completion, never in it. Any wait for further writeback progress is bounded by the deadline.
   ROADMAP §13.12's lock-dependency build gives the wait a class of its own. Memory that waits only
   for an RCU grace period counts as freeable: before the OOM killer runs, reclaim waits, with the
   same deadline, for the grace period in progress to end and for the RCU callbacks it made ready to
   run (§2.12).
4. It never recurses. These are no-reclaim threads, whose allocations draw on the reserve below,
   as deep as their class allows, and then fail: the writeback threads, the swap-out thread,
   threaded interrupt bottom halves (§5.4), the block error handlers (§10.3), a workqueue worker
   while it runs a softirq-equivalent item (§2.2), and a thread already in reclaim.

Why: writeback or an unmap that needed a lock the allocating thread holds would deadlock that thread
on itself. A try-lock never waits, so reclaim may try a page and its reverse-map lock and skip what
it cannot take. For example, a filesystem that allocates while holding its volume lock would reach
writeback of its own dirty pages. A bottom half that waited on reclaim could wait for an I/O
completion that only it can deliver. A thread inside an RCU read-side section may not sleep (§2.12),
and reclaim's wait for a grace period (rule 3) would wait on that thread itself.

Linux guards the same recursion with per-call `GFP_NOFS` and `GFP_NOIO` flags and has moved page
writeback out of direct reclaim. These rules take the second route everywhere, so no call site
carries a flag. Where a thread must not wait for writeback, rule 3 decides from the locks it holds,
as Linux's scoped `memalloc_nofs_save` does, not from a flag at the call. Rejected:
- per-call reclaim flags, the shape of Rust-for-Linux's `KBox::new(x, flags)`, which add one more
  decision to every allocation;
- writeback from direct reclaim, which needs those flags.

Cost: when most reclaimable memory is dirty, an allocation waits for writeback progress rather than
writing itself, up to 16 passes of 100 ms before the OOM killer runs, and a thread that holds a
level-4 or reverse-map lock gets `ENOMEM` after its 16 passes, where Linux retries a small
`GFP_NOFS` allocation without end. ROADMAP §12.5's dirty limit throttles writers before it comes to
that. The limit counts pages dirtied through shared file mappings as well as by `write`, since the
first store to a clean page faults (§4.3).

The reserve (ROADMAP §12.6) is R frames of the buddy's free count: a level of that count, not a
separate pool. R is sized at boot as Linux sizes `min_free_kbytes`: the square root of 16 times the
memory the buddy manages, both in KiB, clamped to Linux's 128 KiB to 256 MiB, which gives 4 MiB in
a 1 GiB guest. An allocation that takes frames from the buddy, directly or by growing the heap, goes
below R only as far as its class allows. The class comes from context, as rule 1's reclaim decision
does, never from a flag:

- General: every allocation not named below. It stops at R. One that may sleep then runs direct
  reclaim and the OOM killer (rules 1 to 3); any other fails.
- Atomic: an allocation made with IF=0, with a spinlock held, or inside an RCU read-side section
  ([§2.12](INVARIANTS.md#212-rcu)), and any allocation by a softirq-equivalent item or a threaded bottom half.
  It may go down to R/2, then fails.
- Progress: the writeback threads, the swap-out thread, a thread while it runs direct reclaim, and
  ROADMAP §19.10's background reclaim thread, whose running frees memory. It may use all of R, then
  fails.

Where two classes apply, as for a progress thread holding a spinlock, the deeper depth does.
Softirq-equivalent items and bottom halves run on their own threads (§2.2, §5.4), so none of them is
ever a progress thread. An OOM victim's threads, while they exit, may also go down to R/2, as
Linux's `ALLOC_OOM` gives a victim half of the min reserve and keeps the rest for reclaim. The OOM
reaper below is in the progress class. So a flood of network receive, which allocates in the atomic
class, can take at most half of R, and the rest stays for the threads that clean and free pages.
This is Linux's split: `GFP_ATOMIC` allocations may dip part of the way below the min watermark, and
`PF_MEMALLOC` reclaimers all the way. Each dedicated progress-class thread (the writeback threads,
the swap-out thread, the background reclaim thread, and the OOM reaper) names the most it allocates
for one unit of its work, such as one writeback pass or one reap step, or zero where it allocates
nothing, and ROADMAP §12.6 lists these bounds. R is the larger of the size above and twice the sum
of those bounds, so the half of R that no other class reaches holds one unit of work for each such
thread. R is recomputed when one starts or stops. A thread in direct reclaim is not in the sum,
because any number of threads may reclaim at once. `meminfo` shows R and, for each class, the lowest
free count one of its allocations has left since boot. Two rules keep ordinary work out of the
atomic class. A block completion allocates nothing, because what it needs was allocated at
submission (ROADMAP §12.5's owned submission); only a stage-2 item that submits more I/O allocates,
its new request, fallibly ([§10.1](BLOCK.md#101-completions)). A fault or `mmap` allocates the page-table
pages it may need before it takes the page-table spinlock, with reclaim allowed, and frees those it
did not use, as Linux's `pte_alloc` does. Rejected: two pools with a refill order between them,
which adds machinery and leaves open which pool a progress thread holding a spinlock uses.

Planned (ROADMAP §27.3): while a node has sections not yet joined
([§4.2](#42-physical-memory-buddy-allocator)), joining one comes before the reserve. An allocation
outside the atomic class that would take free memory below R joins a section of a node it may use
and retries, so it goes below R, and direct reclaim and the OOM killer run, only once no section on
those nodes is left to join. The atomic class never joins; the background reclaim thread joins for
it.

The OOM killer (ROADMAP §12.6) chooses among the user processes of one scope, the machine and later
a cgroup, and never chooses pid 1, whose exit panics the kernel (ROADMAP §10.5), or a kernel thread.
A scope has at most one victim at a time: while that victim's memory is still to be released, the
killer chooses no other, and the allocation that ran it waits, with a deadline, then tries once
more. An OOM reaper thread unmaps the victim's private memory without waiting for it to exit, taking
the lock on the victim's region table only by try-lock, so a victim stuck in an uninterruptible wait
still gives its memory back. With no eligible process the allocation fails with `ENOMEM`, and a user
fault is retried. Linux panics when nothing is killable; here nothing on this path panics (AGENTS.md
rule 4).

Where reclaim is not allowed, a failed allocation must not lose an obligation. An allocation that
may not reclaim (rule 1) can fail at any time, driven by input alone, and its caller keeps what it
owes without it:

- Received data that cannot be buffered is dropped and counted, never half-processed. A TCP segment
  dropped this way is answered with an acknowledgement that advertises a zero window, as Linux does,
  so the sender probes instead of backing off its retransmission timer, and the window update after
  the reader drains restarts it at once.
- A driver that cannot refill a receive ring from its bottom half queues a refill item on an
  ordinary workqueue worker, which allocates with reclaim and retries with backoff until the ring is
  back above its low mark. A ring below that mark always has a refill pending, since an empty ring
  raises no interrupt that would retry. This is Linux virtio-net's `refill_work`.
- A timer callback that cannot allocate what a pending obligation needs re-arms itself instead of
  returning with nothing armed: after 500 ms for a retransmission, a zero-window probe, or a
  keepalive (Linux's `TCP_RESOURCE_PROBE_INTERVAL`), and after 200 ms for an acknowledgement
  (`TCP_DELACK_MAX`).
- Received data that a protocol has acknowledged cumulatively, and sent data not yet acknowledged,
  are never freed to relieve memory. Under pressure a receive queue is collapsed into fewer, fuller
  buffers, and the out-of-order queue, which no cumulative acknowledgement covers, may be pruned,
  reneging any block it had selectively acknowledged, as RFC 2018 §8 allows. These are Linux's
  `tcp_collapse` and `tcp_prune_ofo_queue`.

Why: each obligation has one owner, and nothing else retries it. An empty receive ring raises no
interrupt, so a refill left for the next interrupt never runs, and the queue, with every flow its
hash sends there, is dead until reboot. A retransmit timer is its connection's only liveness once
the peer's acknowledgement is lost. Freeing acknowledged data hands the reader a stream with a hole
and no error. Rejected: a reserve large enough that these allocations never fail, since untrusted
input sets the demand; a refill only at the next interrupt; and reclaim in bottom halves, which
rule 4 forbids.

An operation past its point of no return cannot unwind what it built, so it makes every allocation it
needs before that point and only releases after it:

- A filesystem commit allocates its blocks, and the memory its switch to the new generation uses,
  before it starts the superblock write ([VIBEFS.md](VIBEFS.md) §10). Once the superblock is
  durable, memory must switch to the new generation, and a switch that fails halfway for want of
  memory leaves memory matching neither generation.
- `execve` builds the new address space, its stack and arguments included, before it swaps it in, and
  after the swap only releases: close-on-exec descriptors and the old address space. After the swap
  there is no old image to return an errno to. Rejected: Linux's order, which allocates past its
  point of no return and kills the process with `SIGSEGV` when that fails; building first costs
  holding both images' page tables until the swap. `docs/LINUX.md` lists the difference
  (`execve-late-errno`).
- Exit has no caller to return an errno to, so its point of no return is its first release. Thread
  and process exit, an OOM victim's included, and the reap of a zombie by `wait4` allocate nothing
  from there on. What they need, such as a zombie's exit status and the `SIGCHLD` its parent gets,
  lives in memory allocated when the process or thread was created, where a failure made `fork` or
  `clone` return an errno. An exiting thread's puts go through `put_deferred`
  ([§2.11](INVARIANTS.md#211-object-lifetimes) rule 6), so a release that needs memory, such as freeing an
  unlinked file's blocks at its last close, runs on a workqueue worker. Rejected: a reserve set
  aside at boot for each teardown path, in the shape of Linux's mempool, whose bound would scale
  with the process table.

A path that frees memory does not need memory to finish. `munmap` and a `MAP_FIXED` replacement
make the allocations they may need, for a region split and the new region, before they change their
first PTE; such an allocation may fail with `ENOMEM`, as on Linux, and leaves the mappings as they
were. From its first PTE change on, a freeing path completes even if every allocation fails, because
it collects what it drops in §4.3's gather, which never allocates. Exit and `execve` teardown,
truncate's unmap, the OOM reaper, and reclaim's reverse-map unmap allocate nothing at all. Rejected:
sizing the collection before the walk, since only the walk finds how much it drops, and for exit
that is the resident memory that has run short; and letting the gather grow by allocations that
may fail, as Linux's `mmu_gather` does, since under the page-table spinlock such an allocation is in
the atomic class above and would draw on the reserve that the threads freeing memory need.

A kernel thread or work item has no caller either, a release deferred to one included. When one of
its allocations fails, it records the error where a later call reports it, retries with a named
bound, or drops that work with a counter and a log line at most once a second, as
[§2.5](INVARIANTS.md#25-panic-policy)'s rule that nothing is silently swallowed requires. A driver probe that
fails leaves its device unbound and logs why. A CPU whose idle thread or workers cannot be allocated
stays offline (ROADMAP §10.4, F037).

A slab allocator for hot object types (TCBs, file descriptors, inodes, network buffers) lands in
ROADMAP §19.9; general allocation stays on the heap, which ROADMAP §12.6 makes a constant-time TLSF
allocator, and slab does not replace it. A typed cache has no constructor: an object is initialized
on allocation and dropped on free. It is an allocator for `kalloc`'s owning types, which gain an
allocator parameter defaulting to the heap, not a second family of owning types. Direct reclaim
runs only the shrinkers that take no sleeping lock except by try-lock (rule 2); one that must wait
for a lock runs from ROADMAP §19.10's background reclaim thread.

Planned (ROADMAP §12.1): one set of allocation hooks for the sanitizer builds. Every kernel allocator
(the buddy, the heap, the KVA allocator, and ROADMAP §19.9's slab) calls the same hooks when it hands
memory out, with the span the caller may use, and when it takes memory back, and it holds freed
memory back from reuse for as long as the hooks' quarantine asks. The KASAN build marks red zones
and freed memory in its shadow, the `debug_mm` build fills freed memory with poison and checks it on
reuse, and the MTE build (ROADMAP §18.4) tags each allocation and retags it on free; in the default
build the hooks are empty. A new allocator calls them in the commit that adds it. Why: a sanitizer
sees only what an allocator reports to it, and with one set of hooks an allocator that lands after a
sanitizer build, or a build that lands after an allocator, needs no change to the other, whatever
order ROADMAP Phases 18 and 19 close in. Rejected: each sanitizer build patching each allocator,
which leaves an allocator that lands later, such as the slab after the KASAN build, invisible to it.

## 4.5 Kernel virtual address allocator

The heap answers "give me 40 bytes". The KVA allocator answers "give me 16 KiB of contiguous virtual
address space with an unmapped guard below it". Guarded kernel stacks are the motivating case, `vmap`
is the second.

- A guarded stack is a power-of-two size *S* and starts at an address aligned to *2S*, and the *S*
  bytes of VA below it stay unmapped, so overflow takes a page fault instead of quietly eating
  whatever is below. Every address in the stack then has bit log2(*S*) clear and every address in its
  guard has it set. aarch64 relies on that: an exception taken at the kernel's level runs on the stack
  that overflowed, so each vector entry tests the bit and moves to a per-CPU overflow stack when it is
  set ([§11.5](PORTABILITY.md#115-aarch64-exceptions-and-privilege-transitions) rule 6), which needs every stack the
  aarch64 kernel runs on (thread, idle, and overflow stacks) to have one size, 16 KiB. x86_64 needs no
  test, since `#DF` switches to its IST stack (§5.1), and its `#DF` handler reports stack overflow
  when CR2 lies in the guard of the stack the interrupted code ran on; it uses the same layout, so
  stack allocation has one path. Rule; not yet enforced: ROADMAP §11.3. Today a guarded stack of *n*
  pages reserves *n+1* pages of VA, maps the upper *n*, and has no alignment.
- A `GuardedStack` (`vibeos::thread::GuardedStack`, re-exported as `kva_init::GuardedStack`) is a
  move-only handle with private fields; only `kva_init::alloc_guarded_stack` builds one, and
  `free_stack` takes it by value.
- `kva_init::vmap(Frames) -> Result<Vmap, KvaError>` maps one buddy block of at most 32 frames
  (`MAX_UNMAP`) contiguously and refuses a larger one with `KvaError::Size`; on any failure it returns
  the frames to the buddy. The `Vmap` is a move-only handle with private fields (`base()`, `len()`)
  that holds the block's `Frames` (§4.2). `vunmap(Vmap) -> Frames` unmaps, shoots down, and frees
  exactly the span the handle records, then hands back the `Frames`, so the span unmapped always equals
  the span returned to the free list. Dropping a `Vmap` leaks its span and its `Frames`. A
  compile-time assertion in `kva_init` fails the build if `Vmap` is `Copy` or `Clone`.
- Stack frames are allocated as *n* separate order-0 frames, not one order-*k* block. Stacks do not
  need physical contiguity and requesting it fragments the buddy allocator for nothing. The
  `GuardedStack` holds each frame's `Frames` (§4.2), and `free_stack` frees those tokens after the
  shootdown, never whatever the page-table entries name.
- A freed VA range returns to the free list only after its shootdown completes (§2.4;
  `kva_init::unmap_shootdown`). Freed ranges go to the tail of the free list, so a stale pointer into
  one keeps faulting for as long as possible instead of reaching the range's next owner; that is a
  debugging aid, not the ordering.
- Freeing the stack you are running on does not work. Rule: a dead thread's stack is freed only
  after the CPU that ran it has switched off it (§2.8, invariant I10). `thread_exit` parks the
  stack in its CPU's `PerCpu.dead_stack` slot with IF=0, and `thread_init::finish_switch`, the
  switch tail that runs on that CPU after every `switch_context` (both `schedule_inner` paths, the
  preempt one included, `switch_to`, and a new thread's `trampoline`), empties the slot, so it
  holds at most one stack. The tail never unmaps, allocates, or sends a shootdown: a default-size
  stack goes into the CPU's stack cache of at most two (`per_cpu::StackCache`, Linux's
  `NR_CACHED_STACKS`), which the next spawn on that CPU reuses zeroed and still mapped; any other
  stack goes on the CPU's dead list, linked through the dead stacks themselves
  (`kva_init::park_on_list`), and that CPU's workqueue worker unmaps and frees it with IF=1
  (`kva_init::free_parked`). No other CPU reaches the slot, the cache, or the list. Frame counts
  (`ktest::free_frames`) count cached stacks as free until ROADMAP §12.1's categories. A CPU going
  offline (ROADMAP §19.6) frees its cache.

Default kernel stack is 4 pages (16 KiB) plus its 16 KiB guard. Budget: the deepest use observed on
a kernel stack, interrupts that landed on it included, stays at or below the stack's size minus
4 KiB, which is 12 KiB of a 16 KiB thread stack and 60 KiB of a 64 KiB one. The 4 KiB is the margin
for a hard-IRQ entry frame and top half (§2.2) that no run happened to land at the deepest point.
The size goes up only when a measurement names the path that needs more, and that path and its depth
are recorded here in the same commit; on aarch64 the size is one constant for every stack, and the
entry test's bit follows it. If the measurement shows a top half with its entry frame above 4 KiB,
per-CPU interrupt stacks, as Linux has, come before a larger thread stack.

Why: an overflow hits the guard page and halts the kernel (on x86_64 `#PF` has no IST stack, so the
fault becomes `#DF`; on aarch64 the entry test reports it from the overflow stack), so an overflow
on a path ring 3 drives breaks AGENTS.md rule 3. A path's depth is a sum of frames, a top half's
included, which no per-function bound sees. Rejected: raising the size when a path turns out to be
tight, which nobody learns until the overflow halts the kernel; a static whole-call-graph bound,
which loses the path at every indirect call (`dyn InodeOps`, IRQ handler tables, function pointers);
and 8-page stacks everywhere, 16 MiB at the 1024-thread Phase 10 limit, which hide the regressions a
measurement shows. Rule; not yet enforced: nothing measures stack depth (ROADMAP §10.2).

## 4.6 What comes later

The roadmap covers these in detail. Listed here so the interfaces above are designed with them in
mind:

- Demand paging (ROADMAP §12.2). `map_page` gains a "reserve VA, populate on fault" mode, and `#PF`
  becomes a recoverable exception with a real fault handler rather than a halt.
- Copy on write (ROADMAP §12.3). `fork` clones an address space by sharing frames read-only with a
  refcount; the write fault does the copy. Needs the frame metadata below (ROADMAP §12.1).
- Slab caches with per-CPU magazines, so hot object types avoid the global heap lock (ROADMAP §19.9);
  per-CPU free-frame lists do the same for the buddy lock (ROADMAP §27.3).
- One page cache of mappings (§10.6), which serves `mmap`, file I/O, and the block layer, and whose
  pages share one LRU with anonymous pages (ROADMAP §12.5).
- Swap, which finds every PTE mapping an anonymous page through the reverse map below (ROADMAP
  §12.7).

The per-frame metadata array is the pivot. Refcounting, reverse mapping, and page cache all need it,
so the PMM should be built expecting it to appear.

Frame model (ROADMAP §12.1). The metadata is kept per allocation unit: a naturally aligned block of
2^k frames from one buddy allocation, which Linux calls a folio. The unit's head `Frame` holds its
count, pin count, flags, owner, index, and LRU link; each tail `Frame` names its head and keeps only
per-frame flags, such as ROADMAP §25.3's poison bit; every `Frame` fits 64 bytes. Units are order 0
until ROADMAP §27.4 adds huge pages, so that phase changes no counting, reverse-map, page-cache, or
pin rule. `FrameRef` is one counted reference to a unit. It is not `Copy`; its `try_clone`
saturates, as Linux's `refcount_t` does, and never wraps; the last `put` hands the unit back as a
`Frames` (§4.2) to be freed after its TLB invalidation, and nothing frees implicitly.

- Every present user PTE holds one count on its unit, whatever the backing: anonymous, COW-shared,
  or page cache. The page cache holds one count of its own, and a pin (ROADMAP §19.8) holds one. A
  unit returns to the buddy only when its count reaches zero.
- A kernel-owned unit (the shared zero page, and later the vDSO pages, packet rings, and dumb
  buffers) carries a kernel-owned flag and takes no PTE count. Teardown recognizes the flag and
  drops only the mapping, as Linux's special zero-page PTEs do; counting them would put every CPU's
  demand-zero read fault on one contended atomic.
- A write fault reuses a private page in place only when the page is anonymous and its count is 1,
  so this PTE is its only holder. A file page mapped privately is always copied, since the cache's
  own count keeps it above 1. ROADMAP §19.8 adds that a pinned anonymous page, which is always
  exclusive to one address space, is reused although its pins raise its count.
- A unit splits (ROADMAP §27.4) only after it is unmapped everywhere through the reverse map, with
  migration entries in place of its PTEs, and its count then freezes at the cache's reference plus
  the caller's. A higher count means another holder, so the split remaps the unit and fails, and the
  caller falls back to 4 KiB handling. The unit keeps no map count; ROADMAP §23.4 adds one for
  `smaps`.

Reverse map (ROADMAP §12.1). The map is by object, not by PTE. Each unit's owner and index (the
frame model above) name where its page lives: a file page's owner is its file's mapping (§10.6) and
its index the file page index; an anonymous page's owner is the anonymous object of the region it
was first faulted in, and its index its page index in that object. A file mapping keeps an interval
tree of the regions that map it, and an anonymous object keeps a list of the regions that may map
its pages. A region records its page offset in its object, kept across `mremap` and a split, so a
page's address in a region is the region's start plus (index − offset) × 4 KiB. A region's list and
tree nodes live in the region, so linking allocates nothing; a region's first anonymous fault
allocates its anonymous object, fallibly, before it takes the page-table lock.

- A shared anonymous region (`MAP_SHARED | MAP_ANONYMOUS`) is backed by an unlinked tmpfs file, as
  Linux's shmem is. Its pages are file pages to the reverse map and to the fault path.
- A walk read-locks the object's reverse-map lock and visits its regions in the object's order. For
  each, it takes that address space's page-table spinlock, finds the PTE by address, acts, and drops
  the lock, so a walk holds at most one page-table lock. A walker takes no count. It holds the
  object's reverse-map lock for reading while it borrows each region's core and takes that core's
  page-table lock. Unlinking needs the lock for writing, so a region the walker can see has live
  page tables and a live core.
- The reverse-map lock is a sleeping `RwLock`, one per file mapping and one per anonymous object, at
  §2.1's level 3b. A walk visits as many regions as programs create, so a spinlock would hold IF=0
  for a time nothing bounds (§2.9 rule 2). Direct reclaim takes it only by try-lock and skips the
  page when that fails (§4.4 rule 2).
- A region made from another (`fork`'s copy, an `mremap` destination, a split) is placed after its
  source in the object's order, so a walk that has passed the source also reaches the copy after the
  PTE is copied or moved. When `mremap` cannot keep that order, the PTE move holds each object's
  reverse-map lock for writing, as Linux's `move_ptes` does under `need_rmap_locks`.
- A linked region's start, end, and offset change only while the write lock of every object it is
  linked into is held, since a walker reads them without the address-space lock. A private file
  region with anonymous pages is linked into two objects; the file mapping's lock is taken before
  the anonymous object's, as Linux takes `i_mmap_rwsem` before the `anon_vma` lock.
- `fork` links each child region into its objects, right after its parent region, before it copies
  any PTE, then copies holding the parent's page-table lock and then the child's. It is the one path
  that holds two page-table locks, which is safe because a walk holds one. It copies no PTE of a
  shared file region, a shared anonymous region, or a private file region with no anonymous page;
  the child faults those pages in from their mapping, as on Linux, so `fork`'s time goes to
  anonymous memory.
- `munmap` and exit zap a region's PTEs through the gather (§4.3), then unlink the region under each
  object's write lock.

Known limit: a region `fork` copies from a parent region joins the parent's anonymous object, so a
long-lived parent with many children makes that object's list long, and a walk of any page in it, a
child's private copy included, visits every child. That is Linux's `anon_vma` before 2.6.34, which
then added `anon_vma_chain` to bound walks. ROADMAP §19.10 records the regions each walk visits and
adopts the chained design when the 99th percentile passes 64.

## 4.7 DMA

`DmaBuffer` is physically contiguous: one buddy block whose order `DmaAlloc::order` picks from the
size, the alignment, and an optional power-of-two boundary the buffer must not cross, refusing a size
above the boundary. `DmaBuffer` is a move-only handle with private fields; only
`dma::alloc_from_buddy`, which `dma_init::alloc` calls, builds one, and `dma_init::free` takes it by
value. It holds the `Frames` that `alloc_constrained` returns (§4.2), with `max_phys = u64::MAX`,
since no address limit applies until ROADMAP §20.6. A boundary is not an address limit:
`DmaAlloc::dma32` sets a 4 GiB boundary, so its buffer never crosses a 4 GiB line, but the buffer can
lie above 4 GiB once RAM extends there, and no allocator keeps a 32-bit device's buffer below 4 GiB
(ROADMAP §20.6, F030). The device-visible address is `dma_to_device(phys)` (identity until an IOMMU
exists), never a physmap virtual address. A kernel never assumes that DMA is stopped when it
starts. After a planned kexec, Bus Master Enable is clear on every PCI function (ROADMAP §25.4); a
capture kernel entered from a crash clears it on every function, and aborts SMMUv3 streams, before
it touches a device, routes an interrupt, or turns off an IOMMU translation it found enabled.
Clearing Bus Master Enable also stops a device's MSIs, which are memory writes.

`sync_for_device` / `sync_for_cpu` always run at the API boundary. They and `dma::dma_wmb` /
`dma_rmb` are generic over the port's `Barriers` (§11.1) and call its methods. On x86_64, in
`src/arch/x86_64/mod.rs`, `dma_wmb` is `fence(Release)` + `sfence` and `dma_rmb` is `fence(Acquire)` +
`lfence`. Descriptor publish stores the index after that store-side barrier, not a bare
`compiler_fence`.

Neither barrier orders a store before a later load from another address. After the driver stores
`avail.idx`, it loads `avail_event` (EVENT_IDX) or `used.flags` to decide whether to kick; virtio 1.2
§2.7.13.4.1 requires a full barrier (`mfence`) between the two, and `SplitQueue::get_used` needs one
after its `used_event` store. Without them the driver and the device can each miss the other's
update and the queue stops. `SplitQueue::should_kick` and `get_used` have neither (ROADMAP §10.3,
F016).

Device ordering, on both architectures. An MMIO write through the §11.1 accessors is ordered after
every earlier store to memory, so a driver that stores descriptors and an index and then writes a
doorbell adds no barrier, as Linux's `writel` promises. An MMIO read completes before any later load
from memory, so a status read and then a buffer read see the buffer the status describes, as `readl`
promises. On x86_64 each accessor is a volatile access with a compiler barrier, since an uncached
access is not reordered with earlier stores or later loads. On aarch64, where a Device-nGnRE store
can be observed before an earlier Normal store, `mmio_write` runs `dmb oshst` before its store and
`mmio_read` runs `dmb oshld` after its load, as Linux's arm64 `writel` and `readl` do. Neither
orders an earlier load before a device write: a driver that reads a buffer and then writes a
doorbell that hands the buffer back runs `dma_mb` first. A `_relaxed` accessor carries no barrier
and is used only where a comment says why no ordering is needed. Rule; not yet enforced: drivers
write MMIO with `write_volatile` directly, and the accessors arrive with ROADMAP §10.3's seam and
§11.2.

Planned (ROADMAP §11.2): DMA coherence is a property of each device, from firmware: the device-tree
`dma-coherent` property on the device or a parent bus (ROADMAP §11.5), or ACPI `_CCA` and the IORT
node's coherency attribute (ROADMAP §20.7). A device with neither is non-coherent, as Linux treats
it. The device's registry entry records it ([§12.1](DEVICES.md#121-devices)). For a coherent device,
`sync_for_device` and `sync_for_cpu` are `dma_wmb` and `dma_rmb`. For a non-coherent device,
`sync_for_device` cleans the buffer to the Point of Coherency (`dc cvac` for each line, then
`dsb sy`) whatever the transfer's direction, so no dirty line is written back later over what the
device writes, and `sync_for_cpu` invalidates it (`dc ivac` for each line, then `dsb sy`) before the
CPU reads what the device wrote, as Linux's arm64 DMA sync does. Maintenance works on whole cache
lines, so a non-coherent device's DMA region shares no cache writeback granule (`CTR_EL0.CWG`; 2
KiB, the architectural maximum, when it reads 0) with other data: a `DmaBuffer` is whole pages, a
region smaller than a page is aligned to and sized in granules, and boot asserts that the granule is
no larger than a page. Every device x86_64 drives is coherent, so its sync calls stay the barriers
above.

Why: on a weakly ordered CPU a doorbell can overtake the index it announces, and a completion read
can overtake the status read that announced it; ordering in the accessors means no driver works it
out again, the class of bug F016 was. Coherence is per device because QEMU's `virt` and servers
snoop the CPU caches while boards mark devices one by one, and a board tree that omits
`dma-coherent` needs the maintenance. Rejected: relaxed accessors with barriers at each call site;
`dmb osh` in every `mmio_write`, which orders earlier loads too, at the cost of a full barrier on
every device write for the few hand-back paths that need it; treating every aarch64 device as
non-coherent (maintenance on every transfer where the hardware snoops); and mapping non-coherent
buffers non-cacheable (slower CPU access, and an attribute that disagrees with the physmap's
cacheable alias, which ROADMAP §11.1 forbids for MMIO for the same reason).

## 4.8 Firmware runtime services

Planned (ROADMAP §20.9). UEFI's runtime services (variables, time, reset) are firmware code the
kernel calls after boot.

- One kernel thread, `efi_rt`, makes every call, one at a time, holding the runtime-services lock
  for each. A caller queues a request and sleeps on its completion, so no call runs in a caller's
  context or address space.
- `efi_rt`'s address space maps the runtime code, data, and MMIO regions of `BootInfo`'s EFI memory
  map at their physical addresses, beside the kernel half. The kernel never calls
  `SetVirtualAddressMap`: every call is a physical-mode call through that 1:1 map, so a kernel
  started by kexec (ROADMAP §25.4, §30.6) calls firmware as the first did, and no firmware virtual
  layout crosses a handover. A runtime region the lower half cannot hold leaves runtime services
  off, with a line that says so.
- A call runs with IF=1, so the tick, IPIs, and shootdown acknowledgements are taken while firmware
  runs, and §2.9 needs no new reason for IF=0. The scheduler does not switch away from `efi_rt`
  until the call returns, so firmware sees only the pauses an interrupt makes, and its FP and SIMD
  use needs no per-thread state: before the call the live user state is saved and this CPU's FP
  owner emptied ([§7.5](SMP.md#75-per-cpu-data)).
- While a call runs, the wrapper keeps a per-CPU firmware record: the service, the requesting
  thread, and its own frame pointer, stack pointer, and return address, saved before entry and
  cleared after return. The panic stop (§2.5 step 1), the NMI backtrace, and the core tool (ROADMAP
  §10.7, §25.5) read it: a CPU whose record is set, or whose interrupted PC lies in a runtime
  region, is reported `in firmware: <service>` and walked from the saved frame, never from
  firmware's frame pointer, and one the stop does not reach while its record is set is
  `not stopped (in firmware)`, which tells a wait in SMM from a kernel spin.
- Only the panic path calls firmware outside `efi_rt`: on the panicking CPU, with IF=0, through the
  same map, and only if its trylock of the runtime-services lock succeeds (§2.5; ROADMAP §25.6). It
  sets the record too.

Why: a runtime call has no time bound (a `SetVariable` may erase flash, through SMM in `q35`'s
Secure Boot OVMF), so a call with IF=0 would break §2.9 rule 2, hold off the tick and shootdown
acknowledgements, and trip ROADMAP §25.5's lockup detector. UEFI allows an interrupt during a
runtime call, and Linux makes its calls with interrupts on from one worker thread and does not
preempt a call. `SetVirtualAddressMap` can be called once per boot, so using it would tie every
kexec and live update to carrying the firmware's virtual layout across kernel versions; FreeBSD's
`efirt` likewise calls through a 1:1 map of the runtime regions. Rejected: calls with IF=0; a
preemptible call, which would make firmware's FP state and address space per-thread state and give
firmware pauses no interrupt makes; calls from the caller's own context, which would switch the
caller's page table and FP state in place; `SetVirtualAddressMap` with a fixed layout passed across
kexec, as Linux does on x86_64; and unwinding firmware frames, which carry no unwind data the kernel
reads.
