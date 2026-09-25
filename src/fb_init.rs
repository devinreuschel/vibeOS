//! Framebuffer text console. ROADMAP §5.1.
//!
//! BGRX, Limine pitch, release bounds checks. Drawing stays off the
//! IRQ path. Double buffering is deferred (ROADMAP §5.1; lands in §16.1).

use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vibeos::fb::{
    CellAction, TextGrid, glyph_origin, last_text_row_offset, pack_bgrx, pixel_offset, scroll_copy,
    text_row_bytes,
};
use vibeos::font::{self, FONT_H, FONT_W};
use vibeos::lock::RANK_DEVICE;

use crate::boot::FbInfo;
use crate::sync_init::SpinMutex;

const BANNER_ROWS: u32 = 1;
const BG: u32 = pack_bgrx(0x12, 0x12, 0x18);
const FG: u32 = pack_bgrx(0xDC, 0xDC, 0xE0);
const BANNER_BG: u32 = pack_bgrx(0x28, 0x18, 0x48);
const BANNER_FG: u32 = pack_bgrx(0xF0, 0xE0, 0x88);
const BANNER: &[u8] = b" vibeOS";

struct Fb {
    base: u64,
    width: u32,
    height: u32,
    pitch: u64,
    size: u64,
    grid: TextGrid,
}

static READY: AtomicBool = AtomicBool::new(false);
static FB: SpinMutex<Option<Fb>> = SpinMutex::with_rank(None, RANK_DEVICE);
static FB_PHYS: AtomicU64 = AtomicU64::new(0);
static FB_LEN: AtomicU64 = AtomicU64::new(0);

pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Physical `[base, base+len)` of the console framebuffer, if any.
/// Atomically readable so PCI BAR mapping can skip a UC patch without
/// taking RANK_DEVICE (ioremap needs PT, which ranks below DEVICE).
pub fn overlaps_phys(phys: u64, len: u64) -> bool {
    let span = FB_LEN.load(Ordering::Acquire);
    if span == 0 || len == 0 {
        return false;
    }
    let base = FB_PHYS.load(Ordering::Acquire);
    phys < base.saturating_add(span) && base < phys.saturating_add(len)
}

/// Attach the first framebuffer [`Fb::new`] accepts.
pub fn init() -> bool {
    let Some((info, fb)) = crate::boot::info()
        .framebuffers()
        .find_map(|i| Some((i, Fb::new(&i)?)))
    else {
        return false;
    };
    FB_PHYS.store(info.phys, Ordering::Release);
    FB_LEN.store(info.size, Ordering::Release);
    fb.fill(BG);
    fb.fill_banner();
    fb.paint_string(0, 0, BANNER, BANNER_FG, BANNER_BG);
    *FB.lock() = Some(fb);
    READY.store(true, Ordering::Release);
    true
}

impl Fb {
    /// 32bpp and room for the banner plus one text row.
    fn new(i: &FbInfo) -> Option<Self> {
        if i.bpp != 32 || i.width < FONT_W || i.height < FONT_H * (BANNER_ROWS + 1) || i.pitch < 4 {
            return None;
        }
        Some(Self {
            base: i.virt,
            width: i.width,
            height: i.height,
            pitch: i.pitch,
            size: i.size,
            grid: TextGrid::new(i.width, i.height, BANNER_ROWS)?,
        })
    }

    fn pixel_ptr(&self, x: u32, y: u32) -> Option<*mut u32> {
        let off = pixel_offset(x, y, self.width, self.height, self.pitch)?;
        if off.checked_add(4)? > self.size {
            return None;
        }
        Some(self.base.wrapping_add(off) as *mut u32)
    }

    fn put_pixel(&self, x: u32, y: u32, color: u32) {
        let Some(p) = self.pixel_ptr(x, y) else {
            return;
        };
        unsafe { p.write_volatile(color) };
    }

