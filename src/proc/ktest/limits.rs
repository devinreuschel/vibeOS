//! The tests of full tables (ROADMAP §10.4): each table at its `limits`
//! length, and one entry more.

use core::sync::atomic::{AtomicUsize, Ordering};

use vibeos::acpi::MAX_CPUS;
use vibeos::fs::VfsSizes;
use vibeos::limits::{
    MAX_DENTRIES, MAX_FDS, MAX_INODES, MAX_MOUNTS, MAX_OPEN_FILES, MAX_PROCS, MAX_REGIONS,
    MAX_THREADS,
};
use vibeos::proc::{wait_exited, wexitstatus, wifexited};

use crate::addr_space_init;
use crate::fs_init;
use crate::ktest::user::{self, DEFAULT, Image, user_code};
use crate::ktest::{
    FrameCount, Outcome, quiesce, quiescent_free_frames, spawn_thread_on, spin_until_ns,
};
use crate::per_cpu_init;
use crate::proc_init;
use crate::sched::ktest::fill_threads;
use crate::thread_init::{self, SpawnError};
use crate::user_init;

// fork(): exit 0 when it returns -EAGAIN, 1 for any other error or a pid;
// a child exits 2.
user_code!(
    FORK_EAGAIN,
    "
    mov eax, 57
    syscall
    mov edi, 2
    test rax, rax
    jz 1f
    xor edi, edi
    cmp rax, -11
    je 1f
    mov edi, 1
1:
    mov eax, 60
    syscall
    ud2
    "
);

/// Run [`FORK_EAGAIN`] and check its fork returned `-EAGAIN`.
fn fork_once() -> Result<(), Outcome> {
    let st = match user::run(&Image::Code(FORK_EAGAIN, DEFAULT), &["fork-full"]) {
        Ok(st) => st,
        Err(e) => return Err(crate::fail_fmt!("spawn: {}", e.as_str())),
    };
    match st {
        s if s == wait_exited(0) => Ok(()),
        s if s == wait_exited(2) => Err(Outcome::Fail("fork made a child on a full table")),
        s => Err(crate::fail_fmt!(
            "status {s:#x}, want exited 0 (fork gave EAGAIN)"
        )),
    }
}

/// `fork` on a full thread table returns `-EAGAIN` (ROADMAP §10.4, F037):
/// with all but one slot held by parked kernel threads, the process's own
/// thread takes the last one, and its fork finds none. What the fork took
/// (a pid and proc slot, a cloned address space, descriptor references) is
/// all given back while the fillers still hold the table.
pub(crate) fn fork_full_thread_table() -> Outcome {
    if !quiesce() {
        return Outcome::Fail("threads did not settle");
    }
    let fill = fill_threads(1);
    let r = fork_full_filled();
    if !fill.release() {
        return Outcome::Fail("fillers did not exit");
    }
    match r {
        Ok(()) => Outcome::Ok,
        Err(o) => o,
    }
}

fn fork_full_filled() -> Result<(), Outcome> {
    let (used, cap) = thread_init::table_usage();
    if used + 1 != cap {
        return Err(crate::fail_fmt!("fill left {used} of {cap} slots used"));
    }
    // The first run warms what a process start maps for good.
    fork_once()?;
    let frames0 = quiescent_free_frames();
    let (procs0, _) = proc_init::table_usage();
    fork_once()?;
    let frames1 = quiescent_free_frames();
    let (procs1, _) = proc_init::table_usage();
    if procs1 != procs0 {
        return Err(crate::fail_fmt!("proc slots used {procs0} -> {procs1}"));
    }
    if frames1 != frames0 {
        return Err(crate::fail_fmt!("frames {frames0} -> {frames1}"));
    }
    Ok(())
}

// Fork until fork fails; each child exits 0 at once and stays a zombie.
// The failure must be -EAGAIN; then reap every child with wait4(-1) until
// -ECHILD and exit with the fork count. Anything else, or twice
// MAX_PROCS forks, is a ud2.
user_code!(
    PROC_FILL,
    "
    xor r12d, r12d
1:
    cmp r12d, 512
    jae 9f
    mov eax, 57
    syscall
    test rax, rax
    jz 5f
    js 2f
    inc r12d
    jmp 1b
2:
    cmp rax, -11
    jne 9f
3:
    mov edi, -1
    xor esi, esi
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    test rax, rax
    jg 3b
    cmp rax, -10
    jne 9f
    mov edi, r12d
    mov eax, 60
    syscall
    ud2
5:
    xor edi, edi
    mov eax, 60
    syscall
    ud2
9:
    ud2
    "
);

