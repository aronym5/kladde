//! The Wayland window.
//!
//! The only `unsafe` area of this file is the `ffi` module further down.
//! What is deliberately *not* here: drawing, color, layout, pages. The
//! window receives bytes and reports when something was on screen.
//!
//! Two variants: the real one with `cfg(kladde_wl)`, and one that builds
//! without Wayland (CI, rust-analyzer, laptop without wayland-dev). Both
//! have the same surface, so that `kladde.rs` needs no `#[cfg]` branches.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::io;
use std::ptr;

/// true if built against real Wayland.
pub const AVAILABLE: bool = cfg!(kladde_wl);

/// Events from the compositor. The numbers must match the `KL_WL_*` in
/// `shim/kladde_wl.h` — a wrong number shows up in a test
/// ([`eventkonstanten` in the test module]).
pub mod event {
    pub const SURFACE: i32 = 1; // window size, a = width, b = height
    pub const FRAME: i32 = 2; // presented, time_ms = time point
    pub const GESTURE_DOWN: i32 = 3; // a = x, b = y, c = slot
    pub const GESTURE_MOVE: i32 = 4; // a = x, b = y, c = slot
    pub const GESTURE_UP: i32 = 5; // c = slot, -1 on abort
    pub const KEY: i32 = 6; // a = raw device code (evdev), pressed
    pub const CLOSE: i32 = 7;
    pub const MENU: i32 = 8;
    /// The slot of a gesture event that came from the **mouse**. Touch sends
    /// the real finger ID (>= 0), an aborted finger -1, the mouse -2.
    /// Without this marker a click and a finger tap are indistinguishable in
    /// the event stream.
    pub const MOUSE_SLOT: i32 = -2;
}

/// Keycodes as they travel the wire: **raw device codes** (evdev). The
/// protocol says verbatim: "to determine the xkb keycode, clients must add 8
/// to the key event keycode" (`wayland.xml`) — the XKB number is this value
/// **plus eight**, and exactly those eight were once missing here. The
/// numbers live in `/usr/include/linux/input-event-codes.h`.
pub mod key {
    pub const ESC: i32 = 1;
    pub const Q: i32 = 16;
    pub const F: i32 = 33;
    pub const SPACE: i32 = 57;
    pub const PAGEUP: i32 = 104;
    pub const PAGEDOWN: i32 = 109;
    pub const RIGHT: i32 = 106;
    pub const LEFT: i32 = 105;
    pub const UP: i32 = 103;
    pub const DOWN: i32 = 108;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Event {
    pub kind: i32,
    pub serial: i32,
    pub time_ms: f64,
    pub a: i32,
    pub b: i32,
    pub c: i32,
}

// ---------------------------------------------------------------------
// FFI - die einzige unsichere Stelle der Datei.
// ---------------------------------------------------------------------

#[cfg(kladde_wl)]
mod ffi {
    use super::Event;
    use std::ffi::{c_char, c_int, c_void};

    extern "C" {
        pub fn kl_wl_available() -> c_int;
        pub fn kl_wl_open(
            title: *const c_char,
            w: c_int,
            h: c_int,
            fullscreen: c_int,
            err: *mut c_char,
            errlen: usize,
        ) -> *mut c_void;
        pub fn kl_wl_drop(h: *mut c_void);
        pub fn kl_wl_want(h: *mut c_void, iface: *const c_char, version: *mut c_int) -> c_int;
        pub fn kl_wl_back_buffer(
            h: *mut c_void,
            w: c_int,
            hh: c_int,
            stride: *mut c_int,
            buf_w: *mut c_int,
            buf_h: *mut c_int,
        ) -> *mut u8;
        pub fn kl_wl_commit(h: *mut c_void, x: c_int, y: c_int, cw: c_int, ch: c_int) -> c_int;
        pub fn kl_wl_poll(h: *mut c_void, ev: *mut Event, max: c_int, timeout_ms: c_int) -> c_int;
        pub fn kl_wl_now_ms(h: *mut c_void) -> f64;
        pub fn kl_wl_set_fullscreen(h: *mut c_void, on: c_int);
        pub fn kl_wl_is_fullscreen(h: *mut c_void) -> c_int;
        pub fn kl_wl_has_touch(h: *mut c_void) -> c_int;
        pub fn kl_wl_has_pointer(h: *mut c_void) -> c_int;
        pub fn kl_wl_has_keyboard(h: *mut c_void) -> c_int;
        pub fn kl_wl_idle_inhibited(h: *mut c_void) -> c_int;
        pub fn kl_wl_last_error(h: *mut c_void) -> *const c_char;
    }
}

/// The variant without Wayland. Same names, same return values as the
/// Fehlerfall der echten Fassung.
#[cfg(not(kladde_wl))]
mod ffi {
    use super::Event;
    use std::ffi::{c_char, c_int, c_void};
    use std::ptr;

