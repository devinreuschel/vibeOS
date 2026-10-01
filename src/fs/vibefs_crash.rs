//! The `vibefs_crash` build's write loop (docs/VIBEFS.md §12), which a
//! QEMU kill interrupts. Only that build declares this module.

use crate::vibefs_init;
use vibeos::vibefs::Plant;

/// QEMU-kill workload. Marker `vibeOS: vibefs: wr N` is not a boot
/// contract line. Printed *before* the write+fsync so a kill can land
/// inside `write` / `Flush` (docs/VIBEFS.md §12).
/// The `vibefs_crash` build's workload (VIBEFS §12 item 2, ROADMAP §10.2).
/// Mounts `vda` at `/crash`, commits iteration 0, prints `crash-ready`, then
/// for each N from 1 prints `wr N` and commits iteration N: `/crash/w` with
/// `O_TRUNC`, 300 bytes of `(N + k) as u8`, close, `sync_fs`. Any error
/// prints a registered failure line and halts, so the harness sees it.
pub fn crash_loop() -> ! {
    use crate::arch::current::halt;
    use crate::file_init;

    /// One committed iteration of `crash_loop`. The error is the text its
    /// failure line carries.
    fn crash_iter(n: u32) -> Result<(), &'static str> {
        use crate::file_init;
        use vibeos::fs::{O_CREAT, O_RDWR, O_TRUNC, OpenFlags};

        let flags = OpenFlags::from_bits(O_RDWR | O_CREAT | O_TRUNC);
        let f = file_init::open(b"/crash/w", flags, 0o644).map_err(|e| e.as_str())?;
        let mut buf = [0u8; 300];
        for (k, b) in buf.iter_mut().enumerate() {
            *b = n.wrapping_add(k as u32) as u8;
        }
        let wrote = file_init::write(&f, &buf);
        let closed = file_init::close(f);
        match wrote {
            Ok(w) if w == buf.len() => {}
            Ok(_) => return Err("short write"),
            Err(e) => return Err(e.as_str()),
        }
        closed.map_err(|e| e.as_str())?;
        file_init::sync_fs().map_err(|e| e.as_str())
    }

    // `vibeos.crash_plant=leak|early_super` plants one commit defect, so
    // the crash test shows it can fail (BOOT.md §3.2).
    let plant = match crate::boot::cmdline().get("vibeos.crash_plant") {
        None => Plant::None,
        Some(b"leak") => Plant::Leak,
        Some(b"early_super") => Plant::EarlySuper,
        Some(_) => {
            crate::marker!("vibeOS: vibefs: bad crash_plant");
            halt();
        }
    };
    if let Err(e) = file_init::mkdir(b"/crash", 0o755)
        .and_then(|()| vibefs_init::mount_dev("vda", "/crash", false).map(|_| ()))
        .and_then(|()| vibefs_init::set_plant(b"/crash", plant))
    {
        crate::marker!("vibeOS: vibefs: mount fail {}", e.as_str());
        halt();
    }
    if let Err(e) = crash_iter(0) {
        crate::marker!("vibeOS: vibefs: sync fail {e}");
        halt();
    }
    crate::marker!("vibeOS: vibefs: crash-ready");
    let mut n = 1u32;
    loop {
        crate::marker!("vibeOS: vibefs: wr {n}");
        if let Err(e) = crash_iter(n) {
            crate::marker!("vibeOS: vibefs: sync fail {e}");
            halt();
        }
        n = n.wrapping_add(1);
        crate::thread_init::yield_now();
    }
}
