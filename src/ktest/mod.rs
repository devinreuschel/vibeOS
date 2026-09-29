//! In-guest test registry. DESIGN §8.2.
//!
//! Built only with `--features kernel_tests`. After normal init this
//! module runs the registry over the real IDT, prints the serial protocol,
//! and exits QEMU through `isa-debug-exit`.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vibeos::block::{BlockError, DeviceState, Op};
use vibeos::dev::{ClaimError, Device, Driver, IdMatch, ProbeError};
use vibeos::dma::{self, DMA32_BOUNDARY, DmaAlloc};
use vibeos::fs::{FsError, InodeKind, O_CREAT, O_RDWR};
use vibeos::lock::RANK_DEVICE;
use vibeos::paging::PhysAddr;
use vibeos::pci::{self, Bdf, CFG_COMMAND, CFG_VENDOR, CMD_MASTER, CMD_MEM};
use vibeos::per_cpu::PerCpuRemote;
use vibeos::pmm::Frames;
use vibeos::thread::{ThreadId, ThreadState};
use vibeos::vectors;
use vibeos::virtio::F_VERSION_1;

use crate::block_init::{self, IoWaiter};
use crate::cache_init;
use crate::dev_init;
use crate::dma_init;
use crate::fat_init;
use crate::file_init;
use crate::fs_init;
use crate::ipi_init;
use crate::kva_init;
use crate::paging_init;
use crate::part_init;
use crate::pci_init;
use crate::per_cpu_init;
use crate::pmm_init;
use crate::sync_init::SpinMutex;
use crate::thread_init::{self, ThreadHandle};
use crate::time_init;
use crate::vibefs_init;
use crate::virtio_blk_init;
use crate::virtio_init;
use crate::x86;
use crate::{acpi, arch, boot, console, irq, log, mm, proc, sched, shell, smp, sync, time};
pub(crate) mod user;

const ISA_DEBUG_EXIT: u16 = 0xF4;
const EXIT_PASS: u32 = 0x10;
const EXIT_FAIL: u32 = 0x11;

#[derive(Clone, Copy)]
pub(crate) enum Outcome {
    Ok,
    Fail(&'static str),
    FailFmt(FailMsg),
    Skip(&'static str),
}

pub(crate) const FAIL_MSG_BYTES: usize = 120;

/// A formatted failure reason, cut at [`FAIL_MSG_BYTES`] on a character
/// boundary. Build one with [`crate::fail_fmt!`].
#[derive(Clone, Copy)]
pub(crate) struct FailMsg {
    buf: [u8; FAIL_MSG_BYTES],
    len: u8,
    full: bool,
}

impl FailMsg {
    pub(crate) fn from_args(args: fmt::Arguments<'_>) -> FailMsg {
        let mut m = FailMsg {
            buf: [0; FAIL_MSG_BYTES],
            len: 0,
            full: false,
        };
        if fmt::write(&mut m, args).is_err() {
            // A `Display` impl failed: say so rather than drop the error
            // (DESIGN §2.5). Appended only if it fits whole.
            const MARK: &str = " <fmt error>";
            if m.full || (m.len as usize) + MARK.len() > FAIL_MSG_BYTES {
                return m;
            }
            m.push_whole(MARK);
        }
        m
    }

    pub(crate) fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("<invalid utf-8>")
    }

    fn push_whole(&mut self, s: &str) {
        for ch in s.chars() {
            let at = self.len as usize;
            let n = ch.len_utf8();
            if self.full || at + n > FAIL_MSG_BYTES {
                self.full = true;
                return;
            }
            ch.encode_utf8(&mut self.buf[at..at + n]);
            self.len += n as u8;
        }
    }
}

impl fmt::Write for FailMsg {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push_whole(s);
        Ok(())
    }
}

/// `Outcome::FailFmt` with a `format_args!` reason, cut at 120 bytes.
#[macro_export]
macro_rules! fail_fmt {
    ($($arg:tt)*) => {
        $crate::ktest::Outcome::FailFmt($crate::ktest::FailMsg::from_args(format_args!($($arg)*)))
    };
}

pub(crate) type TestFn = fn() -> Outcome;

/// One registry row. `deadline_ms`, `once` and `opt_in` are data until the
/// runner enforces them (ROADMAP §10.2).
#[derive(Clone, Copy)]
pub(crate) struct Test {
    pub name: &'static str,
    pub run: TestFn,
    pub deadline_ms: u32,
    pub once: bool,
    pub opt_in: bool,
}

/// A row with the default deadline (10 s), run on every repeat, not opt-in.
pub(crate) const fn test(name: &'static str, run: TestFn) -> Test {
    Test {
        name,
        run,
        deadline_ms: 10_000,
        once: false,
        opt_in: false,
    }
}

impl Test {
    pub const fn deadline(self, ms: u32) -> Test {
        Test {
            deadline_ms: ms,
            ..self
        }
    }

    pub const fn once(self) -> Test {
        Test { once: true, ..self }
    }

    pub const fn opt_in(self) -> Test {
        Test {
            opt_in: true,
            ..self
        }
    }
}

pub(crate) type Suite = &'static [Test];

mod p10_s04;
mod p10_s09;
mod p10_s11;
mod p10_s12;
mod p10_s13;

