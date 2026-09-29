//! In-guest tests for console (kernel_tests only). Rows: the list in crate::ktest.

mod hooks;

pub(crate) use hooks::*;

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::console::BackendId;
use vibeos::fb::PIECE_BYTES;

use crate::fb_init::{self, testing as fb_testing};
use crate::ktest::user::{self, Image, Layout, user_code};
use crate::ktest::{Outcome, cpu_remote, free_frames_owned, sleep_until_s19, spin_until};
use crate::kva_init;
use crate::proc_init::testing as proc_testing;
use crate::x86;

pub(crate) fn test_fb_bgrx_roundtrip() -> Outcome {
    if !crate::fb_init::ready() {
        return Outcome::Fail("no framebuffer");
    }
    let color = vibeos::fb::pack_bgrx(0x11, 0x22, 0x33);
    if !put_pixel(0, 0, color) {
        return Outcome::Fail("put origin");
    }
    match get_pixel(0, 0) {
        Some(got) if got == color => Outcome::Ok,
        Some(_) => Outcome::Fail("pixel mismatch"),
        None => Outcome::Fail("get origin"),
    }
}

pub(crate) fn test_fb_pitch() -> Outcome {
    let Some(pitch) = pitch() else {
        return Outcome::Fail("no pitch");
    };
    let Some(width) = width() else {
        return Outcome::Fail("no width");
    };
    // Must not assume pitch == width*4. QEMU often equals; still use pitch.
    if pitch < (width as u64) * 4 {
        return Outcome::Fail("pitch smaller than width*4");
    }
    let color = vibeos::fb::pack_bgrx(0x44, 0x55, 0x66);
    if !put_pixel(0, 1, color) {
        return Outcome::Fail("put row1");
    }
    match get_pixel(0, 1) {
        Some(got) if got == color => Outcome::Ok,
        Some(_) => Outcome::Fail("row1 mismatch"),
        None => Outcome::Fail("get row1"),
    }
}

pub(crate) fn test_fb_cr_home() -> Outcome {
    if !crate::fb_init::ready() {
        return Outcome::Fail("no framebuffer");
    }
    crate::fb_init::write(b"\n");
    let Some((col, row)) = cursor() else {
        return Outcome::Fail("no cursor");
    };
    if col != 0 {
        return Outcome::Fail("newline not col0");
    }
    crate::fb_init::write(b"X");
    let mut hit: Option<(u32, u32)> = None;
    let mut gy = 0u8;
    while gy < vibeos::font::FONT_H as u8 && hit.is_none() {
        let mut gx = 0u8;
        while gx < vibeos::font::FONT_W as u8 {
            if vibeos::font::glyph_pixel(b'X', gx, gy) {
                hit = Some((gx as u32, gy as u32));
                break;
            }
            gx += 1;
        }
        gy += 1;
    }
    let Some((gx, gy)) = hit else {
        return Outcome::Fail("X glyph empty");
    };
    let (ox, oy) = vibeos::fb::glyph_origin(0, row);
    let Some(lit) = get_pixel(ox + gx, oy + gy) else {
        return Outcome::Fail("get lit");
    };
    crate::fb_init::write(b"\r ");
    match get_pixel(ox + gx, oy + gy) {
        Some(after) if after != lit => Outcome::Ok,
        Some(_) => Outcome::Fail("CR did not home"),
        None => Outcome::Fail("get after"),
    }
}

pub(crate) fn test_kbd_gsi_unmasked() -> Outcome {
    if pic_fallback() {
        if crate::apic_init::owns_tick() {
            return Outcome::Fail("pic fallback after pic masked");
        }
        return Outcome::Skip("pic fallback");
    }
    let Some(gsi) = gsi() else {
        return Outcome::Fail("no keyboard gsi");
    };
    match crate::arch::ktest::gsi_masked(gsi) {
        Some(false) => Outcome::Ok,
        Some(true) => Outcome::Fail("keyboard gsi still masked"),
        None => Outcome::Fail("gsi not on ioapic"),
    }
}

pub(crate) fn test_kbd_8042_clock() -> Outcome {
    if !kbd_live() {
        return Outcome::Fail("kbd not live");
    }
    let Some(cfg) = read_cfg() else {
        return Outcome::Fail("cfg read failed");
    };
    if !vibeos::kbd::cfg_clock1_on(cfg) {
        return Outcome::Fail("clock1 disabled");
    }
    if !vibeos::kbd::cfg_int1_on(cfg) {
        return Outcome::Fail("int1 off");
    }
    Outcome::Ok
}

