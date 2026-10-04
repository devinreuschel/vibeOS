//! aarch64 virtio-mmio and `/dev/random` in-guest tests.

use crate::file_init;
use crate::fs_init;
use crate::ktest::{Outcome, spin_until_ns};
use crate::thread_init;
use crate::virtio_init;

#[cfg(target_arch = "aarch64")]
fn mmio_blk<R>(f: impl FnOnce(&crate::virtio_blk_init::VirtioBlk) -> R) -> Option<R> {
    crate::virtio_blk_init::with_mmio_disk(f)
}

/// I/O from every online CPU over virtio-mmio (ROADMAP §11.5, F047).
#[cfg(target_arch = "aarch64")]
pub(crate) fn test_block_vblk_mmio_smp() -> Outcome {
    use core::sync::atomic::{AtomicU32, Ordering as Ord};
    static DONE: AtomicU32 = AtomicU32::new(0);
    static FAIL: AtomicU32 = AtomicU32::new(0);
    static WID: AtomicU32 = AtomicU32::new(0);

    if mmio_blk(|b| b.live()).is_none() {
        return Outcome::Skip("no virtio-mmio blk");
    }
    let nq = mmio_blk(|b| b.num_queues()).unwrap_or(0);
    let mask = crate::per_cpu_init::online_mask();
    let cpus = mask.count_ones();
    if cpus >= 4 && nq < 4 {
        return Outcome::Fail("nq < 4 at smp 4");
    }
    DONE.store(0, Ord::SeqCst);
    FAIL.store(0, Ord::SeqCst);
    WID.store(0, Ord::SeqCst);
    fn worker() {
        let id = WID.fetch_add(1, Ord::SeqCst);
        let lba = 64 + u64::from(id) * 8;
        let mut buf = [0u8; 512];
        let mut i = 0usize;
        while i < 512 {
            buf[i] = (id as u8).wrapping_add(i as u8);
            i += 1;
        }
        let w = mmio_blk(|b| b.write(lba, &buf)).unwrap_or(Err(vibeos::block::BlockError::Gone));
        let mut out = [0u8; 512];
        let r = mmio_blk(|b| b.read(lba, &mut out)).unwrap_or(Err(vibeos::block::BlockError::Gone));
        if w.is_err() || r.is_err() || out != buf {
            FAIL.fetch_add(1, Ord::SeqCst);
        }
        DONE.fetch_add(1, Ord::SeqCst);
    }
    let mut cpu = 0u32;
    let mut spawned = 0u32;
    while cpu < 64 {
        if mask & (1u64 << cpu) != 0 {
            if thread_init::spawn_on("vblk-mmio", worker, cpu).is_err() {
                return Outcome::Fail("spawn");
            }
            spawned += 1;
        }
        cpu += 1;
    }
    let t0 = crate::time_init::uptime_ms();
    while DONE.load(Ord::SeqCst) < spawned {
        if crate::time_init::uptime_ms().saturating_sub(t0) > 15_000 {
            return Outcome::Fail("stall");
        }
        thread_init::yield_now();
    }
    if FAIL.load(Ord::SeqCst) != 0 {
        return Outcome::Fail("io");
    }
    Outcome::Ok
}

#[cfg(target_arch = "aarch64")]
pub(crate) fn test_dev_random_source() -> Outcome {
    if !virtio_init::rng_bound() && crate::arch::current::hw_rng64().is_none() {
        return Outcome::Skip("no entropy");
    }
    if !fs_init::live() {
        return Outcome::Fail("not live");
    }
    let Ok(f) = file_init::open(
        b"/dev/random",
        vibeos::fs::OpenFlags::from_bits(vibeos::fs::O_RDWR),
        0,
    ) else {
        return Outcome::Fail("open");
    };
    let mut buf = [0u8; 16];
    let t0 = crate::time_init::now_ns();
    let n = loop {
        match file_init::read(&f, &mut buf) {
            Err(vibeos::fs::FsError::Again)
                if crate::time_init::now_ns().saturating_sub(t0) < 2_000_000_000 =>
            {
                spin_until_ns(|| false, 1_000_000);
            }
            r => break r,
        }
    };
    #[expect(
        clippy::let_underscore_must_use,
        reason = "close after the read the test already scored: a leaked FileRef is a counter, which nothing can act on (DESIGN §2.5)"
    )]
    let _ = file_init::close(f);
    match n {
        Ok(1..=16) => {}
        Ok(_) => return Outcome::Fail("read count"),
        Err(_) => return Outcome::Fail("read"),
    }
    match vibeos::entropy::last_source() {
        Some(_) => Outcome::Ok,
        None => Outcome::Fail("no source"),
    }
}
