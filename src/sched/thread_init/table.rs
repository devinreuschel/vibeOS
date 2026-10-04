//! The thread table's allocation and its readers: [`init_tables`] builds
//! the scheduler's heap tables at `limits::MAX_THREADS` entries (ROADMAP
//! §10.4, D1); the rest report on them.

use super::*;

/// Allocate the scheduler's tables at `limits::MAX_THREADS` entries and
/// install them: before [`init_bootstrap`], which puts the bootstrap
/// thread in slot 0. Once, on the BSP, before a second CPU or thread runs.
/// On failure nothing is installed, and the caller halts the boot.
pub fn init_tables() -> Result<(), AllocError> {
    let slots = limits::table(MAX_THREADS, || None)?;
    let links = limits::table(MAX_THREADS, || WaitLink::NONE)?;
    let places = limits::table(MAX_THREADS, || (0, ThreadId::NONE, 0))?;
    let timeouts = TimeoutQueue::try_new(MAX_THREADS)?;
    let tids = limits::table(MAX_THREADS, || AtomicU32::new(u32::MAX))?;
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
    testing::init_tables()?;
    // SAFETY: `BootCell::set`'s contract: this is its one write, on the BSP
    // before `smp: done` and before any thread is bound (so no reader of
    // `tid_of_slot` yet); established here, the first boot step to touch
    // the thread table (`main::normal_boot_tail`).
    unsafe { SLOT_TID.set(tids) };
    let old = {
        let mut s = SCHED.lock();
        (
            core::mem::replace(&mut s.slots, slots),
            core::mem::replace(&mut s.links, links),
            core::mem::replace(&mut s.places, places),
            core::mem::replace(&mut s.timeouts, timeouts),
        )
    };
    // The empty tables, dropped after SCHED: no heap free under it.
    drop(old);
    Ok(())
}

/// `spawn_inner`'s reuse test for a slot's TCB: Dead, and no CPU still
/// switching off it.
pub(super) fn dead_reusable(t: &Tcb) -> bool {
    t.state == ThreadState::Dead && t.on_cpu.is_clear()
}

/// Whether a spawn could take `slot`: empty, or a reusable Dead TCB.
pub(super) fn slot_reusable(slot: Option<&Tcb>) -> bool {
    slot.is_none_or(dead_reusable)
}

/// The thread table's use: slots a spawn could not take (by
/// [`slot_reusable`], `spawn_inner`'s own test), and its length.
#[cfg(feature = "kernel_tests")]
pub(crate) fn table_usage() -> (usize, usize) {
    with_sched(|s| {
        let cap = s.slots.len();
        let free = s
            .slots
            .iter()
            .filter(|x| slot_reusable(x.as_deref()))
            .count();
        (cap - free, cap)
    })
}

/// The TCB slot array's base address and length, which VMCOREINFO's
/// `SYMBOL(vibeos_tcbs)` and `LENGTH(vibeos_tcbs)` carry
/// (docs/VMCOREINFO.md). [`init_tables`] allocates the array once, before
/// VMCOREINFO is published, and nothing moves it after; the binding fails
/// to compile if the slot type stops being `TcbSlot`.
pub(crate) fn table_root() -> (u64, u64) {
    let s = SCHED.lock();
    let slots: &[vibeos::thread::TcbSlot] = &s.slots[..];
    (slots.as_ptr().addr() as u64, slots.len() as u64)
}

#[derive(Clone, Copy)]
pub struct ThreadInfo {
    pub id: ThreadId,
    pub name: &'static str,
    pub state: ThreadState,
    pub cpu: u32,
}

