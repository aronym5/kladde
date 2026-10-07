//! Decision about a swipe gesture (first stage: decide on release).
//!
//! A **quick flick** flips exactly one page, **drawn far** (more than 35 %
//! of the width) also flips, **slow** drawing flips nothing, and **two
//! fingers** are ignored (otherwise a palm would flip). Everything here is a
//! pure function without window and without clock — only so it can be
//! verified with reproducible numbers.

/// What the release triggers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Nothing,
    Forward,
    Back,
}

/// Less movement than this is trembling and not a swipe (4 % of the width).
/// Without this threshold every tap would flip.
const JITTER: f64 = 0.04;
/// A flick must travel at least this far ...
const FLICK_DISTANCE: f64 = 0.08;
/// ... and take at most this long, otherwise it was a drag.
const FLICK_MS: f64 = 250.0;
/// Drawn far: then it flips even slowly.
const FAR: f64 = 0.35;

/// What a finger touching down means for the running gesture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Touch {
    /// A perfectly normal gesture: none was open.
    New,
    /// The same finger ID again while a gesture is open: its release never
    /// arrived (the ring buffer can overflow during fast swiping). The old
    /// gesture is closed, the new one begins cleanly.
    CatchUp,
    /// A second finger: invalid for navigation.
    SecondFinger,
}

/// A running swipe movement, fed from the compositor's `GESTURE_DOWN`,
/// `MOVE` and `UP`.
#[derive(Clone, Copy, Debug)]
pub struct Gesture {
    active: bool,
    slot: i32,
    start: (i32, i32),
    start_ms: f64,
    last: (i32, i32),
    /// A second finger was involved: then nothing flips.
    multi: bool,
}

impl Gesture {
    pub fn new() -> Gesture {
        Gesture {
            active: false,
            slot: -1,
            start: (0, 0),
            start_ms: 0.0,
            last: (0, 0),
            multi: false,
        }
    }

    /// A finger touches down. If a second one joins, navigation is over.
    pub fn down(&mut self, slot: i32, x: i32, y: i32, time_ms: f64) -> Touch {
        if self.active {
            if slot != self.slot {
                self.multi = true;
                return Touch::SecondFinger;
            }
            // Same finger ID while a gesture is open: the release never
            // arrived. Close the old gesture, start a new one.
            self.cleanup();
        }
        self.active = true;
        self.slot = slot;
        self.start = (x, y);
        self.last = (x, y);
        self.start_ms = time_ms;
        self.multi = false;
        Touch::New
    }

    /// Close everything, **without** deciding — for the case that the
    /// release never arrived.
    pub fn cleanup(&mut self) {
        self.active = false;
        self.multi = false;
        self.slot = -1;
    }

    /// The finger drags. Only the first slot leads.
    pub fn drag(&mut self, slot: i32, x: i32, y: i32, _t_ms: f64) {
        if !self.active || slot != self.slot {
            return;
        }
        self.last = (x, y);
    }

    /// The finger goes up — here the decision falls. `width` is the window
    /// width in pixels, which the percentages refer to. `slot == -1` means:
    /// the compositor aborted the gesture.
    pub fn up(&mut self, slot: i32, time_ms: f64, width: i32) -> Effect {
        let effect = if !self.active || slot == -1 || slot != self.slot || self.multi {
            Effect::Nothing
        } else {
            self.verdict(time_ms, width)
        };
        self.cleanup();
        effect
    }

    /// Distance (dx, dy) and duration of the gesture just ended — the
    /// numbers to calibrate the thresholds.
    pub fn summary(&self, time_ms: f64) -> (i32, i32, f64) {
        (
            self.last.0 - self.start.0,
            self.last.1 - self.start.1,
            (time_ms - self.start_ms).max(0.0),
        )
    }

    /// The finger's distance **during** the gesture. Before touching down
    /// and after releasing it is (0, 0) — there is nothing to push.
    pub fn distance(&self) -> (i32, i32) {
        if !self.active {
            return (0, 0);
        }
        (self.last.0 - self.start.0, self.last.1 - self.start.1)
    }

    /// A second finger was involved: the gesture is invalid, even *during*
    /// the drag.
    pub fn discarded(&self) -> bool {
        self.active && self.multi
    }

