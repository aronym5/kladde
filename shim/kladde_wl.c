/*
 * Kladde - Wayland client in the C shim. See kladde_wl.h for the
 * rationale behind C and for the rules of this file.
 *
 * SETUP ORDER (the order is the actual work):
 *
 *   1. Bind the registry, roundtrip -> globals + wl_seat
 *   2. Connect wl_compositor, wl_shm, xdg_wm_base
 *   3. Create wl_surface + xdg_surface + xdg_toplevel, set the title,
 *      request server-side decoration (if xdg-decoration is present)
 *   4. wl_surface.commit WITHOUT a buffer -> the compositor replies with
 *      xdg_toplevel.configure (width, height) followed by
 *      xdg_surface.configure (serial)
 *   5. Acknowledge the serial with ack_configure
 *   6. Attach a buffer and commit -> finally an image
 *
 * Without step 4 nothing happens, and without step 5 KWin writes
 * nothing to the screen.
 *
 * ON wp_presentation:
 * The compositor reports when a frame was REALLY on the panel.
 * `KL_WL_VERBOSE=1` lists the globals together with their versions.
 *
 * ON EVENT HANDLING:
 * Everything lands in a ring buffer that kl_wl_poll hands outwards.
 * No callback into Rust, no extra thread, no allocation in the
 * event path.
 *
 * CLEANUP:
 * wl_seat, wl_touch, wl_pointer and wl_keyboard are cleaned up with
 * wl_proxy_destroy, not with the *_release requests. Those requests only
 * exist from a certain protocol version on, and we drop the connection
 * shortly afterwards anyway.
 */

#define _GNU_SOURCE 1

#include <errno.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include <wayland-client.h>

#include "xdg-shell-client-protocol.h"
#include "presentation-time-client-protocol.h"

/* Server-side decoration (title bar, border) is **optional**: the
 * compositor does not have to offer the protocol
 * (zxdg_decoration_manager_v1). Without the header here Kladde still
 * builds and behaves as before - the compositor then decides on its
 * own whether it paints decoration. */
#if __has_include("xdg-decoration-unstable-v1-client-protocol.h")
#include "xdg-decoration-unstable-v1-client-protocol.h"
#define KL_HAVE_DECORATION 1
#endif

/* The idle inhibitor (the screen must not dim or blank while a page is
 * shown) is optional in two ways: the compositor does not have to offer
 * zwp_idle_inhibit_manager_v1, and not every wayland-protocols
 * installation ships the XML of the unstable protocol. Without the
 * header Kladde builds and behaves as before. */
#if __has_include("idle-inhibit-unstable-v1-client-protocol.h")
#include "idle-inhibit-unstable-v1-client-protocol.h"
#define KL_HAVE_IDLE_INHIBIT 1
#endif

#include "kladde_wl.h"

#define KL_NBUF 2
#define KL_QUEUES 64

/* The slot value carried by an event that came from the **mouse** (or a
 * trackpad) and not from a finger. Touch sends the real finger
 * identifiers (>= 0), a cancelled finger the -1; the mouse gets this
 * -2. Only thus can the caller distinguish a click (paging) from a
 * fingertip (nothing). */
#define KL_MOUSE (-2)

struct kl_wl {
    struct wl_display *display;
    struct wl_registry *registry;
    struct wl_compositor *compositor;
    struct wl_shm *shm;
    struct xdg_wm_base *wm_base;
    struct xdg_surface *xdg_surface;
    struct xdg_toplevel *toplevel;
#ifdef KL_HAVE_DECORATION
    struct zxdg_decoration_manager_v1 *deco_mgr;
    struct zxdg_toplevel_decoration_v1 *decoration;
#endif
#ifdef KL_HAVE_IDLE_INHIBIT
    /* "Do not dim the screen while this window is in the foreground." The
     * inhibitor hangs on the surface; see update_idle_inhibit() for when
     * it is created and destroyed. */
    struct zwp_idle_inhibit_manager_v1 *idle_mgr;
    struct zwp_idle_inhibitor_v1 *inhibitor;
#endif
    struct wl_surface *surface;

    struct wl_seat *seat;
    struct wl_touch *touch;
    struct wl_pointer *pointer;
    struct wl_keyboard *keyboard;

    int surface_version;      /* damage_buffer needs version >= 4 */
    int v_compositor;
    int v_shm;
    int v_wm_base;
    int v_seat;
    int v_presentation;
    int v_idle;              /* a version < 0 means: not offered */
    struct wp_presentation *pres;
    uint32_t clk_id;        /* reported by clock_id, see below */
    int clk_known;
    double base_pres_ms; /* our base in the clock from clock_id, or <0 */
    int has_touch;
    int has_pointer;

    /* Back buffers */
    int fd[KL_NBUF];
    unsigned char *map[KL_NBUF];
    size_t size[KL_NBUF];
    int stride[KL_NBUF];
    struct wl_buffer *buf[KL_NBUF];
    int in_use[KL_NBUF];
    int cur;               /* last used index */
    int bw, bh;            /* size of the created buffers */

    /* Configuration */
    uint32_t configure_serial;
    int configured;
    int cw, ch;
    /* Is the window in fullscreen? Set from the configure, not from
     * our own request: the compositor decides whether it accepts it. */
    int fullscreen;
    /* The client's window wish, **without** fullscreen: the size it
     * mentioned when opening. A configure in fullscreen reports the
     * screen size and must therefore not overwrite this pair -
     * otherwise, after leaving fullscreen, the screen size would be
     * "the window size", and the way back would be invisible. */
    int dw, dh;
    /* The size that was last reported as KL_WL_SURFACE. This way each
     * change is reported exactly once and not on every configure
     * again, even if the size has not moved. */
    int rw, rh;

    /* Is the window in the foreground? Two things answer that, and
     * neither of them is the keyboard's business - see
     * update_idle_inhibit() for the rule that reads them. */
    int front;           /* the keyboard focus is here: yes / no */
    int front_known;     /* did the keyboard ever say anything? */
    int activated;       /* does the last configure carry ACTIVATED? */
    int activated_seen;  /* did a configure ever carry ACTIVATED? */
    /* Gesture */
    int pointer_down;
    double last_x, last_y;
    double base_ms;        /* CLOCK_MONOTONIC when opening, in ms */

    /* Events */
    kl_wl_event q[KL_QUEUES];
    int q_head, q_tail;

    int verbose;
    char err[256];
};

/* ------------------------------------------------------------------ */

static double now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec * 1000.0 + (double)ts.tv_nsec / 1e6;
}

/*
 * A compositor-clock timestamp in the same base as now_ms.
 *
 * The compositor reports milliseconds after its own timestamp, which
 * per the protocol is the same clock as the client's. If that is not
 * the case, the differences would be nonsense - so the calculation
 * falls back to the local clock as soon as the result is off by more
 * than one day.
 */
static double compo_ms(struct kl_wl *w, uint32_t time) {
    double t = (double)time - w->base_ms;
    double mine = now_ms() - w->base_ms;
    if (t < -86400000.0 || t > mine + 86400000.0)
        return mine;
    return t;
}

static void set_err(struct kl_wl *w, const char *what) {
    snprintf(w->err, sizeof(w->err), "%s", what);
}

