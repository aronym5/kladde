//! The shown page and the page cache.
//!
//! The last rastered page images are kept as a cache, so that going back
//! does not raster again. The module knows no Wayland and no MuPDF — which
//! makes it testable with numbers.

/// A retained page image: the bytes of a finished rastered page. Own copy,
/// independent of MuPDF's pixmap.
#[derive(Clone, Debug)]
pub struct PageImage {
    /// BGRX bytes: width x height x n.
    pub bytes: Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub stride: usize,
    /// Bytes per pixel (4 for BGRX, 3 for BGR).
    pub n: u8,
}

impl PageImage {
    /// Full 4-byte pixels, e.g. for tests.
    pub fn new(w: u32, h: u32) -> PageImage {
        PageImage::new_bpp(w, h, 4)
    }

    pub fn new_bpp(w: u32, h: u32, n: u8) -> PageImage {
        PageImage {
            bytes: vec![0u8; w as usize * h as usize * n as usize],
            w,
            h,
            stride: w as usize * n as usize,
            n,
        }
    }

    /// Row-wise copy from a pixmap — the pixmap may have stride padding;
    /// caching must result byte-identical to blitting.
    pub fn from_pixmap(px: &crate::Pixmap) -> PageImage {
        let n = px.n();
        let mut b = PageImage::new_bpp(px.w(), px.h(), n);
        let nu = n as usize;
        let src = px.bytes();
        for y in 0..px.h() as usize {
            let s = &src[y * px.stride()..y * px.stride() + px.w() as usize * nu];
            let d = &mut b.bytes[y * b.stride..y * b.stride + b.w as usize * nu];
            d[..s.len()].copy_from_slice(s);
        }
        b
    }
}

/// A cache entry: the image **and the scale it was rastered at**. Without
/// the stamp an image from an earlier window size is indistinguishable from
/// one from today's. `Clone` only for the tests; the production path is
/// [`Book::insert`].
#[derive(Clone)]
struct Entry {
    image: PageImage,
    scale: f32,
}

/// Are two scales the same? With relative tolerance, not with `==`: the
/// result goes into the job to the worker and comes back as an `f32` across
/// the channel boundary — bit-equality would be coincidence.
fn same_scale(a: f32, b: f32) -> bool {
    (a - b).abs() <= 1e-6 * a.abs().max(b.abs()).max(1.0)
}

/// The state of flipping through the document.
#[derive(Default)]
pub struct Book {
    /// The last page actually rendered (shown).
    pub shown: u16,
    /// The window size in which all cache entries hold — as a scale.
    /// **One** number for the whole document.
    scale: f32,
    /// One image per page of the document: `None` is the placeholder, `Some`
    /// the finished page image. Cached pages are never evicted: at the start
    /// almost everything is a placeholder, flipping fills it up — and an
    /// arbitrary capacity would evict the wrong page.
    cache: Vec<Option<Entry>>,
}

impl Book {
    /// Empty state showing page 0; one slot per page.
    pub fn new(n_pages: u16) -> Book {
        Book {
            shown: 0,
            // 0.0 means: no window size known yet, so **no** placeholder is
            // a valid image.
            scale: 0.0,
            cache: vec![None; n_pages as usize],
        }
    }

    /// The window size has changed: the scale applies to all future inserts
    /// from now on, and all previous entries are stale. The old images are
    /// not deleted: while the worker rasters, a readable image stands in the
    /// window instead of paper with a spinner. `true` if something changed.
    pub fn set_scale(&mut self, scale: f32) -> bool {
        if same_scale(self.scale, scale) {
            return false;
        }
        self.scale = scale;
        true
    }

    /// The scale the cache currently holds.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// The cache hit for `page` — **also a stale image**. This function says
    /// "something is there", not "it still fits" (see [`Self::is_current`]). A
    /// hit here is a reason to **show**, never a reason to skip the
    /// re-rastering.
    pub fn get_page(&self, page: u16) -> Option<&PageImage> {
        self.cache
            .get(page as usize)
            .and_then(|s| s.as_ref())
            .map(|e| &e.image)
    }

    /// Does the image for `page` fit the current window size? Only `true`
    /// means "needs no raster pass".
    pub fn is_current(&self, page: u16) -> bool {
        self.cache
            .get(page as usize)
            .and_then(|s| s.as_ref())
            .is_some_and(|e| same_scale(e.scale, self.scale))
    }

