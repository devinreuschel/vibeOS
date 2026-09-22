# Syscall ABI

Linux-shaped x86_64 SysV. Not a Linux clone: numbers, errno **names**, and
the register convention match where they exist. Slice C owns `Process`,
`fork`/`exec`/`wait`, and the real fd table.

Index: [DESIGN.md](DESIGN.md) §1.4. Task list: [ROADMAP.md](ROADMAP.md) §9.3–§9.8.
Entry/exit (`swapgs`, `sysretq`/`iretq`) is Slice A. Dispatch table is B.
Process/fd/fork/exec/wait is C.

---

## 1. Registers

`syscall` / `sysretq`. One entry: `vibeos_syscall_entry` (DESIGN §7.5).

| Role | Register | Notes |
|------|----------|--------|
| number | `rax` | Linux x86_64 numbers |
| arg 0 | `rdi` | |
| arg 1 | `rsi` | |
| arg 2 | `rdx` | |
| arg 3 | `r10` | not `rcx`: `syscall` saves RIP in `rcx` |
| arg 4 | `r8` | |
| arg 5 | `r9` | |
| return | `rax` | see §2 |
| clobber | `rcx`, `r11` | RIP and RFLAGS |

`FMASK` clears `TF|IF|DF|IOPL|NT|AC` on entry. Fast path is `sysretq`.
`iretq` when the saved RIP is non-canonical or `RF|VM` is set in RFLAGS
(Slice A), and for a spawned process's first entry (`enter_user_full`).

---

## 2. Return and errno

Success: non-negative `rax` (byte count, pid, `0`, …).

Error: `rax = -errno`. Linux names, Linux values:

| Name | Value | Used |
|------|------:|------|
| `EPERM` | 1 | reserved |
| `ENOENT` | 2 | `open`/`execve` missing path |
| `ESRCH` | 3 | `kill` unknown pid |
| `EIO` | 5 | VFS I/O |
| `E2BIG` | 7 | `execve` argv too long |
| `ENOEXEC` | 8 | malformed ELF |
| `EBADF` | 9 | closed / out-of-range fd |
| `ECHILD` | 10 | `wait4` with no matching child |
| `EAGAIN` | 11 | process table full (`fork`) |
| `ENOMEM` | 12 | AS clone / load |
| `EACCES` | 13 | reserved |
| `EFAULT` | 14 | bad user pointer / length |
| `EBUSY` | 16 | VFS busy |
| `EEXIST` | 17 | `O_EXCL` |
| `ENOTDIR` | 20 | |
| `EISDIR` | 21 | |
| `EINVAL` | 22 | |
| `EMFILE` | 24 | per-process fd table full |
| `ENAMETOOLONG` | 36 | path |
| `ENOSYS` | 38 | unknown number |

Unknown numbers return `-ENOSYS`.

---

## 3. Dispatch table

Indexed by number. Each live entry has a name, arity, and a pointer
policy. Slice A's stub (`vibeos_syscall_stub`) is the same symbol. There
is no second `syscall` entry.

| nr | name | arity | pointers |
|---:|------|------:|----------|
| 0 | `read` | 3 | `rsi` buffer, `rdx` length |
| 1 | `write` | 3 | `rsi` buffer, `rdx` length |
| 2 | `open` | 3 | path is a C string (copied, not table-masked) |
| 3 | `close` | 1 | |
| 8 | `lseek` | 3 | |
| 24 | `sched_yield` | 0 | |
| 32 | `dup` | 1 | CLOEXEC cleared on the new fd |
| 33 | `dup2` | 2 | |
| 39 | `getpid` | 0 | `0` if the caller is not a process |
| 57 | `fork` | 0 | full AS copy; child `rax=0` |
| 59 | `execve` | 3 | path / argv / envp C strings |
| 60 | `exit` | 1 | status in `rdi` (low 8 bits) |
| 61 | `wait4` | 4 | optional `rsi` status (4 bytes) |
| 62 | `kill` | 2 | default actions only |
| 72 | `fcntl` | 3 | `F_GETFD` / `F_SETFD` (CLOEXEC) |
| 110 | `getppid` | 0 | |
| 500 | `psinfo` | 2 | vibeOS-specific until procfs; `rdi` buf, `rsi` len |

`sched_yield` calls the kernel `yield_now` when the caller has a pid
(bound `run_user` or a spawned process). A kernel-side `dispatch()`
probe with no process (ktest, IF off) returns `0` without scheduling.

---

## 4. File descriptors

Slice B's early fd1/fd2→console special case is **gone**. Each `Process`
has an fd table (`MAX_FDS=16`) with stdio 0/1/2 as the console mux
(serial + framebuffer). `write(1, …)` still reaches the console.

- `dup` / `dup2` copy the slot and clear `FD_CLOEXEC` on the new fd
- `open` `O_CLOEXEC` becomes per-fd `FD_CLOEXEC`; `execve` drops those
- kernel `OpenFile` slots are refcounted so `dup`/`fork` share them
- a kernel-side `dispatch()` probe with no process still sees `getpid=0`
  and `EBADF` for a closed fd; `with_user_as` binds a temporary process
  so pointer-validation tests use the real table

---

## 5. User pointers

Every pointer arg is checked against the caller's `AddressSpace` **before**
use: canonical, user half, not the VA-0 guard, not overflowing `len`,
every leaf present and `USER`. Failure is `-EFAULT`. The kernel does not
panic on user misuse (DESIGN §2.5 vs Phase 9).

**Partial copy:** all-or-nothing for `write`/`read` validation. If any
byte of `[ptr, ptr+len)` fails the range check, no bytes are copied and
the syscall returns `-EFAULT`. `len == 0` is success (`write` returns 0)
and does not touch the pointer.

Copy goes through the page tables + HHDM (`AddressSpace::read_bytes` /
`write_bytes`), not a raw user-VA load that could `#PF` into a kernel
halt. A user `#PF`/`#GP`/`#UD` from the program itself is a process kill
(DESIGN §5.2 CPL split), not `EFAULT`.

---

## 6. Tracing and counters

Tracing is off by default. `syscall::set_trace(true)` logs
`user: syscall <name> … = <ret>` on serial. Not a `vibeOS:` boot marker.

Each TCB has `syscall_count`, incremented on every entry (including
`ENOSYS`).

---

## 7. First userspace

Static ELF64, no libc, hand-written `syscall` stubs. Initrd:

- `/hello` — write + exit 42 (Slice B proof)
- `/sbin/init` — post-init kernel job: `fork`/`exec` tests then `/bin/sh`, then reap
- `/bin/tests` — syscall / `EFAULT` / `fork`+`exec`+`wait` / fault-kill runner
- `/bin/sh` — interactive shell; prints `vibeOS: shell ready` then `vibeos>`

Stack: `argc` / `argv` / `envp` / `auxv` (`AT_PAGESZ`, `AT_ENTRY`,
`AT_RANDOM` from TSC, ids, `AT_NULL`). `PT_INTERP` is refused.
`PT_PHDR` VAs are `check_user_va`'d. Exit status is the kernel-reported
low 8 bits (`user: exit N` diagnostic for the bootstrap hello).

The kernel REPL is debug-only (`--features kernel_shell`). Production
starts `/sbin/init` and parks.

`fork` is a **full AS copy**. COW is Phase 10. `execve` builds the new
AS first and replaces only after a successful load.
