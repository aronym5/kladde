//! Kladde - a thin, safe wrapper around MuPDF.
//!
//! The only `unsafe` area is the `extern` block below; everything above it
//! is normal Rust with Drop responsibility. MuPDF is never called
//! directly, always through the C shim (`shim/kladde_shim.h`) — required because of
//! setjmp/longjmp across Rust frames.

use std::ffi::{c_char, c_double, c_int, CStr, CString};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const ERR_CAP: usize = 512;

/// true if built against the stub (no MuPDF available).
pub const STUB: bool = cfg!(kladde_stub);

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(e.to_string())
    }
}

// ---------------------------------------------------------------------
// Rohes FFI
// ---------------------------------------------------------------------

#[repr(C)]
pub struct ffi_ctx {
    _p: [u8; 0],
}
#[repr(C)]
pub struct ffi_doc {
    _p: [u8; 0],
}
#[repr(C)]
pub struct ffi_list {
    _p: [u8; 0],
}
#[repr(C)]
pub struct ffi_pix {
    _p: [u8; 0],
}

extern "C" {
    fn kl_ctx_new(store_bytes: usize) -> *mut ffi_ctx;
    fn kl_ctx_drop(c: *mut ffi_ctx);
    fn kl_mupdf_version() -> *const c_char;

    fn kl_doc_open(c: *mut ffi_ctx, path: *const c_char, err: *mut c_char, errlen: usize) -> *mut ffi_doc;
    fn kl_doc_drop(d: *mut ffi_doc);
    fn kl_doc_page_count(d: *mut ffi_doc) -> c_int;
    fn kl_page_box(c: *mut ffi_ctx, d: *mut ffi_doc, page: c_int, out: *mut c_double) -> c_int;

    fn kl_parse(c: *mut ffi_ctx, d: *mut ffi_doc, page: c_int, err: *mut c_char, errlen: usize) -> *mut ffi_list;
    fn kl_list_drop(l: *mut ffi_list);

    fn kl_pix_new(c: *mut ffi_ctx, w: c_int, h: c_int, alpha: c_int) -> *mut ffi_pix;
    fn kl_pix_drop(p: *mut ffi_pix);
    fn kl_pix_info(p: *mut ffi_pix, samples: *mut *mut u8, stride: *mut isize, n: *mut c_int) -> c_int;

    fn kl_raster(
        c: *mut ffi_ctx,
        l: *mut ffi_list,
        scale: f32,
        ox: f32,
        oy: f32,
        p: *mut ffi_pix,
        x0: c_int,
        y0: c_int,
        x1: c_int,
        y1: c_int,
        out_nodes: *mut c_int,
        err: *mut c_char,
        errlen: usize,
    ) -> c_int;
}

fn cstr(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

pub fn mupdf_version() -> &'static str {
    unsafe { CStr::from_ptr(kl_mupdf_version()) }
        .to_str()
        .unwrap_or("unknown")
}

// ---------------------------------------------------------------------
// Geometrie
// ---------------------------------------------------------------------

/// Rechteck in MuPDF-Punkten (1/72 Zoll), inklusive /Rotate.
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    pub fn w(&self) -> f64 {
        (self.x1 - self.x0).max(0.0)
    }
    pub fn h(&self) -> f64 {
        (self.y1 - self.y0).max(0.0)
    }
}

/// Rechteck in Pixeln, halboffenes Intervall [x0, x1) x [y0, y1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl IRect {
    pub fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> IRect {
        IRect { x0, y0, x1, y1 }
    }
    pub fn w(&self) -> i32 {
        self.x1 - self.x0
    }
    pub fn h(&self) -> i32 {
        self.y1 - self.y0
    }
    /// Nothing left - after clipping against source or framebuffer.
    pub fn is_empty(&self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }
}

/// Skalierung, um die Seite passend einzufassen. `target` ist in Pixeln.
pub fn fit_scale(r: &Rect, target_w: u32, target_h: u32) -> f32 {
    if r.w() <= 0.0 || r.h() <= 0.0 {
        return 1.0;
    }
    (target_w as f64 / r.w()).min(target_h as f64 / r.h()) as f32
}

