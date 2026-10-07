//! The target buffer and the blitter.
//!
//! MuPDF delivers with `fz_device_bgr` exactly the memory layout of a
//! little-endian `XRGB8888` buffer (byte 0 = B, byte 3 = unused). Copying is
//! therefore a `memcpy` per row and nothing else. The blitter is the only
//! place in the project where framebuffer bytes come into being.

use crate::IRect;

/// Bytes per pixel: B, G, R, X — exactly what an `XRGB8888` framebuffer expects.
pub const XRGB8888: u8 = 4;

/// The area around the page. Gray, not black: the page must stay readable as
/// a sheet, and dark enough that the white sheet stands out.
pub const BACKGROUND: u8 = 0x55;

/// The border at the page itself. Black, because it marks the place where
/// the paper ends.
pub const EDGE: u8 = 0x00;

/// The gap between two pages standing side by side, in pixels. From the
/// left: 1 pixel border of the left page, 2 pixels background, 1 pixel
/// border of the right page.
///
/// It must not be zero: [`Self::outline_rect`] paints one pixel *outside*
/// the rectangle (the page fills exactly `r`). If two pages stood flush,
/// the outer border of the left one would be the first content column of
/// the right one — both would lose a column.
pub const GAP: i32 = 4;

/// Where the page's top-left corner lies in the framebuffer, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const ZERO: Point = Point { x: 0, y: 0 };

    pub const fn new(x: i32, y: i32) -> Point {
        Point { x, y }
    }
}

/// A target buffer in XRGB8888. Width/height in pixels, `stride` in bytes;
/// `stride >= w * n` is allowed (GBM buffers align rows to 64 bytes), so the
/// blitter works with `stride` throughout.
pub struct FrameBuffer {
    bytes: Box<[u8]>,
    w: u32,
    h: u32,
    stride: usize,
    n: u8,
}

impl FrameBuffer {
    /// Heap buffer with tightly packed rows.
    pub fn new(w: u32, h: u32, n: u8) -> FrameBuffer {
        let stride = w as usize * n as usize;
        let len = stride * h as usize;
        FrameBuffer {
            bytes: vec![0u8; len].into_boxed_slice(),
            w,
            h,
            stride,
            n,
        }
    }

    pub fn width(&self) -> u32 {
        self.w
    }
    pub fn height(&self) -> u32 {
        self.h
    }
    pub fn stride(&self) -> usize {
        self.stride
    }
    /// Components per pixel (4 = XRGB8888).
    pub fn n(&self) -> u8 {
        self.n
    }

