//! `switch_context_roundtrip`: the kernel's x86_64 switch assembly, run on the
//! host (ROADMAP §10.2, F142).
//!
//! `vibeos-core` carries no assembly, so this test includes the port's
//! `src/arch/x86_64/switch.rs` itself, with `cli` and `sti` supplied empty,
//! since ring 3 cannot run them. The assembly uses ELF directives and
//! unprefixed symbol names, as the kernel's object format does, so the test
//! builds only on x86_64 Linux; other hosts build an empty test binary.
#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use vibeos::thread::{CpuContext, prepare_thread};

/// `switch.rs`'s `cli`: none on a host.
macro_rules! switch_cli {
    () => {
        ""
    };
}

/// `switch.rs`'s `sti`: none on a host.
macro_rules! switch_sti {
    () => {
        ""
    };
}

#[path = "../../../src/arch/x86_64/switch.rs"]
mod switch;

use switch::switch_context;

static FLAG: AtomicU64 = AtomicU64::new(0);
static MAIN_PTR: AtomicPtr<CpuContext> = AtomicPtr::new(core::ptr::null_mut());
static WORKER_PTR: AtomicPtr<CpuContext> = AtomicPtr::new(core::ptr::null_mut());

/// The worker's stack: 16 KiB, 16-byte aligned.
#[repr(C, align(16))]
struct Stack([u8; 16 * 1024]);

extern "C" fn worker_entry() {
    FLAG.store(0xC0FFEE, Ordering::SeqCst);
    // SAFETY: this worker runs only from the switch in
    // `switch_context_roundtrip`, whose frame holds both contexts and waits
    // in that switch, which saved `MAIN_PTR`'s context; established here, as
    // the test's only worker.
    unsafe {
        switch_context(
            WORKER_PTR.load(Ordering::SeqCst),
            MAIN_PTR.load(Ordering::SeqCst),
        );
    }
    panic!("worker resumed");
}

#[test]
fn switch_context_roundtrip() {
    FLAG.store(0, Ordering::SeqCst);
    let mut stack = Stack([0u8; 16 * 1024]);
    let base = stack.0.as_mut_ptr() as usize;
    let top = (base + stack.0.len()) as u64;
    let mut main_ctx = CpuContext::empty();
    let mut worker_ctx = CpuContext::empty();
    let main_p = &raw mut main_ctx;
    let worker_p = &raw mut worker_ctx;
    MAIN_PTR.store(main_p, Ordering::SeqCst);
    WORKER_PTR.store(worker_p, Ordering::SeqCst);
    // SAFETY: `top` is the 16-byte aligned end of `stack`, which outlives the
    // worker, and both contexts are locals this frame owns; the worker
    // switches straight back; established here.
    unsafe {
        prepare_thread(&mut *worker_p, top, worker_entry as *const () as u64);
        ((*worker_p).rsp as *mut u64).write_volatile(0);
        switch_context(main_p, worker_p);
    }
    assert_eq!(FLAG.load(Ordering::SeqCst), 0xC0FFEE);
    assert!(main_ctx.rip != 0);
    assert!(main_ctx.rsp != 0);
}
