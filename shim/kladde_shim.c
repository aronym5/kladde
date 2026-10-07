#include "kladde_shim.h"

#include <mupdf/fitz.h>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

struct kl_ctx {
	fz_context *ctx;
};

struct kl_doc {
	fz_context *ctx;
	fz_document *doc;
};

struct kl_list {
	fz_context *ctx;
	fz_display_list *list;
};

struct kl_pix {
	fz_context *ctx;
	fz_pixmap *pix;
	int w, h, n;
};

/* Error text into the caller's buffer. Only call from fz_catch. */
static void kl_err(fz_context *ctx, char *err, size_t errlen)
{
	if (err == NULL || errlen == 0)
		return;
	err[0] = '\0';
	const char *m = fz_caught_message(ctx);
	if (m)
		snprintf(err, errlen, "%s", m);
}

/* ------------------------------------------------------------------ */
/* Context                                                             */
/* ------------------------------------------------------------------ */

kl_ctx *kl_ctx_new(size_t store_bytes)
{
	fz_context *ctx = fz_new_context(NULL, NULL, store_bytes);
	if (!ctx)
		return NULL;

	/* Needed in older MuPDF versions, a no-op in newer ones. */
	fz_register_document_handlers(ctx);

	kl_ctx *c = (kl_ctx *)malloc(sizeof *c);
	if (!c) {
		fz_drop_context(ctx);
		return NULL;
	}
	c->ctx = ctx;
	return c;
}

void kl_ctx_drop(kl_ctx *c)
{
	if (!c)
		return;
	fz_drop_context(c->ctx);
	free(c);
}

const char *kl_mupdf_version(void)
{
	return FZ_VERSION;
}

/* ------------------------------------------------------------------ */
/* Document                                                            */
/* ------------------------------------------------------------------ */

kl_doc *kl_doc_open(kl_ctx *c, const char *path, char *err, size_t errlen)
{
	fz_document * volatile doc = NULL;

	fz_try(c->ctx) {
		doc = fz_open_document(c->ctx, path);
	}
	fz_catch(c->ctx) {
		doc = NULL;
		kl_err(c->ctx, err, errlen);
	}
	if (!doc)
		return NULL;

	kl_doc *d = (kl_doc *)malloc(sizeof *d);
	if (!d) {
		fz_drop_document(c->ctx, (fz_document *)doc);
		return NULL;
	}
	d->ctx = c->ctx;
	d->doc = (fz_document *)doc;
	return d;
}

void kl_doc_drop(kl_doc *d)
{
	if (!d)
		return;
	fz_drop_document(d->ctx, d->doc);
	free(d);
}

int kl_doc_page_count(kl_doc *d)
{
	volatile int n = 0;
	fz_try(d->ctx) {
		n = fz_count_pages(d->ctx, d->doc);
	}
	fz_catch(d->ctx) {
		n = 0;
	}
	return n;
}

int kl_page_box(kl_ctx *c, kl_doc *d, int page, double out[4])
{
	volatile int rc = -1;

	fz_try(c->ctx) {
		fz_page *p = fz_load_page(c->ctx, d->doc, page);
		fz_rect r = fz_bound_page(c->ctx, p);
		out[0] = r.x0;
		out[1] = r.y0;
		out[2] = r.x1;
		out[3] = r.y1;
		fz_drop_page(c->ctx, p);
		rc = 0;
	}
	fz_catch(c->ctx) {
		rc = -1;
	}
	return rc;
}

/* ------------------------------------------------------------------ */
/* Phase 1 - interpreting the content stream                           */
/*                                                                     */
/* fz_try / fz_always / fz_catch in this form: catches exceptions and  */
/* still always releases whatever was allocated. That is the decisive  */
/* point - without fz_always, draw devices and display lists remain    */
/* behind on an error. In continuous operation they accumulate until   */
/* the memory is gone.                                                 */
/* ------------------------------------------------------------------ */

kl_list *kl_parse(kl_ctx *c, kl_doc *d, int page, char *err, size_t errlen)
{
	kl_list * volatile out = NULL;
	fz_display_list * volatile lst = NULL;
	fz_page * volatile p = NULL;
	fz_device * volatile dev = NULL;

	fz_try(c->ctx) {
		p = fz_load_page(c->ctx, d->doc, page);
		{
			/* fz_bound_page delivers the CropBox including /Rotate,
			 * so the display list is already correctly oriented. */
			fz_rect box = fz_bound_page(c->ctx, (fz_page *)p);
			fz_display_list *l = fz_new_display_list(c->ctx, box);
			dev = fz_new_list_device(c->ctx, l);
			/* Identity CTM: the list is thus resolution-independent. */
			fz_run_page(c->ctx, (fz_page *)p, (fz_device *)dev,
			            fz_identity, NULL);
			/* MuPDF since 1.27 requires fz_close_device before fz_drop_device.
			 * The close belongs in the fz_try because it may throw
			 * ("items left on stack"). Without the close fz_drop_device
			 * reports "dropping unclosed device", and the output can be
			 * incomplete - with a draw device that has separations,
			 * nothing is copied at the end. */
#ifdef KL_HAVE_CLOSE_DEVICE
			fz_close_device(c->ctx, (fz_device *)dev);
#endif
			lst = l;
			out = (kl_list *)malloc(sizeof *out);
			if (out) {
				out->ctx = c->ctx;
				out->list = l;
			}
		}
	}
	fz_always(c->ctx) {
		if (dev)
			fz_drop_device(c->ctx, (fz_device *)dev);
		if (p)
			fz_drop_page(c->ctx, (fz_page *)p);
		/* Only if it was not handed to the caller. */
		if (!out && lst)
			fz_drop_display_list(c->ctx, (fz_display_list *)lst);
	}
	fz_catch(c->ctx) {
		kl_err(c->ctx, err, errlen);
		free(out);
		out = NULL;
	}
	return out;
}