    fn verdict(&self, time_ms: f64, width: i32) -> Effect {
        if width <= 0 {
            return Effect::Nothing;
        }
        let w = width as f64;
        let dx = (self.last.0 - self.start.0) as f64;
        let dy = (self.last.1 - self.start.1) as f64;
        // A duration below zero can only come from mixed clocks.
        let duration = (time_ms - self.start_ms).max(0.0);
        let trav = dx.abs();

        if trav < w * JITTER {
            return Effect::Nothing;
        }
        if dy.abs() > trav {
            return Effect::Nothing; // vertical: no flipping
        }
        let quick = duration <= FLICK_MS && trav >= w * FLICK_DISTANCE;
        if trav > w * FAR || quick {
            if dx < 0.0 {
                Effect::Forward
            } else {
                Effect::Back
            }
        } else {
            Effect::Nothing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_flick_is_exactly_one_page() {
        let mut g = Gesture::new();
        g.down(0, 800, 500, 0.0);
        g.drag(0, 700, 504, 60.0);
        assert_eq!(g.up(0, 150.0, 1000), Effect::Forward);
    }

    #[test]
    fn flick_right_is_one_page_back() {
        let mut g = Gesture::new();
        g.down(0, 200, 500, 0.0);
        g.drag(0, 300, 498, 60.0);
        assert_eq!(g.up(0, 150.0, 1000), Effect::Back);
    }

    /// The same distance, but slow: nothing happens.
    #[test]
    fn slow_and_short_is_nothing() {
        let mut g = Gesture::new();
        g.down(0, 800, 500, 0.0);
        g.drag(0, 700, 500, 600.0);
        assert_eq!(g.up(0, 900.0, 1000), Effect::Nothing);
    }

    #[test]
    fn drawn_far_counts_even_slowly() {
        let mut g = Gesture::new();
        g.down(0, 800, 500, 0.0);
        g.drag(0, 400, 500, 900.0);
        assert_eq!(g.up(0, 1500.0, 1000), Effect::Forward);
    }

    #[test]
    fn vertical_is_nothing() {
        let mut g = Gesture::new();
        g.down(0, 500, 900, 0.0);
        g.drag(0, 440, 300, 60.0);
        assert_eq!(g.up(0, 150.0, 1000), Effect::Nothing);
    }

    /// A tap trembles a few pixels; no page may come out of it.
    #[test]
    fn jitter_is_nothing() {
        let mut g = Gesture::new();
        g.down(0, 500, 500, 0.0);
        g.drag(0, 520, 502, 30.0);
        assert_eq!(g.up(0, 60.0, 1000), Effect::Nothing);
    }

    #[test]
    fn two_fingers_is_nothing() {
        let mut g = Gesture::new();
        g.down(0, 800, 500, 0.0);
        g.down(1, 300, 500, 20.0);
        g.drag(0, 200, 500, 60.0);
        assert_eq!(g.up(0, 150.0, 1000), Effect::Nothing);
    }

    /// The compositor aborts (slot -1): do nothing, and the next finger
    /// starts cleanly anew.
    #[test]
    fn abort_is_nothing_and_cleans_up() {
        let mut g = Gesture::new();
        g.down(0, 800, 500, 0.0);
        g.drag(0, 200, 500, 40.0);
        assert_eq!(g.up(-1, 60.0, 1000), Effect::Nothing);

        g.down(0, 800, 500, 100.0);
        g.drag(0, 700, 500, 140.0);
        assert_eq!(g.up(0, 200.0, 1000), Effect::Forward);
    }

    #[test]
    fn releasing_twice_flips_only_once() {
        let mut g = Gesture::new();
        g.down(0, 800, 500, 0.0);
        g.drag(0, 700, 500, 60.0);
        assert_eq!(g.up(0, 150.0, 1000), Effect::Forward);
        assert_eq!(g.up(0, 190.0, 1000), Effect::Nothing);
    }

    /// The movement numbers stay available after the release — they are the
    /// basis for calibrating the thresholds.
    #[test]
    fn summary_survives_the_release() {
        let mut g = Gesture::new();
        g.down(0, 800, 500, 100.0);
        g.drag(0, 660, 520, 180.0);
        assert_eq!(g.up(0, 220.0, 1000), Effect::Forward);
        assert_eq!(g.summary(220.0), (-140, 20, 120.0));
    }

    /// The 35 % are a real boundary: exactly 35 % is not enough, just above
    /// is enough already.
    #[test]
    fn far_means_more_than_35_percent() {
        let mut g = Gesture::new();
        g.down(0, 850, 500, 0.0);
        g.drag(0, 500, 500, 800.0);
        assert_eq!(g.up(0, 1000.0, 1000), Effect::Nothing); // exactly 350 px

        let mut g2 = Gesture::new();
        g2.down(0, 850, 500, 0.0);
        g2.drag(0, 490, 500, 800.0);
        assert_eq!(g2.up(0, 1000.0, 1000), Effect::Forward); // 360 px
    }

    /// If a `GESTURE_UP` drops out, the gesture touching down afterwards
    /// must not be folded into the old one: it measures from its own point.
    #[test]
    fn a_lost_release_does_not_swallow_the_next_gesture() {
        let mut g = Gesture::new();
        assert_eq!(g.down(0, 800, 500, 0.0), Touch::New);
        g.drag(0, 400, 500, 60.0);
        // The release does not come.

        assert_eq!(g.down(0, 200, 500, 400.0), Touch::New);
        g.drag(0, 50, 500, 460.0);
        assert_eq!(g.summary(470.0), (-150, 0, 70.0));
        assert_eq!(g.up(0, 470.0, 1000), Effect::Forward);
    }

    /// After the catch-up, the old gesture must not decide once more.
    #[test]
    fn after_catchup_the_old_gesture_no_longer_counts() {
        let mut g = Gesture::new();
        g.down(0, 800, 500, 0.0);
        g.drag(0, 100, 500, 60.0);
        g.down(0, 800, 500, 400.0); // caught up
        assert_eq!(g.up(0, 500.0, 1000), Effect::Nothing);
    }

    #[test]
    fn the_second_finger_is_recognized_and_discarded() {
        let mut g = Gesture::new();
        g.down(0, 800, 500, 0.0);
        assert_eq!(g.down(1, 300, 500, 20.0), Touch::SecondFinger);
        assert!(g.discarded());
        g.drag(0, 200, 500, 60.0);
        assert_eq!(g.up(0, 150.0, 1000), Effect::Nothing);
    }

    #[test]
    fn cleanup_closes_without_deciding() {
        let mut g = Gesture::new();
        g.down(0, 800, 500, 0.0);
        g.drag(0, 100, 500, 60.0);
        g.cleanup();
        assert_eq!(g.distance(), (0, 0));
        assert!(!g.discarded());
        assert_eq!(g.up(0, 150.0, 1000), Effect::Nothing);
    }
}
