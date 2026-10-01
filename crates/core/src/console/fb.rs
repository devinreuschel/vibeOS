//! Framebuffer pixel math and the RAM text grid. ROADMAP §5.1, §10.6.
//!
//! Pixel address is `base + y * pitch + x * 4` (BGRX). Bounds checks are
//! real (`Option`), not `debug_assert`. Pitch is independent of `width*4`.
//!
//! [`TextGrid`] is the console's text in RAM: one byte per cell, a
//! ring-row origin so a scroll costs O(1) plus clearing rows, the cursor,
//! and per-row dirty spans. A writer updates it one [`CHUNK`] at a time;
//! the framebuffer is redrawn from it in [`Piece`]s of at most
//! [`PIECE_BYTES`], so nothing ever reads VRAM back (DESIGN §2.9 rule 2).

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

/// Bytes a console write hands the grid per console-lock hold.
pub const CHUNK: usize = 256;
/// Framebuffer bytes one redraw piece writes at most, per console-lock hold.
pub const PIECE_BYTES: usize = 16 * 1024;
/// Framebuffer bytes one cell covers at 32 bpp.
pub const CELL_BYTES: usize = (FONT_W * FONT_H * 4) as usize;
/// Cells one piece draws at most.
pub const PIECE_CELLS: u32 = (PIECE_BYTES / CELL_BYTES) as u32;
/// Grid limits: 3840×2160 with the 8×8 font. A larger screen shows the
/// grid in its top-left corner.
pub const MAX_COLS: u32 = 3840 / FONT_W;
pub const MAX_ROWS: u32 = 2160 / FONT_H;
/// Cells in the largest grid.
pub const MAX_CELLS: usize = (MAX_COLS * MAX_ROWS) as usize;

const _: () = assert!(PIECE_CELLS >= 1 && PIECE_CELLS as usize * CELL_BYTES <= PIECE_BYTES);
const _: () = assert!(MAX_COLS <= u16::MAX as u32);

/// Up to [`PIECE_CELLS`] dirty cells of one row, `col..col + n`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    pub row: u32,
    pub col: u32,
    pub n: u32,
}

/// What one [`TextGrid::write_chunk`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChunkStats {
    /// 1 if the chunk ran past the last row, else 0.
    pub scrolls: u32,
    /// `min(newlines past the bottom, text rows)`.
    pub scroll_rows: u32,
}

/// Dirty columns `lo..hi` of one row; empty when `lo == hi`.
#[derive(Clone, Copy, Debug)]
struct Span {
    lo: u16,
    hi: u16,
}

impl Span {
    const EMPTY: Span = Span { lo: 0, hi: 0 };

    fn is_empty(self) -> bool {
        self.lo >= self.hi
    }
}

/// The console text in RAM. Banner rows are never the cursor's home,
/// never scroll, and are never dirty. `\n` next row, `\r` column 0 same
/// row, `0x08` backspace (blank the cell). Configured in place: it lives
/// inside the console lock's `static` and is never built on the stack.
pub struct TextGrid {
    cells: [u8; MAX_CELLS],
    dirty: [Span; MAX_ROWS as usize],
    cols: u32,
    rows: u32,
    banner_rows: u32,
    /// Physical row of logical text row `banner_rows`, minus `banner_rows`.
    origin: u32,
    col: u32,
    row: u32,
    /// No row below this one is dirty.
    dirty_from: u32,
    /// Rows scrolled by the chunk being written.
    scrolled: u32,
}

impl TextGrid {
    /// An unconfigured grid: no rows, nothing to draw.
    pub const fn empty() -> Self {
        Self {
            cells: [0; MAX_CELLS],
            dirty: [Span::EMPTY; MAX_ROWS as usize],
            cols: 0,
            rows: 0,
            banner_rows: 0,
            origin: 0,
            col: 0,
            row: 0,
            dirty_from: 0,
            scrolled: 0,
        }
    }

    /// Size the grid for a `pixel_w`×`pixel_h` screen, clear it, and home
    /// the cursor below the banner. `false`, leaving the grid empty, when
    /// the screen has no text row below `banner_rows`.
    pub fn configure(&mut self, pixel_w: u32, pixel_h: u32, banner_rows: u32) -> bool {
        let cols = (pixel_w / FONT_W).min(MAX_COLS);
        let rows = (pixel_h / FONT_H).min(MAX_ROWS);
        self.cols = 0;
        self.rows = 0;
        self.banner_rows = 0;
        self.origin = 0;
        self.col = 0;
        self.row = 0;
        self.dirty_from = 0;
        self.scrolled = 0;
        self.dirty = [Span::EMPTY; MAX_ROWS as usize];
        let Some(used) = (cols as usize).checked_mul(rows as usize) else {
            return false;
        };
        if cols == 0 || rows == 0 || banner_rows >= rows || used > MAX_CELLS {
            return false;
        }
        self.cells[..used].fill(b' ');
        self.cols = cols;
        self.rows = rows;
        self.banner_rows = banner_rows;
        self.row = banner_rows;
        self.dirty_from = rows;
        true
    }