    /// Is there an image for `page` from a **different** window size? The
    /// caller needs it to show as an emergency fallback.
    pub fn is_stale(&self, page: u16) -> bool {
        self.cache
            .get(page as usize)
            .and_then(|s| s.as_ref())
            .is_some_and(|e| !same_scale(e.scale, self.scale))
    }

    /// Does a worker result rastered at `scale` fit the window size that
    /// **now** holds? (Between job and result the window may have changed.)
    /// The same computation as for the cache entries.
    pub fn result_matches(&self, scale: f32) -> bool {
        same_scale(scale, self.scale)
    }

    /// Insert a finished rastered page image; a filled slot is overwritten
    /// (the freshly rastered image wins). The image gets the **current**
    /// scale as stamp. A page outside the field is silently ignored instead
    /// of panicking in the middle of the swipe path.
    pub fn insert(&mut self, page: u16, image: PageImage) {
        let scale = self.scale;
        if let Some(slot) = self.cache.get_mut(page as usize) {
            *slot = Some(Entry { image, scale });
        }
    }

    /// Paints the cached page `page` to the place `at`. `false` if the page
    /// is not in the cache or its pixel width does not fit the framebuffer.
    /// Lives here (and not in the binary) because there are two callers —
    /// page change and swipe path — that must use the same placement
    /// computation.
    pub fn blit_from_cache(
        fb: &mut crate::FrameBuffer,
        book: &Book,
        page: u16,
        at: crate::Point,
    ) -> bool {
        let Some(image) = book.get_page(page) else {
            return false;
        };
        if image.n != fb.n() {
            return false;
        }
        crate::blit_rows(
            fb,
            &image.bytes,
            image.stride,
            image.w as i32,
            image.h as i32,
            image.n,
            crate::IRect::new(0, 0, image.w as i32, image.h as i32),
            at,
        );
        true
    }