/// 0xD2 → IRQ1 → decoder → PS/2 ring. Serial mux cannot satisfy this.
/// Device clock is `kbd_8042_clock` / sendkey.
pub(crate) fn test_kbd_ps2_irq() -> Outcome {
    if !kbd_live() {
        return Outcome::Fail("kbd not live");
    }
    let mut n = 64u32;
    while n > 0 && crate::console_init::read().is_some() {
        n -= 1;
    }
    if !inject_scancode(0x1E) {
        return Outcome::Fail("0xD2 inject");
    }
    let t0 = crate::time_init::now_us();
    loop {
        if let Some(vibeos::kbd::DecodedKey::Char(b'a')) = crate::kbd_init::pop() {
            return Outcome::Ok;
        }
        if crate::time_init::now_us().saturating_sub(t0) > 50_000 {
            return Outcome::Fail("no irq key");
        }
        core::hint::spin_loop();
    }
}

pub(crate) fn test_console_mux() -> Outcome {
    use vibeos::console::BackendId;
    if !live() {
        return Outcome::Fail("mux not live");
    }
    if !enabled(BackendId::Serial) {
        return Outcome::Fail("serial off");
    }
    if crate::fb_init::ready() && !enabled(BackendId::Framebuffer) {
        return Outcome::Fail("fb off");
    }
    crate::console_init::write(b"");
    set_enabled(BackendId::Framebuffer, false);
    if enabled(BackendId::Framebuffer) {
        set_enabled(BackendId::Framebuffer, true);
        return Outcome::Fail("disable failed");
    }
    set_enabled(BackendId::Framebuffer, true);
    if crate::fb_init::ready() && !enabled(BackendId::Framebuffer) {
        return Outcome::Fail("re-enable failed");
    }
    Outcome::Ok
}

pub(crate) fn test_kbd_ring_drain() -> Outcome {
    if !kbd_live() {
        return Outcome::Fail("kbd not live");
    }
    push_for_test(vibeos::kbd::DecodedKey::Char(b'q'));
    match crate::console_init::read() {
        Some(vibeos::kbd::DecodedKey::Char(b'q')) => Outcome::Ok,
        Some(_) => Outcome::Fail("wrong key"),
        None => Outcome::Fail("ring empty"),
    }
}

// Fill the 4096 bytes past the code page with '\n', write(1, them, 4096),
// then getpid (the done flag); exit 0 when the write returned 4096.
user_code!(
    S19_NEWLINES,
    "
    mov rdi, 0x40001000
    mov ecx, 4096
    mov al, 10
    rep stosb
    mov edi, 1
    mov rsi, 0x40001000
    mov edx, 4096
    mov eax, 1
    syscall
    mov r12, rax
    mov eax, 39
    syscall
    xor edi, edi
    cmp r12, 4096
    setne dil
    mov eax, 60
    syscall
    ud2
    "
);

const NEWLINES_LAYOUT: Layout = Layout {
    vaddr: 0x4000_0000,
    memsz: Some(0x2000),
    writable: true,
};

/// Turns the serial backend of `console_init::write` off, and back to
/// what it was on drop, so a test's big write reaches only the grid.
struct SerialOff(bool);

impl SerialOff {
    fn new() -> Self {
        let was = enabled(BackendId::Serial);
        set_enabled(BackendId::Serial, false);
        SerialOff(was)
    }
}

impl Drop for SerialOff {
    fn drop(&mut self) {
        set_enabled(BackendId::Serial, self.0);
    }
}

/// The framebuffer console is up and `console_init::write` reaches it.
fn fb_console_on() -> bool {
    fb_init::ready() && enabled(BackendId::Framebuffer)
}

static NL_PIDS: AtomicU64 = AtomicU64::new(0);

static NL_SPAWNED: AtomicBool = AtomicBool::new(false);

/// 0 while watching, 1 when CPU 0 ticked during the write, 2 when the
/// write was done first, 3 when no grid hold came.
static NL_TICK: AtomicU32 = AtomicU32::new(0);

fn nl_spawner() {
    let pid = match user::spawn(&Image::Code(S19_NEWLINES, NEWLINES_LAYOUT), &["newlines"]) {
        Ok(pid) => u64::from(pid),
        Err(_) => u64::MAX,
    };
    NL_PIDS.store(pid, Ordering::Relaxed);
    NL_SPAWNED.store(true, Ordering::Release);
}

