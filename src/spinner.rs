//! The loading spinner for pages that are not yet in the cache.
//!
//! **When it is drawn:** on every page without a cached image. If the image
//! is in the cache, it is blitted and no spinner is called.
//!
//! **Why time-based, not counter-based:** the progress comes from the clock,
//! not from the number of calls. With a counter, two spinners on one screen
//! would run twice as fast; with time as the source, both read the same
//! clock reading. Two spinners in the same frame therefore show the same
//! phase — the same load, looked at twice.

use std::time::Instant;

use crate::framebuffer::FrameBuffer;
use crate::IRect;

/// How many steps **one half** of the round has: first the ring fills up
/// dark, then it empties again. A full round is `2 × [`HALF_STEPS`]` half
/// steps.
pub const HALF_STEPS: u32 = 12;

/// A full round: two halves of [`HALF_STEPS`] each. In the first, the
/// **end** of the dark arc wanders around (the ring fills up), in the second
/// the **start** (it empties) — no hard jump from "fully dark" to "almost
/// empty".
pub const PHASES: u32 = 2 * HALF_STEPS;

/// How long one step stands still, in milliseconds
/// (`PHASES × STEP_MS` per round).
pub const STEP_MS: u64 = 80;

/// The ring's diameter in pixels, when the page is big enough.
pub const DIAMETER: i32 = 48;

/// The ring's width in pixels.
pub const RING_WIDTH: i32 = 6;

/// The loading spinner. It holds **only** the time reference, nothing else —
/// there is no state that advances with a call.
#[derive(Debug, Clone, Copy)]
pub struct Spinner {
    /// When it came into being. The phase is always `now - start`.
    pub start: Instant,
}

impl Spinner {
    /// A spinner from now on.
    pub fn new() -> Spinner {
        Spinner { start: Instant::now() }
    }

    /// A spinner whose time axis begins at `start` — the route for two
    /// spinners in the same frame: both are computed from the same `start`
    /// and look identical.
    pub fn from_start(start: Instant) -> Spinner {
        Spinner { start }
    }

    /// The reading in **milliseconds** since creation.
    fn elapsed_ms(&self, now: Instant) -> u64 {
        // `saturating_duration_since`: a system clock jumps backwards, and
        // the spinner should stand still on a clock anomaly, not terminate
        // the program.
        now.saturating_duration_since(self.start).as_millis() as u64
    }

    /// The **phase** for the given clock reading, in `0..PHASES`. Pure
    /// modulo on time: the same time gives **always** the same phase —
    /// exactly what two spinners simultaneously need.
    pub fn phase(&self, now: Instant) -> u32 {
        phase_from_ms(self.elapsed_ms(now))
    }
}

/// The phase for a clock reading in milliseconds, in `0..PHASES`. Public,
/// because it must be testable without a clock.
pub fn phase_from_ms(elapsed_ms: u64) -> u32 {
    ((elapsed_ms / STEP_MS) % PHASES as u64) as u32
}

/// The color of the resting ring: gray, visible, but no content.
pub const RING_COLOR: u8 = 0xC0;

/// The color of the wandering tip: dark gray.
pub const TIP_COLOR: u8 = 0x40;

/// The radius of the ring, depending on the page size. Smaller pages get a
/// smaller spinner; below the minimum size nothing is drawn — then a
/// spinner is unreadable, and the empty white page with its border is the
/// better answer.
pub fn radius_for(page: IRect) -> Option<(i32, i32)> {
    let short = page.w().min(page.h());
    if short < DIAMETER * 2 {
        return None;
    }
    let diameter = DIAMETER.min(short / 4).max(1);
    let ring = RING_WIDTH.min(diameter / 3).max(1);
    Some((diameter / 2, ring))
}

