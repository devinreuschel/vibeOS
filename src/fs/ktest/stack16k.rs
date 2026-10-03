//! `fat_vda_16k_stack` (kernel_tests only), re-exported from `fs::ktest`:
//! a disk-backed FAT mount and a 64 KiB write on `vda` from a 16 KiB
//! `spawn` stack, with a self-IPI on the virtio-blk vector each time the
//! write path enters the block cache, so a top half lands on the path
//! (ROADMAP §10.4, F058). The worker's depth must leave room for the
//! deepest top half a probe thread on its CPU takes ([`top_half_room`]),
//! wherever an interrupt lands on the path (ROADMAP §10.2).

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use vibeos::arch::InterruptMask;
use vibeos::fs::{FileRef, O_CREAT, O_RDWR, OpenFlags, SeekFrom};
use vibeos::kalloc::TryVec;
use vibeos::sched::stack_depth;

use crate::arch::current::Arch;
use crate::block::blockdev_init;
use crate::file_init;
use crate::ktest::{Outcome, sleep_for};
use crate::thread_init;
use crate::{apic_init, per_cpu_init};

/// The CPU whose block-cache writes send the self-IPI, or `u32::MAX`.
static ARMED_CPU: AtomicU32 = AtomicU32::new(u32::MAX);
/// The virtio-blk vector [`on_cache_write`] sends.
static ARMED_VEC: AtomicU8 = AtomicU8::new(0);
static SELF_IPIS: AtomicU32 = AtomicU32::new(0);
static SELF_IPI_ERRS: AtomicU32 = AtomicU32::new(0);

/// `fat_init`'s `Io::write` on a block device, just before the block-cache
/// call: on the armed CPU with IF=1, send this CPU a virtio-blk interrupt,
/// so its top half runs on top of the write path.
pub(crate) fn on_cache_write() {
    let me = thread_init::current_cpu();
    if ARMED_CPU.load(Ordering::Acquire) != me || !Arch::enabled() {
        return;
    }
    match apic_init::send_ipi_cpu(me, ARMED_VEC.load(Ordering::Relaxed)) {
        Ok(()) => SELF_IPIS.fetch_add(1, Ordering::Relaxed),
        Err(_) => SELF_IPI_ERRS.fetch_add(1, Ordering::Relaxed),
    };
}

/// Sectors of the FAT image the test writes to `vda`: 256 KiB.
const IMG_SECTORS: usize = 512;
const SEC: usize = 512;
/// Bytes the worker writes, and the file offset it writes them at.
const WRITE_BYTES: usize = 64 * 1024;
const WRITE_OFF: u64 = 100;
/// Self-IPIs the write must send at least.
const MIN_IPIS: u32 = 64;
const MNT: &[u8] = b"/fat16k";
const FILE: &[u8] = b"/fat16k/big.bin";

/// The worker's result: 0 until it finishes, then [`OK`] or the step that
/// failed.
static RESULT: AtomicU32 = AtomicU32::new(0);
const OK: u32 = 1;
static DONE: AtomicBool = AtomicBool::new(false);

fn pattern(i: usize) -> u8 {
    (i.wrapping_mul(31) ^ (i >> 9)) as u8
}

/// Format a 256 KiB FAT32 image and write it to `vda` LBA 0 to 511
/// through the block cache the mount reads, then flush.
fn write_image() -> Result<(), &'static str> {
    fat_image_to(b"vda")
}

/// Format a 256 KiB FAT32 image and write it to block device `dev`'s LBA
/// 0 to 511 through the block cache a mount reads, then flush.
pub(super) fn fat_image_to(dev: &[u8]) -> Result<(), &'static str> {
    let r = blockdev_init::lookup(dev).ok_or("no block device")?;
    let mut img: TryVec<u8> =
        TryVec::try_with_capacity(IMG_SECTORS * SEC).map_err(|_| "image alloc")?;
    let zero = [0u8; SEC];
    for _ in 0..IMG_SECTORS {
        img.try_extend_from_slice(&zero)
            .map_err(|_| "image alloc")?;
    }
    vibeos::fat::mkfs(&mut img, b"S45").map_err(|_| "mkfs")?;
    for (i, chunk) in img.chunks(8 * SEC).enumerate() {
        r.write((i * 8) as u64, chunk).map_err(|_| "image write")?;
    }
    r.flush().map_err(|_| "image flush")
}