static void set_errf(struct kl_wl *w, const char *fmt, const char *arg) {
    snprintf(w->err, sizeof(w->err), fmt, arg);
}

static void push(struct kl_wl *w, int type, int a, int b, int c, double tms) {
    int next = (w->q_head + 1) % KL_QUEUES;
    if (next == w->q_tail) {
        /* Full: better to lose the oldest. A lost touch event is noticed
         * later; a lost close would prevent the user from closing the
         * window. */
        w->q_tail = (w->q_tail + 1) % KL_QUEUES;
    }
    kl_wl_event *e = &w->q[w->q_head];
    e->type = type;
    e->serial = 0;
    e->time_ms = tms;
    e->a = a;
    e->b = b;
    e->c = c;
    w->q_head = next;
}

/* ---- Listener forward declarations -------------------------------- */

/* Does the searched state appear in a configure array?
 *
 * `xdg_toplevel.configure` delivers the states as `wl_array` of `uint32`.
 * That is flat memory, not a vector: you walk it as `uint32_t`,
 * because the byte layout is the stride, not the C type size. */
static int array_has(uint32_t wanted, struct wl_array *a) {
    if (!a)
        return 0;
    const uint32_t *p = a->data;
    size_t n = a->size / sizeof(uint32_t);
    for (size_t i = 0; i < n; i++) {
        if (p[i] == wanted)
            return 1;
    }
    return 0;
}

static const struct wl_registry_listener registry_listener;
static const struct xdg_wm_base_listener wm_base_listener;
static const struct xdg_surface_listener xdg_surface_listener;
static const struct xdg_toplevel_listener toplevel_listener;
static const struct wl_buffer_listener buffer_listener;
static const struct wl_seat_listener seat_listener;
static const struct wl_touch_listener touch_listener;
static const struct wl_pointer_listener pointer_listener;
static const struct wl_keyboard_listener keyboard_listener;
static const struct wp_presentation_listener pres_listener;
static const struct wp_presentation_feedback_listener fb_listener;

/* Defined below with the foreground rule; the registry, the keyboard
 * handlers and the toplevel configure need it before that. */
static void update_idle_inhibit(struct kl_wl *w);

/* ---- Buffers ------------------------------------------------------ */


/*
 * Release the mappings and buffer objects - including those still with
 * the compositor, because the compositor holds the one attached until
 * another arrives. wl_buffer.destroy is always allowed; a release that
 * is in flight is silently discarded by libwayland.
 */
static void free_buffers(struct kl_wl *w) {
    int i;
    for (i = 0; i < KL_NBUF; i++) {
        if (w->buf[i]) {
            wl_buffer_destroy(w->buf[i]);
            w->buf[i] = NULL;
        }
        if (w->map[i]) {
            munmap(w->map[i], w->size[i]);
            w->map[i] = NULL;
            w->size[i] = 0;
        }
        if (w->fd[i] >= 0) {
            close(w->fd[i]);
            w->fd[i] = -1;
        }
        w->stride[i] = 0;
        w->in_use[i] = 0;
    }
    w->bw = w->bh = 0;
}

static int create_buffers(struct kl_wl *w, int width, int height) {
    int stride = (width * 4 + 15) & ~15;
    size_t sz = (size_t)stride * (size_t)height;
    int i;

    for (i = 0; i < KL_NBUF; i++) {
        void *p;
        int fd = memfd_create("kladde", MFD_CLOEXEC);
        if (fd < 0) {
            set_errf(w, "memfd_create: %s", strerror(errno));
            free_buffers(w);
            return -1;
        }
        if (ftruncate(fd, (off_t)sz) != 0) {
            set_errf(w, "ftruncate: %s", strerror(errno));
            close(fd);
            free_buffers(w);
            return -1;
        }
        p = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (p == MAP_FAILED) {
            set_errf(w, "mmap: %s", strerror(errno));
            close(fd);
            free_buffers(w);
            return -1;
        }
        /* light gray instead of random garbage: if only part of the
         * page is drawn, you see light gray instead of straw. */
        memset(p, 0xFF, sz);
        w->fd[i] = fd;
        w->map[i] = (unsigned char *)p;
        w->size[i] = sz;
        w->stride[i] = stride;
        w->in_use[i] = 0;
        w->buf[i] = NULL;
    }
    w->bw = width;
    if (w->verbose)
        fprintf(stderr, "[kladde/wl] buffers %dx%d, stride %d, %zu bytes\n",
                width, height, stride, sz);
    w->bh = height;
    return 0;
}

/* ---- Protocol ----------------------------------------------------- */

static void registry_global(void *data, struct wl_registry *r, uint32_t name,
                            const char *iface, uint32_t ver) {
    struct kl_wl *w = data;
    if (w->verbose)
        fprintf(stderr, "[kladde/wl] global %-20s v%u\n", iface, ver);

    if (strcmp(iface, "wl_compositor") == 0) {
        if (!w->compositor) {
            uint32_t v = ver < 4 ? ver : 4;  /* damage_buffer from 4 */
            w->v_compositor = (int)ver;
            w->compositor = wl_registry_bind(r, name, &wl_compositor_interface, v);
        }
    } else if (strcmp(iface, "wl_shm") == 0) {
        if (!w->shm) {
            uint32_t v = ver < 2 ? ver : 2;  /* XRGB8888 from 2 */
            w->v_shm = (int)ver;
            w->shm = wl_registry_bind(r, name, &wl_shm_interface, v);
        }
    } else if (strcmp(iface, "xdg_wm_base") == 0) {
        if (!w->wm_base) {
            w->v_wm_base = (int)ver;
            w->wm_base = wl_registry_bind(r, name, &xdg_wm_base_interface, 1);
            if (w->wm_base)
                xdg_wm_base_add_listener(w->wm_base, &wm_base_listener, w);
        }
    } else if (strcmp(iface, "wl_seat") == 0) {
        if (!w->seat) {
            uint32_t v = ver < 7 ? ver : 7;
            w->v_seat = (int)ver;
            w->seat = wl_registry_bind(r, name, &wl_seat_interface, v);
            if (w->seat)
                wl_seat_add_listener(w->seat, &seat_listener, w);
        }
    } else if (strcmp(iface, "wp_presentation") == 0) {
        if (!w->pres) {
            uint32_t v = ver < 2 ? ver : 2;
            w->v_presentation = (int)ver;
            w->pres = wl_registry_bind(r, name, &wp_presentation_interface, v);
            if (w->pres)
                wp_presentation_add_listener(w->pres, &pres_listener, w);
        }
    }
#ifdef KL_HAVE_DECORATION
    else if (strcmp(iface, "zxdg_decoration_manager_v1") == 0) {
        if (!w->deco_mgr)
            w->deco_mgr = wl_registry_bind(
                r, name, &zxdg_decoration_manager_v1_interface, 1);
    }
#endif
#ifdef KL_HAVE_IDLE_INHIBIT
    else if (strcmp(iface, "zwp_idle_inhibit_manager_v1") == 0) {
        if (!w->idle_mgr) {
            w->v_idle = (int)ver;
            w->idle_mgr = wl_registry_bind(
                r, name, &zwp_idle_inhibit_manager_v1_interface, 1);
            /* The surface may not exist yet; the inhibitor then follows in
             * setup_surface(). */
            update_idle_inhibit(w);
        }
    }
#endif
}