    const STUB_ERROR: &[u8] = b"built without Wayland\0";

    pub unsafe fn kl_wl_available() -> c_int {
        0
    }
    pub unsafe fn kl_wl_open(
        _title: *const c_char,
        _w: c_int,
        _h: c_int,
        _fullscreen: c_int,
        err: *mut c_char,
        errlen: usize,
    ) -> *mut c_void {
        // Wird nie erreicht: Window::open prüft zuerst AVAILABLE. Trotzdem
        // einen Text hinterlassen, falls jemand die Prüfung herausnimmt.
        if !err.is_null() && errlen > 0 {
            let n = (errlen - 1).min(STUB_ERROR.len() - 1);
            for i in 0..n {
                *err.add(i) = STUB_ERROR[i] as c_char;
            }
            *err.add(n) = 0;
        }
        ptr::null_mut()
    }
    pub unsafe fn kl_wl_drop(_h: *mut c_void) {}
    pub unsafe fn kl_wl_want(_h: *mut c_void, _iface: *const c_char, v: *mut c_int) -> c_int {
        if !v.is_null() {
            *v = -1;
        }
        0
    }
    pub unsafe fn kl_wl_back_buffer(
        _h: *mut c_void,
        _w: c_int,
        _hh: c_int,
        stride: *mut c_int,
        _buf_w: *mut c_int,
        _buf_h: *mut c_int,
    ) -> *mut u8 {
        if !stride.is_null() {
            *stride = 0;
        }
        ptr::null_mut()
    }
    pub unsafe fn kl_wl_commit(_h: *mut c_void, _x: c_int, _y: c_int, _cw: c_int, _ch: c_int) -> c_int {
        -1
    }
    pub unsafe fn kl_wl_poll(
        _h: *mut c_void,
        _ev: *mut Event,
        _max: c_int,
        _timeout_ms: c_int,
    ) -> c_int {
        0
    }
    pub unsafe fn kl_wl_now_ms(_h: *mut c_void) -> f64 {
        0.0
    }
    pub unsafe fn kl_wl_set_fullscreen(_h: *mut c_void, _on: c_int) {}
    pub unsafe fn kl_wl_is_fullscreen(_h: *mut c_void) -> c_int {
        0
    }
    pub unsafe fn kl_wl_has_touch(_h: *mut c_void) -> c_int {
        0
    }
    pub unsafe fn kl_wl_has_pointer(_h: *mut c_void) -> c_int {
        0
    }
    pub unsafe fn kl_wl_has_keyboard(_h: *mut c_void) -> c_int {
        0
    }
    pub unsafe fn kl_wl_idle_inhibited(_h: *mut c_void) -> c_int {
        0
    }
    pub unsafe fn kl_wl_last_error(_h: *mut c_void) -> *const c_char {
        STUB_ERROR.as_ptr() as *const c_char
    }
}

// ---------------------------------------------------------------------
// Das Fenster selbst.
// ---------------------------------------------------------------------

/// Ein offenes Fenster. Kein Thread, keine Kanäle: `poll` ist ein
/// Systemaufruf, der aufhört, wenn etwas da ist.
pub struct Window {
    h: *mut c_void,
    /// Vom Compositor gemeldete Größe, 0 = noch nie konfiguriert.
    width: u32,
    height: u32,
    /// Ereignisse, die während eines Wartens auf einen Compositor-Puffer
    /// angefallen sind. Das ist eine Rettung: `present` muss warten, und der
    /// Compositor liefert seine Ereignisse nur über `poll`. Wer dabei in
    /// einen Wegwerf-Puffer pollt, wirft alles weg — im Wischfall genau das
    /// `GESTURE_UP`. [`Window::poll`] holt diese Ereignisse zuerst.
    pending: PendingQueue,
}

/// Die Ereignisse, die während eines Wartens auf einen Compositor-Puffer
/// angefallen sind. Die Reihenfolge ist die Zusage: **nichts geht verloren,
/// und nichts wird überholt.**
#[derive(Default)]
struct PendingQueue {
    ring: std::collections::VecDeque<Event>,
}

impl PendingQueue {
    /// Die vordersten Ereignisse nach `into`, höchstens `into.len()`.
    fn take(&mut self, into: &mut [Event]) -> usize {
        let n = self.ring.len().min(into.len());
        for slot in into.iter_mut().take(n) {
            match self.ring.pop_front() {
                Some(e) => *slot = e,
                None => break,
            }
        }
        n
    }

