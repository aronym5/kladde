/*
 * Kladde - Wayland client in the C shim.
 *
 * WHY C AND NOT RUST:
 * There is no `libc` crate in this project. The libwayland library talks
 * sockets and uses macros for the listener initialization; that is three
 * lines in C and, in Rust without crates, a hand-written dispatcher.
 *
 * WHAT THIS FILE DOES (and what not):
 * It makes a window, allocates two XRGB8888 buffers, commits them with
 * damage rectangle and hands touch, mouse, keyboard and the compositor's
 * presentation time outwards. It draws nothing and knows nothing about
 * PDF, pages or scheduling.
 *
 * WHY XRGB8888:
 * That is exactly MuPDF's `fz_device_bgr` layout (little-endian, X in the
 * topmost byte). The compositor gets the same memory that MuPDF filled,
 * without a single line being converted anywhere.
 *
 * THREADS:
 * Everything is single-threaded. The client belongs to the thread that
 * also services the render path.
 */

#ifndef KLADDE_WL_H
#define KLADDE_WL_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct kl_wl kl_wl;

/* true if built against real Wayland. false means: the stub,
 * kl_wl_open() then always returns NULL with an explanatory error. */
int kl_wl_available(void);

/* Events that kl_wl_poll delivers. The names say what the user sees,
 * not what the protocol says. */
enum {
    KL_WL_NONE = 0,
    /* The window is fully configured, a = width, b = height. Comes
     * once, then on every change of the window size. */
    KL_WL_SURFACE,
    /* A frame was really on screen now. serial is the commit number,
     * time_ms the point in time after the compositor - both relative to
     * CLOCK_MONOTONIC, the same base as kl_wl_now_ms(). */
    KL_WL_FRAME,
    /* Gesture: finger or mouse. a = x, b = y, c = slot. Touch and mouse
     * yield the same three events; the origin sits in the slot:
     * a finger carries its identifier (>= 0), a cancelled one the -1,
     * **the mouse the -2**. Only thus can the caller distinguish a click
     * (paging) from a fingertip (nothing). */
    KL_WL_GESTURE_DOWN,
    KL_WL_GESTURE_MOVE,
    KL_WL_GESTURE_UP,
    /* a = keycode as it stands on the wire: raw device code (evdev).
     * The xkb keycode is this value plus eight - see wayland.xml,
     * wl_keyboard.keymap. */
    KL_WL_KEY,
    KL_WL_CLOSE,
    /* The right mouse button: a = x, b = y. Its own event, because a
     * right click is **not** a gesture - it should open the menu and
     * not shift a page. */
    KL_WL_MENU
};

/* The slot of a gesture event that came from the mouse. See the comment
 * at KL_WL_GESTURE_DOWN: finger >= 0, cancel -1, mouse -2. */
#define KL_WL_SLOT_MOUSE (-2)

typedef struct {
    int type;
    int serial;
    double time_ms;
    int a, b, c;
} kl_wl_event;

/* Open a window. w/h are the wish; the compositor may decide
 * differently, that arrives back as KL_WL_SURFACE. fullscreen != 0
 * requests fullscreen. Returns NULL and writes a reason into err if it
 * does not work. */
kl_wl *kl_wl_open(const char *title, int w, int h, int fullscreen,
                  char *err, size_t errlen);

void kl_wl_drop(kl_wl *w);

/* Which protocols the compositor offers at all. version < 0 means "not
 * there". Used to log what is active - and to cleanly skip
 * wp_presentation when it is missing. */
int kl_wl_want(kl_wl *w, const char *iface, int *version);

/* Asks for the back buffer for exactly w_px x h_px pixels and delivers
 * pointer, stride (in bytes) and the actual sizes in buf_w/buf_h. The
 * memory belongs to the shim, stays valid until the next call, and is
 * XRGB8888 with alpha = 255.
 *
 * On a different size the buffers are recreated immediately, even if
 * one is still with the compositor; a release in flight is silently
 * discarded by libwayland.
 *
 * NULL is an error or WouldBlock; the sizes then still stand, so the
 * caller does not have to write past the edge. */
unsigned char *kl_wl_back_buffer(kl_wl *w, int w_px, int h_px, int *stride,
                                 int *buf_w, int *buf_h);

/* Present the back buffer as a frame: report only the rectangle
 * x,y,cw,ch as damaged, then commit. Returns 0 on success, -1 on
 * error. */
int kl_wl_commit(kl_wl *w, int x, int y, int cw, int ch);

/* Fetch up to max events, wait at most timeout_ms. Returns the number
 * of delivered events, 0 on timeout, -1 on a compositor error
 * (kl_wl_last_error then says why). */
int kl_wl_poll(kl_wl *w, kl_wl_event *ev, int max, int timeout_ms);

/* Milliseconds since opening, per CLOCK_MONOTONIC. The same base as in
 * the time_ms of the callbacks, so that the Rust code can subtract. */
double kl_wl_now_ms(kl_wl *w);

/* Toggle fullscreen at runtime (1=on, 0=off). A request: the
 * compositor answers with a configure, and only there does it stand
 * whether it accepts it. */
void kl_wl_set_fullscreen(kl_wl *w, int on);

/* Is the window in fullscreen now? From the last configure - the
 * compositor decides, not our request. */
int kl_wl_is_fullscreen(const kl_wl *w);

/* Query seat capabilities. */
int kl_wl_has_touch(const kl_wl *w);
int kl_wl_has_pointer(const kl_wl *w);
int kl_wl_has_keyboard(const kl_wl *w);

/* Reason for the last error, or "" . */
const char *kl_wl_last_error(kl_wl *w);

#ifdef __cplusplus
}
#endif

#endif /* KLADDE_WL_H */
