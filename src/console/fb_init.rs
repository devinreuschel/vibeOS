//! Framebuffer text console. ROADMAP §5.1, §10.6.
//!
//! BGRX, Limine pitch, release bounds checks. Drawing stays off the
//! IRQ path. Double buffering is deferred (ROADMAP §5.1; lands in §16.1).
//!
//! The console lock, [`CONSOLE`], guards the RAM text grid and the
//! framebuffer. A write holds it once per [`CHUNK`] to update the grid,
//! and, when its caller runs with IF=1, redraws what is dirty in pieces of
//! at most [`vibeos::fb::PIECE_BYTES`], one lock hold each, drawing every piece from
//! the grid as it then stands. A write with IF off updates only the grid
//! and leaves the redraw to the next write with IF on (DESIGN §2.9 rule 2).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vibeos::fb::{CHUNK, MAX_CELLS, TextGrid, glyph_origin, pack_bgrx, pixel_offset};
use vibeos::font::{self, FONT_H, FONT_W};
use vibeos::lock::RANK_DEVICE;

use crate::arch::current::interrupts_enabled;
use crate::boot::FbInfo;
use crate::kva_init;
use crate::paging_init;
use crate::sync_init::SpinMutex;
use vibeos::paging::{PhysAddr, VirtAddr, physmap_flags};

const BANNER_ROWS: u32 = 1;
const BG: u32 = pack_bgrx(0x12, 0x12, 0x18);
const FG: u32 = pack_bgrx(0xDC, 0xDC, 0xE0);
const BANNER_BG: u32 = pack_bgrx(0x28, 0x18, 0x48);
const BANNER_FG: u32 = pack_bgrx(0xF0, 0xE0, 0x88);
const BANNER: &[u8] = b" vibeOS";

/// The framebuffer's hardware fields, from the bootloader.
pub(super) struct Fb {
    base: u64,
    pub(super) width: u32,
    height: u32,
    pub(super) pitch: u64,
    size: u64,
}

/// What the console lock guards.
pub(super) struct Console {
    pub(super) grid: TextGrid,
    /// The byte each text cell shows on the framebuffer, row-major by
    /// screen position, so a redraw skips a cell that already shows its
    /// grid byte without reading VRAM.
    shown: [u8; MAX_CELLS],
    pub(super) fb: Option<Fb>,
}

static READY: AtomicBool = AtomicBool::new(false);
/// The console lock: the RAM text grid and the framebuffer it is drawn
/// to. Held for one chunk's grid update or one redraw piece at a time.
pub(super) static CONSOLE: SpinMutex<Console> = SpinMutex::with_rank(
    Console {
        grid: TextGrid::empty(),
        shown: [0; MAX_CELLS],
        fb: None,
    },
    RANK_DEVICE,
);
static FB_PHYS: AtomicU64 = AtomicU64::new(0);
static FB_LEN: AtomicU64 = AtomicU64::new(0);
static FB_VIRT: AtomicU64 = AtomicU64::new(0);

/// The framebuffer console is up; the REPL and the in-guest tests ask.
#[cfg(any(feature = "kernel_tests", feature = "kernel_shell"))]
#[cfg_attr(
    all(target_arch = "aarch64", not(feature = "kernel_tests")),
    expect(dead_code, reason = "boot-CPU S7; unused on this path")
)]
pub fn ready() -> bool {
    // Acquire: pairs with the Release store in `init`.
    READY.load(Ordering::Acquire)
}

/// Physical `[base, base+len)` of the console framebuffer, if any.
/// Atomically readable so PCI BAR mapping can skip a UC patch without
/// taking RANK_DEVICE (ioremap needs PT, which ranks below DEVICE).
pub fn overlaps_phys(phys: u64, len: u64) -> bool {
    // Acquire: pairs with the Release store in `init`.
    let span = FB_LEN.load(Ordering::Acquire);
    if span == 0 || len == 0 {
        return false;
    }
    // Acquire: pairs with the Release store in `init`.
    let base = FB_PHYS.load(Ordering::Acquire);
    phys < base.saturating_add(span) && base < phys.saturating_add(len)
}