static void registry_remove(void *data, struct wl_registry *r, uint32_t name) {
    (void)data;
    (void)r;
    (void)name;
}

static void wm_base_ping(void *data, struct xdg_wm_base *b, uint32_t serial) {
    struct kl_wl *w = data;
    (void)b;
    xdg_wm_base_pong(w->wm_base, serial);
}

static void xdg_surface_configure(void *data, struct xdg_surface *s,
                                  uint32_t serial) {
    struct kl_wl *w = data;
    (void)s;
    w->configure_serial = serial;
    /* Acknowledge as early as possible, not just at the next commit:
     * every configure serial wants to be acknowledged, even when we
     * have nothing to show afterwards. */
    xdg_surface_ack_configure(w->xdg_surface, serial);
    w->configured = 1;
}

static void toplevel_configure(void *data, struct xdg_toplevel *t,
                               int32_t width, int32_t height,
                               struct wl_array *states) {
    struct kl_wl *w = data;
    (void)t;
    /* The fullscreen state lives in the configure and **nowhere else**:
     * a request is a plea, only the compositor decides. */
    w->fullscreen = array_has(XDG_TOPLEVEL_STATE_FULLSCREEN, states);

    /* "The surface is now activated" - the compositor's own word on
     * which window is in front, and the one statement that does not
     * need a keyboard (see update_idle_inhibit()). The deactivation
     * arrives the same way, as a configure **without** the state. */
    w->activated = array_has(XDG_TOPLEVEL_STATE_ACTIVATED, states);
    if (w->activated)
        w->activated_seen = 1;
    update_idle_inhibit(w);

    /* 0 x 0 means "we decide ourselves": the compositor has no
     * geometry for this window - exactly what happens when the window
     * went fullscreen before the first commit and leaves it again.
     * Fall back to our own wished size and report that. */
    int ew = width > 0 ? width : 0;
    int eh = height > 0 ? height : 0;
    if (ew > 0 && eh > 0) {
        w->cw = ew;
        w->ch = eh;
        /* Only outside fullscreen is a reported size the window size.
         * In fullscreen it is the screen's. */
        if (!w->fullscreen) {
            w->dw = ew;
            w->dh = eh;
        }
    } else if (!w->fullscreen && w->dw > 0 && w->dh > 0) {
        ew = w->dw;
        eh = w->dh;
        w->cw = ew;
        w->ch = eh;
    }

    /* Report each **change** exactly once. The client compares with its
     * own size anyway; here we merely note that an unchanged configure
     * creates no event (otherwise it would rain on every commit). */
    if (ew > 0 && eh > 0 && (ew != w->rw || eh != w->rh)) {
        w->rw = ew;
        w->rh = eh;
        push(w, KL_WL_SURFACE, ew, eh, 0, now_ms() - w->base_ms);
    }
}

static void toplevel_close(void *data, struct xdg_toplevel *t) {
    struct kl_wl *w = data;
    (void)t;
    push(w, KL_WL_CLOSE, 0, 0, 0, now_ms() - w->base_ms);
}

static void buffer_release(void *data, struct wl_buffer *b) {
    struct kl_wl *w = data;
    int i;
    for (i = 0; i < KL_NBUF; i++) {
        if (w->buf[i] == b) {
            w->in_use[i] = 0;
            return;
        }
    }
}

/* ---- Seat: touch, mouse, keyboard --------------------------------- */

static void seat_capabilities(void *data, struct wl_seat *s, uint32_t caps) {
    struct kl_wl *w = data;
    (void)s;
    if ((caps & WL_SEAT_CAPABILITY_TOUCH) && !w->touch) {
        w->touch = wl_seat_get_touch(w->seat);
        if (w->touch) {
            wl_touch_add_listener(w->touch, &touch_listener, w);
            w->has_touch = 1;
        }
    }
    if ((caps & WL_SEAT_CAPABILITY_POINTER) && !w->pointer) {
        w->pointer = wl_seat_get_pointer(w->seat);
        if (w->pointer) {
            wl_pointer_add_listener(w->pointer, &pointer_listener, w);
            w->has_pointer = 1;
        }
    }
    if ((caps & WL_SEAT_CAPABILITY_KEYBOARD) && !w->keyboard) {
        w->keyboard = wl_seat_get_keyboard(w->seat);
        if (w->keyboard)
            wl_keyboard_add_listener(w->keyboard, &keyboard_listener, w);
    }
    /* If the capability goes away (hot-unplug of input devices), only
     * the flag is reset. The proxy stays - it then stops listening, and
     * kl_wl_drop cleans up. */
    if (!(caps & WL_SEAT_CAPABILITY_TOUCH))
        w->has_touch = 0;
    if (!(caps & WL_SEAT_CAPABILITY_POINTER))
        w->has_pointer = 0;

    /* Which input routes exist is the first question when "input does
     * not work" is reported: without keyboard capability no single key
     * press, without finger capability no gesture. And when the
     * keyboard is detached (tablet!) the seat reports it again. */
    if (w->verbose)
        fprintf(stderr, "[kladde/wl] seat: touch=%d pointer=%d keyboard=%d\n",
                w->has_touch, w->has_pointer, w->keyboard != NULL);
}

static void touch_down(void *data, struct wl_touch *t, uint32_t serial,
                       uint32_t time, struct wl_surface *surface, int32_t id,
                       wl_fixed_t x, wl_fixed_t y) {
    struct kl_wl *w = data;
    (void)t;
    (void)serial;
    (void)surface;
    if (!w->has_touch)
        return;
    push(w, KL_WL_GESTURE_DOWN, (int)wl_fixed_to_double(x),
             (int)wl_fixed_to_double(y), id, compo_ms(w, time));
}

static void touch_up(void *data, struct wl_touch *t, uint32_t serial,
                     uint32_t time, int32_t id) {
    struct kl_wl *w = data;
    (void)t;
    (void)serial;
    if (!w->has_touch)
        return;
    push(w, KL_WL_GESTURE_UP, 0, 0, id, compo_ms(w, time));
}

static void touch_motion(void *data, struct wl_touch *t, uint32_t time,
                         int32_t id, wl_fixed_t x, wl_fixed_t y) {
    struct kl_wl *w = data;
    (void)t;
    if (!w->has_touch)
        return;
    push(w, KL_WL_GESTURE_MOVE, (int)wl_fixed_to_double(x),
             (int)wl_fixed_to_double(y), id, compo_ms(w, time));
}

static void touch_frame(void *data, struct wl_touch *t) {
    (void)data;
    (void)t;
    /* frame groups the events of a single input together. We deliver
     * them individually: the motion measurement wants every point. */
}

static void touch_cancel(void *data, struct wl_touch *t) {
    struct kl_wl *w = data;
    (void)t;
    if (!w->has_touch)
        return;
    push(w, KL_WL_GESTURE_UP, 0, 0, -1, now_ms() - w->base_ms);
}

static void touch_shape(void *data, struct wl_touch *t, int32_t id,
                        wl_fixed_t major, wl_fixed_t minor) {
    (void)data;
    (void)t;
    (void)id;
    (void)major;
    (void)minor;
}