/// The worker's steps; the first that fails names [`RESULT`].
fn worker_steps(cpu: u32, vec: u8) -> u32 {
    if file_init::mount(b"vda", MNT, b"fat32", false).is_err() {
        return 2;
    }
    let r = write_read(cpu, vec);
    let u = file_init::umount(MNT);
    if r != OK {
        return r;
    }
    if u.is_err() {
        return 9;
    }
    OK
}

fn write_read(cpu: u32, vec: u8) -> u32 {
    let Ok(mut data) = TryVec::<u8>::try_with_capacity(WRITE_BYTES) else {
        return 3;
    };
    for i in 0..WRITE_BYTES {
        if data.try_push(pattern(i)).is_err() {
            return 3;
        }
    }
    let flags = OpenFlags::from_bits(O_CREAT | O_RDWR);
    let Ok(f) = file_init::open(FILE, flags, 0o644) else {
        return 4;
    };
    let r = write_then_read(&f, &mut data, cpu, vec);
    if file_init::close(f).is_err() && r == OK {
        return 10;
    }
    r
}

/// Write `data` at [`WRITE_OFF`] with the hook armed, read it back into
/// `data`, and compare.
fn write_then_read(f: &FileRef, data: &mut TryVec<u8>, cpu: u32, vec: u8) -> u32 {
    if file_init::seek(f, SeekFrom::Start(WRITE_OFF)).is_err() {
        return 5;
    }
    ARMED_VEC.store(vec, Ordering::Relaxed);
    ARMED_CPU.store(cpu, Ordering::Release);
    let w = file_init::write(f, data);
    ARMED_CPU.store(u32::MAX, Ordering::Release);
    if !matches!(w, Ok(n) if n == WRITE_BYTES) {
        return 6;
    }
    if file_init::seek(f, SeekFrom::Start(WRITE_OFF)).is_err() {
        return 5;
    }
    data.fill(0);
    if !matches!(file_init::read(f, data), Ok(n) if n == WRITE_BYTES) {
        return 7;
    }
    if data.iter().enumerate().any(|(i, &b)| b != pattern(i)) {
        return 8;
    }
    OK
}

/// The CPU the worker runs on and the vector it sends.
static WORKER_AT: AtomicU32 = AtomicU32::new(0);

fn fat16k_worker() {
    let at = WORKER_AT.load(Ordering::Acquire);
    let r = worker_steps(at >> 8, at as u8);
    // Release: pairs with the registry's Acquire load of `DONE`.
    RESULT.store(r, Ordering::Relaxed);
    DONE.store(true, Ordering::Release);
}

/// Set to end [`probe_entry`]'s spin with IF on.
static PROBE_STOP: AtomicBool = AtomicBool::new(false);
/// Set while [`top_half_room`] measures the probe's own depth.
static PROBE_IF_OFF: AtomicBool = AtomicBool::new(false);
/// Milliseconds [`top_half_room`] keeps the probe interrupted, sending a
/// virtio-blk top half and a call-function IPI each.
const PROBE_MS: u32 = 200;
/// Milliseconds [`top_half_room`] sleeps with one CPU, while the ticks that
/// switch the two spinners land on the probe.
const PROBE_ALONE_MS: u64 = 20;

/// Bytes [`probe_spin`] puts on its stack before it spins, so the spin
/// sits deeper than anything the probe thread's start does.
const PROBE_PAD: usize = 4096;