    pub fn bytes(&self) -> &[u8] {
        self.bytes.as_ref()
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    pub fn fill(&mut self, v: u8) {
        self.bytes_mut().fill(v);
    }

    /// Fills `r` with `v`, clipped to the buffer. Clipping is the normal
    /// case, not an error: the page sticks out over the edge when swiping.
    pub fn fill_rect(&mut self, r: IRect, v: u8) {
        if r.is_empty() {
            return;
        }
        let y0 = r.y0.max(0) as usize;
        let y1 = (r.y1.min(self.h as i32)).max(0) as usize;
        let x0 = r.x0.max(0) as usize;
        let x1 = (r.x1.min(self.w as i32)).max(0) as usize;
        if y1 <= y0 || x1 <= x0 {
            return;
        }
        let n = self.n as usize;
        let row_len = (x1 - x0) * n;
        for y in y0..y1 {
            let a = y * self.stride() + x0 * n;
            self.bytes_mut()[a..a + row_len].fill(v);
        }
    }

    /// Sets **one** pixel to `v`, if it lies in the buffer. For shapes that
    /// are no rectangles (the loading spinner draws a ring from single
    /// pixels). Outside is silently dropped.
    pub fn set_pixel(&mut self, x: i32, y: i32, v: u8) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return;
        }
        let n = self.n as usize;
        let a = y as usize * self.stride() + x as usize * n;
        self.bytes_mut()[a..a + n].fill(v);
    }

    /// A one-pixel border **outside** `r` — not inside: the page fills
    /// exactly `r`, a border inside the rectangle would mill a row and a
    /// column off the sheet. Whatever would extend over the window edge is
    /// omitted.
    pub fn outline_rect(&mut self, r: IRect, v: u8) {
        if r.is_empty() {
            return;
        }
        let (x0, y0) = (r.x0 - 1, r.y0 - 1);
        let (x1, y1) = (r.x1, r.y1);
        self.fill_rect(IRect::new(x0, y0, x1, y0 + 1), v);
        self.fill_rect(IRect::new(x0, y1, x1, y1 + 1), v);
        self.fill_rect(IRect::new(x0, y0, x0 + 1, y1), v);
        self.fill_rect(IRect::new(x1, y0, x1 + 1, y1), v);
    }

    /// The background: gray all around, paper where the page goes. The
    /// border is deliberately *not* part of it, but a separate call
    /// afterwards — the "never white" background would otherwise paint over
    /// it as soon as the previous page is bigger than the new one.
    pub fn background(&mut self, page: IRect) {
        self.fill(BACKGROUND);
        self.fill_rect(page, 0xFF);
    }

    /// Fills only the rows `y0..y1` with `v`, clipped to the frame. For the
    /// swipe path: there only the row of the page needs blackening; the
    /// letter margin is already in place.
    pub fn fill_rows(&mut self, y0: i32, y1: i32, v: u8) {
        let n = self.n as usize;
        if y1 <= y0 {
            return;
        }
        let lo = y0.max(0) as usize;
        let hi = (y1.min(self.h as i32)).max(0) as usize;
        if hi <= lo {
            return;
        }
        let row = self.w as usize * n;
        for y in lo..hi {
            let a = y * self.stride();
            self.bytes_mut()[a..a + row].fill(v);
        }
    }
}

impl std::fmt::Debug for FrameBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FrameBuffer {}x{} stride={} n={}", self.w, self.h, self.stride, self.n)
    }
}