/// Rows run in this order. A new test goes in its subsystem's ktest.rs, and its row goes after the
/// last row whose path starts with that subsystem, or at the end if it has none (ROADMAP §10.2's T1
/// box splits this list per subsystem).
pub(crate) const TESTS: &[Test] = &[
    test("map_unmap", mm::ktest::test_map_unmap),
    test("nx_enforcement", mm::ktest::test_nx_enforcement),
    test("heap_box", mm::ktest::test_heap_box),
    test("heap_reuse", mm::ktest::test_heap_reuse),
    test("heap_align", mm::ktest::test_heap_align),
    test("heap_growth", mm::ktest::test_heap_growth),
    test("heap_oom", mm::ktest::test_heap_oom),
    test("stack_guard", mm::ktest::test_stack_guard),
    test("kva_roundtrip", mm::ktest::test_kva_roundtrip),
    test("kva_deferred", mm::ktest::test_kva_deferred),
    test("vmap", mm::ktest::test_vmap),
    test("mmio_uc_flags", mm::ktest::test_mmio_uc_flags),
    test("acpi_discovery", acpi::ktest::test_acpi_discovery),
    test("gdt_selectors", arch::ktest::test_gdt_selectors),
    test("star_sysret_layout", arch::ktest::test_star_sysret_layout),
    test(
        "addrspace_map_unmap_teardown",
        proc::ktest::test_addrspace_map_unmap_teardown,
    ),
    test("user_ptr_helpers", proc::ktest::test_user_ptr_helpers),
    test("cr3_switch_skip", proc::ktest::test_cr3_switch_skip),
    test(
        "ring3_syscall_enosys",
        proc::ktest::test_ring3_syscall_enosys,
    ),
    test("ring3_hello_exit", proc::ktest::test_ring3_hello_exit),
    test("syscall_dispatch", proc::ktest::test_syscall_dispatch),
    test(
        "syscall_ptr_validate",
        proc::ktest::test_syscall_ptr_validate,
    ),
    test("user_syscalls", proc::ktest::test_user_syscalls),
    test("int3_roundtrip", arch::ktest::test_int3_roundtrip),
    test("scoped_pf", arch::ktest::test_scoped_pf),
    test("gp_catch", arch::ktest::test_gp_catch),
    test(
        "irqcell_reentry_panics",
        sync::ktest::test_irqcell_reentry_panics,
    ),
    test("bootcell_set_once", sync::ktest::test_bootcell_set_once),
    test("bootinfo_consistent", boot::ktest::test_bootinfo_consistent),
    test("df_on_ist", arch::ktest::test_df_on_ist),
    test("pit_tick_rate", time::ktest::test_pit_tick_rate),
    test("now_us_monotonic", time::ktest::test_now_us_monotonic),
    test("now_us_under_yields", time::ktest::test_now_us_under_yields),
    test("tsc_calib_source", time::ktest::test_tsc_calib_source),
    test("uptime_sides", time::ktest::test_uptime_sides),
    test("rtc_offset", time::ktest::test_rtc_offset),
    test("lapic_timer_mode", arch::ktest::test_lapic_timer_mode),
    test("lapic_timer_rearm", arch::ktest::test_lapic_timer_rearm),
    test(
        "ioapic_pit_gsi_masked",
        arch::ktest::test_ioapic_pit_gsi_masked,
    ),
    test("per_cpu_bsp", smp::ktest::test_per_cpu_bsp),
    test("per_cpu_identity", smp::ktest::test_per_cpu_identity),
    test("trampoline_page", smp::ktest::test_trampoline_page),
    test("failed_ap_cleanup", smp::ktest::test_failed_ap_cleanup),
    test("spawn_sentinel", sched::ktest::test_spawn_sentinel),
    test("switch_two_threads", sched::ktest::test_switch_two_threads),
    test("irq_guard_nest", arch::ktest::test_irq_guard_nest),
    test("spin_mutex", sync::ktest::test_spin_mutex),
    test("lock_spins", sync::ktest::test_lock_spins),
    test("yield_now_switches", sched::ktest::test_yield_now_switches),
    test("sleep_ms_50", sched::ktest::test_sleep_ms_50),
    test(
        "preempt_two_threads",
        sched::ktest::test_preempt_two_threads,
    ),
    test("idle_runs", sched::ktest::test_idle_runs),
    test(
        "reap_returns_frames",
        sched::ktest::test_reap_returns_frames,
    ),
    test("reap_many_via_idle", sched::ktest::test_reap_many_via_idle),
    test(
        "blocking_mutex_counter",
        sync::ktest::test_blocking_mutex_counter,
    ),
    test("rwlock_exclusion", sync::ktest::test_rwlock_exclusion),
    test(
        "rwlock_writer_timeout",
        sync::ktest::test_rwlock_writer_timeout,
    ),
    test("semaphore_wake", sync::ktest::test_semaphore_wake),
    test("condvar_signal", sync::ktest::test_condvar_signal),
    test(
        "condvar_wait_releases",
        sync::ktest::test_condvar_wait_releases,
    ),
    test("channel_mpsc", sync::ktest::test_channel_mpsc),
    test("mutex_deadline", sync::ktest::test_mutex_deadline),
    test("sync_try_paths", sync::ktest::test_sync_try_paths),
    test(
        "sched_lock_timer_irq",
        sched::ktest::test_sched_lock_timer_irq,
    ),
    test(
        "spawn_exit_thousands",
        sched::ktest::test_spawn_exit_thousands,
    ),
    test("cross_cpu_spawn", sched::ktest::test_cross_cpu_spawn),
    test(
        "reschedule_ipi_wake_ap",
        irq::ktest::test_reschedule_ipi_wake_ap,
    ),
    test("call_function_ipi", irq::ktest::test_call_function_ipi),
    test("cpu_hardening", arch::ktest::test_cpu_hardening),
    test("tlb_shootdown_remote", mm::ktest::test_tlb_shootdown_remote),
    test("alloc_stress_smp", mm::ktest::test_alloc_stress_smp),
    test("log_boot_captured", log::ktest::test_log_boot_captured),
    test("log_runtime_filter", log::ktest::test_log_runtime_filter),
    test("log_emit_roundtrip", log::ktest::test_log_emit_roundtrip),
    test(
        "log_dmesg_no_recapture",
        log::ktest::test_log_dmesg_no_recapture,
    ),
    test("fb_bgrx_roundtrip", console::ktest::test_fb_bgrx_roundtrip),
    test("fb_pitch", console::ktest::test_fb_pitch),
    test("fb_cr_home", console::ktest::test_fb_cr_home),
    test("kbd_gsi_unmasked", console::ktest::test_kbd_gsi_unmasked),
    test("kbd_8042_clock", console::ktest::test_kbd_8042_clock),
    test("kbd_ps2_irq", console::ktest::test_kbd_ps2_irq),
    test("console_mux", console::ktest::test_console_mux),
    test("kbd_ring_drain", console::ktest::test_kbd_ring_drain),
    test("shell_registry", shell::ktest::test_shell_registry),
    test("shell_dispatch", shell::ktest::test_shell_dispatch),
    test("shell_dmesg_level", shell::ktest::test_shell_dmesg_level),
    test("pci_qemu_set", test_pci_qemu_set),
    test("pci_bar_map", test_pci_bar_map),
    test("pci_cfg_rw", test_pci_cfg_rw),
    test("pci_claim_exclusive", test_pci_claim_exclusive),
    test("pci_bind_order", test_pci_bind_order),
    test("lspci_cmd", shell::ktest::test_lspci_cmd),
    test("irq_pool", irq::ktest::test_irq_pool),
    test("irq_free_threaded", irq::ktest::test_irq_free_threaded),
    test("msix_cpu", irq::ktest::test_msix_cpu),
    test("intx_fallback", irq::ktest::test_intx_fallback),
    test("intx_free_masks", irq::ktest::test_intx_free_masks),
    test("dma_alloc", test_dma_alloc),
    test("dma_edu", test_dma_edu),
    test("workqueue", sched::ktest::test_workqueue),
    test("virtio_bind", test_virtio_bind),
    test("virtio_vq", test_virtio_vq),
    test("dev_random_source", test_dev_random_source),
    test("block_ramdisk_rw", test_block_ramdisk_rw),
    test("block_concurrent", test_block_concurrent),
    test("block_retry", test_block_retry),
    test("block_vblk_rw", test_block_vblk_rw),
    test("block_vblk_irq", test_block_vblk_irq),
    test("block_vblk_deep", test_block_vblk_deep),
    test("block_vblk_concurrent", test_block_vblk_concurrent),
    test("block_vblk_mq", test_block_vblk_mq),
    test("block_persist", test_block_persist),
    test("block_part_mbr", test_block_part_mbr),
    test("block_part_gpt", test_block_part_gpt),
    test("block_cache_hit", test_block_cache_hit),
    test("block_cache_evict", test_block_cache_evict),
    test("vfs_walk", test_vfs_walk),
    test("pseudo_fs", test_pseudo_fs),
    test("fat_initrd", test_fat_initrd),
    test("vibefs", test_vibefs),
    test("ktest_rows", sched::ktest::test_ktest_rows),
    test("ktest_fail_fmt", sched::ktest::test_ktest_fail_fmt),
    test("ktest_helpers", sched::ktest::test_ktest_helpers),
    test("user_code_exit", proc::ktest::test_user_code_exit),
    test("user_image_elf", proc::ktest::test_user_image_elf),
    test("user_code_layout", proc::ktest::test_user_code_layout),
    test(
        "orphan_freed_no_init",
        proc::ktest::test_orphan_freed_no_init,
    ),
    test("ktest_context", sched::ktest::ktest_context),
    test("msix_cpu_publish_last", irq::ktest::msix_cpu_publish_last),
    test(
        "lifetime_iowaiter_publish_last",
        p10_s04::lifetime_iowaiter_publish_last,
    )
    .deadline(60_000),
    test(
        "lifetime_shootdown_ack_late",
        irq::ktest::lifetime_shootdown_ack_late,
    )
    .deadline(15_000),
    test("percpu_remote_view", smp::ktest::percpu_remote_view),
    test("frames_none_leaked", mm::ktest::frames_none_leaked),
    test(
        "current_mapper_holds_pt",
        mm::ktest::current_mapper_holds_pt,
    ),
    test(
        "teardown_live_root_asserts",
        proc::ktest::teardown_live_root_asserts,
    ),
    test(
        "vmap_32_frames_unmapped",
        mm::ktest::vmap_32_frames_unmapped,
    ),
    test("spawn_stack_oom", sched::ktest::spawn_stack_oom).deadline(10_000),
    test("fork_oom", sched::ktest::fork_oom).deadline(10_000),
    test(
        "lifetime_stack_reclaim",
        sched::ktest::lifetime_stack_reclaim,
    )
    .deadline(120_000),
    test("exit_burst", sched::ktest::exit_burst).deadline(60_000),
    test(
        "lifetime_dead_slot_on_cpu",
        sched::ktest::lifetime_dead_slot_on_cpu,
    )
    .deadline(180_000),
    test("file_table_fork_churn", p10_s09::test_file_table_fork_churn),
    test(
        "file_table_stale_writeback_ebadf",
        p10_s09::test_file_table_stale_writeback_ebadf,
    ),
    test(
        "open_creat_exists_opens",
        p10_s09::test_open_creat_exists_opens,
    ),
    test(
        "inode_size_shared_across_opens",
        p10_s09::test_inode_size_shared_across_opens,
    ),
    test(
        "fat_unlinked_open_frees_at_close",
        p10_s09::test_fat_unlinked_open_frees_at_close,
    ),
    test("vibefs_efbig", p10_s09::test_vibefs_efbig),
    test("vibefs_seek_end_5gib", p10_s09::test_vibefs_seek_end_5gib),
    test("vfs_fat_ops_initrd", p10_s11::test_vfs_fat_ops_initrd),
    test(
        "vfs_fat_unlinked_open_inode",
        p10_s11::test_vfs_fat_unlinked_open_inode,
    ),
    test(
        "vfs_fat_file_api_one_inode",
        p10_s11::test_vfs_fat_file_api_one_inode,
    ),
    test("vfs_vibe_ops_mem", p10_s11::test_vfs_vibe_ops_mem),
    test("vfs_backends_via_ops", p10_s12::test_vfs_backends_via_ops),
    test("vfs_fat_one_inode", p10_s12::test_vfs_fat_one_inode),
    test(
        "cache_flush_waits_writeback",
        p10_s13::cache_flush_waits_writeback,
    ),
    test("block_fua_write", p10_s13::block_fua_write),
    test(
        "ac_clear_on_exception",
        arch::ktest::test_ac_clear_on_exception,
    )
    .deadline(30_000),
    test("ac_clear_user_popf", arch::ktest::test_ac_clear_user_popf).deadline(30_000),
    test("ist_gs_sign", arch::ktest::test_ist_gs_sign).deadline(30_000),
    test("user_exceptions", arch::ktest::test_user_exceptions).deadline(30_000),
    test("user_device_irq", arch::ktest::test_user_device_irq).deadline(30_000),
    test("user_ipi", arch::ktest::test_user_ipi).deadline(30_000),
    test("cpu_control_regs", arch::ktest::cpu_control_regs),
    test("console_read_exit", proc::ktest::test_console_read_exit).deadline(30_000),
    test("user_entry_irq", proc::ktest::test_user_entry_irq).deadline(120_000),
    test(
        "exec_top_page_enoexec",
        proc::ktest::test_exec_top_page_enoexec,
    )
    .deadline(30_000),
    test(
        "noncanonical_rip_sigsegv",
        proc::ktest::test_noncanonical_rip_sigsegv,
    )
    .deadline(30_000),
    test("fp_no_leak", sched::ktest::test_fp_no_leak).deadline(60_000),
    test("fp_migrate_counter", sched::ktest::test_fp_migrate_counter).deadline(30_000),
    test("exec_huge_memsz", proc::ktest::test_exec_huge_memsz).deadline(60_000),
    test(
        "brk_mmap_munmap_user",
        proc::ktest::test_brk_mmap_munmap_user,
    )
    .deadline(30_000),
    test(
        "stop_cont_no_lost_wakeup",
        proc::ktest::test_stop_cont_no_lost_wakeup,
    )
    .deadline(30_000),
    test("syscall_body_if_on", proc::ktest::test_syscall_body_if_on).deadline(30_000),
    test("kill_line_whole", proc::ktest::test_kill_line_whole).deadline(30_000),
    test(
        "console_write_newlines",
        console::ktest::test_console_write_newlines,
    )
    .deadline(60_000),
    test(
        "lifetime_console_write_acks_shootdown",
        console::ktest::test_lifetime_console_write_acks_shootdown,
    )
    .deadline(60_000),
    test(
        "kalloc_fail_after_hook",
        proc::ktest::test_kalloc_fail_after_hook,
    ),
    test("kalloc_nomem", proc::ktest::test_kalloc_nomem).deadline(120_000),
    test("syscall_rcx_canary", proc::ktest::test_syscall_rcx_canary).deadline(30_000),
    test("fork_child_gprs", proc::ktest::test_fork_child_gprs).deadline(30_000),
    test(
        "preempt_gpr_canaries",
        proc::ktest::test_preempt_gpr_canaries,
    )
    .deadline(60_000),
    test("user_single_step", proc::ktest::test_user_single_step).deadline(30_000),
    test("user_int1", proc::ktest::test_user_int1).deadline(30_000),
    test("user_tf_repin", proc::ktest::test_user_tf_repin).deadline(60_000),
    test("user_fork_wait_stall", proc::ktest::user_fork_wait_stall),
    test(
        "shootdown_ack_while_busy",
        irq::ktest::shootdown_ack_while_busy,
    ),
];

/// The one list, [`TESTS`] (DESIGN §8.2).
pub(crate) const SUITES: &[Suite] = &[TESTS];

/// Name of the registry's kernel thread.
const REGISTRY_NAME: &str = "ktest";
/// The registry's stack: 64 KiB, the boot stack size Limine guarantees
/// (ROADMAP §10.2).
const REGISTRY_STACK_PAGES: usize = 16;

/// The registry thread's id, `u32::MAX` until it starts.
static REGISTRY_TID: AtomicU32 = AtomicU32::new(u32::MAX);

/// The id of the thread [`registry_main`] runs on.
pub(crate) fn registry_tid() -> ThreadId {
    ThreadId(REGISTRY_TID.load(Ordering::Acquire))
}

/// Start the registry on its own kernel thread, pinned to CPU 0, and park
/// the bootstrap thread for good (DESIGN §8.2).
pub fn run() -> ! {
    if let Err(e) = thread_init::spawn_opts(
        REGISTRY_NAME,
        registry_main,
        thread_init::SpawnOpts {
            stack_pages: REGISTRY_STACK_PAGES,
            cpu: Some(0),
        },
    ) {
        panic!("ktest: registry thread: {}", e.as_str());
    }
    loop {
        thread_init::park(None);
    }
}

