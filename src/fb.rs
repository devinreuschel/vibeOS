//! Framebuffer pixel math and the text grid. ROADMAP §5.1.
//!
//! Pixel address is `base + y * pitch + x * 4` (BGRX). Bounds checks are
//! real (`Option`), not `debug_assert`. Pitch is independent of `width*4`.

use crate::font::{self, FONT_H, FONT_W};

/// Pack R,G,B into a BGRX `u32` (blue in the low byte).
pub const fn pack_bgrx(r: u8, g: u8, b: u8) -> u32 {
    (b as u32) | ((g as u32) << 8) | ((r as u32) << 16)
}

/// Byte offset of pixel `(x, y)` in a `pitch`-byte scanout.
/// `None` if out of bounds or the multiply overflows.
pub fn pixel_offset(x: u32, y: u32, width: u32, height: u32, pitch: u64) -> Option<u64> {
    if x >= width || y >= height {
        return None;
    }
    let row = (y as u64).checked_mul(pitch)?;
    let col = (x as u64).checked_mul(4)?;
    row.checked_add(col)
}

/// Bytes in one text row: `FONT_H` scanlines of `pitch`.
pub fn text_row_bytes(pitch: u64) -> Option<u64> {
    (FONT_H as u64).checked_mul(pitch)
}

/// `memmove` window for a scroll that leaves `banner_rows` text rows put.
/// Returns `(src_off, dst_off, len)` in bytes from the FB base.
/// `None` if there is nothing to move (too few rows) or overflow.
pub fn scroll_copy(banner_rows: u32, rows: u32, pitch: u64) -> Option<(u64, u64, u64)> {
    if rows <= banner_rows + 1 {
        return None;
    }
    let row_bytes = text_row_bytes(pitch)?;
    let dst = (banner_rows as u64).checked_mul(row_bytes)?;
    let src = dst.checked_add(row_bytes)?;
    let moving = (rows - banner_rows - 1) as u64;
    let len = moving.checked_mul(row_bytes)?;
    Some((src, dst, len))
}