/// Page size in pixels at the given scale.
pub fn pixels(r: &Rect, scale: f32) -> (u32, u32) {
    let w = ((r.w() * scale as f64).round() as i64).max(1) as u32;
    let h = ((r.h() * scale as f64).round() as i64).max(1) as u32;
    (w, h)
}

/// The **minimum** of [`fit_scale`] over all page boxes — so every page fits
/// the window at the same magnification, and the scale does not jump when
/// flipping. The price is the smallest page. With no pages (or only
/// unreadable boxes) 1.0 is used instead of `NaN`.
pub fn fit_scale_pages(boxes: impl IntoIterator<Item = Rect>, target_w: u32, target_h: u32) -> f32 {
    let mut smallest = f32::INFINITY;
    for b in boxes {
        // Skip broken boxes: `fit_scale` would yield 1.0 for them, and as a
        // minimum 1.0 would visibly shrink the whole document.
        if b.w() <= 0.0 || b.h() <= 0.0 {
            continue;
        }
        smallest = smallest.min(fit_scale(&b, target_w, target_h));
    }
    if smallest.is_finite() && smallest > 0.0 {
        smallest
    } else {
        1.0
    }
}

/// [`fit_scale_pages`] over the pages of a document.
/// Computed once per window size, not per page change.
pub fn fit_scale_doc(doc: &Doc, target_w: u32, target_h: u32) -> f32 {
    let boxes: Vec<Rect> = (0..doc.pages()).filter_map(|i| doc.page_box(i).ok()).collect();
    fit_scale_pages(boxes, target_w, target_h)
}

// ---------------------------------------------------------------------
// Engine / Document / DisplayList / Pixmap
// ---------------------------------------------------------------------

/// A MuPDF context. Not `Send`: MuPDF's internal store lives per context
/// and is not thread-safe. The viewer therefore uses **one `Engine`
/// per worker thread**; each opens the document itself (via `mmap`
/// mapped multiple times, the kernel's page cache is shared).
pub struct Engine {
    ctx: *mut ffi_ctx,
}

impl Engine {
    /// `store_bytes` limits MuPDF's resource store (fonts, images,
    /// object cache). 0 = unlimited. On 512-MB devices: 4–8 MB.
    pub fn new(store_bytes: usize) -> Result<Engine> {
        let p = unsafe { kl_ctx_new(store_bytes) };
        if p.is_null() {
            return Err(Error("fz_new_context() failed".into()));
        }
        Ok(Engine { ctx: p })
    }

    pub fn open(&self, path: &Path) -> Result<Doc<'_>> {
        let cpath = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error("path contains a NUL byte".into()))?;
        let mut err = [0 as c_char; ERR_CAP];
        let p = unsafe { kl_doc_open(self.ctx, cpath.as_ptr(), err.as_mut_ptr(), err.len()) };
        if p.is_null() {
            return Err(Error(format!("{}: {}", path.display(), cstr(&err))));
        }
        Ok(Doc { eng: self, p })
    }

    /// Rastet ein Rechteck einer Display List in `px` und meldet, ob etwas
    /// was rendered (`false` for an empty page). The `nodes` counter
    /// belongs to the caller.
    pub fn raster(
        &self,
        list: &DisplayList<'_>,
        scale: f32,
        ox: f32,
        oy: f32,
        px: &mut Pixmap<'_>,
        t: IRect,
        nodes: &mut i32,
    ) -> Result<bool> {
        let mut err = [0 as c_char; ERR_CAP];
        let rc = unsafe {
            kl_raster(
                self.ctx,
                list.p,
                scale,
                ox,
                oy,
                px.p,
                t.x0 as c_int,
                t.y0 as c_int,
                t.x1 as c_int,
                t.y1 as c_int,
                nodes as *mut c_int,
                err.as_mut_ptr(),
                err.len(),
            )
        };
        match rc {
            0 => Ok(true),
            1 => Ok(false), // empty page
            _ => Err(Error(cstr(&err))),
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe { kl_ctx_drop(self.ctx) }
    }
}

pub struct Doc<'a> {
    eng: &'a Engine,
    p: *mut ffi_doc,
}