/// Run every suite with IF on and `irq_nest` 0, the context production
/// kernel threads run in, then exit QEMU.
fn registry_main() {
    REGISTRY_TID.store(thread_init::current_id().0, Ordering::Release);
    crate::marker!("vibeOS: ktest: begin");
    quiesce_frames();
    let mut failed = false;
    for suite in SUITES {
        for t in suite.iter() {
            let name = t.name;
            let mut outcome = (t.run)();
            // A test that needs interrupts off takes its own guard and
            // drops it before it returns.
            let if_on = x86::interrupts_enabled();
            let nest = per_cpu_init::irq_nest();
            if !if_on || nest != 0 {
                per_cpu_init::current().irq_nest.store(0, Ordering::Relaxed);
                x86::sti();
                if !matches!(outcome, Outcome::Fail(_) | Outcome::FailFmt(_)) {
                    outcome = crate::fail_fmt!("left IF={} irq_nest={}", u8::from(if_on), nest);
                }
            }
            match outcome {
                Outcome::Ok => {
                    crate::marker!("vibeOS: ktest: ok {name}");
                }
                Outcome::Fail(why) => {
                    // Same shape as skip: reason on the protocol line so
                    // check_ktest_output (which raises on that line alone)
                    // is enough to diagnose (DESIGN §8.2).
                    crate::marker!("vibeOS: ktest: FAIL {name}: {why}");
                    failed = true;
                }
                Outcome::FailFmt(msg) => {
                    let why = msg.as_str();
                    crate::marker!("vibeOS: ktest: FAIL {name}: {why}");
                    failed = true;
                }
                Outcome::Skip(reason) => {
                    crate::marker!("vibeOS: ktest: skip {name}: {reason}");
                }
            }
        }
    }
    crate::marker!("vibeOS: ktest: end");
    qemu_exit(if failed { EXIT_FAIL } else { EXIT_PASS });
}

fn qemu_exit(code: u32) -> ! {
    unsafe { x86::outl(ISA_DEBUG_EXIT, code) };
    x86::halt();
}

/// Pages per stack in [`quiesce_frames`]' KVA walk: 17 pages of VA a round,
/// so one walk between two coalesces covers about 8 MiB.
const WARM_STACK_PAGES: usize = 16;
/// Ceiling on walk rounds: two coalesces take at most two free lists' worth.
const WARM_ROUNDS: usize = 3 * vibeos::limits::MAX_KVA_RANGES;
/// Default-size stacks the warm-up allocates and frees: past one free-list
/// coalesce, since the node pool is `MAX_KVA_RANGES` (128).
const WARM_DEFAULT_STACKS: usize = 2 * vibeos::limits::MAX_KVA_RANGES;
/// Ceiling on [`settle_threads`]' wait.
const SETTLE_MS: u64 = 2_000;
/// Thread slots [`quiesce_frames`] leaves empty: `thread_init::adopt_ap_idle`
/// takes only an empty slot, and `failed_ap_cleanup` calls it twice.
const EMPTY_SLOT_RESERVE: usize = 2;

/// Set once [`quiesce_frames`] has run; it runs once per boot.
static WARMED: AtomicBool = AtomicBool::new(false);

/// Shared setup before the first frame-accounting test (ROADMAP §10.2,
/// F074): after it, `free_frames()` moves only for what a test itself
/// allocates and frees, so the tests compare against a quiescent baseline
/// and a leak in their window still shows.
///
/// Three things move the count outside a test's window. A thread spawned
/// before the registry (the boot `/hello`, whose `wait_kernel` returns at
/// the reap, before the thread parks its stack) can still be running or
/// have its stack on its CPU's dead list. A spawn into an empty
/// thread slot boxes a new `Tcb`, which can grow the heap. A stack or vmap
/// carved from KVA that no mapping has reached before takes a page-table
/// page that `unmap` never frees. So: let every pending thread finish and
/// its stack come back; fill the empty thread slots, all but
/// [`EMPTY_SLOT_RESERVE`], with threads that exit at once, so later spawns
/// reuse Dead boxes; walk KVA through two coalesces with the timer on, so
/// the free list starts again at VA the walk mapped; and allocate and free
/// [`WARM_DEFAULT_STACKS`] default-size stacks. It runs once per boot, from
/// the registry or from the first `quiescent_free_frames` caller,
/// which then waits for the threads and stacks to settle before it reads
/// the count.
pub(crate) fn quiesce_frames() {
    if WARMED.swap(true, Ordering::AcqRel) {
        return;
    }
    if !settle_threads() {
        crate::marker!("vibeOS: ktest:   warm-up: threads did not settle");
    }
    let mut buf = [thread_init::ThreadInfo {
        id: ThreadId::NONE,
        name: "",
        state: ThreadState::Dead,
        cpu: 0,
    }; vibeos::thread::MAX_THREADS];
    let empty = (vibeos::thread::MAX_THREADS - thread_init::snapshot(&mut buf))
        .saturating_sub(EMPTY_SLOT_RESERVE);
    // Under the guard none of these runs, dies, and frees its slot for the
    // next spawn before every empty slot has a Tcb.
    {
        let _g = x86::InterruptGuard::enter();
        let mut i = 0;
        while i < empty {
            if thread_init::spawn_here("warm", dying_entry).is_err() {
                break;
            }
            i += 1;
        }
    }
    if !settle_threads() {
        crate::marker!("vibeOS: ktest:   warm-up: warm threads did not exit");
    }
    // The first coalesce can come after a round or two, when the free list
    // is nearly full already; the second comes after a full list of rounds,
    // so the VA it merges back to the list's head is mapped past that.
    let (coalesces, rounds) = warm_kva();
    if coalesces < 2 {
        crate::marker!("vibeOS: ktest:   warm-up: kva coalesces {coalesces} in {rounds} rounds");
    }
    // Then the size every default spawn takes, directly, so a spawn that
    // misses its CPU's stack cache maps VA the warm-up mapped.
    let mut i = 0;
    while i < WARM_DEFAULT_STACKS {
        let Ok(stack) = kva_init::alloc_guarded_stack(vibeos::kva::DEFAULT_STACK_PAGES) else {
            crate::marker!("vibeOS: ktest:   warm-up: default stack {i} failed");
            break;
        };
        kva_init::free_stack(stack);
        i += 1;
    }
}

/// Allocate and free guarded stacks until `Kva::free` has coalesced twice.
/// The timer stays on: at `-smp 4` each round's shootdowns take long enough
/// that an IF-off walk loses PIT ticks. Returns (coalesces, rounds).
fn warm_kva() -> (usize, usize) {
    let mut coalesces = 0;
    let mut rounds = 0;
    while coalesces < 2 && rounds < WARM_ROUNDS {
        let Ok(stack) = kva_init::alloc_guarded_stack(WARM_STACK_PAGES) else {
            break;
        };
        let n = kva_init::stats().free_ranges;
        kva_init::free_stack(stack);
        // A free adds one range unless `Kva::free` ran its coalesce.
        if kva_init::stats().free_ranges <= n {
            coalesces += 1;
        }
        rounds += 1;
    }
    (coalesces, rounds)
}

/// With the timer on, sleep until no thread but this one and the idle
/// threads is Ready or Running and no dead thread's stack is still on its
/// way to a stack cache or back to the buddy. False if that takes longer
/// than [`SETTLE_MS`].
pub(crate) fn settle_threads() -> bool {
    let me = thread_init::current_id();
    let t0 = time_init::uptime_ms();
    loop {
        let mut buf = [thread_init::ThreadInfo {
            id: ThreadId::NONE,
            name: "",
            state: ThreadState::Dead,
            cpu: 0,
        }; vibeos::thread::MAX_THREADS];
        let n = thread_init::snapshot(&mut buf);
        let busy = thread_init::stacks_in_flight() != 0
            || buf[..n].iter().any(|t| {
                t.id != me
                    && t.name != "idle"
                    && matches!(t.state, ThreadState::Ready | ThreadState::Running)
            });
        if !busy {
            break true;
        }
        if time_init::uptime_ms().saturating_sub(t0) > SETTLE_MS {
            break false;
        }
        thread_init::sleep_ms(1);
    }
}

/// Free frames: the buddy's, and those of the stacks the CPUs' stack caches
/// hold, which a spawn reuses (ROADMAP §10.10).
pub(crate) fn free_frames() -> usize {
    pmm_init::with_buddy(|b| b.stats().free_frames) + thread_init::cached_stack_frames()
}

pub(crate) fn alloc_frame() -> Option<PhysAddr> {
    alloc_frames(0)
}

pub(crate) fn free_frame(pa: PhysAddr) {
    // SAFETY: an unknown base frees nothing, and a held one is freed once
    // (`ktest::take_held` removes it), so the contract of
    // `ktest::dealloc_frames` holds for any `pa`.
    unsafe { dealloc_frames(pa, 0) };
}

pub(crate) struct Fault {
    pub(crate) cr2: u64,
    pub(crate) error: u64,
}

pub(crate) fn catch_fault<F: FnOnce()>(f: F) -> Option<Fault> {
    // A hit longjmps out of the #PF gate and skips the `iretq` that would
    // restore IF; the guard restores it.
    let _g = x86::InterruptGuard::enter();
    arch::catch::catch(vectors::PF, f).map(|c| Fault {
        cr2: c.cr2,
        error: c.error,
    })
}

pub(crate) fn catch_alloc_error<F: FnOnce()>(f: F) -> bool {
    arch::catch::catch_alloc(f)
}

// Helpers for APIs that concurrent Phase 10 slices change. A new test
// reaches those APIs only through these; the slice that changes one
// updates its helper here (DESIGN §8.2).

pub(crate) fn spawn_thread(name: &'static str, entry: fn()) -> ThreadHandle {
    match thread_init::spawn(name, entry) {
        Ok(h) => h,
        Err(e) => panic!("ktest: spawn {name}: {}", e.as_str()),
    }
}

pub(crate) fn spawn_thread_on(name: &'static str, entry: fn(), cpu: u32) -> ThreadHandle {
    match thread_init::spawn_on(name, entry, cpu) {
        Ok(h) => h,
        Err(e) => panic!("ktest: spawn {name} on cpu{cpu}: {}", e.as_str()),
    }
}

/// Blocks the frame helpers handed out, as the `Frames` that own them.
/// The helpers trade bare addresses (C-SUITES), so the tokens wait here.
/// Never held together with BUDDY.
static HELD: SpinMutex<[Option<Frames>; 16]> =
    SpinMutex::with_rank([const { None }; 16], RANK_DEVICE);

/// Remove and return the held token whose base is `pa`.
fn take_held(pa: PhysAddr) -> Option<Frames> {
    let mut held = HELD.lock();
    let slot = held
        .iter_mut()
        .find(|s| s.as_ref().is_some_and(|f| f.base() == pa.as_u64()))?;
    slot.take()
}