/// Kernel VA of `phys` inside the console framebuffer mapping, if any.
pub fn va_for_phys(phys: u64) -> Option<u64> {
    // Acquire: pairs with the Release store in `init`.
    let span = FB_LEN.load(Ordering::Acquire);
    if span == 0 {
        return None;
    }
    // Acquire: pairs with the Release store in `init`.
    let base = FB_PHYS.load(Ordering::Acquire);
    if phys < base || phys >= base.saturating_add(span) {
        return None;
    }
    // Acquire: pairs with the Release store in `init`.
    let virt = FB_VIRT.load(Ordering::Acquire);
    Some(virt.saturating_add(phys - base))
}

/// Attach the first framebuffer [`Fb::new`] accepts.
pub fn init() -> bool {
    let Some((info, fb)) = crate::boot::info()
        .framebuffers()
        .find_map(|i| Some((i, Fb::new(&i)?)))
    else {
        return false;
    };
    let base = fb.base;
    {
        let mut c = CONSOLE.lock();
        if !c.grid.configure(fb.width, fb.height, BANNER_ROWS) {
            return false;
        }
        fb.fill(BG);
        c.shown.fill(b' ');
        fb.fill_banner();
        fb.paint_string(0, 0, BANNER, BANNER_FG, BANNER_BG);
        c.fb = Some(fb);
    }
    // Release: pairs with the Acquire load in `overlaps_phys`.
    FB_PHYS.store(info.phys, Ordering::Release);
    // Release: pairs with the Acquire load in `va_for_phys`.
    FB_VIRT.store(base, Ordering::Release);
    // Release: pairs with the Acquire load in `overlaps_phys`; publishes `FB_PHYS` too.
    FB_LEN.store(info.size, Ordering::Release);
    // Release: pairs with the Acquire loads in `ready` and `write`.
    READY.store(true, Ordering::Release);
    true
}

fn map_fb(i: &FbInfo) -> Option<u64> {
    let hhdm = paging_init::hhdm_offset();
    let va = VirtAddr(i.virt);
    if paging_init::phys_mapped(i.phys) && paging_init::translate(va).is_some() {
        let end = i.phys.saturating_add(i.size);
        let mut p = i.phys & !(4096 - 1);
        while p < end {
            if paging_init::translate(VirtAddr(hhdm.wrapping_add(p))).is_none() {
                break;
            }
            p = p.saturating_add(4096);
        }
        if p >= end {
            return Some(hhdm.wrapping_add(i.phys));
        }
    }
    // SAFETY: the framebuffer is device or RAM the bootloader already
    // scanned; `memremap` maps it write-back in KVA (MEMORY.md §4.1);
    // established here.
    unsafe { kva_init::memremap(PhysAddr(i.phys), i.size, physmap_flags()) }.map(|v| v.as_u64())
}

impl Fb {
    /// 32bpp and room for the banner plus one text row.
    fn new(i: &FbInfo) -> Option<Self> {
        if i.bpp != 32 || i.width < FONT_W || i.height < FONT_H * (BANNER_ROWS + 1) || i.pitch < 4 {
            return None;
        }
        let Some(base) = map_fb(i) else {
            crate::marker!("vibeOS: fb: unreachable");
            return None;
        };
        Some(Self {
            base,
            width: i.width,
            height: i.height,
            pitch: i.pitch,
            size: i.size,
        })
    }

    /// The one bound on every framebuffer access: `(x, y)` inside the
    /// mode and its 4 bytes inside the mapped `size`.
    pub(super) fn pixel_ptr(&self, x: u32, y: u32) -> Option<*mut u32> {
        let off = pixel_offset(x, y, self.width, self.height, self.pitch)?;
        if off.checked_add(4)? > self.size {
            return None;
        }
        Some(self.base.wrapping_add(off) as *mut u32)
    }

    pub(super) fn put_pixel(&self, x: u32, y: u32, color: u32) {
        let Some(p) = self.pixel_ptr(x, y) else {
            return;
        };
        // SAFETY: invariant: `p` is a 4-byte-aligned pixel inside the
        // framebuffer mapping the bootloader handed over, which stays
        // mapped for the kernel's life; established by
        // `fb_init::Fb::pixel_ptr`.
        unsafe { p.write_volatile(color) };
    }