/// Draws the spinner centered in `page`. `phase` is the step in
/// `0..PHASES`; it comes from outside, so that the drawing need not know
/// anything about time.
///
/// The animation is a fill-and-empty, not a wandering dot: in the first
/// half the **start** of the dark arc stays fixed and its **end** wanders
/// around, in the second the **end** stays fixed and the **start** wanders.
/// No hard jump.
///
/// Drawing is a scan, not a polygon walk: the whole bounding box of the
/// ring is walked row by row, and for **each** pixel it is decided
/// individually whether and how strongly it belongs to the ring. The radial
/// distance decides membership and edge smoothing, the angle decides
/// membership in the dark arc. That yields a pixel-exactly seamless ring
/// with smoothed edges.
pub fn draw(fb: &mut FrameBuffer, page: IRect, phase: u32) {
    let Some((radius, ring)) = radius_for(page) else {
        return;
    };
    let mx = page.x0 + page.w() / 2;
    let my = page.y0 + page.h() / 2;

    // The bounding box of the ring, clipped to the buffer, plus one pixel
    // allowance for edge smoothing.
    let lo = radius + 1;
    let hi = radius - ring - 1;
    let y0 = (my - lo).max(0);
    let y1 = (my + lo + 1).min(fb.height() as i32);
    let x0 = (mx - lo).max(0);
    let x1 = (mx + lo + 1).min(fb.width() as i32);
    if y1 <= y0 || x1 <= x0 {
        return;
    }

    // The two edges of the dark arc, in `0..=HALF_STEPS`. It always holds
    // `a <= b`; the dark region is the arc from `a` forward to `b`.
    let p = phase % PHASES;
    let (a, b) = if p < HALF_STEPS {
        (0.0, p as f64)
    } else {
        ((p - HALF_STEPS) as f64, HALF_STEPS as f64)
    };

    // Width of the soft transition at the arc edges (in angle units).
    let soft = 1.0f64;

    for y in y0..y1 {
        let dy = y - my;
        let dy2 = (dy * dy) as f64;
        for x in x0..x1 {
            let dx = x - mx;
            let d = ((dx * dx) as f64 + dy2).sqrt();
            if d > (radius as f64 + soft) || d < (hi as f64 - soft) {
                continue; // outside the ring strip (incl. smoothing)
            }

            // Radial coverage: 1 in the core of the ring, gliding over the
            // outermost/inner-most pixel layer to 0.
            let middle = (radius as f64 + hi as f64) / 2.0;
            let half = ring as f64 / 2.0;
            let coverage = (half - (d - middle).abs() + 0.5).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }

            // The angle of this pixel, in `0..HALF_STEPS`, counterclockwise
            // starting from the top.
            let angle = -(dx as f64).atan2(dy as f64);
            let angle = (angle / std::f64::consts::PI * (HALF_STEPS as f64) / 2.0)
                .rem_euclid(HALF_STEPS as f64);

            // Does the pixel lie in the dark arc `[a, b]`? Soft transition
            // at both edges.
            let tip = if angle < a || angle > b {
                0.0
            } else {
                let edge_distance = (angle - a).min(b - angle);
                (edge_distance / soft).clamp(0.0, 1.0)
            };

            let val = (RING_COLOR as f64 * (1.0 - tip) + TIP_COLOR as f64 * tip) * coverage
                + (1.0 - coverage) * 255.0;
            fb.set_pixel(x, y, val as u8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two calls at the same time yield the same image — the case of the
    /// current page and the neighbor page in one frame.
    #[test]
    fn two_spinners_same_clock_same_image() {
        let s = Spinner::new();
        let now = s.start;
        let mut a = FrameBuffer::new(120, 160, 1);
        let mut b = FrameBuffer::new(120, 160, 1);
        draw(&mut a, IRect::new(10, 20, 110, 140), s.phase(now));
        draw(&mut b, IRect::new(0, 0, 100, 120), s.phase(now));
        let mut c = FrameBuffer::new(120, 160, 1);
        draw(&mut c, IRect::new(10, 20, 110, 140), s.phase(now));
        assert_eq!(
            a.bytes(),
            c.bytes(),
            "two spinners in the same frame must be identical"
        );
        // And the neighbor area was painted too, not left empty.
        assert!(b.bytes().iter().any(|&p| p != 0xFF));
    }

    #[test]
    fn time_makes_the_ring_wander() {
        let s = Spinner::new();
        let later = s.start + std::time::Duration::from_millis(STEP_MS);
        assert_ne!(s.phase(s.start), s.phase(later));
    }

    /// A full round needs [`PHASES`] step durations and lands back at the
    /// first.
    #[test]
    fn one_round_is_twentyfour_steps() {
        let s = Spinner::new();
        let first = s.phase(s.start);
        for i in 1..PHASES as u64 {
            let later = s.start + std::time::Duration::from_millis(i * STEP_MS);
            assert_ne!(s.phase(later), first, "step {} is the first", i);
        }
        let round = s.start + std::time::Duration::from_millis(PHASES as u64 * STEP_MS);
        assert_eq!(s.phase(round), first, "after one round the start must return");
    }

    #[test]
    fn phase_is_pure_modulo() {
        assert_eq!(phase_from_ms(0), 0);
        assert_eq!(phase_from_ms(STEP_MS), 1);
        assert_eq!(phase_from_ms(STEP_MS * 7), 7);
        assert_eq!(phase_from_ms(STEP_MS * 13), 13);
        assert_eq!(phase_from_ms(STEP_MS * PHASES as u64), 0);
    }

    /// The dark arc fills and empties: in the first half the dark arc length
    /// grows with the phase, in the second it shrinks.
    #[test]
    fn dark_arc_fills_and_empties() {
        let count_dark = |phase: u32| -> usize {
            let mut fb = FrameBuffer::new(200, 300, 1);
            fb.fill(0xFF);
            draw(&mut fb, IRect::new(0, 0, 200, 300), phase);
            fb.bytes().iter().filter(|&&p| p < RING_COLOR).count()
        };
        let empty = count_dark(0);
        let middle = count_dark(HALF_STEPS / 2);
        let full = count_dark(HALF_STEPS);
        assert!(empty <= middle, "fill half does not grow: {} > {}?", empty, middle);
        assert!(middle < full, "end of fill half not darkest point: {} < {}?", middle, full);

        let early = count_dark(HALF_STEPS + 1);
        let late = count_dark(PHASES - 1);
        assert!(early > late, "empty half does not empty: {} > {}?", early, late);
        assert!(
            late < full / 2,
            "end of round still too dark: {} against {} full",
            late,
            full
        );
    }

    /// On a page that is too narrow, **nothing** is drawn.
    #[test]
    fn too_small_page_stays_without_spinner() {
        assert_eq!(radius_for(IRect::new(0, 0, 20, 20)), None);
        assert!(radius_for(IRect::new(0, 0, 400, 400)).is_some());
    }

    /// On a big page something is drawn, **centered**, and it is not the
    /// whole area.
    #[test]
    fn spinner_draws_centered() {
        let mut fb = FrameBuffer::new(200, 300, 1);
        fb.fill(0xFF);
        draw(&mut fb, IRect::new(0, 0, 200, 300), 4);

        // Frame of all pixels that are not white.
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for y in 0..300i32 {
            for x in 0..200i32 {
                if fb.bytes()[(y as usize) * fb.stride() + x as usize] != 0xFF {
                    x0 = x0.min(x);
                    x1 = x1.max(x);
                    y0 = y0.min(y);
                    y1 = y1.max(y);
                }
            }
        }
        assert!(x0 < x1, "nothing was drawn");
        let middle_x = (x0 + x1) / 2;
        let middle_y = (y0 + y1) / 2;
        // Two pixels tolerance: the rings are rounded.
        assert!((middle_x - 100).abs() <= 2, "spinner not centered in x: {}", middle_x);
        assert!((middle_y - 150).abs() <= 2, "spinner not centered in y: {}", middle_y);
        assert!(y1 - y0 < 100, "spinner fills too much: {}", y1 - y0);
    }

    #[test]
    fn tip_wanders_with_the_step() {
        let mut a = FrameBuffer::new(200, 300, 1);
        let mut b = FrameBuffer::new(200, 300, 1);
        draw(&mut a, IRect::new(0, 0, 200, 300), 2);
        draw(&mut b, IRect::new(0, 0, 200, 300), 6);
        assert_ne!(a.bytes(), b.bytes(), "the tip stands in two places");
    }
}