/// A naturally aligned block of `1 << order` frames. `None` when the
/// buddy is out, or when 16 blocks are already out through these helpers.
pub(crate) fn alloc_frames(order: u8) -> Option<PhysAddr> {
    let f = pmm_init::with_buddy(|b| b.alloc(order))?;
    let pa = PhysAddr(f.base());
    let spare = {
        let mut held = HELD.lock();
        match held.iter_mut().find(|s| s.is_none()) {
            Some(slot) => {
                *slot = Some(f);
                None
            }
            None => Some(f),
        }
    };
    match spare {
        None => Some(pa),
        Some(f) => {
            pmm_init::with_buddy(|b| b.free(f));
            None
        }
    }
}

/// Free a block [`alloc_frames`] returned. A base it did not hand out, or
/// already freed, is ignored.
///
/// # Safety
/// `pa` is a block that [`alloc_frames`] returned for this same `order`, and
/// nothing still maps or uses it.
pub(crate) unsafe fn dealloc_frames(pa: PhysAddr, order: u8) {
    if let Some(f) = take_held(pa) {
        debug_assert_eq!(f.order(), order, "ktest: dealloc_frames order");
        pmm_init::with_buddy(|b| b.free(f));
    }
}

/// A naturally aligned block of `1 << order` frames as its owning
/// `Frames`, for an API that takes the token (`kva_init::vmap`). The
/// caller gives it back with [`free_frames_owned`].
pub(crate) fn alloc_frames_owned(order: u8) -> Option<Frames> {
    pmm_init::with_buddy(|b| b.alloc(order))
}

/// Free a block [`alloc_frames_owned`] returned, once nothing maps it.
pub(crate) fn free_frames_owned(f: Frames) {
    pmm_init::with_buddy(|b| b.free(f));
}

pub(crate) fn cpu_remote(id: u32) -> Option<&'static PerCpuRemote> {
    per_cpu_init::cpu(id)
}

pub(crate) fn dying_entry() {}

pub(crate) fn second_cpu() -> Option<u32> {
    let mask = per_cpu_init::online_mask();
    let mut i = 1u32;
    while i < 64 {
        if mask & (1u64 << i) != 0 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// `ipi_init::service_incoming` from thread context. With IF on, an IPI
/// between `service_calls`' acked check and its ack would run the callback
/// twice, so this holds IF off across the call.
pub(crate) fn service_incoming_guarded() {
    let _g = x86::InterruptGuard::enter();
    ipi_init::service_incoming();
}

pub(crate) fn spin_until_ns(pred: impl Fn() -> bool, ns: u64) -> bool {
    let t0 = time_init::now_ns();
    while !pred() {
        if time_init::now_ns().saturating_sub(t0) > ns {
            return false;
        }
        service_incoming_guarded();
        core::hint::spin_loop();
    }
    true
}

const PCI_QEMU_IDS: &[(u16, u16)] = &[
    (0x8086, 0x1237), // 440FX
    (0x8086, 0x7000), // PIIX3 ISA
    (0x8086, 0x7010), // PIIX3 IDE
    (0x8086, 0x7113), // PIIX4 ACPI
    (0x1234, 0x1111), // Bochs VGA
    (0x8086, 0x100e), // e1000
];

fn test_pci_qemu_set() -> Outcome {
    if !pci_init::live() {
        return Outcome::Fail("pci not live");
    }
    if dev_init::len() < PCI_QEMU_IDS.len() {
        return Outcome::Fail("device count");
    }
    let mut i = 0usize;
    while i < PCI_QEMU_IDS.len() {
        let (v, d) = PCI_QEMU_IDS[i];
        if dev_init::find_id(v, d).is_none() {
            return Outcome::Fail("missing qemu id");
        }
        i += 1;
    }
    Outcome::Ok
}

fn test_pci_bar_map() -> Outcome {
    let Some((_, d)) = dev_init::find_id(0x1234, 0x1111) else {
        return Outcome::Fail("no vga");
    };
    let r = d.resources[0];
    if r.is_empty() {
        return Outcome::Fail("vga bar0 empty");
    }
    if r.size == 0 || r.size > pci::MAX_BAR_MAP {
        return Outcome::Fail("vga bar0 size");
    }
    if r.mapped_va == 0 {
        return Outcome::Fail("vga bar0 unmapped");
    }
    // WB physmap alias, not an ioremap UC window over the console FB.
    if r.mapped_va != paging_init::HHDM_BASE.wrapping_add(r.addr) {
        return Outcome::Fail("vga bar0 not wb physmap");
    }
    Outcome::Ok
}

fn test_pci_cfg_rw() -> Outcome {
    let bdf = Bdf::new(0, 0, 0);
    let id = pci_init::cfg_read32(bdf, CFG_VENDOR);
    if id as u16 != 0x8086 {
        return Outcome::Fail("host vendor");
    }
    if (id >> 16) as u16 != 0x1237 {
        return Outcome::Fail("host device");
    }
    let prev = pci_init::cfg_read32(bdf, CFG_COMMAND) as u16;
    pci_init::enable_mem_master(bdf);
    let now = pci_init::cfg_read32(bdf, CFG_COMMAND) as u16;
    pci_init::cfg_write32(bdf, CFG_COMMAND, prev as u32);
    if now & (CMD_MEM | CMD_MASTER) != CMD_MEM | CMD_MASTER {
        return Outcome::Fail("cmd bits");
    }
    Outcome::Ok
}

fn test_pci_claim_exclusive() -> Outcome {
    let Some((i, d)) = dev_init::find_id(0x8086, 0x100e) else {
        return Outcome::Fail("no e1000");
    };
    let mut b = 0u8;
    let mut found = false;
    while (b as usize) < pci::MAX_BARS {
        if !d.resources[b as usize].is_empty() {
            found = true;
            break;
        }
        b += 1;
    }
    if !found {
        return Outcome::Fail("e1000 no bar");
    }
    if let Err(e) = dev_init::claim(i, b) {
        return Outcome::Fail(e.as_str());
    }
    match dev_init::claim(i, b) {
        Err(ClaimError::Already) => Outcome::Ok,
        Err(_) => Outcome::Fail("wrong claim err"),
        Ok(()) => Outcome::Fail("double claim"),
    }
}

struct HostBridgeDrv;

static HOST_BRIDGE_IDS: &[IdMatch] = &[IdMatch::vid_did(0x8086, 0x1237)];
static HOST_BRIDGE_DRV: HostBridgeDrv = HostBridgeDrv;

impl Driver for HostBridgeDrv {
    fn name(&self) -> &'static str {
        "host-bridge"
    }
    fn ids(&self) -> &'static [IdMatch] {
        HOST_BRIDGE_IDS
    }
    fn order(&self) -> u8 {
        1
    }
    fn probe(&self, _dev: &mut Device) -> Result<(), ProbeError> {
        Ok(())
    }
    fn remove(&self, _dev: &mut Device) {}
}

fn test_pci_bind_order() -> Outcome {
    if !dev_init::register_driver(&HOST_BRIDGE_DRV) {
        return Outcome::Fail("register");
    }
    dev_init::bind_all();
    let Some((_, d)) = dev_init::find_id(0x8086, 0x1237) else {
        return Outcome::Fail("no host");
    };
    match d.bound {
        Some("host-bridge") => Outcome::Ok,
        Some(_) => Outcome::Fail("wrong driver"),
        None => Outcome::Fail("unbound"),
    }
}

pub(crate) fn mmio_r32(va: u64, off: u32) -> u32 {
    unsafe { core::ptr::read_volatile((va.wrapping_add(off as u64)) as *const u32) }
}

pub(crate) fn mmio_w32(va: u64, off: u32, val: u32) {
    unsafe { core::ptr::write_volatile((va.wrapping_add(off as u64)) as *mut u32, val) }
}

pub(crate) const EDU_IDENT: u32 = 0x00;
pub(crate) const EDU_IDENT_VAL: u32 = 0x0100_00ED;

pub(crate) fn bar0_va(dev: &Device) -> Option<u64> {
    let r = dev.resources[0];
    if r.mapped_va != 0 {
        Some(r.mapped_va)
    } else {
        None
    }
}

pub(crate) fn find_edu() -> Option<(usize, Device)> {
    // QEMU 8.x edu is 1234:11e8 (old QEMU vendor). Later trees use 1b36:11e8.
    dev_init::find_id(0x1234, 0x11e8).or_else(|| dev_init::find_id(0x1b36, 0x11e8))
}

fn test_dma_alloc() -> Outcome {
    let before = quiescent_free_frames();
    let Some(buf) = dma_init::alloc(DmaAlloc::dma32(0x1000)) else {
        return Outcome::Fail("alloc");
    };
    if buf.device().as_u64() != buf.phys() {
        dma_init::free(buf);
        return Outcome::Fail("device != phys");
    }
    if buf.virt() != paging_init::HHDM_BASE.wrapping_add(buf.phys()) {
        dma_init::free(buf);
        return Outcome::Fail("virt not hhdm");
    }
    if buf.device().as_u64() == buf.virt() {
        dma_init::free(buf);
        return Outcome::Fail("device is va");
    }
    if buf.phys() >= DMA32_BOUNDARY || dma::crosses_boundary(buf.phys(), buf.len(), DMA32_BOUNDARY)
    {
        dma_init::free(buf);
        return Outcome::Fail("dma32");
    }
    buf.sync_for_device();
    unsafe {
        buf.as_ptr().write_volatile(0xA5);
    }
    buf.sync_for_cpu();
    let sg = match dma::SgList::from_buffer(&buf) {
        Ok(s) => s,
        Err(_) => {
            dma_init::free(buf);
            return Outcome::Fail("sg");
        }
    };
    if sg.n != 1 || sg.entries[0].addr != buf.device() {
        dma_init::free(buf);
        return Outcome::Fail("sg entry");
    }
    dma_init::free(buf);
    if dma_init::alloc(DmaAlloc {
        size: 0x1000,
        align: 0x1000,
        boundary: 0x800,
    })
    .is_some()
    {
        return Outcome::Fail("boundary refuse");
    }
    if quiescent_free_frames() != before {
        return Outcome::Fail("leak");
    }
    Outcome::Ok
}

const EDU_DMA_SRC: u32 = 0x80;
const EDU_DMA_DST: u32 = 0x88;
const EDU_DMA_CNT: u32 = 0x90;
const EDU_DMA_CMD: u32 = 0x98;
const EDU_DMA_BUF: u32 = 0x4_0000;
const EDU_DMA_RUN: u32 = 1;
const EDU_DMA_TO_PCI: u32 = 2;

