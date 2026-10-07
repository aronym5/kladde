//! Where the finished images go: the Wayland window.
//!
//! The path of the pixels: framebuffer (BGRX) -> back buffer (XRGB8888) ->
//! `wl_surface.commit`. Both layouts are the same bytes, only the row width
//! can differ — hence row-wise copying instead of rendering directly into
//! the buffer.

use std::io;

use crate::framebuffer::FrameBuffer;
use crate::wayland::{Event, Window};

/// The window as an output.
pub struct Wayland {
    win: Window,
    frames: usize,
    /// An image sits in the framebuffer that the compositor never got.
    ///
    /// `present` copies into a compositor buffer and waits up to 250 ms for
    /// a free one. If it gets none, the image is gone — the caller asks
    /// [`Wayland::present_pending`] and presents once more.
    pending_present: bool,
    /// How often a `GESTURE_UP` arrived and got parked while waiting for a
    /// compositor buffer (diagnostic for the stuck case).
    ups_during_wait: u64,
}

impl Wayland {
    pub fn open(title: &str, w: u32, h: u32, fullscreen: bool) -> io::Result<Wayland> {
        Ok(Wayland {
            win: Window::open(title, w, h, fullscreen)?,
            frames: 0,
            pending_present: false,
            ups_during_wait: 0,
        })
    }

    /// For the event loop in `kladde.rs`.
    pub fn window(&mut self) -> &mut Window {
        &mut self.win
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn name(&self) -> String {
        let (w, h) = self.win.size();
        format!("wayland {}x{}", w, h)
    }

    /// The present for the finished page: blocking, until the compositor
    /// hands over a buffer.
    pub fn present(&mut self, fb: &FrameBuffer) -> io::Result<()> {
        // Mark that an image wants out, and only then record the success.
        // This keeps `pending_present` true even if this call fails.
        self.pending_present = true;
        self.present_once(fb)?;
        self.pending_present = false;
        Ok(())
    }

    /// Is an image still pending that the compositor never got? The caller
    /// then presents once more — it must not poll without presenting.
    pub fn present_pending(&self) -> bool {
        self.pending_present
    }

    /// Send an image to the compositor without blocking — the present for
    /// the **swipe path**. It does not wait for a free buffer and does not
    /// poll: if no buffer is free right away (`WouldBlock`), the frame is
    /// skipped. The swipe frame is transient (the next touch move repaints
    /// anyway); a skipped intermediate frame is no loss, a swallowed
    /// `GESTURE_UP` would be.
    ///
    /// Returns `false` if the frame was omitted.
    pub fn present_nonblocking(&mut self, fb: &FrameBuffer) -> io::Result<bool> {
        let (ptr, stride, _bw, bh) = match self.win.back_buffer(fb.width(), fb.height()) {
            Ok(r) if r.2 == fb.width() && r.3 == fb.height() => r,
            // No matching buffer free right now: skip instead of waiting.
            // No pending present: nothing was committed.
            _ => return Ok(false),
        };
        self.copy_and_commit(fb, ptr, stride, bh)?;
        Ok(true)
    }

    /// How often the `GESTURE_UP` arrived while a swipe present waited for a
    /// buffer. See [`Wayland::ups_during_wait`].
    pub fn ups_during_wait(&self) -> u64 {
        self.ups_during_wait
    }

    /// Cancel the pending present, because the image is gone forever — only
    /// for the size change, where the framebuffer is discarded.
    pub fn clear_pending_present(&mut self) {
        self.pending_present = false;
    }

    /// One present pass with waiting: get a buffer, write the bytes, commit.
    fn present_once(&mut self, fb: &FrameBuffer) -> io::Result<()> {
        // Waiting is right instead of rastering twice: the pixels already
        // exist, and events run into the ring buffer of the main loop in the
        // meantime. `Window::poll` parks what arrives — a swallowed
        // `GESTURE_UP` would leave the page stuck between two pages forever.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        let (ptr, stride, _bw, bh) = loop {
            match self.win.back_buffer(fb.width(), fb.height()) {
                Ok(r) if r.2 == fb.width() && r.3 == fb.height() => break r,
                Ok(r) => {
                    // Size change: wait until the old size is back.
                    if std::time::Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            format!(
                                "Buffer is {}x{}, expected {}x{} - frame skipped",
                                r.2,
                                r.3,
                                fb.width(),
                                fb.height()
                            ),
                        ));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Both buffers with the compositor: wait for the return.
                    if std::time::Instant::now() >= deadline {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
            let mut ev = [Event::default(); 4];
            let _ = self.win.poll(&mut ev, 16)?;
            self.ups_during_wait += self.win.parked_ups();
        };

        let n = fb.n() as usize;
        if n != 3 && n != 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} bytes/pixel I cannot write into an XRGB8888 buffer", n),
            ));
        }
        self.copy_and_commit(fb, ptr, stride, bh)
    }

    /// Shared by `present` and `present_nonblocking` — both must write the
    /// same bytes to the same buffer.
    fn copy_and_commit(&mut self, fb: &FrameBuffer, ptr: *mut u8, stride: usize, bh: u32) -> io::Result<()> {
        let n = fb.n() as usize;
        let w_px = fb.width() as usize;
        let h_px = fb.height() as usize;
        let src_stride = fb.stride();
        let src = fb.bytes();
        // The length comes from the buffer that the shim reported, not from
        // the framebuffer — the two may currently differ.
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr, stride * bh as usize) };
        for y in 0..h_px {
            let s = &src[y * src_stride..y * src_stride + w_px * n];
            let d = &mut dst[y * stride..y * stride + w_px * 4];
            if n == 4 {
                d.copy_from_slice(s);
            } else {
                for (i, px) in s.chunks_exact(3).enumerate() {
                    d[i * 4] = px[0];
                    d[i * 4 + 1] = px[1];
                    d[i * 4 + 2] = px[2];
                    d[i * 4 + 3] = 0xFF;
                }
            }
        }
        self.win.commit(0, 0, fb.width(), fb.height())?;
        self.frames += 1;
        Ok(())
    }
}