/// Snapshot under SCHED, then drop the lock. `ps` must not hold SCHED
/// across console writes. Fills `out` with the threads in slots `start..`,
/// in slot order, and returns how many it wrote and the slot to start the
/// next chunk at, the table's length once every slot is read: a caller
/// walks the table in chunks of its own buffer's size.
pub fn snapshot(start: usize, out: &mut [ThreadInfo]) -> (usize, usize) {
    with_sched(|s| {
        let mut n = 0usize;
        let mut i = start;
        while n < out.len() {
            let Some(slot) = s.slots.get(i) else {
                break;
            };
            if let (Some(t), Some(o)) = (slot.as_ref(), out.get_mut(n)) {
                *o = ThreadInfo {
                    id: t.id,
                    name: t.name,
                    state: t.state,
                    cpu: t.cpu,
                };
                n += 1;
            }
            i += 1;
        }
        (n, i)
    })
}

/// Run `f` on every thread in the table, in slot order, reading the table
/// through [`snapshot`] in chunks of [`SNAPSHOT_CHUNK`], each under its own
/// SCHED section, so `f` runs with no lock held and the buffer stays
/// small. A thread can move between chunks; each chunk is consistent.
pub fn each_thread(mut f: impl FnMut(&ThreadInfo)) {
    let mut buf = [ThreadInfo {
        id: ThreadId::NONE,
        name: "",
        state: ThreadState::Dead,
        cpu: 0,
    }; SNAPSHOT_CHUNK];
    let mut start = 0usize;
    loop {
        let (n, next) = snapshot(start, &mut buf);
        buf.iter().take(n).for_each(&mut f);
        // A short chunk means the table ended in it.
        if n < buf.len() {
            break;
        }
        start = next;
    }
}

/// Threads [`each_thread`] reads per SCHED section.
pub const SNAPSHOT_CHUNK: usize = 16;

/// The scheduler's timeout queue's capacity.
#[cfg(feature = "kernel_tests")]
#[cfg_attr(
    target_arch = "aarch64",
    expect(dead_code, reason = "boot-CPU S7; unused on this path")
)]
pub(crate) fn timeouts_capacity() -> usize {
    with_sched(|s| s.timeouts.capacity())
}

/// Scan every live thread's stack, one `SCHED` section per slot, and hand
/// each measurement to `f` with the lock dropped (TESTING §8.2).
#[cfg(feature = "kernel_tests")]
pub(crate) fn scan_live_stacks(mut f: impl FnMut(vibeos::sched::stack_depth::Deepest)) {
    const WORDS_PER_PAGE: usize = vibeos::paging::PAGE_SIZE_4K as usize / 8;
    let mut i = 0usize;
    while i < MAX_THREADS {
        let d = with_sched(|s| {
            let t = s.slots.get(i)?.as_deref()?;
            if t.state == ThreadState::Dead {
                return None;
            }
            let st = t.stack.as_ref()?;
            let words = st.pages() * WORDS_PER_PAGE;
            // SAFETY: a thread that is not Dead keeps its stack mapped
            // while SCHED is held: `thread_exit` stores Dead under SCHED
            // before its switch hands the stack to reclaim (invariant I10,
            // established at `sched::thread_init::thread_exit`).
            let used = unsafe {
                vibeos::sched::stack_depth::used_volatile(st.base().as_u64() as *const u64, words)
            };
            Some(vibeos::sched::stack_depth::Deepest {
                size: words * 8,
                used,
                tid: t.id.0,
                name: t.name,
            })
        });
        if let Some(d) = d {
            f(d);
        }
        i += 1;
    }
}

/// Print one line per stack size and the report line (TESTING §8.2).
#[cfg(all(feature = "kernel_tests", target_arch = "aarch64"))]
pub(crate) fn report_stack_depth() {
    let mut table = vibeos::sched::stack_depth::DepthTable::new();
    scan_live_stacks(|d| table.record(d));
    for d in table.deepest() {
        crate::marker!(
            "vibeOS: stack: {} used {} of {} by tid {} {}",
            d.size,
            d.used,
            vibeos::sched::stack_depth::budget(d.size),
            d.tid,
            d.name
        );
    }
    crate::marker!(
        "vibeOS: stack: report {} sizes {} lost",
        table.len(),
        table.lost()
    );
}