fn test_dma_edu() -> Outcome {
    let Some((_, dev)) = find_edu() else {
        return Outcome::Skip("no edu");
    };
    let Some(mmio) = bar0_va(&dev) else {
        return Outcome::Fail("edu bar0");
    };
    if mmio_r32(mmio, EDU_IDENT) != EDU_IDENT_VAL {
        return Outcome::Fail("edu ident");
    }
    pci_init::enable_mem_master(dev.addr);
    let Some(src) = dma_init::alloc(DmaAlloc::dma32(64)) else {
        return Outcome::Fail("src");
    };
    let Some(dst) = dma_init::alloc(DmaAlloc::dma32(64)) else {
        dma_init::free(src);
        return Outcome::Fail("dst");
    };
    unsafe {
        let p = src.as_ptr();
        let q = dst.as_ptr();
        let mut i = 0u32;
        while i < 64 {
            p.add(i as usize).write_volatile((0xC0 + i) as u8);
            q.add(i as usize).write_volatile(0);
            i += 1;
        }
    }
    src.sync_for_device();
    dst.sync_for_device();
    mmio_w32(mmio, EDU_DMA_SRC, src.device().as_u64() as u32);
    mmio_w32(mmio, EDU_DMA_DST, EDU_DMA_BUF);
    mmio_w32(mmio, EDU_DMA_CNT, 64);
    dma::dma_wmb();
    mmio_w32(mmio, EDU_DMA_CMD, EDU_DMA_RUN);
    if !spin_until_ns(
        || mmio_r32(mmio, EDU_DMA_CMD) & EDU_DMA_RUN == 0,
        2_000_000_000,
    ) {
        dma_init::free(src);
        dma_init::free(dst);
        return Outcome::Fail("dma to edu");
    }
    mmio_w32(mmio, EDU_DMA_SRC, EDU_DMA_BUF);
    mmio_w32(mmio, EDU_DMA_DST, dst.device().as_u64() as u32);
    mmio_w32(mmio, EDU_DMA_CNT, 64);
    dma::dma_wmb();
    mmio_w32(mmio, EDU_DMA_CMD, EDU_DMA_RUN | EDU_DMA_TO_PCI);
    if !spin_until_ns(
        || mmio_r32(mmio, EDU_DMA_CMD) & EDU_DMA_RUN == 0,
        2_000_000_000,
    ) {
        dma_init::free(src);
        dma_init::free(dst);
        return Outcome::Fail("dma from edu");
    }
    dst.sync_for_cpu();
    let mut bad = false;
    unsafe {
        let p = src.as_ptr();
        let q = dst.as_ptr();
        let mut i = 0usize;
        while i < 64 {
            if p.add(i).read_volatile() != q.add(i).read_volatile() {
                bad = true;
                break;
            }
            i += 1;
        }
    }
    dma_init::free(src);
    dma_init::free(dst);
    if bad {
        Outcome::Fail("mismatch")
    } else {
        Outcome::Ok
    }
}

fn find_rng() -> Option<(usize, Device)> {
    dev_init::find_id(0x1af4, 0x1044).or_else(|| dev_init::find_id(0x1af4, 0x1004))
}

fn test_virtio_bind() -> Outcome {
    let Some((_, d)) = find_rng() else {
        return Outcome::Skip("no virtio-rng");
    };
    if !virtio_init::rng_bound() {
        return Outcome::Fail("unbound");
    }
    match d.bound {
        Some("virtio-rng") => {}
        Some(_) => return Outcome::Fail("wrong driver"),
        None => return Outcome::Fail("id match"),
    }
    if virtio_init::rng_features() & F_VERSION_1 == 0 {
        return Outcome::Fail("no VERSION_1");
    }
    if !virtio_init::rng_uses_indirect() && !virtio_init::rng_uses_event_idx() {
        // Modern QEMU offers both; either is enough to prove negotiation.
        return Outcome::Fail("no optional feats");
    }
    Outcome::Ok
}

fn test_virtio_vq() -> Outcome {
    if !virtio_init::rng_bound() {
        return Outcome::Skip("no virtio-rng");
    }
    let qdev = virtio_init::rng_qdma_device();
    let ddev = virtio_init::rng_data_device();
    let dvirt = virtio_init::rng_data_virt();
    if qdev == 0 || ddev == 0 {
        return Outcome::Fail("dma");
    }
    if ddev == dvirt {
        return Outcome::Fail("device is va");
    }
    let c0 = virtio_init::rng_completions();
    let t0 = virtio_init::rng_top_hits();
    let th0 = virtio_init::rng_thread_hits();
    let s0 = virtio_init::rng_soft_hits();
    if virtio_init::rng_request().is_err() {
        return Outcome::Fail("request");
    }
    if !spin_until_ns(|| virtio_init::rng_completions() > c0, 2_000_000_000) {
        return Outcome::Fail("no complete");
    }
    if virtio_init::rng_last_len() == 0 {
        return Outcome::Fail("empty");
    }
    if virtio_init::rng_top_hits() <= t0 {
        return Outcome::Fail("no top");
    }
    if virtio_init::rng_thread_hits() <= th0 {
        return Outcome::Fail("no thread");
    }
    if !virtio_init::rng_alloced() {
        return Outcome::Fail("thread alloc");
    }
    if !spin_until_ns(|| virtio_init::rng_soft_hits() > s0, 2_000_000_000) {
        return Outcome::Fail("no softirq");
    }
    Outcome::Ok
}

fn test_dev_random_source() -> Outcome {
    if !virtio_init::rng_bound() {
        return Outcome::Skip("no virtio-rng");
    }
    if !fs_init::live() {
        return Outcome::Fail("not live");
    }
    let Ok(f) = fid::open("/dev/random", O_RDWR, 0) else {
        return Outcome::Fail("open");
    };
    let mut buf = [0u8; 16];
    let n = fid::read(f, &mut buf);
    let _ = fid::close(f);
    if n.ok() != Some(16) {
        return Outcome::Fail("read");
    }
    match vibeos::entropy::last_source() {
        vibeos::entropy::Source::XorShift => Outcome::Fail("xorshift"),
        vibeos::entropy::Source::VirtioRng | vibeos::entropy::Source::RdRand => Outcome::Ok,
    }
}

fn test_block_ramdisk_rw() -> Outcome {
    if !block_init::live() {
        return Outcome::Fail("block not live");
    }
    block_init::reset();
    let d = block_init::device();
    if d.name() != block_init::name() {
        return Outcome::Fail("name");
    }
    if d.logical_block_size() != block_init::logical_block_size() {
        return Outcome::Fail("bs");
    }
    if d.capacity_sectors() != block_init::capacity_sectors() {
        return Outcome::Fail("cap");
    }
    if d.state() != DeviceState::Ready {
        return Outcome::Fail("state");
    }
    let mut buf = [0u8; 512];
    let mut i = 0usize;
    while i < 512 {
        buf[i] = (i as u8).wrapping_add(0x3C);
        i += 1;
    }
    if d.write(1, &buf).is_err() {
        return Outcome::Fail("write");
    }
    let mut out = [0u8; 512];
    if d.read(1, &mut out).is_err() {
        return Outcome::Fail("read");
    }
    if out != buf {
        return Outcome::Fail("mismatch");
    }
    if d.flush().is_err() {
        return Outcome::Fail("flush");
    }
    if d.discard(1, 1).is_err() {
        return Outcome::Fail("discard");
    }
    out = [0u8; 512];
    if d.read(1, &mut out).is_err() || out != buf {
        return Outcome::Fail("discard clobber");
    }
    let mut odd = [0u8; 100];
    match d.read(0, &mut odd) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("unaligned"),
    }
    match d.write(block_init::RAM0_SECTORS, &buf) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("past end"),
    }
    match d.discard(block_init::RAM0_SECTORS, 1) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("discard past"),
    }
    if !crate::shell_init::has_command("blk") {
        return Outcome::Fail("no blk");
    }
    if crate::shell_init::dispatch_line("blk").is_err() {
        return Outcome::Fail("blk cmd");
    }
    Outcome::Ok
}

const BLK_ITERS: u32 = 60;
const BLK_SPAN: u64 = 32;
static BLK_WID: AtomicU32 = AtomicU32::new(0);
static BLK_DONE: AtomicU32 = AtomicU32::new(0);
static BLK_FAIL: AtomicU32 = AtomicU32::new(0);

fn blk_worker() {
    let id = BLK_WID.fetch_add(1, Ordering::SeqCst);
    let base = id as u64 * BLK_SPAN;
    let mut i = 0u32;
    while i < BLK_ITERS {
        let lba = base + (i as u64 % BLK_SPAN);
        let mut buf = [0u8; 512];
        let mut j = 0usize;
        while j < 512 {
            buf[j] = (id as u8).wrapping_add(i as u8).wrapping_add(j as u8);
            j += 1;
        }
        if block_init::write(lba, &buf).is_err() {
            BLK_FAIL.fetch_add(1, Ordering::SeqCst);
            break;
        }
        let mut out = [0u8; 512];
        if block_init::read(lba, &mut out).is_err() || out != buf {
            BLK_FAIL.fetch_add(1, Ordering::SeqCst);
            break;
        }
        i += 1;
    }
    BLK_DONE.fetch_add(1, Ordering::SeqCst);
}

static TEAR_ID: AtomicU32 = AtomicU32::new(0);
static TEAR_DONE: AtomicU32 = AtomicU32::new(0);

fn tear_worker() {
    let id = TEAR_ID.fetch_add(1, Ordering::SeqCst);
    let fill = if id == 0 { 0xAAu8 } else { 0x55u8 };
    let buf = [fill; 512];
    if block_init::write(0, &buf).is_err() {
        BLK_FAIL.fetch_add(1, Ordering::SeqCst);
    }
    TEAR_DONE.fetch_add(1, Ordering::SeqCst);
}

fn test_block_concurrent() -> Outcome {
    if !block_init::live() {
        return Outcome::Fail("block not live");
    }
    block_init::reset();
    BLK_WID.store(0, Ordering::SeqCst);
    BLK_DONE.store(0, Ordering::SeqCst);
    BLK_FAIL.store(0, Ordering::SeqCst);
    let Ok(_a) = thread_init::spawn("blk-a", blk_worker) else {
        return Outcome::Fail("spawn");
    };
    let Ok(_b) = thread_init::spawn("blk-b", blk_worker) else {
        return Outcome::Fail("spawn");
    };
    let t0 = time_init::uptime_ms();
    loop {
        if BLK_DONE.load(Ordering::SeqCst) == 2 {
            break;
        }
        if time_init::uptime_ms().saturating_sub(t0) > 8_000 {
            return Outcome::Fail("rw stall");
        }
        thread_init::yield_now();
    }
    if BLK_FAIL.load(Ordering::SeqCst) != 0 {
        return Outcome::Fail("rw corrupt");
    }
    TEAR_ID.store(0, Ordering::SeqCst);
    TEAR_DONE.store(0, Ordering::SeqCst);
    let Ok(_c) = thread_init::spawn("tear-a", tear_worker) else {
        return Outcome::Fail("spawn");
    };
    let Ok(_d) = thread_init::spawn("tear-b", tear_worker) else {
        return Outcome::Fail("spawn");
    };
    let t1 = time_init::uptime_ms();
    loop {
        if TEAR_DONE.load(Ordering::SeqCst) == 2 {
            break;
        }
        if time_init::uptime_ms().saturating_sub(t1) > 8_000 {
            return Outcome::Fail("tear stall");
        }
        thread_init::yield_now();
    }
    if BLK_FAIL.load(Ordering::SeqCst) != 0 {
        return Outcome::Fail("tear write");
    }
    let mut out = [0u8; 512];
    if block_init::read(0, &mut out).is_err() {
        return Outcome::Fail("tear read");
    }
    let b0 = out[0];
    if b0 != 0xAA && b0 != 0x55 {
        return Outcome::Fail("tear pattern");
    }
    let mut i = 1usize;
    while i < 512 {
        if out[i] != b0 {
            return Outcome::Fail("torn sector");
        }
        i += 1;
    }
    Outcome::Ok
}