/// On the second CPU: CPU 0's tick count after the write's first grid
/// hold, then whether it advances before the program's `getpid` after
/// the write.
fn nl_watcher() {
    let ticks = || cpu_remote(0).map_or(0, |c| c.ticks.load(Ordering::Relaxed));
    let started = spin_until(
        || fb_testing::grid_holds() >= 1 && NL_SPAWNED.load(Ordering::Acquire),
        10_000_000_000,
    );
    let t0 = ticks();
    let Ok(pid) = u32::try_from(NL_PIDS.load(Ordering::Relaxed)) else {
        NL_TICK.store(3, Ordering::Release);
        return;
    };
    if !started {
        NL_TICK.store(3, Ordering::Release);
        return;
    }
    // The program calls getpid only after its write returns.
    let base = proc_testing::getpid_count(pid);
    let done = || proc_testing::getpid_count(pid) > base;
    let ticked = spin_until(|| ticks() > t0 || done(), 30_000_000_000);
    let r = if ticked && ticks() > t0 && !done() {
        1
    } else {
        2
    };
    NL_TICK.store(r, Ordering::Release);
}

/// A 4096-newline console `write` from ring 3 on CPU 0 holds IF=0 only
/// per chunk and per redraw piece: CPU 0 ticks during it, each hold draws
/// at most 16 KiB, and each chunk scrolls at most once (ROADMAP §10.6,
/// F044). A write with IF off leaves its redraw to the next write.
pub(crate) fn test_console_write_newlines() -> Outcome {
    let Some(other) = crate::ktest::second_cpu() else {
        return Outcome::Skip("needs 2 CPUs");
    };
    if !fb_console_on() {
        return Outcome::Fail("no framebuffer console");
    }
    NL_PIDS.store(u64::MAX, Ordering::Relaxed);
    NL_SPAWNED.store(false, Ordering::Release);
    NL_TICK.store(0, Ordering::Release);
    let st = {
        let _serial = SerialOff::new();
        fb_testing::reset();
        crate::ktest::spawn_thread_on("s19_nl_watch", nl_watcher, other);
        crate::ktest::spawn_thread_on("s19_nl_spawn", nl_spawner, 0);
        if !sleep_until_s19(|| NL_SPAWNED.load(Ordering::Acquire), 5_000) {
            return Outcome::Fail("spawner did not run");
        }
        let Ok(pid) = u32::try_from(NL_PIDS.load(Ordering::Relaxed)) else {
            return Outcome::Fail("spawn");
        };
        user::wait(pid)
    };
    if !sleep_until_s19(|| NL_TICK.load(Ordering::Acquire) != 0, 35_000) {
        return Outcome::Fail("watcher did not finish");
    }
    if st != 0 {
        return crate::fail_fmt!("newline program status {st:#x}, want 0");
    }
    match NL_TICK.load(Ordering::Acquire) {
        1 => {}
        3 => return Outcome::Fail("the write took no grid hold"),
        _ => return Outcome::Fail("no tick during the write"),
    }
    let holds = fb_testing::grid_holds();
    if holds < 16 {
        return crate::fail_fmt!("{holds} grid holds, want at least 16");
    }
    let piece = fb_testing::max_piece_bytes();
    if piece > PIECE_BYTES as u64 {
        return crate::fail_fmt!("a redraw hold wrote {piece} bytes, want at most {PIECE_BYTES}");
    }
    if fb_testing::pieces() == 0 {
        return Outcome::Fail("the write redrew nothing");
    }
    let scrolls = fb_testing::max_chunk_scrolls();
    if scrolls > 1 {
        return crate::fail_fmt!("a chunk scrolled {scrolls} times");
    }
    if fb_testing::scrolls() == 0 {
        return Outcome::Fail("4096 newlines never scrolled");
    }
    console_if_off_write()
}

/// A `Z` written with IF off stays background until a write with IF on.
fn console_if_off_write() -> Outcome {
    let Some((gx, gy)) = (0..8u8)
        .flat_map(|y| (0..8u8).map(move |x| (x, y)))
        .find(|&(x, y)| vibeos::fb::glyph_pixel(b'Z', x, y))
    else {
        return Outcome::Fail("Z has no lit pixel");
    };
    fb_init::write(b"\n");
    let Some((col, row)) = cursor() else {
        return Outcome::Fail("no cursor");
    };
    let (ox, oy) = vibeos::fb::glyph_origin(col, row);
    let (x, y) = (ox + u32::from(gx), oy + u32::from(gy));
    let Some(bg) = get_pixel(x, y) else {
        return Outcome::Fail("cursor cell off screen");
    };
    {
        let _g = x86::InterruptGuard::enter();
        fb_init::write(b"Z");
    }
    if get_pixel(x, y) != Some(bg) {
        return Outcome::Fail("a write with IF off drew its glyph");
    }
    fb_init::write(b"");
    if get_pixel(x, y) == Some(bg) {
        return Outcome::Fail("the next write with IF on left Z undrawn");
    }
    fb_init::write(b"\n");
    Outcome::Ok
}