impl<'a> Doc<'a> {
    pub fn pages(&self) -> u16 {
        let n = unsafe { kl_doc_page_count(self.p) };
        if n <= 0 {
            0
        } else {
            n.min(u16::MAX as c_int) as u16
        }
    }

    pub fn page_box(&self, i: u16) -> Result<Rect> {
        let mut out = [0f64; 4];
        let rc = unsafe { kl_page_box(self.eng.ctx, self.p, i as c_int, out.as_mut_ptr()) };
        if rc != 0 {
            return Err(Error(format!("page {}: box unreadable", i + 1)));
        }
        Ok(Rect {
            x0: out[0],
            y0: out[1],
            x1: out[2],
            y1: out[3],
        })
    }

    /// Interpretiert den Content-Stream einmalig (Phase 1).
    pub fn parse(&self, i: u16) -> Result<DisplayList<'a>> {
        let mut err = [0 as c_char; ERR_CAP];
        let p = unsafe {
            kl_parse(self.eng.ctx, self.p, i as c_int, err.as_mut_ptr(), err.len())
        };
        if p.is_null() {
            return Err(Error(format!("page {}: {}", i + 1, cstr(&err))));
        }
        Ok(DisplayList { p, _eng: self.eng })
    }
}

impl Drop for Doc<'_> {
    fn drop(&mut self) {
        unsafe { kl_doc_drop(self.p) }
    }
}

/// Result of phase 1: the prepared drawing list of a page —
/// resolution-independent, replayable any number of times and at any
/// smallness.
pub struct DisplayList<'a> {
    p: *mut ffi_list,
    _eng: &'a Engine,
}

impl Drop for DisplayList<'_> {
    fn drop(&mut self) {
        unsafe { kl_list_drop(self.p) }
    }
}

pub struct Pixmap<'a> {
    /// Only for the lifetime binding: `Pixmap` must not outlive the `Engine`
    /// its samples come from. Never read.
    _eng: &'a Engine,
    p: *mut ffi_pix,
    ptr: *mut u8,
    w: u32,
    h: u32,
    stride: usize,
    n: u8,
}

impl<'a> Pixmap<'a> {
    /// `alpha = true` ergibt 4 Byte/Pixel im BGR+-Format — einen 1:1-Blit
    /// in ein XRGB8888-Framebuffer. `false` ergibt 3 Byte/Pixel.
    pub fn new(eng: &'a Engine, w: u32, h: u32, alpha: bool) -> Result<Pixmap<'a>> {
        let p = unsafe { kl_pix_new(eng.ctx, w as c_int, h as c_int, alpha as c_int) };
        if p.is_null() {
            return Err(Error(format!("Pixmap {}x{} cannot be created", w, h)));
        }
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut stride: isize = 0;
        let mut n: c_int = 0;
        let rc = unsafe { kl_pix_info(p, &mut ptr, &mut stride, &mut n) };
        if rc != 0 || ptr.is_null() || stride <= 0 || n <= 0 {
            unsafe { kl_pix_drop(p) };
            return Err(Error("Pixmap: kl_pix_info returned nothing usable".into()));
        }
        Ok(Pixmap {
            _eng: eng,
            p,
            ptr,
            w,
            h,
            stride: stride as usize,
            n: n as u8,
        })
    }

    pub fn w(&self) -> u32 {
        self.w
    }
    pub fn h(&self) -> u32 {
        self.h
    }
    /// Komponenten pro Pixel: 3 (BGR) oder 4 (BGR+Alpha).
    pub fn n(&self) -> u8 {
        self.n
    }
    pub fn stride(&self) -> usize {
        self.stride
    }
    pub fn len(&self) -> usize {
        self.stride * self.h as usize
    }

    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len()) }
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len()) }
    }

    pub fn fill(&mut self, v: u8) {
        self.bytes_mut().fill(v);
    }

    /// Share of pixels that are **not pure white**, in percent.
    /// Sanity check against measurements where nothing was rendered.
    /// "Not white" and not "dark": thin text rasters as light
    /// anti-aliasing gray and would vanish under a darkness threshold.
    pub fn coverage_percent(&self) -> f64 {
        let n = self.n as usize;
        let comps = n.min(3);
        let b = self.bytes();
        let mut marked = 0u64;
        let mut total = 0u64;
        for y in 0..self.h as usize {
            let row_start = y * self.stride;
            let row = &b[row_start..row_start + self.w as usize * n];
            for px in row.chunks_exact(n) {
                total += 1;
                if px[..comps].iter().any(|&c| c < 250) {
                    marked += 1;
                }
            }
        }
        if total == 0 {
            0.0
        } else {
            100.0 * marked as f64 / total as f64
        }
    }
}

