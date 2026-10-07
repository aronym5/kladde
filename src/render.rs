//! The view of a page (scale + placement in the framebuffer) and the
//! statistics of one raster pass.

use std::time::Instant;

use crate::framebuffer::Point;
use crate::IRect;

/// How a page is laid into the framebuffer: `scale` is points→pixels,
/// `at` the page's offset within the framebuffer.
#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    pub scale: f32,
    pub at: Point,
}

impl Viewport {
    /// The document scale (see [`crate::fit_scale_doc`]); the page starts at
    /// (0, 0).
    pub fn scaled(scale: f32) -> Viewport {
        Viewport {
            scale,
            at: Point::ZERO,
        }
    }

    pub fn with_offset(mut self, at: Point) -> Viewport {
        self.at = at;
        self
    }

    /// The rectangle the page occupies in the framebuffer — recomputed per
    /// page, because height *and* placement depend on the page box.
    pub fn bounds(&self, sw: u32, sh: u32) -> IRect {
        IRect::new(self.at.x, self.at.y, self.at.x + sw as i32, self.at.y + sh as i32)
    }
}

/// Everything worth knowing about one raster pass. Every millisecond in
/// `total_ms` must belong to exactly one named phase;
/// [`RenderStats::unaccounted_ms`] makes that checkable.
#[derive(Debug, Clone, Default)]
pub struct RenderStats {
    /// Phase 1: interpret the content stream, resolution-independent.
    pub parse_ms: f64,
    /// Allocate the pixmap and fill it white.
    pub alloc_ms: f64,
    /// Phase 2: raster the page (the single MuPDF call).
    pub raster_ms: f64,
    /// The full pixmap pass for `coverage` — pure diagnostics.
    pub coverage_ms: f64,
    /// Extract the page image from the pixmap (row by row, because of
    /// stride padding).
    pub cache_ms: f64,
    /// The wall-clock time of the whole pass — covers all phases.
    pub total_ms: f64,
    /// Display-list nodes MuPDF counted during replay — a measure of page
    /// complexity.
    pub nodes: i32,
    /// Share of non-white pixels. Sanity check: an empty page looks just as
    /// fast as a full one.
    pub coverage: f64,
    pub empty_page: bool,
}

impl RenderStats {
    /// What sits in `total_ms` but in no named phase. Should be near zero;
    /// if it is not, a phase is not measured.
    pub fn unaccounted_ms(&self) -> f64 {
        self.total_ms
            - (self.parse_ms
                + self.alloc_ms
                + self.raster_ms
                + self.coverage_ms
                + self.cache_ms)
    }
}

pub(crate) fn ms_of(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unaccounted_reports_unmeasured_time() {
        let st = RenderStats {
            parse_ms: 1.0,
            alloc_ms: 2.0,
            raster_ms: 81.0,
            coverage_ms: 1.5,
            cache_ms: 2.5,
            total_ms: 220.0,
            ..Default::default()
        };
        // 220 - (1 + 2 + 81 + 1.5 + 2.5) = 132
        assert!((st.unaccounted_ms() - 132.0).abs() < 1e-9);
    }
}
