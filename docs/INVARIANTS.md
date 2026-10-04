# 2. Invariants

Index: [DESIGN.md](DESIGN.md). This file holds DESIGN §2, and its headings keep DESIGN's numbers.

Break these and the failure shows up somewhere else, hours later.

## 2.1 Lock order

Acquire in this order, release in reverse. Never take a lower number while holding a higher one, and
take a second lock of a number already held only through §2.3's nested acquire.

1. heap
2. page tables: the kernel's lock, and from ROADMAP §12.1 one lock per address space
3. physical allocator (buddy)
4. scheduler (also: wait-queue lists and blocking-primitive predicates)
5. device / driver locks
6. serial

Serial is last so any lock holder can still log. Heap is first because growing it takes PT and then
BUDDY, and nothing allocates from or frees to the heap while holding either. Page tables come before
the buddy because mapping a page allocates its table frames. Blocking `WaitQueue`s are serialized by
the scheduler lock: the predicate check and the enqueue happen under that same lock (DESIGN
[§9.4](PITFALLS.md#94-concurrency)).

Planned (ROADMAP §13.3): a seventh rank, SOCK, ahead of all six, for socket state (below). Planned
(ROADMAP §19.4): a TIMER rank just before SERIAL, for the per-CPU timer bases
([§6.5](TIME.md#65-timers-and-timeouts)), so code under any other spinlock may arm, re-arm, or cancel a
timer.

The six ranks order spinlocks: every `SpinMutex`, and so every lock more than one CPU takes but the
log ring's (§2.3). Sleeping locks form a tier outside all six: `BlockingMutex`, `RwLock`,
`Semaphore`, and waiting for a page or buffer to finish I/O. A thread takes a sleeping lock only with
IF=1 and no spinlock held ([§2.9](#29-preemption-and-interrupt-state) rule 4), so every sleeping lock
ranks before every spin rank, and no spinlock is ever held across a sleep. The per-device lock
(`dev_init::DEV_LOCKS`, [DEVICES.md §12.1](DEVICES.md#121-devices) rule 3) is the outermost sleeping lock: it is
taken with no other lock held and held across a driver's probe and remove. Within the rest of the
sleeping tier, outermost first:

1. the locks a call may hold across a copy to or from user memory, outermost first: an open file
   description's position lock; then a stream's lock (a pipe's lock, a socket's owner lock, a TTY's
   read lock or write lock); then the filesystem namespace and inode locks: the mount table, then a
   directory, then an inode in it; a parent directory before its child. A `rename` takes the
   volume's rename lock, then its two directories: an ancestor before its descendant, and two
   directories neither of which contains the other in address order
2. the address-space lock (ROADMAP §13.1: `mmap`, `munmap`, and `mprotect` take it for writing, the
   fault path for reading). It guards the region tree only. A page-table entry changes under its
   space's page-table spinlock (the PT rank above), so the reverse-map unmap that direct reclaim
   does ([§4.4](MEMORY.md#44-kernel-heap)) takes no address-space lock; it takes the reverse-map lock
   (level 3b) only by try-lock. The fault path never waits while it holds it (below)
3. waits on a page-cache page, in a file's mapping or a block device's
   ([§10.6](BLOCK.md#106-block-cache); ROADMAP §12.5)

   3b. the reverse-map lock of a file mapping or an anonymous object ([§4.6](MEMORY.md#46-what-comes-later)),
   a sleeping `RwLock`. A walk that finds every PTE mapping a page takes it for reading, possibly
   with that page held busy; linking or unlinking a region, or changing a linked region's range,
   takes it for writing, possibly under the address-space lock. No level-4 lock is held when it is
   taken: writeback's clean step walks before writeback takes the filesystem's locks. Code holding
   it takes no lock of levels 1 to 4 and no second reverse-map lock, except that for a region linked
   into two objects the file mapping's is taken before the anonymous object's; spinlocks may follow
   it, as after any sleeping lock. It is numbered 3b so that level 4 keeps its number.
4. a filesystem's block-mapping and volume I/O locks, which its page-fill and writeback paths take
   and which are never held across a user copy. Writeback and a filesystem commit take no lock of
   levels 1 and 2 and take a page busy only by try-lock, coming back later for a page they find
   busy, so a thread that holds an inode lock, its address-space lock, or a busy page can always
   wait for writeback progress ([§4.4](MEMORY.md#44-kernel-heap) rule 3). A thread that has joined an open
   vibefs v2 transaction, which a commit waits for, holds it as a level-4 lock
   ([VIBEFS.md](VIBEFS.md) §15; ROADMAP §14.8)

A copy through a faulting accessor (§5.1) may fault, and the fault path takes the address-space lock
for reading, then page waits, then, to fill a file page, the filesystem's level-4 locks. So such a
copy is allowed while level-1 locks are held, as `write` needs, and under no lock of levels 2 to 4,
level 3b included. The reverse is forbidden: code that holds the address-space lock takes no level-1
lock. A non-faulting accessor (§5.1) takes no fault path and no lock, so it may run under any of
them.

The position lock serializes `read`, `readv`, `write`, `writev`, `lseek`, and `getdents64` on one
open file description of a regular file or directory, so threads and processes that share the
description through `CLONE_FILES`, `fork`, or `dup` each see a whole offset update (Linux's
`f_pos_lock`). Every such call takes it. `pread64` and `pwrite64` take none, and neither does a
description of a pipe, socket, TTY, or character device, whose I/O does not use the offset. The
file's size belongs to its inode and changes under the inode lock. A stream's lock guards the
stream's buffer and is held across its user copy, never across a wait for a party that needs it: a
pipe's lock and a socket's owner lock, which readers and writers both take, are dropped before the
caller sleeps on the stream's wait queue, while a TTY's read lock, which only readers take, may be
held while a reader waits for input (Linux's `atomic_read_lock`). So a `read` blocked on a shared
pipe, socket, or TTY never holds off a `write` through the same description. Two stream locks nest
only in address order, under a ROADMAP §13.12 subclass, as a pipe-to-pipe `splice` needs. A TTY's
input queue, its line and echo state, and the termios settings its input side reads are under a
spinlock, because the line discipline runs in the input device's bottom half
([§5.4](INTERRUPTS.md#54-irq-registration)), which takes no sleeping-tier lock. Session, process-group, and
process-table state is under spinlocks (the process table is a ranked `SpinMutex`, above),
which a holder of any level-1 lock may take.

A buffered `write` whose user buffer maps the very page it writes would fault on that page while it
holds the page busy (level 3) and wait on itself. So the write path copies into a busy page only
through a non-faulting accessor (§5.1), which returns a short count instead of taking the fault. On
a short count it releases the page, faults the rest of the source in with no page held, and retries.
This is Linux's `fault_in_iov_iter_readable` loop. A `read` into a buffer that maps the page it
reads needs no such loop, because it copies from a page that is up to date and not busy.

The fault path meets these levels in that order, but from ROADMAP §13.1 it never waits while it
holds the address-space lock. It holds the lock for reading only to find the region and to install
the PTE, and takes a page busy only by try-lock. When the page is busy, not yet up to date, or under
a writeback that a store must wait for, the fault takes a counted reference to the page, drops the
address-space lock, and then waits for the page or fills it, taking the filesystem's level-4 locks
with no level-2 lock held. It then drops the reference and restarts from the region lookup, since
the region may have been split, moved, or unmapped meanwhile. This is Linux's `VM_FAULT_RETRY`.
Within one attempt the fault still installs only against the PTE it read (ROADMAP §12.2). An
allocation with reclaim may run under the lock, because reclaim takes a sleeping lock only by
try-lock and bounds each of its waits ([§4.4](MEMORY.md#44-kernel-heap) rule 3). So a fault that waits for a
disk holds up no `mmap`, `munmap`, `mprotect`, or `fork` in its process, no fault queued behind
them, and no OOM reaper's try-lock.

Every wait on the fault path ends early when the thread's process has a fatal signal pending: the
address-space lock's acquire, a page wait, the fill's acquire of level-4 locks and its wait for the
page's read, direct reclaim's wait for writeback, and the OOM killer's wait for its victim.
`BlockingMutex` and `RwLock` gain killable acquires for this; they are not new lock types (AGENTS.md
rule 10). A read whose waiter leaves keeps running, and its completion finishes the fill and
releases the page. A user fault then returns to the signal, and a fault inside a user-memory
accessor takes the exception-table fixup. The buffered `write` loop above checks for a fatal signal
on each pass and returns the bytes written so far, or `EINTR` if there are none, and `fork` checks
between the regions it copies and again before it makes the child runnable, and fails if one is
pending, as Linux's `dup_mmap` and `copy_process` do, so an OOM victim cannot finish cloning itself.
Other sleeping locks and waits stay uninterruptible, as most of Linux's are. Planned (ROADMAP §12.6,
§13.1).

The rename order is Linux's too: `rmdir` holds a parent and then the child it removes, so a
`rename` whose directories were an ancestor and its descendant, taken in address order, could hold
the child and wait for the parent. The rename lock serializes cross-directory renames, which is why
unrelated directories may go in any fixed order. `mmap` of a file takes a counted reference to the file's
page-cache object before it takes the address-space lock, never the inode lock inside it. A region's
references go the other way: they are dropped after the address-space lock is released. `munmap`, a
`MAP_FIXED` replacement, `mremap`, `mprotect`'s merge, `execve`'s release of the old image, and
exit's teardown move each removed region onto a local list under the lock and drop the list after
unlocking, since a last reference can release an unlinked inode, which takes that inode's lock and
the filesystem's block-mapping locks (Linux defers `fput` for the same reason). `msync` and an
`fsync` of a mapped range take counted references to the files of the regions they cover under the
lock for reading, release it, and then write back and wait (Linux releases `mmap_lock` before
`vfs_fsync_range`). Holding the lock across that writeback deadlocks three threads: an `msync`
holding it for reading waits for the inode lock of a `write` whose user buffer faults, the fault's
read request parks behind an `mmap` queued for writing, and the `mmap` waits for the `msync`. A
filesystem with a single volume lock ranks it at level 4, so it must drop it before any user copy.
This is Linux's order (`i_rwsem`, then `mmap_lock`, then the page lock, then the filesystem's own
block-mapping locks), chosen for the same reason: a `write` that faults on its user buffer while
holding the inode lock must not meet an `mmap` that holds the address-space lock and wants that
inode lock. ROADMAP §13.12's lock-dependency build checks both tiers.

The rank check fails an allocation or a free made while PT, BUDDY, SCHED, DEVICE, or SERIAL is held,
on every call, whether or not the heap grows. Growth takes PT and then BUDDY after dropping HEAP
(`heap_init::grow_for`), so the frame allocation it makes can enter direct reclaim with nothing held
([§4.4](MEMORY.md#44-kernel-heap) rule 1).

Filesystem spinlocks take `RANK_DEVICE`: each backend's mount and slot-allocation locks, the
working directory (`file_init::CWD`), and the initrd and vibefs images (`fat_init::INITRD`,
`vibefs_init::IMAGE`). The rank order therefore forbids heap allocation under them and allows
logging. The ramfs and kernfs stores (tmpfs's data ops run under kernfs's) are single volume locks
at level 4, `BlockingMutex`es as the FAT and vibefs volumes below are (`fs::StoreLock`): a store op
walks a directory's children, which a user can make number in the thousands, and an IF-off
stretch of that length breaks §2.9 rule 2's bound. kernfs's node table, which grows as nodes are
made, grows with its store unlocked (`KernFs::with_room`): the new table is allocated and the old
one freed outside the lock, and only the copy from one to the other runs under it.

The former cross-CPU `IrqCell`s are ranked `SpinMutex`es: the process table (`proc_init::TABLE`) and
the work queues (`work_init::ST`) at SCHED, each taken through `lock_nested` under the scheduler lock,
which serializes their wait queues; the KVA free-list (`kva_init::KVA`) at PT, through `lock_nested`
under the page-table lock; and the IRQ vector pool and routes (`irq_init::IRQ`), the APIC state
(`apic_init::STATE`), the keyboard ring (`kbd_init::KBD`), and the ECAM window (`pci_init::ECAM`) at
DEVICE. The VFS tables are heap tables that `fs_init::init_tables` allocates once before `irq:
enabled` and nothing grows (ROADMAP §10.4, D1), so bring-up allocates nothing under them. Filesystems get
no spin rank of their own; adding one changes this list and `crates/core/src/sync/lock.rs` in the same commit.

The VFS lock is a `BlockingMutex` at level 1's mount-table position.
It guards the namespace tables (mounts, dentries, and the inode and open-file tables) and is held
for a lookup, an insert, or a removal. It is never held across a backend's data I/O, a wait on a
pipe, TTY, socket, or page, or a user copy. A lookup returns counted inode and file references
(§2.11 rules 1 and 2). `read`, `write`, `truncate`, `fsync`, and `getdents64` then run on those
references with the VFS lock dropped, under the backend's own locks (an inode's at level 1, a
volume's at level 4), and a backend operation receives its superblock and inode through them, never
`&Vfs`. A namespace change (create, unlink, rename, mkdir, mount) and the directory read of a
dentry-cache miss may hold the VFS lock across the backend's directory I/O. Lookups leave the lock
with ROADMAP §19.5's RCU walk; namespace changes stay serialized by it. The fault path and the
writeback threads reach a file's backend through the counted page-cache reference that the region or
the page holds, and never take the VFS lock. Each FAT and vibefs volume is a level-4 `BlockingMutex`
that owns the volume and that nothing force-clears. It is taken with a plain `lock()`, as Linux
takes FAT's `fat_lock`, and contention never fails an operation; from ROADMAP §12.6 only a page
fill's acquire of it is killable, and ends early on a fatal signal (above). A backend never takes the
VFS lock while it holds a volume lock: what data I/O changes in an inode (its size and private
words) sits in the inode slot's words outside the VFS lock, which the backend reaches through the
inode it is handed.

Why: a lock that every file operation takes and that backends block under serializes all file I/O
behind one block wait, deadlocks a named-pipe read against its writer, and leaves the fault path,
which holds the page it fills busy (level 3), no legal way to fill a file page.

A socket has two locks, as Linux's `lock_sock` and `bh_lock_sock` do. Its spinlock, at the SOCK
rank, guards the protocol state and the socket's queues; network receive and timer callbacks
([§2.2](#22-interrupt-handler-rules)) take it, and code under it may allocate fallibly and wake. Its
owner lock is a flag that changes only under that spinlock, with waiters on the socket's wait queue:
a syscall holds it across its user copies, as a level-1 stream lock, and releases it before it
sleeps for data or buffer space. A receive or timer callback that finds the socket owned appends its
packet or event to the socket's bounded backlog, dropping and counting it when the backlog is full;
the owner drains the backlog as it releases the owner lock and clears the flag only once the backlog
is empty. Two sockets' spinlocks nest only through `lock_nested` (§2.3), in address order, as a
socket pair needs. ROADMAP §13.3 adds the lock pair and the rank, §15.5 the backlog.

## 2.2 Interrupt handler rules

An interrupt handler must not:

- allocate or free (no heap, no buddy, no `Vec`, no `format!`)
- take any lock that is ever held with interrupts enabled
- log at anything but the most extreme failure path
- run unbounded loops

An interrupt handler must:

- EOI before it can possibly context switch, so the controller is not held across a switch
- rearm its own one-shot timer source before doing anything else that can yield
- keep its stack frame small: only `#DF`, NMI, `#MC`, and `#DB` enter on dedicated IST stacks
  (§5.1), and NMI, `#MC`, and `#DB` move to the thread's kernel stack when they interrupt ring 3
  (§5.10 rule 3); every other vector runs on the interrupted kernel stack, or on TSS.RSP0 when it
  interrupts ring 3; on aarch64 every vector runs on the interrupted kernel stack, or on the thread's
  kernel stack when it interrupts EL0, after an entry test that moves it to this CPU's overflow stack
  when that stack has overflowed (§11.5 rule 6); on either architecture, a handler on the
  interrupted kernel stack runs inside the 4 KiB that §4.5's stack budget leaves above the deepest
  path

Both of the "must" rules are expanded in [section 5.8](INTERRUPTS.md#58-handler-ordering-rules), because both are
easy to violate and expensive to debug.

Blocking and allocation are a class of bug, not an instance. Context rules:

| Context | May block? | May alloc? | Scheduling class ([§7.8](SMP.md#78-per-cpu-scheduling)) |
|---------|------------|------------|---|
| Hard IRQ / MSI handler | No | No | None: it runs on the thread it interrupted |
| Softirq equivalent (high-prio workqueue) | No | Fallible only, without direct reclaim, down to half of the reserve ([§4.4](MEMORY.md#44-kernel-heap)); the hard IRQ only enqueues | Its CPU's worker: fair, nice -20 |
| RCU read-side section ([§2.12](#212-rcu)) | No; it may be preempted | Fallible only, without direct reclaim, down to half of the reserve ([§4.4](MEMORY.md#44-kernel-heap)) | Its thread's |
| Threaded IRQ bottom half | Only for its own device's resources, with a deadline; never for an I/O completion ([§5.4](INTERRUPTS.md#54-irq-registration)) | Fallible only, without direct reclaim, down to half of the reserve ([§4.4](MEMORY.md#44-kernel-heap)) | `SCHED_FIFO` 50 |
| Block error handler ([§10.3](BLOCK.md#103-failure)) | Yes, with IF=1 and no spinlock held | Fallible only, without direct reclaim ([§4.4](MEMORY.md#44-kernel-heap)) | As a threaded bottom half ([§7.8](SMP.md#78-per-cpu-scheduling)) |
| Workqueue worker | Yes | Yes, fallible only ([§4.4](MEMORY.md#44-kernel-heap)); without direct reclaim while it runs a softirq-equivalent item (row above) | Fair, nice 0 |
| Driver `probe` | Yes | Yes, fallible only ([§4.4](MEMORY.md#44-kernel-heap)); a failed probe leaves its device unbound and logs why | Fair, nice 0 |
| Syscall body, fault handler for a CPL-3 fault | Yes ([§2.9](#29-preemption-and-interrupt-state)) | Yes, fallible only ([§4.4](MEMORY.md#44-kernel-heap)) | The calling thread's |
| CPL-0 fault in a faulting user accessor (§5.1; may sleep from ROADMAP §12.2) | Yes ([§2.9](#29-preemption-and-interrupt-state) rule 3); may take the address-space lock (§2.1) | Yes, fallible only ([§4.4](MEMORY.md#44-kernel-heap)) | The calling thread's |
| CPL-0 fault in a non-faulting user accessor (§5.1) | No: it goes straight to the fixup | No | The calling thread's |
| Network receive: a queue's threaded bottom half ([§5.4](INTERRUPTS.md#54-irq-registration)) | No: it takes no sleeping lock and never waits for a socket's owner; a packet for an owned socket goes on its backlog (§2.1) | Fallible only, without direct reclaim, down to half of the reserve ([§4.4](MEMORY.md#44-kernel-heap)) | `SCHED_FIFO` 50; fair, nice 0 past its budget |
| Timer callback: a timeout-wheel callback ([§6.5](TIME.md#65-timers-and-timeouts)), run as a softirq-equivalent item on the CPU whose wheel fired it | No | Fallible only, without direct reclaim, down to half of the reserve ([§4.4](MEMORY.md#44-kernel-heap)) | Its CPU's worker: fair, nice -20 |
| NMI and `#MC` at any CPL, `#DB` at CPL 0; shootdown and call-function work run from a spin | No | No | None: it runs inside whatever it interrupted |

Code in the last row runs inside whatever IF=0 section its CPU was in, locks included: an NMI,
`#MC`, or `#DB` interrupts it, and a CPU in a serviced spin (§2.3) runs incoming shootdown and
call-function work there. So that code takes no lock of any kind (a `SpinMutex`, an `IrqCell`, or a
TAS, the log ring's TAS and the serial TX lock included), since the section it interrupted may hold
that very lock; `lock_enter` and `IrqCell::with` refuse a lock there (§2.3). It allocates nothing, does a bounded amount of work, and writes only per-CPU state
and lock-free rings. A call-function closure that needs a lock queues a work item on its CPU
instead. The panic path (§2.5) is the one exception: it reads the log ring and writes COM1 without
taking their locks.

The flight recorder (ROADMAP §10.7) is such a lock-free ring, one per CPU: a record masks IF for
its few stores and pushes to its own CPU's ring. NMI, `#MC`, and `#DB` bodies never record, because
`cli` does not mask them and one could land between a record's stores on its CPU. Shootdown and
call-function work run from a spin may record, because a spin never runs inside a record's IF=0
stores. The IDT dispatcher records only through `trace::traced_vector`, which names no event for
those three vectors or `#DF`.

The hard-IRQ top half acknowledges and wakes. Work that allocates or blocks runs on a kernel thread
([section 5.4](INTERRUPTS.md#54-irq-registration), ROADMAP §6.6). A blocking call from a device top half fails at the call:
`park`, `Sched::begin_wait`, and a voluntary `schedule` assert that `hardirq::IN_ISR` is clear,
`schedule_preempt` is the one switch interrupt context makes, and `IN_ISR` is set only around
`irq_init::dispatch`. The timer interrupt's top half also expires
deadline timers whose action is a wake or a signal, at most 32 wakes per interrupt
([§6.5](TIME.md#65-timers-and-timeouts)). A last put of a counted object runs the object's release in place
only where [§2.11](#211-object-lifetimes) rule 6 allows it; anywhere else the release is deferred to
a workqueue worker.

Network receive polls its queue for at most a budget of packets per wake, so one flooded queue
cannot hold its CPU; [§7.8](SMP.md#78-per-cpu-scheduling) gives the budget and what runs past it. Loopback
has no interrupt: its transmit queues the packet, and receive runs as a softirq-equivalent item on
the sending CPU. A timer callback (TCP's retransmit and delayed-ACK timers) runs on the CPU whose
timeout wheel fired it; a POSIX timer's or a `timerfd`'s expiry is a deadline timer and runs in the
timer interrupt's top half instead ([§6.5](TIME.md#65-timers-and-timeouts)).
`cancel_sync` returns only once the callback runs on no CPU, as `free_vector` does for a handler
(§5.4), and a pending timer holds counted references to what its callback touches
([§2.11](#211-object-lifetimes) rule 5).

## 2.3 Locking with interrupts

Every spinlock that is taken from both an ISR and normal context disables interrupts for the whole
critical section. That is the default: `SpinMutex` is IRQ-aware, and the non-IRQ-aware variant does
not exist. The scheduler lock, the input ring, the buddy allocator, and the heap all qualify.

Pick one spinlock implementation and use it everywhere. The old tree ended up with two (a ticket lock
in one design doc, an IRQ-guarded spin mutex in the code) and the mismatch was a source of confusion
for weeks.

`SpinMutex` is that implementation: a compare-and-swap lock until ROADMAP §27.5 makes it a queued
(MCS) lock, for every lock in one change. A waiter spins with IF=0, and nothing that can run inside
another holder's IF=0 section takes a lock (the serviced-spin row below, and
[§2.2](#22-interrupt-handler-rules)'s last row), so a CPU waits for at most one `SpinMutex` at a
time, and the queued lock needs one queue node per CPU. From ROADMAP §21.4, a waiter under KVM on
x86_64 that has spun a bound halts with IF=0 until it is kicked. It still services incoming work, as
§2.9 rule 2 requires: every publisher of work that `service_incoming` serves kicks a target it finds
halted, after its Release store and a full fence.

The lock, the serviced spins, and the two cells:

| Primitive | Use |
|------|-----|
| `SpinMutex` | Shared across CPUs. IRQ-aware. Ranked (§2.1): `lock` refuses a lock whose rank, or a later one, this CPU already holds, and `lock_nested` takes a second lock of a held rank (below). Its spin is a serviced spin. |
| Serviced spin | `SpinMutex::lock`, `ipi_init::wait_acks`, and the call-function slot wait each call `ipi_init::service_incoming` on every iteration, so a CPU that waits on another with IF=0 still acknowledges shootdowns and runs call-function work ([§2.9](#29-preemption-and-interrupt-state) rule 2). That work runs inside whatever the spinning CPU holds, so, like an NMI, `#MC`, or CPL-0 `#DB` handler, it takes no lock ([§2.2](#22-interrupt-handler-rules)'s last row). `service_incoming` first reads this CPU's stop request word, and STOP runs §2.5's stop routine before any slot is served, so a serviced spin stops for a panic with no interrupt (ROADMAP §10.7, F135), on either architecture and GIC version (aarch64: ROADMAP §11.3). On aarch64 a serviced spin never executes WFE and waits with `core::hint::spin_loop`: a masked interrupt is a wake-up event for WFI but not for WFE (Arm ARM DDI 0487, the WFE and WFI wake-up events), so a WFE spinner with IRQs masked neither takes an SGI nor wakes to poll. A line that puts WFE in a serviced spin also makes every publisher of serviced work, the stop word included, issue `sev` after its Release store, and says so. |
| `IrqCell` | IRQ-off exclusive access: `with` takes IRQs off, panics on same-CPU re-entry, and spins while another CPU holds it. It is `IrqCell<T, A>` over the port `A` (§10.3 seam): it masks through `A: InterruptMask`, its owner token is `A::cpu_id() + 1` (`cpu_id` reads 0 until the per-CPU base is live), it runs `CellHooks::acquire_check` before it takes the owner word, which is the kernel's refusal below (`sync_init::check_cell_context`), and it releases the owner word (Release) before it restores the mask. On the stub port each host thread is its own CPU, so a cell two host threads contend is taken in turn. Used only for CPU-local and boot-only state and for the log ring, the one cross-CPU exception: `log_init::LOG` keeps its unranked TAS because the panic path reads it without its lock and its holders never wait on another CPU (§2.5), and ROADMAP §19.5 replaces it with §2.5's lockless ring. The others: `log_init::STAGE`, one slot per CPU; `smp_init::STARTING` and `smp_init::LIVE_TABLES`, AP bring-up before `smp: done` (the `LIVE_TABLES` push allocates under the cell, which no rank allows); `arch::idt::IDT`, written by `idt::init` on the boot thread, whose address the APs load; `shell_init::REG`, CPU 0 only (the boot thread registers, and the kernel shell and the in-guest registry run pinned to CPU 0); `time_init::WRITER`, the clock's one writer, CPU 0 only (its tick and `confirm_clocksource` on the BSP); `arch::catch::LAST`, `kernel_tests` only, one slot per CPU, which only the CPU that armed its catch takes; and the cell the in-guest `irqcell_reentry_panics` test re-enters. It is `Send` and `Sync` only when `T: Send`, as `Mutex` is; const assertions in `crates/core/src/cell.rs` fail the build otherwise. |
| `BootCell` | Write once before `smp: done`, then shared `&T`. State written after publication sits behind its own `UnsafeCell` inside `T`. Not yet enforced: every switch writes the BSP's TSS through a pointer cast from `&Bsp` (ROADMAP §10.3, F089). The set-once check is an `assert!`, so it holds in release builds too (§9.4). It is `Sync` only when `T: Send + Sync` and `Send` only when `T: Send`, as `OnceLock` is, and const assertions in `crates/core/src/cell.rs` fail the build otherwise; `PerCpu`, whose raw pointers make it neither, carries its own `unsafe impl` naming invariants I43 and I21 (§7.5). |

Two locks of one rank nest only through `lock_nested`, in a pair order the call site's comment
names, and a per-rank count keeps the outer rank held when the inner lock drops. ROADMAP §12.1 adds
`fork`'s pair, the parent's page-table lock before the child's. ROADMAP §13.12's lock classes add
address order for a socket pair and check every pair order.

The rank checker enforces both rules (I1). Each CPU keeps one word, `sync_init::HELD`: a 4-bit count
per rank and a lockless depth, changed with atomic adds, since an NMI may run between a load and a
store. `lock` and `try_lock` fail the check while this CPU holds a lock of their rank or a later one;
`lock_nested` allows the held rank itself and counts it, so the inner release leaves the outer rank
held. Each is `#[track_caller]`, so a refusal names its call site, and the check runs before the
spin, so a refused lock is never taken. `sync_init::lockless_section` raises the depth for code in
§2.2's last row: `ipi_init::service_shootdowns` around each slot it serves, `service_calls` around the
call-function closure, and the NMI and `#MC` bodies and a CPL-0 `#DB` body once the `kernel_tests`
intercept has passed. While it is nonzero, `lock_enter` refuses every `SpinMutex`, rank 0 included,
and `IrqCell::with` refuses too (`sync_init::check_cell_context`). Checks and counting stop once
`serial::raw::HALTING` is set: the panic path is §2.2's stated exception.

A wake takes the scheduler lock, which ranks before device and serial locks. So code holding one of
those records the wake and performs it after dropping the lock, as §10.1's completion does, and a
top half wakes its bottom half with no device lock held; the rank check fails the other order on
every call.

They live in `crates/core/src/cell.rs` (`BootCell`, `IrqCell`, `vibeos::cell`, which the kernel names over its port in `src/cell.rs`) and `src/sync/sync_init.rs` (`SpinMutex`). Do not add another `UnsafeCell` + `unsafe impl<T> Sync` wrapper. `scripts/check_cells.py` reads each `unsafe impl` of `Send` or `Sync` whole, from `unsafe impl` to its `{`, and allows a generic one only in `crates/core/src/cell.rs`, `src/sync/sync_init.rs`, `src/sync/blocking_init.rs` and `crates/core/src/kalloc.rs` (`TryArc`'s one bounded pair) and only with AGENTS.md rule 6's bounds: every type parameter bounded by `Send`, and by `Sync` too for `Sync` on a type that shares `&T` (`BootCell`, `RwLock`); `?Sized` alone is no bound. A concrete type that holds an `UnsafeCell` or a raw pointer may carry its own impl, whose `// SAFETY:` line names the invariant, except `PerCpuRemote`, which must be `Sync` from its atomic fields alone (§7.5). `static mut` is only the asm-owned `vibeos_jmpbuf` in `arch/x86_64/catch.rs`. Accessors do not return `&'static mut`.

Cross-CPU rule: a CPU never touches another CPU's run queue directly. Work is handed over through a
per-CPU inbox plus a reschedule IPI. More SMP-specific rules in [section 7.7](SMP.md#77-locking-with-more-than-one-cpu).

## 2.4 Memory invariants

- The buddy takes only memory the boot memory map marks usable, less physical page 0, the AP
  trampoline page (§7.3), and the kernel image, framebuffers, and boot modules wherever they overlap
  usable memory. Limine keeps usable entries clear of every other entry, so the last three are
  defensive; `pmm_init` clips each usable range against all of them as it reads `BootInfo`
  (`vibeos::pmm::clip_usable`), with no fixed-size list, so no exclusion is ever dropped.
- Buddy free list nodes live inside the free pages themselves. A stray write into freed memory
  corrupts the allocator, so guard pages on stacks are not optional: every kernel stack, the
  bootstrap thread's included, is a guarded KVA stack. Boot leaves Limine's stack at
  `thread_init::init_bootstrap`, once KVA is up, for the bootstrap thread's guarded 64 KiB stack
  ([§4.5](MEMORY.md#45-kernel-virtual-address-allocator)).
- A PTE change that removes or narrows a translation takes effect only when every CPU that could hold
  the old translation has invalidated it and acknowledged. Such changes are unmapping, making a PTE
  not-present, read-only, or NX, and clearing its dirty bit. The rule covers kernel and user
  mappings, CPU TLBs and paging-structure caches, and, from ROADMAP §18.1, the IOTLB. On aarch64 the
  acknowledgement is the completion of the broadcast TLBI's `dsb ish`. Until then nothing relies on
  the change: no frame or page-table page is freed or reused, no virtual address is reused,
  `mprotect`, `munmap`, `mremap`, and `madvise(MADV_DONTNEED)` do not return, `fork` does not make
  the child runnable, and no page counts as clean. Clearing only the accessed bit, as LRU aging does,
  needs no completed invalidation: a stale accessed bit only misjudges how recently a page was used.
  Kernel mappings are `GLOBAL`, so every online CPU could hold them ([§7.9](SMP.md#79-tlb-shootdown)).
  Rule; not yet enforced for user mappings: `addr_space_init::shootdown_user` invalidates only on the
  calling CPU, which is enough only while one thread owns each address space (I8; ROADMAP §12.3).
- MMIO pages are mapped uncacheable. QEMU tolerates write-back MMIO; real hardware does not. On
  aarch64 they are Device-nGnRE (ROADMAP §11.1), and a device access is ordered against Normal
  memory only by [§4.7](MEMORY.md#47-dma)'s accessors.
- Every mapping is `NO_EXECUTE` unless it holds code that is fetched. The trampoline page (§7.3) is
  the low identity window's one executable leaf, read-only and not global, and after `smp: done` the
  window's only leaf (ROADMAP §10.6, F085).
- A value copied to user memory has no padding and no uninitialized bytes. Reading a padding byte is
  undefined behaviour in Rust, and copying one out leaks kernel stack or heap. The typed copy-out,
  `uaccess::copy_to_user_val`, takes only a type bounded by `zerocopy`'s `IntoBytes + Immutable`,
  whose derive refuses at compile time a type with padding or uninitialized bytes (a `compile_fail`
  doctest on it copies out a `#[repr(C)]` struct with a hole); the byte form, `copy_to_user`, takes
  a `&[u8]`. A uapi struct whose Linux layout has an implicit hole declares it as an explicit field
  that the kernel zeroes (`_pad: [u8; N]`).
- A second-level translation of guest memory (an EPT or NPT entry on x86_64, a stage-2 entry on
  aarch64) is a mapping of the host frame behind it, since ROADMAP §21.2 backs guest RAM with the
  VMM's address space. Every change or removal of a user PTE (`munmap`, a COW write-protect or
  break, a permission reduction, a reverse-map unmap, a migration, `MADV_DONTNEED`) invalidates the
  second-level translations of that range on every CPU that may hold them before the frame's count
  drops. On x86_64 every vCPU of the VM flushes the VM's translations before it next enters the
  guest (`INVEPT` on VMX, a flush of the guest's ASID through the VMCB on SVM), a vCPU running in
  guest mode is kicked out first, and a vCPU that enters on a CPU other than the one it last ran on
  flushes there too. On aarch64 the host issues `TLBI IPAS2E1IS` for the range, `DSB ISH`,
  `TLBI VMALLE1IS`, and `DSB ISH` under the VM's VMID. A second-level fault reads the address
  space's invalidation sequence before it looks up the host PTE, and retries if an invalidation ran
  in between. A vhost worker reaches guest memory as a thread of the VMM's address space does: it
  holds a `users` reference ([§2.11](#211-object-lifetimes)) from the VMM's `VHOST_SET_OWNER` until
  its device is released, and it copies only through the ROADMAP §10.6 accessors, never through a
  frame or a kernel mapping it caches. This is Linux's `mmu_notifier` with KVM's invalidation
  sequence. Planned (ROADMAP §21.2): no hypervisor exists yet.

## 2.5 Panic policy

Binding order (do not invert):

1. Stop every other CPU first, through one stop primitive (ROADMAP §10.7, F135; §11.3 on aarch64),
   which every path that stops the other CPUs uses, ROADMAP §25.4's capture jump included. Built on
   x86_64 (`ipi_init::{stop_others, stop_this_cpu, nmi_stop}`, `vibeos::irq::stop`):
   - `panic::begin_dump` runs `cli`, then claims the dump (`serial::raw::claim_dump`) before it stops
     anyone. A CPU that finds the dump claimed by another CPU sets no request and runs the stop routine
     itself, as `panic`; the owner re-entering prints `vibeOS: panic: reentered` and halts.
   - The owner sets `HALTING`, then for each other online CPU sets STOP in that CPU's request word
     (`PerCpuRemote.stop_req`, Release) and sends it the stop IPI (`0xFE`, Fixed, on x86_64; the stop
     SGI on aarch64). A CPU stops at the first of: taking the IPI (`ipi`); its next `service_incoming`
     poll, which reads the request word before any slot, so a CPU in a serviced spin (§2.3) stops with
     IF=0 and no interrupt, on either GIC version (`poll`); its next serial write or log append, which
     runs the stop hook the primitive installs with `serial::raw::set_stop_hook` (`poll`); or the NMI
     below (`nmi`). The request stays set, so a CPU that reaches a poll late still stops. The IPIs go
     through `apic_init::send_ipi`, which records no trace event (`trace!` takes an `InterruptGuard`).
   - The owner waits up to 100 ms of counter time (`CycleCounter`, or a poll bound before the counter
     is measured: `irq::stop::stop_budget`) for each `stopped` word, then sends NMI to each CPU that has
     not acknowledged (the NMI IPI to its APIC id on x86_64, at once for a CPU whose `0xFE` the LAPIC
     refused; on aarch64 the GICv3 pseudo-NMI from ROADMAP §25.5, and nothing on GICv2) and waits 10 ms
     more. Only then does it re-initialize serial (step 2). After the backtrace (step 4) it prints, for
     each other online CPU in id order, `vibeOS: panic: cpu N stopped (ipi|poll|nmi|panic)` with a
     `vibeOS: panic: cpu N regs: …` line from its slot, or `vibeOS: panic: cpu N not stopped`; a
     refused NMI leaves its CPU `not stopped`. A CPU left `not stopped` loops without polling, which
     §2.9 rule 2 already makes a bug. Planned (ROADMAP §20.9): a CPU inside a firmware call is the
     exception, since SMM holds off even an NMI; while its firmware record
     ([§4.8](MEMORY.md#48-firmware-runtime-services)) is set it is printed
     `vibeOS: panic: cpu N not stopped (in firmware)`, and every CPU found in firmware also gets a
     `vibeOS: panic: cpu N in firmware: <service>` line and a backtrace from the record's saved
     frame.
   - The stop routine (`ipi_init::stop_this_cpu`) moves its CPU's `stopped` word from running to
     stopping (a CPU already stopping halts at once, so an NMI during a poll stop leaves the slot as it
     was), saves the interrupted registers and frame pointer (its own, for a poll or panic stop) in its
     per-CPU crash-register slot (`PerCpuRemote.crash`), stores how it stopped with Release (the
     acknowledgement), and halts with every interrupt masked (on aarch64, a `wfi` loop with DAIF set).
   - The NMI handler first swaps its CPU's request word to 0 (`ipi_init::nmi_stop`, deciding through
     `irq::stop::nmi_action`) before it writes anything or takes any lock: on a CPU already stopping or
     stopped it halts again at once; an NMI on the dump owner returns at once, so the dump completes;
     STOP runs the stop routine. Any other NMI dumps and halts, as before, until ROADMAP §25.5 makes it
     an all-CPU backtrace.
   - Only the owner writes COM1. A4's raw serial layer (ROADMAP §10.3, `serial::raw`) holds
     `HALTING`, the owner's CPU id (`claim_dump` and `owner_cpu`, which replace the `DUMPING` flag),
     and `write_owner`, a write that takes no lock and no `InterruptGuard` and writes only on the
     owner. From the `cli` that opens `panic::begin_dump`, every dump line, `vibeOS: panic: reentered`
     included, is one `write_owner` call, built in a stack buffer (`panic::line`, `panic::out`), and
     nothing on the dump path creates an `InterruptGuard` (no `IrqCell::with`, `marker!`, `klog!` or
     `Serial`) or takes a lock, so a panic inside a guard's own bookkeeping, such as an `irq_nest`
     underflow, dumps once (ROADMAP §10.7, F071). Once `HALTING` is set, a serial write or log append
     on any other CPU runs the stop routine instead (`serial::raw::stop_if_halting`, which halts with
     no hook set), and a `Serial` write on the owner goes through `write_owner`.

   Why one primitive: an IPI misses a CPU spinning with IF=0, and aarch64 has no NMI before ROADMAP
   §25.5 and none on GICv2, but the commonest such CPU, a waiter on a lock the panicking CPU holds,
   already polls `service_incoming` on every iteration. Rejected: an NMI-only stop, which leaves
   IRQ-masked aarch64 waiters running into the dump and the capture kernel; a separate stop path for
   the capture jump, a second implementation (AGENTS.md rule 10) under which only capture panics
   record the other CPUs' registers; keying the NMI handler on the global `HALTING` flag, under which
   any NMI halts the dumping CPU mid-dump; and dropping other CPUs' writes after `HALTING`, which
   leaves the writer running.
2. Re-initialize serial from scratch, once, on the owner (`serial::raw::init`): the panic may be *in*
   the serial path.
3. Print location and message; dump registers, the current thread, and the last N log records, each
   line one `write_owner` call (`log_init::dump_tail` writes the log tail through `panic::out`).
   From ROADMAP §19.5, every record serial has not printed goes out first (the log contract below),
   unless a capture kernel is loaded (step 6), whose vmcore holds the ring.
4. Symbolized backtrace when frame pointers exist (in-image sorted table, binary search, no alloc).
   An exception's dump starts the walk, and its `vibeOS: regs:` line, at the interrupted frame: the
   `rbp` the entry stub saved in the trap frame, which `panic::exception_halt` and
   `panic::exception_vec` take from the IDT body, not the handler's own (ROADMAP §10.7, F070); a Rust
   panic starts at the panic handler's own frame.
   The walk (`vibeos::log::backtrace::walk`, through `panic::walk_known`) follows `rbp` only into a
   known stack (`panic::known_stacks`): the current thread's KVA stack, the boot stack whose bounds
   `_start` records first (`panic::note_boot_stack`, the Limine stack request's size below the first
   RSP's page), and this CPU's four IST stacks and fallback RSP0 stack (`gdt::this_cpu_stacks`, the
   IST tops read from the live TSS). It reads a frame record only after finding its 16 bytes, 8-byte
   aligned, inside one of them, and ends on a null `rbp`, an unknown stack, a return address outside
   the image, a frame that does not rise, or 24 frames. `vibeos_syscall_entry` zeroes `rbp` just
   before it calls the Rust body, after the frame saved the user's, so a walk from inside a syscall
   ends at the entry instead of following the user's `rbp` (ROADMAP §10.7, F139).
5. Encode the panic record (ROADMAP §20.1) in a fixed buffer, and copy it to §20.1's reserved RAM
   region when one is configured. Memory stores only: no lock, no firmware call, nothing that waits.
   Planned (ROADMAP §20.1); today there is no record.
6. If a capture kernel is loaded (ROADMAP §25.4), jump to it. Before the jump the panicking CPU does
   only this: it takes ROADMAP §20.9's runtime-services lock and the ERST backend's lock (ROADMAP §25.6) each
   with a trylock and never releases either; it sets an armed watchdog (ROADMAP §20.6) to its longest
   timeout and feeds it once; and it writes pvpanic's crash-loaded event (bit 1), which a host
   records without stopping the guest, where the kernel found a pvpanic device (ROADMAP §10.7,
   §11.7) that lists that event: `vibeos::log::pvpanic::event_for(Step::CaptureJump, …)` makes
   that choice and `log::pvpanic_init::signal` the write, as built; the jump that calls them is
   planned (ROADMAP §25.4). It flushes no log backlog, sends nothing over netconsole, and calls
   no firmware. The crash handover passes runtime services, and ERST, on to the capture kernel only
   where that lock's trylock succeeded, and names the CPU the capture kernel starts on and the
   physical address of step 5's record. The capture kernel writes the vmcore, feeding that watchdog
   after each chunk, so a dump that takes longer than the timeout survives and a capture that stops
   making progress is still reset; then it writes that record through each store its handover passed
   on (ROADMAP §25.6), and then resets through step 7's ACPI or PSCI path; where no store was
   passed, the copy in reserved RAM is the record. It never calls firmware its handover withheld,
   which a stopped CPU, or on GICv2 a CPU still running, may have been inside. Planned (ROADMAP
   §25.4, §25.6).
7. Otherwise, halt or reset. Today, after `vibeOS: panic: halted`, `panic::finish` writes
   pvpanic's panicked event (bit 0), after which the host may pause or end the guest (the
   harness's QEMU pauses, `-action panic=pause`), then runs a `cli; hlt` loop. The device is found
   once at boot: `log::pvpanic_init::probe`, right after the Limine handshake, looks for fw_cfg's
   `etc/pvpanic-port` only under a hypervisor (invariant I57), reads the port that file names
   once for the events the device supports, and keeps both in one atomic; the write,
   `pvpanic_init::signal`, is one Acquire load and at most one port write, with no lock, no
   allocation and no interrupt guard, only at that port and only when that mask has bit 0 (ROADMAP
   §10.7). Bare metal never probes fw_cfg, so no port is written there. Planned, before that
   write and in this order: send the dump over netconsole where one is configured (ROADMAP
   §25.6); write the record to an EFI variable or to ERST under that store's trylock (ROADMAP
   §25.6), last among those writes, since a firmware call has no time bound; and after it, with
   `panic=<seconds>` (ROADMAP §22.2), wait and reset through the ACPI or PSCI path of the `reboot`
   call instead of the `cli; hlt`.

   Why two branches: a capture kernel exists to take the one complete dump, so nothing that can
   wait, or that lets a host stop the guest, runs before the jump. pvpanic's panicked event pauses
   the harness's QEMU (`-action panic=pause`) and lets a host's crash policy end the guest; its
   crash-loaded event does neither. Linux likewise runs no panic notifier before its crash jump
   unless `crash_kexec_post_notifiers` is set. Rejected: one linear order (record, pvpanic, jump,
   reset, halt), which pauses the guest and calls firmware before the capture kernel runs; an EFI
   write before the jump whenever the trylock succeeds, which puts an unbounded call ahead of the
   vmcore when the capture kernel can write the same record after it; and a switch like Linux's,
   which is two orders to specify and test.

No unwinding: `panic = "abort"`. Serial TX in this path is a bounded THRE poll; drop the byte on
timeout (see [§9.6](PITFALLS.md#96-hardware-polling)). Do not take SCHED. Allocate nothing, the panic record
(ROADMAP §20.1) included: the dump, the backtrace, and the record use fixed buffers and reserved
memory, since the panicking CPU or a stopped one may hold the heap lock. The log ring is readable
after the halt IPI without taking its TAS (force-unlock if the panicking CPU held it).

The panic record outlives the reset that follows a panic (ROADMAP §20.1, §25.6). Planned: every
store uses one format, a header holding a magic and a format version, a sequence number, and a
checksum, which every release of a major version reads (ROADMAP §22.1). Reserved RAM holds one
record; from ROADMAP §25.6 an EFI variable store or ERST holds at most two, the newest and the one
before, and an EFI record is at most 1 KiB. The kernel writes an EFI variable only when
`QueryVariableInfo` reports room for it with 5 KiB to spare, as Linux's x86 EFI code requires, since
some firmware stops booting with a full variable store; otherwise it writes the next store in line.
The next boot logs each record it finds into the kernel log and then deletes it, so a panic loop
cannot fill the store and ROADMAP §22.2's `BootNext` write keeps working. The retired-frame list
(ROADMAP §25.3) follows the same rules: one variable of at most 64 entries, applied only on the
machine and memory map it was written for.

Log ring (ROADMAP §5.5):

- 256 records of up to 96 message bytes; a wrap drops the oldest record and counts it, and the panic
  dump prints that count with the two drop counts below
  (`vibeOS: log: last N (M dropped, S sink, R reentry)`).
- Compile-time maximum level: `trace` in debug builds, `debug` in release. Runtime filter: an
  `AtomicU8`, default `info`.
- The panic dump prints the last 24 records.
- The ring's lock (`log_init::LOG`, an `IrqCell`) is IRQ-off and outside the §2.1 rank order; never
  hold it across serial TX.
- `klog!` and formatted `Serial` writes keep IF off for the whole emit, so the per-CPU capture stage
  cannot interleave with a preempting thread.
- `dmesg` prints through a plain serial path that does not re-capture.
- Host tests cover overflow, filtering, and symbol lookup.
- One global IRQ-safe ring and a serial try-lock sink. The sink drops its copy of a record when
  another CPU holds the TX lock, and `log_fmt` drops a record its CPU emits while already inside
  `log_fmt`; each drop is counted, in `log_init::sink_drops` (once per record) and
  `log_init::reentry_drops`. `log_fmt` sends each record, newline included, in one try-lock
  write (ROADMAP §10.2, F138). Planned: the log contract below (ROADMAP §19.5).

Planned (ROADMAP §19.5), the log contract:

- One store: a lockless ring of fixed-size slots, each with a state word. A writer takes the
  record's sequence number from one 64-bit counter with a `fetch_add`, claims slot `seq mod N` with
  a compare-and-swap on its state word, fills it, and commits it with a Release store; a writer that
  finds its slot still being written drops its record and counts it. A reader copies a slot and
  checks its state word before and after, as a seqlock reader does. So any context may append, NMI
  and `#MC` included, and the ring is readable after a panic with no lock. A record carries its
  sequence number, CPU id, level, and timestamp. Its timestamp is `now_ns`, which any context may
  read (§6.4).
- One printer thread per console prints records in sequence order, so an emitter never waits for
  the UART. Where records were overwritten or dropped before it reached them, it prints
  `vibeOS: log: N records dropped`, and `/dev/kmsg` returns `EPIPE` there (ROADMAP §13.9). Until a
  console's printer thread runs, records print synchronously as they are appended; after `HALTING`
  only the dump owner prints, synchronously, through the owner write (step 1).
- `marker!` writes its line to serial under one TX hold before it returns, then appends its record
  marked printed on serial. The mark is per console, so the framebuffer printer still prints it.
  Markers therefore keep program order among themselves whatever the printer does; a `klog!` record
  the serial printer has not reached yet can print after a later marker.
- An emitter in NMI or `#MC` context only appends and takes no console lock (§2.2); its records
  print when a printer reaches them.
- The panic dump first prints, through the owner write, every record serial has not printed, unless
  a capture kernel is loaded (step 6).

Why: per-CPU buffers have no total order without a timestamp merge, `/dev/kmsg` exposes one
sequence number per record, and ROADMAP §25.5 logs from NMI context, where the `IrqCell` ring panics
on re-entry. Linux's printk ended in the same shape: a lockless ring from 5.10, with its per-CPU
safe and NMI buffers removed in 5.15. Rejected: per-CPU staging drained by a printer, which has no
total order; printing every record synchronously, which at 115,200 baud (about 11.5 KB/s) stalls
every emitter; markers through the printer, which lose a marker whose CPU then hangs and make the
harness's order depend on scheduling; and Linux's variable-length descriptor and data rings, which
96-byte records do not need and which loom models less easily.

Symbols come from a two-pass link (Makefile `KERNEL_VARIANT`): `nm` output from the first link fills
an in-image table (`KSYMS`) that the second link builds in. Rule: every function has the same address
in both links. `gen_ksyms.py` emits `KSYMS` as a `#[used]` static in its own `.ksyms` section, which
`linker.ld` places after `.rodata` and no kernel code names: `log::ksyms::lookup` finds the entries
only through the linker-defined `__ksyms_start` and `__ksyms_end`, never through a slice whose length
is a compile-time constant, so the code that reads the table is the same size empty and filled. Every
`KERNEL_VARIANT` recipe then reruns `gen_ksyms.py --check` on the final ELF and fails when the table
differs from the one it linked (ROADMAP §10.2, F084).
Frame pointers come from `-C force-frame-pointers=yes` in `.cargo/config.toml` (§3.1); there is no
target JSON.

Rule: `vibeos-core` (`crates/core/src/lib.rs`) does not panic on data; its parsers and table walks return the
module error. Enforced: clippy denies `unwrap_used`, `expect_used`, and `panic` on that crate
(allowed in `#[cfg(test)]`), and `indexing_slicing` and `arithmetic_side_effects` in every byte
parser ROADMAP §10.2's fuzzers cover: module-level in `acpi`, `part`, `fat`, `pci`, and `elf`, and
function-level on `virtio::read_modern_caps`, `shell::tokenize`, and `kbd::Decoder::feed`, which
`scripts/check_core_stable.py` checks. vibefs v1 carries one allow until ROADMAP §14.8 retires it.
Outside the parsers both Cargo profiles set `overflow-checks = true` (§3.5), which turns an
arithmetic overflow into a panic. The kernel binary denies `unwrap_used`, `expect_used`, `panic`,
`unreachable`, `todo`, and `unimplemented` crate-wide (`src/main.rs`), where a site a kernel
invariant bounds keeps an `#[allow]` that names the invariant (§9.4). Crafted input panics portable code in one case: a CRC-valid vibefs leaf whose count
exceeds the per-leaf maximum (F061; ROADMAP §14.8 retires v1 for a v2 that validates every block it reads).
Panics in the kernel binary end in the binding order above.

Exceptions follow the per-vector table in [section 5.2](INTERRUPTS.md#52-idt-and-exceptions), whose Ring 0 column
names each ring-0 case that continues instead of halting. A kernel `#BP`
logs and continues. Planned (ROADMAP §17.4, §18.4): so do three ring-0 `#DB` cases, which §5.2's row
lists: a hit on a debug slot the current thread's tracer armed, a stray single step, and, in the
data-race detector's build, a hit on its own slots. Every other exception taken in ring 0 dumps and
halts in the same order as `#[panic_handler]`. A `#PF` at CPL 0 whose faulting instruction has an
exception-table entry, with CR2 in the user half, is not a kernel fault: it resumes at the entry's
fixup. Only the §5.1 user-memory accessors have entries, and §5.1 says how each kind ends. Planned
(ROADMAP §11.6): the same rule for an aarch64 data abort at EL1. Rule: ring 3 never
halts the kernel. An exception raised by ring-3 code, or by a return to ring 3, sends that process
the signal §5.2 gives the vector (§11.5 the exception class, on aarch64), and the kernel keeps
running. The signal's action then applies, as on Linux: from ROADMAP §13.8 a handler may catch it,
from §17.4 a tracer sees it first, and a fault signal the process blocks or ignores still takes its
default action. The default action, the only one today, ends the process and prints
`user: pid N killed SIG<name>`. Pid 1 is the exception. No process can kill or stop init:
`kill` drops a signal sent to pid 1 unless init has a handler for it, never `SIGKILL` or
`SIGSTOP`, and none before ROADMAP §13.8 (`proc::kill_delivers`). When init exits, by `exit` or by a signal,
the kernel panics with a line naming the exit status, or the signal and, for a fault, the faulting
address, as Linux panics when init dies: `finish_exit` prints `vibeOS: init: pid 1 exited <n>`,
`killed SIG<name>` or `killed SIG<name> addr=0x<hex>` (CR2 for `#PF`, else the faulting RIP) before
any teardown, then panics (F068). The line is the `failure` row `vibeOS: init: pid 1 <text>` of
`tests/contract/markers.toml`, and `make test-e2e-init-fault` boots an initrd whose `/sbin/init`
stores to `0x1000` and requires it. The entry and exit windows of §5.10 hold: every
return to ring 3 runs with IF=0 from its `cli` to its `sysretq` or `iretq` (§5.10 rule 4), and a
non-canonical saved RIP, or a `#GP`, `#NP`, or `#SS` on a return-to-user `iretq`, kills the process
with `SIGSEGV` on the kernel GS (§5.10 rule 2). Every ring-3 trap takes its signal from
§5.2's table through `proc_init::sig_for_vec`, a ring-3 `#DB` included. An NMI dumps and halts on its IST stack; from
ROADMAP §10.7 the NMI handler first reads its CPU's stop request word (step 1).

Rule: nothing is silently swallowed. An error is returned to its caller, or handled where it arises
in one of three ways: a counter plus a log line at most once a second; an error state recorded on
the device, volume, or file, which a later call reports, as Linux's `errseq_t` does for writeback
errors; or a retry whose bound and give-up path its comment names. A result may be dropped only when
it carries no failure anyone could act on, such as cleanup after an earlier error that was already
returned, or the `fmt::Result` of a write to `Serial`, which cannot fail. Clippy's
`let_underscore_must_use` and `unused_result_ok` catch a dropped `#[must_use]` value, and a kept drop
carries `#[expect(clippy::let_underscore_must_use, reason = "...")]` naming the case above, so an
exemption whose drop goes away fails the build. Test code is exempt at its root: `vibeos-core`
allows both lints under `cfg(test)` in `lib.rs`, and each `kernel_tests`-only `ktest` module's `mod`
line carries a permanent allow. An `if let Ok` with no `else`, and `let _ = f().ok()`, are review
items, since no lint sees them. Built (ROADMAP §10.1): root `Cargo.toml`'s
`[workspace.lints.clippy]` denies both lints, and every member (`vibeos`, `vibeos-core`, the hostlib
tests and the user crate) sets `[lints] workspace = true`; every module has had its sweep, so no
module carries an audit-pending allow, and every kept drop names its case. `KError` and every module
error enum are `#[must_use]`. The dropped errors the kernel review found are fixed (F080, F051, F063,
the readahead eviction, F115) but one: F124's `read_dirent` mapping (ROADMAP §13.9), a `match` arm
that turns a read error into end-of-directory, which no lint sees.

Hardware events are also lost in three cases. An exception before `idt::init` (PMM, the CR3 switch,
ACPI discovery, heap, KVA, GDT, PIC) goes to whatever IDT Limine left and resets or hangs with no
output (ROADMAP §11.1, F136). LINT1 is masked on every CPU and MADT NMI entries (types 3 and 4) are
not parsed, so a chipset or external NMI never reaches the NMI handler (ROADMAP §20.1, F096). An
interrupt no handler owns is counted, EOIed, and logged at most once a second per vector, not
lost and not a halt (§5.2).

## 2.6 Serial markers

Every boot line is `vibeOS: ` followed by lowercase text: `<subsystem>: <state>`, or
`<subsystem> <state>` for the one-word lines (`serial online`, `heap ok`, `gdt ok`, `idt ok`,
`console ok`, `shell ready`); units keep their case (`4KiB`), and no line ends in a period or an
exclamation mark. Success markers are asserted by the e2e harness in order. A line the harness knows
is a row of `tests/contract/markers.toml` (ROADMAP, How to read this), added in the commit that first
prints it; `scripts/check_markers.py` fails on a `marker!` line with no row.

The markers are a contract with the harness, not an interface for software outside the tree: ROADMAP
§39.1 classes them `internal`, so a release may change one, with its row in the same commit.

`marker!` for registered lines of every kind (ROADMAP, How to read this); `klog!` for everything
else; `PlainSerial` only for `dmesg` and the IF-off tracer's report (ROADMAP §10.3), whose lines would
wrap the log ring; the panic dump writes through `serial::raw::write_owner` (§2.5 step 1). `marker!` writes serial before it returns; from
ROADMAP §19.5 a `klog!` line reaches serial when a printer thread gets to it
([§2.5](#25-panic-policy)).

Every line the kernel writes to its console UART starts with the byte 0x1E (ASCII RS), which
terminals ignore: `marker!` and `klog!` lines, ktest verdicts, and the panic dump alike. The harness
takes only framed lines as the kernel's (§8.3), so no user byte may produce the frame. The kernel
writes one line per call, and a `\r`, `\n`, or 0x1E inside a line prints as `?`, so a string a user
chose (a path, a thread name) cannot start a line of its own. The console UART's user write path (the
console `write` today, the ROADMAP §13.7 serial TTY and its echo later) prints a 0x1E in user bytes
as `?`, and a record a user writes through `/dev/kmsg` prints unframed. Before a framed line, the
kernel writes a newline when the last byte on that UART was user output that did not end one. The
framebuffer console never draws the frame, and a UART that is not the console carries neither frame
nor escape.

As built (ROADMAP §10.2): `serial::raw`, the only code that writes the UART data register, frames and
escapes every kernel line through `vibeos::log::line::kernel_line`, and the console `write` that user
descriptors reach sends user bytes through `Serial::write_user`, which escapes each 0x1E and adds no
frame (`line::user_bytes`). The flag that says a user line is open starts set, since the loader's last
byte is unknown, so the kernel's first line always starts on a fresh one. A kernel line over 256 bytes
is cut and ends in `...`. `tests/harness/frame.py` classifies each line for every driver: kernel text
from a framed line, user text from an unframed one.

## 2.7 Invariant register

Every invariant the code relies on, and how it is kept. Status: *enforced* means code, a type, or a
test fails when the invariant breaks; *documented* means a rule in the section named or in a code
comment, with nothing that checks it; *assumed* means relied on but stated only in this table.
"Holds today" is the state at the commit that last changed the row; where the answer is no or
partly, the row names the ROADMAP line that fixes it. Relied on names the sections (`§x.y`,
`ROADMAP §x.y`), modules, and other rows whose correctness depends on the row (KERNEL_REVIEW §8.3).
Enforced by names, each in backticks, the tests, lints, types, and scripts that fail when the row
breaks, or is `none`; text outside the backticks is commentary. A name is a path (`scripts/...`), a
`make <target>`, a `clippy::<lint>` or `rust::<lint>` set to `deny` or `forbid`, a host test (a
`#[test]` or `#[kani::proof]` fn), an in-guest test, a `/bin/tests` case, a harness test, or a type;
`scripts/doc_refs.py` in `make check` fails on a name not in the tree, on an empty Relied on or
Enforced by cell, and on a row whose Status starts with *enforced* and whose Enforced by is `none`.
These ids name invariants; ROADMAP's bare `I1` names the architecture review's issue, a
separate namespace, so code, `// SAFETY:` comments, and commit messages cite a row as `invariant I<n>`.
A rule the code relies on gets a row, with the next free id, in the commit that states it. Ids I1 to
I29 descend from KERNEL_REVIEW §3.9, and several have been restated since (I3 most of all), so an id
that review cites means the review's text.

| # | Invariant | Established at | Relied on | Enforced by | Status | Holds today |
|---|-----------|----------------|-----------|-------------|--------|-------------|
| I1 | Lock rank HEAP < PT < BUDDY < SCHED < DEVICE < SERIAL (§2.1); a second lock of a held rank only through `lock_nested` (§2.3) | `lock.rs`, `sync_init::lock_enter`, `IrqCell::with`, `thread_init::switch_now` | §2.1, §2.3, §7.9; `sync_init`, `thread_init::switch_now`, every `SpinMutex` holder | `same_rank_lock_is_refused`, `lock_across_switch_asserts`, `cross_cpu_cells_ranked` | enforced at runtime, per CPU | Partly: `switch_now` asserts that no ranked lock is held across a context switch, but an `IrqCell` or a rank-0 `SpinMutex` held across one is not counted and stays unchecked (ROADMAP §13.12) |
| I2 | Hard-IRQ context never blocks or allocates (§2.2) | `hardirq::IN_ISR`, `sync_init::might_sleep`, `sync_init::assert_not_hard_irq` (in `park`, `Sched::begin_wait`, and a voluntary `schedule`) | §2.2, §5.4; `irq_init::dispatch`, `sync_init`, `thread_init` | `block_in_hard_irq_asserts`, `in_hard_irq_top_bottom` | enforced for blocking in a device top half; documented otherwise | Partly: allocation in a top half is unchecked, and the timer, IPI, and keyboard handlers do not set `IN_ISR`, so a block in one of them is unchecked too |
| I3 | IF=0 through every return-to-user sequence (§5.10 rule 4) | FMASK (§7.2) | §5.10, §7.2, §2.9; `syscall_init`, `proc_init`, `console_init::wait_key` | `console_read_exit`, `user_entry_irq` | documented | Yes: the syscall exit runs `cli` directly after the dispatcher returns, so a `read` that waited in `console_init::wait_key` with IF=1 leaves with IF=0 (F001), and `syscall_init::first_return` runs `cli` before its first `gs` write (F006); a debug build faults before an exit `swapgs` that finds IF set (ROADMAP §10.6) |
| I4 | Kernel code outside the §5.10 entry and exit sequences runs with `GS_BASE` = this CPU's `PerCpu` (§5.10) | `arch::gs`, `per_cpu_init` | §5.10, §7.5; `arch::gs`, `per_cpu_init`, every `per_cpu!` access | `user_device_irq`, `user_ipi`, `ist_gs_sign` | documented | No: a fault on the return-to-user `iretq` (F007) runs on the user base (ROADMAP §10.6) |
| I5 | One entry stub per vector makes the `swapgs` decision (§5.10 rule 1) | `arch/x86_64/idt.rs` | §5.10; `arch::x86_64::idt`, I4, I30 | `scripts/check_entry.py` | enforced by construction: `idt::init` points every gate at a stub it generates, and `scripts/check_entry.py` fails on an `x86-interrupt` handler outside `src/arch/` | Yes |
| I6 | Ring 3 never halts the kernel, pid 1's exit excepted (§2.5, §5.2, §11.5) | `proc_init::try_user_fault` | §2.5, §5.2, §11.5; `proc_init::try_user_fault`, `vibeos::trap` | `user_exceptions`, `user_single_step`, `user_int1` | documented | No: the I4 windows halt (ROADMAP §10.6) |
| I7 | The kernel reads or writes user memory only through the §5.1 user-memory accessors, and writes an address space that is not running only through the fill API (ROADMAP §10.6) | `vibeos::proc::uaccess` and `arch::x86_64::uaccess` (the accessors, and the only `stac`); `vibeos::proc::fill` and `proc::fill_init` (the fill API, whose physmap primitives are private to it) | §5.1, §2.10; `vibeos::proc::uaccess`, `proc::fill_init`, every syscall body | `scripts/check_user_access.py`, `uaccess_smap_stray_fault`, `uaccess_smep_user_jump`, `uaccess_readonly_efault` | enforced by SMAP where the CPU has it (PAN on aarch64, ROADMAP §11.6); the fill-API rule and `stac`'s confinement to the accessor module by `scripts/check_user_access.py` in `make check` | Yes: the loader and `fork`'s copy write through the fill API on a `NewSpace`, which syscall code cannot reach |
| I8 | One thread per address space changes its regions, and another CPU changes its page tables only under its page-table lock (§2.11) | `addr_space_init`'s counted object (`Space`, `SpaceCore`) and its `mm` lock; process model | §2.11, §4.3, I34; `addr_space_init`, `syscall_init` user copies | none | the `mm` lock serializes region and table changes; one thread per space is assumed | Yes: only the owning thread changes a space, and a pin reaches it only under `mm`; no `&'static AddressSpace` exists (ROADMAP §10.6). Lock-free user copies and local-only `invlpg` depend on one thread per space. ROADMAP §12.1's reverse map changes page tables from other CPUs under the space's page-table lock, §12.3 shoots down every CPU in the space's set, and §13.1's address-space lock replaces `mm` |
| I9 | TCBs are never freed, so a `*mut Tcb` stays valid | the `limits::MAX_THREADS` table (1024 slots, allocated at boot), `thread_init` | §2.11, §7.5; `thread_init`, `sync_init`, `log::panic` | `lifetime_dead_slot_on_cpu` | assumed; slot reuse enforced by the in-guest `lifetime_dead_slot_on_cpu` | Yes: `spawn_inner` reuses a Dead slot only after an Acquire load finds its `Tcb.on_cpu` clear, which its CPU's `thread_init::finish_switch` clears with Release once `switch_context` has returned, and `thread_exit` stores `Dead` under SCHED (ROADMAP §10.10, F012) |
| I10 | A dead thread's stack is freed only after its CPU has switched off it (§2.8, §4.5) | `thread_init::finish_switch` | §2.8, §4.5; `thread_init::finish_switch`, `kva_init`, `work_init` | `lifetime_stack_reclaim` | enforced by the in-guest `lifetime_stack_reclaim` | Yes: `thread_exit` parks the stack in its CPU's `PerCpu.dead_stack`, and only that CPU's switch tail, after `switch_context` has returned, moves it into the CPU's stack cache or onto its dead list, which that CPU's worker frees (ROADMAP §10.10, F012) |
| I11 | A completer's publishing store is its last access to the waiter (§2.8) | `block_init::IoWaiter::finish` | §2.8, §10.1; `block_init::IoWaiter`, every block submitter | `lifetime_iowaiter_publish_last`, `loom_io_done_publish_last` | enforced by the in-guest `lifetime_iowaiter_publish_last` | Yes: `finish` runs `wake_all` under SCHED, then stores `done` with Release as its last access (ROADMAP §10.10, F002); ROADMAP §10.8's loom model `loom_io_done_publish_last` checks that the store orders the completer's accesses before the waiter's return |
| I12 | Every kernel PML4 slot exists before the first user address space | `AddressSpace::new` copies PML4[256..512) once | §4.1; `addr_space_init`, `paging_init::install` | none | assumed | Yes, by boot order only: `paging_init::install` creates none of the heap, KVA, and `ioremap` PML4 slots; each appears on its region's first mapping, and no current path makes a first mapping after `/hello` (ROADMAP §12.1, F101) |
| I13 | The low identity window is removed after `smp: done` (§4.1) | `paging_init::teardown_identity`, from `smp_init::init` | §4.1, §7.4; `paging_init::teardown_identity`, `smp_init` | `kernel_va0_faults` | enforced by the in-guest `kernel_va0_faults` | Yes: all but the trampoline page is unmapped and the TLB flushed on every CPU, global entries included, so a kernel read of VA 0 faults (ROADMAP §10.6, F085) |
| I14 | Every buddy frame and page table lies inside the physmap (§4.1) | `pmm_init::init`, `paging_init::physmap_extent` | §4.1, §4.2; `pmm_init`, `paging_init`, `vibeos::paging` | `make test-e2e-highmem` | enforced | Yes; a framebuffer above the 8 GiB cap is not covered (ROADMAP §11.2, F020) |
| I15 | Frame 0, the trampoline page, and the kernel image, framebuffers, and boot modules never enter the buddy (§2.4) | `pmm_init::init` through `vibeos::pmm::clip_usable`; `Buddy::insert_region` skips frame 0 | §2.4, §4.2, §7.4; `pmm_init`, `vibeos::pmm`, `smp_init` | `clip_usable_eight_framebuffers`, `insert_region_skips_frame_zero` | enforced; host tests `clip_usable_eight_framebuffers` and `clip_usable_unsorted_overlapping` | Yes: the trampoline page is the usable page `boot::capture` chose from the memory map, and the clip has no fixed-size list (ROADMAP §10.6) |
| I16 | The kernel PML4 lies below 4 GiB, because the trampoline loads a 32-bit CR3 | `smp_init::start_one` | §7.4; `smp_init::start_one`, the trampoline | none | documented: `smp_init::start_one` skips every AP when the PML4 frame lies above 4 GiB | Not guaranteed: the PML4 frame has no address limit, and above 4 GiB every AP is skipped with a `smp: cr3 above 4GiB` line (ROADMAP §20.1) |
| I17 | MMIO is UC, RAM is WB, and no frame has both (§2.4) | `acpi_init`, `Mapper::patch_physmap_uc` | §2.4, §4.3; `acpi_init`, `paging_init`, `pci_init` | `acpi_discovery` | documented | Partly: `pci_init::map_mmio` refuses a range that overlaps RAM, but `patch_physmap_uc` turns a whole 2 MiB leaf UC, which a BAR can share with RAM, and a trailing leaf can be skipped (ROADMAP §11.2, F104) |
| I18 | EOI before any switch; a one-shot timer is rearmed before yielding (§5.8) | timer ISRs | §5.8, §6.3; `time_init`, `apic_init`, the timer ISRs | none | documented | Yes; no test tier runs the TSC-deadline timer, the only one-shot source, so nothing exercises the rearm (ROADMAP §10.1, F078) |
| I19 | I/O APIC high dword written before the low; IST index zero-based in software, one-based in the gate (§5.1, §5.6) | `apic.rs`, `desc.rs` | §5.1, §5.6; `arch::x86_64::apic`, `arch::x86_64::desc` | `redir_writes_high_dword_before_low`, `idt_gate_packs_offset_and_ist` | enforced, host-tested | Yes |
| I20 | `now_ns` is monotonic | seqlock plus `time::monotonic_max` over `time_init::LAST_NS` | §6.4; `time_init`, every `now_ns` caller | `monotonic_max_holds_high_water`, `now_us_monotonic` | enforced | Yes, by construction, so the monotonicity tests cannot fail (ROADMAP §10.2, F100) |
| I21 | A run queue is touched only by its owner CPU with IF=0 (§2.3) | `per_cpu_init::with_current`, `per_cpu_init::cpu` | §2.3, §7.5, §7.8; `per_cpu_init`, `sched_init` | `PerCpuRemote`, `remote_view_starts_clear`, `scripts/check_current.py` | enforced (busy flag; the remote view) | Yes: other CPUs read only the atomic `runq_len` in `PerCpuRemote`, and `current()` and `try_current()` debug-assert IF=0 from `irq: enabled` on (ROADMAP §10.3, F039) |
| I22 | A `BootCell` is set once, before SMP, and holds `Sync` data (§2.3) | `cell.rs` | §2.3; `vibeos::cell`, `gdt::init_bsp`, every `BootCell` static | `BootCell`, `release_assert_bootcell_set_twice`, `bootcell_set_once`, `scripts/check_cells.py` | enforced in part (the `Sync` part: the `T: Send + Sync` bound and `cell.rs`'s assertions; the set-once part: `BootCell::set`'s `assert!`, which the host test `release_assert_bootcell_set_twice` runs with debug assertions off) | Yes: `gdt::init_bsp` fills the BSP's tables for their final address before `BSP.set`, and after publication the TSS is written only through the `UnsafeCell` `CpuTables::set_rsp0` writes, on the owning CPU with IF=0 (ROADMAP §10.3, F089) |
| I23 | The block layer orders only overlapping writes and a sequential zone's writes; a `Flush` makes durable every write completed before it was submitted, and a `Fua` write is durable when it completes (§10.2) | `block.rs` | §10.2; `block_init`, `vibeos::block`, vibefs commit | none | documented | No: C-LOOK can reorder overlapping writes (ROADMAP §10.11, F043) |
| I24 | vibefs never overwrites a live block before the newer superblock is durable, and from v2 reuses a block a commit freed only after the next commit's superblock is durable, so the older slot's tree stays whole; a v2 NOCOW file's data blocks are the one exception, overwritten in place ([VIBEFS.md](VIBEFS.md) §15) | vibefs commit | §10.2, VIBEFS.md §15; `vibefs_init`, `vibeos::fs::vibefs` | none | documented | No after a failed commit: the in-memory generation advances before the superblock write, so the retry writes the slot that holds the only valid superblock (ROADMAP §12.5, F050). Otherwise it rests on v1's on-disk refcounts, which its mount does not check (F061); v2 keeps no per-block count and checks its pointers and allocation map as it reads each block (VIBEFS.md §15; ROADMAP §14.8) |
| I25 | Per-thread CPU state is saved and restored in full (§7.5) | `syscall_init::on_switch`, `arch::x86_64::switch::switch_context` | §7.5; `thread_init::switch_now`, `syscall_init::on_switch`, `arch::x86_64::switch` | none | documented | No: `FS_BASE` is not switched (ROADMAP §11.6, F022) |
| I26 | Every kernel stack has a guard page (§2.4) | `kva_init::alloc_guarded_stack`; `thread_init::init_bootstrap` moves boot onto one | §2.4, §4.5, §9.2; `kva_init`, `thread_init::init_bootstrap` | `boot_stack_guarded`, `stack_guard` | enforced for the bootstrap thread by the in-guest `boot_stack_guarded`; documented otherwise | Yes: boot leaves Limine's stack at `init_bootstrap`, and every other kernel stack comes from `alloc_guarded_stack` (ROADMAP §10.6, F072) |
| I27 | `vibeos-core` does not panic on data (§2.5) | clippy deny on `unwrap`, `expect`, `panic`; `indexing_slicing` and `arithmetic_side_effects` denied in the byte parsers `scripts/check_core_stable.py` lists | §2.5, §2.10; every `vibeos-core` parser | `clippy::unwrap_used`, `clippy::expect_used`, `clippy::panic`, `scripts/check_core_stable.py` | enforced by clippy; vibefs v1 excepted | Partly: vibefs v1 still panics on one crafted input (F061); §2.5 lists it and its ROADMAP line |
| I28 | A line the harness takes as the kernel's is framed, and no user byte can produce the frame (§2.6) | `serial::raw`, `console_init::write`, `tests/harness/frame.py` | §2.6, §8.3; `serial::raw`, `console_init::write`, the harness | `console_forged_lines`, `tests/harness/test_frame.py`, `serial_frame` | enforced (the `/bin/tests` forged-line case, `test_frame.py`) | Yes |
| I29 | A catch hook intercepts only a CPL-0 fault on the CPU that armed it, inside an in-guest test's catch window | `arch::catch::arm` (`ARMED`, the arming CPU's token) | §8.2; `arch::catch`, the in-guest registry | `catch_ignores_other_cpu`, `catch_ignores_user_frame` | enforced by the in-guest `catch_ignores_other_cpu` and `catch_ignores_user_frame` | Yes: `arch::catch` and the dispatcher's intercept compile only with `kernel_tests`, so production has no catch hook; `intercept`, `on_panic` and `on_alloc_error` act only on the CPU whose token `ARMED` holds, `intercept` only on a CPL-0 frame, and each CPU records its catch in its own `LAST` slot. A window must not span a CPU migration, which nothing does while no preempted thread changes CPU (I36) |
| I30 | Interrupt and exception handlers run with RFLAGS.AC=0 (§5.10 rule 5) | `arch/x86_64/idt.rs` stubs | §5.10, I7; `arch::x86_64::idt` stubs, the user accessors | `scripts/check_entry.py` | enforced by construction: every stub's first instruction is `clac` where the CPU has SMAP | Yes |
| I31 | Every IF=0 stretch outside §2.9 rule 2's exemptions retires at most 100,000 instructions ([§2.9](#29-preemption-and-interrupt-state) rule 2) | §2.9 rule 2; measured by the `irqoff` build's tracer (`sched::irqoff`, ROADMAP §10.3), which `make test-irqoff` runs | §2.9; every IF=0 section, `sched::irqoff` | `make test-irqoff` | documented | No. `make test-irqoff` at 38fd409 (TCG, `-icount shift=0`, `-smp 1`, on an Intel Xeon @ 2.10GHz cloud container) logs these sites over 100,000 ns, with the ROADMAP line that chunks each: `mm/paging_init.rs:222` (`PT` held in `current_mapper` while exec maps and fills an image; up to 6.6 ms, 1,678 over per registry boot): ROADMAP §10.6's fill-API box (line 1393); `mm/pmm_init.rs:29` (`BUDDY` in `with_buddy`; up to 642 µs, about 2,600 over): ROADMAP §12.1's O(`MAX_ORDER`) `Buddy::deallocate` box (line 1715, F029); `log/log_init.rs:89` (the log ring's `IrqCell` while the shell tests read it; up to 841 µs): ROADMAP §19.5's log-store box (line 2980); `sched/thread_init/mod.rs:528` (`schedule_inner`; 82 ms at boot and after the tests that fill the thread table, and as `vec0xf0` when the tick preempts), `console/fb_init.rs:90` (the framebuffer clear at console init; 41 ms), `sched/thread_init/mod.rs:470` (`thread_exit`; up to 143 µs), and `syscall:exit` with `proc/syscall_init.rs:611` (`first_return`; 292 µs once each): no line yet (ROADMAP §12.6's tracer box waits on a box in that section for each). Not logged in that run but known: the heap's first-fit `alloc`, its address-ordered insertion on `dealloc`, and a moving `realloc`'s copy run under the IRQ-off HEAP lock over a free list whose length user churn sets (ROADMAP §12.6); the buddy's double-free check walks the free lists (ROADMAP §12.1, F029); a `klog!` emit waits on the UART with IF off, about 8 ms per 96-byte line on a 115200-baud 16550, which no QEMU tier paces (ROADMAP §19.5); a shootdown survives a violation: `wait_acks` keeps waiting and logs the CPUs that have not acknowledged once a second (§7.9, F011) |
| I32 | A handler on an IST stack never blocks, switches threads, or takes a lock, and an IST vector taken at CPL 3 leaves the IST stack before its body runs (§5.10 rules 3 and 6) | the IST entry stubs and handlers | §5.1, §5.10, §2.5; the IST entry stubs, `arch::idt`, the NMI handler | `df_on_ist`, `ist_gs_sign` | documented | Partly: an IST vector taken at CPL 3 moves its frame to the thread's kernel stack before its body (`arch::idt::vibeos_trap_entry_ist`); on a CPL-0 frame every IST handler halts, so none blocks or switches, and the NMI handler decides through the stop primitive before any write (§2.5 step 1): it returns at once on the panic dump's owner and otherwise stops, halts or dumps, with no lock; except that under `kernel_tests` an armed `catch` steps RIP and returns or longjmps off the IST stack (ROADMAP §10.6, F007) |
| I33 | A fault body reads CR2, DR6, ESR, and FAR from its frame, where the entry stub saved them before IF could turn on (§5.10 rule 9) | the `arch/x86_64/idt.rs` stubs; the aarch64 vectors (ROADMAP §11.3) | §5.10; `arch::x86_64::idt`, every fault body | none | documented | Yes: the generated stubs save CR2 and DR6 before any body turns IF on |
| I34 | A PTE change that removes or narrows a translation takes effect only after every CPU that could hold the old one has invalidated and acknowledged; until then no frame, table page, or VA is reused and no page counts as clean (§2.4) | `kva_init::unmap_shootdown` (kernel); `addr_space_init::shootdown_user` (user) | §2.4, §7.9; `kva_init::unmap_shootdown`, `addr_space_init::shootdown_user`, `ipi_init` | `tlb_shootdown_remote`, `lifetime_shootdown_ack_late`, `shootdown_ack_while_busy` | documented | Partly: kernel unmaps free frames and VA only after `wait_acks`; a user change invalidates only on the calling CPU, enough only while I8 holds, and nothing yet clears a dirty bit (ROADMAP §12.3) |
| I35 | A user PTE change invalidates the second-level translations (EPT, NPT, stage-2) of its range on every CPU that may hold them before the frame's count drops (§2.4) | none yet | §2.4; ROADMAP §21.2's hypervisor | none | documented | Not relied on yet: no hypervisor exists until ROADMAP §21.2, which lands it |
| I36 | `current` is read in one instruction, and every other per-CPU access but the CPU-id hint runs with IF=0 ([§2.9](#29-preemption-and-interrupt-state) rule 5) | `arch::x86_64::percpu` (`current_tcb`, `cpu_id_hint`), `per_cpu_init::current` | §2.9, §7.5; `arch::x86_64::percpu`, `per_cpu_init` | `scripts/check_current.py`, `current_at_if1`, `current_migrate_if1` | enforced (debug builds assert IF=0 in `current()` and `try_current()` from `irq: enabled` on; `scripts/check_current.py`) | Yes: `current_thread`, `current_id` and `current_pid` wrap `arch::current_tcb()`, and `current_at_if1` and `current_migrate_if1` read `current` with IF=1 (ROADMAP §10.3, F039) |
| I37 | Nothing is silently swallowed: an error is returned to its caller, or handled where it arises by a counter and a rate-limited line, a recorded error state, or a bounded retry (§2.5) | every module; clippy's `let_underscore_must_use` and `unused_result_ok`, denied workspace-wide in root `Cargo.toml`; test code exempt at its root (`cfg(test)` in `lib.rs`, each `kernel_tests` `ktest` `mod` line) | §2.5; every module | `clippy::let_underscore_must_use`, `clippy::unused_result_ok` | enforced (the lints, in every member; a kept drop's `#[expect]` names its case) | Partly: F124's `read_dirent` mapping turns a read error into end-of-directory, a drop no lint sees (ROADMAP §13.9) |
| I38 | A return to user mode restores only what the §5.10 rule 10 validator accepted from any writer of the saved frame, and its last check for pending work runs with IF=0 (§5.10 rule 11) | the validators in each port's pure half; the exit paths | §5.10; `syscall_init::exit_work`, each port's exit path | none | documented | Rule 10 holds vacuously: no writer of a saved user context exists before ROADMAP §13.8 and §17.4. Rule 11 holds: `syscall_init::exit_work` runs the check with IF=0 on every exit to ring 3 but an NMI's |
| I39 | On aarch64, an ASID a CPU has used since its last local TLB flush names one address space on that CPU ([§11.2](PORTABILITY.md#112-address-space-on-aarch64)) | the ASID allocator (ROADMAP §11.2) | §11.2; the aarch64 ASID allocator (ROADMAP §11.2) | none | documented | Not relied on yet: the aarch64 port does not exist; ROADMAP §11.2's host tests and loom model enforce it when it lands |
| I40 | A thread sleeps, or takes a sleeping lock, only with IF=1 and no spinlock held (§2.1, [§2.9](#29-preemption-and-interrupt-state) rule 4) | `sync_init::might_sleep`, `Sched::begin_wait` | §2.1, §2.9; `sync_init::might_sleep`, `sched_init`, every sleeping lock | `sleep_under_spinlock_asserts`, `op_gate_kill_sleeps` | enforced in debug and `kernel_tests` builds | Partly: only the entry points §2.9 rule 4 names check, a rank-0 lock or an `IrqCell` held across a sleep is not counted, and a build without `debug_assertions` or `kernel_tests` checks only the hard-IRQ flag |
| I41 | No sleeping lock of levels 2 to 4 is held across a copy to or from user memory, and code that holds the address-space lock takes no level-1 lock (§2.1) | none yet | §2.1; the address-space lock and page waits (ROADMAP §12.5, §13.1) | none | documented | Yes, vacuously: the address-space lock, page waits, and the filesystems' block-mapping locks arrive with ROADMAP §12.5 and §13.1, and ROADMAP §13.12's lock-dependency build reports a violation the first time one happens |
| I42 | Kernel-binary code that a syscall, a device, or a disk image reaches does not panic on that input, running out of memory or table slots included (AGENTS rule 4, [§4.4](MEMORY.md#44-kernel-heap)) | `vibeos::kalloc` on every path after `irq: enabled`; clippy's `disallowed-types` and `disallowed-macros` in `vibeos-core` and the kernel binary | §4.4, §2.10; `vibeos::kalloc`, every syscall, driver and filesystem path | `kalloc_nomem`, `dev_probe_alloc_fail`, `fork_oom` | enforced by clippy (ROADMAP §10.4) and the in-guest `kalloc_nomem` and `dev_probe_alloc_fail` tests | Yes: every allocation after `irq: enabled` is fallible (ROADMAP §10.4, F010), and a full thread table is an error, not a panic (ROADMAP §10.4, F037) |
| I43 | Another CPU reads a CPU's per-CPU state only through its `PerCpuRemote`, whose fields are atomics, and takes `&mut` to another CPU's `PerCpu` only through `with_cpu` while that CPU is not running (§7.5) | `per_cpu_init::cpu`, `per_cpu_init::with_cpu` | §7.5, §7.9; `per_cpu_init`, `ipi_init`, `sched_init` | `PerCpuRemote`, `remote_view_starts_clear`, `scripts/check_cells.py` | enforced (the view type, its const assertion, and `check_cells.py`'s type and must-be-unsafe lists) | Yes, except an AP that accepted a SIPI and stalled past the ready timeout (ROADMAP §11.4, F032) |
| I44 | A page-table root is freed only when no CPU has it loaded (CR3; TTBR0 on aarch64) and no TCB's `as_cr3` names it; a path that drops or replaces a thread's space records the replacement (or 0) in `as_cr3` and loads it before its `users` put | `addr_space_init::SpaceCore`'s drop, the root's free (assertion); `proc_init::finish_exit`, `proc_init::sys_execve` | §4.3, §7.9; `addr_space_init`, `proc_init::sys_execve`, `proc_init::finish_exit` | `teardown_live_root_asserts` | enforced at runtime, every build | Yes |
| I45 | A block-cache slot in WRITEBACK keeps its key, stays readable, is never evicted or re-keyed, and takes no second write until its write completes; `cache_init::flush` sends `Flush` only when the device has no dirty or WRITEBACK slot ([§10.6](BLOCK.md#106-block-cache)) | `vibeos::block::cache`; `block::cache_init::flush` | §10.6; `vibeos::block::cache`, `cache_init`, vibefs commit | `writeback_inflight_keeps_slot`, `cache_flush_waits_writeback` | enforced by the host test `writeback_inflight_keeps_slot` and the in-guest `cache_flush_waits_writeback` | Yes |
| I46 | A frame on a buddy free list is written only by the buddy, which reaches its node at `phys + hhdm_offset`; no other code holds a pointer into a free frame ([§4.2](MEMORY.md#42-physical-memory-buddy-allocator), [§9.2](PITFALLS.md#92-memory)) | `mm::pmm::Buddy::insert_region`, `mm::pmm::Buddy::free` | §4.2, §9.2; `vibeos::pmm`, `pmm_init` | none | documented | Yes, unchecked: a stray write into a freed page corrupts the lists (§9.2) |
| I47 | A block on the heap's free list is written only by `Heap`, and `[base, base + mapped)` is mapped writable before `Heap::init` or `Heap::extend` takes it ([§4.4](MEMORY.md#44-kernel-heap)) | `mm::heap::Heap::init`, `mm::heap::Heap::extend`, `mm::heap_init::grow_for` | §4.4; `vibeos::heap`, `heap_init` | none | documented | Yes |
| I48 | The kernel root's page tables are written only while the PT lock is held, boot's `install` excepted ([§4.3](MEMORY.md#43-page-tables)) | `mm::paging_init::current_mapper` | §4.3, I8; `paging_init`, `kva_init`, `heap_init` | `MapperGuard` | enforced by `MapperGuard` | Yes; user roots follow invariant I8 |
| I49 | A device register page the kernel reaches (the LAPIC, an I/O APIC, the HPET, a BAR holding an MSI-X table) is mapped UC, in the physmap or the ioremap window, before its first access, and stays mapped | `acpi_init::init` (LAPIC, I/O APIC, HPET); `pci_init::map_mmio` (BARs), inside which `irq_init::msix_table_va` bounds an MSI-X table | §2.4, §5.3, §12.3; `acpi_init`, `apic_init`, `pci_init`, `irq_init` | `acpi_discovery` | documented (the in-guest `acpi_discovery` test checks the ACPI pages' PCD and PWT bits) | Partly: `pci_init::map_mmio` drops a failed `patch_physmap_uc` and still hands out the physmap VA (the discard audit, ROADMAP §10.1) |
| I50 | Each legacy I/O port range has one owning module, and only it touches the range: the 8259s (0x20-0x21, 0xA0-0xA1) `arch::x86_64::pic`; the PIT (0x40-0x43), port 0x61 and CMOS (0x70-0x71) `time::time_init`; the ACPI PM timer's `TMR_VAL` port (the FADT's PM timer block, read only) `time::time_init`; the 8042 (0x60, 0x64) `console::kbd_init`; PCI configuration (0xCF8, 0xCFC) `dev::pci_init`; QEMU's fw_cfg (0x510-0x51B) `boot::fw_cfg_init` (invariant I57); COM1 `log::serial`. The named exceptions: port 0x80 is a delay write any of them makes, the master 8259's EOI is written by the IRQ0 and IRQ1 paths (`time_init::eoi_pit`, `kbd_init`), `arch::x86_64::power`'s power-off and reset writes (the FADT's registers, ports 0x604 and 0xB004, 0xCF9, and the 8042's reset pulse) reach ports no module owns and the 8042, and end the machine, and the `kernel_shell` debug build's port commands reach any port | `time_init::init`, `pic::program`, and each owner named | §5.5, §6.2; `arch::x86_64::pic`, `time_init`, `kbd_init`, `pci_init`, `arch::x86_64::power` | none | documented | Yes, by convention; no check finds a port write outside its owner |
| I51 | A loaded `CpuTables` (its GDT and the TSS the GDT's descriptor names), and the IST and RSP0 stacks that TSS names, never move and are never freed while their CPU is online (§5.1) | `gdt::init_bsp` (the BSP's, in a `static`); `smp_init::start_one` (an AP's, a heap `TryBox` moved into `LIVE_TABLES` before INIT) | §5.1, §7.4; `gdt`, `smp_init`, `log::panic` | none | documented | Partly: an AP that accepted its SIPI and stalled past the ready timeout has its tables and stacks freed by `smp_init::free_live_ap` while it may still load and use them (ROADMAP §11.4, F032) |
| I52 | A lock's protected value is reached only through a guard, and a guard exists only while its lock is held: one at a time for a mutex, readers or one writer for a `RwLock` | `sync_init::SpinMutex::lock`, `sync::blocking_init::BlockingMutex::lock_until`, `sync::blocking_init::RwLock::write_until` | §2.1, §2.3; `sync_init`, `sync::blocking_init` | `SpinMutexGuard`, `BlockingMutexGuard`, `RwLockWriteGuard` | enforced by construction (a guard is built only after the acquire succeeds, and `Drop` releases) | Yes |
| I53 | A virtio `SplitQueue`'s `base` is 16-byte aligned and valid for `layout.total` bytes for the queue's life, and only the queue and its device reach that memory | `virtio::SplitQueue::new` (its `# Safety` contract; the kernel passes a `dma_init::alloc` buffer); `virtio::SplitQueue::field` checks each offset against `layout.total` | §12.4; `vibeos::virtio`, `virtio_init`, `virtio_blk_init` | `SplitQueue`, `split_queue_dma_mb_order` | enforced in part (the offset check; the buffer's life is the caller's contract) | Yes |
| I54 | A VA `pci_init::map_mmio` returns maps its BAR or ECAM page, uncached unless the BAR overlaps a framebuffer, which stays write-back: an ECAM page for the rest of the boot, a BAR while its claim is held, and `pci_init::unmap_bar` removes an ioremap BAR only after its driver has stopped the device | `pci_init::map_mmio` (the UC physmap patch or `ioremap`); `dev_init::claim_mem_bars` records a BAR's VA in the entry that holds its claim, and `dev_init::release_bars` takes it back | §12.3, I49; `pci_init`, `dev_init`, the virtio drivers | none | documented | Partly: a BAR below `map_end` keeps its physmap leaf after `unmap_bar` (ROADMAP §11.2) |
| I55 | A block `Request`'s segments name memory valid for their lengths that nothing else touches until the request's completion runs | `virtio_blk_init::VirtioBlk::build` and `block_init`'s submit paths, whose callers keep the buffer until `IoWaiter::wait` returns or the completion runs | §10.1; `block_init`, `virtio_blk_init` | none | documented | Yes: every in-tree submitter waits on its `IoWaiter` before it reuses the buffer |
| I56 | A volume instance's volume (`fs::fat_init::FatVolume`, `fs::vibefs_init::VibeVolume`) is reached only through its `BlockingMutex`, which owns it, or, before the instance is shared, by the one boot or mount path building it | `fs::fat_init::new_volume`, `fs::vibefs_init::new_volume` | §10.1; `fat_init`, `vibefs_init` | `BlockingMutex` | enforced by the type (the lock owns the volume) | Yes: `drop_slot` retires a volume under its lock, waiting for its holder |
| I57 | QEMU's fw_cfg ports (the selector 0x510, the data byte 0x511, the DMA address 0x514-0x51B) are touched only by `boot::fw_cfg_init`, only after CPUID.1:ECX[31] reports a hypervisor, so bare metal never sees a write to them, and by one CPU at a time: `boot::capture` before `smp: done`, then boot-time callers and the in-guest registry. The selector is device-global and unlocked, so a caller that can race takes a ranked lock first (§2.1) | `boot::fw_cfg_init::probe` (the CPUID check before any port access), `boot::fw_cfg_init::select` | §3.2, §2.1, I50; `boot::fw_cfg_init`, `log::pvpanic_init` | `fw_cfg_probe`, `fw_cfg_dma` | enforced in part (`select` debug-asserts the hypervisor bit; one CPU at a time is by convention) | Yes |
| I58 | A device's memory BAR is mapped only through a live `BarClaim`, which `Registry::claim` grants under `REG` only when the BAR overlaps no other claim and no RAM-typed range of the boot memory map (DEVICES.md §12.3) | `dev::Registry::claim`, `pci_init::map_bar` | §12.3, I54; `dev_init`, `pci_init::map_bar`, every driver's `probe` | `BarClaim`, `dev_bar_claims`, `pci_claim_exclusive` | enforced | Partly: a BAR below `map_end` stays reachable through its physmap leaf, and ECAM pages are mapped through `map_mmio` with no claim (ROADMAP §11.2) |

## 2.8 Publish last

The rule for every completion, hand-off, and deferred reclaim:

1. In a completion or hand-off, the store that lets the other side return, free, or reuse an object
   is the publisher's last access to that object; after it the publisher touches only its own stack
   and statics. The other side can see the store on a lock-free path (`IoWaiter::poll`, any Acquire
   load of a done flag) and return before the publisher's next instruction, so a lock taken after
   the store does not make a later access safe. The store is a Release store, a Release
   read-modify-write, or the unlock of a lock that the other side takes before it frees or reuses
   the object, and the other side reads it with Acquire. Program order alone does not make a store
   the last access: on a weakly ordered CPU, a load or store the publisher made to the object
   earlier can still be performed after a Relaxed store is visible.
2. An object that its owning CPU may still use (the kernel stack it runs on, a TCB that is switching
   out, an AP's stacks and tables during bring-up, and the old kernel's code, tables, and stacks
   when a planned kexec hands its memory to the new kernel, ROADMAP §25.4) is freed only after that
   CPU has passed a point the reclaimer can observe: its `switch_context` away from the object has
   returned, or INIT has stopped the AP. A global list that any CPU drains does not meet this rule.

Small portable types in `vibeos-core` carry these stores and loads, each method with its order and
no caller naming one, so ROADMAP §10.8's loom models run the kernel's own code. A cross-CPU wake is a
hand-off: `vibeos::irq::ipi::WakeInbox::push` sets the slot's bit and then its word's summary bit
with Release read-modify-writes, and the owner CPU's `WakeInbox::drain`, masked, takes them with
Acquire swaps. Rule 1's completion store is `vibeos::block::DoneWord::publish` (a Release store, in
`IoWaiter::finish`), and its reader is `DoneWord::poll` (an Acquire load). Rule 2's switch-out store is
`vibeos::sched::thread::OnCpu::clear` (a Release store in `thread_init::finish_switch`, the switch
tail's last access to the TCB), and its readers, `spawn_inner`'s Dead-slot reuse check among them,
use `OnCpu::is_clear` (an Acquire load).

Rule; not yet enforced. The violations, and the ROADMAP lines that fix them:

- On the bring-up timeout, `smp_init::start_one` frees an AP's kernel stack, GDT/TSS, and IST and
  RSP0 stacks without an INIT, so an AP that is still running uses freed memory (ROADMAP §11.4,
  F032).

## 2.9 Preemption and interrupt state

IF here means this CPU's maskable-interrupt enable: RFLAGS.IF on x86_64, and PSTATE.I clear on
aarch64, where the kernel masks and unmasks F with I
([§11.5](PORTABILITY.md#115-aarch64-exceptions-and-privilege-transitions)) and ROADMAP §25.5's pseudo-NMIs later
make `InterruptGuard` mask by priority instead. The rules below hold on both architectures.

The kernel is preemptible wherever IF=1. The timer tick and the reschedule IPI call
`schedule_preempt`, which may switch away from any thread whose `irq_nest` is 0: kernel threads,
syscall bodies, and fault handlers alike. One IF=1 stretch is not preemptible: from
[§3.3](BOOT.md#33-_start-order)'s step 13b to step 15 (`irq: enabled`), the BSP takes the tick with IF=1,
but `on_timer_tick` switches nothing until the idle thread exists. Boot bounds it. Planned (ROADMAP
§20.9): another exception, `efi_rt` inside a firmware call, which runs with IF=1 and is not
switched away from until the call returns ([§4.8](MEMORY.md#48-firmware-runtime-services)). Turning
interrupts off is how code says "not now", so every IF=0 stretch has a reason from the list below
and a bound.

1. IF=0 only in: an interrupt or exception entry or exit stub; a hard-IRQ top half (§2.2); a
   spinlock or `IrqCell` critical section (§2.3); an `InterruptGuard` section that must not be
   preempted or moved to another CPU: a per-CPU access (rule 5), a change to this CPU's registers
   that must match the running thread (FP state, `FS_BASE`), the [§7.9](SMP.md#79-tlb-shootdown)
   shootdown wait, the [§7.6](SMP.md#76-ipis) call-function wait, or the
   [§7.11](SMP.md#711-cpu-offline-and-online) offline rendezvous; the scheduler's switch path;
   `thread_init::with_sched`'s delivery of the wakes its closure recorded, bounded by the fixed
   `places` array, so a caller its closure blocked is not switched off before those wakes are
   placed; the
   return-to-user sequences of [§5.10](INTERRUPTS.md#510-privilege-transitions) rule 4, which begin at rule 11's
   last exit-work check; the panic and halt paths
   (§2.5); and a CPU's bring-up before its first `sti` (the BSP before §3.3's step 13b, an AP
   before it enters idle).
2. An IF=0 stretch does a bounded amount of work: at most 100,000 instructions from the instruction
   that turns IF off to the one that turns it back on, tens of microseconds on a current core. The
   panic, halt, and bring-up paths of rule 1, the [§7.9](SMP.md#79-tlb-shootdown) shootdown and
   call-function waits, and the time a spinlock acquire spends spinning, which the holders' own
   bounds limit, are exempt. No loop whose trip count a user, a device, or a disk image controls
   runs with IF=0, and nothing waits for another CPU with IF=0 without servicing incoming IPIs
   ([§7.9](SMP.md#79-tlb-shootdown)). A TLB invalidation is such a wait on x86_64, so on both
   architectures it is called only where §7.9's calling contract allows. A long job holds its lock for one bounded chunk at a time and turns
   IF back on between chunks; a walk over a user address space's page tables holds the space's
   page-table lock for at most one leaf table (512 entries) at a time. The fill API and `fork`'s
   copy follow the same hold: `PT` for one batch of at most 64 pages inside one leaf table, then the
   batch's bytes with `PT` dropped and IF on (ROADMAP §10.6). ROADMAP §10.3's IF-off tracer
   measures every stretch in the `irqoff` build (the `irqoff` Cargo feature; without it every hook
   compiles to nothing). `sched::irqoff::off` reads the cycle counter where IF goes from 1 to 0:
   `InterruptGuard::enter` (so every `SpinMutex` and `IrqCell` acquire), `cpu::cli`, the raw `cli`
   of the idle loop and `wait_key`, each IDT entry stub whose interrupted RFLAGS had IF=1, and the
   syscall entry and exit stubs. `on` reads it where IF returns to 1: `InterruptGuard`'s restoring
   drop, `cpu::sti`, the idle and `wait_key` `sti; hlt` pairs, the switch into a thread whose
   `irq_nest` is 0, the IDT exit to a frame with IF=1, and the syscall exit before `sysretq` or
   `iretq`. The stretch between is logged against the site that turned IF off (its `file:line`
   through `#[track_caller]`, `vec0xNN` for a stub, `syscall:entry` or `syscall:exit`) when it is
   longer than 100,000 ns of cycle-counter time. The exemptions are subtracted or skipped: the hooks
   do nothing once the panic dump's `HALTING` is set; a CPU arms at its first `sti`; time in
   `wait_acks`, in `call_mask`'s slot wait and in `SpinMutex::lock`'s spin is subtracted; and an
   in-guest test or test hook that holds IF off on purpose takes `sched::irqoff::deliberate`
   (`kernel_tests` builds only), which marks its stretch so it is never over. The instructions from
   `syscall` to the entry's first stamp and from the exit's last stamp to `sysretq` or `iretq`, and
   an interrupt stub's few instructions before its dispatcher and after it, are not measured. The
   bound is checked under TCG with `-icount shift=0` on one CPU (`make test-irqoff`), where guest
   time advances 1 ns per instruction retired, so 100 µs of guest time is exactly the bound whatever
   the host's load. Rule; not yet enforced: ROADMAP §12.6 turns the check on, and I31 lists the
   violations.
3. A syscall body runs with IF=1. After `swapgs`, the entry stub copies the user RSP from
   `PerCpu.syscall_scratch` into its frame on the thread's kernel stack, then runs `sti`; from there
   on the scratch belongs to whichever thread next enters on this CPU. The exit stub runs `cli`
   before it loads the user RSP (§5.10 rule 4), and keeps its state in the thread's user frame
   (§5.10), not in the scratch. A fault or trap taken at CPL 3 runs its body with IF=1 once its
   frame is on the thread's kernel stack with the fault's syndrome saved in it (§5.10 rule 9), and
   so does a `#PF` taken at CPL 0 inside a faulting user-memory accessor (§5.1) once ROADMAP §12.2
   lets it sleep, since the code it interrupted ran with IF=1; a fault inside a non-faulting
   accessor runs no body: the handler finds its exception-table entry before it touches IF and goes
   to the fixup; a hardware interrupt's top half keeps IF=0. Built so: the syscall entry runs `sti`
   directly after the last push of the user frame, and `idt::trap_dispatch` runs `sti` before the
   body of a vector 0 to 31 taken at CPL 3 that does not enter on an IST stack, and of a CPL-3
   `#DB`, whose stub has moved the frame off its IST stack, once `catch::intercept` has declined
   the frame. The syscall exit keeps its return value in the user frame's `rax` slot.
4. Code that may sleep (waits on a wait queue, takes a sleeping lock (§2.1), allocates with
   reclaim (ROADMAP §12.6), or copies through a faulting user-memory accessor (§5.1) once ROADMAP
   §12.2 lets its fault sleep) runs with IF=1, no spinlock held, and outside any RCU read-side section
   ([§2.12](#212-rcu)). `sync_init::might_sleep`, called first in `park`, `BlockingMutex::lock_until`,
   `RwLock::read_until` and `write_until`, `Semaphore::acquire_until`, `Condvar::wait_until` and
   `wait_unless`, and `Channel::send_until` and `recv_until`, before any lock, asserts that the thread is not in a device
   top half in every build, and that this CPU's `HELD` rank mask is empty and IF is on in debug and
   `kernel_tests` builds. `Sched::begin_wait`, which runs under SCHED with IF=0, checks the same on
   the `SleepCtx` that `with_sched` recorded before it took SCHED. A rank-0 lock or an `IrqCell` is
   not counted, and the `try_*` calls, which never sleep, are unchecked. The read-side check comes
   from ROADMAP §19.5.
5. `current`, the running thread's TCB pointer, is read only through `arch::current_tcb()`: one
   instruction that preemption cannot split, whose answer the switch keeps right on every CPU. On
   x86_64 it is one `gs`-relative load of `PerCpu.current` (`mov reg, gs:[offset]`), never `gs:[0]`
   followed by a field load; on aarch64 it reads `SP_EL0`, which holds the running thread's TCB
   pointer whenever the CPU runs at EL1 (EL2 under VHE), as on Linux arm64 (ROADMAP §11.6).
   `arch::cpu_id_hint()` is the one other per-CPU read allowed with IF=1, for callers that tolerate
   an answer that is already stale, such as the log prefix and a queue choice: it loads the
   immutable `PerCpu.cpu_id` through this CPU's per-CPU base and returns the value, never a
   reference. Every other per-CPU access, taking a `&PerCpu` and indexing a per-CPU table by CPU id
   included, happens with IF=0 (`with_current`, `IrqCell`), and the reference does not outlive that
   stretch, because a preemption with IF=1 can move the thread to another CPU between the lookup and
   the use. `per_cpu_init::current_thread`, `thread_init::current_id` and `current_pid` are
   wrappers over `arch::current_tcb()`. Enforced (ROADMAP §10.3, F039): from `irq: enabled` on,
   `per_cpu_init::current()` and `try_current()`, and so the `per_cpu!` macro, assert IF=0 in debug
   builds (`per_cpu_init::arm_if_checks`); `scripts/check_current.py` fails on a read of
   `PerCpu.current` outside `src/arch/`; and the in-guest tests `current_at_if1` and
   `current_migrate_if1` read `current` with IF=1, the second while the `kernel_tests` requeue hook
   moves each preempted thread to the next online CPU.

Why this model: a syscall body that runs with IF=0 cannot acknowledge a TLB shootdown (F011) or
take the tick (F044), so every long syscall (`fork`'s copy, `execve`'s load, a large `read`)
would need its own IF-on window, and ROADMAP §12.2's fault path must sleep. Linux runs syscalls
with interrupts on and preempts wherever no lock is held, so its behaviour settles edge cases.
Rejected: keeping syscall bodies at IF=0 and adding a polling window to each long call, which is
how ROADMAP §10.6 first fixed console `write`; it has to be repeated in every call that loops and
it still starves the tick. The bound counts instructions, not time, so its check gives the same
answer on every run. Rejected: a time bound checked on the ROADMAP §10.1 KVM leg, whose hosted runner is
itself a VM that can deschedule a vCPU mid-stretch.

## 2.10 Trust boundaries

Whom the kernel trusts, and the ROADMAP line where each boundary hardens. Until ROADMAP §18.8's
`docs/THREAT_MODEL.md` exists, this table is the threat model, and that document grows from it.
"Untrusted" means the source may send any bytes, at any time, as often as it likes, and the kernel
must neither halt nor corrupt memory it has not given to that source (AGENTS.md rule 4).

| Principal | Trusted for | Can do today what a hardened kernel stops | Hardens in |
|---|---|---|---|
| Ring-3 code | Nothing: it must not halt or corrupt the kernel (I6) | Every process is root, so it can read any file and signal any process. The ring-3 halts the kernel review found (F004 to F010) are fixed by ROADMAP §10.4, §10.6, §10.10, and §10.11 | ROADMAP §13.9 (uids), §18.6 (capabilities, `seccomp`) |
| Disk images and partition tables | Nothing: a parse returns `Corrupt` | Panic the kernel with a crafted image or table that root mounts or attaches (F061, F117) | ROADMAP §13.9 (partition tables), §14.8 (vibefs v2 validates every block it reads; v1 is retired); §18.7 (a LUKS2 header is parsed by the initrd's unlock tool, never by the kernel) |
| Devices: config space, rings, registers, interrupts | Nothing for halts (rule 4); everything for DMA | Read or write any physical memory by DMA; forge an MSI on a vector no handler owns, which is counted, EOIed, and logged at most once a second per vector (§5.2), so a storm costs CPU time but never halts | ROADMAP §18.1 (IOMMU, interrupt remapping, used-ring checks, F048) |
| Firmware tables: ACPI, device tree, SMBIOS, the memory map | What they describe, but not their bounds: a malformed table is refused, never followed out of range | Halt boot with a malformed table before the IDT exists (F136) | ROADMAP §11.1 (early exceptions report themselves), §20.1 (table bounds) |
| The network | Nothing, from the first packet | Not reachable yet | ROADMAP §15.10 fuzzes every parser from the start; §15.4, §15.6, and §15.8 defend against off-path guessing (keyed sequence numbers, ports, and IP IDs, RFC 5961, SYN cookies, checked PMTU messages, randomized DNS ids), which §15.10's simulated attacker checks |
| Speculation and timing side channels | Out of scope: no KPTI and no Spectre or MDS mitigations; the kernel half is mapped in every user address space (F024, F025, F131, F132) | Read kernel and other processes' memory on an affected CPU | ROADMAP §18.3 |
| Limine, the firmware, the CPU, and, under a VM, the VMM that provides them | Everything | Not applicable | ROADMAP §18.7 measures and verifies the boot chain: the signed Limine binary, its enrolled configuration and command line, the kernel, the initrd, and a kernel `kexec_file_load` starts (§25.4). Each slot's root, the state partition, and the key-set root record on them are outside it (ROADMAP §18.7's owner decision) |
| The host running QEMU, the harness, and CI | Everything; they are the test oracle | Not applicable | Never |
| Agent sessions, and the accounts they act through | Writing code, and opening and merging pull requests through the repository's rulesets; nothing else: no release or phase tag, deployment approval, repository setting, ruleset, environment, or secret | Act as the owner on GitHub: agents run with the owner's account and token, so every owner-only control in the release chain (the `release` environment's approval, the tag rules, ROADMAP §22.5's setting) is one prompt injection away | The agent-boundary decision below, before ROADMAP §14.6's first key |
| Code under test: candidate commits, agent-written code and tools, guests, third-party build systems, as seen by the machines that run them | Nothing: on a CI runner or a rig VM it reaches no secret, credential, or host service beyond its job; on the dev host, no credential beyond the agent account's own | On the dev host, read every credential of the owner's account: the `gh` token, git's credentials, SSH keys, a signed-in browser | The agent-boundary decision below (dev host); ROADMAP §10.1 and §14.6 (release jobs); ROADMAP Funded goals, Self-hosted runners (rig) |

One device can end the system: the one that holds the root filesystem. Once it is `Failed` or
`Gone`, an init that has not locked its memory dies of `SIGBUS` at its next page-in of a page
reclaim dropped, and the pid-1 panic ([§2.5](#25-panic-policy)) follows, which ROADMAP §22.2's
`panic=` turns into a reset. The root device supplies the code init runs, so its loss crosses no
trust boundary. The remedy is a redundant root (ROADMAP §29.2), not an exemption for init, which
would fault again at once.

Consequence: until Phase 10 closes, a process can crash the kernel (the ring-3 row), and until
Phase 18 closes, nothing stops one from reading another process's data. README says not to run
untrusted code on it or keep secrets on it.

**Interim posture (owner decision, 2026-09-23, [design review G006](reviews/DESIGN_REVIEWS.md)).** The
owner accepted the open gaps in the table above until the ROADMAP lines that close them, the last in
Phase 18: speculation side channels (with no KPTI, a user process on a Meltdown-affected Intel CPU,
bare metal or under KVM, can read all RAM through the physmap), DMA that no IOMMU confines (ROADMAP §18.1),
every process running as root (ROADMAP §13.9), and root-mounted crafted images that panic the kernel. Why: the
kernel has no users and no secrets; QEMU's TCG, the harness default, does not model the speculation
Meltdown needs, and under KVM the exposure depends on the host CPU; and ROADMAP §10.6 rewrites the
entry path as one generated stub per vector, which keeps a later KPTI CR3 switch local. Rejected:
moving KPTI and syscall-index masking (ROADMAP §18.3's F024 and F025 boxes) into Phase 13, which costs a slice
of entry-path work, a CR3 switch on every entry, and a measurable syscall slowdown on affected CPUs;
and moving all of ROADMAP §18.3 before Phase 14's `login`, which costs most of a phase ahead of the
self-hosting work.

The acceptance assumes one user. It goes back to the owner before `login` lands (ROADMAP §14.3), when
a second user can share the machine. Keeping any gap past the line that closes it, or adding a gap,
is likewise the owner's decision, not an agent's.

**Agent boundary.** What separates agent sessions, and the code they write, from the owner's
credentials and from wherever the root key is made or used (ROADMAP §14.6) is the owner's decision, in
the block below. Until it is recorded, agents run under the owner's macOS account and GitHub identity,
as the two rows above say, AGENTS.md's Identity rules hold as policy that nothing enforces, and
ROADMAP §14.6's custody box waits for the answer, so no key exists before it.

> **OWNER DECISION NEEDED (review J002)**: agents act as you. They run shell commands under your
> macOS account and push, open, and merge pull requests with your GitHub token, and GitHub checks
> every owner-only step by account: the `release` environment's approval (H007), who may create
> release and phase tags, rulesets, and the private-reporting setting (ROADMAP §22.5). A prompt
> injection in an issue, a pull request comment, or a web page an agent reads could push a release
> tag and approve its own release run in your name. The root key, as ROADMAP §14.6 wrote the
> procedure, would be made on that same account with a tool agents wrote, so anything an agent runs
> could read it, and it is the one key an installed system cannot recover from.
>
> Recommended, at $0: (1) Agents get a GitHub machine account (GitHub's terms allow one beside your
> personal account) with this repository's Write role only: no Maintain or Admin role, no
> environment reviewer, no ruleset bypass. It uses a fine-grained token if GitHub issues one to a
> collaborator on a personal-account repository, and otherwise a classic token with the `repo` and
> `workflow` scopes, whose reach is that Write role; moving the repository to a free organization is
> the other way. None of your own GitHub credentials stays on the macOS account agents run under: not
> the `gh` token, git's keychain entries, SSH keys registered to your account, or a github.com
> session in a browser an agent tool can drive. You push tags and approve release runs from a second
> macOS account or GitHub Mobile. A GitHub App in place of the machine account gives the same
> boundary, with its private key where agents mint tokens. (2) The root key is made and used only in
> that second macOS account, which holds no agent tooling, with macOS's own `/usr/bin/ssh-keygen`,
> which System Integrity Protection keeps agents from changing; it is written passphrase-encrypted
> to removable media and used only while no agent session runs. Cost: two accounts, moving your
> credentials once, and commits and pull requests that show the agent account as their author.
>
> If you decline (1), the H007 record says that anyone holding your token, agent sessions included,
> can approve a release run, and AGENTS.md's maintainer-only steps stay policy that nothing
> enforces. If you decline (2), the root key is made where you choose, and THREAT_MODEL (ROADMAP
> §18.8) lists a root key on a machine agents use as a known escalation path. No key exists before
> `v0.14.0`, and ROADMAP §14.6's custody box waits for this answer.

## 2.11 Object lifetimes

How an object that more than one thread or CPU can reach is created, shared, and destroyed. §2.8
covers the last store of a hand-off; these rules cover the rest.

1. One owner, or a count. An object one thread uses is owned by it (a stack value, a `Box`). An
   object that more than one thread or CPU can reach (an address space, an open file description, an
   inode, a mount, a device instance, a pipe) is reference-counted, and its last put ends it
   (rule 3). A mount and the filesystem instance it shows, its superblock, are separate counted
   objects: several mounts (another namespace's copy, a bind mount, a second mount of the same
   device) can show one superblock, which lives while any mount, inode, or open file holds it. A
   process's working directory and root are counted references to a directory (a mount and a
   dentry), never path strings: a relative walk starts from the working directory and an absolute
   one from the root, `fork` copies both references, rename moves the dentry a reference holds, and
   `chroot` (ROADMAP §14.9) and `pivot_root` (ROADMAP §18.6) replace them. Only a process's own
   thread replaces or drops its root and working-directory references (until ROADMAP §13.1's
   `CLONE_FS`), so a syscall reads them from the process table without taking a count. The
   count is `kalloc`'s `TryArc` (increment `Relaxed`, decrement `Release`, and an `Acquire` fence
   before the release, as `alloc::sync::Arc` does), a table slot's own count (rule 2), or a
   frame's count in ROADMAP §12.1's frame array. A count with other rules, such as a get-unless-zero count whose last
   put runs a teardown before the memory goes, rule 3's operation gate, or a per-CPU count, is
   written once as a shared type, beside `TryArc` in `kalloc` or beside `BlockingMutex` in `sync`,
   with host tests and a loom model (ROADMAP §10.8), and then reused; no subsystem writes its own
   (AGENTS.md rule 10). The get-unless-zero count is `kalloc::UsersArc`, with `kalloc::CoreArc` for
   the core references beneath it. `&'static` refers only to what lives for the whole run: a static item, a
   string literal, the contents of a `BootCell`, or memory allocated at boot and never freed. It
   never refers to heap memory a table owns, and it is never built from a raw pointer
   (AGENTS.md rule 6).
2. Tables hold references or quiescent slots. A lookup structure (the process table, the TCB table,
   the dentry cache, the device registry) holds a counted reference, or a slot it reuses only once
   the object's count is zero and no CPU still runs on it or through it (a TCB's `on_cpu` flag,
   ROADMAP §10.10, which a tracer and the core-dump writer also wait on before they touch the
   thread's saved state, [§7.5](SMP.md#75-per-cpu-data)).
3. Two teardowns. An object whose life ends when its users are done (an address space, a pipe, an
   unlinked inode, a mount after a lazy unmount) is released by the last put; when RCU readers can
   also find it, it is unpublished first, and its memory is freed a grace period after that put
   ([§2.12](#212-rcu)). A lookup structure that can still find it without holding a count loses it
   first: the release unpublishes it and waits for the lookups in progress, under that structure's
   lock or by rule 2's quiescent slot, before it frees anything. Until the last put, a reference
   still held keeps working. An object removed while references remain, because what backs it is
   gone or has been told to go (a device, a network interface), is killed in this order: unpublish
   it from every lookup structure, so no new reference can be taken; close its gate, the count of
   operations in progress that every operation through a reference enters and leaves, so that every
   new operation fails at once with the object's error, and wake every thread sleeping on the
   object, so that each one rechecks the gate and fails; wait only for the operations already inside
   (an active-operation count, or, for lockless readers, an RCU grace period, §2.12), each of which
   ends within the object's own bound, such as a request's deadline ([§10.3](BLOCK.md#103-failure)); release
   what it holds (frames, vectors, DMA buffers, stopping the device first,
   [§5.4](INTERRUPTS.md#54-irq-registration)); and free its memory at the last put, whenever that comes, and a
   grace period later when RCU readers could find it (§2.12). No step waits for a reference to drop,
   or for a lock that an operation in progress needs in order to finish.
4. Ids are not pointers. A pid, tid, descriptor, or device id that crosses the syscall boundary or
   sits in a table is looked up on each use, never cached as a pointer. Pids and tids share one id
   space and one allocator, and a process's pid is the tid of its first thread. They are allocated
   in increasing order up to `pid_max` (ROADMAP §10.4 gives its value) and then wrap to 300, skipping
   every id in use, as Linux does with its `RESERVED_PIDS`, so a freed id is not handed out again at
   once. An id is in use while a process or thread, a process group, or a session carries it, as
   Linux keeps its `struct pid`. A
   pidfd (ROADMAP §23.1) refers to that id object, not to the number, so once the process is reaped
   it answers `ESRCH` and never reaches a later holder of the number.
5. Asynchronous work owns what it touches. A request, timer, work item, or completion that outlives
   the call that started it holds counted references to every object it will touch (ROADMAP §12.5's
   owned block submission). It drops each one after its last access to the object
   ([§2.8](#28-publish-last)) and outside any spinlock section: [§10.1](BLOCK.md#101-completions)'s
   completer drops its waiter and buffer references after it leaves SCHED.
6. Release runs only where it may free and sleep. A last put runs the object's release, which frees
   memory and may sleep or take a sleeping lock. It runs in place only where direct reclaim may run:
   IF=1, this CPU's `HELD` rank mask empty, and a thread that is not a no-reclaim thread and is
   outside any RCU read-side section ([§4.4](MEMORY.md#44-kernel-heap) rule 1). Anywhere else (a hard-IRQ top
   half, a timer or IPI callback, a spinlock section, a threaded bottom half or softirq-equivalent
   item, a writeback thread, a thread already in reclaim, an RCU read-side section), the put defers:
   it links the object onto this CPU's deferred-release list through a node the object's allocation
   carries, so it allocates nothing, and queues a work item that runs the release with IF=1. It
   queues the item at once where this CPU may take `SCHED`; under `SCHED` or a lock ranked after
   it, and in §2.2's last row, it leaves the list for this CPU's next timer tick to queue.
   `TryArc`'s drop makes this check and defers by itself, because a completion or a timer cannot
   know that its put is the last and Rust drops values implicitly; `put_deferred` is the explicit
   form, for code that knows its put may be the last. A count whose release only returns memory to
   an allocator (a frame's, ROADMAP §12.1) follows that allocator's lock rank instead (§2.1). Any
   other count type says which rule it follows, and in debug builds its last put asserts that it may
   release where it is. `kalloc::UsersArc`'s last put runs its teardown in place, never deferred,
   and in debug builds asserts `TryArc`'s release-context test first; the core beneath it is a
   `TryArc` and follows `TryArc`'s rule. Linux defers the same way (`fput` through `delayed_fput`, and
   `mmput_async`).

An address space has two counts, as Linux's `mm_users` and `mm_count`. `users` counts the threads
that run in it and a remote accessor's temporary pin (ptrace, `/proc/<pid>/mem`, `process_vm_readv`,
procfs `maps` and `smaps`). A pin is taken only by get-unless-zero, so it fails once the last thread
has left, and it lasts one access, so no remote reader keeps a dead process's memory. The last
`users` put runs the teardown. The core is a `TryArc` that each region and each `users` holder
references, and so does a CPU that keeps the root loaded after its thread leaves; it holds the root
table, the page-table lock, and the CPU set. Freeing the core frees the root and the struct and runs
no teardown. Teardown runs in one order over the whole space: zap every region's PTEs, keeping the
frames they drop until the shootdown completes (ROADMAP §12.3); unlink every region from its
reverse-map object, taking that object's lock for writing; free the page-table pages below the root,
clearing each parent entry under the space's page-table lock; then drop the regions and their core
references. `munmap` follows the same order for its range and frees only the table pages that no
remaining region covers. The reverse map is a lookup structure in rule 3's sense, and unlinking
under its write lock is rule 3's unpublish together with its wait for operations in progress. So a
reverse-map walker takes no count: it holds the object's reverse-map lock for reading while it
borrows each region's core and takes that core's page-table lock, and a region it can see still has
its core and its page tables. A walker that took `users` could drop the last one and run the
teardown inside direct reclaim ([§4.4](MEMORY.md#44-kernel-heap) rule 2). Code running inside direct reclaim
or the OOM killer releases nothing: a put there that may be the last hands the release to a
workqueue worker. As built (ROADMAP §10.6, F019): `addr_space_init::Space` is a `kalloc::UsersArc`
over `SpaceCore`, which holds the root frame and, under a sleeping `mm` lock, the regions and the
page tables below the root; the process's thread holds one `users` reference in its process-table
slot, and a pin is `CoreRef::pin`. Each region holds a `CoreRef`. The last `users` put frees every
user leaf and table page under `PT` and clears the regions, and the core's free asserts that no CPU
has the root loaded and no TCB names it (I44) before it frees the root. Code reaches a running
space only through a scoped guard (`proc_init::with_current_space`), and no `&'static` refers to
one. Until ROADMAP §12.1 one global `PT` lock serves every space, and the space holds no CPU set.

Why: the kernel review's CRITICAL and HIGH lifetime findings (F002, F012, F019) each came from an
object that one subsystem freed or reused while another could still reach it, under a scheme that
subsystem invented. A removal waits for the operations in progress and not for every reference,
because a mount or an open descriptor keeps its reference for as long as it likes; Linux kills a
block queue or a network device the same way. A release in atomic context is deferred, not
forbidden, because a completion or a timer cannot know that its put is the last. The check sits in
`TryArc`'s drop because Rust drops values implicitly, and it is §4.4 rule 1's test, so one predicate
decides where reclaim and release may run. Rejected: keeping one scheme per subsystem, which is how
those bugs arose; never freeing (today's TCBs), which aliases a dying object as soon as its slot is
reused; a removal that waits for the count to reach zero, which hangs behind any holder; and
deferral only at call sites marked by hand, which misses an implicit drop. RCU (§2.12) is kept for
lockless readers only, and it adds a grace period to their objects' counts rather than replacing
them; everything else uses counts alone.

Today the code breaks rules 1, 2, and 5: TCBs are never freed and their slots are rewritten in
place (I9; ROADMAP §10.10, F012), and block completions point into stack frames
(ROADMAP §12.5, F042). `kalloc::TryArc` implements rule 6's deferred release, and `sync::OpGate`
rule 3's operation gate (ROADMAP §10.4).

## 2.12 RCU

Lockless readers (ROADMAP §19.5: the dentry cache, the mount table, the routing table) find objects
without a lock or a count, so an object they can reach is freed only after every reader that might
hold it has finished. RCU is how the kernel knows. vibeOS's RCU is preemptible, as Linux's is in a
fully preemptible kernel.

1. A read-side section increments a nesting count in the TCB on entry and decrements it on exit. A
   reader runs with IF=1 and may be preempted, but it never sleeps: it takes no sleeping lock
   (§2.1), waits on no queue, copies no user memory, and allocates only without reclaim
   ([§4.4](MEMORY.md#44-kernel-heap) rule 1). Code running with IF=0 is a reader too, because a CPU with IF=0
   passes no quiescent state except at the switch point of a context switch.
2. A CPU passes a quiescent state at a context switch whose outgoing thread's count is zero, in its
   idle loop, on a return to user mode, and at a tick that interrupts a thread whose count is zero.
   A CPU in tickless idle or offline (ROADMAP §19.6) is quiescent throughout.
3. A thread switched out with a nonzero count joins the blocked-reader list and leaves it at its
   outermost exit, on whatever CPU it then runs. The list has its own spinlock, ranked after SCHED
   because the switch path takes it inside SCHED; the rank enters §2.1 with the code. It is one list
   until ROADMAP §27.5's tree gives each leaf its own. A grace period ends when every CPU has passed
   a quiescent state since it began and no reader that entered its section before it began is still
   on the list.
4. A preempted reader holds every grace period open until it runs again. A reader that has held the
   current grace period open past a bound is boosted through ROADMAP §19.4's priority inheritance to
   the priority of RCU's grace-period thread until its outermost exit, as Linux's RCU priority
   boosting does. Real-time threads above that priority can still hold a grace period open for as
   long as they run, as on Linux.
5. An object that RCU readers can find and that is counted (§2.11 rule 1) is freed only after it is
   unpublished and its count has reached zero, and then a grace period has passed. A reader takes a
   count only with get-unless-zero, an increment that fails when the count is already zero, so it
   never revives an object whose last reference is gone.
6. An RCU path walk never waits on I/O or a sleeping lock. On a dentry-cache miss, a mount it cannot
   pin, or a sequence count that changed under it, it takes counted references with get-unless-zero
   to the last dentry and mount it validated, leaves the read side, and continues as a reference
   walk under the sleeping locks, as Linux's `LOOKUP_RCU` walk falls back. When get-unless-zero
   fails, the walk restarts from its starting point as a reference walk.

Why: §2.9 has one way to say "not now", IF=0, and its rule 2 forbids an IF=0 path walk whose length
a user sets, so readers run with IF=1 and can be preempted. The nesting count does not disable
preemption; it only tells the grace-period machinery whom to wait for, so it is not a second "not
now". Linux's preemptible RCU has this shape, and its behaviour settles edge cases. Rejected:
classic RCU with IF=0 readers (rule 2); a preempt-disable count beside IF, which every per-CPU rule
would then have to account for; epoch-based reclamation, which scans every thread's epoch for each
grace period and stalls on a preempted reader the same way; and sleepable RCU alone, which has no
fast read side for the dentry cache.

Cost: the blocked-reader list and boosting make RCU larger than a classic implementation, and a
preempted reader lengthens grace periods, so memory waiting on them grows under load
([§4.4](MEMORY.md#44-kernel-heap) rule 3 counts it as freeable).

Planned (ROADMAP §19.5): nothing uses RCU yet.