    /// Where the cached page `page` would land — the same computation as
    /// [`Self::blit_from_cache`], so that a border sits exactly where the
    /// page lies. `None` if nothing is cached.
    pub fn page_rect(&self, page: u16, at: crate::Point) -> Option<crate::IRect> {
        let image = self.get_page(page)?;
        Some(crate::IRect::new(
            at.x,
            at.y,
            at.x + image.w as i32,
            at.y + image.h as i32,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_from_the_old_window_size_is_not_a_hit() {
        let mut v = Book::new(4);
        v.set_scale(1.0);
        v.insert(0, PageImage::new(800, 1200));
        assert!(v.is_current(0), "just rastered: valid");

        v.set_scale(1.4);

        // The image is still there — it is not deleted …
        assert!(v.get_page(0).is_some(), "the old image stays as emergency");
        // … but it is no hit anymore.
        assert!(!v.is_current(0), "wrong resolution must not count as a hit");
        assert!(v.is_stale(0));
    }

    /// After a size change **the whole** cache is stale, not just one page —
    /// `scale` is document-wide.
    #[test]
    fn after_a_size_change_the_whole_cache_is_stale() {
        let mut v = Book::new(6);
        v.set_scale(1.0);
        for s in 0..4 {
            v.insert(s, PageImage::new(10, 10));
        }
        v.set_scale(0.7);
        for s in 0..4 {
            assert!(v.is_stale(s), "page {} should be stale", s);
        }
        v.insert(2, PageImage::new(7, 7));
        assert!(v.is_current(2));
        assert!(v.is_stale(1), "only the re-rastered page is valid again");
    }

    /// `set_scale` reports only **real** changes — otherwise a constant rain
    /// of `configure` events would re-raster the document forever.
    #[test]
    fn set_scale_reports_only_real_changes() {
        let mut v = Book::new(3);
        assert!(v.set_scale(1.0), "from nothing to 1.0 is a change");
        assert!(!v.set_scale(1.0), "the same again is none");
        assert!(v.set_scale(1.25), "1.25 is another one");
        assert!(!v.set_scale(1.25 + 1e-9), "rounding is no change");
    }

    #[test]
    fn a_result_from_the_old_size_does_not_fit() {
        let mut v = Book::new(3);
        v.set_scale(1.0);
        assert!(v.result_matches(1.0), "rastered for the current size");
        v.set_scale(1.5);
        assert!(!v.result_matches(1.0), "for the old size: no longer fits");
        assert!(v.result_matches(1.5), "for the new size: fits");
    }

    /// `f32 == f32` is the wrong question here — rounding across the channel
    /// boundary is the normal case.
    #[test]
    fn scale_equality_tolerates_rounding_and_stays_sensitive() {
        assert!(same_scale(1.0, 1.0 + 1e-9));
        assert!(same_scale(0.0, 0.0));
        assert!(same_scale(100.0, 100.0 + 1e-6));
        assert!(!same_scale(1.0, 1.01));
        assert!(!same_scale(0.0, 1.0));
    }

    #[test]
    fn stale_images_are_not_discarded_on_change() {
        let mut v = Book::new(2);
        v.set_scale(1.0);
        v.insert(1, PageImage::new(20, 10));
        v.set_scale(2.0);
        assert!(v.get_page(1).is_some(), "the image must survive as emergency");
        assert_eq!(v.get_page(1).map(|b| b.w), Some(20));
    }

    #[test]
    fn cache_puts_in_and_takes_out() {
        let mut v = Book::new(6);
        v.insert(4, PageImage::new(2, 2));
        assert!(v.get_page(4).is_some());
        assert!(v.get_page(5).is_none()); // placeholder
    }

    /// A pattern where every byte reveals its origin: B = page number,
    /// G = column in the page image.
    fn page(num: u16, w: u32, h: u32) -> PageImage {
        let mut b = PageImage::new(w, h);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let o = y * b.stride + x * 4;
                b.bytes[o] = num as u8;
                b.bytes[o + 1] = x as u8;
            }
        }
        b
    }

    fn pixel(fb: &crate::FrameBuffer, x: i32, y: i32) -> (u8, u8) {
        let o = y as usize * fb.stride() + x as usize * 4;
        (fb.bytes()[o], fb.bytes()[o + 1])
    }

    #[test]
    fn blit_from_cache_paints_the_page_at_the_given_place() {
        let mut v = Book::new(8);
        v.insert(7, page(7, 20, 4));
        let mut fb = crate::FrameBuffer::new(40, 4, 4);
        fb.fill(0);
        assert!(Book::blit_from_cache(&mut fb, &v, 7, crate::Point::new(10, 0)));
        assert_eq!(pixel(&fb, 10, 0), (7, 0));
        assert_eq!(pixel(&fb, 29, 0), (7, 19));
        assert_eq!(pixel(&fb, 30, 0), (0, 0)); // right: nothing
    }

    #[test]
    fn blit_from_cache_without_hit_changes_nothing() {
        let v = Book::new(8);
        let mut fb = crate::FrameBuffer::new(40, 4, 4);
        fb.fill(0x33);
        assert!(!Book::blit_from_cache(&mut fb, &v, 7, crate::Point::ZERO));
        assert_eq!(pixel(&fb, 0, 0), (0x33, 0x33));
    }

    #[test]
    fn blit_from_cache_respects_the_offset() {
        let mut v = Book::new(8);
        v.insert(7, page(7, 20, 4));
        let mut fb = crate::FrameBuffer::new(40, 4, 4);
        fb.fill(0);
        assert!(Book::blit_from_cache(&mut fb, &v, 7, crate::Point::new(15, 0)));
        assert_eq!(pixel(&fb, 15, 0), (7, 0));
        assert_eq!(pixel(&fb, 16, 0), (7, 1));
        assert_eq!(pixel(&fb, 14, 0), (0, 0));
    }

    #[test]
    fn page_rect_lies_on_the_blitted_page() {
        let mut v = Book::new(8);
        v.insert(7, page(7, 20, 4));
        let mut fb = crate::FrameBuffer::new(40, 4, 4);
        let at = crate::Point::new(10, 0);
        assert_eq!(v.page_rect(7, at), Some(crate::IRect::new(10, 0, 30, 4)));
        assert!(Book::blit_from_cache(&mut fb, &v, 7, at));
        assert_eq!(pixel(&fb, 10, 0), (7, 0));
        assert_eq!(v.page_rect(5, at), None);
    }
}