static void touch_orientation(void *data, struct wl_touch *t, int32_t id,
                              wl_fixed_t orientation) {
    (void)data;
    (void)t;
    (void)id;
    (void)orientation;
}

static void pointer_enter(void *data, struct wl_pointer *p, uint32_t serial,
                          struct wl_surface *surface, wl_fixed_t sx,
                          wl_fixed_t sy) {
    struct kl_wl *w = data;
    (void)p;
    (void)serial;
    (void)surface;
    w->last_x = wl_fixed_to_double(sx);
    w->last_y = wl_fixed_to_double(sy);
}

static void pointer_leave(void *data, struct wl_pointer *p, uint32_t serial,
                          struct wl_surface *surface) {
    struct kl_wl *w = data;
    (void)p;
    (void)serial;
    (void)surface;
    if (w->pointer_down) {
        w->pointer_down = 0;
        push(w, KL_WL_GESTURE_UP, (int)w->last_x, (int)w->last_y, KL_MOUSE,
             now_ms() - w->base_ms);
    }
}

static void pointer_motion(void *data, struct wl_pointer *p, uint32_t time,
                           wl_fixed_t sx, wl_fixed_t sy) {
    struct kl_wl *w = data;
    (void)p;
    w->last_x = wl_fixed_to_double(sx);
    w->last_y = wl_fixed_to_double(sy);
    if (w->pointer_down)
        push(w, KL_WL_GESTURE_MOVE, (int)w->last_x, (int)w->last_y, KL_MOUSE,
             compo_ms(w, time));
}

static void pointer_button(void *data, struct wl_pointer *p, uint32_t serial,
                           uint32_t time, uint32_t button, uint32_t state) {
    struct kl_wl *w = data;
    (void)p;
    (void)serial;
    if (state == WL_POINTER_BUTTON_STATE_PRESSED) {
        if (button == 0x110) { /* BTN_LEFT */
            w->pointer_down = 1;
            push(w, KL_WL_GESTURE_DOWN, (int)w->last_x, (int)w->last_y, KL_MOUSE,
                 compo_ms(w, time));
        } else if (button == 0x111) { /* BTN_RIGHT */
            push(w, KL_WL_MENU, (int)w->last_x, (int)w->last_y, 0,
                 compo_ms(w, time));
        }
    } else if (w->pointer_down && button == 0x110) {
        w->pointer_down = 0;
        push(w, KL_WL_GESTURE_UP, (int)w->last_x, (int)w->last_y, KL_MOUSE,
             compo_ms(w, time));
    }
}

static void pointer_axis(void *data, struct wl_pointer *p, uint32_t time,
                         uint32_t axis, wl_fixed_t value) {
    (void)data;
    (void)p;
    (void)time;
    (void)axis;
    (void)value;
    /* The mouse wheel is not bound: paging runs via swipe, click and
     * keys. The handler stays (the compositor sends wheel traces
     * anyway; a missing one would crash), it just does nothing. */
}

static void keyboard_keymap(void *data, struct wl_keyboard *k, uint32_t format,
                            int32_t fd, uint32_t size) {
    (void)data;
    (void)k;
    (void)format;
    (void)size;
    /* Kladde does not need XKB: it reacts to arrows, space and Escape
     * and asks for the raw keycodes for that. The descriptor still
     * belongs to us, otherwise it leaks per seat. */
    if (fd >= 0)
        close(fd);
}

static void keyboard_enter(void *data, struct wl_keyboard *k, uint32_t serial,
                           struct wl_surface *surface, struct wl_array *keys) {
    struct kl_wl *w = data;
    (void)k;
    (void)serial;
    (void)surface;
    (void)keys;
    /* The keyboard focus is one of the two statements about the
     * foreground - the other is the toplevel's `activated` state, which
     * a tablet without a keyboard still gets (see
     * update_idle_inhibit()). */
    w->front_known = 1;
    w->front = 1;
    update_idle_inhibit(w);
}

static void keyboard_leave(void *data, struct wl_keyboard *k, uint32_t serial,
                           struct wl_surface *surface) {
    struct kl_wl *w = data;
    (void)k;
    (void)serial;
    (void)surface;
    w->front_known = 1;
    w->front = 0;
    update_idle_inhibit(w);
}

static void keyboard_key(void *data, struct wl_keyboard *k, uint32_t serial,
                         uint32_t time, uint32_t key, uint32_t state) {
    struct kl_wl *w = data;
    (void)k;
    (void)serial;
    if (state == WL_KEYBOARD_KEY_STATE_PRESSED)
        push(w, KL_WL_KEY, (int)key, 0, 0, compo_ms(w, time));
}

static void keyboard_modifiers(void *data, struct wl_keyboard *k,
                               uint32_t serial, uint32_t depressed,
                               uint32_t latched, uint32_t locked,
                               uint32_t group) {
    (void)data;
    (void)k;
    (void)serial;
    (void)depressed;
    (void)latched;
    (void)locked;
    (void)group;
}

static void keyboard_repeat_info(void *data, struct wl_keyboard *k,
                                 int32_t rate, int32_t delay) {
    (void)data;
    (void)k;
    (void)rate;
    (void)delay;
}

/* ---- Idle inhibit: the screen stays on ---------------------------------
 *
 * While a page is on screen, the display must neither dim nor blank: the
 * page **is** the point, and a dimmed page has to be woken up again -
 * during a rehearsal that is exactly the wrong moment. The wish is one
 * protocol request (zwp_idle_inhibit_manager_v1, called "unstable" but
 * implemented by every compositor worth naming: KWin, Mutter, Sway,
 * Hyprland, Weston, phoc).
 *
 * Two rules shape what happens here:
 *
 * 1. The compositor already owns the visibility. The protocol says: "If
 *    the surface is destroyed, unmapped, becomes occluded, loses
 *    visibility, or otherwise becomes not visually relevant for the
 *    user, the idle inhibitor will not be honored". A covered or
 *    iconified window therefore keeps nobody awake, without us doing
 *    anything.
 *
 * 2. What stays is the window that is visible and yet **not in front**.
 *    Which window is in front is a question about windows, not about
 *    devices: a tablet switches windows with a finger just as a desktop
 *    does with a keyboard, and a window that lost the front there must
 *    let the screen dim again. Two statements answer the question:
 *
 *    - the `activated` state of the toplevel in xdg_toplevel.configure:
 *      the compositor's own word on the active window, and one it can
 *      make without a keyboard. The state carries no `since` in
 *      xdg-shell, so it is part of version 1 - the binding stays as it
 *      is, and a compositor that knows the state sends it.
 *    - the keyboard focus (wl_keyboard.enter/leave), which most
 *      compositors move exactly with the activation.
 *
 *    Either statement saying "in front" counts as an answer; whomever we
 *    ask, we never get two contradictory ones in practice.
 *
 *    Turning the inhibitor off needs a statement as well: a signal that
 *    never came is not a signal. That is what `activated_seen` and
 *    `front_known` are for. A compositor whose window management does
 *    not deal in activation never grants ACTIVATED, so it never
 *    withdraws it either - without the bit the missing state would read
 *    as "you are behind", and that is exactly the tablet the wish was
 *    made for. So the window counts as in front until someone says
 *    otherwise, and rule 1 keeps an invisible window from keeping
 *    anybody awake anyway.
 * --------------------------------------------------------------------- */

