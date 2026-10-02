//! In-guest tests for proc (kernel_tests only). Rows: [`TESTS`].
use crate::ktest::{Test, test};

mod counts;
mod entry;
mod exec;
mod exit_work;
mod floor;
mod hooks;
mod lifecycle;
mod limits;
mod open;
mod runtime;
mod segs;
mod space;
mod sysdecl;
mod uaccess;
mod waits;

pub(crate) use counts::*;
pub(crate) use entry::*;
pub(crate) use exec::*;
pub(crate) use exit_work::*;
pub(crate) use floor::*;
pub(crate) use hooks::*;
pub(crate) use lifecycle::*;
pub(crate) use limits::*;
pub(crate) use open::*;
pub(crate) use runtime::*;
pub(crate) use segs::*;
pub(crate) use space::*;
pub(crate) use sysdecl::*;
pub(crate) use uaccess::*;
pub(crate) use waits::*;

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[
    #[cfg(target_arch = "x86_64")]
    test(
        "addrspace_map_unmap_teardown",
        test_addrspace_map_unmap_teardown,
    ),
    test("user_ptr_helpers", test_user_ptr_helpers),
    test("cr3_switch_skip", test_cr3_switch_skip),
    test("ring3_syscall_enosys", test_ring3_syscall_enosys),
    test("ring3_hello_exit", test_ring3_hello_exit),
    test("syscall_dispatch", test_syscall_dispatch),
    test("syscall_ptr_validate", test_syscall_ptr_validate),
    test("user_syscalls", test_user_syscalls).deadline(15_000),
    test("init_reports_failed_tests", test_init_reports_failed_tests),
    test("user_code_exit", test_user_code_exit),
    test("user_image_elf", test_user_image_elf),
    test("user_code_layout", test_user_code_layout),
    test("orphan_freed_no_init", test_orphan_freed_no_init),
    test("fork_full_thread_table", fork_full_thread_table).deadline(60_000),
    test("limits_heap_backed", limits_heap_backed).deadline(60_000),
    test("teardown_live_root_asserts", teardown_live_root_asserts),
    test("as_pin_across_exit", as_pin_across_exit).deadline(30_000),
    test("fill_pt_hold_bounded", fill_pt_hold_bounded).deadline(60_000),
    test("console_read_exit", test_console_read_exit).deadline(30_000),
    test("user_entry_irq", test_user_entry_irq).deadline(120_000),
    test("user_selectors", test_user_selectors),
    test("user_ds_fork", test_user_ds_fork),
    test("user_ds_switch", test_user_ds_switch).deadline(30_000),
    test("exec_top_page_enoexec", test_exec_top_page_enoexec).deadline(30_000),
    test("noncanonical_rip_sigsegv", test_noncanonical_rip_sigsegv).deadline(30_000),
    test("exec_huge_memsz", test_exec_huge_memsz).deadline(60_000),
    test("exec_large_elf_from_file", test_exec_large_elf_from_file).deadline(60_000),
    test("exec_from_tmp", test_exec_from_tmp).deadline(30_000),
    test("elf_shared_page", test_elf_shared_page),
    test("elf_shared_page_jump", test_elf_shared_page_jump),
    test("brk_mmap_munmap_user", test_brk_mmap_munmap_user).deadline(30_000),
    test("stop_cont_no_lost_wakeup", test_stop_cont_no_lost_wakeup).deadline(30_000),
    test("wait4_ends_on_pending_kill", wait4_ends_on_pending_kill).deadline(30_000),
    test("signal_on_return", test_signal_on_return).deadline(30_000),
    test("exit_work_ipi", test_exit_work_ipi).deadline(30_000),
    test("syscall_body_if_on", test_syscall_body_if_on).deadline(30_000),
    test("kill_line_whole", test_kill_line_whole).deadline(30_000),
    test("kalloc_fail_after_hook", test_kalloc_fail_after_hook),
    test("kalloc_nomem", test_kalloc_nomem).deadline(120_000),
    test("syscall_rcx_canary", test_syscall_rcx_canary).deadline(30_000),
    test("fork_child_gprs", test_fork_child_gprs).deadline(30_000),
    test("preempt_gpr_canaries", test_preempt_gpr_canaries).deadline(60_000),
    test("user_single_step", test_user_single_step).deadline(30_000),
    test("user_int1", test_user_int1).deadline(30_000),
    test("user_tf_repin", test_user_tf_repin).deadline(60_000),
    test("user_fork_wait_stall", user_fork_wait_stall),
    test("pid_not_reused_after_reap", pid_not_reused_after_reap),
    test("uaccess_syscall_copies", test_uaccess_syscall_copies).deadline(30_000),
    test("uaccess_readonly_efault", test_uaccess_readonly_efault).deadline(30_000),
    test("user_runtime", user_runtime),
    test("syscall_ptr_decl_efault", syscall_ptr_decl_efault),
    test("read_ebadf_before_efault", read_ebadf_before_efault),
    test("wait4_echild_before_efault", wait4_echild_before_efault),
    test("syscall_errno_checks", syscall_errno_checks),
    test("floor_syscalls_from_user", floor_syscalls_from_user).deadline(60_000),
    test("reboot_bad_args_einval", reboot_bad_args_einval),
    test("reboot_power_off", reboot_power_off).opt_in(),
    test("reboot_restart", reboot_restart).opt_in(),
    test("user_heap_over_brk", user_heap_over_brk).deadline(60_000),
    test("open_trunc_enfile", open_trunc_enfile).deadline(60_000),
    test("proc_syscall_count", proc_syscall_count),
];