fn test_block_retry() -> Outcome {
    if !block_init::live() {
        return Outcome::Fail("block not live");
    }
    block_init::reset();
    let mut buf = [0x11u8; 512];
    block_init::inject_io_fails(3);
    if block_init::write(3, &buf).is_err() {
        return Outcome::Fail("retry should pass");
    }
    let mut out = [0u8; 512];
    if block_init::read(3, &mut out).is_err() || out != buf {
        return Outcome::Fail("after retry");
    }
    block_init::inject_io_fails(4);
    match block_init::write(4, &buf) {
        Err(BlockError::Failed) => {}
        _ => return Outcome::Fail("expected failed"),
    }
    if block_init::state() != DeviceState::Failed {
        return Outcome::Fail("not marked failed");
    }
    match block_init::read(3, &mut out) {
        Err(BlockError::Failed) => {}
        _ => return Outcome::Fail("submit after fail"),
    }
    block_init::reset();
    buf[0] = 0x22;
    if block_init::write(3, &buf).is_err() {
        return Outcome::Fail("reset");
    }
    Outcome::Ok
}

fn find_blk() -> Option<(usize, Device)> {
    dev_init::find_id(0x1af4, 0x1042).or_else(|| dev_init::find_id(0x1af4, 0x1001))
}

fn test_block_vblk_rw() -> Outcome {
    if !virtio_blk_init::live() {
        return Outcome::Skip("no virtio-blk");
    }
    let Some((_, d)) = find_blk() else {
        return Outcome::Fail("id missing");
    };
    match d.bound {
        Some("virtio-blk") => {}
        Some(_) => return Outcome::Fail("wrong driver"),
        None => return Outcome::Fail("unbound"),
    }
    let d = match virtio_blk_init::device() {
        Some(d) => d,
        None => return Outcome::Fail("no device"),
    };
    if d.name() != virtio_blk_init::name() {
        return Outcome::Fail("name");
    }
    if d.logical_block_size() == 0 || d.logical_block_size() % 512 != 0 {
        return Outcome::Fail("bs");
    }
    if d.capacity_sectors() < 16 {
        return Outcome::Fail("cap");
    }
    if d.state() != DeviceState::Ready {
        return Outcome::Fail("state");
    }
    let bs = d.logical_block_size() as usize;
    if bs != 512 {
        return Outcome::Fail("need 512");
    }
    let mut buf = [0u8; 512];
    let mut i = 0usize;
    while i < 512 {
        buf[i] = (i as u8).wrapping_add(0xA1);
        i += 1;
    }
    if d.write(1, &buf).is_err() {
        return Outcome::Fail("write");
    }
    let mut out = [0u8; 512];
    if d.read(1, &mut out).is_err() {
        return Outcome::Fail("read");
    }
    if out != buf {
        return Outcome::Fail("mismatch");
    }
    // unaligned multi-sector: 3 sectors not at LBA 0
    let mut multi = [0u8; 1536];
    i = 0;
    while i < 1536 {
        multi[i] = (i as u8).wrapping_add(0x5C);
        i += 1;
    }
    if d.write(5, &multi).is_err() {
        return Outcome::Fail("multi write");
    }
    let mut mout = [0u8; 1536];
    if d.read(5, &mut mout).is_err() || mout != multi {
        return Outcome::Fail("multi read");
    }
    if d.flush().is_err() {
        return Outcome::Fail("flush");
    }
    if !virtio_blk_init::has_flush() {
        // device did not offer F_FLUSH; flush is a successful no-op
    }
    if virtio_blk_init::has_discard() && d.discard(5, 1).is_err() {
        return Outcome::Fail("discard");
    }
    match d.read(0, &mut [0u8; 100]) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("unaligned buf"),
    }
    match d.write(d.capacity_sectors(), &buf) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("past end"),
    }
    Outcome::Ok
}

fn test_block_vblk_irq() -> Outcome {
    if !virtio_blk_init::live() {
        return Outcome::Skip("no virtio-blk");
    }
    let t0 = virtio_blk_init::top_hits();
    let th0 = virtio_blk_init::thread_hits();
    let c0 = virtio_blk_init::completions();
    let buf = [0x3Du8; 512];
    if virtio_blk_init::write(2, &buf).is_err() {
        return Outcome::Fail("write");
    }
    let mut out = [0u8; 512];
    if virtio_blk_init::read(2, &mut out).is_err() || out != buf {
        return Outcome::Fail("read");
    }
    if virtio_blk_init::completions() <= c0 {
        return Outcome::Fail("no complete");
    }
    if virtio_blk_init::top_hits() <= t0 {
        return Outcome::Fail("no top");
    }
    if virtio_blk_init::thread_hits() <= th0 {
        return Outcome::Fail("no thread");
    }
    Outcome::Ok
}

fn test_block_vblk_deep() -> Outcome {
    if !virtio_blk_init::live() {
        return Outcome::Skip("no virtio-blk");
    }
    let w0 = IoWaiter::new();
    let w1 = IoWaiter::new();
    let w2 = IoWaiter::new();
    let w3 = IoWaiter::new();
    let w4 = IoWaiter::new();
    let w5 = IoWaiter::new();
    let w6 = IoWaiter::new();
    let w7 = IoWaiter::new();
    let b0 = [0x10u8; 512];
    let b1 = [0x11u8; 512];
    let b2 = [0x12u8; 512];
    let b3 = [0x13u8; 512];
    let b4 = [0x14u8; 512];
    let b5 = [0x15u8; 512];
    let b6 = [0x16u8; 512];
    let b7 = [0x17u8; 512];
    // gapped LBAs so the elevator does not merge them into one VQ request
    let subs = [
        virtio_blk_init::submit(Op::Write, 10, 1, b0.as_ptr() as usize, 512, &w0),
        virtio_blk_init::submit(Op::Write, 12, 1, b1.as_ptr() as usize, 512, &w1),
        virtio_blk_init::submit(Op::Write, 14, 1, b2.as_ptr() as usize, 512, &w2),
        virtio_blk_init::submit(Op::Write, 16, 1, b3.as_ptr() as usize, 512, &w3),
        virtio_blk_init::submit(Op::Write, 18, 1, b4.as_ptr() as usize, 512, &w4),
        virtio_blk_init::submit(Op::Write, 20, 1, b5.as_ptr() as usize, 512, &w5),
        virtio_blk_init::submit(Op::Write, 22, 1, b6.as_ptr() as usize, 512, &w6),
        virtio_blk_init::submit(Op::Write, 24, 1, b7.as_ptr() as usize, 512, &w7),
    ];
    let mut i = 0usize;
    while i < 8 {
        if subs[i].is_err() {
            return Outcome::Fail("submit");
        }
        i += 1;
    }
    if w0.wait().is_err()
        || w1.wait().is_err()
        || w2.wait().is_err()
        || w3.wait().is_err()
        || w4.wait().is_err()
        || w5.wait().is_err()
        || w6.wait().is_err()
        || w7.wait().is_err()
    {
        return Outcome::Fail("wait");
    }
    let mut out = [0u8; 512];
    if virtio_blk_init::read(10, &mut out).is_err() || out != b0 {
        return Outcome::Fail("r0");
    }
    if virtio_blk_init::read(18, &mut out).is_err() || out != b4 {
        return Outcome::Fail("r4");
    }
    if virtio_blk_init::read(24, &mut out).is_err() || out != b7 {
        return Outcome::Fail("r7");
    }
    Outcome::Ok
}

const VBLK_ITERS: u32 = 40;
const VBLK_SPAN: u64 = 16;
static VBLK_WID: AtomicU32 = AtomicU32::new(0);
static VBLK_DONE: AtomicU32 = AtomicU32::new(0);
static VBLK_FAIL: AtomicU32 = AtomicU32::new(0);

fn vblk_worker() {
    let id = VBLK_WID.fetch_add(1, Ordering::SeqCst);
    let base = 32 + id as u64 * VBLK_SPAN;
    let mut i = 0u32;
    while i < VBLK_ITERS {
        let lba = base + (i as u64 % VBLK_SPAN);
        let mut buf = [0u8; 512];
        let mut j = 0usize;
        while j < 512 {
            buf[j] = (id as u8).wrapping_add(i as u8).wrapping_add(j as u8);
            j += 1;
        }
        if virtio_blk_init::write(lba, &buf).is_err() {
            VBLK_FAIL.fetch_add(1, Ordering::SeqCst);
            break;
        }
        let mut out = [0u8; 512];
        if virtio_blk_init::read(lba, &mut out).is_err() || out != buf {
            VBLK_FAIL.fetch_add(1, Ordering::SeqCst);
            break;
        }
        i += 1;
    }
    VBLK_DONE.fetch_add(1, Ordering::SeqCst);
}

fn test_block_vblk_concurrent() -> Outcome {
    if !virtio_blk_init::live() {
        return Outcome::Skip("no virtio-blk");
    }
    VBLK_WID.store(0, Ordering::SeqCst);
    VBLK_DONE.store(0, Ordering::SeqCst);
    VBLK_FAIL.store(0, Ordering::SeqCst);
    let Ok(_a) = thread_init::spawn("vblk-a", vblk_worker) else {
        return Outcome::Fail("spawn");
    };
    let Ok(_b) = thread_init::spawn("vblk-b", vblk_worker) else {
        return Outcome::Fail("spawn");
    };
    let t0 = time_init::uptime_ms();
    loop {
        if VBLK_DONE.load(Ordering::SeqCst) == 2 {
            break;
        }
        if time_init::uptime_ms().saturating_sub(t0) > 15_000 {
            return Outcome::Fail("stall");
        }
        thread_init::yield_now();
    }
    if VBLK_FAIL.load(Ordering::SeqCst) != 0 {
        return Outcome::Fail("corrupt");
    }
    Outcome::Ok
}

fn test_block_vblk_mq() -> Outcome {
    if !virtio_blk_init::live() {
        return Outcome::Skip("no virtio-blk");
    }
    let nq = virtio_blk_init::num_queues();
    if nq == 0 {
        return Outcome::Fail("zero queues");
    }
    let cpus = per_cpu_init::online_mask().count_ones() as u8;
    if virtio_blk_init::has_mq() {
        if nq < 2 && cpus >= 2 {
            return Outcome::Fail("mq expected");
        }
        if nq > cpus {
            return Outcome::Fail("nq > cpus");
        }
    } else if nq != 1 {
        return Outcome::Fail("sq fallback");
    }
    Outcome::Ok
}

const PERSIST_MAGIC: [u8; 8] = *b"vibeOS7B";

