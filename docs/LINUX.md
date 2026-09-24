# Linux contract

vibeOS implements Linux's interfaces ([ROADMAP](ROADMAP.md), How to read this). This file holds what
that rule leaves to a register: which Linux release "Linux's" means, each place vibeOS differs from it
on purpose, and each interface only vibeOS offers. A difference that is a bug has no row here:
[SYSCALL.md](SYSCALL.md) describes it with the kernel-review finding and the ROADMAP line that fixes
it. `scripts/check_linux_md.py`, which `make check` runs, checks that every row below has each field
and a unique id, and that each difference SYSCALL.md describes cites its fixing line or its row here.

## Baseline

The baseline is one longterm series on kernel.org, and in it one point release per architecture, the
oracle kernel: built from kernel.org's tarball with the configuration in `tests/linux/`, and booted by
every comparison with Linux ([ROADMAP](ROADMAP.md) How to read this, §12.4). Moving the baseline to a
later series, or the oracle to a later point release, is an edit here that lists what changed.

| Field | Value |
|-------|-------|
| Series | not named yet (ROADMAP §12.4) |
| x86_64 oracle | not built yet: release and tarball SHA-256 |
| aarch64 oracle | not built yet: release and tarball SHA-256 |
| Configuration | `tests/linux/`, not written yet |
| Oracle key | not built yet |
| Assets | the `linux-<key>` pre-release, not published yet |

## Deliberate differences

One row per difference. An expected-failure entry (ROADMAP §13.11, §23.6), a port patch (§24.2), or a
SYSCALL.md note cites a row by its id. The Linux column gives the baseline's behaviour. The Oracle
column names the configuration lines that turn the difference off in the oracle kernel (ROADMAP §12.4),
so the oracle behaves as vibeOS does and no suite case reaches it; `—` means Linux has no such option,
and each case that reaches the difference is on an expected-failure list citing the row (ROADMAP §23.6).

| Id | Interface | Linux | vibeOS | Reason | Decided in | Oracle |
|----|-----------|-------|--------|--------|------------|--------|
| `execve-late-errno` | `execve` that fails after the old image is gone | the process is killed with `SIGSEGV` | every allocation is made before the new address space is swapped in, so a failure returns its errno to the old image | the caller gets an errno it can handle; the cost is holding both images' page tables until the swap | DESIGN §4.4 | — |
| `no-modules` | loadable modules: `init_module`, `finit_module`, `delete_module`, and `modprobe` over them | loads code into the running kernel | the three calls return `ENOSYS`, and `modprobe` fails | one image with its drivers in the tree, so signing and KASLR cover a single ELF | ROADMAP Non-goals | `CONFIG_MODULES=n` |
| `no-32bit` | 32-bit user programs: ia32 and x32 on x86_64 (a 32-bit ELF, `int 0x80`, `sysenter`, a syscall number with bit 30 set) and AArch32 on aarch64 | runs them through its compat entry points where the kernel is configured with them | `execve` of a 32-bit ELF returns `ENOEXEC`, `int 0x80` from a 64-bit process gets `SIGSEGV`, and an x32 number returns `ENOSYS` | Linux compatibility covers 64-bit (LP64) binaries only | ROADMAP Non-goals | `CONFIG_IA32_EMULATION=n` and `CONFIG_X86_X32_ABI=n` on x86_64, `CONFIG_COMPAT=n` on arm64 |
| `no-vsyscall` | the legacy x86_64 vsyscall page at `0xffffffffff600000` | mapped execute-only by default (`vsyscall=xonly`), with a `[vsyscall]` line in `/proc/<pid>/maps` | not mapped, as on Linux booted with `vsyscall=none`: a call there gets `SIGSEGV`, and `maps` has no `[vsyscall]` line | only binaries linked against glibc before 2.14 call it, and it would put a fixed, user-reachable address in the kernel half | here | `CONFIG_LEGACY_VSYSCALL_NONE=y` |
| `no-compat-cs` | the x86_64 compat code selector `0x23` (`__USER32_CS`): a far transfer, `iretq`, or `rt_sigreturn` to it | runs 32-bit code; the descriptor is in every CPU's GDT whatever the kernel's configuration | `SIGSEGV`: GDT slot `0x20` is null | 32-bit user code is a non-goal | ROADMAP Non-goals; DESIGN §5.1 | — |
| `no-modify-ldt` | `modify_ldt` | installs local descriptors, such as 16- and 32-bit code segments | returns `ENOSYS` | its users run 16- and 32-bit code, a non-goal, and an LDT is per-process descriptor state every switch would carry | ROADMAP Non-goals | `CONFIG_MODIFY_LDT_SYSCALL=n` |

## Native interfaces

One row per interface only vibeOS offers, with its format and reason. Each lives under a vibeOS name
(SYSCALL.md §8): `/proc/vibeos/`, `/proc/<pid>/vibeos/`, `/sys/kernel/vibeos/`, a `vibeos/` directory
inside a sysfs or debugfs directory Linux owns, a generic-netlink family `vibeos_<name>`, a node under
`/dev/vibeos/`, or a `vibeos.<name>=` command-line option; `psinfo` is the one exception, until
ROADMAP §13.9. A row lands in the commit that adds the interface.

| Id | Interface | Format | Reason | Until |
|----|-----------|--------|--------|-------|
| `psinfo` | syscall 500 | one `<pid> <ppid> <state> <name>` line per process, whole lines only, at most 512 bytes (SYSCALL.md §3.1) | the `/bin/sh` `ps` built-in, before `procfs` exists | ROADMAP §13.9, which deletes it |
