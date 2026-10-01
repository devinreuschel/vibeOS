# Syscall ABI

The Linux kernel calling convention of each architecture, with Linux's
syscall numbers, errno values, and argument rules: on x86_64 the one in the
x86-64 psABI's appendix A.2, not the SysV function-call convention; on
aarch64 `svc #0` with the number in `x8` (ROADMAP §11.6). The ROADMAP rule
"Linux interfaces" makes a call with a Linux number implement Linux's
interface, as of the baseline Linux release that `docs/LINUX.md` names
(ROADMAP §12.4). This file describes the code as built.
Where the code differs from Linux or from a rule stated here, the section
says how. A note such as (F083; ROADMAP §10.4) names a finding in the
kernel review ([reviews/KERNEL_REVIEW.md](reviews/KERNEL_REVIEW.md)) and the
ROADMAP section that fixes it. The fixing ROADMAP line cites the same id, so
for a note with only an id, search ROADMAP.md for the id.

Index: [DESIGN.md](DESIGN.md) §1.4. Landed: [ROADMAP.md](ROADMAP.md) §9.3–§9.8,
except the boxes the kernel review reopened there. Open: ROADMAP §10.4
(file tables, VFS dispatch), §10.6 (entry paths and user memory), §10.7 (tracing), §10.10 (kernel stack
reclaim and IPI acks), §10.11 (vibefs file size), §11.6 (the aarch64
convention, tagged pointers, and `FS_BASE`), §12.3 (copy-on-write `fork`),
§13.1 (shared open files), §13.7 (process-group `kill` and `wait4`), §13.8
(real-time signals), §13.9 (POSIX floor), §13.10 (`AT_RANDOM`).

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

Segment selectors are ABI (DESIGN §5.1). A process runs with CS `0x33` and
SS `0x2b`, as on Linux: `IA32_STAR`'s SYSRET base is `0x23`, and `sysretq`
loads SS from +8 and CS from +16. DS, ES, FS, and GS hold 0: `execve` and a
new process's first entry load the null selector into all four, `fork`
copies the parent's, and the context switch keeps each thread's.

Numbers and arguments are read as Linux's entry code reads them (ROADMAP
§10.5): the number is `eax` sign-extended, so the high half of `rax` is
ignored, and on aarch64 it is the low 32 bits of `x8`, read as unsigned
(ROADMAP §11.6); a number that names no call after that step returns
`-ENOSYS` (`syscall::NrTable::lookup`). Each argument reaches its handler
converted to the width and signedness of the C type its §3 row declares,
as in Linux's prototype, on both architectures: `int` and `pid_t` to 32
bits signed, `unsigned int` to 32 bits, `long` and `off_t` to 64 bits
signed, `umode_t` to 16 bits, and `unsigned long`, `size_t`, and a
pointer whole. So `read` with `rdi` `0xFFFF_FFFF_0000_0003` reads
descriptor 3, `kill` with `rdi` `0x1_0000_0005` signals pid 5, and `wait4`
with `rdi` `0xFFFF_FFFF` waits for any child, as on Linux. An unknown flag
bit is ignored where Linux's call ignores it (`open`) and returns `EINVAL`
where Linux's call rejects it (`openat2`, `clone3`, `renameat2`).

A syscall preserves the x87 and SSE state. The kernel never touches those
registers (it is soft-float, and `make` rejects a kernel ELF with an FP or
SIMD instruction outside its save and load routines), so neither the entry
nor the exit saves them: the FP binding of DESIGN §7.5 saves a thread's
state only when a switch takes the CPU away from it, and the exit, with
IF=0, loads it only when the registers hold another thread's. `fork` and
`execve` follow the psABI, as on Linux: `fork` saves the caller's live
registers under the binding and copies its x87, XMM, and MXCSR state into
the child before the child can run (`syscall_init::fork_fp`), and `execve`,
after its point of no return, starts the new image from
`vibeos::thread::Fxsave::INITIAL`, FCW `0x037F` and MXCSR `0x1F80` with
the x87 and XMM registers zeroed (`syscall_init::exec_fp`). Every thread,
`/sbin/init` included, starts from that image too, whatever the firmware
left in the registers.

`FMASK` (DESIGN §7.2) clears `TF`, `IF`, `DF`, `IOPL`, `NT`, and `AC` on
entry. The entry saves the user frame, the 21 words of Linux's
`user_regs_struct` (`vibeos::syscall::UserFrame`), at the top of the
thread's kernel stack: RCX in both the `rcx` and `rip` slots, R11 in both
`r11` and `rflags`, the syscall number in `orig_rax`, `-ENOSYS` in the `rax`
slot (as Linux shows at a syscall-entry stop), the user selectors in `cs`
and `ss`, and the user RSP, which it copies out of the per-CPU scratch, in
`rsp`. The exit stores the return value in the `rax` slot and uses
`sysretq` only when the saved RIP equals the saved RCX, the saved RFLAGS
equals the saved R11, CS and SS are the user selectors, RIP is below
`USER_MAP_END`, and RF, TF, and VM are clear
(`vibeos::arch::x86_64::trap::sysret_ok`); otherwise it restores all 15 GPRs from
the frame and uses `iretq` on the frame's tail, as Linux does, so a hook or
a later `execve` that changes RCX or R11 returns them intact. A
spawned or forked process's first entry is the same exit over the frame its
creator wrote (`syscall_init::first_return`, DESIGN §5.10 rule 4). A fault on either `iretq` (a `#GP`, `#NP`, or
`#SS` whose RIP is the labeled instruction) kills the process with
`SIGSEGV`, and never halts the kernel (DESIGN §5.10 rule 2). A restartable
syscall resumes by reloading `rax` from `orig_rax` and moving RIP back 2
bytes (`SyscallAbi::restart`).

The body runs with IF=1 (DESIGN §2.9 rule 3): the entry stub moves the user
RSP out of the per-CPU scratch into its frame and then runs `sti`.

From the return of `vibeos_syscall_stub` to `sysretq` or `iretq`, the exit
path runs with IF=0: it stores the return value in the frame's `rax` slot,
reads everything it restores from the frame, and loads the user RSP before
`swapgs`, so an interrupt there could push its frame on the user stack at
CPL 0. No exit instruction writes a `gs:` operand; the per-CPU scratch
holds only the user RSP, between `syscall` and the entry's stack switch.
The exit's first instruction after the call is `cli`, whatever IF the body
returned with, and in debug builds each exit path checks IF before its
`swapgs` (on the `sysretq` path before it loads the user RSP) and faults at
`vibeos_exit_if_set`, a `ud2`, if IF is set. `console_init::wait_key`, which
a console `read` blocks in, returns with the IF it was entered with.