/// Offset of the last text row (the one a scroll clears).
pub fn last_text_row_offset(rows: u32, pitch: u64) -> Option<u64> {
    if rows == 0 {
        return None;
    }
    let row_bytes = text_row_bytes(pitch)?;
    ((rows - 1) as u64).checked_mul(row_bytes)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellAction {
    None,
    Glyph {
        col: u32,
        row: u32,
        ch: u8,
    },
    Scroll,
    /// Wrap off the last row: scroll first, then paint.
    GlyphThenScroll {
        col: u32,
        row: u32,
        ch: u8,
    },
}

/// Text cursor + wrap/scroll. Banner rows are never the cursor's home
/// and survive `Scroll`. `\n` next row, `\r` column 0 same row, `0x08`
/// backspace (paint space).
#[derive(Clone, Copy, Debug)]
pub struct TextGrid {
    pub cols: u32,
    pub rows: u32,
    pub banner_rows: u32,
    pub col: u32,
    pub row: u32,
}

impl TextGrid {
    pub fn new(pixel_w: u32, pixel_h: u32, banner_rows: u32) -> Option<Self> {
        let cols = pixel_w / FONT_W;
        let rows = pixel_h / FONT_H;
        if cols == 0 || rows == 0 || banner_rows >= rows {
            return None;
        }
        Some(Self {
            cols,
            rows,
            banner_rows,
            col: 0,
            row: banner_rows,
        })
    }

    pub fn put(&mut self, ch: u8) -> CellAction {
        match ch {
            b'\n' => self.newline(),
            b'\r' => {
                self.col = 0;
                CellAction::None
            }
            0x08 => self.backspace(),
            _ => self.put_visible(ch),
        }
    }

    fn put_visible(&mut self, ch: u8) -> CellAction {
        let mut scrolled = false;
        if self.col >= self.cols {
            scrolled = matches!(self.newline(), CellAction::Scroll);
        }
        let col = self.col;
        let row = self.row;
        self.col = self.col.saturating_add(1);
        if scrolled {
            CellAction::GlyphThenScroll { col, row, ch }
        } else {
            CellAction::Glyph { col, row, ch }
        }
    }

    fn newline(&mut self) -> CellAction {
        self.col = 0;
        self.row = self.row.saturating_add(1);
        if self.row >= self.rows {
            self.row = self.rows - 1;
            CellAction::Scroll
        } else {
            CellAction::None
        }
    }

    fn backspace(&mut self) -> CellAction {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > self.banner_rows {
            self.row -= 1;
            self.col = self.cols.saturating_sub(1);
        } else {
            return CellAction::None;
        }
        CellAction::Glyph {
            col: self.col,
            row: self.row,
            ch: b' ',
        }
    }
}

/// Origin of glyph `(col, row)` in pixels.
pub fn glyph_origin(col: u32, row: u32) -> (u32, u32) {
    (col.saturating_mul(FONT_W), row.saturating_mul(FONT_H))
}

pub fn font_w() -> u32 {
    FONT_W
}

pub fn font_h() -> u32 {
    FONT_H
}

pub use font::glyph_pixel;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pitch_is_not_width_times_four() {
        // 640×480 with 2560 pitch (padding) vs 640*4 = 2560 equal;
        // 641 would not match. Use a padded pitch.
        let width = 10;
        let height = 8;
        let pitch = 64; // 10*4 = 40, so 24 bytes of padding
        assert_ne!(pitch, (width as u64) * 4);
        assert_eq!(pixel_offset(0, 0, width, height, pitch), Some(0));
        assert_eq!(pixel_offset(1, 0, width, height, pitch), Some(4));
        assert_eq!(pixel_offset(0, 1, width, height, pitch), Some(64));
        assert_eq!(pixel_offset(2, 3, width, height, pitch), Some(3 * 64 + 8));
        assert_eq!(pixel_offset(width, 0, width, height, pitch), None);
        assert_eq!(pixel_offset(0, height, width, height, pitch), None);
        assert_eq!(pixel_offset(9, 7, width, height, pitch), Some(7 * 64 + 36));
    }

    #[test]
    fn out_of_bounds_is_none_in_release_shape() {
        // The kernel uses this Option, not debug_assert.
        assert!(pixel_offset(100, 0, 10, 10, 40).is_none());
        assert!(pixel_offset(0, 100, 10, 10, 40).is_none());
        assert!(pixel_offset(0, 0, 0, 10, 40).is_none());
    }

    #[test]
    fn bgrx_blue_in_low_byte() {
        let p = pack_bgrx(0x11, 0x22, 0x33);
        assert_eq!(p & 0xFF, 0x33);
        assert_eq!((p >> 8) & 0xFF, 0x22);
        assert_eq!((p >> 16) & 0xFF, 0x11);
        assert_eq!(p >> 24, 0);
    }

    #[test]
    fn wrap_and_scroll_leave_banner() {
        let mut g = TextGrid::new(16, 24, 1).unwrap(); // 2 cols, 3 rows, banner=1
        assert_eq!(g.cols, 2);
        assert_eq!(g.rows, 3);
        assert_eq!(g.row, 1);
        assert_eq!(
            g.put(b'A'),
            CellAction::Glyph {
                col: 0,
                row: 1,
                ch: b'A'
            }
        );
        assert_eq!(
            g.put(b'B'),
            CellAction::Glyph {
                col: 1,
                row: 1,
                ch: b'B'
            }
        );
        // wrap to next text row
        assert_eq!(
            g.put(b'C'),
            CellAction::Glyph {
                col: 0,
                row: 2,
                ch: b'C'
            }
        );
        assert_eq!(
            g.put(b'D'),
            CellAction::Glyph {
                col: 1,
                row: 2,
                ch: b'D'
            }
        );
        // next wrap scrolls; banner row 0 is not the cursor
        let a = g.put(b'E');
        assert_eq!(
            a,
            CellAction::GlyphThenScroll {
                col: 0,
                row: 2,
                ch: b'E'
            }
        );
        assert_eq!(g.row, 2);
        assert_eq!(g.banner_rows, 1);
        assert_eq!(g.put(b'\n'), CellAction::Scroll);
        assert_eq!(g.row, 2);
        assert_eq!(g.col, 0);
    }

    #[test]
    fn newline_from_banner_home() {
        let mut g = TextGrid::new(8, 24, 1).unwrap();
        assert_eq!(g.rows, 3);
        assert_eq!(g.put(b'\n'), CellAction::None);
        assert_eq!(g.row, 2);
        assert_eq!(g.col, 0);
    }

    #[test]
    fn scroll_copy_skips_banner() {
        let pitch = 40u64;
        let (src, dst, len) = scroll_copy(1, 4, pitch).unwrap();
        let row = text_row_bytes(pitch).unwrap();
        assert_eq!(dst, row); // start of row 1
        assert_eq!(src, 2 * row);
        assert_eq!(len, 2 * row); // rows 2..3 → 1..2
        assert!(scroll_copy(1, 2, pitch).is_none());
        assert_eq!(last_text_row_offset(4, pitch), Some(3 * row));
    }

    #[test]
    fn glyph_origin_is_8x8() {
        assert_eq!(glyph_origin(3, 2), (24, 16));
    }

    #[test]
    fn cr_homes_column_same_row() {
        let mut g = TextGrid::new(80, 32, 1).unwrap(); // 10 cols, 4 rows
        let row = g.row;
        assert_eq!(
            g.put(b'A'),
            CellAction::Glyph {
                col: 0,
                row,
                ch: b'A'
            }
        );
        assert_eq!(
            g.put(b'B'),
            CellAction::Glyph {
                col: 1,
                row,
                ch: b'B'
            }
        );
        assert_eq!(
            g.put(b'C'),
            CellAction::Glyph {
                col: 2,
                row,
                ch: b'C'
            }
        );
        assert_eq!(g.col, 3);
        assert_eq!(g.put(b'\r'), CellAction::None);
        assert_eq!(g.col, 0);
        assert_eq!(g.row, row);
        assert_eq!(
            g.put(b'X'),
            CellAction::Glyph {
                col: 0,
                row,
                ch: b'X'
            }
        );
        assert_eq!(g.col, 1);
        assert_eq!(g.row, row);
    }

    #[test]
    fn crlf_is_one_newline() {
        let mut g = TextGrid::new(16, 32, 1).unwrap();
        let start = g.row;
        g.put(b'A');
        assert_eq!(g.put(b'\r'), CellAction::None);
        assert_eq!(g.put(b'\n'), CellAction::None);
        assert_eq!(g.col, 0);
        assert_eq!(g.row, start + 1);
    }

    #[test]
    fn cr_paint_overwrites_in_place() {
        // Shell paint: CR, rewrite, pad shorter with spaces, CR, cursor.
        let mut g = TextGrid::new(80, 32, 1).unwrap();
        let home = g.row;
        for &b in b"ab" {
            g.put(b);
        }
        assert_eq!(g.col, 2);
        assert_eq!(g.put(b'\r'), CellAction::None);
        assert_eq!(
            g.put(b'a'),
            CellAction::Glyph {
                col: 0,
                row: home,
                ch: b'a'
            }
        );
        assert_eq!(
            g.put(b' '),
            CellAction::Glyph {
                col: 1,
                row: home,
                ch: b' '
            }
        );
        assert_eq!(g.put(b'\r'), CellAction::None);
        assert_eq!(
            g.put(b'a'),
            CellAction::Glyph {
                col: 0,
                row: home,
                ch: b'a'
            }
        );
        assert_eq!(g.col, 1);
        assert_eq!(g.row, home);
    }
}
