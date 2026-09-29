//! In-guest test registry. DESIGN §8.2.
//!
//! Built only with `--features kernel_tests`. After normal init this
//! module runs the registry over the real IDT, prints the serial protocol,
//! and exits QEMU through `isa-debug-exit`.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vibeos::dev::Device;
use vibeos::fmt_util::StackBuf;
use vibeos::lock::RANK_DEVICE;
use vibeos::paging::PhysAddr;
use vibeos::per_cpu::PerCpuRemote;
use vibeos::pmm::Frames;
use vibeos::thread::{ThreadId, ThreadState};
use vibeos::vectors;

use crate::ipi_init;
use crate::kva_init;
use crate::per_cpu_init;
use crate::pmm_init;
use crate::sync_init::SpinMutex;
use crate::thread_init::{self, ThreadHandle};
use crate::time_init;
use crate::x86;
use crate::{
    acpi, arch, block, boot, console, dev, drivers, fs, irq, log, mm, proc, sched, shell, smp,
    sync, time,
};
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
/// boundary. Build one with [`crate::fail_fmt!`]. It writes through
/// [`StackBuf`], the one fixed-buffer writer (DESIGN §8.2).
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

    /// Append the longest prefix of `s` that ends on a character boundary
    /// and fits; after the first character that does not fit, nothing more.
    fn push_whole(&mut self, s: &str) {
        if self.full {
            return;
        }
        let at = self.len as usize;
        let room = FAIL_MSG_BYTES - at;
        let mut n = s.len().min(room);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        let mut w = StackBuf::new(&mut self.buf[at..]);
        w.push_bytes(&s.as_bytes()[..n]);
        // `n <= room <= FAIL_MSG_BYTES`, which fits a u8.
        self.len += w.len() as u8;
        if n < s.len() {
            self.full = true;
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
    test("fw_cfg_probe", boot::ktest::test_fw_cfg_probe),
    test("fw_cfg_dma", boot::ktest::test_fw_cfg_dma),
    test("cmdline_captured", boot::ktest::test_cmdline_captured),
    test(
        "strace_flag_matches_cmdline",
        boot::ktest::test_strace_flag_matches_cmdline,
    ),
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
    test(
        "late_wake_after_exit",
        sync::ktest::test_late_wake_after_exit,
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
        "spin_poll_hook_installed",
        sync::ktest::test_spin_poll_hook_installed,
    ),
    test(
        "rank_alloc_under_pt_asserts",
        sync::ktest::test_rank_alloc_under_pt_asserts,
    ),
    test(
        "rank_same_rank_lock_asserts",
        sync::ktest::test_rank_same_rank_lock_asserts,
    ),
    test(
        "rank_lock_nested_keeps_outer",
        sync::ktest::test_rank_lock_nested_keeps_outer,
    ),
    test(
        "cross_cpu_cells_ranked",
        sync::ktest::test_cross_cpu_cells_ranked,
    ),
    test("op_gate_kill_sleeps", sync::ktest::test_op_gate_kill_sleeps).once(),
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
        "counted_deferred_release",
        sched::ktest::test_counted_deferred_release,
    ),
    test(
        "reschedule_ipi_wake_ap",
        irq::ktest::test_reschedule_ipi_wake_ap,
    ),
    test("call_function_ipi", irq::ktest::test_call_function_ipi),
    test(
        "reschedule_hook_installed",
        irq::ktest::test_reschedule_hook_installed,
    ),
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
    test(
        "log_reentry_drop_counted",
        log::ktest::test_log_reentry_drop_counted,
    ),
    test("serial_lines_whole", log::ktest::test_serial_lines_whole).deadline(60_000),
    test("serial_frame", log::ktest::test_serial_frame),
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
    test("pci_qemu_set", dev::ktest::test_pci_qemu_set),
    test("pci_bar_map", dev::ktest::test_pci_bar_map),
    test("pci_cfg_rw", dev::ktest::test_pci_cfg_rw),
    test("pci_claim_exclusive", dev::ktest::test_pci_claim_exclusive),
    test("pci_bind_order", dev::ktest::test_pci_bind_order),
    test("lspci_cmd", shell::ktest::test_lspci_cmd),
    test("irq_pool", irq::ktest::test_irq_pool),
    test("irq_free_threaded", irq::ktest::test_irq_free_threaded),
    test("msix_cpu", irq::ktest::test_msix_cpu),
    test("intx_fallback", irq::ktest::test_intx_fallback),
    test("intx_free_masks", irq::ktest::test_intx_free_masks),
    test("dma_alloc", dev::ktest::test_dma_alloc),
    test("dma_edu", dev::ktest::test_dma_edu),
    test("workqueue", sched::ktest::test_workqueue),
    test("virtio_bind", dev::ktest::test_virtio_bind),
    test("virtio_vq", dev::ktest::test_virtio_vq),
    test("dev_random_source", dev::ktest::test_dev_random_source),
    test(
        "dev_probe_alloc_fail",
        dev::ktest::test_dev_probe_alloc_fail,
    ),
    test("block_ramdisk_rw", block::ktest::test_block_ramdisk_rw),
    test("block_concurrent", block::ktest::test_block_concurrent),
    test("block_retry", block::ktest::test_block_retry),
    test("block_vblk_rw", drivers::ktest::test_block_vblk_rw),
    test("block_vblk_irq", drivers::ktest::test_block_vblk_irq),
    test("block_vblk_deep", drivers::ktest::test_block_vblk_deep),
    test(
        "block_vblk_concurrent",
        drivers::ktest::test_block_vblk_concurrent,
    ),
    test("block_vblk_mq", drivers::ktest::test_block_vblk_mq),
    test("block_persist", drivers::ktest::test_block_persist),
    test("block_part_mbr", block::ktest::test_block_part_mbr),
    test("block_part_gpt", block::ktest::test_block_part_gpt),
    test("block_cache_hit", block::ktest::test_block_cache_hit),
    test("block_cache_evict", block::ktest::test_block_cache_evict),
    test("vfs_walk", fs::ktest::test_vfs_walk),
    test("pseudo_fs", fs::ktest::test_pseudo_fs),
    test("fat_initrd", fs::ktest::test_fat_initrd),
    test("initrd_module_sized", fs::ktest::test_initrd_module_sized),
    test("vibefs", fs::ktest::test_vibefs),
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
        block::ktest::lifetime_iowaiter_publish_last,
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
    test(
        "file_table_fork_churn",
        fs::ktest::test_file_table_fork_churn,
    ),
    test(
        "file_table_stale_writeback_ebadf",
        fs::ktest::test_file_table_stale_writeback_ebadf,
    ),
    test(
        "open_creat_exists_opens",
        fs::ktest::test_open_creat_exists_opens,
    ),
    test(
        "inode_size_shared_across_opens",
        fs::ktest::test_inode_size_shared_across_opens,
    ),
    test(
        "fat_unlinked_open_frees_at_close",
        fs::ktest::test_fat_unlinked_open_frees_at_close,
    ),
    test("vibefs_efbig", fs::ktest::test_vibefs_efbig),
    test("vibefs_seek_end_5gib", fs::ktest::test_vibefs_seek_end_5gib),
    test("vfs_fat_ops_initrd", fs::ktest::test_vfs_fat_ops_initrd),
    test(
        "vfs_fat_unlinked_open_inode",
        fs::ktest::test_vfs_fat_unlinked_open_inode,
    ),
    test(
        "vfs_fat_file_api_one_inode",
        fs::ktest::test_vfs_fat_file_api_one_inode,
    ),
    test("vfs_vibe_ops_mem", fs::ktest::test_vfs_vibe_ops_mem),
    test("vfs_backends_via_ops", fs::ktest::test_vfs_backends_via_ops),
    test("vfs_fat_one_inode", fs::ktest::test_vfs_fat_one_inode),
    test(
        "fs_drop_slot_busy_keeps_slot",
        fs::ktest::test_fs_drop_slot_busy_keeps_slot,
    )
    .deadline(20_000),
    test(
        "cache_flush_waits_writeback",
        block::ktest::cache_flush_waits_writeback,
    ),
    test("block_fua_write", block::ktest::block_fua_write),
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
    test(
        "catch_ignores_other_cpu",
        arch::ktest::test_catch_ignores_other_cpu,
    ),
    test(
        "catch_ignores_user_frame",
        arch::ktest::test_catch_ignores_user_frame,
    )
    .deadline(30_000),
    test(
        "force_kernel_irq_window",
        arch::ktest::test_force_kernel_irq_window,
    )
    .deadline(30_000),
    test("arch_seam_core", arch::ktest::test_arch_seam_core),
    test(
        "uaccess_smap_stray_fault",
        arch::ktest::test_uaccess_smap_stray_fault,
    ),
    test(
        "uaccess_smep_user_jump",
        arch::ktest::test_uaccess_smep_user_jump,
    ),
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
    test(
        "lock_across_switch_asserts",
        sched::ktest::lock_across_switch_asserts,
    ),
    test(
        "block_in_hard_irq_asserts",
        sched::ktest::block_in_hard_irq_asserts,
    ),
    test(
        "in_hard_irq_top_bottom",
        sched::ktest::in_hard_irq_top_bottom,
    ),
    test(
        "sleep_under_spinlock_asserts",
        sched::ktest::sleep_under_spinlock_asserts,
    ),
    test("exec_huge_memsz", proc::ktest::test_exec_huge_memsz).deadline(60_000),
    test(
        "exec_large_elf_from_file",
        proc::ktest::test_exec_large_elf_from_file,
    )
    .deadline(60_000),
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
        "pid_not_reused_after_reap",
        proc::ktest::pid_not_reused_after_reap,
    ),
    test(
        "uaccess_syscall_copies",
        proc::ktest::test_uaccess_syscall_copies,
    )
    .deadline(30_000),
    test(
        "uaccess_readonly_efault",
        proc::ktest::test_uaccess_readonly_efault,
    )
    .deadline(30_000),
    test(
        "shootdown_ack_while_busy",
        irq::ktest::shootdown_ack_while_busy,
    ),
    test(
        "wake_inbox_and_kva_pool",
        irq::ktest::wake_inbox_and_kva_pool,
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
    // SAFETY: `ISA_DEBUG_EXIT` is the isa-debug-exit port the harness gives
    // every kernel_tests guest, which no other code uses; the write ends the
    // guest and touches no memory; established here.
    unsafe { x86::outl(ISA_DEBUG_EXIT, code) };
    x86::halt();
}

