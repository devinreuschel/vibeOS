//! aarch64 in-guest test of ROADMAP §11.3's one stack size: every stack the
//! kernel runs on is 16 KiB in the guarded layout, and the full vector table
//! goes live on the bootstrap thread's stack.

use core::sync::atomic::{AtomicU64, Ordering};

use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::paging::PAGE_SIZE_4K;

use crate::arch::current::InterruptGuard;
use crate::ktest::Outcome;
use crate::paging_init;
use crate::thread_init;

/// The one stack size on aarch64 (ROADMAP §11.3): what the vector entry's
/// stack-bit test accepts.
const STACK_16K: u64 = 16 * 1024;

/// `[base, top)` has the layout of DESIGN §4.5 at the one size: 16 KiB,
/// `base` 32 KiB-aligned, every page mapped, and the 16 KiB below unmapped.
fn stack_layout_16k(base: u64, top: u64) -> Result<(), &'static str> {
    if top.wrapping_sub(base) != STACK_16K {
        return Err("not 16 KiB");
    }
    if !base.is_multiple_of(2 * STACK_16K) {
        return Err("base not 32 KiB-aligned");
    }
    let mut off = 0;
    while off < STACK_16K {
        if paging_init::translate(vibeos::paging::VirtAddr(base.wrapping_add(off))).is_none() {
            return Err("stack page unmapped");
        }
        if paging_init::translate(vibeos::paging::VirtAddr(
            base.wrapping_sub(STACK_16K).wrapping_add(off),
        ))
        .is_some()
        {
            return Err("guard page mapped");
        }
        off += PAGE_SIZE_4K;
    }
    Ok(())
}

/// Per CPU: its overflow stack's top, its idle thread's stack `[base, top)`.
static CPU_STACKS: [[AtomicU64; 3]; 64] = [const { [const { AtomicU64::new(0) }; 3] }; 64];
static CPU_STACKS_SEEN: AtomicU64 = AtomicU64::new(0);

/// On each CPU, IRQs off: note its overflow stack and idle thread's stack.
fn note_cpu_stacks(_: *mut ()) {
    let c = crate::per_cpu_init::current();
    let Some(slot) = CPU_STACKS.get(c.cpu_id as usize) else {
        return;
    };
    // SAFETY: `c.idle` is this CPU's idle TCB, which lives for the boot, and
    // only `thread_init::spawn_inner` writes its `stack`, before it first
    // runs. established by `thread_init::adopt_ap_idle`.
    let idle = unsafe { c.idle.as_ref() }
        .and_then(|t| t.stack.as_ref())
        .map(|s| (s.base().as_u64(), s.top().as_u64()))
        .unwrap_or((0, 0));
    // Relaxed: published by the Release `fetch_or` below; pairs with nothing.
    slot[0].store(c.overflow_sp, Ordering::Relaxed);
    // Relaxed: as above; pairs with nothing.
    slot[1].store(idle.0, Ordering::Relaxed);
    // Relaxed: as above; pairs with nothing.
    slot[2].store(idle.1, Ordering::Relaxed);
    // Release: pairs with the Acquire load in `test_stacks_16k`.
    CPU_STACKS_SEEN.fetch_or(1u64 << c.cpu_id, Ordering::Release);
}

/// ROADMAP §11.3: on aarch64 every stack the kernel runs on is 16 KiB at a
/// 32 KiB-aligned base with the 16 KiB below unmapped: every live thread's
/// (the bootstrap and registry threads among them), each online CPU's idle
/// thread's and overflow stack; the sizes those stacks are built from are
/// 16 KiB; and the full vector table went live on the bootstrap thread's
/// stack, after `thread_init::init_bootstrap` moved boot onto it.
pub(crate) fn test_stacks_16k() -> Outcome {
    let sizes = [
        ("default", DEFAULT_STACK_PAGES),
        ("registry", crate::ktest::REGISTRY_STACK_PAGES),
        ("bootstrap", thread_init::BOOT_STACK_PAGES),
    ];
    for (what, pages) in sizes {
        if pages as u64 * PAGE_SIZE_4K != STACK_16K {
            return crate::fail_fmt!("{what} stack is {pages} pages, want 4");
        }
    }
    let mut bad: Option<(u32, &'static str, &'static str)> = None;
    let mut threads = 0u32;
    thread_init::live_stack_spans(|id, name, base, top| {
        threads += 1;
        if bad.is_none()
            && let Err(e) = stack_layout_16k(base, top)
        {
            bad = Some((id, name, e));
        }
    });
    if let Some((id, name, e)) = bad {
        return crate::fail_fmt!("thread {id} {name}: {e}");
    }
    if threads == 0 {
        return Outcome::Fail("no live thread stack");
    }
    let online = crate::per_cpu_init::online_mask();
    CPU_STACKS_SEEN.store(0, Ordering::SeqCst);
    {
        let _g = InterruptGuard::enter();
        note_cpu_stacks(core::ptr::null_mut());
    }
    crate::ipi_init::call_mask(online & !1, note_cpu_stacks, core::ptr::null_mut(), true);
    // Acquire: pairs with the Release `fetch_or` in `note_cpu_stacks`.
    if CPU_STACKS_SEEN.load(Ordering::Acquire) != online {
        return Outcome::Fail("a cpu did not note its stacks");
    }
    for (cpu, slot) in CPU_STACKS.iter().enumerate() {
        if online & (1u64 << cpu) == 0 {
            continue;
        }
        // Relaxed: the Acquire load of `CPU_STACKS_SEEN` above orders these; pairs with nothing.
        let [ovf, idle_base, idle_top] = [0, 1, 2].map(|i| slot[i].load(Ordering::Relaxed));
        if let Err(e) = stack_layout_16k(ovf.wrapping_sub(STACK_16K), ovf) {
            return crate::fail_fmt!("cpu {cpu} overflow stack {ovf:#x}: {e}");
        }
        if let Err(e) = stack_layout_16k(idle_base, idle_top) {
            return crate::fail_fmt!("cpu {cpu} idle stack {idle_base:#x}: {e}");
        }
    }
    let Some((range, _, _)) = thread_init::bootstrap_stack() else {
        return Outcome::Fail("no bootstrap stack");
    };
    let sp = crate::arch::aarch64::vectors::full_table_sp();
    if !range.contains(&sp) {
        return crate::fail_fmt!(
            "full table went live on sp {sp:#x}, outside the bootstrap stack {:#x}..{:#x}",
            range.start,
            range.end
        );
    }
    Outcome::Ok
}