#ifdef KL_HAVE_IDLE_INHIBIT

/* Is the window in front - and who says so? `why` names the statement
 * for the verbose log: without it a log line reading "idle inhibit off"
 * leaves the reader guessing which of the two statements did it. */
static int in_foreground(const struct kl_wl *w, const char **why) {
    if (w->activated) {
        *why = "the compositor calls the window active";
        return 1;
    }
    if (w->front_known && w->front) {
        *why = "the keyboard focus is here";
        return 1;
    }
    if (w->activated_seen) {
        *why = "the compositor calls another window active";
        return 0;
    }
    if (w->front_known) {
        *why = "the keyboard focus moved away";
        return 0;
    }
    *why = "no word about the foreground";
    return 1;
}

static void update_idle_inhibit(struct kl_wl *w) {
    const char *why = "?";
    int want;

    if (!w->idle_mgr || !w->surface)
        return;
    want = in_foreground(w, &why);
    if (want == (w->inhibitor != NULL))
        return; /* nothing changed: no request, no flush */

    if (want) {
        w->inhibitor = zwp_idle_inhibit_manager_v1_create_inhibitor(
            w->idle_mgr, w->surface);
        if (!w->inhibitor) {
            set_err(w, "could not create the idle inhibitor");
            return;
        }
    } else {
        zwp_idle_inhibitor_v1_destroy(w->inhibitor);
        w->inhibitor = NULL;
    }
    if (w->verbose)
        fprintf(stderr, "[kladde/wl] idle inhibit %s (%s)\n",
                want ? "on" : "off", why);

    /* The request goes out now and not "sometime with the next frame":
     * exactly while the screen is idle no commit follows to carry it
     * along. EAGAIN means "kernel buffer full", not "error" - the next
     * kl_wl_poll takes over (same reasoning as in
     * kl_wl_set_fullscreen). */
    if (w->display && wl_display_flush(w->display) < 0 && errno != EAGAIN)
        set_errf(w, "failed to send the idle inhibit request: %s",
                 strerror(errno));
}

#else

/* No header, no wish: the callers stay free of #ifdef. */
static void update_idle_inhibit(struct kl_wl *w) { (void)w; }

#endif

/* ---- wp_presentation ------------------------------------------------
 * Here stands the point in time when the image *really* was on the
 * panel, not when we were done rasterizing.
 *
 * From the XML (wayland-protocols, stable/presentation-time): `feedback`
 * takes surface + new object and belongs BEFORE the commit; `presented`
 * and `discarded` are destructors; the time arrives in tv_sec_hi/lo +
 * tv_nsec after the clock from `clock_id`.
 * ------------------------------------------------------------------ */

static void pres_clock_id(void *data, struct wp_presentation *p, uint32_t clk) {
    struct kl_wl *w = data;
    struct timespec ts;
    (void)p;
    w->clk_id = clk;
    w->clk_known = 1;
    /* Our base in THIS clock: CLOCK_MONOTONIC is carried along by the
     * NTP drift handling, CLOCK_MONOTONIC_RAW is not - after weeks of
     * uptime they separate by milliseconds. So the base is measured
     * where it is needed. */
    if (clock_gettime((clockid_t)clk, &ts) == 0)
        w->base_pres_ms = (double)ts.tv_sec * 1000.0 +
                         (double)ts.tv_nsec / 1000000.0;
    else
        w->base_pres_ms = -1.0; /* clock unreadable: discard the time */
}

static void fb_sync_output(void *data, struct wp_presentation_feedback *f,
                           struct wl_output *out) {
    /* Which output delivers the frame. Of no value to us: the time
     * sits in `presented`. */
    (void)data;
    (void)f;
    (void)out;
}

static void fb_presented(void *data, struct wp_presentation_feedback *f,
                         uint32_t sec_hi, uint32_t sec_lo, uint32_t nsec,
                         uint32_t refresh, uint32_t seq_hi, uint32_t seq_lo,
                         uint32_t flags) {
    struct kl_wl *w = data;
    double sec, ms;
    (void)f;
    (void)refresh;
    (void)seq_hi;
    (void)seq_lo;

    sec = (double)sec_hi * 4294967296.0 + (double)sec_lo;
    ms = sec * 1000.0 + (double)nsec / 1000000.0;
    if (w->base_pres_ms < 0.0) {
        push(w, KL_WL_FRAME, 0, 2, 0, 0.0);
        return;
    }
    ms -= w->base_pres_ms;

    /* The clock id is not compared, the result is checked: CLOCK_MONOTONIC
     * and its variants share the same basis (boot); a clock with a
     * different basis, e.g. CLOCK_REALTIME, would yield a difference
     * of years. Such numbers are discarded (b = 2). */
    if (ms < -86400000.0 || ms > 86400000.0) {
        push(w, KL_WL_FRAME, 0, 2, 0, 0.0);
        return;
    }
    /* a = flags (vsync/hw_clock/...), b = 0: displayed. */
    push(w, KL_WL_FRAME, (int)flags, 0, 0, ms);
}

static void fb_discarded(void *data, struct wp_presentation_feedback *f) {
    struct kl_wl *w = data;
    (void)f;
    /* b = 1: never displayed (window covered, output gone, ...).
     * That is a statement, not an error. */
    push(w, KL_WL_FRAME, 0, 1, 0, 0.0);
}

/* ---- Placeholders ---------------------------------------------------
 * Events that Kladde does not evaluate still need a function.
 * libwayland reports an empty slot in the event table and aborts
 * ("listener function for opcode 1 of wl_seat is NULL"). That is why
 * every slot is occupied, including those that never fire with our
 * bound protocol version.
 * ------------------------------------------------------------------ */

static void tl_configure_bounds(void *data, struct xdg_toplevel *t, int32_t w,
                                int32_t h) {
    (void)data;
    (void)t;
    (void)w;
    (void)h;
    /* xdg-shell v4: the compositor limits the window size when another
     * window is covered. Kladde does not scale; the actual size sits in
     * `configure`. */
}

static void tl_wm_capabilities(void *data, struct xdg_toplevel *t,
                               struct wl_array *caps) {
    (void)data;
    (void)t;
    (void)caps;
    /* xdg-shell v5: which window properties the compositor can
     * (maximize, fullscreen, ...). Of no value for Kladde. */
}

static void seat_name(void *data, struct wl_seat *s, const char *name) {
    (void)data;
    (void)s;
    (void)name;
    /* wl_seat v2: the name of the seat, usually "seat0". We do not need
     * it - but the slot has to be occupied. */
}

static void ptr_frame(void *data, struct wl_pointer *p) {
    (void)data;
    (void)p;
    /* Groups the events that belong to the same input. For Kladde every
     * point is interesting on its own, which is why nothing is grouped
     * here. */
}

static void ptr_axis_source(void *data, struct wl_pointer *p, uint32_t src) {
    (void)data;
    (void)p;
    (void)src;
}

static void ptr_axis_stop(void *data, struct wl_pointer *p, uint32_t time,
                          uint32_t axis) {
    (void)data;
    (void)p;
    (void)time;
    (void)axis;
}