    /// The pixel at `(x, y)`; the in-guest tests' read-back.
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
    pub(super) fn get_pixel(&self, x: u32, y: u32) -> Option<u32> {
        let p = self.pixel_ptr(x, y)?;
        // SAFETY: invariant: as in `put_pixel`; established by
        // `fb_init::Fb::pixel_ptr`.
        Some(unsafe { p.read_volatile() })
    }

    fn fill(&self, color: u32) {
        let mut y = 0;
        while y < self.height {
            let mut x = 0;
            while x < self.width {
                self.put_pixel(x, y, color);
                x += 1;
            }
            y += 1;
        }
    }

    fn fill_banner(&self) {
        let h = BANNER_ROWS * FONT_H;
        let mut y = 0;
        while y < h && y < self.height {
            let mut x = 0;
            while x < self.width {
                self.put_pixel(x, y, BANNER_BG);
                x += 1;
            }
            y += 1;
        }
    }

    /// Paint cell `(col, row)`. Its last pixel's bound covers the whole
    /// cell, so the rows are written without a check per pixel.
    fn paint_glyph(&self, col: u32, row: u32, ch: u8, fg: u32, bg: u32) {
        let (px, py) = glyph_origin(col, row);
        let (Some(first), Some(_)) = (
            self.pixel_ptr(px, py),
            self.pixel_ptr(px.saturating_add(FONT_W - 1), py.saturating_add(FONT_H - 1)),
        ) else {
            return;
        };
        let mut line = first as u64;
        for &bits in font::glyph(ch) {
            let mut gx = 0u32;
            while gx < FONT_W {
                let color = if bits & (1 << gx) != 0 { fg } else { bg };
                let p = line.wrapping_add(u64::from(gx) * 4) as *mut u32;
                // SAFETY: invariant: pixel `(px + gx, py + gy)` lies between
                // the cell's first and last pixels, which both passed the
                // bound, so it is inside the framebuffer mapping the
                // bootloader handed over; established by
                // `fb_init::Fb::pixel_ptr`.
                unsafe { p.write_volatile(color) };
                gx += 1;
            }
            line = line.wrapping_add(self.pitch);
        }
    }

    fn paint_string(&self, col: u32, row: u32, s: &[u8], fg: u32, bg: u32) {
        let mut c = col;
        for &ch in s {
            self.paint_glyph(c, row, ch, fg, bg);
            c = c.saturating_add(1);
        }
    }
}

/// Redraw what is dirty, one piece per console-lock hold, each drawn from
/// the grid as it stands under that hold. Only with IF=1.
fn redraw() {
    loop {
        let mut g = CONSOLE.lock();
        let c = &mut *g;
        let Some(p) = c.grid.next_piece() else {
            return;
        };
        #[cfg_attr(
            not(all(feature = "kernel_tests", target_arch = "x86_64")),
            allow(unused_variables, unused_assignments)
        )]
        let mut painted = 0usize;
        if let Some(fb) = c.fb.as_ref() {
            let cols = c.grid.cols() as usize;
            let mut i = 0;
            while i < p.n {
                let col = p.col + i;
                let ch = c.grid.cell(p.row, col);
                let at = (p.row as usize)
                    .checked_mul(cols)
                    .and_then(|r| r.checked_add(col as usize));
                if let Some(shown) = at.and_then(|at| c.shown.get_mut(at))
                    && *shown != ch
                {
                    fb.paint_glyph(col, p.row, ch, FG, BG);
                    *shown = ch;
                    painted += 1;
                }
                i += 1;
            }
            let _ = painted;
        }
        #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
        testing::on_piece(painted * vibeos::fb::CELL_BYTES);
        #[cfg(not(feature = "kernel_tests"))]
        let _ = painted;
    }
}