void kl_list_drop(kl_list *l)
{
	if (!l)
		return;
	fz_drop_display_list(l->ctx, l->list);
	free(l);
}

/* ------------------------------------------------------------------ */
/* Pixmap                                                              */
/* ------------------------------------------------------------------ */

kl_pix *kl_pix_new(kl_ctx *c, int w, int h, int alpha)
{
	kl_pix * volatile p = NULL;

	fz_try(c->ctx) {
		/* BGR is the memory order of a little-endian XRGB8888
		 * framebuffer (byte 0 = B). build.rs checks whether
		 * fz_device_bgr exists and defines KL_BGR. */
#ifdef KL_BGR
		fz_colorspace *cs = fz_device_bgr(c->ctx);
#else
		fz_colorspace *cs = fz_device_rgb(c->ctx);
#endif
		/* The seps parameter (spot colors) came in 1.26;
		 * NULL means: no separations. */
		fz_pixmap *pm = fz_new_pixmap(c->ctx, cs, w, h, NULL, alpha);
		kl_pix *tmp = (kl_pix *)malloc(sizeof *tmp);
		if (tmp) {
			tmp->ctx = c->ctx;
			tmp->pix = pm;
			tmp->w = w;
			tmp->h = h;
			tmp->n = pm->n;
			p = tmp;
		} else {
			fz_drop_pixmap(c->ctx, pm);
		}
	}
	fz_catch(c->ctx) {
		p = NULL;
	}
	return p;
}

void kl_pix_drop(kl_pix *p)
{
	if (!p)
		return;
	fz_drop_pixmap(p->ctx, p->pix);
	free(p);
}

int kl_pix_info(kl_pix *p, unsigned char **samples, ptrdiff_t *stride, int *n)
{
	if (!p || !samples || !stride || !n)
		return -1;
	*samples = p->pix->samples;
	*stride = p->pix->stride;
	*n = p->pix->n;
	return 0;
}

/* ------------------------------------------------------------------ */
/* Phase 2 - rasterizing a rectangle                                   */
/* ------------------------------------------------------------------ */

int kl_raster(kl_ctx *c, kl_list *l, float scale, float ox, float oy,
              kl_pix *p, int x0, int y0, int x1, int y1,
              int *out_nodes, char *err, size_t errlen)
{
	volatile int rc = -1;
	fz_device * volatile dev = NULL;
	fz_matrix ctm;
	fz_irect clip;
	fz_rect scissor;

	if (out_nodes)
		*out_nodes = 0;

	/* Points -> pixels, page origin -> pixel origin. The scaling belongs
	 * at fz_run_display_list; the draw device stays at fz_identity. */
	ctm.a = scale;  ctm.b = 0.0f;
	ctm.c = 0.0f;   ctm.d = scale;
	ctm.e = ox;     ctm.f = oy;

	/* The draw device's bounding box is in device pixels, that stays
	 * so. */
	clip.x0 = x0; clip.y0 = y0; clip.x1 = x1; clip.y1 = y1;

	/* Scissor in device pixels, not in list points: do not divide by scale. */
	scissor.x0 = (float)x0; scissor.y0 = (float)y0;
	scissor.x1 = (float)x1; scissor.y1 = (float)y1;

	fz_try(c->ctx) {
		if (fz_display_list_is_empty(c->ctx, l->list)) {
			rc = 1;
		} else {
			dev = fz_new_draw_device_with_bbox(c->ctx, fz_identity,
			                                   p->pix, &clip);
			{
				fz_cookie ck;
				memset(&ck, 0, sizeof ck);
				fz_run_display_list(c->ctx, l->list, (fz_device *)dev,
				                    ctm, scissor, &ck);
				/* MuPDF sets progress_max = number of display nodes
				 * before the loop even runs. That is our complexity
				 * measure. */
				if (out_nodes)
					*out_nodes = ck.progress_max;
			}
			/* Close before drop, see comment in kl_parse. Here it is
			 * especially important: the draw device only finishes its
			 * separations at the end. */
#ifdef KL_HAVE_CLOSE_DEVICE
			fz_close_device(c->ctx, (fz_device *)dev);
#endif
			rc = 0;
		}
	}
	fz_always(c->ctx) {
		if (dev)
			fz_drop_device(c->ctx, (fz_device *)dev);
	}
	fz_catch(c->ctx) {
		rc = -1;
		kl_err(c->ctx, err, errlen);
	}
	return rc;
}