    pub fn cols(&self) -> u32 {
        self.cols
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }

    pub fn banner_rows(&self) -> u32 {
        self.banner_rows
    }

    /// `(col, row)` of the next cell a visible byte fills.
    pub fn cursor(&self) -> (u32, u32) {
        (self.col, self.row)
    }

    fn text_rows(&self) -> u32 {
        self.rows - self.banner_rows
    }

    /// Index of logical cell `(col, row)`; `None` outside the grid.
    fn index(&self, col: u32, row: u32) -> Option<usize> {
        if col >= self.cols || row >= self.rows {
            return None;
        }
        let phys = if row < self.banner_rows {
            row
        } else {
            self.banner_rows + (row - self.banner_rows + self.origin) % self.text_rows()
        };
        Some(phys as usize * self.cols as usize + col as usize)
    }

    /// The byte at `(col, row)` as the grid stands; a blank outside it.
    pub fn cell(&self, row: u32, col: u32) -> u8 {
        self.index(col, row)
            .and_then(|i| self.cells.get(i).copied())
            .unwrap_or(b' ')
    }

    fn set(&mut self, col: u32, row: u32, ch: u8) {
        if let Some(c) = self.index(col, row).and_then(|i| self.cells.get_mut(i)) {
            *c = ch;
        }
        self.mark(row, col, col + 1);
    }

    /// Mark columns `lo..hi` of text row `row` dirty.
    fn mark(&mut self, row: u32, lo: u32, hi: u32) {
        if row < self.banner_rows || row >= self.rows {
            return;
        }
        let hi = hi.min(self.cols);
        if lo >= hi {
            return;
        }
        let Some(s) = self.dirty.get_mut(row as usize) else {
            return;
        };
        if s.is_empty() {
            *s = Span {
                lo: lo as u16,
                hi: hi as u16,
            };
        } else {
            s.lo = s.lo.min(lo as u16);
            s.hi = s.hi.max(hi as u16);
        }
        self.dirty_from = self.dirty_from.min(row);
    }

    /// Advance the ring by one row and blank the new last row.
    fn scroll_one(&mut self) {
        self.origin = (self.origin + 1) % self.text_rows();
        let last = self.rows - 1;
        if let Some(i) = self.index(0, last) {
            let end = i + self.cols as usize;
            if let Some(row) = self.cells.get_mut(i..end) {
                row.fill(b' ');
            }
        }
        self.scrolled = self.scrolled.saturating_add(1);
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 >= self.rows {
            self.scroll_one();
        } else {
            self.row += 1;
        }
    }