static void ptr_axis_discrete(void *data, struct wl_pointer *p, uint32_t axis,
                              int32_t discrete) {
    (void)data;
    (void)p;
    (void)axis;
    (void)discrete;
}

static void ptr_axis_value120(void *data, struct wl_pointer *p, uint32_t axis,
                              int32_t v) {
    (void)data;
    (void)p;
    (void)axis;
    (void)v;
}

static void ptr_axis_relative_direction(void *data, struct wl_pointer *p,
                                        uint32_t axis, uint32_t dir) {
    (void)data;
    (void)p;
    (void)axis;
    (void)dir;
}

/* ---- Listener structs ---------------------------------------------- */

static const struct wl_registry_listener registry_listener = {
    .global = registry_global,
    .global_remove = registry_remove,
};

static const struct xdg_wm_base_listener wm_base_listener = {
    .ping = wm_base_ping,
};

static const struct xdg_surface_listener xdg_surface_listener = {
    .configure = xdg_surface_configure,
};

static const struct xdg_toplevel_listener toplevel_listener = {
    .configure = toplevel_configure,
    .close = toplevel_close,
    .configure_bounds = tl_configure_bounds,
    .wm_capabilities = tl_wm_capabilities,
};

static const struct wl_buffer_listener buffer_listener = {
    .release = buffer_release,
};

static const struct wl_seat_listener seat_listener = {
    .capabilities = seat_capabilities,
    .name = seat_name,
};

static const struct wl_touch_listener touch_listener = {
    .down = touch_down,
    .up = touch_up,
    .motion = touch_motion,
    .frame = touch_frame,
    .cancel = touch_cancel,
    .shape = touch_shape,
    .orientation = touch_orientation,
};

static const struct wl_pointer_listener pointer_listener = {
    .enter = pointer_enter,
    .leave = pointer_leave,
    .motion = pointer_motion,
    .button = pointer_button,
    .axis = pointer_axis,
    .frame = ptr_frame,
    .axis_source = ptr_axis_source,
    .axis_stop = ptr_axis_stop,
    .axis_discrete = ptr_axis_discrete,
    .axis_value120 = ptr_axis_value120,
    .axis_relative_direction = ptr_axis_relative_direction,
};

static const struct wl_keyboard_listener keyboard_listener = {
    .keymap = keyboard_keymap,
    .enter = keyboard_enter,
    .leave = keyboard_leave,
    .key = keyboard_key,
    .modifiers = keyboard_modifiers,
    .repeat_info = keyboard_repeat_info,
};

static const struct wp_presentation_listener pres_listener = {
    .clock_id = pres_clock_id,
};

static const struct wp_presentation_feedback_listener fb_listener = {
    .sync_output = fb_sync_output,
    .presented = fb_presented,
    .discarded = fb_discarded,
};

#ifdef KL_HAVE_DECORATION
static void decoration_configure(void *data,
                                 struct zxdg_toplevel_decoration_v1 *d,
                                 uint32_t mode) {
    struct kl_wl *w = data;
    (void)d;
    /* Here the compositor confirms (or rejects) the request. Nothing
     * changes in behavior: drawing happens in the size from
     * xdg_surface.configure, and Kladde does not paint client-side
     * decoration - the request stays in place. */
    if (w->verbose)
        fprintf(stderr, "[kladde/wl] decoration %s\n",
                mode == ZXDG_TOPLEVEL_DECORATION_V1_MODE_SERVER_SIDE
                    ? "server-side"
                    : "client-side");
}

static const struct zxdg_toplevel_decoration_v1_listener decoration_listener = {
    .configure = decoration_configure,
};
#endif

/* ---- Opening ------------------------------------------------------ */

static int setup_surface(struct kl_wl *w) {
    w->surface = wl_compositor_create_surface(w->compositor);
    if (!w->surface) {
        set_err(w, "wl_compositor_create_surface failed");
        return -1;
    }
    w->surface_version = wl_proxy_get_version((struct wl_proxy *)w->surface);

    /* From here on the display stays awake while this window is in front -
     * early, so that the screen cannot start to dim between the first
     * frame and the first statement about the foreground (see
     * update_idle_inhibit()). */
    update_idle_inhibit(w);

    w->xdg_surface = xdg_wm_base_get_xdg_surface(w->wm_base, w->surface);
    if (!w->xdg_surface) {
        set_err(w, "xdg_wm_base_get_xdg_surface failed");
        return -1;
    }
    xdg_surface_add_listener(w->xdg_surface, &xdg_surface_listener, w);

    w->toplevel = xdg_surface_get_toplevel(w->xdg_surface);
    if (!w->toplevel) {
        set_err(w, "xdg_surface_get_toplevel failed");
        return -1;
    }
    xdg_toplevel_add_listener(w->toplevel, &toplevel_listener, w);

#ifdef KL_HAVE_DECORATION
    /* **Request** server-side decoration (title bar, border): without
     * this wish CLIENT_SIDE is the default, and compositors that honor
     * that (Sway) then paint nothing. The compositor may reject it; its
     * answer state arrives in the decoration's configure. */
    if (w->deco_mgr) {
        w->decoration = zxdg_decoration_manager_v1_get_toplevel_decoration(
            w->deco_mgr, w->toplevel);
        if (w->decoration) {
            zxdg_toplevel_decoration_v1_add_listener(
                w->decoration, &decoration_listener, w);
            zxdg_toplevel_decoration_v1_set_mode(
                w->decoration, ZXDG_TOPLEVEL_DECORATION_V1_MODE_SERVER_SIDE);
        }
    }
#endif
    return 0;
}

int kl_wl_available(void) { return 1; }