/// Silent. Never logs. One console-lock hold per [`CHUNK`] of `bytes`
/// for the grid; with IF=1 the redraw follows each chunk, and an empty
/// write only redraws.
pub fn write(bytes: &[u8]) {
    // Acquire: pairs with the Release store in `init`.
    if !READY.load(Ordering::Acquire) {
        return;
    }
    let mut rest = bytes;
    loop {
        let (chunk, tail) = rest.split_at(rest.len().min(CHUNK));
        {
            let _st = CONSOLE.lock().grid.write_chunk(chunk);
            #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
            testing::on_chunk(_st);
        }
        if interrupts_enabled() {
            redraw();
        }
        if tail.is_empty() {
            return;
        }
        rest = tail;
    }
}

/// In-guest test counters. `kernel_tests` only (AGENTS.md rule 9).
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub(crate) mod testing {
    use core::sync::atomic::{AtomicU64, Ordering};

    use vibeos::fb::ChunkStats;

    static HOLDS: AtomicU64 = AtomicU64::new(0);
    static MAX_PIECE_BYTES: AtomicU64 = AtomicU64::new(0);
    static MAX_CHUNK_SCROLLS: AtomicU64 = AtomicU64::new(0);
    static SCROLLS: AtomicU64 = AtomicU64::new(0);
    static PIECES: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn reset() {
        // Release: pairs with the Acquire load in `grid_holds`.
        HOLDS.store(0, Ordering::Release);
        // Release: pairs with the Acquire load in `max_piece_bytes`.
        MAX_PIECE_BYTES.store(0, Ordering::Release);
        // Release: pairs with the Acquire load in `max_chunk_scrolls`.
        MAX_CHUNK_SCROLLS.store(0, Ordering::Release);
        // Release: pairs with the Acquire load in `scrolls`.
        SCROLLS.store(0, Ordering::Release);
        // Release: pairs with the Acquire load in `pieces`.
        PIECES.store(0, Ordering::Release);
    }

    /// Redraw pieces taken, since [`reset`].
    pub(crate) fn pieces() -> u64 {
        // Acquire: pairs with the Release store in `reset` and the AcqRel add in `on_piece`.
        PIECES.load(Ordering::Acquire)
    }

    /// Console-lock holds that updated the grid, since [`reset`].
    pub(crate) fn grid_holds() -> u64 {
        // Acquire: pairs with the Release store in `reset` and the AcqRel add in `on_chunk`.
        HOLDS.load(Ordering::Acquire)
    }

    /// The most framebuffer bytes one redraw piece wrote, since [`reset`].
    pub(crate) fn max_piece_bytes() -> u64 {
        // Acquire: pairs with the Release store in `reset` and the AcqRel max in `on_piece`.
        MAX_PIECE_BYTES.load(Ordering::Acquire)
    }

    /// The most scrolls one chunk made.
    pub(crate) fn max_chunk_scrolls() -> u64 {
        // Acquire: pairs with the Release store in `reset` and the AcqRel max in `on_chunk`.
        MAX_CHUNK_SCROLLS.load(Ordering::Acquire)
    }

    /// Chunks that scrolled.
    pub(crate) fn scrolls() -> u64 {
        // Acquire: pairs with the Release store in `reset` and the AcqRel add in `on_chunk`.
        SCROLLS.load(Ordering::Acquire)
    }

    pub(super) fn on_chunk(st: ChunkStats) {
        // AcqRel: pairs with the Acquire load in `grid_holds`.
        HOLDS.fetch_add(1, Ordering::AcqRel);
        // AcqRel: pairs with the Acquire load in `max_chunk_scrolls`.
        MAX_CHUNK_SCROLLS.fetch_max(u64::from(st.scrolls), Ordering::AcqRel);
        // AcqRel: pairs with the Acquire load in `scrolls`.
        SCROLLS.fetch_add(u64::from(st.scrolls), Ordering::AcqRel);
    }

    pub(super) fn on_piece(bytes: usize) {
        // AcqRel: pairs with the Acquire load in `pieces`.
        PIECES.fetch_add(1, Ordering::AcqRel);
        // AcqRel: pairs with the Acquire load in `max_piece_bytes`.
        MAX_PIECE_BYTES.fetch_max(bytes as u64, Ordering::AcqRel);
    }
}
