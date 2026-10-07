/*
 * Kladde - C shim around MuPDF.
 *
 * WHY IT EXISTS:
 *
 * MuPDF's error handling runs via fz_try/fz_catch, which are C macros on
 * setjmp/longjmp. A longjmp across Rust frames is UB: Rust frames are
 * not unwound, destructors do not run, catch_unwind does not help.
 * Hence the rule in the whole project:
 *
 *     Not a single MuPDF symbol is called directly from Rust.
 *     Everything runs through this file.
 *
 * Thus the complete unsafe area is one short C file that you read once,
 * instead of being scattered across .rs files.
 *
 * RULES FOR THIS FILE:
 *   1. Exactly ONE fz_try/fz_catch pair per function, at the top level.
 *      The actual work sits in a static helper function.
 *   2. Automatic variables written between setjmp and longjmp and read
 *      afterwards MUST be volatile. Otherwise their value is undefined
 *      after fz_catch. MuPDF offers the macro fz_var(x) for that, but we
 *      use plain volatile - that is portable.
 *   3. In the fz_catch block, do not call any MuPDF function that could
 *      throw. fz_caught_message() is safe.
 */

#ifndef KLADDE_SHIM_H
#define KLADDE_SHIM_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct kl_ctx  kl_ctx;
typedef struct kl_doc  kl_doc;
typedef struct kl_list kl_list;
typedef struct kl_pix  kl_pix;

/* ---- Context -------------------------------------------------------- */
/* store_bytes: upper limit for MuPDF's resource store. 0 = unlimited.
 * On a 512 MB device: 4..8 MB per worker. */
kl_ctx *kl_ctx_new(size_t store_bytes);
void     kl_ctx_drop(kl_ctx *c);
const char *kl_mupdf_version(void);

/* ---- Document ------------------------------------------------------- */
kl_doc *kl_doc_open(kl_ctx *c, const char *path, char *err, size_t errlen);
void     kl_doc_drop(kl_doc *d);
int      kl_doc_page_count(kl_doc *d);
/* out = {x0, y0, x1, y1} in points (CropBox, /Rotate already applied).
 * Returns 0 on success, -1 on error. */
int      kl_page_box(kl_ctx *c, kl_doc *d, int page, double out[4]);

/* ---- Phase 1: interpret the content stream --------------------------- */
/* Creates the display list. It is resolution-independent: parse once,
 * raster as often as you like at any resolution. */
kl_list *kl_parse(kl_ctx *c, kl_doc *d, int page, char *err, size_t errlen);
void     kl_list_drop(kl_list *l);

/* ---- Pixmap ---------------------------------------------------------- */
/* BGR order, so that the bytes fit directly into an XRGB8888 framebuffer
 * (memcpy per line instead of conversion). alpha=1 yields 4 bytes/pixel
 * and hence a 1:1 blit; alpha=0 yields 3 bytes/pixel. */
kl_pix  *kl_pix_new(kl_ctx *c, int w, int h, int alpha);
void     kl_pix_drop(kl_pix *p);
int      kl_pix_info(kl_pix *p, unsigned char **samples, ptrdiff_t *stride,
                     int *n);

/* ---- Phase 2: raster a rectangle ------------------------------------ */
/* scale: points -> pixels. ox/oy: page origin (points) -> pixel origin.
 * Rectangle (x0,y0,x1,y1) in pixel coordinates of the pixmap - in
 * practice the whole page.
 *
 * Return:  0 = rendered
 *          1 = page was empty (fz_display_list_is_empty)
 *         -1 = error, text in err
 *
 * out_nodes: length of the display list, i.e. a complexity measure of
 *            the page. Set by MuPDF during replay (cookie->progress_max).
 *
 * CANCELLATION: There is deliberately no cancel parameter here. The
 * worker always rasters the page to completion; preemption happens at
 * the job level (the newest job wins), not in the middle of a MuPDF
 * call. Without a parameter, no thread can change a shared flag during
 * a running call either.
 */
int kl_raster(kl_ctx *c, kl_list *l, float scale, float ox, float oy,
              kl_pix *p, int x0, int y0, int x1, int y1,
              int *out_nodes, char *err, size_t errlen);

#ifdef __cplusplus
}
#endif
#endif /* KLADDE_SHIM_H */