/// A probe thread: with [`PROBE_IF_OFF`], spin 1 ms with IF off, so only
/// its own frames touch its stack; otherwise spin with IF on until
/// [`PROBE_STOP`], taking whatever interrupts its CPU gets.
fn probe_entry() {
    if PROBE_IF_OFF.load(Ordering::Acquire) {
        let _irq = crate::arch::current::InterruptGuard::enter();
        probe_spin(true);
    } else {
        probe_spin(false);
    }
}

/// [`PROBE_PAD`] bytes down, spin until 1 ms has passed (`if_off`) or
/// until [`PROBE_STOP`]: the same calls either way, so the two depths
/// differ only by the interrupts that landed on the spin.
#[inline(never)]
fn probe_spin(if_off: bool) {
    let mut pad = [0u8; PROBE_PAD];
    core::hint::black_box(&mut pad);
    let end = crate::time_init::read_tsc().saturating_add(crate::time_init::tsc_per_ms());
    while !probe_done(if_off, end) {
        core::hint::spin_loop();
    }
    core::hint::black_box(&mut pad);
}

/// Whether [`probe_spin`] is over: the TSC past `end` for the IF-off spin,
/// [`PROBE_STOP`] for the other, which reads no TSC, since a TSC read on
/// each pass makes that spin, which lasts the whole probe, crawl under
/// `-icount` (the `test-irqoff` tier). One leaf call either way.
#[inline(never)]
fn probe_done(if_off: bool, end: u64) -> bool {
    if if_off {
        crate::time_init::read_tsc() >= end
    } else {
        PROBE_STOP.load(Ordering::Acquire)
    }
}

fn probe_call(_: *mut ()) {}

/// Run [`probe_entry`] as thread `name` on `cpu` and return its stack
/// depth at exit.
fn probe_depth(name: &'static str, cpu: u32) -> Result<usize, &'static str> {
    let h = thread_init::spawn_on(name, probe_entry, cpu).map_err(|_| "probe spawn")?;
    crate::sched::ktest::wait_exit_depth(h.id().0).ok_or("no probe depth recorded")
}

/// The stack the deepest interrupt takes: the depth of a probe thread whose
/// spin takes interrupts, less its depth with IF off ([`probe_spin`]). It runs on a CPU other
/// than the registry's when there is one, since `ipi_init::call_mask` sends
/// the calling CPU nothing. While it spins, a sibling spinner on its CPU
/// makes the ticks switch between them, which puts the scheduler's and the
/// switch tail's frames under a tick's, and each millisecond the registry
/// sends that CPU the virtio-blk vector `vec`, the disk's top half, and a
/// call-function IPI. With one CPU those would land on the registry, so it
/// only sleeps while the ticks land on the spinners.
fn top_half_room(vec: u8) -> Result<usize, &'static str> {
    let cpu = crate::ktest::second_cpu().unwrap_or_else(thread_init::current_cpu);
    PROBE_IF_OFF.store(true, Ordering::Release);
    let own = probe_depth("probe-own", cpu)?;
    PROBE_IF_OFF.store(false, Ordering::Release);
    PROBE_STOP.store(false, Ordering::Release);
    let probe = thread_init::spawn_on("probe-irq", probe_entry, cpu).map_err(|_| "probe spawn")?;
    let spinner = thread_init::spawn_on("probe-spin", probe_entry, cpu);
    let alone = cpu == thread_init::current_cpu();
    let mut sent = 0u32;
    if spinner.is_ok() && alone {
        // With one CPU an IPI this thread sends lands on this thread, not
        // the probe, and each 1 ms round waits on the spinners' ticks.
        thread_init::sleep_ms(PROBE_ALONE_MS);
    } else if spinner.is_ok() {
        while sent < PROBE_MS {
            if apic_init::send_ipi_cpu(cpu, vec).is_err() {
                break;
            }
            crate::ipi_init::call_cpu(cpu, probe_call, core::ptr::null_mut(), true);
            thread_init::sleep_ms(1);
            sent += 1;
        }
    }
    PROBE_STOP.store(true, Ordering::Release);
    let deep = crate::sched::ktest::wait_exit_depth(probe.id().0);
    if spinner.is_err() {
        return Err("probe spinner spawn");
    }
    if !alone && sent < PROBE_MS {
        return Err("probe self-IPI failed");
    }
    Ok(deep.ok_or("no probe depth recorded")?.saturating_sub(own))
}