    fn park(&mut self, ev: &[Event]) {
        self.ring.extend(ev.iter().copied());
    }

    /// Nur von den Tests gebraucht.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.ring.len()
    }

    /// Wie viele geparkte Ereignisse `GESTURE_UP` sind.
    fn count_parked_ups(&self) -> u64 {
        self.ring.iter().filter(|e| e.kind == event::GESTURE_UP).count() as u64
    }
}

impl Window {
    /// Das Fenster öffnen. `w x h` ist der Wunsch; was der Compositor daraus
    /// macht, kommt nach dem ersten `poll` als [`event::SURFACE`] zurück.
    pub fn open(title: &str, w: u32, h: u32, fullscreen: bool) -> io::Result<Window> {
        if !AVAILABLE {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Kladde was built without Wayland (wayland-dev was missing at \
                 build time, or KLADDE_WAYLAND=stub was set). The window is \
                 the only output of this program - rebuild with `wayland-dev` \
                 (Alpine) or `libwayland-dev` (Debian).",
            ));
        }
        if unsafe { ffi::kl_wl_available() } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this binary has no Wayland support",
            ));
        }
        let ctitle = CString::new(title).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "title contains a NUL byte")
        })?;
        let mut err = [0u8; 512];
        let handle = unsafe {
            ffi::kl_wl_open(
                ctitle.as_ptr(),
                w as c_int,
                h as c_int,
                fullscreen as c_int,
                err.as_mut_ptr() as *mut c_char,
                err.len(),
            )
        };
        if handle.is_null() {
            let end = err.iter().position(|&b| b == 0).unwrap_or(err.len());
            let msg = String::from_utf8_lossy(&err[..end]).into_owned();
            return Err(io::Error::new(
                io::ErrorKind::Other,
                if msg.is_empty() { "could not open the window".into() } else { msg },
            ));
        }
        Ok(Window {
            h: handle,
            width: w,
            height: h,
            pending: PendingQueue::default(),
        })
    }

    /// Gemeldete Größe vermerken (aus [`event::SURFACE`]), damit `name()` die
    /// tatsächliche Größe zeigt.
    pub fn set_size(&mut self, w: u32, h: u32) {
        if w > 0 && h > 0 {
            self.width = w;
            self.height = h;
        }
    }

    /// Fenstergröße, wie der Compositor sie zuletzt meldete; bis zum ersten
    /// [`event::SURFACE`] die Größe aus `open`.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Welche Version eines Protokolls der Compositor anbietet. `None`
    /// heißt "nicht da".
    pub fn wants(&self, iface: &str) -> Option<i32> {
        let c = CString::new(iface).ok()?;
        let mut v: c_int = -1;
        let has = unsafe { ffi::kl_wl_want(self.h, c.as_ptr(), &mut v) };
        if has != 0 && v >= 0 {
            Some(v as i32)
        } else {
            None
        }
    }

    pub fn set_fullscreen(&self, on: bool) {
        unsafe {
            ffi::kl_wl_set_fullscreen(self.h, if on { 1 } else { 0 });
        }
    }

    /// Steht das Fenster **jetzt** im Vollbild? Aus dem letzten `configure`,
    /// nicht aus der letzten Anfrage — der Compositor entscheidet.
    pub fn is_fullscreen(&self) -> bool {
        unsafe { ffi::kl_wl_is_fullscreen(self.h) != 0 }
    }

    pub fn has_touch(&self) -> bool {
        unsafe { ffi::kl_wl_has_touch(self.h) != 0 }
    }

    pub fn has_pointer(&self) -> bool {
        unsafe { ffi::kl_wl_has_pointer(self.h) != 0 }
    }

    pub fn has_keyboard(&self) -> bool {
        unsafe { ffi::kl_wl_has_keyboard(self.h) != 0 }
    }

    /// Is the screen currently kept from dimming and blanking? The shim
    /// creates and destroys the inhibitor with the window's foreground
    /// state - the compositor's `activated` state or the keyboard focus
    /// (see `update_idle_inhibit()` in `shim/kladde_wl.c`); `false`
    /// therefore also means "the compositor does not offer the protocol"
    /// or "another window is in the foreground".
    pub fn idle_inhibited(&self) -> bool {
        unsafe { ffi::kl_wl_idle_inhibited(self.h) != 0 }
    }

    /// Back buffer for `w x h` pixels. Returns pointer, stride (bytes per
    /// row) and the **actual** buffer dimensions; the dimensions can differ
    /// from `w x h` while a resize is in flight — the caller *must* use
    /// them. `Err(WouldBlock)` means: both buffers sit at the compositor.
    pub fn back_buffer(&mut self, w: u32, h: u32) -> io::Result<(*mut u8, usize, u32, u32)> {
        let mut stride: c_int = 0;
        let mut bw: c_int = 0;
        let mut bh: c_int = 0;
        let p = unsafe {
            ffi::kl_wl_back_buffer(
                self.h,
                w as c_int,
                h as c_int,
                &mut stride,
                &mut bw,
                &mut bh,
            )
        };
        if p.is_null() {
            return Err(match self.error() {
                Some(m) => io::Error::new(io::ErrorKind::Other, m),
                None => io::Error::new(io::ErrorKind::WouldBlock, "both buffers are in use"),
            });
        }
        Ok((p, stride as usize, bw.max(0) as u32, bh.max(0) as u32))
    }

    /// Rückpuffer präsentieren: Rechteck als beschädigt melden, committen.
    pub fn commit(&mut self, x: i32, y: i32, w: u32, h: u32) -> io::Result<()> {
        let r = unsafe { ffi::kl_wl_commit(self.h, x, y, w as c_int, h as c_int) };
        if r != 0 {
            if let Some(m) = self.error() {
                return Err(io::Error::new(io::ErrorKind::Other, m));
            }
        }
        Ok(())
    }

    /// Take events, waiting at most `timeout_ms`. First the parked events
    /// come; if any are parked, nothing is polled (they are older than
    /// everything in the compositor's ring buffer).
    pub fn poll(&mut self, into: &mut [Event], timeout_ms: i32) -> io::Result<usize> {
        if into.is_empty() {
            return Ok(0);
        }
        let n = self.pending.take(into);
        if n > 0 {
            return Ok(n);
        }
        let m = unsafe {
            ffi::kl_wl_poll(self.h, into.as_mut_ptr(), into.len() as c_int, timeout_ms)
        };
        if m < 0 {
            let s = self
                .error()
                .unwrap_or_else(|| "compositor reports an error".into());
            return Err(io::Error::new(io::ErrorKind::Other, s));
        }
        // Was der Compositor gerade geliefert hat, wandert in die
        // PendingQueue und kommt sofort wieder heraus — sonst könnten
        // geparkte und neue Ereignisse sich überholen.
        self.pending.park(&into[..m as usize]);
        Ok(self.pending.take(into))
    }

    /// How many `GESTURE_UP` events were parked recently.
    pub fn parked_ups(&self) -> u64 {
        self.pending.count_parked_ups()
    }

    /// Millisekunden seit dem Öffnen, nach CLOCK_MONOTONIC. Dieselbe Basis
    /// wie `Event::time_ms`.
    pub fn now_ms(&self) -> f64 {
        unsafe { ffi::kl_wl_now_ms(self.h) }
    }

    fn error(&self) -> Option<String> {
        let p = unsafe { ffi::kl_wl_last_error(self.h) };
        if p.is_null() {
            return None;
        }
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if !self.h.is_null() {
            unsafe { ffi::kl_wl_drop(self.h) };
            self.h = ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ohne Wayland-Session darf nichts abstürzen: `open` muss einen
    /// erklärenden Fehler liefern.
    #[test]
    fn open_without_session_hints_and_does_not_crash() {
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            return;
        }
        match Window::open("Kladde", 64, 64, false) {
            Ok(_) => {} // in einer Session, alles gut
            Err(e) => {
                let m = e.to_string();
                assert!(
                    m.contains("Wayland") || m.contains("WAYLAND_DISPLAY"),
                    "the error must say what is missing: {}",
                    m
                );
            }
        }
    }

    /// Die Event-Konstanten müssen zu `shim/kladde_wl.h` passen; der
    /// Kompilierer sieht diesen Fehler nicht.
    #[test]
    fn event_constants_are_unchanged() {
        assert_eq!(event::SURFACE, 1);
        assert_eq!(event::FRAME, 2);
        assert_eq!(event::GESTURE_DOWN, 3);
        assert_eq!(event::GESTURE_MOVE, 4);
        assert_eq!(event::GESTURE_UP, 5);
        assert_eq!(event::KEY, 6);
        assert_eq!(event::CLOSE, 7);
        assert_eq!(event::MENU, 8);
        assert_eq!(event::MOUSE_SLOT, -2);
    }

    /// Die Tastencodes sind **rohe Gerätecodes** (XKB = Wert + 8). Der Test
    /// vergleicht mit der Kernel-Kopfdatei; fehlt die Datei, bleibt die
    /// Liste unten als Rückfall.
    #[test]
    fn keycodes_are_raw_device_codes() {
        let cases = [
            ("KEY_ESC", 1, key::ESC),
            ("KEY_Q", 16, key::Q),
            ("KEY_SPACE", 57, key::SPACE),
            ("KEY_UP", 103, key::UP),
            ("KEY_PAGEUP", 104, key::PAGEUP),
            ("KEY_LEFT", 105, key::LEFT),
            ("KEY_RIGHT", 106, key::RIGHT),
            ("KEY_DOWN", 108, key::DOWN),
            ("KEY_PAGEDOWN", 109, key::PAGEDOWN),
        ];
        let header = std::fs::read_to_string("/usr/include/linux/input-event-codes.h").ok();

        for (name, expected, value) in cases {
            assert_eq!(value, expected, "{}", name);
            if let Some(text) = &header {
                let from_header = text
                    .lines()
                    .find(|l| {
                        l.starts_with("#define") && l.split_whitespace().nth(1) == Some(name)
                    })
                    .and_then(|l| l.split_whitespace().nth(2))
                    .and_then(|v| v.parse::<i32>().ok())
                    .unwrap_or_else(|| panic!("{} missing in input-event-codes.h", name));
                assert_eq!(value, from_header, "{} differs from the header file", name);
            }
        }
    }

    fn ev(kind: i32) -> Event {
        Event {
            kind,
            serial: kind,
            time_ms: kind as f64,
            a: 0,
            b: 0,
            c: 0,
        }
    }

    fn take(w: &mut PendingQueue, n: usize) -> Vec<Event> {
        let mut buf = vec![
            Event {
                kind: -1,
                serial: 0,
                time_ms: 0.0,
                a: 0,
                b: 0,
                c: 0,
            };
            n
        ];
        let m = w.take(&mut buf);
        buf.truncate(m);
        buf
    }

    /// Ereignisse, die während eines Wartens anfallen, gehen nicht verloren,
    /// und die Reihenfolge bleibt stehen.
    #[test]
    fn parked_events_are_not_dropped() {
        let mut w = PendingQueue::default();
        w.park(&[ev(event::GESTURE_UP)]);
        assert_eq!(w.len(), 1);
        let got = take(&mut w, 8);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, event::GESTURE_UP);
        assert_eq!(w.len(), 0);

        w.park(&[ev(10), ev(11), ev(12)]);
        w.park(&[ev(13)]);
        let got: Vec<i32> = take(&mut w, 8).iter().map(|e| e.kind).collect();
        assert_eq!(got, vec![10, 11, 12, 13]);
    }

    /// Ein zu kleiner Puffer darf nichts verschlucken: der Rest bleibt für
    /// den nächsten Aufruf liegen.
    #[test]
    fn a_small_buffer_loses_nothing() {
        let mut w = PendingQueue::default();
        w.park(&[ev(1), ev(2), ev(3), ev(4), ev(5)]);
        assert_eq!(take(&mut w, 2).len(), 2);
        assert_eq!(w.len(), 3);
        assert_eq!(
            take(&mut w, 2).iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert_eq!(
            take(&mut w, 2).iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![5]
        );
        assert_eq!(w.len(), 0);
    }

    #[test]
    fn an_empty_queue_delivers_nothing() {
        let mut w = PendingQueue::default();
        assert_eq!(take(&mut w, 8).len(), 0);
        w.park(&[]);
        assert_eq!(take(&mut w, 8).len(), 0);
        assert_eq!(take(&mut w, 0).len(), 0);
    }
}