// Fill 1 MiB past the code page with "x\r" pairs, which neither wrap nor
// scroll, and write(1, them, 1 MiB); exit 0 when it returned 1 MiB.
user_code!(
    S19_XCR_MIB,
    "
    mov rdi, 0x40001000
    mov ecx, 0x80000
    mov ax, 0x0d78
    rep stosw
    mov edi, 1
    mov rsi, 0x40001000
    mov edx, 0x100000
    mov eax, 1
    syscall
    xor edi, edi
    cmp rax, 0x100000
    setne dil
    mov eax, 60
    syscall
    ud2
    "
);

const XCR_LAYOUT: Layout = Layout {
    vaddr: 0x4000_0000,
    memsz: Some(0x10_1000),
    writable: true,
};

static ACK_PID: AtomicU64 = AtomicU64::new(0);

static ACK_SPAWNED: AtomicBool = AtomicBool::new(false);

static ACK_DONE: AtomicBool = AtomicBool::new(false);

/// What the write record held when the unmap returned; `u64::MAX` when
/// the write took fewer than 2 grid holds in time or `vmap` failed.
static ACK_AT_UNMAP: AtomicU64 = AtomicU64::new(0);

fn ack_spawner() {
    let pid = match user::spawn(&Image::Code(S19_XCR_MIB, XCR_LAYOUT), &["xcr_mib"]) {
        Ok(pid) => u64::from(pid),
        Err(_) => u64::MAX,
    };
    ACK_PID.store(pid, Ordering::Relaxed);
    ACK_SPAWNED.store(true, Ordering::Release);
}

/// On the second CPU: once the write has taken 2 grid holds, map and
/// unmap one frame, and record whether the write had returned by then.
fn ack_unmapper() {
    let rec = if spin_until(|| fb_testing::grid_holds() >= 2, 10_000_000_000) {
        match crate::ktest::alloc_frames_owned(0).map(kva_init::vmap) {
            Some(Ok(v)) => {
                free_frames_owned(kva_init::vunmap(v));
                proc_testing::console_write_done_ns()
            }
            _ => u64::MAX,
        }
    } else {
        u64::MAX
    };
    ACK_AT_UNMAP.store(rec, Ordering::Release);
    ACK_DONE.store(true, Ordering::Release);
}

/// A 1 MiB console `write` from ring 3 on CPU 0 acknowledges a shootdown
/// another CPU sends during it: the unmap returns before the write does
/// (ROADMAP §10.10, F011).
pub(crate) fn test_lifetime_console_write_acks_shootdown() -> Outcome {
    let Some(other) = crate::ktest::second_cpu() else {
        return Outcome::Skip("needs 2 CPUs");
    };
    if !fb_console_on() {
        return Outcome::Fail("no framebuffer console");
    }
    ACK_SPAWNED.store(false, Ordering::Release);
    ACK_DONE.store(false, Ordering::Release);
    ACK_AT_UNMAP.store(0, Ordering::Release);
    let st = {
        let _serial = SerialOff::new();
        fb_testing::reset();
        proc_testing::arm_console_write_record();
        crate::ktest::spawn_thread_on("s19_ack_unmap", ack_unmapper, other);
        crate::ktest::spawn_thread_on("s19_ack_spawn", ack_spawner, 0);
        if !sleep_until_s19(|| ACK_SPAWNED.load(Ordering::Acquire), 5_000) {
            return Outcome::Fail("spawner did not run");
        }
        let Ok(pid) = u32::try_from(ACK_PID.load(Ordering::Relaxed)) else {
            return Outcome::Fail("spawn");
        };
        user::wait(pid)
    };
    if !sleep_until_s19(|| ACK_DONE.load(Ordering::Acquire), 15_000) {
        return Outcome::Fail("unmap thread did not finish");
    }
    if st != 0 {
        return crate::fail_fmt!("write program status {st:#x}, want 0");
    }
    match ACK_AT_UNMAP.load(Ordering::Acquire) {
        0 => {}
        u64::MAX => return Outcome::Fail("no unmap during the write"),
        _ => return Outcome::Fail("the write returned before the unmap"),
    }
    if proc_testing::console_write_done_ns() == 0 {
        return Outcome::Fail("the write record is empty");
    }
    let holds = fb_testing::grid_holds();
    if holds < 4096 {
        return crate::fail_fmt!("{holds} grid holds, want at least 4096");
    }
    Outcome::Ok
}