/// Opt-in, in its own boot on a fresh disk: see the module doc. Fails
/// unless the mount, the 64 KiB unaligned write, the read-back and the
/// unmount succeed, at least 64 self-IPIs were sent with no error, the
/// virtio-blk top half ran at least that often more, and the worker's
/// recorded depth plus the deepest top half a probe on its CPU took
/// ([`top_half_room`]) is within its 16 KiB stack's 12 KiB budget.
pub(crate) fn fat_vda_16k_stack() -> Outcome {
    let Some((cpu, vec)) = (0..64u32)
        .filter(|&c| per_cpu_init::is_online(c))
        .find_map(|c| {
            crate::drivers::ktest::vda(|b| b.queue_vector(c))
                .flatten()
                .map(|v| (c, v))
        })
    else {
        return Outcome::Fail("no virtio-blk queue vector");
    };
    let room = match top_half_room(vec) {
        Ok(r) => r,
        Err(why) => return Outcome::Fail(why),
    };
    if let Err(why) = write_image() {
        return Outcome::Fail(why);
    }
    if file_init::mkdir(MNT, 0o755).is_err() {
        return Outcome::Fail("mkdir");
    }
    SELF_IPIS.store(0, Ordering::Relaxed);
    SELF_IPI_ERRS.store(0, Ordering::Relaxed);
    DONE.store(false, Ordering::Relaxed);
    RESULT.store(0, Ordering::Relaxed);
    WORKER_AT.store((cpu << 8) | u32::from(vec), Ordering::Release);
    let top0 = crate::drivers::ktest::top_hits();
    // `spawn_on` gives `spawn`'s 16 KiB default stack.
    let h = match thread_init::spawn_on("fat16k", fat16k_worker, cpu) {
        Ok(h) => h,
        Err(_) => return Outcome::Fail("spawn"),
    };
    if !sleep_for(|| DONE.load(Ordering::Acquire)) {
        ARMED_CPU.store(u32::MAX, Ordering::Release);
        return Outcome::Fail("worker did not finish by the run's deadline");
    }
    let r = RESULT.load(Ordering::Relaxed);
    if r != OK {
        return crate::fail_fmt!("worker step {} failed", r);
    }
    let ipis = SELF_IPIS.load(Ordering::Relaxed);
    let errs = SELF_IPI_ERRS.load(Ordering::Relaxed);
    let tops = crate::drivers::ktest::top_hits().wrapping_sub(top0);
    let depth = crate::sched::ktest::wait_exit_depth(h.id().0);
    crate::ktest_info!(
        "cpu {} vector {:#x}: {} self-IPIs, {} errors, {} top halves, depth {:?}, top half room {}",
        cpu,
        vec,
        ipis,
        errs,
        tops,
        depth,
        room
    );
    if errs != 0 {
        return crate::fail_fmt!("{} self-IPI errors", errs);
    }
    if ipis < MIN_IPIS {
        return crate::fail_fmt!("{} self-IPIs < {}", ipis, MIN_IPIS);
    }
    if tops < ipis {
        return crate::fail_fmt!("{} top halves < {} self-IPIs", tops, ipis);
    }
    let budget = stack_depth::budget(16 * 1024);
    match depth {
        None => Outcome::Fail("no exit depth recorded"),
        Some(d) if d.saturating_add(room) > budget => crate::fail_fmt!(
            "worker used {d} bytes, and a top half {room} more is over the {budget}-byte budget"
        ),
        Some(_) => Outcome::Ok,
    }
}