// Open /dev/null until -EMFILE and exit with the count; any other error,
// or twice MAX_FDS opens, is a ud2.
user_code!(
    FD_FILL,
    "
    xor r12d, r12d
1:
    cmp r12d, 512
    jae 9f
    lea rdi, [rip + 8f]
    xor esi, esi
    xor edx, edx
    mov eax, 2
    syscall
    test rax, rax
    js 2f
    inc r12d
    jmp 1b
2:
    cmp rax, -24
    jne 9f
    mov edi, r12d
    mov eax, 60
    syscall
9:
    ud2
8:
    .asciz \"/dev/null\"
    "
);

// mmap one anonymous page at a time, PROT_READ and PROT_READ|PROT_WRITE in
// turn so no two regions merge, until -ENOMEM, and exit with the count; any
// other error, or twice MAX_REGIONS maps, is a ud2.
user_code!(
    MAP_FILL,
    "
    xor r12d, r12d
1:
    cmp r12d, 512
    jae 9f
    xor edi, edi
    mov esi, 4096
    mov edx, 1
    test r12d, 1
    jz 3f
    mov edx, 3
3:
    mov r10d, 0x22
    mov r8, -1
    xor r9d, r9d
    mov eax, 9
    syscall
    cmp rax, -4096
    ja 2f
    inc r12d
    jmp 1b
2:
    cmp rax, -12
    jne 9f
    mov edi, r12d
    mov eax, 60
    syscall
9:
    ud2
    "
);

/// Each online CPU's run-queue capacity, as its own probe thread read it.
static RUNQ_CAP: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static PROBES: AtomicUsize = AtomicUsize::new(0);

fn runq_probe() {
    let (cpu, cap) = per_cpu_init::with_current(|c| (c.cpu_id as usize, c.runq.capacity()));
    if let Some(r) = RUNQ_CAP.get(cpu) {
        r.store(cap, Ordering::Release);
    }
    PROBES.fetch_add(1, Ordering::AcqRel);
}

/// (a): every table's capacity is its `limits` length.
fn capacities() -> Result<(), Outcome> {
    let (_, threads) = thread_init::table_usage();
    let timeouts = thread_init::timeouts_capacity();
    let (_, procs) = proc_init::table_usage();
    let fds = proc_init::fd_row_capacity();
    let vfs = fs_init::with(|v| v.sizes());
    if threads != MAX_THREADS || timeouts != MAX_THREADS {
        return Err(crate::fail_fmt!("threads {threads}, timeouts {timeouts}"));
    }
    if procs != MAX_PROCS || fds != MAX_FDS {
        return Err(crate::fail_fmt!("procs {procs}, fds {fds}"));
    }
    let want = VfsSizes {
        inodes: MAX_INODES,
        dentries: MAX_DENTRIES,
        mounts: MAX_MOUNTS,
        files: MAX_OPEN_FILES,
    };
    if vfs != want {
        return Err(crate::fail_fmt!("vfs {vfs:?}"));
    }
    let Ok(space) = addr_space_init::create() else {
        return Err(Outcome::Fail("no memory for an address space"));
    };
    let regions = space.mm().region_capacity();
    drop(space);
    if regions != MAX_REGIONS {
        return Err(crate::fail_fmt!("regions {regions}"));
    }
    RUNQ_CAP.iter().for_each(|r| r.store(0, Ordering::Relaxed));
    PROBES.store(0, Ordering::Release);
    let online = per_cpu_init::online_mask();
    let mut n = 0usize;
    for cpu in (0..MAX_CPUS as u32).filter(|&c| online & (1u64 << c) != 0) {
        spawn_thread_on("runq-probe", runq_probe, cpu);
        n += 1;
    }
    if !spin_until_ns(|| PROBES.load(Ordering::Acquire) == n, 5_000_000_000) {
        return Err(Outcome::Fail("run-queue probes did not run"));
    }
    for (cpu, r) in RUNQ_CAP.iter().enumerate() {
        let cap = r.load(Ordering::Acquire);
        if online & (1u64 << cpu) != 0 && cap != MAX_THREADS {
            return Err(crate::fail_fmt!("cpu{cpu} runq {cap}"));
        }
    }
    Ok(())
}