fn test_block_persist() -> Outcome {
    if !virtio_blk_init::live() {
        return Outcome::Skip("no virtio-blk");
    }
    let lba = virtio_blk_init::persist_lba();
    if lba == 0 {
        return Outcome::Fail("no persist lba");
    }
    let mut buf = [0u8; 512];
    if virtio_blk_init::read(lba, &mut buf).is_err() {
        return Outcome::Fail("read");
    }
    if buf[0..8] == PERSIST_MAGIC {
        let mut i = 8usize;
        while i < 512 {
            if buf[i] != 0xA5 {
                return Outcome::Fail("corrupt");
            }
            i += 1;
        }
        crate::marker!("vibeOS: persist: intact");
        return Outcome::Ok;
    }
    buf[0..8].copy_from_slice(&PERSIST_MAGIC);
    let mut i = 8usize;
    while i < 512 {
        buf[i] = 0xA5;
        i += 1;
    }
    if virtio_blk_init::write(lba, &buf).is_err() {
        return Outcome::Fail("write");
    }
    if virtio_blk_init::flush().is_err() {
        return Outcome::Fail("flush");
    }
    crate::marker!("vibeOS: persist: wrote");
    Outcome::Ok
}

fn test_block_part_mbr() -> Outcome {
    if !part_init::live() {
        return Outcome::Fail("parts not live");
    }
    let Some(i) = part_init::find_name("ram0p1") else {
        return Outcome::Fail("no ram0p1");
    };
    let Some(i2) = part_init::find_name("ram0p2") else {
        return Outcome::Fail("no ram0p2");
    };
    if part_init::find_name("ram0p3").is_none() {
        return Outcome::Fail("no ram0p3");
    }
    let Some(d) = part_init::device(i) else {
        return Outcome::Fail("no device");
    };
    if d.name() != "ram0p1" {
        return Outcome::Fail("name");
    }
    if d.capacity_sectors() != 32 {
        return Outcome::Fail("cap");
    }
    let mut buf = [0u8; 512];
    buf[0] = 0xC1;
    buf[511] = 0xC2;
    if d.write(0, &buf).is_err() {
        return Outcome::Fail("write");
    }
    let mut out = [0u8; 512];
    if d.read(0, &mut out).is_err() || out != buf {
        return Outcome::Fail("read");
    }
    match d.write(32, &buf) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("overflow"),
    }
    match d.read(31, &mut [0u8; 1024]) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("overflow 2"),
    }
    let Some((_, n2, _, k2)) = part_init::info(i2) else {
        return Outcome::Fail("info p2");
    };
    if n2 != 24 {
        return Outcome::Fail("logical size");
    }
    if part_init::type_str(k2) != "linux" {
        return Outcome::Fail("type");
    }
    Outcome::Ok
}

fn test_block_part_gpt() -> Outcome {
    if !virtio_blk_init::live() {
        return Outcome::Skip("no virtio-blk");
    }
    let Some(i1) = part_init::find_name("vdap1") else {
        return Outcome::Fail("no vdap1");
    };
    let Some(i2) = part_init::find_name("vdap2") else {
        return Outcome::Fail("no vdap2");
    };
    let Some((_, _, _, k1)) = part_init::info(i1) else {
        return Outcome::Fail("info p1");
    };
    let Some((_, n2, _, k2)) = part_init::info(i2) else {
        return Outcome::Fail("info p2");
    };
    if part_init::type_str(k1) != "efi" {
        return Outcome::Fail("efi guid");
    }
    if part_init::type_str(k2) != "linux" {
        return Outcome::Fail("linux guid");
    }
    if n2 < 16 {
        return Outcome::Fail("linux small");
    }
    let Some(d) = part_init::device(i1) else {
        return Outcome::Fail("no d1");
    };
    let mut buf = [0u8; 512];
    buf[0] = 0xE1;
    buf[100] = 0xE2;
    if d.write(1, &buf).is_err() {
        return Outcome::Fail("write");
    }
    let mut out = [0u8; 512];
    if d.read(1, &mut out).is_err() || out != buf {
        return Outcome::Fail("read");
    }
    if d.flush().is_err() {
        return Outcome::Fail("flush");
    }
    match d.write(d.capacity_sectors(), &buf) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("gpt overflow"),
    }
    Outcome::Ok
}

fn test_block_cache_hit() -> Outcome {
    if !cache_init::live() || !block_init::live() {
        return Outcome::Fail("not live");
    }
    let lba = 200u64;
    let mut buf = [0u8; 512];
    buf[3] = 0x44;
    if block_init::write(lba, &buf).is_err() {
        return Outcome::Fail("seed");
    }
    let raw0 = block_init::io_reqs();
    if block_init::read(lba, &mut [0u8; 512]).is_err() {
        return Outcome::Fail("raw1");
    }
    if block_init::read(lba, &mut [0u8; 512]).is_err() {
        return Outcome::Fail("raw2");
    }
    let raw_delta = block_init::io_reqs().saturating_sub(raw0);
    let s0 = cache_init::stats();
    let mut out = [0u8; 512];
    if cache_init::read(cache_init::DEV_RAM0, lba, &mut out).is_err() {
        return Outcome::Fail("c1");
    }
    if out != buf {
        return Outcome::Fail("data");
    }
    let s1 = cache_init::stats();
    if cache_init::read(cache_init::DEV_RAM0, lba, &mut out).is_err() || out != buf {
        return Outcome::Fail("c2");
    }
    let s2 = cache_init::stats();
    if s1.device_reads <= s0.device_reads {
        return Outcome::Fail("miss reqs");
    }
    if s2.device_reads != s1.device_reads {
        return Outcome::Fail("hit extra req");
    }
    if s2.hits <= s1.hits {
        return Outcome::Fail("no hit");
    }
    if raw_delta < 2 {
        return Outcome::Fail("raw not 2");
    }
    let cached = s2.device_reads.saturating_sub(s0.device_reads);
    if cached >= raw_delta {
        return Outcome::Fail("no reduce");
    }
    crate::marker!(
        "vibeOS: cache: hits {} misses {} device {} raw {}",
        s2.hits,
        s2.misses,
        s2.device_reqs(),
        raw_delta
    );
    Outcome::Ok
}

fn test_block_cache_evict() -> Outcome {
    if !cache_init::live() || !block_init::live() {
        return Outcome::Fail("not live");
    }
    let s0 = cache_init::stats();
    let mut buf = [0u8; 512];
    let mut i = 0u64;
    while i < 18 {
        let lba = i * 8;
        buf[0] = i as u8;
        if cache_init::read(cache_init::DEV_RAM0, lba, &mut buf).is_err() {
            return Outcome::Fail("fill");
        }
        i += 1;
    }
    let s1 = cache_init::stats();
    if s1.evicts <= s0.evicts {
        return Outcome::Fail("no evict");
    }
    Outcome::Ok
}

fn test_vfs_walk() -> Outcome {
    if !fs_init::live() {
        return Outcome::Fail("not live");
    }
    if file_init::mkdir(b"/a", 0o755).is_err() {
        return Outcome::Fail("mkdir");
    }
    if file_init::creat(b"/a/f").is_err() {
        return Outcome::Fail("creat");
    }
    match fid::stat_path("/a/./f") {
        Ok(s) if s.kind == InodeKind::Reg => {}
        _ => return Outcome::Fail("dot walk"),
    }
    match fid::stat_path("/a/f/../f") {
        Ok(s) if s.kind == InodeKind::Reg => {}
        _ => return Outcome::Fail("dotdot"),
    }
    if file_init::mkdir(b"/ram", 0o755).is_err() {
        return Outcome::Fail("ramdir");
    }
    if file_init::mount(b"none", b"/ram", b"ramfs", false).is_err() {
        return Outcome::Fail("mount");
    }
    if file_init::creat(b"/ram/f").is_err() {
        return Outcome::Fail("rf");
    }
    if file_init::symlink_path(b"/ram/l", b"/ram/f").is_err() {
        return Outcome::Fail("symlink");
    }
    match fid::stat_path("/ram/l") {
        Ok(s) if s.kind == InodeKind::Reg => {}
        _ => return Outcome::Fail("follow"),
    }
    if file_init::symlink_path(b"/ram/loop", b"/ram/loop").is_err() {
        return Outcome::Fail("loopc");
    }
    match fid::stat_path("/ram/loop") {
        Err(FsError::Loop) => {}
        _ => return Outcome::Fail("noloop"),
    }
    match fid::stat_path("/ram/..") {
        Ok(s) if s.kind == InodeKind::Dir => {}
        _ => return Outcome::Fail("cross"),
    }
    Outcome::Ok
}

fn dir_has(path: &str, want: &[u8]) -> bool {
    let flags = vibeos::fs::OpenFlags::from_bits(vibeos::fs::O_RDONLY | vibeos::fs::O_DIRECTORY);
    let Ok(f) = file_init::open(path.as_bytes(), flags, 0) else {
        return false;
    };
    let mut hit = false;
    let r = file_init::readdir(&f, &mut |d| {
        hit = d.name.eq_bytes(want);
        !hit
    });
    let _ = file_init::close(f);
    r.is_ok() && hit
}

fn test_pseudo_fs() -> Outcome {
    if !fs_init::live() {
        return Outcome::Fail("not live");
    }
    match fid::stat_path("/dev") {
        Ok(s) if s.kind == InodeKind::Dir => {}
        _ => return Outcome::Fail("/dev"),
    }
    match fid::stat_path("/proc") {
        Ok(s) if s.kind == InodeKind::Dir => {}
        _ => return Outcome::Fail("/proc"),
    }
    match fid::stat_path("/tmp") {
        Ok(s) if s.kind == InodeKind::Dir => {}
        _ => return Outcome::Fail("/tmp"),
    }
    match fid::stat_path("/sys") {
        Ok(s) if s.kind == InodeKind::Dir => {}
        _ => return Outcome::Fail("/sys"),
    }
    if !dir_has("/dev", b"null")
        || !dir_has("/dev", b"zero")
        || !dir_has("/dev", b"random")
        || !dir_has("/dev", b"console")
        || !dir_has("/dev", b"tty")
    {
        return Outcome::Fail("dev chars");
    }
    if !dir_has("/dev", b"ram0") {
        return Outcome::Fail("dev ram0");
    }
    match fid::stat_path("/dev/null") {
        Ok(s) if s.kind == InodeKind::Chr => {}
        _ => return Outcome::Fail("null kind"),
    }
    let Ok(f) = fid::open("/dev/null", O_RDWR, 0) else {
        return Outcome::Fail("open null");
    };
    if fid::write(f, b"x").ok() != Some(1) {
        let _ = fid::close(f);
        return Outcome::Fail("write null");
    }
    let _ = fid::close(f);
    let Ok(z) = fid::open("/dev/zero", O_RDWR, 0) else {
        return Outcome::Fail("open zero");
    };
    let mut buf = [0xFFu8; 4];
    if fid::read(z, &mut buf).ok() != Some(4) || buf != [0u8; 4] {
        let _ = fid::close(z);
        return Outcome::Fail("read zero");
    }
    let _ = fid::close(z);
    let Ok(r) = fid::open("/dev/random", O_RDWR, 0) else {
        return Outcome::Fail("open rand");
    };
    if fid::read(r, &mut buf).ok() != Some(4) {
        let _ = fid::close(r);
        return Outcome::Fail("read rand");
    }
    let _ = fid::close(r);
    if fid::creat("/tmp/f").is_err() {
        return Outcome::Fail("tmp creat");
    }
    let Ok(t) = fid::open("/tmp/f", O_RDWR | O_CREAT, 0o644) else {
        return Outcome::Fail("tmp open");
    };
    if fid::write(t, b"ok").ok() != Some(2) {
        let _ = fid::close(t);
        return Outcome::Fail("tmp write");
    }
    if fid::seek(t, 0, vibeos::fs::SEEK_SET).is_err() {
        let _ = fid::close(t);
        return Outcome::Fail("tmp seek");
    }
    buf = [0u8; 4];
    if fid::read(t, &mut buf).ok() != Some(2) || &buf[..2] != b"ok" {
        let _ = fid::close(t);
        return Outcome::Fail("tmp read");
    }
    let _ = fid::close(t);
    if !dir_has("/proc", b"1") || !dir_has("/proc", b"self") {
        return Outcome::Fail("proc stubs");
    }
    if !dir_has("/proc/1", b"cmdline")
        || !dir_has("/proc/1", b"status")
        || !dir_has("/proc/1", b"maps")
        || !dir_has("/proc/1", b"fd")
    {
        return Outcome::Fail("proc/1");
    }
    let Ok(c) = fid::open("/proc/1/cmdline", O_RDWR, 0) else {
        return Outcome::Fail("cmdline");
    };
    buf = [0u8; 4];
    match fid::read(c, &mut buf) {
        Ok(n) if n > 0 => {}
        _ => {
            let _ = fid::close(c);
            return Outcome::Fail("cmdline read");
        }
    }
    let _ = fid::close(c);
    if !dir_has("/sys", b"devices") || !dir_has("/sys", b"bus") {
        return Outcome::Fail("sys skeleton");
    }
    Outcome::Ok
}