    fn put(&mut self, ch: u8) {
        match ch {
            b'\n' => self.newline(),
            b'\r' => self.col = 0,
            0x08 => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > self.banner_rows {
                    self.row -= 1;
                    self.col = self.cols - 1;
                } else {
                    return;
                }
                self.set(self.col, self.row, b' ');
            }
            _ => {
                if self.col >= self.cols {
                    self.newline();
                }
                self.set(self.col, self.row, ch);
                self.col += 1;
            }
        }
    }

    /// Put `bytes` (at most [`CHUNK`] from a console write) into the
    /// grid. A chunk that runs past the last row reports one scroll of
    /// `min(newlines past the bottom, text rows)` rows and marks every
    /// text row dirty; nothing is drawn.
    pub fn write_chunk(&mut self, bytes: &[u8]) -> ChunkStats {
        if self.rows == 0 {
            return ChunkStats::default();
        }
        self.scrolled = 0;
        for &b in bytes {
            self.put(b);
        }
        if self.scrolled == 0 {
            return ChunkStats::default();
        }
        let scroll_rows = self.scrolled.min(self.text_rows());
        self.scrolled = 0;
        let mut r = self.banner_rows;
        while r < self.rows {
            self.mark(r, 0, self.cols);
            r += 1;
        }
        ChunkStats {
            scrolls: 1,
            scroll_rows,
        }
    }

    /// Take the next run of at most [`PIECE_CELLS`] dirty cells of one
    /// row and clear their marks. The caller draws them from [`cell`] as
    /// the grid then stands, under the same console-lock hold.
    ///
    /// [`cell`]: TextGrid::cell
    pub fn next_piece(&mut self) -> Option<Piece> {
        let mut r = self.dirty_from.max(self.banner_rows);
        while r < self.rows {
            let s = self.dirty.get_mut(r as usize)?;
            if !s.is_empty() {
                let lo = u32::from(s.lo);
                let n = (u32::from(s.hi) - lo).min(PIECE_CELLS);
                s.lo += n as u16;
                if s.is_empty() {
                    *s = Span::EMPTY;
                }
                self.dirty_from = r;
                return Some(Piece { row: r, col: lo, n });
            }
            r += 1;
        }
        self.dirty_from = self.rows;
        None
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
    extern crate std;

    use std::boxed::Box;
    use std::vec::Vec;

    use super::*;

    fn grid(pixel_w: u32, pixel_h: u32, banner_rows: u32) -> Box<TextGrid> {
        let mut g = Box::new(TextGrid::empty());
        assert!(g.configure(pixel_w, pixel_h, banner_rows));
        g
    }

    fn drain(g: &mut TextGrid) -> Vec<Piece> {
        let mut v = Vec::new();
        while let Some(p) = g.next_piece() {
            v.push(p);
        }
        v
    }

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
    fn configure_rejects_no_text_row() {
        let mut g = Box::new(TextGrid::empty());
        assert!(!g.configure(16, 8, 1));
        assert!(!g.configure(4, 24, 1));
        assert_eq!(g.write_chunk(b"abc\n\n\n"), ChunkStats::default());
        assert_eq!(g.next_piece(), None);
        // A huge screen keeps the grid within its limits.
        assert!(g.configure(u32::MAX, u32::MAX, 1));
        assert_eq!((g.cols(), g.rows()), (MAX_COLS, MAX_ROWS));
    }

    #[test]
    fn wrap_and_scroll_leave_banner() {
        let mut g = grid(16, 24, 1); // 2 cols, 3 rows, banner=1
        assert_eq!((g.cols(), g.rows(), g.banner_rows()), (2, 3, 1));
        assert_eq!(g.cursor(), (0, 1));
        assert_eq!(g.write_chunk(b"AB"), ChunkStats::default());
        assert_eq!((g.cell(1, 0), g.cell(1, 1)), (b'A', b'B'));
        // wrap to next text row
        assert_eq!(g.write_chunk(b"CD"), ChunkStats::default());
        assert_eq!((g.cell(2, 0), g.cell(2, 1)), (b'C', b'D'));
        // next wrap scrolls; banner row 0 is not the cursor
        let st = g.write_chunk(b"E");
        assert_eq!(
            st,
            ChunkStats {
                scrolls: 1,
                scroll_rows: 1
            }
        );
        assert_eq!((g.cell(1, 0), g.cell(1, 1)), (b'C', b'D'));
        assert_eq!((g.cell(2, 0), g.cell(2, 1)), (b'E', b' '));
        assert_eq!(g.cell(0, 0), b' ');
        assert_eq!(g.cursor(), (1, 2));
        assert_eq!(g.write_chunk(b"\n").scrolls, 1);
        assert_eq!(g.cursor(), (0, 2));
        assert_eq!((g.cell(1, 0), g.cell(2, 0)), (b'E', b' '));
        assert!(drain(&mut g).iter().all(|p| p.row >= 1));
    }

    #[test]
    fn newline_from_banner_home() {
        let mut g = grid(8, 24, 1);
        assert_eq!(g.rows(), 3);
        assert_eq!(g.write_chunk(b"\n"), ChunkStats::default());
        assert_eq!(g.cursor(), (0, 2));
    }

    #[test]
    fn grid_scrolls_once_per_chunk() {
        let mut g = grid(16, 32, 1); // 2 cols, 4 rows, 3 text rows
        let st = g.write_chunk(b"A\nB\nC\nD");
        assert_eq!(
            st,
            ChunkStats {
                scrolls: 1,
                scroll_rows: 1
            }
        );
        assert_eq!(
            [g.cell(1, 0), g.cell(2, 0), g.cell(3, 0)],
            [b'B', b'C', b'D']
        );
        // 10 newlines past the bottom: one scroll, capped at the text rows.
        let st = g.write_chunk(&[b'\n'; 10]);
        assert_eq!(
            st,
            ChunkStats {
                scrolls: 1,
                scroll_rows: 3
            }
        );
        assert_eq!([g.cell(1, 0), g.cell(2, 0), g.cell(3, 0)], [b' '; 3]);
        assert_eq!(g.write_chunk(b"xy").scrolls, 0);
        let mut big = [b'\n'; CHUNK];
        big[0] = b'q';
        let st = g.write_chunk(&big);
        assert_eq!(st.scrolls, 1);
        assert_eq!(st.scroll_rows, 3);
        assert_eq!(g.cell(0, 0), b' ');
    }

    #[test]
    fn redraw_pieces_at_most_16k() {
        let mut g = grid(1024, 768, 1); // 128 cols, 96 rows
        assert_eq!(g.write_chunk(&[b'\n'; CHUNK]).scrolls, 1);
        let pieces = drain(&mut g);
        let mut cells = 0u64;
        for p in &pieces {
            assert!(p.n >= 1 && p.n <= PIECE_CELLS);
            assert!(p.n as usize * CELL_BYTES <= PIECE_BYTES);
            assert!(p.row >= 1 && p.row < 96);
            assert!(p.col + p.n <= 128);
            cells += u64::from(p.n);
        }
        assert_eq!(cells, 128 * 95);
        assert_eq!(pieces.len(), 95 * 2);
        assert_eq!(g.next_piece(), None);
    }

    #[test]
    fn piece_draws_grid_as_it_stands() {
        let mut g = grid(80, 32, 1); // 10 cols, 4 rows
        g.write_chunk(b"abc");
        let row = g.cursor().1;
        let p = g.next_piece().unwrap();
        assert_eq!(p, Piece { row, col: 0, n: 3 });
        // A writer changes the grid before the piece is drawn: the
        // piece draws the new byte, and the cell is dirty again.
        g.write_chunk(b"\rX");
        assert_eq!(g.cell(p.row, p.col), b'X');
        assert_eq!(g.next_piece(), Some(Piece { row, col: 0, n: 1 }));
        assert_eq!(g.next_piece(), None);
    }

    #[test]
    fn if_off_write_leaves_redraw() {
        let mut g = grid(80, 32, 1);
        // A writer with IF off updates only the grid; its cells stay
        // dirty for the next writer with IF on.
        g.write_chunk(b"hi");
        g.write_chunk(b"");
        let row = g.cursor().1;
        assert_eq!(g.next_piece(), Some(Piece { row, col: 0, n: 2 }));
        assert_eq!(g.next_piece(), None);
    }

    #[test]
    fn backspace_blanks_the_cell() {
        let mut g = grid(16, 32, 1);
        g.write_chunk(b"ab");
        drain(&mut g);
        g.write_chunk(&[0x08]);
        assert_eq!(g.cursor(), (1, 1));
        assert_eq!(g.cell(1, 1), b' ');
        assert_eq!(
            g.next_piece(),
            Some(Piece {
                row: 1,
                col: 1,
                n: 1
            })
        );
        // At the banner's edge backspace does nothing.
        g.write_chunk(&[0x08, 0x08, 0x08]);
        assert_eq!(g.cursor(), (0, 1));
    }

    #[test]
    fn glyph_origin_is_8x8() {
        assert_eq!(glyph_origin(3, 2), (24, 16));
    }

    #[test]
    fn cr_homes_column_same_row() {
        let mut g = grid(80, 32, 1); // 10 cols, 4 rows
        let row = g.cursor().1;
        g.write_chunk(b"ABC");
        assert_eq!(g.cursor(), (3, row));
        g.write_chunk(b"\r");
        assert_eq!(g.cursor(), (0, row));
        g.write_chunk(b"X");
        assert_eq!(g.cursor(), (1, row));
        assert_eq!([g.cell(row, 0), g.cell(row, 1)], [b'X', b'B']);
    }

    #[test]
    fn crlf_is_one_newline() {
        let mut g = grid(16, 32, 1);
        let start = g.cursor().1;
        g.write_chunk(b"A\r\n");
        assert_eq!(g.cursor(), (0, start + 1));
    }

    #[test]
    fn cr_paint_overwrites_in_place() {
        // Shell paint: CR, rewrite, pad shorter with spaces, CR, cursor.
        let mut g = grid(80, 32, 1);
        let home = g.cursor().1;
        g.write_chunk(b"ab");
        assert_eq!(g.cursor(), (2, home));
        g.write_chunk(b"\ra \ra");
        assert_eq!([g.cell(home, 0), g.cell(home, 1)], [b'a', b' ']);
        assert_eq!(g.cursor(), (1, home));
    }
}