/// (b): the thread table fills to `MAX_THREADS`, and one spawn more is
/// `Err(NoSlot)`.
fn threads_full() -> Result<(), Outcome> {
    let fill = fill_threads(0);
    let usage = thread_init::table_usage();
    let last = fill.last;
    if !fill.release() {
        return Err(Outcome::Fail("fillers did not exit"));
    }
    if usage != (MAX_THREADS, MAX_THREADS) {
        return Err(crate::fail_fmt!("full table usage {usage:?}"));
    }
    if last != Some(SpawnError::NoSlot) {
        return Err(crate::fail_fmt!("spawn past the table: {last:?}"));
    }
    Ok(())
}

/// Run `img` and return its exit code; a signal or a load error fails.
fn exit_code(img: &Image, name: &str) -> Result<u32, Outcome> {
    let st = user::run(img, &[name]).map_err(|e| crate::fail_fmt!("{name}: {}", e.as_str()))?;
    if !wifexited(st) {
        return Err(crate::fail_fmt!("{name}: status {st:#x}"));
    }
    Ok(wexitstatus(st))
}

/// (c), (d) and (e): the process table fills to `MAX_PROCS` and one fork
/// more is `-EAGAIN`; a descriptor row fills to `MAX_FDS` and one open more
/// is `-EMFILE`; the region table fills to `MAX_REGIONS` and one `mmap`
/// more is `-ENOMEM`.
fn user_tables_full() -> Result<(), Outcome> {
    let (used, _) = proc_init::table_usage();
    let forks = exit_code(&Image::Code(PROC_FILL, DEFAULT), "proc-fill")? as usize;
    // The filler's own slot and the `used` slots before it hold the rest.
    if forks + used + 1 != MAX_PROCS {
        return Err(crate::fail_fmt!(
            "{forks} forks with {used} slots used before"
        ));
    }
    let opens = exit_code(&Image::Code(FD_FILL, DEFAULT), "fd-fill")? as usize;
    // fds 0 to 2 are the console.
    if opens + 3 != MAX_FDS {
        return Err(crate::fail_fmt!("{opens} opens"));
    }
    let map = Image::Code(MAP_FILL, DEFAULT);
    let base = match user_init::load_image(&user::elf_bytes(&map), &[b"map-fill"]) {
        Ok(l) => {
            // The space's last `users` put tears it down.
            l.space.mm().regions().count()
        }
        Err(e) => return Err(crate::fail_fmt!("load: {}", e.as_str())),
    };
    let maps = exit_code(&map, "map-fill")? as usize;
    if maps + base != MAX_REGIONS {
        return Err(crate::fail_fmt!("{maps} maps over {base} image regions"));
    }
    Ok(())
}

/// Every table is its `limits` length in the default guest, and filling
/// the thread, process, descriptor and region tables ends in the error
/// Linux returns: `Err(NoSlot)` from a kernel-thread spawn, `-EAGAIN` from
/// `fork`, `-EMFILE` from `open` and `-ENOMEM` from `mmap` (ROADMAP §10.4,
/// D1). A first pass warms what a full table allocates for good (TCB boxes,
/// heap), and the second leaves frames and table use where it found them.
/// Warm-up passes [`limits_heap_backed`] makes at most before its counted
/// one.
const WARM_PASSES: usize = 4;

pub(crate) fn limits_heap_backed() -> Outcome {
    if let Err(o) = capacities() {
        return o;
    }
    let pass = || -> Result<(), Outcome> {
        threads_full()?;
        user_tables_full()
    };
    // Warm-up passes until one leaves the kernel heap as it found it: the
    // heap never shrinks, and while first-fit settles the holes a pass
    // leaves, the next can still grow it (3 pages when this test runs
    // alone). The counted pass then grows it only for a leak.
    let heap = || crate::heap_init::stats().capacity;
    let mut cap = heap();
    for _ in 0..WARM_PASSES {
        if let Err(o) = pass() {
            return o;
        }
        if !quiesce() {
            return Outcome::Fail("threads did not settle");
        }
        let now = heap();
        if now == cap {
            break;
        }
        cap = now;
    }
    let frames = FrameCount::quiescent();
    let threads = thread_init::table_usage();
    let procs = proc_init::table_usage();
    if let Err(o) = pass() {
        return o;
    }
    let now = FrameCount::quiescent();
    if thread_init::table_usage() != threads || proc_init::table_usage() != procs {
        return Outcome::Fail("table use did not come back");
    }
    frames.unchanged(&now)
}