Every return to ring 3 acts on a pending kill or stop (DESIGN §5.10 rule
11): after the `cli` and the return-value store, the syscall exit (and a new
thread's first entry, which enters it) calls `syscall_init::exit_work`, and
every vector's exit to CPL 3 calls it from `idt::exit_to_user` after that
exit's `cli`, except an NMI's. With IF=0 it checks the current process
(`proc_init::exit_work_pending`: `Stopped`, or `SIGKILL`, `SIGSTOP`, or a
signal whose default action is Term or Stop pending); when it finds work it
turns IF on, acts (`proc_init::do_exit_work`: a kill ends the process, a
stop waits on `stop_wq`), turns IF off, and checks again, so a process that
makes no syscall is killed or stopped at its next interrupt. Debug builds
assert IF=0 at the check. `kill` of a Term or Stop signal sends the
target's CPU the reschedule IPI after it publishes the signal, when the
target is running on another CPU. Handler delivery is ROADMAP §13.8's.

The top user page is never mapped: user mappings end at `USER_MAP_END`
(`0x0000_7FFF_FFFF_F000`), and `execve` of an image with a segment above it
fails with `ENOEXEC`, so a `syscall` in the last mappable page returns to a
canonical RIP. A saved RIP that is non-canonical anyway reaches neither
`sysretq` nor `iretq`: after its `cli` the exit tests it and kills the
process with `SIGSEGV` on the kernel GS, before any `swapgs`.

---

## 2. Return and errno

Success: any value outside the error range (a byte count, a pid, an
address, `0`, …).

Error: `rax = -errno`, from -4095 to -1, which userspace tests as an
unsigned compare (`rax` above `-4096`). A success value falls in that range
only where Linux's does, as Linux's `F_GETOWN` returns a process group as a
negative number, which is why glibc reads it through `F_GETOWN_EX`. Linux
names, Linux values:

<!-- gen_syscalls: begin errno-table -->

| Name | Value | Used |
|------|------:|------|
| `EPERM` | 1 | `mmap` with `MAP_FIXED` or `MAP_FIXED_NOREPLACE` below `NULL_GUARD_LEN` (page 0); making a symlink, a device node, or a directory, a hard link, a rename, or a removal that the filesystem cannot make, as FAT's `symlink` and `link` (no syscall makes one yet) |
| `ENOENT` | 2 | `open`/`execve` missing path |
| `ESRCH` | 3 | `kill`: no such process, a zombie, `pid` 0, or a negative 32-bit `pid` (§3.1) |
| `EIO` | 5 | device I/O error; on-disk corruption, a failed checksum or bad magic on FAT or vibefs |
| `E2BIG` | 7 | `execve`: a string over 131,072 bytes with its NUL, or strings and pointers together over max(128 KiB, min(`RLIMIT_STACK`/4, 6 MiB)), 2 MiB at the fixed 8 MiB `RLIMIT_STACK` (§3.1) |
| `ENOEXEC` | 8 | malformed ELF, `ET_DYN`, or `PT_INTERP` |
| `EBADF` | 9 | closed / out-of-range fd; `read` on an `O_WRONLY` fd and `write` on an `O_RDONLY` one; a file `mmap` (no `MAP_ANONYMOUS`) with a bad fd |
| `ECHILD` | 10 | `wait4` with no matching child |
| `EAGAIN` | 11 | `fork` with every process-table slot in use, zombies included (`limits::MAX_PROCS` is 256), or no pid free (pids and tids share one allocator, up to 32,767, then from 300), or the thread table has no free slot (ROADMAP §10.4, F037); `read` of `/dev/random` or `/dev/urandom` when virtio-rng and `RDRAND` supply no byte (ROADMAP §10.12; until §13.10) |
| `ENOMEM` | 12 | AS clone / load; an image above `limits::EXEC_IMAGE_MAX`; `mmap` with no free range, a full region table (256 regions, `limits::MAX_REGIONS`, where Linux's `vm.max_map_count` allows 65,530; ROADMAP §10.4), a `len` past `USER_MAP_END`, or no frames; a `munmap` that must split a region when the region table is full; a kernel heap allocation that fails in `fork`, `execve`, or `open` (DESIGN §4.4), `execve` argument buffers included |
| `EACCES` | 13 | `open` with `O_CREAT` of a new file in `/dev`, `/proc`, or `/sys` |
| `EFAULT` | 14 | bad user pointer / length |
| `EBUSY` | 16 | `dup2` onto a descriptor an `open` in progress reserved, which no process reaches while each has one thread |
| `EEXIST` | 17 | `O_EXCL`; `mmap` with `MAP_FIXED_NOREPLACE` (or `MAP_FIXED`, §3.1) over a mapping |
| `EXDEV` | 18 | a `rename` or `link` across mounts (no syscall makes one yet) |
| `ENODEV` | 19 | a file `mmap` (no `MAP_ANONYMOUS`) on an open fd: file mappings come in ROADMAP §12.4 |
| `ENOTDIR` | 20 | |
| `EISDIR` | 21 | |
| `EINVAL` | 22 | `lseek` with a bad `whence` or a resulting offset below 0, unknown `fcntl` command, `kill` signal 0 or above 31; the `mmap` and `munmap` argument checks in §3.1; `read` or `write` of an object that cannot be read or written; `open` or `execve` of the empty path (Linux: `ENOENT`); `open` with `O_TRUNC` of a `/proc` file |
| `ENFILE` | 23 | `open` or `execve` with the system-wide open-file table full: 1024 open files, `limits::MAX_OPEN_FILES` |
| `EMFILE` | 24 | per-process fd table full: 256 descriptors, `limits::MAX_FDS` (`open`, `dup`) |
| `EFBIG` | 27 | a vibefs `write` that starts at or past the file-size limit, byte 2^44 − 4096 (VIBEFS.md §3); a FAT `write` past 4 GiB, FAT's file-size limit |
| `ENOSPC` | 28 | `write` or `open` with `O_CREAT` on a volume out of blocks, inodes, or directory entries, or a vibefs `write` that needs a fifth extent |
| `ESPIPE` | 29 | `lseek` on the console, `/dev/console`, or `/dev/tty` |
| `EROFS` | 30 | defined; no syscall returns it: a write to a read-only virtio-blk device fails with it in the block layer |
| `ENAMETOOLONG` | 36 | path of 256 bytes or more; name above 64 bytes. ROADMAP §13.9 moves the path and name limits to Linux's 4096 and 255 |
| `ENOSYS` | 38 | unknown number |
| `ENOTEMPTY` | 39 | defined; no syscall returns it |
| `ELOOP` | 40 | `open` or `execve` through too many symbolic links, or a walk of more than 80 steps (`limits::MAX_WALK`) |
| `EOPNOTSUPP` | 95 | defined; no syscall returns it. It is left for the cases Linux gives it, such as an extended-attribute namespace a mount refuses (ROADMAP §14.8) |

<!-- gen_syscalls: end errno-table -->

Unknown numbers return `-ENOSYS`.

### 2.1 Differences from Linux

The mapping is `vibeos::kerror`: each module error converts to one `KError`
through its `From` impl, and the `KError` table generates §2 (ROADMAP §10.4).

- `fork` near memory exhaustion returns `ENOMEM`: a kernel stack, TCB or
  address-space slot that cannot be allocated frees what was taken
  (F010; ROADMAP §10.4)

---

## 3. Syscalls

`proc_init::dispatch_frame` looks the number up in the port's table
(`SyscallAbi::table`, x86_64's generated into `crates/core/src/arch/x86_64/syscall.rs`) and calls the row's handler, a method of
`syscall::Handlers` that takes each argument in its C type and returns
`Result<usize, KError>`; dispatch encodes an error as `-errno`. One table,
`crates/core/src/proc/syscalls.toml`, holds each call's number per
architecture, its arguments' C types in order, and its pointer arguments;
`scripts/gen_syscalls.py` writes from it the kernel's table
(`vibeos::syscall::ROWS`, the `SYS_*` numbers, and the §6 trace names), the
user stubs (`vibeos_user::sys`), and the table below, and `make check` fails
when one differs (ROADMAP §10.5).

Dispatch checks no pointer itself (ROADMAP §10.5). Each row declares its
pointer arguments, and the table's "pointer arguments" column shows them
(`—` for a call with none): a buffer the kernel reads (`in`) or writes
(`out`) with the argument that holds its length, a fixed-size value, a C
string, or a NULL-terminated vector of C strings, each marked where NULL is
valid, or a pointer not read yet with the ROADMAP line that reads it. Each
declaration names where its handler first copies through it: after the
checks Linux's handler makes before that copy (the descriptor, the flags,
whether a path or a child exists), so a call with two bad arguments returns
the errno the baseline returns: `read(-1, <unmapped>, 1)` is `EBADF`, and
`wait4` with no child and an unmapped status pointer is `ECHILD`. The
in-guest test `syscall_ptr_decl_efault` passes an unmapped and a
kernel-half pointer in each declared pointer argument, with every other
argument valid, and needs `EFAULT` from each, so a declaration cannot drift
from its handler (F150).

Each row also lists, in its `errors` key and the table's "errors" column,
every errno its handler returns, audited against the handler and its
callees. An errno that only a kernel fault produces (a device error, a
failed kernel allocation) names in parentheses the in-guest test that
provokes it. `/bin/tests`' `errno_matrix` (ROADMAP §10.5) reads the
generated `vibeos_user::sys::CALLS`: it calls every row once with valid
arguments and checks the result, except `exit`, which its child cases
call, and `reboot`, whose valid call powers off; it provokes every other
listed pair and fails on a pair it has no case for. Its `efault_matrix`
gives every declared pointer a kernel-half address, an unmapped page, a
range crossing `USER_MAP_END` and one crossing `USER_END`, a read-only
page for an `out` pointer, a length of 2^63 for a buffer, and NULL where
the row does not allow it, and needs `EFAULT` from each.

<!-- gen_syscalls: begin syscall-table -->

| x86_64 | aarch64 | name | arity | arguments | pointer arguments | errors | notes |
|---:|---:|------|------:|-----------|-------------------|--------|-------|
| 0 | 63 | `read` | 3 | `unsigned int fd`, `char *buf`, `size_t count` | `buf`: out, `count` bytes, after the `fd` lookup | `EBADF`, `EFAULT`, `EISDIR`, `EIO` (`vblk_bad_sector`) | — |
| 1 | 64 | `write` | 3 | `unsigned int fd`, `const char *buf`, `size_t count` | `buf`: in, `count` bytes, after the `fd` lookup | `EBADF`, `EFAULT`, `EINVAL`, `EFBIG`, `ENOSPC`, `EIO` (`vblk_bad_sector`) | — |
| 2 | — | `open` | 3 | `const char *pathname`, `int flags`, `umode_t mode` | `pathname`: C string, before anything else | `EFAULT`, `ENAMETOOLONG`, `EINVAL`, `ENOENT`, `ENOTDIR`, `EISDIR`, `EEXIST`, `EACCES`, `ELOOP`, `EMFILE`, `ENFILE`, `ENOSPC`, `ENOMEM` (`kalloc_nomem`), `EIO` (`vblk_bad_sector`) | `pathname` at most 255 bytes |
| 3 | 57 | `close` | 1 | `unsigned int fd` | — | `EBADF` | — |
| 5 | 80 | `fstat` | 2 | `unsigned int fd`, `struct stat *statbuf` | `statbuf`: out, 144 bytes, after the `fd` lookup | `EBADF`, `EFAULT` | x86_64's 144-byte `struct stat`; see SYSCALL.md §3.1 |
| 8 | 62 | `lseek` | 3 | `unsigned int fd`, `off_t offset`, `unsigned int whence` | — | `EBADF`, `ESPIPE`, `EINVAL` | — |
| 9 | 222 | `mmap` | 6 | `unsigned long addr`, `unsigned long length`, `unsigned long prot`, `unsigned long flags`, `unsigned long fd`, `unsigned long offset` | — | `EINVAL`, `EBADF`, `ENODEV`, `ENOMEM`, `EPERM`, `EEXIST` | anonymous and private only; returns the address |
| 11 | 215 | `munmap` | 2 | `unsigned long addr`, `size_t length` | — | `EINVAL`, `ENOMEM` | — |
| 12 | 214 | `brk` | 1 | `unsigned long addr` | — | — | returns the break; `0` if the caller is not a process |
| 24 | 124 | `sched_yield` | 0 | — | — | — | — |
| 32 | 23 | `dup` | 1 | `unsigned int oldfd` | — | `EBADF`, `EMFILE` | CLOEXEC cleared on the new fd |
| 33 | — | `dup2` | 2 | `unsigned int oldfd`, `unsigned int newfd` | — | `EBADF` | — |
| 35 | 101 | `nanosleep` | 2 | `const struct __kernel_timespec *rqtp`, `struct __kernel_timespec *rmtp` | `rqtp`: in, 16 bytes, before anything else; `rmtp`: not read (ROADMAP §13.8) | `EFAULT`, `EINVAL` | `CLOCK_MONOTONIC`, rounded up to the tick; see SYSCALL.md §3.1 |
| 39 | 172 | `getpid` | 0 | — | — | — | `0` if the caller is not a process |
| 57 | — | `fork` | 0 | — | — | `EAGAIN`, `ENOMEM` (`fork_oom`) | full address-space copy; the child returns 0 |
| 59 | 221 | `execve` | 3 | `const char *pathname`, `const char *const *argv`, `const char *const *envp` | `pathname`: C string, before anything else; `argv`: C string vector, may be NULL, after `pathname`; `envp`: C string vector, may be NULL, after `argv` | `EFAULT`, `ENAMETOOLONG`, `EINVAL`, `ENOENT`, `ENOTDIR`, `ELOOP`, `ENFILE`, `E2BIG`, `ENOEXEC`, `ENOMEM` | `argv` and `envp`: NULL-terminated vectors of C strings, copied to the new stack under Linux's limits (§3.1) |
| 60 | 93 | `exit` | 1 | `int status` | — | — | the low 8 bits of `status` |
| 61 | 260 | `wait4` | 4 | `pid_t pid`, `int *wstatus`, `int options`, `struct rusage *rusage` | `wstatus`: out, 4 bytes, may be NULL, after a child is reaped; `rusage`: not read (ROADMAP §13.7) | `ECHILD`, `EFAULT` | — |
| 62 | 129 | `kill` | 2 | `pid_t pid`, `int sig` | — | `EINVAL`, `ESRCH` | default actions only |
| 72 | 25 | `fcntl` | 3 | `unsigned int fd`, `unsigned int cmd`, `unsigned long arg` | — | `EBADF`, `EINVAL` | `F_GETFD` and `F_SETFD` (`FD_CLOEXEC`) only |
| 110 | 173 | `getppid` | 0 | — | — | — | — |
| 169 | 142 | `reboot` | 4 | `int magic1`, `int magic2`, `unsigned int cmd`, `void *arg` | `arg`: C string, for `RESTART2` only, after the uid, magic and command checks | `EINVAL`, `EFAULT` | power off and restart; see SYSCALL.md §3.1 |
| 217 | 61 | `getdents64` | 3 | `unsigned int fd`, `struct linux_dirent64 *dirent`, `unsigned int count` | `dirent`: out, `count` bytes, after the `fd` lookup and the first record's fit | `EBADF`, `ENOTDIR`, `ESPIPE`, `EINVAL`, `EFAULT` | at most 512 bytes a call; see SYSCALL.md §3.1 |
| 500 | — | `psinfo` | 2 | `char *buf`, `size_t len` | `buf`: out, `len` bytes, before anything else | `EFAULT` | vibeOS-specific (SYSCALL.md §8; LINUX.md `psinfo`) |

<!-- gen_syscalls: end syscall-table -->

`sched_yield` calls the kernel `yield_now` when the caller has a pid
(any spawned process). A kernel-side `dispatch()`
probe with no process (ktest, IF off) returns `0` without scheduling.

### 3.1 Behavior and differences from Linux

- `read`: a file read copies through a 256-byte kernel buffer and returns
  at most 256 bytes per call (a short read in the middle of a regular file,
  which Linux never gives: it stops early only at end of file, at a fault, or
  on a fatal signal; ROADMAP §12.5); a console read returns after a newline
  or `rdx` bytes
- `open`: `mode` is ignored, and a vibefs file is created 0644 (F149;
  ROADMAP §13.9). Unknown flag bits are ignored, as in Linux. `open`
  reserves the descriptor (`EMFILE`) and the open-file slot (`ENFILE`)
  before it creates or truncates, so a failed `open` changes no file; a
  trailing `/` after a missing name fails with `ENOTDIR` unless the call is
  `mkdir` (F057; ROADMAP §10.4)
- `lseek`: `SEEK_END` reads the size from the file's inode (FAT's
  counted in-core inode or the vibefs inode), so it sees writes through
  any descriptor. On a vibefs file a resulting offset above 2^44 − 4096
  (VIBEFS.md §3) returns `EINVAL`, as Linux's does past a filesystem's
  maximum file size; a `write` that starts at or past the limit returns
  `EFBIG`, and one that would cross it is cut short at the limit, as
  Linux's is. On a FAT file any offset from 0 to `i64::MAX` is accepted
  (F008; ROADMAP §10.11)
- `mmap`: anonymous private mappings only: `MAP_PRIVATE|MAP_ANONYMOUS`,
  plus any of `MAP_FIXED`, `MAP_FIXED_NOREPLACE`, `MAP_NORESERVE`,
  `MAP_POPULATE`, and `MAP_STACK`. `prot` is any mix of `PROT_READ`,
  `PROT_WRITE`, and `PROT_EXEC`, where W or X also allows reads, and
  `PROT_NONE` reserves the range with no frames. Each page is allocated and
  zeroed at the call until ROADMAP §12.4. The checks run in mmap(2)'s
  order: an `off` that is not page-aligned is `EINVAL`; a file mapping (no
  `MAP_ANONYMOUS`) is `EBADF` for a bad fd and `ENODEV` otherwise, until
  ROADMAP §12.4 adds file mappings; then a `len` of 0, a map type other
  than `MAP_PRIVATE` (`MAP_SHARED` included, until ROADMAP §12.4), any
  other flag bit, and a `prot` bit other than the three are `EINVAL`,
  where Linux ignores unknown flag bits; a `len` that rounds past
  `USER_MAP_END` is `ENOMEM`; a fixed request whose `addr` is not
  page-aligned is `EINVAL`. A fixed request below page 0's guard is
  `EPERM`, and one past `USER_MAP_END` is `ENOMEM`. `MAP_FIXED` over an
  existing mapping returns `EEXIST`, as `MAP_FIXED_NOREPLACE` does, where
  Linux replaces the mapping (ROADMAP §12.4). Without a fixed flag a free
  hint is used, rounded down to a page; otherwise the highest free range
  below `0x7FFF_F7FF_F000` (DESIGN §4.1), or `ENOMEM`. Each call is its own
  region, never merged with a neighbour, so a full region table (256,
  `limits::MAX_REGIONS`) is `ENOMEM` (ROADMAP §10.4 sizes it)
- `munmap`: `addr` must be page-aligned, `len` non-zero, and the range at or
  below `USER_MAP_END` (`EINVAL`); `len` rounds up to a page. It trims,
  splits, or removes the mappings in the range, and holes are fine. Each
  page leaves the TLB before its frame is freed. A split that finds the
  region table full returns `ENOMEM` with nothing unmapped, as mmap(2)
  documents. It allocates no memory (ROADMAP §10.5)
- `brk`: returns the new break, or the current one when the call fails,
  as the raw call does (glibc's `brk` wrapper turns that into `-1` and
  `ENOMEM`). The break starts on the page after the image's highest
  `PT_LOAD`, as on Linux with randomization off, and `execve` resets it,
  while `fork` copies it. `brk(0)`, a value below the start, a value past
  `USER_MAP_END`, growth into another mapping (the stack included), and a
  frame shortage leave it where it is. Growth maps zeroed pages at the call
  (ROADMAP §12.4 makes them lazy); shrinking unmaps the whole pages above
  the new break
- `execve`: the loader reads the ELF header and program headers from the
  open file and copies each segment's file bytes into the new address space
  in 512-byte chunks, so the file's size has no bound of its own; a file that
  ends inside a segment, or shrinks while it loads, is `ENOEXEC`, and a
  filesystem error keeps its errno. An image whose page-rounded
  `PT_LOAD` and `PT_TLS` bytes together exceed 1 GiB
  (`limits::EXEC_IMAGE_MAX`) returns `ENOMEM` before anything is mapped,
  where Linux loads it while memory lasts (LINUX.md `exec-image-cap`; F009,
  ROADMAP §10.6). Under the cap, every page is allocated and zeroed at the
  call, in chunks of at most 512 pages (one leaf table), with the page-table
  lock dropped between chunks; a frame shortage unmaps and frees what the
  load mapped and returns `ENOMEM` to the old image. `argv` and `envp`
  take Linux's limits, as execve(2) states them: a string of 131,072 bytes
  or more before its NUL (`MAX_ARG_STRLEN`) returns `E2BIG`, and so do
  strings, NULs and 8-byte pointers together over max(128 KiB,
  min(`RLIMIT_STACK`/4, 6 MiB)), 2 MiB at the fixed 8 MiB `RLIMIT_STACK`
  (`limits::RLIMIT_STACK_DEFAULT` until ROADMAP §13.9's `setrlimit`); there
  is no count cap. Both are copied, `argv` then `envp`, into one per-call
  kernel buffer before the load starts, so `E2BIG`, and `ENOMEM` for a
  buffer that cannot grow, return to the old image. A NULL or empty `argv`
  starts the image with `argc` 1 and an empty `argv[0]`, as Linux does; a
  NULL `envp` is an empty environment. The new stack holds the strings,
  their pointers and the auxiliary vector, page-rounded, plus 128 KiB
  (ROADMAP §10.5)
- `wait4`: `pid > 0` waits for that child, any `pid < 0` for any child, and
  `pid == 0` returns `ECHILD`; Linux reads 0 and `pid < -1` as process
  groups. A NULL `wstatus` is accepted and nothing is written, as on Linux;
  otherwise the child is reaped before its status is copied out, so a bad
  `wstatus` returns `EFAULT` with the child gone, and the next `wait4` for
  it is `ECHILD`. A stopped child is never reported. Only `WNOHANG` is read; other option bits are accepted and ignored, and `r10`
  (`rusage`) is not read (F149; ROADMAP §13.7)
- `kill`: signal 0 returns `EINVAL`, where Linux checks existence and
  permission (F149; ROADMAP §13.7). Signals 32 to 64 return `EINVAL` until
  ROADMAP §13.8 adds real-time signals. `pid` is truncated to 32 bits, and
  `pid` 0 and every negative 32-bit `pid` return `ESRCH`, where Linux
  signals, for 0, the caller's process group, for -1, every process the
  caller may signal except pid 1 and the caller, and for any other negative
  `pid`, process group `-pid` (F149; ROADMAP §13.7). A `pid` that names a
  zombie returns `ESRCH`; Linux returns 0 (ROADMAP §13.7). A signal sent
  to pid 1 is dropped, and `kill` returns 0, unless init has a handler for
  it, as Linux does; none can exist before ROADMAP §13.8, and never for
  `SIGKILL` or `SIGSTOP` (F068)
- `exit`: the caller's children go to the reaper `proc::reaper_for` picks:
  pid 1 while init is live or stopped; otherwise none, so a child reads
  `getppid()` 0 and is freed when it exits (a zombie child at once), as in
  the `kernel_tests` and `kernel_shell` builds. A process the kernel starts
  has parent 0: `/sbin/init`, and the boot `/hello` and the in-guest test
  programs, which `proc_init::wait_kernel` reaps. Linux reparents to a subreaper or
  init and panics when init exits (ROADMAP §10.5, F068)
- `psinfo`: writes one `<pid> <ppid> <state> <name> <syscalls>` line per
  process, in pid order (state `run`, `stop`, or `zombie`; `<syscalls>` is
  the sum of `Tcb.syscall_count` over the process's live threads, every
  entry counted, `ENOSYS` included). It formats the whole lines that fit in
  512 bytes and copies at most `rsi` of those bytes. Number 500 is in the
  range Linux allocates next (F149); ROADMAP §13.9 deletes the call when
  `ps` moves to `procfs`
- `getdents64`: writes whole `linux_dirent64` records, `.` and `..`
  first, and returns at most 512 bytes per call, which its 512-byte kernel
  buffer bounds; `EINVAL` when the first record does not fit in `count`,
  and 0 at the end. `d_off` is the cookie of the next entry: `lseek(fd,
  d_off, SEEK_SET)` resumes at that entry, and 0 rewinds. The position moves
  only after the copy succeeds, so an `EFAULT` leaves it where it was. The
  position is read and stored in two steps, so an overlapping call on an
  open file shared through `fork` or `dup` can lose an update until
  ROADMAP §13.1 (F055); entries added or removed between calls may repeat
  or be skipped, which POSIX leaves unspecified. The console descriptors a
  process starts with are `ENOTDIR`; `/dev/console` or `/dev/tty` opened by
  path is `ESPIPE`, as its `lseek` is
- `fstat`: x86_64's 144-byte `struct stat` for any descriptor, the console
  a character device (`S_IFCHR | 0620`). `st_dev` and `st_rdev` are 0 until
  ROADMAP §23.3, and `st_uid` and `st_gid` 0 until ROADMAP §13.9; the times
  are whole seconds, their nanosecond fields 0; `st_blksize` is 4096 and
  `st_blocks` is ⌈size/512⌉
- `nanosleep`: sleeps until a `CLOCK_MONOTONIC` deadline, rounded up to the
  next scheduler tick, and `SIGKILL` ends the sleeper at once. `EINVAL` when
  `tv_nsec` is outside 0 to 999,999,999 or `tv_sec` is negative. A deadline
  past 2^64 ns sleeps as long as the clock runs. `rmtp` is never written,
  since nothing can interrupt the sleep with `EINTR` before ROADMAP §13.8
  gives signals handlers; a stop and continue resumes the sleep, as Linux
  restarts it. So `rmtp` is never read either, and a bad `rmtp` is not
  `EFAULT`: Linux writes it only on `EINTR`, and `efault_matrix` exempts it
- `reboot`: checks, in this order, that the caller's effective uid is 0
  (`EPERM` otherwise: root holds `CAP_SYS_BOOT` until ROADMAP §18.6), that
  `magic1` is `0xfee1dead` and `magic2` one of reboot(2)'s four values
  (`EINVAL`), and then the command. `POWER_OFF` prints `vibeOS: reboot:
  power off` and powers off; `RESTART` prints `vibeOS: reboot: restart` and
  restarts; `RESTART2` reads its `arg` string (`EFAULT` if it cannot) and
  restarts, ignoring the string, as x86_64 does; `CAD_ON` and `CAD_OFF`
  return 0 and change nothing, since the keyboard has no Ctrl-Alt-Del
  action. `HALT` is `EINVAL`, where Linux halts: vibeOS has no halt outside
  the panic stop. `KEXEC`, `SW_SUSPEND` and any other value are `EINVAL`, as
  on a Linux built without them. There is no implicit sync, as on Linux
  (reboot(2)). On x86_64 a power-off writes ACPI S5 through the FADT's
  `SLEEP_CONTROL_REG` with a hard-coded `SLP_TYP` of 5, then QEMU's PM1a
  ports `0x604` and `0xB004`; a restart writes the FADT's reset register,
  then pulses the 8042, then writes `0xCF9` (`arch::x86_64::power`; ROADMAP
  §20.2 reads `_S5` and the PM1 control block, F097)
- `read`, `fstat`, `getdents64`, the `wait4` status, and `psinfo` write
  user memory through the accessors, which honour the page's W bit, so a
  read-only destination is `EFAULT` (§5; F023, ROADMAP §10.6)

---

## 4. File descriptors

Each process (`proc_init::Proc`) has a 256-slot fd table (`limits::MAX_FDS`), a row allocated
with the process table before `irq: enabled` (MEMORY.md §4.4). Fds 0, 1, and 2 are the console
mux (serial and framebuffer). A 0x1E byte written to them prints as `?` on the serial console
(docs/LINUX.md `console-rs-escape`).

A file fd names a slot in one system-wide open-file table, the VFS's
(`fs::Vfs`, 1024 entries, `limits::MAX_OPEN_FILES`), shared by every process with no
per-process quota, and the generation the slot had when the file was
opened. The slot holds a reference to the file's inode (a counted
reference to FAT's in-core inode, or a vibefs inode number), the offset,
and the open flags, so fds that `dup` or `fork` copied share one offset,
and separate opens of one FAT file share its size and first cluster.
While the table is full, every `open` and every `execve` (which needs a
slot to read the image) fails with `ENFILE` in every process (F057;
ROADMAP §10.4).

- `dup` / `dup2` copy the slot and clear `FD_CLOEXEC` on the new fd
- `open` `O_CLOEXEC` becomes per-fd `FD_CLOEXEC`; `execve` drops those
- open-file slots are refcounted so `dup`/`fork` share them
- a relative path resolves against the calling process's working
  directory, a counted reference to a directory that `fork` copies and
  exit drops (DESIGN §2.11); a process the kernel starts has `/` as its
  root and working directory, and there is no `chdir` until ROADMAP
  §13.9 (F057, F086)
- `open` and `execve` resolve a path through the VFS walker, one component
  at a time, crossing mounts, so a process reaches every mounted
  filesystem: `open("/dev/null")` opens devfs's `null`, and `/proc`,
  `/tmp`, `/sys`, and `/vibe` are procfs, tmpfs, sysfs, and vibefs.
  The walker follows path_resolution(7), with no string pass before it:
  repeated slashes count as one, `.` is the directory reached so far, and
  `..` is the physical parent of that directory, after any symlink before
  it has been followed, stays put at the process's root, and at a mount's
  root steps to the parent of the mountpoint. A component followed by `/`
  must be a directory (a symlink there is followed), else the call fails
  with `ENOTDIR`; only `mkdir` accepts a `/` after a name it creates. FAT
  names compare without regard to case in the dentry cache too, so
  `/VIBE/f` is `/vibe/f`, under the vibefs mount (F056; ROADMAP §10.4)
- `read`, `write`, and `lseek` copy the slot out, drop the table lock for
  the I/O, and write back only the offset. `refs` and `used` change only
  in `addref` and `close`, under the table lock, and the `close` that
  frees a slot bumps its generation, so a lookup or write-back through a
  descriptor whose slot was closed and reused fails with `EBADF`. Two
  calls on one open file that overlap still lose one call's offset update:
  syscall bodies are preemptible, so a process and its `fork` child can
  overlap on one inherited descriptor and lose an offset update until
  §13.1's position lock (F055)
- a kernel-side `dispatch()` probe with no process still sees `getpid=0`
  and `EBADF` for a closed fd; pointer-validation tests run as spawned
  ring-3 programs

---

## 5. User pointers

Syscalls copy user memory through the user-VA accessors of DESIGN §5.1
(`proc::uaccess_init` over `vibeos::proc::uaccess`), which dereference the
user address inside `stac`/`clac`, so the user PTE's present and `WRITABLE`
bits and `CR0.WP` apply, and a fault there becomes `EFAULT` through an
exception-table fixup. No syscall path walks the page tables first.

**Range check.** Before any copy, `uaccess::user_range_ok(ptr, len)`, a pure
check, accepts a non-empty range from `NULL_GUARD_LEN` (the first page is
never user memory) up to at most `USER_MAP_END` with no overflow, and an
empty range only below `USER_MAP_END`, as Linux's `access_ok`. A refused
range returns `-EFAULT` before any I/O, so `read(fd, kernel_ptr, 0)` returns
`-EFAULT`, as on Linux.

**Byte counts.** An accessor reports how many bytes it copied before a
fault. `read`, `write`, and `psinfo` return that count when it is above 0
and `-EFAULT` when it is 0, as Linux's do. `read` from a file reads at most
256 bytes, copies them out, and seeks back by the bytes it could not copy,
so the next `read` returns them; a file that cannot seek keeps them, as a
Linux device does. `write` copies each 256-byte chunk in, writes the bytes
it copied, and stops at a short chunk. `len == 0` returns 0 once the range
check passes.

**State before the copy.** A call that changes state before its copy-out
keeps the change and returns `-EFAULT`: `wait4` reaps the child, then copies
the status with no lock held, and a failed copy returns `-EFAULT`, so the
next `wait4` returns `-ECHILD`.

**Strings and vectors.** `open` and `execve` copy a C string with
`strncpy_from_user`, in chunks that stop at each page boundary and at
`USER_MAP_END`, so a string that ends before an unmapped page copies; a
path that fills the kernel buffer is `-ENAMETOOLONG`, an `execve` argument
or environment string over Linux's limits `-E2BIG` (§3.1), and a fault
before its NUL `-EFAULT`. Each 8-byte argv or envp pointer is one all-or-nothing
copy.

A user `#PF`, `#GP`, or `#UD` from the program itself is a process kill
(DESIGN §5.2 CPL split), not `EFAULT`.

**Kernel survival:** no user program may panic or halt the kernel (DESIGN
§2.5 for ring-3 exceptions, AGENTS.md rule 4 for syscall paths). The code
does not meet this yet:

- a device or keyboard interrupt taken in ring 3 runs with the user GS
  base and halts (F004; ROADMAP §10.6)
- the exit-path faults in §1 (F001, F007; ROADMAP §10.6)

---

## 6. Tracing and counters

`vibeos.strace=1` on the kernel command line (BOOT.md §3.2) makes
`syscall_init::init_bsp` call `syscall_init::set_trace(true)`, and
`proc_init::syscall` then logs `user: syscall <name> nr=<nr> = <ret>` on
serial after each call that returns (`?` names an unknown number); the line
is not a `vibeOS:` marker. `exit`, which never returns, prints no line.

`vibeos_syscall_stub` increments the calling TCB's `syscall_count`, an
atomic statistic (Relaxed), on every entry, `ENOSYS` included
(`syscall_init::bump_counter`). `psinfo` reports each process's sum over its
live threads, which `thread_init::sum_syscalls` takes in one pass over the
thread table (`vibeos::proc::sum_syscalls`), and the `/bin/sh` `ps` built-in
prints it; ROADMAP §13.9 moves it to `/proc/<pid>/vibeos/syscalls`, since
Linux's `/proc/<pid>/syscall` already means something else (F150).

---

## 7. First userspace

Static ELF64, no libc: Rust programs of the `vibeos-user` crate, built by
`make user` to `build/user/<name>` (below). Initrd:

- `/hello` (`user/src/bin/hello.rs`) — write + exit 42 (Slice B proof)
- `/sbin/init` (`user/src/bin/init.rs`) — post-init kernel job, which
  checks every `fork`, `execve` and `wait4` result and writes each failure
  to fd 2 in one line: `fork`/`exec` `/bin/tests`, printing
  `init: /bin/tests exited <status>` when its wait status is nonzero (or
  `init: /bin/tests start failed: <why>`), then `/bin/sh`, both with init's
  environment, and reap orphans until the shell ends
  (`init: /bin/sh ended: <status>`) or `wait4` fails
  (`init: wait4: errno <n>`, `ECHILD` included); then yield and start the
  shell again. A failed start is a `fork` error
  (`init: /bin/sh start failed: fork errno <n>`) or a shell child that exits
  127, the status its child exits with after
  `init: /bin/sh start failed: execve errno <n>`; a shell on the console
  never exits 127, since a console read never returns end of file. After
  three failed starts in a row init exits 1. Its exit, by `exit` or by
  a signal, panics the kernel after the line
  `vibeOS: init: pid 1 <how>` (`exited <n>`, `killed SIG<name>`, or
  `killed SIG<name> addr=0x<hex>` for a fault; INVARIANTS.md §2.5)
- `/bin/tests` (`user/src/bin/tests.rs`) — syscall / `EFAULT` /
  `fork`+`exec`+`wait` / fault-kill runner. Its cases, in `user/src/tests/`,
  run through `vibeos_user::utest::Runner`, which prints the ktest protocol
  with `utest:` (`vibeOS: utest: begin <n>`, `run <name> <deadline_ms>`,
  `ok <name>`, `FAIL <name>: <why>`, `skip <name>: <reason>`, `end`);
  `user: tests begin` comes first, and `user: tests ok` (status 0) or
  `user: tests fail` (status 1) last
- `/bin/sh` (`user/src/bin/sh.rs`) — interactive shell; prints
  `vibeOS: shell ready` then `vibeos>`, and reads fd 0 a byte at a time,
  echoing it, into a 4096-byte line of at most 64 words split on spaces and
  tabs, with no quoting, pipes or redirection until ROADMAP §13.7. Its
  built-ins are `poweroff` and `reboot` (the `reboot` call) and `ps` (what
  `psinfo` returns); a failing one prints `sh: <name>: errno <n>`, status
  1. Any other first word is a program it runs with `fork`, `execve` and
  `wait4`: a name with a `/` as given, any other from each `PATH`
  directory in turn (`/bin:/sbin` when `PATH` is unset), past `ENOENT` and
  `ENOTDIR`, passing the shell's environment. The child prints
  `sh: <name>: not found` and exits 127 when nothing is found, or
  `sh: <name>: cannot run: errno <n>` and exits 126; on fd 2 the shell
  prints `sh: <name>: exit <n>` for a non-zero exit and
  `sh: <name>: signal <n>` for a signal, and its last status is the code
  or 128 plus the signal. A read error exits 1; end of input (a file on
  fd 0; a console read never ends it) exits with the last status
- `/bin/envcheck` (`user/src/bin/envcheck.rs`) — exits 0 when its
  environment holds `K=v`, else 1 (`/bin/tests`' `exec_env_*` cases)
- `/bin/argcheck` (`user/src/bin/argcheck.rs`) — checks its `argv` against
  the mode its environment's `ARGCHECK` names: `empty` (`argc` 1 and an
  empty `argv[0]`) or `<n>:<m>` (`argc` `n`, every later argument `m`
  bytes); exits 0 when it holds, 2 for a bad mode, 3 for `argc`, 4 for
  `argv[0]`, 5 for a length (`/bin/tests`' `exec_*` argument cases)
- `/bin/ls`, `/bin/cat`, `/bin/echo`, `/bin/grep`, `/bin/wc`, `/bin/true`,
  `/bin/false`, `/bin/sleep`, `/bin/yes` and `/bin/cmp`
  (`user/src/bin/<name>.rs`, sharing `vibeos_user::cmd`) — the utilities
  ROADMAP §13's `ls | grep foo | wc -l` gate joins. `true` and `false` exit 0
  and 1; `echo [-n] [arg...]` joins its arguments with single spaces;
  `cat [file...]` copies each file, or fd 0 for none or `-`; `grep pattern
  [file...]` prints the lines that hold a fixed string, prefixed `<file>:`
  for several files (status 0 on a match, 1 on none, 2 on an error);
  `wc [-l] [-w] [-c] [file...]` prints the selected counts single-spaced and
  unpadded, then the name (none for fd 0), and a `total` line for several
  files; `ls [-a] [path...]` prints a directory's `getdents64` names sorted
  bytewise, dot-names only under `-a`, a non-directory operand itself, and
  `<path>:` headers for several operands (status 2 if one fails); `sleep
  <seconds>` sleeps whole seconds through `nanosleep`; `yes [arg...]` writes
  `y`, or its arguments, one `write` a line until killed; `cmp file1 file2`
  prints `<f1> <f2> differ: char <n>, line <l>`, or `cmp: EOF on <f>` on fd 2
  for a prefix (status 0 equal, 1 different, 2 on an error). Each reads until
  `read` returns 0 and reports an error on fd 2 as `<prog>: <what>: errno
  <n>`

Stack: `argc`, `argv`, `envp` (the caller's, copied by `execve`;
`/sbin/init`'s from the kernel command line, BOOT.md §3.2, at most 8
arguments and 8 environment strings), and `auxv`. The
`auxv`: `AT_PAGESZ`, `AT_ENTRY`, `AT_PHENT`, `AT_PHNUM`,
`AT_PHDR` (0 when no header table is mapped), `AT_BASE` 0, `AT_FLAGS` 0,
`AT_UID`, `AT_EUID`, `AT_GID`, and `AT_EGID` (all 0), `AT_CLKTCK` 100,
`AT_SECURE` 0, `AT_RANDOM`, `AT_NULL`. `AT_RANDOM` is one TSC read and a
multiply, not random (F140; ROADMAP §13.10 fills it from the kernel CSPRNG).
Only `ET_EXEC` loads: `ET_DYN` and `PT_INTERP` return `ENOEXEC`. Two
`PT_LOAD`s that share a page load as Linux loads them: the page gets the
later segment's permissions and holds both segments' bytes (F031). `PT_PHDR`
VAs are `check_user_va`'d. Exit status is the kernel-reported low 8 bits
(`user: exit N` diagnostic for the bootstrap hello).

The Rust user runtime (`vibeos-user`, ROADMAP §10.5) is built, and
`kernel_tests` kernels embed its programs for the in-guest tests
(`Image::UserBin`; `user_runtime` runs `ktest_rt` in ring 3). `_start`, in `user/src/arch/<arch>/`,
passes the initial stack pointer to `rt::start`, which reads `argc`,
`argv`, `envp` and `auxv` into an `env::Env`, calls the program's
`main!` function, and exits with its return value as the status. A panic
writes `panicked at <file>:<line>:<col>:` and the message, one line
each, to fd 2 in one `write` (cut at 512 bytes) and exits with status 101. The runtime's `#[global_allocator]`
(`user/src/alloc.rs`) grows the heap with `brk`, so `alloc`'s `Box`, `Vec`
and `String` work in user programs. Each program
links as a static non-PIE `ET_EXEC` at `0x4000_0000` for
`x86_64-unknown-linux-musl`, with no libc and no crt objects (BOOT.md
§3.1).

The kernel REPL is debug-only (`--features kernel_shell`). Production
starts `/sbin/init` and parks.

`fork` is a full address-space copy until ROADMAP §12.3 adds
copy-on-write. `execve` builds the new address space first and replaces the
old one only after a successful load.

`FS_BASE` is written at a process's first ring-3 entry and at `execve`, and
`fork` copies the live MSR into the child. No context switch saves or
restores it, so a process that uses TLS can resume with the base another
process left, or with 0 (F022). FP state follows the psABI (§1).

---

## 8. Native interfaces

vibeOS takes nothing in a space Linux allocates as it goes: syscall numbers, `prctl` and `arch_prctl`
options, auxv types, flag bits, signal and errno numbers, `ioctl` numbers on a node Linux defines,
fixed device numbers, netlink protocol numbers, and the names of `/proc`, `/sys`, and debugfs files,
generic-netlink families, and kernel command-line options. Linux fills each space over time, on both
architectures from one shared syscall range since 5.1, so a value or name free today is taken by some
later Linux, and a binary or a system built for that Linux would then get vibeOS's meaning of it.

A vibeOS-only interface lives under a vibeOS name: a file under `/proc/vibeos/`, `/proc/<pid>/vibeos/`,
or `/sys/kernel/vibeos/`, or in a `vibeos/` directory inside a sysfs or debugfs directory Linux owns
(such as debugfs `block/<dev>/vibeos/`); a generic-netlink family named `vibeos_<name>`; a device node
under `/dev/vibeos/` on a dynamic major or a dynamic misc minor, with its own `ioctl`s; or a kernel
command-line option `vibeos.<name>=` (DESIGN §3.2). `docs/LINUX.md` lists each with its format and
reason (ROADMAP, How to read this). The per-process counters show why: Linux already has
`/proc/<pid>/syscall`, the name a syscall count would otherwise take, and appends fields to
`/proc/<pid>/stat`, which tools read by position, so vibeOS's counts are `/proc/<pid>/vibeos/syscalls`
and `/proc/<pid>/vibeos/faults` (ROADMAP §13.9), and `/proc/<pid>/stat` carries only Linux's fields.

The one exception is `psinfo` (500), which predates this rule. Linux has not reached 500. It carries
the per-process syscall count (ROADMAP §10.7) and fault counts (§12.2) until ROADMAP §13.9 deletes it
with the move of `ps` to `procfs`, and it gains no other field. §13.9's syscall-table check fails on a
row whose number the baseline does not define: the numbers are a fact table generated from the
baseline release's kernel.org headers (DESIGN §1.5), and musl's pinned header must agree with it on
every name musl defines.