kl_wl *kl_wl_open(const char *title, int w, int h, int fullscreen,
                  char *err, size_t errlen) {
    struct kl_wl *wl;
    const char *display_env;
    int i;

    wl = calloc(1, sizeof *wl);
    if (!wl) {
        if (err && errlen)
            snprintf(err, errlen, "out of memory");
        return NULL;
    }
    for (i = 0; i < KL_NBUF; i++)
        wl->fd[i] = -1;
    wl->verbose = getenv("KL_WL_VERBOSE") != NULL;
    wl->base_ms = now_ms();
    /* "Not bound" is not "version 0": a protocol the compositor does not
     * offer has to stay "not there" for kl_wl_want() - calloc would leave
     * 0, which reads as "offered". */
    wl->v_compositor = wl->v_shm = wl->v_wm_base = -1;
    wl->v_seat = wl->v_presentation = wl->v_idle = -1;
    /* The client's wish sits in the shim from the start: without it a
     * `configure(0, 0)` has nothing to fall back on. */
    wl->dw = w > 0 ? w : 0;
    wl->dh = h > 0 ? h : 0;

    display_env = getenv("WAYLAND_DISPLAY");
    if (!display_env || !*display_env) {
        set_err(wl, "WAYLAND_DISPLAY is not set - Kladde belongs "
                    "in a Wayland session, not a plain console");
        goto fail;
    }

    wl->display = wl_display_connect(NULL);
    if (!wl->display) {
        set_errf(wl, "Wayland display unreachable: %s", strerror(errno));
        goto fail;
    }

    wl->registry = wl_display_get_registry(wl->display);
    wl_registry_add_listener(wl->registry, &registry_listener, wl);

    if (wl_display_roundtrip(wl->display) < 0) {
        set_err(wl, "registry roundtrip failed");
        goto fail;
    }
    if (!wl->compositor || !wl->shm || !wl->wm_base) {
        set_err(wl, "The compositor offers neither wl_compositor, wl_shm nor "
                    "xdg_wm_base");
        goto fail;
    }

    if (setup_surface(wl) < 0)
        goto fail;

    xdg_toplevel_set_title(wl->toplevel, title ? title : "Kladde");

    /* **Fullscreen does not come here yet.** See further below. */

    /* Step 4: commit without a buffer, so that the compositor first
     * sends size and serial. Without this call KWin waits forever. */
    wl_surface_commit(wl->surface);
    if (wl_display_roundtrip(wl->display) < 0) {
        set_err(wl, "The compositor never configured");
        goto fail;
    }

    /* Buffers in the size the compositor reported, not in the wished
     * one: the compositor sized the window to its panel size when
     * opening. Without that the first page would immediately force a
     * size change. */
    {
        /* What was **reported** last, else what the compositor
         * configured, else the wish. With the reported pair first, the
         * 0x0 case from `toplevel_configure` runs through correctly
         * here. */
        int bw = wl->rw > 0 ? wl->rw : (wl->cw > 0 ? wl->cw : w);
        int bh = wl->rh > 0 ? wl->rh : (wl->ch > 0 ? wl->ch : h);
        if (create_buffers(wl, bw, bh) < 0)
            goto fail;
    }

    /* Show the first frame: light gray, which is already in the freshly
     * created buffers. Without this call the window stays black until
     * the first page is fully rasterized. Errors are swallowed: then it
     * just stays black until the page arrives. */
    if (wl->configured) {
        int s = 0, bw = 0, bh = 0;
        unsigned char *p = kl_wl_back_buffer(wl, wl->bw, wl->bh, &s, &bw, &bh);
        if (p)
            (void)kl_wl_commit(wl, 0, 0, bw, bh);
    }

    /* **And now** the wish for fullscreen - after the first commit, not
     * before. Otherwise the compositor sees no window size, and the
     * answer to `unset_fullscreen` would be `configure(0, 0)` without a
     * fallback option. */
    if (fullscreen) {
        xdg_toplevel_set_fullscreen(wl->toplevel, NULL);
        wl_surface_commit(wl->surface);
        if (wl_display_roundtrip(wl->display) < 0) {
            set_err(wl, "The compositor never configured");
            goto fail;
        }
        /* The buffers are still in the window size. They need not be
         * recreated: the `configure` arrives as `KL_WL_SURFACE` and
         * `kl_wl_back_buffer()` recreates them then. */
        if (wl->verbose)
            fprintf(stderr,
                    "[kladde/wl] fullscreen requested, compositor says %s "
                    "(window %dx%d, buffers %dx%d)\n",
                    wl->fullscreen ? "yes" : "no", wl->rw, wl->rh, wl->bw,
                    wl->bh);
    }

    if (wl->verbose)
        fprintf(stderr,
                "[kladde/wl] compositor v%d shm v%d wm_base v%d seat v%d "
                "presentation v%d idle v%d surface v%d window %dx%d configured=%d\n",
                wl->v_compositor, wl->v_shm, wl->v_wm_base, wl->v_seat,
                wl->v_presentation, wl->v_idle, wl->surface_version, w, h,
                wl->configured);

    return wl;

fail:
    if (err && errlen)
        snprintf(err, errlen, "%s", wl->err);
    kl_wl_drop(wl);
    return NULL;
}

void kl_wl_drop(kl_wl *w) {
    if (!w)
        return;
    free_buffers(w);
    if (w->display) {
        if (w->toplevel)
            xdg_toplevel_destroy(w->toplevel);
        if (w->xdg_surface)
            xdg_surface_destroy(w->xdg_surface);
#ifdef KL_HAVE_IDLE_INHIBIT
        /* wl_proxy_destroy, not the destroy request: the connection goes
         * down a few lines further anyway (see the header comment on
         * cleanup). The surface is destroyed after this, as it carries
         * the inhibitor. */
        if (w->inhibitor)
            wl_proxy_destroy((struct wl_proxy *)w->inhibitor);
        if (w->idle_mgr)
            wl_proxy_destroy((struct wl_proxy *)w->idle_mgr);
#endif
        if (w->surface)
            wl_surface_destroy(w->surface);
        /* See the header comment: wl_proxy_destroy, not release. */
        if (w->touch)
            wl_proxy_destroy((struct wl_proxy *)w->touch);
        if (w->pointer)
            wl_proxy_destroy((struct wl_proxy *)w->pointer);
        if (w->keyboard)
            wl_proxy_destroy((struct wl_proxy *)w->keyboard);
        if (w->seat)
            wl_proxy_destroy((struct wl_proxy *)w->seat);
#ifdef KL_HAVE_DECORATION
        if (w->decoration)
            wl_proxy_destroy((struct wl_proxy *)w->decoration);
        if (w->deco_mgr)
            wl_proxy_destroy((struct wl_proxy *)w->deco_mgr);
#endif
        if (w->wm_base)
            xdg_wm_base_destroy(w->wm_base);
        if (w->pres)
            wp_presentation_destroy(w->pres);
        if (w->shm)
            wl_shm_destroy(w->shm);
        if (w->compositor)
            wl_compositor_destroy(w->compositor);
        if (w->registry)
            wl_registry_destroy(w->registry);
        wl_display_disconnect(w->display);
    }
    free(w);
}

