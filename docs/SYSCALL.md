# Syscall ABI

x86_64 SysV, with Linux's syscall numbers, errno values, and register
convention. The ROADMAP rule "Linux interfaces" makes a call with a Linux
number implement Linux's interface, as of the baseline Linux release that
`docs/LINUX.md` names (ROADMAP §13.11). This file describes the code as built.
Where the code differs from Linux or from a rule stated here, the section
says how. A note such as (F083; ROADMAP §10.4) names a finding in the
kernel review ([reviews/KERNEL_REVIEW.md](reviews/KERNEL_REVIEW.md)) and the
ROADMAP section that fixes it. The fixing ROADMAP line cites the same id, so
for a note with only an id, search ROADMAP.md for the id.

Index: [DESIGN.md](DESIGN.md) §1.4. Landed: [ROADMAP.md](ROADMAP.md) §9.3–§9.8,
except the boxes the kernel review reopened there. Open: ROADMAP §10.4
(errno table, file tables, VFS dispatch), §10.5 (generated syscall table),
§10.6 (entry paths and user memory), §10.7 (tracing), §10.10 (kernel stack
reclaim and IPI acks), §10.11 (vibefs file size), §12.3 (copy-on-write
`fork`), §13.1 (shared open files, `FS_BASE`), §13.7 (process-group `kill`
and `wait4`), §13.8 (real-time signals), §13.9 (POSIX floor), §13.10
(`AT_RANDOM`).

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

The entry saves the x87 and SSE state (`fxsave64`) and the exit restores it
(`fxrstor64`), so a syscall preserves it. Two calls differ from Linux
(F069; ROADMAP §10.6): a `fork` child starts from the boot FXSAVE
template instead of the parent's x87, XMM, and MXCSR state, and `execve`
hands the new image the old image's x87 and XMM registers, MXCSR, and FCW.
The rule the fix implements: `fork` copies the caller's FP state, and
`execve` loads FCW `0x037F` and MXCSR `0x1F80` with the x87 and XMM
registers zeroed.

`FMASK` (`0x47700`) clears `TF`, `IF`, `DF`, `IOPL`, `NT`, and `AC` on
entry. The fast path is `sysretq`. The exit takes `iretq` when the saved
RIP is non-canonical or `RF` or `VM` is set in RFLAGS; a spawned or forked process's
first entry also uses `iretq` (`enter_user_full`).

The rule ROADMAP §10.6 implements (DESIGN §5.10): every entry from ring 3
saves a complete user frame, in the order of Linux's `user_regs_struct`, and
the syscall exit uses `sysretq` only when the saved RIP equals the saved
RCX, the saved RFLAGS equals the saved R11, CS and SS are the user
selectors, RIP is below `USER_MAP_END`, and RF, TF, and VM are clear;
otherwise it restores every register from the frame and uses `iretq`, as
Linux does. A restartable syscall resumes by reloading `rax` from `orig_rax`
and moving RIP back 2 bytes. Today the frame keeps RIP and RFLAGS only in
the RCX and R11 slots, so a context whose RCX and R11 differ from them
cannot be returned to.

The body runs with IF=1 (DESIGN §2.9 rule 3): the entry stub moves the user
RSP out of the per-CPU scratch into its frame and then runs `sti`. Today it
does not, and FMASK's IF=0 lasts until the body blocks (ROADMAP §10.6).

From the return of `vibeos_syscall_stub` to `sysretq` or `iretq`, the exit
path needs IF=0: it stores the return value in `gs:[retval]`, stages the
`iretq` frame in `gs:[iret_*]`, and loads the user RSP before `swapgs`.
ROADMAP §10.6 moves the return value and the `iretq` frame into the thread's
user frame, which leaves the per-CPU scratch holding only the user RSP
between `syscall` and the stack switch. A console `read` breaks this: it
returns through `console_init::wait_key`, which leaves IF=1, and nothing
clears IF before the exit (F001; ROADMAP §10.6).

A non-canonical saved RIP reaches `iretq`, which raises `#GP` at CPL 0
after `swapgs` has loaded the user GS base; on KVM and hardware the kernel
then hangs or triple-faults (TCG skips the canonical check). A `syscall` in
the last two bytes of a mapping that ends at `USER_END` produces that RIP
(F007; ROADMAP §10.6 keeps the top user page unmapped and sends a
non-canonical RIP to `SIGSEGV`).

---

## 2. Return and errno

Success: non-negative `rax` (byte count, pid, `0`, …).

Error: `rax = -errno`. Linux names, Linux values:

| Name | Value | Used |
|------|------:|------|
| `EPERM` | 1 | defined; no syscall returns it |
| `ENOENT` | 2 | `open`/`execve` missing path |
| `ESRCH` | 3 | `kill`: no such process, a zombie, `pid` 0, or a negative 32-bit `pid` (§3.1) |
| `EIO` | 5 | device I/O error; a FAT or vibefs volume still busy after 1,000,000 yields |
| `E2BIG` | 7 | `execve` argv with 16 or more entries |
| `ENOEXEC` | 8 | malformed ELF, `ET_DYN`, or `PT_INTERP` |
| `EBADF` | 9 | closed / out-of-range fd |
| `ECHILD` | 10 | `wait4` with no matching child |
| `EAGAIN` | 11 | `fork` with pids 2 to 15 all in use, zombies included (`MAX_PROCS` is 16; pid 0 is unused and pid 1 is reserved for `/sbin/init`) |
| `ENOMEM` | 12 | AS clone / load; an ELF file above 64 KiB |
| `EACCES` | 13 | defined; no syscall returns it |
| `EFAULT` | 14 | bad user pointer / length |
| `EBUSY` | 16 | defined; no syscall returns it |
| `EEXIST` | 17 | `O_EXCL` |
| `ENOTDIR` | 20 | |
| `EISDIR` | 21 | |
| `EINVAL` | 22 | `lseek` with a bad `whence` or a resulting offset below 0, unknown `fcntl` command, `kill` signal 0 or above 31; the non-Linux cases in §2.1 |
| `EMFILE` | 24 | per-process fd table full (`open`); the non-Linux cases in §2.1 |
| `ENAMETOOLONG` | 36 | path of 256 bytes or more; name above 64 bytes; an `execve` argv string of 256 bytes or more, which Linux accepts. ROADMAP §13.9 moves the path and name limits to Linux's 4096 and 255 |
| `ENOSYS` | 38 | unknown number |

Unknown numbers return `-ENOSYS`.

### 2.1 Differences from Linux

The mapping is `proc_init::fs_errno` plus the FAT and vibefs error
conversions (`FatError::to_fs`, `vibefs::Error::to_fs`). ROADMAP §10.4 (E2;
F083) replaces them with one `KError` table that generates §2.

- `FsError::NoSpace` maps to `EMFILE`, so `EMFILE` also means a full
  system-wide open-file table (Linux `ENFILE`, 23), a volume out of blocks,
  inodes, or directory entries, or a vibefs file that needs a fifth extent
  (Linux `ENOSPC`, 28), and a FAT file that would pass 4 GiB (Linux `EFBIG`, 27)
  (F083, F052, F057; ROADMAP §10.4)
- FAT and vibefs map `Corrupt` to `FsError::Inval`, so a failed checksum or
  bad magic returns `EINVAL` (Linux `EIO`) (F083; ROADMAP §10.4)
- a FAT or vibefs operation that waits 1,000,000 yields for its busy volume
  fails with `EIO`; Linux waits (F060; ROADMAP §10.4)
- `lseek` on the console returns `EINVAL` (Linux `ESPIPE`, 29) (F083;
  ROADMAP §10.4)
- `read` on an `O_WRONLY` fd and `write` on an `O_RDONLY` fd return `EINVAL`
  (Linux `EBADF`) (ROADMAP §10.4)
- `open` and `execve` return `EINVAL` for a path or argument that is not
  UTF-8; Linux hands a path's bytes to the filesystem and accepts any
  argument byte but NUL (ROADMAP §10.4)
- `dup` with a full fd table returns `EBADF` (Linux `EMFILE`) (ROADMAP §10.4)
- `ENFILE`, `EFBIG`, `ENOSPC`, `ESPIPE`, `ENOTEMPTY`, and `ELOOP` are not
  defined in `src/syscall.rs` (F083; ROADMAP §10.4)
- `fork` near memory exhaustion panics instead of returning `ENOMEM`:
  `thread_init::spawn_inner` calls `expect` on its stack allocation. More
  than 8 exits in a row, each switching to a thread resumed from timer
  preemption, overflow the 8-entry deferred-stack list, and `defer_free`
  panics on a full list (F010; ROADMAP §10.10)

---

## 3. Syscalls