impl Drop for Pixmap<'_> {
    fn drop(&mut self) {
        unsafe { kl_pix_drop(self.p) }
    }
}

// The viewer, layer by layer; each knows only the one below it:
// output (where a finished image goes), framebuffer (target buffer + blitter),
// render (view of a page + stats), prefetch (the worker's raster path),
// book (cache), swipe (swipe path), spinner (loading spinner),
// gesture (gesture → effect), wayland (window).
// The render path knows no DRM and the compositor knows no MuPDF.
pub mod framebuffer;
pub mod gesture;
pub mod output;
pub mod prefetch;
pub mod render;
pub mod swipe;
pub mod spinner;
pub mod book;
pub mod wayland;

pub use framebuffer::{blit_rows, BACKGROUND, FrameBuffer, EDGE, Point, XRGB8888};
pub use output::Wayland;
pub use render::{RenderStats, Viewport};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn irect_new_builds() {
        let r = IRect::new(1, 2, 3, 4);
        assert_eq!((r.w(), r.h()), (2, 2));
        assert!(!r.is_empty());
        assert!(IRect::new(3, 3, 3, 3).is_empty());
    }

    #[test]
    fn fit_scale_uses_the_smaller_axis() {
        let r = Rect { x0: 0.0, y0: 0.0, x1: 595.0, y1: 842.0 };
        let s = fit_scale(&r, 1080, 1920);
        assert!(s < 1.82 && s > 1.81);
        assert_eq!(pixels(&r, s), (1080, 1528));
    }

    /// The essence of the document scale: it is the **minimum** over
    /// all pages, not per page its own minimum.
    #[test]
    fn fit_scale_pages_takes_smallest_of_all() {
        let tall = Rect { x0: 0.0, y0: 0.0, x1: 200.0, y1: 200.0 };
        let wide = Rect { x0: 0.0, y0: 0.0, x1: 400.0, y1: 100.0 };
        let (fw, fh) = (600u32, 400u32);

        assert!(fit_scale(&tall, fw, fh) > fit_scale(&wide, fw, fh));
        let s = fit_scale_pages([tall, wide], fw, fh);
        assert_eq!(s, fit_scale(&wide, fw, fh));
        assert_eq!(fit_scale(&tall, fw, fh) as f64, 2.0);
        assert!(s < fit_scale(&tall, fw, fh));

        // Die hohe Seite passt jetzt rundum ins Fenster (300x300 in 600x400)
        // — with margin on all four sides. That works only with a uniform
        // scale, because `fit_scale` always fills at least one axis to the edge.
        let (w, h) = pixels(&tall, s);
        assert_eq!((w, h), (300, 300));
        assert!(w < fw && h < fh, "margin on all sides possible");
    }

    /// Order does not matter, and a broken box (width or height 0) must not
    /// pull the result down to 1.0.
    #[test]
    fn fit_scale_pages_is_order_independent_and_robust() {
        let a = Rect { x0: 0.0, y0: 0.0, x1: 400.0, y1: 100.0 };
        let b = Rect { x0: 0.0, y0: 0.0, x1: 200.0, y1: 200.0 };
        let degenerate = Rect { x0: 0.0, y0: 0.0, x1: 0.0, y1: 0.0 };
        let s1 = fit_scale_pages([a, b], 600, 400);
        let s2 = fit_scale_pages([b, a], 600, 400);
        assert_eq!(s1, s2);
        assert_eq!(fit_scale_pages([a, b, degenerate], 600, 400), s1);
        assert_eq!(fit_scale_pages([], 600, 400), 1.0);
    }
}