/// Pages per stack in [`quiesce_frames`]' KVA walk: 17 pages of VA a round,
/// so one walk between two coalesces covers `MAX_KVA_RANGES` times that.
const WARM_STACK_PAGES: usize = 16;
/// Ceiling on walk rounds: two coalesces take at most two free lists' worth.
const WARM_ROUNDS: usize = 3 * vibeos::limits::MAX_KVA_RANGES;
/// Default-size stacks the warm-up allocates and frees: past one free-list
/// coalesce, since the node pool is `MAX_KVA_RANGES`.
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

pub(crate) fn mmio_r32(va: u64, off: u32) -> u32 {
    // SAFETY: invariant: every caller passes a device's BAR 0 VA, which
    // `pci_init` mapped uncached, and a register offset inside that BAR;
    // established by `ktest::bar0_va`.
    unsafe { core::ptr::read_volatile((va.wrapping_add(off as u64)) as *const u32) }
}

pub(crate) fn mmio_w32(va: u64, off: u32, val: u32) {
    // SAFETY: invariant: as for `mmio_r32`; established by `ktest::bar0_va`.
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
    crate::dev::ktest::find_id(0x1234, 0x11e8)
        .or_else(|| crate::dev::ktest::find_id(0x1b36, 0x11e8))
}

/// The free-frame count at a quiescent point (ROADMAP §10.2, F074): the
/// shared warm-up has run (once per boot, from whichever caller comes
/// first), no thread but the caller and the idle threads is runnable, and
/// no dead thread's stack is still on its way back. Every frame-accounting
/// test takes its `before` and `after` from here.
pub(crate) fn quiescent_free_frames() -> usize {
    settle();
    free_frames()
}

