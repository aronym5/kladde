/*
 * Stub implementation: makes `cargo check` / `cargo build` and
 * rust-analyzer possible on machines without MuPDF (CI, developer
 * laptop).
 *
 * Activated with KLADDE_MUPDF=stub. All calls fail immediately, and the
 * bench binary aborts with a clear message - so a stub can never
 * unnoticed find its way into a running system.
 */

#include "kladde_shim.h"

#include <stdio.h>

kl_ctx *kl_ctx_new(size_t store_bytes) { (void)store_bytes; return NULL; }
void kl_ctx_drop(kl_ctx *c) { (void)c; }
const char *kl_mupdf_version(void) { return "stub (no MuPDF)"; }

kl_doc *kl_doc_open(kl_ctx *c, const char *path, char *err, size_t errlen)
{
	(void)c; (void)path;
	if (err && errlen) snprintf(err, errlen, "Kladde was built as a stub (KLADDE_MUPDF=stub)");
	return NULL;
}
void kl_doc_drop(kl_doc *d) { (void)d; }
int kl_doc_page_count(kl_doc *d) { (void)d; return 0; }
int kl_page_box(kl_ctx *c, kl_doc *d, int page, double out[4])
{
	(void)c; (void)d; (void)page; (void)out; return -1;
}

kl_list *kl_parse(kl_ctx *c, kl_doc *d, int page, char *err, size_t errlen)
{
	(void)c; (void)d; (void)page;
	if (err && errlen) snprintf(err, errlen, "Kladde was built as a stub (KLADDE_MUPDF=stub)");
	return NULL;
}
void kl_list_drop(kl_list *l) { (void)l; }

kl_pix *kl_pix_new(kl_ctx *c, int w, int h, int alpha)
{
	(void)c; (void)w; (void)h; (void)alpha; return NULL;
}
void kl_pix_drop(kl_pix *p) { (void)p; }
int kl_pix_info(kl_pix *p, unsigned char **samples, ptrdiff_t *stride, int *n)
{
	(void)p; (void)samples; (void)stride; (void)n; return -1;
}

int kl_raster(kl_ctx *c, kl_list *l, float scale, float ox, float oy,
              kl_pix *p, int x0, int y0, int x1, int y1,
              int *out_nodes, char *err, size_t errlen)
{
	(void)c; (void)l; (void)scale; (void)ox; (void)oy; (void)p;
	(void)x0; (void)y0; (void)x1; (void)y1; (void)out_nodes;
	if (err && errlen) snprintf(err, errlen, "Kladde was built as a stub (KLADDE_MUPDF=stub)");
	return -1;
}
