//! Parallel prefetch: a worker thread with its own MuPDF context rasters the
//! likely target page **completely**, without blocking the swipe path.
//!
//! It builds its **own context** and its **own document** (MuPDF contexts are
//! not thread-safe), rasters in the background and delivers the finished
//! [`PageImage`] over a channel — pure data (own `Vec<u8>`) that may cross
//! thread boundaries.
//!
//! **The newest job wins:** if the user swipes faster than the worker can
//! raster, older jobs are discarded (with a discard message, so the caller
//! knows the page will not arrive). The idle gate lives with the caller:
//! jobs are sent only when there is nothing to show and no gesture is open.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::render::{ms_of, RenderStats};
use crate::book::{Book, PageImage};
use crate::{Doc, Engine, IRect, Pixmap};

/// What the worker needs to build its **own** context.
pub struct WorkerConfig {
    pub store_bytes: usize,
    pub pdf: PathBuf,
    pub alpha: bool,
    /// Silent store for the worker's own diagnostics (context/document).
    pub quiet: bool,
}

/// A job for the worker: raster one page completely — at the scale the
/// caller computes for it — or stop.
pub enum Job {
    Page { page: u16, scale: f32 },
    Stop,
}

/// A result from the worker: the finished page image — or `None` if the job
/// was displaced (a newer one won) or failed — and the statistics of the
/// raster pass.
///
/// **`scale` is the validity proof, not decoration.** The job named a scale,
/// the result belongs to exactly that one. If the window size changes while
/// the worker is rastering, a page arrives that fits the old size — and it
/// must not go into the cache as a hit for the new one.
pub struct Outcome {
    pub page: u16,
    pub image: Option<PageImage>,
    pub stats: Option<RenderStats>,
    /// The scale that was really rastered — not the one currently in force.
    pub scale: f32,
}

/// Starts the worker thread. It ends when [`Job::Stop`] arrives or the
/// sender falls — the caller cleans up with a Stop job and `join`.
pub fn start_worker(config: WorkerConfig) -> (Sender<Job>, Receiver<Outcome>, JoinHandle<()>) {
    let (job_tx, job_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || worker_loop(job_rx, result_tx, config));
    (job_tx, result_rx, handle)
}

fn worker_loop(rx: Receiver<Job>, tx: Sender<Outcome>, config: WorkerConfig) {
    // Own context, own document: MuPDF contexts are not thread-safe. If the
    // setup fails (memory, broken file), the worker never rasters and the
    // caller covers flipping with its normal rendering path.
    let eng = match Engine::new(config.store_bytes) {
        Ok(e) => e,
        Err(e) => {
            if !config.quiet {
                eprintln!("  Prefetch worker: context: {}", e);
            }
            return;
        }
    };
    let doc = match eng.open(&config.pdf) {
        Ok(d) => d,
        Err(e) => {
            if !config.quiet {
                eprintln!("  Prefetch worker: {}: {}", config.pdf.display(), e);
            }
            return;
        }
    };
    loop {
        // Blocking: without a job the worker uses no compute time.
        let mut current = match rx.recv() {
            Ok(Job::Stop) | Err(_) => return,
            Ok(Job::Page { page, scale }) => (page, scale),
        };
        // The newest job wins: fetch pending ones, discard older ones.
        while let Ok(newer) = rx.try_recv() {
            match newer {
                Job::Stop => return,
                Job::Page { page, scale } => {
                    // The displaced job reports **its** scale, not that of
                    // the new one: the result is worthless, and the caller
                    // should recognize that it must not cache it.
                    tx.send(Outcome {
                        page: current.0,
                        image: None,
                        stats: None,
                        scale: current.1,
                    })
                    .ok();
                    current = (page, scale);
                }
            }
        }
        let (page, scale) = current;
        let (image, stats) = match raster_page(&eng, &doc, page, scale, config.alpha) {
            Some((image, stats)) => (Some(image), Some(stats)),
            None => (None, None),
        };
        tx.send(Outcome { page, image, stats, scale }).ok();
    }
}

/// Rasters `page` **completely** at `scale` and delivers page image and
/// statistics. This is **the** raster path of the program — hence public:
/// the geometry test uses the same path. `None` if something fails (broken
/// page, memory) — the caller then decides what to do.
pub fn raster_page(
    eng: &Engine,
    doc: &Doc,
    page: u16,
    scale: f32,
    alpha: bool,
) -> Option<(PageImage, RenderStats)> {
    let wall = Instant::now();
    let mut st = RenderStats::default();

    let box_ = doc.page_box(page).ok()?;

    let t = Instant::now();
    let list = doc.parse(page).ok()?;
    st.parse_ms = ms_of(t);

    let t = Instant::now();
    let ox = (-box_.x0 * scale as f64) as f32;
    let oy = (-box_.y0 * scale as f64) as f32;
    let (w, h) = crate::pixels(&box_, scale);
    let mut px = Pixmap::new(eng, w, h, alpha).ok()?;
    px.fill(255);
    st.alloc_ms = ms_of(t);

    // From here on is the time you feel on the device.
    let t_raster = Instant::now();
    let mut nodes = 0i32;
    let drew = eng
        .raster(
            &list,
            scale,
            ox,
            oy,
            &mut px,
            IRect::new(0, 0, w as i32, h as i32),
            &mut nodes,
        )
        .ok()?;
    st.raster_ms = ms_of(t_raster);
    st.nodes = nodes;
    st.empty_page = !drew;

    let t = Instant::now();
    st.coverage = px.coverage_percent();
    st.coverage_ms = ms_of(t);

    // Extract the page image (row by row, because of stride padding).
    let t = Instant::now();
    let image = PageImage::from_pixmap(&px);
    st.cache_ms = ms_of(t);
    st.total_ms = ms_of(wall);
    Some((image, st))
}