/// Run the shared warm-up, then wait for [`quiesce`], saying so if it
/// timed out.
fn settle() {
    quiesce_frames();
    if !quiesce() {
        crate::marker!("vibeOS: ktest:   quiesce: threads did not settle");
    }
}

/// What `lifetime_stack_reclaim` accounts for, read at a quiescent point.
///
/// The kernel page tables are counted with the free frames. Each stack that
/// misses its CPU's stack cache is carved from KVA, and `Kva` hands out VA
/// first-fit over a list that frees append to and that is merged in address
/// order only when its node pool runs out; after each merge, the fresh VA
/// starts above the highest stack still live, and which stacks those are
/// (the ones the CPUs' stack caches hold, the ones still running or on a
/// dead list) depends on timing. So the highest VA the test maps moves by
/// several MiB from run to run, and each 2 MiB span it reaches for the first
/// time takes a table page, which stays in the kernel tables for good
/// (`mm::ktest::table_pages`) and is no frame lost. No warm-up can map the
/// span ahead, since nothing bounds it but the KVA window.
pub(crate) struct FrameCount {
    /// The buddy's free frames.
    buddy: usize,
    /// Frames of the stacks the stack caches hold.
    cached: usize,
    /// Page-table pages the kernel mapper has taken since boot.
    tables: usize,
    /// Mapped heap pages, for the failure line. They are not added back:
    /// heap growth in the test's window takes buddy frames and fails it.
    heap: usize,
}

impl FrameCount {
    pub(crate) fn quiescent() -> Self {
        settle();
        Self {
            buddy: pmm_init::with_buddy(|b| b.stats().free_frames),
            cached: thread_init::cached_stack_frames(),
            tables: crate::mm::ktest::table_pages(),
            heap: crate::heap_init::stats().capacity / vibeos::paging::PAGE_SIZE_4K as usize,
        }
    }

    /// The frames free ([`free_frames`]) or in kernel page tables.
    fn total(&self) -> usize {
        self.buddy + self.cached + self.tables
    }

    /// `Ok` when `after` accounts for as many frames as `self`; otherwise a
    /// failure line that names every count before and after.
    pub(crate) fn unchanged(&self, after: &Self, exits: usize) -> Outcome {
        if after.total() == self.total() {
            return Outcome::Ok;
        }
        crate::fail_fmt!(
            "frames {} -> {} after {exits} exits (buddy {}->{} cache {}->{} pt {}->{} heap {}->{})",
            self.total(),
            after.total(),
            self.buddy,
            after.buddy,
            self.cached,
            after.cached,
            self.tables,
            after.tables,
            self.heap,
            after.heap,
        )
    }
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