`proc_init::dispatch_frame` dispatches with a `match` on `rax`, and each
handler checks its own pointers. `src/syscall.rs` holds a `SyscallInfo` row
per call (name, arity, pointer mask, length argument), but the kernel reads
only the name, for the §6 trace line, and only host tests run
`syscall::validate_args`. The rows have drifted: `OPEN`, `EXECVE`, and
`WAIT4` have `ptr_mask` 0 although each takes user pointers (F150; ROADMAP
§10.5 dispatches through the generated syscall table).

| nr | name | arity | pointers |
|---:|------|------:|----------|
| 0 | `read` | 3 | `rsi` buffer, `rdx` length |
| 1 | `write` | 3 | `rsi` buffer, `rdx` length |
| 2 | `open` | 3 | `rdi` path, a C string of at most 255 bytes |
| 3 | `close` | 1 | |
| 8 | `lseek` | 3 | |
| 24 | `sched_yield` | 0 | |
| 32 | `dup` | 1 | CLOEXEC cleared on the new fd |
| 33 | `dup2` | 2 | |
| 39 | `getpid` | 0 | `0` if the caller is not a process |
| 57 | `fork` | 0 | full AS copy; child `rax=0` |
| 59 | `execve` | 3 | `rdi` path; `rsi` argv, at most 15 C strings of at most 255 bytes each; `rdx` envp, not read |
| 60 | `exit` | 1 | status in `rdi` (low 8 bits) |
| 61 | `wait4` | 4 | optional `rsi` status (4 bytes); `r10` rusage not read |
| 62 | `kill` | 2 | default actions only |
| 72 | `fcntl` | 3 | `F_GETFD` / `F_SETFD` (CLOEXEC) |
| 110 | `getppid` | 0 | |
| 500 | `psinfo` | 2 | `rdi` buf, `rsi` len; vibeOS-specific |

`sched_yield` calls the kernel `yield_now` when the caller has a pid
(bound `run_user` or a spawned process). A kernel-side `dispatch()`
probe with no process (ktest, IF off) returns `0` without scheduling.

### 3.1 Behavior and differences from Linux

- `read`: a file read copies through a 256-byte kernel buffer and returns
  at most 256 bytes per call (a short read, which POSIX allows); a console read
  returns after a newline or `rdx` bytes
- `open`: `mode` is ignored, and a vibefs file is created 0644 (F149;
  ROADMAP §13.9). Unknown flag bits are ignored, as in Linux. `O_CREAT` and
  `O_TRUNC` take effect before the open-file slot and the fd are allocated, so with
  a full open-file or fd table `open(O_TRUNC)` truncates the file and then
  fails with `EMFILE` (F057;
  ROADMAP §10.4)
- `lseek`: any offset from 0 to `i64::MAX` is accepted, and `SEEK_END`
  uses the 32-bit `OpenFile.size`. A vibefs `write` just below file offset
  2^44 makes the next access to that block panic the kernel in the default
  dev profile, and a write at 2^44 or above overwrites the file's low
  blocks (F008; ROADMAP §10.11)
- `execve`: the image is read whole and must be at most 64 KiB (`ENOMEM`)
  until ROADMAP §10.4 removes `MAX_ELF`. `p_memsz` is bounded only by
  `USER_END`, so a small ELF can map pages until physical memory runs out,
  with IF=0 and the page-table lock held (F009; ROADMAP §10.6). An empty
  argv becomes `[path]`. `envp` is not read, and the new stack gets an
  empty environment (§7; ROADMAP §9.4 defers the copy to §10.5)
- `wait4`: `pid > 0` waits for that child, any `pid < 0` for any child, and
  `pid == 0` returns `ECHILD`; Linux reads 0 and `pid < -1` as process
  groups. Only `WNOHANG` is read; other option bits are accepted and ignored, and `r10`
  (`rusage`) never reaches the handler (F149; ROADMAP §13.7)
- `kill`: signal 0 returns `EINVAL`, where Linux checks existence and
  permission (F149; ROADMAP §13.7). Signals 32 to 64 return `EINVAL` until
  ROADMAP §13.8 adds real-time signals. `pid` is truncated to 32 bits, and
  `pid` 0 and every negative 32-bit `pid` return `ESRCH`, where Linux
  signals, for 0, the caller's process group, for -1, every process the
  caller may signal except pid 1 and the caller, and for any other negative
  `pid`, process group `-pid` (F149; ROADMAP §13.7). A `pid` that names a
  zombie returns `ESRCH`; Linux returns 0. A default-terminate or
  default-stop signal to pid 1 kills or stops init,
  after which orphans stay zombies; Linux delivers to init only the signals
  it handles (F068; ROADMAP §10.5)