fn test_fat_initrd() -> Outcome {
    if !fat_init::live() {
        return Outcome::Fail("not live");
    }
    if fat_init::nvol() == 0 {
        return Outcome::Fail("nvol");
    }
    match fid::stat_path("/hello.txt") {
        Ok(s) if s.kind == InodeKind::Reg && s.size > 0 => {}
        Ok(_) => return Outcome::Fail("hello meta"),
        Err(_) => match fid::stat_path("/HELLO.TXT") {
            Ok(s) if s.kind == InodeKind::Reg && s.size > 0 => {}
            _ => return Outcome::Fail("hello"),
        },
    }
    if file_init::mkdir(b"/kt", 0o755).is_err() {
        return Outcome::Fail("mkdir");
    }
    match fid::open("/kt/w.txt", O_RDWR | O_CREAT, 0o644) {
        Ok(fid) => {
            if fid::write(fid, b"abc").ok() != Some(3) {
                let _ = fid::close(fid);
                return Outcome::Fail("write");
            }
            if fid::seek(fid, 0, vibeos::fs::SEEK_SET).is_err() {
                let _ = fid::close(fid);
                return Outcome::Fail("seek");
            }
            let mut buf = [0u8; 4];
            match fid::read(fid, &mut buf) {
                Ok(3) if &buf[..3] == b"abc" => {}
                _ => {
                    let _ = fid::close(fid);
                    return Outcome::Fail("read");
                }
            }
            let _ = fid::close(fid);
        }
        Err(_) => return Outcome::Fail("open"),
    }
    if file_init::truncate_path(b"/kt/w.txt", 1).is_err() {
        return Outcome::Fail("trunc");
    }
    if fid::unlink_path("/kt/w.txt", false).is_err() {
        return Outcome::Fail("unlink");
    }
    if file_init::symlink_path(b"/s", b"/kt").err() != Some(FsError::NotSupp) {
        return Outcome::Fail("symlink supp");
    }
    if file_init::link_path(b"/hello.txt", b"/h2").err() != Some(FsError::NotSupp) {
        return Outcome::Fail("link supp");
    }
    if file_init::sync_fs().is_err() {
        return Outcome::Fail("sync");
    }
    Outcome::Ok
}

fn test_vibefs() -> Outcome {
    if !vibefs_init::live() {
        return Outcome::Fail("not live");
    }
    if vibefs_init::nvol() == 0 {
        return Outcome::Fail("nvol");
    }
    match fid::stat_path("/vibe") {
        Ok(s) if s.kind == InodeKind::Dir => {}
        _ => return Outcome::Fail("mount"),
    }
    if file_init::mkdir(b"/vibe/d", 0o755).is_err() {
        return Outcome::Fail("mkdir");
    }
    match fid::open("/vibe/d/f", O_RDWR | O_CREAT, 0o644) {
        Ok(fid) => {
            if fid::write(fid, b"hello").ok() != Some(5) {
                let _ = fid::close(fid);
                return Outcome::Fail("write");
            }
            if fid::seek(fid, 0, vibeos::fs::SEEK_SET).is_err() {
                let _ = fid::close(fid);
                return Outcome::Fail("seek");
            }
            let mut buf = [0u8; 8];
            match fid::read(fid, &mut buf) {
                Ok(5) if &buf[..5] == b"hello" => {}
                _ => {
                    let _ = fid::close(fid);
                    return Outcome::Fail("read");
                }
            }
            let _ = fid::close(fid);
        }
        Err(_) => return Outcome::Fail("open"),
    }
    match fid::stat_path("/vibe/d/f") {
        Ok(s) if s.kind == InodeKind::Reg && (s.mode & 0o777) == 0o644 => {}
        _ => return Outcome::Fail("mode"),
    }
    if file_init::symlink_path(b"/vibe/l", b"/vibe/d/f").is_err() {
        return Outcome::Fail("symlink");
    }
    match fid::open("/vibe/big", O_RDWR | O_CREAT, 0o644) {
        Ok(fid) => {
            let payload = [b'x'; 200];
            if fid::write(fid, &payload).ok() != Some(200) {
                let _ = fid::close(fid);
                return Outcome::Fail("extent w");
            }
            if fid::seek(fid, 0, vibeos::fs::SEEK_SET).is_err() {
                let _ = fid::close(fid);
                return Outcome::Fail("extent seek");
            }
            let mut out = [0u8; 200];
            match fid::read(fid, &mut out) {
                Ok(200) if out == payload => {}
                _ => {
                    let _ = fid::close(fid);
                    return Outcome::Fail("extent r");
                }
            }
            let _ = fid::close(fid);
        }
        Err(_) => return Outcome::Fail("extent open"),
    }
    if vibefs_init::snapshot(vibefs_init::VOL_MEM, b"s0").is_err() {
        return Outcome::Fail("snap");
    }
    if file_init::sync_fs().is_err() {
        return Outcome::Fail("sync");
    }
    Outcome::Ok
}

/// The free-frame count at a quiescent point (ROADMAP §10.2, F074): the
/// shared warm-up has run (once per boot, from whichever caller comes
/// first), no thread but the caller and the idle threads is runnable, and
/// no dead thread's stack is still on its way back. Every frame-accounting
/// test takes its `before` and `after` from here.
pub(crate) fn quiescent_free_frames() -> usize {
    quiesce_frames();
    if !quiesce() {
        crate::marker!("vibeOS: ktest:   quiesce: threads did not settle");
    }
    free_frames()
}

/// Wait, bounded, until no thread but this one and the idle threads is
/// Ready or Running and no dead thread's stack sits in a CPU's dead-stack
/// slot or on its dead list. False if that did not happen in time.
pub(crate) fn quiesce() -> bool {
    settle_threads()
}

/// The File API with the copyable [`FileId`] handles the earlier suites
/// were written against: each call takes back, or hands out, the count a
/// [`FileRef`] carries, so their scenarios and assertions stay as they
/// were.
pub(crate) mod fid {
    use vibeos::fs::{FileId, FileRef, FsError, OpenFlags, SeekFrom, Stat};

    use crate::file_init;

    pub(crate) fn open(path: &str, flags: u32, mode: u32) -> Result<FileId, FsError> {
        file_init::open(path.as_bytes(), OpenFlags::from_bits(flags), mode).map(FileRef::into_raw)
    }

    pub(crate) fn read(id: FileId, buf: &mut [u8]) -> Result<usize, FsError> {
        file_init::read(&FileRef::from_raw(id), buf)
    }

    pub(crate) fn write(id: FileId, buf: &[u8]) -> Result<usize, FsError> {
        file_init::write(&FileRef::from_raw(id), buf)
    }

    pub(crate) fn seek(id: FileId, off: i64, whence: u32) -> Result<u64, FsError> {
        let pos = SeekFrom::from_whence(off, whence)?;
        file_init::seek(&FileRef::from_raw(id), pos)
    }

    pub(crate) fn close(id: FileId) -> Result<(), FsError> {
        file_init::close(FileRef::from_raw(id))
    }

    pub(crate) fn addref(id: FileId) -> Result<(), FsError> {
        file_init::addref(id)
    }

    pub(crate) fn stat_path(path: &str) -> Result<Stat, FsError> {
        file_init::stat_path(path.as_bytes())
    }

    /// `lstat` of absolute `path`.
    pub(crate) fn lstat_path(path: &str) -> Result<Stat, FsError> {
        crate::fs_init::api().stat_path(None, path.as_bytes(), false)
    }

    pub(crate) fn creat(path: &str) -> Result<(), FsError> {
        file_init::creat(path.as_bytes())
    }

    pub(crate) fn unlink_path(path: &str, rmdir: bool) -> Result<(), FsError> {
        if rmdir {
            file_init::rmdir(path.as_bytes())
        } else {
            file_init::unlink(path.as_bytes())
        }
    }
}

/// CPUID.01H:ECX[31] (a hypervisor is present) and leaf `0x4000_0000`
/// naming it `KVMKVMKVM\0\0\0`.
pub(crate) fn on_kvm() -> bool {
    let (_, _, ecx1, _) = x86::cpuid(1, 0);
    if ecx1 & (1 << 31) == 0 {
        return false;
    }
    let (_, b, c, d) = x86::cpuid(0x4000_0000, 0);
    let mut id = [0u8; 12];
    id[..4].copy_from_slice(&b.to_le_bytes());
    id[4..8].copy_from_slice(&c.to_le_bytes());
    id[8..].copy_from_slice(&d.to_le_bytes());
    &id == b"KVMKVMKVM\0\0\0"
}

/// Sleep until `pred` holds, for at most `ms`.
pub(crate) fn sleep_until(pred: impl Fn() -> bool, ms: u64) -> bool {
    let deadline = time_init::now_ns().saturating_add(ms.saturating_mul(1_000_000));
    while !pred() {
        if time_init::now_ns() >= deadline {
            return false;
        }
        thread_init::sleep_ms(1);
    }
    true
}

/// Sleep until `pred` holds, for at most `ms`.
pub(crate) fn sleep_until_s19(pred: impl Fn() -> bool, ms: u64) -> bool {
    let deadline = time_init::now_ns().saturating_add(ms.saturating_mul(1_000_000));
    while !pred() {
        if time_init::now_ns() >= deadline {
            return false;
        }
        thread_init::sleep_ms(1);
    }
    true
}

/// Spin on TSC time until `pred` holds, for at most `ns`.
pub(crate) fn spin_until(pred: impl Fn() -> bool, ns: u64) -> bool {
    let t0 = time_init::now_ns();
    while !pred() {
        if time_init::now_ns().saturating_sub(t0) > ns {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}
