# Linux contract

vibeOS implements Linux's interfaces ([ROADMAP](ROADMAP.md), How to read this). This file holds what
that rule leaves to a register: which Linux release "Linux's" means, each place vibeOS differs from it
on purpose, and each interface only vibeOS offers. A difference that is a bug has no row here:
[SYSCALL.md](SYSCALL.md) describes it with the kernel-review finding and the ROADMAP line that fixes
it. `scripts/check_linux_md.py`, which `make check` runs, checks that every row below has each field
and a unique id, and that each difference SYSCALL.md describes cites its fixing line or its row here.

## Baseline

| Field | Value |
|-------|-------|
| Series | not named yet |

## Deliberate differences

One row per difference. An expected-failure entry (ROADMAP §13.11, §23.6), a port patch (§24.2), or a
SYSCALL.md note cites a row by its id. The Linux column gives the baseline's behaviour.

| Id | Interface | Linux | vibeOS | Reason | Decided in |
|----|-----------|-------|--------|--------|------------|
| `execve-late-errno` | `execve` that fails after the old image is gone | the process is killed with `SIGSEGV` | every allocation is made before the new address space is swapped in, so a failure returns its errno to the old image | the caller gets an errno it can handle; the cost is holding both images' page tables until the swap | DESIGN §4.4 |
| `no-modules` | loadable modules: `init_module`, `finit_module`, `delete_module`, and `modprobe` over them | loads code into the running kernel | the three calls return `ENOSYS`, and `modprobe` fails | one image with its drivers in the tree, so signing and KASLR cover a single ELF | ROADMAP Non-goals |
| `no-32bit` | 32-bit user programs: ia32 and x32 on x86_64 (a 32-bit ELF, `int 0x80`, `sysenter`, a syscall number with bit 30 set) and AArch32 on aarch64 | runs them through its compat entry points where the kernel is configured with them | `execve` of a 32-bit ELF returns `ENOEXEC`, `int 0x80` from a 64-bit process gets `SIGSEGV`, and an x32 number returns `ENOSYS` | Linux compatibility covers 64-bit (LP64) binaries only | ROADMAP Non-goals |
| `no-vsyscall` | the legacy x86_64 vsyscall page at `0xffffffffff600000` | mapped execute-only by default (`vsyscall=xonly`), with a `[vsyscall]` line in `/proc/<pid>/maps` | not mapped, as on Linux booted with `vsyscall=none`: a call there gets `SIGSEGV`, and `maps` has no `[vsyscall]` line | only binaries linked against glibc before 2.14 call it, and it would put a fixed, user-reachable address in the kernel half | here |

## Native interfaces

One row per interface only vibeOS offers, with its format and reason (SYSCALL.md §8). A row lands in
the commit that adds the interface.

| Id | Interface | Format | Reason | Until |
|----|-----------|--------|--------|-------|
| `psinfo` | syscall 500 | one `<pid> <ppid> <state> <name>` line per process, whole lines only, at most 512 bytes (SYSCALL.md §3.1) | the `/bin/sh` `ps` built-in, before `procfs` exists | ROADMAP §13.9, which deletes it |