/// The blitter as a free function, so that it is testable without MuPDF.
/// Copies the region `rect` out of `src` so that `rect.x0` lands on `at.x`.
/// Nothing is **converted**: `n` must be equal on both sides, otherwise
/// nothing is copied.
pub fn blit_rows(
    fb: &mut FrameBuffer,
    src: &[u8],
    src_stride: usize,
    src_w: i32,
    src_h: i32,
    n: u8,
    rect: IRect,
    at: Point,
) -> IRect {
    let none = IRect::new(0, 0, 0, 0);
    let nn = n as i32;
    if n == 0 || n != fb.n || src_stride == 0 || src_w <= 0 || src_h <= 0 {
        return none;
    }
    // A too-short source is a programming error; the render path has no
    // panic, so copy nothing (the caller sees it in the empty return).
    let need = (src_stride * (src_h as usize - 1)) + (src_w as usize * n as usize);
    if src.len() < need {
        return none;
    }

    // 1. Clip the region against the source.
    let cx0 = rect.x0.max(0);
    let cy0 = rect.y0.max(0);
    let cx1 = rect.x1.min(src_w);
    let cy1 = rect.y1.min(src_h);
    if cx1 <= cx0 || cy1 <= cy0 {
        return none;
    }

    // 2. Compute target coordinates and clip against the framebuffer. When
    //    clipped on the left, the *source* column moves along — otherwise
    //    one copies the first pixels of the region a second time.
    let dxo = at.x - rect.x0;
    let dyo = at.y - rect.y0;
    let dx0 = (cx0 + dxo).max(0);
    let dx1 = (cx1 + dxo).min(fb.w as i32);
    if dx1 <= dx0 {
        return none;
    }
    let sx0 = dx0 - dxo;
    debug_assert!(sx0 >= cx0 && sx0 < cx1);
    let fbh = fb.h as i32;
    let dst_stride = fb.stride;
    // SAFETY: below via raw parts — the rows were clipped against source
    // and framebuffer above.
    let dst = fb.bytes.as_mut_ptr();
    let nbytes = ((dx1 - dx0) * nn) as usize;

    // 3. Row by row: one memcpy per row. first/last remember the rows
    //    actually copied in target coordinates.
    let mut first: Option<i32> = None;
    let mut last = 0i32;
    for sy in cy0..cy1 {
        let dy = sy + dyo;
        if dy < 0 || dy >= fbh {
            continue;
        }
        let s = (sy as usize) * src_stride + (sx0 as usize) * n as usize;
        let d = (dy as usize) * dst_stride + (dx0 as usize) * n as usize;
        // SAFETY: both rows were clipped against source resp. framebuffer
        // above, d lies in [0, len). Overlap is impossible.
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr().add(s), dst.add(d), nbytes);
        }
        if first.is_none() {
            first = Some(dy);
        }
        last = dy + 1;
    }
    match first {
        Some(y0) => IRect::new(dx0, y0, dx1, last),
        None => none,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pattern where every byte reveals its position: B = y, G = x.
    fn pattern(w: i32, h: i32, n: u8, stride: usize) -> Vec<u8> {
        let mut v = vec![0u8; stride * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let o = y * stride + x * n as usize;
                v[o] = y as u8;
                v[o + 1] = x as u8;
            }
        }
        v
    }

    fn fb(w: u32, h: u32) -> FrameBuffer {
        FrameBuffer::new(w, h, 4)
    }

    fn b(f: &FrameBuffer, x: i32, y: i32) -> u8 {
        f.bytes()[y as usize * f.stride() + x as usize * 4]
    }

    #[test]
    fn blit_copies_row_wise_correctly() {
        let mut f = fb(8, 4);
        let src = pattern(8, 4, 4, 32);
        let got = blit_rows(&mut f, &src, 32, 8, 4, 4, IRect::new(0, 0, 8, 4), Point::ZERO);
        assert_eq!(got, IRect::new(0, 0, 8, 4));
        let bytes = f.bytes();
        for y in 0..4usize {
            for x in 0..8usize {
                let o = y * f.stride() + x * 4;
                assert_eq!(bytes[o], y as u8, "B at ({},{})", x, y);
                assert_eq!(bytes[o + 1], x as u8, "G at ({},{})", x, y);
            }
        }
    }

    #[test]
    fn blit_clips_at_right_and_bottom_edge() {
        let mut f = fb(8, 4);
        let src = pattern(8, 4, 4, 32);
        // The rectangle (4,2)-(12,9) sits at (4,2) and sticks out right and
        // bottom — just like the cache blitter (book.rs) when a page sticks
        // out over the edge, e.g. slid during a swipe.
        let got = blit_rows(&mut f, &src, 32, 8, 4, 4, IRect::new(4, 2, 12, 9), Point::new(4, 2));
        assert_eq!(got, IRect::new(4, 2, 8, 4));
        let o = 2 * f.stride() + 4 * 4;
        assert_eq!(&f.bytes()[o..o + 2], &[2, 4], "B = source y 2, G = source x 4");
    }

    #[test]
    fn blit_shifts_left_and_up() {
        let mut f = fb(8, 8);
        let src = pattern(8, 8, 4, 32);
        let got = blit_rows(&mut f, &src, 32, 8, 8, 4, IRect::new(0, 0, 8, 8), Point::new(-2, -1));
        assert_eq!(got, IRect::new(0, 0, 6, 7));
        // Source pixel (0,0) is gone, (2,1) must now stand at (0,0).
        assert_eq!(f.bytes()[0], 1);
        assert_eq!(f.bytes()[1], 2);
    }

    #[test]
    fn blit_respects_src_stride_with_padding() {
        let mut f = fb(4, 2);
        // Source 3 pixels wide, 64 bytes per row: the padding bytes must not
        // shift the target.
        let src = pattern(3, 2, 4, 64);
        let got = blit_rows(&mut f, &src, 64, 3, 2, 4, IRect::new(0, 0, 3, 2), Point::ZERO);
        assert_eq!(got, IRect::new(0, 0, 3, 2));
        let b = f.bytes();
        assert_eq!(&b[0..4], &[0, 0, 0, 0]);
        assert_eq!(&b[4..8], &[0, 1, 0, 0]);
        assert_eq!(&b[f.stride()..f.stride() + 4], &[1, 0, 0, 0]);
    }

    #[test]
    fn blit_shifts_down_right() {
        let mut f = fb(8, 8);
        let src = pattern(8, 8, 4, 32);
        let got = blit_rows(&mut f, &src, 32, 8, 8, 4, IRect::new(0, 0, 8, 8), Point::new(1, 2));
        assert_eq!(got, IRect::new(1, 2, 8, 8));
        // Source pixel (0,0) now sits at (1,2), (0,1) at (1,3).
        assert_eq!(&f.bytes()[2 * f.stride() + 4..2 * f.stride() + 6], &[0, 0]);
        assert_eq!(&f.bytes()[3 * f.stride() + 4..3 * f.stride() + 6], &[1, 0]);
    }

    #[test]
    fn fill_rows_touches_only_named_rows() {
        let mut f = fb(4, 5);
        f.fill(0x11);
        f.fill_rows(1, 3, 0xFF);
        let b = f.bytes();
        for y in 0..5usize {
            let o = y * f.stride();
            for x in 0..4usize {
                let p = o + x * 4;
                let want = if (1..3).contains(&y) { 0xFF } else { 0x11 };
                assert_eq!(b[p], want, "row {}", y);
            }
        }
    }

    #[test]
    fn fill_rows_clips_at_edge_and_ignores_nonsense() {
        let mut f = fb(2, 3);
        f.fill(0x22);
        f.fill_rows(-5, 99, 0xFF);
        f.fill_rows(3, 1, 0xFF);
        for y in 0..3usize {
            assert_eq!(f.bytes()[y * f.stride()], 0xFF, "row {}", y);
        }
        f.fill_rows(9, 12, 0x00);
        assert_eq!(f.bytes()[0], 0xFF, "outside nothing may happen");
    }

    #[test]
    fn blit_mismatched_formats_copy_nothing() {
        let mut f = fb(4, 4);
        let src = pattern(4, 4, 3, 12);
        assert!(blit_rows(&mut f, &src, 12, 4, 4, 3, IRect::new(0, 0, 4, 4), Point::ZERO).is_empty());
    }

    #[test]
    fn blit_too_short_source_copies_nothing_instead_of_panicking() {
        let mut f = fb(4, 4);
        let src = vec![0u8; 8]; // claims 4x4, nothing is delivered
        assert!(blit_rows(&mut f, &src, 16, 4, 4, 4, IRect::new(0, 0, 4, 4), Point::ZERO).is_empty());
    }

    #[test]
    fn blit_completely_outside_copies_nothing() {
        let mut f = fb(4, 4);
        let src = pattern(4, 4, 4, 16);
        for at in [Point::new(100, 0), Point::new(0, -100), Point::new(-100, -100)] {
            assert!(blit_rows(&mut f, &src, 16, 4, 4, 4, IRect::new(0, 0, 4, 4), at).is_empty());
        }
    }

    /// The picture the wish describes: gray all around, white paper in the
    /// page, black border at it.
    #[test]
    fn background_draws_gray_white_and_edge() {
        let mut f = fb(40, 20);
        let page = IRect::new(10, 4, 30, 16);
        f.background(page);
        f.outline_rect(page, EDGE);
        assert_eq!(b(&f, 20, 0), BACKGROUND);
        assert_eq!(b(&f, 20, 19), BACKGROUND);
        assert_eq!(b(&f, 0, 10), BACKGROUND);
        assert_eq!(b(&f, 39, 10), BACKGROUND);
        assert_eq!(b(&f, 20, 3), EDGE, "top");
        assert_eq!(b(&f, 20, 16), EDGE, "bottom");
        assert_eq!(b(&f, 9, 10), EDGE, "left");
        assert_eq!(b(&f, 30, 10), EDGE, "right");
        assert_eq!(b(&f, 10, 4), 0xFF);
        assert_eq!(b(&f, 29, 15), 0xFF);
        assert_eq!(b(&f, 8, 10), BACKGROUND);
        assert_eq!(b(&f, 31, 10), BACKGROUND);
    }

    /// The border must never reach into the page: for a page that fills the
    /// window exactly, everything stays paper.
    #[test]
    fn edge_does_not_eat_the_page() {
        let mut f = fb(20, 10);
        f.background(IRect::new(0, 0, 20, 10));
        for y in 0..10 {
            for x in 0..20 {
                assert_eq!(b(&f, x, y), 0xFF, "at {},{}", x, y);
            }
        }
    }

    /// A page bigger than the window sticks out and gets clipped — without
    /// crashing.
    #[test]
    fn page_bigger_than_window_gets_clipped() {
        let mut f = fb(40, 20);
        f.background(IRect::new(0, 0, 60, 30));
        assert_eq!(b(&f, 0, 0), 0xFF, "paper to the edge");
        assert_eq!(b(&f, 39, 19), 0xFF, "paper to the edge");
        assert_eq!(f.width(), 40);
        assert_eq!(f.height(), 20);
    }

    #[test]
    fn negative_placement_gets_clipped_not_shifted() {
        let mut f = fb(40, 20);
        let page = IRect::new(-10, -5, 10, 5);
        f.background(page);
        f.outline_rect(page, EDGE);
        assert_eq!(b(&f, 0, 0), 0xFF, "visible part is paper");
        assert_eq!(b(&f, 10, 2), EDGE, "right edge");
        assert_eq!(b(&f, 11, 2), BACKGROUND, "beyond is background");
        assert_eq!(b(&f, 5, 5), EDGE, "bottom edge");
    }

    /// Two pages with the gap between them: both keep all their columns, and
    /// each border belongs to one page.
    #[test]
    fn two_pages_with_gap_keep_their_content() {
        let mut f = fb(40, 20);
        f.fill(BACKGROUND);
        let left = IRect::new(0, 0, 20, 20);
        let right = IRect::new(24, 0, 44, 20);
        // Order as in the swipe path: first the neighbor, then the base.
        f.fill_rect(right, 0xFF);
        f.outline_rect(right, EDGE);
        f.fill_rect(left, 0xFF);
        f.outline_rect(left, EDGE);
        assert_eq!(b(&f, 0, 10), 0xFF, "left begins unharmed");
        assert_eq!(b(&f, 19, 10), 0xFF, "left ends unharmed");
        assert_eq!(b(&f, 24, 10), 0xFF, "right begins unharmed");
        assert_eq!(b(&f, 20, 10), EDGE, "border of the left page");
        assert_eq!(b(&f, 21, 10), BACKGROUND, "gray background");
        assert_eq!(b(&f, 22, 10), BACKGROUND, "gray background");
        assert_eq!(b(&f, 23, 10), EDGE, "border of the right page");
    }

    #[test]
    fn edge_that_would_leave_the_window_is_omitted() {
        let mut f = fb(40, 20);
        f.background(IRect::new(-5, -5, 35, 15));
        assert_eq!(b(&f, 0, 0), 0xFF);
        assert_eq!(b(&f, 0, 19), BACKGROUND, "bottom is still background");
    }
}