/// The most important missing page, in this order: `page + 1` (the next
/// swipe usually goes forward), `page - 1`, then `page + 2` and `page - 2`
/// — further away, only if both neighbors are already in the cache. `None`
/// if all four are usable in the cache.
///
/// **"Missing" does not mean "unknown", it means "not current".** A neighbor
/// that sits in the cache but stems from an earlier window size is picked up
/// just like one that does not exist yet — after a size change the neighbor
/// pages must be rastered again by themselves, in idle.
pub fn next_missing(book: &Book, page: u16, n_pages: u16) -> Option<u16> {
    for d in [1, -1, 2, -2] {
        let s = page as i32 + d;
        if s >= 0 && s < n_pages as i32 && !book.is_current(s as u16) {
            return Some(s as u16);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Neighbors that sit in the cache but at the wrong resolution are picked
    /// up again — the second part of the job after a size change.
    #[test]
    fn stale_neighbors_are_picked_up_again() {
        let mut v = Book::new(8);
        v.set_scale(1.0);
        for s in 0..8 {
            v.insert(s, PageImage::new(2, 2));
        }
        assert_eq!(next_missing(&v, 3, 8), None, "full, valid cache");

        v.set_scale(1.5);
        // Forward first: the next swipe usually goes first.
        assert_eq!(next_missing(&v, 3, 8), Some(4));
    }

    #[test]
    fn the_order_holds_for_stale_pages_too() {
        let mut v = Book::new(10);
        v.set_scale(1.0);
        for s in 0..10 {
            v.insert(s, PageImage::new(2, 2));
        }
        v.set_scale(2.0);
        for s in 0..10 {
            assert!(v.is_stale(s), "page {} should be stale", s);
        }
        // From page 3: 4, then 2, then 5, then 1 — and then it stops,
        // because page 0 is too far away.
        let mut course = Vec::new();
        while let Some(s) = next_missing(&v, 3, 10) {
            course.push(s);
            v.insert(s, PageImage::new(3, 3));
            assert!(course.len() < 12, "the loop must end");
        }
        assert_eq!(course, vec![4, 2, 5, 1]);
    }

    /// **Mixed**: re-raster the current page while the neighbors still wait
    /// their turn.
    #[test]
    fn mixed_current_and_stale_neighbor() {
        let mut v = Book::new(8);
        v.set_scale(1.0);
        for s in 0..8 {
            v.insert(s, PageImage::new(2, 2));
        }
        v.set_scale(1.5);
        // The front neighbor is already re-rastered, the back one not yet.
        v.insert(4, PageImage::new(3, 3));
        assert!(v.is_current(4));
        assert_eq!(next_missing(&v, 3, 8), Some(2), "now the back neighbor");
    }

    #[test]
    fn direction_bonus_picks_the_next_page() {
        let v = Book::new(8);
        assert_eq!(next_missing(&v, 3, 8), Some(4));
    }

    #[test]
    fn if_forward_is_cached_go_back() {
        let mut v = Book::new(8);
        v.insert(4, PageImage::new(2, 2));
        assert_eq!(next_missing(&v, 3, 8), Some(2));
    }

    #[test]
    fn if_neighbors_are_cached_go_one_page_further() {
        let mut v = Book::new(8);
        v.insert(2, PageImage::new(2, 2));
        v.insert(4, PageImage::new(2, 2));
        // Front and back are occupied, front+2 (page 5) not yet.
        assert_eq!(next_missing(&v, 3, 8), Some(5));
    }

    #[test]
    fn only_when_all_four_candidates_are_cached_nothing_to_do() {
        let mut v = Book::new(8);
        for s in [1, 2, 4, 5] {
            v.insert(s, PageImage::new(2, 2));
        }
        assert_eq!(next_missing(&v, 3, 8), None);
    }

    #[test]
    fn the_second_page_back_is_chosen_too() {
        let mut v = Book::new(8);
        for s in [2, 4, 5] {
            v.insert(s, PageImage::new(2, 2));
        }
        assert_eq!(next_missing(&v, 3, 8), Some(1));
    }

    #[test]
    fn at_the_last_page_only_back_remains() {
        let v = Book::new(8);
        assert_eq!(next_missing(&v, 7, 8), Some(6));
    }

    #[test]
    fn at_the_first_page_only_forward_remains() {
        let v = Book::new(8);
        assert_eq!(next_missing(&v, 0, 8), Some(1));
    }
}
