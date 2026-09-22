# Syscall ABI

Linux-shaped x86_64 SysV. Not a Linux clone: numbers, errno **names**, and
the register convention match where they exist. The surface is tiny until
Slice C (`Process`, `fork`/`exec`/`wait`).

Index: [DESIGN.md](DESIGN.md) §1.4. Task list: [ROADMAP.md](ROADMAP.md) §9.3.
Entry/exit (`swapgs`, `sysretq`/`iretq`) is Slice A. This file is Slice B.

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
(Slice A).

---

## 2. Return and errno

Success: non-negative `rax` (byte count, pid, `0`, …).

Error: `rax = -errno`. Linux names, Linux values:

| Name | Value | Used in B |
|------|------:|-----------|
| `EPERM` | 1 | reserved |
| `ENOENT` | 2 | reserved |
| `EBADF` | 9 | `write` on an fd that is not the early sink |
| `ENOMEM` | 12 | reserved |
| `EFAULT` | 14 | bad user pointer / length |
| `EINVAL` | 22 | reserved |
| `ENOSYS` | 38 | unknown number, or a number not wired yet |

Unknown numbers return `-ENOSYS`. No table growth in A; B owns the table.

---

## 3. Dispatch table

Indexed by number. Each live entry has a name, arity, and a pointer
policy (which args are user pointers, which arg is the length). Slice A’s
stub (`vibeos_syscall_stub`) is the same symbol; it now looks up the table
instead of always returning `ENOSYS`. There is no second `syscall` entry.

Wired in B (proof set):

| nr | name | arity | pointers |
|---:|------|------:|----------|
| 1 | `write` | 3 | `rsi` buffer, `rdx` length |
| 24 | `sched_yield` | 0 | |
| 39 | `getpid` | 0 | |
| 60 | `exit` | 1 | status in `rdi` (low 8 bits) |

`read`/`open`/`close`/`fork`/`execve`/`wait4`/… stay `-ENOSYS` until C.

`sched_yield` calls the kernel `yield_now` only while a ring-3 task is
in `run_user`. A kernel-side `dispatch()` probe (ktest, IF off) returns
`0` without scheduling.

---

## 4. Early fd1 → console (bootstrap, not a Process)

There is **no** `Process` and **no** fd table in B. `write` does not look
up a descriptor.

Until C replaces this with a real table + `dup`/`CLOEXEC`:

- fd **1** and fd **2** are a fixed sink to the kernel console mux
  (serial + framebuffer). Freestanding `write(1, buf, n)` reaches the
  console. That is the Phase 9 exit-gate bullet, proven here.
- any other fd → `-EBADF`
- this path is **temporary**. C owns the descriptor table and must
  delete the special case, not grow it.

No half-Process, no fake `files[MAX_FD]`. One `if fd == 1 \|\| fd == 2`.

`getpid` returns bootstrap pid **1** for the same reason: no pid allocator
yet. C’s `Process` replaces it.

---

## 5. User pointers

Every pointer arg is checked against the caller’s `AddressSpace` **before**
use: canonical, user half, not the VA-0 guard, not overflowing `len`,
every leaf present and `USER`. Failure is `-EFAULT`. The kernel does not
panic on user misuse (DESIGN §2.5 vs Phase 9).

**Partial copy:** all-or-nothing. If any byte of `[ptr, ptr+len)` fails,
no bytes are copied and the syscall returns `-EFAULT`. `len == 0` is
success (`write` returns 0) and does not touch the pointer.

Copy goes through the page tables + HHDM (`AddressSpace::read_bytes` /
`write_bytes`), not a raw user-VA load that could `#PF` into DESIGN §5.2
halt. A `#PF` in a future copy path still has to become `EFAULT` (C;
DESIGN §5.2 still says halt for kernel faults).

---

## 6. Tracing and counters

Tracing is off by default. `syscall::set_trace(true)` (or the kernel
wrapper) logs `user: syscall <name> … = <ret>` on serial. Not a `vibeOS:`
boot marker.

Each TCB has `syscall_count`, incremented on every entry (including
`ENOSYS`). That is the procfs hook until a `Process` exists; C can
aggregate per-pid.

---

## 7. First userspace

Static ELF64, no libc, hand-written `syscall` stubs. Loaded from the
initrd (`/hello`). Stack: `argc` / `argv` / `envp` / `auxv` (`AT_PAGESZ`,
`AT_ENTRY`, `AT_RANDOM`, ids, `AT_NULL`). `PT_INTERP` is refused, not
ignored. Exit status is the kernel-reported low 8 bits (`user: exit N`
diagnostic).