- `psinfo`: writes one `<pid> <ppid> <state> <name>` line per process
  (state `run`, `stop`, or `zombie`). It formats the whole lines that fit in
  512 bytes and copies at most `rsi` of those bytes. Number 500 is in the
  range Linux allocates next (F149); ROADMAP §13.9 deletes the call when
  `ps` moves to `procfs`
- `read`, the `wait4` status, and `psinfo` write user memory without
  checking the page's W bit (§5)

---

## 4. File descriptors

Each process (`proc_init::Proc`) has a 16-slot fd table (`proc::MAX_FDS`). Fds 0, 1, and 2
are the console mux (serial and framebuffer).

A file fd names a slot in one system-wide open-file table,
`file_init::FILES` (16 entries), shared by every process with no
per-process quota. The slot holds the offset, size, and open flags, so fds
that `dup` or `fork` copied share one offset. While the table is full, every
`open` and every `execve` (which needs a slot to read the image) fails with
`EMFILE` in every process (F057; ROADMAP §10.4).

- `dup` / `dup2` copy the slot and clear `FD_CLOEXEC` on the new fd
- `open` `O_CLOEXEC` becomes per-fd `FD_CLOEXEC`; `execve` drops those
- open-file slots are refcounted so `dup`/`fork` share them
- a relative path resolves against one kernel-global cwd,
  `file_init::CWD`, which only the debug shell's `cd` changes;
  `Proc::cwd` is copied on `fork` and never read, and there is no `chdir`
  (F057, F086; ROADMAP §10.4, with `chdir` in §13.9)
- a path goes to FAT or vibefs by byte-prefix match on the path as passed,
  never through the VFS. To a process, `/dev`, `/proc`, `/tmp`, and `/sys`
  are FAT directories: `open("/dev/null")` returns `ENOENT`, and with
  `O_CREAT` creates a FAT file. `/./vibe/f`, `//vibe/f`, and `/VIBE/f`
  reach the FAT `vibe` directory that the vibefs mount hides, and
  `/vibe/./f` returns `EINVAL` (F056, F086; ROADMAP §10.4)
- `read`, `write`, and `lseek` copy the slot out, drop the table lock for
  the I/O, and write the whole slot back, `refs` included, so two calls on
  one open file that overlap lose one call's update, and a stale `refs` can
  free a slot another descriptor still holds. They cannot overlap while
  syscall bodies run with IF=0, every user thread runs on one CPU, and no
  file syscall blocks. ROADMAP §10.4 keeps `refs` out of the write-back
  before §10.6 makes syscall bodies preemptible; after that a process and
  its `fork` child can overlap on one inherited descriptor and lose an
  offset update until §13.1's position lock (F055)
- a kernel-side `dispatch()` probe with no process still sees `getpid=0`
  and `EBADF` for a closed fd; `with_user_as` binds a temporary process
  so pointer-validation tests use a process fd table

---

## 5. User pointers

Current behavior; ROADMAP §10.6 replaces the page-table pre-walk and the
physmap copy with user-VA accessors and an exception-table fixup.

Before any copy, `AddressSpace::check_user_range` checks the whole range of
a pointer argument against the caller's address space: canonical, below
`USER_END`, not in the first page (`NULL_GUARD_LEN`), `ptr + len` does not
overflow, and every leaf present with `USER` set. Failure is `-EFAULT`.

The check does not test `WRITABLE`. `read`, the `wait4` status, and
`psinfo` copy out with `AddressSpace::write_bytes`, which writes through the
kernel's physmap alias, so the user PTE's W bit and `CR0.WP` do not apply:
a destination in the caller's read-only or executable pages succeeds where
Linux returns `EFAULT` (F023; ROADMAP §10.6).

**Partial copy:** for `read`, `write`, `psinfo`, and the `wait4` status,
the range check covers `[ptr, ptr+len)` before the first byte moves, so a
failure copies nothing and returns `-EFAULT`. `len == 0` is success
(`write` returns 0) and does not touch the pointer. `open` and `execve`
check and copy each C string into a kernel buffer one byte at a time, and
each 8-byte argv pointer in one piece.

Copies go through the page tables and the physmap
(`AddressSpace::read_bytes` / `write_bytes`) rather than a user-VA access,
because there is no fault fixup yet. A user `#PF`, `#GP`, or `#UD` from the
program itself is a process kill (DESIGN §5.2 CPL split), not `EFAULT`.

**Kernel survival:** no user program may panic or halt the kernel (DESIGN
§2.5 for ring-3 exceptions, AGENTS.md rule 4 for syscall paths). The code
does not meet this yet:

- ring-3 `#DB` (from `RFLAGS.TF` or `INT1`) halts every CPU, and `#AC` has
  no signal mapping either (F005; ROADMAP §10.6)
- a device or keyboard interrupt taken in ring 3 runs with the user GS
  base and halts (F004; ROADMAP §10.6)
- the exit-path faults in §1 (F001, F007; ROADMAP §10.6)
- a forked or spawned process's first ring-3 entry, `enter_user_full`,
  runs with IF=1, so an interrupt between its `mov gs` and its `iretq`
  reads `gs:[0]` at VA 0 at CPL 0 and halts (F006; ROADMAP §10.6)
- a console `write` keeps IF=0 for its whole length and acks no IPI, so a
  TLB shootdown that another CPU sends during a write longer than about
  1 s panics in `wait_acks` (F011; ROADMAP §10.10)
- a vibefs `write` just below file offset 2^44 (§3.1; F008, ROADMAP §10.11)
- an ELF with a huge `p_memsz` (§3.1; F009, ROADMAP §10.6)
- a `fork` near memory exhaustion, or a burst of exits (§2.1; F010,
  ROADMAP §10.10)

---

## 6. Tracing and counters

`syscall_init::set_trace(true)` makes `proc_init::syscall` log
`user: syscall <name> nr=<nr> = <ret>` on serial after each call; the line
is not a `vibeOS:` marker. Nothing calls `set_trace`, so no build can turn
tracing on (F150; ROADMAP §10.7).

`vibeos_syscall_stub` increments the calling TCB's `syscall_count` and the
global `SYSCALLS` on every entry, `ENOSYS` included. Nothing reads either:
`syscall_init::syscall_count` has no caller, and there is no per-process
sum (F150; ROADMAP §10.7).

---

## 7. First userspace

Static ELF64, no libc, hand-written `syscall` stubs. Initrd:

- `/hello` — write + exit 42 (Slice B proof)
- `/sbin/init` — post-init kernel job: `fork`/`exec` tests then `/bin/sh`, then reap
- `/bin/tests` — syscall / `EFAULT` / `fork`+`exec`+`wait` / fault-kill runner
- `/bin/sh` — interactive shell; prints `vibeOS: shell ready` then `vibeos>`

Stack: `argc`, `argv`, an empty `envp` (`execve` does not read its `envp`
argument), and `auxv`: `AT_PAGESZ`, `AT_ENTRY`, `AT_PHENT`, `AT_PHNUM`,
`AT_PHDR` (0 when no header table is mapped), `AT_BASE` 0, `AT_FLAGS` 0,
`AT_UID`, `AT_EUID`, `AT_GID`, and `AT_EGID` (all 0), `AT_CLKTCK` 100,
`AT_SECURE` 0, `AT_RANDOM`, `AT_NULL`. `AT_RANDOM` is one TSC read and a
multiply, not random (F140; ROADMAP §13.10 fills it from the `/dev/random`
source). Only `ET_EXEC` loads: `ET_DYN` and `PT_INTERP` return `ENOEXEC`.
`PT_PHDR` VAs are `check_user_va`'d. Exit status is the kernel-reported
low 8 bits (`user: exit N` diagnostic for the bootstrap hello).

The kernel REPL is debug-only (`--features kernel_shell`). Production
starts `/sbin/init` and parks.

`fork` is a full address-space copy until ROADMAP §12.3 adds
copy-on-write. `execve` builds the new address space first and replaces the
old one only after a successful load.

`FS_BASE` is written at a process's first ring-3 entry and at `execve`, and
`fork` copies the live MSR into the child. No context switch saves or
restores it, so a process that uses TLS can resume with the base another
process left, or with 0 (F022). The FP-state differences are in §1 (F069).

---

## 8. Native interfaces

vibeOS allocates no syscall numbers. Linux allocates numbers as it goes,
on both architectures from one shared range since 5.1, so a number free
today is taken by some later Linux, and a binary built for that Linux would
then call vibeOS's meaning of it. A vibeOS-only interface is a file under
`/proc` or `/sys`, an `ioctl` on a vibeOS device node, or a generic-netlink
family, and `docs/LINUX.md` lists it with its reason (ROADMAP, How to read
this).

The one exception is `psinfo` (500), which predates this rule. Linux has
not reached 500, and ROADMAP §13.9 deletes the call when `ps` moves to
`procfs`; its syscall-table check fails on any row whose number musl's
pinned header does not name.