    fn get_pixel(&self, x: u32, y: u32) -> Option<u32> {
        let p = self.pixel_ptr(x, y)?;
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

    fn paint_glyph(&self, col: u32, row: u32, ch: u8) {
        let (px, py) = glyph_origin(col, row);
        let (fg, bg) = if row < self.grid.banner_rows {
            (BANNER_FG, BANNER_BG)
        } else {
            (FG, BG)
        };
        let mut gy = 0u8;
        while gy < FONT_H as u8 {
            let mut gx = 0u8;
            while gx < FONT_W as u8 {
                let on = font::glyph_pixel(ch, gx, gy);
                self.put_pixel(px + gx as u32, py + gy as u32, if on { fg } else { bg });
                gx += 1;
            }
            gy += 1;
        }
    }

    fn paint_string(&self, col: u32, row: u32, s: &[u8], fg: u32, bg: u32) {
        let (mut px, py) = glyph_origin(col, row);
        for &ch in s {
            let mut gy = 0u8;
            while gy < FONT_H as u8 {
                let mut gx = 0u8;
                while gx < FONT_W as u8 {
                    let on = font::glyph_pixel(ch, gx, gy);
                    self.put_pixel(px + gx as u32, py + gy as u32, if on { fg } else { bg });
                    gx += 1;
                }
                gy += 1;
            }
            px = px.saturating_add(FONT_W);
        }
    }

    fn scroll(&self) {
        let Some((src, dst, len)) = scroll_copy(self.grid.banner_rows, self.grid.rows, self.pitch)
        else {
            return;
        };
        if dst.checked_add(len).map(|e| e > self.size).unwrap_or(true)
            || src.checked_add(len).map(|e| e > self.size).unwrap_or(true)
        {
            return;
        }
        unsafe {
            ptr::copy(
                self.base.wrapping_add(src) as *const u8,
                self.base.wrapping_add(dst) as *mut u8,
                len as usize,
            );
        }
        self.clear_last_row();
    }

    fn clear_last_row(&self) {
        let Some(off) = last_text_row_offset(self.grid.rows, self.pitch) else {
            return;
        };
        let Some(row_bytes) = text_row_bytes(self.pitch) else {
            return;
        };
        if off
            .checked_add(row_bytes)
            .map(|e| e > self.size)
            .unwrap_or(true)
        {
            return;
        }
        let y0 = (self.grid.rows - 1) * FONT_H;
        let mut y = y0;
        while y < y0 + FONT_H && y < self.height {
            let mut x = 0;
            while x < self.width {
                self.put_pixel(x, y, BG);
                x += 1;
            }
            y += 1;
        }
    }

    /// `\r` homes the column (same row). The line editor paints in place
    /// with it; dropping CR made the FB reprint the prompt.
    fn write_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match self.grid.put(b) {
                CellAction::None => {}
                CellAction::Glyph { col, row, ch } => self.paint_glyph(col, row, ch),
                CellAction::Scroll => self.scroll(),
                CellAction::GlyphThenScroll { col, row, ch } => {
                    self.scroll();
                    self.paint_glyph(col, row, ch);
                }
            }
        }
    }
}

/// Silent. Never logs.
pub fn write(bytes: &[u8]) {
    if !READY.load(Ordering::Acquire) {
        return;
    }
    let mut g = FB.lock();
    if let Some(fb) = g.as_mut() {
        fb.write_bytes(bytes);
    }
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn put_pixel(x: u32, y: u32, color: u32) -> bool {
    let g = FB.lock();
    let Some(fb) = g.as_ref() else {
        return false;
    };
    if fb.pixel_ptr(x, y).is_none() {
        return false;
    }
    fb.put_pixel(x, y, color);
    true
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn get_pixel(x: u32, y: u32) -> Option<u32> {
    let g = FB.lock();
    g.as_ref()?.get_pixel(x, y)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn pitch() -> Option<u64> {
    let g = FB.lock();
    g.as_ref().map(|f| f.pitch)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn width() -> Option<u32> {
    let g = FB.lock();
    g.as_ref().map(|f| f.width)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn cursor() -> Option<(u32, u32)> {
    let g = FB.lock();
    g.as_ref().map(|f| (f.grid.col, f.grid.row))
}