int kl_wl_want(kl_wl *w, const char *iface, int *version) {
    int v;
    if (!w || !iface)
        return 0;
    if (strcmp(iface, "wl_compositor") == 0)
        v = w->v_compositor;
    else if (strcmp(iface, "wl_shm") == 0)
        v = w->v_shm;
    else if (strcmp(iface, "xdg_wm_base") == 0)
        v = w->v_wm_base;
    else if (strcmp(iface, "wl_seat") == 0)
        v = w->v_seat;
    else if (strcmp(iface, "wp_presentation") == 0)
        v = w->v_presentation;
    else if (strcmp(iface, "zwp_idle_inhibit_manager_v1") == 0)
        v = w->v_idle;
    else
        v = -1;
    if (version)
        *version = v;
    return v >= 0;
}
unsigned char *kl_wl_back_buffer(kl_wl *w, int want_w, int want_h, int *stride,
                                 int *buf_w, int *buf_h) {
    int i, pick = -1;

    /* The actual sizes are reported on every return path, even when
     * nothing works: the caller copies into this mapping and would
     * otherwise write past the edge on a size change. */
    if (buf_w)
        *buf_w = w ? w->bw : 0;
    if (buf_h)
        *buf_h = w ? w->bh : 0;
    if (!w || !stride || want_w <= 0 || want_h <= 0)
        return NULL;

    /* Size change: rebuild immediately, even when buffers are in
     * flight. Old buffers are destroyed in the process (see
     * free_buffers); a release in flight is silently discarded by
     * libwayland. The actual sizes stand above and on every return
     * path, so the caller does not have to guess. */
    if (want_w != w->bw || want_h != w->bh) {
        free_buffers(w);
        if (create_buffers(w, want_w, want_h) < 0)
            return NULL;
    }

    for (i = 0; i < KL_NBUF; i++) {
        int idx = (w->cur + i) % KL_NBUF;
        if (!w->in_use[idx]) {
            pick = idx;
            break;
        }
    }
    if (pick < 0) {
        /* Both buffers with the compositor: anyone seeing this line has
         * a raster time of several screen periods - then the answer has
         * to come from memory and not from the render path. NULL here is
         * WouldBlock, not an error. */
        *stride = w->stride[0];
        return NULL;
    }

    if (!w->buf[pick]) {
        struct wl_shm_pool *pool = wl_shm_create_pool(w->shm, w->fd[pick],
                                                      (int)w->size[pick]);
        if (!pool) {
            set_err(w, "wl_shm_create_pool failed");
            return NULL;
        }
        w->buf[pick] = wl_shm_pool_create_buffer(pool, 0, w->bw, w->bh,
                                                 w->stride[pick],
                                                 WL_SHM_FORMAT_XRGB8888);
        wl_shm_pool_destroy(pool);
        if (!w->buf[pick]) {
            set_err(w, "wl_shm_pool_create_buffer failed");
            return NULL;
        }
        wl_buffer_add_listener(w->buf[pick], &buffer_listener, w);
    }

    w->cur = pick;
    *stride = w->stride[pick];
    if (buf_w)
        *buf_w = w->bw;
    if (buf_h)
        *buf_h = w->bh;
    return w->map[pick];
}

int kl_wl_commit(kl_wl *w, int x, int y, int cw, int ch) {
    int idx;
    if (!w || !w->surface)
        return -1;
    idx = w->cur;
    if (idx < 0 || idx >= KL_NBUF || !w->buf[idx])
        return -1;

    if (w->surface_version >= 4)
        wl_surface_damage_buffer(w->surface, x, y, cw, ch);
    else
        wl_surface_damage(w->surface, x, y, cw, ch);

    wl_surface_attach(w->surface, w->buf[idx], 0, 0);
    /* Feedback for exactly this frame: before the commit, because only
     * the commit can hand one out. */
    if (w->pres) {
        struct wp_presentation_feedback *pfb =
            wp_presentation_feedback(w->pres, w->surface);
        if (pfb)
            wp_presentation_feedback_add_listener(pfb, &fb_listener, w);
    }
    wl_surface_commit(w->surface);
    w->in_use[idx] = 1;

    if (wl_display_flush(w->display) < 0 && errno != EAGAIN) {
        set_errf(w, "Sending to the compositor failed: %s",
                 strerror(errno));
        return -1;
    }
    return 0;
}

int kl_wl_poll(kl_wl *w, kl_wl_event *ev, int max, int timeout_ms) {
    struct pollfd pfd;
    int n = 0, ready;

    if (!w || !w->display || !ev || max <= 0)
        return -1;

    /* Ring buffer first: what is already there costs no syscall. */
    while (n < max && w->q_tail != w->q_head) {
        ev[n++] = w->q[w->q_tail];
        w->q_tail = (w->q_tail + 1) % KL_QUEUES;
    }
    if (n > 0)
        return n;

    if (wl_display_get_error(w->display) != 0) {
        set_errf(w, "Connection to the compositor lost: %s",
                 strerror(errno));
        return -1;
    }

    /* Reader routine per the book: prepare_read only between
     * dispatch_pending and flush, otherwise a read may be missed. */
    while (wl_display_prepare_read(w->display) != 0) {
        if (wl_display_dispatch_pending(w->display) < 0) {
            set_errf(w, "receiving events failed: %s",
                     strerror(errno));
            return -1;
        }
        if (w->q_head != w->q_tail)
            goto drain;
    }

    if (wl_display_flush(w->display) < 0 && errno != EAGAIN) {
        wl_display_cancel_read(w->display);
        set_errf(w, "Sending to the compositor failed: %s",
                 strerror(errno));
        return -1;
    }

    pfd.fd = wl_display_get_fd(w->display);
    pfd.events = POLLIN;
    pfd.revents = 0;
    ready = poll(&pfd, 1, timeout_ms);
    if (ready <= 0) {
        /* Timeout or interruption: the record has not been read yet, so
         * abort and retry on the next call. */
        wl_display_cancel_read(w->display);
        return 0;
    }
    if (wl_display_read_events(w->display) < 0) {
        set_errf(w, "receiving events failed: %s", strerror(errno));
        return -1;
    }
    if (wl_display_dispatch_pending(w->display) < 0) {
        set_errf(w, "processing events failed: %s", strerror(errno));
        return -1;
    }

drain:
    while (n < max && w->q_tail != w->q_head) {
        ev[n++] = w->q[w->q_tail];
        w->q_tail = (w->q_tail + 1) % KL_QUEUES;
    }
    return n;
}

double kl_wl_now_ms(kl_wl *w) {
    if (!w)
        return 0.0;
    return now_ms() - w->base_ms;
}

/* Toggle fullscreen at runtime (1=on, 0=off).
 *
 * This is a **request**: the compositor answers with a configure, and
 * only there does it stand whether it accepts it. That is why this
 * function does *not* change the state here - whoever needs the current
 * state asks `kl_wl_is_fullscreen()`.
 *
 * Flushing happens here ourselves: `wl_display_flush` is otherwise only
 * called by `kl_wl_commit` and by `kl_wl_poll`. A request that is set
 * while no frame is being committed would otherwise remain in the
 * client indefinitely. */
void kl_wl_set_fullscreen(kl_wl *w, int on) {
    if (!w || !w->toplevel)
        return;
    if (on)
        xdg_toplevel_set_fullscreen(w->toplevel, NULL);
    else
        xdg_toplevel_unset_fullscreen(w->toplevel);
    /* The request goes out now, not "sometime at the next frame".
     * EAGAIN means "kernel buffer full", not "error": then the next
     * `kl_wl_poll` takes over, and that one waits beforehand anyway. */
    if (w->display && wl_display_flush(w->display) < 0 && errno != EAGAIN)
        set_errf(w, "failed to send the fullscreen request: %s",
                 strerror(errno));
}

/* Is the window in fullscreen **right now**? From the last configure. */
int kl_wl_is_fullscreen(const kl_wl *w) {
    return w && w->fullscreen;
}

int kl_wl_has_touch(const kl_wl *w) {
    return w && w->has_touch;
}

int kl_wl_has_pointer(const kl_wl *w) {
    return w && w->has_pointer;
}

int kl_wl_has_keyboard(const kl_wl *w) {
    return w && w->keyboard != NULL;
}

/* Is the screen kept from dimming right now? 0 also covers "the
 * compositor has no such protocol" and "the window is not in the
 * foreground" - both mean: the display behaves as usual. */
int kl_wl_idle_inhibited(const kl_wl *w) {
#ifdef KL_HAVE_IDLE_INHIBIT
    return w != NULL && w->inhibitor != NULL;
#else
    (void)w;
    return 0;
#endif
}

const char *kl_wl_last_error(kl_wl *w) {
    if (!w)
        return "no window";
    return w->err;
}
